#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
readonly PROBE_BIN=${1:-$REPO_ROOT/target/debug/leyline-probe}
cd "$REPO_ROOT"

[[ -x $PROBE_BIN ]] || cargo build -p leyline-probe
"$PROBE_BIN" scene --json
cargo test -p leyline --lib \
    frame_composer::tests::underline_styles_produce_bounded_physical_primitives
cargo test -p leyline --lib \
    terminal::core::tests::display_protocol_state_is_isolated_between_terminal_cores
printf 'cursor_underline_result=passed\n'

