#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly PROBE_BIN=${1:-$REPO_ROOT/target/debug/leyline-probe}
readonly PANE_SCRIPT="$SCRIPT_DIR/scene-pane.sh"

fail() {
    printf 'scene_fixture_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in head rg script sha256sum tail tmux timeout; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ -x $PROBE_BIN ]] || fail "probe binary is not executable: $PROBE_BIN"

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux-scene.XXXXXXXX")
readonly case_root
readonly socket="$case_root/tmux.sock"
readonly config="$case_root/tmux.conf"
readonly fixture="$case_root/tmux.fixture"

cleanup() {
    local status=0
    if [[ -S $socket ]]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || status=1
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux-scene.* ]] || return 1
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
printf '%s\n' \
    'set -g default-terminal tmux-256color' \
    'set -g status on' \
    'set -g status-left status' \
    'set -g status-right "#{pane_mode}"' \
    'set -g status-style "fg=colour196,bg=colour17,bold"' \
    'set -g mouse on' \
    'set -g focus-events on' \
    'set -g set-clipboard external' \
    'set -g set-titles on' \
    'set -g set-titles-string "tmux scene"' >"$config"

printf -v pane_one 'bash %q %q' "$PANE_SCRIPT" pane-1
printf -v pane_two 'bash %q %q' "$PANE_SCRIPT" pane-2
TERM=xterm-256color COLORTERM=truecolor \
    tmux -S "$socket" -f "$config" new-session -d -x 80 -y 24 -s scene "$pane_one"
tmux -S "$socket" split-window -d -h -t scene "$pane_two"
tmux -S "$socket" select-layout -t scene even-horizontal >/dev/null
tmux -S "$socket" select-pane -t scene.0

deadline=$((SECONDS + 3))
while [[ $(tmux -S "$socket" display-message -p -t scene.0 '#{alternate_on}') != 1 ]]; do
    (( SECONDS < deadline )) || fail 'pane did not enter its alternate screen'
    sleep 0.05
done
tmux -S "$socket" copy-mode -t scene.0
[[ $(tmux -S "$socket" display-message -p -t scene.0 '#{pane_in_mode}') == 1 ]] \
    || fail 'pane did not enter tmux copy-mode'

printf -v attach_command 'TERM=xterm-256color COLORTERM=truecolor tmux -S %q attach-session -t scene' \
    "$socket"
{ sleep 1; printf '\002d'; } \
    | timeout --foreground --kill-after=1s 4s script -qefc "$attach_command" "$fixture" \
        >/dev/null \
    || fail 'tmux scene client did not detach cleanly'

# Strip the typescript header and the detach sequence that clears the tmux grid.
scene_offset=$(rg --text --byte-offset --only-matching --fixed-strings $'\033[?1049h' "$fixture" \
    | head -n 1 | cut -d: -f1)
teardown_offset=$(rg --text --byte-offset --only-matching --fixed-strings $'\033[1;0r' "$fixture" \
    | tail -n 1 | cut -d: -f1)
[[ $scene_offset =~ ^[1-9][0-9]*$ ]] || fail 'tmux fixture has no alternate-screen entry'
[[ $teardown_offset =~ ^[1-9][0-9]*$ ]] || fail 'tmux fixture has no scroll-region teardown'
(( scene_offset < teardown_offset )) || fail 'tmux fixture teardown precedes its scene'
tail -c "+$((scene_offset + 1))" "$fixture" \
    | head -c "$((teardown_offset - scene_offset))" >"$fixture.trimmed"
mv -- "$fixture.trimmed" "$fixture"

# A pseudo-terminal has no compositor focus source, and tmux consumes OSC 52 internally.
# Add non-rendering sentinels so the composite fixture retains the full scene contract.
printf '\033[?1004h\033]52;c;Zm9v\007' >>"$fixture"
rg --text -q 'copy-mode' "$fixture" || fail 'captured scene does not expose copy-mode'

"$PROBE_BIN" scene --json --terminal-fixture "$fixture"
printf 'fixture_bytes=%s\n' "$(wc -c <"$fixture")"
printf 'fixture_sha256=%s\n' "$(sha256sum "$fixture" | awk '{print $1}')"
printf 'alternate_screen=pass\n'
printf 'copy_mode=pass\n'
printf 'scene_fixture_result=passed\n'
