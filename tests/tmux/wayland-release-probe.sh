#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s RESULT_FILE\n' "$0" >&2
    exit 64
fi

readonly RESULT_FILE=$1
readonly TIMEOUT_SECONDS=5

fail() {
    printf 'wayland_release_probe=failed detail=%q\n' "$1" >&2
    printf 'wayland_release_probe=failed\ndetail=%s\n' "$1" >"$RESULT_FILE"
    exit 1
}

record_progress() {
    printf 'probe_step=%s\n' "$1" >>"$RESULT_FILE.progress"
}

for command in cmp mktemp rg script tail tmux timeout vim wl-copy wl-paste; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || fail 'not running in a Wayland session'
[[ -n ${WAYLAND_DISPLAY:-} ]] || fail 'WAYLAND_DISPLAY is unset'
[[ -z ${TMUX:-} ]] || fail 'probe must run in the Leyline PTY, not an existing tmux pane'

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-wl-release.XXXXXXXX")
readonly case_root
readonly socket="$case_root/tmux.sock"
readonly config="$case_root/tmux.conf"
readonly fixture="$case_root/tui.txt"
readonly osc_pane="$case_root/osc-pane.sh"

clipboard_owner=
primary_owner=
selections_replaced=false
clipboard_had_content=false
primary_had_content=false

cleanup() {
    local status=0
    if [[ -S $socket ]]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    fi
    if [[ $selections_replaced == true ]]; then
        [[ -z $clipboard_owner ]] || kill "$clipboard_owner" >/dev/null 2>&1 || true
        [[ -z $primary_owner ]] || kill "$primary_owner" >/dev/null 2>&1 || true
        [[ -z $clipboard_owner ]] || timeout 1s tail --pid="$clipboard_owner" -f /dev/null \
            >/dev/null 2>&1 || kill -KILL "$clipboard_owner" >/dev/null 2>&1 || true
        [[ -z $primary_owner ]] || timeout 1s tail --pid="$primary_owner" -f /dev/null \
            >/dev/null 2>&1 || kill -KILL "$primary_owner" >/dev/null 2>&1 || true
        [[ -z $clipboard_owner ]] || wait "$clipboard_owner" 2>/dev/null || true
        [[ -z $primary_owner ]] || wait "$primary_owner" 2>/dev/null || true
        if [[ $clipboard_had_content == true ]]; then
            timeout --foreground 3s wl-copy <"$case_root/clipboard.before" || status=1
        else
            timeout --foreground 3s wl-copy --clear || status=1
        fi
        if [[ $primary_had_content == true ]]; then
            timeout --foreground 3s wl-copy --primary <"$case_root/primary.before" || status=1
        else
            timeout --foreground 3s wl-copy --primary --clear || status=1
        fi
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-wl-release.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    cleanup || (( exit_status != 0 )) || exit_status=1
    printf '%s\n' "$exit_status" >"$RESULT_FILE.done"
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

wait_for() {
    local description=$1
    shift
    local deadline=$((SECONDS + TIMEOUT_SECONDS))
    until "$@"; do
        (( SECONDS < deadline )) || fail "timeout waiting for $description"
        sleep 0.05
    done
}

no_clients() {
    [[ -z $(tmux -S "$socket" list-clients -t manual-tui 2>/dev/null || true) ]]
}

alternate_screen_on() {
    [[ $(tmux -S "$socket" display-message -p -t manual-tui '#{alternate_on}' 2>/dev/null) == 1 ]]
}

attach_and_detach() {
    local transcript=$1
    local attach_command
    printf -v attach_command 'TERM=xterm-256color COLORTERM=truecolor tmux -S %q attach-session -t manual-tui' \
        "$socket"
    { sleep 0.4; printf '\002d'; } \
        | timeout --foreground --kill-after=1s 3s script -qefc "$attach_command" "$transcript"
}

run_tui_lifecycle() {
    printf '%s\n' \
        'Leyline tmux automated TUI fixture' \
        'ASCII | 中文宽字符 | é | 0123456789' \
        'DETACH-REATTACH-STATE-MUST-PERSIST' >"$fixture"
    local pane_command
    printf -v pane_command 'exec vim -Nu NONE -n -R %q' "$fixture"
    TERM=xterm-256color COLORTERM=truecolor \
        tmux -S "$socket" -f "$config" new-session -d -x 100 -y 30 -s manual-tui \
            "$pane_command"
    wait_for 'Vim alternate screen' alternate_screen_on
    attach_and_detach "$case_root/tui-first.typescript" \
        || fail 'first real tmux client did not detach cleanly'
    tmux -S "$socket" has-session -t manual-tui \
        || fail 'TUI session died after first detach'
    no_clients || fail 'first client remained attached'
    tmux -S "$socket" capture-pane -p -e -t manual-tui >"$case_root/tui.after-first"
    rg -q 'DETACH-REATTACH-STATE-MUST-PERSIST' "$case_root/tui.after-first" \
        || fail 'TUI fixture marker disappeared after first detach'

    attach_and_detach "$case_root/tui-second.typescript" \
        || fail 'second real tmux client did not detach cleanly'
    tmux -S "$socket" has-session -t manual-tui \
        || fail 'TUI session died after reattach/detach'
    no_clients || fail 'second client remained attached'
    [[ $(tmux -S "$socket" display-message -p -t manual-tui '#{alternate_on}') == 1 ]] \
        || fail 'Vim left alternate screen across detach/reattach'
    tmux -S "$socket" capture-pane -p -e -t manual-tui >"$case_root/tui.after-second"
    cmp -s "$case_root/tui.after-first" "$case_root/tui.after-second" \
        || fail 'TUI pane state changed across detach/reattach'
    tmux -S "$socket" kill-session -t manual-tui
}

save_selection_contents() {
    if timeout --foreground 3s wl-paste --no-newline \
        >"$case_root/clipboard.before" 2>/dev/null; then
        clipboard_had_content=true
    fi
    if timeout --foreground 3s wl-paste --primary --no-newline \
        >"$case_root/primary.before" 2>/dev/null; then
        primary_had_content=true
    fi
}

capture_selection() {
    local target=$1
    local output=$2
    local state=$3
    local -a args=(--no-newline)
    [[ $target == Clipboard ]] || args=(--primary --no-newline)
    if timeout --foreground 3s wl-paste "${args[@]}" >"$output" 2>/dev/null; then
        printf 'present\n' >"$state"
    else
        local status=$?
        (( status != 124 && status != 137 )) \
            || fail "timed out reading $target selection"
        : >"$output"
        printf 'absent\n' >"$state"
    fi
}

install_selection_sentinels() {
    printf %s LEYLINE_CLIPBOARD_SENTINEL >"$case_root/clipboard.sentinel"
    printf %s LEYLINE_PRIMARY_SENTINEL >"$case_root/primary.sentinel"
    wl-copy --foreground <"$case_root/clipboard.sentinel" &
    clipboard_owner=$!
    wl-copy --primary --foreground <"$case_root/primary.sentinel" &
    primary_owner=$!
    selections_replaced=true
    sleep 0.3
    capture_selection Clipboard "$case_root/clipboard.baseline" \
        "$case_root/clipboard.baseline.state"
    capture_selection Primary "$case_root/primary.baseline" \
        "$case_root/primary.baseline.state"
    if kill -0 "$clipboard_owner" >/dev/null 2>&1; then
        record_progress clipboard_owner_initially_alive
    else
        record_progress clipboard_owner_managed_externally
    fi
    if kill -0 "$primary_owner" >/dev/null 2>&1; then
        record_progress primary_owner_initially_alive
    else
        record_progress primary_owner_managed_externally
    fi
}

emit_direct_osc52_attempt() {
    local checkpoint=$1
    local sequence=$2
    printf '%b' "$sequence"
    sleep 0.2
    assert_selection_contents "$checkpoint"
}

emit_tmux_osc52_attempts() {
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'sleep 0.3' \
        "tmux set-buffer -w 'LEYLINE_OSC52_ATTACK'" \
        "printf '\\033]52;c;TEVZTElORV9PU0M1Ml9BVFRBQ0s=\\007'" \
        "printf '\\033]52;p;TEVZTElORV9PU0M1Ml9BVFRBQ0s=\\007'" \
        'sleep 0.3' >"$osc_pane"
    chmod 700 "$osc_pane"
    local pane_command
    printf -v pane_command 'exec bash %q' "$osc_pane"
    TERM=xterm-256color COLORTERM=truecolor \
        timeout --foreground --kill-after=1s 4s tmux -S "$socket" -f "$config" \
            new-session -s manual-osc52 "$pane_command"
}

assert_selection_contents() {
    local checkpoint=$1
    capture_selection Clipboard "$case_root/clipboard.check" "$case_root/clipboard.check.state"
    capture_selection Primary "$case_root/primary.check" "$case_root/primary.check.state"
    cmp -s "$case_root/clipboard.baseline.state" "$case_root/clipboard.check.state" \
        || fail "Clipboard presence changed at checkpoint: $checkpoint"
    cmp -s "$case_root/primary.baseline.state" "$case_root/primary.check.state" \
        || fail "Primary presence changed at checkpoint: $checkpoint"
    cmp -s "$case_root/clipboard.baseline" "$case_root/clipboard.check" \
        || fail "Clipboard changed at checkpoint: $checkpoint"
    cmp -s "$case_root/primary.baseline" "$case_root/primary.check" \
        || fail "Primary changed at checkpoint: $checkpoint"
    record_progress "content_unchanged_$checkpoint"
}

umask 077
printf '%s\n' \
    'set -g default-terminal tmux-256color' \
    'set -g status on' \
    'set -g status-left "[Leyline release probe] "' \
    'set -g set-clipboard on' >"$config"

record_progress tui_start
run_tui_lifecycle
record_progress tui_pass
save_selection_contents
install_selection_sentinels
record_progress sentinels_ready
assert_selection_contents before_osc52
emit_direct_osc52_attempt direct_clipboard_set '\033]52;c;TEVZTElORV9PU0M1Ml9BVFRBQ0s=\007'
emit_direct_osc52_attempt direct_primary_set '\033]52;p;TEVZTElORV9PU0M1Ml9BVFRBQ0s=\007'
emit_direct_osc52_attempt direct_clipboard_query '\033]52;c;?\007'
emit_direct_osc52_attempt direct_primary_query '\033]52;p;?\007'
emit_direct_osc52_attempt direct_clipboard_clear '\033]52;c;\007'
emit_direct_osc52_attempt direct_primary_clear '\033]52;p;\007'
record_progress tmux_osc52_start
emit_tmux_osc52_attempts || fail 'tmux OSC 52 client did not exit cleanly'
record_progress tmux_osc52_done
sleep 0.2
assert_selection_contents after_tmux_osc52
assert_selection_contents final
record_progress sentinels_pass

result_tmp="$RESULT_FILE.tmp.$$"
printf '%s\n' \
    'wayland_tui_alternate_screen=pass' \
    'wayland_tui_attach_detach_roundtrips=2' \
    'wayland_tui_state_preserved=pass' \
    'wayland_osc52_owner_identity=not_observable' \
    'wayland_osc52_clipboard_content=pass' \
    'wayland_osc52_primary_content=pass' \
    'wayland_osc52_content=pass' \
    'wayland_release_probe=passed' >"$result_tmp"
mv -- "$result_tmp" "$RESULT_FILE"
