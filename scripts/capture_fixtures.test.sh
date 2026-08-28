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
CONFINE="$HERE/drivers/lib/confine.sh"

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

# A trace holding a SENT request that never completed: ingress and
# outgoing bodies plus both structural summaries, and no `upstream success
# body` / `stream summary` line for the rig to key a completion on. This is
# what a 429, an upstream that returned no success body, or a client that
# died mid-request leaves in the log -- the shape that makes driver mode
# land zero fixtures with nothing refused.
trace_no_completion() {
    local id="$1" kind="${2:-anthropic}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    cat <<TRACE
2026-08-25T10:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:complete{provider=$kind:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="$kind" provider=p body={"model":"claude-sonnet-4-5"} redact_prompts_enabled=false
2026-08-25T10:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"]]
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" headers=[["content-type","application/json"]]
TRACE
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# A LARGE `tool_result`-shaped ingress body, with `$1` buried deep inside
# the padded region -- past the first few thousand bytes, so an assertion
# on it fails the moment the scrub gate reads a prefix instead of the whole
# file. The padding imitates what the client actually puts on the wire for
# a file read: line-numbered content, JSON-escaped, wrapped in a
# `tool_result` block. That framing is the reason a size reduction may
# never run before the gate, so the fixture the ordering cases drive has to
# carry it.
large_tool_result_body() {
    local buried="$1"
    local pad="" i=1
    while [ "$i" -le 120 ]; do
        pad+="   $i\\tconst answer = 42;\\n"
        i=$((i + 1))
    done
    printf '{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"%s%s%s"}]}]}' \
        "$pad" "read $buried" "$pad"
}

# A driver trace whose ingress body is the large `tool_result` block above.
# `$2` is the path buried in the middle of it.
trace_driver_large() {
    local id="$1" buried="$2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local body
    body="$(large_tool_result_body "$buried")"
    printf '2026-08-25T10:00:00.000000Z TRACE %s:messages{ingress="anthropic"}: %s ingress request body ingress="anthropic" body=%s redact_prompts_enabled=false\n' \
        "$span" "$target" "$body"
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
    mkdir -p "$tmp/repo/scripts/drivers/lib" \
        "$tmp/repo/crates/routectl-cli/tests/fixtures"
    cp "$RIG" "$tmp/repo/scripts/capture_fixtures.sh"
    # The rig sources its --out confinement from the shared library and
    # refuses to run without it, so the throwaway repo carries the real one.
    cp "$CONFINE" "$tmp/repo/scripts/drivers/lib/confine.sh"
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

# Same as rig_run, but keeps the two streams APART: `<tmp>/rig.out` and
# `<tmp>/rig.err`. The zero-landing verdict splits across them on purpose
# -- the human `captured=` line stays on stdout while the machine-facing
# refusal names the case on stderr -- and a merged log cannot tell a
# regression that moved one onto the other.
rig_run_split() {
    local tmp="$1" trace_text="$2"
    shift 2
    printf '%s\n' "$trace_text" >"$tmp/trace.log"
    local rc=0
    (
        cd "$tmp/repo" || exit 2
        bash scripts/capture_fixtures.sh --log "$tmp/trace.log" "$@"
    ) >"$tmp/rig.out" 2>"$tmp/rig.err" || rc=$?
    return "$rc"
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

# Set / clear the four driver pins in one call, so a case cannot leak a
# pin into the next one (driver mode is fail-closed on all four, so a
# leaked pin turns a refusal assertion into a silent pass). The wire
# pattern defaults because most cases care about the other three; a case
# that needs a specific claim passes it as the fourth argument.
set_pins() {
    export ROUTECTL_FIXTURE_CASE_ID="$1"
    export ROUTECTL_FIXTURE_CONFIG_SHA="$2"
    export ROUTECTL_FIXTURE_CONNECTION_MODE="$3"
    export ROUTECTL_FIXTURE_WIRE_PATTERN="${4:-baseline}"
}
clear_pins() {
    unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA \
        ROUTECTL_FIXTURE_CONNECTION_MODE ROUTECTL_FIXTURE_WIRE_PATTERN
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
export ROUTECTL_FIXTURE_WIRE_PATTERN="tool-use-multiturn"
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000004 anthropic)")"
meta="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000004/meta.json"
unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA ROUTECTL_FIXTURE_CONNECTION_MODE \
    ROUTECTL_FIXTURE_WIRE_PATTERN
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "case_id comes from the environment" "smoke-01" "$(meta_get "$meta" case_id)"
    check "config_sha comes from the environment" "deadbeef" "$(meta_get "$meta" config_sha)"
    check "connection_mode comes from the environment" \
        "base-url" "$(meta_get "$meta" client.connection_mode)"
    check "wire_pattern comes from the environment" \
        "tool-use-multiturn" "$(meta_get "$meta" wire_pattern)"
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
# Four cases plus a paired control. Without the control a rig that
# refused unconditionally -- or one that refused for an unrelated reason --
# would pass all four refusal assertions.
for missing in CASE_ID CONFIG_SHA CONNECTION_MODE WIRE_PATTERN; do
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

# Paired control: the SAME trace with all four pins unset captures fine
# on the unflagged live-box path, where an empty pin is honest.
clear_pins
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000007)" || rc=$?
meta="$(captured_of "$work")/019eab77-0000-4000-8000-000000000007/meta.json"
check "live-box mode tolerates all four pins unset" "0" "$rc"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "an unpinned live-box capture leaves case_id empty" \
        "" "$(meta_get "$meta" case_id)"
    check "an unpinned live-box capture leaves config_sha empty" \
        "" "$(meta_get "$meta" config_sha)"
    check "an unpinned live-box capture leaves connection_mode empty" \
        "" "$(meta_get "$meta" client.connection_mode)"
    check "an unpinned live-box capture leaves wire_pattern empty" \
        "" "$(meta_get "$meta" wire_pattern)"
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
set_pins tools-multiturn-01 abc123 base-url tool-use-multiturn
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
    check "the wire pattern pin reaches a driver meta.json" \
        "tool-use-multiturn" "$(meta_get "$dir/meta.json" wire_pattern)"
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
# A refusal is exit 1 and must never arrive as the zero-landing 3: a
# refused fixture is a defect a runner must never retry, while a zero
# landing is retryable. Conflated, the caller has to parse stderr.
check "a scrub refusal exits 1, not the zero-landing 3" "1" "$rc"
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

# --- Case 14b: driver mode refuses an unclassified lane ---------------
# The rig asks `scrub-fixture.sh --lane-known <lane>` and refuses on a
# non-zero answer, so a fixture on a lane whose credential shape nobody
# has classified cannot land. Every lane token normalize_lane emits is
# classified in the shipped table -- which is the point of the table --
# so the unclassified state is produced by narrowing the throwaway repo's
# COPY of the gate, not by inventing a lane token the rig would map to an
# empty lane (that is the neighbouring refusal, already covered).
#
# The narrowing is verified before the rig runs: a sed that matched
# nothing would leave the table intact and this case would assert against
# the classified path while claiming to cover the unclassified one.
strip_shape_row() {
    local tmp="$1" lane="$2"
    local gate="$tmp/repo/scripts/scrub-fixture.sh"
    grep -q "^  \"$lane=" "$gate" || return 1
    sed -i "/^  \"$lane=/d" "$gate"
    ! grep -q "^  \"$lane=" "$gate"
}

set_pins unclassified-lane-01 abc123 base-url
work="$(make_repo)"
if strip_shape_row "$work" gemini; then
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000001d gemini)" \
        --driver-mode || rc=$?
    check "an unclassified lane refuses with exit 1, not the zero-landing 3" "1" "$rc"
    if [ -d "$(captured_of "$work")/gemini" ]; then
        echo "FAIL: a fixture on an unclassified lane reached the corpus"
        fails=$((fails + 1))
    else
        echo "PASS: a fixture on an unclassified lane does not reach the corpus"
    fi
    # The message must NAME the lane: a runner reading only "not promoting"
    # cannot tell this refusal from the scrub-residue one.
    check_log "the unclassified-lane refusal names the lane" \
        "lane 'gemini' has no credential-shape classification" "$work/rig.log"
    # And it must not echo fixture content into a CI log.
    if grep -qF "claude-sonnet-4-5" "$work/rig.log"; then
        echo "FAIL: the unclassified-lane refusal echoed fixture content"
        fails=$((fails + 1))
    else
        echo "PASS: the unclassified-lane refusal echoes no fixture content"
    fi
    # Nothing staged is left behind: the tmp directory is discarded, not
    # abandoned under the corpus root for a later run to promote.
    if [ -d "$(captured_of "$work")" ] &&
        [ -n "$(find "$(captured_of "$work")" -maxdepth 1 -name '.tmp.*' -print -quit)" ]; then
        echo "FAIL: the refused fixture left a staged tmp directory behind"
        fails=$((fails + 1))
    else
        echo "PASS: the refused fixture leaves no staged tmp directory"
    fi
else
    echo "FAIL: could not narrow the shape table in the throwaway gate copy"
    fails=$((fails + 1))
fi
clear_pins
rm -rf "$work"

# Paired positive control for the case above: the SAME trace on the SAME
# lane promotes against the unnarrowed table, so the refusal is the
# lane-classification gate firing and not driver mode refusing every
# gemini capture.
set_pins classified-lane-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000001e gemini)" \
    --driver-mode || rc=$?
clear_pins
check "a classified lane still promotes at exit 0" "0" "$rc"
if [ -f "$(captured_of "$work")/gemini/classified-lane-01/meta.json" ]; then
    echo "PASS: a fixture on a classified lane is promoted"
else
    echo "FAIL: a fixture on a classified lane was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# The rule has ONE enforcement point. capture_driver.sh must carry no lane
# vocabulary and no copy of the table: a second copy is the drift point
# the single owner exists to avoid.
assert_runner_holds_no_shape_table() {
    local runner="$HERE/capture_driver.sh"
    local pattern='PROVIDER_SHAPE_(KINDS|EXCLUDED)|--lane-known'
    # Positive control: the pattern must actually match SOMETHING in this
    # repo, or the absence assertion below passes against a typo.
    if ! grep -qE "$pattern" "$RIG"; then
        echo "FAIL: the shape-vocabulary pattern matches nothing in the rig; it cannot detect a copy"
        fails=$((fails + 1))
        return
    fi
    if grep -qE "$pattern" "$runner"; then
        echo "FAIL: capture_driver.sh carries a second copy of the shape vocabulary"
        fails=$((fails + 1))
    else
        echo "PASS: capture_driver.sh holds no copy of the shape table"
    fi
}
assert_runner_holds_no_shape_table

# --- Case 15: json_escape still covers the driver-mode pins -----------
# Driver mode makes the pins mandatory, so the hostile values go in the
# pins themselves. A case id names a DIRECTORY now, so the quote rides in
# config_sha and connection_mode while the case id stays path-safe -- the
# escaping contract is per-value, and these are the two whose values a
# driver sets programmatically without a path constraint.
set_pins quote-case-01 'sha"with\quote' 'mode"x' 'wire"y'
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
        check "an embedded quote round-trips through wire_pattern" \
            'wire"y' "$(meta_get "$meta" wire_pattern)"
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

# --- Case 18: driver mode landing ZERO fixtures is a failed run --------
# The shape a 429, an upstream that returned no success body, or a client
# that died mid-request leaves behind: the request was SENT (ingress and
# outgoing bodies are traced) and nothing ever completed, so there is no
# `upstream success body` and no `stream summary` for the rig to key on.
# Exit 3 is the whole point of the case: without it this run is
# byte-identical to a live-box quiet window and spends real tokens
# reporting success.
set_pins no-completion-01 abc123 base-url
work="$(make_repo)"
rc=0
# PREMISE ASSERTION. Without this the exit-3 assertions below hold for ANY
# trace the rig finds nothing in -- an empty file, an unparseable one -- rather
# than for the named cause: a request that was SENT and never COMPLETED. Same
# shape as the vacuous-negative class in meta/learnings.md, where the fixture
# never reaches the branch the test is named for.
no_completion_trace="$(trace_no_completion 019eab77-0000-4000-8000-000000000017)"
if printf '%s\n' "$no_completion_trace" | grep -q 'ingress request body' &&
   printf '%s\n' "$no_completion_trace" | grep -q 'outgoing request body' &&
   printf '%s\n' "$no_completion_trace" | grep -q 'structural summary direction="ingress"' &&
   printf '%s\n' "$no_completion_trace" | grep -q 'structural summary direction="outgoing"' &&
   ! printf '%s\n' "$no_completion_trace" | grep -q 'upstream success body' &&
   ! printf '%s\n' "$no_completion_trace" | grep -q 'stream summary'; then
    echo "PASS: the no-completion trace holds a SENT request and no completion"
else
    echo "FAIL: the no-completion fixture is not the shape it is named for --"
    echo "FAIL: exit-3 would then be asserted for 'found nothing', not 'never completed'"
    fails=$((fails + 1))
fi

rig_run_split "$work" "$(trace_no_completion 019eab77-0000-4000-8000-000000000017)" \
    --driver-mode || rc=$?
clear_pins
check "a driver run that lands zero fixtures exits 3" "3" "$rc"
check_log "the zero-landing run still prints its captured= line on stdout" \
    "captured=0" "$work/rig.out"
check_log "the zero-landing message names the case id" "no-completion-01" \
    "$work/rig.err"
check_log "the zero-landing message names the trace path" "$work/trace.log" \
    "$work/rig.err"
rm -rf "$work"

# The other direction, on the SAME trace: in live-box mode zero captures
# is the normal answer for a quiet window and MUST stay exit 0. The two
# modes' policies differ on purpose, so both are asserted here -- and the
# exit-0 direction is the one that decays silently if the guard is ever
# widened.
clear_pins
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(trace_no_completion 019eab77-0000-4000-8000-000000000018)" || rc=$?
check "a LIVE-BOX run over the same trace exits 0" "0" "$rc"
check_log "the live-box zero-capture run reports captured=0" "captured=0" \
    "$work/rig.out"
if grep -q 'landed no fixture' "$work/rig.err"; then
    echo "FAIL: live-box mode emitted the driver-mode zero-landing message"
    fails=$((fails + 1))
else
    echo "PASS: live-box mode emits no zero-landing message"
fi
rm -rf "$work"

# Positive control for the guard: the same driver path over a trace that
# DOES hold a completed request still exits 0, so the 3 above is the
# zero-landing check firing and not driver mode failing outright.
set_pins completion-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000019)" \
    --driver-mode || rc=$?
clear_pins
check "a driver run that lands a fixture still exits 0" "0" "$rc"
check_log "the landing run reports captured=1" "captured=1" "$work/rig.out"
rm -rf "$work"

# --- Case 19: scrub --write and --check COMPOSE over the full bytes ----
# The ordering contract documented at the scrub block in write_fixture:
# `--write`, then `--check` on the FULL bytes, then any size reduction,
# then promote. No reduction exists yet, so what is pinned here is that the
# two existing mechanisms compose -- a fixture is scrubbed AND checked
# before it lands, and the check reads the whole file rather than a prefix
# or a reduced form. That is the property a later reduction inserted in the
# wrong place would break, and it is why the comment names the position.
#
# The offending content is a synthetic third-party home path buried in the
# MIDDLE of a large `tool_result`-shaped block: `--write` has no safe
# rewrite for another account's home, so the residue reaches `--check`, and
# its depth in the body is what makes a prefix-only scan observable.
BURIED_PATH="/home/someoneelse/notes.txt"

# PREMISE ASSERTION. Without it the refusal below would hold for a body
# where the path sits in the first bytes -- which a prefix scan also
# catches -- so the case would pass while proving nothing about the full
# bytes. Same vacuous-negative shape as the no-completion premise above.
large_body="$(large_tool_result_body "$BURIED_PATH")"
buried_offset="$(printf '%s' "${large_body%%"$BURIED_PATH"*}" | wc -c)"
if [ "${#large_body}" -gt 4000 ] && [ "$buried_offset" -gt 2000 ]; then
    echo "PASS: the offending path sits deep inside a large tool_result body"
else
    echo "FAIL: the large-body fixture is not the shape it is named for --"
    echo "FAIL: body ${#large_body} bytes, path at offset $buried_offset"
    fails=$((fails + 1))
fi
unset large_body buried_offset

set_pins deep-residue-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver_large 019eab77-0000-4000-8000-00000000001f "$BURIED_PATH")" \
    --driver-mode || rc=$?
clear_pins
check "a deep residue in a large body refuses with exit 1" "1" "$rc"
# The class NAMES the mechanism that caught it: home-prefix is the class
# whose whole purpose is the `file_path` shape a tool_result block carries,
# so a refusal under any other class would mean the composition held by
# accident.
check_log "the deep-residue refusal names the home-prefix class" "home-prefix" \
    "$work/rig.log"
check_log "the deep-residue refusal is the scrub check" "scrub check refused" \
    "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/deep-residue-01" ]; then
    echo "FAIL: a fixture with a deep residue reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a fixture with a deep residue does not reach the corpus"
fi
# Nothing at all lands under the corpus root, staged or promoted: the
# ordering contract ends in `promote`, and a refusal must not leave the
# reduced-or-not bytes anywhere a later run could pick up.
if [ -d "$(captured_of "$work")" ] &&
    [ -n "$(find "$(captured_of "$work")" -mindepth 1 -print -quit)" ]; then
    echo "FAIL: the deep-residue refusal left content under the corpus root"
    find "$(captured_of "$work")" -mindepth 1 | sed -n '1,10p'
    fails=$((fails + 1))
else
    echo "PASS: nothing lands under the corpus root on a deep-residue refusal"
fi
rm -rf "$work"

# Positive control: the SAME large body without the offending path
# promotes at exit 0, so the refusal above is the gate reading the whole
# file and not the rig refusing a body for its size.
set_pins large-clean-01 abc123 base-url
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver_large 019eab77-0000-4000-8000-000000000020 /tmp/notes.txt)" \
    --driver-mode || rc=$?
clear_pins
check "the same large body without the residue promotes at exit 0" "0" "$rc"
ingress="$(captured_of "$work")/anthropic-api/large-clean-01/ingress_request.json"
if [ -f "$ingress" ]; then
    echo "PASS: a large clean driver fixture is promoted"
    # The promoted bytes are the FULL body: the contract's third step is
    # "only then any size reduction", and nothing reduces today. A future
    # reduction landing before the gate would show up here as a short file.
    if [ "$(wc -c <"$ingress")" -gt 4000 ]; then
        echo "PASS: the promoted fixture carries the full body, unreduced"
    else
        echo "FAIL: the promoted fixture is shorter than the body it captured"
        fails=$((fails + 1))
    fi
else
    echo "FAIL: a large clean driver fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"
unset ingress

# The ORDER of the two steps, not just their presence. A body carrying the
# CONTRIBUTOR'S OWN home path is clean only AFTER `--write` neutralizes it:
# `--check` derives its deny set from the environment, so the same bytes
# scanned first are a `home-path` finding and the fixture is refused. This
# case therefore promotes under `--write` -> `--check` and refuses under the
# reverse, which is the one assertion that distinguishes the two orders.
# (Guarded: with $HOME unset or already the neutral placeholder there is no
# rewrite to observe, and the case would assert nothing.)
own_home="${HOME:-}"
own_home="${own_home%/}"
# The placeholder is the gate's own constant; reading it from there keeps
# this guard from drifting the day the placeholder changes.
placeholder_home="$(sed -n 's/^PLACEHOLDER_HOME="\(.*\)"$/\1/p' "$SCRUB")"
if [ -z "$placeholder_home" ]; then
    echo "FAIL: could not read PLACEHOLDER_HOME out of the scrub gate"
    fails=$((fails + 1))
fi
if [ -n "$own_home" ] && [ "$own_home" != "$placeholder_home" ]; then
    set_pins own-home-01 abc123 base-url
    work="$(make_repo)"
    rc=0
    rig_run "$work" \
        "$(trace_driver_large 019eab77-0000-4000-8000-000000000021 "$own_home/notes.txt")" \
        --driver-mode || rc=$?
    clear_pins
    check "a body holding the contributor's own home promotes: --write ran before --check" \
        "0" "$rc"
    ingress="$(captured_of "$work")/anthropic-api/own-home-01/ingress_request.json"
    if [ -f "$ingress" ] && ! grep -qF -- "$own_home" "$ingress"; then
        echo "PASS: the promoted fixture carries the placeholder home, not the real one"
    else
        echo "FAIL: the contributor's own home survived into the promoted fixture"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
    unset ingress
else
    echo "FAIL: \$HOME is unset or already the placeholder; the scrub ORDER is unasserted"
    fails=$((fails + 1))
fi
unset own_home placeholder_home

# --- Case 20: --out confinement, both directions ---------------------
# The confinement lives in scripts/drivers/lib/confine.sh, sourced by the
# rig and by every other script that writes capture output to a
# caller-supplied path. Fixtures carry RAW headers, so an --out the rig
# accepts is a write of credential-bearing content to that path.
#
# Every refusal below is asserted alongside the ACCEPT that proves the
# same machinery says yes when it should. Without the accept controls a
# confinement that refused unconditionally -- or one that refused because
# the throwaway repo was malformed -- would pass every refusal assertion.
confine_trace="$(trace_non_stream 019eab77-0000-4000-8000-000000000020 anthropic)"

# ACCEPT CONTROL for the whole case: an ordinary subdirectory under the
# captured tree, no symlink anywhere, is accepted and the fixture lands.
work="$(make_repo)"
captured="$(captured_of "$work")"
mkdir -p "$captured/plain"
rc=0
rig_run "$work" "$confine_trace" --out "$captured/plain" || rc=$?
check "a plain subdirectory of the captured tree is accepted" "0" "$rc"
if [ -d "$captured/plain/019eab77-0000-4000-8000-000000000020" ]; then
    echo "PASS: the accepted --out is where the fixture lands"
else
    echo "FAIL: the accepted --out landed no fixture (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# REFUSAL 1: a DANGLING symlink component under the captured tree. This
# is the case the per-component `[ -L ]` walk exists for and the only one
# the physical resolution alone cannot see: `cd -P` walks up to the
# nearest EXISTING ancestor, and a broken link plus a not-yet-created
# leaf has no existing ancestor below the captured root to resolve
# through, so the physical compare reports the path as confined.
work="$(make_repo)"
captured="$(captured_of "$work")"
mkdir -p "$captured"
ln -s /nonexistent-confinement-target "$captured/dangling"
rc=0
rig_run "$work" "$confine_trace" --out "$captured/dangling/leaf" || rc=$?
check "a dangling symlink component in --out is refused with exit 2" "2" "$rc"
check_log "the dangling-symlink refusal names the symlink component" \
    "symlink component at" "$work/rig.log"
rm -rf "$work"

# PREMISE ASSERTION for refusal 1. The refusal above is only evidence
# about the `[ -L ]` walk if the physical compare would have ACCEPTED the
# same shape -- otherwise the walk is decoration and deleting it would
# leave the suite green. Resolve the identical path through the library's
# own `abspath_physical` and assert it reports as confined: `cd -P` walks
# up to the nearest EXISTING ancestor, so the broken link and the
# not-yet-created leaf both survive as an in-tree-looking tail.
work="$(make_repo)"
captured="$(captured_of "$work")"
mkdir -p "$captured"
ln -s /nonexistent-confinement-target "$captured/dangling"
phys_verdict="$(
    # shellcheck source=scripts/drivers/lib/confine.sh
    . "$CONFINE"
    root="$(abspath_physical "$captured")"
    cand="$(abspath_physical "$captured/dangling/leaf")"
    case "$cand" in
        "$root" | "$root"/*) printf 'confined\n' ;;
        *) printf 'outside\n' ;;
    esac
)"
check "physical resolution alone reads the dangling path as confined" \
    "confined" "$phys_verdict"
rm -rf "$work"

# REFUSAL 2: a LIVE symlink component under the captured tree pointing
# out of it. Distinct from refusal 1 -- this one the physical compare
# would also catch, so it pins that a resolvable link is refused at the
# earlier, more specific message rather than falling through.
work="$(make_repo)"
captured="$(captured_of "$work")"
mkdir -p "$captured" "$work/elsewhere"
ln -s "$work/elsewhere" "$captured/live"
rc=0
rig_run "$work" "$confine_trace" --out "$captured/live/sub" || rc=$?
check "a live symlink component in --out is refused with exit 2" "2" "$rc"
check_log "the live-symlink refusal names the symlink component" \
    "symlink component at" "$work/rig.log"
rm -rf "$work"

# REFUSAL 3: a `..` traversal that climbs out of the captured tree. The
# lexical collapse normalizes it before the compare, so the refusal is
# the containment test firing rather than a string mismatch on `..`.
work="$(make_repo)"
captured="$(captured_of "$work")"
rc=0
rig_run "$work" "$confine_trace" --out "$captured/../../../../src" || rc=$?
check "a .. traversal out of the captured tree is refused with exit 2" "2" "$rc"
check_log "the traversal refusal names the default captured dir" \
    "outside the default captured dir" "$work/rig.log"
rm -rf "$work"

# ACCEPT CONTROL for refusal 3: a `..` traversal that lands BACK inside
# the captured tree is accepted. Without it "reject anything containing
# `..`" satisfies refusal 3, and the lexical collapse the refusal is
# supposed to prove would be deletable.
work="$(make_repo)"
captured="$(captured_of "$work")"
mkdir -p "$captured/inner"
rc=0
rig_run "$work" "$confine_trace" --out "$captured/inner/../inner" || rc=$?
check "a .. traversal that stays inside the captured tree is accepted" "0" "$rc"
rm -rf "$work"

# ACCEPT CONTROL for the bypass: --allow-unsafe-out is what makes a
# deliberate out-of-tree capture possible, so refusal 3 must not hold
# with the flag set. This also proves the refusals above come from the
# confinement block and not from an unrelated failure on the path.
work="$(make_repo)"
rc=0
rig_run "$work" "$confine_trace" --out "$work/outside" --allow-unsafe-out || rc=$?
check "--allow-unsafe-out permits an out-of-tree --out" "0" "$rc"
rm -rf "$work"

# The helper is the ONLY copy: a re-implementation in the rig is the
# exact drift D6 forbids, and it would be invisible to every assertion
# above (both copies would pass).
for fn in 'abspath_lexical()' 'abspath_physical()'; do
    check "the rig defines no local $fn" "0" \
        "$(grep -cF "$fn" "$RIG")"
    check "the shared library defines $fn" "1" \
        "$(grep -cF "$fn" "$CONFINE")"
done
# The literal being grepped for is the rig's own source line, so the `$`
# inside it is text rather than an expansion.
# shellcheck disable=SC2016
source_lines="$(grep -c '^\. "\$CONFINE_LIB"$' "$RIG" | tr -d ' ')"
check "the rig sources the shared confinement library" "1" "$source_lines"

# A NEWLINE in --out defeated every check below it: both resolvers read their
# result back through `$(...)`, which strips trailing newlines, so the guard
# validated only the FIRST line while `mkdir -p` created `<root>\n...` as a
# SIBLING of the confinement root -- and that sibling is NOT covered by the
# captured tree's gitignore entry, so credential-bearing content would land on
# a git-TRACKED path. Measured and fixed 2026-08-28.
work="$(make_repo)"
confine_trace="$work/trace.log"
: >"$confine_trace"
rc=0
rig_run "$work" "$confine_trace" --out "$(captured_of "$work")
/escaped" || rc=$?
check "a newline in --out is refused" "2" "$rc"
check "the newline refusal names the cause" "1" \
    "$(grep -c 'contains a newline' "$work/rig.log")"
# The sibling the bypass used to create must not exist.
check "no sibling of the confinement root is created" "0" \
    "$(find "$work/repo/crates/routectl-cli/tests/fixtures" -maxdepth 1 -name 'captured*' \
        ! -name captured | wc -l | tr -d ' ')"
# ACCEPT CONTROL: the same path WITHOUT the newline is still accepted, so the
# guard discriminates rather than refusing every path. (An earlier draft used
# `$(printf '\n')` as the pattern, which strips to EMPTY and matches
# everything -- it would have rejected every legitimate --out.)
rc=0
rig_run "$work" "$confine_trace" --out "$(captured_of "$work")/ok" || rc=$?
check "the same --out without a newline is accepted" "0" "$rc"
rm -rf "$work"

# The fail-closed half of that contract: with the library ABSENT the rig
# must refuse rather than fall back to writing unconfined.
#
# TWO independent mechanisms enforce this, which is why only the message
# assertion below goes red when the explicit guard is downgraded to a
# warning: the `. "$CONFINE_LIB"` source itself fails under `set -e`, so
# the exit code and the no-directory assertions still hold. That is
# defence in depth, not vacuity -- measured 2026-08-27. Keep all three:
# the message assertion is the one that pins the EXPLICIT guard, and the
# other two pin the outcome whichever mechanism fires. Every other
# assertion in this case runs WITH the library present, so those are the
# paired accept control -- without this one, a refactor that reordered the
# guard below `mkdir -p "$OUT"`, or downgraded it to a warning, would keep
# the whole suite green while turning the rig into an unconfined write
# primitive for credential-bearing fixtures.
work="$(make_repo)"
confine_trace="$work/trace.log"
: >"$confine_trace"
rm -f "$work/repo/scripts/drivers/lib/confine.sh"
rc=0
rig_run "$work" "$confine_trace" --out "$work/outside" --allow-unsafe-out || rc=$?
check "the rig refuses to run when the confinement library is absent" "1" "$rc"
# rig_run funnels stdout+stderr into rig.log and returns only the code.
check "the refusal names the missing library" "1" \
    "$(grep -c 'confinement library not found' "$work/rig.log")"
check "no --out directory is created when the library is absent" "0" \
    "$([ -e "$work/outside" ] && echo 1 || echo 0)"
rm -rf "$work"

# The other fail-closed half: an UNRESOLVABLE confinement root. Both
# resolvers report failure by `exit 2`, which terminates only the
# command-substitution SUBSHELL -- so the assignment in the caller yields
# an EMPTY string, and the prefix compare below it runs against an empty
# root, which matches EVERY path. Without the explicit `|| exit 2` pair
# and the emptiness refusal, the confinement RETURNS 0 for `/etc/anything`.
#
# The trigger is an ancestor the process cannot traverse: mode 000 on the
# directory above the captured tree makes `cd -P` fail there, so the root
# resolves empty while the candidate resolves fine. Root traverses
# mode-000 directories, so the case is skipped -- named, never silently
# passed -- for euid 0.
#
# The assertions call the library the way the SECOND caller shape does
# (`confine_out_under ... || rc=$?`), not through the rig. The rig calls
# it bare under `set -e`, so an unresolvable root aborts the rig by
# errexit whether or not the guard exists: measured 2026-08-28, every
# rig-level assertion here stays green with the guard deleted. Only a
# caller that MAPS the exit code can observe the difference, and the
# library is shared, so that shape is part of its contract.
#
# The guard is TWO mechanisms -- the `|| exit 2` pair on the assignments
# and the emptiness refusal below them -- and either one alone refuses.
# So the negative assertion goes red only when BOTH are removed, which is
# the fail-open shape it exists to forbid; it is not evidence about either
# half in isolation. Measured 2026-08-28.
if [ "$(id -u)" = "0" ]; then
    echo "SKIP: unresolvable-root confinement (euid 0 traverses mode-000 dirs)"
else
    work="$(make_repo)"
    captured="$(captured_of "$work")"
    # The captured tree lives under `<repo>/crates`; blocking traversal
    # there is what makes the confinement root unresolvable.
    blocked="$work/repo/crates"
    # `chmod 000` on an ancestor leaves a directory nothing can delete, so
    # the mode is restored from an EXIT trap too: a failure anywhere below
    # would otherwise break the NEXT run and the harness's tmp cleanup.
    trap 'chmod 755 "$blocked" 2>/dev/null || true; rm -rf "$work"' EXIT
    chmod 000 "$blocked"

    # PREMISE ASSERTION. The refusal below is only evidence about the
    # guard if the compare it sits above would have ACCEPTED the same
    # shape -- otherwise the guard is decoration and deleting it would
    # leave the suite green. Resolve both sides through the library's own
    # `abspath_physical` and run that bare compare.
    phys_verdict="$(
        # shellcheck source=scripts/drivers/lib/confine.sh
        . "$CONFINE"
        root="$(abspath_physical "$captured" 2>/dev/null)" || :
        cand="$(abspath_physical "$work/outside")"
        if [ -n "$root" ]; then
            printf 'root-resolved\n'
        else
            case "$cand" in
                "$root" | "$root"/*) printf 'empty-root-accepts\n' ;;
                *) printf 'empty-root-refuses\n' ;;
            esac
        fi
    )"
    check "an unresolvable root resolves EMPTY and the bare compare then accepts an out-of-root path" \
        "empty-root-accepts" "$phys_verdict"

    # REFUSAL: the same out-of-root path through the real confinement is
    # refused, with the exit code MAPPED rather than propagated by errexit.
    # This is the assertion the guard owns.
    rc=0
    (
        set -eu
        # shellcheck source=scripts/drivers/lib/confine.sh
        . "$CONFINE"
        inner=0
        confine_out_under "$work/outside" "$captured" || inner=$?
        exit "$inner"
    ) 2>"$work/confine.log" || rc=$?
    check "an out-of-root path is refused when the confinement root is unresolvable" "2" "$rc"
    # The refusal must come from the RESOLUTION arm. The newline check and
    # the symlink walk sit ABOVE this guard and the out-of-tree compare
    # below it, so a refusal from any of those would satisfy the exit-code
    # assertion while testing nothing here.
    check_log "the unresolvable-root refusal names the resolution failure" \
        "cannot physically resolve path ancestor" "$work/confine.log"
    for other_arm in 'contains a newline' 'symlink component at' \
        'outside the default captured dir'; do
        check "the unresolvable-root refusal is not the '$other_arm' arm" "0" \
            "$(grep -cF -- "$other_arm" "$work/confine.log")"
    done
    # Only the ROOT side of the pair is asserted: an unresolvable CANDIDATE
    # leaves a NON-empty root, so the compare below the guard rejects it on
    # the out-of-tree arm regardless, and an assertion there would pass with
    # the guard deleted.

    # ACCEPT CONTROL: with traversal restored -- everything resolvable --
    # the same call accepts an in-tree path. Without it a guard that refused
    # unconditionally would satisfy the refusal above.
    chmod 755 "$blocked"
    mkdir -p "$captured/resolvable"
    rc=0
    (
        set -eu
        # shellcheck source=scripts/drivers/lib/confine.sh
        . "$CONFINE"
        inner=0
        confine_out_under "$captured/resolvable" "$captured" || inner=$?
        exit "$inner"
    ) 2>"$work/confine.log" || rc=$?
    check "an in-tree path is accepted once the confinement root resolves" "0" "$rc"

    # And the rig-level outcome, which the errexit propagation also
    # enforces: an unresolvable root must land no --out directory.
    chmod 000 "$blocked"
    confine_trace="$work/trace.log"
    : >"$confine_trace"
    rc=0
    rig_run "$work" "$confine_trace" --out "$work/outside" || rc=$?
    check "the rig refuses when the confinement root is unresolvable" "2" "$rc"
    check "no --out directory is created when the confinement root is unresolvable" "0" \
        "$([ -e "$work/outside" ] && echo 1 || echo 0)"
    chmod 755 "$blocked"

    trap - EXIT
    rm -rf "$work"
    unset blocked phys_verdict other_arm
fi

# --- Case 21: --help still renders the header ------------------------
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
