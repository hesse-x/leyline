#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
cd -- "$REPO_ROOT"

cargo test -p leyline --lib \
    terminal::core::tests::display_protocol_state_is_isolated_between_terminal_cores \
    -- --exact
printf 'tab_isolation_result=passed\n'
