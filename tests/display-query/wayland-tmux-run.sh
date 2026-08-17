#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly LEYLINE_BIN=${1:-$REPO_ROOT/target/debug/leyline}
readonly REQUESTED_EVIDENCE=${2-}
readonly TIMEOUT_SECONDS=20

fail() {
    printf 'wayland_display_query_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in date rg sha256sum tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ -x $LEYLINE_BIN ]] || fail "Leyline binary is not executable: $LEYLINE_BIN"
[[ ${XDG_SESSION_TYPE-} == wayland && -n ${WAYLAND_DISPLAY-} ]] \
    || fail 'a live Wayland session is required'

owned_evidence=false
if [[ -n $REQUESTED_EVIDENCE ]]; then
    mkdir -p -- "$REQUESTED_EVIDENCE"
    evidence=$(cd -- "$REQUESTED_EVIDENCE" && pwd -P)
else
    evidence=$(mktemp -d "${TMPDIR:-/tmp}/leyline-display-query.XXXXXXXX")
    owned_evidence=true
fi
readonly evidence
readonly config_root="$evidence/config"
readonly config="$config_root/leyline/config.toml"
readonly tmux_config="$evidence/tmux.conf"
readonly result="$evidence/result.txt"
readonly log="$evidence/leyline.log"

cleanup() {
    if [[ $owned_evidence == true && ${KEEP_EVIDENCE-0} != 1 ]]; then
        rm -rf -- "$evidence"
    fi
}
trap cleanup EXIT INT TERM

mkdir -p -- "$(dirname -- "$config")"
printf '%s\n' \
    '[colors]' \
    'foreground = "#123456"' \
    'background = "#0a1b2ccc"' \
    '[behavior]' \
    'hold_after_exit = false' >"$config"
printf '%s\n' \
    'set -g default-terminal tmux-256color' \
    'set -g status off' \
    'set -g allow-passthrough on' >"$tmux_config"

set +e
XDG_CONFIG_HOME="$config_root" \
    timeout --foreground --kill-after=3s "${TIMEOUT_SECONDS}s" \
    "$LEYLINE_BIN" -vv -e bash "$SCRIPT_DIR/wayland-protocol-probe.sh" \
        "$result" "$SCRIPT_DIR/protocol-fixture.sh" "$tmux_config" >"$log" 2>&1
run_status=$?
set -e
if (( run_status != 0 )); then
    sed -n '1,200p' "$log" >&2
    [[ ! -f $evidence/direct.txt ]] || sed -n '1,160p' "$evidence/direct.txt" >&2
    [[ ! -f $evidence/tmux.txt ]] || sed -n '1,160p' "$evidence/tmux.txt" >&2
    fail "Leyline protocol fixture failed: status=$run_status"
fi
rg -qx 'result=passed' "$result" || fail 'aggregate result missing'

{
    printf 'case_id=DQ-WL-%s-%s\n' "$(date -u +%Y%m%dT%H%M%SZ)" "$$"
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'terminal_under_test=Leyline\n'
    printf 'display_protocol=wayland\n'
    printf 'tmux_version=%s\n' "$(tmux -V)"
    printf 'binary_sha256=%s\n' "$(sha256sum "$LEYLINE_BIN" | awk '{print $1}')"
    printf 'direct_result_sha256=%s\n' "$(sha256sum "$evidence/direct.txt" | awk '{print $1}')"
    printf 'tmux_result_sha256=%s\n' "$(sha256sum "$evidence/tmux.txt" | awk '{print $1}')"
    printf 'wayland_display_query_result=passed\n'
} | tee "$evidence/metadata.txt"
sed -n '1,160p' "$evidence/direct.txt"
sed -n '1,160p' "$evidence/tmux.txt"
if [[ $owned_evidence == true ]]; then
    printf 'evidence_retention=temporary set_KEEP_EVIDENCE=1_to_preserve\n'
else
    printf 'evidence_path=%s\n' "$evidence"
fi

