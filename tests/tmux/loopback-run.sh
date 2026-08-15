#!/usr/bin/env bash
set -euo pipefail

readonly TIMEOUT_SECONDS=10
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly HARNESS="$SCRIPT_DIR/harness.sh"
readonly FAILURE_MATRIX="$SCRIPT_DIR/ssh-failure-matrix.sh"

fail() {
    printf 'loopback_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in ssh sshd ssh-keygen timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-sshd.XXXXXXXX")
readonly case_root
readonly client_key="$case_root/client"
readonly host_key="$case_root/host"
readonly authorized_keys="$case_root/authorized_keys"
readonly known_hosts="$case_root/known_hosts"
readonly sshd_config="$case_root/sshd_config"
readonly sshd_log="$case_root/sshd.log"
readonly sshd_pid_file="$case_root/sshd.pid"
sshd_pid=

cleanup() {
    local status=0
    if [[ -n $sshd_pid ]] && kill -0 "$sshd_pid" 2>/dev/null; then
        kill "$sshd_pid" 2>/dev/null || status=1
        wait "$sshd_pid" 2>/dev/null || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-sshd.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        printf 'loopback_cleanup=pass\n'
    else
        printf 'loopback_cleanup=fail\n' >&2
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
ssh-keygen -q -t ed25519 -N '' -f "$client_key"
ssh-keygen -q -t ed25519 -N '' -f "$host_key"
cp -- "$client_key.pub" "$authorized_keys"

# Try a small deterministic range of unprivileged ports without touching the system sshd.
port=$((20000 + $$ % 20000))
for _ in {1..20}; do
    cat >"$sshd_config" <<EOF
ListenAddress 127.0.0.1
Port $port
HostKey $host_key
PidFile $sshd_pid_file
AuthorizedKeysFile $authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin no
StrictModes no
LogLevel ERROR
EOF
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

deadline=$((SECONDS + TIMEOUT_SECONDS))
while ! ssh -F /dev/null -p "$port" \
    -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
    -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$known_hosts" \
    -i "$client_key" "${USER}@127.0.0.1" true 2>/dev/null; do
    (( SECONDS < deadline )) || fail "isolated sshd did not become ready: $(<"$sshd_log")"
    sleep 0.05
done

"$HARNESS" loopback "${USER}@127.0.0.1" "$port" "$client_key" "$known_hosts"
bash "$FAILURE_MATRIX" "${USER}@127.0.0.1" "$port" "$client_key" "$known_hosts"
printf 'loopback_result=passed\n'
