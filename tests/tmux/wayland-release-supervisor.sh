#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 7 ]]; then
    printf 'usage: %s REPO_ROOT LEYLINE_BIN PROBE RESULT LOG STATUS TIMEOUT\n' "$0" >&2
    exit 64
fi

readonly REPO_ROOT=$1
readonly LEYLINE_BIN=$2
readonly PROBE=$3
readonly RESULT=$4
readonly LOG=$5
readonly STATUS=$6
readonly TIMEOUT_SECONDS=$7
readonly DONE="$RESULT.done"

leyline_pid=
stop_leyline() {
    [[ -n $leyline_pid ]] || return 0
    kill "$leyline_pid" >/dev/null 2>&1 || true
    timeout 3s tail --pid="$leyline_pid" -f /dev/null >/dev/null 2>&1 \
        || kill -KILL "$leyline_pid" >/dev/null 2>&1 || true
    wait "$leyline_pid" 2>/dev/null || true
}
trap stop_leyline EXIT INT TERM

cd "$REPO_ROOT"
env -u TMUX "$LEYLINE_BIN" -vv -e bash "$PROBE" "$RESULT" >"$LOG" 2>&1 &
leyline_pid=$!

deadline=$((SECONDS + TIMEOUT_SECONDS))
while [[ ! -s $DONE ]] && kill -0 "$leyline_pid" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
        printf '124\n' >"$STATUS"
        exit 0
    fi
    sleep 0.1
done

if [[ -s $DONE ]]; then
    probe_status=$(<"$DONE")
    printf '%s\n' "$probe_status" >"$STATUS"
else
    set +e
    wait "$leyline_pid" 2>/dev/null
    leyline_status=$?
    set -e
    leyline_pid=
    printf '%s\n' "$leyline_status" >"$STATUS"
fi
