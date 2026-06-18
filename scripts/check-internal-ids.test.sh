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

if [[ "$fails" -ne 0 ]]; then
    echo "check-internal-ids self-test: $fails failure(s)" >&2
    exit 1
fi
echo "check-internal-ids self-test: all assertions passed"
exit 0
