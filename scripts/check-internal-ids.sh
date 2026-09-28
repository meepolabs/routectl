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
#   --history A..B      Scan ADDED lines of EVERY commit in `A..B` against
#                       its first parent (CI path). Content added by one
#                       commit and removed by a later one is invisible to
#                       `--range`, which compares only the two endpoints.
#   --commit-range A..B Scan commit messages in `git log A..B` (CI path).
#   --push-inputs BEFORE
#                       Print the CI scan inputs for a push whose previous
#                       tip is BEFORE (or a PR base) as `endpoint=`,
#                       `history=`, and `messages=` lines, for `--range`,
#                       `--history`, and `--commit-range` respectively. An
#                       empty or all-zero BEFORE (a push that creates the
#                       branch) compares the empty tree to HEAD and covers
#                       every commit reachable from HEAD, root included.
#
# Every diff mode disables rename detection: a 100% rename emits no added
# lines, so a file moved from an excluded path to a scanned one would
# otherwise carry its whole content past the scan.
#
# Local bypass: ROUTECTL_SKIP_ID_SCAN=1 exits 0 without scanning. CI MUST
# NOT set this (the guard fails closed).
#
# Exit codes: 0 = clean, 1 = a banned token was found, 2 = usage error,
# 3 = scanner or tool error (grep or git failed, missing, or could not read
# its input). Only 0 means clean.

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
# scanner's own source is never scanned against the full pattern set. It is
# still scanned for the one literal neither file may ever spell, the
# private-docs directory name (see `scan_scanner_source_text`).
SCANNER_SOURCES=(
    "scripts/check-internal-ids.sh"
    "scripts/check-internal-ids.test.sh"
)
EXCLUDE_PATHS=(
    "crates/routectl-cli/tests/fixtures/captured/"
    "crates/routectl-router/catalog_data/"
    "${SCANNER_SOURCES[@]}"
)

EXIT_FOUND=1
EXIT_USAGE=2
EXIT_TOOL=3

# LINE-level exemption, deliberately not a path entry.
#
# A test whose SUBJECT is the id shape cannot avoid spelling the shape: the
# census parser's own paired controls assert that its `holds_task_id` scan
# refuses a one-digit suffix, which requires writing one. Those controls use
# this synthetic slug precisely so no real id is present.
#
# Scoped to the LINE rather than the file on purpose. Excluding the whole
# census test file would blind every OTHER core (the private-docs path, `DEC-`,
# `MEE-`, `RV-`, `M<n>.<n>`, `Table A`) on ~970 lines of actively-edited,
# prose-heavy test source -- measured: all six ride through clean under a
# file entry. That trades a five-line problem for a file-sized blind spot,
# which is the opposite of what this gate is for.
#
# The exemption is narrow by construction: a line earns it only by carrying
# this synthetic slug, which no real id can spell.
#
# RESIDUAL COST, accepted and measured: the exemption is whole-LINE, so a
# genuine leak sharing a line with the marker rides through. Keeping it
# line-scoped rather than match-scoped is what keeps this implementable in
# one `grep -vF`; the alternative re-implements match-offset bookkeeping in
# shell for a case that requires an author to write both on one line.
CONTROL_FIXTURE_MARKER='placeholder-slug.'

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
# feature / decision ids): `f<n>.<m>` task shorthand, `(pre-|post-)f<n>`
# planning commentary, and standalone `D<nn>` decision shorthand (the
# token boundary keeps `d17_tail`-style identifiers and hex bytes clear).
#
# CAUTION on the task-shorthand core: bare `f<n>` is NOT catchable -- it
# collides with the Rust float types `f16` / `f32` / `f64` / `f128`, and
# these are numeric wire-translation surfaces where `f32.0` / `f64.5`
# prose is likely. A core of `f[0-9]+\.[0-9]+` would red-fail on that
# correct prose, and a gate that blocks correct commits gets loosened or
# bypassed -- worse than any gap it closes. Do NOT "simplify" it back.
#
# The bound was originally TWO digits after the dot (`f[0-9]+\.[0-9]{2}`),
# which sidestepped the collision only because a float literal rarely
# carries two fractional digits in that boundary. That let a real id with
# a single-digit task suffix (`f2.7`, `f10.7`) through: board ids are
# zero-padded by convention, but nothing enforces the convention and
# prose drops the padding naturally.
#
# So the float widths are excluded BY SPELLING instead, PORTED FROM the
# census parser's `holds_task_id`: the digit run before the dot may be
# ANY run except the literals `16`, `32`, `64`, `128`, and the suffix is
# one or more digits. ERE has no negative lookahead, so the exclusion is
# spelled as an alternation over every other run. A LEADING-ZERO run
# (`f032.0`) is deliberately matched: it is neither a real float width
# nor plausible prose.
#
# PORTED FROM, not identical to -- do not assume parity in either
# direction. This gate additionally requires a whole-token RIGHT boundary
# (added by `joined_pattern`), while `holds_task_id` never inspects the
# character after the suffix digits. So `f2.7x` is refused by the census
# test and rides past this gate. The Rust scan is strictly stricter;
# a change made to one does not automatically hold for the other.
#
# The alternation is verified mechanically over runs 0..1500 plus wide
# runs on three engines -- if you touch it, re-verify rather than
# re-read it. The width boundaries it hinges on (15/17, 31/33, 63/65,
# 127/129) are asserted in the self-test; keep them there.
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
# The gate evaluates added lines and commit messages. Historical tracked content
# may still match a core; range and history modes prevent new matches from
# entering reachable commits.
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
    # Task shorthand `f<run>.<digits>`, where <run> is any digit run that
    # does not spell a Rust float width (16 / 32 / 64 / 128). The arms, in
    # order: one digit; two digits excluding 16/32/64; three digits
    # excluding 128; four or more digits.
    'f([0-9]|1[0-57-9]|3[0-13-9]|6[0-35-9]|[0245789][0-9]|12[0-79]|1[013-9][0-9]|[02-9][0-9][0-9]|[0-9]{4,})\.[0-9]+'
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
    #   - the private-docs path core lives in its own tier below
    #     (`PATTERNS_PATH_PREFIX`), not here: a whole-token RIGHT boundary
    #     stops at the `/` the core ends in, so any path with a tail passed.
    'Table-[AB]'
    'Table [AB]'
    'lane-contract'
    # The cloak enumeration's private companion. Same class as `lane-contract`
    # and added for the same reason: the doc states on its own first line that it
    # never enters the code repo, and the natural way to cite the two-tier
    # ceiling in a weld's module doc is to point at the file that explains it --
    # which is a filename a reader of this repo cannot open.
    #
    # Bare rather than `\.md`, matching the `lane-contract` precedent: a citation
    # drops the extension as readily as it keeps it.
    #
    # MEASURED collision-free across all 1008 tracked files: whole-token
    # `cloak-baseline` returns zero lines outside this scanner. The hyphen is
    # what makes it safe -- the weld binary and its support modules spell
    # themselves with UNDERSCORES (`cloak_baseline_weld`,
    # `cloak_population`), so no code identifier can match this core.
    'cloak-baseline'
    'foundations\.md'
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

# Third tier: path PREFIXES. Left boundary only, no right boundary: the core
# ends in `/`, and whatever follows it (any filename, any depth) is the path
# tail the core exists to catch.
#
# The left boundary is the default one (`[^[:alnum:]_]`): it keeps a longer
# identifier that merely ENDS in the directory name clean (the catalog
# codegen has a real function of that shape, and an underscore-prefixed
# directory is not this one), while still catching the core after `/`, `./`,
# a backtick, a quote, or line start. The trailing `/` keeps the bare name
# clean where it is an ordinary identifier (a parameter, a `<name>_window`
# helper).
#
# The directory name is assembled from fragments at runtime so this file
# never spells the path it guards against; the self-test fails if either
# script spells it again.
#
# MEASURED: across all tracked files minus `EXCLUDE_PATHS`, this core
# returns zero lines, the same as the whole-token form it replaced.
PRIVATE_DOCS_DIR="$(printf '%s_%s' 'llm' 'context')"
LEFT_BOUNDARY='(^|[^[:alnum:]_])'
PATTERNS_PATH_PREFIX=(
    "$PRIVATE_DOCS_DIR/"
)

# Join the cores of all three tiers into one ERE: the first two tiers wrap
# each core in whole-token boundaries (the second tier's left boundary
# additionally excludes `-`), the path tier takes a left boundary only, then
# all are OR-ed with `|`.
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
    for p in "${PATTERNS_PATH_PREFIX[@]}"; do
        out="$out|$LEFT_BOUNDARY($p)"
    done
    printf '%s' "$out"
}

# Every git-produced input is fixed against user and repo config that would
# change its shape: color, an external diff driver or textconv, custom or
# mnemonic path prefixes, a relative diff root, or rename detection. The
# `+++ b/<path>` header is how `split_added_lines` attributes a line to its
# file, so any of those could silently re-home or hide content.
DIFF_FLAGS=(
    --no-color
    --no-ext-diff
    --no-textconv
    --no-relative
    --no-renames
    --unified=0
    --src-prefix=a/
    --dst-prefix=b/
)

WORK_DIR=""

tool_error() {
    echo "check-internal-ids: scanner error: $*" >&2
    exit "$EXIT_TOOL"
}

ensure_work_dir() {
    if [[ -z "$WORK_DIR" ]]; then
        WORK_DIR="$(mktemp -d)" || tool_error "could not create a temporary directory"
        trap 'rm -rf -- "$WORK_DIR"' EXIT
    fi
}

# Scan FILE against the full pattern set. Returns 0 when clean and 1 when a
# banned token is present; any grep status above 1 (bad pattern, unreadable
# input, missing binary) is a scanner error, never a clean result.
scan_file() {
    local label="$1" file="$2"
    local pattern matches filtered rc
    pattern="$(joined_pattern)"
    matches="$(grep -nE -- "$pattern" "$file")" && rc=0 || rc=$?
    case "$rc" in
        0) ;;
        1) return 0 ;;
        *) tool_error "grep exited $rc while scanning $label" ;;
    esac
    # Drop matched lines that carry the synthetic control-fixture marker.
    # Runs AFTER `grep -n` so the reported line numbers stay accurate.
    filtered="$(printf '%s\n' "$matches" | grep -vF -- "$CONTROL_FIXTURE_MARKER")" && rc=0 || rc=$?
    case "$rc" in
        0) ;;
        1) return 0 ;;
        *) tool_error "grep exited $rc while filtering $label" ;;
    esac
    echo "check-internal-ids: banned internal ID(s) found in $label:" >&2
    printf '%s\n' "$filtered" >&2
    return 1
}

# Added lines of the scanner's own sources are exempt from the pattern set,
# but never from the private-docs directory name: neither file may spell it.
# It takes the path tier's left boundary but no trailing `/`, so the bare
# name is caught too, while a longer identifier that merely ENDS in the name
# stays clean here exactly as it does in every other file.
SCANNER_SOURCE_PATTERN="$LEFT_BOUNDARY$PRIVATE_DOCS_DIR"

scan_scanner_source_file() {
    local label="$1" file="$2"
    local matches rc
    matches="$(grep -nE -- "$SCANNER_SOURCE_PATTERN" "$file")" && rc=0 || rc=$?
    case "$rc" in
        0) ;;
        1) return 0 ;;
        *) tool_error "grep exited $rc while scanning scanner sources in $label" ;;
    esac
    echo "check-internal-ids: scanner source spells the private-docs directory in $label:" >&2
    printf '%s\n' "$matches" >&2
    return 1
}

is_scanner_source() {
    local path="$1" src
    for src in "${SCANNER_SOURCES[@]}"; do
        if [[ "$path" == "$src" ]]; then
            return 0
        fi
    done
    return 1
}

append_line() {
    printf '%s\n' "$2" >>"$1" || tool_error "could not write $1"
}

# Split the ADDED lines (without the leading '+') of the unified diff in
# DIFF into SCANNED (every non-excluded path) and SOURCES (the scanner's own
# sources). The target path comes from the `+++ ` header, which is honoured
# only inside a file header -- before the first hunk, or after a
# `diff --git` line and before that file's first `@@` -- so an added line
# whose content begins `++ ` cannot re-home the lines after it.
split_added_lines() {
    local diff="$1" scanned="$2" sources="$3"
    local in_header=1 dest="$scanned" line path
    [[ -r "$diff" ]] || tool_error "could not read diff $diff"
    : >"$scanned" || tool_error "could not write $scanned"
    : >"$sources" || tool_error "could not write $sources"
    while IFS= read -r line || [[ -n "$line" ]]; do
        case "$line" in
            'diff --git '*)
                in_header=1
                ;;
            '@@'*)
                in_header=0
                ;;
            '+++ '*)
                if [[ "$in_header" -eq 1 ]]; then
                    path="${line#+++ }"
                    path="${path#b/}"
                    if is_scanner_source "$path"; then
                        dest="$sources"
                    elif is_excluded "$path"; then
                        dest=""
                    else
                        dest="$scanned"
                    fi
                elif [[ -n "$dest" ]]; then
                    append_line "$dest" "${line#+}"
                fi
                ;;
            '+'*)
                if [[ -n "$dest" ]]; then
                    append_line "$dest" "${line#+}"
                fi
                ;;
        esac
    done <"$diff"
}

# Scan the unified diff in DIFF: the pattern set over non-excluded added
# lines, and the private-docs literal over the scanner's own added lines.
scan_diff_file() {
    local label="$1" diff="$2"
    local status=0
    ensure_work_dir
    split_added_lines "$diff" "$WORK_DIR/scanned" "$WORK_DIR/sources"
    scan_file "$label added lines" "$WORK_DIR/scanned" || status=1
    scan_scanner_source_file "$label" "$WORK_DIR/sources" || status=1
    return "$status"
}

# Run `git <args>` into OUT, turning any git failure into a scanner error.
git_to_file() {
    local out="$1" rc
    shift
    git "$@" >"$out" && rc=0 || rc=$?
    if [[ "$rc" -ne 0 ]]; then
        tool_error "git $1 exited $rc"
    fi
}

scan_staged() {
    ensure_work_dir
    git_to_file "$WORK_DIR/diff" diff --cached "${DIFF_FLAGS[@]}"
    scan_diff_file "staged diff" "$WORK_DIR/diff"
}

scan_range() {
    local range="$1"
    ensure_work_dir
    git_to_file "$WORK_DIR/diff" diff "${DIFF_FLAGS[@]}" --end-of-options "$range" --
    scan_diff_file "diff range $range" "$WORK_DIR/diff"
}

# Each commit in RANGE against its first parent (a root commit against the
# empty tree). A merge's first-parent diff covers what it brought in, and
# the merged commits are also in the range, so each is scanned on its own.
scan_history() {
    local range="$1" commit status=0 count=0
    ensure_work_dir
    git_to_file "$WORK_DIR/commits" rev-list --reverse --end-of-options "$range" --
    [[ -r "$WORK_DIR/commits" ]] || tool_error "could not read commit list"
    while IFS= read -r commit; do
        [[ -n "$commit" ]] || continue
        count=$((count + 1))
        git_to_file "$WORK_DIR/diff" diff-tree -p --no-commit-id --root \
            --diff-merges=first-parent "${DIFF_FLAGS[@]}" "$commit"
        scan_diff_file "commit $commit" "$WORK_DIR/diff" || status=1
    done <"$WORK_DIR/commits"
    echo "check-internal-ids: scanned $count commit(s) in $range" >&2
    return "$status"
}

scan_commit_messages() {
    local range="$1"
    ensure_work_dir
    git_to_file "$WORK_DIR/messages" log --format=%B --end-of-options "$range" --
    scan_file "commit messages in $range" "$WORK_DIR/messages"
}

# The tree object with no entries, in this repository's object format.
empty_tree() {
    local tree rc
    tree="$(git hash-object -t tree /dev/null)" && rc=0 || rc=$?
    [[ "$rc" -eq 0 && -n "$tree" ]] || tool_error "git hash-object exited $rc"
    printf '%s' "$tree"
}

# True (0) when BEFORE names no previous tip: empty, or git's all-zero id
# for a push that creates the branch (40 or 64 zeros by object format).
is_null_before() {
    [[ -z "$1" || "$1" =~ ^(0{40}|0{64})$ ]]
}

# Print the three CI scan inputs for a push from BEFORE to HEAD. With no
# previous tip every commit reachable from HEAD is new, so the endpoint scan
# starts from the empty tree and the per-commit and message scans take all
# of HEAD's history -- never `HEAD~1`, which a root commit does not have and
# which would skip every earlier commit of a multi-commit initial push.
push_inputs() {
    local before="$1" base rc
    if is_null_before "$before"; then
        base="$(empty_tree)" || exit "$EXIT_TOOL"
        printf 'endpoint=%s..HEAD\n' "$base"
        printf 'history=HEAD\n'
        printf 'messages=HEAD\n'
        return 0
    fi
    base="$(git rev-parse --verify --quiet --end-of-options "$before^{commit}")" && rc=0 || rc=$?
    [[ "$rc" -eq 0 && -n "$base" ]] || tool_error "push base $before does not name a commit"
    printf 'endpoint=%s...HEAD\n' "$base"
    printf 'history=%s..HEAD\n' "$base"
    printf 'messages=%s..HEAD\n' "$base"
}

scan_diff_stdin() {
    ensure_work_dir
    cat >"$WORK_DIR/diff" || tool_error "could not read diff from stdin"
    scan_diff_file "diff (stdin)" "$WORK_DIR/diff"
}

usage() {
    echo "usage: $0 --staged | --commit-msg FILE | --range A..B | --history A..B | --commit-range A..B | --push-inputs BEFORE | --diff-stdin" >&2
    exit "$EXIT_USAGE"
}

# Map a mode's status onto the exit contract: 0 clean, 1 found, and anything
# else a scanner error.
finish() {
    case "$1" in
        0) exit 0 ;;
        1) exit "$EXIT_FOUND" ;;
        *) tool_error "scan ended with unexpected status $1" ;;
    esac
}

main() {
    [[ $# -ge 1 ]] || usage
    local mode="$1" status
    case "$mode" in
        --staged)
            scan_staged && status=0 || status=$?
            ;;
        --commit-msg)
            [[ $# -ge 2 ]] || usage
            scan_file "commit message" "$2" && status=0 || status=$?
            ;;
        --range)
            [[ $# -ge 2 ]] || usage
            scan_range "$2" && status=0 || status=$?
            ;;
        --history)
            [[ $# -ge 2 ]] || usage
            scan_history "$2" && status=0 || status=$?
            ;;
        --commit-range)
            [[ $# -ge 2 ]] || usage
            scan_commit_messages "$2" && status=0 || status=$?
            ;;
        --push-inputs)
            [[ $# -ge 2 ]] || usage
            push_inputs "$2" && status=0 || status=$?
            ;;
        --diff-stdin)
            # Test-only seam: scan a unified diff supplied on stdin
            # through the same added-lines + exclusion path the git modes
            # use, without invoking git. Exercised by the self-test.
            scan_diff_stdin && status=0 || status=$?
            ;;
        *)
            usage
            ;;
    esac
    finish "$status"
}

main "$@"
