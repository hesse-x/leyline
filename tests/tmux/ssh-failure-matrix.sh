#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
    printf 'usage: %s TARGET PORT IDENTITY KNOWN_HOSTS\n' "$0" >&2
    exit 64
fi

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly REMOTE_CLIENT="$SCRIPT_DIR/remote-client.sh"
readonly REMOTE_PROBE="$SCRIPT_DIR/remote-probe.sh"
readonly REMOTE_OUTPUT_VALIDATE="$SCRIPT_DIR/remote-output-validate.sh"
readonly TARGET=$1
readonly PORT=$2
readonly IDENTITY=$3
readonly KNOWN_HOSTS=$4

fail() {
    printf 'ssh_failure_matrix_result=failed detail=%q\n' "$1" >&2
    exit 1
}

for command in rg ssh-keygen; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
[[ $PORT =~ ^[0-9]+$ ]] && (( PORT >= 1 && PORT <= 65535 )) || fail 'invalid SSH port'
[[ -f $IDENTITY ]] || fail 'identity is not a file'
[[ -f $KNOWN_HOSTS ]] || fail 'known_hosts is not a file'

temp_parent=$(cd -- "${TMPDIR:-/tmp}" && pwd -P)
readonly temp_parent
case_root=$(mktemp -d "$temp_parent/leyline-tmux.failure-matrix.XXXXXXXX")
readonly case_root

cleanup() {
    if [[ ${interrupt_root-} == "$case_root/interrupt-remote" ]]; then
        if [[ -S ${interrupt_socket-} ]]; then
            tmux -S "$interrupt_socket" kill-server >/dev/null 2>&1 || true
        fi
        rm -f -- "${interrupt_socket-}" "$interrupt_root/config"
        rmdir -- "$interrupt_root" 2>/dev/null || true
    fi
    [[ $(dirname -- "$case_root") == "$temp_parent" ]] || return 1
    [[ $(basename -- "$case_root") == leyline-tmux.failure-matrix.* ]] || return 1
    rm -rf -- "$case_root"
}

cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT INT TERM
    if cleanup; then
        printf 'ssh_failure_matrix_cleanup=pass\n'
    else
        printf 'ssh_failure_matrix_cleanup=fail\n' >&2
        (( exit_status != 0 )) || exit_status=1
    fi
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

umask 077
readonly timeout_probe="$case_root/timeout.sh"
readonly oversize_probe="$case_root/oversize.sh"
readonly dependency_probe="$case_root/dependency.sh"
readonly malformed_probe="$case_root/malformed.sh"
readonly interrupt_probe="$case_root/interrupt.sh"
printf '%s\n' 'sleep 5' >"$timeout_probe"
printf '%s\n' "awk 'BEGIN { for (i = 0; i < 4096; i++) print \"0123456789abcdef\" }'" >"$oversize_probe"
printf '%s\n' 'PATH=/nonexistent; command -v tmux >/dev/null 2>&1 || exit 127' >"$dependency_probe"
printf '%s\n' 'printf "term=tmux-256color\npartial=true\n"' >"$malformed_probe"
readonly interrupt_root="$case_root/interrupt-remote"
readonly interrupt_socket="$interrupt_root/tmux.sock"
printf '%s\n' \
    'set -eu' \
    "root='$interrupt_root'" \
    "socket='$interrupt_socket'" \
    'cleanup() {' \
    '  tmux -S "$socket" kill-server >/dev/null 2>&1 || true' \
    '  rm -f -- "$socket" "$root/config"' \
    '  rmdir -- "$root" 2>/dev/null || true' \
    '}' \
    'trap cleanup EXIT HUP INT TERM' \
    'mkdir "$root"' \
    'printf "%s\n" "set -g default-terminal tmux-256color" >"$root/config"' \
    'TERM=xterm-256color tmux -S "$socket" -f "$root/config" new-session -d -s interrupted "exec sleep 30"' \
    'printf "interrupt_ready=yes\n"' \
    'while :; do printf "heartbeat=yes\n"; sleep 0.1; done' >"$interrupt_probe"

wrong_key="$case_root/wrong-client"
wrong_host="$case_root/wrong-host"
wrong_known_hosts="$case_root/wrong-known-hosts"
ssh-keygen -q -t ed25519 -N '' -f "$wrong_key"
ssh-keygen -q -t ed25519 -N '' -f "$wrong_host"
printf '[127.0.0.1]:%s %s\n' "$PORT" "$(<"$wrong_host.pub")" >"$wrong_known_hosts"

expect_failure() {
    local name=$1
    local probe=$2
    local timeout_seconds=$3
    local max_bytes=$4
    local identity=$5
    local known_hosts=$6
    local expected_detail=$7
    local case_dir="$case_root/leyline-tmux.$name"
    local output="$case_dir/remote.txt"
    mkdir "$case_dir"
    if "$REMOTE_CLIENT" "$output" "$probe" "$timeout_seconds" "$max_bytes" \
        "$TARGET" "$PORT" "$identity" "$known_hosts"; then
        fail "$name unexpectedly succeeded"
    fi
    rg -q '^remote_client_result=failed ' "$output" \
        || fail "$name did not produce a structured failure"
    rg -q "$expected_detail" "$output" \
        || fail "$name produced the wrong failure classification"
    printf 'ssh_failure_%s=pass\n' "$name"
}

expect_failure host_key "$REMOTE_PROBE" 3 65536 "$IDENTITY" "$wrong_known_hosts" 'status\\ 255'
expect_failure authentication "$REMOTE_PROBE" 3 65536 "$wrong_key" "$KNOWN_HOSTS" 'status\\ 255'
expect_failure timeout "$timeout_probe" 1 65536 "$IDENTITY" "$KNOWN_HOSTS" 'status\\ 124'
expect_failure output_limit "$oversize_probe" 3 1024 "$IDENTITY" "$KNOWN_HOSTS" 'exceeds\\ 1024'
expect_failure missing_dependency "$dependency_probe" 3 65536 "$IDENTITY" "$KNOWN_HOSTS" 'status\\ 127'
expect_failure interrupted_cleanup "$interrupt_probe" 1 65536 "$IDENTITY" "$KNOWN_HOSTS" 'status\\ 124'

deadline=$((SECONDS + 3))
orphan_risk=none
while [[ -e $interrupt_root ]]; do
    if (( SECONDS >= deadline )); then
        orphan_risk=detected
        if [[ -S $interrupt_socket ]]; then
            tmux -S "$interrupt_socket" kill-server >/dev/null 2>&1 || true
        fi
        rm -f -- "$interrupt_socket" "$interrupt_root/config"
        rmdir -- "$interrupt_root" 2>/dev/null || true
        [[ ! -e $interrupt_root ]] || fail 'scoped orphan recovery did not remove the remote resource'
        break
    fi
    sleep 0.05
done
printf 'ssh_failure_interrupted_orphan_risk=%s\n' "$orphan_risk"
printf 'ssh_failure_interrupted_recovery=pass\n'

malformed_dir="$case_root/leyline-tmux.malformed"
malformed_output="$malformed_dir/remote.txt"
mkdir "$malformed_dir"
"$REMOTE_CLIENT" "$malformed_output" "$malformed_probe" 3 65536 \
    "$TARGET" "$PORT" "$IDENTITY" "$KNOWN_HOSTS"
if bash "$REMOTE_OUTPUT_VALIDATE" "$malformed_output" >/dev/null 2>&1; then
    fail 'malformed remote output passed schema validation'
fi
printf 'ssh_failure_malformed_output=pass\n'

printf 'ssh_failure_matrix_result=passed\n'
