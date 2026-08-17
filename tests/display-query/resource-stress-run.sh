#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
readonly DURATION_SECONDS=${DISPLAY_QUERY_STRESS_SECONDS:-10}
readonly OUTPUT=${1:-$REPO_ROOT/tests/display-query/results/resource-stress-current.txt}

fail() {
    printf 'resource_stress_result=failed detail=%q\n' "$1" >&2
    exit 1
}

[[ $DURATION_SECONDS =~ ^[1-9][0-9]*$ ]] || fail 'DISPLAY_QUERY_STRESS_SECONDS must be a positive integer'
mkdir -p -- "$(dirname -- "$OUTPUT")"
cd -- "$REPO_ROOT"

started=$(date +%s)
deadline=$((started + DURATION_SECONDS))
iterations=0
while (( $(date +%s) < deadline )); do
    cargo test -q -p leyline --lib \
        terminal::core::tests::synchronized_updates_publish_only_when_committed \
        -- --exact
    cargo test -q --manifest-path third_party/vte/Cargo.toml --features ansi \
        ansi::tests::sync_commit_counters_and_discard_are_bounded \
        -- --exact
    iterations=$((iterations + 1))
done
ended=$(date +%s)

{
    printf 'schema_version=1\n'
    printf 'scope=repeatable_sync_limit_regression\n'
    printf 'requested_seconds=%s\n' "$DURATION_SECONDS"
    printf 'elapsed_seconds=%s\n' "$((ended - started))"
    printf 'iterations=%s\n' "$iterations"
    printf 'sync_buffer_limit_bytes=2097152\n'
    printf 'retained_capacity_limit_bytes=65536\n'
    printf 'resource_stress_result=passed\n'
    printf 'measurement_note=This gate exercises asserted internal limits; use the 30-minute manual checklist for live-process RSS and FD plateaus.\n'
} | tee "$OUTPUT"
