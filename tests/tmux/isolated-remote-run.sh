#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly HARNESS="$SCRIPT_DIR/harness.sh"
readonly ISOLATED_SHELL="$SCRIPT_DIR/isolated-shell.sh"

fail() {
    printf 'isolated_remote_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in bwrap getent infocmp ldd sed ssh sshd ssh-keygen tee tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-remote-env.XXXXXXXX")
readonly case_root
readonly rootfs="$case_root/rootfs"
readonly client_key="$case_root/client"
readonly host_key="$case_root/host"
readonly authorized_keys="$case_root/authorized_keys"
readonly known_hosts="$case_root/known_hosts"
readonly sshd_config="$case_root/sshd_config"
readonly sshd_log="$case_root/sshd.log"
readonly sshd_pid_file="$case_root/sshd.pid"
readonly r1_output="$case_root/r1.txt"
readonly lr1_output="$case_root/lr1.txt"
sshd_pid=

cleanup() {
    local status=0
    if [[ -n $sshd_pid ]] && kill -0 "$sshd_pid" 2>/dev/null; then
        kill "$sshd_pid" 2>/dev/null || status=1
        wait "$sshd_pid" 2>/dev/null || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-remote-env.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        printf 'isolated_remote_cleanup=pass\n'
    else
        printf 'isolated_remote_cleanup=fail\n' >&2
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
mkdir -p "$rootfs/bin" "$rootfs/dev" "$rootfs/etc" "$rootfs/proc" \
    "$rootfs/tmp" "$rootfs/usr/bin" "$rootfs/usr/lib/locale" \
    "$rootfs/usr/share/terminfo/t" \
    "$rootfs/usr/share/terminfo/x"

declare -a remote_commands=(
    awk grep hostname infocmp mktemp rm rmdir sha256sum sh sleep stty tmux
)
declare -a binary_paths=()
for command in "${remote_commands[@]}"; do
    binary_paths+=("$(command -v "$command")")
done

copy_into_rootfs() {
    local source=$1
    mkdir -p "$rootfs$(dirname -- "$source")"
    cp -L -- "$source" "$rootfs$source"
}

for binary in "${binary_paths[@]}"; do
    copy_into_rootfs "$binary"
    while IFS= read -r library; do
        [[ -n $library ]] && copy_into_rootfs "$library"
    done < <(ldd "$binary" | awk '$2 == "=>" && $3 ~ /^\// { print $3 } $1 ~ /^\// { print $1 }')
done

# The forced command and tmux both require a stable shell path inside the copied rootfs.
cp -L -- "$(command -v sh)" "$rootfs/bin/sh"

cp -- /usr/share/terminfo/t/tmux-256color "$rootfs/usr/share/terminfo/t/"
cp -- /usr/share/terminfo/x/xterm-256color "$rootfs/usr/share/terminfo/x/"
cp -a -- /usr/lib/locale/C.utf8 "$rootfs/usr/lib/locale/"
printf '%s:x:%s:%s:isolated tmux probe:/tmp:/bin/sh\n' \
    "$USER" "$(id -u)" "$(id -g)" >"$rootfs/etc/passwd"
printf '%s:x:%s:%s\n' "$(id -gn)" "$(id -g)" "$USER" >"$rootfs/etc/group"
printf 'hosts: files dns\npasswd: files\ngroup: files\n' >"$rootfs/etc/nsswitch.conf"
printf '127.0.0.1 localhost\n' >"$rootfs/etc/hosts"

ssh-keygen -q -t ed25519 -N '' -f "$client_key"
ssh-keygen -q -t ed25519 -N '' -f "$host_key"
readonly rootfs_identity=$(sha256sum "$client_key.pub" | awk '{print $1}')
printf '%s\n' "$rootfs_identity" >"$rootfs/etc/machine-id"
printf '%s\n' "$rootfs_identity" >"$rootfs/etc/leyline-rootfs-id"
printf 'restrict,command="%s %s" %s\n' \
    "$ISOLATED_SHELL" "$rootfs" "$(<"$client_key.pub")" >"$authorized_keys"

port=$((40000 + $$ % 10000))
for _ in {1..20}; do
    printf '%s\n' \
        "ListenAddress 127.0.0.1" \
        "Port $port" \
        "HostKey $host_key" \
        "PidFile $sshd_pid_file" \
        "AuthorizedKeysFile $authorized_keys" \
        "PasswordAuthentication no" \
        "KbdInteractiveAuthentication no" \
        "UsePAM no" \
        "PermitRootLogin no" \
        "StrictModes no" \
        "LogLevel ERROR" >"$sshd_config"
    /usr/sbin/sshd -D -e -f "$sshd_config" 2>"$sshd_log" &
    sshd_pid=$!
    sleep 0.05
    if kill -0 "$sshd_pid" 2>/dev/null; then
        break
    fi
    wait "$sshd_pid" 2>/dev/null || true
    sshd_pid=
    port=$((port + 1))
done
[[ -n $sshd_pid ]] || fail "could not start isolated sshd: $(<"$sshd_log")"

printf '[127.0.0.1]:%s %s\n' "$port" "$(<"$host_key.pub")" >"$known_hosts"
readonly target="${USER}@127.0.0.1"

"$HARNESS" remote "$target" "$port" "$client_key" "$known_hosts" | tee "$r1_output"
"$HARNESS" local-remote "$target" "$port" "$client_key" "$known_hosts" | tee "$lr1_output"

readonly expected_identity=$(sha256sum "$rootfs/etc/leyline-rootfs-id" | awk '{print $1}')
readonly r1_identity=$(sed -n 's/^remote_rootfs_identity=//p' "$r1_output")
readonly lr1_identity=$(sed -n 's/^remote_rootfs_identity=//p' "$lr1_output")
readonly r1_custom_terminfo=$(sed -n 's/^remote_custom_terminfo=//p' "$r1_output")
[[ -n $r1_identity && $r1_identity == "$expected_identity" ]] \
    || fail 'R1 did not report the case-owned rootfs identity'
[[ $lr1_identity == "$r1_identity" ]] \
    || fail 'R1 and LR1 did not reach the same isolated rootfs'
[[ $r1_custom_terminfo == missing ]] \
    || fail 'isolated rootfs unexpectedly contained the Leyline prototype terminfo'
if [[ -r /etc/machine-id ]]; then
    readonly local_identity=$(sha256sum /etc/machine-id | awk '{print $1}')
    [[ $r1_identity != "$local_identity" ]] \
        || fail 'isolated rootfs reused the local machine identity'
fi

printf 'isolated_remote_identity=%s\n' "$r1_identity"
printf 'isolated_remote_result=passed\n'
