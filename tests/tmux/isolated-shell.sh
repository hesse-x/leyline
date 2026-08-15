#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s ROOTFS\n' "$0" >&2
    exit 64
fi

readonly rootfs=$(cd -- "$1" && pwd -P)
[[ $(basename -- "$rootfs") == rootfs ]] || {
    printf 'isolated shell refused unexpected rootfs: %s\n' "$rootfs" >&2
    exit 64
}
[[ $(basename -- "$(dirname -- "$rootfs")") == leyline-tmux-remote-env.* ]] || {
    printf 'isolated shell refused unowned rootfs: %s\n' "$rootfs" >&2
    exit 64
}
[[ ${SSH_ORIGINAL_COMMAND-} == 'sh -s' ]] || {
    printf 'isolated shell refused command: %s\n' "${SSH_ORIGINAL_COMMAND-}" >&2
    exit 64
}

exec bwrap \
    --die-with-parent \
    --unshare-all \
    --share-net \
    --bind "$rootfs" / \
    --dev /dev \
    --proc /proc \
    --tmpfs /tmp \
    --setenv HOME /tmp \
    --setenv LC_ALL C.utf8 \
    --chdir /tmp \
    /bin/sh -s
