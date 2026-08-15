#!/bin/sh
set -eu

fail() {
    printf 'remote_result=failed detail=%s\n' "$1" >&2
    exit 1
}

command -v tmux >/dev/null 2>&1 || fail 'missing tmux'
command -v infocmp >/dev/null 2>&1 || fail 'missing infocmp'
command -v sha256sum >/dev/null 2>&1 || fail 'missing sha256sum'

remote_tmp=${TMPDIR:-/tmp}
remote_tmp=$(cd -- "$remote_tmp" && pwd -P)
case_root=$(mktemp -d "$remote_tmp/leyline-tmux-remote.XXXXXXXX")
socket="$case_root/tmux.sock"
config="$case_root/tmux.conf"
size_samples="$case_root/size-samples.txt"
cleaned=false

cleanup() {
    cleanup_status=0
    if [ -S "$socket" ]; then
        tmux -S "$socket" kill-server >/dev/null 2>&1 || cleanup_status=1
    fi
    rm -f -- "$socket" "$config" "$size_samples" || cleanup_status=1
    rmdir -- "$case_root" || cleanup_status=1
    cleaned=true
    return "$cleanup_status"
}

cleanup_on_exit() {
    exit_status=$?
    trap - EXIT INT TERM
    if [ "$cleaned" = false ]; then
        if cleanup; then
            printf 'remote_cleanup=pass\n'
        else
            printf 'remote_cleanup=fail\n' >&2
            [ "$exit_status" -ne 0 ] || exit_status=1
        fi
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
printf '%s\n' 'set -g default-terminal tmux-256color' >"$config"

printf 'remote_tmux_version=%s\n' "$(tmux -V)"
if [ -r /etc/leyline-rootfs-id ]; then
    printf 'remote_rootfs_identity=%s\n' \
        "$(sha256sum /etc/leyline-rootfs-id | awk '{print $1}')"
else
    printf 'remote_rootfs_identity=not-captured\n'
fi
if [ -r /etc/machine-id ]; then
    printf 'remote_host_identity_kind=machine-id-sha256\n'
    printf 'remote_host_identity=%s\n' \
        "$(sha256sum /etc/machine-id | awk '{print $1}')"
else
    printf 'remote_host_identity_kind=hostname-sha256\n'
    printf 'remote_host_identity=%s\n' \
        "$(hostname | sha256sum | awk '{print $1}')"
fi
printf 'remote_terminfo_hash=%s\n' \
    "$(infocmp -x xterm-256color | sha256sum | awk '{print $1}')"
if infocmp -x leyline-256color >/dev/null 2>&1; then
    printf 'remote_custom_terminfo=present\n'
else
    printf 'remote_custom_terminfo=missing\n'
fi
printf 'remote_config_hash=%s\n' "$(sha256sum "$config" | awk '{print $1}')"

pane_command='printf "term=%s\ncolorterm=%s\nsize=" "$TERM" "${COLORTERM-}"; stty size; printf "tty=%s\n" "$(test -t 0 && printf yes || printf no)"; while :; do stty size >'"$size_samples"'; sleep 0.05; done'
TERM=xterm-256color COLORTERM=truecolor \
    tmux -S "$socket" -f "$config" new-session -d -x 80 -y 24 -s probe "$pane_command"

attempt=0
observed=
while [ "$attempt" -lt 100 ]; do
    observed=$(tmux -S "$socket" capture-pane -p -t probe)
    printf '%s\n' "$observed" | grep -q '^term=' && break
    attempt=$((attempt + 1))
    sleep 0.05
done
[ "$attempt" -lt 100 ] || fail 'pane probe timed out'

printf '%s\n' "$observed"
tmux -S "$socket" display-message -p -t probe \
    'pane_size=#{pane_width}x#{pane_height}'

tmux -S "$socket" resize-window -t probe -x 81 -y 25
tmux -S "$socket" resize-window -t probe -x 103 -y 33
tmux -S "$socket" resize-window -t probe -x 97 -y 31
attempt=0
while [ "$attempt" -lt 100 ]; do
    [ -r "$size_samples" ] && grep -qx '31 97' "$size_samples" && break
    attempt=$((attempt + 1))
    sleep 0.05
done
[ "$attempt" -lt 100 ] || fail 'remote kernel winsize did not converge'
[ "$(tmux -S "$socket" display-message -p -t probe '#{pane_width}x#{pane_height}')" = 97x31 ] \
    || fail 'remote tmux pane did not converge'
printf 'remote_resize_tmux_final=97x31\n'
printf 'remote_resize_kernel_final=97x31\n'

tmux -S "$socket" new-session -d -x 40 -y 12 -s survivor 'exec sleep 30'
tmux -S "$socket" kill-session -t probe
tmux -S "$socket" has-session -t survivor \
    || fail 'closing the remote probe session terminated its server'
printf 'remote_scoped_session_close=pass\n'
printf 'remote_server_retained_without_client=pass\n'

if cleanup; then
    trap - EXIT INT TERM
    printf 'remote_cleanup=pass\n'
    printf 'remote_result=passed\n'
else
    fail 'scoped cleanup failed'
fi
