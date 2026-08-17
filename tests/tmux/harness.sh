#!/usr/bin/env bash
set -euo pipefail

readonly TIMEOUT_SECONDS=10
readonly DEFAULT_DEPTH=3
readonly SCHEMA_VERSION=1
readonly MAX_REMOTE_OUTPUT_BYTES=65536

usage() {
    printf '%s\n' \
        "Usage: $0 local|nested|recursive [DEPTH]" \
        "       $0 loopback SSH_TARGET SSH_PORT SSH_IDENTITY KNOWN_HOSTS" \
        "       $0 remote SSH_TARGET SSH_PORT SSH_IDENTITY KNOWN_HOSTS" \
        "       $0 local-remote SSH_TARGET SSH_PORT SSH_IDENTITY KNOWN_HOSTS"
}

fail() {
    printf 'result=failed detail=%q\n' "$1" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

record() {
    printf '%s=%q\n' "$1" "$2"
}

wait_for_file() {
    local path=$1
    local deadline=$((SECONDS + TIMEOUT_SECONDS))
    while [[ ! -s $path ]]; do
        (( SECONDS < deadline )) || fail "probe timed out: $path"
        sleep 0.05
    done
}

wait_for_line() {
    local path=$1
    local expected=$2
    local deadline=$((SECONDS + TIMEOUT_SECONDS))
    while ! rg -qx "$expected" "$path" 2>/dev/null; do
        (( SECONDS < deadline )) || fail "probe timed out waiting for $expected in $path"
        sleep 0.05
    done
}

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly PANE_PROBE="$SCRIPT_DIR/pane-probe.sh"
readonly BYTE_PROBE="$SCRIPT_DIR/byte-probe.sh"
readonly BACKGROUND_OUTPUT_PROBE="$SCRIPT_DIR/background-output-probe.sh"
readonly LIFECYCLE_PROBE="$SCRIPT_DIR/lifecycle-probe.sh"
readonly NEST_LAUNCH="$SCRIPT_DIR/nest-launch.sh"
readonly REMOTE_PROBE="$SCRIPT_DIR/remote-probe.sh"
readonly REMOTE_CLIENT="$SCRIPT_DIR/remote-client.sh"
readonly REMOTE_OUTPUT_VALIDATE="$SCRIPT_DIR/remote-output-validate.sh"
readonly TOPOLOGY=${1-}

case "$TOPOLOGY" in
    local|nested|recursive|loopback|remote|local-remote) ;;
    *) usage >&2; exit 64 ;;
esac

require_command tmux
require_command infocmp
require_command rg
require_command sha256sum
require_command script
require_command tic
require_command timeout

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux.XXXXXXXX")
readonly case_root
readonly config="$case_root/tmux.conf"
readonly outer_socket="$case_root/outer.sock"
readonly terminfo_db="$case_root/terminfo"
declare -a owned_sockets=("$outer_socket")

cleanup() {
    local socket
    local status=0
    for socket in "${owned_sockets[@]}"; do
        if [[ -S $socket ]]; then
            tmux -S "$socket" kill-server >/dev/null 2>&1 || status=1
        fi
    done
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux.* ]] || return 1
    rm -rf -- "$case_root"
    return "$status"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        record cleanup_verdict pass
    else
        record cleanup_verdict fail
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
mkdir -p "$terminfo_db"
tic -x -o "$terminfo_db" "$SCRIPT_DIR/../../terminfo/leyline.terminfo"
printf '%s\n' 'set -g default-terminal tmux-256color' >"$config"

readonly terminfo_hash=$(TERMINFO="$terminfo_db" TERMINFO_DIRS="$terminfo_db:" \
    infocmp -x leyline-256color | sha256sum | awk '{print $1}')
case "$TOPOLOGY" in
    local) topology_id=L1 ;;
    nested) topology_id=L2 ;;
    recursive) topology_id=LN ;;
    loopback) topology_id=RS1 ;;
    remote) topology_id=R1 ;;
    local-remote) topology_id=LR1 ;;
esac
readonly topology_id
readonly case_id="S1-BASE-$(date -u +%Y%m%dT%H%M%SZ)-$$"
readonly git_commit=$(git -C "$SCRIPT_DIR/../.." rev-parse HEAD 2>/dev/null || printf unknown)
if [[ -n $(git -C "$SCRIPT_DIR/../.." status --porcelain 2>/dev/null) ]]; then
    readonly dirty_state=dirty
else
    readonly dirty_state=clean
fi
# This prototype deliberately omits a recoverable source artifact.
readonly evidence_class=exploratory
record schema_version "$SCHEMA_VERSION"
record case_id "$case_id"
record captured_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
record git_commit "$git_commit"
record source_tree_hash not-captured
record dirty_state "$dirty_state"
record evidence_class "$evidence_class"
record patch_artifact_hash none
record build_profile not-applicable
record build_flags not-applicable
record binary_hash not-applicable
record os "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")"
record desktop "${XDG_CURRENT_DESKTOP-not-captured}"
record display_protocol "${XDG_SESSION_TYPE-not-captured}"
record terminal_under_test headless-transport-only
record topology "$TOPOLOGY"
record topology_id "$topology_id"
record socket_id "$(basename "$outer_socket")"
record tmux_version "$(tmux -V)"
record ssh_version "$(ssh -V 2>&1 || printf not-captured)"
record outer_term leyline-256color
record outer_colorterm truecolor
record terminfo_hash_kind infocmp-x-output
record terminfo_hash "$terminfo_hash"
record config_hash "$(sha256sum "$config" | awk '{print $1}')"
record probe_id tmux-transport-v2
record input_fixture_hash "$(printf 1b4f50 | sha256sum | awk '{print $1}')"
record timeout_seconds "$TIMEOUT_SECONDS"
record max_remote_output_bytes "$MAX_REMOTE_OUTPUT_BYTES"
record counterexample_or_reference_terminal not-captured

start_server() {
    local socket=$1
    local session=$2
    local command=$3
    TERM=leyline-256color TERMINFO="$terminfo_db" TERMINFO_DIRS="$terminfo_db:" COLORTERM=truecolor \
        tmux -S "$socket" -f "$config" new-session -d -x 80 -y 24 -s "$session" "$command"
}

assert_probe() {
    local output=$1
    local expected_term=$2
    wait_for_file "$output"
    rg -qx "term=$expected_term" "$output" || fail "unexpected pane TERM in $output"
    rg -qx 'colorterm=truecolor' "$output" || fail "unexpected pane COLORTERM in $output"
    rg -qx 'size=24 80' "$output" || fail "unexpected pane size in $output"
    rg -qx 'tty=yes' "$output" || fail "pane is not attached to a tty"
}

run_local() {
    local output="$case_root/local.txt"
    local byte_output="$case_root/bytes.txt"
    local command
    printf -v command '%q %q' "$PANE_PROBE" "$output"
    start_server "$outer_socket" leyline-local "$command"
    assert_probe "$output" tmux-256color
    record pane_term "$(sed -n 's/^term=//p' "$output")"
    record pane_colorterm "$(sed -n 's/^colorterm=//p' "$output")"
    record pane_size "$(tmux -S "$outer_socket" display-message -p -t leyline-local '#{pane_width}x#{pane_height}')"
    record default_terminal "$(tmux -S "$outer_socket" show-options -gv default-terminal)"
    record extended_keys "$(tmux -S "$outer_socket" show-options -gv extended-keys)"
    record set_clipboard "$(tmux -S "$outer_socket" show-options -gv set-clipboard)"
    record nesting_depth 1

    local byte_ready="$case_root/bytes.ready"
    printf -v command '%q %q %q %q' "$BYTE_PROBE" "$byte_output" 3 "$byte_ready"
    tmux -S "$outer_socket" new-window -d -t leyline-local -n bytes "$command"
    wait_for_file "$byte_ready"
    tmux -S "$outer_socket" send-keys -t leyline-local:bytes -H 1b 4f 50
    wait_for_file "$byte_output"
    [[ $(<"$byte_output") == 1b4f50 ]] || fail 'tmux changed the F1 byte fixture'
    record byte_fixture_f1 1b4f50

    local resize_ready="$case_root/resize.ready"
    local resize_samples="$case_root/resize-samples.txt"
    printf -v command '%q %q %q' "$LIFECYCLE_PROBE" "$resize_ready" "$resize_samples"
    tmux -S "$outer_socket" new-window -d -t leyline-local -n resize "$command"
    wait_for_file "$resize_ready"
    tmux -S "$outer_socket" resize-window -t leyline-local:resize -x 81 -y 25
    tmux -S "$outer_socket" resize-window -t leyline-local:resize -x 103 -y 33
    tmux -S "$outer_socket" resize-window -t leyline-local:resize -x 97 -y 31
    wait_for_line "$resize_samples" 'size=31 97'
    [[ $(tmux -S "$outer_socket" display-message -p -t leyline-local:resize '#{pane_width}x#{pane_height}') == 97x31 ]] \
        || fail 'tmux pane did not converge to the last resize'
    record resize_policy latest-wins
    record resize_tmux_final 97x31
    record resize_kernel_final 97x31

    tmux -S "$outer_socket" new-session -d -x 40 -y 12 -s survivor 'exec sleep 30'
    [[ $(tmux -S "$outer_socket" list-clients -F '#{client_pid}' | wc -l) -eq 0 ]] \
        || fail 'detached baseline unexpectedly has an attached client'
    tmux -S "$outer_socket" kill-session -t leyline-local
    tmux -S "$outer_socket" has-session -t survivor \
        || fail 'closing one session terminated the case tmux server'

    local attach_command
    local attach_round
    printf -v attach_command 'TERM=leyline-256color TERMINFO=%q TERMINFO_DIRS=%q COLORTERM=truecolor tmux -S %q attach-session -t survivor' \
        "$terminfo_db" "$terminfo_db:" "$outer_socket"
    for attach_round in 1 2; do
        { sleep 0.2; printf '\002d'; } \
            | timeout --foreground --kill-after=1s 3s script -qefc "$attach_command" /dev/null \
                >/dev/null \
            || fail "attached client did not detach cleanly in round $attach_round"
        tmux -S "$outer_socket" has-session -t survivor \
            || fail 'detaching the client terminated its session'
        [[ $(tmux -S "$outer_socket" display-message -p -t survivor '#{session_attached}') == 0 ]] \
            || fail 'detached client remained attached'
    done

    local layout_first_ready="$case_root/layout-first.ready"
    local layout_first_samples="$case_root/layout-first-samples.txt"
    local layout_second_ready="$case_root/layout-second.ready"
    local layout_second_samples="$case_root/layout-second-samples.txt"
    printf -v command '%q %q %q' "$LIFECYCLE_PROBE" "$layout_first_ready" "$layout_first_samples"
    tmux -S "$outer_socket" new-window -d -t survivor -n layout "$command"
    printf -v command '%q %q %q' "$LIFECYCLE_PROBE" "$layout_second_ready" "$layout_second_samples"
    tmux -S "$outer_socket" split-window -d -h -t survivor:layout "$command"
    wait_for_file "$layout_first_ready"
    wait_for_file "$layout_second_ready"
    tmux -S "$outer_socket" resize-window -t survivor:layout -x 100 -y 30
    tmux -S "$outer_socket" select-layout -t survivor:layout even-horizontal >/dev/null
    [[ $(tmux -S "$outer_socket" display-message -p -t survivor:layout '#{window_panes}') == 2 ]] \
        || fail 'split layout did not retain two panes'
    tmux -S "$outer_socket" resize-pane -Z -t survivor:layout.0
    [[ $(tmux -S "$outer_socket" display-message -p -t survivor:layout '#{window_zoomed_flag}') == 1 ]] \
        || fail 'layout did not enter zoomed state'
    tmux -S "$outer_socket" resize-pane -Z -t survivor:layout.0
    [[ $(tmux -S "$outer_socket" display-message -p -t survivor:layout '#{window_zoomed_flag}') == 0 ]] \
        || fail 'layout did not leave zoomed state'

    local first_width
    local first_height
    local second_width
    local second_height
    first_width=$(tmux -S "$outer_socket" display-message -p -t survivor:layout.0 '#{pane_width}')
    first_height=$(tmux -S "$outer_socket" display-message -p -t survivor:layout.0 '#{pane_height}')
    second_width=$(tmux -S "$outer_socket" display-message -p -t survivor:layout.1 '#{pane_width}')
    second_height=$(tmux -S "$outer_socket" display-message -p -t survivor:layout.1 '#{pane_height}')
    wait_for_line "$layout_first_samples" "size=$first_height $first_width"
    wait_for_line "$layout_second_samples" "size=$second_height $second_width"

    local background_ready="$case_root/background.ready"
    printf -v command 'bash %q %q' "$BACKGROUND_OUTPUT_PROBE" "$background_ready"
    tmux -S "$outer_socket" new-window -d -t survivor -n background "$command"
    wait_for_file "$background_ready"
    timeout 2s tmux -S "$outer_socket" display-message -p -t survivor:background '#{pane_width}x#{pane_height}' \
        >/dev/null || fail 'tmux control path stalled during background output'
    tmux -S "$outer_socket" capture-pane -p -t survivor:background -S -5 \
        | rg -q 'LEYLINE_TMUX_BACKGROUND_DONE' \
        || fail 'background pane did not preserve its final output marker'

    record detached_client_count 0
    record attach_detach_roundtrip pass
    record attach_detach_roundtrips 2
    record split_zoom_roundtrip pass
    record split_kernel_sizes_final "${first_width}x${first_height},${second_width}x${second_height}"
    record background_output_bytes 1048576
    record background_control_responsive pass
    record scoped_session_close pass
    record server_retained_without_client pass
}

run_recursive() {
    local depth=$1
    [[ $depth =~ ^[0-9]+$ ]] && (( depth >= 2 && depth <= 8 )) \
        || fail 'recursive depth must be between 2 and 8'

    local level
    for ((level = 2; level <= depth; level++)); do
        owned_sockets+=("$case_root/level-$level.sock")
    done

    local command
    printf -v command '%q %q %q %q %q %q' \
        "$NEST_LAUNCH" 1 "$depth" "$case_root" "$config" "$NEST_LAUNCH"
    start_server "$outer_socket" level-1 "$command"

    for ((level = 1; level <= depth; level++)); do
        local output="$case_root/level-$level.txt"
        wait_for_file "$output"
        rg -qx 'term=tmux-256color' "$output" || fail "unexpected pane TERM in $output"
        rg -qx 'colorterm=truecolor' "$output" || fail "unexpected pane COLORTERM in $output"
        rg -qx 'tty=yes' "$output" || fail "pane is not attached to a tty"
        record "level_${level}_term" "$(sed -n 's/^term=//p' "$output")"
        record "level_${level}_colorterm" "$(sed -n 's/^colorterm=//p' "$output")"
        record "level_${level}_size" "$(sed -n 's/^size=//p' "$output")"
        if (( level == 1 )); then
            record "level_${level}_socket" "$outer_socket"
        else
            record "level_${level}_socket" "$case_root/level-$level.sock"
        fi
    done
    record nesting_depth "$depth"
    if (( depth == 1 )); then
        record innermost_size "$(tmux -S "$outer_socket" display-message -p '#{pane_width}x#{pane_height}')"
    else
        record innermost_size "$(tmux -S "$case_root/level-$depth.sock" display-message -p '#{pane_width}x#{pane_height}')"
    fi

    local deepest_socket="$outer_socket"
    if (( depth > 1 )); then
        deepest_socket="$case_root/level-$depth.sock"
    fi
    local byte_output="$case_root/nested-bytes.txt"
    local byte_ready="$case_root/nested-bytes.ready"
    printf -v command '%q %q %q %q' "$BYTE_PROBE" "$byte_output" 3 "$byte_ready"
    tmux -S "$deepest_socket" new-window -d -t "level-$depth" -n bytes "$command"
    wait_for_file "$byte_ready"
    tmux -S "$deepest_socket" select-window -t "level-$depth:bytes"
    tmux -S "$outer_socket" send-keys -t level-1 -H 1b 4f 50
    wait_for_file "$byte_output"
    [[ $(<"$byte_output") == 1b4f50 ]] || fail 'nested tmux changed the F1 byte fixture'
    record byte_fixture_f1 1b4f50

    local resize_ready="$case_root/nested-resize.ready"
    local resize_samples="$case_root/nested-resize-samples.txt"
    printf -v command '%q %q %q' "$LIFECYCLE_PROBE" "$resize_ready" "$resize_samples"
    tmux -S "$deepest_socket" new-window -d -t "level-$depth" -n resize "$command"
    wait_for_file "$resize_ready"
    tmux -S "$deepest_socket" select-window -t "level-$depth:resize"
    tmux -S "$outer_socket" resize-window -t level-1 -x 100 -y 30

    local expected_deepest_height=$((31 - depth))
    local resize_deadline=$((SECONDS + TIMEOUT_SECONDS))
    while [[ $(tmux -S "$deepest_socket" display-message -p -t "level-$depth:resize" \
        '#{pane_width}x#{pane_height}') != "100x$expected_deepest_height" ]]; do
        (( SECONDS < resize_deadline )) || fail 'nested tmux sizes did not converge'
        sleep 0.05
    done
    wait_for_line "$resize_samples" "size=$expected_deepest_height 100"

    for ((level = 1; level <= depth; level++)); do
        local level_socket="$outer_socket"
        if (( level > 1 )); then
            level_socket="$case_root/level-$level.sock"
        fi
        record "level_${level}_resize_final" \
            "$(tmux -S "$level_socket" display-message -p '#{pane_width}x#{pane_height}')"
    done
    record nested_resize_kernel_final "100x$expected_deepest_height"
}

run_remote() {
    local target=$1
    local port=$2
    local identity=$3
    local known_hosts=$4
    local through_tmux=${5-false}
    [[ $port =~ ^[0-9]+$ ]] && (( port >= 1 && port <= 65535 )) \
        || fail 'SSH port must be between 1 and 65535'
    [[ -f $identity ]] || fail "SSH identity is not a file: $identity"
    [[ -f $known_hosts ]] || fail "known_hosts is not a file: $known_hosts"
    [[ $target =~ ^[A-Za-z0-9._@:-]+$ ]] || fail 'SSH target contains unsupported characters'
    require_command ssh
    require_command head

    local observed
    local output="$case_root/remote.txt"
    if [[ $through_tmux == true ]]; then
        local command
        printf -v command '%q ' \
            "$REMOTE_CLIENT" "$output" "$REMOTE_PROBE" "$TIMEOUT_SECONDS" \
            "$MAX_REMOTE_OUTPUT_BYTES" "$target" "$port" "$identity" "$known_hosts"
        command+='; exec sleep 30'
        start_server "$outer_socket" leyline-local-remote "$command"
        wait_for_file "$output"
        observed=$(<"$output")
        record local_pane_term tmux-256color
        record local_pane_size \
            "$(tmux -S "$outer_socket" display-message -p -t leyline-local-remote '#{pane_width}x#{pane_height}')"
    else
        if ! "$REMOTE_CLIENT" "$output" "$REMOTE_PROBE" "$TIMEOUT_SECONDS" \
            "$MAX_REMOTE_OUTPUT_BYTES" "$target" "$port" "$identity" "$known_hosts"; then
            observed=$(<"$output")
            fail "isolated SSH probe failed: $observed"
        fi
        observed=$(<"$output")
    fi
    bash "$REMOTE_OUTPUT_VALIDATE" "$output" >/dev/null \
        || fail 'remote output failed schema validation'
    record ssh_target "$target"
    record ssh_port "$port"
    record known_hosts_hash "$(sha256sum "$known_hosts" | awk '{print $1}')"
    record remote_term tmux-256color
    record remote_colorterm truecolor
    record remote_size 80x24
    record remote_host_identity_kind \
        "$(sed -n 's/^remote_host_identity_kind=//p' <<<"$observed")"
    record remote_host_identity \
        "$(sed -n 's/^remote_host_identity=//p' <<<"$observed")"
    record remote_rootfs_identity \
        "$(sed -n 's/^remote_rootfs_identity=//p' <<<"$observed")"
    record remote_terminfo_hash \
        "$(sed -n 's/^remote_terminfo_hash=//p' <<<"$observed")"
    record remote_custom_terminfo \
        "$(sed -n 's/^remote_custom_terminfo=//p' <<<"$observed")"
    record remote_resize_tmux_final 97x31
    record remote_resize_kernel_final 97x31
    record remote_scoped_session_close pass
    record remote_server_retained_without_client pass
    if [[ $through_tmux == true ]]; then
        record nesting_depth 2
    else
        record nesting_depth 1
    fi
}

case "$TOPOLOGY" in
    local)
        run_local
        ;;
    nested)
        run_recursive 2
        ;;
    recursive)
        run_recursive "${2-$DEFAULT_DEPTH}"
        ;;
    loopback|remote)
        [[ $# -eq 5 ]] || { usage >&2; exit 64; }
        run_remote "$2" "$3" "$4" "$5" false
        ;;
    local-remote)
        [[ $# -eq 5 ]] || { usage >&2; exit 64; }
        run_remote "$2" "$3" "$4" "$5" true
        ;;
esac

record result passed
