#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
cd "$REPO_ROOT"

cargo test -p leyline --lib \
    terminal::core::tests::standard_device_queries_emit_bounded_xterm_replies
cargo test -p leyline --lib \
    terminal::core::tests::approved_queries_are_typed_and_color_mutations_remain_blocked
cargo test -p leyline --lib \
    ui_runtime::tests::terminal_queries_use_config_colors_and_cell_grid_pixels
printf 'exact_reply_result=passed\n'

