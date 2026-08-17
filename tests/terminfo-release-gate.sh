#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
readonly SCHEMA_VERSION=1
case_root=
declare -a failed_cases=()

cleanup() {
    local status=0
    if [[ -n $case_root && -d $case_root ]]; then
        [[ $(basename -- "$case_root") == leyline-terminfo-gate.* ]] || return 1
        rm -rf -- "$case_root" || status=1
    fi
    printf 'cleanup_verdict=%s\n' "$([[ $status == 0 ]] && printf pass || printf fail)"
    return "$status"
}
trap cleanup EXIT

run_case() {
    local name=$1
    shift
    printf 'case=%s status=running\n' "$name"
    if "$@"; then
        printf 'case=%s status=pass\n' "$name"
    else
        failed_cases+=("$name")
        printf 'case=%s status=fail\n' "$name" >&2
    fi
}

source_gate() {
    ! rg -n '^[[:space:]]*use=' terminfo/leyline.terminfo
    ! rg -n '(^|[[:space:]])(Ms|Cs|Cr|Ss|Se|initc|mc0|mc4|mc5)=' terminfo/leyline.terminfo
    tic -x -o "$case_root/terminfo" terminfo/leyline.terminfo
    TERMINFO="$case_root/terminfo" TERMINFO_DIRS="$case_root/terminfo" \
        infocmp -x -1 leyline-256color >"$case_root/normalized"
    rg -qx '[[:space:]]*RGB,' "$case_root/normalized"
}

install_gate() {
    local database="$case_root/user-terminfo"
    target/release/leyline terminfo install --user --database "$database"
    target/release/leyline terminfo check --database "$database"
    target/release/leyline terminfo install --user --database "$database"
    target/release/leyline terminfo uninstall --user --database "$database"
    if target/release/leyline terminfo check --database "$database"; then
        return 1
    fi
}

wayland_gate() {
    [[ ${XDG_SESSION_TYPE:-} == wayland && -n ${WAYLAND_DISPLAY:-} ]]
    TERMINFO="$case_root/terminfo" TERMINFO_DIRS="$case_root/terminfo:" \
        tests/tmux/wayland-release-run.sh target/release/leyline
}

cd -- "$REPO_ROOT"
umask 077
case_root=$(mktemp -d "${TMPDIR:-/tmp}/leyline-terminfo-gate.XXXXXXXX")
mkdir -p "$case_root/terminfo"

for command in cargo cargo-deny infocmp rg tic tmux; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'gate_result=failed missing_command=%s\n' "$command" >&2
        exit 1
    }
done

printf 'schema_version=%s\n' "$SCHEMA_VERSION"
run_case source-and-manifest source_gate
run_case rust-fmt cargo fmt --all -- --check
run_case rust-clippy cargo clippy --workspace --all-targets -- -D warnings
run_case rust-tests cargo test --workspace --locked
run_case vte-tests cargo test --manifest-path third_party/vte/Cargo.toml --locked
run_case release-build cargo build --workspace --release --locked
run_case dependency-policy cargo deny check
run_case install-lifecycle install_gate
run_case tmux-rgb bash tests/tmux/terminfo-prototype-run.sh
run_case tmux-scene bash tests/tmux/scene-fixture-run.sh
run_case unsupported-protocols bash tests/tmux/extended-keys-run.sh
run_case tmux-local bash tests/tmux/harness.sh local
run_case tmux-nested bash tests/tmux/harness.sh nested
run_case tmux-recursive bash tests/tmux/harness.sh recursive 3
run_case ssh-loopback bash tests/tmux/loopback-run.sh
run_case ssh-isolated-r1-lr1 bash tests/tmux/isolated-remote-run.sh
run_case wayland-snapshot wayland_gate

if ((${#failed_cases[@]})); then
    printf 'gate_result=failed failed_cases=' >&2
    printf '%s,' "${failed_cases[@]}" >&2
    printf '\n' >&2
    exit 1
fi
printf 'gate_result=pass schema_version=%s\n' "$SCHEMA_VERSION"
