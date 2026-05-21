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
- `src/schema.rs` -- canonical wire types: `ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`, `ReasoningDetail`, `Usage`, `RoutectlInternal`
- `src/content_part.rs` -- typed `ContentPart` enum (text/image/document/tool_use/tool_result/thinking/Other) for `MessageContent::Parts`
- `src/system_content.rs` -- typed top-level `system` field (flat string OR array of `SystemBlock` with per-block cache_control)
- `src/tool_def.rs` -- typed `ToolDef::Custom(CustomTool)` + `ToolDef::Other(Value)` with `from_openai_function` interop
- `src/cache_control.rs` -- Anthropic `CacheControl` type, breakpoint validator (4-cap, 1h-before-5m TTL ordering)
- `src/reasoning_dialect.rs` -- crate-neutral `ReasoningDialect` + `HistoryReasoning` enums carried on `RoutectlInternal`
- `src/reserved.rs` -- `is_canonical_request_key` allowlist guarding extras-merge from clobbering `ChatRequest` fields
- `src/provider.rs` -- `Provider` trait every backend implements (normalize_request/response/chunk + complete + stream)
- `src/log_safe.rs` -- log sanitization, body-trace helpers (4 directions), prompt redaction, structural-summary extractor
- `src/error.rs` -- `Error` enum (Upstream/NormalizeRequest/Validation/Streaming/Auth/Config/...) and `Result` alias

### Tests

- `tests/schema_roundtrip.rs` -- serde round-trip for `ChatRequest`/`ChatResponse`/`ChatChunk` against real wire fixtures

## routectl-providers

### Top-level

- `src/lib.rs` -- feature-gated module exports for `openai_compat`, `anthropic_api`, `bedrock`, `openai_responses`
- `src/model_profile.rs` -- per-model quirks table (drops_sampling_params, requires_reasoning_effort, adaptive_thinking, etc.)
- `src/http_client.rs` -- shared `reqwest::Client` factory with TLS-1.2 pin and User-Agent override

### anthropic_api

- `src/anthropic_api/mod.rs` -- `AnthropicApiProvider` impl + `AuthKind` (ApiKey / OauthBearer); SSE drain
- `src/anthropic_api/types.rs` -- Anthropic Messages wire types (`AnthropicRequest`, content blocks, system, thinking config, usage)
- `src/anthropic_api/request.rs` -- canonical `ChatRequest` -> Anthropic wire body translation
- `src/anthropic_api/response.rs` -- Anthropic response -> canonical `ChatResponse` (content-block walk, stop_reason map, usage cache stats)
- `src/anthropic_api/sse.rs` -- Anthropic SSE event state machine (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
- `src/anthropic_api/parts.rs` -- image-source translation (data-URI -> base64) and trailing-Text-after-tool_use stripping

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
- `src/openai_compat/wire_lift/content.rs` -- image content blocks (`{image,source:base64}` -> `image_url`), drops documents
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
- `src/openai_responses/auth.rs` -- header injection per `AuthKind` (ChatgptOauth Bearer+Account-Id+originator, ApiKey Bearer, BedrockMantle deferred)
- `src/openai_responses/request.rs` -- orchestrator: builds `ResponsesRequest` from `ChatRequest` via system/messages/tools/extras submodules
- `src/openai_responses/system.rs` -- canonical `system` -> Responses `instructions` flat string (drops per-block cache_control with WARN)
- `src/openai_responses/messages.rs` -- canonical `messages[]` -> Responses `input[]` (Message/Reasoning/FunctionCall/FunctionCallOutput items)
- `src/openai_responses/tools.rs` -- canonical tools -> flat Responses `{type,name,description,parameters}` shape; tool_choice mapping
- `src/openai_responses/extras.rs` -- reasoning translation + 6-key provider_extras allowlist; ChatgptOauth `store=false` lock
- `src/openai_responses/response.rs` -- Responses response -> canonical (output walk, finish_reason from status, usage)
- `src/openai_responses/sse.rs` -- Responses SSE state machine keyed on `output_index` (Text/Reasoning/ToolUse blocks)

### bedrock

- `src/bedrock/mod.rs` -- `BedrockProvider`; topology comment for Invoke vs Converse dispatch
- `src/bedrock/auth.rs` -- AWS credential resolution (`Bearer` short-circuit, `SigV4` via `SharedCredentialsProvider`)
- `src/bedrock/signing.rs` -- SigV4 signing entry point; merges Authorization/x-amz-date/x-amz-security-token onto request
- `src/bedrock/endpoint.rs` -- region-to-bedrock-runtime URL builders; ARN/bracket-suffix path encoding
- `src/bedrock/eventstream.rs` -- AWS eventstream binary frame decoder for InvokeModel-stream (with 8 MB DoS cap)
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
- `src/bedrock/converse/request_tests_round2.rs` -- review-finding test sidecar imported from `request.rs` (toolChoice none, extras merge, document siblings)

### Tests

- `tests/common/mod.rs` -- shared canonical-`ChatRequest` scenario builders mirrored with `routectl-cli/tests/common/mod.rs`
- `tests/anthropic_api.rs` -- wiremock-based complete + stream tests for Anthropic Messages API egress
- `tests/openai_compat.rs` -- wiremock-based complete + stream tests for openai-compat egress (DeepSeek multi-turn, etc.)
- `tests/contract_egress.rs` -- canonical -> Anthropic+openai-compat wire body snapshots via insta
- `tests/contract_egress_bedrock_invoke.rs` -- canonical -> Bedrock-Invoke (Anthropic-shape) body snapshots
- `tests/contract_egress_bedrock_converse.rs` -- canonical -> Bedrock-Converse vendor-neutral body snapshots
- `tests/contract_egress_openai_responses.rs` -- canonical -> OpenAI Responses body snapshots; pins flat tool/tool_choice shapes
- `tests/contract_response_egress.rs` -- canned upstream body -> canonical `ChatResponse` (Anthropic + openai-compat)
- `tests/contract_stream_egress.rs` -- canned SSE bodies through `stream()` asserting canonical chunk sequence (Bug B / Bug G classes)

## routectl-router

- `src/lib.rs` -- crate root; re-exports `Config`, `Router`, `ResolvedModel`, factory builders
- `src/config.rs` -- TOML schema (`Config`, `ProviderEntry`, `ModelEntry`, `AliasValue`, `RetryPolicy`, `ServerAuth`, etc.)
- `src/factory.rs` -- secret resolution + `build_provider`/`build_resolved_models`; validation guards
- `src/glob.rs` -- `[aliases]` table suffix-glob parser + longest-prefix lookup index (`AliasPattern`, `PrefixIndex`)
- `src/resolved.rs` -- `ResolvedModel` carrying provider, upstream, reasoning defaults, header/payload extras per `[models.X]`
- `src/router.rs` -- alias resolution + fallback-chain walk; per-model overlay merge (header/payload) and gate dispatch
- `src/runtime_state.rs` -- per-provider token-bucket RPM limiter + circuit breaker state machine

### Tests

- `tests/factory.rs` -- secret-store-backed provider construction across all four provider kinds
- `tests/router.rs` -- fallback-chain semantics, runtime-gate behavior with mock `Provider` impls

## routectl-auth

- `src/lib.rs` -- crate root; re-exports `MemoryStore`, `SecretRef`, `SecretStore`, session types
- `src/store.rs` -- `SecretStore` async trait (get/set/delete) for credential providers
- `src/secret_ref.rs` -- `SecretRef` enum (`env://`, `file://`, `literal:`) plus URI parser
- `src/memory_store.rs` -- default in-process `SecretStore` resolving env/file/literal references at read-time
- `src/session.rs` -- v0.2 cookie-session capture trait + `Cookie` / `CapturedSession` types (deferred)

### Tests

- `tests/secret_resolution.rs` -- `SecretRef::parse` happy/error paths plus `MemoryStore` env/file resolution

## routectl-cli

- `src/main.rs` -- clap CLI entry point; dispatches `serve` / `test` / `config` / `login` subcommands
- `src/lib.rs` -- library surface exposing `commands`, `handlers`, `ingress`, `server` modules to integration tests

### server

- `src/server/mod.rs` -- axum app construction; `serve_on_listener`, `check_bind_safety` loopback guard
- `src/server/auth.rs` -- listener middleware enforcing `[server.auth].tokens` via constant-time comparison
- `src/server/request_id.rs` -- request-id middleware (`x-request-id` echo + `tracing` span field with allowlist sanitization)

### handlers

- `src/handlers/mod.rs` -- groups per-route HTTP handlers
- `src/handlers/health.rs` -- `GET /health` returning version + status
- `src/handlers/models.rs` -- `GET /v1/models` listing aliases + `[models]` keys (skips `default`, skips `selectable=false`)
- `src/handlers/chat_completions.rs` -- `POST /v1/chat/completions` thin wrapper around `ingress_handle` with `OpenAiIngress`
- `src/handlers/messages.rs` -- `POST /v1/messages` thin wrapper around `ingress_handle` with `AnthropicIngress`
- `src/handlers/ingress_handle.rs` -- generic ingress driver: parse + route + render; SSE streaming with cancellation

### ingress

- `src/ingress/mod.rs` -- `IngressAdapter` trait, `SseEvent`, `read_alias_header` (`x-routectl-alias` override)
- `src/ingress/openai.rs` -- OpenAI Chat Completions ingress; lifts `role:"system"` messages into `req.system`, lifts function tools
- `src/ingress/anthropic/mod.rs` -- `AnthropicIngress` impl + streaming state types (`AnthropicStreamState`, `OpenBlockKind`)
- `src/ingress/anthropic/parse.rs` -- Anthropic body -> canonical `ChatRequest`; forward-compat sweep into `provider_extras`
- `src/ingress/anthropic/render.rs` -- canonical `ChatResponse` -> Anthropic Messages response body shape
- `src/ingress/anthropic/stream.rs` -- canonical `ChatChunk` -> Anthropic SSE events with monotonic terminal-state guard

### commands

- `src/commands/mod.rs` -- groups CLI subcommand entry points (test/config/login)
- `src/commands/config.rs` -- `routectl config check/show/example` (secret resolution, alias chain validation)
- `src/commands/test.rs` -- `routectl test <target>` one-shot completion against an alias or model nickname
- `src/commands/login.rs` -- `routectl login <provider>` stub returning a deferred-feature error

### Tests

- `tests/common/mod.rs` -- shared canonical scenario builders mirrored with `routectl-providers/tests/common/mod.rs`
- `tests/server.rs` -- end-to-end axum server tests with wiremock upstreams
- `tests/commands.rs` -- `test` / `config` / `login` subcommand integration tests
- `tests/anthropic_ingress.rs` -- `/v1/messages` end-to-end (cache_control round-trip, forward-compat, listener auth)
- `tests/contract_ingress.rs` -- request wire body -> canonical `ChatRequest` shape per ingress
- `tests/contract_response_ingress.rs` -- canonical `ChatResponse` -> Anthropic wire body via `render_response`
- `tests/contract_stream_ingress.rs` -- canonical chunk sequences -> Anthropic SSE events (Bug B class ordering)
- `tests/e2e_reasoning.rs` -- end-to-end reasoning round-trip across DeepSeek / vLLM / Anthropic dialects
- `tests/reasoning_defaults_ingress.rs` -- per-model `[models.X] thinking`/`enabled` operator-side reasoning defaults
- `tests/live_matrix.rs` -- live provider matrix (OpenRouter / opencode-go / NIM); requires API keys, gated by feature flag
- `tests/live_anthropic_oauth.rs` -- live OAuth-bearer test against `api.anthropic.com`; gated by env token file
