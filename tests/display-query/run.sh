#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly MODE=${1:---headless}
readonly LEYLINE_BIN=${2:-$REPO_ROOT/target/debug/leyline}
readonly CASE_ID=$(date -u +%Y%m%dT%H%M%SZ)
readonly EVIDENCE_ROOT="$SCRIPT_DIR/results/$CASE_ID"

case "$MODE" in
    --headless|--full) ;;
    *) printf 'usage: %s [--headless|--full] [LEYLINE_BIN]\n' "$0" >&2; exit 64 ;;
esac

mkdir -p -- "$EVIDENCE_ROOT"
bash "$SCRIPT_DIR/exact-reply-run.sh"
bash "$SCRIPT_DIR/sync-atomic-run.sh"
bash "$SCRIPT_DIR/cursor-underline-run.sh"
bash "$SCRIPT_DIR/tab-isolation-run.sh"
bash "$SCRIPT_DIR/resource-stress-run.sh" "$EVIDENCE_ROOT/resource-stress.txt"

if [[ $MODE == --full ]]; then
    bash "$SCRIPT_DIR/wayland-tmux-run.sh" "$LEYLINE_BIN" "$EVIDENCE_ROOT/wayland-tmux"
    bash "$SCRIPT_DIR/loopback-ssh-run.sh" direct "$LEYLINE_BIN" "$EVIDENCE_ROOT/ssh-direct"
    bash "$SCRIPT_DIR/loopback-ssh-run.sh" nested-tmux "$LEYLINE_BIN" "$EVIDENCE_ROOT/ssh-nested-tmux"
fi

printf 'display_query_suite_result=passed mode=%s evidence_path=%s\n' "$MODE" "$EVIDENCE_ROOT"
