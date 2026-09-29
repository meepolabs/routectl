#!/usr/bin/env bash
# Self-test for assert-aws-lc-env.sh. Exits 0 when all assertions pass,
# non-zero otherwise.
#
# Every case runs the preflight under `env -i` with only PATH plus the
# variables the case plants, so the caller's own environment never decides a
# verdict. Each rejection case also asserts on the variable name the
# preflight printed, so a case cannot pass because the preflight died for an
# unrelated reason, and every case asserts the planted value never reaches
# the output.
#
# Run it from anywhere:
#   bash scripts/assert-aws-lc-env.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFLIGHT="$HERE/assert-aws-lc-env.sh"

fails=0

# A value distinctive enough that finding it in the output can only mean the
# preflight printed a value.
readonly CANARY='canary-value-7f3a'

# The two pins exactly as the workflows set them.
readonly PINS=(AWS_LC_SYS_USE_SYSTEM=0 AWS_LC_SYS_STATIC=1)

# Run the preflight with only PATH and the NAME=VALUE arguments set.
probe() {
    env -i PATH="$PATH" "$@" bash "$PREFLIGHT" 2>&1
}

value_leaked() {
    local desc="$1" out="$2"
    if printf '%s' "$out" | grep -qF "$CANARY"; then
        echo "FAIL: output carries a variable value -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
        return 0
    fi
    return 1
}

assert_pass() {
    local desc="$1" out
    shift
    if out="$(probe "$@")" && printf '%s' "$out" | grep -q 'aws-lc-env: PASS'; then
        value_leaked "$desc" "$out" || echo "PASS: accepted -- $desc"
    else
        echo "FAIL: expected PASS -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}

assert_reject() {
    local desc="$1" expect="$2" out
    shift 2
    if out="$(probe "$@")"; then
        echo "FAIL: expected rejection but passed -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    elif ! printf '%s' "$out" | grep -qF "aws-lc-env: FAIL: $expect:"; then
        echo "FAIL: rejected for the WRONG reason (no '$expect') -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    else
        value_leaked "$desc" "$out" || echo "PASS: rejected -- $desc"
    fi
}

assert_pass "clean environment"
assert_pass "both workflow pins at their pinned values" "${PINS[@]}"
assert_pass "unrelated AWS-LC build variable" "${PINS[@]}" AWS_LC_SYS_CMAKE_BUILDER="$CANARY"

assert_reject "target-suffixed USE_SYSTEM beside the pins" \
    AWS_LC_SYS_USE_SYSTEM_x86_64_unknown_linux_gnu \
    "${PINS[@]}" AWS_LC_SYS_USE_SYSTEM_x86_64_unknown_linux_gnu="$CANARY"

assert_reject "target-suffixed STATIC beside the pins" \
    AWS_LC_SYS_STATIC_aarch64_apple_darwin \
    "${PINS[@]}" AWS_LC_SYS_STATIC_aarch64_apple_darwin="$CANARY"

assert_reject "suffixed SYSTEM_BINDINGS" \
    AWS_LC_SYS_SYSTEM_BINDINGS_x86_64_pc_windows_msvc \
    "${PINS[@]}" AWS_LC_SYS_SYSTEM_BINDINGS_x86_64_pc_windows_msvc="$CANARY"

assert_reject "suffixed NO_PREFIX" \
    AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu \
    "${PINS[@]}" AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu="$CANARY"

assert_reject "suffix that is not a shell identifier" \
    AWS_LC_SYS_STATIC_x86_64-unknown-linux-gnu \
    "${PINS[@]}" AWS_LC_SYS_STATIC_x86_64-unknown-linux-gnu="$CANARY"

assert_reject "unsuffixed SYSTEM_DIR" \
    AWS_LC_SYS_SYSTEM_DIR \
    "${PINS[@]}" AWS_LC_SYS_SYSTEM_DIR="$CANARY"

assert_reject "suffixed SYSTEM_DIR" \
    AWS_LC_SYS_SYSTEM_DIR_x86_64_unknown_linux_gnu \
    "${PINS[@]}" AWS_LC_SYS_SYSTEM_DIR_x86_64_unknown_linux_gnu="$CANARY"

assert_reject "USE_SYSTEM at a value other than 0" \
    AWS_LC_SYS_USE_SYSTEM \
    AWS_LC_SYS_USE_SYSTEM="$CANARY" AWS_LC_SYS_STATIC=1

assert_reject "USE_SYSTEM at a value aws-lc-sys also reads as false" \
    AWS_LC_SYS_USE_SYSTEM \
    AWS_LC_SYS_USE_SYSTEM=no AWS_LC_SYS_STATIC=1

assert_reject "STATIC at a value other than 1" \
    AWS_LC_SYS_STATIC \
    AWS_LC_SYS_USE_SYSTEM=0 AWS_LC_SYS_STATIC="$CANARY"

assert_reject "STATIC set but empty" \
    AWS_LC_SYS_STATIC \
    AWS_LC_SYS_USE_SYSTEM=0 AWS_LC_SYS_STATIC=

assert_reject "lower-case spelling of STATIC" \
    aws_lc_sys_static \
    "${PINS[@]}" aws_lc_sys_static=1

assert_reject "lower-case spelling of a suffixed USE_SYSTEM" \
    aws_lc_sys_use_system_x86_64_pc_windows_msvc \
    "${PINS[@]}" aws_lc_sys_use_system_x86_64_pc_windows_msvc="$CANARY"

if [[ "$fails" -ne 0 ]]; then
    echo "assert-aws-lc-env.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "assert-aws-lc-env.test.sh: all assertions passed"
