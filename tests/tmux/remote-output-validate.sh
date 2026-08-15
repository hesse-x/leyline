#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s OUTPUT\n' "$0" >&2
    exit 64
fi

readonly output=$1

fail() {
    printf 'remote_output_validation=failed detail=%q\n' "$1" >&2
    exit 1
}

[[ -f $output ]] || fail 'output is not a regular file'
rg -qx 'term=tmux-256color' "$output" || fail 'unexpected or missing pane TERM'
rg -qx 'colorterm=truecolor' "$output" || fail 'unexpected or missing pane COLORTERM'
rg -qx 'size=24 80' "$output" || fail 'unexpected or missing initial pane size'
rg -qx 'tty=yes' "$output" || fail 'pane is not attached to a tty'
rg -qx 'remote_resize_tmux_final=97x31' "$output" || fail 'missing final tmux resize'
rg -qx 'remote_resize_kernel_final=97x31' "$output" || fail 'missing final kernel resize'
rg -qx 'remote_scoped_session_close=pass' "$output" || fail 'missing scoped session result'
rg -qx 'remote_server_retained_without_client=pass' "$output" || fail 'missing server ownership result'
rg -qx 'remote_cleanup=pass' "$output" || fail 'remote cleanup did not pass'

printf 'remote_output_validation=pass\n'
