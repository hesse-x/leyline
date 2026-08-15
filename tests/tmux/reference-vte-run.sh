#!/usr/bin/env bash
set -euo pipefail

readonly TIMEOUT_SECONDS=15
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly PANE_PROBE="$SCRIPT_DIR/wayland-pane-probe.sh"

fail() {
    printf 'result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in gnome-terminal tmux timeout rg; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ ${XDG_SESSION_TYPE-} == wayland ]] || fail 'not running in a Wayland session'

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-vte.XXXXXXXX")
readonly case_root
readonly socket="$case_root/tmux.sock"
readonly config="$case_root/tmux.conf"
readonly output="$case_root/pane.txt"

cleanup() {
    if [[ -S $socket ]]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-vte.* ]] || return 1
    rm -rf -- "$case_root"
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

timeout --foreground --kill-after=3s "${TIMEOUT_SECONDS}s" \
    gnome-terminal --wait -- \
    tmux -S "$socket" -f "$config" new-session -s reference-vte \
    "$PANE_PROBE $output" || fail 'GNOME Terminal/tmux client failed or timed out'

[[ -s $output ]] || fail 'pane probe produced no output'
rg -qx 'pane_TERM=tmux-256color' "$output" || fail 'unexpected pane TERM'
rg -qx 'pane_tty=yes' "$output" || fail 'pane did not receive a tty'
rg -qx 'client_attached=1' "$output" || fail 'tmux client was not attached'

printf 'case_id=S1-REF-VTE-%s-%s\n' "$(date -u +%Y%m%dT%H%M%SZ)" "$$"
printf 'topology_id=L1\nterminal_under_test=GNOME-Terminal\n'
printf 'terminal_version=%q\n' "$(gnome-terminal --version)"
sed -n '/^pane_TERM=/p;/^pane_COLORTERM=/p;/^pane_size=/p;/^client_termname=/p;/^client_size=/p;/^pane_size_tmux=/p;/^client_attached=/p' "$output"
printf 'result=passed\n'
