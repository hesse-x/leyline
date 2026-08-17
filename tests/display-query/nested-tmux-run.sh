#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec bash "$SCRIPT_DIR/loopback-ssh-run.sh" nested-tmux "${1-}" "${2-}"
