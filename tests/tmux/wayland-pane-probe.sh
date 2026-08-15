#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s OUTPUT\n' "$0" >&2
    exit 64
fi

readonly output=$1
umask 077
{
    printf 'pane_TERM=%s\n' "${TERM-}"
    printf 'pane_COLORTERM=%s\n' "${COLORTERM-}"
    printf 'pane_size='
    stty size
    printf 'pane_tty=%s\n' "$(test -t 0 && printf yes || printf no)"
    tmux display-message -p \
        'client_termname=#{client_termname}
client_size=#{client_width}x#{client_height}
pane_size_tmux=#{pane_width}x#{pane_height}
client_attached=#{session_attached}'
} >"$output"

# Keep the client attached long enough for the compositor to present the scene.
sleep 1
