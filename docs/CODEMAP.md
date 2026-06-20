# Codemap

File-tree map of every Rust source file under `crates/`. Use this when
you know the kind of code you're looking for ("eventstream decoding",
"Anthropic content-block translation", "SigV4 signing") but not the
file path. For module-level architecture and dataflow, see
[ARCHITECTURE.md](ARCHITECTURE.md).

Each line is one file: path, then a one-line description of what the
file does. Test sidecars (`*_tests.rs` paired with a source file) are
omitted; cross-crate integration tests live under `tests/` and are
listed at the bottom of each crate.

## routectl-core

- `src/lib.rs` -- crate root; re-exports schema types, error type, Provider trait, log helpers, and the canonical-key allowlist
- `src/schema.rs` -- canonical wire types: `ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`, `ReasoningDetail`, `Usage`, `RoutectlInternal`; `RequestProvenance` (`Library` / `AnthropicIngress` / `OpenaiIngress`, the ingress-dialect tag carried on `RoutectlInternal`)
- `src/schema_opaque.rs` -- transport-internal `OpaqueSseEvent` carrier for unknown Anthropic SSE bytes (skip-serialized; preserves unknown content_block types verbatim through the canonical pipeline so Anthropic ingress can re-emit byte-for-byte)
- `src/upstream_meta.rs` -- transport-internal `UpstreamMeta` carrier (skip-serialized on `ChatResponse`/`ChatChunk`) for non-canonical upstream metadata; today the provider-namespaced `AnthropicUnifiedQuota` (the `anthropic-ratelimit-unified-*` quota/overage family, raw strings + `extras` forward-compat + `is_overage()`)
- `src/content_part.rs` -- typed `ContentPart` enum (text/image/image_url/file/document/tool_use/tool_result/thinking/redacted_thinking (plus the `Other` catchall)) for `MessageContent::Parts`
- `src/system_content.rs` -- typed top-level `system` field (flat string OR array of `SystemBlock` with per-block cache_control)
- `src/tool_def.rs` -- typed `ToolDef::Custom(CustomTool)` + `ToolDef::Other(Value)` with `from_openai_function` interop
- `src/cache_control.rs` -- Anthropic `CacheControl` type, breakpoint validator (4-cap, 1h-before-5m TTL ordering); `CacheBreakpointSource` trait (single source of truth for the tools->system->messages->top-level breakpoint walk) with the `ChatRequest` impl, `validate_source` (validate any source), `FrozenFloor` + `compute_frozen_floor` (count + positions of caller-supplied breakpoints, reusing the same walk; consumed by the dispatch-path auto-emitter); `mutable_suffix_start` (the cache-safe mutable-tail boundary -- the message index strictly after the last caller breakpoint; `None` when the whole request is frozen; consumed by the context reducer)
- `src/context_reduction.rs` -- lossless, cache-safe JSON-whitespace minifier over a request's mutable message tail: `minify_json_whitespace` (custom byte lexer dropping insignificant whitespace outside string literals; lossless, never a serde reparse), `apply_json_minify` (drives the minify over `tool_result.content` / `tool_use.input` JSON-valued strings at or after `mutable_suffix_start`), `ReductionOutcome` (`NoMutableTail` / `NothingToStrip` / `Applied`) + `ReductionDelta` (`strings_minified` / `bytes_saved` / `est_tokens_saved`)
- `src/volatile.rs` -- structural volatile-content detector over a request's stable cacheable prefix (system text + tool name/description): `scan_volatile` (pure, non-mutating) returns a `VolatileReport` with `VolatileConfidence` (None/Low/High) + matched `VolatileKind`s (Uuid/Timestamp/Jwt/HexBlob); HIGH (`is_high_confidence_veto`) is the non-mutating veto for auto-cache emission, LOW is warn-only
- `src/reasoning_dialect.rs` -- crate-neutral `ReasoningDialect` + `HistoryReasoning` enums carried on `RoutectlInternal`
- `src/reserved.rs` -- `is_canonical_request_key` allowlist guarding extras-merge from clobbering `ChatRequest` fields
- `src/provider.rs` -- `Provider` trait every backend implements (normalize_request/response/chunk + complete + stream + on_auth_failure hook for 401 recovery)
- `src/token_source.rs` -- `TokenSource` async trait (`Arc<dyn TokenSource>` per-provider) + `StaticToken` default impl; lets OAuth refresh rotate without daemon restart
- `src/log_safe.rs` -- log sanitization, body-trace helpers (4 directions), prompt redaction, structural-summary extractor, `[log]`-block override seeding
- `src/test_utils.rs` -- single source of truth for the cross-crate contract-test fixture builders (`scenarios::*`, `user_msg`, `get_weather_tool`); gated behind `#[cfg(any(test, feature = "test-utils"))]` so it never ships in a release build; re-exported by both `routectl-providers/tests/common/mod.rs` and `routectl-cli/tests/common/mod.rs`
- `src/identity/mod.rs` -- provider identity-header module root; one canonical home for the compiled HTTP-fingerprint constants and default-header builders (`pub mod codex; pub mod anthropic;`)
- `src/identity/codex.rs` -- shared codex CLI HTTP fingerprint (UA, originator, residency) + `default_identity_headers()` (originator/residency/version trio); consumed by both the openai-responses egress client and the routectl-auth OAuth refresh client so token-endpoint round-trips do not drift from real codex traffic
- `src/identity/anthropic.rs` -- compiled Claude Code SDK (Stainless) identity-header defaults (`default_claude_code_identity_headers`, `default_claude_code_user_agent`); consumed by the anthropic-api egress on the OauthBearer path so a zero-config provider emits the Claude Code fingerprint
- `src/error.rs` -- `Error` enum (Upstream/NormalizeRequest/Validation/Streaming/Auth/Config/NotImplemented/...) and `Result` alias; `Error::Upstream` carries a structural `retry_after: Option<Duration>` (populated only on a rate-limit/overload reset hint) plus the `upstream_with_retry_after` ctor

### Tests

- `tests/schema_roundtrip.rs` -- serde round-trip for `ChatRequest`/`ChatResponse`/`ChatChunk` against real wire fixtures
- `tests/header_trace_emit_disabled.rs` -- emit-path coverage for the four header-trace emitters with tracing OFF; isolated test binary so `header_trace_enabled()` freezes to false in its own process
- `tests/header_trace_emit_enabled.rs` -- emit-path coverage for `trace_ingress_headers` / `trace_outgoing_headers` / `trace_upstream_response_headers` / `trace_egress_headers` with tracing ENABLED; pairs with the disabled-path test
- `tests/header_trace_outgoing_redacts.rs` -- end-to-end coverage that `trace_outgoing_headers` collapses a live `authorization` Bearer JWT and `x-api-key` value to `Bearer [REDACTED]` / `[REDACTED]` before emit; isolated binary so the `ROUTECTL_TRACE_HEADERS` OnceLock freezes ON
- `tests/header_trace_upstream_redacts.rs` -- end-to-end coverage that `trace_upstream_response_headers` (direction 3) collapses a `set-cookie` session credential and the SigV4 `x-amz-security-token` STS credential to `[REDACTED]` before emit, while a non-secret rate-limit header round-trips verbatim; isolated binary so the `ROUTECTL_TRACE_HEADERS` OnceLock freezes ON
- `tests/log_overrides_redact_prompts.rs` -- resolution-rule coverage for the `redact_prompts` knob (env > `[log]` > default); isolated binary so OnceLock state stays clean
- `tests/log_overrides_trace_body_bytes.rs` -- resolution-rule coverage for `trace_body_bytes`; isolated binary
- `tests/log_overrides_trace_headers.rs` -- resolution-rule coverage for `trace_headers`; isolated binary

## routectl-providers

### Top-level

- `src/lib.rs` -- feature-gated module exports for `openai_compat`, `anthropic_api`, `bedrock`, `openai_responses`; also declares crate-internal feature-gated helper modules `system_filter`, `claude_signing`, `tool_id`, `upstream_log`
- `src/model_profile.rs` -- per-model quirks table (drops_sampling_params, etc.)
- `src/http_client.rs` -- shared `reqwest::Client` factory with TLS-1.2 pin and User-Agent override
- `src/effort.rs` -- shared `clamp_effort_to_supported` helper; clamps caller `reasoning.effort` against per-model `effort_levels` (rounds toward most-capable above max, least-capable below min); single source of truth across openai-compat, anthropic-api, bedrock, openai-responses
- `src/header_trace.rs` -- lazily-gated header-trace helpers shared by every egress provider; centralizes the `ROUTECTL_TRACE_HEADERS` gate plus the redaction layer for dir-2 (routectl -> upstream) and dir-3 (upstream -> routectl) emit sites
- `src/retry_after.rs` -- parser for the standard HTTP `Retry-After` response header (RFC 9110 delta-seconds or HTTP-date) plus `is_rate_limit_status`; every egress lifts the hint on a 429/503/529 and carries it on `Error::Upstream.retry_after` for the router to honor (the Codex `usage_limit_reached` `resets_at` / `resets_in_seconds` reset is parsed in `openai_responses/response.rs` and preferred over the header hint)
- `src/tool_calls.rs` -- shared parse step (`normalize_tool_calls`) for OpenAI-shape `Message.tool_calls` entries (`{id, function:{name, arguments}}`, arguments a JSON-encoded string); returns `{id, name, arguments: Value}` with missing-id synthesis (`call_<index>`) and a `{"_arguments": ...}` fallback on unparseable arguments. Consumed by the bedrock-converse and openai-responses egresses to re-emit `tool_calls` as native tool-use items; gated on those two features (the anthropic-api egress keeps its own inline parse to stay byte-identical on the empty-id path)
- `src/system_filter.rs` -- shared predicate + strip helper for the Claude Code billing/attribution system block; used by the egresses before forwarding upstream
- `src/claude_signing.rs` -- byte-level re-signer for the billing-header checksum; re-signs an existing billing block in place after egress body mutations
- `src/tool_id.rs` -- shared tool-call id charset sanitizer (chars outside `[a-zA-Z0-9_-]` -> `_`, deterministic) so sanitized `tool_use` ids and their `tool_result` correlators stay equal
- `src/upstream_log.rs` -- shared WARN emitter for upstream HTTP failures (401/403-vs-other auth-warn split) across egresses

### anthropic_api

- `src/anthropic_api/mod.rs` -- `AnthropicApiProvider` impl + `AnthropicApiConfig` (fields: `auth`, `base_url`, `anthropic_version`, `auth_kind`, `header_extras`, `user_agent`, `allowed_betas`, `forward_client_headers`, `context_management`, `max_thinking_entry_bytes`, `session_id`) + `AuthKind` (ApiKey / OauthBearer); SSE drain
- `src/anthropic_api/context_management.rs` -- LRU+TTL thinking-block store for context-management beta emulation; exports `ThinkingCache`, `ThinkingCacheKey`, `ThinkingCacheEntry`, `CONTEXT_MANAGEMENT_BETA`, `CLEAR_THINKING_EDIT_TYPE`, `THINKING_CACHE_CAP`, `THINKING_CACHE_TTL`, `snapshot_to_cache`, `lookup_thinking`, `extract_tool_thinking`, `apply_clear_thinking_edit`
- `src/anthropic_api/types.rs` -- Anthropic Messages wire types (`AnthropicRequest`, content blocks, system, thinking config, usage)
- `src/anthropic_api/request.rs` -- orchestrator: builds the Anthropic wire body from `ChatRequest` via the system/messages/tools/extras submodules; owns `normalize` (entry point), top-level body assembly, and cache_control breakpoint validation (`validate_breakpoints`); re-exports `build_thinking`, `filter_anthropic_betas`, `translate_tool`, `translate_system`, `lift_legacy_system` for the Bedrock egress and `mod.rs`
- `src/anthropic_api/system.rs` -- system-prompt translation: `translate_system` (typed `SystemContent` -> wire) + `lift_legacy_system` (Role::System fallback for direct callers) + `lift_legacy_system_stripped` (billing-aware variant that drops the Claude Code fingerprint block per-message before joining); all `pub(crate)` for Bedrock Converse reuse
- `src/anthropic_api/tools.rs` -- tool + tool_choice translation: `translate_tool` (`ToolDef` -> `AnthropicTool`, incl. legacy OpenAI-shape rewrite) + `translate_tool_choice` (OpenAI/Anthropic shape mapping)
- `src/anthropic_api/messages.rs` -- per-role content-block translation: `translate_messages`, `build_assistant_content`, `emit_reasoning_blocks`, `build_tool_message`, content-part walk, plus `normalize_replay_invariants` (tool_call_id reject + unsigned-thinking strip)
- `src/anthropic_api/extras.rs` -- thinking-budget composition (`build_thinking`, effort clamp, `build_output_config`) + post-merge body reconciliation (`merge_provider_extras`, `filter_anthropic_betas`, `reconcile_output_config_effort`, `strip_thinking_when_tool_choice_forces_use`)
- `src/anthropic_api/response.rs` -- Anthropic response -> canonical `ChatResponse` (content-block walk, stop_reason map, usage cache stats)
- `src/anthropic_api/sse.rs` -- Anthropic SSE event state machine (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
- `src/anthropic_api/sse_opaque.rs` -- bounded opaque-event capture per unknown content block (256 KB / 10000 deltas per block; degrades to sink-drain on overflow with WARN); records bytes for the matching ingress to re-emit verbatim
- `src/anthropic_api/sse_unknown.rs` -- forward-compat handling for unknown SSE content blocks plus the per-block-index invariant; opens `OpenBlockKind::Unknown`, drops misattributed deltas
- `src/anthropic_api/types_sse.rs` -- forward-compat catchalls (`Other(Value)` arms) on the three strict-tagged Anthropic SSE enums (`SseEvent`, `SseContentBlockStart`, `SseDelta`); extracted from `types.rs` for the 800-LOC ceiling
- `src/anthropic_api/parts.rs` -- image-source translation (data-URI -> base64) and trailing-Text-after-tool_use stripping; also OpenAI file-part -> Anthropic document-source translation (`parse_file_document_source`; PDF base64 data URI only) and url-source image translation
- `src/anthropic_api/ratelimit_unified.rs` -- tolerant parser for the `anthropic-ratelimit-unified-*` quota/overage response-header family (`parse_unified_quota` -> `AnthropicUnifiedQuota`; None when absent, non-UTF8 values skipped, unknown suffixes captured in `extras`) plus the once-per-flip overage state machine (`classify_overage_transition`); wired into the egress complete/stream dir-3 sites
- `src/anthropic_api/cloak.rs` -- Claude Code identity cloak for the OauthBearer surface; strips billing/attribution unconditionally; for non-CC clients mints stable identity + `metadata.user_id` and stamps the identity system block; no-op beyond the billing strip for genuine CC clients

### openai_compat

- `src/openai_compat/mod.rs` -- `OpenAiCompatProvider` impl; owns `ThinkTagAccumulator` for cross-chunk `<think>` state
- `src/openai_compat/dialect.rs` -- public `ReasoningDialect` enum + format-tag accessors
- `src/openai_compat/request.rs` -- `ChatRequest` -> OpenAI-compat wire body (dialect dispatch + extras merge)
- `src/openai_compat/response.rs` -- response normalization; lifts `reasoning_content` into `reasoning_details`, strips OpenAI envelope keys
- `src/openai_compat/sse.rs` -- stateless per-chunk parsing + `ThinkTagAccumulator` for the `<think>` cross-chunk path
- `src/openai_compat/util.rs` -- shared `build_reasoning_detail` helper for request/response/SSE normalizers

### openai_compat/dialects

- `src/openai_compat/dialects/mod.rs` -- `Dialect` trait + `ReasoningDialect::as_dyn` dispatch table
- `src/openai_compat/dialects/openai.rs` -- vanilla OpenAI o-series: `reasoning_effort` param, drops sampling params per profile
- `src/openai_compat/dialects/deepseek.rs` -- DeepSeek: lift `reasoning_content`, strip echo-back; effort derived from budget
- `src/openai_compat/dialects/vllm.rs` -- vLLM thinking models (Qwen3, MiMo): `chat_template_kwargs.enable_thinking`, lift reasoning_content
- `src/openai_compat/dialects/openrouter.rs` -- OpenRouter: pass-through with `reasoning_details` history preservation
- `src/openai_compat/dialects/raw_think_tag.rs` -- response-side regex-strip of `<think>...</think>` blocks
- `src/openai_compat/dialects/passthrough.rs` -- no-op dialect for unknown OpenAI-compat hosts
- `src/openai_compat/dialects/util.rs` -- helpers shared between dialect impls (lift, strip, preserve, drop_sampling_params, think-tag regex)

### openai_compat/wire_lift

- `src/openai_compat/wire_lift/mod.rs` -- ordered dispatch table rewriting Anthropic-shape body fields to OpenAI-compat wire shape
- `src/openai_compat/wire_lift/content.rs` -- image content blocks (Anthropic base64 source and url source shapes -> `image_url`), drops documents
- `src/openai_compat/wire_lift/thinking.rs` -- assistant `thinking`/`redacted_thinking` blocks -> message-envelope `reasoning_details`
- `src/openai_compat/wire_lift/tools.rs` -- canonical `ToolDef::Custom` -> `{type:"function",function:{...}}` wire shape
- `src/openai_compat/wire_lift/tool_use.rs` -- assistant `tool_use` content blocks -> top-level `tool_calls` array
- `src/openai_compat/wire_lift/tool_result.rs` -- user-message `tool_result` blocks -> separate `role:"tool"` wire messages
- `src/openai_compat/wire_lift/tool_choice.rs` -- Anthropic tool_choice tagged objects -> OpenAI bare-string / function-object form
- `src/openai_compat/wire_lift/response_format.rs` -- `output_config.format` -> top-level `response_format`; strips Anthropic-only field

### openai_responses

- `src/openai_responses/mod.rs` -- `OpenAiResponsesProvider`; force-streams `complete()`, drains to `response.completed`
- `src/openai_responses/types.rs` -- request wire types: `ResponsesRequest`, `ResponseInputItem` union, `ResponsesTool` flat shape
- `src/openai_responses/response_types.rs` -- response + SSE event wire types (`ResponsesResponse`, output-item union, stream events)
- `src/openai_responses/auth.rs` -- header injection per `AuthKind` (ChatgptOauth Bearer+Account-Id+originator + codex identity headers `version`/`session-id`/`x-codex-installation-id`/`x-codex-window-id`/`thread-id`/`x-client-request-id`/residency, ApiKey Bearer, BedrockMantle Bearer)
- `src/openai_responses/cookies.rs` -- persistent Cloudflare cookie jar (allowlist-pinned to non-secret cookie names)
- `src/openai_responses/request.rs` -- orchestrator: builds `ResponsesRequest` from `ChatRequest` via system/messages/tools/extras submodules
- `src/openai_responses/system.rs` -- canonical `system` -> Responses `instructions` flat string (drops per-block cache_control with DEBUG)
- `src/openai_responses/messages.rs` -- canonical `messages[]` -> Responses `input[]` (Message/Reasoning/FunctionCall/FunctionCallOutput items); also translates `File` content blocks -> `InputFile` items with `file_data` or `file_id`
- `src/openai_responses/tools.rs` -- canonical tools -> flat Responses `{type,name,description,parameters}` shape; tool_choice mapping
- `src/openai_responses/extras.rs` -- reasoning translation + 6-key provider_extras allowlist; ChatgptOauth `store=false` lock
- `src/openai_responses/response.rs` -- Responses response -> canonical (output walk, finish_reason from status, usage)
- `src/openai_responses/sse.rs` -- Responses SSE state machine keyed on `output_index` (Text/Reasoning/ToolUse blocks)

### bedrock

- `src/bedrock/mod.rs` -- `BedrockProvider`; topology comment for Invoke vs Converse dispatch
- `src/bedrock/auth.rs` -- AWS credential resolution (`Bearer` short-circuit, `SigV4` via `SharedCredentialsProvider`)
- `src/bedrock/signing.rs` -- SigV4 signing entry point; merges Authorization/x-amz-date/x-amz-security-token onto request
- `src/bedrock/endpoint.rs` -- region-to-bedrock-runtime URL builders; ARN/bracket-suffix path encoding
- `src/bedrock/frame.rs` -- shared AWS-eventstream framing driver for both Bedrock egresses; owns the byte loop, the 12-byte prelude/length/CRC invariants, the `MAX_FRAME_BYTES` 8 MB DoS cap, decode-error recovery, and the WARN/TRACE log-hygiene split (prelude-only at WARN, full payload hex at TRACE); both the InvokeModel-stream and ConverseStream decoders delegate to `decode_frames`
- `src/bedrock/eventstream.rs` -- InvokeModel-stream frame handler / payload interpreter (base64-unwrap of Anthropic SSE per frame); delegates the framing byte loop and DoS cap to `frame.rs`
- `src/bedrock/invoke.rs` -- InvokeModel adapter: reuses `anthropic_api::request::normalize`, patches `anthropic_version: "bedrock-2023-05-31"`
- `src/bedrock/betas.rs` -- shared `anthropic_beta` allowlist filter (Invoke body + Converse `additionalModelRequestFields`)
- `src/bedrock/body_fields.rs` -- shared `allowed_body_fields` filter against AWS strict-schema 400s

### bedrock/converse

- `src/bedrock/converse/mod.rs` -- groups Converse adapter (vendor-neutral envelope)
- `src/bedrock/converse/types.rs` -- request wire types (`ConverseRequest`, AWS-shape content blocks, `ToolConfig`, `InferenceConfig`)
- `src/bedrock/converse/response_types.rs` -- response + ConverseStream event wire types
- `src/bedrock/converse/request.rs` -- canonical -> Converse request body orchestrator (camelCase + `additionalModelRequestFields`)
- `src/bedrock/converse/system.rs` -- canonical `system` -> Converse `[{text}|{cachePoint}]` block array
- `src/bedrock/converse/messages.rs` -- canonical messages -> Converse messages (per-role dispatch, cachePoint interleave)
- `src/bedrock/converse/tools.rs` -- canonical tools/tool_choice -> Converse `toolConfig` ({auto/any/tool} union)
- `src/bedrock/converse/extras.rs` -- assembles `additionalModelRequestFields` (thinking, anthropic_beta, cache_control, output_config)
- `src/bedrock/converse/response.rs` -- Converse response body -> canonical (content walk, stopReason map, cacheDetails -> cache_creation)
- `src/bedrock/converse/eventstream.rs` -- ConverseStream binary-frame decoder; per-block-index state map

### Tests

- `tests/common/mod.rs` -- thin re-export shim of `routectl_core::test_utils` (the single source of truth for the canonical scenario builders); enabled via the `test-utils` dev-dependency feature on core
- `tests/anthropic_api.rs` -- wiremock-based complete + stream tests for Anthropic Messages API egress (incl. `anthropic-ratelimit-unified-*` quota carrier wire-in: complete populates `ChatResponse.upstream_meta`, stream carries it on the first chunk only, absent family yields None)
- `tests/anthropic_overage_tracing.rs` -- captured-subscriber tracing coverage for the overage-flip log: a flip into overage emits one WARN with the non-secret quota fields, steady state is silent, recovery emits one INFO; isolated binary so the thread-local capture subscriber does not leak
- `tests/context_management.rs` -- wiremock-driven complete() + streaming end-to-end for context-management emulation; asserts beta-header strip, context_management body-key strip, and thinking-block injection; gated on `#[cfg(feature = "anthropic-api")]` (run with `--features test-utils` to exercise helpers that pre-populate the thinking cache)
- `tests/openai_compat.rs` -- wiremock-based complete + stream tests for openai-compat egress (DeepSeek multi-turn, etc.)
- `tests/bedrock_streaming.rs` -- scoped Bedrock integration tests over the public credential-resolution / auth-dispatch API (`bedrock::auth::resolve` Bearer vs SigV4 variants across regions)
- `tests/contract_egress.rs` -- canonical -> Anthropic+openai-compat wire body snapshots via insta
- `tests/contract_egress_bedrock_invoke.rs` -- canonical -> Bedrock-Invoke (Anthropic-shape) body snapshots
- `tests/contract_egress_bedrock_converse.rs` -- canonical -> Bedrock-Converse vendor-neutral body snapshots
- `tests/contract_egress_openai_responses.rs` -- canonical -> OpenAI Responses body snapshots; pins flat tool/tool_choice shapes
- `tests/contract_response_egress.rs` -- canned upstream body -> canonical `ChatResponse` (Anthropic + openai-compat)
- `tests/contract_stream_egress.rs` -- canned SSE bodies through `stream()` asserting canonical chunk sequence (catches stream-ordering and usage-merge regressions)

## routectl-router

- `src/lib.rs` -- crate root; re-exports `Config`, `Router`, `ResolvedModel`, factory builders
- `src/config.rs` -- TOML schema (`Config`, `ProviderEntry`, `ModelEntry`, `AliasValue`, `RetryPolicy`, `ServerAuth`, `RegistryEntry`/`PricingConfig` pricing table, etc.); `Config::pricing_for` resolves an upstream-id glob (provider-scoped beats agnostic, then longest-prefix) to a `&PricingConfig`; `ProviderEntry::api_key_ref()` exposes the primary key ref (`None` for Bedrock) so the usage CLI can detect `oauth://` subscription providers; `ModelEntry` carries `reported_model: Option<String>` (response model-label echo override) and `visible_routectl_provider: bool` (default true; false suppresses the provider name in response metadata); `CacheConfig` (the global `[cache]` table: `auto_emit_top_level_breakpoint: bool`, default true) and `CacheCapability` (`supports_top_level_cache_control` + `cache_hit_observable`, with conservative `for_provider_kind` per-kind defaults); per-`ProviderEntry` `cache_capability` + `auto_emit_top_level_breakpoint` overrides, surfaced via `ProviderEntry::cache_capability()` (anthropic-api fails closed on a non-default base URL) and `ProviderEntry::auto_emit_top_level_breakpoint()`
- `src/factory.rs` -- secret resolution + `build_provider`/`build_resolved_models`; validation guards (incl. `validate_registry_patterns`, rejecting malformed `[registry]` glob keys at startup); OAuth credential-pool expansion (a bare-pool `oauth://<provider>` ref with >1 stored seat builds one seat-pinned provider per seat via `list_seats`)
- `src/glob.rs` -- `[aliases]` table suffix-glob parser + longest-prefix lookup index (`AliasPattern`, `PrefixIndex`)
- `src/resolved.rs` -- `ResolvedModel` carrying provider, upstream, reasoning defaults, header/payload extras per `[models.X]`; optional `seats` slice (one `SeatTarget` per OAuth pool seat, `None` for the single-seat / non-pooled case); `reported_model: Option<String>` (per-model response-echo override)
- `src/router.rs` -- alias resolution + fallback-chain walk; per-model overlay merge (header/payload) and gate dispatch; `expand_chain_to_targets` expands a pooled model into one per-seat `DispatchTarget` (seat order from `seat_pool`); `rate_limit_reset_hint` (clamp an `Error::Upstream.retry_after` to the configured ceiling) + `park_provider` (open the breaker for a honored reset) thread upstream resets into the three dispatch loops; `maybe_apply_auto_cache_control` (the dispatch-path auto-cache emitter: clone -> set top-level ephemeral_5m -> validate -> keep-or-rollback on the per-attempt clone, never the original) gated by `AutoCacheRequestPlan` (built once per request: caller-breakpoint count via `compute_frozen_floor` + `scan_volatile` HIGH veto + global switch); `CacheInjection` variants map to the stable `strategy_str()` decision tokens stamped on `DispatchMeta.cache_strategy` and logged as `cache_auto_decision`. Auto-cache runs on `complete`/`stream` paths only, not `count_tokens`; the dispatch-path context reducer runs alongside it (after overlays, before auto-cache) via `apply_json_minify` gated on `reduction.enabled` AND the per-provider `reduction_enabled()` override, with `reduction_strategy_token` mapping `ReductionOutcome` to the stable `reduction_strategy` tokens stamped on `DispatchMeta.reduction_strategy` and logged as `context_reduction` (DEBUG, only when applied)
- `src/runtime_state.rs` -- per-model (nickname-keyed) token-bucket RPM limiter + circuit breaker state machine; `force_open` parks the breaker for an explicit reset hint, bypassing the consecutive-failure threshold
- `src/seat_pool.rs` -- OAuth credential-pool glue: `SeatTarget` (seat-pinned provider + per-seat `state_key`), `seat_state_key` (bare nickname for the default seat, `nickname#label` for labeled seats), `seat_order_for_request` + `RoundRobinCursors` (per-pool `AtomicUsize` rotating the start seat per request under `RoundRobin`; `FillFirst` walks a fixed default-first order)
- `src/feature_keys.rs` -- feature-key derivation for the alias-chain pre-filter; walks `ToolDef::Other(v)["type"]` strings and strips date suffixes (e.g. `_20250305`) so `unsupported_features` on `ProviderRuntimePolicy` can match capability-class regardless of vendor versioning; `ToolDef::Custom` (user-defined tools) does not contribute feature keys

### Tests

- `tests/factory.rs` -- secret-store-backed provider construction across all four provider kinds
- `tests/factory_context_management_warning.rs` -- coverage for the `context_management` + `history_reasoning != "preserve"` consistency WARN emitted by `build_resolved_models` (fires once when inconsistent; silent otherwise)
- `tests/router.rs` -- fallback-chain semantics, runtime-gate behavior with mock `Provider` impls

## routectl-auth

- `src/lib.rs` -- crate root; re-exports `MemoryStore`, `SecretRef`, `SecretStore`, session types; feature-gated re-exports of `LoginOptions`, `OAuthError`, `OAuthStore`, `SecretToken` under `oauth`
- `src/store.rs` -- `SecretStore` async trait (get/set/delete) for credential providers
- `src/secret_ref.rs` -- `SecretRef` enum (`env://`, `file://`, `literal:`) plus URI parser
- `src/memory_store.rs` -- default in-process `SecretStore` resolving env/file/literal references at read-time
- `src/oauth/mod.rs` -- crate-internal entry for the OAuth 2.0 PKCE subsystem; defines `OAuthError` and re-exports `OAuthStore`, `LoginOptions`, `run_login`, `known_provider_ids`, token types
- `src/oauth/types.rs` -- on-disk schema: `CredentialsFile`, `TokenRecord` (incl. optional `session_id`), `AccountInfo`, `SecretToken` (Drop-zeroized, redacted Debug), `SCHEMA_VERSION`, `unix_now`; `seat_key(provider, label)` composes the credentials-map key (bare provider for the default seat, `provider#label` otherwise) and `CredentialsFile::seats_for_provider` enumerates a provider's seats (default first, then sorted labels)
- `src/oauth/file_io.rs` -- atomic load/save of `~/.config/routectl/credentials.json` (TOCTOU-safe fstat, `0o600` enforcement on Unix, tempfile + fsync + rename)
- `src/oauth/pkce.rs` -- PKCE verifier / SHA-256 challenge / CSRF state; `OsRng`-sourced, Drop-zeroized, constant-time state compare
- `src/oauth/login.rs` -- login flow driver: PKCE bundle, axum callback sub-app on loopback, `webbrowser` launch, `--print-url` headless fallback, 120s timeout; `LoginOptions.label` writes the exchanged record under `seat_key(provider, label)` so a labeled login adds a seat without overwriting the default
- `src/oauth/rate_limit.rs` -- per-source-port + listener-wide sliding-window rate limit on the loopback callback server (turns sustained 400-spam into 429)
- `src/oauth/store.rs` -- `OAuthStore` `SecretStore` impl; cached `CredentialsFile` + per-seat single-flight refresh mutex + atomic writeback; resolves labeled seats by `seat_key`, `list_seats` expands a bare pool ref to one ref per stored seat; `force_refresh(provider, label)` targets one seat (drives `routectl refresh --label`); near-expiry (300s lead) and 401-recovery hooks; refresh client carries codex CLI HTTP client headers; preserves an existing `session_id` across token rotation (the codex provider flow mints the fresh `session_id` on first OAuth exchange)
- `src/oauth/providers/mod.rs` -- `OAuthFlow` trait + `lookup` registry + `known_provider_ids` (anthropic, codex); `AuthParams` and `truncate` helper
- `src/oauth/providers/anthropic.rs` -- claude.ai OAuth flow: `claude.com/cai/oauth/authorize` + `platform.claude.com/v1/oauth/token`, `anthropic-beta: oauth-2025-04-20`, manual-paste redirect support
- `src/oauth/providers/codex.rs` -- OpenAI ChatGPT/Codex OAuth 2.0 PKCE flow (public client, JWT-derived expiry, lazy refresh-token rotation)

### Tests

- `tests/secret_resolution.rs` -- `SecretRef::parse` happy/error paths plus `MemoryStore` env/file resolution
- `tests/codex_refresh_tracing.rs` -- refresh-flow tracing coverage for the codex (chatgpt-oauth) provider; drives the response decoder through the success and 401-`refresh_token_expired` paths under a captured subscriber, asserting the contractual structured fields (status, `new_refresh_token_present`, sha8) emit without leaking token values

## routectl-usage

Usage-accounting crate: a bounded-channel producer (`UsageHandle`) feeding a single background writer thread that persists one row per routed request to a local SQLite DB. The hot path never blocks/awaits/panics; overflow and disabled-gate drops are counted, not surfaced.

- `src/lib.rs` -- crate root; re-exports `UsageHandle`/`UsageCounters`, `UsageRecord`/`Outcome`, `UsageWriter`/`CHANNEL_CAPACITY`, `UsageDb`/`open`/`open_readonly`, `aggregate`/`latencies`/`latest_quota`/`AggRow`/`GroupKey`/`QuotaSnapshot`, `prune`/`PruneOutcome`, `estimate_cost`/`estimate_cost_tokens`/`CostBreakdown`/`Rates`, schema + migrate constants
- `src/record.rs` -- `UsageRecord` (one field per capture column; epoch-ms `i64` timestamps, nullable `Option<T>` columns) and the closed `Outcome` enum (`ok`, `upstream_error`, `client_disconnect`, `timeout`, `cancelled`, `gate_blocked`) with `as_str`/`FromStr` wire tokens that mirror the DB CHECK constraint
- `src/cost.rs` -- pure leaf-safe cost estimation: `estimate_cost(&UsageRecord, &Rates)` and the aggregate-token entry point `estimate_cost_tokens(input, output, reasoning, cache_read, cache_write_5m, cache_write_1h, &Rates) -> Option<CostBreakdown>` (the record path converts its `Option<u64>` fields to `i64` and delegates; per-dimension `tokens * rate / 1e6`, `None` when the rate table is fully unpriced, `Some(0.0)` when priced with no tokens; reasoning excluded -- billed as output upstream); `Rates` is a usage-owned mirror of the router `PricingConfig` so the crate stays a leaf
- `src/handle.rs` -- `UsageHandle` (cheap `Clone` producer): `try_send` (never blocks/awaits/panics -- safe from `Drop`), runtime-flippable `enabled` gate, shared lock-free `UsageCounters` (enqueued / dropped_full / dropped_disabled / persisted / write_errors / prune_errors)
- `src/query.rs` -- read-only windowed aggregation over the requests table; exports `aggregate`, `latencies`, `latest_quota`, `AggRow`, `GroupKey`, `QuotaSnapshot`
- `src/writer.rs` -- `UsageWriter`: opens the DB once at boot (degrades to a no-op drain loop on open failure), drains the bounded channel on a dedicated thread via `blocking_recv`, bounded-deadline drain + join on `shutdown`
- `src/db.rs` -- `UsageDb` wrapper + `open`: connection setup (WAL, foreign keys), the `INSERT OR IGNORE` write keyed on the UNIQUE `request_id` (idempotency), schema-presence assertions
- `src/schema.rs` -- `requests` table DDL (the capture columns + `request_id UNIQUE` idempotency key + `outcome` CHECK), `meta` table, `SCHEMA_VERSION`; v2 added the nullable `strategy TEXT` column (the per-request auto-cache decision token) via the migrate-on-open ladder (`ALTER TABLE requests ADD COLUMN strategy`, same end shape whether created fresh at v2 or migrated from v1); v3 added the nullable `reduction_strategy TEXT` column (the per-request context-reduction decision token) the same way (`ALTER TABLE requests ADD COLUMN reduction_strategy`)
- `src/migrate.rs` -- forward-only schema migration / version stamping against the `meta` table
- `src/retention.rs` -- `prune` (startup-only, best-effort) dropping rows older than the configured retention window; `PruneOutcome` counters

## routectl-cli

- `src/main.rs` -- clap CLI entry point; dispatches `serve` / `login` / `logout` / `refresh` / `whoami` / `test` / `config` / `usage` subcommands
- `src/lib.rs` -- library surface exposing `commands`, `handlers`, `ingress`, `server` modules to integration tests

### server

- `src/server/mod.rs` -- axum app construction; `serve_on_listener`, `check_bind_safety` loopback guard; hot-reload coordination (file-watch + SIGHUP fan-in, parse/validate/build/swap of the live `Router` behind `ArcSwap`, and the restart-required diff against the previous `Config`)
- `src/server/auth.rs` -- listener middleware enforcing `[server.auth].tokens` via constant-time comparison
- `src/server/file_watch.rs` -- `notify-debouncer-full` fs-watch task; watches the parent dirs of `config.toml` / `credentials.json`, basename-routes events back to a `ReloadRequest::{Config,Credentials}` channel; debounce coalesces tempfile + rename bursts
- `src/server/request_id.rs` -- request-id middleware (`x-request-id` echo + `tracing` span field with allowlist sanitization)
- `src/server/secrets.rs` -- `CompositeStore` `SecretStore` dispatching `oauth://<provider>` to `OAuthStore` and `env://` / `file://` / `literal:` to `MemoryStore`; degrades gracefully when no `HOME` / `XDG_CONFIG_HOME`

### handlers

- `src/handlers/mod.rs` -- groups per-route HTTP handlers
- `src/handlers/health.rs` -- `GET /health` returning version + status
- `src/handlers/models.rs` -- `GET /v1/models` listing aliases + `[models]` keys (skips `default`, skips `selectable=false`)
- `src/handlers/chat_completions.rs` -- `POST /v1/chat/completions` thin wrapper around `ingress_handle` with `OpenAiIngress`
- `src/handlers/messages.rs` -- `POST /v1/messages` thin wrapper around `ingress_handle` with `AnthropicIngress`
- `src/handlers/messages_count_tokens.rs` -- `POST /v1/messages/count_tokens` proxy through the FIRST provider in the dispatch chain only (no fallback walk; tokenizer-specific count must match the chosen model)
- `src/handlers/ingress_handle.rs` -- generic ingress driver: parse + route + render; SSE streaming with cancellation. Constructs the `UsageCapture` guard (now defined in `usage_capture.rs`) at the boundary and drives its `observe_*` / `finalize` calls across the non-stream (`complete_response`) and stream (`stream_response` / `render_stream_task`) paths
- `src/handlers/usage_capture.rs` -- `UsageCapture`, the unified RAII capture guard (replaces the former `EgressStreamSummary`) that records exactly ONE `UsageRecord` per request on both ingress paths: a draft is seeded from the request shape + `RequestId` (`build_usage_draft`), the dispatch/token/quota/outcome columns are stamped from `DispatchMeta` + `ChatResponse`/`ChatChunk` + `UpstreamMeta`, `finalize(outcome)` emits the row once (idempotent), and the Drop fallback stamps `client_disconnect` for a cancelled/disconnected request. Also subsumes the egress stream trace-summary line. Owns the outcome-mapping helpers (`outcome_for_dispatch_err`, `error_class_of`) and the auto-cache outcome signal (`is_cache_thrash` + `emit_cache_outcome`: `cache_auto_outcome` at DEBUG when an auto-emitted breakpoint is created-and-read, WARN when created-without-read this request)

### ingress

- `src/ingress/mod.rs` -- `IngressAdapter` trait, `SseEvent`, `read_alias_header` (`x-routectl-alias` override)
- `src/ingress/openai.rs` -- OpenAI Chat Completions ingress; lifts `role:"system"` and `role:"developer"` messages into `req.system` (preserving per-block `cache_control`), lifts function tools, strips internal `matched_stop_sequence` on render. Tests: inline unit tests in this file plus `tests/server.rs`, `tests/contract_ingress.rs`, `tests/cross_dialect_render.rs`, `tests/e2e_reasoning.rs`, `tests/replay_ingress.rs`
- `src/ingress/anthropic/mod.rs` -- `AnthropicIngress` impl + streaming state types (`AnthropicStreamState`, `OpenBlockKind`)
- `src/ingress/anthropic/parse.rs` -- Anthropic body -> canonical `ChatRequest`; forward-compat sweep into `provider_extras`
- `src/ingress/anthropic/render.rs` -- canonical `ChatResponse` -> Anthropic Messages response body shape
- `src/ingress/anthropic/stream.rs` -- canonical `ChatChunk` -> Anthropic SSE events with monotonic terminal-state guard

### commands

- `src/commands/mod.rs` -- groups CLI subcommand entry points (test, prompt_size, config, login, logout, refresh, whoami, usage; `serve` lives in `crate::server`)
- `src/commands/config.rs` -- `routectl config check/show/example` (secret resolution, alias chain validation)
- `src/commands/test.rs` -- `routectl test <target>` one-shot completion against an alias or model nickname
- `src/commands/prompt_size.rs` -- `routectl prompt-size --alias <X> --request <fixture.json>` offline report of a request fixture's per-tier (SYSTEM / TOOLS / MESSAGES / TOTAL) byte + approx-token footprint and the projected auto-emit decision (caller-supplied / would-inject / no_capability / volatile_vetoed / indeterminate) + reduction outcome. Pure `build_report(&ChatRequest, Option<bool>) -> Report`; config-only alias->provider cache-capability resolution (no secret resolution, no provider build, no network). Runs the same cheap config guards as `test`
- `src/commands/login.rs` -- `routectl login <provider> [--label <name>]` runs the OAuth 2.0 PKCE flow (anthropic, codex), persists tokens via `OAuthStore`; `--label` registers an additional seat without overwriting the default; `--print-url` headless flow guarded against providers without a paste-back landing page
- `src/commands/logout.rs` -- `routectl logout <provider> [--label <name>]` -- removes one seat (`--label` removes only the named seat; no label removes the default) from the credentials store; first-time logout reported but not an error
- `src/commands/refresh.rs` -- `routectl refresh <provider> [--label <name>]` -- forces a refresh of one seat through the per-seat single-flight gate, regardless of expiry
- `src/commands/whoami.rs` -- `routectl whoami` -- prints OAuth seat state grouped by provider (default seat as `<provider> (default)`, labeled seats as `<provider>#<label>`), each with its own expiry; exits 0 when at least one seat is logged in, 2 otherwise
- `src/commands/seat.rs` -- shared `--label` validation for the seat-aware OAuth commands (rejects empty/whitespace labels, mirroring the `oauth://` ref parser)
- `src/commands/usage.rs` -- `routectl usage` read surface over the usage DB (read-only). Calendar windows (`--today`/`--this-week` (Monday-start ISO week)/`--this-month`/`--all`) and ad-hoc `--since D [--until E]` ranges computed in LOCAL time against an injectable `now` (testable window-math fns); no flag + no `--since` prints a multi-window summary. `build_window_report` aggregates, rolls fine `AggRow`s up to `--by model|provider|alias` (or a single total), and bifurcates cost: a provider whose `[providers.X] api_key_ref` starts `oauth://` is subscription (`n/a (subscription)`, no $), an API-key provider prices its summed tokens via `Config::pricing_for` -> `estimate_cost_tokens` (`$X.XX` or `n/a` when unpriced). `--detail` adds cache-write split + nearest-rank p95/max latency + wall-time + server-tool counts. Footer: cache-hit-rate (`cache_read / (cache_read + input)`, divide-by-zero guarded) and error count. `OpenError::NoData` -> friendly stdout + exit 0; `VersionTooNew` -> hard error

### Tests

- `tests/common/mod.rs` -- thin re-export shim of `routectl_core::test_utils` (single source of truth for the canonical scenario builders) plus the cli-only `replay` harness submodule; the builders are enabled via the `test-utils` dev-dependency feature on core
- `tests/server.rs` -- end-to-end axum server tests with wiremock upstreams
- `tests/hot_reload.rs` -- file-watch + SIGHUP hot-reload integration tests; boots `serve_on_listener` against a tempdir-rooted config.toml + credentials.json and polls for the live `Router` swap
- `tests/commands.rs` -- `test` / `config` / `login` subcommand integration tests
- `tests/anthropic_ingress.rs` -- `/v1/messages` end-to-end (cache_control round-trip, forward-compat, listener auth)
- `tests/contract_ingress.rs` -- request wire body -> canonical `ChatRequest` shape per ingress
- `tests/contract_response_ingress.rs` -- canonical `ChatResponse` -> Anthropic wire body via `render_response`
- `tests/contract_stream_ingress.rs` -- canonical chunk sequences -> Anthropic SSE events (asserts terminal-event ordering)
- `tests/replay_egress.rs` -- replay-driven egress contract test; walks `tests/fixtures/captured/`, drives each captured ingress request through the matching egress provider's `normalize_request`, and structurally diffs the upstream-bound body against the on-disk `outgoing_request.json` (anthropic / openai-compat / openai-responses)
- `tests/replay_ingress.rs` -- replay-driven ingress contract test; walks captured fixtures, mounts the upstream response in wiremock, drives egress `complete()`, renders the canonical `ChatResponse` via `AnthropicIngress::render_response`, and asserts it matches the captured egress response structurally (anthropic ingress, non-stream scope)
- `tests/cross_dialect_render.rs` -- pins the per-egress-allowlist contract; asserts that a foreign upstream (openai-compat DeepSeek dialect) through canonical normalize and Anthropic ingress render does not leak vendor envelope keys or a `signature:null` thinking block into the Anthropic-shape response
- `tests/e2e_reasoning.rs` -- end-to-end reasoning round-trip across DeepSeek / vLLM / Anthropic dialects
- `tests/live_matrix.rs` -- live provider matrix (OpenRouter / opencode-go / NIM); requires API keys, gated by feature flag
- `tests/live_anthropic_oauth.rs` -- live OAuth-bearer test against `api.anthropic.com`; gated by env token file
- `tests/anthropic_forward_compat_stream.rs` -- full-pipeline integration tests for the Anthropic SSE forward-compat opaque-events fix; hand-crafted SSE wire-byte fixtures driven egress -> canonical -> ingress, asserting verbatim re-emission of unknown content_block types
