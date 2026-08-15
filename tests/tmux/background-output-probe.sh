#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s READY\n' "$0" >&2
    exit 64
fi

readonly ready=$1

# Emit enough data to exercise a detached pane without retaining terminal content.
dd if=/dev/zero bs=65536 count=16 2>/dev/null | tr '\000' x
printf '\nLEYLINE_TMUX_BACKGROUND_DONE\n'
printf 'ready\n' >"$ready"
exec sleep 30
