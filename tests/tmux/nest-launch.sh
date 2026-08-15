#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
    printf 'usage: %s LEVEL DEPTH CASE_ROOT CONFIG SELF\n' "$0" >&2
    exit 64
fi

level=$1
depth=$2
case_root=$3
config=$4
self=$5
output="$case_root/level-$level.txt"

umask 077
{
    printf 'term=%s\n' "${TERM-}"
    printf 'colorterm=%s\n' "${COLORTERM-}"
    printf 'size='
    stty size
    printf 'tty=%s\n' "$(test -t 0 && printf yes || printf no)"
} >"$output"

if (( level < depth )); then
    next=$((level + 1))
    socket="$case_root/level-$next.sock"
    printf -v command '%q %q %q %q %q %q' \
        "$self" "$next" "$depth" "$case_root" "$config" "$self"
    # Attach the next client to this pane so input crosses every tmux layer.
    exec env -u TMUX tmux -S "$socket" -f "$config" \
        new-session -s "level-$next" "$command"
fi

exec sleep 30
