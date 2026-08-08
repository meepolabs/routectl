# Logging

This document covers routectl's logging surface: env filter, default
level, recipes, request correlation, triage modes for full body
inspection, and the prompt-redaction knob. For TOML configuration of
providers, models, and the runtime, see [CONFIGURATION.md](CONFIGURATION.md).

The first half is for operators (filtering, triage, redaction); the
"Event catalog" second half is the per-event field reference.

- [Env filter and default level](#env-filter-and-default-level)
- [Recipes](#recipes)
- [Request correlation](#request-correlation)
- [Triage recipes (full bodies on demand)](#triage-recipes-full-bodies-on-demand)
- [Redaction](#redaction)
- [What's never logged](#whats-never-logged)
- [Trace-level surfaces](#trace-level-surfaces)
- Event catalog:
  [SSE forward-compat](#anthropic-sse-forward-compat-observability),
  [auth failures](#auth-failure-log-shapes),
  [usage accounting](#usage-accounting-log-shapes),
  [config-edit audit](#config-edit-audit-shape),
  [prompt-cache auto-emission](#prompt-cache-auto-emission-log-shapes),
  [activation inventory](#auto-activation-inventory-audit-events),
  [context reduction](#context-reduction-log-shapes),
  [stream first-activity](#stream-first-activity-mark),
  [capability intelligence](#capability-intelligence-events)

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
- Existing `body_excerpt=...` WARN on every 4xx/5xx (256-char
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

The 16 KB trace body cap is the DEFAULT. Override it with
`ROUTECTL_TRACE_BODY_BYTES=<N>` (env, read once at first trace) or
`[log] trace_body_bytes = <N>` (config, applied at startup). The
resolved value is announced at boot. The 4 KB debug excerpt cap is fixed.

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
image_url data URIs, reasoning-replay carry blobs
(`encrypted_content` and the `redacted_thinking` `data` blob), Bedrock
Converse `toolUse.input` and
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

### Reasoning-replay degradation WARN

When a request carries reasoning-replay artifacts onto a lane that
rejects them, the router strips them and re-dispatches the same target
once (the fixed strip-repair branch). Each such request emits EXACTLY
ONE aggregated WARN at resolution -- `"reasoning_replay_degraded"` -- and
none when nothing degraded. The line carries a CLOSED SET of tokens
only, never the artifact bytes, a reasoning item id, any hash/digest,
the session key, or the upstream body:

| Field | Meaning |
|---|---|
| `action` | What the router did (`strip_repair`) |
| `target_lane` | The lane stripped against (`codex` / `mantle` / `gray`) |
| `state_key` | Sanitized `[providers]` state key of the repaired target |
| `source_schemes` | Distinct source schemes of the stripped artifacts, comma-joined |
| `reason` | Why the strip fired (`upstream_replay_rejection`) |
| `artifact_count` | Count of non-portable artifacts stripped |
| `repair_attempted` | The strip-repair branch fired |
| `repair_succeeded` | The stripped re-dispatch reached success / first chunk |
| `learned` | The confirmed negative was persisted to the learned registry |

Correlate across the retry/fallback hops with the request span's
`request_id`. A classified replay rejection is converted to a body-free
structured error before it reaches the generic retry/fallback logs, so
the rejection envelope never renders into an `error = ?e` line.

## What's never logged

- Resolved secret values (env contents, file contents, OAuth tokens,
  bearer keys, AWS access/secret keys).
- The supplied `x-api-key` / `Authorization: Bearer` value on a
  rejected listener auth (we log only header presence).
- Full upstream request/response bodies. Bodies are only excerpted to
  256 chars on 4xx/5xx upstream paths, intentionally. Full body
  inspection is available at trace level -- see the Triage recipes
  section above.

## Trace-level surfaces

Operator grep cheat sheet:

| Surface | Direction | Filter |
|---|---|---|
| `"ingress request body"`           | 1 client -> routectl     | `ingress=<openai\|anthropic>` |
| `"outgoing request body"`          | 2 routectl -> upstream   | `provider_kind=<kind>` |
| `"upstream success body"`          | 3 upstream -> routectl   | `provider_kind=<kind>` |
| `"egress response body"`           | 4 routectl -> client     | `ingress=<openai\|anthropic>` |
| `"structural summary"`             | 1 + 2 (request-side only) | `direction=ingress\|outgoing` |
| `"stream summary"` `direction=upstream` | provider-side stream end | `chunks=`, `finish_reason=` |
| `"stream summary"` `direction=egress`   | ingress-side stream end  | `chunks=`, `finish_reason=` |

The `"structural summary"` line fires on every REQUEST-side body
(directions 1 and 2 only -- response bodies are not summarized). It
carries a stable set of prompt-content-free fields for grepping
wire-shape invariants (`model=`,
`max_tokens=`, `thinking_shape=`, `output_config_effort=`,
`tool_choice_shape=`, `cache_control_count=`, `messages_len=`,
`tools_len=`, `anthropic_beta=`, `provider_extras_keys=`, `stream=`)
without fighting the 16 KB body cap that truncates fields appearing
after a large messages array. Existing field names in this line are
stable; pin scripts on them freely.

### Header trace redaction policy

When `ROUTECTL_TRACE_HEADERS=1` is set, the four `trace_*_headers`
emitters apply DIFFERENT redaction per direction by design:

| Direction | Headers emitted | Redaction |
|-----------|-----------------|-----------|
| 1 -- ingress request (client -> routectl) | Redacted | `authorization` Bearer values are replaced with `Bearer [REDACTED]` (scheme kept); `x-api-key`, non-Bearer `authorization`, and `proxy-authorization` collapse to `[REDACTED]` so a live client session token never lands in log archives |
| 2 -- outgoing request (routectl -> upstream) | Redacted | `authorization` Bearer values are replaced with `Bearer [REDACTED]` (scheme kept); `x-api-key`, non-Bearer `authorization`, and `proxy-authorization` collapse to `[REDACTED]` so live tokens never land in log archives |
| 3 -- upstream response (upstream -> routectl) | Redacted | `set-cookie` session credentials, an `authorization` echo, and the `x-amz-security-token` STS credential collapse to `[REDACTED]` (or `Bearer [REDACTED]`); rate-limit metadata, `x-amz-date`, and other non-secret headers round-trip verbatim |
| 4 -- egress response (routectl -> client) | RAW, no redaction | None -- egress headers carry no secrets |

This is intentional. Directions 1, 2, and 3 are the only directions
that carry auth or session material (a client session token inbound,
Bearer JWTs / api keys outbound, and an occasional session-cookie /
STS echo on the upstream response). Direction 4 is raw so
fixture-capture and triage workflows see the exact wire values
without workarounds; the fixture-capture rig parses the same TRACE
lines these emitters produce, so a direction-1 fixture's
`ingress_request.headers.json` carries the redacted value too --
consistent with the already-redacted `outgoing_request.headers.json`
and `upstream_response.headers.json` on directions 2 and 3.

**Treat TRACE logs as sensitive.** Even with direction-1, -2, and -3
redaction, direction 4 emits header values verbatim. Beta flags that
double as capability indicators still appear in the trace log on
every direction. Restrict log-archive
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
| Bedrock SigV4 / cred chain failed (Profile) | `WARN routectl_providers::bedrock::auth auth_kind=Profile profile=<name> region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock SigV4 / cred chain failed (DefaultChain) | `WARN routectl_providers::bedrock::auth auth_kind=DefaultChain region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock upstream 401 | `WARN routectl_providers::bedrock provider=<id> status=401 body_excerpt=... "bedrock upstream auth rejected"` |
| Bedrock SigV4 sign failure | `ERROR routectl_providers::bedrock::signing failure_kind=<kind> ... "bedrock auth failed"` -- where `<kind>` is one of `bearer_header_invalid`, `creds_unavailable`, `body_unbuffered`, `signing_params_build`, `non_ascii_header`, `signable_request_build`, `sigv4_sign`, `signed_header_name_invalid`, `signed_header_value_invalid`, `unexpected_query_params` |
| Bedrock 403 (IAM denied) | `WARN routectl_providers::bedrock provider=<id> status=403 action=<bedrock-runtime:InvokeModel...> principal_present=<bool> "bedrock IAM access denied"` -- `action` extracted from the AWS error body so you immediately see WHICH IAM action your role lacks |
| Bedrock in-stream auth event | `WARN routectl_providers::bedrock::eventstream provider=<id> event_type=accessDeniedException\|unauthorizedException\|authentication_error\|permission_error message=... "bedrock in-stream auth/permission exception"` |
| Anthropic upstream 401/403 | `WARN routectl_providers::anthropic_api provider=<id> status=<401\|403> auth_kind=<ApiKey\|OauthBearer> context=anthropic body_excerpt=... "upstream auth failed"` |
| OpenAI-compat upstream 401/403 | `WARN routectl_providers::openai_compat provider=<id> status=<401\|403> context=openai-compat body_excerpt=... "upstream auth failed"` |

Both rows share the message string `"upstream auth failed"`; the `context` field distinguishes the call site.

## Usage accounting log shapes

The `routectl-usage` writer subsystem emits the following lines.
All have `target: "routectl_usage::writer"` or `"routectl_usage::handle"`
and inherit no `request_id` span (the writer runs on a dedicated OS thread).

| Level | Module target | Key fields | Message |
|---|---|---|---|
| ERROR | `routectl_usage::writer` | `error=...` | `"usage writer degraded -- dropping rows it cannot persist"` (healthy->degraded edge; fired once on transition) |
| ERROR | `routectl_usage::writer` | `write_errors=<N>` | `"usage writer still degraded"` (rate-limited: every 1024 errors after the first) |
| INFO  | `routectl_usage::writer` | (none) | `"usage writer recovered -- persisting rows again"` (degraded->healthy recovery edge) |
| ERROR | `routectl_usage::writer` | `error=...` | `"usage db open failed -- running degraded (records will be dropped)"` |
| WARN  | `routectl_usage::handle` | `dropped_total=<N>` | `"usage channel full -- dropping record (capture lags writer)"` (rate-limited: first drop + every 1024 thereafter) |
| WARN  | `routectl_usage::writer` | `error=...` | `"usage retention prune failed -- continuing"` |

The usage ledger's `http_status` column records the transport status the
client received: 200 for a delivered non-streaming body and once the SSE
head commits, while a mid-stream provider failure keeps 200 and is carried
by `outcome` / `error_class` / `stream_stage` instead (streaming rows
written before this rule was in force are NULL and are not back-migrated).

## Config-edit audit shape

`routectl config set` emits exactly one audit event on a successful
write (a no-op set or a rejected edit emits nothing):

| Level | Fields | Message |
|---|---|---|
| INFO | `surface="cli"`, `verb="set"`, `path=<dotted path>`, `restart_required=[<field>, ...]`, `high_consequence=<bool>` | `"config edit committed"` |

The event records WHICH key changed and whether the change was
egress-defining or restart-only -- never the value written. The value
may be a `literal:` secret, so it is deliberately absent from the audit
trail (as it is from every other log surface).

## Prompt-cache auto-emission log shapes

The dispatch-path prompt-cache auto-emitter (see CONFIGURATION.md,
"Prompt-cache auto-emission") emits two lines per request: a per-dispatch
`cache_auto_decision` at DEBUG, and a `cache_auto_outcome` at DEBUG on the
healthy path or WARN when a cache thrash is detected (see each section
below for the exact level). Both carry counts and stable tokens only --
never bodies, prompt content, or secrets.

### `cache_auto_decision` (DEBUG, per dispatch)

Emitted once per dispatch target with the decision the auto-emitter
made for that target.

| Field      | Meaning                                                        |
|------------|----------------------------------------------------------------|
| `provider` | The provider name the request dispatched to.                   |
| `model`    | The resolved model id.                                         |
| `strategy` | The stable decision token (vocabulary below).                  |

The `strategy` token is a stable contract, but a LOG-ONLY one: the usage
DB's `strategy` column is write-stopped (retained in the schema, NULL for
every row written by this version onward), so this DEBUG line is the only
place the token appears -- it is not persisted anywhere:

| Token                                  | Meaning                                                                 |
|----------------------------------------|-------------------------------------------------------------------------|
| `auto_emitted`                         | routectl injected a top-level ephemeral_5m breakpoint.                  |
| `caller_supplied`                      | The caller already supplied a breakpoint; routectl deferred entirely.   |
| `volatile_vetoed`                      | The stable prefix carried high-confidence volatile tokens; vetoed.      |
| `auto_skipped:global_disabled`         | `[cache] auto_emit_top_level_breakpoint = false`.                       |
| `auto_skipped:provider_disabled`       | The provider's `auto_emit_top_level_breakpoint = false`.                |
| `auto_skipped:no_capability`           | The provider does not honor a top-level breakpoint (or capability unknown -- fail closed). |
| `auto_skipped:breakpoint_cap`          | Injecting would exceed the 4-breakpoint maximum.                        |
| `auto_skipped:validation_rolled_back`  | Injection was attempted but failed post-injection validation; rolled back. |

### `cache_auto_outcome` (DEBUG healthy / WARN on thrash)

Emitted only when routectl auto-emitted a breakpoint AND the upstream
reported cache creation this request. Compares what the auto-emitted
breakpoint cost against what it returned.

| Field            | Meaning                                                  |
|------------------|----------------------------------------------------------|
| `provider`       | The provider name.                                       |
| `model`          | The served upstream model id.                            |
| `strategy`       | Always `auto_emitted` for this line.                     |
| `cache_creation` | Aggregate cache-write tokens (5m + 1h) the upstream reported. |
| `cache_read`     | Cache-read tokens the upstream reported.                 |

- **DEBUG** (healthy): the auto-emitted breakpoint created a cache entry
  AND got a read -- the cache is paying off.
- **WARN** (thrash): the auto-emitted breakpoint created a cache entry
  but got NO read this request (`cache_creation > 0` and `cache_read ==
  0`). The stable prefix is being cached on every request without ever
  being re-read, so the premium cache-write tokens are spent for no
  payoff. **Remedy:** disable auto-emit for that provider with the
  per-provider `auto_emit_top_level_breakpoint = false`, or set its
  `cache_capability` `supports_top_level_cache_control = false` (see
  CONFIGURATION.md). A caller-supplied or skipped strategy is never
  flagged as thrash -- routectl only warns on decisions it made itself.

### per-request cache summary (`cache=READ/PROMPT (PCT%)`)

Emitted once per request from the usage-capture finalize path, alongside
the thrash signal above. The message reads `cache=READ/PROMPT (PCT%)`:

- **READ** -- the cache-read token count the upstream reported
  (`cache_read`).
- **PROMPT** -- the cache-INCLUSIVE prompt total. The usage DB stores a
  cache-EXCLUSIVE `input_tokens`, so this line reconstructs the inclusive
  prompt as `input_tokens + cache_read + cache_write_5m + cache_write_1h`.
- **PCT%** -- integer cache-hit percentage, `READ * 100 / PROMPT` (guards
  `PROMPT == 0` -> `0%`).

| Field          | Meaning                                                   |
|----------------|-----------------------------------------------------------|
| `request_id`   | The request correlation id.                               |
| `provider`     | The served provider name.                                 |
| `model`        | The served upstream model id.                             |
| `strategy`     | The stable cache-decision token (vocabulary above).       |
| `cache_read`   | Cache-read tokens the upstream reported.                  |
| `prompt`       | Cache-inclusive prompt total (reconstructed, see above).  |
| `cache_hit_pct`| Integer cache-hit percentage.                             |

Level gating, to avoid flooding INFO with `cache=0/0` on every uncached
request:

- **INFO** when there was cache activity -- a read (`cache_read > 0`), a
  write (`cache_write_5m + cache_write_1h > 0`), or an auto-emitted
  decision (`strategy == auto_emitted`). Cached / auto-emitted requests
  get an INFO breadcrumb.
- **DEBUG** otherwise (no cache activity).

Counts, ids, and stable tokens only -- never bodies, prompt content, or
secrets.

## Startup cache-policy banner

At server startup, immediately after the `routectl listening on ...` line,
routectl emits one INFO banner summarizing the two cache-policy switches:

| Field                 | Meaning                                                  |
|-----------------------|----------------------------------------------------------|
| `auto_emit_top_level` | `[cache] auto_emit_top_level_breakpoint` (bool).         |
| `reduction`           | `[reduction] enabled` (bool).                            |

The human message reads `cache policy: auto-emit top-level breakpoint
<enabled|disabled>, context reduction <enabled|disabled>`. It lets an
operator confirm at a glance which cache behaviors are live for this
process without grepping the config.

## Auto-activation inventory audit events

routectl tracks which of its own OAuth providers (anthropic, codex, xai,
antigravity) currently carry a usable LOCAL credential -- computed at
server boot and recomputed on every config or credentials reload. Each
transition into or out of the activated set emits one audit event. All
share the stable message `activation inventory` (grep this to isolate the
trail). The probe is local-only: it reads the in-memory OAuth token cache
and never touches the network.

| Level | Trigger condition | Key fields | Message |
|---|---|---|---|
| INFO | A provider became activated | `provider`, `kind`, `trigger`, `transition=activated`, `referenced_by_aliases` | `activation inventory` |
| INFO | A provider became unresolved (lost its credential) | `provider`, `kind`, `trigger`, `transition=deactivated`, `reason`, `referenced_by_aliases` | `activation inventory` |
| WARN | No OAuth credential store to probe (no HOME/XDG) | `trigger` | `activation inventory: no OAuth credential store available to probe` |

Nothing is emitted when a recompute changes nothing (a routine token
refresh that keeps every provider activated is silent). Field vocabulary:

| Field | Meaning |
|---|---|
| `provider` | OAuth provider id (`anthropic`, `codex`, `xai`, `antigravity`). |
| `kind` | The provider's own-credential config kind (`anthropic-api`, `openai-responses`, `openai-compat`, `gemini`). |
| `trigger` | What caused the recompute: `startup`, `config_change`, or `credentials_change`. |
| `transition` | `activated` or `deactivated`. |
| `reason` | Deactivation reason code (deactivated only): `oauth_missing`, `oauth_expired`, `oauth_store_unavailable`, `not_cataloged`, or `unknown`. |
| `referenced_by_aliases` | `true` when a configured provider consumes this credential AND is reachable via the alias table; `false` for a bare login with no matching config. |

These fields carry only display-safe discriminants -- never a token, a
filesystem path, or an env value. The initially-activated set at boot
surfaces as `transition=activated` events with `trigger=startup`.

```bash
# Watch activation transitions (login / logout / expiry) live.
ROUTECTL_LOG=info ./routectl serve 2>&1 | grep "activation inventory"

# Only the deactivation reason codes.
ROUTECTL_LOG=info ./routectl serve 2>&1 \
  | grep "activation inventory" | grep transition=deactivated
```

## Context-reduction log shapes

The dispatch-path context reducer (see CONFIGURATION.md, "Context
reduction") emits one line per request, and only when reduction actually
stripped bytes. The line carries counts and stable tokens only -- never
message bodies, tool content, prompt text, or secrets.

### `context_reduction` (DEBUG, only when applied)

Emitted once per dispatch when the whitespace-only minify pass changed at
least one JSON-valued string in the mutable tail. A request where
reduction is disabled, has no mutable tail, or finds nothing to strip
logs nothing here -- and its decision is NOT persisted either: the usage
DB's `reduction_strategy` column is write-stopped (see below).

| Field             | Meaning                                                       |
|-------------------|---------------------------------------------------------------|
| `provider`        | The provider name the request dispatched to.                  |
| `model`           | The resolved model id.                                        |
| `strategy`        | The stable decision token (always `applied` for this line).   |
| `strings_minified`| How many JSON-valued strings were minified this request.      |
| `bytes_saved`     | Total bytes removed across those strings.                     |
| `est_tokens_saved`| Estimated tokens saved (a byte-derived approximation).        |

The `strategy` token is a stable contract, but a LOG-ONLY one: the usage
DB's `reduction_strategy` column is write-stopped (retained in the schema,
NULL for every row written by this version onward). Observability is
PARTIAL -- only the `applied` token ever reaches a log line, because
`context_reduction` is emitted only when reduction actually stripped
bytes. The `skipped:*` tokens below are the vocabulary of the reducer's
decision, not of anything observable: they are neither logged nor
persisted:

| Token                       | Meaning                                                                  |
|-----------------------------|--------------------------------------------------------------------------|
| `applied`                   | Reduction ran and stripped whitespace from at least one JSON string.     |
| `skipped:disabled`          | Reduction not effective (global off, or provider `reduction_enabled = false`); the minify pass never ran. |
| `skipped:no-tail`           | No mutable tail (every message is frozen behind a caller breakpoint); nothing to safely touch. |
| `skipped:nothing-to-strip`  | The pass ran but no JSON-valued string in the tail had insignificant whitespace to remove. |
| `skipped:unknown`           | Reduction ran but produced an outcome this build does not map (forward-compat catch-all). |

Only `applied` emits a `context_reduction` log line; the `skipped:*`
tokens produce no log line and, since the ledger column is write-stopped,
leave no record at all.

## Stream first-activity mark

`try_stream_with_first_content` (routectl-router) emits one DEBUG line the
instant a streaming upstream's response headers arrive -- before the
first content chunk is awaited. This is the first sign of upstream
life, distinct from the existing first-CONTENT `ttfb_ms` mark
(`mark_first_byte`), which additionally waits out any upstream
`message_start`/`ping` events the SSE parser swallows. There is no
automated regression test for this line (capturing a `tracing` event
through a thread-local subscriber proved flaky under the parallel test
harness); observe it manually instead:

```bash
ROUTECTL_LOG=routectl_router=debug ./routectl serve
```

Then issue a streaming request. Look for:

```
stream first-activity: upstream response headers received provider=... upstream=... elapsed_ms=...
```

`elapsed_ms` is measured from the per-attempt clock at dispatch. The
gap between this mark and the request's existing first-content mark
(`mark_first_byte`, recorded as `ttfb_ms` in the usage DB -- see
`routectl usage`) is the first-activity-to-first-content delta --
effectively the upstream prefill time.

## Capability intelligence events

routectl's learned-capability subsystem (see CONFIGURATION.md,
"Capability intelligence") emits a fixed vocabulary of structured events
as it learns, re-probes, clears, and strips per-target capability
negatives, plus two config-layer events for operator override hygiene.
Every event carries a stable `event` discriminator so alerts pin on that
field, never on the human message string. The events share one unified
field vocabulary: `event` names the kind, `state_key` names the dispatch
target's session/target key, and `capability_key` carries the normalized
capability token. All fields are display-safe discriminants -- capability
TOKENS, session/target keys, and counts only. Never a request body,
prompt content, or secret.

**Revision:** field vocabulary last changed in 0.9.0 (the capability
event vocabulary was unified on `event` / `state_key` / `capability_key`;
the tail-demotion event was renamed to `route_away` with INFO/WARN
levels; the `learn` event gained the `provider_kind` / `upstream_status`
/ `upstream_code` / `upstream_param` enrichment fields; `clear`,
`expire_probe`, and `count_tokens` were added). The field names and
`outcome` / `signal_tier` / `event` tokens below are a stable
contract; new fields may be added between releases.

**Not the stable API.** `routectl doctor --json` surfaces the capability
panel (catalog priors and operator overrides read by a fresh process;
the learned registry is runtime-only and NOT visible to doctor) for
human triage, but its shape is NOT a stability guarantee and may
change between releases. Build tooling against the event contract
documented here (these `event` tokens and field names), not against
`doctor --json`.

Summary (grep the `event` field to isolate a kind):

| `event` | Level | Module target | Message |
|---|---|---|---|
| `learn` | WARN | `routectl_router::router` | `learned-capability negative observed` |
| `clear` | INFO | `routectl_router::learned_capability` | `learned-capability negative cleared by successful re-probe` |
| `expire_probe` | INFO | `routectl_router::learned_capability` | `lapsed learned negative admitted for its single re-probe` |
| `evict` | WARN | `routectl_router::learned_capability` | `learned-capability registry at capacity; evicted oldest entry` |
| `route_away` | INFO / WARN | `routectl_router::router` | `learned-capability negative de-prioritized this target to the tail` (INFO) / `... routed this target away; request survives only via the de-prioritized learned tail` (WARN) |
| `count_tokens` | INFO | `routectl_router::router` | `count_tokens seat terminal; resilience class policy applied` |
| `invalidation` | WARN | `routectl_router::router` | `catalog/overlay changed across reload; clearing learned-capability registry` |
| `strip` | WARN | `routectl_router::router` | `capability_strip_decision` |
| `suppression` | WARN | `routectl_router::router` | `force_supported override contradicted: masked capability still rejected upstream` |
| `dead_override_key` | WARN | `routectl_router::override_registry` | `capability override key is rewritten by normalization; ...` |
| `legacy_deprecation` | WARN | `routectl_cli::server` | `deprecated capability-list keys are set; ...` |

### `learn` (WARN)

Emitted once per request per `(state_key, capability_key)` when an
upstream rejection teaches routectl a new (or reconfirming) capability
negative. `capability_key` is the CANONICAL capability the shared
resolver attributed the fault to (e.g. `web_search`, `structured_output`)
-- not the raw upstream `error.code` token, which is carried separately as
`upstream_code` for observability.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `learn`. |
| `state_key` | string | The dispatch target's session/target key. |
| `capability_key` | string | The normalized canonical capability token learned unsupported. |
| `provider_kind` | string | The target provider's egress kind (`anthropic-api`, `openai-compat`, ...). |
| `upstream_status` | integer | The upstream HTTP status that carried the rejection (`400` or `422`). |
| `upstream_code` | string | The upstream `error.code` token, or empty when the upstream sent none. |
| `upstream_param` | string | The upstream `error.param` value -- PRESENT ONLY when the sanitizer deemed it safe to log verbatim (bounded, single-token, no whitespace/control bytes); the field is OMITTED entirely otherwise, so an adversarial or oversized `error.param` never reaches the log. |
| `signal_tier` | string | `self-identifying` or `inferred` -- how the negative was classified. |
| `observations` | integer | How many times this negative has been observed (>= 1). |
| `acting` | bool | `true` once the entry is acting (routes away / strips); `false` while still pending. |

```
WARN routectl_router::router event=learn state_key=m1
  capability_key=structured_output provider_kind=openai-compat
  upstream_status=400 upstream_code=unsupported_parameter
  upstream_param=response_format signal_tier=self-identifying
  observations=1 acting=true "learned-capability negative observed"
```

### `clear` (INFO)

Emitted when a successful re-probe clears a resident learned negative.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `clear`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token cleared. |
| `signal_tier` | string | `self-identifying` or `inferred` (from the cleared entry). |

```
INFO routectl_router::learned_capability event=clear state_key=nick
  capability_key=web_search signal_tier=self-identifying
  "learned-capability negative cleared by successful re-probe"
```

### `expire_probe` (INFO)

Emitted when a lapsed learned negative is admitted for its single
re-probe.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `expire_probe`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token being re-probed. |
| `signal_tier` | string | `self-identifying` or `inferred`. |

```
INFO routectl_router::learned_capability event=expire_probe state_key=nick
  capability_key=web_search signal_tier=self-identifying
  "lapsed learned negative admitted for its single re-probe"
```

### `evict` (WARN)

Emitted when the registry is at capacity and evicts the oldest entry. A
safety valve, not a routine cache policy.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `evict`. |
| `state_key` | string | The evicted entry's session/target key. |
| `capability_key` | string | The evicted entry's capability token. |
| `max_entries` | integer | The registry capacity that triggered eviction. |

```
WARN routectl_router::learned_capability event=evict state_key=n
  capability_key=cap_a max_entries=2
  "learned-capability registry at capacity; evicted oldest entry"
```

### `route_away` (INFO / WARN)

Emitted once per learned-tail demotion when an acting capability negative
de-prioritizes a target. The LEVEL distinguishes the two outcomes:

- **INFO** when a supported alternative still fronts the chain -- the
  learned negative simply moved this target to the tail.
- **WARN** when the chain survives ONLY via the de-prioritized learned
  tail (every other target was filtered out), so the request rides the
  route-away floor. This is the level to alert on: it is the moment a
  learned negative -- possibly a mislearn -- actually changes which target
  serves traffic.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `route_away`. |
| `state_key` | string | The demoted target's session/target key. |
| `capability_key` | string | The normalized capability token that routed the target away. |

```
INFO routectl_router::router event=route_away state_key=front
  capability_key=web_search
  "learned-capability negative de-prioritized this target to the tail"

WARN routectl_router::router event=route_away state_key=only
  capability_key=web_search
  "learned-capability negative routed this target away; request survives
   only via the de-prioritized learned tail"
```

### `count_tokens` (INFO)

Emitted once when a `count_tokens` seat reaches its class/remap/debit
settle point -- an upstream health error (rate-limit / server / timeout /
network / overload) that the token-count path surfaces as terminal (it
never falls back on health). The messages path emits a class-decision
event at every error arm; the token-count path was otherwise silent, so
this event makes a `count_tokens` breaker debit or park triageable. A
clean count (the happy path) and a capability walk (a wire-501 that
advances to the next capable seat) do NOT emit it. Safe dimensions only
-- never a body or prompt.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `count_tokens`. |
| `state_key` | string | The seat's session/target key. |
| `provider` | string | The provider name the seat dispatched to. |
| `status` | integer | The upstream HTTP status, or `0` for a transport/non-upstream error. |
| `upstream_type` | string | The upstream error `type` token, or empty. |
| `upstream_code` | string | The upstream error `code` token, or empty. |
| `effective_class` | string | The failure class after any operator remap (`rate_limited`, `server_error`, `timeout`, `network_error`, `overloaded`, `bad_request`, `unknown`, ...). |
| `matched_by` | string | How the native class was decided (`variant`, `status`, `upstream_type`). |
| `remapped` | bool | `true` when an operator per-provider status remap replaced the native class. |
| `debit` | bool | `true` when the class debited the seat's circuit breaker (or parked it on a rate-limit reset hint); `false` when the slot was released without a health debit. |

```
INFO routectl_router::router event=count_tokens state_key=haiku
  provider=prov status=500 upstream_type= upstream_code=
  effective_class=server_error matched_by=status remapped=false debit=true
  "count_tokens seat terminal; resilience class policy applied"
```

### `invalidation` (WARN)

Emitted when a catalog or overlay change across a hot reload clears the
entire learned-capability registry (fresher config truth wins over
learned negatives).

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `invalidation`. |
| `catalog_changed` | bool | Whether the baked catalog version changed. |
| `overlay_changed` | bool | Whether the operator overlay revision changed. |
| `previous_catalog_version` | integer | The outgoing Router's catalog version. |
| `catalog_version` | integer | The incoming Router's catalog version. |
| `previous_overlay_revision` | integer | The outgoing Router's overlay revision. |
| `overlay_revision` | integer | The incoming Router's overlay revision. |

```
WARN routectl_router::router event=invalidation catalog_changed=true
  overlay_changed=false previous_catalog_version=7 catalog_version=8
  previous_overlay_revision=0 overlay_revision=0
  "catalog/overlay changed across reload; clearing learned-capability registry"
```

### `strip` (WARN)

Emitted once per capability-strip decision. `capability_key` names the
verdict's keys (already sorted + normalized; comma-joined at the
per-decision site, a single token at the probe-bypass site). `outcome`
is the stable decision token.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `strip`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized strip verdict key(s), comma-joined. |
| `outcome` | string | The stable decision token (vocabulary below). |

The `outcome` token is a stable contract -- pin alerts on it, not on the
message:

| Token | Meaning |
|---|---|
| `applied` | The strip ran and removed the capability surface from the attempt request. |
| `noop` | The verdict named a capability the request did not carry; nothing to strip. |
| `strict_rejected` | `strict_translation` is on; the strip would mutate, so the request is rejected before any change. |
| `validation_rolled_back` | The strip created a post-strip hazard; the request was restored byte-for-byte and the attempt routes away. |
| `probe_bypassed` | A strip-eligible feature was admitted for re-probe, so it was intentionally NOT stripped (emitted at the verdict site). |

A `disabled` kill switch (empty strip verdict) emits NO `strip` event --
the verdict is skipped entirely, so there is no per-decision context to
name.

```
WARN routectl_router::router event=strip state_key=nick
  capability_key=advisor outcome=applied "capability_strip_decision"
```

### `suppression` (WARN)

Emitted once per request (deduped) when an operator `force_supported`
override is contradicted: the masked capability was still rejected
upstream.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `suppression`. |
| `state_key` | string | The target's session/target key. |
| `capability_key` | string | The normalized capability token the operator forced on. |

```
WARN routectl_router::router event=suppression state_key=m1
  capability_key=unsupported_parameter
  "force_supported override contradicted: masked capability still rejected upstream"
```

### `dead_override_key` (WARN)

A config-layer event, emitted once per operator override key that
normalization rewrites: such a key can never match a normalized registry
key, so the override is dead.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `dead_override_key`. |
| `target_spec` | string | The override target (`provider` or `provider:nickname`). |
| `raw_key` | string | The operator's key as written in config. |
| `normalized_key` | string | What normalization rewrites it to (use this form instead). |

```
WARN routectl_router::override_registry event=dead_override_key
  target_spec=br raw_key=additionalModelRequestFields.anthropic_beta
  normalized_key=anthropic_beta
  "capability override key is rewritten by normalization; it can never
  match and is dead -- use the normalized form"
```

### `legacy_deprecation` (WARN)

A config-layer event, emitted exactly once on a serve cold-start or hot
reload when the loaded config carries any legacy capability-list key. It
names key NAMES only -- never config values (secrets can live near these
tables). `config check` never emits it.

| Field | Type | Meaning |
|---|---|---|
| `event` | string | Always `legacy_deprecation`. |
| `legacy_keys` | string | Debug-rendered list of the present legacy key names. |
| `successor` | string | Always `[capability.overrides]`. |
| `migrate_command` | string | Always `config migrate`. |

```
WARN routectl_cli::server event=legacy_deprecation
  legacy_keys=["unsupported_features", "allowed_betas"]
  successor=[capability.overrides] migrate_command="config migrate"
  "deprecated capability-list keys are set; they are tolerated for one
  release cycle and rejected at the next config schema version. Move them
  under [capability.overrides] with `config migrate`."
```
