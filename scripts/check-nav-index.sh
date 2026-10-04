#!/usr/bin/env bash
# Navigation-doc coverage check: every `crates/**/*.rs` source file (minus
# the `*_tests.rs` sidecars docs/CODEMAP.md's own header already excludes)
# and every `scripts/**/*.sh` script must appear by path in docs/CODEMAP.md
# or docs/DEVELOPMENT.md -- or, for a script, in a README.md in its own
# directory, which this repo already uses as a navigation home for the
# driver, case and profile surfaces.
#
# EXISTENCE ONLY. This never checks row wording, accuracy, or freshness --
# those have no mechanical oracle and stay with human review. It exists to
# catch the case a reviewer easily misses: a whole new file that landed
# with zero mention in either navigation doc.
#
# A Rust file is matched by its path relative to its crate root (the same
# shape CODEMAP.md rows use), searched within that crate's own "## <crate>"
# section only, so a same-named file in a different crate (`src/lib.rs` is
# every crate's) cannot mask a genuine gap. A script is matched by basename
# against both docs combined, mirroring the manual rule in CODEMAP.md's
# scripts/ section preamble.
#
# Two modes:
#
#   check-nav-index.sh            the full report: every gap, advisory.
#   check-nav-index.sh --enforce  the commit gate: fails only on a gap that
#                                 is NOT listed in check-nav-index.allowlist
#                                 beside this script, or on an allowlist
#                                 entry that is stale. Inside a git work
#                                 tree a new gap counts only once the file
#                                 is in the index (staged or committed).
#
# The allowlist records the gaps that already existed when the gate was
# wired, so a doc lag that predates it does not block every commit while a
# NEW unindexed file does. It only shrinks: an entry whose file gained a
# row, or no longer exists, fails the gate until it is removed, so a gap
# closed once cannot silently reopen behind a forgotten entry. The list
# must stay sorted and duplicate-free (`LC_ALL=C sort -u`) so every change
# to it is a one-line diff.
#
# Exit codes: 0 = every file indexed (report) or no gap outside the
# allowlist and no stale entry (--enforce), 1 = at least one such
# failure, 2 = usage error or an unreadable allowlist.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CODEMAP="$REPO_ROOT/docs/CODEMAP.md"
DEVELOPMENT="$REPO_ROOT/docs/DEVELOPMENT.md"
ALLOWLIST="$SCRIPT_DIR/check-nav-index.allowlist"

usage() {
    echo "usage: $0 [--enforce]" >&2
    exit 2
}

# Print the lines of $1 (a CODEMAP-shaped file) belonging to the
# "## <crate>" section named by $2, up to (excluding) the next top-level
# "## " heading.
crate_section() {
    local codemap="$1" crate="$2"
    awk -v heading="## $crate" '
        $0 == heading { found=1; next }
        found && /^## / { exit }
        found { print }
    ' "$codemap"
}

check_rust_files() {
    local crates_root="$1" codemap="$2" development="$3"
    local -n out_ref="$4"
    local path relpath crate rest section
    while IFS= read -r -d '' path; do
        relpath="${path#"$crates_root"/}"
        case "$(basename "$relpath")" in
            *_tests.rs) continue ;;
        esac
        crate="${relpath%%/*}"
        rest="${relpath#"$crate"/}"
        section="$(crate_section "$codemap" "$crate")"
        if ! grep -qF "$rest" <<<"$section" && ! grep -qF "$rest" "$development"; then
            out_ref+=("crates/$relpath")
        fi
    done < <(find "$crates_root" -type f -name '*.rs' -print0 | sort -z)
}

check_scripts() {
    local scripts_root="$1" codemap="$2" development="$3" repo_root="$4"
    local -n scripts_out_ref="$5"
    local path base readme
    while IFS= read -r -d '' path; do
        base="$(basename "$path")"
        if grep -qF "$base" "$codemap" || grep -qF "$base" "$development"; then
            continue
        fi
        # A README beside the script counts as a navigation home: this repo
        # already keeps per-directory READMEs for the driver, case and profile
        # surfaces, and one of them carries a CODEMAP row of its own. Only the
        # script's OWN directory qualifies -- a README one level up describes a
        # different surface.
        readme="$(dirname "$path")/README.md"
        if [ -r "$readme" ] && grep -qF "$base" "$readme"; then
            continue
        fi
        scripts_out_ref+=("${path#"$repo_root"/}")
    done < <(find "$scripts_root" -type f -name '*.sh' -print0 | sort -z)
}

# The allowlist's entries: every line that is neither blank nor a comment.
read_allowlist() {
    grep -v -e '^#' -e '^[[:space:]]*$' "$1" || true
}

# Inside a git work tree, keep only the paths of $2 that are in the index.
# pre-commit stashes unstaged edits but leaves untracked files on disk, so
# without this a scratch or generated file nobody is committing would
# block every commit. Outside a work tree every path is kept.
filter_to_index() {
    local repo_root="$1"
    local -n paths_ref="$2"
    git -C "$repo_root" rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 0
    [[ "${#paths_ref[@]}" -gt 0 ]] || return 0
    local -A tracked=()
    local path kept=()
    while IFS= read -r -d '' path; do
        tracked["$path"]=1
    done < <(git -C "$repo_root" ls-files -z -- "${paths_ref[@]}")
    for path in "${paths_ref[@]}"; do
        if [[ -n "${tracked[$path]:-}" ]]; then
            kept+=("$path")
        fi
    done
    paths_ref=("${kept[@]}")
}

# Compare the gaps found against the allowlist and exit with the verdict.
enforce_allowlist() {
    local allowlist="$1" repo_root="$2"
    local -n gaps_ref="$3"
    if [[ ! -r "$allowlist" ]]; then
        echo "check-nav-index: allowlist not readable at ${allowlist#"$repo_root"/}" >&2
        exit 2
    fi
    local entries failed=0 entry path
    entries="$(read_allowlist "$allowlist")"
    if [[ "$entries" != "$(LC_ALL=C sort -u <<<"$entries")" ]]; then
        echo "check-nav-index: ${allowlist#"$repo_root"/} is not sorted and duplicate-free (LC_ALL=C sort -u)" >&2
        failed=1
    fi

    local -A allowed=() gap_set=()
    local new=() stale=() gone=()
    while IFS= read -r entry; do
        if [[ -n "$entry" ]]; then
            allowed["$entry"]=1
        fi
    done <<<"$entries"
    for path in "${gaps_ref[@]}"; do
        gap_set["$path"]=1
        if [[ -z "${allowed[$path]:-}" ]]; then
            new+=("$path")
        fi
    done
    filter_to_index "$repo_root" new
    for entry in "${!allowed[@]}"; do
        if [[ ! -e "$repo_root/$entry" ]]; then
            gone+=("$entry")
        elif [[ -z "${gap_set[$entry]:-}" ]]; then
            stale+=("$entry")
        fi
    done

    if [[ "${#new[@]}" -gt 0 ]]; then
        echo "check-nav-index: ${#new[@]} file(s) with no navigation-doc row and not in the allowlist:" >&2
        printf '  %s\n' "${new[@]}" >&2
        echo "  Add a row to docs/CODEMAP.md or docs/DEVELOPMENT.md; the allowlist only shrinks." >&2
        failed=1
    fi
    if [[ "${#stale[@]}" -gt 0 ]]; then
        echo "check-nav-index: ${#stale[@]} stale allowlist line(s) -- now indexed; remove from the allowlist:" >&2
        printf '  %s\n' "${stale[@]}" | LC_ALL=C sort >&2
        failed=1
    fi
    if [[ "${#gone[@]}" -gt 0 ]]; then
        echo "check-nav-index: ${#gone[@]} stale allowlist line(s) -- file no longer exists; remove from the allowlist:" >&2
        printf '  %s\n' "${gone[@]}" | LC_ALL=C sort >&2
        failed=1
    fi
    if [[ "$failed" -ne 0 ]]; then
        exit 1
    fi
    echo "check-nav-index: no unindexed file outside the allowlist (${#gaps_ref[@]} allowlisted gap(s) remain)"
    exit 0
}

main() {
    local enforce=0
    case "$#:${1:-}" in
        0:) ;;
        1:--enforce) enforce=1 ;;
        *) usage ;;
    esac
    local missing=()
    check_rust_files "$REPO_ROOT/crates" "$CODEMAP" "$DEVELOPMENT" missing
    check_scripts "$REPO_ROOT/scripts" "$CODEMAP" "$DEVELOPMENT" "$REPO_ROOT" missing
    if [[ "$enforce" -eq 1 ]]; then
        enforce_allowlist "$ALLOWLIST" "$REPO_ROOT" missing
    fi
    if [[ "${#missing[@]}" -gt 0 ]]; then
        echo "check-nav-index: ${#missing[@]} file(s) with no navigation-doc row:" >&2
        printf '  %s\n' "${missing[@]}" >&2
        exit 1
    fi
    echo "check-nav-index: every crates/**/*.rs and scripts/**/*.sh is indexed"
    exit 0
}

main "$@"
