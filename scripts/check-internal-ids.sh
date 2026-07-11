#!/usr/bin/env bash
# Internal-ID scanner: blocks high-signal internal planning / review IDs
# from entering tracked content (code or commit messages). These tokens
# are meaningful only inside private planning docs; they must never reach
# a public mirror of this repo.
#
# Single source of truth for the patterns -- the pre-commit hook,
# commit-msg hook, and CI guard all call this script so the rule set
# lives in exactly one place.
#
# Modes:
#   --staged            Scan ADDED lines of the staged diff (code path).
#   --commit-msg FILE   Scan a commit-message file.
#   --range A..B        Scan ADDED lines of `git diff A..B` (CI PR path).
#   --commit-range A..B Scan commit messages in `git log A..B` (CI path).
#
# Local bypass: ROUTECTL_SKIP_ID_SCAN=1 exits 0 without scanning. CI MUST
# NOT set this (the guard fails closed).
#
# Exit codes: 0 = clean, 1 = a banned token was found, 2 = usage error.

set -euo pipefail

if [[ "${ROUTECTL_SKIP_ID_SCAN:-0}" == "1" ]]; then
    echo "check-internal-ids: ROUTECTL_SKIP_ID_SCAN=1, skipping"
    exit 0
fi

# Captured replay fixtures and vendored catalog snapshots hold real
# upstream model ids, token counts, and UUIDs that would false-trip the
# high-signal patterns (e.g. vendor model names shaped like M<n>.<m>).
# They are not author-written content, so exclude those trees from the
# scan. The scanner's own self-test carries synthetic ID-shaped fixtures
# by design, so it is excluded too.
EXCLUDE_PATHS=(
    "crates/routectl-cli/tests/fixtures/captured/"
    "crates/routectl-router/catalog_data/"
    "scripts/check-internal-ids.test.sh"
)

# True (0) when a path is excluded. Directory entries (ending in `/`)
# exclude any path UNDER that prefix; file entries match by EXACT
# equality only, so `scripts/check-internal-ids.test.sh` does not also
# exempt `scripts/check-internal-ids.test.sh.bak`.
is_excluded() {
    local path="$1"
    local ex
    for ex in "${EXCLUDE_PATHS[@]}"; do
        case "$ex" in
            */)
                # Directory prefix: match this dir or anything under it.
                if [[ "$path" == "$ex"* ]]; then
                    return 0
                fi
                ;;
            *)
                # File entry: exact match only.
                if [[ "$path" == "$ex" ]]; then
                    return 0
                fi
                ;;
        esac
    done
    return 1
}

# High-signal, anchored patterns ONLY. Deliberately NOT matching bare
# L\d / T\d / H\d -- those collide with ordinary prose, line refs, and
# type names. Each entry is the CORE of an extended-regex (grep -E)
# alternative; the surrounding whole-token boundaries are added by
# `joined_pattern` so the rule set stays readable here.
#
# Boundaries are POSIX-portable, NOT `\b`: `\b` is a GNU grep extension
# that BSD/macOS grep does not honor, which would let a local hook
# false-green on a Mac. Each core is wrapped as
# `(^|[^[:alnum:]_])(<core>)([^[:alnum:]_]|$)` so a standalone token is
# caught while a token embedded in a larger identifier (e.g.
# `xR2-EXAMPLEy`, `myRV-99thing`) is NOT -- identical on GNU and BSD.
PATTERNS=(
    'R2-[A-Za-z0-9][A-Za-z0-9_-]*'
    'RV-[0-9]+'
    'T-(BREAKER|GATE|CLONE|DENY|ALIAS|ZERO|SSRF)'
    'TODO\(M[0-9]{1,3}(-[A-Za-z0-9_-]+)?\)'
    'M[0-9]+\.[0-9]+'
    'H[0-9]{1,3} (fix|invariant)'
)

# Join the cores into one ERE: each core is wrapped in
# whole-token boundaries, then OR-ed with `|`.
joined_pattern() {
    local out=""
    local p
    for p in "${PATTERNS[@]}"; do
        local wrapped="(^|[^[:alnum:]_])($p)([^[:alnum:]_]|\$)"
        if [[ -z "$out" ]]; then
            out="$wrapped"
        else
            out="$out|$wrapped"
        fi
    done
    printf '%s' "$out"
}

# Scan a blob of text on stdin. Prints offending lines (prefixed) and
# returns 1 if any banned token is present, 0 otherwise.
scan_text() {
    local label="$1"
    local pattern
    pattern="$(joined_pattern)"
    local matches
    # grep -E returns 1 on no-match; tolerate that without `set -e` abort.
    matches="$(grep -nE "$pattern" || true)"
    if [[ -n "$matches" ]]; then
        echo "check-internal-ids: banned internal ID(s) found in $label:" >&2
        echo "$matches" >&2
        return 1
    fi
    return 0
}

# Emit the ADDED lines (without the leading '+') of a diff on stdin,
# skipping the +++ file header and excluded paths. Tracks the current
# target file from the +++ header so excluded files contribute no added
# lines.
added_lines_from_diff() {
    local skip=0
    local line path
    while IFS= read -r line; do
        case "$line" in
            '+++ '*)
                path="${line#+++ }"
                path="${path#b/}"
                if is_excluded "$path"; then
                    skip=1
                else
                    skip=0
                fi
                ;;
            '+'*)
                if [[ "$skip" -eq 0 ]]; then
                    printf '%s\n' "${line#+}"
                fi
                ;;
        esac
    done
}

usage() {
    echo "usage: $0 --staged | --commit-msg FILE | --range A..B | --commit-range A..B | --diff-stdin" >&2
    exit 2
}

main() {
    [[ $# -ge 1 ]] || usage
    local mode="$1"
    case "$mode" in
        --staged)
            git diff --cached --unified=0 -- . | added_lines_from_diff \
                | scan_text "staged added lines"
            ;;
        --commit-msg)
            [[ $# -ge 2 ]] || usage
            scan_text "commit message" <"$2"
            ;;
        --range)
            [[ $# -ge 2 ]] || usage
            git diff --unified=0 "$2" -- . | added_lines_from_diff \
                | scan_text "diff range $2 added lines"
            ;;
        --commit-range)
            [[ $# -ge 2 ]] || usage
            git log --format=%B "$2" | scan_text "commit messages in $2"
            ;;
        --diff-stdin)
            # Test-only seam: scan a unified diff supplied on stdin
            # through the same added-lines + exclusion path the git modes
            # use, without invoking git. Exercised by the self-test.
            added_lines_from_diff | scan_text "diff (stdin) added lines"
            ;;
        *)
            usage
            ;;
    esac
}

main "$@"
