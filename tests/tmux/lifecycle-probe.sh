#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    printf 'usage: %s READY SAMPLES\n' "$0" >&2
    exit 64
fi

readonly ready=$1
readonly samples=$2

record_size() {
    local size
    size=$(stty size)
    printf 'size=%s\n' "$size" >>"$samples"
}

umask 077
: >"$samples"
record_size
printf 'ready\n' >"$ready"

# Sample the kernel view independently from tmux's format state.
while true; do
    record_size
    sleep 0.05
done
