#!/usr/bin/env bash
# Coverage snapshot: runs cargo-llvm-cov over the workspace and writes a
# machine-readable JSON report plus a human summary into an output dir.
#
# This is an INFORMATIONAL tool -- supporting evidence only. It is NOT
# wired into the commit gate, there are NO coverage baselines checked
# into the repo, and there is NO threshold that fails a build. It is meant
# to be run by hand at a feature's base and tip, and optionally at wave
# boundaries, to observe where production behavior is exercised. The JSON
# and summary artifacts live with the feature's report inputs, not in the
# repo tree.
#
# Bootstrap (one-time, per machine):
#   cargo install cargo-llvm-cov --locked --version 0.8.7
#   rustup component add llvm-tools-preview
#
# Toolchain: the workspace rust-toolchain.toml pin governs (pinned stable,
# no nightly needed). RUSTUP_TOOLCHAIN is left unset so the pinned stable is
# selected through the rustup proxy.
#
# Note: cargo-llvm-cov excludes test sidecars (tests.rs / *_tests.rs) from
# its reports by default, which is correct here -- we measure coverage of
# production behavior, not of the tests themselves.
#
# Mode:
#   coverage.sh run <outdir>    Instrument, run tests, write coverage.json,
#                               summary.txt, run.log and git-head.txt.
#
# Exit codes: 0 = report written, 1 = cargo-llvm-cov failed, 2 = usage,
# 3 = cargo-llvm-cov not installed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LLVM_COV_VERSION=0.8.7

require_llvm_cov() {
    if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
        cat >&2 <<EOF
coverage: cargo-llvm-cov is not installed.
Install the pinned version, then re-run:
  cargo install cargo-llvm-cov --locked --version ${LLVM_COV_VERSION}
  rustup component add llvm-tools-preview
EOF
        exit 3
    fi
}

run() {
    local outdir="$1"
    mkdir -p "$outdir"
    outdir="$(cd "$outdir" && pwd)"

    local target_dir="${CARGO_TARGET_DIR:-$REPO_ROOT/.cargo-target-cov}"
    local log="$outdir/run.log"
    local json="$outdir/coverage.json"
    local summary="$outdir/summary.txt"

    # debuginfo=0 keeps the instrumented binaries small; coverage builds
    # are large. cargo-llvm-cov appends its own instrumentation flags on
    # top of this RUSTFLAGS value.
    local head_sha
    head_sha="$(cd "$REPO_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"
    printf '%s\n' "$head_sha" >"$outdir/git-head.txt"

    echo "coverage: instrumenting + running workspace tests (slow) -> $outdir" >&2

    (
        cd "$REPO_ROOT"
        CARGO_TARGET_DIR="$target_dir" \
        RUSTFLAGS="-C debuginfo=0" \
            cargo llvm-cov --workspace --features bedrock --release \
                --json --output-path "$json"
    ) >"$log" 2>&1 </dev/null

    (
        cd "$REPO_ROOT"
        CARGO_TARGET_DIR="$target_dir" \
        RUSTFLAGS="-C debuginfo=0" \
            cargo llvm-cov report --release --summary-only
    ) >"$summary" 2>>"$log" </dev/null

    echo "coverage: wrote ${json}, ${summary}, ${outdir}/git-head.txt (HEAD ${head_sha})" >&2
}

usage() {
    echo "usage: $0 run <outdir>" >&2
    exit 2
}

main() {
    [[ $# -ge 1 ]] || usage
    local mode="$1"
    shift
    case "$mode" in
        run)
            [[ $# -eq 1 ]] || usage
            require_llvm_cov
            run "$1"
            ;;
        *)
            usage
            ;;
    esac
}

main "$@"
