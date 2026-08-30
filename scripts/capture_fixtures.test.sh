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
VERIFY_PATTERN="$HERE/drivers/lib/verify_pattern.py"
INGRESS_KINDS="$HERE/drivers/lib/ingress_kinds.sh"

# The ingress dialects the rig accepts as an expected-ingress pin, read out
# of the shared library rather than restated: a restated list would pass
# this suite while the rig refused the value a real run passes. The library
# is itself welded to `IngressAdapter::id()` further down.
declare -a KNOWN_INGRESS_KINDS
mapfile -t KNOWN_INGRESS_KINDS < <(
    sed -n '/^# --- BEGIN INGRESS_KINDS ---$/,/^# --- END INGRESS_KINDS ---$/p' \
        "$INGRESS_KINDS" | sed -n 's/^ *"\([^"]*\)" *$/\1/p'
)
CLIENT_VERSION="$HERE/drivers/lib/client_version.py"

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
#
# `provider_kind` rides the OUTGOING and UPSTREAM header lines as well as
# the two body lines, and rides NEITHER ingress-side header line, because
# that is which log_safe call sites pass the field: the two egress-direction
# header traces take a provider kind, the two ingress-direction ones take
# the dialect. The rig's provider-vocabulary harvest is anchored to exactly
# that emitter set, so a replica that carried the field on all four or on
# none would leave half the anchor unexercised.
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
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
2026-08-25T10:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
2026-08-25T10:00:00.310000Z TRACE $span: $target egress response headers direction="egress" headers=[["content-type","application/json"]]
TRACE
}

# A STREAM request: the completion marker is a `stream summary` line and
# there is NO `upstream success body`, so the fixture lands with upstream
# response HEADERS and no upstream response BODY -- the combination the
# deleted `has_*` flags rejected.
#
# Same `provider_kind` placement as trace_non_stream, for the same reason:
# the field rides the two egress-direction header lines because that is
# which log_safe call sites pass it.
trace_stream() {
    local id="$1" kind="$2"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    cat <<TRACE
2026-08-25T11:00:00.000000Z TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body={"model":"claude-sonnet-4-5","stream":true} redact_prompts_enabled=false
2026-08-25T11:00:00.100000Z TRACE $span:complete_with_options{alias=my-alias}:stream{provider=$kind:p model=claude-sonnet-4-5}: $target outgoing request body provider_kind="$kind" provider=p body={"model":"claude-sonnet-4-5","stream":true} redact_prompts_enabled=false
2026-08-25T11:00:00.200000Z TRACE $span: $target stream summary direction="upstream" finish_reason="end_turn" prompt_tokens=11 completion_tokens=22 total_tokens=33
2026-08-25T11:00:00.010000Z TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"]]
2026-08-25T11:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
2026-08-25T11:00:00.210000Z TRACE $span: $target upstream response headers direction="upstream" provider_kind="$kind" provider=p headers=[["content-type","text/event-stream"]]
TRACE
}

# One REAL structural-summary line for one direction, in the layout
# log_safe.rs emits: the event message is `structural summary` right after
# the `routectl_core::log_safe: ` target, and `direction=` is a field of
# the same line. `$3` is the sub-second part of the timestamp so a caller
# can order the two directions.
#
# `thinking_shape=disabled` is the spelling the real client's explicit
# `{"type":"disabled"}` block produces, and the three predicate fields
# (`tools_len`, `thinking_shape`, `cache_control_count`) make this line
# `baseline` -- which is what the driver cases below claim, and what the
# rig's promotion gate now reads.
#
# `kind` and `id` are DIRECTION-DEPENDENT, as the emitter writes them: the
# ingress call site passes the ingress dialect token, the outgoing one
# passes the provider kind and the provider's configured id. The `baseline`
# predicate scopes itself to the Anthropic dialect off the ingress line's
# `id`, so a line reusing the outgoing spelling for both directions would
# be refused -- correctly, for naming a dialect no ingress adapter emits.
structural_line() {
    local id="$1" direction="$2" frac="${3:-400000}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local kind="anthropic" source="anthropic-api:anthropic"
    if [ "$direction" = "ingress" ]; then
        kind="ingress"
        source="anthropic"
    fi
    printf '%s TRACE %s: routectl_core::log_safe: structural summary direction="%s" kind="%s" id="%s" model=claude-sonnet-4-5 max_tokens=64 thinking_shape=disabled output_config_effort= tool_choice_shape= cache_control_count=0 messages_len=2 tools_len=0 anthropic_beta= provider_extras_keys= stream=false\n' \
        "2026-08-25T10:00:00.${frac}Z" "$span" "$direction" "$kind" "$source"
}

# A non-stream trace carrying BOTH request-side structural summaries --
# the shape a driver capture must produce, since driver mode refuses a
# fixture with half its structural evidence. `$3` is the traced INGRESS
# token, forwarded so a case can drive a dialect other than the default:
# the expected-ingress gate compares its pin against exactly this value,
# and with every driver trace saying `anthropic` a rig that ignored the
# traced token would be indistinguishable from one that reads it.
trace_driver() {
    local id="$1" kind="${2:-anthropic}" ingress="${3:-anthropic}"
    trace_non_stream "$id" "$kind" "$ingress"
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

# A driver trace whose ingress body carries a tool-call turn AND a later
# turn carrying its result -- the RESENT pair no single-turn capture can
# produce, and the census the `tool-use-multiturn` predicate reads. Its
# structural line offers a tools array, as a real permitted-tools request
# does, so this capture is NOT baseline and a mislabelled claim is
# refusable on the same bytes.
trace_driver_tools() {
    local id="$1"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local body='{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"list the files"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"notes.txt"}]}]}'
    printf '2026-08-25T10:00:00.000000Z TRACE %s:messages{ingress="anthropic"}: %s ingress request body ingress="anthropic" body=%s redact_prompts_enabled=false\n' \
        "$span" "$target" "$body"
    trace_non_stream "$id" anthropic | tail -n +2
    structural_line "$id" ingress 400000 | sed 's/tools_len=0/tools_len=16/'
    structural_line "$id" outgoing 500000 | sed 's/tools_len=0/tools_len=16/'
}

# A driver trace whose ingress structural line claims MORE than the
# `baseline` shape: an active thinking block and cache breakpoints. Used
# as the body of the pattern-gate refusal cases, where the recorded claim
# is `baseline` and the captured line contradicts it.
trace_driver_not_baseline() {
    local id="$1"
    trace_driver "$id" | sed \
        -e 's/thinking_shape=disabled/thinking_shape=enabled:31999/' \
        -e 's/cache_control_count=0/cache_control_count=3/'
}

# The MITM front-proxy seam header, as the rig spells it. Read out of the
# rig rather than restated, so the two cannot drift apart -- and the rig's
# own spelling is asserted against the Rust redaction list below.
SEAM_HEADER="$(sed -n 's/^MITM_SEAM_HEADER="\(.*\)"$/\1/p' "$RIG")"

# The same trace with the seam header ADDED to the captured ingress
# headers -- what a request that really transited the MITM listener leaves
# in the trace. The value is a synthetic nonce; the scrub gate redacts it
# by header-name class and keeps the NAME, which is what the gate reads.
with_seam_header() {
    sed 's/\(ingress request headers direction="ingress" headers=\[\)/\1["'"$SEAM_HEADER"'","d41d8cd98f00b204e9800998ecf8427e"],/'
}

# The ROUTER's capability-observation WARN for one request: a line carrying
# a `provider_kind` field spelled in the CONFIG ENTRY vocabulary
# (`ProviderEntry::kind_str()`), from a different emitter than the log_safe
# traces that carry the PROVIDER vocabulary. Both spellings are correct at
# their own site -- `PROVIDER_KIND` names the egress implementation,
# `kind_str()` names the config variant that selected it -- and the field
# name being shared is exactly what made an unqualified harvest read two
# vocabularies as one.
#
# The line shape is derived from the emitter, not copied from any capture:
# a WARN, the router target, the observation message, and the observation's
# own fields. Every value is synthetic. `$2` is the config spelling the
# WARN carries; `$3` is the capability key, so a caller can drive the
# measured shape of two acting observations per request.
capability_observe_line() {
    local id="$1" config_kind="${2:-anthropic-api}" key="${3:-prompt_caching}"
    local frac="${4:-250000}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    printf '%s WARN %s:messages{ingress="anthropic"}:complete_with_options{alias=my-alias}: routectl_router::router::capability_observe: response-evidence capability observation acted event="observe" state_key=synthetic-state capability_key="%s" provider_kind="%s" evidence_class="cache_hit" direction="verified" signal_tier="self-identifying" source="live"\n' \
        "2026-08-25T10:00:00.${frac}Z" "$span" "$key" "$config_kind"
}

# Append the capability WARN AFTER the trace it is given -- the measured
# ordering of the run this fixes, where the WARN follows every log_safe
# body line and an unqualified last-wins harvest took its spelling.
after_capability_observe() {
    cat
    capability_observe_line "$1" "${2:-anthropic-api}" prompt_caching 250000
    capability_observe_line "$1" "${2:-anthropic-api}" thinking 250001
}

# The MIRROR ordering: the capability WARN comes FIRST, before any log_safe
# line. This is the ordering a first-wins harvest would resolve correctly by
# accident, so it is what proves the fix is an emitter anchor rather than a
# reordering.
before_capability_observe() {
    capability_observe_line "$1" "${2:-anthropic-api}" prompt_caching 250000
    capability_observe_line "$1" "${2:-anthropic-api}" thinking 250001
    cat
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
2026-08-25T10:00:00.110000Z TRACE $span: $target outgoing request headers direction="outgoing" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
TRACE
    structural_line "$id" ingress 400000
    structural_line "$id" outgoing 500000
}

# One CANDIDATE of a multi-request driver trace: a complete driver-shaped
# request with both its timestamps and its wire shape chosen by the caller.
#
# The two timestamps are separate parameters because they are separate
# facts and the selector's ordering basis is the difference between them:
# `$2` is when the request's `ingress request body` was logged (the
# ordering key) and `$3` is when its completion marker was (what the
# fixture records as `captured_at_ts`). Passing them in opposite orders
# across two candidates is how the ordering control drives a trace whose
# completion order contradicts its ingress order.
#
# `$4` is the shape:
#
#   baseline      a plain body, no turn list, no tools offered -- satisfies
#                 the `baseline` claim and refuses `tool-use-multiturn`
#   tools         a resent tool_use / tool_result pair over THREE turns,
#                 with a tools array on the structural line -- the reverse
#   tools-long    the same interaction one turn further on: FIVE turns, the
#                 first three byte-identical to `tools`. This is what a tool
#                 loop's next request really looks like, and it is the only
#                 shape that satisfies the claim AND is a strict
#                 CONTINUATION of `tools`
#   tools-retry   three turns like `tools` but a DIFFERENT tool call, so it
#                 satisfies the claim while carrying no more history than
#                 the candidate before it -- a retry, not a later turn
#
# `$5` and `$6` override the model and the traced provider kind, so a
# candidate can satisfy the claim while differing from the selected one in
# an identity field the continuation check reads. Every value is synthetic.
candidate_trace() {
    local id="$1" ts_ing="$2" ts_comp="$3" shape="${4:-baseline}"
    local model="${5:-claude-sonnet-4-5}" kind="${6:-anthropic}"
    local span="request{method=POST path=/v1/messages request_id=$id}"
    local target="routectl_core::log_safe:"
    local body="{\"model\":\"$model\"}"
    local tools_len=0
    # The three-turn tool loop, as one exchange. `tools-long` appends the
    # assistant's answer and the user's next prompt to exactly these turns,
    # so the continuation relation the rig checks is visible in the fixture
    # text rather than asserted about it.
    local t_user='{"role":"user","content":"list the files"}'
    local t_call='{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]}'
    local t_result='{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"notes.txt"}]}'
    case "$shape" in
        tools)
            body="{\"model\":\"$model\",\"messages\":[$t_user,$t_call,$t_result]}"
            tools_len=16
            ;;
        tools-long)
            body="{\"model\":\"$model\",\"messages\":[$t_user,$t_call,$t_result,{\"role\":\"assistant\",\"content\":\"notes.txt is the only file\"},{\"role\":\"user\",\"content\":\"now read it\"}]}"
            tools_len=16
            ;;
        tools-retry)
            body="{\"model\":\"$model\",\"messages\":[$t_user,{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_02\",\"name\":\"Bash\",\"input\":{\"command\":\"ls -a\"}}]},{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_02\",\"content\":\"notes.txt\"}]}]}"
            tools_len=16
            ;;
    esac
    cat <<TRACE
$ts_ing TRACE $span:messages{ingress="anthropic"}: $target ingress request body ingress="anthropic" body=$body redact_prompts_enabled=false
$ts_ing TRACE $span:complete_with_options{alias=my-alias}:complete{provider=$kind:p model=$model}: $target outgoing request body provider_kind="$kind" provider=p body=$body redact_prompts_enabled=false
$ts_comp TRACE $span: $target upstream success body provider_kind="$kind" provider=p body={"id":"msg_1"} redact_prompts_enabled=false
$ts_comp TRACE $span: $target egress response body ingress="anthropic" body={"id":"msg_1"} redact_prompts_enabled=false
$ts_ing TRACE $span: $target ingress request headers direction="ingress" headers=[["user-agent","claude-cli/2.1.167 (external, cli)"],["content-type","application/json"]]
$ts_ing TRACE $span: $target outgoing request headers direction="outgoing" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
$ts_comp TRACE $span: $target upstream response headers direction="upstream" provider_kind="$kind" provider=p headers=[["content-type","application/json"]]
$ts_comp TRACE $span: $target egress response headers direction="egress" headers=[["content-type","application/json"]]
TRACE
    structural_line "$id" ingress 400000 |
        sed -e "s/tools_len=0/tools_len=$tools_len/" -e "s/claude-sonnet-4-5/$model/"
    structural_line "$id" outgoing 500000 |
        sed -e "s/tools_len=0/tools_len=$tools_len/" -e "s/claude-sonnet-4-5/$model/"
}

# The request ids and timestamps the selector controls share. Two
# candidates, A initiated before B, both completing after both ingress
# bodies -- the shape one agentic turn really produces. A third id and pair
# of timestamps for the measured three-candidate shape (control 4b).
SEL_ID_A="019eab77-0000-4000-8000-0000000000a1"
SEL_ID_B="019eab77-0000-4000-8000-0000000000b2"
SEL_ID_C="019eab77-0000-4000-8000-0000000000c4"
SEL_TS_ING_A="2026-08-25T12:00:00.100000Z"
SEL_TS_ING_B="2026-08-25T12:00:01.100000Z"
SEL_TS_ING_C="2026-08-25T12:00:02.000000Z"
SEL_TS_COMP_A="2026-08-25T12:00:02.100000Z"
SEL_TS_COMP_B="2026-08-25T12:00:03.100000Z"
SEL_TS_COMP_C="2026-08-25T12:00:04.100000Z"

# A two-candidate trace: `$1` is A's shape, `$2` is B's. Written A-first,
# which is also ingress order, so an ordering-independent selector and a
# correctly-ordered one agree here -- control 8 is the case where they
# cannot.
selector_trace() {
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" "$1"
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" "$2"
}

# The MEASURED shape of the failed paid capture: three candidates in
# ingress order, shapes chosen by the caller.
selector_trace_3() {
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" "$1"
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" "$2"
    candidate_trace "$SEL_ID_C" "$SEL_TS_ING_C" "$SEL_TS_COMP_C" "$3"
}

# Length of a landed fixture's captured ingress turn list, printed so an
# assertion on WHICH request landed can read the turn count rather than a
# grep verdict. `none` when there is no landed body to count.
landed_turn_count() {
    if [ ! -f "$1/ingress_request.json" ]; then
        printf 'none\n'
        return 0
    fi
    python3 - "$1/ingress_request.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    body = json.load(handle)
print(len(body.get("messages", [])))
PY
}

# Body of a landed fixture's ingress capture, as one line.
landed_ingress_body() {
    cat "$1/ingress_request.json" 2>/dev/null | tr -d '\n'
}

# Does a landed ingress body carry the tool pair (the `tools` shape) or
# not (the `baseline` shape)? Printed as the shape name so an assertion
# reads as body IDENTITY rather than as a grep result.
landed_shape() {
    local body
    body="$(landed_ingress_body "$1")"
    if [ -z "$body" ]; then
        printf 'no-body\n'
    elif printf '%s' "$body" | grep -qF '"tool_result"'; then
        printf 'tools\n'
    else
        printf 'baseline\n'
    fi
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
    # Same for the wire-pattern predicate: the rig refuses to run without
    # it rather than promote an unverified claim.
    cp "$VERIFY_PATTERN" "$tmp/repo/scripts/drivers/lib/verify_pattern.py"
    # And for the ingress vocabulary the expected-ingress pin is validated
    # against, for the same fail-closed reason.
    cp "$INGRESS_KINDS" "$tmp/repo/scripts/drivers/lib/ingress_kinds.sh"
    # Same for the client-version comparator: without it the rig would
    # promote the client-controlled user-agent as an unchecked claim, so it
    # refuses to run rather than skip the comparison.
    cp "$CLIENT_VERSION" "$tmp/repo/scripts/drivers/lib/client_version.py"
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

# Set / clear the five driver pins in one call, so a case cannot leak a
# pin into the next one (driver mode is fail-closed on all five, so a
# leaked pin turns a refusal assertion into a silent pass). The wire
# pattern and the expected ingress default because most cases care about
# the other three; a case that needs a specific claim passes it as the
# fourth or fifth argument. Both defaults match what `trace_driver`
# produces, so a case that does not name them still promotes.
set_pins() {
    export ROUTECTL_FIXTURE_CASE_ID="$1"
    export ROUTECTL_FIXTURE_CONFIG_SHA="$2"
    export ROUTECTL_FIXTURE_CONNECTION_MODE="$3"
    export ROUTECTL_FIXTURE_WIRE_PATTERN="${4:-baseline}"
    export ROUTECTL_FIXTURE_EXPECTED_INGRESS="${5:-anthropic}"
}
clear_pins() {
    unset ROUTECTL_FIXTURE_CASE_ID ROUTECTL_FIXTURE_CONFIG_SHA \
        ROUTECTL_FIXTURE_CONNECTION_MODE ROUTECTL_FIXTURE_WIRE_PATTERN \
        ROUTECTL_FIXTURE_EXPECTED_INGRESS
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

# --- Case 2b: the provider_kind harvest is scoped to its own emitter ---
#
# TWO EMITTERS SPELL A FIELD OF THE SAME NAME IN TWO VOCABULARIES. The
# log_safe traces carry the providers-crate `PROVIDER_KIND` const
# (`anthropic`); the router's capability-observation WARN carries the config
# entry's `kind_str()` (`anthropic-api`). Neither is wrong at its own site,
# and `normalize_lane` maps only the first. An unqualified scan on the bare
# field name read both as one vocabulary, assigned unconditionally, and
# therefore took whichever line came LAST -- which handed the config
# spelling to `normalize_lane`, emptied the lane, and refused the run on
# every request where a capability observation acted.
#
# The fix is the anchor the ingress arm already had: harvest only from the
# emitter that owns the vocabulary. BOTH ORDERINGS are asserted, because
# first-wins would resolve one of them correctly by accident and is
# explicitly not the mechanism.

# PREMISE ASSERTION for the whole case. The two line kinds must really
# carry the two different spellings under the one field name, and the
# WARN must really come from a different emitter -- otherwise every
# assertion below is about a trace with only one vocabulary in it, which
# the unqualified scan would also pass.
pk_premise="$(
    pk_ls="$(trace_non_stream 019eab77-0000-4000-8000-00000000002a anthropic |
        grep -c 'routectl_core::log_safe:.*provider_kind="anthropic"')"
    pk_warn="$(capability_observe_line 019eab77-0000-4000-8000-00000000002a |
        grep -c 'routectl_router::router::capability_observe:.*provider_kind="anthropic-api"')"
    pk_warn_ls="$(capability_observe_line 019eab77-0000-4000-8000-00000000002a |
        grep -c 'routectl_core::log_safe:')"
    printf '%s %s %s\n' "$pk_ls" "$pk_warn" "$pk_warn_ls"
)"
check "the two emitters really carry two spellings of one field name" "4 1 0" \
    "$pk_premise"
unset pk_premise

# And `normalize_lane` must really have NO arm for the config spelling --
# the whole finding is that the two vocabularies are distinct, so an added
# arm would be the conflation rather than the fix. Driven through the real
# rig: a trace whose log_safe lines spell the CONFIG token leaves the lane
# empty and warns.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-00000000002b anthropic-api)")"
meta="$(captured_of "$work")/019eab77-0000-4000-8000-00000000002b/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "the config spelling is still UNMAPPED by normalize_lane" "" \
        "$(meta_get "$meta" lane)"
    check_log "and the rig says so rather than accepting it as a lane" \
        "unmapped provider_kind 'anthropic-api'" "$work/rig.log"
else
    echo "FAIL: the config-spelling capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# THE MEASURED ORDERING: the capability WARN FOLLOWS every log_safe line.
# This is the ordering the real refused run carried, and the one an
# unconditional last-wins assignment gets wrong.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-00000000002c anthropic |
    after_capability_observe 019eab77-0000-4000-8000-00000000002c)")"
meta="$(captured_of "$work")/019eab77-0000-4000-8000-00000000002c/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "a capability WARN AFTER the body lines does not displace the provider spelling" \
        "anthropic" "$(meta_get "$meta" provider_kind)"
    check "and the lane still resolves from the provider vocabulary" "anthropic-api" \
        "$(meta_get "$meta" lane)"
    if grep -q "unmapped provider_kind" "$work/rig.log"; then
        echo "FAIL: the capability WARN's spelling still reached normalize_lane"
        fails=$((fails + 1))
    else
        echo "PASS: the capability WARN's spelling never reached normalize_lane"
    fi
else
    echo "FAIL: the WARN-after capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# THE MIRROR ORDERING: the capability WARN comes FIRST. A first-wins
# harvest resolves this leg by accident, so this is the leg that says the
# fix is an emitter anchor and not a reordering -- and the leg that keeps
# holding the day an emitter moves.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-00000000002d anthropic |
    before_capability_observe 019eab77-0000-4000-8000-00000000002d)")"
meta="$(captured_of "$work")/019eab77-0000-4000-8000-00000000002d/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "a capability WARN BEFORE the body lines does not set the provider spelling either" \
        "anthropic" "$(meta_get "$meta" provider_kind)"
    check "and that lane resolves too" "anthropic-api" "$(meta_get "$meta" lane)"
else
    echo "FAIL: the WARN-before capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# PAIRED POSITIVE CONTROL: the SAME trace with no capability WARN at all
# harvests the same value. Without it both legs above pass on a rig that
# harvests nothing and reports an empty provider_kind -- and `anthropic` is
# not a value an empty harvest could produce, but the lane assertion would
# still need the control to be non-vacuous.
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-00000000002e anthropic)")"
meta="$(captured_of "$work")/019eab77-0000-4000-8000-00000000002e/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "the WARN-free trace harvests the same provider spelling" "anthropic" \
        "$(meta_get "$meta" provider_kind)"
    check "and the same lane" "anthropic-api" "$(meta_get "$meta" lane)"
else
    echo "FAIL: the WARN-free capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# THE ANCHOR IS A REAL FILTER, not merely a precedence. With the field
# REMOVED from every log_safe line, the capability WARN is the only line in
# the trace carrying it -- and the request must then not be a candidate at
# all, because in-scope selection requires a harvested provider kind. A rig
# whose arm still read the WARN would capture this request and lane it from
# the config spelling.
pk_warn_only="$(trace_non_stream 019eab77-0000-4000-8000-00000000002f anthropic |
    sed 's/ provider_kind="anthropic"//' |
    after_capability_observe 019eab77-0000-4000-8000-00000000002f)"
work="$(run_rig "$pk_warn_only")"
if [ -d "$(captured_of "$work")/019eab77-0000-4000-8000-00000000002f" ]; then
    echo "FAIL: a request whose only provider_kind is on the capability WARN was captured"
    fails=$((fails + 1))
else
    echo "PASS: a request whose only provider_kind is on the capability WARN is no candidate"
fi
rm -rf "$work"

# PREMISE ASSERTION for that filter leg: the stripped trace must really
# have lost the field from the log_safe lines and really still carry it on
# the WARN. Without it "no candidate" would pass on a trace that lost the
# field everywhere, which says nothing about the anchor.
check "the filter leg's trace carries the field ONLY on the capability WARN" "0 2" \
    "$(printf '%s\n' "$pk_warn_only" |
        awk '/routectl_core::log_safe:.*provider_kind=/ {ls++}
             /capability_observe:.*provider_kind=/ {w++}
             END {printf "%d %d\n", ls+0, w+0}')"
unset pk_warn_only

# THE EMITTER SET IS A REPLICA and must equal the log_safe call sites that
# really pass a `provider_kind` field. A replica naming a message the
# emitter does not write would silently harvest nothing; one MISSING a
# message the emitter does write would leave a real capture laneless.
# Guarded on the crates tree -- this suite also runs from a scripts-only
# checkout.
#
# `upstream error body` is deliberately excluded from the rig's set: it is
# a DEBUG line on the failure path, and a request whose upstream errored
# carries no completion marker, so it is never a candidate. It is asserted
# as an exclusion rather than left unmentioned, so the comparison below is
# an equality against a stated set.
pk_msgs_rig="$(sed -n '/^PROVIDER_KIND_MESSAGES="/,/"$/p' "$RIG" |
    sed -e 's/^PROVIDER_KIND_MESSAGES="//' -e 's/"$//' | sort | tr '\n' '|')"
check "the rig declares a non-empty provider-vocabulary message set" "1" \
    "$([ -n "$pk_msgs_rig" ] && echo 1 || echo 0)"
pk_target_rig="$(sed -n 's/^PROVIDER_KIND_TARGET="\(.*\)"$/\1/p' "$RIG")"
check "the rig's provider-vocabulary target is the log_safe module target" \
    "routectl_core::log_safe: " "$pk_target_rig"
log_safe_src="$HERE/../crates/routectl-core/src/log_safe.rs"
if [ -f "$log_safe_src" ]; then
    # Every tracing macro invocation in log_safe.rs that passes a
    # `provider_kind` field, reduced to the message it writes: the trailing
    # string literal of a `trace!`/`debug!` block, or the `message =`
    # HDR_MSG_* const the header traces name. Derived from the source rather
    # than listed, so a new emitter of the field shows up here.
    pk_msgs_rust="$(python3 - "$log_safe_src" <<'PY'
import re
import sys

src = open(sys.argv[1], encoding="utf-8").read()
consts = dict(re.findall(r'const (HDR_MSG_\w+): &str = "([^"]+)"', src))
found = set()
for block in re.findall(r'tracing::(?:trace|debug)!\((.*?)\n    \);', src, re.S):
    if not re.search(r'^\s*provider_kind\b', block, re.M):
        continue
    named = re.search(r'message = (HDR_MSG_\w+)', block)
    if named:
        found.add(consts[named.group(1)])
        continue
    literal = re.findall(r'"([^"]+)"\s*$', block.strip())
    if literal:
        found.add(literal[-1])
print("|".join(sorted(found - {"upstream error body"})) + "|")
PY
)"
    check "the rig's message replica equals the log_safe provider_kind call sites" \
        "$pk_msgs_rust" "$pk_msgs_rig"
    # And the exclusion is real: the DEBUG error-body emitter DOES pass the
    # field, so leaving it out is a decision this assertion pins rather than
    # an omission the equality above would hide.
    if grep -qF '"upstream error body"' "$log_safe_src"; then
        echo "PASS: the excluded error-body emitter really exists in log_safe"
    else
        echo "FAIL: the excluded error-body emitter is not in log_safe -- the exclusion is stale"
        fails=$((fails + 1))
    fi
    unset pk_msgs_rust
else
    echo "PASS: no crates tree in this checkout; the emitter-set weld is not asserted"
fi
unset pk_msgs_rig pk_target_rig log_safe_src


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

# The extraction must not be displaceable by the expected-ingress PIN. That
# pin is compared against the traced token, and `promote_fixture.sh` reads
# `meta.ingress_kind` back as the traced fact at the second boundary -- so a
# rig that recorded the pin instead would turn both gates into a value
# compared with itself, and the whole check would pass on every capture.
#
# Driven on the LIVE-BOX path with the pin set, because that is the only
# configuration where the two values can be made to differ: in driver mode a
# disagreement is refused before anything lands, so no landed fixture could
# ever distinguish them.
export ROUTECTL_FIXTURE_EXPECTED_INGRESS="anthropic"
work="$(run_rig "$(trace_non_stream 019eab77-0000-4000-8000-000000000006b anthropic openai-responses)")"
unset ROUTECTL_FIXTURE_EXPECTED_INGRESS
meta="$work/repo/crates/routectl-cli/tests/fixtures/captured/019eab77-0000-4000-8000-000000000006b/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "ingress_kind stays the TRACED token when a contradicting pin is set" \
        "openai-responses" "$(meta_get "$meta" ingress_kind)"
else
    echo "FAIL: the pin-vs-trace capture produced no parseable meta.json"
    fails=$((fails + 1))
fi
rm -rf "$work"

# --- Case 7: driver mode refuses on each unset pin, naming it ---------
# Five cases plus a paired control. Without the control a rig that
# refused unconditionally -- or one that refused for an unrelated reason --
# would pass all five refusal assertions.
for missing in CASE_ID CONFIG_SHA CONNECTION_MODE WIRE_PATTERN EXPECTED_INGRESS; do
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

# Paired control: the SAME trace with every pin unset captures fine
# on the unflagged live-box path, where an empty pin is honest.
clear_pins
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000007)" || rc=$?
meta="$(captured_of "$work")/019eab77-0000-4000-8000-000000000007/meta.json"
check "live-box mode tolerates every pin unset" "0" "$rc"
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
#
# The trace is the tool-loop one because the pin this case reads back is
# `tool-use-multiturn`, and the promotion gate now checks the captured
# bytes against the recorded claim.
set_pins tools-multiturn-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver_tools 019eab77-0000-4000-8000-000000000008)" \
    --driver-mode || rc=$?
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
set_pins quote-case-01 'sha"with\quote' 'mode"x' baseline
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

# The wire pattern carries no hostile value here because it can no longer
# hold one: the promotion gate resolves the recorded pin against the closed
# predicate table, so an out-of-vocabulary spelling never reaches a landed
# meta.json at all. Asserted as the refusal it is, rather than dropped.
set_pins quote-pattern-01 abc123 base-url 'wire"y'
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000001b)" --driver-mode || rc=$?
clear_pins
check "an out-of-vocabulary wire pattern is refused, not escaped into meta.json" \
    "1" "$rc"
check_log "the refusal names the unknown pattern" "no predicate for wire_pattern" \
    "$work/rig.log"
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

# --- Case 17: two requests that are not one interaction are refused ----
# One case id pins ONE interaction. Two completions in a single driver
# trace both key on the same landing path, and these two are byte-identical
# single-turn requests -- neither extends the other, so the trace holds two
# separate interactions rather than two turns of one. The refusal says so.
#
# Two requests that ARE one interaction is a different verdict entirely and
# is asserted by the redundant-match controls further down: a tool loop
# resends its pair in every later request, so a later match is the normal
# shape rather than a defect.
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
check_log "the refusal says the second request does not extend the first" \
    "A continuation extends the history" "$work/rig.log"
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

# --- Case 19b: the recorded wire pattern is ENFORCED at promotion ------
# `meta.wire_pattern` is otherwise a claim nothing reads: a case asking for
# tools only passes a permission flag to the client, so a fixture with zero
# tool calls lands, scrubs clean, and is later asserted by the replay
# harness as evidence of a shape it never carried.
#
# PREMISE ASSERTION: the refusing trace's ingress structural line must
# really contradict `baseline` and the promoting one must really exhibit
# it. Without this both legs hold for a predicate that reads neither line.
not_baseline_trace="$(trace_driver_not_baseline 019eab77-0000-4000-8000-000000000030)"
if printf '%s\n' "$not_baseline_trace" |
    grep -q 'structural summary direction="ingress".*thinking_shape=enabled:31999' &&
    printf '%s\n' "$(trace_driver 019eab77-0000-4000-8000-000000000031)" |
    grep -q 'structural summary direction="ingress".*thinking_shape=disabled'; then
    echo "PASS: the pattern-gate traces differ in the clause the claim turns on"
else
    echo "FAIL: the pattern-gate traces are not the shapes they are named for --"
    echo "FAIL: a refusal would then be asserted for something other than the claim"
    fails=$((fails + 1))
fi

set_pins pattern-mismatch-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$not_baseline_trace" --driver-mode || rc=$?
clear_pins
check "a fixture contradicting its claimed pattern refuses with exit 1" "1" "$rc"
# The message must name the PATTERN and the reason: a runner reading only
# "not promoting" cannot tell this refusal from the scrub-residue one.
check_log "the pattern refusal names the claimed pattern" "baseline" "$work/rig.log"
check_log "the pattern refusal names the clause that failed" "thinking_shape" \
    "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/pattern-mismatch-01" ]; then
    echo "FAIL: a fixture contradicting its claimed pattern reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a fixture contradicting its claimed pattern does not reach the corpus"
fi
# The staged directory is DISCARDED, not abandoned under the corpus root
# for a later run to promote.
if [ -d "$(captured_of "$work")" ] &&
    [ -n "$(find "$(captured_of "$work")" -maxdepth 1 -name '.tmp.*' -print -quit)" ]; then
    echo "FAIL: the pattern refusal left a staged tmp directory behind"
    fails=$((fails + 1))
else
    echo "PASS: the pattern refusal leaves no staged tmp directory"
fi
rm -rf "$work"
unset not_baseline_trace

# PAIRED CONTROL, mandatory: a fixture that DOES exhibit its claimed
# pattern promotes. Without it the gate is satisfiable by a predicate that
# refuses everything.
set_pins pattern-match-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000031)" --driver-mode || rc=$?
clear_pins
check "a fixture exhibiting its claimed pattern still promotes at exit 0" "0" "$rc"
if [ -f "$(captured_of "$work")/anthropic-api/pattern-match-01/meta.json" ]; then
    echo "PASS: a fixture exhibiting its claimed pattern is promoted"
else
    echo "FAIL: a matching fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# A SECOND accepted pattern, on the body-census side. The baseline control
# above rides the structural line alone, so on its own it cannot tell the
# gate apart from one that hardcodes a single predicate -- and the corpus
# commits tool fixtures next.
set_pins tools-pattern-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver_tools 019eab77-0000-4000-8000-000000000032)" \
    --driver-mode || rc=$?
clear_pins
check "a tool-loop fixture claiming tool-use-multiturn promotes at exit 0" "0" "$rc"
if [ -f "$(captured_of "$work")/anthropic-api/tools-pattern-01/meta.json" ]; then
    echo "PASS: a body-census pattern is verified off the captured body"
else
    echo "FAIL: a tool-loop fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# The same tool-loop trace claiming `baseline` is REFUSED: the pattern the
# gate reads is the recorded one, so the two legs above cannot both be
# explained by the trace alone.
set_pins tools-mislabelled-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver_tools 019eab77-0000-4000-8000-000000000033)" \
    --driver-mode || rc=$?
clear_pins
check "the SAME capture under a different claim is refused" "1" "$rc"
rm -rf "$work"

# An absent or unrunnable predicate is a HARD failure, never an unverified
# promotion -- the same fail-closed shape the rig uses for the scrub
# script. The removal is verified before the run: a delete that matched
# nothing would assert against the present-predicate path.
set_pins no-predicate-01 abc123 base-url baseline
work="$(make_repo)"
if [ -f "$work/repo/scripts/drivers/lib/verify_pattern.py" ] &&
    rm -f "$work/repo/scripts/drivers/lib/verify_pattern.py" &&
    [ ! -e "$work/repo/scripts/drivers/lib/verify_pattern.py" ]; then
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000034)" \
        --driver-mode || rc=$?
    check "an absent wire-pattern predicate refuses the run with exit 1" "1" "$rc"
    check_log "the refusal names the missing predicate" \
        "wire-pattern predicate not found" "$work/rig.log"
    if [ -d "$(captured_of "$work")/anthropic-api" ]; then
        echo "FAIL: a fixture landed with no predicate to verify its claim"
        fails=$((fails + 1))
    else
        echo "PASS: nothing lands when the predicate is absent"
    fi
else
    echo "FAIL: could not remove the predicate from the throwaway repo"
    fails=$((fails + 1))
fi
clear_pins
rm -rf "$work"

# --- Case 19c: the seam header must agree with the connection mode -----
# An environment carrier proves INTENT, not TRANSIT: a client that
# silently fell back to a direct connection would land a fixture labelled
# front-proxy whose shape is base-url, and every later cross-mode diff
# would read as client drift. The seam header is the only evidence of
# transit that no env check can provide.
#
# The rig's copy of the header name must equal the Rust redaction list's
# spelling: that list is WHY a captured header set retains the name, so a
# drifted replica would gate on a header no capture carries. Guarded on the
# Rust file's presence -- this suite also runs from a scripts-only tree.
check "the rig carries a seam header name at all" "1" \
    "$([ -n "$SEAM_HEADER" ] && echo 1 || echo 0)"
redact_list="$HERE/../crates/routectl-core/src/log_safe.rs"
if [ -f "$redact_list" ]; then
    if grep -qF "\"$SEAM_HEADER\"" "$redact_list"; then
        echo "PASS: the rig's seam header name matches the redaction list's spelling"
    else
        echo "FAIL: the rig's seam header '$SEAM_HEADER' is not in the redaction list"
        fails=$((fails + 1))
    fi
else
    echo "PASS: no crates tree in this checkout; the seam-name weld is not asserted"
fi
unset redact_list

# The rig's turn-list constants are REPLICAS of verify_pattern.py's, and the
# continuation check compares a turn count the predicate also reads. A
# drifted replica would count turns under a key the predicate does not use,
# so a redundant candidate's history length would be measured as 0 and every
# redundant match would refuse for a reason nothing in the trace caused.
for pair in "INGRESS_BODY_FILE:INGRESS_BODY_FILE" \
    "ANTHROPIC_TURNS_KEY:ANTHROPIC_TURNS_KEY" \
    "RESPONSES_TURNS_KEY:RESPONSES_TURNS_KEY"; do
    IFS=: read -r rig_name py_name <<<"$pair"
    rig_value="$(sed -n "s/^$rig_name=\"\\(.*\\)\"$/\\1/p" "$RIG")"
    py_value="$(sed -n "s/^$py_name = \"\\(.*\\)\"$/\\1/p" "$VERIFY_PATTERN")"
    check "the rig's $rig_name replica equals the predicate's" "$py_value" "$rig_value"
    check "and the $rig_name replica is non-empty" "1" \
        "$([ -n "$rig_value" ] && echo 1 || echo 0)"
done
unset rig_name py_name rig_value py_value

# PREMISE ASSERTION: the seam-bearing trace must really carry the header
# NAME in its ingress header line and the plain one must really not.
seam_trace="$(trace_driver 019eab77-0000-4000-8000-000000000035 | with_seam_header)"
if printf '%s\n' "$seam_trace" |
    grep -q "ingress request headers.*\[\"$SEAM_HEADER\"" &&
    ! printf '%s\n' "$(trace_driver 019eab77-0000-4000-8000-000000000036)" |
        grep -qF "$SEAM_HEADER"; then
    echo "PASS: the seam traces differ in exactly the header the gate reads"
else
    echo "FAIL: the seam traces are not the shapes they are named for --"
    echo "FAIL: a refusal would then be asserted for something other than the seam"
    fails=$((fails + 1))
fi

# A front-proxy fixture with NO seam header is refused.
set_pins fp-no-seam-01 abc123 front-proxy baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000036)" --driver-mode || rc=$?
clear_pins
check "a front-proxy fixture with no seam header refuses with exit 1" "1" "$rc"
check_log "the refusal says the run did not transit the MITM listener" \
    "did not transit the MITM listener" "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/fp-no-seam-01" ]; then
    echo "FAIL: a front-proxy fixture with no seam header reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a front-proxy fixture with no seam header does not reach the corpus"
fi
if [ -d "$(captured_of "$work")" ] &&
    [ -n "$(find "$(captured_of "$work")" -maxdepth 1 -name '.tmp.*' -print -quit)" ]; then
    echo "FAIL: the seam-absent refusal left a staged tmp directory behind"
    fails=$((fails + 1))
else
    echo "PASS: the seam-absent refusal leaves no staged tmp directory"
fi
rm -rf "$work"

# PAIRED CONTROL: the same case WITH the seam header promotes.
set_pins fp-seam-01 abc123 front-proxy baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$seam_trace" --driver-mode || rc=$?
clear_pins
check "a front-proxy fixture carrying the seam header promotes at exit 0" "0" "$rc"
seam_headers="$(captured_of "$work")/anthropic-api/fp-seam-01/ingress_request.headers.json"
if [ -f "$seam_headers" ] && grep -qF "$SEAM_HEADER" "$seam_headers"; then
    echo "PASS: the promoted front-proxy fixture retains the seam header name"
else
    echo "FAIL: the front-proxy fixture did not promote with its seam header"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
# The gate reads the NAME, never the value: by promotion time the scrub
# gate has replaced the nonce with its placeholder, so a value comparison
# would be testing the scrub gate instead of the fixture's provenance.
if [ -f "$seam_headers" ] && ! grep -qF "d41d8cd98f00b204e9800998ecf8427e" "$seam_headers"; then
    echo "PASS: the seam header's VALUE is redacted in the promoted fixture"
else
    echo "FAIL: the seam nonce survived into the promoted fixture"
    fails=$((fails + 1))
fi
rm -rf "$work"
unset seam_headers

# The REVERSE direction: a base-url fixture that DOES carry the seam
# header is refused. Without it the gate is satisfiable by one that only
# ever looks at front-proxy runs.
set_pins bu-seam-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$seam_trace" --driver-mode || rc=$?
clear_pins
check "a base-url fixture carrying the seam header refuses with exit 1" "1" "$rc"
check_log "the refusal says the run DID transit the MITM listener" \
    "DID transit the MITM listener" "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/bu-seam-01" ]; then
    echo "FAIL: a base-url fixture carrying the seam header reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a base-url fixture carrying the seam header does not reach the corpus"
fi
rm -rf "$work"

# PAIRED CONTROL for the reverse: base-url WITHOUT the header promotes.
# (Case 8's landing is the same shape; this one is the seam gate's own
# control, so a gate that refused every base-url run would still fail here.)
set_pins bu-no-seam-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000037)" --driver-mode || rc=$?
clear_pins
check "a base-url fixture with no seam header promotes at exit 0" "0" "$rc"
if [ -f "$(captured_of "$work")/anthropic-api/bu-no-seam-01/meta.json" ]; then
    echo "PASS: a base-url fixture with no seam header is promoted"
else
    echo "FAIL: a clean base-url fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# The match is CASE-INSENSITIVE on the name: HTTP header names are
# case-insensitive on the wire, and a client or a proxy hop may spell the
# seam header in any case. A case-sensitive gate would refuse a real
# front-proxy capture.
set_pins fp-seam-upper-01 abc123 front-proxy baseline
work="$(make_repo)"
upper_seam="$(printf '%s' "$SEAM_HEADER" | tr '[:lower:]' '[:upper:]')"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-000000000038 |
        sed "s/\(ingress request headers direction=\"ingress\" headers=\[\)/\1[\"$upper_seam\",\"nonce\"],/")" \
    --driver-mode || rc=$?
clear_pins
check "an upper-cased seam header name still satisfies a front-proxy claim" "0" "$rc"
rm -rf "$work"
unset upper_seam

# Unreadable captured ingress headers are a REFUSAL, not a pass: the mode
# claim is unprovable, and the fail-open answer would promote exactly the
# fixture whose provenance nothing could check. The trace omits the ingress
# header line entirely, so the rig writes no header file.
set_pins fp-headerless-01 abc123 front-proxy baseline
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-000000000039 |
        grep -vF 'ingress request headers')" --driver-mode || rc=$?
clear_pins
check "a fixture whose ingress headers cannot be read is refused" "1" "$rc"
check_log "the refusal says the connection mode is unprovable" "unprovable" \
    "$work/rig.log"
rm -rf "$work"
unset seam_trace

# --- Case 19d: the expected ingress must equal the TRACED dialect ------
# The failure class three new clients introduce: a client that ACCEPTS the
# runner's connection carriers and then reaches routectl on its OWN dialect
# anyway. Every environment check passes -- the environment recorded the
# intent faithfully -- and the fixture lands as evidence for a dialect it
# never carried. `meta.ingress_kind` is a TRACED fact, so the gate is a
# comparison against the run's pin.
#
# The vocabulary the pin is validated against must be derivable and must
# match the ingress adapters' own `id()` bodies: a drifted replica would
# refuse a dialect this build really parses, or accept one it does not.
# Guarded on the crates tree's presence -- this suite also runs from a
# scripts-only checkout.
check "the ingress vocabulary parses to a non-empty set" "1" \
    "$([ "${#KNOWN_INGRESS_KINDS[@]}" -gt 0 ] && echo 1 || echo 0)"
ingress_src_dir="$HERE/../crates/routectl-cli/src/ingress"
if [ -d "$ingress_src_dir" ]; then
    real_kinds="$(grep -rhA1 -- "fn id(&self) -> &'static str {" "$ingress_src_dir" |
        sed -n 's/^ *"\([a-z0-9-]\+\)" *$/\1/p' | sort -u | tr '\n' ' ')"
    declared_kinds="$(printf '%s\n' "${KNOWN_INGRESS_KINDS[@]}" | sort -u | tr '\n' ' ')"
    check "the declared ingress vocabulary equals the adapters' own id() set" \
        "$real_kinds" "$declared_kinds"
    unset real_kinds declared_kinds
else
    echo "PASS: no crates tree in this checkout; the ingress vocabulary weld is not asserted"
fi
unset ingress_src_dir

# PREMISE ASSERTION: the two traces must really differ in the ingress token
# the gate reads, and in nothing else the other gates key on. Without this
# the refusal below could be about anything.
if printf '%s\n' "$(trace_driver 019eab77-0000-4000-8000-000000000040 anthropic openai-responses)" |
    grep -q 'ingress request body ingress="openai-responses"' &&
    printf '%s\n' "$(trace_driver 019eab77-0000-4000-8000-000000000041)" |
    grep -q 'ingress request body ingress="anthropic"'; then
    echo "PASS: the ingress-gate traces differ in the traced dialect the gate reads"
else
    echo "FAIL: the ingress-gate traces are not the shapes they are named for --"
    echo "FAIL: a refusal would then be asserted for something other than the dialect"
    fails=$((fails + 1))
fi

# A capture whose TRACED dialect is not the pinned one is refused.
set_pins ingress-mismatch-01 abc123 base-url baseline anthropic
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-000000000040 anthropic openai-responses)" \
    --driver-mode || rc=$?
clear_pins
check "a capture on an unexpected ingress dialect refuses with exit 1" "1" "$rc"
# Both dialects in the message: a runner reading only "not promoting"
# cannot tell this refusal from the pattern or seam ones, and the pair is
# what says whether the client or the pin was wrong.
check_log "the refusal names the traced dialect" "openai-responses" "$work/rig.log"
check_log "the refusal names the expected dialect" "expects 'anthropic'" \
    "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/ingress-mismatch-01" ]; then
    echo "FAIL: a fixture on an unexpected dialect reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a fixture on an unexpected dialect does not reach the corpus"
fi
if [ -d "$(captured_of "$work")" ] &&
    [ -n "$(find "$(captured_of "$work")" -maxdepth 1 -name '.tmp.*' -print -quit)" ]; then
    echo "FAIL: the ingress refusal left a staged tmp directory behind"
    fails=$((fails + 1))
else
    echo "PASS: the ingress refusal leaves no staged tmp directory"
fi
rm -rf "$work"

# PAIRED CONTROL, mandatory: the same trace under the pin that MATCHES it
# promotes. Without it the gate is satisfiable by one that refuses every
# non-Anthropic capture.
set_pins ingress-match-01 abc123 base-url baseline openai-responses
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-000000000040 anthropic openai-responses)" \
    --driver-mode || rc=$?
clear_pins
check "the same capture under the matching pin promotes at exit 0" "0" "$rc"
if [ -f "$(captured_of "$work")/anthropic-api/ingress-match-01/meta.json" ]; then
    echo "PASS: a capture whose traced dialect equals its pin is promoted"
    check "the promoted fixture records the traced dialect it was gated on" \
        "openai-responses" \
        "$(meta_get "$(captured_of "$work")/anthropic-api/ingress-match-01/meta.json" ingress_kind)"
else
    echo "FAIL: a matching-dialect fixture was not promoted (rig log: $work/rig.log)"
    cat "$work/rig.log"
    fails=$((fails + 1))
fi
rm -rf "$work"

# A trace with NO extractable ingress token lands NOTHING rather than
# landing a fixture the pin cannot be checked against: the rig's in-scope
# selection requires the `ingress request body` line to NAME a dialect, so
# a request whose dialect the trace does not carry is never a candidate.
# Driver mode reports that as its zero-landing exit 3 (retryable), which is
# a different verdict from the mismatch refusal above (never retryable) --
# asserted here so the two cannot silently collapse into one.
set_pins ingress-unpinned-01 abc123 base-url baseline anthropic
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-000000000041 |
        sed 's/ingress request body ingress="anthropic"/ingress request body ingress=""/')" \
    --driver-mode || rc=$?
clear_pins
check "a capture whose ingress dialect was not traced lands nothing (exit 3)" "3" "$rc"
if [ -d "$(captured_of "$work")/anthropic-api" ]; then
    echo "FAIL: a fixture with no traced dialect reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a fixture with no traced dialect does not reach the corpus"
fi
rm -rf "$work"

# A pin OUTSIDE the vocabulary is a usage error (exit 2), refused before
# the trace is read: it can never equal a traced token, so leaving it to
# the comparison would refuse every capture with a message about the
# fixture rather than about the typo.
set_pins ingress-bogus-01 abc123 base-url baseline anthropic-api
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000042)" \
    --driver-mode || rc=$?
clear_pins
check "an expected-ingress value outside the vocabulary is a usage error" "2" "$rc"
check_log "the vocabulary refusal names the offending value" "anthropic-api" \
    "$work/rig.log"
check_log "the vocabulary refusal lists what it would have accepted" \
    "openai-responses" "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api" ]; then
    echo "FAIL: a fixture landed under an out-of-vocabulary expected ingress"
    fails=$((fails + 1))
else
    echo "PASS: nothing lands under an out-of-vocabulary expected ingress"
fi
rm -rf "$work"

# EVERY member of the vocabulary is accepted as a pin, driven end to end
# against a trace on that dialect. A single-value control could not tell
# the validator apart from one hardcoding `anthropic`, and the whole point
# of the pin is the dialects that are NOT it.
for kind in "${KNOWN_INGRESS_KINDS[@]}"; do
    set_pins "ingress-vocab-01" abc123 base-url baseline "$kind"
    work="$(make_repo)"
    rc=0
    rig_run "$work" \
        "$(trace_driver 019eab77-0000-4000-8000-000000000043 anthropic "$kind")" \
        --driver-mode || rc=$?
    clear_pins
    check "the vocabulary member '$kind' is accepted as a pin and promotes" "0" "$rc"
    rm -rf "$work"
done

# An absent ingress vocabulary is a HARD failure, never an unvalidated pin
# -- the same fail-closed shape the rig uses for the scrub script and the
# wire-pattern predicate. The removal is verified before the run: a delete
# that matched nothing would assert against the present-library path.
set_pins no-vocab-01 abc123 base-url baseline anthropic
work="$(make_repo)"
if [ -f "$work/repo/scripts/drivers/lib/ingress_kinds.sh" ] &&
    rm -f "$work/repo/scripts/drivers/lib/ingress_kinds.sh" &&
    [ ! -e "$work/repo/scripts/drivers/lib/ingress_kinds.sh" ]; then
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-000000000044)" \
        --driver-mode || rc=$?
    check "an absent ingress vocabulary refuses the run with exit 1" "1" "$rc"
    check_log "the refusal names the missing vocabulary" \
        "ingress vocabulary not found" "$work/rig.log"
    if [ -d "$(captured_of "$work")/anthropic-api" ]; then
        echo "FAIL: a fixture landed with no vocabulary to validate its pin"
        fails=$((fails + 1))
    else
        echo "PASS: nothing lands when the ingress vocabulary is absent"
    fi
else
    echo "FAIL: could not remove the ingress vocabulary from the throwaway repo"
    fails=$((fails + 1))
fi
clear_pins
rm -rf "$work"

# --- Case 19e: the two client-version statements must agree ------------
# `meta.client.version` comes from the CLIENT-CONTROLLED ingress
# user-agent; `meta.client.binary_version` is the version the driver read
# off the running binary and the runner forwarded. Two reads of one client,
# so a disagreement means the fixture is evidence about neither -- and a
# client that auto-updated mid-run is exactly what produces one.
#
# The wire value in `trace_driver` is `2.1.167`. Every leg below drives the
# binary-side pin and nothing else, so a verdict is attributable to the pair.

# The pin the runner forwards. Set alongside the four so a leg cannot leak
# it into the next one -- a leaked value would turn an absence assertion
# into a silent pass.
set_binary_version() {
    export ROUTECTL_FIXTURE_CLIENT_BINARY_VERSION="$1"
}
clear_binary_version() {
    unset ROUTECTL_FIXTURE_CLIENT_BINARY_VERSION
}

# AGREEMENT PROMOTES, and the binary-side value LANDS. The pin carries the
# decorated spelling a real binary prints while the wire carries the bare
# token, so this leg is also what proves the comparison is on tokens: a
# string comparison would refuse it and refuse every real capture.
set_pins cv-agree-01 abc123 base-url baseline
set_binary_version "2.1.167 (Claude Code)"
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000003a)" --driver-mode || rc=$?
clear_pins
clear_binary_version
check "agreeing client versions promote at exit 0" "0" "$rc"
meta="$(captured_of "$work")/anthropic-api/cv-agree-01/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "the binary-side version lands verbatim, decoration included" \
        "2.1.167 (Claude Code)" "$(meta_get "$meta" client.binary_version)"
    check "the wire version lands unchanged beside it" \
        "2.1.167" "$(meta_get "$meta" client.version)"
else
    echo "FAIL: the agreeing case produced no parseable meta.json"
    cat "$work/rig.log"
    fails=$((fails + 2))
fi
rm -rf "$work"

# DISAGREEMENT REFUSES. Same trace, same pins, only the binary-side value
# moved -- so the refusal cannot be explained by anything but the pair.
set_pins cv-disagree-01 abc123 base-url baseline
set_binary_version "2.1.246 (Claude Code)"
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000003b)" --driver-mode || rc=$?
clear_pins
clear_binary_version
check "disagreeing client versions refuse with exit 1" "1" "$rc"
check_log "the refusal names the binary-side reading" "off the binary" "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/cv-disagree-01" ]; then
    echo "FAIL: a fixture whose two client versions disagree reached the corpus"
    fails=$((fails + 1))
else
    echo "PASS: a fixture whose two client versions disagree does not reach the corpus"
fi
rm -rf "$work"

# AN ABSENT BINARY-SIDE PIN IS RECORDED AS ABSENT AND PROMOTES. This is the
# live-box shape (no binary to interrogate) and the shape of every fixture
# captured before the pin existed, so refusing it would refuse the corpus
# rather than the contradiction. The recorded field is EMPTY, never
# backfilled from the wire: a field that mirrored its counterpart could
# never contradict it, which is the whole mechanism.
set_pins cv-absent-binary-01 abc123 base-url baseline
work="$(make_repo)"
rc=0
rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000003c)" --driver-mode || rc=$?
clear_pins
check "an unset binary-side version promotes at exit 0" "0" "$rc"
meta="$(captured_of "$work")/anthropic-api/cv-absent-binary-01/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "an unset binary-side version is recorded as empty" "" \
        "$(meta_get "$meta" client.binary_version)"
    check "the wire version is still recorded, so the empty field is the new one" \
        "2.1.167" "$(meta_get "$meta" client.version)"
else
    echo "FAIL: the absent-binary-version case produced no parseable meta.json"
    cat "$work/rig.log"
    fails=$((fails + 2))
fi
rm -rf "$work"

# AN ABSENT WIRE VERSION IS RECORDED AS ABSENT AND PROMOTES, the reverse
# direction. The trace's user-agent is stripped of its version while the
# binary-side pin is populated, so the pair is unprovable rather than
# contradicted. Without this leg the gate would be satisfiable by one that
# only ever handled an absent binary side.
set_pins cv-absent-wire-01 abc123 base-url baseline
set_binary_version "2.1.167 (Claude Code)"
work="$(make_repo)"
rc=0
rig_run "$work" \
    "$(trace_driver 019eab77-0000-4000-8000-00000000003d |
        sed 's|claude-cli/2\.1\.167 (external, cli)|claude-cli|')" --driver-mode || rc=$?
clear_pins
clear_binary_version
check "an absent wire version promotes at exit 0" "0" "$rc"
meta="$(captured_of "$work")/anthropic-api/cv-absent-wire-01/meta.json"
if [ -f "$meta" ] && is_valid_json "$meta"; then
    check "an absent wire version is recorded as empty" "" \
        "$(meta_get "$meta" client.version)"
    check "the binary-side version is NOT copied into the wire field" \
        "2.1.167 (Claude Code)" "$(meta_get "$meta" client.binary_version)"
    check "the client name still parses, so the version alone went missing" \
        "claude-cli" "$(meta_get "$meta" client.name)"
else
    echo "FAIL: the absent-wire-version case produced no parseable meta.json"
    cat "$work/rig.log"
    fails=$((fails + 3))
fi
rm -rf "$work"

# An absent comparator is a HARD failure, never an unchecked promotion --
# the same fail-closed shape the wire-pattern predicate has. The removal is
# verified before the run: a delete that matched nothing would assert
# against the present-comparator path.
set_pins cv-no-comparator-01 abc123 base-url baseline
work="$(make_repo)"
if [ -f "$work/repo/scripts/drivers/lib/client_version.py" ] &&
    rm -f "$work/repo/scripts/drivers/lib/client_version.py" &&
    [ ! -e "$work/repo/scripts/drivers/lib/client_version.py" ]; then
    rc=0
    rig_run "$work" "$(trace_driver 019eab77-0000-4000-8000-00000000003e)" \
        --driver-mode || rc=$?
    check "an absent client-version comparator refuses the run with exit 1" "1" "$rc"
    check_log "the refusal names the missing comparator" \
        "client-version comparator not found" "$work/rig.log"
    if [ -d "$(captured_of "$work")/anthropic-api" ]; then
        echo "FAIL: a fixture landed with no comparator to check its versions"
        fails=$((fails + 1))
    else
        echo "PASS: nothing lands when the comparator is absent"
    fi
else
    echo "FAIL: could not remove the comparator from the throwaway repo"
    fails=$((fails + 1))
fi
clear_pins
rm -rf "$work"

# --- Case 19f: the wire pattern is a SELECTOR over the turn requests --
#
# ONE agentic turn produces SEVERAL completed upstream requests, and the
# case's recorded claim is the statement of which one it means. Before this
# the rig landed the FIRST completed request and a pattern mismatch aborted
# the run, so every tool-using case was unlandable while a single-request
# case succeeded -- the claim read as a run gate rather than as a selector.
#
# PER-REQUEST FACTS MAY SKIP, PER-RUN FACTS ABORT. Nine controls, each
# named by what would silently pass without it.

# PREMISE ASSERTION for the whole block. The two candidate shapes must
# really differ in the clause the claim turns on, and each must really
# satisfy exactly one of the two claims -- otherwise a "skipped" candidate
# proves nothing about the selector, and both directions of the mirror
# could be explained by one shape satisfying everything.
sel_premise_ok=1
if ! printf '%s\n' "$(candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" baseline)" |
    grep -q 'structural summary direction="ingress".*tools_len=0'; then
    sel_premise_ok=0
fi
if ! printf '%s\n' "$(candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools)" |
    grep -qF '"tool_result"'; then
    sel_premise_ok=0
fi
if [ "$sel_premise_ok" = 1 ]; then
    echo "PASS: the two candidate shapes differ in the clause each claim turns on"
else
    echo "FAIL: the selector candidates are not the shapes they are named for --"
    echo "FAIL: a skip would then be asserted for something other than the claim"
    fails=$((fails + 1))
fi
unset sel_premise_ok

# The ingress body a `baseline` candidate carries versus a `tools` one:
# these are the two values every body-identity assertion below reads, so
# the mapping is asserted once here rather than restated per control.
check "the shape reader tells a tools body from a baseline one" "tools baseline" \
    "$(
        d1="$(mktemp -d)" && d2="$(mktemp -d)"
        printf '{"messages":[{"content":[{"type":"tool_result"}]}]}' >"$d1/ingress_request.json"
        printf '{"model":"m"}' >"$d2/ingress_request.json"
        printf '%s %s\n' "$(landed_shape "$d1")" "$(landed_shape "$d2")"
        rm -rf "$d1" "$d2"
    )"

# CONTROL 1: first candidate FAILS the claim, second PASSES -> the run
# succeeds AND the landed body is the SECOND request's.
#
# The assertion is on BODY IDENTITY, not on the exit status. An exit-only
# assertion passes on a rig that landed the FIRST request, which is exactly
# the bug: the whole failure was a rig landing request 1 under a case whose
# claim only request 2 satisfies.
set_pins select-second-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace baseline tools)" --driver-mode || rc=$?
clear_pins
dir="$(captured_of "$work")/anthropic-api/select-second-01"
check "a run whose second candidate matches exits 0" "0" "$rc"
check "the landed body is the SECOND request's, not the first's" "tools" \
    "$(landed_shape "$dir")"
check "meta.request_id names the candidate that matched" "$SEL_ID_B" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
check "exactly one fixture lands for the case" "1" \
    "$(find "$(captured_of "$work")/anthropic-api" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')"
rm -rf "$work"

# CONTROL 2: the MIRROR -- first PASSES, second FAILS -> the FIRST lands
# and the run does not abort. Without this leg the selector could be "skip
# until the last candidate" rather than "take the matching one", and a
# trailing client side-request would poison every good capture.
set_pins select-first-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace tools baseline)" --driver-mode || rc=$?
clear_pins
dir="$(captured_of "$work")/anthropic-api/select-first-01"
check "a run whose FIRST candidate matches exits 0" "0" "$rc"
check "the landed body is the FIRST request's" "tools" "$(landed_shape "$dir")"
check "meta.request_id names the first candidate" "$SEL_ID_A" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
# A trailing non-matching request must not append a manifest line either:
# the manifest is append-only, so a spurious entry has no rewrite path.
check "the trailing non-matching candidate appends no manifest line" "1" \
    "$(wc -l <"$(captured_of "$work")/manifest.jsonl" 2>/dev/null | tr -d ' ')"
rm -rf "$work"

# CONTROL 3: two candidates, NEITHER satisfies -> the REFUSAL exit, with
# the candidate count named. Asserted in the SAME BLOCK as zero candidates
# -> the zero-landing exit, because that boundary is the thing that could
# collapse: `captured=0` used to mean only "no completed request", and it
# now splits two ways with opposite retry verdicts.
set_pins select-none-01 abc123 base-url thinking
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace baseline baseline)" --driver-mode || rc=$?
clear_pins
check "two candidates and no match is the REFUSAL exit 1, not the zero-landing 3" \
    "1" "$rc"
check_log "the refusal names how many candidates it examined" "examined 2 candidate" \
    "$work/rig.err"
check_log "the refusal names the claim none of them exhibited" "thinking" \
    "$work/rig.err"
if [ -d "$(captured_of "$work")/anthropic-api/select-none-01" ]; then
    echo "FAIL: a run whose candidates all failed still landed a fixture"
    fails=$((fails + 1))
else
    echo "PASS: a run whose candidates all failed lands no fixture"
fi
rm -rf "$work"

# The other side of that boundary, same block: ZERO candidates stays the
# retryable zero-landing exit 3 with its own message. Collapsed into one
# code, the matrix runner would retry a case defect forever or refuse a
# transient 429 permanently.
set_pins select-zero-01 abc123 base-url thinking
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(trace_no_completion 019eab77-0000-4000-8000-0000000000c3)" \
    --driver-mode || rc=$?
clear_pins
check "zero candidates stays the retryable zero-landing exit 3" "3" "$rc"
check_log "the zero-candidate message says the trace holds no completed request" \
    "holds no completed request" "$work/rig.err"
if grep -qF 'examined' "$work/rig.err"; then
    echo "FAIL: the zero-candidate run reported a candidate examination"
    fails=$((fails + 1))
else
    echo "PASS: the zero-candidate run reports no candidate examination"
fi
rm -rf "$work"

# CONTROL 4: two candidates, BOTH satisfy the claim -> the FIRST lands, the
# run SUCCEEDS, and the selection line reports one REDUNDANT candidate.
#
# A tool loop resends its `tool_use` / `tool_result` pair in every later
# request, so under a monotone claim every candidate after the first witness
# satisfies it: two matches is the NORMAL shape of a multi-turn capture, not
# an ambiguity signal. The earlier ruling refused here, and that refusal
# destroyed a correct fixture on a real paid run.
#
# Asserted on BODY IDENTITY: an exit-only assertion passes on a rig that
# landed the SECOND candidate. The second is a strict continuation carrying
# five turns against the first's three, so the turn count says which one
# filled the directory.
set_pins select-both-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace tools tools-long)" --driver-mode || rc=$?
clear_pins
dir="$(captured_of "$work")/anthropic-api/select-both-01"
check "two candidates that BOTH satisfy the claim SUCCEED" "0" "$rc"
check "the FIRST candidate's body landed, not the later witness's" "3" \
    "$(landed_turn_count "$dir")"
check "meta.request_id names the FIRST candidate" "$SEL_ID_A" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
check "the redundant witness appends no second manifest line" "1" \
    "$(wc -l <"$(captured_of "$work")/manifest.jsonl" 2>/dev/null | tr -d ' ')"
check_log "the selection line counts the later match as REDUNDANT" \
    "candidates_redundant=1" "$work/rig.out"
# And NOT as a skip: a reader seeing a skip would conclude a request failed
# the claim, when it carried the same shape one turn further on.
check_log "the redundant witness is not counted as a skip" \
    "candidates_skipped=0" "$work/rig.out"
rm -rf "$work"

# PREMISE ASSERTION for control 4. The two candidates must really both
# satisfy the claim, and the second must really be the longer one --
# otherwise "the first landed" is asserted against a trace where the second
# was skipped for failing the claim, which every rig in this file's history
# would pass.
sel_both_premise="$(
    d1="$(mktemp -d)" && d2="$(mktemp -d)"
    for pair in "$d1 tools" "$d2 tools-long"; do
        set -- $pair
        mkdir -p "$1"
        printf '%s\n' "$(candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" "$2")" |
            sed -n 's/.*ingress request body ingress="anthropic" body=\(.*\) redact_prompts_enabled=false$/\1/p' \
            >"$1/ingress_request.json"
        printf '%s\n' "$(structural_line "$SEL_ID_A" ingress 400000 | sed 's/tools_len=0/tools_len=16/')" \
            >"$1/structural.txt"
    done
    v1=refused
    v2=refused
    python3 "$VERIFY_PATTERN" "$d1" tool-use-multiturn 2>/dev/null && v1=satisfied
    python3 "$VERIFY_PATTERN" "$d2" tool-use-multiturn 2>/dev/null && v2=satisfied
    printf '%s %s %s %s\n' "$v1" "$v2" \
        "$(landed_turn_count "$d1")" "$(landed_turn_count "$d2")"
    rm -rf "$d1" "$d2"
)"
check "both control-4 candidates satisfy the claim and the second is longer" \
    "satisfied satisfied 3 5" "$sel_both_premise"
unset sel_both_premise

# CONTROL 4b: THE MEASURED SHAPE of the failed paid capture -- three
# candidates, the first failing the claim, the second and third matching.
# The SECOND's body lands, the third counts redundant, the run succeeds.
# This is the exact run that landed the correct fixture and then deleted it.
set_pins select-measured-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace_3 baseline tools tools-long)" \
    --driver-mode || rc=$?
clear_pins
dir="$(captured_of "$work")/anthropic-api/select-measured-01"
check "the measured three-candidate shape SUCCEEDS" "0" "$rc"
check "the SECOND candidate's body landed" "3" "$(landed_turn_count "$dir")"
check "meta.request_id names the second candidate" "$SEL_ID_B" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
check_log "the measured run counts one skip and one redundant witness" \
    "candidates_examined=3 candidates_skipped=1 candidates_redundant=1" "$work/rig.out"
rm -rf "$work"

# CONTROL 4c: a redundant match that is NOT a strict CONTINUATION is a
# REFUSAL and nothing lands. This is the surviving job of the old
# already-landed refusal: two requests satisfying one monotone claim is
# normal, but a trace holding two genuinely DIFFERENT interactions under one
# case id is a case the driver did not isolate, and nothing else in the
# pipeline would catch it.
#
# Four independent non-continuations are driven, one per field the check
# reads, because a mistake in any one of them would otherwise be masked by
# the others passing.
for leg in \
    "retry:tools-retry:claude-sonnet-4-5:anthropic:A continuation extends the history" \
    "model:tools-long:claude-opus-4-1:anthropic:its model differs" \
    "provider:tools-long:claude-sonnet-4-5:openai-compat:its lane differs"; do
    IFS=: read -r name shape model kind msg <<<"$leg"
    set_pins "noncont-$name-01" abc123 base-url tool-use-multiturn
    work="$(make_repo)"
    rc=0
    rig_run_split "$work" "$(
        candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
        candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" "$shape" \
            "$model" "$kind"
    )" --driver-mode || rc=$?
    clear_pins
    check "a non-continuation redundant match ($name) REFUSES with exit 1" "1" "$rc"
    check_log "the $name refusal names the field that differs" "$msg" "$work/rig.err"
    if [ -d "$(captured_of "$work")/anthropic-api/noncont-$name-01" ]; then
        echo "FAIL: the $name non-continuation left a promoted fixture"
        fails=$((fails + 1))
    else
        echo "PASS: the $name non-continuation lands nothing"
    fi
    check "the $name non-continuation appends no manifest line" "0" \
        "$([ -f "$(captured_of "$work")/manifest.jsonl" ] &&
            wc -l <"$(captured_of "$work")/manifest.jsonl" | tr -d ' ' || echo 0)"
    rm -rf "$work"
done
unset name shape model kind msg

# ACCEPT CONTROL for 4c: the SAME two-candidate shape whose second candidate
# IS a strict continuation promotes at exit 0. Without it every refusal
# above holds for a rig that refuses any two-candidate trace, and the
# continuation check would be satisfiable by a check that always says no.
set_pins noncont-accept-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace tools tools-long)" --driver-mode || rc=$?
clear_pins
check "a strict continuation of the selected candidate promotes at exit 0" "0" "$rc"
check "and the FIRST candidate's body is what landed" "3" \
    "$(landed_turn_count "$(captured_of "$work")/anthropic-api/noncont-accept-01")"
rm -rf "$work"

# CONTROL 4d: scrub residue on a REDUNDANT candidate still ABORTS fatally
# and nothing is promoted. This is the keep-scanning safety property: a
# `break` on the first match would leave every later candidate of the turn
# unscrubbed and un-`--check`ed, so the loudest safety signal in the
# pipeline would become a per-request footnote by omission rather than by
# decision.
#
# The residue rides the SECOND candidate, which satisfies the claim -- so
# the run has already made its selection when the dirty body is read, and a
# rig that stopped scanning at the selection would exit 0 having landed the
# first.
set_pins redundant-scrub-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long |
        sed 's#"now read it"#"now read /home/someoneelse/notes.txt"#'
)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
check "scrub residue on a REDUNDANT candidate is FATAL, not a redundant skip" "1" "$rc"
check_log "the fatal refusal is the scrub check" "scrub check refused" "$work/rig.err"
if [ -d "$captured/anthropic-api/redundant-scrub-01" ]; then
    echo "FAIL: residue on a redundant candidate still promoted the selection"
    fails=$((fails + 1))
else
    echo "PASS: residue on a redundant candidate promotes nothing"
fi
check "residue on a redundant candidate appends no manifest line" "0" \
    "$([ -f "$captured/manifest.jsonl" ] && wc -l <"$captured/manifest.jsonl" | tr -d ' ' || echo 0)"
rm -rf "$work"

# PREMISE ASSERTION for 4d: the residue really must ride the LATER
# candidate. Without it the refusal above holds for a rig that read the
# residue on the first candidate -- the shape control 6 already covers --
# and the keep-scanning property would be untested.
redundant_dirty_position="$(
    printf '%s\n' "$(
        candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
        candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long |
            sed 's#"now read it"#"now read /home/someoneelse/notes.txt"#'
    )" | grep -c "request_id=$SEL_ID_A.*someoneelse"
)"
check "the 4d residue rides the LATER candidate only" "0" \
    "$redundant_dirty_position"
unset redundant_dirty_position

# CONTROL 4e: a refusal from a candidate AFTER the selected one leaves no
# promoted fixture and no manifest line -- and CANNOT have deleted a fixture
# promoted earlier in the run, because promote-once means none was.
#
# This is the class the amendment found, not one instance of it. The gates
# the earlier ruling called per-run are all computed PER CANDIDATE: the seam
# check from that
# request's header capture, the expected-ingress pin from that request's
# traced dialect. So a LATER candidate can refuse for a reason the selected
# candidate passed. Driven through the expected-ingress pin, whose refusal
# code is 1 and whose input differs per candidate. (The lane was in this
# list until control 4g reclassified it as the per-request fact it is.)
#
# The structural claim is asserted directly: no `.tmp.` staging survives
# either, so the run left the landing root exactly as it found it rather
# than promoting and unwinding.
set_pins later-refusal-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long |
        sed 's/ingress="anthropic"/ingress="openai"/g'
)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
check "a refusal from a LATER candidate exits 1" "1" "$rc"
check_log "the later candidate's own dialect is what refused" \
    "was captured on ingress dialect 'openai'" "$work/rig.err"
if [ -d "$captured/anthropic-api/later-refusal-01" ]; then
    echo "FAIL: a later candidate's refusal left the selection promoted"
    fails=$((fails + 1))
else
    echo "PASS: a later candidate's refusal leaves no promoted fixture"
fi
check "a later candidate's refusal leaves no manifest line" "0" \
    "$([ -f "$captured/manifest.jsonl" ] && wc -l <"$captured/manifest.jsonl" | tr -d ' ' || echo 0)"
if [ -n "$(find "$captured" -maxdepth 1 -name '.tmp.*' -print -quit 2>/dev/null)" ]; then
    echo "FAIL: a later candidate's refusal left staged tmp state behind"
    fails=$((fails + 1))
else
    echo "PASS: a later candidate's refusal leaves no staged tmp state"
fi
# THE STRUCTURAL PROPERTY, asserted rather than inferred: the rig must never
# have created the landing path at all during the run. A promote-then-unwind
# rig leaves the LANE directory behind (mkdir -p ran before the promotion);
# a promote-once rig creates it only at the single promotion, which this run
# never reached.
if [ -d "$captured/anthropic-api" ]; then
    echo "FAIL: the run created the lane directory, so it promoted mid-scan"
    fails=$((fails + 1))
else
    echo "PASS: promote-once never created the lane directory on the refusal path"
fi
rm -rf "$work"

# PAIRED CONTROL for 4e's structural assertion: the same shape WITHOUT the
# later refusal does create the lane directory and land the fixture. Without
# it "no lane directory" passes on a rig that never creates one at all.
set_pins later-refusal-clean-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace tools tools-long)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
check "the same shape without the later refusal exits 0" "0" "$rc"
if [ -d "$captured/anthropic-api/later-refusal-clean-01" ]; then
    echo "PASS: the clean run DOES create the lane directory and land the fixture"
else
    echo "FAIL: the clean run landed nothing, so 4e's lane assertion is vacuous"
    fails=$((fails + 1))
fi
rm -rf "$work"

# CONTROL 4g: A LANE-RESOLUTION FAILURE IS A PER-REQUEST FACT. The lane
# comes from THIS request's own traced `provider_kind`, so a candidate whose
# lane will not resolve says nothing about a candidate already selected off
# its own lines. Under the earlier rig it returned a run-level code, so a
# LATER candidate's laneless kind discarded an already-gated selection: the
# stage-all/promote-once transaction meant nothing was corrupted, but the run
# still ended with no fixture when a valid one had been chosen.
#
# BOTH SIDES, because the split is the whole decision: with a selection held
# it is a SKIP and the selection promotes; with none held the laneless
# candidate IS the request the case would pin, and a fixture nothing can gate
# still fails closed.
#
# Driven through the candidate's own traced provider kind -- the CONFIG-entry
# spelling, which `normalize_lane` deliberately has no arm for -- because
# that is the input the real run carried and it differs per candidate.

# PREMISE ASSERTION: the laneless candidate must really satisfy the claim,
# so it really reaches the lane gate rather than being skipped one gate
# earlier as a pattern mismatch. Without it "the selection promoted" is
# asserted against a run where the lane gate was never reached.
lane_skip_premise="$(
    d="$(mktemp -d)"
    printf '%s\n' "$(candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" \
        tools-long claude-sonnet-4-5 anthropic-api)" |
        sed -n 's/.*ingress request body ingress="anthropic" body=\(.*\) redact_prompts_enabled=false$/\1/p' \
        >"$d/ingress_request.json"
    printf '%s\n' "$(structural_line "$SEL_ID_B" ingress 400000 | sed 's/tools_len=0/tools_len=16/')" \
        >"$d/structural.txt"
    v=refused
    python3 "$VERIFY_PATTERN" "$d" tool-use-multiturn 2>/dev/null && v=satisfied
    printf '%s\n' "$v"
    rm -rf "$d"
)"
check "the laneless candidate really satisfies the claim, so it reaches the lane gate" \
    "satisfied" "$lane_skip_premise"
unset lane_skip_premise

# THE SKIP LEG: the laneless candidate is the LATER one, so a selection is
# already held. Asserted on BODY IDENTITY, not exit status: an exit-only
# assertion passes on a rig that landed the laneless candidate.
set_pins lane-skip-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long \
        claude-sonnet-4-5 anthropic-api
)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
dir="$captured/anthropic-api/lane-skip-01"
check "a laneless LATER candidate does not discard the selection" "0" "$rc"
check "the selected candidate's body is what landed" "3" "$(landed_turn_count "$dir")"
check "meta.request_id names the selected candidate" "$SEL_ID_A" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
check "the promotion appended exactly one manifest line" "1" \
    "$(wc -l <"$captured/manifest.jsonl" 2>/dev/null | tr -d ' ')"
check_log "the skip names the candidate and why its lane is a per-request fact" \
    "skipping this" "$work/rig.err"
check_log "and the selection line counts it as a skip" \
    "candidates_examined=2 candidates_skipped=1 candidates_redundant=0" "$work/rig.out"
rm -rf "$work"

# THE REFUSAL LEG: the SAME laneless kind on the candidate that WOULD be the
# selection. Nothing is held, so the request the case would pin is the one
# that cannot be gated, and the run still refuses. A later clean candidate
# that also satisfies the claim is present, which is the point -- a rig that
# made every lane failure skippable would sail past this and land it.
set_pins lane-refuse-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools \
        claude-sonnet-4-5 anthropic-api
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long
)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
check "a laneless SELECTED candidate still REFUSES the run" "1" "$rc"
check_log "the refusal names the candidate and does not promote" \
    "not promoting the fixture" "$work/rig.err"
if [ -d "$captured/anthropic-api/lane-refuse-01" ]; then
    echo "FAIL: a laneless selected candidate promoted anyway"
    fails=$((fails + 1))
else
    echo "PASS: a laneless selected candidate promotes nothing"
fi
check "the laneless-selection refusal appends no manifest line" "0" \
    "$([ -f "$captured/manifest.jsonl" ] && wc -l <"$captured/manifest.jsonl" | tr -d ' ' || echo 0)"
rm -rf "$work"

# PREMISE ASSERTION for both legs: the driven kind must really be the one
# `normalize_lane` leaves unmapped, and the rig must really say so. Without
# it either leg could be about a kind that resolves fine, and the skip leg
# would then be asserting nothing.
set_pins lane-premise-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long \
        claude-sonnet-4-5 anthropic-api
)" --driver-mode || true
clear_pins
check_log "the driven kind is really the one normalize_lane leaves unmapped" \
    "unmapped provider_kind 'anthropic-api'" "$work/rig.err"
rm -rf "$work"

# PAIRED CONTROL for 4g: the SAME two-candidate shape with the later
# candidate's kind LANEABLE promotes the same selection at exit 0 and counts
# the later one REDUNDANT rather than skipped. Without it the skip leg holds
# for a rig that skips every second candidate, and "the selection promoted"
# would say nothing about the lane.
set_pins lane-skip-clean-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace tools tools-long)" --driver-mode || rc=$?
clear_pins
check "the same shape with a LANEABLE later candidate also exits 0" "0" "$rc"
check "and lands the same selected body" "3" \
    "$(landed_turn_count "$(captured_of "$work")/anthropic-api/lane-skip-clean-01")"
check_log "but counts the later candidate REDUNDANT, not skipped" \
    "candidates_skipped=0 candidates_redundant=1" "$work/rig.out"
rm -rf "$work"

# CONTROL 4f: the selection line carries `candidates_redundant` on BOTH the
# success and the refusal path. The success leg is asserted in control 4
# above; this is the refusal leg, where a reader most needs the count and
# where a line that only ever printed the field on success would pass.
set_pins redundant-line-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_A" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools-long
    candidate_trace "$SEL_ID_C" "$SEL_TS_ING_C" "$SEL_TS_COMP_C" tools-long |
        sed 's/ingress="anthropic"/ingress="openai"/g'
)" --driver-mode || rc=$?
clear_pins
check "the redundant-then-refused run exits 1" "1" "$rc"
sel_line="$(grep -h 'capture_fixtures: selection ' "$work/rig.out" "$work/rig.err" 2>/dev/null)"
for field in "candidates_examined=3" "candidates_skipped=0" \
    "candidates_redundant=1" "selected_request_id=$SEL_ID_A"; do
    if printf '%s' "$sel_line" | grep -qF -- "$field"; then
        echo "PASS: the refusal-path selection line carries $field"
    else
        echo "FAIL: the refusal-path selection line lacks $field -- '$sel_line'"
        fails=$((fails + 1))
    fi
done
unset sel_line
rm -rf "$work"

# CONTROL 5: THE SET -E HAZARD. Capturing write_fixture's status to enable
# the skip disables errexit for that call, so "a refusal aborts the run"
# stopped being free and became an explicit re-raise. A NON-pattern refusal
# class must still ABORT -- get this wrong and every fail-closed class in
# the rig silently becomes an ignored error.
#
# Two independent classes are driven, because the re-raise is one branch
# and a mistake in it fails them all together:
#
#   * seam/mode incoherence -- a base-url claim on a trace carrying the
#     MITM seam header;
#   * an unclassified lane -- the scrub gate holds no credential-shape
#     classification for it.
#
# Both are PER-RUN facts, so both abort rather than skip: skipping past
# them would only reach the same verdict one candidate later while
# reporting the wrong one. Each is driven with a SECOND candidate present
# that WOULD satisfy the claim, which is the whole point -- a rig that
# treated every non-zero return as skippable would sail past the refusal
# and land the other candidate at exit 0.
set_pins hazard-seam-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace baseline tools | with_seam_header)" \
    --driver-mode || rc=$?
clear_pins
check "a seam/mode refusal still ABORTS the run despite a matching later candidate" \
    "1" "$rc"
check_log "the seam refusal is the class that fired" "DID transit the MITM listener" \
    "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/hazard-seam-01" ]; then
    echo "FAIL: a seam-incoherent run landed a later candidate anyway"
    fails=$((fails + 1))
else
    echo "PASS: a seam-incoherent run lands nothing, later candidate or not"
fi
# THE SELECTION LINE ON THE REFUSAL PATH. This is the run where a reader
# most needs it -- the success path's line is useless for diagnosing a
# refusal, and reconstructing which candidate was reached by correlating
# request ids across a log is the work the line exists to remove. It must
# report the SKIP that happened before the refusal, so a line printing
# zeroes would pass a weaker assertion.
sel_line="$(grep -h 'capture_fixtures: selection ' "$work/rig.log" 2>/dev/null)"
for field in "case=hazard-seam-01" "candidates_examined=2" \
    "candidates_skipped=1" "ordering_basis=first-ingress-body"; do
    if printf '%s' "$sel_line" | grep -qF -- "$field"; then
        echo "PASS: the refusal-path selection line carries $field"
    else
        echo "FAIL: the refusal-path selection line lacks $field -- '$sel_line'"
        fails=$((fails + 1))
    fi
done
rm -rf "$work"

set_pins hazard-lane-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
if strip_shape_row "$work" anthropic-api; then
    rc=0
    rig_run "$work" "$(selector_trace baseline tools)" --driver-mode || rc=$?
    check "an unclassified-lane refusal still ABORTS despite a matching later candidate" \
        "1" "$rc"
    check_log "the unclassified-lane refusal is the class that fired" \
        "has no credential-shape classification" "$work/rig.log"
    if [ -d "$(captured_of "$work")/anthropic-api/hazard-lane-01" ]; then
        echo "FAIL: an unclassified-lane run landed a later candidate anyway"
        fails=$((fails + 1))
    else
        echo "PASS: an unclassified-lane run lands nothing, later candidate or not"
    fi
else
    echo "FAIL: could not narrow the shape table for the anthropic-api lane"
    fails=$((fails + 1))
fi
clear_pins
rm -rf "$work"

# The re-raise must also PRESERVE the code, not flatten every refusal onto
# 1: the runner maps 3 to a retryable verdict and everything else to a
# defect, so a re-raise that returned a constant would make a transient
# zero-landing look like a case defect. Driven through the class whose code
# is NOT 1: an out-of-vocabulary expected-ingress pin exits 2.
set_pins hazard-code-01 abc123 base-url tool-use-multiturn anthropic
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace baseline tools)" --driver-mode || rc=$?
clear_pins
check "the re-raise did not turn a clean two-candidate run into a refusal" "0" "$rc"
rm -rf "$work"

# CONTROL 6: scrub residue on a candidate that WOULD otherwise be skipped
# still aborts FATALLY. This is the ONE deliberate exception to the
# per-request-may-skip rule, and it is the exception because silently
# skipping past a body that carried a credential shape would turn the
# loudest safety signal in the pipeline into a per-request footnote.
#
# The residue rides the FIRST candidate under a claim that candidate does
# not satisfy -- so a rig that ran the pattern check before the scrub check,
# or that treated the scrub refusal as skippable, would skip the dirty body
# and land the clean second candidate at exit 0.
set_pins hazard-scrub-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(
    trace_driver_dirty "$SEL_ID_A"
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_B" tools
)" --driver-mode || rc=$?
clear_pins
check "scrub residue on a skippable candidate is FATAL, not skipped" "1" "$rc"
check_log "the fatal refusal is the scrub check" "scrub check refused" "$work/rig.log"
if [ -d "$(captured_of "$work")/anthropic-api/hazard-scrub-01" ]; then
    echo "FAIL: a run with scrub residue landed the clean candidate anyway"
    fails=$((fails + 1))
else
    echo "PASS: scrub residue anywhere in the trace lands nothing"
fi
rm -rf "$work"

# PAIRED CONTROL for control 6: the SAME two-candidate shape with the
# residue REMOVED promotes at exit 0 and lands the tools body. Without it
# the refusal above holds for a rig that refuses any two-candidate trace
# whose first candidate is dirty-shaped for some unrelated reason.
set_pins hazard-scrub-clean-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace baseline tools)" --driver-mode || rc=$?
clear_pins
check "the same shape without the residue promotes at exit 0" "0" "$rc"
check "and lands the matching candidate's body" "tools" \
    "$(landed_shape "$(captured_of "$work")/anthropic-api/hazard-scrub-clean-01")"
rm -rf "$work"

# CONTROL 7: a run ending in a REFUSAL leaves NO promoted fixture and no
# manifest line. The pre-selector refusal was not transactional: the first
# request was promoted and appended to the manifest before the second was
# even examined, so the abort left the WRONG fixture in scratch. Under the
# selector a non-matching candidate never promotes, but the property is
# asserted rather than assumed.
#
# Driven through a refusal from a candidate whose REFUSAL CLASS is the one
# the earlier rig raised only after a promotion had happened: the second
# candidate satisfies the claim but is not a continuation, so the run has
# made its selection and staged it before the refusal fires. That is the only
# shape where a promoted fixture could survive a refusal at all -- and under
# promote-once the selection is still staged rather than promoted, which is
# why nothing survives.
set_pins refusal-clean-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(selector_trace tools tools-retry)" --driver-mode || rc=$?
clear_pins
captured="$(captured_of "$work")"
check "a run ending in refusal exits 1" "1" "$rc"
if [ -d "$captured/anthropic-api/refusal-clean-01" ]; then
    echo "FAIL: a run ending in refusal left a promoted fixture behind"
    find "$captured" -mindepth 1 -maxdepth 3 | sed -n '1,10p'
    fails=$((fails + 1))
else
    echo "PASS: a run ending in refusal leaves no promoted fixture"
fi
check "a run ending in refusal leaves no manifest line" "0" \
    "$([ -f "$captured/manifest.jsonl" ] && wc -l <"$captured/manifest.jsonl" | tr -d ' ' || echo 0)"
if [ -d "$captured" ] &&
    [ -n "$(find "$captured" -maxdepth 1 -name '.tmp.*' -print -quit)" ]; then
    echo "FAIL: a run ending in refusal left a staged tmp directory behind"
    fails=$((fails + 1))
else
    echo "PASS: a run ending in refusal leaves no staged tmp directory"
fi
rm -rf "$work"

# CONTROL 8: THE ORDERING BASIS. Two candidates whose COMPLETION markers
# arrive in the OPPOSITE order from their ingress bodies must be selected
# by INGRESS order.
#
# `in_scope_ids` used to key each request on its completion marker's
# timestamp and sort on that, so stream flush timing could reorder
# candidates even under a deterministic client -- and a selector over a
# nondeterministic order is a nondeterministic selector. Without this
# control the ordering fix is untested, because every other trace in this
# file has the two orders agreeing.
#
# A is initiated first and completes LAST; B is initiated second and
# completes FIRST. Both satisfy the claim and NEITHER extends the other --
# they are two three-turn exchanges with different tool calls -- so whichever
# is considered second is refused as a non-continuation, and the refusal
# names it. Ordering is therefore the only thing that can decide which id
# appears in the message.
sel_swap_trace="$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_B" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_A" tools-retry
)"

# PREMISE ASSERTION: the two orders must really contradict. Without this
# the selection below could be asserted against a trace where both bases
# agree, and the completion-keyed rig would pass it too.
sel_ing_order="$(printf '%s\n' "$sel_swap_trace" |
    grep 'ingress request body' | awk '{print $1}' | tr '\n' ' ')"
sel_comp_order="$(printf '%s\n' "$sel_swap_trace" |
    grep 'upstream success body' | awk '{print $1}' | tr '\n' ' ')"
if [ "$sel_ing_order" = "$SEL_TS_ING_A $SEL_TS_ING_B " ] &&
    [ "$sel_comp_order" = "$SEL_TS_COMP_B $SEL_TS_COMP_A " ]; then
    echo "PASS: the ordering trace's ingress and completion orders really contradict"
else
    echo "FAIL: the ordering trace's two orders do not contradict --"
    echo "FAIL: a completion-keyed selector would pass this case too"
    fails=$((fails + 1))
fi
unset sel_ing_order sel_comp_order

# The candidate considered SECOND is the one the non-continuation refusal
# names, which is what says which one was considered first. Ingress order
# puts A first, so B must be the one named.
set_pins order-basis-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$sel_swap_trace" --driver-mode || rc=$?
clear_pins
check "the contradicting-order run refuses the non-continuation" "1" "$rc"
check_log "candidates are ordered by INGRESS body: the later-initiated one is refused" \
    "capture_fixtures: $SEL_ID_B also exhibits" "$work/rig.log"
if grep -qF "capture_fixtures: $SEL_ID_A also exhibits" "$work/rig.log"; then
    echo "FAIL: the earlier-INITIATED request was refused, so the order keyed on completion"
    fails=$((fails + 1))
else
    echo "PASS: the earlier-initiated request was considered first"
fi
rm -rf "$work"

# And the same ordering read through a LANDING rather than a refusal: with
# only the earlier-initiated candidate satisfying the claim, the run must
# land THAT body even though the other one completed first. The refusal leg
# above says which was considered first; this one says the selection
# actually follows it.
set_pins order-landing-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run "$work" "$(
    candidate_trace "$SEL_ID_A" "$SEL_TS_ING_A" "$SEL_TS_COMP_B" tools
    candidate_trace "$SEL_ID_B" "$SEL_TS_ING_B" "$SEL_TS_COMP_A" baseline
)" --driver-mode || rc=$?
clear_pins
dir="$(captured_of "$work")/anthropic-api/order-landing-01"
check "the earlier-initiated candidate lands though the other completed first" "0" "$rc"
check "and the landed request is the earlier-INITIATED one" "$SEL_ID_A" \
    "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" request_id)"
# `captured_at_ts` stays the COMPLETION timestamp: the ordering key is the
# ingress body, but what the fixture records about itself is when the
# request finished. Conflating the two would rewrite every fixture's clock.
check "captured_at_ts is still the completion timestamp, not the ordering key" \
    "$SEL_TS_COMP_B" "$([ -f "$dir/meta.json" ] && meta_get "$dir/meta.json" captured_at_ts)"
rm -rf "$work"
unset sel_swap_trace

# THE SELECTION LINE. A reviewer reading a committed fixture cannot
# otherwise tell WHICH request of an agentic turn they are looking at, and
# the run is the only place that fact exists. One structured line, and it
# carries NO body content -- a rig log is a CI artifact.
set_pins selection-line-01 abc123 base-url tool-use-multiturn
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace baseline tools)" --driver-mode || rc=$?
clear_pins
check "the selection-line run promotes" "0" "$rc"
sel_line="$(grep -h 'capture_fixtures: selection ' "$work/rig.out" "$work/rig.err" 2>/dev/null)"
check "exactly ONE selection line is emitted" "1" \
    "$(printf '%s\n' "$sel_line" | grep -c 'selection ' || true)"
for field in "case=selection-line-01" "selected_request_id=$SEL_ID_B" \
    "candidates_examined=2" "candidates_skipped=1" "candidates_redundant=0" \
    "ordering_basis=first-ingress-body"; do
    if printf '%s' "$sel_line" | grep -qF -- "$field"; then
        echo "PASS: the selection line carries $field"
    else
        echo "FAIL: the selection line does not carry $field -- '$sel_line'"
        fails=$((fails + 1))
    fi
done
# NO BODY CONTENT, in either stream. The bodies are unscrubbed at the point
# a candidate is examined, and the model id is the shortest string every
# candidate body in this suite carries.
for stream in rig.out rig.err; do
    if grep -qF 'tool_result' "$work/$stream" 2>/dev/null ||
        grep -qF '"messages"' "$work/$stream" 2>/dev/null; then
        echo "FAIL: the rig's $stream carries fixture body content"
        fails=$((fails + 1))
    else
        echo "PASS: the rig's $stream carries no fixture body content"
    fi
done
unset sel_line
rm -rf "$work"

# The selection line is DRIVER MODE ONLY: a live-box capture has no case
# claim to select on, so a line reporting a selection would name a decision
# nothing made.
clear_pins
work="$(make_repo)"
rc=0
rig_run_split "$work" "$(selector_trace baseline tools)" || rc=$?
check "a live-box run over the same trace exits 0" "0" "$rc"
if grep -qh 'capture_fixtures: selection ' "$work/rig.out" "$work/rig.err" 2>/dev/null; then
    echo "FAIL: live-box mode emitted a selection line"
    fails=$((fails + 1))
else
    echo "PASS: live-box mode emits no selection line"
fi
# And live-box mode still captures BOTH requests: it keys on request_id, so
# the selector's one-of-several logic must not have narrowed it.
check "live-box mode still captures every completed request" "2" \
    "$(find "$(captured_of "$work")" -mindepth 1 -maxdepth 1 -type d ! -name '.tmp.*' | wc -l | tr -d ' ')"
rm -rf "$work"

# CONTROL 9: the committed `plain-turn-01` -- ONE candidate, one claim --
# is byte-unaffected. The selector changed which BODY fills a landing
# directory when several candidates exist; with one candidate there is
# nothing to select, and the committed fixture must still adjudicate PASS
# against its own recorded claim.
#
# Guarded on the fixture's presence: this suite also runs from a
# scripts-only tree.
committed_fixture="$HERE/../crates/routectl-cli/tests/fixtures/driver/anthropic-api/plain-turn-01"
if [ -d "$committed_fixture" ]; then
    committed_pattern="$(meta_get "$committed_fixture/meta.json" wire_pattern)"
    check "the committed fixture records a claim to adjudicate" "1" \
        "$([ -n "$committed_pattern" ] && echo 1 || echo 0)"
    if python3 "$VERIFY_PATTERN" "$committed_fixture" "$committed_pattern" 2>"$HERE/.sel-verify.err"; then
        echo "PASS: the committed plain-turn-01 still adjudicates PASS on its own claim"
    else
        echo "FAIL: the committed plain-turn-01 no longer exhibits its recorded claim"
        sed -n '1,5p' "$HERE/.sel-verify.err"
        fails=$((fails + 1))
    fi
    rm -f "$HERE/.sel-verify.err"
    # A single-candidate driver run still lands, still counts ONE candidate,
    # and still reports zero skipped -- the selector adds no skip where
    # there is nothing to skip.
    set_pins plain-turn-01 abc123 base-url baseline
    work="$(make_repo)"
    rc=0
    rig_run_split "$work" "$(trace_driver 019eab77-0000-4000-8000-0000000000c9)" \
        --driver-mode || rc=$?
    clear_pins
    check "a single-candidate driver run still promotes at exit 0" "0" "$rc"
    check_log "the single-candidate run examined exactly one candidate" \
        "candidates_examined=1 candidates_skipped=0" "$work/rig.out"
    rm -rf "$work"
    unset committed_pattern
else
    echo "PASS: no committed driver fixture in this checkout; control 9 is not asserted"
fi
unset committed_fixture

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
