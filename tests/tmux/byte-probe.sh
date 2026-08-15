#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 || ! $2 =~ ^[1-9][0-9]*$ ]]; then
    printf 'usage: %s OUTPUT BYTE_COUNT READY\n' "$0" >&2
    exit 64
fi

output=$1
count=$2
ready=$3
umask 077
stty raw -echo
printf 'ready\n' >"$ready"
od -An -N "$count" -tx1 -v | tr -d ' \n' >"$output"
exec sleep 30
