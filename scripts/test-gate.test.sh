#!/usr/bin/env bash
# Self-test for the gate registry's public-api subcommand, the informational
# public-API report wrapper, and the wiring that keeps that check
# informational. Runs no cargo and no cargo-public-api.
#
# Pins:
#   - `test-gate.sh --print public-api` is exactly the public-api.sh
#     --check over every crate, and the subcommand refuses extra arguments;
#   - public-api-report.sh always exits 0 and classifies a stub check's
#     outcome as clean / drift / could-not-run, one annotation and one job
#     summary line per run, each case paired with a planted-defect control;
#   - the CI `public-api` job continues on error and its last step runs the
#     wrapper, the `required` job does not depend on it, no pre-commit hook
#     runs it, and the retired pre-push leg script stays deleted -- each
#     wiring check holds on the real tree and fails on a stub carrying the
#     defect it pins.
#
# Run it from anywhere:
#   bash scripts/test-gate.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
REGISTRY="$HERE/test-gate.sh"
SYSTEM_PATH=/usr/bin:/bin

fails=0
pass() { echo "PASS: $*"; }
fail() {
    echo "FAIL: $*"
    fails=$((fails + 1))
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- registry ---------------------------------------------------------------

got="$(bash "$REGISTRY" --print public-api 2>&1)"
if [[ "$got" == "bash scripts/public-api.sh --check all" ]]; then
    pass "--print public-api is the baseline check over every crate"
else
    fail "--print public-api printed '$got'"
fi

bash "$REGISTRY" --print public-api extra >/dev/null 2>&1
rc=$?
if [[ "$rc" -eq 2 ]]; then
    pass "public-api refuses extra arguments (exit 2)"
else
    fail "public-api with an extra argument exited $rc, want 2"
fi

# --- public-api report wrapper ---------------------------------------------

# The wrapper runs from its own scratch dir beside a public-api.sh stub that
# records its argv, prints one stdout marker, writes $STUB_STDERR to stderr and
# exits $STUB_RC. Each case is a predicate over one wrapper copy; it must hold
# for the real wrapper and fail for a copy planted with the defect it pins.
REPORT="$HERE/public-api-report.sh"
REPORT_DIR="$TMP/report"
REPORT_LOG="$TMP/report-check-invoked"
SUMMARY="$TMP/step-summary"
mkdir -p "$REPORT_DIR"
cat >"$REPORT_DIR/public-api.sh" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >"$REPORT_LOG"
echo "stub-check-stdout"
[[ -n "\${STUB_STDERR:-}" ]] && printf '%s\n' "\$STUB_STDERR" >&2
exit "\${STUB_RC:-0}"
STUB

# The wrapper decides could-not-run from `command -v cargo-public-api` alone,
# so the tool-present PATH holds a stub of it and the tool-absent PATH holds
# nothing beyond the system dirs.
if [[ -n "$(PATH="$SYSTEM_PATH" command -v cargo-public-api)" ]]; then
    fail "cargo-public-api is in $SYSTEM_PATH, so the tool-absent case cannot be hermetic"
fi
report_tool_bin="$TMP/bin-report-tool"
report_no_tool_bin="$TMP/bin-report-no-tool"
mkdir -p "$report_tool_bin" "$report_no_tool_bin"
printf '#!/bin/sh\necho "cargo-public-api 0.0.0"\n' >"$report_tool_bin/cargo-public-api"
chmod +x "$report_tool_bin/cargo-public-api"

# Runs wrapper copy $1 with PATH = $2 plus the system dirs, the stub exiting
# $3 with stderr $4, and GITHUB_STEP_SUMMARY on a file holding one prior
# line. Sets OUT, RC, LAST (the last output line) and APPENDED (the summary
# lines the run added).
run_report() {
    rm -f "$REPORT_LOG"
    printf 'prior summary line\n' >"$SUMMARY"
    OUT="$(PATH="$2:$SYSTEM_PATH" STUB_RC="$3" STUB_STDERR="$4" \
        GITHUB_STEP_SUMMARY="$SUMMARY" bash "$1" 2>&1)"
    RC=$?
    LAST="$(printf '%s\n' "$OUT" | tail -n 1)"
    APPENDED="$(tail -n +2 "$SUMMARY")"
}

check_ran() { [[ -f "$REPORT_LOG" && "$(cat "$REPORT_LOG")" == "--check all" ]]; }

# Holds when the run exited 0, printed annotation $1 as its last line after
# the check's own output (unless $3 says the check must not run), and added
# exactly the summary line $2.
report_ok() {
    local annotation="$1" summary="$2" ran="${3:-ran}"
    [[ "$RC" -eq 0 && "$LAST" == "$annotation" && "$APPENDED" == "$summary" ]] || return 1
    if [[ "$ran" == ran ]]; then
        check_ran && printf '%s\n' "$OUT" | grep -qx 'stub-check-stdout'
    else
        ! check_ran
    fi
}

case_clean() {
    run_report "$1" "$report_tool_bin" 0 ""
    report_ok "::notice title=public-api::public API baselines match" \
        "public-api: public API baselines match"
}

case_drift() {
    run_report "$1" "$report_tool_bin" 1 "$(printf '%s\n' \
        '+pub fn added()' \
        'public-api: surface drift for routectl-core (see public-api/POLICY.md)' \
        'public-api: routectl-router unchanged' \
        'public-api: missing baseline for routectl-usage (run: x generate routectl-usage)')"
    report_ok "::warning title=public-api::drift in routectl-core, routectl-usage" \
        "public-api: drift in routectl-core, routectl-usage"
}

case_could_not_run() {
    run_report "$1" "$report_tool_bin" 2 "$(printf '%s\n' 'first line' 'public-api: unknown crate')"
    report_ok "::warning title=public-api::could not run (public-api: unknown crate)" \
        "public-api: could not run (public-api: unknown crate)"
}

case_tool_absent() {
    run_report "$1" "$report_no_tool_bin" 0 ""
    report_ok "::warning title=public-api::could not run (cargo-public-api not installed)" \
        "public-api: could not run (cargo-public-api not installed)" not-run
}

case_no_summary_env() {
    rm -f "$REPORT_LOG" "$SUMMARY"
    OUT="$(PATH="$report_tool_bin:$SYSTEM_PATH" STUB_RC=0 \
        env -u GITHUB_STEP_SUMMARY bash "$1" 2>&1)"
    RC=$?
    [[ "$RC" -eq 0 && ! -e "$SUMMARY" ]] && check_ran
}

case_rejects_args() {
    rm -f "$REPORT_LOG"
    PATH="$report_tool_bin:$SYSTEM_PATH" bash "$1" extra >/dev/null 2>&1
    RC=$?
    [[ "$RC" -eq 2 ]] && ! check_ran
}

# Writes a copy of the wrapper with sed expression $2 applied to
# $REPORT_DIR/$1 and prints its path; fails when the expression changed
# nothing, so a stale mutation cannot pass as a caught one.
mutant() {
    local path="$REPORT_DIR/$1"
    if ! sed -E "$2" "$REPORT" >"$path" || cmp -s "$REPORT" "$path"; then
        echo "mutation '$2' did not apply" >&2
        return 1
    fi
    echo "$path"
}

# Runs case $2 against the real wrapper (must hold) and against the mutant
# built by sed expression $3 (must not).
assert_report_case() {
    local desc="$1" case_fn="$2" expr="$3" copy
    cp "$REPORT" "$REPORT_DIR/public-api-report.sh"
    if "$case_fn" "$REPORT_DIR/public-api-report.sh"; then
        pass "report: $desc"
    else
        fail "report: $desc: rc=$RC last='${LAST:-}' summary='${APPENDED:-}' out='$OUT'"
    fi
    if ! copy="$(mutant "$case_fn.sh" "$expr")"; then
        fail "report: $desc: control mutation did not apply"
    elif "$case_fn" "$copy"; then
        fail "report: $desc: control passed on a wrapper planted with '$expr'"
    else
        pass "report: $desc: control fails on the planted defect"
    fi
}

assert_report_case "check exiting 0 gives the clean notice" case_clean \
    's/report notice "public API baselines match"/report warning "public API baselines match"/'
# shellcheck disable=SC2016  # the sed expressions name the wrapper's own variables literally
assert_report_case "drift for two crates gives one warning naming both, exit 0" case_drift \
    's/^exit 0$/exit "$rc"/'
assert_report_case "drift names every crate, not only the first" case_drift \
    "s/awk '!seen\[\\\$0\]\+\+/awk 'NR == 1/"
# shellcheck disable=SC2016  # the sed expressions name the wrapper's own variables literally
assert_report_case "check exiting 2 gives could-not-run with its reason, exit 0" case_could_not_run \
    's/^exit 0$/exit "$rc"/'
assert_report_case "could-not-run carries the check's last stderr line" case_could_not_run \
    's/tail -n 1\)/head -n 1)/'
assert_report_case "cargo-public-api absent: could-not-run, the check never runs" case_tool_absent \
    's/^if ! command -v cargo-public-api /if false \&\& ! command -v cargo-public-api /'
# shellcheck disable=SC2016  # the sed expressions name the wrapper's own variables literally
assert_report_case "exactly one summary line is appended per run" case_clean \
    's/^( *)if ! printf .%s: %s\\n. "\$TITLE" "\$message" >>"\$GITHUB_STEP_SUMMARY"; then/\1printf "x\\n" >>"$GITHUB_STEP_SUMMARY"; &/'
# shellcheck disable=SC2016  # the sed expressions name the wrapper's own variables literally
assert_report_case "no GITHUB_STEP_SUMMARY: still exit 0, no summary file" case_no_summary_env \
    's#\$\{GITHUB_STEP_SUMMARY:-\}#${GITHUB_STEP_SUMMARY:-'"$SUMMARY"'}#'
assert_report_case "any argument is a usage error (exit 2)" case_rejects_args \
    's/^\[\[ \$# -eq 0 \]\] \|\| usage$/true/'

# --- wiring -----------------------------------------------------------------

# The check stays informational only while CI wires it that way and nothing
# local runs it as a gate. Each predicate walks the canonical block layout of
# the file it reads (two-space job keys, four-space job fields, six-space
# step items) and fails closed when that layout is not where it expects it,
# so a reformatted file reads as a failure rather than a pass. Each holds on
# the real tree and fails on a copy derived from it with one defect planted.
CI_WORKFLOW="$REPO_ROOT/.github/workflows/ci.yml"
PRE_COMMIT_CONFIG="$REPO_ROOT/.pre-commit-config.yaml"
REPORT_RUN="        run: bash scripts/public-api-report.sh"
# The retired hook id and leg script, spelled in two pieces so a repo-wide
# search for either name finds nothing, this file included.
RETIRED_HOOK_ID="public-api-""baseline"
RETIRED_LEG="public-api-pre""-push"

# Lines of CI job $2 in workflow $1, minus whole-line comments and blanks.
ci_job_block() {
    awk -v head="  $2:" '
        $0 == head { in_job = 1; next }
        in_job && /^  [A-Za-z0-9_-]+:$/ { exit }
        in_job && !/^[[:space:]]*(#.*)?$/ { print }
    ' "$1"
}

# Holds when the `public-api` job sets job-level `continue-on-error: true`
# exactly once and its last step is exactly a `name:` line plus the wrapper's
# run line.
public_api_job_informational() {
    local job last
    job="$(ci_job_block "$1" public-api)"
    [[ -n "$job" ]] || return 1
    [[ "$(grep -c '^    continue-on-error:' <<<"$job")" -eq 1 ]] || return 1
    grep -qx '    continue-on-error: true' <<<"$job" || return 1
    last="$(awk '/^      - / { buf = "" } { buf = buf $0 "\n" } END { printf "%s", buf }' <<<"$job")"
    [[ "$(wc -l <<<"$last")" -eq 2 ]] || return 1
    [[ "$(sed -n 1p <<<"$last")" =~ ^"      - name: ".+$ ]] || return 1
    [[ "$(sed -n 2p <<<"$last")" == "$REPORT_RUN" ]]
}

# Holds when the `required` job has one block-style `needs:` list of bare
# job names, at least one, none of them `public-api`. Any other shape of the
# list fails closed.
required_skips_public_api() {
    local job
    job="$(ci_job_block "$1" required)"
    [[ -n "$job" ]] || return 1
    awk '
        /^    needs:$/ { if (seen) bad = 1; seen = 1; in_list = 1; next }
        /^    needs:/ { bad = 1; next }
        in_list && /^      - / {
            if ($0 !~ /^      - [a-z0-9-]+$/) bad = 1
            if ($0 == "      - public-api") hit = 1
            n++
            next
        }
        in_list && /^     / { bad = 1; next }
        in_list { in_list = 0 }
        END { exit !(seen && !bad && n > 0 && !hit) }
    ' <<<"$job"
}

# Holds when pre-commit config $1 still carries this self-test's own hook
# (so it is the real config, not an empty file) and names neither the
# retired hook id nor the retired leg script anywhere.
precommit_has_no_public_api_hook() {
    [[ -f "$1" ]] || return 1
    grep -qx '        entry: bash scripts/test-gate.test.sh' "$1" || return 1
    ! grep -qF -e "$RETIRED_HOOK_ID" -e "$RETIRED_LEG" "$1"
}

# Holds when the scripts/ dir under root $1 exists and lacks the retired leg.
retired_leg_absent() {
    [[ -d "$1/scripts" && ! -e "$1/scripts/$RETIRED_LEG.sh" ]]
}

WIRING_DIR="$TMP/wiring"
mkdir -p "$WIRING_DIR"

# Writes file $2 with sed expression $3 applied to $WIRING_DIR/$1 and prints
# its path; fails when the expression changed nothing, so a stale plant
# cannot pass as a caught one.
planted() {
    local path="$WIRING_DIR/$1"
    if ! sed -E "$3" "$2" >"$path" || cmp -s "$2" "$path"; then
        echo "plant '$3' did not apply" >&2
        return 1
    fi
    echo "$path"
}

# Runs predicate $2 on $3 (must hold), then on the copy of $3 planted with
# each remaining sed expression (each must fail).
assert_wiring() {
    local desc="$1" predicate="$2" real="$3" expr copy
    shift 3
    if "$predicate" "$real"; then
        pass "wiring: $desc"
    else
        fail "wiring: $desc: does not hold on ${real#"$REPO_ROOT"/}"
    fi
    for expr in "$@"; do
        if ! copy="$(planted "$predicate.yml" "$real" "$expr")"; then
            fail "wiring: $desc: plant did not apply: $expr"
        elif "$predicate" "$copy"; then
            fail "wiring: $desc: holds on a copy planted with '$expr'"
        else
            pass "wiring: $desc: fails on a copy planted with '$expr'"
        fi
    done
}

assert_wiring "the CI public-api job continues on error and ends by running the wrapper" \
    public_api_job_informational "$CI_WORKFLOW" \
    '/^    continue-on-error: true$/d' \
    's#^        run: bash scripts/public-api-report\.sh$#        run: bash scripts/public-api.sh --check all#' \
    's#^        run: bash scripts/public-api-report\.sh$#&\n      - run: "true"#' \
    's/^  public-api:$/  public-api-report:/'

assert_wiring "the required job does not depend on the public-api job" \
    required_skips_public_api "$CI_WORKFLOW" \
    's/^      - osv-scan$/&\n      - public-api/' \
    's/^      - osv-scan$/      - "public-api"/' \
    's/^    needs:$/    needs: [check]/'

assert_wiring "no pre-commit hook runs the public-API check" \
    precommit_has_no_public_api_hook "$PRE_COMMIT_CONFIG" \
    "s/^      - id: test-gate-self-test\$/      - id: $RETIRED_HOOK_ID\\n&/" \
    "s#^        entry: bash scripts/test-gate\\.test\\.sh\$#&\\n        args: [scripts/$RETIRED_LEG.sh]#" \
    '/^        entry: bash scripts\/test-gate\.test\.sh$/d'

if retired_leg_absent "$REPO_ROOT"; then
    pass "wiring: the retired pre-push leg script is gone"
else
    fail "wiring: scripts/$RETIRED_LEG.sh exists"
fi
mkdir -p "$WIRING_DIR/root/scripts"
touch "$WIRING_DIR/root/scripts/$RETIRED_LEG.sh"
if retired_leg_absent "$WIRING_DIR/root"; then
    fail "wiring: the retired-leg check holds on a root that carries the script"
else
    pass "wiring: the retired-leg check fails on a root that carries the script"
fi
if retired_leg_absent "$WIRING_DIR/no-such-root"; then
    fail "wiring: the retired-leg check holds on a root with no scripts dir"
else
    pass "wiring: the retired-leg check fails closed on a root with no scripts dir"
fi

if ((fails)); then
    echo "test-gate.test.sh: $fails failure(s)" >&2
    exit 1
fi
echo "test-gate.test.sh: all assertions passed"
