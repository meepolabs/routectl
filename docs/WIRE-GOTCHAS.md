# Wire Gotchas

This is the running log of upstream wire-shape weirdness routectl handles
internally. Each entry names the wire-level reality, where in the code it's
handled, and any residual seams. When a model breaks the live matrix, grep
this doc first for similar patterns. For operator-facing config recipes see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md); for the debug runbook see
[DEVELOPMENT.md](DEVELOPMENT.md).

## openai-compat surface

- **`null` content alongside non-null reasoning**. NIM's
  `meta/llama-3.3-70b-instruct` returns both `reasoning: null` and
  `reasoning_content: null`. Handled by `merge_reasoning_keys` in
  `crates/routectl-providers/src/openai_compat/response.rs` -- it
  drops null `reasoning` and promotes non-null `reasoning_content`.
  `MessageContent::Null` variant in
  `crates/routectl-core/src/schema.rs` lets the response deserialize
  cleanly.

- **Missing `id` / `created` on responses or chunks**. NIM's
  `google/gemma-3-12b-it` omits `created`; some chunks omit `id`.
  Handled by `#[serde(default)]` on the optional fields in
  `ChatResponse` / `ChatChunk`.

- **Post-`[DONE]` SSE trailers**. Some openai-compat hosts emit a
  bookkeeping chunk (e.g. `data: {"choices":[],"cost":"0"}`) after
  the `[DONE]` terminator. Handled by an explicit `return` in the SSE
  loop at `crates/routectl-providers/src/openai_compat/mod.rs` --
  `[DONE]` stops parsing.

- **DeepSeek 400 on `reasoning_content` echoed in history**. The
  DeepSeek dialect's `Dialect::strip_history_reasoning()` returns
  true; the request normalizer strips the field before sending.

- **`<think>` tags split across stream chunks**. Handled by
  `ThinkTagAccumulator` in
  `crates/routectl-providers/src/openai_compat/sse.rs` -- a
  cross-chunk state machine. Lives outside the dispatch path because
  the trait is stateless.

- **HTML upstream error bodies leaking into the JSON envelope**.
  Misconfigured `base_url`s land on marketing 404 pages. Handled by
  `sanitize_upstream_body` in
  `crates/routectl-providers/src/openai_compat/mod.rs`.

- **Strict openai-compat hosts 400 on top-level `system`**. NIM
  rejects with `Validation: Unsupported parameter(s): system` because
  `system` is the Anthropic-shape top-level field; OpenAI carries it
  as a `role: "system"` message. The OpenAI ingress lifts wire
  `role: "system"` into canonical `req.system`; the openai-compat
  egress at
  `crates/routectl-providers/src/openai_compat/request.rs::normalize`
  does the inverse lower (synthetic `role: "system"` message
  prepended; top-level `system` removed) so neither lenient (OpenAI,
  OpenRouter, opencode-go) nor strict (NIM) hosts see the
  Anthropic-shape field.

## Anthropic API surface

- **Anthropic `tool_choice` rejects bare-string OpenAI shape**.
  Bedrock validators 400 on `tool_choice: "auto"` (OpenAI) where
  Anthropic expects `{"type":"auto"}`. Handled at the Anthropic-API
  egress by `translate_tool_choice` in
  `crates/routectl-providers/src/anthropic_api/request.rs`. Maps
  `"auto"|"none"|"required"` plus the OpenAI `{"type":"function",...}`
  object form into the Anthropic tagged-enum shape; Anthropic-shape
  inputs pass through unchanged.

- **Anthropic structured-output `output_format` (legacy) at the
  ingress.** Anthropic's current wire shape is
  `output_config.format = {type: "json_schema", schema: ...}`. Older
  callers (and the Claude-Code legacy SDK path) still send a
  top-level `output_format` field; serde on `ChatRequest` does not
  know that name and would silently drop it. `merge_output_format`
  in `crates/routectl-cli/src/ingress/anthropic/parse.rs` rewrites the
  legacy field into `output_config.format` (preserving any existing
  `output_config.effort`); when both shapes arrive on one request it
  prefers the nested form and WARNs, mirroring claude-code's own
  deprecation message. The egresses need no extra translation:
  Bedrock-Invoke for Claude is Anthropic-shape passthrough, and the
  Anthropic-API egress already merges `provider_extras["output_config"]`
  into the body.

- **Forward-compat sweep on the Anthropic ingress.** Anthropic adds
  new top-level body fields on a quarterly cadence (e.g. recent
  additions: `context_management`, `context_hint`, `speed`,
  `diagnostics`, `mcp_servers`). Without an explicit pull-out, serde
  on `ChatRequest` would silently drop them at the ingress boundary.
  `translate_request` sweeps every key NOT in
  `CANONICAL_CHAT_REQUEST_WIRE_FIELDS` into `provider_extras` so the
  egress's `merge_provider_extras` forwards them upstream verbatim.
  When canonical adds a new field, also add it to the
  `CANONICAL_CHAT_REQUEST_WIRE_FIELDS` const in
  `crates/routectl-cli/src/ingress/anthropic/parse.rs`.

- **Inbound `anthropic-beta` HTTP header lift.** The
  `@anthropic-ai/sdk` Beta API translates the SDK option `betas:
  [...]` into the `anthropic-beta: a,b,c` HTTP header (not into the
  body's `anthropic_beta` array). claude-code uses this surface for
  first-party betas (context-management, prompt-cache-1h,
  adaptive-thinking, ...). routectl lifts inbound header values into
  canonical `req.anthropic_beta` (deduplicated, preserving body
  order) so the egress emits them in the upstream body. Anthropic
  accepts either surface.

- **Anthropic-only `stop_reason` values pass through.** The
  Anthropic egress maps `stop_reason -> finish_reason` for the
  OpenAI overlap (`end_turn`/`stop_sequence` -> `stop`,
  `max_tokens` -> `length`, `tool_use` -> `tool_calls`) and passes
  everything else through verbatim. The Anthropic ingress's
  `openai_finish_to_anthropic_stop` reverse-maps the four overlap
  values and passes unknown values through, so claude-code's
  per-stop-reason error handling for `pause_turn`, `refusal`, and
  `model_context_window_exceeded` works.

- **Matched `stop_sequence` round-trip.** The canonical schema
  carries the matched value via `Choice.matched_stop_sequence:
  Option<String>` (and `ChunkChoice.matched_stop_sequence`). The
  Anthropic egress (and Bedrock-Invoke transitively) lifts the wire
  `stop_sequence` into this field on both non-streaming and SSE
  `MessageDelta` paths. The Anthropic ingress's `render_response`
  and `emit_message_delta` emit wire `stop_reason:"stop_sequence"` +
  `stop_sequence:"<value>"` when the field is set. For openai-compat
  upstreams, where the wire spec carries no equivalent field,
  `openai_compat::response::apply_stop_sequence_heuristic` runs
  after `normalize_response` and on terminal stream chunks: it
  suffix-matches the response content against `req.stop` (longest
  first), and falls back to the single configured stop when exactly
  one was configured AND the response carried non-empty content. The
  fallback is gated on content presence so tool-only / null-content
  responses don't over-claim, but a single-stop request that
  naturally ends mid-thought (without emitting the sequence) WILL
  get `stop_sequence` instead of `end_turn` -- a known residual
  seam, since the openai-compat wire carries no signal to
  disambiguate. Bedrock Converse is not yet covered: AWS surfaces
  the matched sequence via `additionalModelResponseFields` only when
  the request opts in via `additionalModelResponseFieldPaths`.
  Tracked as a follow-up.

- **Anthropic streaming reasoning replay residual.** Strategy A
  buffers `thinking_delta` text and `signature_delta` on the open
  block, then emits one aggregated `ReasoningDetail` at
  `content_block_stop` carrying both. When Anthropic 4.5 omits the
  `signature_delta` event on a tool-only thinking turn, the terminal
  detail emits with `signature: ""`. The replay path
  (`anthropic_api/request.rs::emit_reasoning_blocks`) WARNs and
  skips any detail with an empty signature -- because Anthropic 400s
  on a `Thinking` block missing the field, and a partial echo is
  better than a hard rejection that breaks every Claude 4.5
  multi-turn after a tool-only thinking turn.

  Residual seam: when an assistant message has MULTIPLE thinking
  blocks where some have signatures and some don't, the replayed
  history loses the unsigned blocks. Anthropic upstream sees a
  shorter block sequence than it generated, which can cause cache
  misses or quality drift on the follow-up turn. There is no clean
  fix without one of:
  (a) a synthetic signature (Anthropic would reject it),
  (b) a hard error on every Claude 4.5 multi-turn (regressing usability),
  (c) a canonical-schema change to track per-block "unsigned" sentinels.

  Operators triaging "why is the model losing context?" should grep
  for the WARN line `skipping Thinking blocks on replay: signature
  missing or empty` to correlate (one WARN per request with
  `skipped_count=N` and `skipped_indices=[...]`, NOT one per detail).
  Mirrored in `bedrock/converse/eventstream.rs` for the Converse
  stream path.

- **Forward-compat for unknown Anthropic SSE block types.**
  Anthropic ships new `content_block.type` values whenever the
  platform adds a feature (`server_tool_use` for `web_search`,
  `web_search_tool_result` with `citations_delta` inside, etc.).
  The egress used strict-tagged serde enums at three sites
  (`SseEvent`, `SseContentBlockStart`, `SseDelta`) so an unknown
  variant returned `Error::Streaming` and walked the router
  fallback chain. Two-layer fix: (v1, continuity) `Other(Value)`
  catchalls on the three enums via custom `Deserialize` plus
  `OpenBlockKind::Unknown` in the SSE state machine
  (`crates/routectl-providers/src/anthropic_api/sse_unknown.rs`,
  `types_sse.rs`); index-invariant validation across all variants
  WARNs and drops misattributed deltas. (v2, fidelity) a
  `#[serde(skip)] opaque_events: Vec` carrier on `ChatChunk`
  (transport-internal, never on the wire) captures each unknown
  event's bytes; the matching Anthropic ingress reads the carrier
  and re-emits `content_block_start` / `delta` / `stop` SSE
  verbatim so strict clients (citation links, search-status UI)
  see the full upstream wire. v2 is non-authoritative: bounded
  caps in
  `crates/routectl-providers/src/anthropic_api/sse_opaque.rs`
  (256 KB bytes / 10000 deltas per block) downgrade overflowed
  blocks to v1 silently with a WARN, and any replay-path failure
  logs and skips that one event without terminating the stream.
  Bedrock-Invoke inherits the v1 fix free (delegates to the same
  `parse_event`); Bedrock-Converse streaming forward-compat is a
  separate task.

## Bedrock surface

- **Bedrock rejects unsupported `anthropic_beta` values + unknown
  body fields.** Bedrock validates each entry of `anthropic_beta`
  independently and 400s the entire request on the first unsupported
  value -- there is no per-flag fallback. The same strict-schema
  validator also rejects any unrecognized top-level body field (or
  Converse `additionalModelRequestFields` key) with `"Extra inputs
  are not permitted"`. claude-code's TS SDK ships ~10 betas via the
  `anthropic-beta` HTTP header and the Anthropic ingress's
  forward-compat sweep forwards quarterly-added Anthropic body fields
  (`mcp_servers`, `diagnostics`, `context_hint`, ...), only a subset
  of which AWS gates for distribution.

  routectl ships NO const default for either surface -- AWS schema
  drift is operator-tracked, not release-bound. Both surfaces are
  filtered against operator-supplied TOML lists:

  ```toml
  [bedrock]
  allowed_betas       = [...]   # filters body's anthropic_beta array
  allowed_body_fields = [...]   # filters top-level body keys
  ```

  See `examples/bedrock.toml` for the empirical 2026-05-12 baseline
  (16 betas + 16 body fields). Empty list (or omitted [bedrock]
  section) = pass-through (no filter applied) -- the discovery
  default for bringing routectl up against a fresh AWS account; use
  `ROUTECTL_LOG=routectl_providers::bedrock=trace` to capture sent
  fields/flags, then populate the lists.

  Filters live in `bedrock/{betas,body_fields}.rs` and apply on both
  Invoke (top-level Anthropic body) and Converse
  (`additionalModelRequestFields` bag). Drops log at DEBUG (not WARN
  -- the SDK reliably sends a handful of unsupported entries per
  request and WARN would flood `routectl-warn.log`).

  Per-provider escape hatch -- `[providers.X] anthropic_beta = [...]`
  is unchanged: those flags are always sent and bypass the filter
  (operator-asserted), independent of the global allowlist.

## OpenAI Responses surface

- **OpenAI Responses chatgpt-oauth endpoint is stream-only.** Sending
  `stream:false` returns HTTP 400 `{"detail":"Stream must be set to true"}`.
  `OpenAiResponsesProvider::complete()` forces `stream:true`, drains the SSE
  stream until a `response.completed` (or `response.failed` /
  `response.cancelled`) event, and extracts the `response` field from that
  event as the final body. The streaming tests in `mod.rs::e2e_tests` use
  a wiremock that returns an SSE `response.completed` event rather than a
  plain JSON body.

- **OpenAI Responses tools/tool_choice must use the flat Responses shape.**
  The chatgpt-oauth backend 400s with
  `"Missing required parameter: 'tools[0].name'"` on the nested
  chat-completions shape `{type:"function",function:{name,...}}`. The flat
  shape is `{type:"function",name,description,parameters,strict}` (no
  nested `function` key). Similarly, `tool_choice` named-function must be
  `{"type":"function","name":"X"}` (flat); the nested form
  `{"type":"function","function":{"name":"X"}}` returns
  `"Unknown parameter: 'tool_choice.function'"`. Both are handled in
  `openai_responses/types.rs` (`ResponsesTool::Function` variant) and
  `openai_responses/tools.rs` (`translate_tool_choice_object`).

- **OpenAI Responses `instructions` field must always serialize.** The
  chatgpt-oauth backend returns HTTP 400 `{"detail":"Instructions are
  required"}` when the field is absent. An empty string `""` is accepted.
  The field on `ResponsesRequest` does NOT carry
  `#[serde(skip_serializing_if = "String::is_empty")]` -- it is always
  emitted (possibly as `""`) so the server never sees the field missing.

## claude-code's hardcoded URL bypasses

- claude-code performs `WebFetch`, `WebSearch`, `PushNotification`,
  `RemoteTrigger`, `ShareOnboardingGuide`, the `/login` OAuth flow,
  and MCP server connections against hardcoded URLs that ignore
  `ANTHROPIC_BASE_URL`. routectl never sees these requests --
  capturing them at the gateway layer alone is incomplete. Pair
  routectl with a network-level proxy (mitmproxy, a side-channel
  HTTP intercept) if full claude-code egress capture is required.
