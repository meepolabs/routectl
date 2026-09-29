#!/usr/bin/env bash
# Self-test for check-aws-lc-pin.sh. Exits 0 when all assertions pass,
# non-zero otherwise.
#
# Every case writes a synthetic workflow into a throwaway directory and runs
# the checker on it, never on this repo's own workflows. Each rejection case
# also asserts on the finding the checker printed, so a case cannot pass
# because the checker died for an unrelated reason.
#
# Run it from anywhere:
#   bash scripts/check-aws-lc-pin.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-aws-lc-pin.sh"

fails=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

HEADER='name: ci
on:
  pull_request:
'

PINNED_ENV='env:
  CARGO_TERM_COLOR: always
  # Build aws-lc-sys from its vendored AWS-LC source as a static library.
  AWS_LC_SYS_USE_SYSTEM: "0"
  AWS_LC_SYS_STATIC: "1"
'

JOBS='jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: bash scripts/assert-aws-lc-env.sh
      - run: cargo build
'

# Write $2 to a fresh workflow file named after case $1; print its path.
workflow() {
    local path="$tmp/$1.yml"
    printf '%s' "$2" >"$path"
    printf '%s\n' "$path"
}

assert_pass() {
    local desc="$1" path="$2" out
    if out="$(bash "$CHECKER" "$path" 2>&1)" && printf '%s' "$out" | grep -q 'aws-lc-pin: PASS'; then
        echo "PASS: accepted -- $desc"
    else
        echo "FAIL: expected PASS -- $desc"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}

assert_reject() {
    local desc="$1" path="$2" expect="$3" out
    if out="$(bash "$CHECKER" "$path" 2>&1)"; then
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

assert_pass "both pins in the top-level env" \
    "$(workflow pinned "$HEADER$PINNED_ENV$JOBS")"

assert_pass "four-space indentation under the top-level env" \
    "$(workflow four-space "$HEADER"'env:
    CARGO_TERM_COLOR: always
    AWS_LC_SYS_USE_SYSTEM: "0"
    AWS_LC_SYS_STATIC: "1"
'"$JOBS")"

assert_reject "pins present only in a comment" \
    "$(workflow comment-only "$HEADER"'env:
  CARGO_TERM_COLOR: always
  # AWS_LC_SYS_USE_SYSTEM: "0"
  # AWS_LC_SYS_STATIC: "1"
'"$JOBS")" \
    'lacks AWS_LC_SYS_USE_SYSTEM: "0"'

assert_reject "pins set only at job level" \
    "$(workflow job-only "$HEADER"'env:
  CARGO_TERM_COLOR: always
jobs:
  build:
    runs-on: ubuntu-22.04
    env:
      AWS_LC_SYS_USE_SYSTEM: "0"
      AWS_LC_SYS_STATIC: "1"
')" \
    'lacks AWS_LC_SYS_STATIC: "1"'

assert_reject "job-level override alongside the top-level pin" \
    "$(workflow job-override "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    env:
      AWS_LC_SYS_STATIC: "0"
')" \
    'set outside the top-level pin'

assert_reject "target-suffixed override in a step env" \
    "$(workflow suffixed "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - run: cargo build
        env:
          AWS_LC_SYS_STATIC_x86_64_unknown_linux_gnu: "0"
')" \
    'set outside the top-level pin'

# The literal $GITHUB_ENV is the workflow text under test, not a shell expansion.
# shellcheck disable=SC2016
assert_reject "override written through GITHUB_ENV" \
    "$(workflow github-env "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - run: echo "AWS_LC_SYS_USE_SYSTEM=1" >> "$GITHUB_ENV"
')" \
    'set outside the top-level pin'

assert_reject "second top-level env mapping" \
    "$(workflow duplicate-env "$HEADER$PINNED_ENV"'env:
  AWS_LC_SYS_STATIC: "0"
'"$JOBS")" \
    'duplicate top-level env: mapping'

assert_reject "STATIC pin missing" \
    "$(workflow no-static "$HEADER"'env:
  CARGO_TERM_COLOR: always
  AWS_LC_SYS_USE_SYSTEM: "0"
'"$JOBS")" \
    'lacks AWS_LC_SYS_STATIC: "1"'

assert_pass "job that never runs cargo needs no environment preflight" \
    "$(workflow no-cargo-job "$HEADER$PINNED_ENV$JOBS"'  scan:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - name: cargo-free lockfile scan
        run: osv-scanner scan source -r .
')"

assert_reject "cargo job without the environment preflight" \
    "$(workflow no-preflight "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - name: cargo build
        run: cargo build
')" \
    'job build runs cargo with no preceding step: run: bash scripts/assert-aws-lc-env.sh'

assert_reject "second cargo job without the preflight beside a compliant one" \
    "$(workflow second-job "$HEADER$PINNED_ENV$JOBS"'  audit:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo install cargo-audit --locked
          cargo audit
')" \
    'job audit runs cargo with no preceding step'

assert_reject "environment preflight placed after the first cargo step" \
    "$(workflow late-preflight "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: cargo fmt --all -- --check
      - run: bash scripts/assert-aws-lc-env.sh
      - run: cargo build
')" \
    'job build runs cargo with no preceding step'

assert_reject "Rust toolchain setup ahead of the environment preflight" \
    "$(workflow late-after-toolchain "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@0000000000000000000000000000000000000000 # v1
      - run: bash scripts/assert-aws-lc-env.sh
      - run: cargo build
')" \
    'job build runs cargo with no preceding step'

assert_reject "workflow file missing" \
    "$tmp/absent.yml" \
    'workflow file not found'

if [[ "$fails" -ne 0 ]]; then
    echo "check-aws-lc-pin.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-aws-lc-pin.test.sh: all assertions passed"
