#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly BYTE_PROBE="$SCRIPT_DIR/byte-probe.sh"
readonly CUSTOM_TERM=leyline-256color
readonly MISSING_TERM=leyline-test-missing
readonly TIMEOUT_SECONDS=5

fail() {
    printf 'terminfo_prototype_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in infocmp rg script tic tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-terminfo.XXXXXXXX")
readonly case_root
readonly terminfo_source="$REPO_ROOT/terminfo/leyline.terminfo"
readonly terminfo_db="$case_root/terminfo"
readonly empty_db="$case_root/empty-terminfo"
declare -a owned_sockets=()

cleanup() {
    local socket
    local status=0
    for socket in "${owned_sockets[@]}"; do
        if [[ -S $socket ]]; then
            tmux -S "$socket" kill-server >/dev/null 2>&1 || status=1
        fi
    done
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-terminfo.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        printf 'cleanup_verdict=pass\n'
    else
        printf 'cleanup_verdict=fail\n' >&2
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
mkdir -p "$terminfo_db" "$empty_db"
if rg -q '^[[:space:]]*use=' "$terminfo_source"; then
    fail 'canonical terminfo must be standalone'
fi
tic -x -o "$terminfo_db" "$terminfo_source"
compiled=$(TERMINFO="$terminfo_db" infocmp -x "$CUSTOM_TERM")
rg -q 'RGB' <<<"$compiled" || fail 'custom terminfo did not declare RGB'
rg -q 'kf1=\\EOP' <<<"$compiled" || fail 'custom terminfo changed the F1 contract'

socket="$case_root/custom.sock"
config="$case_root/custom.conf"
bytes="$case_root/custom.bytes"
ready="$case_root/custom.ready"
typescript="$case_root/custom.typescript"
custom_info="$case_root/custom.info"
owned_sockets+=("$socket")
printf '%s\n' 'set -g default-terminal tmux-256color' >"$config"
printf -v pane_command '%q %q %q %q' "$BYTE_PROBE" "$bytes" 3 "$ready"
TERM=xterm-256color COLORTERM=truecolor \
    tmux -S "$socket" -f "$config" new-session -d -x 80 -y 24 -s custom "$pane_command"

deadline=$((SECONDS + TIMEOUT_SECONDS))
while [[ ! -s $ready ]]; do
    (( SECONDS < deadline )) || fail 'custom terminfo byte probe did not become ready'
    sleep 0.05
done
printf -v attach_command 'TERM=%q TERMINFO=%q TERMINFO_DIRS=%q COLORTERM=truecolor tmux -S %q attach-session -t custom' \
    "$CUSTOM_TERM" "$terminfo_db" "$terminfo_db" "$socket"
{ sleep 0.3; printf '\033OP'; sleep 0.2; printf '\002d'; } \
    | timeout --foreground --kill-after=1s 3s script -qefc "$attach_command" "$typescript" \
        >/dev/null &
attach_pid=$!

deadline=$((SECONDS + TIMEOUT_SECONDS))
client_term=
while [[ -z $client_term ]]; do
    client_term=$(tmux -S "$socket" list-clients -F '#{client_termname}' 2>/dev/null || true)
    (( SECONDS < deadline )) || fail 'custom terminfo client did not attach'
    [[ -n $client_term ]] || sleep 0.05
done
tmux -S "$socket" info >"$custom_info"
wait "$attach_pid" || fail 'custom terminfo client did not detach cleanly'
[[ $client_term == "$CUSTOM_TERM" ]] || fail 'tmux observed the wrong custom terminal name'
[[ $(<"$bytes") == 1b4f50 ]] || fail 'custom terminfo changed the F1 byte fixture'
rg -q 'RGB:.*true' "$custom_info" || fail 'tmux did not consume the custom RGB capability'
printf 'custom_term_client_termname=%s\n' "$client_term"
printf 'custom_term_rgb=declared\n'
printf 'custom_term_tmux_rgb=detected\n'
printf 'custom_term_f1=1b4f50\n'

missing_socket="$case_root/missing.sock"
missing_config="$case_root/missing.conf"
missing_typescript="$case_root/missing.typescript"
owned_sockets+=("$missing_socket")
printf '%s\n' 'set -g default-terminal tmux-256color' >"$missing_config"
TERM=xterm-256color TERMINFO="$empty_db" TERMINFO_DIRS="$empty_db" \
    tmux -S "$missing_socket" -f "$missing_config" \
    new-session -d -x 80 -y 24 -s missing 'exec sleep 30'
printf -v missing_command 'TERM=%q TERMINFO=%q TERMINFO_DIRS=%q tmux -S %q attach-session -t missing' \
    "$MISSING_TERM" "$empty_db" "$empty_db" "$missing_socket"
set +e
timeout --foreground 3s script -qefc "$missing_command" "$missing_typescript" </dev/null >/dev/null
missing_status=$?
set -e
(( missing_status != 0 )) || fail 'missing custom terminfo unexpectedly attached'
rg --text -qi 'missing or unsuitable terminal|unknown terminal' "$missing_typescript" \
    || fail 'missing custom terminfo did not produce a stable diagnostic'
tmux -S "$missing_socket" has-session -t missing \
    || fail 'failed custom client attachment terminated the tmux server'
printf 'missing_custom_term=explicit-failure\n'
printf 'missing_custom_term_server_retained=pass\n'
printf 'terminfo_prototype_result=passed\n'
