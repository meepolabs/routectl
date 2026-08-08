#!/usr/bin/env bash
# Report stray cargo build-scratch directories, in bytes, so an accumulation
# is visible before it becomes a disk emergency.
#
# WHY THIS EXISTS: agents and reviewers set a private CARGO_TARGET_DIR to dodge
# the shared target/ cargo lock (legitimate -- it keeps a parallel fan-out
# parallel), but nothing tears those dirs down. Three separate incidents:
# 193 GB across 24 orphaned dirs, then 17 GB nested INSIDE target/, then 253 GB
# of incremental debug artifacts in target/debug itself.
#
# Two of the three hiding places defeat the obvious checks:
#   - a dir at the PROJECT root (the repo's parent) sits OUTSIDE the git repo,
#     so .gitignore / git status / git clean never surface it;
#   - a dir NESTED under target/ is covered by the `target` ignore rule, so it
#     never shows up either -- and a sweep looking for siblings of target/
#     walks straight past it.
# Being gitignored was never the problem. Nothing SCANNED. This scans.
#
# Reports only: it never deletes. Deleting a build cache mid-gate has broken
# runs here twice, so the removal stays a human decision.
#
# Exit codes: 0 = nothing stray, 1 = candidates found (report printed), 2 = usage.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_ROOT="$(cd "$REPO_ROOT/.." && pwd)"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi

# A directory is cargo build scratch if it holds a CACHEDIR.TAG (cargo writes
# one) or contains nothing but build outputs. The second test matters because
# a dir created by an explicit CARGO_TARGET_DIR= on a short-lived invocation
# does not always get the tag.
is_build_scratch() {
    local d="$1"
    [[ -f "$d/CACHEDIR.TAG" ]] && return 0
    local entry
    for entry in "$d"/*; do
        [[ -e "$entry" ]] || return 1
        case "$(basename "$entry")" in
            debug|release|doc|tmp|package|CACHEDIR.TAG|.rustc_info.json) ;;
            *) return 1 ;;
        esac
    done
    return 0
}

found=0
report() {
    local d="$1" why="$2"
    local size
    size="$(du -sh "$d" 2>/dev/null | cut -f1)"
    printf '  %-8s %-34s %s\n' "$size" "$(basename "$d")" "$why"
    found=1
}

echo "stray-build-dirs: scanning"
echo
echo "in-repo scratch (siblings of target/):"
shopt -s nullglob
for d in "$REPO_ROOT"/.cargo-target-* "$REPO_ROOT"/target-* "$REPO_ROOT"/.target-*; do
    [[ -d "$d" ]] && is_build_scratch "$d" && report "$d" "gitignored, but nothing prunes it"
done

echo
echo "nested INSIDE target/ (a sibling-only sweep misses these):"
if [[ -d "$REPO_ROOT/target" ]]; then
    for d in "$REPO_ROOT"/target/*/; do
        d="${d%/}"
        case "$(basename "$d")" in
            debug|release|doc|tmp|package|flycheck*) continue ;;
        esac
        [[ -d "$d" ]] && is_build_scratch "$d" && report "$d" "nested scratch"
    done
fi

echo
echo "at the PROJECT root, OUTSIDE the git repo (git cannot see these at all):"
for d in "$PROJECT_ROOT"/*target* "$PROJECT_ROOT"/.*target*; do
    [[ -d "$d" ]] || continue
    [[ "$d" == "$REPO_ROOT"* ]] && continue
    is_build_scratch "$d" && report "$d" "outside the repo -- invisible to git"
done
shopt -u nullglob

echo
if [[ -d "$REPO_ROOT/target/debug" ]]; then
    debug_size_k="$(du -sk "$REPO_ROOT/target/debug" 2>/dev/null | cut -f1)"
    if (( debug_size_k > 50 * 1024 * 1024 )); then
        printf 'target/debug is %s of incremental artifacts.\n' \
            "$(du -sh "$REPO_ROOT/target/debug" | cut -f1)"
        echo "A cold workspace rebuild here is under a minute, so this cache is"
        echo "not worth tens of gigabytes. Consider: rm -rf target/debug"
        found=1
    fi
fi

if (( found )); then
    echo
    echo "Nothing was deleted. Verify no cargo/rustc is running (pgrep cargo rustc),"
    echo "then remove what you do not need. Prefer dropping target/debug alone over"
    echo "cargo clean, which also discards the slower-to-rebuild release cache."
    exit 1
fi

echo "no stray build dirs."
exit 0
