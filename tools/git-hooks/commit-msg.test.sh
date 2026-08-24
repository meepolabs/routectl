#!/usr/bin/env bash
# Self-test for the commit-msg hook's wiring to its two checker scripts.
# Exits 0 when all assertions pass, non-zero on the first failure.
#
# `.git/hooks/commit-msg` is a symlink shared by every worktree, so the
# hook's ROOT can be a tree that predates one of its checker scripts (an
# older worktree, a bisect step). This test builds throwaway trees with
# and without each checker present and runs the real commit-msg script
# against them directly -- never against this repo's own .git/hooks, so
# running this test cannot alter the operator's real hooks or history.
#
# Run it from anywhere:
#   bash tools/git-hooks/commit-msg.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMMIT_MSG_HOOK="$HERE/commit-msg"
SUBJECT_CHECKER_SRC="$HERE/check-subject-length.sh"
ID_CHECKER_SRC="$HERE/../../scripts/check-internal-ids.sh"

fails=0

# Builds a throwaway tree containing a copy of the real commit-msg hook,
# plus its checker scripts only where requested, and runs the hook
# against a commit-message file holding $1. $2 = "with_subject_checker"
# or "without_subject_checker". $3 = "with_id_checker" or
# "without_id_checker". Writes stdout/stderr/exit code back via nameref.
run_probe() {
    local msg_body="$1" subject_mode="$2" id_mode="$3"
    local -n out_ref="$4" err_ref="$5" rc_ref="$6"
    local tmp msgfile errfile

    tmp="$(mktemp -d)"
    mkdir -p "$tmp/tools/git-hooks" "$tmp/scripts"
    cp "$COMMIT_MSG_HOOK" "$tmp/tools/git-hooks/commit-msg"

    if [[ "$subject_mode" == "with_subject_checker" ]]; then
        cp "$SUBJECT_CHECKER_SRC" "$tmp/tools/git-hooks/check-subject-length.sh"
    fi
    if [[ "$id_mode" == "with_id_checker" ]]; then
        cp "$ID_CHECKER_SRC" "$tmp/scripts/check-internal-ids.sh"
    fi

    (cd "$tmp" && git init -q .)

    msgfile="$tmp/COMMIT_EDITMSG"
    errfile="$tmp/stderr.log"
    printf '%s' "$msg_body" >"$msgfile"

    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    out_ref="$(cd "$tmp" && bash tools/git-hooks/commit-msg "$msgfile" 2>"$errfile")"
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    rc_ref=$?
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    err_ref="$(cat "$errfile" 2>/dev/null)"
    rm -rf "$tmp"
}

short_subject="fix: short subject line"
over_limit_subject="$(printf 'a%.0s' $(seq 1 80))"

# A tree WITHOUT check-subject-length.sh must not hit a bare 127 (the
# reproduced defect): it must exit 0, name the missing script, and say
# the commit was allowed.
# shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
out=""; err=""; rc=""
run_probe "$short_subject" without_subject_checker with_id_checker out err rc
if [[ "$rc" -eq 0 ]] \
    && printf '%s' "$err" | grep -q 'check-subject-length.sh' \
    && printf '%s' "$err" | grep -qi 'ALLOWED' \
    && ! printf '%s' "$err" | grep -q 'No such file or directory'; then
    echo "PASS: missing subject checker -- allowed with a named, unambiguous warning"
else
    echo "FAIL: missing subject checker did not produce the expected allow+warning (rc=$rc, stderr='$err')"
    fails=$((fails + 1))
fi

# Positive control: a tree WITH check-subject-length.sh must still
# REJECT an over-length subject. Proves the fix did not turn the gate
# fail-open.
# shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
out=""; err=""; rc=""
run_probe "$over_limit_subject" with_subject_checker with_id_checker out err rc
if [[ "$rc" -ne 0 ]] && printf '%s' "$err" | grep -q 'check-subject-length'; then
    echo "PASS: subject checker present -- over-length subject still rejected"
else
    echo "FAIL: over-length subject was not rejected when the checker is present (rc=$rc, stderr='$err')"
    fails=$((fails + 1))
fi

# A tree WITH check-subject-length.sh must still ACCEPT a valid subject.
# shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
out=""; err=""; rc=""
run_probe "$short_subject" with_subject_checker with_id_checker out err rc
if [[ "$rc" -eq 0 ]]; then
    echo "PASS: subject checker present -- valid subject accepted"
else
    echo "FAIL: valid subject was rejected when the checker is present (rc=$rc, stderr='$err')"
    fails=$((fails + 1))
fi

# The internal-ID scan still runs (and blocks) when its script is
# present, using a synthetic ID-shaped token so this test never needs a
# real one. Built from parts at runtime, not written as a contiguous
# literal, so this test file's own tracked source never itself matches
# the pattern it is exercising (the internal-ID scan covers this file
# too, once staged).
banned_id_prefix="R"
banned_id_suffix="V"
banned_subject="fix: touches ${banned_id_prefix}${banned_id_suffix}-42 reference"
# shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
out=""; err=""; rc=""
run_probe "$banned_subject" with_subject_checker with_id_checker out err rc
if [[ "$rc" -ne 0 ]] && printf '%s' "$err" | grep -q 'check-internal-ids'; then
    echo "PASS: internal-ID checker present -- banned token still rejected"
else
    echo "FAIL: internal-ID checker present but banned token was not rejected (rc=$rc, stderr='$err')"
    fails=$((fails + 1))
fi

# Same missing-checker treatment applies to check-internal-ids.sh: a
# tree without it must allow with a named, unambiguous warning rather
# than a bare 127.
# shellcheck disable=SC2034  # out is bound as a nameref target inside run_probe, not read directly here
out=""; err=""; rc=""
run_probe "$short_subject" with_subject_checker without_id_checker out err rc
if [[ "$rc" -eq 0 ]] \
    && printf '%s' "$err" | grep -q 'check-internal-ids.sh' \
    && printf '%s' "$err" | grep -qi 'ALLOWED' \
    && ! printf '%s' "$err" | grep -q 'No such file or directory'; then
    echo "PASS: missing internal-ID checker -- allowed with a named, unambiguous warning"
else
    echo "FAIL: missing internal-ID checker did not produce the expected allow+warning (rc=$rc, stderr='$err')"
    fails=$((fails + 1))
fi

if [[ "$fails" -ne 0 ]]; then
    echo "commit-msg.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "commit-msg.test.sh: all assertions passed"
