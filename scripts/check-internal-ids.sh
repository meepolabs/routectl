#!/usr/bin/env bash
# Internal-ID scanner: blocks high-signal internal planning / review IDs
# from entering tracked content (code or commit messages). These tokens
# are meaningful only inside private planning docs; they must never reach
# a public mirror of this repo.
#
# Single source of truth for the patterns -- both commit-gate stages
# (pre-commit and commit-msg) and the CI guard all call this script so the
# rule set lives in exactly one place.
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
#
# This script excludes ITSELF for the same reason: it IS the rule set, so
# its pattern literals are indistinguishable from real leaks and any diff
# that edits the rule set would block its own commit. Accepted cost: the
# scanner's own source is never scanned for genuine leaks.
EXCLUDE_PATHS=(
    "crates/routectl-cli/tests/fixtures/captured/"
    "crates/routectl-router/catalog_data/"
    "scripts/check-internal-ids.sh"
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
# L\d / H\d -- those collide with ordinary prose and line refs. Each entry
# is the CORE of an extended-regex (grep -E) alternative; the surrounding
# whole-token boundaries are added by `joined_pattern` so the rule set
# stays readable here.
#
# Boundaries are POSIX-portable, NOT `\b`: `\b` is a GNU grep extension
# that BSD/macOS grep does not honor, which would let a local hook
# false-green on a Mac. Each core is wrapped as
# `(^|[^[:alnum:]_])(<core>)([^[:alnum:]_]|$)` so a standalone token is
# caught while a token embedded in a larger identifier (e.g.
# `xR2-EXAMPLEy`, `myRV-99thing`) is NOT -- identical on GNU and BSD.
#
# The last three cores catch the planning-shorthand class (task /
# feature / decision ids). CAUTION: bare `f<n>` is NOT catchable -- it
# collides with the Rust float types `f32` / `f64` -- so only the SAFE
# forms are matched: `f<n>.<nn>` task shorthand (a dotted two-digit
# suffix a float literal never carries in that boundary), `(pre-|post-)f<n>`
# planning commentary, and standalone `D<nn>` decision shorthand (the
# token boundary keeps `d17_tail`-style identifiers and hex bytes clear).
#
# The stage-label cores are NARROWED to the spellings that actually occurred,
# because this scanner BLOCKS commits and a false positive on legitimate prose
# is a developer-facing outage. Measured against the labels the 2026-08-11
# sweep removed, which were `SLICE 1`, `SLICE 2`, `SLICE 3`, `Slice-2`, and
# `slice 2's`:
#
#   - `SLICE <n>` all-caps with a space: a label, never prose.
#   - `[Ss]lice-<n>` hyphenated: a label; prose says "slice 2", not "slice-2".
#   - `[Ss]lice <n>'s` possessive: the "matches slice 2's grouping" form.
#
# DELIBERATELY NOT matched: lowercase `slice <n>` with a plain space. It
# collides with legitimate technical prose ("copy the second buffer into slice
# 2 of the ring"), and blocking that is worse than missing a label a reviewer
# can catch. Same reason `(R<n>)` is gone entirely: "conformance with external
# requirement (R2)" is a legitimate sentence, and the `R2-` core above still
# catches the prefixed form this repo actually used in identifiers.
#
# The cores spell their own character classes rather than relying on `grep -i`,
# because `scan_text` runs one case-SENSITIVE pattern per tier.
# MEASURED: all cores return zero lines across all tracked files minus
# `EXCLUDE_PATHS`.
#
# KNOWN COVERAGE GAP, accepted: short lowercase prefix-hyphen-token ids
# (a two-letter lowercase prefix, a hyphen, then a slug or digits) are NOT
# matched by either tier. That shape is indistinguishable from ordinary
# hyphenated lowercase identifiers and slugs in tracked content, so a
# pattern for it would block legitimate commits. Keeping ids of that shape
# out of code and commit messages is the author's and reviewer's job, not
# this gate's.
PATTERNS=(
    # ORG-WIDE decision and tracking ids. Unlike every other core in this
    # tier these are NOT routectl's -- they are conventions every project
    # here uses, which is exactly why they leak: an id pasted from a
    # decision log or a board reads as harmless prose in a comment. Found
    # in a tracked comment in this repo (an internal tracking id in a test
    # header) and, per a fleet scan, in tracked files across most repos,
    # several of them public.
    #
    # MEASURED collision-free, so they need no per-repo tuning: across all
    # tracked files, `DEC-` followed by letters returns zero (no DECIMAL /
    # DECODE hit, since the core requires a hyphen THEN digits), and `MEE-`
    # followed by letters returns zero.
    #
    # The digit count is BOUNDED at 3, which is not cosmetic. An unbounded
    # `DEC-[0-9]+` matches the first three digits of `DEC-2024`, so a
    # legitimate date-shaped token would be refused as an internal id --
    # verified as a real false positive before this bound was added, using
    # `DEC-2024 archive format support` as the fixture. Real ids are
    # three digits today and a fourth would be a numbering change worth
    # noticing here rather than absorbing silently.
    'DEC-[0-9]{1,3}'
    'MEE-[0-9]{1,3}'
    'R2-[A-Za-z0-9][A-Za-z0-9_-]*'
    'RV-[0-9]+'
    'T-(BREAKER|GATE|CLONE|DENY|ALIAS|ZERO|SSRF)'
    'TODO\(M[0-9]{1,3}(-[A-Za-z0-9_-]+)?\)'
    'M[0-9]+\.[0-9]+'
    'H[0-9]{1,3} (fix|invariant)'
    'f[0-9]+\.[0-9]{2}'
    '(pre-|post-)f[0-9]+'
    'D[0-9]{2}'
    'SLICE [0-9]{1,3}'
    'SLICE-[0-9]{1,3}'
    '[Ss]lice-[0-9]{1,3}'
    "[Ss]lice [0-9]{1,3}'s"

    # INTERNAL DOCUMENT references, not ids. Added after this class leaked
    # FOUR times in one feature's batch and passed every hook each time:
    # the tiers above look for identifier SHAPES, and a private doc's
    # filename or a table label inside it is ordinary prose to them. The
    # leak reads as helpful provenance ("classified TRANSLATION per Table
    # A"), which is exactly why an author writes it and a reviewer skims
    # past it -- and the documents named here state on their own first line
    # that they never enter the code repo.
    #
    # The right way to cite that reasoning in shipped code is to RESTATE it
    # in terms a reader of the code can check, never to point at a file
    # they cannot open.
    #
    # MEASURED collision-free across every tracked .rs/.sh/.py/.toml/.md
    # file (982 tracked files): zero matches outside this scanner. Two
    # bounds are load-bearing rather than cosmetic:
    #   - `Table [AB]` needs its leading word boundary: without it,
    #     "...for the mutable Table Api" and similar prose would match.
    #   - `llm_context` needs a trailing `/` AND a non-letter to its left:
    #     bare `llm_context` false-matched `litellm_context`, a real
    #     function name in the catalog codegen, verified before this bound
    #     was added.
    'Table-[AB]'
    'Table [AB]'
    'lane-contract'
    'foundations\.md'
    'llm_context/'
)

# Second tier: same whole-token wrapping, but the LEFT boundary also
# excludes `-` (`(^|[^[:alnum:]_-])`). This tier exists because its cores
# are short enough to collide with hyphenated vendor model names.
# MEASURED: with the default left boundary, `minimax/MiniMax-M3` and
# `models_dev_model: "MiniMax-M3"` false-match the bare `M<n>` core; the
# hyphen-excluding left boundary drops both to zero while still catching
# `the M1 recorder` and `M3 generation`. Accepted cost, by design:
# `pre-M1`-style and `M3_BODY`-style identifier-embedded tokens do NOT
# match this tier -- those were removed by a one-time tree-wide scrub, and
# this gate is not their safety net.
#
# The existing PATTERNS tier MUST keep its own boundary: `R2-`, `RV-`, and
# `(pre-|post-)f<n>` legitimately sit after or contain a hyphen.
#
# Bare `T<n>` is MEASURED-safe here (this supersedes the older refusal to
# match it): across all tracked files, minus `catalog_data/` and this
# scanner's self-test, whole-token `T<n>` returned exactly 6 lines, every
# one an internal label the scrub removed -- zero generic-parameter or
# type-name collisions. Bare `F<n>` stays uncatchable: `FailurePhase::F1`
# / `F2` / `F3` are real enum variants.
PATTERNS_NO_HYPHEN=(
    'M[0-9]{1,3}'
    'T[0-9]{1,3}'
    'later (increment|phase|milestone)'
    'this milestone'
)

# Join the cores of both tiers into one ERE: each core is wrapped in
# whole-token boundaries (the second tier's left boundary additionally
# excludes `-`), then all are OR-ed with `|`.
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
    for p in "${PATTERNS_NO_HYPHEN[@]}"; do
        local wrapped="(^|[^[:alnum:]_-])($p)([^[:alnum:]_]|\$)"
        out="$out|$wrapped"
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
