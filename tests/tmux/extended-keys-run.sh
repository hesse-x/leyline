#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly BYTE_PROBE="$SCRIPT_DIR/byte-probe.sh"
readonly TIMEOUT_SECONDS=5

fail() {
    printf 'extended_keys_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in rg script tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-extkeys.XXXXXXXX")
readonly case_root
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
    [[ $(basename -- "$case_root") == leyline-tmux-extkeys.* ]] || return 1
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
for mode in off on always; do
    socket="$case_root/$mode.sock"
    config="$case_root/$mode.conf"
    bytes="$case_root/$mode.bytes"
    ready="$case_root/$mode.ready"
    typescript="$case_root/$mode.typescript"
    owned_sockets+=("$socket")
    printf '%s\n' \
        'set -g default-terminal tmux-256color' \
        "set -g extended-keys $mode" >"$config"

    printf -v pane_command '%q %q %q %q' "$BYTE_PROBE" "$bytes" 3 "$ready"
    TERM=xterm-256color COLORTERM=truecolor \
        tmux -S "$socket" -f "$config" new-session -d -x 80 -y 24 -s extkeys "$pane_command"

    deadline=$((SECONDS + TIMEOUT_SECONDS))
    while [[ ! -s $ready ]]; do
        (( SECONDS < deadline )) || fail "$mode byte probe did not become ready"
        sleep 0.05
    done
    printf -v attach_command 'TERM=xterm-256color COLORTERM=truecolor tmux -S %q attach-session -t extkeys' \
        "$socket"
    { sleep 0.3; printf '\033OP'; sleep 0.2; printf '\002d'; } \
        | timeout --foreground --kill-after=1s 3s script -qefc "$attach_command" "$typescript" \
            >/dev/null \
        || fail "$mode client did not detach cleanly"

    [[ -s $bytes ]] || fail "$mode did not deliver the F1 fixture"
    [[ $(<"$bytes") == 1b4f50 ]] || fail "$mode changed the traditional F1 fixture"
    if rg --text --quiet --fixed-strings $'\033[>4;1m' "$typescript"; then
        fail "$mode negotiated modifyOtherKeys without an extkeys declaration"
    fi
    printf 'extended_keys_%s=pass\n' "$mode"
    printf 'extended_keys_%s_f1=1b4f50\n' "$mode"
    printf 'extended_keys_%s_negotiated=no\n' "$mode"
done

printf 'extended_keys_result=passed\n'
