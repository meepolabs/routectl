#!/usr/bin/env bash
# Self-test for check-live-gate-isolation.sh's static contract. Runs no
# namespace and no cargo: it drives the checker's --self-check mode.
#
# Pins:
#   - the deadline budget fits the CI step timeout, and the job timeout
#     leaves room for the job's other steps;
#   - a deadline raised past the budget makes the checker refuse to run;
#   - the fixed credential names are always planted, whatever the source
#     scan finds, and dropping one is caught;
#   - the source scan resolves from the repo root, whatever the cwd.
#
# Every "passes" assertion has a control proving the same assertion fails
# on a copy of the checker planted with the defect.
#
# Run it from anywhere:
#   bash scripts/check-live-gate-isolation.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
CHECKER="$HERE/check-live-gate-isolation.sh"
WORKFLOW="$REPO_ROOT/.github/workflows/ci.yml"
STEP_NAME="standard test gate makes no network attempt"
# Minutes the test job needs besides the isolation step: setup, the release
# workspace test before it (about 37 minutes on recent runs), and the steps
# after it (about 6).
OTHER_STEPS_MINUTES=60

# The provider and router-smoke credential names that must be planted even
# if no live source mentions them.
EXPECTED_FIXED=(
    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    GEMINI_API_KEY
    AWS_ACCESS_KEY_ID
    AWS_SECRET_ACCESS_KEY
    AWS_SESSION_TOKEN
    AWS_BEARER_TOKEN_BEDROCK
    AWS_REGION
    ROUTECTL_LIVE_BASE_URL
    ROUTECTL_LIVE_API_KEY
)
# A name only the live sources carry, so its presence proves the scan ran.
SOURCE_ONLY_NAME=OPENROUTER_API_KEY

fails=0
pass() { echo "PASS: $*"; }
fail() {
    echo "FAIL: $*" >&2
    fails=$((fails + 1))
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A copy of the repo surface the checker reads, with the checker edited by
# the given sed expression. Prints the copy's checker path.
mutant() {
    local name="$1" expr="$2" root="$TMP/$1"
    mkdir -p "$root/scripts" "$root/crates/routectl-cli/tests/live_matrix"
    cp "$REPO_ROOT"/crates/routectl-cli/tests/live_*.rs "$root/crates/routectl-cli/tests/"
    cp "$REPO_ROOT"/crates/routectl-cli/tests/live_matrix/*.rs "$root/crates/routectl-cli/tests/live_matrix/"
    sed -e "$expr" "$CHECKER" >"$root/scripts/check-live-gate-isolation.sh"
    if cmp -s "$CHECKER" "$root/scripts/check-live-gate-isolation.sh"; then
        echo "mutation '$name' changed nothing" >&2
        return 1
    fi
    echo "$root/scripts/check-live-gate-isolation.sh"
}

# --- budget ---------------------------------------------------------------

self_check_budget() { bash "$1" --self-check 2>&1 | head -n1; }

budget_line="$(self_check_budget "$CHECKER")"
if [[ "$budget_line" == "budget worst_case="* ]]; then
    pass "deadline budget fits: $budget_line"
else
    fail "deadline budget check failed: $budget_line"
fi

if m="$(mutant overflow 's/^STANDARD_DEADLINE=.*/STANDARD_DEADLINE=4000/')"; then
    out="$(self_check_budget "$m")"
    if [[ "$out" == *"exceeds STEP_BUDGET"* ]]; then
        pass "control: a deadline past the budget is refused"
    else
        fail "control: raised STANDARD_DEADLINE was not refused: $out"
    fi
    if bash "$m" --require-netns >/dev/null 2>&1; then
        fail "control: an over-budget checker still ran"
    else
        pass "control: an over-budget checker exits non-zero before any leg"
    fi
else
    fail "could not build the overflow mutant"
fi

step_budget="$(sed -n 's/^STEP_BUDGET=//p' "$CHECKER")"
step_minutes="$(awk -v name="$STEP_NAME" '
    $0 ~ "- name: " name { found = 1; next }
    found && /timeout-minutes:/ { print $2; exit }
    found && /- name:/ { exit }' "$WORKFLOW")"
job_minutes="$(awk '
    /^  test:$/ { in_job = 1; next }
    in_job && /^  [a-z-]+:$/ { exit }
    in_job && /^    timeout-minutes:/ { print $2; exit }' "$WORKFLOW")"
if [[ -z "$step_minutes" || -z "$job_minutes" ]]; then
    fail "could not read step ($step_minutes) or job ($job_minutes) timeout from $WORKFLOW"
elif ((step_minutes * 60 != step_budget)); then
    fail "STEP_BUDGET=$step_budget but the CI step timeout is ${step_minutes}m"
elif ((job_minutes < step_minutes + OTHER_STEPS_MINUTES)); then
    fail "job timeout ${job_minutes}m leaves under ${OTHER_STEPS_MINUTES}m beyond the ${step_minutes}m step"
else
    pass "CI step timeout ${step_minutes}m = STEP_BUDGET; job ${job_minutes}m covers step + ${OTHER_STEPS_MINUTES}m"
fi

# --- planted names --------------------------------------------------------

# Prints the fixed names missing from a checker's planted set.
missing_fixed() {
    local names name
    names="$(bash "$1" --self-check 2>/dev/null | tail -n +2)"
    for name in "${EXPECTED_FIXED[@]}"; do
        grep -qx "$name" <<<"$names" || echo "$name"
    done
}

# Sparse copy: live sources carrying one unrelated SCREAMING_SNAKE literal,
# so only the fixed list can supply the expected names.
root="$TMP/sparse"
mkdir -p "$root/scripts" "$root/crates/routectl-cli/tests/live_matrix"
printf 'const K: &str = "SPARSE_ONLY_NAME";\n' >"$root/crates/routectl-cli/tests/live_x.rs"
printf '\n' >"$root/crates/routectl-cli/tests/live_matrix/empty.rs"
cp "$CHECKER" "$root/scripts/check-live-gate-isolation.sh"
gone="$(missing_fixed "$root/scripts/check-live-gate-isolation.sh")"
if [[ -z "$gone" ]]; then
    pass "every fixed credential name is planted even when no live source names it"
else
    fail "fixed names not planted: $(tr '\n' ' ' <<<"$gone")"
fi

if m="$(mutant dropname '/^FIXED_PLANTED_NAMES=(/,/^)/{/^    ROUTECTL_LIVE_API_KEY$/d}')"; then
    gone="$(missing_fixed "$m")"
    if [[ "$gone" == "ROUTECTL_LIVE_API_KEY" ]]; then
        pass "control: removing a fixed name is caught"
    else
        fail "control: dropped ROUTECTL_LIVE_API_KEY not caught (missing: ${gone:-none})"
    fi
else
    fail "could not build the dropped-name mutant"
fi

# --- invocation from anywhere ----------------------------------------------

scan_from() { (cd "$1" && bash "$2" --self-check 2>/dev/null | grep -cx "$SOURCE_ONLY_NAME"); }

for dir in "$REPO_ROOT/crates/routectl-cli" /; do
    if [[ "$(scan_from "$dir" "$CHECKER")" == 1 ]]; then
        pass "source scan finds live-source names when run from $dir"
    else
        fail "source scan found no live-source names when run from $dir"
    fi
done

# shellcheck disable=SC2016 # the sed pattern matches a literal $REPO_ROOT
if m="$(mutant relative 's|"\$REPO_ROOT"/crates/routectl-cli/tests/|crates/routectl-cli/tests/|')"; then
    if [[ "$(scan_from / "$m")" == 1 ]]; then
        fail "control: cwd-relative LIVE_SOURCES still found names from /"
    else
        pass "control: cwd-relative LIVE_SOURCES finds nothing from /"
    fi
else
    fail "could not build the cwd-relative mutant"
fi

if ((fails)); then
    echo "check-live-gate-isolation.test.sh: $fails failure(s)" >&2
    exit 1
fi
echo "check-live-gate-isolation.test.sh: all assertions passed"
