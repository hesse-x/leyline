#!/usr/bin/env bash
set -euo pipefail

readonly TIMEOUT_SECONDS=15
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly LEYLINE_BIN=${1:-$REPO_ROOT/target/debug/leyline}
readonly PANE_PROBE="$SCRIPT_DIR/wayland-pane-probe.sh"

fail() {
    printf 'result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in tmux timeout rg; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ -x $LEYLINE_BIN ]] || fail "Leyline binary is not executable: $LEYLINE_BIN"
[[ ${XDG_SESSION_TYPE-} == wayland ]] || fail 'not running in a Wayland session'
[[ -n ${WAYLAND_DISPLAY-} ]] || fail 'WAYLAND_DISPLAY is unset'

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-wayland.XXXXXXXX")
readonly case_root
readonly socket="$case_root/tmux.sock"
readonly config="$case_root/tmux.conf"
readonly output="$case_root/pane.txt"

cleanup() {
    local status=0
    if [[ -S $socket ]]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-wayland.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        printf 'cleanup_verdict=pass\n'
    else
        printf 'cleanup_verdict=fail\n' >&2
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
printf '%s\n' 'set -g default-terminal tmux-256color' >"$config"

set +e
timeout --foreground --kill-after=3s "${TIMEOUT_SECONDS}s" "$LEYLINE_BIN" -e \
    tmux -S "$socket" -f "$config" new-session -s leyline-wayland \
    "$PANE_PROBE $output"
run_status=$?
set -e

if [[ -s $output ]]; then
    sed -n '/^pane_TERM=/p;/^pane_COLORTERM=/p;/^pane_size=/p;/^client_termname=/p;/^client_size=/p;/^pane_size_tmux=/p;/^client_attached=/p' "$output"
fi
(( run_status == 0 )) || fail "Leyline/tmux foreground client failed or timed out: status=$run_status"

[[ -s $output ]] || fail 'pane probe produced no output'
rg -qx 'pane_TERM=tmux-256color' "$output" || fail 'unexpected pane TERM'
rg -qx 'pane_tty=yes' "$output" || fail 'pane did not receive a tty'
rg -qx 'client_termname=leyline-256color' "$output" || fail 'tmux saw the wrong Leyline TERM'
rg -qx 'client_attached=1' "$output" || fail 'tmux client was not attached'
rg -q '^client_size=[1-9][0-9]*x[1-9][0-9]*$' "$output" || fail 'invalid client size'
rg -q '^pane_size_tmux=[1-9][0-9]*x[1-9][0-9]*$' "$output" || fail 'invalid pane size'

printf 'case_id=S1-L1-WL-%s-%s\n' "$(date -u +%Y%m%dT%H%M%SZ)" "$$"
printf 'topology_id=L1\nterminal_under_test=Leyline\n'
printf 'result=passed\n'
