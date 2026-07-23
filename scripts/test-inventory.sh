#!/usr/bin/env bash
# Named-test inventory: enumerates every test the workspace declares
# (via `cargo test -- --list`, enumeration only -- no test is executed
# and no instrumented build is needed) into one file per test binary,
# and diffs two such dumps to report the exact removed/added test names.
#
# This is an INFORMATIONAL tool. Nothing here gates a commit. Its purpose
# is to make a test-consolidation change auditable by NAME: `dump` before
# a change, `dump` after, then `diff` to see precisely which named tests
# vanished and which appeared -- a far stronger guard than watching an
# absolute test count rise or fall.
#
# Modes:
#   test-inventory.sh dump <outdir>              Enumerate into <outdir>.
#   test-inventory.sh diff <before-dir> <after-dir>
#                                                Report REMOVED/ADDED names.
#
# `dump` writes one <binary>.txt per test binary (sorted test names, one
# per line) plus SUMMARY.txt (totals, per-binary counts, git HEAD, feature
# flags). `diff` prints fully-qualified names (<binary>::<test path>) that
# are in the before set but not the after set (REMOVED) and vice versa
# (ADDED); it exits 0 only when both sets are empty, 1 otherwise. Doctests
# are intentionally excluded from the inventory (they are not part of the
# consolidation surface).
#
# Toolchain: the workspace rust-toolchain.toml pin governs. RUSTUP_TOOLCHAIN
# is deliberately left unset so the pinned stable is selected through the
# rustup proxy -- unlike the nightly public-api.sh, no override is needed.
#
# All cargo output is redirected to a file and parsed from there rather
# than through a pipe, so the full enumeration is captured intact and can
# be re-inspected after the run.
#
# Exit codes: 0 = dump ok / diff clean, 1 = diff found a delta, 2 = usage.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RAW_LOG_NAME="list-raw.log"
SUMMARY_NAME="SUMMARY.txt"
FEATURE_FLAGS="--workspace --features bedrock --release"

# Strip cargo's trailing content hash from a test-binary artifact basename
# so "routectl_core-1a2b3c4d5e6f7a8b" becomes the stable label
# "routectl_core". A 16-hex suffix is the current cargo shape; the shortest
# trailing "-<hex>" is stripped as a fallback if that ever changes.
label_from_artifact() {
    local base="${1##*/}"
    local re16='^(.+)-[0-9a-f]{16}$'
    local rehex='^(.+)-[0-9a-f]+$'
    if [[ "$base" =~ $re16 ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
    elif [[ "$base" =~ $rehex ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
    else
        printf '%s\n' "$base"
    fi
}

dump() {
    local outdir="$1"
    mkdir -p "$outdir"
    outdir="$(cd "$outdir" && pwd)"

    local target_dir="${CARGO_TARGET_DIR:-$REPO_ROOT/.cargo-target-inventory}"
    local raw="$outdir/$RAW_LOG_NAME"

    # Clear any prior dump artifacts so a re-run never mixes two enumerations.
    rm -f "$outdir"/*.txt "$raw"

    echo "test-inventory: enumerating ($FEATURE_FLAGS) -- building test binaries, this is slow" >&2
    (
        cd "$REPO_ROOT"
        CARGO_TARGET_DIR="$target_dir" \
            cargo test --workspace --features bedrock --release -- --list
    ) >"$raw" 2>&1 </dev/null

    parse_dump "$raw" "$outdir"
}

# Parse the interleaved `cargo test -- --list` log into per-binary files.
# A "Running ... (<artifact>)" line opens a binary section; a "Doc-tests"
# line closes attribution (doctests are dropped); a line ending in ": test"
# inside an open section is a test name.
parse_dump() {
    local raw="$1"
    local outdir="$2"
    local current="" line name label
    local -A seen=()
    local re_running='Running[[:space:]].*\(([^)]+)\)'
    local re_doctests='^[[:space:]]*Doc-tests[[:space:]]'

    while IFS= read -r line; do
        if [[ "$line" =~ $re_running ]]; then
            label="$(label_from_artifact "${BASH_REMATCH[1]}")"
            current="$label"
            if [[ -z "${seen[$label]:-}" ]]; then
                : >"$outdir/$label.txt"
                seen[$label]=1
            fi
        elif [[ "$line" =~ $re_doctests ]]; then
            current=""
        elif [[ -n "$current" && "$line" == *": test" ]]; then
            name="${line%: test}"
            printf '%s\n' "$name" >>"$outdir/$current.txt"
        fi
    done <"$raw"

    if [[ ${#seen[@]} -eq 0 ]]; then
        echo "test-inventory: no test binaries found in cargo output (see $raw)" >&2
        return 1
    fi

    local total=0 count f label_out
    local summary="$outdir/$SUMMARY_NAME"
    : >"$summary"
    for f in "$outdir"/*.txt; do
        [[ "$(basename "$f")" == "$SUMMARY_NAME" ]] && continue
        LC_ALL=C sort -o "$f" "$f"
        count=$(wc -l <"$f")
        total=$((total + count))
    done

    local head_sha
    head_sha="$(cd "$REPO_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"

    {
        echo "test inventory summary"
        echo "git HEAD: $head_sha"
        echo "feature flags: $FEATURE_FLAGS"
        echo "binaries: ${#seen[@]}"
        echo "total named tests: $total"
        echo
        echo "per-binary counts:"
        for f in "$outdir"/*.txt; do
            label_out="$(basename "$f" .txt)"
            [[ "$label_out.txt" == "$SUMMARY_NAME" ]] && continue
            printf '  %-48s %d\n' "$label_out" "$(wc -l <"$f")"
        done | LC_ALL=C sort
    } >"$summary"

    echo "test-inventory: wrote ${#seen[@]} binaries, $total named tests -> ${outdir}" >&2
    echo "test-inventory: summary at ${summary}" >&2
}

# Emit sorted "<binary>::<test path>" lines for every per-binary file in a
# dump directory, skipping the summary and raw log.
fqns_for_dir() {
    local dir="$1"
    local f label t
    for f in "$dir"/*.txt; do
        [[ -e "$f" ]] || continue
        [[ "$(basename "$f")" == "$SUMMARY_NAME" ]] && continue
        label="$(basename "$f" .txt)"
        while IFS= read -r t; do
            [[ -n "$t" ]] && printf '%s::%s\n' "$label" "$t"
        done <"$f"
    done | LC_ALL=C sort -u
}

diff_dumps() {
    local before_dir="$1"
    local after_dir="$2"
    if [[ ! -d "$before_dir" ]]; then
        echo "test-inventory: before-dir not found: $before_dir" >&2
        return 2
    fi
    if [[ ! -d "$after_dir" ]]; then
        echo "test-inventory: after-dir not found: $after_dir" >&2
        return 2
    fi

    local before after removed added
    before="$(mktemp)"
    after="$(mktemp)"
    removed="$(mktemp)"
    added="$(mktemp)"
    # shellcheck disable=SC2064
    trap "rm -f '$before' '$after' '$removed' '$added'" RETURN

    fqns_for_dir "$before_dir" >"$before"
    fqns_for_dir "$after_dir" >"$after"

    LC_ALL=C comm -23 "$before" "$after" >"$removed"
    LC_ALL=C comm -13 "$before" "$after" >"$added"

    local n_removed n_added
    n_removed=$(wc -l <"$removed")
    n_added=$(wc -l <"$added")

    echo "REMOVED (in before, not in after): $n_removed"
    if [[ "$n_removed" -gt 0 ]]; then
        sed 's/^/  - /' "$removed"
    fi
    echo "ADDED (in after, not in before): $n_added"
    if [[ "$n_added" -gt 0 ]]; then
        sed 's/^/  + /' "$added"
    fi

    if [[ "$n_removed" -eq 0 && "$n_added" -eq 0 ]]; then
        return 0
    fi
    return 1
}

usage() {
    echo "usage: $0 dump <outdir> | diff <before-dir> <after-dir>" >&2
    exit 2
}

main() {
    [[ $# -ge 1 ]] || usage
    local mode="$1"
    shift
    case "$mode" in
        dump)
            [[ $# -eq 1 ]] || usage
            dump "$1"
            ;;
        diff)
            [[ $# -eq 2 ]] || usage
            diff_dumps "$1" "$2"
            ;;
        *)
            usage
            ;;
    esac
}

main "$@"
