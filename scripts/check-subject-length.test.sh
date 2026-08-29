#!/usr/bin/env bash
# Self-test for check-subject-length.sh. Exits 0 when all assertions
# pass, non-zero on the first failure.
#
# Every probe writes a throwaway commit-message file under a tmpdir and
# invokes the checker against it directly -- never against a real commit
# or this repo's own history.
#
# Run it from anywhere:
#   bash scripts/check-subject-length.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-subject-length.sh"

fails=0

# Writes $1 as a commit-message file and runs the checker against it.
# Captures stdout/stderr separately and the exit code via nameref.
run_probe() {
    local body="$1"
    local -n out_ref="$2" err_ref="$3" rc_ref="$4"
    local tmp msgfile errfile
    tmp="$(mktemp -d)"
    msgfile="$tmp/COMMIT_EDITMSG"
    errfile="$tmp/stderr.log"
    printf '%s' "$body" >"$msgfile"
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    out_ref="$(bash "$CHECKER" "$msgfile" 2>"$errfile")"
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    rc_ref=$?
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    err_ref="$(cat "$errfile" 2>/dev/null)"
    rm -rf "$tmp"
}

assert_pass() {
    local desc="$1" body="$2" rc err
    # shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
    local out
    run_probe "$body" out err rc
    if [[ "$rc" -eq 0 ]]; then
        echo "PASS: accepted -- $desc"
    else
        echo "FAIL: expected exit 0 but got $rc -- $desc"
        printf '%s\n' "$err"
        fails=$((fails + 1))
    fi
}

assert_reject() {
    local desc="$1" body="$2" rc
    # shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
    local out err
    run_probe "$body" out err rc
    if [[ "$rc" -ne 0 ]] && printf '%s' "$err" | grep -q 'check-subject-length'; then
        echo "PASS: rejected -- $desc"
    else
        echo "FAIL: expected a non-zero exit naming check-subject-length but got rc=$rc, stderr='$err' -- $desc"
        fails=$((fails + 1))
    fi
}

# Positive control: a subject sitting exactly at the limit must pass --
# proves the boundary is <= 70, not < 70, and that the checker can ever
# accept a real subject at all.
at_limit_subject="$(printf 'a%.0s' $(seq 1 70))"
assert_pass "70-char subject at the limit" "$at_limit_subject"

over_limit_subject="$(printf 'a%.0s' $(seq 1 71))"
assert_reject "71-char subject one over the limit" "$over_limit_subject"

# Empty message: git's own aborted-empty-commit check owns this case,
# so the length checker must stay out of the way.
assert_pass "empty commit message" ""

# Comment-only message (the unedited template, e.g. from an aborted
# interactive commit): the first line is a comment, not a subject.
long_comment="# $(printf 'a%.0s' $(seq 1 90))"
assert_pass "comment-only message with a long comment line" "$long_comment
# Please enter the commit message for your changes."

# A short subject with a long body line must still pass: the body is
# not bounded by this rule.
short_subject_long_body="fix: short subject

$(printf 'a%.0s' $(seq 1 200))"
assert_pass "short subject with a long body line" "$short_subject_long_body"

if [[ "$fails" -ne 0 ]]; then
    echo "check-subject-length.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-subject-length.test.sh: all assertions passed"
