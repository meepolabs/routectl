#!/usr/bin/env bash
# Self-test for check-live-gate-isolation.sh's static contract. Runs no
# namespace and no cargo: it drives the checker's --self-check mode.
#
# Pins:
#   - the deadline budget fits the CI step timeout, STEP_BUDGET is exactly
#     the worst case plus the headroom rounded up to a minute, and the job
#     timeout leaves room for the job's other steps;
#   - a deadline raised past the budget makes the checker refuse to run;
#   - the fixed credential names are always planted, whatever the source
#     scan finds, and dropping one is caught;
#   - the source scan resolves from the repo root, whatever the cwd, and
#     covers the router crate's live sources as well as the cli's;
#   - LIVE_TARGETS names exactly the workspace's `test = false` targets
#     gated on `live-integration`, so every live target gets a positive
#     control leg;
#   - the standard-gate leg runs the gate registry's workspace-all-features
#     subcommand, and that subcommand keeps the all-features selection and
#     the --offline / --no-fail-fast flags the namespaced leg depends on;
#   - each live leg's cargo selection is that subcommand's, narrowed only by
#     --test, so the live legs reuse the standard gate's build;
#   - the registry's conservation subcommand is that same selection plus
#     --test conservation, so CI's conservation step reuses it too.
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
GATE_REGISTRY="$HERE/test-gate.sh"
WORKFLOW="$REPO_ROOT/.github/workflows/ci.yml"
STEP_NAME="standard test gate makes no network attempt"
# Minutes the test job keeps beyond the isolation step. That step is the
# job's only workspace build and test run; besides it the job has only setup
# and the conservation step, which reuses its build, so this is headroom for
# a cold cache and a slow runner rather than a measured cost.
OTHER_STEPS_MINUTES=60

# The provider credential names that must be planted even if no live
# source mentions them.
EXPECTED_FIXED=(
    ANTHROPIC_API_KEY
    OPENAI_API_KEY
    GEMINI_API_KEY
    AWS_ACCESS_KEY_ID
    AWS_SECRET_ACCESS_KEY
    AWS_SESSION_TOKEN
    AWS_BEARER_TOKEN_BEDROCK
    AWS_REGION
)
# A name only the cli live sources carry, so its presence proves the scan ran.
SOURCE_ONLY_NAME=OPENROUTER_API_KEY
# The names only the router live smoke carries, so their presence proves the
# scan reached the router crate.
ROUTER_ONLY_NAMES=(ROUTECTL_LIVE_BASE_URL ROUTECTL_LIVE_API_KEY)

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
    mkdir -p "$root/scripts" "$root/crates/routectl-cli/tests/live_matrix" "$root/crates/routectl-router/tests"
    cp "$REPO_ROOT"/crates/routectl-cli/tests/live_*.rs "$root/crates/routectl-cli/tests/"
    cp "$REPO_ROOT"/crates/routectl-cli/tests/live_matrix/*.rs "$root/crates/routectl-cli/tests/live_matrix/"
    cp "$REPO_ROOT"/crates/routectl-router/tests/live_*.rs "$root/crates/routectl-router/tests/"
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
headroom="$(sed -n 's/^BUDGET_HEADROOM=//p' "$CHECKER")"
worst_case="$(sed -n 's/^budget worst_case=\([0-9]*\) .*/\1/p' <<<"$budget_line")"
if [[ -z "$worst_case" || -z "$headroom" ]]; then
    fail "could not read worst_case ($worst_case) or BUDGET_HEADROOM ($headroom)"
else
    derived_budget=$(((worst_case + headroom + 59) / 60 * 60))
    if ((step_budget == derived_budget)); then
        pass "STEP_BUDGET=$step_budget is worst case ${worst_case}s + ${headroom}s headroom, rounded up to a minute"
    else
        fail "STEP_BUDGET=$step_budget, but worst case ${worst_case}s + ${headroom}s headroom rounds up to $derived_budget"
    fi
fi

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
mkdir -p "$root/scripts" "$root/crates/routectl-cli/tests/live_matrix" "$root/crates/routectl-router/tests"
printf 'const K: &str = "SPARSE_ONLY_NAME";\n' >"$root/crates/routectl-cli/tests/live_x.rs"
printf '\n' >"$root/crates/routectl-cli/tests/live_matrix/empty.rs"
printf '\n' >"$root/crates/routectl-router/tests/live_x.rs"
cp "$CHECKER" "$root/scripts/check-live-gate-isolation.sh"
gone="$(missing_fixed "$root/scripts/check-live-gate-isolation.sh")"
if [[ -z "$gone" ]]; then
    pass "every fixed credential name is planted even when no live source names it"
else
    fail "fixed names not planted: $(tr '\n' ' ' <<<"$gone")"
fi

if m="$(mutant dropname '/^FIXED_PLANTED_NAMES=(/,/^)/{/^    ANTHROPIC_API_KEY$/d}')"; then
    gone="$(missing_fixed "$m")"
    if [[ "$gone" == "ANTHROPIC_API_KEY" ]]; then
        pass "control: removing a fixed name is caught"
    else
        fail "control: dropped ANTHROPIC_API_KEY not caught (missing: ${gone:-none})"
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

# Prints the router-only names a checker's planted set lacks.
missing_router_names() {
    local names name
    names="$(bash "$1" --self-check 2>/dev/null | tail -n +2)"
    for name in "${ROUTER_ONLY_NAMES[@]}"; do
        grep -qx "$name" <<<"$names" || echo "$name"
    done
}

gone="$(missing_router_names "$CHECKER")"
if [[ -z "$gone" ]]; then
    pass "source scan plants the router live smoke's variables"
else
    fail "router live smoke variables not planted: $(tr '\n' ' ' <<<"$gone")"
fi

# shellcheck disable=SC2016 # the sed pattern matches a literal $REPO_ROOT
if m="$(mutant no-router-scan '\|^    "\$REPO_ROOT"/crates/routectl-router/tests/live_\*\.rs$|d')"; then
    if [[ -n "$(missing_router_names "$m")" ]]; then
        pass "control: a scan without the router live sources is caught"
    else
        fail "control: a scan without the router live sources still planted their variables"
    fi
else
    fail "could not build the no-router-scan mutant"
fi

# --- live targets ------------------------------------------------------------

# Every `[[test]]` in a workspace crate manifest that is `test = false` and
# requires `live-integration`, sorted.
manifest_live_targets() {
    awk '
        function flush() {
            if (in_test && name != "" && off && live) print name
            in_test = 0; name = ""; off = 0; live = 0
        }
        /^\[/ { flush() }
        /^\[\[test\]\]$/ { in_test = 1; next }
        in_test && /^name *= */ { name = $0; sub(/^name *= *"/, "", name); sub(/".*$/, "", name) }
        in_test && /^test *= *false *$/ { off = 1 }
        in_test && /^required-features *=.*"live-integration"/ { live = 1 }
        END { flush() }' "$REPO_ROOT"/crates/*/Cargo.toml | sort -u
}
# A checker's LIVE_TARGETS, sorted.
checker_live_targets() {
    sed -n 's/^LIVE_TARGETS=(\(.*\))$/\1/p' "$1" | tr ' ' '\n' | sed '/^$/d' | sort -u
}
# Prints the mismatch, or nothing when the checker's LIVE_TARGETS is the
# manifests' live-target set.
live_targets_mismatch() {
    local want got
    want="$(manifest_live_targets)"
    got="$(checker_live_targets "$1")"
    if [[ -z "$want" || -z "$got" ]]; then
        echo "unreadable: manifests='$want' checker='$got'"
    elif [[ "$got" != "$want" ]]; then
        echo "checker='$(tr '\n' ' ' <<<"$got")' manifests='$(tr '\n' ' ' <<<"$want")'"
    fi
}

gone="$(live_targets_mismatch "$CHECKER")"
if [[ -z "$gone" ]]; then
    pass "LIVE_TARGETS is every live-integration test = false target: $(manifest_live_targets | tr '\n' ' ')"
else
    fail "LIVE_TARGETS differs from the manifests' live targets: $gone"
fi

if m="$(mutant drop-target '/^LIVE_TARGETS=(/s/ live_learned_capability)$/)/')"; then
    if [[ -n "$(live_targets_mismatch "$m")" ]]; then
        pass "control: a live target missing from LIVE_TARGETS is caught"
    else
        fail "control: a live target missing from LIVE_TARGETS passed"
    fi
else
    fail "could not build the drop-target mutant"
fi

# --- standard gate source ---------------------------------------------------

# shellcheck disable=SC2016 # matches the literal "$HERE" in the checker source
REGISTRY_GATE_LINE='STANDARD_GATE=(bash "$HERE/test-gate.sh" workspace-all-features)'
runs_registry_gate() { grep -qxF "$REGISTRY_GATE_LINE" "$1"; }

if runs_registry_gate "$CHECKER"; then
    pass "standard-gate leg runs test-gate.sh workspace-all-features"
else
    fail "standard-gate leg does not run test-gate.sh workspace-all-features"
fi

if m="$(mutant inline-gate 's|^STANDARD_GATE=.*|STANDARD_GATE=(cargo test --workspace --all-features --offline --no-fail-fast)|')"; then
    if runs_registry_gate "$m"; then
        fail "control: an inline standard-gate command still passed the registry check"
    else
        pass "control: an inline standard-gate command is caught"
    fi
else
    fail "could not build the inline-gate mutant"
fi

# A registry copy's command for a subcommand, as its --print mode prints it.
registry_command() { bash "$1" --print "$2" 2>/dev/null; }

# Runs the registry's pre-push subcommand against a `cargo` stub on PATH,
# with any extra registry flags given, and prints what the stub recorded:
# its argv when it ran, nothing when the registry never reached it. The stub
# exits 0, so nothing builds.
stub_cargo_run() {
    local dir="$TMP/stub-cargo" log="$TMP/stub-cargo/invoked"
    mkdir -p "$dir"
    rm -f "$log"
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >"%s"\n' "$log" >"$dir/cargo"
    chmod +x "$dir/cargo"
    PATH="$dir:$PATH" bash "$GATE_REGISTRY" "$@" pre-push >/dev/null 2>&1
    [[ -f "$log" ]] && cat "$log"
    return 0
}

invoked="$(export TEST_GATE_DRY_RUN=1; stub_cargo_run)"
if [[ "$invoked" == "test --workspace "* ]]; then
    pass "an inherited TEST_GATE_DRY_RUN=1 does not turn a gate into a no-op"
else
    fail "with TEST_GATE_DRY_RUN=1 exported, pre-push never ran cargo (stub saw: '$invoked')"
fi

invoked="$(stub_cargo_run --print)"
if [[ -z "$invoked" ]]; then
    pass "control: --print never runs cargo"
else
    fail "control: --print ran cargo (stub saw: '$invoked')"
fi

# A copy of the registry edited by the given sed expression. Prints its path.
registry_mutant() {
    local name="$1" expr="$2" path="$TMP/test-gate-$1.sh"
    sed -e "$expr" "$GATE_REGISTRY" >"$path"
    if cmp -s "$GATE_REGISTRY" "$path"; then
        echo "registry mutation '$name' changed nothing" >&2
        return 1
    fi
    echo "$path"
}

# Prints each flag the namespaced leg needs that a registry copy's
# workspace-all-features command lacks.
missing_gate_flags() {
    local cmd flag
    cmd="$(registry_command "$1" workspace-all-features)"
    if [[ "$cmd" != "cargo test --workspace "* ]]; then
        echo "<no cargo test --workspace command>"
        return
    fi
    for flag in --all-features "--profile test-release" --offline --no-fail-fast; do
        [[ " $cmd " == *" $flag "* ]] || echo "$flag"
    done
}

gone="$(missing_gate_flags "$GATE_REGISTRY")"
if [[ -z "$gone" ]]; then
    pass "test-gate.sh workspace-all-features carries every flag the namespaced leg needs"
else
    fail "test-gate.sh workspace-all-features lacks: $(tr '\n' ' ' <<<"$gone")"
fi

if m="$(registry_mutant online 's/^\(WORKSPACE_ALL_FEATURES=.*\) --offline/\1/;s/^    --offline --no-fail-fast)$/    --no-fail-fast)/')"; then
    if [[ "$(missing_gate_flags "$m")" == "--offline" ]]; then
        pass "control: a registry command without --offline is caught"
    else
        fail "control: dropped --offline not caught"
    fi
else
    fail "could not build the dropped --offline registry mutant"
fi

# --- registry selections ----------------------------------------------------

# The registry's workspace-all-features cargo arguments, up to the harness
# separator.
registry_selection() {
    registry_command "$1" "${2:-workspace-all-features}" | sed -n 's/^\(cargo test .*\) --\( .*\)\{0,1\}$/\1/p'
}

# Prints the mismatch, or nothing when a registry copy's conservation
# command is its workspace-all-features selection plus --test conservation.
conservation_mismatch() {
    local want got
    want="$(registry_selection "$1")"
    got="$(registry_selection "$1" conservation)"
    if [[ -z "$want" || -z "$got" ]]; then
        echo "unreadable: workspace-all-features='$want' conservation='$got'"
    elif [[ "$got" != "$want --test conservation" ]]; then
        echo "conservation='$got' workspace-all-features='$want'"
    fi
}

gone="$(conservation_mismatch "$GATE_REGISTRY")"
if [[ -z "$gone" ]]; then
    pass "test-gate.sh conservation is the workspace-all-features selection plus --test conservation"
else
    fail "test-gate.sh conservation diverges from workspace-all-features: $gone"
fi

# shellcheck disable=SC2016 # matches the literal array expansion in the registry
if m="$(registry_mutant conservation-narrow 's|^        run "${WORKSPACE_ALL_FEATURES\[@\]}" --test conservation|        run cargo test -p routectl-cli --profile test-release --test conservation|')"; then
    if [[ -n "$(conservation_mismatch "$m")" ]]; then
        pass "control: a conservation command selecting another feature set is caught"
    else
        fail "control: a conservation command selecting another feature set passed"
    fi
else
    fail "could not build the conservation-narrow mutant"
fi

# --- live leg selection ----------------------------------------------------

# A checker's live_command cargo arguments, up to its --test.
live_selection() {
    awk '/^live_command\(\) \{$/ { on = 1; next }
        on && /^}$/ { exit }
        on { print }' "$1" | tr '\n' ' ' | tr -s ' ' |
        sed -n 's/^ *LIVE_COMMAND=(\(cargo test .*\) --test .*$/\1/p'
}
# Prints the mismatch, or nothing when the live legs select what the
# standard gate builds.
live_selection_mismatch() {
    local want got
    want="$(registry_selection "$GATE_REGISTRY")"
    got="$(live_selection "$1")"
    if [[ -z "$want" || -z "$got" ]]; then
        echo "unreadable: registry='$want' live='$got'"
    elif [[ "$got" != "$want" ]]; then
        echo "live='$got' registry='$want'"
    fi
}

gone="$(live_selection_mismatch "$CHECKER")"
if [[ -z "$gone" ]]; then
    pass "live legs select the standard gate's build, narrowed by --test"
else
    fail "live leg selection differs from the standard gate: $gone"
fi

if m="$(mutant live-narrow 's|LIVE_COMMAND=(cargo test --workspace --all-features|LIVE_COMMAND=(cargo test -p routectl-cli --features live-integration|')"; then
    if [[ -n "$(live_selection_mismatch "$m")" ]]; then
        pass "control: a live leg selecting another feature set is caught"
    else
        fail "control: a live leg selecting another feature set passed"
    fi
else
    fail "could not build the live-narrow mutant"
fi

if ((fails)); then
    echo "check-live-gate-isolation.test.sh: $fails failure(s)" >&2
    exit 1
fi
echo "check-live-gate-isolation.test.sh: all assertions passed"
