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
SCRUB="$HERE/scrub-fixture.sh"

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

# One REAL structural-summary line for one direction, in the layout
# log_safe.rs emits: the event message is `structural summary` right after
# the `routectl_core::log_safe: ` target, and `direction=` is a field of
# the same line. `$3` is the sub-second part of the timestamp so a caller
# can order the two directions.
structural_line() {
    local id="$1" direction="$2" frac="${3:-400000}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    printf '%s TRACE %s: routectl_core::log_safe: structural summary direction="%s" kind="anthropic" id=p model=claude-sonnet-4-5 max_tokens=64 thinking_shape="" output_config_effort="" tool_choice_shape="" cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta="" provider_extras_keys="" stream=false\n' \
        "2026-08-25T10:00:00.${frac}Z" "$span" "$direction"
}

# A non-stream trace carrying BOTH request-side structural summaries --
# the shape a driver capture must produce, since driver mode refuses a
# fixture with half its structural evidence.
trace_driver() {
    local id="$1" kind="${2:-anthropic}"
    trace_non_stream "$id" "$kind"
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# A stream trace carrying both structural summaries. Used as the RERUN of
# a case first captured non-stream: the response-slot file set differs
# between the two, which is what makes replace-vs-merge observable.
trace_driver_stream() {
    local id="$1" kind="${2:-anthropic}"
    trace_stream "$id" "$kind"
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# A driver trace whose ingress body carries a SYNTHETIC third-party home
# prefix. `scrub-fixture.sh --write` has no safe automatic rewrite for
# another account's home path, so the residue survives into `--check`,
# which is the split the driver-mode landing gate exists to catch. The
# path is invented for this test and matches nothing on any real machine.
trace_driver_dirty() {
    local id="$1"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"read /home/someoneelse/notes.txt"}]} redact_prompts_enabled=false
TRACE
    trace_non_stream "$id" anthropic | tail -n +2
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# The decoy: a driver trace whose ingress request BODY quotes a routectl
# log line, phrase included. This is routine traffic -- a coding session
# about routectl's own logging -- and it is what the unanchored
# `grep 'structural summary' | head -2` selected instead of the real
# outgoing summary, dropping the outgoing side entirely. The decoy line
# comes FIRST in the trace, exactly as an ingress body does.
trace_driver_decoy() {
    local id="$1"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local decoy='routectl_core::log_safe: structural summary direction=\"outgoing\" messages_len=99'
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"why does $decoy show up twice"}]} redact_prompts_enabled=false
TRACE
    trace_non_stream "$id" anthropic | tail -n +2
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# Build a throwaway repo and print its root. Split out of run_rig so a
# case can invoke the rig TWICE against the same captured tree, which is
# the only way to observe what a rerun does to an existing landing dir.
make_repo() {
    local tmp
    tmp="$(mktemp -d)"
    mkdir -p "$tmp/repo/scripts" "$tmp/repo/crates/routectl-cli/tests/fixtures"
    cp "$RIG" "$tmp/repo/scripts/capture_fixtures.sh"
    # The rig delegates scrubbing to scrub-fixture.sh and refuses to run
    # without it, so the throwaway repo carries the real script too.
    cp "$SCRUB" "$tmp/repo/scripts/scrub-fixture.sh"
    # The rig reads the workspace version from the repo-root Cargo.toml.
    printf '[workspace.package]\nversion = "9.9.9"\n' >"$tmp/repo/Cargo.toml"
    printf '%s\n' "$tmp"
}

# Run the real rig inside an existing throwaway repo against the given
# trace text, with any extra rig flags appended. Returns the rig's exit
# status; stdout+stderr land in `<tmp>/rig.log` (truncated per run) so a
# case can assert on a refusal message.
rig_run() {
    local tmp="$1" trace_text="$2"
    shift 2
    printf '%s\n' "$trace_text" >"$tmp/trace.log"
    local rc=0
    (
        cd "$tmp/repo" || exit 2
        bash scripts/capture_fixtures.sh --log "$tmp/trace.log" "$@"
    ) >"$tmp/rig.log" 2>&1 || rc=$?
    return "$rc"
}

# Path of the captured tree inside a throwaway repo.
captured_of() {
    printf '%s\n' "$1/repo/crates/routectl-cli/tests/fixtures/captured"
}

# Build a throwaway repo, run the real rig in it against the given trace
# text, and print the absolute path of the resulting captured tree. The
# caller inspects it and removes it. Rig stderr/stdout land in
# `<tree>/../rig.log` so a case can assert on a warning.
run_rig() {
    local tmp
    tmp="$(make_repo)"
    rig_run "$tmp" "$1" || true
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

# Assert a file contains a substring. Used on the rig log, where the
# MESSAGE is part of the contract: a driver-mode refusal that does not
# name the missing pin sends the runner hunting through the rig.
check_log() {
    local label="$1" needle="$2" file="$3"
    if grep -qF -- "$needle" "$file"; then
        echo "PASS: $label"
    else
        echo "FAIL: $label -- '$needle' absent from $file"
        sed -n '1,20p' "$file"
        fails=$((fails + 1))
    fi
}

# Set / clear the three driver pins in one call, so a case cannot leak a
# pin into the next one (driver mode is fail-closed on all three, so a
# leaked pin turns a refusal assertion into a silent pass).
set_pins() {
    export ROUTECTL_FIXTURE_CASE_ID="$1"
    export ROUTECTL_FIXTURE_CONFIG_SHA="$2"
    export ROUTECTL_FIXTURE_CONNECTION_MODE="$3"
}
clear_pins() {
    unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA \
        ROUTECTL_FIXTURE_CONNECTION_MODE
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

# --- Case 7: driver mode refuses on each unset pin, naming it ---------
# Three cases plus a paired control. Without the control a rig that
# refused unconditionally -- or one that refused for an unrelated reason --
# would pass all three refusal assertions.
for missing in CASE_ID CONFIG_SHA CONNECTION_MODE; do
    set_pins drift-01 abc123 base-url
    unset "ROUTECTL_FIXTURE_$missing"
    work="$(make_repo)"
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000007)" --driver-mode || rc=$?
    clear_pins
    if [ "$rc" -eq 0 ]; then
        echo "FAIL: driver mode ran with ROUTECTL_FIXTURE_$missing unset"
        fails=$((fails + 1))
    else
        echo "PASS: driver mode refuses with ROUTECTL_FIXTURE_$missing unset"
    fi
    check_log "the refusal names ROUTECTL_FIXTURE_$missing" \
        "ROUTECTL_FIXTURE_$missing" "$work/rig.log"
    if [ -d "$(captured_of "$work")/anthropic-api" ]; then
        echo "FAIL: driver mode landed a fixture with ROUTECTL_FIXTURE_$missing unset"
        fails=$((fails + 1))
    else
        echo "PASS: nothing lands when ROUTECTL_FIXTURE_$missing is unset"
    fi
    rm -rf "$work"
done

# Paired control: the SAME trace with all three pins unset captures fine
# on the unflagged live-box path, where an empty pin is honest.
clear_pins
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000007)" || rc=$?
meta="$(captured_of "$work")/019eab77-0000-4000-8000-000000000007/meta.json"
check "live-box mode tolerates all three pins unset" "0" "$rc"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "an unpinned live-box capture leaves case_id empty" \
        "" "$(meta_get "$meta" case_id)"
    check "an unpinned live-box capture leaves config_sha empty" \
        "" "$(meta_get "$meta" config_sha)"
    check "an unpinned live-box capture leaves connection_mode empty" \
        "" "$(meta_get "$meta" client.connection_mode)"
else
    echo "FAIL: unpinned live-box capture produced no parseable meta.json"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 8: driver landing keys on (lane, case_id) -------------------
# A UUID-keyed corpus grows a fresh sibling per rerun and has nothing to
# diff against. The lane directory comes from the NORMALIZED lane
# (`anthropic` -> `anthropic-api`), so this also pins that the landing
# path uses the kind_str() vocabulary rather than the traced token.
set_pins tools-multiturn-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000008)" --driver-mode || rc=$?
captured="$(captured_of "$work")"
clear_pins
check "a driver capture exits 0" "0" "$rc"
dir="$captured/anthropic-api/tools-multiturn-01"
if [ -d "$dir" ]; then
    echo "PASS: driver mode lands at <lane>/<case_id>"
    assert_files_present "$dir" "a driver capture" "${REQUIRED_FILES[@]}"
    check "request_id survives in meta.json for traceability" \
        "019eab77-0000-4000-8000-000000000008" "$(meta_get "$dir/meta.json" request_id)"
else
    echo "FAIL: driver mode did not land at <lane>/<case_id> (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
if [ -d "$captured/019eab77-0000-4000-8000-000000000008" ]; then
    echo "FAIL: driver mode also landed a request_id-keyed directory"
    fails=$((fails + 1))
else
    echo "PASS: driver mode lands no request_id-keyed directory"
fi
rm -rf "$work"

# --- Case 9: a driver rerun REPLACES the same case's directory --------
# Second run, different request_id, same case id, and a trace shape whose
# response-slot file set DIFFERS (stream: headers but no body). Replace
# means the stale non-stream `upstream_response.json` is gone; a merge
# would leave it behind and file presence IS the schema.
work="$(make_repo)"
set_pins tools-multiturn-01 abc123 base-url
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000009)" --driver-mode || true
captured="$(captured_of "$work")"
dir="$captured/anthropic-api/tools-multiturn-01"
first_req="$(meta_get "$dir/meta.json" request_id)"
first_has_body=absent
[ -f "$dir/upstream_response.json" ] && first_has_body=present
rc=0
rig_run "$work" "$(trace_driver_stream 019eab77-0000-4000-8000-00000000000a)" --driver-mode --force || rc=$?
clear_pins
check "a rerun of the same case exits 0" "0" "$rc"
check "the first run wrote an upstream response body" "present" "$first_has_body"
check "the rerun re-lands on the same directory" \
    "019eab77-0000-4000-8000-00000000000a" "$(meta_get "$dir/meta.json" request_id)"
check "the first run was a different request" \
    "019eab77-0000-4000-8000-000000000009" "$first_req"
if [ -f "$dir/upstream_response.json" ]; then
    echo "FAIL: the rerun merged into the previous fixture instead of replacing it"
    fails=$((fails + 1))
else
    echo "PASS: the rerun replaces the previous fixture wholesale"
fi
lane_dirs="$(find "$captured/anthropic-api" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')"
check "the rerun adds no sibling directory" "1" "$lane_dirs"
if find "$captured" -maxdepth 1 -name '.tmp.*' | grep -q .; then
    echo "FAIL: the rerun left a tmp directory behind"
    fails=$((fails + 1))
else
    echo "PASS: the rerun leaves no tmp directory behind"
fi
rm -rf "$work"

# --- Case 10: two different cases land side by side -------------------
# The negative control for case 9: the replace behavior must be keyed on
# the case id, not on the lane.
work="$(make_repo)"
set_pins case-alpha abc123 base-url
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000000b)" --driver-mode || true
set_pins case-beta abc123 base-url
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000000c)" --driver-mode --force || true
clear_pins
captured="$(captured_of "$work")"
for case_name in case-alpha case-beta; do
    if [ -f "$captured/anthropic-api/$case_name/meta.json" ]; then
        echo "PASS: $case_name lands in its own directory"
    else
        echo "FAIL: $case_name did not land (rig log: $work/rig.log)"
        cat "$work/rig.log"
        fails=$((fails + 1))
    fi
done
rm -rf "$work"

# --- Case 11: a body line quoting the phrase cannot displace the -------
# --- outgoing structural summary --------------------------------------
# The bug: `grep 'structural summary' | head -2` matched the ingress
# request BODY line whose JSON content carries the phrase, then kept it
# plus the real ingress summary and discarded the outgoing one. Measured
# on a real corpus at 15% of fixtures. Both directions are asserted, and
# case 12's clean pair is the paired positive control proving the
# selection is not simply dropping the phrase everywhere.
set_pins decoy-01 abc123 base-url
work="$(make_repo)"
rig_run "$work" "$(trace_driver_decoy 019eab77-0000-4000-8000-00000000000d)" --driver-mode || true
clear_pins
structural="$(captured_of "$work")/anthropic-api/decoy-01/structural.txt"
if [ -f "$structural" ]; then
    check "the decoy trace still lands both structural summaries" \
        "2" "$(wc -l <"$structural" | tr -d ' ')"
    if grep -q 'direction="outgoing"' "$structural"; then
        echo "PASS: the outgoing structural summary survives a decoy body line"
    else
        echo "FAIL: a decoy body line displaced the outgoing structural summary"
        cat "$structural"
        fails=$((fails + 1))
    fi
    if grep -q 'direction="ingress"' "$structural"; then
        echo "PASS: the ingress structural summary survives a decoy body line"
    else
        echo "FAIL: the ingress structural summary is missing"
        fails=$((fails + 1))
    fi
    if grep -q 'messages_len=99' "$structural"; then
        echo "FAIL: structural.txt selected the request-body line"
        fails=$((fails + 1))
    else
        echo "PASS: structural.txt selects no request-body line"
    fi
else
    echo "FAIL: decoy capture wrote no structural.txt (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 12: a clean pair lands both directions, in order ------------
set_pins clean-01 abc123 base-url
work="$(make_repo)"
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000000e)" --driver-mode || true
clear_pins
structural="$(captured_of "$work")/anthropic-api/clean-01/structural.txt"
if [ -f "$structural" ]; then
    check "a clean pair lands two structural lines" \
        "2" "$(wc -l <"$structural" | tr -d ' ')"
    check "the ingress summary is the first line" \
        "ingress" "$(sed -n '1s/.*direction="\([a-z]*\)".*/\1/p' "$structural")"
    check "the outgoing summary is the second line" \
        "outgoing" "$(sed -n '2s/.*direction="\([a-z]*\)".*/\1/p' "$structural")"
else
    echo "FAIL: clean-pair capture wrote no structural.txt (rig log: $work/rig.log)"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 13: an absent outgoing summary fails the fixture in driver --
# --- mode, and only warns on the live-box path ------------------------
# `trace_non_stream` carries NO structural summary at all, so it is the
# ingress-and-outgoing-absent case. Half the structural evidence is not a
# canonical fixture; a drained live-box log is whatever the daemon emitted.
set_pins halfway-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_non_stream 019eab77-0000-4000-8000-00000000000f anthropic)" --driver-mode || rc=$?
clear_pins
if [ "$rc" -eq 0 ]; then
    echo "FAIL: driver mode accepted a fixture with no structural summary"
    fails=$((fails + 1))
else
    echo "PASS: driver mode fails a fixture with no structural summary"
fi
if [ -d "$(captured_of "$work")/anthropic-api/halfway-01" ]; then
    echo "FAIL: driver mode landed a fixture with no structural summary"
    fails=$((fails + 1))
else
    echo "PASS: no fixture lands when a structural summary is absent"
fi
rm -rf "$work"

work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_non_stream 019eab77-0000-4000-8000-000000000010 anthropic)" || rc=$?
check "live-box mode exits 0 with no structural summary" "0" "$rc"
check_log "live-box mode warns about the absent summary" \
    "WARN no ingress and outgoing structural summary" "$work/rig.log"
if [ -d "$(captured_of "$work")/019eab77-0000-4000-8000-000000000010" ]; then
    echo "PASS: live-box mode keeps a fixture with no structural summary"
else
    echo "FAIL: live-box mode dropped a fixture over an absent structural summary"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 14: driver mode refuses to promote what --check rejects ------
# The scrub `--check` deny set is derived from the environment, so the
# seeded value is a SYNTHETIC third-party home prefix -- never a real
# path from this machine. `--write` cannot rewrite another account's home
# (there is no safe automatic mapping), so the residue reaches `--check`,
# which is exactly the split this gate exists to catch.
set_pins dirty-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver_dirty 019eab77-0000-4000-8000-000000000011)" --driver-mode || rc=$?
clear_pins
if [ "$rc" -eq 0 ]; then
    echo "FAIL: driver mode promoted a fixture the scrub check rejects"
    fails=$((fails + 1))
else
    echo "PASS: driver mode refuses to promote a fixture the scrub check rejects"
fi
if [ -d "$(captured_of "$work")/anthropic-api/dirty-01" ]; then
    echo "FAIL: a scrub-refused fixture reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a scrub-refused fixture does not reach the corpus"
fi
check_log "the scrub refusal is reported" "scrub check refused" "$work/rig.log"
rm -rf "$work"

# Paired positive control: the same driver path promotes a clean fixture,
# so the refusal above is the check firing rather than driver mode being
# unable to promote anything at all. (Case 8 asserts the same landing;
# this one asserts it against the scrub gate specifically.)
set_pins clean-scrub-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000012)" --driver-mode || rc=$?
clear_pins
check "driver mode promotes a fixture the scrub check accepts" "0" "$rc"
if [ -f "$(captured_of "$work")/anthropic-api/clean-scrub-01/meta.json" ]; then
    echo "PASS: a clean driver fixture is promoted"
else
    echo "FAIL: a clean driver fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 15: json_escape still covers the driver-mode pins -----------
# Driver mode makes the pins mandatory, so the hostile values go in the
# pins themselves. A case id names a DIRECTORY now, so the quote rides in
# config_sha and connection_mode while the case id stays path-safe -- the
# escaping contract is per-value, and these are the two whose values a
# driver sets programmatically without a path constraint.
set_pins quote-case-01 'sha"with\quote' 'mode"x'
work="$(make_repo)"
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000013)" --driver-mode || true
clear_pins
captured="$(captured_of "$work")"
meta="$captured/anthropic-api/quote-case-01/meta.json"
if [ -f "$meta" ]; then
    if is_valid_json "$meta"; then
        echo "PASS: a driver meta.json parses with a quote in a pin"
        check "an embedded quote and backslash round-trip through config_sha" \
            'sha"with\quote' "$(meta_get "$meta" config_sha)"
        check "an embedded quote round-trips through connection_mode" \
            'mode"x' "$(meta_get "$meta" client.connection_mode)"
    else
        echo "FAIL: a driver meta.json is invalid JSON when a pin carries a quote"
        cat "$meta"
        fails=$((fails + 1))
    fi
else
    echo "FAIL: quoted-pin driver capture produced no meta.json (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 16: a case id that is not a single path segment is refused --
# The case id names the landing directory, so a separator or a traversal
# segment in it would write outside the lane dir -- past the --out
# confinement check, which ran on OUT alone.
for bad in ../escape 'nested/case'; do
    set_pins "$bad" abc123 base-url
    work="$(make_repo)"
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000014)" --driver-mode || rc=$?
    clear_pins
    if [ "$rc" -eq 0 ]; then
        echo "FAIL: driver mode accepted the case id '$bad'"
        fails=$((fails + 1))
    else
        echo "PASS: driver mode refuses the case id '$bad'"
    fi
    rm -rf "$work"
done

# --- Case 17: two completed requests under one case id are refused ----
# One case id pins ONE interaction. Two completions in a single driver
# trace both key on the same landing path, so the second would silently
# overwrite the first and the corpus entry would depend on completion
# order. The refusal says the driver captured a case it did not isolate.
set_pins shared-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000015)
$(trace_driver 019eab77-0000-4000-8000-000000000016)" --driver-mode || rc=$?
clear_pins
if [ "$rc" -eq 0 ]; then
    echo "FAIL: driver mode accepted two requests under one case id"
    fails=$((fails + 1))
else
    echo "PASS: driver mode refuses two requests under one case id"
fi
check_log "the refusal names the already-landed case" "already landed this run" \
    "$work/rig.log"
rm -rf "$work"

# --- Case 18: --help still renders the header ------------------------
# The usage extraction is sentinel-delimited; a line-count range silently
# starts cutting the moment the header grows, and the driver-mode policy
# is exactly the part a caller needs to read.
help_out="$(bash "$RIG" --help 2>&1 || true)"
if printf '%s' "$help_out" | grep -q -- '--driver-mode' &&
    ! printf '%s' "$help_out" | grep -q 'END USAGE'; then
    echo "PASS: --help renders the header including driver mode"
else
    echo "FAIL: --help output is truncated or leaks the sentinel"
    printf '%s\n' "$help_out" | tail -5
    fails=$((fails + 1))
fi

if [ "$fails" -gt 0 ]; then
    echo "capture_fixtures self-test: $fails failure(s)"
    exit 1
fi
echo "capture_fixtures self-test: all assertions passed"
