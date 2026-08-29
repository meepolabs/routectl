#!/usr/bin/env bash
# Public-API change detector: lists each library crate's public surface
# via cargo-public-api and diffs it against a checked-in baseline. CI runs
# it unconditionally and a stale baseline FAILS the build; locally it is a
# commit-gate leg that blocks only where the tooling below is installed
# (the availability probe lives in scripts/public-api-if-available.sh), so
# the local leg is an early warning and CI is the guarantee. A surface diff is
# expected whenever the author intended to change the API; the author
# regenerates the touched baselines IN THE SAME COMMIT (see
# public-api/POLICY.md).
#
# Bootstrap (one-time, per machine):
#   cargo install cargo-public-api --version 0.52.0
#   rustup toolchain install nightly-2026-07-22 --profile minimal
#
# cargo-public-api builds rustdoc JSON with a nightly toolchain. This
# script pins that nightly to PUBLIC_API_NIGHTLY (below) so the emitted
# surface is reproducible across machines and over time. cargo-public-api
# 0.52.0 has no --toolchain flag; the nightly is selected through cargo's
# `+toolchain` shorthand (`cargo +<pin> public-api ...`).
#
# Deterministic feature set per crate (see feature_args_for): every
# library crate is audited at its DEFAULT feature set. For
# routectl-providers and routectl-router that default IS the shipped
# provider surface; the other four carry no non-default features that
# affect the public API. `generate` and `--check` route through the same
# function, so the two modes can never disagree on features.
#
# Modes:
#   public-api.sh generate [crate|all]   Write/refresh baseline(s).
#   public-api.sh --check  [crate|all]   Diff live surface vs baseline;
#                                        non-zero exit on drift.
# When the crate argument is omitted it defaults to `all`. The routectl-cli
# bin crate is exempt (no library surface, no baseline).
#
# Exit codes: 0 = clean / generated, 1 = drift or generation failure,
# 2 = usage error.

set -euo pipefail

PUBLIC_API_NIGHTLY=nightly-2026-07-22

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BASELINE_DIR="$REPO_ROOT/public-api"

# Library crates carrying a baseline. routectl-cli is intentionally
# absent: it is a bin crate with no public library surface.
CRATES=(
    routectl-core
    routectl-providers
    routectl-router
    routectl-auth
    routectl-usage
    routectl-testkit
)

# Emit the cargo-public-api feature flags for a crate, one flag per line
# (empty output = default features). Shared by generate and --check.
feature_args_for() {
    case "$1" in
        # All library crates are audited at their DEFAULT feature set, so
        # no flags are emitted. A crate needing a non-default set would be
        # added here, e.g.:
        #   routectl-foo) printf '%s\n' --no-default-features --features bar ;;
        *) : ;;
    esac
}

# List a crate's public surface on stdout using the pinned nightly.
# --color=never keeps ANSI escapes out of captured baselines.
api_for() {
    local crate="$1"
    local -a feats=()
    mapfile -t feats < <(feature_args_for "$crate")
    (
        cd "$REPO_ROOT"
        cargo "+$PUBLIC_API_NIGHTLY" public-api -p "$crate" --color=never \
            ${feats[@]+"${feats[@]}"}
    )
}

# Fail if a machine-specific path leaked into the surface listing, so a
# polluted baseline is never written or accepted.
assert_no_machine_paths() {
    local file="$1"
    local crate="$2"
    if grep -nE '/home|/root|/Users' "$file"; then
        echo "public-api: machine-specific path in $crate surface; aborting" >&2
        return 1
    fi
    return 0
}

# Writes a FRESH listing per crate: the surface goes to a temp file which then
# REPLACES the baseline, so a baseline never accumulates across runs. A type
# re-exported at the crate root is therefore legitimately listed once per public
# path it is reachable through (see public-api/POLICY.md) -- those repeated
# inherent-impl lines are correct output, not residue from an earlier run, and
# hand-removing them just fails the next --check.
generate_one() {
    local crate="$1"
    local out="$BASELINE_DIR/$crate.txt"
    local tmp
    tmp="$(mktemp)"
    if ! api_for "$crate" >"$tmp"; then
        rm -f "$tmp"
        echo "public-api: failed to list surface for $crate" >&2
        return 1
    fi
    if ! assert_no_machine_paths "$tmp" "$crate"; then
        rm -f "$tmp"
        return 1
    fi
    mkdir -p "$BASELINE_DIR"
    mv "$tmp" "$out"
    chmod 0644 "$out"
    echo "public-api: wrote baseline for $crate -> ${out#"$REPO_ROOT"/}"
}

check_one() {
    local crate="$1"
    local baseline="$BASELINE_DIR/$crate.txt"
    if [[ ! -f "$baseline" ]]; then
        echo "public-api: missing baseline for $crate (run: $0 generate $crate)" >&2
        return 1
    fi
    local live
    live="$(mktemp)"
    if ! api_for "$crate" >"$live"; then
        rm -f "$live"
        echo "public-api: failed to list surface for $crate" >&2
        return 1
    fi
    if ! assert_no_machine_paths "$live" "$crate"; then
        rm -f "$live"
        return 1
    fi
    if ! diff -u "$baseline" "$live"; then
        rm -f "$live"
        echo "public-api: surface drift for $crate -- regenerate its baseline in the same commit" >&2
        return 1
    fi
    rm -f "$live"
    echo "public-api: $crate unchanged"
}

resolve_targets() {
    local sel="$1"
    if [[ "$sel" == "all" ]]; then
        printf '%s\n' "${CRATES[@]}"
        return 0
    fi
    local c
    for c in "${CRATES[@]}"; do
        if [[ "$c" == "$sel" ]]; then
            printf '%s\n' "$sel"
            return 0
        fi
    done
    echo "public-api: unknown crate '$sel' (known: ${CRATES[*]}, or 'all')" >&2
    return 2
}

usage() {
    echo "usage: $0 generate [crate|all] | --check [crate|all]" >&2
    exit 2
}

run_over() {
    local action="$1"
    local sel="$2"
    local -a targets=()
    mapfile -t targets < <(resolve_targets "$sel")
    if [[ ${#targets[@]} -eq 0 ]]; then
        return 2
    fi
    local rc=0
    local crate
    for crate in "${targets[@]}"; do
        if ! "$action" "$crate"; then
            rc=1
        fi
    done
    return "$rc"
}

main() {
    [[ $# -ge 1 ]] || usage
    local mode="$1"
    shift
    case "$mode" in
        generate)
            run_over generate_one "${1:-all}"
            ;;
        --check)
            run_over check_one "${1:-all}"
            ;;
        *)
            usage
            ;;
    esac
}

main "$@"
