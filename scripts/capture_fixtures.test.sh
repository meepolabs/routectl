#!/usr/bin/env bash
# Self-test for capture_fixtures.sh's meta.json emission. Exits 0 when
# all assertions pass, non-zero on the first failure.
#
# The rig derives its ROOT from its own location and confines --out to
# `<ROOT>/crates/routectl-cli/tests/fixtures/captured`, so every case
# runs the REAL script from inside a throwaway repo whose captured tree
# is that path. The input is a synthetic trace log written per case:
# that pins the emitted schema (lane normalization, the absent
# file-presence flags, the schema major) against the actual awk/sed
# extraction rather than against a re-implementation of it.
#
# It also pins COMPLETENESS: every file a fixture must carry is asserted
# present, because the rig writes each body / header file only when its
# trace line was found and a silently-dropped required file has no other
# detector -- the corpus is gitignored, so CI never loads a real fixture.
#
# Requires python3 (JSON parsing: a malformed meta.json must fail as
# itself, not as an empty string a value assertion might accept).
#
# Run it from anywhere:
#   bash scripts/capture_fixtures.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RIG="$HERE/capture_fixtures.sh"

fails=0

# One synthetic trace covering a NON-STREAM request: ingress body,
# outgoing body (carrying provider_kind + the model/alias spans),
# upstream success body (the completion marker), egress body, and all
# four header lines. `$1` is the request id, `$2` the traced
# provider_kind, `$3` the traced ingress token (default `anthropic`).
#
# The ingress token is a parameter so a case can drive a value OTHER than
# the default: with every trace saying `anthropic`, a rig that hardcoded
# the ingress kind would be indistinguishable from one that extracts it.
trace_non_stream() {
    local id="$1" kind="$2" ingress="${3:-anthropic}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="$ingress"}: $target ingress request body ingress="$ingress" body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:complete{provider=$kind:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="$kind" provider=p body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.200000Z TRACE $span: $target upstream success body provider_kind="$kind" provider=p body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-25T10:00:00.300000Z TRACE $span: $target egress response body ingress="$ingress" body={"id":"msg_1"} redact_prompts_enabled=false
2026-08-25T10:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"],["content-type","application/json"]]
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" headers=[["content-type","application/json"]]
2026-08-25T10:00:00.310000Z TRACE $span: $target egress response headers direction="egress" headers=[["content-type","application/json"]]
TRACE
}

# A STREAM request: the completion marker is a `stream summary` line and
# there is NO `upstream success body`, so the fixture lands with upstream
# response HEADERS and no upstream response BODY -- the combination the
# deleted `has_*` flags rejected.
trace_stream() {
    local id="$1" kind="$2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    cat <<TRACE
2026-08-25T11:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5","stream":true} redact_prompts_enabled=false
2026-08-25T11:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:stream{provider=$kind:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="$kind" provider=p body={"model":"claude-sonnet-4-5","stream":true} redact_prompts_enabled=false
2026-08-25T11:00:00.200000Z TRACE $span: $target stream summary direction="upstream" finish_reason="end_turn" prompt_tokens=11 completion_tokens=22 total_tokens=33
2026-08-25T11:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"]]
2026-08-25T11:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
2026-08-25T11:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" headers=[["content-type","text/event-stream"]]
TRACE
}

# Build a throwaway repo, run the real rig in it against the given trace
# text, and print the absolute path of the resulting captured tree. The
# caller inspects it and removes it. Rig stderr/stdout land in
# `<tree>/../rig.log` so a case can assert on a warning.
run_rig() {
    local trace_text="$1"
    local tmp
    tmp="$(mktemp -d)"
    mkdir -p "$tmp/repo/scripts" "$tmp/repo/crates/routectl-cli/tests/fixtures"
    cp "$RIG" "$tmp/repo/scripts/capture_fixtures.sh"
    # The rig reads the workspace version from the repo-root Cargo.toml.
    printf '[workspace.package]\nversion = "9.9.9"\n' >"$tmp/repo/Cargo.toml"
    printf '%s\n' "$trace_text" >"$tmp/trace.log"
    (
        cd "$tmp/repo" || exit 2
        bash scripts/capture_fixtures.sh --log "$tmp/trace.log"
    ) >"$tmp/rig.log" 2>&1
    printf '%s\n' "$tmp"
}

# Read a field out of a meta.json by JSON PATH, so a nested key is
# addressed as the nesting says (`client.name`) and a future top-level key
# of the same name cannot silently redirect the assertion. Parsing with a
# real JSON parser also means a malformed meta.json fails LOUDLY here
# instead of yielding an empty string that a value assertion might
# coincidentally accept.
#
# python3 is a hard dependency of this self-test (present on the CI runner
# image and on any dev box that can run the repo's tooling); an absent
# interpreter is reported rather than silently skipping assertions.
meta_get() {
    local file="$1" path="$2"
    python3 - "$file" "$path" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    node = json.load(fh)
for part in sys.argv[2].split("."):
    node = node[part]
print(node)
PY
}

# True when the file is valid JSON. Separate from meta_get so a case can
# assert parseability as its own named result.
is_valid_json() {
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$1" 2>/dev/null
}

# Assert every file a fixture directory MUST carry is present. The rig
# writes each body / header file only when its trace line was found, so a
# silently-dropped required file is the failure mode with no other
# detector: the corpus is gitignored, so CI never loads a real fixture
# and a rig regression would otherwise surface only on a contributor's
# next capture -- against evidence that cannot be recaptured.
assert_files_present() {
    local dir="$1" label="$2"
    shift 2
    local f
    for f in "$@"; do
        if [ -f "$dir/$f" ]; then
            echo "PASS: $label writes $f"
        else
            echo "FAIL: $label is missing $f"
            fails=$((fails + 1))
        fi
    done
}

# The four files every fixture must carry regardless of direction.
REQUIRED_FILES=(
    meta.json
    ingress_request.json
    ingress_request.headers.json
    outgoing_request.json
    outgoing_request.headers.json
)

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- expected '$expected', got '$actual'"
        fails=$((fails + 1))
    fi
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; this self-test cannot verify JSON output"
    exit 1
fi

# --- Case 1: a complete non-stream capture ---------------------------
# The traced provider_kind is `anthropic` (the routectl-providers
# PROVIDER_KIND const); meta.lane must carry the kind_str() spelling.
# This case also owns the completeness assertions: every required file
# plus, for this fully-traced request, both response slots.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000001 anthropic)")"
captured="$work/repo/crates/routectl-cli/tests/fixtures/captured"
dir="$captured/019eab77-0000-4000-8000-000000000001"
meta="$dir/meta.json"
if [ -d "$dir" ]; then
    assert_files_present "$dir" "a complete non-stream capture" \
        "${REQUIRED_FILES[@]}" \
        upstream_response.json upstream_response.headers.json \
        egress_response.json egress_response.headers.json
fi
if [ -f "$meta" ] && is_valid_json "$meta"; then
    echo "PASS: meta.json parses as JSON"
    check "lane normalizes anthropic to anthropic-api" \
        "anthropic-api" "$(meta_get "$meta" lane)"
    check "provider_kind stays in the providers-crate vocabulary" \
        "anthropic" "$(meta_get "$meta" provider_kind)"
    check "ingress_kind is written from the trace" \
        "anthropic" "$(meta_get "$meta" ingress_kind)"
    check "schema_version is written" "1" "$(meta_get "$meta" schema_version)"
    check "client name comes from the ingress user-agent" \
        "claude-cli" "$(meta_get "$meta" client.name)"
    check "client version comes from the ingress user-agent" \
        "2.1.167" "$(meta_get "$meta" client.version)"
    if grep -q '"has_' "$meta"; then
        echo "FAIL: meta.json still carries a has_* presence flag"
        fails=$((fails + 1))
    else
        echo "PASS: meta.json carries no has_* presence flag"
    fi
else
    echo "FAIL: non-stream capture produced no parseable meta.json (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 2: an unmapped provider_kind leaves lane empty and warns ----
# Negative control for case 1: proves the mapping is a real lookup, not a
# constant, and that an unknown spelling is not passed through as if it
# were a lane token.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000002 wat-provider)")"
captured="$work/repo/crates/routectl-cli/tests/fixtures/captured"
meta="$captured/019eab77-0000-4000-8000-000000000002/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "an unmapped provider_kind leaves lane empty" "" "$(meta_get "$meta" lane)"
    if grep -q "unmapped provider_kind" "$work/rig.log"; then
        echo "PASS: an unmapped provider_kind is reported"
    else
        echo "FAIL: an unmapped provider_kind was not reported"
        fails=$((fails + 1))
    fi
else
    echo "FAIL: unmapped-kind capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 3: a stream capture lands headers without a body -----------
# The 96%-of-corpus shape. The rig must write the upstream response
# HEADERS file, must NOT write the upstream response BODY file, and must
# say nothing about either in meta.json. Case 1's paired positive control
# (a non-stream trace DOES produce upstream_response.json) is what stops
# the absence assertion below from passing vacuously against a rig that
# writes no body files at all.
work="$(run_rig "$(trace_stream 019eab77-0000-4000-8000-000000000003 anthropic)")"
dir="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000003"
if [ -d "$dir" ]; then
    assert_files_present "$dir" "a stream capture" \
        "${REQUIRED_FILES[@]}" upstream_response.headers.json
    if [ -f "$dir/upstream_response.json" ]; then
        echo "FAIL: stream capture wrote an upstream response body it never saw"
        fails=$((fails + 1))
    else
        echo "PASS: stream capture writes no upstream response body"
    fi
    if grep -q '"has_' "$dir/meta.json"; then
        echo "FAIL: stream meta.json still carries a has_* presence flag"
        fails=$((fails + 1))
    else
        echo "PASS: stream meta.json carries no has_* presence flag"
    fi
else
    echo "FAIL: stream capture produced no fixture directory (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 4: the environment pins land verbatim ----------------------
export ROUTECTL_FIXTURE_CASE_ID="smoke-01"
export ROUTECTL_FIXTURE_CONFIG_SHA="deadbeef"
export ROUTECTL_FIXTURE_CONNECTION_MODE="base-url"
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000004 anthropic)")"
meta="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000004/meta.json"
unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA ROUTECTL_FIXTURE_CONNECTION_MODE
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "case_id comes from the environment" "smoke-01" "$(meta_get "$meta" case_id)"
    check "config_sha comes from the environment" "deadbeef" "$(meta_get "$meta" config_sha)"
    check "connection_mode comes from the environment" \
        "base-url" "$(meta_get "$meta" client.connection_mode)"
else
    echo "FAIL: pinned capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 5: a quote in a pin or in the user-agent stays valid JSON ---
# meta.json is emitted by hand, so every interpolated string must be
# escaped. Both sources here are reachable: a driver sets the pins
# programmatically, and the user-agent is client-controlled. Unescaped,
# either yields invalid JSON that the rig promotes at exit 0 and the
# loader then skips forever.
export ROUTECTL_FIXTURE_CASE_ID='bad"case'
export ROUTECTL_FIXTURE_CONFIG_SHA='back\slash'
trace_quoted_ua() {
    local out
    out="$(trace_non_stream 019eab77-0000-4000-8000-000000000005 anthropic)"
    printf '%s\n' "${out//claude-cli\/2.1.167 (external, cli)/evil\\\"cli/9.9.9}"
}
work="$(run_rig "$(trace_quoted_ua)")"
meta="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000005/meta.json"
manifest="$work/repo/crates/routectl-cli/tests/fixtures/captured/manifest.jsonl"
unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA
if [ -f "$meta" ]; then
    if is_valid_json "$meta"; then
        echo "PASS: meta.json parses with a quote in a pin and in the user-agent"
        check "an embedded quote round-trips through case_id" \
            'bad"case' "$(meta_get "$meta" case_id)"
        check "an embedded backslash round-trips through config_sha" \
            'back\slash' "$(meta_get "$meta" config_sha)"
    else
        echo "FAIL: meta.json is invalid JSON when a value carries a quote"
        cat "$meta"
        fails=$((fails + 1))
    fi
    if [ -f "$manifest" ] && python3 -c 'import json,sys
for line in open(sys.argv[1]):
    line = line.strip()
    if line:
        json.loads(line)' "$manifest" 2>/dev/null; then
        echo "PASS: manifest.jsonl parses with a quote in a pin"
    else
        echo "FAIL: manifest.jsonl is invalid JSON when a value carries a quote"
        fails=$((fails + 1))
    fi
else
    echo "FAIL: quoted-value capture produced no meta.json (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 6: ingress_kind is EXTRACTED, not assumed -------------------
# Paired with case 1's `anthropic` assertion: two different traced tokens
# must produce two different meta values, which is what distinguishes an
# extraction from a hardcoded default. The value is also the vocabulary a
# consumer dispatches on, so a wrong one silently routes a fixture to the
# wrong adapter.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000006 anthropic openai-responses)")"
meta="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000006/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "ingress_kind reflects a non-default traced ingress token" \
        "openai-responses" "$(meta_get "$meta" ingress_kind)"
else
    echo "FAIL: non-default-ingress capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

if [ "$fails" -gt 0 ]; then
    echo "capture_fixtures self-test: $fails failure(s)"
    exit 1
fi
echo "capture_fixtures self-test: all assertions passed"
