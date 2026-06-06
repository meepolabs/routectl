# Logging

This document covers routectl's logging surface: env filter, default
level, recipes, request correlation, triage modes for full body
inspection, and the prompt-redaction knob. For TOML configuration of
providers, models, and the runtime, see [CONFIGURATION.md](CONFIGURATION.md).

## Env filter and default level

routectl uses `tracing` with the env filter `ROUTECTL_LOG` (NOT the
default `RUST_LOG`, since we don't want stray `RUST_LOG=debug` exports
turning routectl into a firehose).

Default level is `info`. Every log line carries the module path
(`routectl_router::router`, `routectl_providers::bedrock`, etc.) and,
inside an HTTP request, the `request_id` field for correlation across
fallback hops.

## Recipes

```bash
# Full debug across all routectl crates.
ROUTECTL_LOG=routectl=debug,routectl_providers=debug,routectl_router=debug \
  ./routectl serve

# Bedrock-only deep dive (SigV4 inputs + eventstream frames).
ROUTECTL_LOG=routectl=info,routectl_providers::bedrock=trace ./routectl serve

# Auth tracing only (secret resolution + credential failures + listener
# rejections + upstream 401/403).
ROUTECTL_LOG=routectl_auth=warn,routectl_providers::bedrock::auth=warn,\
routectl_providers::bedrock::signing=warn,\
routectl_cli::server::auth=warn ./routectl serve

# Quiet -- only warnings and errors.
ROUTECTL_LOG=warn ./routectl serve
```

## Request correlation

Every request gets a `request_id`. Either supply your own via the
`x-request-id` header (echoed back on the response so your client logs
match) or routectl mints a `Uuid::now_v7()` (sortable by time). All
log lines emitted while processing the request inherit `request_id` as
a span field, so:

```bash
ROUTECTL_LOG=info ./routectl serve 2>&1 | grep request_id=probe-1
```

shows every event for one specific request: ingress parse, alias
resolution, fallback hops, retry attempts, upstream calls, response
shape, errors.

## Triage recipes (full bodies on demand)

When `body_excerpt=...` in a WARN line isn't enough -- typically when an
upstream returns a generic `400 "request not valid"` and you need to
see WHICH wire field tripped it -- flip the log level. The output
includes ingress body, outgoing egress body, and the full upstream
error body, all carrying the same `request_id` so a single grep
correlates them:

```bash
# Full upstream error bodies (4 KB cap, debug):
ROUTECTL_LOG=routectl=debug ./routectl serve

# Also outgoing + ingress bodies (16 KB cap, trace):
ROUTECTL_LOG=routectl=trace ./routectl serve

# Trace one specific request end-to-end:
ROUTECTL_LOG=routectl=trace ./routectl serve 2>&1 | grep request_id=<id>

# Which Bedrock-Invoke beta flags are getting filtered (operator
# suspects AWS allowlist drift):
ROUTECTL_LOG=routectl_providers::bedrock=debug ./routectl serve 2>&1 \
  | grep "dropping beta flag"
```

What you get at debug:
- Existing `body_excerpt=...` WARN on every 4xx/5xx (512-char
  truncated; greppable from any tracing subscriber that records
  WARN-level events)
- New `body=...` DEBUG with the full upstream error body (4 KB cap,
  HTML-collapsed)

What you get at trace, additionally (full 4-direction visibility):
- `body=...` TRACE `"ingress request body"` -- the body the client
  sent on `/v1/chat/completions` or `/v1/messages` (16 KB cap, fields
  `ingress=openai|anthropic`).
- `body=...` TRACE `"outgoing request body"` -- the JSON body routectl
  sent to the upstream (16 KB cap, fields
  `provider_kind=openai-compat|anthropic|bedrock-invoke|bedrock-converse|openai-responses`,
  `provider=<id>`).
- `body=...` TRACE `"upstream success body"` -- the deserialized 2xx
  body the upstream returned, traced BEFORE routectl's normalization
  rewrites it (16 KB cap; same `provider_kind` / `provider` fields).
  4xx/5xx error bodies stay on the existing DEBUG path.
- `body=...` TRACE `"egress response body"` -- what the client
  actually receives, traced AFTER canonical -> wire serialization
  (16 KB cap, field `ingress=openai|anthropic`). Single call site in
  `routectl-cli/src/handlers/ingress_handle.rs` covers both ingresses.
- TRACE `"stream summary"` lines on streaming completion: one per
  direction (`direction=upstream` from the provider-side wrapper,
  `direction=egress` from the ingress-side render loop). Carries
  `chunks=<N>`, `finish_reason=<...>`, `prompt_tokens`,
  `completion_tokens`, `total_tokens`. Streams DO NOT emit per-chunk
  body traces -- the per-chunk firehose floods the log without adding
  signal beyond the summary.

Sensitivity caveat: bodies contain user prompts AND assistant outputs
at TRACE. Leave `ROUTECTL_LOG` at the default `info` level in
production. Only flip to debug/trace during active triage and prefer
redirecting the output to a file (`./routectl serve 2>/tmp/triage.log`)
rather than tailing live.

## Redaction

For sensitive environments where TRACE is needed but raw prompts are
not OK to disk, set `ROUTECTL_LOG_REDACT_PROMPTS=1` BEFORE launching
routectl. The redactor walks every traced body and replaces known
prompt-bearing fields (text blocks, system, instructions, tool_use
input, function_call arguments, refusal blocks, image source data,
image_url data URIs, Bedrock Converse `toolUse.input` and
`toolResult.content[*].json`) with `<redacted len=N>` placeholders
while preserving structural fields (model, tools, sampling params,
finish_reason, usage). Best-effort: unknown wire shapes (a new
Anthropic content-block type, a new OpenAI Responses output kind)
can still leak. The env var is read once on first traced body --
flipping it after the first trace fires has no effect; the server
emits a one-shot `info` line at boot reporting the resolved value
(`redact_prompts=true|false`) so operators can confirm the setting
took effect.

Two known residual leaks even with the knob ON:
- `<redacted len=N>` reveals the char count of the original content.
  Short fixed-vocabulary prompts (e.g. "yes" / "no" tool confirms)
  are disambiguable by length alone. Treat redacted traces as a
  length-leaking side channel.
- The 4xx/5xx upstream error body (`debug_upstream_error_body` at
  DEBUG level) is NOT redacted -- error bodies are raw strings, not
  JSON values; they may echo back portions of the request. Operators
  flipping DEBUG (not TRACE) for triage on a sensitive environment
  should be aware.

```bash
# Redacted triage. All four trace directions still fire; user content
# replaced with `<redacted len=N>` markers; model/tools/usage/
# finish_reason intact for diagnosis.
ROUTECTL_LOG=routectl=trace ROUTECTL_LOG_REDACT_PROMPTS=1 \
  ./routectl serve 2>/tmp/triage.log
```

## Trace-level surfaces

Operator grep cheat sheet:

| Surface | Direction | Filter |
|---|---|---|
| `"ingress request body"`           | 1 client -> routectl     | `tag:ingress request_id=<id>` |
| `"outgoing request body"`          | 2 routectl -> upstream   | `provider_kind=<kind>` |
| `"upstream success body"`          | 3 upstream -> routectl   | `provider_kind=<kind>` |
| `"egress response body"`           | 4 routectl -> client     | `ingress=<openai\|anthropic>` |
| `"structural summary"`             | 1 + 2 (request-side only) | `direction=ingress\|outgoing` |
| `"stream summary"` `direction=upstream` | provider-side stream end | `chunks=`, `finish_reason=` |
| `"stream summary"` `direction=egress`   | ingress-side stream end  | `chunks=`, `finish_reason=` |

The `"structural summary"` line fires on every REQUEST-side body
(directions 1 and 2 only -- response bodies are not summarized). It
carries a stable set of prompt-content-free fields so the operator's
smart-heartbeat validator can grep wire-shape invariants (`model=`,
`max_tokens=`, `thinking_shape=`, `output_config_effort=`,
`tool_choice_shape=`, `cache_control_count=`, `messages_len=`,
`tools_len=`, `anthropic_beta=`, `provider_extras_keys=`, `stream=`)
without fighting the 16 KB body cap that truncates fields appearing
after a large messages array. Field-name stability: adding a new
field is allowed without ceremony; renaming or removing an existing
field requires updating this table.

### Header trace redaction policy

When `ROUTECTL_TRACE_HEADERS=1` is set, the four `trace_*_headers`
emitters apply DIFFERENT redaction per direction by design:

| Direction | Headers emitted | Redaction |
|-----------|-----------------|-----------|
| 1 -- ingress request (client -> routectl) | RAW, no redaction | None -- fixture captures need the real auth/beta/version headers the client sent |
| 2 -- outgoing request (routectl -> upstream) | Redacted | `authorization`, `x-api-key`, and `proxy-authorization` values are replaced with `[REDACTED: Bearer JWT]` / `[REDACTED: api key]` etc. so live tokens never land in log archives |
| 3 -- upstream response (upstream -> routectl) | RAW, no redaction | None -- response headers carry no outgoing secrets |
| 4 -- egress response (routectl -> client) | RAW, no redaction | None -- egress headers carry no secrets |

This is intentional. Direction 2 is the only direction that carries
outgoing auth material (Bearer JWTs, api keys). The remaining three
directions are raw so fixture-capture and triage workflows see the
exact wire values without workarounds.

**Treat TRACE logs as sensitive.** Even with direction-2 redaction,
directions 1, 3, and 4 emit header values verbatim. Any listener
auth tokens the caller sends (direction 1) or beta flags that double
as capability indicators appear in the trace log. Restrict log-archive
access accordingly and avoid leaving `ROUTECTL_TRACE_HEADERS=1` on in
long-running production processes.

## Anthropic SSE forward-compat observability

routectl's Anthropic-API egress sink-drains unknown SSE block / delta
/ event types and (when in budget) preserves their wire bytes through
the canonical pipeline for the matching Anthropic ingress to re-emit
verbatim. The capture is bounded per block at 256 KB total bytes
and 10000 deltas; once either cap trips, the block degrades to
sink-drain for the rest of its life and the canonical stream keeps
flowing. The five log emission sites give operators visibility into
what's being passed through, dropped, or capped. A typed delta that
arrives inside an `Unknown` block is captured opaquely (it surfaces
through `record_delta`'s "captured opaque delta" DEBUG line, like any
other opaque delta) rather than sink-drained, so it has no separate
log site.

| Site | Level | Fields |
|---|---|---|
| Unknown block opened (`sse_unknown::open_unknown_block`) | WARN | `provider`, `upstream_index`, `block_type`, `mode="v2_capture"` |
| Index mismatch (`sse_unknown::index_matches`) | WARN | `provider`, `expected_index`, `got_index`, `event_kind`, `open_block_type` |
| Per-delta capture (`sse_opaque::record_delta`) | DEBUG | `provider`, `upstream_index`, `delta_bytes` |
| Block stop summary (`sse_opaque::record_stop`) | INFO | `provider`, `upstream_index`, `block_type`, `captured_bytes`, `delta_count` |
| Cap exceeded / degrade (`sse_opaque::degrade`) | WARN | `provider`, `upstream_index`, `block_type`, `reason`, `captured_bytes`, `delta_count` |

Example lines (formatted for readability; real output is one event
per line and inherits the `request_id` span field):

```
WARN routectl_providers::anthropic_api::sse_unknown
  provider=anthropic-prod upstream_index=1 block_type=server_tool_use
  mode=v2_capture
  "anthropic SSE: opening forward-compat opaque content block"

WARN routectl_providers::anthropic_api::sse_unknown
  provider=anthropic-prod expected_index=0 got_index=1
  event_kind=delta open_block_type=text
  "anthropic SSE: content-block index mismatch; dropping misattributed event"

DEBUG routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 delta_bytes=312
  "anthropic SSE: captured opaque delta"

INFO routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 block_type=web_search_tool_result
  captured_bytes=2048 delta_count=4
  "anthropic SSE: opaque block closed"

WARN routectl_providers::anthropic_api::sse_opaque
  provider=anthropic-prod upstream_index=1 block_type=web_search_tool_result
  reason=byte_overflow captured_bytes=261888 delta_count=287
  "anthropic SSE: opaque-capture cap exceeded; degrading block to sink-drain"
```

`reason` on the degrade WARN is one of `byte_overflow` or
`delta_overflow` -- pin on this field in alerts, not on the message
string.

DEBUG-level logs are off by default; enable with
`ROUTECTL_LOG=routectl=debug` to see per-delta opaque-capture
detail. The INFO block-stop summary fires at the
default level, so operators routinely see one summary per
unknown block in production.

## Auth-failure log shapes

No secret values, ever:

| Surface | Log line |
|---|---|
| Listener auth (wrong `x-api-key` / `Bearer`) | `WARN routectl_cli::server::auth has_x_api_key=<bool> has_bearer=<bool> route=<path> "listener auth rejected"` |
| Bad secret ref (`env://NONEXISTENT`) | `WARN routectl_auth::memory_store scheme=env:// var=<NAME> reason="not set" "secret resolution failed"` |
| Bad secret ref (file perm too open) | `WARN routectl_auth::memory_store scheme=file:// path=<P> mode=<oct> reason="group/other readable; chmod 600 or 400" "secret resolution failed"` |
| Bedrock SigV4 / cred chain failed | `WARN routectl_providers::bedrock::auth variant=Profile\|DefaultChain region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock SigV4 sign failure | `ERROR routectl_providers::bedrock::signing failure_kind=<kind> ... "bedrock auth failed"` -- where `<kind>` is one of `bearer_header_invalid`, `creds_unavailable`, `body_unbuffered`, `signing_params_build`, `non_ascii_header`, `signable_request_build`, `sigv4_sign`, `signed_header_name_invalid`, `signed_header_value_invalid`, `unexpected_query_params` |
| Bedrock 403 (IAM denied) | `WARN routectl_providers::bedrock provider=<id> status=403 action=<bedrock-runtime:InvokeModel...> principal_present=<bool> "bedrock IAM access denied"` -- `action` extracted from the AWS error body so you immediately see WHICH IAM action your role lacks |
| Bedrock in-stream auth event | `WARN routectl_providers::bedrock::eventstream provider=<id> event_type=accessDeniedException\|unauthorizedException\|authentication_error\|permission_error message=... "bedrock in-stream auth/permission exception"` |
| Anthropic upstream 401/403 | `WARN routectl_providers::anthropic_api provider=<id> status=<401\|403> auth_kind=<ApiKey\|OauthBearer> message=... "anthropic upstream auth failed"` |
| OpenAI-compat upstream 401/403 | `WARN routectl_providers::openai_compat provider=<id> status=<401\|403> body_excerpt=... "openai-compat upstream auth failed"` |

## What's never logged

- Resolved secret values (env contents, file contents, OAuth tokens,
  bearer keys, AWS access/secret keys).
- The supplied `x-api-key` / `Authorization: Bearer` value on a
  rejected listener auth (we log only header presence).
- Full upstream request/response bodies. Bodies are only excerpted to
  512 chars on 4xx/5xx upstream paths, intentionally. Full body
  inspection is available at trace level -- see the Triage recipes
  section above.
