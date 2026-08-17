#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
    printf 'usage: %s direct|nested-tmux OUTPUT FIXTURE TMUX_CONFIG\n' "$0" >&2
    exit 64
fi

readonly MODE=$1
readonly OUTPUT=$2
readonly FIXTURE=$3
readonly TMUX_CONFIG=$4
readonly SOCKET_NAME="leyline-display-query-ssh-$$"

case "$MODE" in
    direct)
        exec bash "$FIXTURE" "$OUTPUT" direct
        ;;
    nested-tmux)
        cleanup() {
            tmux -L "$SOCKET_NAME" kill-server >/dev/null 2>&1 || true
        }
        trap cleanup EXIT INT TERM
        printf -v pane_command 'exec bash %q %q tmux' "$FIXTURE" "$OUTPUT"
        TERM=xterm-256color COLORTERM=truecolor \
            tmux -L "$SOCKET_NAME" -f "$TMUX_CONFIG" \
                new-session -x 80 -y 24 -s display-query-ssh "$pane_command"
        ;;
    *)
        printf 'unknown mode: %s\n' "$MODE" >&2
        exit 64
        ;;
esac
