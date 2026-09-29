#!/usr/bin/env bash
# Self-test for assert-toolchain.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# The preflight reads the real rust-toolchain.toml and the real rustc /
# rustfmt, so every assertion drives it inside a throwaway git repo with a
# stub PATH: the pin is whatever the case writes, and rustc / rustfmt are
# tiny scripts printing whatever version line the case wants. That pins both
# directions (a matching toolchain passes, a mismatched or absent one fails)
# without touching the machine's real toolchain.
#
# The stub PATH deliberately carries the handful of real tools the preflight
# itself shells out to (git, sed, awk, head). Omitting one makes a case fail
# for the wrong reason -- an assertion that "fails closed" because `sed` was
# missing proves nothing about the check under test -- so each failure case
# also asserts on the message the preflight printed, not merely on a non-zero
# exit.
#
# Run it from anywhere:
#   bash scripts/assert-toolchain.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFLIGHT="$HERE/assert-toolchain.sh"

fails=0

# Tools the preflight (and the `bash` that launches it) resolve through PATH.
# A stub PATH missing any of these makes the preflight die for a reason
# unrelated to the assertion -- `bash` itself is the easy one to forget, and
# omitting it turns every case into a spurious "fail-closed".
STUB_TOOLS=(bash git sed awk head)

# Build a throwaway repo pinned to $1, with a rustc stub printing $2 and a
# rustfmt stub printing $3 (an empty string means "that tool is absent"),
# then run the preflight in it with only the stub PATH visible. Prints the
# preflight's combined output; returns its exit code.
run_probe() {
    local toml_body="$1" rustc_out="$2" rustfmt_out="$3"
    local tmp out rc
    tmp="$(mktemp -d)"
    out="$(
        cd "$tmp" || exit 2
        git init -q .
        mkdir -p scripts stubbin
        cp "$PREFLIGHT" scripts/assert-toolchain.sh
        printf '%s\n' "$toml_body" >rust-toolchain.toml
        for tool in "${STUB_TOOLS[@]}"; do
            ln -sf "$(command -v "$tool")" "stubbin/$tool"
        done
        for spec in "rustc:$rustc_out" "rustfmt:$rustfmt_out"; do
            local name="${spec%%:*}" line="${spec#*:}"
            [[ -n "$line" ]] || continue
            printf '#!/bin/sh\nprintf "%%s\\n" %q\n' "$line" >"stubbin/$name"
            chmod +x "stubbin/$name"
        done
        PATH="$tmp/stubbin" bash scripts/assert-toolchain.sh 2>&1
    )"
    rc=$?
    rm -rf "$tmp"
    printf '%s\n' "$out"
    return $rc
}

PINNED_TOML='[toolchain]
channel = "1.95.0"'

MATCHING_RUSTC='rustc 1.95.0 (59807616e 2026-04-14)'
MATCHING_RUSTFMT='rustfmt 1.9.0-stable (59807616e1 2026-04-14)'

assert_pass() {
    local desc="$1" toml="$2" rustc_out="$3" rustfmt_out="$4"
    local out
    if out="$(run_probe "$toml" "$rustc_out" "$rustfmt_out")"; then
        if printf '%s' "$out" | grep -q 'toolchain: PASS'; then
            echo "PASS: accepted -- $desc"
        else
            echo "FAIL: exited 0 without a PASS line -- $desc"
            printf '%s\n' "$out"
            fails=$((fails + 1))
        fi
    else
        echo "FAIL: expected PASS but rejected -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}

# A failure case must fail for the INTENDED reason: $5 is a substring the
# preflight's own message must contain, which is what distinguishes a real
# rejection from the preflight dying on a broken stub environment.
assert_reject() {
    local desc="$1" toml="$2" rustc_out="$3" rustfmt_out="$4" expect="$5"
    local out
    if out="$(run_probe "$toml" "$rustc_out" "$rustfmt_out")"; then
        echo "FAIL: expected rejection but passed -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    elif printf '%s' "$out" | grep -qF "$expect"; then
        echo "PASS: rejected -- $desc"
    else
        echo "FAIL: rejected for the WRONG reason (no '$expect') -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}

assert_pass "rustc and rustfmt from the pinned toolchain build" \
    "$PINNED_TOML" "$MATCHING_RUSTC" "$MATCHING_RUSTFMT"

assert_reject "neither version line carries a toolchain build id" \
    "$PINNED_TOML" 'rustc 1.95.0' 'rustfmt 1.9.0-stable' \
    "cannot verify rustfmt provenance"

assert_reject "rustc carries a build id but rustfmt does not" \
    "$PINNED_TOML" "$MATCHING_RUSTC" 'rustfmt 1.9.0-stable' \
    "cannot verify rustfmt provenance"

assert_reject "rustc is a different version than the pin" \
    "$PINNED_TOML" 'rustc 1.90.0 (aaaaaaaaa 2025-01-01)' "$MATCHING_RUSTFMT" \
    "rustc reports 'rustc 1.90.0"

assert_reject "rustfmt is from a different toolchain build than rustc" \
    "$PINNED_TOML" "$MATCHING_RUSTC" 'rustfmt 1.8.0-stable (bbbbbbbbbb 2025-11-02)' \
    "not from the same toolchain build"

assert_reject "rustc is absent" \
    "$PINNED_TOML" '' "$MATCHING_RUSTFMT" \
    "rustc not found on PATH"

assert_reject "rustfmt is absent" \
    "$PINNED_TOML" "$MATCHING_RUSTC" '' \
    "rustfmt not found on PATH"

assert_reject "the pin file has no channel line" \
    '[toolchain]
components = ["rustfmt"]' "$MATCHING_RUSTC" "$MATCHING_RUSTFMT" \
    "no 'channel"

# The wrong-channel message must name the three ways the pin gets bypassed;
# without them the rejection tells an operator nothing actionable.
for cause in RUSTUP_TOOLCHAIN "rustup override" "first on PATH"; do
    assert_reject "wrong-channel message names cause: $cause" \
        "$PINNED_TOML" 'rustc 1.90.0 (aaaaaaaaa 2025-01-01)' "$MATCHING_RUSTFMT" \
        "$cause"
done

if [[ "$fails" -ne 0 ]]; then
    echo "assert-toolchain.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "assert-toolchain.test.sh: all assertions passed"
