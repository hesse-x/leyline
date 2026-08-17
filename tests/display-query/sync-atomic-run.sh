#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
cd "$REPO_ROOT"

cargo test -p leyline --lib \
    terminal::core::tests::synchronized_updates_publish_only_when_committed
cargo test --manifest-path third_party/vte/Cargo.toml --features ansi \
    ansi::tests::sync_commit_counters_and_discard_are_bounded
printf 'sync_atomic_result=passed\n'

