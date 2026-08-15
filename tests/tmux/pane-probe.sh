#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s OUTPUT\n' "$0" >&2
    exit 64
fi

output=$1
umask 077
{
    printf 'term=%s\n' "${TERM-}"
    printf 'colorterm=%s\n' "${COLORTERM-}"
    printf 'size='
    stty size
    printf 'tty=%s\n' "$(test -t 0 && printf yes || printf no)"
} >"$output"

# Keep the pane alive long enough for the harness to inspect tmux state.
exec sleep 30
