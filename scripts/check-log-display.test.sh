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

# Build a throwaway repo whose scanned path holds the given Rust source,
# plus the given allowlist body, then run the scanner in it. Returns the
# scanner's exit code. `extra_setup` runs inside the repo before the scan
# (used by the fail-closed cases) and `path_prefix` overrides the PATH the
# scanner sees.
run_source() {
    local source="$1" allowlist_body="$2" extra_setup="${3:-}" scanner_path="${4:-\$PATH}"
    local tmp
    tmp="$(mktemp -d)"
    (
        cd "$tmp" || exit 2
        git init -q .
        mkdir -p crates/routectl-providers/src tools scripts
        cp "$SCANNER" scripts/check-log-display.sh
        printf '%s\n' "$allowlist_body" >tools/log-display-allowlist.txt
        printf '%s\n' "$source" >crates/routectl-providers/src/probe.rs
        # The scanner requires the ingress path to exist as well.
        mkdir -p crates/routectl-cli/src/ingress
        : >crates/routectl-cli/src/ingress/.keep
        if [[ -n "$extra_setup" ]]; then
            eval "$extra_setup" || exit 2
        fi
        # Expanded HERE, not at the call site: a PATH override naming the
        # throwaway repo (e.g. "$PWD/stubbin") must resolve inside it.
        local resolved_path
        resolved_path="$(eval printf '%s' "\"$scanner_path\"")"
        PATH="$resolved_path" bash scripts/check-log-display.sh >/dev/null 2>&1
    )
    local rc=$?
    rm -rf "$tmp"
    return $rc
}

# Shorthand for the common single-field shape `$field = %$expr`.
run_case() {
    local field="$1" expr="$2" allowlist_body="$3"
    run_source "fn probe(v: &str) {
    tracing::warn!(
        $field = %$expr,
        \"probe\"
    );
}" "$allowlist_body"
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

assert_source_caught() {
    local desc="$1" source="$2"
    if run_source "$source" ""; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
}

assert_source_clean() {
    local desc="$1" source="$2"
    if run_source "$source" ""; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but caught -- $desc"
        fails=$((fails + 1))
    fi
}

# Fail-closed assertions: the scanner must exit non-zero rather than print
# PASS after scanning nothing.
assert_fail_closed() {
    local desc="$1" extra_setup="$2" scanner_path="${3:-\$PATH}"
    if run_source "fn probe() {}" "" "$extra_setup" "$scanner_path"; then
        echo "FAIL: expected FAIL-CLOSED but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: fail-closed -- $desc"
    fi
}

assert_caught "raw % on a wire field" "type_tag" "v"
assert_caught "raw % on a second wire field name" "block_type" "v"
assert_clean "sanitized % on a wire field" "type_tag" "sanitize_for_log(v)"
assert_clean "path-qualified sanitizer on a wire field" \
    "type_tag" "routectl_core::sanitize_for_log(v)"
assert_clean "sanitize_detail_for_log counts as sanitized" \
    "type_tag" "sanitize_detail_for_log(v)"
assert_clean "non-wire field name is out of scope" "provider" "v"
assert_clean "allowlisted path+field" "type_tag" "v" \
    "crates/routectl-providers/src/probe.rs:type_tag  # test fixture"
assert_clean "allowlist comments and blank lines are ignored" "type_tag" "v" \
    "# a comment

crates/routectl-providers/src/probe.rs:type_tag  # test fixture"
assert_caught "an allowlist entry for a DIFFERENT field does not cover this one" \
    "type_tag" "v" \
    "crates/routectl-providers/src/probe.rs:block_type  # test fixture"

# Bypass shape 1: a trailing comment that merely MENTIONS the sanitizer.
assert_source_caught "trailing comment naming the sanitizer does not launder a raw field" \
    'fn probe(v: &str) {
    tracing::warn!(
        type_tag = %v, // value comes pre-cleaned, cf. sanitize_for_log
        "probe"
    );
}'

# Bypass shape 2: a sanitized sibling field sharing the line with a raw one.
assert_source_caught "a sanitized sibling on the same line does not launder a raw field" \
    'fn probe(v: &str) {
    tracing::warn!(part_type = %sanitize_for_log(v), block_type = %v, "probe");
}'

# Missed shape 3: field name and `= %` split across lines by rustfmt.
assert_source_caught "field name and = % split across lines" \
    'fn probe(v: &str) {
    tracing::warn!(
        event_type
            = %v,
        "probe"
    );
}'

# Missed shape 4: tracing positional shorthand (no sanitizer can wrap it).
assert_source_caught "positional shorthand %field on a wire field" \
    'fn probe(finish_reason: &str) {
    tracing::warn!(%finish_reason, "probe");
}'

assert_source_caught "positional shorthand after a preceding field" \
    'fn probe(tool_id: &str) {
    tracing::warn!(status = 200, %tool_id, "probe");
}'

assert_source_clean "positional shorthand on a non-wire field is out of scope" \
    'fn probe(method: &str) {
    tracing::warn!(%method, "probe");
}'

assert_source_clean "a commented-out call is prose, not a call site" \
    'fn probe(_v: &str) {
    // historical shape: tracing::warn!(type_tag = %v, "probe");
}'

assert_fail_closed "a missing search path is a gate failure, not a vacuous PASS" \
    'rm -rf crates/routectl-cli/src/ingress'
assert_fail_closed "an absent rg is a gate failure, not a vacuous PASS" \
    'mkdir -p stubbin
     for tool in bash git sed mktemp; do
         ln -sf "$(command -v "$tool")" "stubbin/$tool"
     done' \
    '$PWD/stubbin'

if [[ "$fails" -ne 0 ]]; then
    echo "check-log-display.test.sh: $fails assertion(s) failed" >&2
    exit 1
fi
echo "check-log-display.test.sh: all assertions passed"
