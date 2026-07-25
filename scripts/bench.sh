#!/usr/bin/env bash
# Perf bench runner + baseline capture.
#
# Runs the workspace's three criterion bench targets (wall-time) plus the
# deterministic allocation-count pass, applies a two-run noise contract, and
# writes a distilled baseline record to an output directory.
#
# Usage:
#   scripts/bench.sh [OUTPUT_DIR]
#
#   OUTPUT_DIR  Directory the baseline record is written to.
#               Default: <repo>/bench-results (gitignored). The operator or
#               a runbook passes a durable path when archiving a baseline;
#               this script never hardcodes such a path.
#
# Optional environment:
#   BENCH_WORKDIR      Base dir for the per-run hermetic scratch (HOME/XDG
#                      redirect + logs). Default: a sibling of the repo root
#                      on real disk. NEVER /tmp -- that is often tmpfs (RAM),
#                      which distorts any bench that touches the filesystem.
#   CARGO_TARGET_DIR   Build/criterion output dir. Default: a repo-local
#                      real-disk dir (gitignored via .cargo-target-*).
#
# Output record (OUTPUT_DIR/baseline-<shortsha>[-dirty].txt):
#   - metadata header: commit sha + dirty flag, toolchain (rustc --version),
#     build flags/profile, the fixture-profile catalog, loadavg before/after,
#     CPU count + load threshold.
#   - a summary table: bench | median run1 ns/op | median run2 ns/op |
#     allocs/op | bytes/op | status (stable/UNSTABLE).
#
# Bench naming: the pinned `<stage>__<profile>__<dialect>` convention (see
# the bench sources and the SpectrumProfile enum). Names are STABLE -- the
# baseline record and downstream feature reports cite them verbatim.
#
# Median source: criterion writes per-benchmark estimates to
#   <CRITERION_HOME>/<bench>/<baseline>/estimates.json
# and `.median.point_estimate` is the per-iteration median in nanoseconds.
# Reading that JSON with jq is cheaper and more robust than scraping the
# human-formatted console output (which auto-scales units us/ms/ns), so the
# script saves each wall-time run under its own criterion baseline label and
# reads the medians back with jq.
#
# Noise contract: the wall-time suite runs TWICE (run1, run2). Per bench the
# two medians must agree within 5% (relative to the smaller). If any bench is
# out of tolerance the whole suite reruns once more (run3); a flagged bench is
# then judged by the BEST pairwise agreement across the three runs -- if any
# two runs agree within 5% the spread was a transient and the bench is
# stable, otherwise it is recorded UNSTABLE. The alloc-count pass is
# deterministic (byte-stable fixtures, single-threaded, no clock/entropy) so
# it is run once and never needs a noise contract.
#
# Load guard: the 1-minute loadavg is recorded before and after. If it exceeds
# ncpu/2 at the start the record is tagged elevated-load (WARN) but the run is
# NOT aborted and NO scheduler tricks (nice/taskset) are applied -- changing
# scheduling priority would change the measurement; recording the load is the
# honest option.
#
# Hermeticity: HOME and every XDG_* dir are redirected into a fresh per-run
# scratch dir on real disk BEFORE any cargo/bench invocation, so no bench can
# read or write ambient user config/cache. CARGO_HOME and RUSTUP_HOME are
# captured from the original HOME first so the pinned toolchain and the
# registry cache still resolve.
#
# Toolchain: the workspace rust-toolchain.toml pin governs. RUSTUP_TOOLCHAIN
# is left unset so the pinned stable is selected through the rustup proxy.
#
# All cargo output is redirected to log files under the scratch dir.
#
# Exit codes: 0 = ran to completion (benches may be tagged UNSTABLE without
# failing the run), 2 = usage error, 3 = environment/build failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# (crate, bench-target) pairs, index-aligned. These are SHIPPED targets.
BENCH_CRATES=(routectl-cli routectl-router routectl-providers)
BENCH_TARGETS=(hotpath dispatch_clone egress)

# The fixture-profile catalog (SpectrumProfile snake names), pinned. Recorded
# in the metadata header so a baseline documents the spectrum it covered.
FIXTURE_PROFILES="tool_heavy large_image plain_round_trip no_marker cache_less long_session"

# Per-bench noise tolerance, percent (medians agree within this -> stable).
TOLERANCE_PCT=5

usage() {
    echo "usage: $0 [--profile heap] [OUTPUT_DIR]" >&2
    exit 2
}

fail() {
    echo "bench.sh: $*" >&2
    exit 3
}

# Percent difference of two positive numbers relative to the smaller, printed
# as a plain number (e.g. "3.42"). Used for the noise-contract comparison.
pct_diff() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        if (a <= 0 || b <= 0) { print "100"; exit }
        d = a - b; if (d < 0) d = -d;
        m = (a < b) ? a : b;
        printf "%.2f", (d / m) * 100;
    }'
}

# True (exit 0) if the two medians agree within TOLERANCE_PCT.
within_tolerance() {
    awk -v p="$(pct_diff "$1" "$2")" -v t="$TOLERANCE_PCT" 'BEGIN { exit !(p <= t) }'
}

# median <criterion_home> <bench> <baseline_label> -> per-iteration ns.
median_for() {
    local home="$1" bench="$2" label="$3"
    local f="$home/$bench/$label/estimates.json"
    [[ -f "$f" ]] || return 1
    jq -e '.median.point_estimate' "$f"
}

main() {
    # Arg parse: an optional `--profile heap` selects the dhat heap-profile
    # suite; with no --profile the behaviour is byte-identical to before.
    local profile=""
    local -a positional=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --profile)
                [[ $# -ge 2 ]] || usage
                profile="$2"; shift 2 ;;
            --profile=*)
                profile="${1#--profile=}"; shift ;;
            --)
                shift; while [[ $# -gt 0 ]]; do positional+=("$1"); shift; done ;;
            -*)
                usage ;;
            *)
                positional+=("$1"); shift ;;
        esac
    done
    set -- ${positional[@]+"${positional[@]}"}
    [[ $# -le 1 ]] || usage
    [[ -z "$profile" || "$profile" == "heap" ]] \
        || { echo "bench.sh: unknown --profile '$profile' (only 'heap' is supported)" >&2; usage; }
    local output_dir="${1:-$REPO_ROOT/bench-results}"

    command -v cargo >/dev/null || fail "cargo not found on PATH"
    command -v jq >/dev/null || fail "jq not found on PATH (required to read criterion medians)"

    # Capture the real toolchain/registry homes BEFORE redirecting HOME.
    local orig_home="${HOME:-}"
    export CARGO_HOME="${CARGO_HOME:-$orig_home/.cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-$orig_home/.rustup}"

    # Fresh per-run hermetic scratch on real disk (never /tmp).
    local workdir_base="${BENCH_WORKDIR:-$(dirname "$REPO_ROOT")/.routectl-bench-work}"
    mkdir -p "$workdir_base"
    local run_dir
    run_dir="$(mktemp -d "$workdir_base/run-XXXXXX")" || fail "could not create run dir under $workdir_base"

    # Redirect HOME + all XDG dirs into the scratch so no bench touches
    # ambient user state. Create each dir so tools that stat them succeed.
    export HOME="$run_dir/home"
    export XDG_CONFIG_HOME="$run_dir/xdg/config"
    export XDG_CACHE_HOME="$run_dir/xdg/cache"
    export XDG_DATA_HOME="$run_dir/xdg/data"
    export XDG_STATE_HOME="$run_dir/xdg/state"
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"

    # Repo-local real-disk build + criterion output (gitignored).
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/.cargo-target-bench}"
    export CRITERION_HOME="$CARGO_TARGET_DIR/criterion"

    local ncpu threshold loadavg_before
    ncpu="$(nproc 2>/dev/null || echo 1)"
    threshold="$(awk -v n="$ncpu" 'BEGIN { printf "%.2f", n / 2 }')"
    loadavg_before="$(cut -d' ' -f1 </proc/loadavg 2>/dev/null || echo unknown)"
    local load_tag="ok"
    if awk -v l="$loadavg_before" -v t="$threshold" 'BEGIN { exit !(l > t) }' 2>/dev/null; then
        load_tag="WARN elevated-load"
        echo "bench.sh: WARNING 1-min loadavg $loadavg_before exceeds threshold $threshold (ncpu/2); tagging record" >&2
    fi

    local sha shortsha dirty toolchain
    sha="$(cd "$REPO_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"
    shortsha="$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    if [[ -n "$(cd "$REPO_ROOT" && git status --porcelain 2>/dev/null)" ]]; then
        dirty="dirty"
    else
        dirty="clean"
    fi
    toolchain="$(rustc --version 2>/dev/null || echo unknown)"

    echo "bench.sh: run dir $run_dir" >&2
    echo "bench.sh: target dir $CARGO_TARGET_DIR" >&2

    # Heap-profile suite: attribution only, single run per target, no
    # baseline record. Everything above (hermetic HOME/XDG + target dir +
    # sha/dirty + load tag) is shared with the default path.
    if [[ "$profile" == "heap" ]]; then
        run_dhat_suite "$output_dir" "$shortsha" "$dirty"
        local loadavg_after_heap
        loadavg_after_heap="$(cut -d' ' -f1 </proc/loadavg 2>/dev/null || echo unknown)"
        echo "bench.sh: heap profile complete (loadavg before=$loadavg_before after=$loadavg_after_heap [$load_tag])" >&2
        return
    fi

    run_wall_suite run1
    run_wall_suite run2

    # Enumerate bench names from criterion's run1 output.
    local -a bench_names=()
    local d name
    for d in "$CRITERION_HOME"/*/; do
        name="$(basename "$d")"
        [[ -f "$CRITERION_HOME/$name/run1/estimates.json" ]] || continue
        bench_names+=("$name")
    done
    [[ ${#bench_names[@]} -gt 0 ]] || fail "no criterion estimates found under $CRITERION_HOME"

    # Collect medians and decide per-bench stability.
    declare -A m1 m2 m3 status
    local need_rerun=0
    for name in "${bench_names[@]}"; do
        m1["$name"]="$(median_for "$CRITERION_HOME" "$name" run1)" || fail "missing run1 median for $name"
        m2["$name"]="$(median_for "$CRITERION_HOME" "$name" run2)" || fail "missing run2 median for $name"
        if within_tolerance "${m1[$name]}" "${m2[$name]}"; then
            status["$name"]="stable"
        else
            status["$name"]="pending"
            need_rerun=1
        fi
    done

    if [[ "$need_rerun" -eq 1 ]]; then
        echo "bench.sh: some benches out of tolerance after 2 runs; running one rerun (run3)" >&2
        run_wall_suite run3
        for name in "${bench_names[@]}"; do
            [[ "${status[$name]}" == "pending" ]] || continue
            m3["$name"]="$(median_for "$CRITERION_HOME" "$name" run3)" || fail "missing run3 median for $name"
            if within_tolerance "${m1[$name]}" "${m2[$name]}" \
                || within_tolerance "${m1[$name]}" "${m3[$name]}" \
                || within_tolerance "${m2[$name]}" "${m3[$name]}"; then
                status["$name"]="stable"
            else
                status["$name"]="UNSTABLE"
            fi
        done
    fi

    # Deterministic allocation-count pass (once). Filled by name via nameref.
    # shellcheck disable=SC2034  # read back through write_record's namerefs
    declare -A allocs bytes
    run_alloc_suite allocs bytes

    local loadavg_after
    loadavg_after="$(cut -d' ' -f1 </proc/loadavg 2>/dev/null || echo unknown)"

    mkdir -p "$output_dir"
    output_dir="$(cd "$output_dir" && pwd)"
    local record="$output_dir/baseline-$shortsha"
    [[ "$dirty" == "dirty" ]] && record="$record-dirty"
    record="$record.txt"

    write_record "$record" "$sha" "$dirty" "$toolchain" \
        "$ncpu" "$threshold" "$loadavg_before" "$loadavg_after" "$load_tag" \
        bench_names m1 m2 m3 status allocs bytes

    echo "bench.sh: baseline written to $record" >&2
    cat "$record"
}

# Run every bench target once in wall-time mode under a given criterion
# baseline label. Uses the default (dev-only) feature set of each crate;
# egress's required anthropic-api feature is a default feature.
run_wall_suite() {
    local label="$1" i crate target log
    for i in "${!BENCH_CRATES[@]}"; do
        crate="${BENCH_CRATES[$i]}"
        target="${BENCH_TARGETS[$i]}"
        log="$run_dir/wall-$crate-$target-$label.log"
        echo "bench.sh: wall-time $crate/$target [$label]" >&2
        (
            cd "$REPO_ROOT"
            cargo bench -p "$crate" --bench "$target" -- --save-baseline "$label"
        ) >"$log" 2>&1 </dev/null || fail "wall-time run failed ($crate/$target $label); see $log"
    done
}

# Run every bench target once in allocation-count mode and fill the two
# name-keyed maps (passed by name) with allocs/op and bytes/op.
run_alloc_suite() {
    local -n out_allocs="$1"
    local -n out_bytes="$2"
    local i crate target log line bench a b
    for i in "${!BENCH_CRATES[@]}"; do
        crate="${BENCH_CRATES[$i]}"
        target="${BENCH_TARGETS[$i]}"
        log="$run_dir/alloc-$crate-$target.log"
        echo "bench.sh: alloc-count $crate/$target" >&2
        (
            cd "$REPO_ROOT"
            BENCH_ALLOC_COUNT=1 cargo bench -p "$crate" --bench "$target"
        ) >"$log" 2>&1 </dev/null || fail "alloc-count run failed ($crate/$target); see $log"
        # Lines of the form: <name> allocs=<n> bytes=<n>
        while IFS= read -r line; do
            bench="${line%% allocs=*}"
            a="${line#* allocs=}"; a="${a%% bytes=*}"
            b="${line##* bytes=}"
            # shellcheck disable=SC2034  # namerefs write back to the caller's maps
            out_allocs["$bench"]="$a"
            # shellcheck disable=SC2034
            out_bytes["$bench"]="$b"
        done < <(grep -E '^[a-z_]+__[a-z_]+__[a-z]+ allocs=[0-9]+ bytes=[0-9]+$' "$log" || true)
    done
}

# Run each bench target ONCE under the dhat heap profiler and write one
# JSON per target into the output dir, named
# dhat-<target>-<shortsha>[-dirty].json. Each target is compiled with its
# `dhat` feature (swapping the bench global allocator to dhat::Alloc) and
# invoked with BENCH_DHAT=1, so the bench main() runs every case once under
# a single profiler instead of criterion's sampling loop. A single run is
# correct: dhat attribution is deterministic on the byte-stable fixtures,
# so the wall-time noise contract does not apply -- and wall time is never
# read under dhat (the allocator + backtrace capture distort it).
run_dhat_suite() {
    local out="$1" shortsha="$2" dirty="$3"
    mkdir -p "$out"
    out="$(cd "$out" && pwd)"
    local suffix=""
    [[ "$dirty" == "dirty" ]] && suffix="-dirty"
    local i crate target json log
    for i in "${!BENCH_CRATES[@]}"; do
        crate="${BENCH_CRATES[$i]}"
        target="${BENCH_TARGETS[$i]}"
        json="$out/dhat-$target-$shortsha$suffix.json"
        log="$run_dir/dhat-$crate-$target.log"
        echo "bench.sh: heap-profile $crate/$target -> $json" >&2
        (
            cd "$REPO_ROOT"
            BENCH_DHAT=1 DHAT_OUTPUT="$json" \
                cargo bench -p "$crate" --bench "$target" --features "$crate/dhat"
        ) >"$log" 2>&1 </dev/null || fail "heap-profile run failed ($crate/$target); see $log"
    done
    echo "bench.sh: heap profiles written to $out" >&2
}

# Assemble the metadata header + summary table into the record file.
write_record() {
    local record="$1" sha="$2" dirty="$3" toolchain="$4"
    local ncpu="$5" threshold="$6" load_before="$7" load_after="$8" load_tag="$9"
    local -n names_ref="${10}"
    local -n m1_ref="${11}"
    local -n m2_ref="${12}"
    local -n m3_ref="${13}"
    local -n status_ref="${14}"
    local -n allocs_ref="${15}"
    local -n bytes_ref="${16}"

    local name r1 r2 st al by rerun_notes=""
    {
        echo "routectl perf baseline"
        echo "======================="
        echo "commit:        $sha ($dirty)"
        echo "toolchain:     $toolchain"
        echo "build profile: bench (inherits [profile.release]: lto=thin, codegen-units=1)"
        echo "build flags:   RUSTFLAGS='${RUSTFLAGS:-}' (default); per-crate default features"
        echo "bench targets: routectl-cli/hotpath routectl-router/dispatch_clone routectl-providers/egress"
        echo "profiles:      $FIXTURE_PROFILES"
        echo "cpu count:     $ncpu"
        echo "load threshold:$threshold (ncpu/2)"
        echo "loadavg 1-min: before=$load_before after=$load_after [$load_tag]"
        echo "noise contract: 2 runs, per-bench medians within ${TOLERANCE_PCT}% -> stable; else 1 rerun,"
        echo "                best pairwise agreement across 3 runs decides stable/UNSTABLE."
        echo "median source: criterion estimates.json .median.point_estimate (ns/op)."
        echo
        printf '%-42s %14s %14s %10s %12s %-9s\n' \
            "bench" "median_r1_ns" "median_r2_ns" "allocs/op" "bytes/op" "status"
        printf '%-42s %14s %14s %10s %12s %-9s\n' \
            "-----" "------------" "------------" "---------" "--------" "------"

        for name in $(printf '%s\n' "${names_ref[@]}" | LC_ALL=C sort); do
            r1="$(printf '%.0f' "${m1_ref[$name]}")"
            r2="$(printf '%.0f' "${m2_ref[$name]}")"
            al="${allocs_ref[$name]:-?}"
            by="${bytes_ref[$name]:-?}"
            st="${status_ref[$name]}"
            printf '%-42s %14s %14s %10s %12s %-9s\n' "$name" "$r1" "$r2" "$al" "$by" "$st"
            if [[ -n "${m3_ref[$name]:-}" ]]; then
                rerun_notes+="  $name: run3 median=$(printf '%.0f' "${m3_ref[$name]}") ns -> $st"$'\n'
            fi
        done

        if [[ -n "$rerun_notes" ]]; then
            echo
            echo "rerun notes (benches that triggered run3):"
            printf '%s' "$rerun_notes"
        fi
    } >"$record"
}

main "$@"
