#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    printf 'usage: %s OUTPUT [direct|tmux]\n' "$0" >&2
    exit 64
fi

export LC_ALL=C
readonly OUTPUT=$1
readonly PROFILE=${2:-direct}
readonly TIMEOUT_SECONDS=2
readonly FOREGROUND_REPLY=$'\033]10;rgb:1212/3434/5656\033\\'
readonly BACKGROUND_REPLY=$'\033]11;rgb:0a0a/1b1b/2c2c\007'
readonly TEMP_OUTPUT="$OUTPUT.tmp.$$"
original_stty=$(stty -g)

cleanup() {
    stty "$original_stty" 2>/dev/null || true
    rm -f -- "$TEMP_OUTPUT"
}
trap cleanup EXIT INT TERM

fail() {
    printf 'result=failed\ndetail=%s\n' "$1" >"$TEMP_OUTPUT"
    mv -- "$TEMP_OUTPUT" "$OUTPUT"
    exit 1
}

record() {
    printf '%s=%s\n' "$1" "$2" >>"$TEMP_OUTPUT"
}

hex() {
    od -An -tx1 -v | tr -d ' \n'
}

expect_exact() {
    local id=$1
    local request=$2
    local expected=$3
    local actual=
    printf '%s' "$request"
    if ! IFS= read -r -N "${#expected}" -t "$TIMEOUT_SECONDS" actual; then
        fail "$id timed out waiting for ${#expected} reply bytes"
    fi
    [[ $actual == "$expected" ]] || fail "$id reply mismatch: $(printf %s "$actual" | hex)"
    record "$id" "$(printf %s "$actual" | hex)"
}

expect_no_reply() {
    local id=$1
    local request=$2
    local actual=
    printf '%s' "$request"
    if IFS= read -r -N 1 -t 0.25 actual; then
        fail "$id unexpectedly returned $(printf %s "$actual" | hex)"
    fi
    record "$id" none
}

stty raw -echo min 1 time 0
: >"$TEMP_OUTPUT"
record schema_version 1
record term "${TERM-unknown}"
record colorterm "${COLORTERM-unknown}"
record tty yes
record profile "$PROFILE"

read -r lines columns < <(stty size)
[[ $lines =~ ^[1-9][0-9]*$ && $columns =~ ^[1-9][0-9]*$ ]] \
    || fail "invalid stty size: $lines $columns"
record grid "${columns}x${lines}"

# tmux terminates terminal-identification queries itself and does not reliably
# forward OSC color queries. Keep this profile focused on the intermediary's
# advertised identity plus the display sequences that must traverse it.
if [[ $PROFILE == tmux ]]; then
    printf '%s' $'\033[c'
    tmux_da=
    if ! IFS= read -r -d c -t "$TIMEOUT_SECONDS" tmux_da; then
        fail 'primary_da timed out behind tmux'
    fi
    tmux_da+=c
    tmux_da_prefix=$'\033[?'
    [[ $tmux_da == "$tmux_da_prefix"*c ]] \
        || fail "invalid tmux primary_da: $(printf %s "$tmux_da" | hex)"
    tmux_da_body=${tmux_da#"$tmux_da_prefix"}
    tmux_da_body=${tmux_da_body%c}
    [[ $tmux_da_body =~ ^[0-9]+(\;[0-9]+)*$ ]] \
        || fail "invalid tmux primary_da parameters: $tmux_da_body"
    record primary_da "$(printf %s "$tmux_da" | hex)"
    printf '%s' $'\033[2J\033[Htmux cursor/underline traversal\r\n\033[4mSingle\033[24m \033[4:2mDouble\033[24m \033[4:3mCurly\033[24m \033[4:4mDotted\033[24m \033[4:5mDashed\033[24m\033[5 q'
    record display_fixture emitted
    record result passed
    mv -- "$TEMP_OUTPUT" "$OUTPUT"
    trap - EXIT INT TERM
    stty "$original_stty"
    exit 0
fi

expect_exact osc10_st $'\033]10;?\033\\' "$FOREGROUND_REPLY"
expect_exact osc11_bel $'\033]11;?\007' "$BACKGROUND_REPLY"
printf -v expected_csi18 '\033[8;%s;%st' "$lines" "$columns"
expect_exact csi18 $'\033[18t' "$expected_csi18"

printf '%s' $'\033[14t'
pixel_reply=
if ! IFS= read -r -d t -t "$TIMEOUT_SECONDS" pixel_reply; then
    fail 'csi14 timed out'
fi
pixel_reply+=t
pixel_prefix=$'\033[4;'
[[ $pixel_reply == "$pixel_prefix"*t ]] || fail "invalid csi14 prefix: $(printf %s "$pixel_reply" | hex)"
pixel_body=${pixel_reply#"$pixel_prefix"}
pixel_body=${pixel_body%t}
IFS=';' read -r pixel_height pixel_width extra <<<"$pixel_body"
[[ -z ${extra-} && $pixel_height =~ ^[1-9][0-9]*$ && $pixel_width =~ ^[1-9][0-9]*$ ]] \
    || fail "invalid csi14 dimensions: $pixel_body"
(( pixel_height % lines == 0 && pixel_width % columns == 0 )) \
    || fail "csi14 is not an exact cell grid: ${pixel_width}x${pixel_height} for ${columns}x${lines}"
record csi14 "$(printf %s "$pixel_reply" | hex)"
record cell_px "$((pixel_width / columns))x$((pixel_height / lines))"

[[ $PROFILE == direct ]] || fail "unknown profile: $PROFILE"
expect_exact primary_da $'\033[c' $'\033[?6c'
expect_exact secondary_da $'\033[>c' $'\033[>0;2501;1c'
expect_exact dsr5 $'\033[5n' $'\033[0n'
expect_exact dsr6 $'\033[2;3H\033[6n' $'\033[2;3R'
expect_exact decrqm_private $'\033[?1$p' $'\033[?1;2$y'
expect_exact decrqm_public $'\033[4$p' $'\033[4;2$y'
expect_exact decrqm_unknown $'\033[?9999$p' $'\033[?9999;0$y'
expect_no_reply dsr_unknown $'\033[99n'

printf '%s' $'\033]10;#ffffff\007\033]11;#ffffff\007\033]12;#ffffff\007\033]104\007\033]110\007\033]111\007\033]112\007'
expect_exact color_mutation_blocked $'\033]10;?\033\\' "$FOREGROUND_REPLY"
expect_exact background_mutation_blocked $'\033]11;?\007' "$BACKGROUND_REPLY"

printf '%s' $'\033[?2026h\033]10;?\033\\'
unexpected=
if IFS= read -r -N 1 -t 0.05 unexpected; then
    fail "sync explicit leaked reply before ESU: $(printf %s "$unexpected" | hex)"
fi
expect_exact sync_explicit $'\033[?2026l' "$FOREGROUND_REPLY"

started_ns=$(date +%s%N)
expect_exact sync_timeout $'\033[?2026h\033]10;?\033\\' "$FOREGROUND_REPLY"
ended_ns=$(date +%s%N)
timeout_ms=$(((ended_ns - started_ns) / 1000000))
(( timeout_ms >= 50 && timeout_ms <= 1000 )) \
    || fail "sync timeout outside acceptance window: ${timeout_ms}ms"
record sync_timeout_ms "$timeout_ms"

# Leave a deterministic display-state fixture for compositor/manual inspection before exit.
printf '%s' $'\033[2J\033[HDECSCUSR block/beam/underline\r\n\033[4mSingle\033[24m \033[4:2mDouble\033[24m \033[4:3mCurly\033[24m \033[4:4mDotted\033[24m \033[4:5mDashed\033[24m\033[5 q'
record display_fixture emitted
record result passed
mv -- "$TEMP_OUTPUT" "$OUTPUT"
trap - EXIT INT TERM
stty "$original_stty"
