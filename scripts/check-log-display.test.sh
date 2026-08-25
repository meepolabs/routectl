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
        # The scanner requires every configured search path to exist.
        mkdir -p crates/routectl-cli/src/ingress crates/routectl-cli/src/server \
            crates/routectl-cli/src/handlers crates/routectl-cli/src/commands \
            crates/routectl-cli/src/proxy crates/routectl-router/src
        : >crates/routectl-cli/src/ingress/.keep
        : >crates/routectl-cli/src/server/.keep
        : >crates/routectl-cli/src/handlers/.keep
        : >crates/routectl-cli/src/commands/.keep
        : >crates/routectl-cli/src/proxy/.keep
        : >crates/routectl-router/src/.keep
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

# Same as `run_source`, but the source lands on a CONFIG_KEY_PATHS member
# instead of the wire-only providers path.
run_config_source() {
    local source="$1"
    run_source "fn unrelated() {}" "" \
        "printf '%s\\n' ${source@Q} >crates/routectl-router/src/probe.rs"
}

assert_config_caught() {
    local desc="$1" source="$2"
    if run_config_source "$source"; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
}

assert_config_clean() {
    local desc="$1" source="$2"
    if run_config_source "$source"; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but caught -- $desc"
        fails=$((fails + 1))
    fi
}

# Same again, but landing on `commands/`. That directory is in BOTH tiers,
# so it needs its own assertions: the config-key cases above run on
# `routectl-router/src`, which is also in both, and would keep passing if
# `commands/` were dropped from either path set.
run_commands_source() {
    local source="$1"
    run_source "fn unrelated() {}" "" \
        "printf '%s\\n' ${source@Q} >crates/routectl-cli/src/commands/probe.rs"
}

assert_commands_caught() {
    local desc="$1" source="$2"
    if run_commands_source "$source"; then
        echo "FAIL: expected CAUGHT but passed -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
}

assert_caught "raw % on a wire field" "type_tag" "v"
assert_caught "raw % on a second wire field name" "block_type" "v"
assert_clean "sanitized % on a wire field" "type_tag" "sanitize_for_log(v)"
assert_clean "path-qualified sanitizer on a wire field" \
    "type_tag" "routectl_core::sanitize_for_log(v)"
assert_clean "sanitize_detail_for_log counts as sanitized" \
    "type_tag" "sanitize_detail_for_log(v)"
assert_caught "raw % on the inbound thinking.display field" "thinking_display" "v"
assert_clean "sanitized % on the inbound thinking.display field" \
    "thinking_display" "sanitize_detail_for_log(v)"
assert_caught "raw % on the inbound thinking.type field" "thinking_type" "v"
assert_clean "sanitized % on the inbound thinking.type field" \
    "thinking_type" "sanitize_detail_for_log(v)"
assert_caught "raw % on the stripped thinking.display value" "stripped_display" "v"
assert_clean "sanitized % on the stripped thinking.display value" \
    "stripped_display" "sanitize_detail_for_log(v)"
assert_caught "raw % on the cache_control ttl field" "ttl" "v"
assert_clean "sanitized % on the cache_control ttl field" \
    "ttl" "sanitize_for_log(v)"
assert_clean "non-wire field name is out of scope" "status" "v"
assert_clean "a config-key field is out of scope on a wire-only path" "provider" "v"
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

# The config-key tier: `provider` / `model` / `warning` on a startup or
# routing path, where the value is an operator-written config table key.
assert_config_caught "raw % on a config-key field, on a config-key path" \
    'fn probe(v: &str) {
    tracing::warn!(provider = %v, "probe");
}'

assert_config_caught "raw % on a validator-message field" \
    'fn probe(warning: &str) {
    tracing::warn!(warning = %warning, "probe");
}'

assert_config_clean "sanitized % on a config-key field" \
    'fn probe(v: &str) {
    tracing::warn!(model = %routectl_core::sanitize_for_log(v), "probe");
}'

assert_config_clean "a surface-named sanitizer in the family counts as sanitized" \
    'fn probe(v: &str) {
    tracing::warn!(warning = %sanitize_warning_for_log(v), "probe");
}'

assert_config_clean "sanitize_for_log_with_cap counts as sanitized" \
    'fn probe(v: &str) {
    tracing::warn!(warning = %sanitize_for_log_with_cap(v, 512), "probe");
}'

assert_config_caught "positional shorthand on a config-key field" \
    'fn probe(provider: &str) {
    tracing::warn!(%provider, "probe");
}'

# Shape 4: a `_safe` local, accepted only when its `let` really sanitizes.
assert_config_clean "a _safe local backed by a sanitizing let is accepted" \
    'fn probe(v: &str) {
    let provider_safe = routectl_core::sanitize_for_log(v);
    tracing::warn!(provider = %provider_safe, "probe {provider_safe}");
    tracing::warn!(provider = %provider_safe, "second arm");
}'

assert_config_caught "a _safe local with NO sanitizing let is not accepted" \
    'fn probe(v: &str) {
    let provider_safe = v.to_string();
    tracing::warn!(provider = %provider_safe, "probe");
}'

# The `_safe` pairing is file-scoped, not scope-aware: a single sanitized
# `let` must not vouch for a second, unsanitized origin of the same name
# anywhere in the file. Four ways to introduce one, all findings.
assert_config_caught "an inner shadow of a _safe local is not laundered by the outer sanitized let" \
    'fn probe(v: &str) {
    let provider_safe = routectl_core::sanitize_for_log(v);
    tracing::warn!(provider = %provider_safe, "outer");
    if v.is_empty() {
        let provider_safe = v.to_string();
        tracing::warn!(provider = %provider_safe, "inner shadow");
    }
}'

assert_config_caught "a raw _safe let in a SECOND function is not laundered by the first" \
    'fn sanitizing(v: &str) {
    let provider_safe = routectl_core::sanitize_for_log(v);
    tracing::warn!(provider = %provider_safe, "clean arm");
}
fn wrong_function(v: &str) {
    let provider_safe = v.to_string();
    tracing::warn!(provider = %provider_safe, "raw arm");
}'

assert_config_caught "a _safe PARAMETER is not laundered by a sanitized let elsewhere in the file" \
    'fn sanitizing(v: &str) {
    let provider_safe = routectl_core::sanitize_for_log(v);
    tracing::warn!(provider = %provider_safe, "clean arm");
}
fn taking_param(provider_safe: &str) {
    tracing::warn!(provider = %provider_safe, "raw arm");
}'

assert_config_caught "a mut re-assignment after the sanitized let is not laundered" \
    'fn probe(v: &str) {
    let mut provider_safe = routectl_core::sanitize_for_log(v);
    provider_safe = v.to_string();
    tracing::warn!(provider = %provider_safe, "probe");
}'

# SANITIZERS is a closed list of four names, not an open family shape: a
# helper merely NAMED like a sanitizer proves nothing.
assert_config_caught "an invented sanitize_*_for_log name is not accepted" \
    'fn probe(v: &str) {
    tracing::warn!(provider = %sanitize_nothing_for_log(v), "probe");
}'

# Shape 5: `{field}` interpolated into the message body renders through
# Display with no field ever present.
assert_config_caught "a config-key field interpolated into the message body" \
    'fn probe(provider: &str) {
    tracing::warn!(count = 1, "no route for [providers.{provider}]");
}'

assert_source_caught "a wire field interpolated into a multiline message body" \
    'fn probe(finish_reason: &str) {
    tracing::warn!(
        chunks = 2,
        "second finish_reason (new={finish_reason}) on one stream"
    );
}'

assert_source_clean "a _safe capture in the message body is out of scope for shape 5" \
    'fn probe(v: &str) {
    let finish_reason_safe = routectl_core::sanitize_for_log(v);
    tracing::warn!("second finish_reason (new={finish_reason_safe})");
}'

assert_source_clean "a non-field capture in the message body is out of scope" \
    'fn probe(host: &str) {
    tracing::warn!("routectl bound to {host}");
}'

assert_source_clean "a commented-out call is prose, not a call site" \
    'fn probe(_v: &str) {
    // historical shape: tracing::warn!(type_tag = %v, "probe");
}'

# `commands/` sits in both tiers: it renders operator config keys AND it is
# an upstream-bytes boundary (`probe` classifies a response body, `catalog
# import` fetches vendor JSON).
assert_commands_caught "commands/ is in the config-key tier" \
    'fn probe(v: &str) {
    tracing::warn!(warning = %v, "config check");
}'

assert_commands_caught "commands/ is in the wire tier too" \
    'fn probe(v: &str) {
    tracing::warn!(type_tag = %v, "probe capture");
}'

assert_fail_closed "a missing search path is a gate failure, not a vacuous PASS" \
    'rm -rf crates/routectl-cli/src/ingress'
assert_fail_closed "a missing config-key search path is a gate failure too" \
    'rm -rf crates/routectl-router/src'
assert_fail_closed "a .rs outside every tier and undeclared is a gate failure" \
    'printf "fn orphan() {}\n" >crates/routectl-cli/src/orphan.rs'
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
