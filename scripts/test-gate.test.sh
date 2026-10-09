#!/usr/bin/env bash
# Self-test for the gate registry's public-api subcommand and the pre-push
# leg that reaches it. Runs no cargo and no cargo-public-api.
#
# Pins:
#   - `test-gate.sh --print public-api` is exactly the public-api.sh
#     --check over every crate, and the subcommand refuses extra arguments;
#   - the pre-push leg runs that subcommand only when cargo-public-api at
#     the pinned version AND the pinned nightly are installed, and
#     propagates its failure, so a stale baseline fails the push;
#   - each missing piece of tooling makes the leg print the one skip line
#     and exit 0 without reaching the registry;
#   - a pin public-api.sh no longer carries fails the leg instead of
#     skipping it;
#   - public-api-report.sh always exits 0 and classifies a stub check's
#     outcome as clean / drift / could-not-run, one annotation and one job
#     summary line per run, each case paired with a planted-defect control.
#
# The leg is driven from a scratch copy of scripts/ whose test-gate.sh is a
# stub recording its argv, with stub cargo-public-api, rustup, and rustup's
# cargo / rustdoc / rustc proxies on a PATH that holds only them and the
# system directories, so the caller's own toolchain never decides a verdict.
# Each skip case is paired with the run case it differs from by one missing
# tool; the rustup-absent case also drops the three proxies rustup provides.
#
# Run it from anywhere:
#   bash scripts/test-gate.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REGISTRY="$HERE/test-gate.sh"
LEG="$HERE/public-api-pre-push.sh"
PUBLIC_API="$HERE/public-api.sh"
SYSTEM_PATH=/usr/bin:/bin

fails=0
pass() { echo "PASS: $*"; }
fail() {
    echo "FAIL: $*"
    fails=$((fails + 1))
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

NIGHTLY="$(grep -oE '^PUBLIC_API_NIGHTLY=.*' "$PUBLIC_API" | cut -d= -f2)"
VERSION="$(grep -oE 'cargo-public-api --version [0-9]+\.[0-9]+\.[0-9]+' "$PUBLIC_API" \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
if [[ -z "$NIGHTLY" || -z "$VERSION" ]]; then
    echo "test-gate.test.sh: cannot read the pins from $PUBLIC_API" >&2
    exit 1
fi

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

# --- pre-push leg -----------------------------------------------------------

# A scratch scripts/ dir: the real leg and public-api.sh, and a test-gate.sh
# stub that records its argv and exits with $STUB_GATE_RC.
SCRIPTS="$TMP/scripts"
GATE_LOG="$TMP/gate-invoked"
mkdir -p "$SCRIPTS"
cp "$LEG" "$PUBLIC_API" "$SCRIPTS/"
cat >"$SCRIPTS/test-gate.sh" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >"$GATE_LOG"
exit "\${STUB_GATE_RC:-0}"
STUB

# Writes the tool stubs into a fresh bin dir named $1 and prints its path.
# Options: --tool-version V, --no-tool, --toolchains "A B" (installed names
# without the host triple), --no-rustup, --plain-cargo (a cargo that is not
# the rustup proxy), --no-rustdoc, --no-rust-std (the installed toolchains
# lack that component).
make_bin() {
    local dir="$TMP/bin-$1" tool_version="$VERSION" toolchains="$NIGHTLY" tool=1 rustup=1
    local plain_cargo=0 rustdoc=1 rust_std=1
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --tool-version) tool_version="$2"; shift 2 ;;
            --toolchains) toolchains="$2"; shift 2 ;;
            --no-tool) tool=0; shift ;;
            --no-rustup) rustup=0; shift ;;
            --plain-cargo) plain_cargo=1; shift ;;
            --no-rustdoc) rustdoc=0; shift ;;
            --no-rust-std) rust_std=0; shift ;;
        esac
    done
    mkdir -p "$dir"
    local libdir="$dir/rustlib/lib"
    mkdir -p "$libdir"
    if ((rust_std)); then
        touch "$libdir/libstd-0000000000000000.rlib"
    fi
    if ((tool)); then
        printf '#!/bin/sh\necho "cargo-public-api %s"\n' "$tool_version" >"$dir/cargo-public-api"
        chmod +x "$dir/cargo-public-api"
    fi
    if ((rustup)); then
        # Models `rustup which --toolchain T BIN`: T resolves only when it is
        # an installed name, bare or with the host triple appended.
        cat >"$dir/rustup" <<STUB
#!/usr/bin/env bash
triple=x86_64-unknown-linux-gnu
installed=(stable $toolchains)
if [[ "\$1 \$2" == "which --toolchain" ]]; then
    for t in "\${installed[@]}"; do
        if [[ "\$3" == "\$t" || "\$3" == "\$t-\$triple" ]]; then
            echo "/stub/toolchains/\$t-\$triple/bin/\$4"
            exit 0
        fi
    done
    echo "error: toolchain '\$3' is not installed" >&2
    exit 1
fi
echo "rustup stub: unsupported: \$*" >&2
exit 1
STUB
        chmod +x "$dir/rustup"
        # Models a rustup proxy invoked as `<proxy> +T ...`: T must be an
        # installed name and the proxied component present.
        local proxy present
        for proxy in cargo rustdoc rustc; do
            present=1
            [[ "$proxy" == rustdoc ]] && present=$rustdoc
            cat >"$dir/$proxy" <<STUB
#!/usr/bin/env bash
installed=(stable $toolchains)
found=0
for t in "\${installed[@]}"; do
    [[ "\$1" == "+\$t" ]] && found=1
done
if ((!found)); then
    echo "error: toolchain '\${1#+}' is not installed" >&2
    exit 1
fi
if ((!$present)); then
    echo "error: '$proxy' is not installed for the toolchain" >&2
    exit 1
fi
if [[ "$proxy \$2 \$3" == "rustc --print target-libdir" ]]; then
    echo "$libdir"
    exit 0
fi
echo "$proxy 1.0.0-nightly (stub)"
STUB
            chmod +x "$dir/$proxy"
        done
    fi
    if ((plain_cargo)); then
        cat >"$dir/cargo" <<'STUB'
#!/bin/sh
case "$1" in +*) echo "error: no such command: $1" >&2; exit 101 ;; esac
echo "cargo 1.0.0"
STUB
        chmod +x "$dir/cargo"
    fi
    echo "$dir"
}

# Runs the scratch leg with PATH = $1 plus the system dirs. Sets OUT and RC.
run_leg() {
    rm -f "$GATE_LOG"
    OUT="$(PATH="$1:$SYSTEM_PATH" STUB_GATE_RC="${2:-0}" bash "$SCRIPTS/public-api-pre-push.sh" 2>&1)"
    RC=$?
}

gate_ran() { [[ -f "$GATE_LOG" && "$(cat "$GATE_LOG")" == "public-api" ]]; }

skip_line() { printf '%s\n' "$OUT" | grep -q '^public-api: SKIPPED locally (.*); CI runs this check\.'; }

for bin in cargo-public-api rustup cargo rustdoc rustc; do
    if [[ -n "$(PATH="$SYSTEM_PATH" command -v "$bin")" ]]; then
        fail "$bin is in $SYSTEM_PATH, so the absent-tool cases cannot be hermetic"
    fi
done

full="$(make_bin full)"
run_leg "$full"
if [[ "$RC" -eq 0 ]] && gate_ran && ! skip_line; then
    pass "tooling present: the leg runs test-gate.sh public-api"
else
    fail "tooling present: rc=$RC gate_ran=$(gate_ran && echo yes || echo no) out='$OUT'"
fi

run_leg "$full" 1
if [[ "$RC" -ne 0 ]] && gate_ran; then
    pass "tooling present: a failing baseline check fails the leg (rc=$RC)"
else
    fail "tooling present: a failing check exited $RC"
fi

assert_skip() {
    local desc="$1" bin="$2" reason="$3"
    run_leg "$bin"
    if [[ "$RC" -ne 0 ]]; then
        fail "$desc: exited $RC, a skip must exit 0 (out='$OUT')"
    elif gate_ran; then
        fail "$desc: the registry ran"
    elif ! skip_line; then
        fail "$desc: no skip line (out='$OUT')"
    elif [[ "$(printf '%s\n' "$OUT" | wc -l)" -ne 1 ]]; then
        fail "$desc: the skip printed more than one line (out='$OUT')"
    elif ! printf '%s\n' "$OUT" | grep -qF "$reason"; then
        fail "$desc: skip line lacks '$reason' (out='$OUT')"
    else
        pass "$desc: skipped with one line, exit 0"
    fi
}

assert_skip "cargo-public-api absent" "$(make_bin no-tool --no-tool)" "cargo-public-api not on PATH"
assert_skip "cargo-public-api at another version" \
    "$(make_bin old-tool --tool-version 0.0.1)" "is not the pinned $VERSION"
assert_skip "pinned nightly absent" \
    "$(make_bin no-nightly --toolchains "nightly-1999-01-01")" "toolchain $NIGHTLY not installed"
assert_skip "rustup absent" "$(make_bin no-rustup --no-rustup)" "rustup not on PATH"
assert_skip "cargo on PATH is not the rustup proxy" \
    "$(make_bin plain-cargo --plain-cargo)" "not the rustup proxy"
assert_skip "pinned nightly lacks rustdoc" \
    "$(make_bin no-rustdoc --no-rustdoc)" "rustdoc for $NIGHTLY unavailable"
assert_skip "pinned nightly lacks the host rust-std" \
    "$(make_bin no-rust-std --no-rust-std)" "lacks rust-std for the host"

# The nightly match must not accept a toolchain whose name merely starts
# with the pin, with or without a separating dash.
assert_skip "only a longer-named toolchain sharing the pin's prefix" \
    "$(make_bin prefix-nightly --toolchains "${NIGHTLY}0")" "toolchain $NIGHTLY not installed"
assert_skip "only a custom toolchain named after the pin" \
    "$(make_bin custom-nightly --toolchains "${NIGHTLY}-custom")" "toolchain $NIGHTLY not installed"

# A public-api.sh without its version pin is a wiring defect, not a skip.
sed -i 's/cargo-public-api --version [0-9.]*/cargo-public-api/' "$SCRIPTS/public-api.sh"
run_leg "$full"
if [[ "$RC" -eq 1 ]] && ! gate_ran && printf '%s\n' "$OUT" | grep -q 'could not read the pins'; then
    pass "an unreadable pin fails the leg"
else
    fail "an unreadable pin: rc=$RC out='$OUT'"
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
report_tool_bin="$(make_bin report-tool)"
report_no_tool_bin="$(make_bin report-no-tool --no-tool)"

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
        'public-api: surface drift for routectl-core -- regenerate its baseline in the same commit' \
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

if ((fails)); then
    echo "test-gate.test.sh: $fails failure(s)" >&2
    exit 1
fi
echo "test-gate.test.sh: all assertions passed"
