#!/usr/bin/env bash
# Self-test for check-internal-ids.sh. Exits 0 when all assertions pass,
# non-zero on the first failure. ASCII-only; uses ONLY synthetic
# ID-shaped tokens (R2-EXAMPLE, RV-99, TODO(M99), etc.) so this file
# carries no real internal IDs.
#
# Run it from anywhere:
#   bash scripts/check-internal-ids.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCANNER="$HERE/check-internal-ids.sh"

fails=0

# Assert that scanning TEXT in commit-msg mode is CAUGHT (exit 1).
assert_caught() {
    local desc="$1"
    local text="$2"
    local tmp
    tmp="$(mktemp)"
    printf '%s\n' "$text" >"$tmp"
    if bash "$SCANNER" --commit-msg "$tmp" >/dev/null 2>&1; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
    rm -f "$tmp"
}

# Assert that scanning TEXT in commit-msg mode is CLEAN (exit 0).
assert_clean() {
    local desc="$1"
    local text="$2"
    local tmp
    tmp="$(mktemp)"
    printf '%s\n' "$text" >"$tmp"
    if bash "$SCANNER" --commit-msg "$tmp" >/dev/null 2>&1; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but caught -- $desc"
        fails=$((fails + 1))
    fi
    rm -f "$tmp"
}

# Assert that scanning a unified DIFF (on stdin) in --diff-stdin mode is
# CAUGHT (exit 1).
assert_diff_caught() {
    local desc="$1"
    local diff="$2"
    if printf '%s\n' "$diff" | bash "$SCANNER" --diff-stdin >/dev/null 2>&1; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
}

# Assert that scanning a unified DIFF (on stdin) in --diff-stdin mode is
# CLEAN (exit 0).
assert_diff_clean() {
    local desc="$1"
    local diff="$2"
    if printf '%s\n' "$diff" | bash "$SCANNER" --diff-stdin >/dev/null 2>&1; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but caught -- $desc"
        fails=$((fails + 1))
    fi
}

# POSITIVE: each of the 6 banned formats is caught.
assert_caught "R2- token" "see R2-EXAMPLE for context"
assert_caught "RV- token" "deferred to RV-99 backlog"
assert_caught "T-GATE token" "covered by T-GATE invariant"
assert_caught "T-SSRF token" "the T-SSRF case"
assert_caught "TODO(M..) token" "TODO(M99) wire this up"
assert_caught "TODO(M..-suffix) token" "TODO(M99-CLOSE) finalize"
assert_caught "M#.# milestone" "shipped in M99.7 cycle"
assert_caught "H# fix token" "applies the H42 fix here"
assert_caught "H# invariant token" "preserves the H7 invariant"

# POSITIVE (planning-shorthand class): the task / feature / decision
# shorthand forms are caught.
assert_caught "f<n>.<nn> task shorthand" "landed in f1.02 slice"
assert_caught "(pre-)f<n> planning commentary" "adjust pre-f2 before merge"
assert_caught "(post-)f<n> planning commentary" "post-f3 cleanup pass"
assert_caught "D<nn> decision shorthand" "recorded under D42 rationale"

# POSITIVE (stage-label class): the three spellings that actually occurred in
# this repo -- all-caps with a space, hyphenated, and the possessive form.
assert_caught "hyphenated slice label" "mirrors the slice-2 renderer"
assert_caught "possessive slice label" "matches slice 2's id-grouping"
assert_caught "uppercase slice label" "the SLICE 3 state machine"
assert_caught "uppercase hyphenated slice label" "the SLICE-3 state machine"

# NEGATIVE (stage-label class): Rust's own slice vocabulary, vendor model ids
# carrying an `R<n>` tail, and -- the two that matter most -- legitimate
# technical prose that a wider core would have blocked. This scanner BLOCKS
# commits, so a false positive is a developer-facing outage; lowercase
# `slice <n>` with a plain space and bare `(R<n>)` are deliberately NOT
# matched for exactly that reason.
assert_clean "Rust slice prose" "lift_all iterates this slice in order"
assert_clean "slice word before non-digit" "the LIFT_STEPS slice defines order"
assert_clean "slice inside identifier" "the slice2_helper function"
assert_clean "vendor model DeepSeek-R1" "route to deepseek-ai/DeepSeek-R1 here"
assert_clean "R<n> without parens in model id" "id is MAI-DS-R1 upstream"
assert_clean "legitimate lowercase slice N prose" \
    "copy the second buffer into slice 2 of the ring"
assert_clean "legitimate parenthesized external requirement" \
    "conformance with external requirement (R2)"

# POSITIVE (hyphen-excluding tier): the bare-label class is caught when it
# stands alone as a token.
assert_caught "bare M<n> label" "the M1 recorder writes here"
assert_caught "bare M<n> label mid-sentence" "planned for M3 generation"
assert_caught "bare T<n> label" "covered by T5 already"
assert_caught "later increment phrase" "deferred to a later increment"
assert_caught "later phase phrase" "handled in a later phase"
assert_caught "later milestone phrase" "punted to a later milestone"
assert_caught "this milestone phrase" "out of scope for this milestone"

# NEGATIVE (hyphen-excluding tier): hyphenated vendor model names carry an
# `M<n>` tail and must NOT trip -- this is why the tier excludes `-` on the
# LEFT boundary.
assert_clean "vendor model MiniMax-M2" "route to MiniMax-M2 for cheap calls"
assert_clean "vendor model MiniMax-M3" "route to MiniMax-M3 for cheap calls"
assert_clean "vendor model with provider prefix" "id is minimax/MiniMax-M3 upstream"
assert_clean "vendor model in JSON value" '{"models_dev_model":"MiniMax-M3"}'

# NEGATIVE (hyphen-excluding tier, whole-token boundary): the bare cores
# embedded in a larger identifier must NOT trip.
assert_clean "M<n> inside identifier" "the M3_BODY constant"
assert_clean "T<n> inside identifier" "field T5_total here"

# NEGATIVE: ordinary content must NOT be caught.
assert_clean "plain TODO" "TODO: refactor this later"
assert_clean "issue ref" "fixes #123 in the tracker"
assert_clean "model name claude" "route to claude-opus-4-8"
assert_clean "model name gpt-5" "default model is gpt-5"
assert_clean "max_tokens value" "set max_tokens 4096 for the call"
assert_clean "ordinary prose" "this change improves the retry policy"
assert_clean "version-like decimal in prose word" "release 2 point 0 notes"
assert_clean "captured-fixture-like JSON" '{"model":"claude-opus-4-8","usage":{"input_tokens":4096},"id":"msg_01ABCdef"}'

# NEGATIVE (whole-token boundary): a synthetic ID embedded inside a
# larger identifier is NOT a standalone internal ID and must NOT trip.
# The boundary intent is whole-token, not substring.
assert_clean "R2 embedded in identifier" "the xR2-EXAMPLEy symbol"
assert_clean "RV embedded in identifier" "field myRV-99thing here"

# NEGATIVE (planning-shorthand near-misses): tokens that resemble the
# task / feature / decision shorthand but must NOT trip.
assert_clean "genuine float literal near f<n>.<nn>" "let scale = 1.02 as ratio"
assert_clean "f<n>.<nn> embedded in identifier" "the conf1.02 setting"
assert_clean "word containing postfix" "apply the postfix operator here"
assert_clean "word containing prefix" "strip the prefix from the key"
assert_clean "D-digits inside a longer identifier" "the buildD42tag helper"
assert_clean "hex byte near D<nn>" "mask with 0xD42F here"

# POSITIVE (boundary still catches standalone tokens at edges): a token
# bounded by punctuation / line edges must still be caught.
assert_caught "R2 token in parens" "context (R2-EXAMPLE) applies"
assert_caught "RV token at line start" "RV-99 is deferred"

# DIFF-PATH (exclusion semantics): the test fixture file itself is
# EXCLUDED (exact file entry), but a sibling like
# `scripts/check-internal-ids.test.sh.bak` must NOT inherit that
# exemption -- it is still scanned. Construct synthetic diffs that add a
# banned token under each path and assert the exclusion only covers the
# exact file.
assert_diff_clean "exact-match excluded file is not scanned" \
"+++ b/scripts/check-internal-ids.test.sh
+see RV-99 here"

assert_diff_caught "sibling .bak of excluded file IS scanned" \
"+++ b/scripts/check-internal-ids.test.sh.bak
+see RV-99 here"

assert_diff_clean "captured fixtures dir is excluded" \
"+++ b/crates/routectl-cli/tests/fixtures/captured/x.json
+see RV-99 here"

assert_diff_caught "ordinary source file IS scanned" \
"+++ b/crates/routectl-core/src/lib.rs
+// TODO(M99) wire this up"

# DIFF-PATH (self-exemption): the scanner IS the rule set, so a diff that
# adds its own pattern literals must not block. A sibling path must still
# be scanned.
assert_diff_clean "scanner excludes itself" \
"+++ b/scripts/check-internal-ids.sh
+    'later (increment|phase|milestone)'
+    'this milestone'"

assert_diff_caught "sibling .bak of scanner IS scanned" \
"+++ b/scripts/check-internal-ids.sh.bak
+    'this milestone'"

# DIFF-PATH: the vendored catalog selector source is NOT path-excluded and
# must stay clean of bare labels on its own merits.
assert_diff_clean "catalog selector source with vendor model name is clean" \
"+++ b/crates/routectl-router/src/catalog_codegen_selectors.rs
+// keeps MiniMax-M3 selectable"

if [[ "$fails" -ne 0 ]]; then
    echo "check-internal-ids self-test: $fails failure(s)" >&2
    exit 1
fi
echo "check-internal-ids self-test: all assertions passed"
exit 0
