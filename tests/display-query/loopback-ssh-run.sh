#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly MODE=${1:-direct}
readonly LEYLINE_BIN=${2:-$REPO_ROOT/target/debug/leyline}
readonly REQUESTED_EVIDENCE=${3-}
readonly TIMEOUT_SECONDS=25

fail() {
    printf 'loopback_ssh_display_query_result=failed detail=%q\n' "$1" >&2
    exit 1
}

[[ $MODE == direct || $MODE == nested-tmux ]] || fail "invalid mode: $MODE"
for command in date sha256sum ssh sshd ssh-keygen timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ -x $LEYLINE_BIN ]] || fail "Leyline binary is not executable: $LEYLINE_BIN"
[[ ${XDG_SESSION_TYPE-} == wayland && -n ${WAYLAND_DISPLAY-} ]] \
    || fail 'a live Wayland session is required'

runtime=$(mktemp -d "${TMPDIR:-/tmp}/leyline-display-query-ssh.XXXXXXXX")
readonly runtime
if [[ -n $REQUESTED_EVIDENCE ]]; then
    mkdir -p -- "$REQUESTED_EVIDENCE"
    evidence=$(cd -- "$REQUESTED_EVIDENCE" && pwd -P)
else
    evidence="$runtime/evidence"
    mkdir -p -- "$evidence"
fi
readonly evidence
readonly client_key="$runtime/client"
readonly host_key="$runtime/host"
readonly authorized_keys="$runtime/authorized_keys"
readonly known_hosts="$runtime/known_hosts"
readonly sshd_config="$runtime/sshd_config"
readonly sshd_log="$evidence/sshd.log"
readonly sshd_pid_file="$runtime/sshd.pid"
readonly result="$evidence/$MODE.txt"
readonly tmux_config="$runtime/tmux.conf"
readonly config_root="$runtime/config"
readonly config="$config_root/leyline/config.toml"
readonly leyline_log="$evidence/leyline-$MODE.log"
sshd_pid=

cleanup() {
    if [[ -n $sshd_pid ]] && kill -0 "$sshd_pid" 2>/dev/null; then
        kill "$sshd_pid" 2>/dev/null || true
        wait "$sshd_pid" 2>/dev/null || true
    fi
    if [[ $(dirname -- "$runtime") == "${TMPDIR:-/tmp}" && $(basename -- "$runtime") == leyline-display-query-ssh.* ]]; then
        rm -rf -- "$runtime"
    fi
}
trap cleanup EXIT INT TERM

umask 077
mkdir -p -- "$(dirname -- "$config")"
printf '%s\n' \
    '[colors]' \
    'foreground = "#123456"' \
    'background = "#0a1b2ccc"' \
    '[behavior]' \
    'hold_after_exit = false' >"$config"
ssh-keygen -q -t ed25519 -N '' -f "$client_key"
ssh-keygen -q -t ed25519 -N '' -f "$host_key"
cp -- "$client_key.pub" "$authorized_keys"
printf '%s\n' 'set -g default-terminal tmux-256color' 'set -g status off' >"$tmux_config"

port=$((30000 + $$ % 10000))
for _ in {1..20}; do
    printf '%s\n' \
        'ListenAddress 127.0.0.1' \
        "Port $port" \
        "HostKey $host_key" \
        "PidFile $sshd_pid_file" \
        "AuthorizedKeysFile $authorized_keys" \
        'PasswordAuthentication no' \
        'KbdInteractiveAuthentication no' \
        'UsePAM no' \
        'PermitRootLogin no' \
        'StrictModes no' \
        'LogLevel ERROR' >"$sshd_config"
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
readonly -a ssh_options=(
    -F /dev/null -p "$port" -tt
    -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none
    -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$known_hosts"
    -i "$client_key"
)

deadline=$((SECONDS + 10))
while ! ssh "${ssh_options[@]}" -T "$target" true 2>/dev/null; do
    (( SECONDS < deadline )) || fail "isolated sshd did not become ready: $(<"$sshd_log")"
    sleep 0.05
done

set +e
timeout --foreground --kill-after=3s "${TIMEOUT_SECONDS}s" \
    env XDG_CONFIG_HOME="$config_root" "$LEYLINE_BIN" -vv -e ssh "${ssh_options[@]}" "$target" \
        bash "$SCRIPT_DIR/ssh-protocol-probe.sh" "$MODE" "$result" \
        "$SCRIPT_DIR/protocol-fixture.sh" "$tmux_config" >"$leyline_log" 2>&1
run_status=$?
set -e
if (( run_status != 0 )); then
    sed -n '1,200p' "$leyline_log" >&2
    [[ ! -f $result ]] || sed -n '1,160p' "$result" >&2
    fail "Leyline/SSH fixture failed: status=$run_status"
fi
rg -qx 'result=passed' "$result" || fail 'protocol result missing'

{
    printf 'case_id=DQ-SSH-%s-%s-%s\n' "${MODE^^}" "$(date -u +%Y%m%dT%H%M%SZ)" "$$"
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'target=%s\n' "$target"
    printf 'address=127.0.0.1\n'
    printf 'topology=%s\n' "$MODE"
    printf 'binary_sha256=%s\n' "$(sha256sum "$LEYLINE_BIN" | awk '{print $1}')"
    printf 'result_sha256=%s\n' "$(sha256sum "$result" | awk '{print $1}')"
    printf 'loopback_ssh_display_query_result=passed\n'
} | tee "$evidence/metadata-$MODE.txt"
sed -n '1,160p' "$result"
[[ -z $REQUESTED_EVIDENCE ]] || printf 'evidence_path=%s\n' "$evidence"
