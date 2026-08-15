#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s LABEL\n' "$0" >&2
    exit 64
fi

case $1 in
    pane-1)
        printf '\033[?1049h\033[2J\033[H\033[?1000h\033[?1006h\033[?1004h\033[?2004hpane-1 e\314\201 \344\270\255'
        sleep 0.5
        printf '\033]52;c;Zm9v\007'
        ;;
    pane-2)
        printf '\033[2J\033[Hpane-2'
        ;;
    *)
        exit 64
        ;;
esac

exec sleep 30
