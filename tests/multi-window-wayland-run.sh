#!/usr/bin/env bash
set -euo pipefail

if [[ -z ${WAYLAND_DISPLAY:-} || -z ${XDG_RUNTIME_DIR:-} ]]; then
    printf 'multi-window Wayland gate requires WAYLAND_DISPLAY and XDG_RUNTIME_DIR\n' >&2
    exit 2
fi

export LEYLINE_RUN_WAYLAND_INTEGRATION=1
cargo test -p leyline-gfx --test multi_window_wayland --locked -- --ignored --exact \
    two_windows_share_host_and_reject_destroyed_surface_key
