#!/usr/bin/env bash
# Self-test for check-log-display.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# The scanner reads the real tree, so these assertions drive it through a
# throwaway git repo built per case: that pins both directions (a raw `%`
# field is caught, a sanitized or allowlisted one is not) without editing
# any real source file.
#
# Run it from anywhere:
#   bash scripts/check-log-display.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCANNER="$HERE/check-log-display.sh"

fails=0

# Build a throwaway repo whose scanned path holds ONE tracing call with
# `$field = %$expr`, plus the given allowlist body, then run the scanner
# in it. Echoes the scanner's exit code.
run_case() {
    local field="$1" expr="$2" allowlist_body="$3"
    local tmp
    tmp="$(mktemp -d)"
    (
        cd "$tmp" || exit 2
        git init -q .
        mkdir -p crates/routectl-providers/src tools scripts
        cp "$SCANNER" scripts/check-log-display.sh
        printf '%s\n' "$allowlist_body" >tools/log-display-allowlist.txt
        cat >crates/routectl-providers/src/probe.rs <<RS
fn probe(v: &str) {
    tracing::warn!(
        $field = %$expr,
        "probe"
    );
}
RS
        # The scanner requires the ingress path to exist as well.
        mkdir -p crates/routectl-cli/src/ingress
        : >crates/routectl-cli/src/ingress/.keep
        bash scripts/check-log-display.sh >/dev/null 2>&1
    )
    local rc=$?
    rm -rf "$tmp"
    return $rc
}

assert_caught() {
    local desc="$1" field="$2" expr="$3" allowlist="${4:-}"
    if run_case "$field" "$expr" "$allowlist"; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
}

assert_clean() {
    local desc="$1" field="$2" expr="$3" allowlist="${4:-}"
    if run_case "$field" "$expr" "$allowlist"; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but caught -- $desc"
        fails=$((fails + 1))
    fi
}

assert_caught "raw % on a wire field" "type_tag" "v"
assert_caught "raw % on a second wire field name" "block_type" "v"
assert_clean "sanitized % on a wire field" "type_tag" "sanitize_for_log(v)"
assert_clean "non-wire field name is out of scope" "provider" "v"
assert_clean "allowlisted path+field" "type_tag" "v" \
    "crates/routectl-providers/src/probe.rs:type_tag  # test fixture"
assert_clean "allowlist comments and blank lines are ignored" "type_tag" "v" \
    "# a comment

crates/routectl-providers/src/probe.rs:type_tag  # test fixture"
assert_caught "an allowlist entry for a DIFFERENT field does not cover this one" \
    "type_tag" "v" \
    "crates/routectl-providers/src/probe.rs:block_type  # test fixture"

if [[ "$fails" -ne 0 ]]; then
    echo "check-log-display.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-log-display.test.sh: all assertions passed"
