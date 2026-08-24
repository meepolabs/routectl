#!/usr/bin/env bash
# Self-test for check-drift.sh. Exits 0 when all assertions pass, non-zero
# on the first failure.
#
# Everything runs inside a throwaway directory standing in for both
# tools/git-hooks/ (the tracked source) and .git/hooks/ (the installed
# copy) -- never against this repo's own .git/hooks, so running this test
# cannot alter the operator's real hooks.
#
# Run it from anywhere:
#   bash tools/git-hooks/check-drift.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK_DRIFT="$HERE/check-drift.sh"

fails=0

# Build a throwaway tracked-hook + installed-hook pair and run the check
# against them. $1 = tracked content, $2 = installed content (symlink if
# empty means "symlink to tracked instead of a copy"), $3 = "symlink" or
# "copy", $4 = name of the caller's variable to receive stdout, $5 =
# name of the caller's variable to receive stderr. Returns
# check_hook_drift's exit code. stdout and stderr are captured
# separately (not merged) so a regression that moves the warning off
# fd 2 -- onto stdout, where a caller's own output would swallow it --
# shows up as a failed assertion instead of passing silently.
run_probe() {
    local tracked_body="$1" installed_body="$2" mode="$3"
    local -n stdout_ref="$4" stderr_ref="$5"
    local tmp errfile rc
    tmp="$(mktemp -d)"
    errfile="$tmp/stderr.log"
    # shellcheck disable=SC2034  # namerefs write back to the caller's vars
    stdout_ref="$({
        mkdir -p "$tmp/tools/git-hooks" "$tmp/hooks"
        printf '%s\n' "$tracked_body" >"$tmp/tools/git-hooks/pre-commit"
        if [[ "$mode" == "symlink" ]]; then
            ln -s "../tools/git-hooks/pre-commit" "$tmp/hooks/pre-commit"
        else
            printf '%s\n' "$installed_body" >"$tmp/hooks/pre-commit"
        fi
        # shellcheck disable=SC1090
        source "$CHECK_DRIFT"
        check_hook_drift "pre-commit" "$tmp/hooks/pre-commit" "$tmp/tools/git-hooks/pre-commit"
    } 2>"$errfile")"
    rc=$?
    # shellcheck disable=SC2034  # nameref writes back to the caller's var
    stderr_ref="$(cat "$errfile" 2>/dev/null)"
    rm -rf "$tmp"
    return $rc
}

# Every assertion below also checks that stdout stayed empty: the
# function only ever writes to fd 2 (or nothing), so ANY stdout output,
# on any path, is itself a regression worth failing on.
assert_stdout_empty() {
    local desc="$1" stdout_got="$2"
    if [[ -n "$stdout_got" ]]; then
        echo "FAIL: expected no stdout output but got some -- $desc"
        printf '%s\n' "$stdout_got"
        fails=$((fails + 1))
        return 1
    fi
    return 0
}

assert_silent() {
    local desc="$1" tracked="$2" installed="$3" mode="$4"
    local out err
    run_probe "$tracked" "$installed" "$mode" out err
    assert_stdout_empty "$desc" "$out" || return
    if [[ -n "$err" ]]; then
        echo "FAIL: expected silence but got stderr output -- $desc"
        printf '%s\n' "$err"
        fails=$((fails + 1))
    else
        echo "PASS: silent -- $desc"
    fi
}

assert_warns() {
    local desc="$1" tracked="$2" installed="$3" mode="$4"
    local out err
    run_probe "$tracked" "$installed" "$mode" out err
    assert_stdout_empty "$desc" "$out" || return
    if printf '%s' "$err" | grep -q 'WARNING'; then
        echo "PASS: warned -- $desc"
    else
        echo "FAIL: expected a WARNING on stderr but got none -- $desc"
        printf '%s\n' "$err"
        fails=$((fails + 1))
    fi
}

# Always returns 0 -- a hygiene nag must never fail the commit it rides on.
assert_exit_zero() {
    local desc="$1" tracked="$2" installed="$3" mode="$4"
    local out err rc
    run_probe "$tracked" "$installed" "$mode" out err
    rc=$?
    assert_stdout_empty "$desc" "$out" || true
    if [[ "$rc" -eq 0 ]]; then
        echo "PASS: exited 0 -- $desc"
    else
        echo "FAIL: exited $rc, must always be 0 -- $desc"
        fails=$((fails + 1))
    fi
}

assert_silent "installed hook is the install.sh symlink" \
    "echo hook v1" "" "symlink"

assert_silent "installed copy still matches the tracked source" \
    "echo hook v1" "echo hook v1" "copy"

assert_warns "installed copy has diverged from the tracked source" \
    "echo hook v2" "echo hook v1" "copy"

assert_exit_zero "diverged copy never fails the check" \
    "echo hook v2" "echo hook v1" "copy"

# The warning must describe divergence, not assert a direction: cmp only
# knows the two differ, not which one moved, so it must never claim the
# installed copy specifically is the stale one (a contributor could have
# edited it on purpose, ahead of the tracked source, not behind it).
tmp="$(mktemp -d)"
mkdir -p "$tmp/tools/git-hooks" "$tmp/hooks"
printf '%s\n' "echo hook v2" >"$tmp/tools/git-hooks/pre-commit"
printf '%s\n' "echo hook v1" >"$tmp/hooks/pre-commit"
# shellcheck disable=SC1090
source "$CHECK_DRIFT"
wording="$(check_hook_drift "pre-commit" "$tmp/hooks/pre-commit" "$tmp/tools/git-hooks/pre-commit" 2>&1)"
rm -rf "$tmp"
if printf '%s' "$wording" | grep -qi 'stale'; then
    echo "FAIL: warning must not assert staleness/direction (found 'stale' in: $wording)"
    fails=$((fails + 1))
else
    echo "PASS: warning states divergence without asserting a direction"
fi

# Second run against the SAME installed/tracked pair must stay silent --
# the once-per-day marker suppresses the repeat nag.
tmp="$(mktemp -d)"
mkdir -p "$tmp/tools/git-hooks" "$tmp/hooks"
printf '%s\n' "echo hook v2" >"$tmp/tools/git-hooks/pre-commit"
printf '%s\n' "echo hook v1" >"$tmp/hooks/pre-commit"
# shellcheck disable=SC1090
source "$CHECK_DRIFT"
first="$(check_hook_drift "pre-commit" "$tmp/hooks/pre-commit" "$tmp/tools/git-hooks/pre-commit" 2>&1)"
second="$(check_hook_drift "pre-commit" "$tmp/hooks/pre-commit" "$tmp/tools/git-hooks/pre-commit" 2>&1)"
rm -rf "$tmp"
if printf '%s' "$first" | grep -q 'WARNING' && [[ -z "$second" ]]; then
    echo "PASS: repeat check within the window stays silent"
else
    echo "FAIL: repeat check should warn once then go silent (first='$first' second='$second')"
    fails=$((fails + 1))
fi

# The guard shape each hook actually uses is two layers, not one:
# `[[ -f check-drift.sh ]]` around the source, AND `source ... 2>/dev/null
# || true` around the source itself, AND `declare -f check_hook_drift`
# gating the call. Each layer defends a different way the helper can be
# unusable at commit time: absent (a tree checked out before
# check-drift.sh existed -- an older worktree, a bisect step), present
# but unreadable (restrictive permissions), and present but
# syntactically broken (mid-edit, stray merge markers). All three must
# leave a `set -e` hook able to reach its own end, silently.
guard_probe() {
    local helper_state="$1" tmp out rc
    tmp="$(mktemp -d)"
    mkdir -p "$tmp/tools/git-hooks"
    case "$helper_state" in
        absent)
            : # no file at all
            ;;
        chmod000)
            printf '%s\n' 'check_hook_drift() { :; }' >"$tmp/tools/git-hooks/check-drift.sh"
            chmod 000 "$tmp/tools/git-hooks/check-drift.sh"
            ;;
        syntax_error)
            printf '%s\n' 'check_hook_drift() { if [[ true' >"$tmp/tools/git-hooks/check-drift.sh"
            ;;
        *)
            echo "guard_probe: unknown helper_state '$helper_state'" >&2
            return 2
            ;;
    esac
    out="$(bash -c '
        set -e
        ROOT="$1"
        if [[ -f "$ROOT/tools/git-hooks/check-drift.sh" ]]; then
            # shellcheck disable=SC1091
            source "$ROOT/tools/git-hooks/check-drift.sh" 2>/dev/null || true
            if declare -f check_hook_drift >/dev/null 2>&1; then
                check_hook_drift "pre-commit" "$0" "$ROOT/tools/git-hooks/pre-commit" || true
            fi
        fi
        echo REACHED
    ' _ "$tmp" 2>&1)"
    rc=$?
    rm -rf "$tmp"
    printf '%s\n' "$out"
    return $rc
}

assert_guard_survives() {
    local desc="$1" helper_state="$2" out rc
    out="$(guard_probe "$helper_state")"
    rc=$?
    if [[ "$rc" -eq 0 ]] && printf '%s' "$out" | grep -q 'REACHED' \
        && ! printf '%s' "$out" | grep -q 'WARNING'; then
        echo "PASS: hook survives a $desc helper, reaching the end silently"
    else
        echo "FAIL: hook guard did not survive a $desc helper (rc=$rc, out='$out')"
        fails=$((fails + 1))
    fi
}

assert_guard_survives "missing" absent
assert_guard_survives "chmod-000 (unreadable)" chmod000
assert_guard_survives "syntactically invalid" syntax_error

if [[ "$fails" -ne 0 ]]; then
    echo "check-drift.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
