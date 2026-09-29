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

# Write $3 to a workflow file named $2 in a fresh directory for case $1, so the
# checker applies the exemptions listed for that file name; print its path.
named_workflow() {
    mkdir -p "$tmp/$1"
    printf '%s' "$3" >"$tmp/$1/$2"
    printf '%s\n' "$tmp/$1/$2"
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

assert_reject "cargo job without the environment preflight" \
    "$(workflow no-preflight "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - name: cargo build
        run: cargo build
')" \
    'job build does not run the preflight as the step right after checkout: bash scripts/assert-aws-lc-env.sh'

assert_reject "second cargo job without the preflight beside a compliant one" \
    "$(workflow second-job "$HEADER$PINNED_ENV$JOBS"'  audit:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo install cargo-audit --locked
          cargo audit
')" \
    'job audit does not run the preflight as the step right after checkout'

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
    'job build does not run the preflight as the step right after checkout'

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
    'job build does not run the preflight as the step right after checkout'

assert_reject "checkout job without the preflight that never runs cargo" \
    "$(workflow checkout-no-cargo "$HEADER$PINNED_ENV$JOBS"'  scan:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: osv-scanner scan source -r .
')" \
    'job scan does not run the preflight'

assert_pass "job without a checkout that never runs cargo" \
    "$(workflow no-checkout "$HEADER$PINNED_ENV$JOBS"'  notify:
    runs-on: ubuntu-22.04
    steps:
      - run: echo done
')"

assert_reject "job without a checkout that runs cargo" \
    "$(workflow no-checkout-cargo "$HEADER$PINNED_ENV$JOBS"'  audit:
    runs-on: ubuntu-22.04
    steps:
      - run: cargo install cargo-audit --locked
')" \
    'job audit runs cargo without a checkout'

assert_pass "preflight as the first line of a run block" \
    "$(workflow run-block "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: AWS-LC build environment preflight
        run: |
          bash scripts/assert-aws-lc-env.sh
      - run: cargo build
')"

assert_reject "run block whose first line is not the preflight" \
    "$(workflow run-block-late "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo fetch
          bash scripts/assert-aws-lc-env.sh
      - run: cargo build
')" \
    'job build does not run the preflight'

EXEMPT_JOB='  osv-scan:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: osv-scanner scan source -r .
'

assert_pass "listed exempt job without the preflight" \
    "$(named_workflow exempt ci.yml "$HEADER$PINNED_ENV$JOBS$EXEMPT_JOB")"

assert_reject "listed exempt job that runs cargo" \
    "$(named_workflow exempt-cargo ci.yml "$HEADER$PINNED_ENV$JOBS"'  osv-scan:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: cargo audit
')" \
    'exempt job osv-scan runs cargo'

assert_reject "exemption naming a job the workflow no longer defines" \
    "$(named_workflow stale-exemption ci.yml "$HEADER$PINNED_ENV$JOBS")" \
    'exemption names job osv-scan, which this workflow does not define'

# The literal $GITHUB_ENV is the workflow text under test, not a shell expansion.
# shellcheck disable=SC2016
for var in aws_lc_sys_use_system_x86_64_unknown_linux_gnu AWS_LC_SYS_SYSTEM_DIR \
    AWS_LC_SYS_SYSTEM_BINDINGS AWS_LC_SYS_NO_PREFIX; do
    assert_reject "$var written through GITHUB_ENV" \
        "$(workflow "github-env-$var" "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: bash scripts/assert-aws-lc-env.sh
      - run: echo "'"$var"'=1" >> "$GITHUB_ENV"
')" \
        "set outside the top-level pin"
done

PREFLIGHT_JOB_HEAD='jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: bash scripts/assert-aws-lc-env.sh
'

assert_reject "checkout step written as an alias" \
    "$(workflow alias-checkout "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: *checkout
      - run: cargo build
')" \
    'YAML anchor or alias is not supported here:       - uses: *checkout'

assert_reject "run command written as an alias" \
    "$(workflow alias-run "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - run: *build
')" \
    'YAML anchor or alias is not supported here:       - run: *build'

assert_reject "whole step written as an alias" \
    "$(workflow alias-step "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - *preflight
')" \
    'YAML anchor or alias is not supported here:       - *preflight'

assert_reject "anchor on a step value" \
    "$(workflow anchor "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - run: &build cargo build
')" \
    'YAML anchor or alias is not supported here:       - run: &build cargo build'

assert_reject "merge key in a step" \
    "$(workflow merge-key "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - <<: *base
        name: build
')" \
    'YAML merge key is not supported here'

assert_reject "bare dash sequence item for a step" \
    "$(workflow bare-dash "$HEADER$PINNED_ENV"'jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      -
        uses: actions/checkout@v4
      - run: bash scripts/assert-aws-lc-env.sh
      - run: cargo build
')" \
    'bare - sequence item is not supported under jobs:'

assert_reject "flow-style step" \
    "$(workflow flow-step "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - { run: cargo build }
')" \
    'flow-style sequence item is not supported under jobs:'

# The literal "$f" is workflow text under test, not a shell expansion.
# shellcheck disable=SC2016
assert_pass "globs, redirects and quoted stars in run bodies and values" \
    "$(workflow globs "$HEADER$PINNED_ENV$PREFLIGHT_JOB_HEAD"'      - run: cargo build
      - name: aggregate *.sha256 sidecars
        run: |
          for f in *.sha256; do cat "$f" >> SHA256SUMS; done
          rm -f ./*.sha256
          echo "done" >&2
          *) echo glob-case ;;
      - run: rm -f ./*.sha256 && echo "*alias" >&2
      - uses: some/action@v1
        with:
          files: dist/*
          pattern: "*.tar.gz"
          other: '"'"'&not-an-anchor'"'"'
')"

assert_reject "workflow file missing" \
    "$tmp/absent.yml" \
    'workflow file not found'

if [[ "$fails" -ne 0 ]]; then
    echo "check-aws-lc-pin.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-aws-lc-pin.test.sh: all assertions passed"
