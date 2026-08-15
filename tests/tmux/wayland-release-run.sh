#!/usr/bin/env bash
set -euo pipefail

readonly TIMEOUT_SECONDS=30
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly LEYLINE_BIN=${1:-$REPO_ROOT/target/release/leyline}
readonly INNER_PROBE="$SCRIPT_DIR/wayland-release-probe.sh"
readonly SUPERVISOR="$SCRIPT_DIR/wayland-release-supervisor.sh"

fail() {
    printf 'wayland_release_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in rg tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ -x $LEYLINE_BIN ]] || fail "Leyline binary is not executable: $LEYLINE_BIN"
[[ -x $INNER_PROBE ]] || fail "inner probe is not executable: $INNER_PROBE"
[[ -x $SUPERVISOR ]] || fail "supervisor is not executable: $SUPERVISOR"
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || fail 'not running in a Wayland session'
[[ -n ${WAYLAND_DISPLAY:-} ]] || fail 'WAYLAND_DISPLAY is unset'

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-wl-runner.XXXXXXXX")
readonly case_root
readonly socket="$case_root/outer-tmux.sock"
readonly config="$case_root/outer-tmux.conf"
readonly result="$case_root/result.txt"
readonly exit_status_file="$case_root/exit-status.txt"
readonly log="$case_root/leyline.log"

cleanup() {
    local status=0
    if [[ -S $socket ]]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-wl-runner.* ]] || return 1
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
printf '%s\n' 'set -g remain-on-exit on' >"$config"

printf -v runner_command '%q %q %q %q %q %q %q %q' \
    "$SUPERVISOR" "$REPO_ROOT" "$LEYLINE_BIN" "$INNER_PROBE" "$result" "$log" \
    "$exit_status_file" "$TIMEOUT_SECONDS"
tmux -S "$socket" -f "$config" new-session -d -s leyline-runner "$runner_command"

deadline=$((SECONDS + TIMEOUT_SECONDS + 5))
while [[ ! -s $exit_status_file ]]; do
    (( SECONDS < deadline )) || fail 'outer tmux runner timed out'
    sleep 0.1
done

run_status=$(<"$exit_status_file")
if [[ $run_status != 0 ]]; then
    sed -n '1,160p' "$log" >&2
    [[ ! -f $result.progress ]] || sed -n '1,120p' "$result.progress" >&2
    [[ ! -f $result ]] || sed -n '1,120p' "$result" >&2
    fail "Leyline inner release probe failed: status=$run_status"
fi
if [[ ! -s $result ]]; then
    sed -n '1,200p' "$log" >&2
    fail 'inner release probe produced no result'
fi
for expected in \
    'wayland_tui_alternate_screen=pass' \
    'wayland_tui_attach_detach_roundtrips=2' \
    'wayland_tui_state_preserved=pass' \
    'wayland_osc52_owner_identity=not_observable' \
    'wayland_osc52_clipboard_content=pass' \
    'wayland_osc52_primary_content=pass' \
    'wayland_osc52_content=pass' \
    'wayland_release_probe=passed'; do
    if ! rg -qx "$expected" "$result"; then
        sed -n '1,120p' "$result" >&2
        fail "missing result field: $expected"
    fi
done

sed -n '1,120p' "$result"
printf 'topology=outer-tmux--leyline--inner-tmux\n'
printf 'excluded_gate=physical_keyboard_mouse_focus_resize_fractional_scale\n'
printf 'wayland_release_result=passed\n'
