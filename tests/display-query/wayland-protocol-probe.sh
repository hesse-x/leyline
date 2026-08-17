#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    printf 'usage: %s RESULT FIXTURE TMUX_CONFIG\n' "$0" >&2
    exit 64
fi

readonly RESULT=$1
readonly FIXTURE=$2
readonly TMUX_CONFIG=$3
readonly CASE_ROOT=$(dirname -- "$RESULT")
readonly SOCKET="$CASE_ROOT/inner-tmux.sock"

cleanup() {
    if [[ -S $SOCKET ]]; then
        tmux -S "$SOCKET" kill-server >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

fail() {
    printf 'result=failed\ndetail=%s\n' "$1" >"$RESULT"
    exit 1
}

bash "$FIXTURE" "$CASE_ROOT/direct.txt" || fail 'direct PTY fixture failed'
rg -qx 'result=passed' "$CASE_ROOT/direct.txt" || fail 'direct PTY result missing'

printf -v pane_command 'exec bash %q %q tmux' "$FIXTURE" "$CASE_ROOT/tmux.txt"
TERM=xterm-256color COLORTERM=truecolor \
    tmux -S "$SOCKET" -f "$TMUX_CONFIG" new-session -x 80 -y 24 -s display-query \
        "$pane_command" || fail 'tmux PTY fixture failed'
rg -qx 'result=passed' "$CASE_ROOT/tmux.txt" || fail 'tmux PTY result missing'

{
    printf 'schema_version=1\n'
    printf 'direct_protocol=pass\n'
    printf 'tmux_protocol=pass\n'
    printf 'tmux_version=%s\n' "$(tmux -V)"
    printf 'result=passed\n'
} >"$RESULT"
