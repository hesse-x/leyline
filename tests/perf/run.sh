#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 {idle-blink|throughput|unicode|all} [output-directory]" >&2
    exit 2
}

workload=${1:-}
output_dir=${2:-target/perf}
case "$workload" in
    idle-blink|throughput|unicode|all) ;;
    *) usage ;;
esac

mkdir -p "$output_dir"

run_case() {
    local name=$1
    local size=$2
    local context=$3
    local columns=${size%x*}
    local lines=${size#*x}
    local output="$output_dir/${name}-${size}-${context}.json"
    LEYLINE_PERF_OUTPUT="$output" \
    LEYLINE_PERF_WORKLOAD="${name}:${size}:${context}" \
    LEYLINE_PERF_BIDI=1 \
    LEYLINE_PERF_LIGATURES=1 \
    LEYLINE_PERF_COLOR_GLYPHS=1 \
        cargo run --quiet -p leyline --example perf_workload -- "$name" "$columns" "$lines"
    python3 -m json.tool "$output" >/dev/null
}

run_workload() {
    local name=$1
    for size in 80x24 240x80; do
        if [[ "$name" == throughput ]]; then
            for context in current-tab background-tab multi-window; do
                run_case "$name" "$size" "$context"
            done
        else
            run_case "$name" "$size" single-window
        fi
    done
}

if [[ "$workload" == all ]]; then
    run_workload idle-blink
    run_workload throughput
    run_workload unicode
else
    run_workload "$workload"
fi
