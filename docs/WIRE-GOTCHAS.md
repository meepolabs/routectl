# Wire Gotchas

This is the running log of upstream wire-shape weirdness routectl handles
internally. Each entry names the wire-level reality, where in the code it's
handled, and any residual seams. When a model breaks the live integration
matrix (`docs/TESTED_MODELS.md` -- the opt-in tests against real provider
keys), grep this doc first for similar patterns. For operator-facing config
recipes see [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md); for the debug runbook
see [DEVELOPMENT.md](DEVELOPMENT.md).

Surfaces: [openai-compat](#openai-compat-surface) -
[Anthropic API](#anthropic-api-surface) -
[Bedrock](#bedrock-surface) -
[OpenAI Responses](#openai-responses-surface) -
[Gemini](#gemini-native-surface) -
[xAI OAuth](#xai-grok-oauth-surface) -
[claude-code URL bypasses](#claude-codes-hardcoded-url-bypasses) -
[chatgpt-oauth client identity](#chatgpt-oauth-client-identity-surface)

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
  `extract_upstream_message` in
  `crates/routectl-core/src/log_safe.rs`, called from
  `crates/routectl-providers/src/openai_compat/mod.rs`; for non-JSON-envelope
  bodies `extract_upstream_message` falls back to `sanitize_upstream_body`.

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

- **Seven canonical sampling knobs have no counterpart here.** `n`,
  `seed`, `logprobs`, `top_logprobs`, `logit_bias`, `presence_penalty`
  and `frequency_penalty` do not appear in the Anthropic Messages API
  request surface, verified 2026-08-10 against Anthropic's published
  parameter reference and corroborated against `anthropic-sdk-python`'s
  `message_create_params`. The documented request params are
  `max_tokens`, `messages`, `model`, `cache_control`, `container`,
  `inference_geo`, `metadata`, `output_config`, `service_tier`,
  `stop_sequences`, `stream`, `system`, `temperature`, `thinking`,
  `tool_choice`, `tools`, `top_k` and `top_p`. This is an absence from
  the documented request contract, NOT a demonstration that the service
  rejects the seven -- routectl drops them at the egress rather than
  inventing a mapping, with one structured WARN naming the dropped
  fields (see `crates/routectl-providers/src/sampling_drop_guard.rs`).
  `top_k` IS documented by Anthropic but is not one of the seven and is
  not carried by the canonical schema, so it is a deliberate
  non-inclusion rather than a gap. Applies to bedrock-invoke too, which
  reaches this wire shape by delegation.

- **The OAuth seat 400s `temperature` and `top_p`.** A `/v1/messages`
  body carrying EITHER sampling param is rejected when the credential
  is a Claude Code OAuth bearer against `api.anthropic.com` (both
  confirmed independently; the failure is per-param, not per-pair).
  `normalize_claude_sampling` in
  `crates/routectl-providers/src/anthropic_api/extras.rs` drops both as
  the LAST body mutation before egress, called from both `complete` and
  `stream`, so it catches the caller's value, routectl's own
  thinking-clamp `temperature: 1.0`, and anything a later pass could add.
  Three notes for whoever debugs this next:
  - The gate is the LANE predicate `is_cloak_lane` (oauth-bearer + exact
    `api.anthropic.com` host + not the forwarded leg), deliberately NOT
    the cloak mode. Gating on the cloak flag would let
    `cloak.mode = "never"` re-introduce a lane-wide 400 -- the rejection
    is a property of the credential, not of the disguise.
  - `stop_sequences` is NOT stripped: probed and accepted (200). Only
    the two alternative-continuation sampling knobs are rejected.
  - `top_k` is the sibling hole. `reserved.rs` treats it as
    non-canonical pass-through, so it is the one sampling param
    `provider_extras` can still smuggle onto the wire. No confirmed 400
    today, so nothing strips it -- if a `top_k` request starts failing
    on the OAuth lane, this is the first place to look.

- **Anthropic `tool_choice` rejects bare-string OpenAI shape**.
  Bedrock validators 400 on `tool_choice: "auto"` (OpenAI) where
  Anthropic expects `{"type":"auto"}`. Handled at the Anthropic-API
  egress by `translate_tool_choice` in
  `crates/routectl-providers/src/anthropic_api/tools.rs`. Maps
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
  disambiguate. Bedrock Converse IS covered: the request opts in via
  `additionalModelResponseFieldPaths=["/stop_sequence"]`, and the
  egress lifts the matched value on both the non-streaming response
  path and the streaming `messageStop` path, gated on
  `stop_reason == "stop_sequence"`.

- **Anthropic streaming reasoning replay residual.** The SSE decoder
  buffers `thinking_delta` text and `signature_delta` on the open
  block, then emits one aggregated `ReasoningDetail` at
  `content_block_stop` carrying both. When Anthropic 4.5 omits the
  `signature_delta` event on a tool-only thinking turn, the terminal
  detail emits with `signature: ""`. The replay path
  (`anthropic_api/messages.rs::emit_reasoning_blocks`) WARNs and
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
  missing or empty`. The reasoning-skip WARNs are aggregated per
  OUTBOUND PROVIDER ATTEMPT, not per turn and not per detail: one
  `translate_messages` call pools every assistant turn in the request,
  so a transcript with several skipping turns emits at most ONE such
  line carrying `skipped_count=N` (exact total), `turns_affected=M`
  (exact turn count), and `skipped_locations=[(message_index,
  detail_index), ...]`. A router retry or a fallback to another
  provider builds a fresh request and so emits its own set of lines --
  the honest unit is the attempt, not the request.
  `skipped_count` and `turns_affected` are exact; `skipped_locations`
  is a sample capped at 8 `(message_index, detail_index)` pairs, with
  `skipped_locations_truncated=true` when pairs were omitted. The pair
  (not a bare index) is required because each message's
  `reasoning_details` has its own index space, so a detail index pooled
  across turns cannot be located without the message index beside it; a
  detail index the upstream did not supply stays `None`.
  The foreign-format skip is a SEPARATE category on its own WARN line
  (`skipping reasoning blocks on replay: format is not
  anthropic-claude-v1`), because a missing signature and a
  non-Anthropic format have different remediations -- so an attempt
  that hits both causes emits TWO lines, never one merged line and
  never one per message. The empty-content backstop
  (`event=empty_content_backstop`) is folded into the same per-attempt
  tally with a `backstop_count`, so a Null-content transcript's WARN
  count stays independent of turn count.
  The Converse egress
  (`bedrock/converse/messages.rs`, `emit_reasoning_blocks_converse`)
  mirrors ONLY the unsigned aggregation, under its own distinct message
  string (`skipping Thinking blocks on Converse replay: signature
  missing or empty`). It has NO foreign-format signal at all: a
  reasoning detail whose format is not `anthropic-claude-v1` is dropped
  silently on that seam, with no counter and no WARN.

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
  value-preserving (semantically lossless for valid JSON -- the
  captured `serde_json::Value` is re-serialized, not echoed as the
  exact upstream byte slice) so strict clients (citation links,
  search-status UI) see the full upstream wire. v2 is non-authoritative: bounded
  caps in
  `crates/routectl-providers/src/anthropic_api/sse_opaque.rs`
  (256 KB bytes / 10000 deltas per block) downgrade overflowed
  blocks to v1 silently with a WARN, and any replay-path failure
  logs and skips that one event without terminating the stream.
  Bedrock-Invoke inherits the v1 fix free (delegates to the same
  `parse_event`); Bedrock-Converse streaming forward-compat is a
  separate task.

- **OpenAI `{type:"file"}` parts must be translated to Anthropic document blocks.**
  Anthropic and Bedrock 400 on a raw OpenAI file block forwarded verbatim. A
  `file.file_data` base64 `application/pdf` data URI is translated to an
  Anthropic document block (base64 source, `title` from `file.filename`).
  Untranslatable shapes -- `file_id` reference, non-PDF MIME type, non-base64
  data URI, or empty `file_data` -- are re-emitted verbatim as
  `ContentBlock::Other` on the Anthropic egress so the upstream surfaces a
  clean error rather than a silent drop. The Converse egress drops
  untranslatable shapes with a WARN. Handled by `parse_file_document_source`
  in `crates/routectl-providers/src/anthropic_api/parts.rs`; Converse mirror
  in `crates/routectl-providers/src/bedrock/converse/messages.rs`.

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

- **`ValidationException` 400s are header-discriminated, not
  `__type`-bodied.** AWS docs show request-validation errors as a flat
  body carrying a `__type` key
  (`{"__type":"...ValidationException","message":"..."}`), but real
  bedrock-runtime `InvokeModel` 400s serve a flat MINIMAL body with NO
  `__type` -- just `{"message":"..."}` -- and put the discriminator in
  the `x-amzn-ErrorType` response header
  (`ValidationException:http://internal.amazon.com/coral/...`, a fixed
  public value, not account-specific). The capability matcher gates its
  learn path on this token, so the native lane lifts it:
  `read_error_body` in `crates/routectl-providers/src/bedrock/mod.rs`
  reads the header BEFORE the body read consumes the response and falls
  back to it when the body carries no `__type` (body `__type` still
  wins). The lift
  (`crate::aws_error::classify_aws_error_type_header`) requires a single
  unambiguous header value, splits at the first `:` to drop the coral
  URL tail, and validates the bare name through the same bounded-token
  path as the body lift -- the URL tail never reaches an `Error` field
  or a log line. A stripped / duplicated / garbled header surfaces a
  bounded reason-labeled WARN (`missing|invalid|ambiguous|conflict`)
  instead of silently degrading to a non-attributed rejection.

## OpenAI Responses surface

- **OpenAI Responses chatgpt-oauth endpoint is stream-only.** Sending
  `stream:false` returns HTTP 400 `{"detail":"Stream must be set to true"}`.
  `OpenAiResponsesProvider::complete()` forces `stream:true`, drains the SSE
  stream until a `response.completed` / `response.incomplete` /
  `response.failed` / `response.cancelled` event, and extracts the `response`
  field from that event as the final body. `response.incomplete` is
  success-with-cutoff (maps to a `length` finish_reason); `response.failed`
  and `response.cancelled` surface as upstream errors. The streaming tests
  in `mod.rs::e2e_tests` use a wiremock that returns an SSE
  `response.completed` event rather than a plain JSON body.

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

- **A Responses-family reasoning artifact loses its id and scheme across
  an Anthropic-dialect client.** The Anthropic wire has no slot for
  either, so the blob flattens to `redacted_thinking.data`. When the
  client echoes it back there is nothing on the wire saying what it is,
  so it cannot be replayed onto the lane that issued it and reasoning
  continuity is lost. `routectl-core/src/reasoning_envelope.rs` carries
  the artifact in a self-describing envelope instead:

  ```text
  rctl1.<scheme>.<id>.<blob>
  ```

  - The `rctl1` version prefix is a CLOSED set. An unrecognized version
    is a non-match, never a best-effort parse, so a future format cannot
    be mis-read by an older reader.
  - **Separator invariant:** `.` is safe because no artifact family
    routectl carries contains it -- content-prefixed blobs (`rsn_`,
    `smry_`) and Anthropic signatures (`CAIS`, `Erk`) are `.`-free as
    probed, and Fernet-shaped blobs are base64url, which excludes `.` by
    construction. Pinned by the `SEPARATOR_ABSENT_FROM_PROBED_BLOBS`
    test constant. The invariant is collision-avoidance, not safety: a
    blob that ever did contain `.` degrades to a non-match, which is the
    safe direction.
  - The blob rides the remainder UNTOUCHED whatever its alphabet, and
    unwrapping returns a slice, so bytes replayed upstream are identical
    to what the provider issued -- prompt-cache affinity is preserved.
  - **The unwrapped `(scheme, id)` is CLIENT-CONTROLLED and is a HINT,
    never an authorization.** Anyone can mint a string claiming any
    scheme. Carry-vs-strip policy must run on an unwrapped result
    exactly as on a natively tagged artifact; a claim can never be what
    admits a blob to a lane. Parsing is total -- malformed,
    unknown-version, or non-matching input degrades to opaque-foreign-blob
    handling, never an error or a panic.
  - Being stateless is the point: continuity survives a daemon restart,
    an unbounded session, and several router instances behind a balancer
    without session affinity -- none of which a recovery table offers.
  - **Encode site:** the two Anthropic-ingress flatten sites --
    `build_content_array` in
    `crates/routectl-cli/src/ingress/anthropic/render.rs` and its
    streaming twin in `.../stream.rs` -- both go through
    `encrypted_detail_data` in `.../anthropic/mod.rs`, so the two paths
    cannot drift. A detail with no recoverable id wraps id-less rather
    than not wrapping at all: one lane family validates content and
    ignores the id entirely, so a scheme-only envelope is still fully
    replayable there. An empty blob is never wrapped -- it carries
    nothing to replay.
  - **Anthropic-byte-verbatim carve-out.** The wrap fires ONLY on a
    Responses-family detail, decided by the shared
    `is_responses_family(format)` classifier. Everything else --
    Anthropic-sourced above all, plus untagged and other-dialect details
    -- reaches the wire byte-for-byte as the upstream issued it. An
    Anthropic signature is precisely what makes same-model replay work
    on that lane, and it is platform-portable same-model and silently
    ignored cross-model, so it is never rejected; wrapping it would
    corrupt a mechanism that works today. Never introduce a second local
    notion of "is this Anthropic" beside the shared classifier.

- **Reasoning `format` tags are a family, and comparing one with `==`
  silently drops details.** `ReasoningDetail.format` serializes outward to
  OpenAI-dialect clients, so its values are a wire contract and in-flight
  client histories already carry them. The vocabulary lives in
  `routectl-core::reasoning_format`: `openai-responses-v1` (recognized
  forever, no longer emitted), plus the lane-faithful `codex-oauth`,
  `openai-apikey` and `bedrock-mantle`. Every reader uses
  `is_responses_family(format)` -- an exact-equality check against a single
  tag drops every newly-tagged detail instead of failing loudly.

- **The Responses egress stamps the lane's own tag, and never the legacy
  shared one.** Both the non-streaming translator and the SSE state machine
  take the provider's `auth_kind` and stamp `lane_format_tag(auth_kind)` on
  every reasoning detail they emit, so the two paths agree tag-for-tag for a
  given lane -- a divergence there would make a streamed artifact
  unreplayable on the lane that minted it. `openai-responses-v1` is read
  forever but emitted by nothing: more than one lane once stamped it, so
  re-emitting it would keep minting artifacts that name no lane and can only
  be treated as unestablished.

- **Replay portability is per-lane, not per-model, and the lanes are not
  interchangeable.** `scheme_of(format)` maps a tag to its validator
  family: codex-oauth and openai-apikey validate the reasoning item id and
  ignore the blob content; Bedrock mantle validates the content prefix
  (`rsn_` / `smry_`) and ignores the id, 400ing with
  `encrypted content missing recognized prefix` on a foreign blob. Both
  lanes mint `rs_`-prefixed ids, so id shape can never discriminate --
  only the tag can. `openai-responses-v1` maps to `ReplayScheme::Gray`
  because both lanes emitted it, so a detail bearing it is genuinely
  ambiguous; `is_replayable(detail, lane)` answers `Carry`/`Strip` only for
  proven pairs and `Gray` otherwise.

## Gemini (native) surface

Operator-facing config recipes live in
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) (`## Gemini (native)`); this
section names only the wire-level realities the egress handles.

- **Tool result -> user-turn `functionResponse`.** Gemini has no
  dedicated tool role. A canonical `Role::Tool` message (and an
  inline `ToolResult` content part) is emitted as a `role:"user"`
  turn carrying a `functionResponse` part, whose `name` must match
  the preceding `functionCall.name`. Assistant `tool_calls` /
  `ToolUse` blocks map to `functionCall` parts on a `role:"model"`
  turn. Handled by `build_contents` / `content_part_to_part` in
  `crates/routectl-providers/src/gemini/request.rs`.

- **System prompt -> `systemInstruction` with no role.** System
  content (both `Role::System` messages and the Anthropic-ingress
  top-level `system` field) is collected into the top-level
  `systemInstruction.parts`, which Gemini forbids a `role` field on --
  it is NOT a `contents[]` turn. Handled by `build_system_instruction`
  in `crates/routectl-providers/src/gemini/request.rs`; the wire type
  `SystemInstruction` in `gemini/types.rs` carries `parts` only.

- **`thoughtSignature` reasoning replay.** Gemini emits an opaque
  `thoughtSignature` on thinking parts and requires it verbatim on
  multi-turn replay to continue a chain-of-thought. The response /
  SSE path carries it on the emitted `reasoning_details[]` entry
  (format tag `gemini-v1`, `payload.thought_signature`); the request
  path replays only `gemini-v1`-tagged details as `thought` parts
  ahead of the visible answer. Foreign-provider reasoning (Anthropic,
  OpenAI) is dropped -- replaying it without a matching Gemini
  signature would not continue reasoning and risks an upstream
  reject. Handled by `reasoning_details_to_thought_parts` in
  `gemini/request.rs`, `thought_detail` in `gemini/response.rs`, and
  `reasoning_chunk` in `gemini/sse.rs`.

  Residual seam: the `thoughtSignature` is only valid for the
  originating Gemini turn. Reasoning that arrives without one (a
  foreign detail, or a Gemini detail whose signature was stripped
  upstream) is dropped from the replayed history rather than sent
  unsigned -- the same class of loss as the Anthropic
  unsigned-thinking-block seam above.

- **Error envelope carries `status` + numeric `code`.** Gemini 4xx/5xx
  bodies are `{"error":{"code":<int>,"message":...,"status":"<UPPER_
  SNAKE>"}}`. `error.status` (e.g. `RESOURCE_EXHAUSTED`,
  `INVALID_ARGUMENT`, `PERMISSION_DENIED`) is the classifier an SDK
  branches on and lifts to `Error::Upstream.upstream_type`; the
  numeric `error.code` lifts (stringified) to `upstream_code`. Handled
  by `parse_gemini_error_classifier` / `map_gemini_upstream_error` in
  `crates/routectl-providers/src/gemini/mod.rs`, which uses
  `Error::upstream_full` so the classifier survives to the ingress
  (rather than the generic collapse the older `upstream_with_retry_after`
  path produced) while still parking the provider on a `Retry-After`
  reset hint for rate-limit statuses. The `status` field name is
  Gemini-specific -- the OpenAI / Anthropic family names its
  classifier `error.type`.

## xAI (Grok) OAuth surface

- **Redirect URI must be literal `127.0.0.1`, not `localhost`.** xAI's public
  PKCE client registers `http://127.0.0.1:56121/callback` exactly. A callback
  server that binds on `localhost` resolves to `::1` on dual-stack hosts,
  producing a redirect-URI mismatch. The xAI provider's
  `manual_redirect_url()` override hard-codes `http://127.0.0.1:56121/callback`
  regardless of the actual bind address.

- **Fixed callback port 56121 -- no fallback port.** The codex flow registers
  both port 1455 and 1457 as fallbacks; xAI registers only 56121. The xAI
  provider's `callback_port_candidates()` override returns `[56121]` only. If
  that port is busy the login fails with a bind error rather than silently
  using an unregistered port and receiving a redirect-URI-mismatch 400 from
  xAI.

- **Lazy refresh rotation.** xAI's token endpoint routinely omits
  `refresh_token` from a successful refresh response. The prior refresh token
  remains valid and is re-used. The `decode_token_response` path passes
  `prior_refresh: Some(...)` so `map_to_record` falls back to the prior token
  when the response body omits a new one.

- **Status-gated `invalid_grant`.** xAI maps a dead refresh token to
  `{"error":"invalid_grant"}`, but only on 400 or 401. A 5xx body carrying the
  same error string is a transient fault; `check_status_error` in
  `crates/routectl-auth/src/oauth/providers/xai.rs` gates `RefreshExpired`
  on the status code (`400 || 401`) AND the error string, so a 503 with
  `invalid_grant` does NOT terminate the credential.

## claude-code's hardcoded URL bypasses

- claude-code performs `WebFetch`, `WebSearch`, `PushNotification`,
  `RemoteTrigger`, `ShareOnboardingGuide`, the `/login` OAuth flow,
  and MCP server connections against hardcoded URLs that ignore
  `ANTHROPIC_BASE_URL`. routectl never sees these requests --
  capturing them at the gateway layer alone is incomplete. Pair
  routectl with a network-level proxy (mitmproxy, a side-channel
  HTTP intercept) if full claude-code egress capture is required.

## chatgpt-oauth client-identity surface

The `openai-responses` provider in `chatgpt-oauth` mode mimics the
codex CLI client header contract (operator-facing summary in
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) "chatgpt-oauth"). This
section is the contributor-facing maintenance contract.

**Pinned default** (source of truth for the baked-in `codex_version`):

- Tag: `rust-v0.145.0` (most recent codex Rust release tag at adoption)
- Commit: `1635de866c61d1b76e50b31928ee6d61482435a8`
- Source: the `version` field on `codex-rs/cli/Cargo.toml` in the
  upstream codex repo (workspace `version = "0.0.0"` is the
  tip-of-tree dev placeholder; the tag pin above is what routectl
  encodes against)

Keep this pin in sync with the codex CLI version routectl targets by
default; the `codex_version` config knob is the per-deployment escape
hatch, not a replacement for bumping the pin when the target moves.

**Headers that MUST stay in lockstep with codex** (any deviation
breaks the client-compatibility contract):

| Header                              | Source in codex-rs                                            | Notes                                                                |
|-------------------------------------|---------------------------------------------------------------|----------------------------------------------------------------------|
| `Authorization`                     | injected by routectl's auth layer per request                 | OAuth bearer JWT (`Bearer <jwt>`); resolved per request from the token store. Redacted to `Bearer [REDACTED]` in TRACE-level outgoing-headers logs. NOT a pinned constant -- the value rotates on every refresh -- but the header is mandatory and absence triggers a 401 immediately. |
| `ChatGPT-Account-Id`                | injected by routectl's auth layer per request                 | The `chatgpt_account_id` claim parsed out of the bearer JWT; mandatory account-routing header. Stable per account. |
| `User-Agent`                        | `login/src/auth/default_client.rs::get_codex_user_agent`      | `<originator>/<build_version> (<os_type> <os_version>; <arch>) <terminal>`; build_version is `CARGO_PKG_VERSION` of the codex binary, not routectl's. |
| `originator`                        | `login/src/auth/default_client.rs::DEFAULT_ORIGINATOR`        | Constant `"codex_cli_rs"` for first-party CLI traffic.               |
| `version` / per-request build tag   | passed through `CodexRequestBuilder` per call                 | Matches the targeted codex CLI build version.                        |
| `session_id`                        | `core/src/client.rs::ModelClientState`                        | Stable per process; never reset within a routectl process lifetime.  |
| `x-codex-installation-id`           | `core/src/client.rs::X_CODEX_INSTALLATION_ID_HEADER`          | Stable per install (persisted under `~/.config/routectl/`).          |
| `x-codex-window-id`                 | `core/src/client.rs::X_CODEX_WINDOW_ID_HEADER`                | Per-window correlation; codex bumps on each new shell window.        |
| `thread-id`                         | `core/src/client.rs::X_CODEX_PARENT_THREAD_ID_HEADER` family  | Per-conversation; new value on every fresh `ChatRequest`.            |
| `x-client-request-id`               | `codex-api/src/endpoint/responses.rs:92`                      | Per-request UUID; carries the `thread_id` for upstream correlation.  |
| `x-openai-internal-codex-residency` | `login/src/auth/default_client.rs::RESIDENCY_HEADER_NAME`     | Set when `--residency us` is configured; absent otherwise.           |

The OAuth refresh client (used for `grant_type=refresh_token` POSTs to
`https://auth.openai.com/oauth/token`) carries its OWN header set
distinct from the responses-API client; both must carry the pinned
codex client headers.

**Defense-in-depth for the bearer JWT**: the `authorization` header on
every outgoing request to `chatgpt.com/backend-api/codex` carries an
OAuth access-token JWT that embeds `chatgpt_account_id`, `email`,
`session_id`, `jti`, and `plan_type`. routectl's outgoing-headers
TRACE log redacts this value to `Bearer [REDACTED]` before any line is
emitted (`routectl_core::log_safe::redact_outgoing_header_values`);
the same redaction applies to `x-api-key` and `proxy-authorization`.

**Cross-link**: when adjusting any of the above, compare against the
corresponding upstream source paths under the codex repo's `codex-rs/`:

- Auth client and originator: `login/src/auth/default_client.rs`
- Cookie jar:                  `codex-client/src/chatgpt_cloudflare_cookies.rs`
- Header constants:            `core/src/client.rs`
- Responses-API request:       `codex-api/src/endpoint/responses.rs`

**Compatibility-contract risk**: future deviations (a UA bump on the
routectl side that codex did not ship; a missing identity header; a
refresh-client header re-ordering) are NOT debuggable as build / wire
errors. The first symptom is a refresh endpoint that 401s on every
retry. A mismatch with the targeted codex client contract can cause
the upstream to reject the refresh token and require the operator to
re-authenticate through the ChatGPT web UI. Treat any change here with
the same gravity as a database migration on a production system.
