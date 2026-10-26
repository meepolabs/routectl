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

- **Reasoning models refuse sampling parameters one per 400.** OpenAI
  answers `Unsupported parameter: '<name>' is not supported with this
  model.` with `error.param` set to the one parameter it refused, even
  when the body carries several. For `temperature`, `top_p`,
  `presence_penalty` and `frequency_penalty` the refusal resolves to a
  field-repair drop row: one retry without the parameter, learned for
  the lane when the retry succeeds; a body carrying several refused
  parameters is repaired one parameter per retry. Ids the OpenAI
  chat-lane rule matches never reach that refusal:
  `uses_modern_chat_shape` in `model_profile.rs` (the id, lowercased
  with at most one leading `openai.` removed, starts with `gpt-`, `o1`,
  `o3` or `o4`, or the lowercased id contains `reasoner`) drops
  `SAMPLING_DROP` (`openai_compat/dialects/util.rs`) before send, and
  the OpenAI dialect also sends `max_tokens` as `max_completion_tokens`.
  Every `gpt-` id matches, `gpt-4o` and `gpt-oss-*` included. The rule
  is static: no learned verdict renames a field and `logprobs` /
  `top_logprobs` have no row. It acts only in the `openai` and
  `deepseek` dialects (DeepSeek drops, never renames); `vllm`,
  `raw-think-tag`, `openrouter` and `passthrough` leave the body alone.
  Newer OpenAI models (gpt-5.6 and later) are better served on a
  Responses lane.
  The grounding envelopes are under
  `crates/routectl-router/tests/fixtures/rejections/openai-compat/`.

- **Client extras forward only from the Chat ingress.** A Chat client's
  unmodeled top-level fields (`metadata`, `service_tier`, `prediction`,
  ...) are merged onto the Chat body as sent; there is no key blocklist
  to keep current, so a strict host that refuses one answers with its
  own 400. Fields an Anthropic or Responses client sent are spelled for
  another vendor and never reach this wire: they count once per request
  as the `foreign_dialect_extra` drop class, except such a client's
  `metadata` identity block, which counts as
  `client_fingerprint_stripped` (`select_client_extras` in
  `openai_compat/request.rs`). The operator's `payload_extras` always
  ship. One field never does, whoever wrote it: `mcp_servers`, whose
  entries can carry a token issued for a remote MCP server, is removed
  as the last mutation of the body and counted as the
  `credential_bearing_field_withheld` policy action
  (`drop_credential_bearing_fields`).

- **A stream without `include_usage` ends with no usage.** Many
  openai-compat hosts close a stream on a bare `[DONE]`, with no
  terminal usage chunk or `finish_reason`, unless the body asks for
  `stream_options.include_usage: true`. The egress sets it on every
  streaming request after merging extras (`force_stream_include_usage`),
  overriding a client's `false` or non-object `stream_options` as a
  declared edit counted once as the `client_stream_include_usage_overridden`
  policy action. Only an operator `payload_extras` value for
  `stream_options.include_usage` is left as configured.

## Anthropic API surface

- **A mispaired tool transcript is forwarded; of its pairing defects only
  a Gemini target refuses one.** A transcript whose tool calls and tool
  results carry ids but do not pair up (a result answering no call, a call
  left unanswered, a duplicated result) reaches every non-Gemini target
  unchanged: routectl neither
  synthesizes nor drops a turn. A tool result with an empty or absent id
  that follows a tool call turn is such a defect too
  (`empty_result_id`): it cannot name the call it answers, and the
  Converse and Responses egresses give an id-less call a synthesized
  `call_<index>` id (`normalize_tool_calls`) while forwarding the result's
  id empty, so the two never match upstream. A call with an empty id and no result is
  not a defect. `record_transcript_defect` in
  `crates/routectl-router/src/router/transcript_defect.rs` validates the
  pairing once per request and keeps the defect as a request fact. A
  Gemini target refuses locally (`tool-pairing`, `egress_pairing_refusal`)
  because its `functionResponse` must name the call it answers, and the
  chain moves on. An upstream 400 or 422 on a defective transcript
  (`is_transcript_rejection`) is the client's defect, not the lane's: it is
  not retried, does not walk the chain, is not field-repaired, and teaches
  no learned capability; it counts once as unlocalized. The one exception,
  on complete and stream only: when the rejection names one declared
  routectl edit (an operator payload extra or an auto-placed cache marker),
  routectl undoes it and retries the same target once. The anthropic-api
  egress still refuses a `Role::Tool` result turn with no id, wherever it
  sits (`tool-call-id-missing`, `normalize_replay_invariants` in
  `crates/routectl-providers/src/anthropic_api/messages.rs`), the one
  egress that refuses a missing tool id: it builds that turn's
  `tool_result` block from the id. A client-authored `tool_result` content
  part is relayed verbatim, so an empty or absent `tool_use_id` there is
  forwarded for the upstream to judge. On the Anthropic ingress tool
  results arrive only as content parts, so the refusal fires only for a
  request from a translating ingress (chat completions, responses).

- **A local egress refusal walks the fallback chain, except a missing
  forwarded bearer.** A refusal raised while translating for one target
  (`tool-pairing`, `tool-call-id-missing`, `unrepresentable`,
  `strict-translation` and the other egress reasons) moves the request to
  the next target in the chain, since that target may hold the shape. A
  request bound for a forwarded-credential target that carries no bearer
  (`forwarded-bearer-missing`) ends instead: it is never retried on
  another target.

- **A canonical field that duplicates a wire field creates a both-present
  case, and a translation arm written as "already handled elsewhere"
  deletes content when the elsewhere did not run.** `messages[]` accepts a
  mid-conversation `role: "system"` turn (position-gated: it must precede an
  `assistant` turn or end the array; also model-gated; validation order is
  shape -> position -> model support). The canonical request ALSO carries a
  top-level `system`, and real clients send both at once. The egress
  computed the wire `system` as `req.system` OR the lift of `Role::System`
  messages, which encodes an exclusive choice, while the per-role walk
  dropped those same turns on the premise the lift had consumed them --
  so with both present neither branch owned them and they vanished with no
  log line. Two general lessons: an `.or_else()` between a canonical field
  and a wire-shape fallback asserts XOR, so state whether the inputs are
  actually exclusive; and a drop arm justified by another pass must name
  the condition under which that pass runs, because the justification is
  a premise, not a fact. The discriminator now lives in ONE place
  (`SystemTurnPolicy`, resolved from canonical-`system` presence in
  `crates/routectl-providers/src/anthropic_api/request.rs::normalize` and
  threaded into the walk), and the walk carries an accounted-identity
  ledger so any future unaccounted drop is a hard error.

- **The directive-only system form cannot round-trip canonical.**
  Anthropic accepts a system turn of `content: []` plus `output_config` at
  ANY position (the position gate does not apply to it). Canonical
  `Message` (`crates/routectl-core/src/schema.rs`) has no
  `#[serde(flatten)]` catchall, so a sibling key like `output_config` on a
  message is dropped at parse time and the form cannot survive an ingress
  round-trip. Nothing is built for it -- the shape is unconfirmed beyond
  the probed error text -- but a request that needs it will silently lose
  the directive rather than fail, so this is where to start.

- **An empty or blank forwarded system turn is dropped, not forwarded.**
  After an idle compaction Claude Code sends an in-band `role: "system"`
  turn with no content (`content: []`, `content: ""`, or only blank text
  blocks) next to a top-level `system`, which puts the walk on the forward
  policy. `screen_forwarded_system_content` returns
  `SystemTurnVerdict::DroppedEmpty` for it and `emit_forwarded_system_turn`
  in `crates/routectl-providers/src/anthropic_api/messages.rs` omits the
  turn, merges the neighbors the omission leaves adjacent, carries its cache
  breakpoint over the seam, and declares the drop to the wire check. The
  recorded verdict is a lossless repair: the turn carries no content, so
  nothing the model would see is lost, and the session continues on both
  the complete and the stream leg with no local refusal (pinned in
  `crates/routectl-cli/tests/anthropic_ingress.rs` by
  `empty_in_band_system_turn_is_omitted_not_refused`,
  `empty_cached_system_turn_between_user_turns_merges_and_keeps_breakpoint`,
  `empty_in_band_system_turn_is_omitted_on_the_stream_leg` and
  `a_session_across_an_empty_system_turn_survives_without_a_local_refusal`). Whether
  Anthropic itself accepts an empty system turn is NOT grounded: no
  committed envelope records its answer to this shape, and what is
  recorded about blank content concerns empty text blocks, a different
  shape. A live capture that gets a 200 for
  the empty turn would let a follow-up remove the drop and forward the
  turn verbatim; until then the drop stays.

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
  `stream`, so it catches the caller's value, the 1.0 the thinking
  clamp rewrites a client temperature to, and anything a later pass
  could add. This strip is a shipped seed, not a learned verdict: the
  seat refuses either key, one per 400, and field repair makes one
  plan per request, so a request sending both could never be repaired.
  Three notes for whoever debugs this next:
  - The gate is the LANE predicate `is_cloak_lane` (oauth-bearer + exact
    `api.anthropic.com` host + not the forwarded leg), deliberately NOT
    the cloak mode. Gating on the cloak flag would let
    `cloak.mode = "never"` re-introduce a lane-wide 400 -- the rejection
    is a property of the credential, not of the disguise.
  - `stop_sequences` is NOT stripped: probed and accepted (200). Only
    the two alternative-continuation sampling knobs are rejected.
  - `top_k` is the sibling hole. `reserved.rs` treats it as
    non-canonical pass-through, so it is the one sampling param an
    Anthropic client's `provider_extras` can still put on the wire (a
    Chat client's `top_k` is a foreign-dialect extra and never reaches
    this egress). No confirmed 400 today, so nothing strips it: if a
    `top_k` request starts failing on the OAuth lane, this is the first
    place to look.

- **`max_tokens` is required, so routectl fills it, and the fill can be
  refused.** The Messages API 400s a body without `max_tokens`, so the
  `anthropic-api` egress (Mantle included) and Bedrock Invoke send the
  client's value, else the per-model ceiling the router resolves (the
  catalog prior, lowered by a learned ceiling), else 64000
  (`resolve_max_tokens` in `anthropic_api/extras.rs`). The fill is
  declared as a required-field default. A model whose output limit is
  lower answers `max_tokens: SENT > N, which is the maximum allowed
  number of output tokens for <model>`, on Messages and on Invoke
  alike. That refusal resolves to the `max_tokens` ceiling row: one
  retry with the value lowered to N, learned as the lane's ceiling
  when the retry succeeds. Because a declared default cannot be undone
  (dropping a required field trades one 400 for another), a refusal
  that matches it still reaches field repair instead of stopping at
  the declaration. Lowering the fill writes the per-model ceiling slot,
  not the client's `max_tokens`, so the egress keeps declaring its own
  default, the legacy thinking budget is fitted under the lowered
  value, and the repair is lossless (`field_ceiling_fill_repair`).
  Lowering a client value is lossy (`field_ceiling_repair`). On a
  Bedrock Mantle sub-lane field repair acts only on the rows a refusal
  captured on that sub-lane grounds: this ceiling on the Anthropic lane;
  the `top_p`, `presence_penalty` and `frequency_penalty` drops on the
  Chat Completions lane; the `minimal` and `max` effort levels on the
  Responses lane. Chat Completions `reasoning_effort` and `temperature` (refused
  with `unsupported_value`) are not admitted there.

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
  deprecation message. The egresses need no extra translation for the
  legacy-vs-nested question: Bedrock-Invoke for Claude is Anthropic-shape
  passthrough, and the Anthropic-API egress already merges
  `provider_extras["output_config"]` into the body. The format object's
  MEMBER SET is a separate constraint -- see the next entry.

- **Anthropic `output_config.format` accepts ONLY `type` and `schema`.**
  Measured against the live wire 2026-08-11: `{"type":"json_schema",
  "schema":{...}}` is accepted (HTTP 200), while adding `name` or
  `strict` is rejected with HTTP 400 `output_config.format.name: Extra
  inputs are not permitted` (same for `.strict`). `{name, schema,
  strict}` is the CONVENTIONAL OpenAI structured-output shape every
  OpenAI-compatible client emits, so this hit the common path, not an
  edge case. `drop_unrepresentable_output_format_keys` in
  `crates/routectl-providers/src/anthropic_api/request.rs` omits both
  keys with one structured WARN (`dropped_name` / `dropped_strict`
  booleans; never the caller's `name` VALUE, which is
  caller-controlled). The caller's `schema` ships unchanged -- routectl
  does NOT strip JSON-schema keywords Anthropic rejects per-type
  (`minimum` on an integer, etc.); those get a truthful upstream 400
  naming the field rather than a silent constraint loss.
  Applied on ALL THREE Claude seams (Anthropic egress, Bedrock-Converse
  bag, Bedrock-Invoke) because AWS forwards the field verbatim to an
  Anthropic validator; Bedrock acceptance itself is unmeasured and a
  probe item is filed. The drop reads the ASSEMBLED body, not just the
  `response_format` converter: an Anthropic client's own
  `output_config.format` WINS over the converter and would otherwise
  never be reshaped. On the Anthropic egress and Invoke, `output_config`
  is not a routectl-managed key, so the client's object rides through
  `provider_extras`; on Converse it is a managed bag key, so only the
  client's `format` member is merged into the `output_config` routectl
  composes (`insert_client_output_format` in
  `bedrock/converse/extras.rs`). Either way the client value is read only
  from the Anthropic ingress or a library caller. A converted format's
  lost `name` / `strict` is counted once per request as the
  `response_format_keys_unrepresentable` drop class on all three seams;
  a conversion that loses to the client's own format counts nothing.

- **Forward-compat sweep on the Anthropic ingress.** Anthropic adds
  new top-level body fields on a quarterly cadence (e.g. recent
  additions: `context_management`, `context_hint`, `speed`,
  `diagnostics`, `mcp_servers`). Without an explicit pull-out, serde
  on `ChatRequest` would silently drop them at the ingress boundary.
  `translate_request` sweeps every key `is_canonical_request_key`
  (`crates/routectl-core/src/reserved.rs`) does not claim into
  `provider_extras`, the same predicate the OpenAI Chat ingress sweeps
  with, so the two dialects agree on what an extra is. A swept key is
  forwarded verbatim only to an egress that speaks the client's
  dialect: Anthropic-ingress extras reach anthropic-api, Invoke and the
  Converse bag, Chat-ingress extras reach openai-compat, and any other
  pairing drops the key and counts it once per request as
  `foreign_dialect_extra` (`managed_keys::client_extras_for`). When
  canonical adds a new field, add it to `is_canonical_request_key`.

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

- **The Anthropic `thinking` object and `output_config.effort` forward
  as sent; canonical reasoning cannot represent them.** The canonical
  `ReasoningConfig` has no way to say `adaptive` without a budget,
  `between_tools`, an unknown `type`, or "the client sent no thinking", so
  rebuilding the object from it changed what the client asked for. The
  Anthropic ingress keeps the client's whole `thinking` object (minus
  `display`, which has its own carrier) on
  `routectl_internal.anthropic_thinking`, and every Anthropic-shape egress
  emits it through one function,
  `anthropic_api/extras.rs::compose_thinking` (Bedrock Converse via
  `bedrock/converse/extras.rs::insert_thinking`):
  - **The object ships verbatim, unknown types included.** Any `type`
    and any member reach anthropic-api, Bedrock Invoke, Bedrock Converse
    and `count_tokens` unchanged; the upstream judges them
    (`client_thinking_shapes_forward_verbatim`,
    `bedrock_invoke_body_carries_the_forwarded_thinking`,
    `count_tokens_body_carries_the_forwarded_thinking`,
    `the_client_thinking_object_reaches_the_bag_as_sent`; known types stay
    byte-identical to the committed driver captures,
    `committed_driver_captures_reach_the_wire_byte_identical`). No config
    key edits it any more (see
    [CONFIGURATION.md](CONFIGURATION.md#removed-model-keys)); a forcing
    `tool_choice` and a learned thinking repair are the only edits, and
    each is declared.
    A native `budget_tokens` below Anthropic's 1024 minimum is NOT raised:
    the upstream judges it, where a budget routectl translates from
    canonical reasoning is still floored
    (`a_sub_floor_budget_is_forwarded_on_sent_and_floored_on_translation`).
    An unknown type cannot cross dialects: openai-compat,
    openai-responses and gemini build their reasoning from the default and
    count one `thinking_type_unrepresentable` translation drop per request
    (`an_unmodeled_thinking_type_counts_one_openai_compat_drop`,
    `an_unmodeled_thinking_type_counts_one_openai_responses_drop`,
    `an_unmodeled_thinking_type_counts_one_gemini_drop`).
  - **Effort ships as sent, beside any thinking type.** A client's
    `output_config.effort` is not dropped beside `disabled` or `enabled`
    thinking, and `"none"` is not turned into `disabled` thinking. The
    committed capture of a real Claude Code turn carries `disabled`
    thinking beside effort `"high"`, and it ships unchanged
    (`the_plain_turn_ships_its_effort_with_no_output_config_edit`,
    `the_plain_turn_ships_its_effort_on_bedrock_invoke`,
    `the_plain_turn_capture_keeps_its_effort_beside_disabled_thinking`).
    No config key re-clamps it; only a learned effort repair rewrites a
    level the lane refused.
  - **The sampling clamp reads the shipped type.** Anthropic accepts
    only `temperature: 1.0` and no `top_p` while thinking is active, so
    the clamp keys off the emitted `thinking.type`, never the canonical
    config: under `enabled` and `adaptive` a client temperature is
    rewritten to 1.0 and `top_p` is dropped, and a request with no client
    temperature ships none (1.0 is the default); `disabled`,
    `between_tools` and an unknown type keep the caller's sampling
    (`the_clamp_rewrites_only_a_client_temperature_under_active_thinking`,
    `active_thinking_without_a_client_temperature_ships_no_temperature`,
    `the_body_keeps_caller_sampling_for_inactive_forwarded_types`).
  - **Forced tool use strips only active thinking, as a seed.** Anthropic
    400s with "Thinking may not be enabled when tool_choice forces tool
    use." `anthropic_api/extras.rs::shipped_thinking` (and Converse's
    `strip_thinking_when_tool_choice_forces_use`) removes `thinking` when
    its type is `enabled` or `adaptive` (`is_active_thinking_type`) and
    `tool_choice` forces a tool (`anthropic_api/tools.rs::forced_tool_choice_type`),
    keeping `tool_choice`; the removal is declared once
    (`forced_tool_use_strips_active_thinking_with_one_declaration`,
    `forced_tool_use_strips_active_thinking_once_and_restores_the_caller_sampling`).
    `disabled`, `between_tools` and unknown types ride beside the forced
    choice as sent (`forced_tool_use_keeps_inactive_thinking_and_the_caller_sampling`,
    `forced_tool_use_keeps_inactive_thinking_with_the_client_effort`).
    A client's own effort survives the strip; only an effort routectl
    composed for the stripped thinking is dropped
    (`a_forced_tool_strip_keeps_the_client_effort_and_composes_none`,
    `forced_tool_use_drops_only_a_routectl_composed_effort`). The strip is
    baked in rather than learned because the rejection names no field
    path, so the rejection parser cannot attribute it, and no envelope of
    it is captured to ground a learned repair. Its Claude Code trigger is
    the WebSearch sub-request, which forces `web_search` beside adaptive
    thinking.

  A thinking type the upstream refuses (`thinking.type.enabled` on Opus
  4.7+, `thinking.type.disabled` on Opus 5.5 and Bedrock Sonnet 5.5) is
  no longer prevented up front: the router repairs it per lane through
  `FieldSurface::AnthropicThinkingType` in
  `crates/routectl-router/src/router/field_repair.rs`, reading the
  Bedrock off-switch template with `rejection_parser.rs::thinking_off_switch`.
  See [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#anthropic-opus-47-and-any-future-adaptive-thinking-models)
  for what each repair sends.

- **A thinking-suppressing `thinking.display` returns an EMPTY thinking
  block, not no block.** The `display` vocabulary is beta-extensible:
  `"summarized"` and `"omitted"` are the long-standing values, and the
  `thinking-display-updates-2026-08-18` beta added `"updates"`. routectl
  therefore does NOT police the value -- any string forwards verbatim to
  the wire and upstream rules on it
  (`ingress/anthropic/parse.rs::translate_thinking_display`); only a
  non-string `display` earns a local 400, because the carrier that
  preserves the literal is a string. Under a suppressing value
  (`"omitted"`, and `"updates"` alike) Anthropic still emits a `thinking`
  block -- with empty `thinking` text and a FULL signature -- both
  non-streaming and streaming. Billing is unchanged (thinking tokens are
  still charged). Two consequences routectl depends on:
  - The empty-text-plus-signature block must survive normalization and
    replay. Every drop/skip decision on the thinking path keys on the
    SIGNATURE, never on the text
    (`anthropic_api/response.rs::walk_content_blocks`,
    `anthropic_api/messages.rs::is_anthropic_emittable_detail`), so an
    omitted-display turn stays replayable.
  - Streaming sends `content_block_start` + `signature_delta` +
    `content_block_stop` with NO `thinking_delta` at all. The SSE
    decoder's empty-block drop guard requires empty text AND an absent
    signature, so this shape still emits its aggregated detail
    (`anthropic_api/sse.rs`).

  The value the Anthropic ingress captured rides a raw-string carrier
  (`routectl_internal.anthropic_thinking_display`) and the `anthropic-api`
  egress stamps it back verbatim, so a value the canonical model does not
  know still reaches upstream intact. The canonical
  `reasoning.exclude` boolean is the fallback derivation for library
  callers and other ingresses, which set only the boolean (`Some(true)`
  -> `"omitted"`, `Some(false)` -> `"summarized"`); the known suppressing
  values map onto `exclude = Some(true)` on the way in. An ABSENT display
  stays absent on the wire: the upstream default is model-dependent, so
  emitting an explicit value would override a newer model's own choice.
  The Bedrock Converse egress forwards `display` verbatim in the
  `additionalModelRequestFields` bag too, adding the
  `thinking-display-updates-2026-08-18` beta for `"updates"`
  (`bedrock/converse/extras.rs::insert_thinking`).

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

- **`message_start` input usage rides a transport-internal carrier,
  not canonical `usage`.** Anthropic reports the input side of usage
  (`input_tokens` plus the disjoint `cache_creation_input_tokens` /
  `cache_read_input_tokens` / per-TTL `cache_creation`) once, in
  `message_start`. The egress copies it verbatim onto the content-free
  opening role chunk as the skip-serialized `upstream_meta.opening_usage`
  (`anthropic_api/sse.rs`); Bedrock InvokeModel inherits it through the
  shared parser. It must NOT be set on `ChatChunk.usage`: OpenAI-shape
  ingresses serialize that field onto the client's first frame, and usage
  accounting reads it as cumulative counters, so a truncated stream would
  ledger input-only numbers. The opening value can legitimately differ from
  the terminal `message_delta` usage (server-side tools add input
  mid-response). The router's pre-content buffer replays it only for the
  attempt that reaches first content; a failed attempt's opener is dropped
  with its buffer. The Anthropic ingress renders it into the client
  `message_start` only when the dispatch resolved inside the 2500ms flush
  grace; after the grace the stream has already opened with an estimate
  (session anchor, calibrated or raw), so the later opener is not rendered
  and the terminal `message_delta` carries the real count.

- **A terminal prompt count is not proof of terminal measurement.** The
  Anthropic parser backfills the closing chunk's input and cache fields from
  `message_start` when `message_delta` reports output only, and Gemini folds
  an interim `usageMetadata` onto its finish chunk. Each parser therefore
  stamps `upstream_meta.usage_input_source` beside the usage it emits:
  `ExplicitFinal` when the closing event itself reported the input,
  `VendorOpening` for a backfill from the first-party API host or a Bedrock
  InvokeModel stream, `ProxyOpening` for a backfill from any other
  Anthropic-compatible `base_url`, `InterimCarry` for Gemini's carry. A
  closing delta that reports some input components but not all counts as a
  backfill. A delta that reports any positive input or cache component is
  an input report, and every component it states is authoritative, zero
  included (zero uncached input beside a cache read is a fully cached
  prompt); only a delta with no positive input component has its zeros
  treated as placeholders. `ExplicitFinal` additionally requires the delta
  to state `input_tokens` itself (an absent cache field reads as zero); a
  delta stating only cache components, with nothing positive carried over,
  is `PartialFinal`. Only `ExplicitFinal` and `VendorOpening` anchor
  the context meter, and `ExplicitFinal` from a compatible proxy (a routectl
  back hop included) is what that immediate upstream reported, not proof of
  its backend's measurement. Client usage and ledger accounting read
  `usage` unchanged.

## Bedrock surface

- **Converse forwards what its wire types can hold.** A value the
  Converse type has a slot for is forwarded even when the upstream may
  object: an empty or absent image or document media type (an empty
  `format`), an empty payload, an empty data-URI payload, an empty or
  absent reasoning signature, and a tool result without a tool id
  (`toolUseId: ""`). Only a value with no slot is refused locally
  (`unrepresentable`): a source that is not an object, a source with no
  type, an image url source (`ConverseImageSource` carries bytes only),
  and a non-string media type or payload. Handled by
  `translate_image_source`, `translate_document` and `build_tool_message`
  in `crates/routectl-providers/src/bedrock/converse/messages.rs`; pinned
  by `empty_image_media_type_forwards_as_empty_format_on_every_carrier`,
  `empty_document_media_type_forwards_as_empty_format_on_every_carrier`,
  `image_url_data_uri_with_empty_payload_forwards_as_empty_bytes`,
  `thinking_block_signature_forwards_verbatim_when_absent_or_empty` and
  `tool_message_without_id_forwards_an_empty_tool_use_id`.

- **Converse refuses `temperature` and an over-limit `maxTokens` with
  fixed messages that name no path.** Both arrive as HTTP 400 before
  generation in the flat `{"message": ...}` body, with no Converse
  model-errors wrapper: `This model doesn't support the temperature
  field. Remove temperature and try again.` and `The maximum tokens you
  requested exceeds the model limit of N. Try again with a maximum
  tokens value that is lower than N.` Because neither names a field,
  `rejection_parser.rs` matches each template verbatim (a near-miss
  wording localizes nothing) and maps it to a const path: the first to
  the `temperature` drop row, the second to the `max_tokens` ceiling
  row. N is read from both positions; it counts only as canonical
  digits of a nonzero `u32` and only when both copies agree, and the
  repair applies it only when it is below the value sent, so a limit at
  or above it makes no retry. Converse builds `inferenceConfig.maxTokens`
  from the client's `max_tokens` alone (`build_inference_config` in
  `bedrock/converse/request.rs`) and sends none when the client sent
  none, so only a client value is ever clamped there, and the clamp is
  lossy. The per-model ceiling slot still sizes the legacy thinking
  budget in the bag
  (`the_converse_thinking_budget_fits_the_operator_ceiling_with_no_client_limit`).
  The grounding envelopes are
  `crates/routectl-router/tests/fixtures/rejections/bedrock/temperature-field-unsupported.json`
  and `max-tokens-above-model-limit.json` beside it. Invoke's refusal is
  the Anthropic `max_tokens: SENT > N` form (see the
  [Anthropic API surface](#anthropic-api-surface)).

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

  routectl forwards both surfaces as-is: there is no allowlist of betas
  or body fields to keep current, so a flag or field AWS later accepts
  needs no routectl release. A few entries are still filtered:

  - the router withholds, per lane, its shipped seed of Bedrock-rejected
    client betas and any beta flag it learned the lane rejects, and
    every client beta from a Converse model whose id is provably not
    Anthropic (such a model refuses the `anthropic_beta` bag key itself,
    and `output_config` too, which the Converse egress omits for it);
  - a rejection naming its client betas is repaired by the router's
    learning engine (`router/learn_beta_repair.rs`): one re-send without
    those flags, drawn from the request's shared repair budget, which
    teaches the lane those flags once an attempt without them succeeds;
  - a body field a Chat or Responses client sent is dropped as a
    foreign-dialect extra;
  - `mcp_servers` never ships, and neither does a `tool_choice` left
    with no non-empty `tools`.

  Use `ROUTECTL_LOG=routectl_providers::bedrock=trace` to capture what
  is sent (see
  [CONFIGURATION.md](CONFIGURATION.md#bedrock-which-client-betas-reach-aws)).
  The withhold decision lives in the router (`router/beta_withhold.rs`),
  the carriers apply it through `beta_withhold.rs` and `bedrock/betas.rs`,
  and the body-field drops live in `bedrock/body_fields.rs`. The withholding and the `mcp_servers` drop
  apply on both Invoke (top-level Anthropic body) and Converse
  (`additionalModelRequestFields` bag); the orphan `tool_choice` drop is
  Invoke-only. Drops log at
  DEBUG (not WARN -- the SDK reliably sends a handful of rejected
  entries per request and WARN would flood `routectl-warn.log`).

  Per-provider escape hatch -- `[providers.X] anthropic_beta = [...]`:
  those flags are always sent, once each (operator-asserted). Together
  with `header_extras`-pinned betas it always sends a seeded or
  learned-rejected flag, and the beta repair never withholds it.

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
  `UpstreamErrorRead` in `crates/routectl-providers/src/upstream_error.rs`
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

- **Converse demands a `toolConfig` as soon as the transcript carries a
  `toolResult`.** A request whose `messages` include a `toolResult`
  content block but whose body omits `toolConfig` is rejected with
  `The toolConfig field must be defined when using toolUse and
  toolResult content blocks.` -- a Converse-API-level
  missing-required-field check, measured with no `toolConfig` sent. So
  when no caller tool defs survive translation (none supplied, an empty
  list, or every entry was an Anthropic builtin that dropped),
  `bedrock/converse/tools.rs` backfills exactly one reserved
  `toolSpec` named `routectl__history_compat_noop` with an
  empty-object input schema, and leaves `toolChoice` absent (a forcing
  choice would compel a nonsensical call; absent means Converse's own
  `auto`, so the model MAY select the dummy -- the response and stream
  lanes recognize the reserved name). The injection logs a WARN and is
  never silent.

  Triggering condition: at least one `toolResult` block, zero surviving
  tool defs, and `tool_choice` not `"none"`. Two deliberate
  non-triggers:

  - **A lone `toolUse`** (no following `toolResult`) does NOT trigger
    it. That shape is rejected by a different, model-level pairing
    check, whose message carries the `The model returned the following
    errors:` prefix and names the offending id. No `toolConfig` can
    repair a pairing error, so injecting there would mutate the request
    with no possible benefit. The two classes are distinguishable on
    the wire by that prefix; if AWS ever merges the validators, the
    predicate needs revisiting.
  - **`tool_choice: "none"`** leaves the shape KNOWN UNREPAIRED: it
    still gets no `toolConfig` and AWS still rejects it. Converse has
    no native "none" mode, so shipping a `tools` array would permit
    auto-selection of a tool the caller explicitly forbade. Violating
    stated caller intent is worse than the rejection.

  What the backfill buys is precise: routectl stops omitting a
  `toolConfig` the wire requires. It does not promise the request then
  succeeds -- an unpaired `toolResult` carrying a valid `toolConfig` is
  an unmeasured cell, and the same pairing check that governs a lone
  `toolUse` may still reject it.

- **The mantle Anthropic lane's model id is VENDOR-PREFIXED and
  PUBLIC-NAMED, and the working cell is region-specific.** The mantle
  lane's id is NOT the Bedrock version-suffixed inference-profile id the
  native lanes use, and it is NOT bare. Measured 2026-08-12 direct to AWS
  (SigV4 scope `bedrock-mantle`, Anthropic Messages body):
  `anthropic.claude-haiku-4-5` in **us-east-1** returns 200 and the
  response echoes `model: claude-haiku-4-5-20251001`, while the bare
  `claude-haiku-4-5`, the version-suffixed
  `anthropic.claude-haiku-4-5-20251001-v1:0`, and every one of those
  spellings in us-east-2 return 404 `not_found_error`. An earlier comment
  in the live vehicle claimed the id must be bare because the lane
  "carries the model verbatim" -- that was inference, never wire-verified,
  and it is false. Whether us-east-2 offers a differently-named Anthropic
  model is unswept. Pinned by `MANTLE_MODEL` in
  `crates/routectl-cli/tests/live_matrix/mantle_anthropic.rs`.

- **The Anthropic `thinking` signature namespace is shared between the
  mantle Anthropic lane and the bedrock-native `InvokeModel` lane --
  measured, not assumed.** Same 2026-08-12 capture, on
  `anthropic.claude-haiku-4-5` / us-east-1: the lane accepts its own
  thinking signature on multi-turn replay (200), AND it accepts a
  signature minted on the bedrock-native `InvokeModel` lane in a
  DIFFERENT region (us-west-2) -- also 200. Two strictness controls prove
  those 200s are not vacuous "unrecognized field ignored" passes: a
  garbage base64 signature is rejected (`invalid_request_error`, message
  `Invalid signature in thinking block`), and a thinking block with the
  signature removed is rejected (`thinking.signature: Field required`).
  The signature is mandatory and genuinely validated.

  This does NOT add a scheme. `ReplayScheme` stays `{Codex, Mantle, Gray}`
  with no Anthropic variant: Anthropic-sourced details carry no
  Responses-family format tag (see the byte-verbatim carve-out under
  *OpenAI Responses surface*), so `scheme_of` returns `ReplayScheme::Gray`
  for them through its `_` fallthrough. That is deliberate:
  `is_replayable` leaves every Gray pair on the
  optimistic carry-once path so the learned layer settles it from a real
  upstream verdict, instead of baking a distinction this evidence does not
  yet compel into the deterministic rules. Treat the sharing as a DATED
  OBSERVATION: if AWS later diverges the two lanes' signature validation,
  that reads as new evidence for the learned layer, not as a contradiction
  of this note.

## OpenAI Responses surface

- **Responses and openai-compat forward what their wire types can hold.**
  An empty or absent tool call id reaches the body as `call_id: ""`; an
  empty or absent image url, or an empty base64 payload, forwards as an
  empty `image_url` or data URI; a file part naming no carrier forwards as a
  bare `input_file`; and the openai-compat lift carries `tool_use` and
  `tool_result` ids and names verbatim. Only a value no slot can hold is
  refused locally (`unrepresentable`): a non-object `image_url`, a
  non-string image slot or file carrier (`read_image_url`, `string_slot`,
  `file_carrier` in
  `crates/routectl-providers/src/openai_responses/messages.rs`), and a
  `tool_use` block that is not an object (`tool_use_to_tool_call` in
  `crates/routectl-providers/src/openai_compat/wire_lift/tool_use.rs`).
  One slot-holding shape is refused too: on Bedrock Mantle a user file
  whose only carrier is `file_url` (`translate_file_part`): the Mantle lane
  forwards no client-supplied file url, so a file must arrive inline or by
  file id; beside `file_data` or `file_id` the url is left off the wire.
  Pinned by `an_absent_or_empty_tool_output_id_reaches_the_body_as_an_empty_call_id`,
  `user_image_url_part_with_empty_or_absent_url_forwards_an_empty_image_url`,
  `user_file_with_no_carrier_forwards_as_a_bare_input_file`,
  `user_file_url_only_forwards_on_the_api_key_and_chatgpt_oauth_lanes`,
  `user_file_url_only_is_refused_on_the_mantle_lane` and
  `tool_result_with_absent_or_non_string_tool_use_id_forwards_it_verbatim`.

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

- **A Responses function_call can arrive with no argument deltas.** The
  chatgpt-oauth backend may stream a `function_call` item as
  `output_item.added` (empty `arguments`) followed directly by
  `function_call_arguments.done` and `output_item.done`, both carrying the
  full arguments, with no `function_call_arguments.delta` in between. The
  SSE translator (`openai_responses/sse.rs`) therefore emits the arguments
  from the first done event when no delta carried them, never twice, and
  drops any delta that arrives after a done event.

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
