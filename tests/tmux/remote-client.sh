#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 8 ]]; then
    printf 'usage: %s OUTPUT REMOTE_PROBE TIMEOUT MAX_BYTES TARGET PORT IDENTITY KNOWN_HOSTS\n' "$0" >&2
    exit 64
fi

readonly output=$1
readonly remote_probe=$2
readonly timeout_seconds=$3
readonly max_bytes=$4
readonly target=$5
readonly port=$6
readonly identity=$7
readonly known_hosts=$8
readonly output_parent=$(cd -- "$(dirname -- "$output")" && pwd -P)
readonly pending="$output.pending"

abort() {
    printf 'remote client refused output path: %s\n' "$1" >&2
    exit 64
}

fail() {
    rm -f -- "$pending"
    printf 'remote_client_result=failed detail=%q\n' "$1" >"$output"
    printf 'remote client failed: %s\n' "$1" >&2
    exit 1
}

[[ $(basename -- "$output_parent") == leyline-tmux.* ]] \
    || abort 'output must belong to a harness case directory'
[[ $(basename -- "$output") == remote.txt ]] || abort 'unexpected output filename'
[[ ! -e $output && ! -e $pending ]] || abort 'output already exists'
[[ -r $remote_probe ]] || fail 'remote probe is not readable'
[[ -f $identity ]] || fail 'SSH identity is not a file'
[[ -f $known_hosts ]] || fail 'known_hosts is not a file'
[[ $timeout_seconds =~ ^[1-9][0-9]*$ ]] || fail 'timeout must be a positive integer'
[[ $max_bytes =~ ^[1-9][0-9]*$ ]] || fail 'maximum output must be a positive integer'

set +e
timeout --foreground "${timeout_seconds}s" \
    ssh -F /dev/null -p "$port" \
    -o BatchMode=yes \
    -o IdentitiesOnly=yes \
    -o IdentityAgent=none \
    -o StrictHostKeyChecking=yes \
    -o "UserKnownHostsFile=$known_hosts" \
    -i "$identity" "$target" 'sh -s' <"$remote_probe" \
    | head -c "$((max_bytes + 1))" >"$pending"
statuses=("${PIPESTATUS[@]}")
set -e

readonly captured_bytes=$(wc -c <"$pending")
(( captured_bytes <= max_bytes )) || fail "remote output exceeds ${max_bytes} bytes"
(( statuses[0] == 0 )) || fail "SSH probe exited with status ${statuses[0]}"
(( statuses[1] == 0 )) || fail "output limiter exited with status ${statuses[1]}"

mv -- "$pending" "$output"
