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

- `src/lib.rs` -- crate root; re-exports schema types, error type, Provider
  trait, log helpers, and the canonical-key allowlist
- `src/schema.rs` -- canonical wire types: `ChatRequest`, `ChatResponse`,
  `ChatChunk`, `Message`, `ReasoningDetail`, `Usage`, `RoutectlInternal`;
  `RequestProvenance` (`Library` / `AnthropicIngress` / `OpenaiIngress`, the
  ingress-dialect tag carried on `RoutectlInternal`);
  `RoutectlInternal.forwarded_bearer: Option<ForwardedBearer>`
  (redact-on-Debug/Display carrier for the pure-proxy client bearer captured
  off the inbound `Authorization` header; never serialized to the wire) +
  `RoutectlInternal.stainless_headers: Vec<(String, String)>` (captured
  inbound `x-stainless-*` fingerprint headers for the forwarded leg; never
  serialized to the wire) +
  `RoutectlInternal.responses_input_passthrough: Vec<ResponsesPassthroughItem>`
  (unknown Responses input-item kinds preserved with a `modeled_prefix`
  source-position index for order-preserving Responses-egress replay; never
  serialized to the wire, never read by another egress)
- `src/schema_opaque.rs` -- transport-internal `OpaqueSseEvent` carrier for
  unknown Anthropic SSE bytes (skip-serialized; preserves unknown
  content_block types verbatim through the canonical pipeline so Anthropic
  ingress can re-emit byte-for-byte)
- `src/upstream_meta.rs` -- transport-internal `UpstreamMeta` carrier
  (skip-serialized on `ChatResponse`/`ChatChunk`) for non-canonical upstream
  metadata; today the provider-namespaced `AnthropicUnifiedQuota` (the
  `anthropic-ratelimit-unified-*` quota/overage family, raw strings + `extras`
  forward-compat + `is_overage()`) and `CodexQuota` (the `x-codex-*` family:
  `active_limit`, `primary_used_percent`, `primary_reset_at` + `extras`)
- `src/content_part.rs` -- typed `ContentPart` enum
  (text/image/image_url/file/document/tool_use/tool_result/thinking/redacted_thinking
  (plus the `Other` catchall)) for `MessageContent::Parts`
- `src/system_content.rs` -- typed top-level `system` field (flat string OR
  array of `SystemBlock` with per-block cache_control); `is_blank` is the
  shared egress screen that keeps a meaningless `system: ""` off every wire
- `src/tool_def.rs` -- typed `ToolDef::Custom(CustomTool)` +
  `ToolDef::Other(Value)` with `from_openai_function` interop
- `src/cache_control.rs` -- Anthropic `CacheControl` type, breakpoint
  validator (4-cap, 1h-before-5m TTL ordering); `CacheBreakpointSource` trait
  (single source of truth for the tools->system->messages->top-level
  breakpoint walk) with the `ChatRequest` impl, `validate_source` (validate
  any source), `FrozenFloor` + `compute_frozen_floor` (count + positions of
  caller-supplied breakpoints, reusing the same walk; consumed by the
  dispatch-path auto-emitter); `mutable_suffix_start` (the cache-safe
  mutable-tail boundary -- the message index strictly after the last caller
  breakpoint; `None` when the whole request is frozen; consumed by the context
  reducer)
- `src/context_reduction.rs` -- lossless, cache-safe JSON-whitespace minifier
  over a request's mutable message tail: `minify_json_whitespace` (custom byte
  lexer dropping insignificant whitespace outside string literals; lossless,
  never a serde reparse), `apply_json_minify` (drives the minify over
  `tool_result.content` / `tool_use.input` JSON-valued strings at or after
  `mutable_suffix_start`), `ReductionOutcome` (`NoMutableTail` /
  `NothingToStrip` / `Applied`) + `ReductionDelta` (`strings_minified` /
  `bytes_saved` / `est_tokens_saved`)
- `src/volatile.rs` -- structural volatile-content detector over a request's
  stable cacheable prefix (system text + tool name/description):
  `scan_volatile` (pure, non-mutating) returns a `VolatileReport` with
  `VolatileConfidence` (None/Low/High) + matched `VolatileKind`s
  (Uuid/Timestamp/Jwt/HexBlob); HIGH (`is_high_confidence_veto`) is the
  non-mutating veto for auto-cache emission, LOW is warn-only.
  `scan_caller_prefix_advisory` (pure, non-mutating) is a separate WARN-tier
  pass over the CALLER-cached region (tools/system/leading messages up to the
  caller's final breakpoint), returning a `CallerPrefixAdvisory` of
  `CallerPrefixFinding`s (HIGH kinds only) for the dispatch-path advisory WARN
- `src/reasoning_dialect.rs` -- crate-neutral `ReasoningDialect` +
  `HistoryReasoning` enums carried on `RoutectlInternal`
- `src/reasoning_envelope.rs` -- std-only string codec making a reasoning
  artifact self-describing when it crosses a dialect with no slot for its id
  and scheme: `wrap` (closed `rctl1` version prefix + `.`-separated scheme/id,
  blob copied verbatim) and `unwrap` (TOTAL parse -- malformed, truncated,
  unknown-version, non-token-field, or empty-blob input returns `None`, never
  errors, never panics). Stateless, so continuity survives restart, unbounded
  sessions, and multiple router instances. The unwrapped `(scheme, id)` is
  CLIENT-CONTROLLED and is a HINT, never an authorization -- carry-vs-strip
  policy lives in the callers. `SEPARATOR_ABSENT_FROM_PROBED_BLOBS` pins the
  observed blob prefixes the `.` separator choice rests on
- `src/reasoning_format.rs` -- forever `ReasoningDetail.format` tag vocabulary
  (`OPENAI_RESPONSES_V1` recognized-but-never-emitted, plus lane-faithful
  `CODEX_OAUTH`/`OPENAI_APIKEY`/`BEDROCK_MANTLE`) + the pure predicates every
  consumer shares: `is_responses_family` (family test, never `==`),
  `scheme_of` -> `ReplayScheme` (Codex/Mantle/Gray validator family), and
  `is_replayable(detail, lane)` -> `Replayability` (Carry/Strip/Gray)
- `src/reasoning_ingest.rs` -- `normalize_reasoning_detail_payloads`, the
  inbound reasoning-vocabulary normalization BOTH ingresses run after
  deserialization. `ReasoningDetailKind` serde-aliases the Anthropic block
  names (`thinking` -> `Text`, `redacted_thinking` -> `Encrypted`) so a
  client echoing an assistant turn in Anthropic vocabulary is not rejected;
  this moves the Anthropic `thinking` payload key onto canonical `text`,
  which the aliases alone cannot do. Shared, not duplicated per ingress:
  the defect it closes was the two dialects drifting apart on which
  vocabulary they accept. Read-only scan first, `Arc::make_mut` only when a
  rewrite is due, so the common request pays no message-buffer copy
- `src/reserved.rs` -- `is_canonical_request_key` allowlist guarding
  extras-merge from clobbering `ChatRequest` fields
- `src/capability.rs` -- shared capability-key vocabulary (`WEB_SEARCH`,
  `COMPUTER_USE`, `STRUCTURED_OUTPUT`, `PROMPT_CACHING`, `THINKING`,
  `WELL_KNOWN_CAPABILITY_KEYS`) so the router's `feature_keys.rs` pre-filter
  and the catalog's capability priors key off identical strings; open-ended,
  documents only the well-known subset. Also the forever evidence-class tokens
  written verbatim to the capability ledger and read back open-set-tolerant on
  replay (`SCHEMA_PARSE`/`SCHEMA_MISMATCH`,
  `SEARCH_BLOCKS`/`SEARCH_ABSENT_FORCED`, `CACHE_HIT`, `THINKING_BLOCKS`),
  collected in `EVIDENCE_CLASSES` with the `is_known_evidence_class`
  membership predicate the warm rebuild uses to fail closed on an unrecognized
  class. Also `normalize_capability_key` (the learned-capability storage
  canonicalizer: strips the bedrock bag prefix then takes the first segment,
  else passes the key through unchanged, raw string as the final fallback) and
  `SignalTier` (`as_str` -> `"self-identifying"` / `"inferred"`, `parse` ->
  `Option`, the persisted signal-tier contract mirrored by the usage
  learn-event row's CHECK set and read back open-set-tolerant on warm
  rebuild). Also the durable read-model contract enums, each with a forever
  `as_str` token and an open-set-tolerant parse that never panics on an
  unknown token: `FailurePhase` (`"f1"`/`"f2"`/`"f3"`, `parse` -> `Option`),
  `EvidenceSource` (`"live"`/`"probe"`, `parse` -> `Option`), and `Verdict`
  (`Assumed(bool)`->`"assumed"`, `VerifiedWorking`->`"verified"`,
  `LearnedBroken(FailurePhase)`->`"broken"`, `SuspectIgnored`->`"suspect"`,
  `Cleared`->`"cleared"` (a probe-settled negative removed on warm rebuild),
  `Unknown`->`"unknown"`; `broken_phase` exposes the phase of a learned
  negative; reconstructed for ledger replay via `from_parts(token, phase,
  prior)` which reads the sibling columns -- unknown token or a data-carrying
  token missing its sibling column yields `Unknown`, never a fabricated
  phase/prior). The bool and phase carried by the data variants live in
  sibling ledger fields, not in the verdict token
- `src/provider.rs` -- `Provider` trait every backend implements
  (normalize_request/response/chunk + complete + stream + on_auth_failure hook
  for 401 recovery); `async fn probe(&self) -> ProbeOutcome` is the free
  reachability check the diagnostics surfaces call (defaults to
  `UnsupportedFreeProbe`, so a backend opts in by overriding it).
  `ProbeOutcome` (`#[non_exhaustive]`: `Reachable` / `AuthFailed(String)` /
  `Unreachable(String)` / `UnsupportedFreeProbe` / `Skipped(String)`) is the
  display-safe result enum -- every payload is an operator-facing message,
  never a token/path/env value -- consumed by `routectl-router`'s doctor types
  and re-exported from there
- `src/token_source.rs` -- `TokenSource` async trait (`Arc<dyn TokenSource>`
  per-provider) + `StaticToken` default impl; lets OAuth refresh rotate
  without daemon restart
- `src/cloud_project.rs` -- `CloudProjectCache` async trait (`Arc<dyn
  CloudProjectCache>` held by a provider so the Cloud Code project id resolves
  lazily instead of being baked in at construction): `get` / `put` /
  `clear_if_matches` (compare-and-clear, race-safe -- a late stale-id failure
  never wipes a fresh id a concurrent request already re-resolved);
  `InMemoryProjectCache` default impl; lives in core so `routectl-providers`
  avoids the `SecretRef`/`SecretStore` surface (the auth-crate
  `OAuthStoreProjectCache` is the persistent adapter)
- `src/log_safe.rs` -- log sanitization, body-trace helpers (4 directions),
  prompt redaction, structural-summary extractor, `[log]`-block override
  seeding
- `src/test_utils.rs` -- single source of truth for the cross-crate
  contract-test fixture builders (`scenarios::*`, `user_msg`,
  `get_weather_tool`); gated behind `#[cfg(any(test, feature =
  "test-utils"))]` so it never ships in a release build; re-exported by both
  `routectl-providers/tests/common/mod.rs` and
  `routectl-cli/tests/common/mod.rs`
- `src/identity/mod.rs` -- provider identity-header module root; one canonical
  home for the compiled HTTP-fingerprint constants and default-header builders
  (`pub mod codex; pub mod anthropic;`)
- `src/identity/codex.rs` -- shared codex CLI HTTP fingerprint (UA,
  originator, residency) + `default_identity_headers()`
  (originator/residency/version trio); consumed by both the openai-responses
  egress client and the routectl-auth OAuth refresh client so token-endpoint
  round-trips do not drift from real codex traffic. The version is threaded
  through one typed `CodexIdentity` (`version` + derived `user_agent()` +
  `identity_headers()`; `Default` = `PINNED_CODEX_VERSION`), resolved
  PROCESS-GLOBALLY once by the factory (`set_resolved` / `resolved_identity`,
  a set-once `OnceLock`); `codex_user_agent()` and
  `default_identity_headers()` are thin wrappers over the resolved identity,
  so the operator `codex_version` knob (restart-required) reaches the UA, the
  `version` header, and the refresh fingerprint from a single derivation point
  (unset = the pinned default, byte-identical to before the knob)
- `src/identity/anthropic.rs` -- compiled Claude Code SDK (Stainless)
  identity-header defaults (`default_claude_code_identity_headers`,
  `default_claude_code_user_agent`); consumed by the anthropic-api egress on
  the OauthBearer path so a zero-config provider emits the Claude Code
  fingerprint. Also `is_anthropic_api_host(base_url) -> bool`, the shared
  exact-host predicate (`api.anthropic.com` only,
  credentials/path/query/fragment-smuggle-proof) gating the pure-proxy
  forwarded-credential and forwarded-identity paths in `anthropic_api/mod.rs`,
  the `credential_source = "forwarded"` provider-config validation
  (`factory::validate_provider_credential_sources`), and the `/v1/models`
  forwarded-lane proxy-through decision
  (`handlers::models::forwarded_proxy_target`). Also the Claude Code
  anthropic-beta floor: `default_claude_code_anthropic_betas()` (the 9-flag
  model-agnostic CC beta base) plus the shared single-source-of-truth const
  literals `OAUTH_ANTHROPIC_BETA` (`oauth-2025-04-20`), `CONTEXT_1M_BETA`
  (`context-1m-2025-08-07`, NOT in the floor -- model-gated, reaches upstream
  as client pass-through), `EFFORT_BETA` (`effort-2025-11-24`, also NOT in the
  floor -- model-gated), and `STRUCTURED_OUTPUTS_BETA`
  (`structured-outputs-2025-12-15`, also unioned by the egress whenever the
  assembled body carries `output_config.format`), consumed by the
  anthropic-api provider's header composition (`build_headers`) and the
  beta-decision 4xx observability
- `src/error.rs` -- `Error` enum
  (Upstream/NormalizeRequest/Validation/Streaming/Auth/Config/NotImplemented/...)
  and `Result` alias; `Error::Upstream` carries a structural `retry_after:
  Option<Duration>` (populated only on a rate-limit/overload reset hint) plus
  `upstream_type`/`upstream_code`/`upstream_request_id` diagnostic fields
  (all `Option<Box<str>>` -- kept small so a `const _` assert holds
  `size_of::<Error>() <= 128`, the `Result` success-slot size) and the
  `upstream_with_retry_after` / `with_upstream_request_id` ctors; `Debug` is
  hand-written (not derived)
  so `Error::Upstream`'s `body` renders as a bounded `body_excerpt` (capped at
  `MAX_LOG_BODY_EXCERPT` chars) + `body_len` marker -- every `?e` log sink is
  bounded from one place even where the request-fault body runs to
  `MAX_ERROR_BODY_BYTES`; all other variants/fields keep the derived shape
- `src/failure_class.rs` -- coarse, stable failure-classification leaf:
  `#[non_exhaustive] FailureClass`
  (RateLimited/Auth/BadRequest/ContentPolicy/ContextWindow/ServerError/Timeout/NetworkError/Overloaded/`FeatureUnsupported{capability}`/Unknown)
  + `ClassifiedFailure{class, matched_by}` (`MatchedBy`
  Variant/Status/UpstreamType); total `classify(&Error, provider_kind:
  Option<&str>)` is status-driven -- the numeric upstream status picks the
  policy row, and an `upstream_type`/`upstream_code` token may only lift a
  classification BETWEEN classes in the SAME policy row (a lift never changes
  retry / fallback / debit); per-provider-family token tables (`anthropic-api`
  / `openai-compat` / `bedrock` + a `UNION` fallback for an absent or
  unrecognized kind) keyed by the config `kind` string; reads only the
  structured `Error::Upstream` fields, zero config / router / provider
  coupling; never returns `Timeout`. Non-`Upstream` variants classify by
  variant alone (`Streaming` -> `NetworkError`, `Auth` -> `Auth`, every other
  variant -> `Unknown`). `FailureClass::class_token(&self) -> Option<&'static
  str>` is THE single kebab-token vocabulary source
  (`rate-limited`/`auth`/`bad-request`/`content-policy`/`context-window`/`server-error`/`timeout`/`network-error`/`overloaded`/`feature-unsupported`;
  `Unknown` -> `None` = "unclassified") shared by the ledger `resolved_class`
  column, the `/status` `errors_by_class` JSON keys, and the `[retry.classes]`
  config tokens -- the config handler (`handlers/status/config.rs`) and
  `config_migrate.rs` delegate to it, and a tripwire test in `class_policy.rs`
  pins it to `ConfigFailureClass`'s serde kebab output. `LastOutcome` (serde
  snake_case
  `ok`/`rate_limited`/`timeout`/`transport_error`/`http_4xx`/`http_5xx`/`circuit_open`)
  is the thin derived per-target outcome wrapper co-located to prevent a
  second drifting taxonomy: `from_failure_class(&FailureClass)` collapses the
  taxonomy into HTTP families (client-error classes -> `Http4xx`,
  `ServerError`/`Overloaded`/`Unknown` -> `Http5xx`,
  `RateLimited`/`Timeout`/`NetworkError` direct), never producing `Ok`
  (success) or `CircuitOpen` (both DTO-derived at the health panel).
  `classify_with_attempt(&Error, provider_kind, ReplayAttempt)` is the
  reasoning-replay-aware entry point (`classify` = the same call with
  `ReplayAttempt::none()`, so every existing caller is unchanged): a private
  `replay` submodule holds the CLOSED, fixture-backed matcher that lifts a
  proven reasoning-replay rejection to `FeatureUnsupported{capability:
  "reasoning_replay"}` -- no new variant, `class_token()` vocabulary
  unchanged. Four conjunctive gates: status 400/422, the caller-supplied
  `ReplayAttempt` reports >= 1 carried gray artifact (the dispatcher's own
  record of what went on the wire -- unrecoverable from the rejection
  itself), the provider `kind` is one the matcher has a captured envelope
  for, and the type/code tokens plus an anchored prefix of the normalized
  `/error/message` match a proven signature. The tokens are taken from the
  canonical `upstream_type`/`upstream_code` when set, falling back to the
  body envelope's own `error.type`/`error.code` -- a provider that
  recognizes a first-party `{"error":{...}}` body carries it RAW and leaves
  the canonical fields empty, so a canonical-only match would be inert on
  exactly the family this matcher serves. Body parsing is the LAST gate,
  bounded by `MAX_ERROR_BODY_BYTES`, and only a rejection that would
  otherwise be a plain `BadRequest` is eligible -- an upstream that named
  its own cause (content-policy / context-window / feature token) keeps
  that class

### Tests

- `tests/schema_roundtrip.rs` -- serde round-trip for
  `ChatRequest`/`ChatResponse`/`ChatChunk` against real wire fixtures
- `tests/codex_identity_resolved.rs` -- single-test binary pinning the
  resolved-identity wrapper coherence: `set_resolved` installs a custom
  `CodexIdentity` and the free-function wrappers (`codex_user_agent`,
  `default_identity_headers`) serve that identity's derived values; isolated
  binary because the resolved slot is a set-once process-global
- `tests/header_trace_emit_disabled.rs` -- emit-path coverage for the four
  header-trace emitters with tracing OFF; isolated test binary so
  `header_trace_enabled()` freezes to false in its own process
- `tests/header_trace_emit_enabled.rs` -- emit-path coverage for
  `trace_ingress_headers` / `trace_outgoing_headers` /
  `trace_upstream_response_headers` / `trace_egress_headers` with tracing
  ENABLED; pairs with the disabled-path test
- `tests/header_trace_outgoing_redacts.rs` -- end-to-end coverage that
  `trace_outgoing_headers` collapses a live `authorization` Bearer JWT and
  `x-api-key` value to `Bearer [REDACTED]` / `[REDACTED]` before emit;
  isolated binary so the `ROUTECTL_TRACE_HEADERS` OnceLock freezes ON
- `tests/header_trace_upstream_redacts.rs` -- end-to-end coverage that
  `trace_upstream_response_headers` (direction 3) collapses a `set-cookie`
  session credential and the SigV4 `x-amz-security-token` STS credential to
  `[REDACTED]` before emit, while a non-secret rate-limit header round-trips
  verbatim; isolated binary so the `ROUTECTL_TRACE_HEADERS` OnceLock freezes
  ON
- `tests/log_overrides_redact_prompts.rs` -- resolution-rule coverage for the
  `redact_prompts` knob (env > `[log]` > default); isolated binary so OnceLock
  state stays clean
- `tests/log_overrides_trace_body_bytes.rs` -- resolution-rule coverage for
  `trace_body_bytes`; isolated binary
- `tests/log_overrides_trace_headers.rs` -- resolution-rule coverage for
  `trace_headers`; isolated binary

## routectl-providers

### Top-level

- `src/lib.rs` -- feature-gated module exports for `openai_compat`,
  `anthropic_api`, `bedrock`, `openai_responses`, `gemini`; also declares
  crate-internal feature-gated helper modules `system_filter`,
  `claude_signing`, `tool_id`, `upstream_log`, `anthropic_error`,
  `retry_after`, `responses_reasoning_guard`
- `src/model_profile.rs` -- per-model quirks table (drops_sampling_params, etc.)
- `src/http_client.rs` -- shared `reqwest::Client` factory with TLS-1.2 pin
  and User-Agent override; also owns the response-body cap cluster shared by
  all five provider egresses: `read_body_capped` (two-guard buffered read --
  fast-reject on an honest over-cap `Content-Length` plus a mid-transfer
  running-total abort for chunked/understated bodies, returns `(bytes,
  hit_cap)`), the `MAX_RESPONSE_BODY_BYTES` 16 MB egress-side buffering cap
  (sibling to the streaming `MAX_FRAME_BYTES` frame cap; hardcoded, not a
  config knob), `body_cap_exceeded_message` (fixed client-safe string, never
  echoes upstream bytes), and `warn_body_cap` (one-WARN emitter with a fixed,
  drift-proof field set)
- `src/effort.rs` -- shared `clamp_effort_to_supported` helper; clamps caller
  `reasoning.effort` against per-model `effort_levels` (rounds toward
  most-capable above max, least-capable below min); single source of truth
  across openai-compat, anthropic-api, bedrock, openai-responses
- `src/header_trace.rs` -- lazily-gated header-trace helpers shared by every
  egress provider; centralizes the `ROUTECTL_TRACE_HEADERS` gate plus the
  redaction layer for dir-2 (routectl -> upstream) and dir-3 (upstream ->
  routectl) emit sites
- `src/retry_after.rs` -- parser for the upstream reset hint: the standard HTTP
  `Retry-After` header (RFC 9110 delta-seconds or HTTP-date) plus the
  non-standard `retry-after-ms` header (integer milliseconds, preferred when
  present so a sub-second hint survives), plus `is_rate_limit_status`;
  every egress lifts the hint on a 429/503/529 and carries it on
  `Error::Upstream.retry_after` for the router to honor (the Codex
  `usage_limit_reached` `resets_at` / `resets_in_seconds` reset is parsed in
  `openai_responses/response.rs` and preferred over the header hint)
- `src/tool_calls.rs` -- shared parse step (`normalize_tool_calls`) for
  OpenAI-shape `Message.tool_calls` entries (`{id, function:{name,
  arguments}}`, arguments a JSON-encoded string); returns `{id, name,
  arguments: Value}` with missing-id synthesis (`call_<index>`) and a
  `{"_arguments": ...}` fallback on unparseable arguments. Consumed by the
  bedrock-converse and openai-responses egresses to re-emit `tool_calls` as
  native tool-use items; gated on those two features (the anthropic-api egress
  keeps its own inline parse to stay byte-identical on the empty-id path)
- `src/system_filter.rs` -- shared predicate + strip helper for the Claude
  Code billing/attribution system block; used by the egresses before
  forwarding upstream
- `src/responses_reasoning_guard.rs` -- shared leak-guard
  (`warn_dropped_reasoning_dialect`): one WARN per request (no field values)
  when a non-Responses egress (openai-compat/openrouter, anthropic-api,
  gemini) drops the OpenAI-Responses-dialect `reasoning.context` /
  `reasoning.mode` carried through `provider_extras["reasoning"]`
- `src/claude_signing.rs` -- byte-level re-signer for the billing-header
  checksum; re-signs an existing billing block in place after egress body
  mutations
- `src/tool_id.rs` -- shared tool-call id charset sanitizer: maps an id into
  `[a-zA-Z0-9_-]` within the 64-byte `toolUseId` ceiling (wire-safe ids at or
  under the ceiling pass through, anything else is hex-escaped under a reserved
  prefix, any form over the ceiling folds to a digest) -- injective on the
  escape path, collision-resistant on the digest fold -- and deterministic, so
  a `tool_use` id still equals its `tool_result` correlator on every lane
- `src/upstream_log.rs` -- shared WARN emitter for upstream HTTP failures
  (401/403-vs-other auth-warn split) across egresses
- `src/upstream_request_id.rs` -- `parse_upstream_request_id(&HeaderMap)`:
  lifts the upstream provider's correlation id (first of `x-request-id` /
  `x-oai-request-id` / `cf-ray`) off an error response so the ingress can
  echo it on a client-facing `x-upstream-request-id` header
- `src/anthropic_error.rs` -- shared Anthropic `error.type` ->
  synthetic-status mapping (`anthropic_error_type_to_status`; unknown tokens
  -> 502) consumed by `anthropic_api/sse.rs` and `bedrock/eventstream.rs` so
  an in-stream error event classifies identically to the sync error path
- `src/aws_error.rs` -- shared redaction + token lift for AWS/Bedrock upstream
  error envelopes: single `classify_bedrock_error` source drives both
  `classify_client_error_message` (client-facing) and `sanitized_debug_body`
  (DEBUG log), so a 403 AccessDenied body surfaces only the IAM action and
  never the caller principal ARN / account id / resource ARN; also owns
  `lift_aws_error_tokens` (flat `__type` namespace-stripped + top-level `code`
  -> `upstream_type`/`upstream_code`, inert on native nested `{"error":{...}}`
  bodies; each token is bounded at the lift boundary via
  `is_bounded_aws_token` -- token-shaped charset + 128-char cap -- so an
  ARN/account/body-text blob smuggled through `__type`/`code` lifts as `None`
  instead of reaching a client-facing error field past the 403 scrub) shared
  by the `anthropic_api` mantle lift and both OpenAI readers; gated
  `any(anthropic-api, bedrock, openai-compat, openai-responses)` (no AWS SDK)
  so every lane that can front a mantle upstream shares one classifier and
  cannot drift; also owns `is_carryable_flat_envelope(body, max_bytes)` -- the
  request-fault (400/422) carry gate the native Bedrock `map_error_message`
  uses to retain a body RAW (byte-capped) only for the flat AWS shape the
  capability matcher re-parses (top-level `message` and/or `__type`, within
  the byte ceiling), so any other 400/422 shape (nested
  `{"error":{"message":...}}` from a proxy, HTML, plain text) keeps the short
  512-char excerpt and cannot reflect a large body to the caller; also owns
  `lift_aws_error_type_from_headers` / `classify_aws_error_type_header` --
  the `x-amzn-errortype` response-header lift (a single unambiguous value,
  split at the first `:` to drop the coral URL tail, validated through the
  same bounded-token path) the native Bedrock lane falls back to when the
  400 body carries no `__type`; a duplicate / conflicting / malformed /
  missing header fails closed with a bounded reason label
  (`missing|invalid|ambiguous|conflict`) and the URL tail never reaches an
  `Error` field or a log line
- `src/mantle.rs` -- shared helpers for the Bedrock mantle lanes: pure
  region-to-URL builders (`mantle_host` ->
  `https://bedrock-mantle.<region>.api.aws`, `mantle_anthropic_base` ->
  `.../anthropic`, `mantle_openai_base` -> `.../openai/v1`, all
  trailing-slash-free) plus the `MANTLE_SERVICE` = `bedrock-mantle` SigV4
  scope; builders and const are unconditional (region-derived base URLs need
  no AWS SDK), the `sign` wrapper (delegates to
  `bedrock::signing::apply_with_service`) is gated on `bedrock`, and `probe`
  (also `bedrock`-gated) is a credential-resolve reachability check for a
  mantle lane -- `Bearer` is trivially reachable, `Sigv4` re-provides its
  chain bounded by `probe::PROBE_TIMEOUT`, so `doctor` never dials the mantle
  inference host. Also the shared home (cfg `bedrock`) for `MantleAuth`
  (`region` + resolved `ResolvedCreds`, redacting Debug, `auth_mode` ->
  `bearer`|`sigv4`), reused by the anthropic-api and both OpenAI mantle lanes;
  `anthropic_api::MantleAuth` re-exports it so downstream paths are unchanged

### anthropic_api

- `src/anthropic_api/mod.rs` -- request/response/stream orchestration: `impl
  Provider for AnthropicApiProvider`
  (`normalize_request`/`normalize_response`, `complete`, `stream` + SSE drain,
  `count_tokens`, `on_auth_failure`, `probe`) plus the upstream-error/body
  helpers `map_success_body`, `parse_anthropic_error_type`,
  `read_anthropic_error` (text-first-then-opportunistic-JSON error mapping
  shared by all three HTTP paths; when the Anthropic `error.type` shape is
  absent it lifts flat AWS/Bedrock envelope tokens via
  `aws_error::lift_aws_error_tokens` (`__type` namespace-stripped + top-level
  `code`) into `upstream_type`/`upstream_code` and derives the client-facing
  message via `aws_error::classify_client_error_message` (403 AccessDenied
  collapses to an IAM-action-only line; other statuses get the capped
  sanitized excerpt), so a mantle-lane 403/429 classifies and logs
  truthfully), and `build_count_tokens_body` (the count_tokens field
  allowlist). Re-exports the public construction surface
  (`AnthropicApiProvider`, `AnthropicApiConfig`, `AuthKind`, plus `MantleAuth`
  under cfg `bedrock`, re-exported from `crate::mantle`) from `client.rs` so
  downstream `anthropic_api::` paths are unchanged. On the mantle lane (cfg
  `bedrock`) all three HTTP paths (`complete`, `stream`, `count_tokens`)
  serialize the body to bytes and `sign_mantle` the built request BEFORE the
  header trace and execute -- `count_tokens` drops its `.json()` unsigned
  exemption on this lane (SigV4 needs a hashable body) -- and each records the
  lane span fields; `probe` on a mantle provider routes to `mantle::probe`
  (credential-resolve, no inference-host dial) instead of the `x-api-key`
  `/v1/models` GET
- `src/anthropic_api/client.rs` -- provider construction, auth-kind
  resolution, and header plumbing: `AnthropicApiConfig` (fields: `auth`,
  `base_url`, `anthropic_version`, `auth_kind`, `header_extras`, `user_agent`,
  `allowed_betas`, `forward_client_headers`, `context_management`,
  `max_thinking_entry_bytes`, `session_id`, `cloak`, `use_forwarded_bearer`,
  `mantle` (`Option<MantleAuth>`, cfg `bedrock`)) + `AuthKind` (ApiKey /
  OauthBearer) +
  `AnthropicApiProvider::new`/`resolve_user_agent`/`build_headers`/`cloak_body`/`is_non_cc`/`is_cloak_lane`
  and the beta-decision 4xx observability (`BetaDecision`,
  `should_log_beta_4xx`, `log_beta_decision_on_4xx`).
  `should_use_forwarded_bearer` is the shared three-way WIRE gate consulted by
  both `resolve_effective_token` and `build_headers`:
  `self.cfg.use_forwarded_bearer` (set at construction from the provider's
  `credential_source = "forwarded"`) AND
  `req.routectl_internal.forwarded_bearer` is `Some` AND `base_url`'s host is
  `api.anthropic.com` (via `is_anthropic_api_host`) -- any one leg false means
  own-mode behavior, byte-for-byte unchanged, in particular an own-mode
  provider (`use_forwarded_bearer` false) never consumes a floating bearer
  captured for a sibling forwarded provider on the same router;
  `resolve_effective_token` returns the forwarded bearer in place of
  `self.cfg.auth.token()` when the gate is armed; `build_headers` gates the
  same pin (`forwarded_leg`) to stamp the client's captured
  `stainless_headers` + `claude_code_headers` LAST, overriding routectl's
  minted identity fingerprint on that leg only. `is_cloak_lane` composes
  `forwarded_leg` + `is_anthropic_api_host` + `AuthKind::OauthBearer` into the
  single own-OAuth-to-Anthropic LANE predicate the lane-gated header/body sites
  share (it is the lane, independent of `is_non_cc`/`CloakMode`, which remain
  separate inner gates). Bedrock mantle lane (cfg
  `bedrock`): `MantleAuth` (defined in `crate::mantle`) selects the lane when
  `cfg.mantle` is `Some`; `is_mantle` gates a no-redirect client
  (`http_client::build_no_redirect`, Policy::none()), a `build_headers` skip
  of `x-api-key`/`Authorization` (the signer owns auth; the OauthBearer-gated
  Claude-Code identity headers/UA never fire on this ApiKey lane),
  `sign_mantle` (post-build SigV4/bearer signing via `crate::mantle::sign`),
  and `record_mantle_span_fields` (records `lane`/`auth_mode`/`region` on the
  request span). `build_headers` also takes the assembled `wire_body`
  (`Option<&Value>`) so the beta compose can union capability betas the
  shipped body implies -- today `output_config.format` ->
  `STRUCTURED_OUTPUTS_BETA`, unioned last (post-allowlist, post-floor,
  post-context_management-strip, suppressed on the forwarded leg) and
  idempotent, so an OAuth Claude-Code list stays byte-identical
- `src/anthropic_api/context_management.rs` -- LRU+TTL thinking-block store
  for context-management beta emulation; exports `ThinkingCache`,
  `ThinkingCacheKey`, `ThinkingCacheEntry`, `CONTEXT_MANAGEMENT_BETA`,
  `CLEAR_THINKING_EDIT_TYPE`, `THINKING_CACHE_CAP`, `THINKING_CACHE_TTL`,
  `snapshot_to_cache`, `lookup_thinking`, `extract_tool_thinking`,
  `apply_clear_thinking_edit`
- `src/anthropic_api/types.rs` -- Anthropic Messages wire types
  (`AnthropicRequest`, content blocks, system, thinking config, usage)
- `src/anthropic_api/request.rs` -- orchestrator: builds the Anthropic wire
  body from `ChatRequest` via the system/messages/tools/extras submodules;
  owns `normalize` (entry point), top-level body assembly, and cache_control
  breakpoint validation (`validate_breakpoints`); re-exports `build_thinking`,
  `filter_anthropic_betas`, `translate_tool`, `translate_system`,
  `lift_legacy_system` for the Bedrock egress and `mod.rs`
- `src/anthropic_api/system.rs` -- system-prompt translation:
  `translate_system` (typed `SystemContent` -> wire) + `lift_legacy_system`
  (Role::System fallback for direct callers) + `lift_legacy_system_stripped`
  (billing-aware variant that drops the Claude Code fingerprint block
  per-message before joining); all `pub(crate)` for Bedrock Converse reuse
- `src/anthropic_api/tools.rs` -- tool + tool_choice translation:
  `translate_tool` (`ToolDef` -> `AnthropicTool`, incl. legacy OpenAI-shape
  rewrite) + `translate_tool_choice` (OpenAI/Anthropic shape mapping)
- `src/anthropic_api/messages.rs` -- per-role content-block translation:
  `translate_messages`, `build_assistant_content`, `emit_reasoning_blocks`,
  `build_tool_message`, content-part walk, plus `normalize_replay_invariants`
  (tool_call_id reject, unsigned-thinking strip, and dropping a whole
  assistant turn whose reasoning is entirely non-emittable -- via the
  `message_has_emittable_reasoning`/`is_anthropic_emittable_detail` predicate
  shared with `emit_reasoning_blocks` so the keep-decision and the emit gate
  cannot drift, with an aggregated WARN on any dropped turns);
  `build_assistant_content` carries an empty-content backstop that inserts one
  empty text block so an assembled-empty turn never ships `content: []`
- `src/anthropic_api/extras.rs` -- thinking-budget composition
  (`build_thinking`, effort clamp, `build_output_config`) + post-merge body
  reconciliation (`merge_provider_extras`, `filter_anthropic_betas`,
  `reconcile_output_config_effort`,
  `strip_thinking_when_tool_choice_forces_use`); also the structured-outputs
  capability-beta union (`body_has_output_config_format` +
  `union_structured_outputs_beta` for the header carrier /
  `apply_structured_outputs_beta_to_body` for the body carrier, applied by
  the Bedrock-Invoke egress after its own allowlist filters), which gates
  the beta on the ASSEMBLED body's `output_config.format` and bypasses
  `allowed_betas` as a server requirement rather than a client-opted beta
- `src/anthropic_api/response.rs` -- Anthropic response -> canonical
  `ChatResponse` (content-block walk, stop_reason map, usage cache stats)
- `src/anthropic_api/sse.rs` -- Anthropic SSE event state machine
  (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
- `src/anthropic_api/sse_opaque.rs` -- bounded opaque-event capture per
  unknown content block (per-block caps: 256 KB / 10000 deltas; per-stream
  ceiling: 4 MB / 40000 events), each degrading to sink-drain on overflow with
  one WARN; records bytes for the matching ingress to re-emit verbatim
- `src/anthropic_api/sse_unknown.rs` -- forward-compat handling for unknown
  SSE content blocks plus the per-block-index invariant; opens
  `OpenBlockKind::Unknown`, drops misattributed deltas; once the per-stream
  opaque cap trips, open/delta capture short-circuits to sink-drain (post-trip
  blocks emit neither start nor stop; pre-trip starts still get their paired
  stop)
- `src/anthropic_api/types_sse.rs` -- forward-compat catchalls (`Other(Value)`
  arms) on the three strict-tagged Anthropic SSE enums (`SseEvent`,
  `SseContentBlockStart`, `SseDelta`); extracted from `types.rs` for the
  800-LOC ceiling
- `src/anthropic_api/parts.rs` -- image-source translation (data-URI ->
  base64) and trailing-Text-after-tool_use stripping; also OpenAI file-part ->
  Anthropic document-source translation (`parse_file_document_source`; PDF
  base64 data URI only) and url-source image translation
- `src/anthropic_api/ratelimit_unified.rs` -- tolerant parser for the
  `anthropic-ratelimit-unified-*` quota/overage response-header family
  (`parse_unified_quota` -> `AnthropicUnifiedQuota`; None when absent,
  non-UTF8 values skipped, unknown suffixes captured in `extras`) plus the
  once-per-flip overage state machine (`classify_overage_transition`); wired
  into the egress complete/stream dir-3 sites
- `src/anthropic_api/cloak.rs` -- OauthBearer-egress cloak root: shared
  config/types (`CloakMode`, `ToolRename`, `CloakConfig`, `CloakResult`,
  `ClaudeCodeIdentity`) and the `cloak_oauth_egress` orchestrator sequencing
  the concern submodules below
- `src/anthropic_api/cloak/billing.rs` -- strips the Claude Code
  billing/attribution system block unconditionally (`strip_billing_block`)
- `src/anthropic_api/cloak/identity.rs` -- non-CC client system relocation
  into a `<system-reminder>` block plus identity-only system and minted
  `metadata.user_id` (`relocate_client_system`, `mint_metadata_user_id`)
- `src/anthropic_api/cloak/tool_rename.rs` -- tool-name `mcp__` normalization
  and operator `tool_rename` over the same tool-name paths, recording the
  per-request reverse map (`normalize_tool_names_to_mcp`, `apply_tool_rename`)
- `src/anthropic_api/cloak/tool_sort.rs` -- all-or-nothing stable sort of
  `tools[]` by name on the non-CC egress (`sort_custom_tools_by_name`), gated
  on `is_non_cc && CloakConfig::normalize_tools`; stands the whole sort down
  on any opaque/builtin tool, missing name, or duplicate name; runs after name
  normalization so it orders final wire names (idempotent)
- `src/anthropic_api/cloak/obfuscate.rs` -- zero-width-space obfuscation of
  configured sensitive words in system and message text
  (`obfuscate_sensitive_words`, `SensitiveWordMatcher`)

### openai_compat

- `src/openai_compat/mod.rs` -- `OpenAiCompatProvider` impl; owns
  `ThinkTagAccumulator` for cross-chunk `<think>` state;
  `map_openai_compat_upstream_error` lifts the native OpenAI
  `error.type`/`code` classifier and, for a non-envelope (flat AWS mantle)
  body, lifts `aws_error::lift_aws_error_tokens` and routes a 403 through the
  `aws_error` scrub; the optional `OpenAiCompatConfig::mantle` selects the
  Bedrock mantle lane (SigV4/bearer-signed body bytes via `mantle::sign`, no
  first-party Bearer, no-redirect client, `mantle::probe` credential check)
- `src/openai_compat/dialect.rs` -- public `ReasoningDialect` enum +
  format-tag accessors
- `src/openai_compat/request.rs` -- `ChatRequest` -> OpenAI-compat wire body
  (dialect dispatch + extras merge)
- `src/openai_compat/response.rs` -- response normalization; lifts
  `reasoning_content` into `reasoning_details`, strips OpenAI envelope keys
- `src/openai_compat/sse.rs` -- stateless per-chunk parsing +
  `ThinkTagAccumulator` for the `<think>` cross-chunk path +
  `StreamedToolCallIds` (per-stream synthesis of missing tool-call ids)
- `src/openai_compat/util.rs` -- shared `build_reasoning_detail` helper for
  request/response/SSE normalizers

### openai_compat/dialects

- `src/openai_compat/dialects/mod.rs` -- `Dialect` trait +
  `ReasoningDialect::as_dyn` dispatch table
- `src/openai_compat/dialects/openai.rs` -- vanilla OpenAI o-series:
  `reasoning_effort` param, drops sampling params per profile
- `src/openai_compat/dialects/deepseek.rs` -- DeepSeek: lift
  `reasoning_content`, strip echo-back; effort derived from budget
- `src/openai_compat/dialects/vllm.rs` -- vLLM thinking models (Qwen3, MiMo):
  `chat_template_kwargs.enable_thinking`, lift reasoning_content
- `src/openai_compat/dialects/openrouter.rs` -- OpenRouter: pass-through with
  `reasoning_details` history preservation
- `src/openai_compat/dialects/raw_think_tag.rs` -- response-side regex-strip
  of `<think>...</think>` blocks
- `src/openai_compat/dialects/passthrough.rs` -- no-op dialect for unknown
  OpenAI-compat hosts
- `src/openai_compat/dialects/util.rs` -- helpers shared between dialect impls
  (lift, strip, preserve, drop_sampling_params, think-tag regex,
  `reasoning_enabled_for_wire` reasoning-signal predicate)

### openai_compat/wire_lift

- `src/openai_compat/wire_lift/mod.rs` -- ordered dispatch table rewriting
  Anthropic-shape body fields to OpenAI-compat wire shape
- `src/openai_compat/wire_lift/content.rs` -- image content blocks (Anthropic
  base64 source and url source shapes -> `image_url`), drops documents
- `src/openai_compat/wire_lift/thinking.rs` -- assistant
  `thinking`/`redacted_thinking` blocks -> message-envelope
  `reasoning_details`
- `src/openai_compat/wire_lift/tools.rs` -- canonical `ToolDef::Custom` ->
  `{type:"function",function:{...}}` wire shape
- `src/openai_compat/wire_lift/tool_use.rs` -- assistant `tool_use` content
  blocks -> top-level `tool_calls` array
- `src/openai_compat/wire_lift/tool_result.rs` -- user-message `tool_result`
  blocks -> separate `role:"tool"` wire messages
- `src/openai_compat/wire_lift/tool_choice.rs` -- Anthropic tool_choice tagged
  objects -> OpenAI bare-string / function-object form
- `src/openai_compat/wire_lift/response_format.rs` -- `output_config.format`
  -> top-level `response_format`; strips Anthropic-only field

### openai_responses

- `src/openai_responses/mod.rs` -- `impl Provider` orchestration:
  force-streams `complete()`, drains to `response.completed`; upstream-error
  mapping helpers (`map_responses_upstream_error` builds
  `Error::upstream_full`, lifting AWS mantle `__type`/`code` tokens and
  routing a 403 body through the `aws_error` scrub). On the mantle lane (cfg
  `bedrock`) both dispatch paths (`complete`, `stream`) serialize the body to
  bytes and `sign_mantle` the built request BEFORE the header trace and
  execute (the first-party lane is byte-unchanged: same bytes + one
  deterministic content-type), and each records the lane span fields; `probe`
  on a mantle provider routes to `mantle::probe` (credential-resolve, no
  `/models` dial) instead of the api-key GET. Also owns the lane-mapping
  helpers bridging `AuthKind` to the core tag vocabulary: `lane_format_tag`
  -> the lane-faithful tag this lane EMITS (an observation), `lane_scheme`
  -> the `ReplayScheme` this lane ACCEPTS (a revisable judgment; both
  first-party lanes share `Codex`, mantle is `Mantle`)
- `src/openai_responses/client.rs` -- provider construction, config, and auth
  wiring: `OpenAiResponsesConfig` (incl. `session_id` and `installation_id:
  Option<String>`, plus cfg `bedrock` `mantle: Option<MantleAuth>`),
  `OpenAiResponsesProvider` (client build + cookie-jar Drop persistence),
  header assembly. `build_headers` stamps the ChatgptOauth-only `session-id`
  and `x-codex-installation-id` headers in the compiled-defaults phase (before
  the `header_extras` loop, so an operator override still wins; per-request
  UUIDs win after); both are omitted when unset and never appear on the ApiKey
  / BedrockMantle paths. Mantle lane (cfg `bedrock`): `cfg.mantle` `Some`
  builds a no-redirect client (`http_client::build_no_redirect`) with no
  Cloudflare cookie jar, `sign_mantle` post-build signs via
  `crate::mantle::sign`, and `record_mantle_span_fields` stamps
  `lane`/`auth_mode`/`region` on the request span
- `src/openai_responses/types.rs` -- request wire types: `ResponsesRequest`,
  `ResponseInput` wrapper (`Item(ResponseInputItem)` | untagged
  `Passthrough(Value)` re-emitting preserved unknown input kinds),
  `ResponseInputItem` union, `ResponsesTool` flat shape
- `src/openai_responses/response_types.rs` -- response + SSE event wire types
  (`ResponsesResponse`, output-item union, stream events)
- `src/openai_responses/auth.rs` -- `apply` injects only the auth pair per
  `AuthKind` (ChatgptOauth Bearer + `ChatGPT-Account-Id`, ApiKey Bearer,
  legacy BedrockMantle Bearer); the codex identity/fingerprint headers live in
  `client.rs::build_headers`, not here. On the mantle lane (cfg `bedrock`,
  `cfg.mantle` `Some`) `apply` attaches NO `Authorization` -- the SigV4/bearer
  signer owns it post-build -- and the legacy `BedrockMantle -> apply_api_key`
  arm stays only for enum completeness (unreachable once the factory sets the
  block)
- `src/openai_responses/cookies.rs` -- persistent Cloudflare cookie jar
  (allowlist-pinned to non-secret cookie names)
- `src/openai_responses/request.rs` -- orchestrator: builds `ResponsesRequest`
  from `ChatRequest` via system/messages/tools/extras submodules
- `src/openai_responses/system.rs` -- canonical `system` -> Responses
  `instructions` flat string (drops per-block cache_control with DEBUG)
- `src/openai_responses/messages.rs` -- canonical `messages[]` -> Responses
  `input[]` (Message/Reasoning/FunctionCall/FunctionCallOutput items); gates
  reasoning replay per target lane (family recognition + carry/strip/gray);
  restores a reasoning artifact's id and scheme from a self-describing
  envelope on a `redacted_thinking` blob, re-gating the client-controlled
  claim through the same replay ladder; enforces the empty-item floor
  (producers return `Option`, `retain_replayable_reasoning` sweeps before
  emission); also translates `File` content blocks -> `InputFile` items with
  `file_data` or `file_id`; runs every emitted `call_id` (both
  `function_call` and `function_call_output` sites) through `tool_id` so one
  logical id keeps one wire id and a tool result still correlates; an empty
  correlating id fails the request on BOTH output shapes (a tool-role
  message's `tool_call_id` and a `tool_result` part's `tool_use_id`)
- `src/openai_responses/tools.rs` -- canonical tools -> flat Responses
  `{type,name,description,parameters}` shape; tool_choice mapping
- `src/openai_responses/extras.rs` -- reasoning translation + 6-key
  provider_extras allowlist; ChatgptOauth + BedrockMantle `store=false` lock.
  `apply_reasoning` sets `effort` from the canonical value, defaults `summary`
  to `"auto"` only when the caller set none, and overlays the Responses-dialect
  remainder (`summary`/`context`/`mode`/future) carried in
  `provider_extras["reasoning"]`; summary/context/mode are independently
  emission-worthy, an explicit `enabled:false` (no effort) still omits
- `src/openai_responses/response.rs` -- Responses response -> canonical
  (output walk, finish_reason from status, usage); stamps
  `lane_format_tag(auth_kind)` on every emitted reasoning detail
- `src/openai_responses/sse.rs` -- Responses SSE state machine keyed on
  `output_index` (Text/Reasoning/ToolUse blocks); carries the lane on
  `ResponsesStreamState::new` so streamed reasoning details bear the same
  lane tag the non-streaming path emits
- `src/openai_responses/quota_headers.rs` -- tolerant parser for the
  `x-codex-*` quota response-header family (`parse_codex_quota` ->
  `CodexQuota`; None when absent, non-UTF8 values skipped, only
  `active-limit` / `primary-used-percent` / `primary-reset-at` typed and
  every other suffix captured in `extras`); wired into the egress
  complete/stream dir-3 sites, attached to the response and to the first
  stream chunk only

### gemini

Native Google Gemini egress (`generateContent` / `streamGenerateContent`,
  v1beta REST); auth is an API key sent as `x-goog-api-key`, OR (when
  `auth_mode = "cloud-code"`) an OAuth bearer against the Cloud Code surface.
  Gated on the `gemini` cargo feature.

- `src/gemini/mod.rs` -- `GeminiProvider` + `GeminiConfig` (fields: `id`,
  `auth`, `base_url`, `header_extras`, `user_agent`, `mode`); `new_with_auth`
  (api-key) vs `new_cloud_code` (bearer + `Arc<dyn CloudProjectCache>`);
  dispatches `complete`/`stream` to the api-key or cloud-code arm by
  `GeminiAuthMode`; builds the `models/{model}:generateContent` /
  `:streamGenerateContent?alt=sse` URLs and the `GEMINI_FORMAT` (`gemini-v1`)
  reasoning tag; SSE drain; `resolve_lock` single-flight serializing cold
  Cloud Code project resolution (warm path reads the cache lock-free, then
  double-checks under the lock before onboarding once); project-mismatch
  cache-invalidation hook (`clear_if_matches`) on both cloud-code >=400 error
  branches (complete + stream). Wiremock tests pin the cloud-code transport
  (`envelope_wrap_and_response_unwrap`, `stream_unwraps_response_envelope`,
  `onboards_via_loadcodeassist`, `onboards_via_onboarduser`,
  `preserves_reasoning_and_structured_output`)
- `src/gemini/cloudcode.rs` -- Cloud Code ("antigravity") egress:
  `GeminiAuthMode` enum; `cloudcode-pa` `/v1internal:{generate,stream}Content`
  paths; `{project,request,model}` request envelope + `response`-wrapper
  unwrap (non-stream and per-SSE-chunk); project-id resolution via
  `loadCodeAssist` falling back to polled `onboardUser`, cached through a
  `CloudProjectCache`; antigravity short UA on generate/stream/loadCodeAssist
  + Node UA on onboardUser; control-plane headers; `is_project_mismatch`
  predicate (bare `PERMISSION_DENIED`/`NOT_FOUND` classifier on an `Upstream`
  error only; auth/quota/5xx/transport left untouched) gating the mod.rs cache
  invalidation
- `src/gemini/auth.rs` -- `apply` injects the `x-goog-api-key` header (no
  `Authorization`) for the api-key mode; key resolved per request so a managed
  token source can rotate
- `src/gemini/types.rs` -- Gemini wire types (`GeminiResponse`,
  `Content`/`Part`, `ThinkingConfig` (`thinkingBudget` | `thinkingLevel`
  oneof), `UsageMetadata` incl.
  `cachedContentTokenCount` / `thoughtsTokenCount`)
- `src/gemini/request.rs` -- `ChatRequest` -> Gemini body: system ->
  `systemInstruction`, messages -> `contents`/`parts`, tools ->
  `functionDeclarations`, `build_thinking_config` (Gemini-3+ ->
  `thinkingLevel` string by effort, selected by model generation; older
  -> numeric `thinkingBudget` verbatim / effort table / dynamic `-1`;
  `includeThoughts`), `build_response_format`
  (json_schema / json_object -> `responseMimeType` + `clean_schema`-ed
  `responseSchema`; unrecognized shape warns),
  thought-part replay carrying `thoughtSignature`; `split_base64_data_uri`
  is the one RFC 2397 base64 `data:` URI parser (params before `;base64,`
  tolerated), shared by the image arm (via `data_uri_inline_data`) and the
  `File` arm, which reads the canonical inner OpenAI object the Anthropic
  and Converse egresses read; every bytes-carrying arm (`Image`,
  `ImageUrl`, `File`, `Document`) drops-with-warn rather than emit a part
  with no bytes (a `data:` URI never falls through to text; a non-base64
  or reference-only source never becomes empty `inlineData`);
  `warn_dropped_cache_control`
  emits the drop-with-warn breadcrumb for caller `cache_control` markers
  (Gemini has no breakpoint surface), matching the openai-compat/responses
  egresses
- `src/gemini/schema.rs` -- `clean_schema`: pure JSON-Schema -> Gemini
  OpenAPI-subset cleaner shared by tool `parameters` and
  `generationConfig.responseSchema` (oneOf -> anyOf, strip
  `$schema`/`$ref`/`additionalProperties`, nullable-union lift, numeric-enum
  coercion, uppercased `type`), recursing nested objects/arrays/combinators
- `src/gemini/response.rs` -- Gemini response -> canonical `ChatResponse`;
  `translate_usage` maps `cachedContentTokenCount` ->
  `cache_read_input_tokens` and `thoughtsTokenCount` -> `reasoning_tokens`
- `src/gemini/sse.rs` -- per-chunk `streamGenerateContent` SSE parsing ->
  canonical `ChatChunk` (text + thought parts, usage)
- `src/gemini/sse_tests.rs` -- streaming-path unit tests for the SSE parser

### bedrock

- `src/bedrock/mod.rs` -- `BedrockProvider`; topology comment for Invoke vs
  Converse dispatch
- `src/bedrock/auth.rs` -- AWS credential resolution (`Bearer` short-circuit,
  `SigV4` via `SharedCredentialsProvider`)
- `src/bedrock/signing.rs` -- SigV4 signing entry points; merges
  Authorization/x-amz-date/x-amz-security-token onto request. `apply` signs in
  the `bedrock` scope; `apply_with_service` takes the service scope as a
  parameter so non-bedrock AWS-signed lanes (mantle) can reuse the same signer
- `src/bedrock/endpoint.rs` -- region-to-bedrock-runtime URL builders;
  ARN/bracket-suffix path encoding
- `src/bedrock/frame.rs` -- shared AWS-eventstream framing driver for both
  Bedrock egresses; owns the byte loop, the 12-byte prelude/length/CRC
  invariants, the `MAX_FRAME_BYTES` 8 MB DoS cap, decode-error recovery, and
  the WARN/TRACE log-hygiene split (prelude-only at WARN, full payload hex at
  TRACE); both the InvokeModel-stream and ConverseStream decoders delegate to
  `decode_frames`. Also owns `frame_type` (protocol frame classification from
  `:message-type` / `:event-type` / `:exception-type`) and `exception_error`
  (exception member name -> HTTP status), shared so the two lanes classify
  upstream failures identically
- `src/bedrock/eventstream.rs` -- InvokeModel-stream frame handler / payload
  interpreter (base64-unwrap of Anthropic SSE per frame); delegates the
  framing byte loop and DoS cap to `frame.rs`
- `src/bedrock/invoke.rs` -- InvokeModel adapter: reuses
  `anthropic_api::request::normalize`, patches `anthropic_version:
  "bedrock-2023-05-31"`, and applies the structured-outputs body-beta union
  LAST (after both allowlist filters) so a body shipping
  `output_config.format` never egresses without its gating flag
- `src/bedrock/betas.rs` -- shared `anthropic_beta` allowlist filter (Invoke
  body + Converse `additionalModelRequestFields`)
- `src/bedrock/body_fields.rs` -- shared `allowed_body_fields` filter against
  AWS strict-schema 400s

### bedrock/converse

- `src/bedrock/converse/mod.rs` -- groups Converse adapter (vendor-neutral
  envelope)
- `src/bedrock/converse/types.rs` -- request wire types (`ConverseRequest`,
  AWS-shape content blocks, `ToolConfig`, `InferenceConfig`,
  `ConverseDocument` with its optional `citations` /
  `ConverseCitationsConfig`)
- `src/bedrock/converse/response_types.rs` -- response + ConverseStream event
  wire types
- `src/bedrock/converse/request.rs` -- canonical -> Converse request body
  orchestrator (camelCase + `additionalModelRequestFields`)
- `src/bedrock/converse/system.rs` -- canonical `system` -> Converse
  `[{text}|{cachePoint}]` block array
- `src/bedrock/converse/messages.rs` -- canonical messages -> Converse
  messages (per-role dispatch, cachePoint interleave). Three
  document-construction paths: the canonical message-content document
  (`translate_document` -> typed `ConverseDocument`), the raw Anthropic-shape
  tool_result document, and the canonical tool_result document
  (`document_to_tool_result`); the two tool_result paths both delegate their
  wire value to the shared assembler `tool_result_document_value`.
  `sanitize_document_name` is the single `document.name` charset/length
  enforcement point and `translate_document_citations` the single citations
  bool lift, and all three paths route through them so they cannot drift
- `src/bedrock/converse/tools.rs` -- canonical tools/tool_choice -> Converse
  `toolConfig` ({auto/any/tool} union); backfills a reserved dummy `toolSpec`
  when the translated transcript references tool blocks but no tools survive
- `src/bedrock/converse/extras.rs` -- assembles `additionalModelRequestFields`
  (thinking, anthropic_beta, cache_control, output_config)
- `src/bedrock/converse/response.rs` -- Converse response body -> canonical
  (content walk, stopReason map, cacheDetails -> cache_creation)
- `src/bedrock/converse/eventstream.rs` -- ConverseStream binary-frame
  decoder; per-block-index state map

### Tests

- `tests/common/mod.rs` -- thin re-export shim of `routectl_core::test_utils`
  (the single source of truth for the canonical scenario builders); enabled
  via the `test-utils` dev-dependency feature on core
- `tests/anthropic_api.rs` -- coordinator for the Anthropic Messages API
  egress test binary: gates the whole binary on `anthropic-api`, holds the
  shared fixtures/helpers, and declares the per-scenario submodules under
  `tests/anthropic_api/` (`request_normalization`, `cache_control`,
  `response_normalization`, `sse`, `integration`, `beta_headers`,
  `count_tokens`, `unified_quota`, `probe`, and `mantle` under cfg `bedrock`)
  via `#[path]` so they stay one binary; covers wiremock-based complete +
  stream (incl. `anthropic-ratelimit-unified-*` quota carrier wire-in:
  complete populates `ChatResponse.upstream_meta`, stream carries it on the
  first chunk only, absent family yields None). `mantle` pins the Bedrock
  mantle lane against a mock upstream: bearer- and SigV4-signed egress
  (`AWS4-HMAC-SHA256` `Authorization` scoped to
  `.../bedrock-mantle/aws4_request` + `x-amz-date`), no `x-api-key`,
  `anthropic-version` + bare model id on the wire, a signed `count_tokens`, a
  no-redirect client, and an AWS-shaped 403 (`SignatureDoesNotMatch`)
  round-tripping to `FailureClass::Auth`
- `tests/anthropic_overage_tracing.rs` -- captured-subscriber tracing coverage
  for the overage-flip log: a flip into overage emits one WARN with the
  non-secret quota fields, steady state is silent, recovery emits one INFO;
  isolated binary so the thread-local capture subscriber does not leak
- `tests/context_management.rs` -- wiremock-driven complete() + streaming
  end-to-end for context-management emulation; asserts beta-header strip,
  context_management body-key strip, and thinking-block injection; gated on
  `#[cfg(feature = "anthropic-api")]` (run with `--features test-utils` to
  exercise helpers that pre-populate the thinking cache)
- `tests/openai_compat.rs` -- wiremock-based complete + stream tests for
  openai-compat egress (DeepSeek multi-turn, etc.); the cfg(`bedrock`)
  `mantle` submodule pins the mantle lane on the wire (bearer + SigV4
  `bedrock-mantle` scope, no first-party Bearer, no-redirect 3xx, 501
  count_tokens, credential-resolve probe, and end-to-end AWS-403 scrub->Auth +
  429 retry_after preservation)
- `tests/bedrock_streaming.rs` -- scoped Bedrock integration tests over the
  public credential-resolution / auth-dispatch API (`bedrock::auth::resolve`
  Bearer vs SigV4 variants across regions)
- `tests/contract_egress.rs` -- canonical -> Anthropic+openai-compat wire body
  snapshots via insta
- `tests/contract_egress_bedrock_invoke.rs` -- canonical -> Bedrock-Invoke
  (Anthropic-shape) body snapshots
- `tests/contract_egress_bedrock_converse.rs` -- canonical -> Bedrock-Converse
  vendor-neutral body snapshots
- `tests/contract_egress_openai_responses.rs` -- canonical -> OpenAI Responses
  body snapshots; pins flat tool/tool_choice shapes
- `tests/contract_response_egress.rs` -- canned upstream body -> canonical
  `ChatResponse` (Anthropic + openai-compat)
- `tests/contract_stream_egress.rs` -- canned SSE bodies through `stream()`
  asserting canonical chunk sequence (catches stream-ordering and usage-merge
  regressions)

## routectl-router

- `src/lib.rs` -- crate root; re-exports `Config`, `Router`, `ResolvedModel`,
  factory builders, the `activation` inventory types (`ActivationState`,
  `ActivationEntry`, `ActivationStatus`, `UnresolvedReason`,
  `ActivatedChange`, `DeactivatedChange`, `ActivationDelta`,
  `compute_activation`, `diff` as `diff_activation`), and the `doctor` report
  types (`Status`, `Finding`, `WouldTrimPanel`, `DoctorPanels`,
  `DoctorReport`, `ProbeOutcome`, `overall_exit`)
- `src/activation.rs` -- PURE auto-activation inventory (leaf module; imports
  ONLY `config` types, `catalog::is_cataloged_provider_kind`, and
  `routectl_auth::LocalProbe` -- no `router`/`factory` dep, so activation
  state is "never traffic" by construction). `compute_activation(probes:
  &[(&str, LocalProbe)], config) -> ActivationState` is pure + infallible: the
  candidate universe is the caller-supplied probe slice (each
  `routectl_auth::oauth::known_provider_ids()` id paired with its local
  probe), each id maps via the hardcoded `provider_kind_for_id` table
  (`anthropic`->`anthropic-api`, `codex`->`openai-responses`,
  `xai`->`openai-compat`, `antigravity`->`gemini`) gated by
  `is_cataloged_provider_kind` -- an uncataloged kind (today `gemini`) yields
  `Unresolved{NotCataloged}` regardless of probe outcome, otherwise
  `Present`->`Activated`, `Missing`/`Expired`/`StoreUnavailable`->the matching
  `UnresolvedReason` (snake_case reason codes; a future `#[non_exhaustive]`
  `LocalProbe` variant maps conservatively to `Unknown`).
  `referenced_by_aliases` is true iff a configured provider's `api_key_ref()`
  names `oauth://<id>` (bare or `#seat-label`) AND that provider is reachable
  through the alias->nickname->model->provider walk
  (`reachable_model_nicknames` follows every alias value recursively,
  alias-key-wins-over-model shadowing, glob-key values included). `diff(prev,
  next) -> ActivationDelta` is pure and emits NO tracing (the server maps
  deltas to events): newly-activated + newly-deactivated (with the new reason)
  only, nothing for unchanged or reason-only-among-unresolved changes.
  Redaction: every field is a display-safe discriminant (provider id, kind
  token, reason code, bool) -- never a token, path, or env value.
  `ActivationState` is a read-only newtype over a `BTreeMap<String,
  ActivationEntry>` (iter/get/len/is_empty; no mutation surface, honoring the
  immutability invariant); growth types are `#[non_exhaustive]`
- `src/doctor.rs` -- serialize-safe `routectl doctor` report data types
  (orchestration + rendering stay CLI-side): `Status` (fixed
  `Pass`/`Warn`/`Fail` triad), `Finding`
  (`section`/`name`/`status`/`detail`/`remediation` -- messages are
  operator-facing, never a token/path/env value), `WouldTrimPanel` +
  `DoctorPanels` (extensible `Option`-field panel bag; router-local mirror of
  the usage crate's would-trim summary since router does not depend on usage),
  and the capability truth-matrix panel types
  `CapabilityMatrixPanel`/`MatrixLane`/`MatrixCell`/`MatrixAvailability`
  (rows=lanes, cells=verdict+source+age resolved through
  `capability_display::resolve_display_verdict`, availability tri-state
  available/empty/unavailable so a read failure can never render as an empty
  registry), and `DoctorReport { schema_version, findings, panels }`.
  `overall_exit(&[Finding]) -> i32` is the STABLE exit-code contract shared by
  both diagnostics surfaces: nonzero iff any finding is `Fail`
  (`Pass`/`Warn`/empty -> 0), pure in the slice and order-independent.
  Re-exports `routectl_core::ProbeOutcome`; all these types plus
  `overall_exit` are re-exported from the crate root (`lib.rs`) and consumed
  by the CLI `provider probe` + `doctor` commands
- `src/config/mod.rs` -- Config schema root: the top-level `Config` struct
  (all
  `[server]`/`[providers]`/`[models]`/`[aliases]`/`[retry]`/`[cache]`/`[capability]`/`[registry]`/`[mitm]`/...
  tables) with `version: u32` schema stamp, and `impl Config` (`pricing_for`
  resolves an upstream-id glob -- provider-scoped beats agnostic, then
  longest-prefix -- to a `&PricingConfig`). Re-exports the `schema` +
  `validate` submodules so every internal `crate::config::X` path AND every
  crate-root `routectl_router::` re-export resolves unchanged
- `src/config/schema.rs` -- Config value types: `ProviderEntry` (one variant
  per provider kind incl. the `gemini`-feature-gated `Gemini { api_key_ref,
  base_url, header_extras, payload_extras, user_agent, auth_mode, ... }`;
  constructor `ProviderEntry::gemini`, `with_gemini_auth_mode` sets
  `GeminiAuthMode` {ApiKey default, CloudCode}, `kind_str() == "gemini"`;
  `api_key_ref()` exposes the primary key ref, `None` for Bedrock, so the
  usage CLI can detect `oauth://` subscription providers; `cache_capability()`
  fails closed for anthropic-api on a non-default base URL, plus
  `auto_emit_top_level_breakpoint()`); the cfg(`bedrock`) `BedrockMantleConfig
  { region, creds }` sub-config is reused verbatim across the mantle lanes --
  `bedrock_mantle: Option<BedrockMantleConfig>` on `AnthropicApi`,
  `OpenaiCompat`, and `OpenaiResponses` (its presence selects the Bedrock
  mantle lane, deriving the endpoint from `region`); `OpenaiCompat.base_url`
  is `#[serde(default)]` (empty) so the mantle lane may omit it (validation
  still requires it non-empty on the standard lane); `ModelEntry`
  (`reported_model: Option<String>` response model-label echo override,
  `visible_routectl_provider: bool` default true); `AliasValue`, `ServerAuth`,
  `RegistryEntry`/`PricingConfig` pricing table; `CacheConfig` (global
  `[cache]`: `auto_emit_top_level_breakpoint: bool`, default true) and
  `CacheCapability` (`supports_top_level_cache_control` +
  `cache_hit_observable`, conservative `for_provider_kind` per-kind defaults
  incl. `gemini` -> implicit prefix cache) with per-`ProviderEntry` overrides;
  `MitmConfig` transport-only (bind port, cert dir, upstream/host pin) -- a
  forwarded credential is a per-provider choice
  (`ProviderEntry::AnthropicApi.credential_source: CredentialSource`, `Own`
  default / `Forwarded`), not a `[mitm]` one (a hot-reloaded `[mitm]` edit is
  restart-required via `config_classify::collect_restart_required_changes` in
  routectl-cli); `RetryPolicy` carries `classes: BTreeMap<ConfigFailureClass,
  ClassPolicy>` (per-class retry/fallback overlay resolved by
  `class_policy::RetryPolicy::resolved_class`) and `ProviderRuntimePolicy`
  carries `class_overrides: BTreeMap<u16, ConfigFailureClass>` (operator remap
  of a raw upstream status to a failure class), both empty-by-default and
  defined in `src/class_policy.rs`; `CapabilityConfig` (global `[capability]`:
  `enabled`, `decay_hours`, `inferred_window_hours`, `staleness_hint_days`)
  drives the learned-capability registry and is hot-reloadable. The whole
  `Config` tree derives `schemars::JsonSchema` alongside serde so
  `schema_gen.rs` can render the committed `routectl.schema.json`
  (`class_overrides`, a `BTreeMap<u16, _>`, carries a `#[schemars(with)]`
  override so it renders as a string-keyed object rather than a u16 map)
- `src/config/validate.rs` -- Config version preflight + non-schema
  validation: `version: u32` schema-stamps the file (`CURRENT_CONFIG_VERSION
  == 3`); `preflight_config_version(raw_toml)` reads `version` off the RAW
  TOML before the `deny_unknown_fields` typed deserialize, failing closed with
  `ConfigVersionError` (too-new -> upgrade the binary; too-old -> `config
  migrate`, never mutated on load) rather than a confusing unknown-field
  error; `validate_cache_pricing_retired` rejects a non-empty
  `[cache_pricing]` at v2+ as a hand-edited inconsistency;
  `preflight_legacy_mitm_credential_source(raw_toml)` reads `[mitm]` off the
  RAW TOML and rejects the removed `credential_source` key with an actionable
  error naming the provider-block replacement
- `src/config_error.rs` -- the single production `Config` parse funnel
  `parse_config(text) -> Result<Config, String>` (re-exported from the crate
  root; consumed by `serve`/hot-reload load and every CLI config load) plus
  its did-you-mean enhancer: on a serde `unknown field`/`unknown variant`
  failure it reads the offending token + the candidate list back out of the
  rendered `toml` error `Display`, scores each candidate with
  `strsim::jaro_winkler`, and appends a `did you mean `Y`?` line for the
  closest match at/above `SUGGESTION_THRESHOLD` (0.7); no second field-name
  registry (the candidate list is serde's own, so it cannot drift), and a
  non-matching or low-confidence message passes through unchanged -- never a
  new error class
- `src/config_locate.rs` -- `locate_dotted_path(raw_text, dotted) ->
  Option<usize>`: the 1-based source line of a dotted TOML key path, via
  `toml_edit`'s span-retaining immutable `Document` parse. Lets `config check`
  prefix a semantic validation error with the `config.toml` line that produced
  it; returns `None` (caller falls back to the plain message) when the text
  does not parse, the path is absent, or the key/item carries no span
- `src/config_write.rs` -- the SINGLE config.toml write primitive:
  `edit_config_toml(path, base_bytes_snapshot, edit_fn)` acquires a sibling
  `config.toml.lock` advisory fd-lock, re-reads the file under the lock,
  byte-compares against the caller's snapshot (mismatch =
  `ConfigWriteError::Conflict`, nothing written, no retry loop), parses the
  RE-READ bytes into a `toml_edit::DocumentMut`, runs the caller's closure
  (which owns validation and returns `EditOutcome::{Modified,Unchanged}` or
  its own error, surfaced transparently as `Edit(E)`), and atomically writes
  only on `Modified`. `write_config_atomic` (mode-preserving temp + fsync +
  rename + parent-dir fsync, extracted from `config_migrate`) lives here --
  the one atomic config writer in the workspace. `EditResult` reports
  `committed_len`, deliberately NOT the document bytes (config.toml can hold
  `literal:` secrets; the audit contract forbids logging values). The lock is
  never held across interactive prompts (callers confirm first); consumers:
  `config set`/`unset`, the `config migrate` migrator (its v2->v3 commit
  rung), and the onboarding wizard
- `src/config_path.rs` -- schema-driven dotted-path validator:
  `validate_config_path(dotted) -> Result<PathShape{Scalar,Table}, PathError>`
  walks the OnceLock'd `schema_for!(Config)` object graph segment by segment
  (named properties unioned across `oneOf`/`anyOf` arms, `$ref` deref, map
  nodes accept arbitrary keys via `additionalProperties`), so an unknown
  segment errors NAMING the segment plus its sorted valid siblings BEFORE any
  `toml_edit` mutation. Array/indexed targets are rejected (`ArrayTarget` --
  Vec fields are hand-edit-only), as are quoted segments containing literal
  dots. The schema IS the key registry: a struct rename auto-updates it, no
  drift surface
- `src/config_effective.rs` -- PURE effective-view derivation
  `derive_effective_view(&Config, &CatalogOverlay) -> EffectiveView{models,
  classes, capabilities, aliases, provider_ids}` over the SAME lookups
  `apply_catalog_overlay` uses
  (`lookup_baked_with_overrides` + `lookup_overlay_cell` + `merge`) -- no
  `build_resolved_models`, no secret resolution, no network. `ModelCell`
  carries the merged `EffectiveRow` (source: baked/import/user/disabled) per
  config-referenced `(provider_kind, upstream)`; `ClassPolicyCell` tags each
  failure class `ClassPolicySource::{Config, BakedDefault}` via
  `resolved_class`; `AliasChain` flattens each `[aliases]` entry into its
  ORDERED fallback chain via `AliasValue::nicknames` (Single -> one element,
  Chain verbatim -- the order IS the sequence dispatch walks) and
  `provider_ids` lists the `[providers.X]` keys, so a read surface counts
  aliases/providers off the same view it renders. Consumed by `config show
  --effective` (cli) and the `/status/config` panel
- `src/schema_gen.rs` -- `render_schema_json() -> String`: the single source
  of the committed `routectl.schema.json` at the repo root, rendered from
  `schemars::schema_for!(Config)` as pretty JSON with a trailing newline
  (deterministic). Stamps two root markers: `x-routectl-config-version` (=
  `CURRENT_CONFIG_VERSION`, metadata only -- NOT a hard constraint on the
  `version` field, so editors keep accepting older migratable configs) and
  `x-generated` (the `@generated by `cargo run --bin gen_schema`` warning). A
  golden test (`committed_schema_matches_render`, gated on the
  `bedrock`+`openai-responses`+`gemini` features) diffs the committed file
  byte-for-byte against this function's output so the two cannot silently
  diverge
- `src/class_policy.rs` -- config-facing failure-class policy overlay + the
  adapters to/from the canonical `routectl_core::failure_class::FailureClass`.
  `ConfigFailureClass` is the closed, kebab-case-serialized, operator-nameable
  subset (the ten classes; omits `Unknown`, flattens `FeatureUnsupported`'s
  upstream `capability` token away); `to_failure_class` re-synthesizes
  `FeatureUnsupported` with the stable `OPERATOR_REMAP_CAPABILITY`
  (`operator-remap`) provenance token, `from_failure_class` returns `None` for
  `Unknown` and (fail-closed) any future `#[non_exhaustive]` variant.
  `ClassPolicy { retry: Option<u32>, fallback: Option<bool> }`
  (`deny_unknown_fields`, each leaf independently optional) is the per-class
  overlay. `RetryPolicy::resolved_class(&FailureClass) -> (retry_cap,
  fallback)` lives here as an `impl RetryPolicy` extension: it layers a
  present `ClassPolicy` leaf over the baked class-default matrix (reproduces
  the router's `retry_cap_for`/`should_fallback` outcomes for an empty
  overlay), overriding only the leaf the operator set. Both
  `ConfigFailureClass` and `ClassPolicy` derive `schemars::JsonSchema` so they
  surface in the committed `routectl.schema.json`
- `src/config_migrate.rs` -- the config schema migration ladder as a PURE
  planning phase (no on-disk mutation until the caller commits).
  `plan_migration(&DocumentMut, raw_version, &cache_pricing, overlay_path) ->
  Result<MigrationPlan, MigrateError>` runs every ladder transform in memory
  and every refusal/conflict check up front, so a returned plan means all
  validation has passed and nothing has been written. `MigrationPlan { from,
  to, write_kind: WriteKind, removed_keys, steps }` carries a human-readable
  `removed_keys` summary plus the `write_kind`, which FOLDS the write payloads
  into its variants so the illegal "candidate present but nothing to write"
  state is unrepresentable: `WriteKind` is `NoChange` / `ConfigOnly(String)`
  (the fully-migrated config text) / `ConfigAndOverlay(String, OverlayWrite)`
  (config text + the pending overlay fold to commit FIRST).
  `MigrationPlan::config_candidate() -> Option<&str>` and `overlay_candidate()
  -> Option<&OverlayWrite>` are read-models over `write_kind` (no separate
  `Option` fields to `.expect()` a cross-field invariant against).
  `OverlayWrite { base_revision, cells }` is a pending revision-checked
  overlay save. `plan_v1_overlay` (private) computes the v1 `[cache_pricing]`
  -> catalog-overlay fold WITHOUT writing: it loads the overlay (a read),
  validates + merges candidate cells, and returns `Some(OverlayWrite)`
  (new/edited cells), `None` (idempotent no-op -- every candidate already
  matches), or `Err(Conflict)` (a pre-existing DIFFERENT overlay value fails
  closed, for ANY key). `apply_config_transforms(&mut DocumentMut,
  raw_version) -> Result<Vec<StepOutcome>, Refusal>` is the pure document-only
  ladder (v1->v2 via `apply_v1_to_v2_doc` stamping the LITERAL `2` and
  dropping `[cache_pricing]`, then v2->v3, then the v3->v3 same-version
  normalization) -- both `plan_migration` (on a clone, to build the candidate)
  and the caller's commit closure (on the re-read document under the write
  lock) call it, so the committed bytes reproduce exactly what planning gated.
  `migrate_v2_to_v3(&mut DocumentMut) -> Result<StepOutcome, Refusal>` retires
  the per-status `retry_allowlist`/`retry_denylist` keys, stamping `version =
  3` and dropping both keys only when they carry NO behavior, else
  `Refusal::BehaviorBearing` (non-empty lists have no lossless
  `[retry.classes.*]` fold, carrying rendered per-code guidance) /
  `Refusal::Malformed` (a non-`u16` entry, refused rather than silently
  dropped) with the document byte-untouched.
  `normalize_capability_overrides(&mut DocumentMut) -> Result<bool, Refusal>`
  folds legacy `unsupported_features` into `[capability.overrides]`
  (same-version, no version bump, idempotent), refusing on a behavior-bearing
  egress allowlist (`Refusal::EgressAllowlist`). `RefusalSource`
  (`Allowlist`/`Denylist`/`Both`) names which retired key(s) bore the cause.
  `MigrationError` (`InvalidSelector`/`InvalidOverride`/`Conflict`/`Overlay`)
  is the overlay-fold error; `MigrateError` (`V1ToV2(#[from] MigrationError)`,
  `Refused(Refusal)`, `VersionTooNew{found, supported}`) is the planner's
  error. `StepOutcome{from_version, to_version}` records what each rung
  stamped. `LATEST_MIGRATION_VERSION` (a LITERAL, compile-time-asserted `>=
  CURRENT_CONFIG_VERSION`) is the highest version the ladder can produce --
  deliberately NOT the const, so a bare bump of `CURRENT_CONFIG_VERSION`
  without a matching rung fails the build. The two-file commit (overlay first,
  config last as the visible completion marker) is NOT literally atomic
  without a journal; it is recoverable instead -- a crash between phases
  leaves config.toml at its old version, so a rerun re-plans (the overlay fold
  is now a no-op) and completes the config stamp
- `src/factory/mod.rs` -- provider-factory module root; declares the private
  `build`/`validate`/`warnings` submodules (plus the cfg(`openai-responses`)
  `installation_id` submodule) and re-exports the crate-facing surface
  (`build_provider`, `build_provider_with_options`, `build_resolved_models`,
  `apply_catalog_overlay`, `BuildOptions`, the `validate_*` family,
  `resolved_codex_version`, `collect_config_validation`, `ConfigValidation`,
  `validate_bedrock_global_config`, `class_policy_warnings`,
  `codex_identity_warnings`) so `crate::factory::X` and the `lib.rs` `pub use
  factory::{...}` block resolve unchanged
- `src/factory/build.rs` -- secret resolution +
  `build_provider`/`build_resolved_models`; one build arm per `ProviderEntry`
  kind (incl. the `gemini`-feature-gated arm: the `ApiKey` mode resolves the
  `x-goog-api-key` source, the `CloudCode` mode requires an `oauth://` ref,
  resolves the bearer, and wires an `OAuthStoreProjectCache` into
  `GeminiConfig::new_cloud_code`); OAuth credential-pool expansion (a
  bare-pool `oauth://<provider>` ref with >1 stored seat builds one
  seat-pinned provider per seat via `list_seats`); `apply_catalog_overlay`
  runs the two-layer catalog merge once at chain-build/load time and stamps
  the result onto each `ResolvedModel::effective_row`, so the dispatch path
  never re-runs the merge per request; calls the row-reading `validate_*`
  guards from `validate.rs` at construction time; the `AnthropicApi` arm, when
  `bedrock_mantle` is `Some` (cfg `bedrock`), derives `base_url` from
  `mantle::mantle_anthropic_base(region)`, resolves the AWS credential via
  `resolve_bedrock_creds` + `bedrock::auth::resolve` (fail-fast probe for
  Profile/DefaultChain, shared with the `Bedrock` arm), and builds the config
  with `mantle: Some(MantleAuth)` and an empty api-key token; the
  `OpenaiCompat` and `OpenaiResponses` arms mirror this against
  `mantle::mantle_openai_base(region)` (the Responses arm also sets the
  `BedrockMantle` runtime auth-kind marker), so all three mantle lanes derive
  their endpoint from `region` alone -- the legacy us-east-1 default-endpoint
  fallback (and its WARN) is gone, unreachable behind the validation rejects
- `src/factory/validate.rs` -- the config-row `validate_*` family + validation
  collection (incl. `validate_registry_patterns`, rejecting malformed
  `[registry]` glob keys at startup); `validate_class_policy` HARD-rejects an
  operator override of the reserved `[retry.classes.feature-unsupported]` key
  and any `[providers.X.class_overrides]` remap whose target falls outside
  `{bad-request, content-policy, context-window, feature-unsupported}`
  (`ALLOWED_REMAP_TARGETS`) -- a remap may only move a status into a terminal,
  non-retrying class, naming the offending provider/status/target on reject;
  `class_token` renders a `ConfigFailureClass` as its kebab-case TOML
  spelling; `collect_config_validation(&Config) -> ConfigValidation` is the
  single ordered invocation of the whole `validate_*` suite (bare-message
  contract: each error stripped to its inner `Error::Config` text), routed
  through by all four config surfaces -- `config check`, `test`,
  `prompt-size`, and the `serve` pre-parse gate -- so a validator can never be
  silently present on one path and missing from another; the suite also runs
  `validate_float_fields` (rejects a non-finite float leaf -- NaN/inf in a
  `[registry]`/`[cache_pricing]` price or a non-positive/non-finite
  `retry.backoff_multiplier`) and `validate_base_urls` (rejects an
  `openai-compat` entry with no/empty `base_url` -- it is now defaulted-empty
  in the schema but REQUIRED non-empty on the standard lane, skipped only when
  `bedrock_mantle` selects the mantle lane; rejects a present-but-empty
  `base_url` on the other kinds, whose omitted field keeps its kind default,
  mirroring `validate_base_url_scheme`'s own empty-string rejection); the
  cfg(`bedrock`) `validate_provider_bedrock_mantle` (anthropic-api lane) and
  `validate_provider_openai_mantle` (openai-compat + openai-responses lanes)
  reject an incoherent mantle sub-config -- with `bedrock_mantle` set,
  `api_key_ref`/`base_url` must be empty and `region` non-empty; the
  openai-responses lane additionally rejects a set `account_id_ref` and a
  `store` key in `payload_extras`, and CLOSES the legacy bearer-only surface
  (`auth_kind = "bedrock-mantle"` without the block is a hard error naming the
  block form, regardless of base_url); the cfg(`bedrock`)
  `validate_bedrock_creds_refs` rejects a present-but-empty required creds ref
  (after trim) via one shared per-descriptor check across the native Bedrock
  lane (`creds`) and all three `bedrock_mantle.creds` lanes --
  `BearerKey.key_ref`, `Static.access_key_ref`/`secret_key_ref` must be
  non-empty and a present-but-empty `Static.session_token_ref` (`Some("")`) is
  rejected while an omitted one is fine, closing the config-check hole the
  secret-ref parse walk's empty-slot skip left open (`Profile`/`DefaultChain`
  have no ref to check); `validate_codex_version` rejects a syntactically
  illegal `codex_version` (empty, > 64 bytes, or a non-printable-ASCII byte --
  never sanitized) and ERRORS when two openai-responses entries set DIFFERENT
  values (the codex identity is process-global; the error names both
  providers), with `resolved_codex_version(&Config)` returning the single
  agreed value (None -> pinned) for the factory's `set_resolved`; the `[mitm]`
  validator is deliberately excluded (router-build-specific)
- `src/factory/warnings.rs` -- non-fatal config warnings;
  `class_policy_warnings` is the advisory twin of `validate_class_policy` over
  the same surface (never fails): a `class_overrides` remap whose SOURCE
  status is a breaker health signal (`is_health_status`: 408, 429, any
  500..=599), and an empty `[retry.classes.<c>]` block (both leaves `None`);
  `warn_context_management_needs_preserve` flags the `context_management` +
  `history_reasoning != "preserve"` inconsistency once at build time;
  `codex_identity_warnings` flags a chatgpt-oauth openai-responses provider
  whose `header_extras` overrides `version` or `user-agent` with a value
  diverging from the derived codex identity (the override still WINS --
  advisory only)
- `src/factory/installation_id.rs` -- (cfg `openai-responses`) read-or-create
  the persistent per-installation UUIDv4 at `<config-dir>/installation_id`;
  `resolve_installation_id` adopts an existing valid file
  (lowercase-normalized), re-mints an empty/corrupt one, and mints an absent
  one via `routectl_auth::atomic_write::write_0600_atomic` (owner-only 0600);
  any read/write failure yields `None` + a structured WARN naming the error
  class (never the UUID), so the egress simply omits the
  `x-codex-installation-id` header and the next construction retries; the
  openai-responses `build` arm resolves it for the ChatgptOauth surface only
  and stamps it on `OpenAiResponsesConfig::installation_id`
- `src/glob.rs` -- `[aliases]` table suffix-glob parser + longest-prefix
  lookup index (`AliasPattern`, `PrefixIndex`)
- `src/catalog.rs` -- two-layer catalog: layer 1 is the baked reference table
  (`CatalogRow`:
  `wm`/`rm`/`ttl_seconds`/`min_prefix_tokens`/`max_context_tokens`/`input_cost_per_token`/`output_cost_per_token`/`capabilities`,
  keyed `(provider_kind, model[, tier])`, `TABLE` populated from
  `catalog_baked::baked_cells` at startup), layer 2 is `catalog_overlay.json`.
  `lookup(provider_kind, model, tier)` does three-tier fallback (exact-or-glob
  -> provider `"*"` catch-all -> conservative `CatalogRow::sentinel()`);
  `lookup_baked_with_overrides` reports a genuine catalog miss as `None`
  instead of a sentinel. `merge(baked_row, overlay_cell) -> EffectiveRow`
  (`Present{row, source, verified_at}` / `Disabled` / `Missing`) is the ONLY
  place provenance is computed -- `CatalogRow` itself carries none; overlay
  wins over baked, JSON `null` disables. `CachePricingOverride` (config.toml
  `[cache_pricing]`, legacy-retired at v2; `validate()` delegates its
  `wm`/`rm`/`max_context_tokens` structural checks to `cell_value_defects`
  then layers its ack-gated posture on the result) and `CachePricingSelector`
  (`"provider_kind:model_glob"` parse/format) round out the override surface.
  `cell_value_defects(wm, rm, max_context_tokens, input_cost_per_token,
  output_cost_per_token) -> Vec<CellDefect>` (both
  `pub(crate)`) is the ONE home of the cell-value invariants, shared with the
  overlay `load` path: `CellDefect` classifies each degeneracy (HARD
  `ReadMultiplier`/`WriteMultiplierNotFinite`/`ZeroMaxContextTokens` vs the
  one SOFT `WriteMultiplierBelowSentinel`; `is_hard`/`field`/`describe`
  accessors) and every caller owns its posture on the result.
  `warn_if_stale()` emits a startup WARN for any effective row >90 days stale.
  `is_stale_days`/`is_stale_days_today` parameterize the staleness horizon
  (the config staleness hint); `today_epoch_day()` and `epoch_day_age(date,
  today) -> Option<i64>` (whole-day age clamped at zero, `None` on an
  unparseable stamp) share the one epoch-day parse so a rendered age never
  disagrees with a stale flag
- `src/catalog_overlay.rs` -- Layer-2 on-disk store (`catalog_overlay.json`):
  `CatalogOverlay{schema_version, revision, cells}`, `OverlayCell` (sparse
  per-field overrides + `source: OverlaySource{Import,User}` + `verified_at`),
  three-state `Option<Option<OverlayCell>>` cell semantics
  (absent/disabled/value). `load`/`save` are fail-closed (corrupt or too-new
  `schema_version` errors, missing file -> empty overlay) and revision-checked
  (`save`'s `expected_revision` rejects a stale write, no auto-retry); `load`
  additionally validates cell VALUE degeneracy per cell via the shared
  `catalog::cell_value_defects` predicate -- any HARD defect (rm <= 0 or
  non-finite, non-finite wm, `max_context_tokens` of 0) fails closed naming
  the selector + field, the one SOFT below-sentinel `wm` defect warns and is
  accepted (a hot-reload load failure keeps the prior router live); writer
  extends the OAuth credentials-file atomic-write discipline with a
  post-rename parent-directory `fsync`. Designed as an extraction seam: only
  two router-crate touch points remain (`config::routectl_config_dir` via
  `default_path`, `catalog::cell_value_defects` via `load`), no
  `routectl_core` type imports
- `src/catalog_import.rs` -- PURE import pipeline: candidate build + diff +
  shrink decision, zero I/O. `build_import_candidate(origin, litellm,
  models_dev, verified_at) -> ImportCandidate{origin, verified_at, cells,
  skipped}` runs `derive_cells` with an empty allowlist and the
  GROUP-AND-AGREE mapper (a field lands on the candidate `OverlayCell` only
  when every tiered cell in a selector's group agrees on it -- keeps a shared
  5m/1h overlay key from picking up one tier's `wm`), rejecting selectors
  absent from the baked table; each `skipped` entry carries a public
  `SkipKind` discriminator (`CrossCheckDisagreement` / `UnknownSelector` /
  `DegenerateValue` / the fail-safe `Other` default) beside its human
  `reason`. `diff_overlay(current_overlay, candidate, baked) ->
  ImportDiff{applied, skipped, conflicted, cleared}`: a `source: import` cell (or an
  absent key) sorts into `applied`; a `source: user` cell OR an explicit
  disable ALWAYS sorts into `conflicted`
  (`ExistingCell::{Absent,Disabled,Present}` -- import never overwrites a user
  cell, no equal-value carve-out); `cleared` names the selectors whose stale
  `source: import` cell must be REMOVED because this run skipped them for a
  `CrossCheckDisagreement` (`stale_import_cells`, gated on the public
  `is_import_cell` -- without it an older snapshot's import cell would keep
  overriding the vetted baked row forever; `source: user` cells and explicit
  disables are never cleared, and no other skip kind clears at all); each
  `DiffRow` carries the escalated
  `ImpactClass` (via `catalog_state::classify_field`/`escalate`) of every
  field that differs from the baked-or-existing effective value, plus a
  `cheaper_direction` flag (`wm` down or `rm` up -- lowers the break-even
  reuse count in `cost_gate::break_even_k`). `shrink_guard(candidate_counts,
  baseline_counts) -> ShrinkVerdict` is a pure
  per-source-90%/per-family-(<=5-exact,>5-80%) floor check with no bypass
  parameter -- the CLI decides whether to honor a shrink rejection;
  `candidate_shrink_counts` (counts a candidate's admitted cells PLUS its
  `CrossCheckDisagreement`-skipped selectors -- those models have not
  vanished, their two sources merely disagreed under the empty allowlist;
  every other skip kind stays uncounted so a genuinely-vanished selector still
  trips the guard), `baked_shrink_counts`, and `baked_row_map` derive the
  caller-supplied counts and baked-row map, partitioning by `family_table()`
  (provider kind as source; `openai-compat`'s per-vendor `models_dev_provider`
  as the finer family, so one vendor's snapshot truncating cannot hide behind
  the others)
- `src/catalog_import_state.rs` -- non-behavioral import-baseline sidecar
  (`catalog_import_state.json`, same warn-and-rebuild posture as
  `catalog_state.rs`, never fail-closed like the overlay):
  `CatalogImportState{schema_version, last_import_date, per_source_counts,
  per_family_counts, source_hashes}`; `load_baseline(path, baked_fallback) ->
  ShrinkCounts` folds a missing file (quiet, first run) or any load failure
  (corrupt/too-new, warned once) into the caller's baked-table fallback;
  `load_last_import(path) -> Option<CatalogImportState>` is the read-only
  diagnostic loader (doctor freshness section) folding missing/unreadable into
  `None`; `persist_baseline` writes the just-completed import's counts
  atomically (0600, temp-file fsync, rename, parent-dir fsync, no revision
  check) for the next import's `catalog_import::shrink_guard` to compare
  against
- `src/catalog_baked.rs` -- GENERATED (`@generated` header, DO NOT EDIT): the
  checked-in baked catalog table (`CATALOG_VERSION`, `CATALOG_SNAPSHOT_DATE`,
  `baked_cells()`). Regenerate via `cargo run --bin gen_catalog` then `cargo
  fmt`; see `catalog_codegen.rs` for the derivation
- `src/catalog_codegen.rs` -- codegen core shared by the `gen_catalog` bin and
  its own drift-guard test: derives `catalog_baked.rs` from two vendored JSON
  snapshots under `catalog_data/` (`models_dev.json` primary for economics +
  `structured_output`, `litellm_model_prices_and_context_window.json`
  cross-check + sole source for `web_search`/`computer_use`/1h cache-write
  tier); CROSS-CHECK fails generation on a models.dev-vs-litellm disagreement
  unless `catalog_data/cross_check_allowlist.json` allowlists that exact key.
  The `CROSS_CHECK_MISMATCH_MARKER` const +
  `reason_is_cross_check_mismatch(reason)` predicate (both `pub(crate)`) let
  `catalog_import` distinguish a genuine source disagreement from a
  missing-key / absent-data `Err` when tagging a skipped selector's
  `SkipKind`.
  `ttl_seconds`/`min_prefix_tokens`/`auto_cacher`/provider-catch-all rows are
  curated constants (neither vendor feed publishes them). Base per-token rates
  (`input_cost_per_token`/`output_cost_per_token`) go through the same
  cross-check, normalizing models.dev's per-million-token unit to per-token;
  a `price_ambiguous` selector stays priced-`None` rather than baking one
  representative model's rate, and `narrow_rate` drops any rate that is
  negative/non-finite at source OR that overflows to infinity / underflows a
  positive value to zero on the `f64 -> f32` cast (a source zero passes
  through as a real free tier). Renders through
  `rustfmt` before writing; the drift-guard test diffs byte-for-byte against
  the committed file
- `src/catalog_codegen_selectors.rs` -- static data tables for
  `catalog_codegen.rs`: which vendored snapshot entries become baked cells,
  plus the curated per-family facts (`ttl_seconds`, `min_prefix_tokens`,
  `auto_cacher`, `economics_unconfirmed`/`context_ambiguous`/`price_ambiguous`
  escape hatches)
- `src/bin/gen_catalog.rs` -- `cargo run --bin gen_catalog` regenerates
  `src/catalog_baked.rs` from `catalog_data/` via
  `catalog_codegen::render_catalog_baked_rs`; fails loudly on a parse error or
  un-allowlisted mismatch, never writes a partial file
- `src/bin/gen_schema.rs` -- `cargo run --bin gen_schema` (re)writes the
  committed `routectl.schema.json` at the repo root from
  `schema_gen::render_schema_json`; re-run and commit the result whenever the
  config surface changes (the `schema_gen` golden test enforces this)
- `../../routectl.schema.json` -- GENERATED (repo-root artifact, `@generated`,
  do not edit by hand): the JSON Schema for the `config.toml` surface, for
  editor autocomplete / validation. Regenerate via `cargo run --bin
  gen_schema`; pinned by the `schema_gen` golden test
- `catalog_data/` -- vendored snapshot inputs to codegen: `models_dev.json`,
  `litellm_model_prices_and_context_window.json`,
  `cross_check_allowlist.json`, `NOTICE` (source URLs + licenses for the two
  vendored snapshots)
- `src/catalog_state.rs` -- cross-version catalog drift observability
  (`catalog_state.json`, separate from and never as behavioral as the
  overlay): `check_drift_and_persist_state` diffs the prior in-use-selector
  snapshot against today's baked rows on a `CATALOG_VERSION` change and emits
  one structured WARN per changed selector. NEVER fails serve -- every
  I/O/corruption error is caught and treated as rebuild-from-empty; writer
  discipline extends the OAuth file-write standard with the same post-rename
  parent-directory `fsync` as the overlay, but deliberately carries no
  revision check (last-write-wins is harmless for rebuildable data)
- `src/cost_gate.rs` -- PURE break-even cost gate consuming
  `catalog::CatalogRow`; advisory-only (no live mutation, no reuse estimator,
  no dispatch wiring). `break_even_k(row, &PrefixReductionCandidate{d,
  c_after, c})` returns the minimum reuse count `K*` (None when `d == 0`);
  branches on `row.auto_cacher` -- write-premium `K* = (c_after*wm)/(d*rm)`,
  auto-cacher `K* = (c_after*(1-rm))/(d*rm)`. `evaluate(row, &candidate, k) ->
  GateDecision` applies guards in order, each KEEP-ing: `d==0 || c_after==0`
  -> `Keep{NoCandidate}`, `(c-d) < min_prefix_tokens` ->
  `Keep{BelowMinPrefix}`, else the branch inequality `k*d*rm > one_time_tax`
  -> `Break{delta_tokens}` or `Keep{NetNegative}`. Trust-gating is no longer
  this module's concern: a row's provenance lives on the two-layer merge
  result (`catalog::EffectiveRow`), and callers decide whether to invoke
  `evaluate`/`break_even_k` at all -- a `Disabled`/`Missing` merge result
  never reaches this module (`KeepReason::InsufficientData` still exists as a
  variant callers may use for that gate). `GateDecision` (`#[non_exhaustive]`:
  `Keep{KeepReason}` / `Break{delta_tokens}` / reserved
  `FreeBreak{delta_tokens, reason}` for a later eviction-guard phase) has
  `strategy_str()` returning stable append-only ledger tokens
  (`cost_gate:keep` / `cost_gate:break` / `cost_gate:free_break`).
  `KeepReason` and `PrefixReductionCandidate` are also `#[non_exhaustive]`;
  `f32` row multipliers are widened to `f64` for the arithmetic. Imports ONLY
  the pricing row + std -- no Config/Router/provider/async
- `src/resolved.rs` -- `ResolvedModel` carrying provider, upstream, reasoning
  defaults, header/payload extras per `[models.X]`; optional `seats` slice
  (one `SeatTarget` per OAuth pool seat, `None` for the single-seat /
  non-pooled case); `reported_model: Option<String>` (per-model response-echo
  override)
- `src/router/mod.rs` -- the `Router` type family + construction/lifecycle
  plus submodule wiring; the dispatch retry state machine itself lives in
  `dispatch.rs`. Holds the `Router` struct (providers map, per-`[models.X]`
  runtime gates, resolved-model map, learned-capability + override + k-sample
  registries) and the `RouterOptions` / `DispatchMeta` / `Dispatched` /
  `DispatchedStream` / `RouterMetrics` / `DispatchTarget` types with their
  non-dispatch impls (`DispatchTarget.capability_prior`, the metric counters,
  `DispatchMeta::for_alias`/`mark_target`; `DispatchMeta` carries the additive
  capability ride-alongs `learned_capabilities` / `capability_observations` /
  `cleared_capabilities` plus the `replay_degradation` reasoning-replay
  degradation record read once by the per-request WARN, and `served_seat` --
  the credential identity of the target that ACTUALLY served, copied by
  `mark_target` from `DispatchTarget.seat` and left `None` on the
  forwarded-credential path). Construction + hot-reload lifecycle: `new`,
  `install_resolved_models`, the `carry_over_runtime_state_from` /
  `carry_over_sticky_from` / `carry_over_k_store_from` /
  `carry_over_learned_from` reload carries + `note_overlay_revision`, the
  `catalog_version` / `overlay_revision` getters and
  `rebuild_learned_from_ledger` (the boot warm-rebuild seam: delegates to
  `capability_rebuild::rebuild_capabilities_into` over the PRIVATE learned
  registry so it stays encapsulated), `register`, `record_k_sample`,
  `resolve_nickname`, `has_forwarded_provider`, `override_registry`. Also owns
  the header-compose constants
  (`AUTH_HEADERS`/`MANAGED_HEADERS`/`LIST_VALUED_HEADERS`, consumed by
  `overlays`) and `ALIAS_MAX_RECURSION_DEPTH` (consumed by `chain`), and
  declares the per-concern submodules: `dispatch`, `class_observe`, `chain`,
  `overlays`, `feature_filter`, `capability_learn`, `capability_observe`,
  `capability_cleared`, `cache_plan`, `runtime_gate`, `sticky`,
  `count_tokens`, `status`, `replay_repair`
- `src/router/dispatch.rs` -- the dispatch retry state machine (the module
  exempt from the line-size target: `complete`/`stream` are one retry loop and
  the lossy-trim live-cut lands here). Public API:
  `complete`/`complete_with_options` + `stream`/`stream_with_options` and
  their `complete_inner`/`stream_inner` loop bodies, which walk the resolved
  chain (from `chain::dispatch_chain_for_request`) attempting each
  `DispatchTarget`, retrying within a provider per `RetryPolicy` with
  exponential backoff (`add_jitter`/`mul_duration`, `INLOOP_RETRY_AFTER_CAP`);
  `rate_limit_reset_hint` clamps an `Error::Upstream.retry_after` to the
  configured ceiling and folds a small reset into the next sleep (a larger
  reset parks the provider via the breaker instead). `run_with_timeout`,
  `wrap_with_breaker_accounting`, and `try_stream_with_first_content` (buffers
  content-free leading chunks -- a `delta.role` opener, id/model metadata --
  until the first content-bearing chunk per `is_content_bearing`, so the
  fallback boundary is first CONTENT, not stream-open) wrap each
  attempt; `policy_for` + `compose_attempt_policy` resolve the per-attempt
  policy. Failure-class routing threads
  `routectl_core::failure_class::classify` through every error arm:
  `apply_remap(native, status, overrides)` (status via
  `upstream_status_for_remap`) applies the target's `class_overrides` keeping
  the native `matched_by`, then `should_fallback` /
  `should_retry_same_provider` / `retry_cap_for` decide the outcome and the
  class-decoupled `class_debits` set gates breaker debit (`pub` and re-exported
  at the crate root, so the `/status/config` panel reports each class's
  `debits_breaker` from this ONE definition rather than restating the set). Class-decision
  observability: `emit_class_observability` emits one `emit_class_decision`
  event per arm (with `ClassDecisionObs` / `DispatchSurface` / `UpstreamFacts`
  / `upstream_facts`, safe structured dimensions only) plus a
  `routectl::feature_unsupported` INFO via `emit_feature_unsupported` on a
  lift, and bumps the `RouterMetrics` counters. Would-trim recording lives
  here with the loop: `record_would_trim` + `would_trim_k_floor_for_meta` +
  `record_near_lossless_marks` (`NEAR_LOSSLESS_RECORDER_VERSION`). The
  dispatch-path context reducer (`apply_json_minify`,
  `reduction_strategy_token` -> `DispatchMeta.reduction_strategy`) runs after
  overlays and before the auto-cache injection call
  `maybe_apply_auto_cache_control` (gated by
  `cache_plan::AutoCacheRequestPlan`, mapping `cache_plan::CacheInjection` ->
  `DispatchMeta.cache_strategy`); both run on `complete`/`stream` only, never
  `count_tokens`. Forwarded-credential handling:
  `missing_forwarded_bearer_error` refuses a `use_forwarded_credential` target
  with no client bearer BEFORE egress, `resolve_reported_model` keeps the
  client's requested model verbatim, and `forwarded_terminal_status` +
  `log_forwarded_auth_terminal` surface a 401/403/429 on a forwarded
  credential verbatim (no refresh, no fallback). The class/remap helpers are
  `pub(super)`, shared with `count_tokens` and `feature_filter`; the pure
  classification/observability labels (`DispatchSurface`,
  `UpstreamFacts`/`upstream_facts`, `class_label`, `matched_by_label`) now
  live in `class_observe` and are imported from there. The reasoning-replay
  strip-repair branch lives here too: a fixed one-shot correctness arm
  (bounded additive, never nested across fallback) that on a classified
  replay rejection admits via `replay_repair`, swaps to the stripped variant,
  and retries once; `replay_rejection_body_free` rebuilds the rejection with
  no upstream body before generic logging, and `emit_replay_degradation`
  fires the single per-request degradation WARN
- `src/router/replay_repair.rs` -- the router-side reasoning-replay carry
  admission + strip-repair settlement the dispatch arm drives:
  `ReplayCarryPlan` / `plan_replay_carry` decide carry-once vs proactive
  strip against the `learned_replay` registry, and the plan settles the
  two-phase learn (`commit` on stripped-repair success, `settle_success` ->
  clear-on-carried-success returning the `CapabilityClearedEvent` rows for
  `DispatchMeta.cleared_capabilities`, unsettled `Drop` on an unrelated error)
  and records the `DispatchMeta.replay_degradation` summary
- `src/router/class_observe.rs` -- pure classification/observability leaf
  shared across the dispatch surfaces: `DispatchSurface` (+ `as_str`),
  `UpstreamFacts` (+ `upstream_facts`, the safe-facts extractor that carries
  only numeric status and structured classifier tokens, never
  body/prompt/header/Display text), `class_label` (fail-closed low-cardinality
  `FailureClass` token), and `matched_by_label`. All `pub(super)`, imported by
  `dispatch`, `count_tokens`, and `feature_filter`; the shared leaf keeps
  `feature_filter` from importing out of `dispatch`, so the import edge
  between them is one-way
- `src/router/chain.rs` -- alias/chain resolution and expansion into dispatch
  targets:
  `resolve_v6_alias`/`resolve_default_alias`/`expand_alias_value`/`dispatch_chain`/`dispatch_chain_for_request`,
  `expand_chain_to_targets` (+ its only-when-None post-loop provider_kind
  fill) and `push_seat_targets`, plus the
  `into_one_dispatch_target`/`dispatch_target_for_seat`
  (provider_kind-at-construction) target builders
- `src/router/status.rs` -- route-target status facade for the /status
  surface: `RouteTargetStatus` (re-exported from `router` for the crate-root
  path `routectl_router::router::RouteTargetStatus`), `Router::status_targets`
  (per-target gate + seat snapshot), `Router::seat_count_for`, and the private
  `gate_status_for` gate read
- `src/router/count_tokens.rs` -- the token-counting dispatch path
  (`Router::count_tokens` + `count_tokens_try_seat`): walks past
  count_tokens-incapable targets by provider kind, runs no reducer/cache, so
  it never touches the would-trim seam. Owns the `pub(super)`
  `COUNT_TOKENS_CAPABLE_KIND` (the sole count_tokens-capable egress kind) and
  `CountSeatOutcome` (the per-seat walk outcome)
- `src/router/overlays.rs` -- layered header/payload overlay merge:
  `apply_layered_overlays` (per-target header/payload/beta/reasoning
  overlays), `operator_betas`, the `pub
  merge_header_extras`/`merge_payload_extras` deep-merge helpers
  (anthropic-beta comma-union, model>provider>ingress precedence),
  `deep_merge_value`, and the `is_auth_reserved`/`is_managed_reserved` guards
- `src/router/feature_filter.rs` -- capability pre-filter + strip-interceptor
  application: `filter_chain_by_features` (alias-chain pre-filter with the
  prior/learned soft-drop tail: prior-demoted targets sort ahead of
  learned-demoted ones), `unsupported_feature_for_target` (override hard-drop
  > learned strip/route > verified-working mask > catalog prior, all under the
  capability kill switch; F2 learned negatives always route away, never strip;
  a resident acting VerifiedWorking positive masks a `Some(false)` catalog
  prior via `learned_capabilities.is_verified_working`),
  `beta_pinned_for_target`, `override_forces_supported`,
  `apply_strip_interceptor` (the pre-dispatch strip hook -> `StripDecision`),
  the `FilterSource`/`StripDecision` enums, and the
  `catalog_capabilities`/`emit_feature_unsupported` helpers
- `src/router/capability_learn.rs` -- learned-capability observation, expiry,
  and snapshot: `CapabilityLearnEvent` (the ledger event),
  `observe_for_learning` (the 400/422 capture gate: kill switch + status +
  remapped/forwarded + resolve, then delegates), `commit_learned_observation`
  (the resolved-capability mint pipeline split out of it --
  request-membership, force_supported mask, probe-settle, per-request dedupe,
  the mint + phase-carrying learn WARN + `CapabilityLearnEvent` ride-along,
  and the F2-specific mint gates: an F2 feature-naming candidate mints ONLY on
  self-identifying evidence of a deterministic request fault --
  `f2_evidence_is_mintable`/`f2_class_is_deterministic` pure predicates -- and
  never when an F1 negative for the same capability was already seen earlier
  in this attempt chain via the cross-lane `LearnDedupeKey::F1Seen {
  feature_key }` marker; a same-chain-F1 F2 candidate is dropped with a
  suppression WARN + `f2_same_chain_suppressed_total` deduped cross-lane on
  the capability (`LearnDedupeKey::F2Suppressed`), the reverse F2-then-F1
  order self-heals; an F1 negative records F1Seen only once it ACTS (a
  self-identifying F1 on its first observation, an inferred F1 once
  corroborated -- a still-pending inferred F1 must not mask a stronger F2) and
  a re-probe that reconfirms a resident acting F1 (`settled_negative_phase`)
  counts too; acting mints bump the phase-split
  `learned_negatives_f1_total`/`_f2_total`),
  `observe_bedrock_validation_drift` (WARN +
  `bedrock_validation_unmatched_total` counter, deduped once per request per
  target, when the matcher attributed no capability yet the rejection was a
  flat bedrock `ValidationException`), `observe_feature_naming_drift` (WARN +
  `feature_naming_unmatched_total` counter, deduped once per request per
  target, when a deterministic feature-carrying 400/422 against a provider
  that HAS a feature-naming table matched no template -- gated via
  `capability_matcher::has_feature_naming_table` so it never fires on
  tableless providers),
  `expire_learned_on_override_change`/`override_identity_for` (targeted expiry
  on override-cell change), `learned_capability_snapshot` (the status
  read-model), and `learned_replay` (the crate-internal `&self` delegate handing
  the dispatch arm the `ReplayLearnRegistry` for its carry-slot claim)
- `src/router/capability_observe.rs` -- response-evidence observer: the
  SUCCESS-arm mirror of `observe_for_learning`, run inline on the terminal
  successful NON-STREAMING response (the streaming arm records nothing -- no
  assembled response, fail closed). `CapabilityObserveEvent` (the additive
  `DispatchMeta.capability_observations` ride-along row: capability_key,
  evidence_class, direction, tier, source + state_key, provider_kind,
  request_features -- the columns a later warm-rebuild replays; NO ledger
  write, mirroring `CapabilityLearnEvent`), `observe_capabilities`
  (kill-switch + no-provider-kind short-circuits, builds the `DetectorContext`
  from the request, runs the pure `capability_detect::detect` slice, dedupes
  one observation per `(state_key, capability)` per request),
  `admit_observation` (stage-two admission with `now` +
  `EvidenceSource::Live`: `observe_positive` for a Verified positive,
  `observe` at `FailurePhase::F3`/`SignalTier::Inferred` for a suspected
  absence; on an acting outcome emits a structured WARN -- token + state_key
  only -- bumps `RouterMetrics::verified_working_total`/`f3_suspect_total`,
  and rides the event out on `meta`). Pure `DetectorContext` derivation
  helpers: strict-output via `derive_feature_keys` STRUCTURED_OUTPUT
  membership, `schema_required_keys` (top-level `required` from
  `output_config.format.schema` or a strict tool's `input_schema`),
  `forces_web_search` (bounded `tool_choice` directive read),
  `reasoning_requested`, `cache_requested`
- `src/router/capability_cleared.rs` -- `CapabilityClearedEvent`, the additive
  `DispatchMeta.cleared_capabilities` ride-along row (state_key,
  capability_key, provider_kind -- the registry key of a resident negative a
  successful re-probe cleared in memory). Collected ONLY at
  `LearnedProbeGuard::settle_success` (a same-capability rejection refreshes
  with backoff, a drop records a transient error -- neither clears), threaded
  onto the meta at the `dispatch.rs` success arms so the usage-capture layer
  persists a `cleared` ledger row and a warm rebuild removes the same resident
  negative on boot rather than resurrecting it; NO ledger write here,
  mirroring `CapabilityLearnEvent` / `CapabilityObserveEvent`
- `src/router/cache_plan.rs` -- the pure-read auto-cache request plan:
  `AutoCacheRequestPlan` (per-request frozen-floor / volatile-veto /
  global-switch gate, built once) and `CacheInjection` (variants -> stable
  `strategy_str()` decision tokens). The injection CALL
  (`maybe_apply_auto_cache_control`) stays in the dispatch module
- `src/router/runtime_gate.rs` -- breaker/RPM gate + probe-slot admission
  RAII: `gate_check`, the
  `record_success`/`record_failure`/`record_failure_opened`/`park_provider`/`release_probe_slot`
  breaker-accounting methods, `force_open_breaker`/`breaker_open_for`, the
  `ProbeSlotGuard`/`ProbeAdmission`/`LearnedProbeGuard`/`ProbeAdmissionSet`
  RAII guards (Drop-settles the probe slot on every outcome incl.
  cancellation), and
  `emit_probe_settlement`/`is_probe_request`/`log_probe_fast_fail`
- `src/router/sticky.rs` -- sticky seat ordering + capacity snapshots:
  `sticky_seat_order` (resolve session pin -> gather non-mutating per-seat
  `capacity_snapshot_for` reads -> `seat_pool::sticky_least_loaded_order`),
  `gather_capacity_snapshots`, and `apply_sticky_outcome` (stamps the
  selection_decision token)
- `src/runtime_state.rs` -- per-model (nickname-keyed) token-bucket RPM
  limiter + circuit breaker state machine; `force_open` parks the breaker for
  an explicit reset hint, bypassing the consecutive-failure threshold;
  `capacity_snapshot(&self, now)` is a non-mutating read (`CapacitySnapshot` =
  projected RPM headroom + `CircuitPhase` {Closed, Open, HalfOpenReady} +
  `is_dispatchable`) for least-loaded seat selection -- it shares the leak
  arithmetic with `refill_tokens` (via `projected_tokens`) so the two cannot
  drift, and never claims the half-open probe slot the way a
  `try_dispatch`-based read would
- `src/seat_pool.rs` -- OAuth credential-pool glue: `SeatTarget` (seat-pinned
  provider + per-seat `state_key`), `seat_state_key` (bare nickname for the
  default seat, `nickname#label` for labeled seats), `seat_identity` (the
  persistable `provider#label` credential identity of a `SecretRef`, `None`
  for every non-OAuth scheme so no path or env-var name reaches the usage
  ledger), `seat_order_for_request`
  + `RoundRobinCursors` (per-pool `AtomicUsize` rotating the start seat per
  request under `RoundRobin`; `FillFirst` walks a fixed default-first order);
  `StickyLeastLoaded` selection adds `StickyPins` (a bounded-LRU `session_key
  -> SeatPin{state_key, repinned}` map carried across hot-reload via
  `carry_over_sticky_from`), the pure comparator `pick_least_loaded`
  (dispatchable + Closed-preferred + max RPM headroom + deterministic
  anti-herd tiebreak), and `sticky_least_loaded_order` returning the walk
  order + a `SelectionOutcome` (Birth / Stay / one-time OverflowRepin with
  hysteresis / DeferNoHealthy), home-first with the fill-first tail kept as
  fallback
- `src/feature_keys.rs` -- feature-key derivation for the alias-chain
  pre-filter; walks `ToolDef::Other(v)["type"]` strings and strips date
  suffixes (e.g. `_20250305`) so `unsupported_features` on
  `ProviderRuntimePolicy` can match capability-class regardless of vendor
  versioning; `ToolDef::Custom` (user-defined tools) does not contribute
  tool-type keys. ALSO emits the request-derived `structured_output` key
  (appended after tool-type keys) when the request needs constrained decoding
  -- either `provider_extras["output_config"]["format"]` is non-null, the
  canonical top-level `response_format` requests json (`json_schema` /
  `json_object`), or any tool is strict (`ToolDef::Custom.strict == Some(true)`
  or `ToolDef::Other` with `"strict": true`);
  `derive_feature_keys(tools, provider_extras, response_format)` is pure
- `src/learned_capability.rs` -- bounded in-memory capability-truth registry
  keyed `(state_key, normalized feature_key) -> LearnedEntry`,
  verdict-discriminated by `EntryVerdict` (a `Verified` VerifiedWorking
  positive vs a learned `Negative`) so positives and negatives coexist on
  distinct keys. Each entry carries a `SignalTier`, an observation count,
  `expires_at`, an `in_flight` flag, plus a `FailurePhase` + `EvidenceSource`
  attribution threaded through every contract surface -- the snapshot row
  (which also exposes the derived read-model `Verdict` via `read_verdict`,
  consistent with `Verdict::from_parts`), export (which carries the raw
  `EntryVerdict` for an identical round-trip), and the
  `RoutingDecision::RouteAway { signal, phase }` variant. Mint paths take
  `source` from the caller (`observe`/`observe_positive` carry an
  `EvidenceSource` param; every live call site passes `Live`). Negative acting
  rule: a self-identifying signal counts as unsupported at 1 observation, an
  inferred signal only at >=2 observations inside the decay window (the
  F3-suspect negative reuses this exact path with `FailurePhase::F3` +
  `SignalTier::Inferred` -- no new threshold). `observe_positive` admits a
  VerifiedWorking positive: acts on 1 observation, never decays (`is_expired`
  excludes it), never claims a re-probe slot, never backs off, and NO-OPs when
  any negative resides (a passive positive never clears a negative).
  Write-path recency: a fresh negative observation REPLACES a resident
  VerifiedWorking. The dispatch gate `acting_negative_for` is keyed on
  (verdict, phase, source) via `acting_decision`: `Verified` and acting
  F3+Live -> `Allow` (F3+Live is advisory-only, visible in the snapshot but
  routes nothing); acting F1/F2 and F3+Probe -> `RouteAway`.
  `is_verified_working` answers the filter's prior-pass mask query. Expiry is
  lazy and gates single-flight probe admission; `record_probe_outcome` clears
  the entry on a probe success, refreshes it with a capped exponential backoff
  on a reject, and releases the in-flight slot on any other outcome.
  `snapshot`/`export`/`import`/`clear` surface and seed the registry; a
  1024-entry cap evicts the oldest `last_seen` first. Constructed via `new` or
  `from_capability_config` (decay / inferred-window from the `[capability]`
  hours + the shared `DEFAULT_MAX_ENTRIES` cap) -- the shared sizing path for
  both the router build and the doctor's read-only one-shot ledger rebuild;
  `LearnedCapabilityRegistry` is a public export for that reuse. Also exposes
  `negative_state -> NegativeState{Absent,Acting,Lapsed}`, the read-only decay
  view that never claims the probe slot -- crate-internal, consumed by the
  sibling `learned_replay` lifecycle, which runs its own admission discipline
- `src/learned_replay.rs` -- the reasoning-replay learned lifecycle layered
  over the registry. Crate-internal surface: nothing here is re-exported from
  the crate root, so the whole lifecycle is reachable only from the router's
  own dispatch arm. `ReplayLearnKey::new(state_key, provider_kind, lane
  scheme, artifact scheme)` builds the `(scheme_tag, target_lane)` identity:
  the lane discriminant is the configured target state key plus the lane's
  `ReplayScheme` token and the capability key is `reasoning_replay:<artifact
  scheme>` -- the caller-supplied model string never enters a key, so sibling
  models on one lane share ONE learned truth. `ReplayLearnRegistry::
  admit_provisional -> Option<ReplayProbeGuard>` is the per-pair single-flight
  claim: `Some` means carry the artifacts (unknown or lapsed pair, no other
  probe outstanding), `None` means strip. The guard is the two-phase learn --
  `commit` persists the negative and returns the `CapabilityLearnEvent`
  emission row ONLY after the stripped repair succeeded, `clear` drops the
  entry when the carry itself succeeded and returns a `CapabilityClearedEvent`
  when a resident negative was removed (rides out on the meta so a warm rebuild
  cannot resurrect it), and an unsettled `Drop` (plus a test-only explicit
  `release`) frees the slot without learning. Together those cover the four
  decay settlements: lapse -> one carry, success -> clear, same rejection ->
  refresh, unrelated error -> release unchanged
- `src/capability_rebuild.rs` -- boot warm-rebuild of the learned registry
  from the persisted capability-event ledger, mirroring the K estimator's
  `rebuild.rs`. Owns the `CapabilityLedgerReader` dependency-inversion trait
  (`tombstone` + `read_events`, the concrete reader lives in the binary that
  bridges the usage crate), the router-side `CapabilityEventRow`
  (`#[non_exhaustive]`, built through `new`, carrying `observed_at: Instant`
  plus the RAW persisted verdict/phase/source/tier/evidence-class tokens so
  replay parses them open-set-tolerant), the `ReplayTombstone` boundary
  descriptor (`rowid` + stamped `catalog_version`/`overlay_revision`), and the
  `CapabilityRebuildSummary` tally. `should_replay(event, tombstone) ->
  Replay|Skip` is the pure replay-boundary seam: Skip at-or-before the
  tombstone rowid, and Skip a post-tombstone straggler whose revision differs
  from the boundary's (post-tombstone survival is deliberately NOT
  unconditional). `rebuild_capabilities_into(reader, registry)` replays
  survivors oldest-first (same-instant rows tie-break by rowid, so
  negative-then-cleared is deterministic): the source token is parsed once and
  threaded into the shared admission so probe and live rows run the SAME arms
  -- `verified` -> `observe_positive(source)` (requires a recognized evidence
  class, else skip + WARN), `broken` -> `observe(tier, phase, source)`,
  `suspect` -> `observe(tier, F3, source)` (requires phase `f3` AND a
  recognized evidence class -- the live path always mints suspect at F3 with a
  class, so both fail closed on mismatch), `cleared` -> `remove_keyed` (drops
  the resident negative so a probe-settled clear does not resurrect across
  restart; a probe-sourced clear reaches the same source-agnostic removal
  arm), a probe-source row bumps the by-source `replayed_probe` tally
  alongside its by-verdict counter, unrecognized verdict/source/tier/phase ->
  skip + WARN, never panic. A missing tombstone replays nothing (fail-closed;
  the caller writes the fresh boot tombstone)
- `src/capability_matcher.rs` -- the single shared closed-set resolver mapping
  a use-time upstream rejection to the CANONICAL capability it names, in the
  request-capability namespace (`derive_feature_keys` vocabulary) so learn
  capture, the act-side route-away/strip lookup, and probe same-capability
  settlement all meet on identical keys.
  `resolve_requested_capability(provider_kind, err, cf) -> Option<(FeatureKey,
  SignalTier, FailurePhase)>` is the sole entry point (re-exported at the
  crate root for the capability-probe surface); the third element attributes a
  detection phase (the wire-token arms are `F1`, the feature-naming arm is
  `F2`). Three arms off `FailureClass`: a `FeatureUnsupported` classification
  is self-identifying -- for `openai-compat` the class token is an
  `error.code` (NOT a capability) and `/error/param` is a WIRE param name
  (also not the request key), so it maps the param onto the request-capability
  namespace through a CLOSED set: the `OPENAI_PARAM_TRANSLATIONS` table
  (`response_format` -> `structured_output`) for wire params the request side
  keys differently, plus a passthrough for a param that -- after
  `feature_keys::strip_date_suffix` + `normalize_capability_key` -- is ALREADY
  a `WELL_KNOWN_CAPABILITY_KEYS` entry (a typed built-in an upstream rejects
  by `type`); a param outside that closed set, or a missing param, yields
  `None` (EXCEPT the closed `OPENAI_PARAMLESS_ROUTE_AWAY` set --
  `unsupported_country_region_territory` -- whose paramless geo/region block
  keys on the code token itself). `OPENAI_PARAM_TRANSLATIONS` is the extension
  point for new provider body surfaces. Other providers (Bedrock) carry a
  field path in the token and normalize it directly. The `anthropic-api`
  `BadRequest` arm runs the body's `error.message` through a whole-phrase
  table to yield an inferred signal; the `openai-responses` arm has its own
  table whose replay-rejection row is PREFIX-anchored (the real message ends
  in a variable enumeration of accepted content prefixes) and resolves to
  `REASONING_REPLAY`, never `THINKING` -- the lane rejects a replayed
  artifact, not reasoning itself. The `bedrock` `BadRequest` arm instead
  reads a FLAT `{"__type","message"}` AWS envelope via its own top-level
  `bedrock_flat_field` reader (distinct from the nested
  `upstream_error_field`, same `MAX_ERROR_BODY_BYTES` cap) and runs the
  message through an anchored-template engine (`extract_bedrock_capability`):
  each `(prefix, suffix)` template extracts one token that must pass
  `is_safe_param_token`, normalize via `normalize_capability_key`, and hit a
  CLOSED translation table for a `SelfIdentifying` signal. The arm gates on
  `is_bedrock_validation_exception` (the lifted, namespace-stripped
  `upstream_type == "ValidationException"`) BEFORE the message read: the
  captured must-not-learn rejections (bad model id, unknown beta flag) share
  the learnable rejection's exact flat shape, so only the lifted discriminator
  may unlock a match. `BEDROCK_VALIDATION_TEMPLATES` are grounded in captured
  InvokeModel 400s (`tool type '<type>' is not supported for this model`;
  `<field>: Extra inputs are not permitted`); `BEDROCK_TOKEN_TRANSLATIONS`
  maps the rejected tool type onto the identically-named `derive_feature_keys`
  tool-type key (`advisor` -> `advisor`) -- a rejected wire field name has no
  row and stays dormant. `pub is_bedrock_validation_exception` also lets the
  learn site flag drift when a real `ValidationException` matched no template.
  A third F2
  FEATURE-NAMING arm handles a `BadRequest` whose nested `error.message` names
  the offending feature explicitly: `match_feature_naming` runs the message
  through the same anchored-template engine shape
  (`extract_feature_naming_capability`, `(prefix, suffix)` + a CLOSED
  translation table, `SelfIdentifying` signal at `FailurePhase::F2`) against
  per-provider tables (`ANTHROPIC_FEATURE_NAMING_TEMPLATES` / `_TRANSLATIONS`)
  that also ship EMPTY, so F2 never fires on real traffic until grounded; it
  is tried before the inferred arm so a self-identifying feature name outranks
  inferred prose. `pub has_feature_naming_table(provider_kind)` reports
  whether a provider carries an F2 table, letting the learn site fire
  feature-naming drift only for a provider whose (empty) table matched
  nothing. Both nested body reads share one `upstream_error_field` extractor
  capped at `MAX_ERROR_BODY_BYTES`. Every other class / provider /
  malformed-or-oversized body yields `None` -- no other classification arm
  learns, and unresolvable rejections never learn (this closed-set +
  no-learn-on-unknown replaces the removed cross-namespace capture gate)
- `src/override_registry.rs` -- operator capability-override read-model, built
  from `Config` at `Router::new` (rebuilt on reload since reload constructs a
  fresh Router) and held on `Router` (accessor `Router::override_registry`).
  `OverrideRegistry::build` flattens FOUR sources into one map keyed
  `(target_spec, normalized_capability_key)` carrying PROVENANCE: legacy
  `[providers.X].unsupported_features` -> `RouteAway`/`ProviderStatic` (key =
  provider name); legacy `[models.X].unsupported_features` ->
  `RouteAway`/`ModelStatic` (key = `provider:nickname`); new
  `[capability.overrides.<spec>].unsupported` -> `RouteAway`/`Override`;
  `.force_supported` -> `ForceSupported`/`Override`. Keys normalized via
  `routectl_core::capability::normalize_capability_key` with the target's
  provider kind so a stored override meets a normalized lookup; legacy entries
  keep static provenance so existing configs stay byte-identical in behavior
  AND labels. `resolve(provider, nickname, cap, kind)` consults the
  model-scoped cell before the provider-scoped cell (model wins). A dead-key
  WARN fires per override key that normalization rewrites (dead: it could
  never match). `validate_capability_overrides` (wired into
  `factory::collect_config_validation`, so `serve` load, `config check`, and
  the `config migrate` gate all see it) fails a config where one `(target,
  capability)` cell carries contradictory `RouteAway` + `ForceSupported`
  verdicts, naming the cell and both sources; identical duplicates pass.
  `snapshot()` returns rows. The routing consult that reads this model lives
  in `router.rs`
- `src/capability_display.rs` -- ONE pure read-only display resolver
  `resolve_display_verdict(override_cell, learned, prior) -> DisplayVerdict`
  pinning the within-target precedence `override > learned > verified-working
  > prior > unknown` for the doctor capability matrix panel. An EXTRACTION of
  the order `Router::unsupported_feature_for_target` enforces (that seam is
  side-effecting -- probe admission, `in_flight`, metrics -- so it cannot run
  from a read-only diagnostic). `DisplayVerdict { verdict, supported, source
  }` carries a stable token (the core `Verdict::as_str` vocabulary plus the
  PANEL-ONLY override tokens `FORCED_SUPPORTED` / `FORCED_UNSUPPORTED`), a
  support polarity (`None` only for `unknown`), and a source tag (`override` /
  `live` / `probe` / `prior`, `None` for `unknown`). A sibling drift test
  asserts the order agrees with `router::capability_precedence_matrix_tests`
- `src/capability_strip.rs` -- the strip-vs-route policy plus the single
  request interceptor. `action_for(feature_key) -> CapabilityAction` is the
  const-style policy table: essentials
  (`structured_output`/`computer_use`/`web_search`) and the catch-all `_` are
  `RouteAway` (fail-closed -- an unmapped key is never auto-stripped), while
  the seeded droppables `advisor -> Strip(ToolParam)` and `context_management
  -> Strip(BetaFlag)` are stripped in place. `strip_plan` is the per-key
  transform, able to remove a capability across MORE THAN ONE surface (`tools`
  type, `anthropic_beta` token, `provider_extras` body key) --
  `context_management` spans the `context-management-2025-06-27` beta token
  AND the `context_management` body key; `advisor` strips only its grounded
  tool shape (no beta token is fabricated). `reasoning_replay ->
  Strip(AssistantReasoning)` has NO `strip_plan` row by design: its transform
  is lane-directional, so it lives in `strip_replay_artifacts(req, lane)`,
  which drops the assistant-turn `reasoning_details` whose
  `is_replayable(scheme_of(detail.format), lane)` is `Strip` or `Gray` (the
  repair variant carries no unproven artifact) while keeping proven-portable
  `Carry` details and the legacy plaintext `reasoning` text, mutating through
  the `Arc::make_mut` copy-on-write seam behind a read-only pre-scan so a
  no-op never clones and the rest of the request stays byte-identical
  (prompt-cache affinity). `StripInterceptor` skips that key -- it is not a
  key-only transform -- and is not yet wired to a dispatch repair branch.
  `strip_beta_tokens(feature_key)`
  exposes the beta surface so the dispatch layer's operator-floor-pin guard
  can route away when a stripped token would be re-added downstream.
  `StripInterceptor` (impl of `RequestInterceptor`) applies the transform
  under a snapshot -> strict-pre-check -> strip-in-sorted-key-order ->
  narrow-post-strip-check -> rollback discipline over a per-attempt clone: it
  de-dups/sorts `StripContext.keys` so output is order-independent and
  byte-stable across identical inputs, rejects (400) without mutation under
  `strict`, normalizes an emptied `tools` list back to `None` after a
  tool-surface strip (never serializing `tools: []`, which Anthropic / Bedrock
  Invoke reject with a 400), and on a strip-created hazard restores the
  touched surfaces and returns `Outcome::Reject` for the caller to route away.
  `validate_post_strip(req, stripped_tools)` guards two hazards, and only when
  the strip actually touched the tools surface (`stripped_tools`) so a
  pre-existing invalid request -- e.g. a mandatory `tool_choice` over no tools
  before this run -- is never misclassified as a rollback: a dangling forced
  `tool_choice` naming a removed tool, and a mandatory `tool_choice` over
  now-empty tools (`tool_choice_is_mandatory` matches Anthropic `any`/`tool` +
  OpenAI `required`/`function`; `tools_are_empty` treats
  `None`-after-normalization or an empty list as empty). `Outcome` =
  `Unchanged`/`Stripped`/`Reject`; the dispatch hook lives in
  `router.rs::apply_strip_interceptor`

### Tests

- `tests/factory.rs` -- secret-store-backed provider construction across all
  four provider kinds
- `tests/factory_context_management_warning.rs` -- coverage for the
  `context_management` + `history_reasoning != "preserve"` consistency WARN
  emitted by `build_resolved_models` (fires once when inconsistent; silent
  otherwise)
- `tests/router.rs` -- coordinator for the router integration test binary:
  shared mock-`Provider` fixtures plus `#[path]` wiring of the per-scenario
  submodules under `tests/router/` (`default_alias`, `dispatch_fallback`,
  `dispatch_meta`, `gate_per_attempt`, `reported_model`, `retry`,
  `runtime_policy`); one binary covering fallback-chain semantics and
  runtime-gate behavior
- `tests/learned_capability_loop.rs` -- coordinator for the learned-capability
  loop end-to-end binary: shared upstream/router fixtures plus `#[path]`
  wiring of the scenario submodules under `tests/learned_capability_loop/`
  (`real_envelope`, `learn_and_decay`, `never_learn`, `learned_tail`,
  `streaming`); one binary
- `tests/delta_config.rs` -- pins the delta-config contract through real TOML
  loads: a sparse `[retry.classes.<class>]` single-leaf override inherits
  every other baked default via `resolved_class`; Vec fields replace whole;
  map fields merge per-key; an empty class table equals absent. Test-only --
  the semantics live in `class_policy`/serde, not a merge module
- `tests/codex_identity_fingerprint_coherence.rs` -- single-test binary
  pinning the codex fingerprint-coherence invariant end to end: with
  `codex_version` configured, `build_resolved_models` + a wiremock upstream
  prove the SAME version reaches the egress `User-Agent`, the egress `version`
  identity header, and the UA the OAuth refresh client stamps
  (`codex_user_agent()`, composed with the auth-crate stamping unit test)
- `tests/codex_identity_build.rs` -- single-test binary pinning that
  `build_resolved_models` installs the configured `codex_version` into the
  process-global resolved identity (set-once OnceLock; isolated binary so no
  other test contends for the slot)
- `tests/codex_identity_probe_reject.rs` -- single-test binary pinning the
  unvalidated direct-build path (probe/doctor config load skips
  `validate_codex_version`): an illegal `codex_version` does not panic,
  rejects to the pinned identity, and emits the fallback WARN
- `tests/codex_identity_reload_pending.rs` -- single-test binary walking boot
  -> changed-reload -> same-value re-install: boot INFO fires once, a refused
  re-install with a different value WARNs pending-restart (no false INFO), and
  a same-value no-op stays silent
- `tests/forwarded_auth_terminal_log.rs` -- structured-log safety for the
  forwarded-credential terminal path: an upstream 401/403/429 on a forwarded
  target surfaces verbatim (no refresh, no fallback) with a bounded,
  credential-free log shape (pre-dates the codex-identity work; listed here to
  close a tests-inventory gap)
- `tests/codex_installation_id_boot_stability.rs` -- single-test binary
  pinning installation-id stability across two `build_resolved_models` boots
  over one temp XDG config dir (mint then adopt): the same id reaches the
  egress wire on both boots, and boot 2 leaves the persisted file's content
  and mtime untouched (adoption is read-only)

## routectl-auth

- `src/lib.rs` -- crate root; the public facade. `memory_store`,
  `secret_capture`, `secret_ref`, and `store` are crate-internal modules
  surfaced only through root re-exports: `MemoryStore`, `SecretRef`,
  `SecretStore`, and the secret-capture surface (`ManagedSecretStore`,
  `SecretCaptureError`, `SecretCaptureResult`, `default_secret_dir`,
  `env_ref`); feature-gated re-exports of `LocalProbe`, `LoginOptions`,
  `OAuthError`, `OAuthStore`, `OAuthStoreProjectCache`, `OpenOutcome` under
  `oauth`. Carries `#![warn(missing_docs)]`
- `src/store.rs` -- `SecretStore` async trait (get/set/delete) for credential
  providers
- `src/secret_ref.rs` -- `SecretRef` enum (`env://`, `file://`, `literal:`)
  plus URI parser
- `src/memory_store.rs` -- default in-process `SecretStore` resolving
  env/file/literal references at read-time
- `src/atomic_write.rs` -- crate-internal (`pub(crate)`), unconditional (not
  behind the `oauth` feature) `write_0600_atomic(path, bytes)`: the ONE
  temp-file + fsync + rename + force-`0o600` + parent-dir-fsync sequence
  shared by every routectl-auth secret writer (the OAuth credentials store and
  the secret-capture primitive). FORCE-sets `0o600` (unlike routectl-router's
  mode-PRESERVING `config.toml` writer -- a secrets file has one correct mode
  a routectl writer always asserts); `ensure_dir_0700` self-heals a widened
  parent-dir mode on every call; on any error nothing is persisted at `path`
- `src/secret_capture.rs` -- crate-internal (surfaced via the lib.rs facade),
  unconditional "value/env -> ref" secret-capture primitive.
  `ManagedSecretStore` (owner-only `0700` secrets dir; `open` is fail-closed
  on a non-directory / group-or-world-accessible / unwritable dir and never
  downgrades the posture; `ref_path` percent-encodes the secret name to a
  single traversal-proof filename component; `put` captures a value to a
  `0600` `file://` via `atomic_write` then re-reads-and-byte-verifies,
  removing the file on mismatch), `env_ref(var)` (verifies the var resolves
  non-empty NOW -> `env://` ref, stores nothing), `default_secret_dir()`
  (`$XDG_CONFIG_HOME/routectl/secrets` else `~/.config/routectl/secrets`,
  sibling of `credentials.json`), and
  `SecretCaptureError`/`SecretCaptureResult` (`#[non_exhaustive]`, mapped to
  `Error::Auth` at the CLI boundary via `From`). Consumed by `provider add`;
  source-preference policy (prompt vs env auto-detect) lives in the CLI, not
  here
- `src/oauth/mod.rs` -- crate-internal entry for the OAuth 2.0 PKCE subsystem;
  defines `OAuthError` and re-exports `OAuthStore`, `LoginOptions`,
  `run_login`, `known_provider_ids`, token types
- `src/oauth/types.rs` -- on-disk schema: `CredentialsFile`, `TokenRecord`
  (incl. optional `session_id` and optional `cloud_project_id`),
  `AccountInfo`, `SecretToken` (Drop-zeroized, redacted Debug),
  `SCHEMA_VERSION`, `unix_now`; `seat_key(provider, label)` composes the
  credentials-map key (bare provider for the default seat, `provider#label`
  otherwise) and `CredentialsFile::seats_for_provider` enumerates a provider's
  seats (default first, then sorted labels)
- `src/oauth/file_io.rs` -- atomic load/save of
  `~/.config/routectl/credentials.json`; load is TOCTOU-safe (fstat + `0o600`
  enforcement on Unix), save serializes to pretty JSON and delegates the write
  to the crate's shared `atomic_write::write_0600_atomic` (temp file + fsync +
  rename + force-`0o600` + parent-dir fsync) -- every writer fault surfaces as
  `OAuthError::Io`
- `src/oauth/pkce.rs` -- PKCE verifier / SHA-256 challenge / CSRF state;
  `OsRng`-sourced, Drop-zeroized, constant-time state compare
- `src/oauth/login.rs` -- login flow driver: PKCE bundle, axum callback
  sub-app on loopback, `webbrowser` launch, `--print-url` headless fallback,
  120s timeout; `LoginOptions.label` writes the exchanged record under
  `seat_key(provider, label)` so a labeled login adds a seat without
  overwriting the default
- `src/oauth/rate_limit.rs` -- per-source-port + listener-wide sliding-window
  rate limit on the loopback callback server (turns sustained 400-spam into
  429)
- `src/oauth/store/mod.rs` --
  `OAuthStore`/`Inner`/`SeatCooldown`/`LocalProbe`/`OpenOutcome` shared types
  + open/lifecycle:
  `open`/`open_or_degraded`/`open_default`/`open_default_degradable`
  (start-and-degrade contract),
  `path`/`http`/`read_record`/`load_error_cause`, HTTP client build,
  `sanitize_open_error`; declares the `crud`/`refresh`/`seat` submodules
- `src/oauth/store/crud.rs` -- token-record CRUD + cloud-project-id cache
  adapter: `peek_account_id`/`peek_session_id`/`peek_cloud_project_id`,
  `set_cloud_project_id`, `clear_cloud_project_id_if_matches`
  (compare-and-clear only when the stored id matches, disk-first under
  `update_under_lock`; missing/differing is a no-write `Ok(false)`),
  `write_record`/`remove_provider`/`credential_keys`/`probe_local`/`list`/`logout`
- `src/oauth/store/refresh.rs` -- per-seat single-flight refresh:
  `force_refresh(provider, label)` targets one seat (drives `routectl refresh
  --label`), `refresh_under_lock` (near-expiry 300s lead + 401-recovery +
  reload-generation guard), `reload_from_disk`, transient-failure cooldown
  bookkeeping, and the transient-vs-terminal error classifier
- `src/oauth/store/seat.rs` -- `SecretStore` impl for `OAuthStore`: resolves
  labeled seats by `seat_key`, `list_seats` expands a bare pool ref to one ref
  per stored seat, `get` near-expiry refresh + `on_auth_failure` 401 recovery;
  preserves an existing `session_id` across token rotation (the codex provider
  flow mints the fresh `session_id` on first OAuth exchange);
  `seat_ref_from_key` inverse of `seat_key`
- `src/oauth/providers/mod.rs` -- `OAuthFlow` trait + `lookup` registry +
  `known_provider_ids` (anthropic, codex, antigravity); `AuthParams`,
  `token_parse_error`, and `token_status_error`/`safe_token_error_code`
  (body-free token-endpoint error mapping)
- `src/oauth/providers/anthropic.rs` -- claude.ai OAuth flow:
  `claude.com/cai/oauth/authorize` + `platform.claude.com/v1/oauth/token`,
  `anthropic-beta: oauth-2025-04-20`, manual-paste redirect support
- `src/oauth/providers/codex.rs` -- OpenAI ChatGPT/Codex OAuth 2.0 PKCE flow
  (public client, JWT-derived expiry, lazy refresh-token rotation)
- `src/oauth/providers/antigravity.rs` -- Google/Gemini OAuth 2.0 PKCE flow
  backing `routectl login antigravity`; mints the `oauth://antigravity`
  credential used by the Gemini cloud-code egress; `invalid_grant` on refresh
  status-gates to `RefreshExpired`
- `src/oauth/project_cache.rs` -- `OAuthStoreProjectCache`: a
  `CloudProjectCache` (trait in routectl-core) backed by the OAuth store;
  `get` delegates to `SecretStore::peek_cloud_project_id`, `put` to
  `set_cloud_project_id`, `clear_if_matches` to
  `SecretStore::clear_cloud_project_id_if_matches`, persisting the resolved
  Cloud Code project id into the credential record

### Tests

- `tests/secret_resolution.rs` -- `SecretRef::parse` happy/error paths plus
  `MemoryStore` env/file resolution
- `tests/codex_refresh_tracing.rs` -- refresh-flow tracing coverage for the
  codex (chatgpt-oauth) provider; drives the response decoder through the
  success and 401-`refresh_token_expired` paths under a captured subscriber,
  asserting the contractual structured fields (status,
  `new_refresh_token_present`, sha8) emit without leaking token values

## routectl-usage

Usage-accounting crate: a bounded-channel producer (`UsageHandle`) feeding a
  single background writer thread that persists one row per routed request to
  a local SQLite DB. The hot path never blocks/awaits/panics; overflow and
  disabled-gate drops are counted, not surfaced.

- `src/lib.rs` -- crate root; re-exports `UsageHandle` (+ the `#[doc(hidden)]`
  test-introspection `UsageCounters`),
  `UsageRecord`/`Outcome`/`ParseOutcomeError`,
  `UsageWriter`/`CHANNEL_CAPACITY`,
  `UsageDb`/`open`/`open_readonly`/`open_readonly_fastfail`/`open_rw`/`OpenError`,
  the read-side query surface
  (`aggregate`/`ttfbs`/`latest_quota_by_seat`/`query`/`k_calibration_summary`/`m1_attribution_summary`/`shadow_misfire_summary`/`would_trim_summary`/`read_reuse_samples_since`
  + the capability-ledger reads
  `read_capability_events_after`/`latest_tombstone` + the row/summary types
  `AggRow`/`GroupKey`/`QuotaSnapshot`/`QuerySpec`/`GroupDim`/`RowCost`/`QueryResult`/`QueryGroup`/`QueryMetrics`/`QueryTotals`/`CostStatus`/`KCalibration`/`M1AttributionSummary`/`ShadowMisfireSummary`/`WouldTrimSummary`/`ReuseSampleRow`/`CapabilityEventRow`/`TombstoneRow`/`QueryError`),
  `estimate_cost_tokens`/`CostBreakdown`/`Rates` (+ the `#[doc(hidden)]`
  record-path `estimate_cost`), `CapabilityLearnEvent`, `CapabilityEvent` +
  `insert_capability_event` (the append-only capability-ledger writer,
  re-exported for the CLI capability probe's synchronous insert),
  `MigrateError`, and the `SCHEMA_VERSION` constant; carries
  `#![warn(missing_docs)]`. `prune`/`prune_capability_events`/`PruneOutcome`
  and the `META_*` schema keys are crate-internal (not re-exported)
- `src/record.rs` -- `UsageRecord` (one field per WRITTEN capture column --
  the three write-stopped legacy decision columns carry no field; epoch-ms
  `i64` timestamps, nullable `Option<T>` columns) and the closed `Outcome`
  enum (`ok`, `upstream_error`, `client_disconnect`, `timeout`, `cancelled`,
  `gate_blocked`) with `as_str`/`FromStr` wire tokens that mirror the DB CHECK
  constraint
- `src/learn_event.rs` -- the `capability_learn_events` row + its insert:
  `CapabilityLearnEvent` (plain-type fields only, keeping the crate a leaf --
  the producer pre-normalizes the feature key and stringifies the tier;
  `remapped` is always false by construction but persisted for defensive
  replay; `request_features` is the in-flight derived feature set) and
  `insert_learn_event` (append-only bound-parameter `INSERT`, no dedup / no
  `OR IGNORE`, one row per observation)
- `src/capability_event.rs` -- the unified `capability_events` ledger write
  shape: `CapabilityEvent` (plain-type fields keeping the crate a leaf --
  `ts`, NORMALIZED `lane_key` / `capability`, open-set `verdict` / `phase` /
  `source` / `tier` tokens, nullable `evidence_class` / `upstream_token`,
  `catalog_version` / `overlay_revision` boundary stamps), the
  `CapabilityEvent::tombstone(ts, catalog_version, overlay_revision)`
  boundary-marker constructor (tombstone verdict, empty lane / capability, no
  phase/source/tier/evidence), and `insert_capability_event` (append-only
  bound-parameter `INSERT`, all 11 columns bound, no dedup); the private
  tombstone-verdict literal mirrors the read side's copy in
  `query/capability.rs` (agreement pinned by the round-trip test)
- `src/cost.rs` -- pure leaf-safe cost estimation:
  `estimate_cost(&UsageRecord, &Rates)` and the aggregate-token entry point
  `estimate_cost_tokens(input, output, reasoning, cache_read, cache_write_5m,
  cache_write_1h, &Rates) -> Option<CostBreakdown>` (the record path converts
  its `Option<u64>` fields to `i64` and delegates; per-dimension `tokens *
  rate / 1e6`, `None` when the rate table is fully unpriced, `Some(0.0)` when
  priced with no tokens; reasoning priced only when `Rates.reasoning_per_mtok`
  is set -- the caller sets it to the output rate for a disjoint-reasoning
  provider (Gemini's `thoughtsTokenCount`) and leaves it unset for providers
  that fold reasoning into output, so reasoning is never double-counted);
  `Rates` is a usage-owned mirror of the router `PricingConfig` so the crate
  stays a leaf
- `src/handle.rs` -- `UsageHandle` (cheap `Clone` producer): `try_send` (never
  blocks/awaits/panics -- safe from `Drop`), `try_send_learn_event` (the same
  best-effort discipline for a `CapabilityLearnEvent`, dropping on a
  full/closed channel under its own counter and honoring the shared `enabled`
  gate), `try_send_capability_event` (same discipline for a unified-ledger
  `CapabilityEvent`), runtime-flippable `enabled` gate, shared lock-free
  `UsageCounters` (enqueued / dropped_full / dropped_disabled / persisted /
  write_errors / prune_errors plus the learn-event trio learn_events_enqueued
  / learn_events_dropped_full / learn_events_persisted and the
  capability-event trio capability_events_enqueued /
  capability_events_dropped_full / capability_events_persisted)
- `src/query/mod.rs` -- read-query facade: owns the shared row/error types
  `QueryError` (`Sqlite` + `Interrupted`, the latter distinguishing a fired
  query deadline from a real DB fault, + `InvalidBucket` for a time-bucket grid
  that violates its width/count invariants), `GroupKey`, `AggRow` and re-exports
  the whole read-side surface from the four submodules so every symbol stays at
  `routectl_usage::` unchanged
- `src/query/aggregate.rs` -- aggregate + breakdown queries over the requests
  table; exports `aggregate`, `errors_by_class` (flat per-group failure-class
  breakdown, same window predicate + group key as `aggregate`, sums to
  `AggRow::errors`), `ttfbs`, `latest_quota_by_seat` (newest quota-bearing row
  PER `seat`, eligibility `quota_status OR quota_utilization` non-NULL, NULL-seat
  rows kept as their own bucket), `QuotaSnapshot` (carries `seat` +
  `provider_kind`, the discriminator for the vendor-shared `quota_*` columns),
  `earliest_ts_start` (`MIN(ts_start)` at or after a caller-supplied lower bound,
  `None` when no row qualifies, so a caller can anchor an unbounded window off
  the oldest IN-WINDOW row instead of the epoch). Also
  holds `QUERY_AGG_SQL` / `SERIES_AGG_SQL` + `FineRow` (crate-internal): the
  grouped-query statement and its bucketed twin, both assembled from the same
  `query_agg_select!` / `query_agg_from_where!` / `agg_group_by!` `concat!`
  macros over this module's base column list, so no statement can drift past the
  shared row mappers -- the series statement is the SAME select plus one trailing
  `(ts_start - ?1) / ?5 AS bucket` column and `, bucket` on the GROUP BY
- `src/query/grouped.rs` -- the grouped, priced, deadline-bounded aggregate:
  exports `query(db, &QuerySpec, price, deadline)` plus `QuerySpec`,
  `GroupDim`, `RowCost`, `QueryResult`, `QueryGroup`, `QueryMetrics` /
  `QueryTotals`, `CostStatus`, and the time-series types `BucketSpec` /
  `QuerySeries` / `SeriesBucket`. One statement reads at the fine
  `(model, provider, upstream, alias)` grain with alias/provider filters as
  BIND params; the fold prices each fine row through the caller's closure
  BEFORE upstream is dropped, rolls to the coarse `GroupDim` (sums additive,
  MAX-across-MAX, ratios kept as numerator/denominator pairs), derives the
  display metrics as `Option` (absent, never 0, when no row was eligible), and
  folds totals from the same accumulators. `QuerySpec::bucket` switches on the
  bucketed statement and folds each row ONCE into both the coarse groups and a
  per-bucket accumulator map, so groups / totals / series reconcile by
  construction; the caller-resolved grid is re-checked here (`width_ms > 0`,
  `count` in `1..=1000`, and the last bucket start computed in `i128` so it
  cannot overflow the dense fill) as this crate's trust boundary, and a row whose
  bucket index falls outside the grid is refused before it reaches either
  accumulator -- all as `QueryError::InvalidBucket` rather than asserted; absent
  buckets densify
  through the same `finish` an empty group takes, while a window matching no row
  at all yields an EMPTY series rather than a thousand synthetic zeros. Cost
  enters only via the closure, so the crate stays a leaf; a `progress_handler`
  deadline surfaces as `QueryError::Interrupted` and is detached by an RAII guard
  on every exit path, unwinding included; a cost sum that an extreme configured
  rate overflowed to non-finite is a VALUE outcome (`cost_usd: None` +
  `unpriced`), never an assert, because the fold is network-reachable and the
  release profile aborts on panic.
  `QueryResult`/`QueryGroup`/`QueryMetrics`/`CostStatus`/`QuerySeries`/`SeriesBucket`
  derive `Serialize`: the metric field names ARE the `/status/query` wire
  vocabulary, absent `Option`s serialize as explicit `null` (never skipped,
  `series` included), and `CostStatus` renames to the same lowercase tokens
  `as_str` returns
- `src/query/would_trim.rs` -- would-trim + K-calibration read queries;
  exports `would_trim_summary`, `shadow_misfire_summary`,
  `m1_attribution_summary`, `k_calibration_summary`,
  `read_reuse_samples_since` with their summary/row types (`WouldTrimSummary`,
  `ShadowMisfireSummary`, `M1AttributionSummary`, `KCalibration`,
  `ReuseSampleRow`); carries the COALESCE zero-row guards so a SUM/CASE over
  an empty ledger reads as 0 rather than erroring
- `src/query/capability.rs` -- capability-ledger read queries for the
  warm-rebuild replayer; exports `read_capability_events_after(conn,
  after_rowid, limit)` (rows with rowid > `after_rowid`, ordered `ts ASC,
  rowid ASC`, capped at `limit`) and `latest_tombstone(conn)` (the
  highest-rowid tombstone's boundary key + stamped revision, or `None`) with
  their row types `CapabilityEventRow` (rowid + all columns as `Option` per
  the nullable-by-DDL schema) and `TombstoneRow`; holds a private
  tombstone-verdict literal mirroring `capability_event.rs`
- `src/writer.rs` -- `UsageWriter`: opens the DB once at boot (degrades to a
  no-op drain loop on open failure), drains the bounded channel on a dedicated
  thread via `blocking_recv`, bounded-deadline drain + join on `shutdown`. The
  channel carries a `WriterMessage` enum (`Request(Box<UsageRecord>)` -> the
  `requests` table, `LearnEvent(CapabilityLearnEvent)` -> the
  `capability_learn_events` table, `CapabilityEvent(CapabilityEvent)` -> the
  unified `capability_events` ledger; the record is boxed so the variants stay
  close in size), so one actor + one connection serves every row kind with the
  message variant selecting the destination table. The one-shot startup prune
  runs over both `requests` (`retention::prune`) and `capability_events`
  (`retention::prune_capability_events`, tombstone-exempt)
- `src/db.rs` -- `UsageDb` wrapper + `open`: connection setup (WAL, foreign
  keys), the `INSERT OR IGNORE` write keyed on the UNIQUE `request_id`
  (idempotency), schema-presence assertions.
  `open_readonly`/`open_readonly_fastfail` open a non-creating, non-migrating
  READ-ONLY view (WAL confirmed by reading the pragma, schema-version match
  enforced -> `NoData`/`VersionTooOld`/`VersionTooNew`/`NotWal`). `open_rw` is
  the READ-WRITE non-migrating sibling (a thin variant of
  `open_readonly_with_timeout` reusing
  `verify_readable_version`/`ensure_requests_table`): `SQLITE_OPEN_READ_WRITE`
  without `CREATE`, so a missing file is still `NoData` and a mismatched
  schema still errors -- the connection may `INSERT` but never creates or
  migrates, letting the one-shot CLI capability probe attach to the daemon's
  existing WAL database instead of forking a second unmigrated one
- `src/schema.rs` -- `requests` table DDL (the capture columns + `request_id
  UNIQUE` idempotency key + `outcome` CHECK), `meta` table, `SCHEMA_VERSION`;
  v2 added the nullable `strategy TEXT` column (the per-request auto-cache
  decision token) via the migrate-on-open ladder (`ALTER TABLE requests ADD
  COLUMN strategy`, same end shape whether created fresh at v2 or migrated
  from v1) -- WRITE-STOPPED as of 0.9.x: the column is retained in the DDL
  but the writer no longer binds it, so rows written by this version onward
  read NULL; v3 added the nullable `reduction_strategy TEXT` column (the
  per-request context-reduction decision token) the same way (`ALTER TABLE
  requests ADD COLUMN reduction_strategy`) -- also WRITE-STOPPED as of
  0.9.x; v4 added the nullable
  `selection_decision TEXT` column (the per-request seat-selection decision
  token: birth_pick / sticky_stay / overflow_repin / defer_no_healthy /
  keyless_fill_first) the same way (`ALTER TABLE requests ADD COLUMN
  selection_decision`) -- also WRITE-STOPPED as of 0.9.x (none of the three
  tokens is persisted any more; each is only partially visible in the trace
  logs -- cache via `cache_auto_decision`, reduction only when bytes were
  stripped, selection only on sticky birth/repin); v5 added the
  steady-state would-trim
  advisory pair
  `would_trim_tokens` (the candidate freed-token count `d`) +
  `would_trim_break_even_k` (the break-even reuse count K*) the same way
  (`ALTER TABLE requests ADD COLUMN ...`); v6 added the nullable
  `would_trim_k_floor REAL` (the per-session K estimator's lower confidence
  bound, stamped only for a `Calibrated` estimate) the same way; v7 added
  `would_trim_shadow_misfire INTEGER` (the shadow-misfire monitor: 0 = Stable,
  1 = Misfire, NULL = FirstSeen or no session key) the same way; v8 plumbed
  the near-lossless attribution set -- `would_trim_dedup_tokens` /
  `would_trim_supersession_tokens` (per-heuristic freed-token counts), the
  count-pair `would_trim_path_units` / `would_trim_path_extractable` (kept
  unaveraged so the extractability rate reconstructs offline via SUM/SUM),
  `would_trim_recorder_version` (NULL on pre-recorder rows, stamped by the
  near-lossless recorder), the capped JSON `would_trim_raw_marks`, and
  `would_trim_context_fraction` (NULL when the pricing row's context window is
  unknown) -- columns only, the recorder pass computes their values (same
  `ALTER TABLE ... ADD COLUMN` discipline). v9 adds the separate append-only
  `capability_learn_events` table (`CREATE_CAPABILITY_LEARN_EVENTS_TABLE`:
  `ts`, `state_key`, `feature_key` (NORMALIZED), `provider_kind`,
  `signal_tier` (CHECK-constrained to `self-identifying` / `inferred`),
  `observations`, `upstream_status`, `remapped`, `request_features` JSON-array
  TEXT) -- deliberately NOT a `requests` row (every reporting query treats
  `requests` rows as requests), the forever-contract landing pad for the
  warm-rebuild replayer; carries no body / message / prompt column (log
  hygiene). v10 (`SCHEMA_VERSION = 10`) is a forward-only, idempotent keyspace
  invalidation: the v9 -> v10 step truncates `capability_learn_events`
  (`DELETE FROM`) because the openai-compat learned-capability keyspace
  changed from the `error.code` token to the canonical `/error/param`
  capability, so any pre-change row would replay under a keyspace that no
  longer exists (safe: nothing reads the table yet); v11 (`SCHEMA_VERSION =
  11`) renames the learn-events `feature_key` column to `capability_key`
  (`migrate_v10_to_v11` in migrate.rs: guarded `ALTER TABLE ... RENAME
  COLUMN`, idempotent + fresh-DB-safe; same normalized-token contract); v12
  (`SCHEMA_VERSION = 12`) adds the nullable `resolved_class TEXT` column to
  `requests` (the canonical kebab failure-class token stamped by the CLI
  capture for a dispatch-reached failure; NULL for a success, a pre-dispatch /
  validation / local-gate row, and an `Unknown` class -- no backfill) via
  `migrate_v11_to_v12` (guarded `ALTER TABLE requests ADD COLUMN
  resolved_class`, one transaction with the version bump, same append-last
  shape whether created fresh at v12 or migrated from v11). v13
  (`SCHEMA_VERSION = 13`) adds the separate append-only `capability_events`
  table (`CREATE_CAPABILITY_EVENTS_TABLE` +
  `CREATE_CAPABILITY_EVENTS_TS_INDEX` on `ts`: `id INTEGER PRIMARY KEY` (an
  explicit rowid alias SQLite preserves across VACUUM -- the insertion-order
  boundary key the tombstone / replay-after / prune address via its rowid
  alias), `ts` NOT NULL epoch-ms, nullable `lane_key` / `capability`
  NORMALIZED keys (empty on a tombstone row), open-set `verdict` / `phase` /
  `source` / `tier` tokens (tier persisted so live-vs-rebuild equivalence
  distinguishes self-identifying from inferred), nullable `evidence_class`
  (pinned observation tokens), nullable `upstream_token` (forensic / display
  only -- never consulted by admission or replay), and `catalog_version` /
  `overlay_revision` boundary-revision stamps) via `migrate_v12_to_v13`
  (mirrors the v8 -> v9 whole-new-table step: `CREATE TABLE / INDEX IF NOT
  EXISTS` in one transaction with the version bump, fresh-create and
  migrated-open both covered) -- the unified forever-contract ledger the
  warm-rebuild replayer reads on boot; carries no body / message / prompt
  column (log hygiene)
- `src/migrate.rs` -- forward-only schema migration / version stamping against
  the `meta` table
- `src/retention.rs` -- `prune` (startup-only, best-effort) dropping
  `requests` rows older than the configured retention window, and
  `prune_capability_events` (same shape for the `capability_events` ledger)
  which age-prunes only rows before the latest tombstone's rowid so the
  tombstone boundary and every row after it survive regardless of age (no
  tombstone -> ordinary age prune); `PruneOutcome` counters

## routectl-cli

- `src/main.rs` -- clap CLI entry point; `main` wraps `run` (which dispatches
  `serve` / `init` / `login` / `logout` / `refresh` / `whoami` / `doctor` /
  `probe` (top-level `probe --capabilities`, distinct from `provider probe`) /
  `test` / `config` / `provider` / `usage` subcommands) and on an error routes
  the message through
  `commands::parse_error_redaction::redact_config_load_error` (fail-safe strip
  of config-load path/value leaks; non-config errors pass through unchanged)
  then prints it via `Display` -- preserving the multi-line actionable
  config-load message rather than `Debug`-escaping it -- before exiting 1
- `src/lib.rs` -- test-scaffolding facade (not a stable library API):
  re-exposes `commands`, `handlers` (`#[doc(hidden)]`), `ingress`, `proxy`
  (`#[doc(hidden)]`), and `server` to the crate's own integration-test
  binaries; `config_classify` is crate-internal (`pub(crate)`, consumed only
  via `crate::config_classify::`)

### server

- `src/server/mod.rs` -- server module hub: the shared `AppState` every axum
  handler reads (`Router` behind `ArcSwap` for lockless hot-swap, plus the
  sibling usage handle, activation inventory, and MITM seam nonce), the
  `check_bind_safety` loopback guard, the per-concern submodule declarations,
  and the `server::` re-exports callers use. Unit tests are paired
  per-concern: the `#[path]`-included hub sidecar `tests.rs` keeps only the
  bind-safety tests, each submodule owns its own `<name>_tests.rs` sidecar,
  and the shared `#[cfg(test)]` fixture helper lives in `test_support.rs`
- `src/server/serve.rs` -- listener bind + serve loop: `serve` (bind then
  serve) / `serve_on_listener` / `serve_on_listener_with_overlay` boot path,
  `build_axum_router` route wiring (every registered path is classified by
  the `PUBLIC_ROUTES` / `AUTH_GATED_ROUTES` test-only inventory consts, which
  `serve_tests.rs` enforces against a scan of the crate's registered path
  literals plus real 401-vs-200 probes, so an unclassified route fails a test
  instead of shipping unauthenticated), the graceful bounded-drain shutdown
  (`serve_with_bounded_drain` + `drain_deadline_watcher` + `DRAIN_DEADLINE`),
  MITM front-proxy spawn (`start_mitm_proxy`), and the usage-writer lifecycle
  (`build_usage_writer` / `drain_usage_writer`). On the owned router before
  the `ArcSwap`, boot runs the K-store warm (`k_rebuild`) then the
  learned-capability warm (`capability_rebuild`); `build_usage_writer` is
  sequenced AHEAD of the capability warm so the fail-closed boot tombstone has
  a `UsageHandle` to enqueue through. Router construction is delegated to
  `router_build`. Boot seeds the initial activation inventory and spawns the
  reload pipeline (both owned by the `reload` submodule); a forwarded
  (pure-proxy) egress is an explicit `[providers.X] credential_source =
  "forwarded"` block -- no zero-config synthetic-egress injection
- `src/server/capability_rebuild.rs` -- serve-side reaction to the shared
  capability-ledger read (the reader, clock map, and boundary classification
  live in `ledger_reader.rs`). `warm_capability_registry_from_ledger` (called
  from `serve` on the owned router before the `ArcSwap`, bootstrap-only --
  never on hot-reload) classifies the boundary via
  `ledger_reader::classify_boundary` and either replays the post-boundary
  slice through `Router::rebuild_learned_from_ledger` (matching-revision
  tombstone) or fails closed: `log_fail_closed` logs the case at its warranted
  level (debug for a cold ledger / absent tombstone, info for a revision
  mismatch, WARN only for a genuinely unreadable ledger) and
  `enqueue_fresh_tombstone` writes exactly one fresh tombstone stamped this
  boot's revision via `try_send_capability_event`. `emit_rebuild_log` reports
  the per-verdict tally with WARN-on-`REBUILD_ROW_LIMIT`-truncate. Boot never
  fails. Tests in the `#[path]`-included `capability_rebuild_tests.rs`
- `src/server/ledger_reader.rs` -- shared read-only bridge from the usage
  capability-event ledger to the `routectl-router` replay seam, constructed by
  BOTH the serve warm (`capability_rebuild.rs`) and the offline doctor gather
  (`commands/doctor/gather.rs`), so the two surfaces cannot drift on the clock
  map, row cap, or lane-key contract. Owns the concrete
  `LedgerCapabilityReader` (`CapabilityLedgerReader` impl: per-call read-only
  open, `now`/`now_ms` clock anchors captured once so every mapped row shares
  one basis, `loaded_rows` count), the wall-clock-to-`Instant` `map_instant`
  (age clamped to zero so a future-dated row lands at `now`, `checked_sub`
  clamp so an ancient row never underflows), the `REBUILD_ROW_LIMIT` = 5000
  read cap, and `epoch_ms_now` (shared with the hot-reload tombstone seam).
  `classify_boundary(db_path, catalog_version, overlay_revision) ->
  BoundaryOutcome` resolves the tombstone boundary read-only and PURELY -- no
  logging, no writes -- into `Replay | Cold | NoTombstone | RevisionMismatch |
  Unreadable(class)` so each caller applies its own reaction.
  `open_error_class` maps a usage-DB `OpenError` to a fixed path-free class
  token (a new variant is a compile error; reused by `doctor_panels.rs`).
  Lane-key contract: the persisted `lane_key` IS the registry `state_key` and
  the persisted `capability` is already normalized, so `provider_kind` is
  inert on replay. Tests in the `#[path]`-included `ledger_reader_tests.rs`
- `src/server/router_build.rs` -- Router construction from a parsed config +
  catalog overlay: `build_router_from_config_with_overlay` (the shared build
  path reused by boot and every hot-reload -- runs the full startup validation
  gauntlet then installs resolved models + overlay revision) and the
  `#[cfg(test)]` empty-overlay wrapper `build_router_from_config`. Split out
  of `serve` so the `reload` coordinator depends on this builder directly,
  keeping the serve <-> reload edge one-way; both re-exported at `server::`
  paths from `mod.rs`
- `src/server/reload.rs` -- config/credential/seat reload + activation
  coordinator, owning the `spawn_blocking` hot-reload boundary.
  `spawn_reload_pipeline` wires the file-watch task + SIGHUP listener
  (`run_sighup_listener`, cfg(unix)) + `run_reload_coordinator`, which drains
  a `ReloadRequest` channel and fans each into `handle_config_reload` (re-read
  config.toml + overlay via `read_parse_validate_config` off a
  `spawn_blocking` worker, rebuild the live `Router` behind `ArcSwap`, carry
  over per-nickname runtime state, flip the usage capture gate, enqueue one
  capability replay-boundary tombstone stamped the new revision when the
  reload advanced the catalog version or overlay revision (the hot-reload
  counterpart to the boot seam in `capability_rebuild.rs`, best-effort via
  `try_send_capability_event`), and emit the restart-required diff via
  `config_classify::collect_restart_required_changes` --
  bind/listener-auth/body-limit, the three `[log]` knobs, `usage.db_path`, and
  `[mitm]` all restart-required since their listeners/state are startup-only)
  or `handle_credentials_reload` -> `rebuild_router_for_seat_change`
  (seat-set-gated rebuild). `apply_activation` (+ `gather_probes` /
  `emit_activation_delta`, `ActivationTrigger`) recomputes the auto-activation
  inventory at startup and after each reload; `await_reload_tasks` bounds
  graceful-shutdown joins. Unit tests live in the `#[path]`-included sibling
  sidecars `activation_tests.rs` (activation-recompute + audit-event tests,
  run on the current-thread runtime for the capture subscriber) and
  `reload_tests.rs` (reload-pipeline + coordinator tests)
- `src/server/config_load.rs` -- effective-config load/parse/validate,
  re-exported at `server::` paths from `mod.rs`. The shared config loader
  splits into `parse_config_only` (version + legacy-mitm preflight + typed
  parse, NO overlay) and `load_overlay_default` (overlay only);
  `load_effective_config_unvalidated` composes both, `load_effective_config`
  adds the fail-fast `validate_effective_config` gate, while `doctor` calls
  each independently so its capability panel degrades the two layers
  separately. `warn_deprecated_capability_lists` emits the one-shot legacy-key
  deprecation WARN via the shared `commands::capability_legacy` helper;
  `read_parse_validate_config` is the synchronous hot-reload loader;
  `warn_if_config_world_readable` (unix) WARNs on group/world-readable configs
  carrying secrets; `compute_max_body_bytes` maps the zero-means-default
  body-limit knob
- `src/server/auth.rs` -- listener middleware enforcing `[server.auth].tokens`
  via constant-time comparison
- `src/server/file_watch.rs` -- `notify-debouncer-full` fs-watch task; watches
  the parent dirs of `config.toml` / `credentials.json`, basename-routes
  events back to a `ReloadRequest::{Config,Credentials}` channel; debounce
  coalesces tempfile + rename bursts
- `src/server/request_id.rs` -- request-id middleware (`x-request-id` echo +
  `tracing` span field with allowlist sanitization)
- `src/server/status_gate.rs` -- status-subtree-ONLY middleware (`/v1/*`
  carries none of it). `StatusHostAllowlist` + `host_guard`
  (anti-DNS-rebinding: rejects a `Host` outside {loopback literals
  with/without port, the bound `host:port`} with a fixed 403 `forbidden_host`;
  a missing `Host` is permitted). Under a wildcard bind (`0.0.0.0` / `::`) no
  client can name the unspecified address literally, so the guard degrades to
  a PORT-only check -- a `Host` is allowed iff its parsed port equals the
  bound port (a portless Host fails closed); the token auth layer now sits
  ABOVE the guard and carries the real access decision on such binds.
  `apply_overload_layers` wraps a status router with
  `HandleErrorLayer(LoadShedLayer(GlobalConcurrencyLimitLayer(STATUS_MAX_INFLIGHT=4)))`
  -- a subtree-wide (`Global`, one shared semaphore across all status routes,
  not per-route) hardcoded concurrency cap that sheds excess IMMEDIATELY as
  the fixed JSON 503 `{"schema_version":1,"error":{"code":"overloaded",...}}`
  (never queues); `handle_status_overload` maps the shed error. Also owns
  `QUERY_BUDGET_MS = 1000`, the per-request wall-clock budget for one
  `/status/query` grouped aggregate (an overrun sheds as the `query_timeout`
  unavailable panel). Neither const is a config knob (no `config_classify`
  section)
- `src/server/secrets.rs` -- `CompositeStore` `SecretStore` dispatching
  `oauth://<provider>` to `OAuthStore` and `env://` / `file://` / `literal:`
  to `MemoryStore`; degrades gracefully when no `HOME` / `XDG_CONFIG_HOME`

### handlers

- `src/handlers/mod.rs` -- groups per-route HTTP handlers
- `src/handlers/health.rs` -- `GET /health` returning version + status
- `src/handlers/models.rs` -- `GET /v1/models` listing aliases + `[models]`
  keys (skips `default`, skips `selectable=false`); on the forwarded
  (pure-proxy) lane (`forwarded_proxy_target`:
  `Router::has_forwarded_provider` AND the request arrived via the MITM
  reinject leg carrying a captured client bearer AND the forwarded provider's
  `base_url` is pinned to `api.anthropic.com`) the handler proxies through to
  Anthropic's real `/v1/models` list via `crate::proxy::forward` and returns
  it verbatim; every other case, including any proxy failure, fails soft to
  the local list above
- `src/handlers/chat_completions.rs` -- `POST /v1/chat/completions` thin
  wrapper around `ingress_handle` with `OpenAiIngress`
- `src/handlers/messages.rs` -- `POST /v1/messages` thin wrapper around
  `ingress_handle` with `AnthropicIngress`
- `src/handlers/messages_count_tokens.rs` -- `POST /v1/messages/count_tokens`
  proxy through the FIRST provider in the dispatch chain only (no fallback
  walk; tokenizer-specific count must match the chosen model)
- `src/handlers/responses.rs` -- `POST /v1/responses` thin wrapper around
  `ingress_handle` with `ResponsesIngress`
- `src/handlers/ingress_handle.rs` -- generic ingress driver: parse + route +
  render; SSE streaming with cancellation. Constructs the `UsageCapture` guard
  (now defined in `usage_capture.rs`) at the boundary and drives its
  `observe_*` / `finalize` calls across the non-stream (`complete_response`)
  and stream paths. Streaming uses the inverted grace-gated commit (option
  (b')): `stream_response` holds the dispatch as an un-awaited `'static`
  `DispatchFut` and hands it to `stream_dispatch_gated`, which
  `tokio::select!`s it (biased) against a `STREAM_EARLY_FLUSH_GRACE` (2500ms)
  flush-timing backstop. FAST branch (resolves within grace): `Ok` spawns
  `render_stream_task` on the resolved stream; `Err` returns a REAL HTTP
  status via `map_error` (preserving the SDK pre-stream 529 retry).
  GRACE-EXPIRY branch: commits the SSE `Response` (`build_sse_response`) and
  spawns `warm_render_task`, which flushes the dialect `early_frame` as the
  first body byte BEFORE awaiting the still-pending dispatch
  (emit-then-dispatch invariant), then on `Ok` drives the shared
  `drive_stream` loop (dedups the already-emitted `message_start`;
  `drive_stream` `tokio::select!`s each upstream poll (biased) against
  `tx.closed()`, so a client disconnect cancels the upstream immediately on
  channel close rather than waiting for a send to fail) or on `Err` emits ONE
  terminal in-stream `render_error_eos` + finalizes `UpstreamError` with the
  `pre_content_dispatch` stage marker (`observe_meta` relocated into the task;
  Drop `client_disconnect` reserved for genuine cancellation). Also owns the
  forwarded-mode (pure-proxy) ingress path: `enforce_pure_proxy_admission`
  (the shared admission gate all three dialect handlers funnel through, backed
  by the pure `classify_pure_proxy_rejection` decision core over
  `PureProxyAdmissionInputs`) runs BEFORE body parse and rejects a malformed
  forwarded request via `pure_proxy_metrics::record_rejection`;
  `capture_forwarded_bearer` / `capture_stainless_headers` populate
  `req.routectl_internal.forwarded_bearer` / `.stainless_headers` under the
  shared two-key `forwarded_capture_armed` gate (the process's `MitmSeamNonce`
  value matches the inbound seam header AND
  `Router::has_forwarded_provider()`; a spoofed seam header with a
  non-matching nonce is treated as seam-absent). `map_error` -> `error_response`
  also echoes an `Error::Upstream.retry_after` reset hint onto a client-facing
  `Retry-After` header (RFC 7231 integer seconds, sub-second hints rounded UP to
  at least 1s) so a client SDK's own 429/503 backoff keeps the upstream hint
- `src/handlers/pure_proxy_metrics.rs` -- forwarded-mode (pure-proxy) ingress
  admission-rejection counter + structured rejection log.
  `PureProxyRejectionReason` (closed 2-variant enum: `TokenMissing` /
  `IdentityMissing`, each mapping to an HTTP status via `status()`) is the
  sole dimension of the lock-free `PureProxyRejections` counter (`by_reason:
  [AtomicU64; 2]`, one process-global `static LazyLock` instance since ingress
  admission rejects before any listener-scoped metrics carrier exists);
  `record_rejection(reason, has_client_session_id)` bumps the counter and
  emits one WARN carrying only safe dimensions (reason, status, a fixed
  `credential_source = "forwarded"` token, and the session-id-presence
  boolean) -- never the forwarded token itself
- `src/handlers/usage_capture.rs` -- `UsageCapture`, the unified RAII capture
  guard (replaces the former `EgressStreamSummary`) that records exactly ONE
  `UsageRecord` per request on both ingress paths: a draft is seeded from the
  request shape + `RequestId` (`build_usage_draft`), the
  dispatch/token/quota/outcome columns are stamped from `DispatchMeta` +
  `ChatResponse`/`ChatChunk` + `UpstreamMeta`, `finalize(outcome)` emits the
  row once (idempotent), and the Drop fallback stamps `client_disconnect` for
  a cancelled/disconnected request. Also subsumes the egress stream
  trace-summary line. Owns the outcome-mapping helpers
  (`outcome_for_dispatch_err`, `error_class_of`) and the auto-cache outcome
  signal (`is_cache_thrash` + `emit_cache_outcome`: `cache_auto_outcome` at
  DEBUG when an auto-emitted breakpoint is created-and-read, WARN when
  created-without-read this request). Also owns `StreamStage` +
  `mark_stream_stage`, which stamps `extra.stream_stage`
  (`pre_content_dispatch` vs `mid_stream`) so the ledger keeps a warm-hold
  pre-content dispatch failure distinct from a mid-stream cut (both are
  `UpstreamError`). Also owns the `http_status` transport-status contract (the
  status the CLIENT received): `observe_response` stamps a fixed 200,
  `mark_stream_http_committed` stamps 200 at the first client-visible SSE byte
  (idempotent -- writes only while `http_status` is unset), and
  `observe_error` records an upstream status ONLY while `http_status` is still
  unset (pre-head) and never for the status-0 local sentinel, so a mid-stream
  provider failure after the head committed keeps the client-seen 200 with the
  fault carried by `outcome`/`error_class`/`stream_stage` instead; pre-fix
  streaming rows are NULL. `observe_error` also stamps the ledger
  `resolved_class` column, GATED on the request having reached a dispatch
  attempt (only when `provider_kind` was already set by `observe_meta`): it
  classifies via `routectl_core::failure_class::classify` +
  `FailureClass::class_token` (a pre-dispatch / validation / local-gate row,
  or an `Unknown` class with no token, persists NULL -> "unclassified"). Also
  owns `drain_capability_events`, which drains `DispatchMeta`'s captured
  capability signals into the unified `capability_events` ledger (one
  `try_send_capability_event` per event, stamped with the `(catalog_version,
  overlay_revision)` `observe_meta` reads off the `Router` getters at the
  ingress boundary): `learned_capabilities` -> `broken` rows (phase / tier
  from the event, no evidence class), `capability_observations` -> `verified`
  / `suspect` rows (phase `f3`, the pinned evidence-class token),
  `cleared_capabilities` -> `cleared` rows (`live` source). Empty on the
  common non-capability path, best-effort like every usage write. The legacy
  `capability_learn_events` write path is RETIRED here (the request path no
  longer calls `try_send_learn_event`; the table / variant /
  `try_send_learn_event` remain, deprecation-doc-commented, no DROP)
- `src/handlers/status/mod.rs` -- read-only `/status` family. `StatusState`
  carries ONLY read handles (a `StatusRouterHandle` read-only facade over the
  router `ArcSwap` -- see `router_view.rs` -- plus the `activation` `ArcSwap`
  + resolved `usage_db_path`/`config_path`, plus a `DaemonMetaHandle` read
  facade -- see `daemon_meta.rs`) via `from_app`, so a status
  handler is structurally incapable of mutation, of reaching a raw
  `Router`/`.config`/dispatch, or of touching the forwarding seam;
  `PanelObservability`/`PanelCounters` are the per-panel last-availability +
  shed-count scaffold (minimal `record` hook now, read side wired later), with
  `/status/query`'s two request modes on SEPARATE detectors (`status_query` /
  `status_query_series`) so a healthy aggregate poll cannot mask a consistently
  failing series poll.
  `status_router() -> Router<Arc<StatusState>>` registers GET-only panel routes
  (`/status`, `/status/{usage,health,config,doctor}`; non-GET gets 405) plus the
  ONE method carve-out `/status/query`, registered with `any()` because axum's
  `MethodFilter` cannot express `QUERY` -- the carve-out is route-scoped, so
  `QUERY` against any sibling path is still a 405.
  `guard_panel` runs every panel builder on `spawn_blocking` + `catch_unwind`,
  mapping a panic OR a join failure to an unavailable `Panel<T>` (never a
  500/crash). The `/status` aggregate composes the four REAL panel builders
  CONCURRENTLY (`tokio::join!`, so one slow panel never stalls the others)
  into `{panels:{usage,health,config,doctor}}` -- each an INDEPENDENT
  per-panel envelope with its OWN `schema_version` (usage = 3, health = 5,
  config = 2, doctor = 4)/`as_of`/availability, with NO outer envelope version
  (push-ready: a future push event is the same per-panel shape keyed by panel
  name). Reuses the same builders the per-panel endpoints call, so one panel's
  source failure OR panic renders only THAT panel unavailable and leaves
  siblings intact. Merged into the serve process by
  `server::build_axum_router`, behind the `server::status_gate` subtree-only
  middleware (`/v1/*` inherits none of it) and, whenever
  `status_requires_auth` holds (tokens configured OR a non-loopback bind),
  beneath the same listener auth layer as `/v1/*` (the auth layer sits UNDER
  the host guard); token-less loopback keeps the zero-auth dev path
- `src/handlers/status/types.rs` -- `Panel<T>` envelope (snake_case
  `serde::Serialize`: `schema_version`/`as_of`/`data`/`unavailable`) with
  constructors enforcing available => `unavailable: None` and unavailable =>
  `data`/`as_of` `None`; `now_utc_rfc3339` (RFC3339-UTC `as_of` helper) and
  `utc_rfc3339` (same format over a caller-pinned instant, for a panel deriving
  `as_of` and a relative age from ONE clock read);
  `vocabulary` const module -- the fixed snake_case tokens the wire DTOs reuse
  from the event surface (`state_key`/`capability_key`/`signal_tier`,
  provenance tokens `provider`/`model`/`override`/`learned`, signal-tier
  tokens) plus the `unavailable` reason codes
  (`no_data`/`schema_mismatch`/`db_busy`/`db_unavailable`/`config_unavailable`/`doctor_unavailable`/`no_config_path`/`query_timeout`)
- `src/handlers/status/query.rs` -- `QUERY /status/query` (schema_version 1),
  the grouped windowed aggregate. Three EXCLUSIVE regimes: 405 (method is not
  `QUERY`, guarded by the handler's first extractor), 400 (body outside the
  closed vocabulary -- `deny_unknown_fields` + closed `window`
  {today,week,month,all} / `group_by` {model,provider,alias} / `bucket`
  {hour,day} enums + optional
  alias/provider filters -- or over `MAX_BODY_BYTES` (8 KiB), or still arriving
  at the `BODY_READ_TIMEOUT` (2s) read deadline that stops a stalled send from
  parking a concurrency permit; all refused with the FIXED envelope
  `{"schema_version":1,"error":{"code":"invalid_query",...}}` that never echoes
  serde text or body bytes and never opens the ledger), and 200 for everything
  else including every data-source failure. Runs `routectl_usage::query` under
  the same isolation as the panels (`guard_panel` blocking worker +
  `catch_unwind`, per-request `open_readonly_fastfail`), priced through the
  facade's `QueryPricer` (ONE pinned config snapshot per request) and bounded by
  `QUERY_BUDGET_MS`; a fired deadline sheds `query_timeout`, open/query failures
  reuse the usage panel's shed-code mapping. Serializes the crate's
  `QueryResult` EXPLICITLY (`render`, not the `Json` responder, whose own
  failure arm would be a bare 500 outside the three regimes -- a render failure
  degrades to the unavailable panel instead), so the metric field names ARE the
  wire vocabulary (contracts sec 15), with absent `Option` metrics as explicit
  `null`. A PRESENT `bucket` selects the series mode: the grid is resolved on the
  blocking worker via `commands::usage::resolve_bucket` (reading
  `earliest_ts_start` first, and ONLY for `window: all`, whose epoch lower bound
  is re-anchored to the oldest in-window row's local midnight, clamped never to
  fall below that bound -- an identical row set, so groups/totals match the
  non-series path), then handed to the leaf as a plain
  `BucketSpec`; an all-time window over an empty ledger has no anchor and answers
  with an EMPTY series at the requested unit's width rather than an error. All
  local-calendar math stays here, keeping `routectl-usage` chrono-free.
  `spec_from_body` is `pub(super)` so the dashboard's drift test can validate
  the request shapes the page declares against THIS parser instead of a second
  copy of the vocabulary
- `src/handlers/status/router_view.rs` -- the read-only router facade that
  STRUCTURALLY enforces the `/status` read-only seam. `StatusRouterHandle`
  wraps the router `Arc<ArcSwap<Router>>` with a PRIVATE inner field; `view()`
  loads a snapshot into `StatusRouterView` (also private inner `Arc<Router>`)
  which exposes ONLY three read methods -- `route_targets(now)`,
  `learned_capabilities()`, and `effective_view(&overlay)` (runs
  `derive_effective_view` against the live config INTERNALLY, so panels never
  touch raw `Config`), plus `pricer()` -> `QueryPricer`, an OWNED `'static`
  pricing facade over one pinned snapshot whose only method costs an `AggRow`
  through `commands::usage::cost_for_row` (so `/status/query` and the CLI usage
  report price a row through one function, and a hot-swap mid-query cannot make
  two rows of one result price against different rate tables). Rust module
  privacy makes the raw `Router` unreachable
  from the sibling panel modules -- a panel cannot obtain a `&Router`, call
  `.complete`/`.stream`/dispatch, or read `.config`; the `mod.rs`
  forbidden-token scan also covers this file with a facade-specific rule
- `src/handlers/status/daemon_meta.rs` -- read-only facade over the
  process-level daemon facts the config panel's source strip needs.
  `DaemonMeta` (built once at bind, `stamp_config_loaded()` called at
  bootstrap and by the reload coordinator after every successful config /
  overlay reload) holds the bound `listen_addr` plus an atomic last-load
  epoch-ms; `DaemonMetaHandle` keeps the `Arc` PRIVATE and exposes only
  `snapshot(now_ms) -> DaemonMetaSnapshot{listen_addr, version,
  config_loaded_age_ms}`, so a panel can read the facts but never reach the
  stamp writer. An unstamped load reports `None` (never an epoch sentinel)
  and a backwards clock step clamps the age to zero (never negative)
- `src/handlers/status/usage.rs` --
  `/status/usage?window=today|week|month|all` (default today). Opens the
  ledger read-only PER REQUEST via `open_readonly_fastfail` inside
  `guard_panel`'s blocking worker, runs
  `aggregate`/`errors_by_class`/`latest_quota_by_seat`/`would_trim_summary` over the
  `window_bounds` window, and maps to the aggregates-only `UsagePanel` DTO
  (schema_version 3: per-`(alias,provider,model,upstream)` rollups + windowed
  totals + one per-seat quota snapshot each (`seat` + `provider_kind`, per-row
  `ts_start_ms` freshness) + would-trim summary; per-group and totals
  `errors_by_class` failure-class breakdown, `client_disconnect_total` +
  reporting-only `cache_read_present` denominator; NEVER request
  rows/ids/bodies/prompts). Ledger-open/query failures classify to the shed
  codes `no_data`/`schema_mismatch`/`db_busy`/`db_unavailable` (busy/lock read
  from the `rusqlite` error code)
- `src/handlers/status/health.rs` -- `/status/health` (schema_version 5).
  Snapshots the router through the read-only facade ONCE (`router.view()`) and
  reads `route_targets` + `learned_capabilities` from that single view,
  mapping to `HealthPanel` (per-target
  `state_key`/`nickname`/`provider_name`/`upstream`/`seat_label`/`circuit`/`rpm_available`/`half_open_probe_in_flight`/`open_since_ms`/`last_outcome`/`last_outcome_at_ms`
  + learned rows
  `state_key`/`capability_key`/`verdict`/`signal_tier`/`observations`/`phase`/`source`/`last_seen_ms`
  (epoch-ms of the last observation from the single pinned snapshot clock,
  future-dated clamps to a zero age), where the `verdict` token -- `verified`
  vs `broken` from the core `Verdict` -- distinguishes a VerifiedWorking
  positive from a learned negative that merely carries `phase=f3`).
  `CircuitPhase` and `LastOutcome` -> snake_case strings are owned here (not
  `Serialize` derives on the router enums);
  `open_since_ms`/`last_outcome_at_ms` are epoch-ms converted from the gate's
  monotonic elapsed ages (`None` == closed/never-seen, never a 0/epoch
  sentinel); `last_outcome` renders the derived `circuit_open` when the phase
  is open, else the stored outcome token; `feature_key` is renamed to the
  contract token `capability_key`. No dial, no mutation
- `src/handlers/status/config.rs` -- `/status/config`. Renders the
  provenance-annotated EFFECTIVE (live, in-effect) config view: snapshots the
  router through the read-only facade, loads a fresh catalog overlay PER
  REQUEST via `server::load_overlay_default()` inside `guard_panel`'s blocking
  worker (no overlay retained on the router), and folds them into
  `view.effective_view(&overlay) -> EffectiveView`. Maps to a purpose-built
  `ConfigPanel` DTO (model cells with `source` token
  baked/import/user/disabled/missing + economics; class-policy cells with
  kebab class + `config`/`baked-default` source + `debits_breaker` read from
  the router's public `class_debits` so the transient-health set is never
  restated on the wire; capability cells with
  `route-away`/`force-supported` verdict + `provider`/`model`/`override`
  provenance reusing the shared vocabulary; `aliases` carrying each alias's
  ORDERED fallback chain; a `source` strip with
  `config_path`/`loaded_age_ms`/`alias_count`/`provider_count`/`listen_addr`/`version`,
  counts from the same effective view and daemon facts from the
  `daemon_meta` facade) plus the activation inventory
  (`activation.load_full()` once, each `ActivationEntry` mapped to
  provider_id/provider_kind/status/reason/referenced_by_aliases). Raw `Config`
  is NEVER imported or serialized (the forbidden-import seam test enforces
  it). An overlay load/parse failure is routed through the shared
  `redact_config_load_error`, logged, and collapsed to a `config_unavailable`
  code -- no path/value reaches the wire
- `src/handlers/status/doctor.rs` -- `/status/doctor`, strictly NO network.
  Runs only the no-network doctor sections via
  `commands::doctor::gather_context_no_network(config_path)` +
  `build_report_no_network(&ctx)` (never
  `gather_probe_results`/`section_probe`); the async disk-I/O gather runs
  under `guard_panel`'s `spawn_blocking` via `Handle::current().block_on`.
  Embeds the resulting `DoctorReport` (schema_version 4, reused verbatim) and
  a reachability summary DERIVED from one live `route_targets(...)` read of
  each target's last settled outcome (`ok` -> `reachable`, none-yet ->
  `unknown`, any failure family / gate refusal -> `degraded`) -- never a
  re-dial. Config-load errors are already redacted inside the gather (no
  second copy). `config_path: None` -> `no_config_path` unavailable; a gather
  failure -> `doctor_unavailable`
- `src/handlers/status/page.rs` -- the embedded dashboard page. ASSEMBLES the
  document at COMPILE time from three sources (`include_str!` x3 --
  `dashboard.html` markup + `dashboard.css` + `dashboard.js`) by splicing the
  style and script bodies into the markup's two `@@DASHBOARD_*@@` slots as one
  inline `<style>` + one inline `<script>`; the join is `const fn` work over
  byte arrays, so the served `PAGE: &str` is still static bytes and the split
  is authoring-only (the house `include_str!` pattern -- no
  `ServeDir`/`rust-embed`). A missing or duplicated slot is a compile-time
  panic. `page_router() -> Router<()>` serves the single self-contained
  document at `GET /` with a `Cache-Control: no-store` header, stateless and
  static-bytes-only (structurally read-only; covered by the `mod.rs`
  forbidden-import scan + its own GET-only 405 assertion). Merged into the
  serve process under the SAME `status_gate::host_guard` and the SAME
  conditional listener auth gate (applied whenever `status_requires_auth`
  holds -- tokens configured OR a non-loopback bind) as the JSON, but
  deliberately OFF the JSON `apply_overload_layers` shed budget: a zero-I/O
  `&'static str` response cannot stall/hold a permit, so an overload sheds
  status DATA while the operator's incident window (the shell) still loads.
  Four guard tests, all reading the sources with comments stripped so prose
  cannot read as code: (1) the mutation scan -- a deny-list of mutating verbs
  in every spelling (quoted/unquoted/computed/form-attribute) plus form
  affordances and a `/status`-only path allowlist, AND a positive scan
  asserting the set of `method:` values the script sets is a subset of
  `{"query"}` (fails closed on a verb nobody deny-listed); (2) the
  `EXPECTED` schema-version map pinned to the panel `SCHEMA_VERSION` consts
  (`pub` on usage/health/config, `DOCTOR_SCHEMA_VERSION` on doctor,
  `query::SCHEMA_VERSION`) so client render-target and server wire versions
  cannot drift; (3) `QUERY_METRICS` + `QUERY_TOKENS` field names asserted to be
  fields of a serde-serialized `QueryMetrics` (derived, not a second hardcoded
  list) and the COMPLETE `QUERY_SHAPES` request vocabulary -- every selectable
  window x every group_by x each series mode, checked for completeness and
  duplicate-freedom -- validated through the route's own
  `query::spec_from_body`; (4) self-containment of the ASSEMBLED page -- every
  `src`/`href` attribute (case-insensitive, whitespace tolerated around `=`)
  must be an inline `data:` URI and no CSS
  `url(...)`/`@import` may appear, so the artifact still renders offline
- `src/handlers/status/dashboard.html` -- dashboard MARKUP only: the verdict
  strip (state dot + one plain-language sentence + req/span + window picker +
  poll indicator, which reports the as_of AGE of the active tab's data; carries
  no cost figure by design), the six-tab bar
  (Overview default, then Usage / Routing / Health / Config / Doctor), the
  banner, one identical pane shell per tab (`status-<tab>` +
  `body-<tab>`, the status line reporting only a NOT-current source), and the
  `modal-host` overlay host outside `<main>` (a pane's entry animation would
  otherwise become the containing block for a fixed-position modal). Never
  served alone; carries no `<link>`/`<script src>`
- `src/handlers/status/dashboard.css` -- dashboard STYLE body. One token set
  (dark primary, light re-valued under `prefers-color-scheme`), 8px grid,
  hairline borders, system sans for UI and mono for data, one accent plus one
  non-semantic second data hue, semantic green/amber/red reserved for
  good/degraded/broken. Layout dimensions (column min-widths, flex bases, the
  step-card width) are named `--col-*`/`--fb-*`/`--w-*` tokens on the 8px grid;
  hairlines and control details stay at their intentional off-grid values.
  Tables scroll inside their card (`.tablewrap`) so the page never scrolls
  sideways from 380px to 1440px
- `src/handlers/status/dashboard.js` -- dashboard SCRIPT body: the whole
  client. Six DATA SOURCES (`usage`/`health`/`config`/`doctor` off the GET
  `/status` aggregate, plus `query` and `usage_all`) each with an independent
  state record
  (`loading|live|empty|unavailable|incompatible|invalid_payload|stale|dead`),
  mapped to the six tabs by `TAB_SOURCES` -- so a dead QUERY degrades only
  Overview and Usage while the four GET-backed tabs keep rendering. `usage_all`
  is a SEPARATE GET of `/status/usage?window=all` on its own controller,
  backoff index, timer, and `as_of`, validated against the usage panel's wire
  version via `SOURCE_PANEL`; it exists because Routing attributes over ALL
  HISTORY while the aggregate's usage panel stays today-scoped for the readers
  that want today (the Health quota tiles, the Overview seat surface, the
  verdict strip). A section builder that throws is recorded in `RENDER_FAULTS`
  against its source, so `effectiveState` reports that source
  `invalid_payload` to the pane status line, the tab badges, the page verdict,
  and the favicon rather than calling it live beside an error card.
  `renderPanel` validates `schema_version` BEFORE reading `data`, then enforces
  the envelope invariant (exactly one of a meaningful `data` xor an
  `unavailable` code; zero or both -> `invalid_payload` with no transport
  backoff) and fails closed per source; `renderPanelGuarded` wraps each source
  independently so one malformed panel cannot fail the whole round.
  `safeSection(rec, build)` is the section-level boundary a multi-source tab
  builder wraps EVERY TAB_SOURCES dependency in, so a secondary source's fault
  costs that section only; `renderActiveTab`'s own try/catch is the whole-tab
  last resort. `queryStatus`
  issues `method:'QUERY'` + `Content-Type: application/json` + a stable
  stringified body + `cache:'no-store'` under a ~3500ms budget, single-flight
  (aborts the previous request and bumps a GENERATION so a late old response
  cannot repaint a newer selection), on backoff state SEPARATE from the GET
  loop's (a QUERY failure never slows the healthy 5s GET cadence): 403
  terminal, 400/405 -> incompatible and not retried until the input changes,
  503/network/timeout and the 200-borne `db_busy`/`db_unavailable`/
  `query_timeout` codes -> QUERY-only 10/20/30s backoff off the shared
  `BACKOFF_STEPS_MS` ladder. `QUERY_METRICS` (numeric) + `QUERY_TOKENS`
  (pass-through, e.g. `cost_status`) are the ONLY home of raw query field
  names, consumed solely by the thin `QueryAdapter` flat-extraction layer
  (num0 coercion on the numeric half, no rename, no computed model); render
  code reads adapter properties. Per-tab `buildX` functions live in
  marker-delimited blocks registered in `BUILDERS`; `buildOverview` is multi-source
  (`query` primary, plus `usage` for the seat surface): the provider row leads
  with an all-providers AGGREGATE card built from the query `totals` (the scope
  reset), then the busiest `PROVIDER_CARD_CAP` provider cards from `groups`, then
  -- only when more remain -- one overflow card whose expansion lists the rest.
  Every card stays on screen while scoped (the scoped one highlighted, another
  card moves the scope) and each re-issues the query with a `provider:` scope
  through `queryInputChanged`; the last UNSCOPED view is retained per window to
  draw the row, since a scoped response narrows `groups` to one provider. The
  scope is labeled directly above the KPI grid, and a scope strip renders outside
  the section boundary ONLY when the scoped query is unrenderable (so the scope
  stays reversible with no cards on screen). A card's seat affordance is per-seat
  DOTS plus the seat count, opening a page-level centered MODAL (backdrop click,
  close control, or Escape) of one quota tile per seat over the usage `quota[]`
  rows whose seat key names that provider -- the same tiles Health renders, so a
  provider with no quota row gets no affordance, no synthesized tile, and no
  cross-seat rollup. Eight KPI tiles come from `totals`, each carrying a
  sparkline over `series.buckets[].metrics` drawn at each bucket's own
  `start_ms`; the seat read is guarded on the usage record alone, so a usage
  fault costs the seat surface and leaves the KPI and provider blocks live;
  `buildUsage` renders the group-by picker (model/alias/provider, re-issuing
  the non-series query through `queryInputChanged`) outside the section
  boundary, then one two-line card per query group ranked by requests, with
  zero-traffic groups omitted and their count reported in the header;
  `buildRouting` is a MULTI-SOURCE tab (`config` primary, plus `usage_all`
  and `health`) and is WINDOWLESS (in `WINDOWLESS_TABS` beside Config and
  Doctor: the picker dims and goes inert, with an "All history" label beside
  it) -- the configured chains render from the config `aliases` and
  the live per-target circuit state from health, both as EXACT facts, visually
  separated from the estimated block whose per-step traffic comes from the sole
  `deriveStepTraffic(groups, chains)` derivation over the ALL-HISTORY usage
  `groups`
  (`~` + whole-percent figures, off-chain recorded models surfaced as their own
  footnote line, and the later-step headline read off the step distribution
  rather than the ledger's `fallback_served`); each of the three sources is
  wrapped in its own `safeSection`, so health going dark costs the state list
  alone. Every data-age figure (circuit "open for", "last ok",
  learned-negative age, quota reset) is computed against the as_of of the
  record it came from, passed down as `nowMs` from `panelNowMs(rec)` -- there
  is no page-global render clock; `buildHealth` is a MULTI-SOURCE tab (`health` primary, plus
  `usage`) -- per-target cards collapse a pooled model's seats by nickname
  through the same worst-circuit rule Routing uses and split into
  needs-attention / healthy / not-observed sections (a target with no settled
  outcome is `unknown`, never `Healthy`), while the per-seat quota tiles read
  the usage panel's `quota[]` discriminated by `provider_kind`, rendering one
  line per POPULATED utilization field (no synthesized second line, no
  cross-seat rollup) and distinguishing a missing snapshot, a null
  utilization, and a measured `0`; each source has its own `safeSection`, so a
  dark ledger costs the quota tiles alone and a dark health source leaves the
  quota tiles live; `buildConfig` is single-source (`config`) and FAILS CLOSED
  for the WHOLE tab -- the payload is validated in one place before any section
  is built, so a version mismatch or a malformed panel never partially renders
  config vocabulary -- rendering a source strip (config path, load age, resolved
  alias/provider counts, listen address, version) above the reference tables for
  aliases + their ordered chains, models + their winning catalog layer,
  provider activation, the capability overrides (default-closed disclosure), and
  the retry-class policy whose columns are exactly retry cap / fallback /
  breaker debit (the wire's `debits_breaker`) / source; `buildDoctor` is the
  second single-source FAIL-CLOSED tab (`doctor`) -- `validatedReport` checks
  every finding severity against the fixed `Pass|Warn|Fail` triad and every
  reachability verdict against `reachable|degraded|unknown` before a section is
  built, so an unknown token replaces the whole tab instead of being
  interpreted -- rendering a verdict card (state dot + headline + the panel's
  reachability rollup + failed/warning/passed counts), one card per
  failing/warning finding (severity pill, name, section, detail, and the
  remediation only when the finding carries one), and the passing checks behind
  a default-closed `buildExpander`; a report with findings but none needing
  attention reads as a welcoming all-clear, and a report with no check at all
  says so rather than claiming health

### ingress

- `src/ingress/mod.rs` -- `IngressAdapter` trait (incl. `early_frame`:
  warm-hold first-body-byte SSE events, default no-op; Anthropic emits the
  synthesized `message_start`, OpenAI / Responses emit nothing), `SseEvent`,
  `StreamRequestContext` (request-derived seed for `new_stream_state`: local
  input-token estimate + resolved model), `read_alias_header`
  (`x-routectl-alias` override); `MITM_PROXIED_HEADER`
  (`x-routectl-mitm-proxied`) -- the seam header the MITM front-proxy stamps
  on the re-injected `api.anthropic.com` inference leg, shared between the
  proxy set site and the `handlers::ingress_handle` forwarded-mode
  capture/admission read sites so the two cannot drift on the literal
- `src/ingress/token_estimate.rs` -- `estimate_input_tokens(&ChatRequest) ->
  u64`: pure/total zero-dependency char/token heuristic (`CHARS_PER_TOKEN`
  divisor) over system + message text; seeds the synthesized
  `message_start.usage.input_tokens` so the pre-inversion fast path shows a
  live context meter until the terminal `message_delta` overwrites it
- `src/ingress/openai.rs` -- OpenAI Chat Completions ingress; lifts
  `role:"system"` and `role:"developer"` messages into `req.system`
  (preserving per-block `cache_control`), promotes the Cursor-style
  top-level `systemPrompt` alias into canonical `system` (explicit
  `system` wins; the alias key is always removed so it never reaches
  `provider_extras`; a non-string alias is a local `Error::Validation`
  rather than a silent drop), lifts function tools, strips
  internal `matched_stop_sequence` on render, and stamps the OpenAI
  envelope on render (`object`, nullable `system_fingerprint`, `created`
  on chunks; synthesizes `chatcmpl-<uuid>` id + unix `created` when the
  upstream omitted them, stream-stable across chunks). Tests: inline unit tests in this
  file plus `tests/server.rs`, `tests/contract_ingress.rs`,
  `tests/cross_dialect_render.rs`, `tests/e2e_reasoning.rs`,
  `tests/replay_ingress.rs`
- `src/ingress/anthropic/mod.rs` -- `AnthropicIngress` impl (incl.
  `early_frame`: flushes the synthesized `message_start` and sets
  `state.started` so the real first-content chunk dedups it) + streaming state
  types (`AnthropicStreamState`, `OpenBlockKind`) + `encrypted_detail_data`,
  the shared `redacted_thinking.data` encoder used by both flatten sites
  (wraps a Responses-family blob in the `reasoning_envelope`; everything
  else, Anthropic-sourced above all, byte-verbatim)
- `src/ingress/anthropic/parse.rs` -- Anthropic body -> canonical
  `ChatRequest`; forward-compat sweep into `provider_extras`.
  `resolve_inbound_session_key` captures the inbound per-conversation key (the
  `x-claude-code-session-id` request header, else body `metadata.session_id`)
  into `routectl_internal.inbound_session_key` for session-sticky seat
  selection; the `metadata` read is non-destructive so `metadata` still
  round-trips into `provider_extras`
- `src/ingress/anthropic/render.rs` -- canonical `ChatResponse` -> Anthropic
  Messages response body shape
- `src/ingress/anthropic/stream.rs` -- canonical `ChatChunk` -> Anthropic SSE
  events with monotonic terminal-state guard; `new_state` seeds the state from
  `StreamRequestContext` and `emit_message_start` carries the input-token
  estimate (`usage.input_tokens`, `output_tokens` stays 0)
- `src/ingress/openai_responses/mod.rs` -- `ResponsesIngress` impl (`POST
  /v1/responses`, OpenAI Responses dialect / Codex client) + streaming state
  types (`ResponsesStreamState`, `OpenOutputItem`, `ToolCallBuffer`); inverse
  of the openai-responses egress
- `src/ingress/openai_responses/parse.rs` -- Responses body -> canonical
  `ChatRequest`: flattens the tagged-union `input[]` (`message` /
  `function_call` / `function_call_output` / `reasoning`) into `messages[]`,
  lifts `instructions`->`system`, `max_output_tokens`->`max_tokens`,
  `text.format`->`response_format`; forward-compat sweep into
  `provider_extras`. `reasoning.effort` lifts to canonical `ReasoningConfig`;
  the reasoning remainder (`summary`/`context`/`mode`/future) is stashed under
  `provider_extras["reasoning"]` (closed enums `summary`/`context` validated ->
  local 400 on an out-of-range value, `mode` open passthrough). Statefulness
  contract: `previous_response_id` -> 400; `store:true` (no prior id) accepted
  with WARN (persistence ignored)
- `src/ingress/openai_responses/render.rs` -- canonical `ChatResponse` ->
  Responses response body (`object:"response"`, `status`, `output[]` of
  message / function_call / reasoning items)
- `src/ingress/openai_responses/stream.rs` -- canonical `ChatChunk` ->
  Responses event-named SSE state machine (`response.created` ...
  `response.completed`, or `response.failed` on an error finish_reason) with
  monotonic `sequence_number`

### proxy

- `src/proxy/mod.rs` -- MITM front-proxy hub for first-party credential
  passthrough, deliberately isolated (imports none of `crate::handlers` /
  `server::AppState` / `crate::ingress`, so removing the feature is deleting
  this directory + the `pub mod proxy;` line + three call sites). Declares the
  submodules and documents the leg pipeline: `listener` binds the CONNECT
  front and builds the shared `MitmCtx` / `TlsAcceptor` once, `mitm`
  terminates TLS + serves HTTP/1.1 per connection, `split` classifies each
  decrypted request against `ANTHROPIC_INFERENCE_PATHS`, `forward` is the
  shared classification-agnostic byte forwarder both legs reuse, `ca` owns the
  CA / leaf lifecycle, `metrics` the lock-free counters, `cc_version` the
  warn-and-proceed Claude Code version check. The three outside call sites are
  `commands/rc.rs` (`proxy::ca`), `server/mod.rs` (`proxy::listener`), and
  `handlers/models.rs` (`proxy::forward` + `proxy::metrics`); the whole task
  is spawned only when `Config::mitm.is_some()`
- `src/proxy/ca.rs` -- local CA + leaf certificate lifecycle for the
  configured MITM host. `load_or_create(cert_dir, mitm_host) -> TlsAcceptor`
  generates/loads a long-lived local CA (aws-lc-rs backend, matching the
  workspace rustls provider) plus a matching leaf meant for operator install
  via `NODE_EXTRA_CA_CERTS`; `regenerate` re-mints, `ca_cert_path` locates the
  CA PEM, `CaError` is the error enum. Expiry is metadata-driven: the exact
  `not_after` (truncated to midnight UTC to match the `Time::MIDNIGHT` rcgen
  bakes into the DER) is mirrored into a sidecar TOML read directly on reload,
  rather than re-parsing the X.509 DER `notAfter`
- `src/proxy/cc_version.rs` -- Claude Code version warn-and-proceed check:
  WARNS (never hard-refuses, so a CC release never breaks routectl) when the
  observed version drifts from the tested one, the only signal that CC's wire
  shape moved out from under `routectl_core::identity::anthropic`'s pinned
  defaults. `observed_cc_version(&HeaderMap)` extracts the `<version>` token
  from a `claude-cli/<version> (external, cli)` `User-Agent`;
  `CcVersionWarnGuard::check(tested, observed)` dedups so a steady mismatch
  warns exactly once and a version change re-warns
- `src/proxy/forward.rs` -- the dumb, classification-agnostic byte forwarder
  both split legs reuse (loopback re-inject and catch-all upstream forward):
  streams bytes and records what it is told, never classifies. `forward(...)`
  is the async forward call, `build_client` builds the shared reqwest
  `Client`, `ForwardState` holds it plus the concurrency cap + idle window,
  `ForwardRequest` carries the per-call inputs, `ForwardBody` =
  `UnsyncBoxBody<Bytes, ForwardBodyError>` (the `http` / `http-body` /
  `http-body-util` vocabulary reqwest and hyper 1.x share, so the
  `http::Response<ForwardBody>` hands straight to a hyper `Service`). Consts:
  `CONNECT_TIMEOUT` (10s), `STREAM_IDLE_WINDOW` (10m),
  `DEFAULT_MAX_CONCURRENT_STREAMS` (256)
- `src/proxy/listener.rs` -- the CONNECT front-listener assembly point: binds
  a loopback TCP port, speaks the HTTP `CONNECT` tunneling protocol a client
  configures via `HTTPS_PROXY`, and dispatches each accepted connection to
  either `mitm::handle_mitm_connection` (the configured `mitm_host`) or an
  opaque `tokio::io::copy_bidirectional` blind tunnel (every other host --
  since `HTTPS_PROXY` is process-global, a client's telemetry / Sentry / other
  outbound HTTPS must pass through untouched, never terminating TLS).
  `build_and_bind` binds the socket + constructs the shared `MitmCtx` /
  `TlsAcceptor` once at startup; `spawn` runs the accept loop;
  `ProxyListenerConfig` is the inputs, `ProxyStartError` the bind/startup
  errors. The feature-enabled gate lives in `server::serve_on_listener` (keyed
  on `Config::mitm.is_some()`), not here
- `src/proxy/metrics.rs` -- lock-free observability primitives for the proxy.
  routectl has no metrics backend or exporter, so this stands none up (no
  `/metrics` endpoint, no exporter dep) -- each metric is a `Relaxed`
  `AtomicU64` counter mirroring the `routectl_usage::handle::UsageCounters`
  pattern, with the Prometheus-shaped names kept only as `tracing`-snapshot
  label metadata; `ProxyMetrics::log_snapshot` is the single seam to swap for
  an exporter if one ever lands, emitted from TWO places in the listener's
  accept loop (a periodic tick and the graceful-shutdown flush), each with its
  own regression test. `ProxyMetrics` counts requests by the three
  closed dims (`Leg` / `ResultClass` / `PathClass`), open/closed streams, idle
  aborts, unknown forwarded paths, and TLS handshake failures/timeouts;
  `WarnOnce::warn_once(method, path)` dedups a per-path warning. By
  construction no token / credential / body ever enters a counter dimension or
  a log line -- the only inputs are the small closed enums plus method + path
- `src/proxy/mitm.rs` -- per-connection TLS termination + HTTP/1.1 serving.
  `handle_mitm_connection(tcp, acceptor, ctx, permit)` is the per-connection
  entry the listener spawns once per accepted tunnel: it assumes `tcp` is the
  raw duplex behind an established CONNECT tunnel, terminates TLS via the
  shared acceptor, serves HTTP/1.1 (`hyper::server::conn::http1` +
  `service_fn`), and hands each request to `split::handle_request`. Fails
  closed (a handshake or connection-level error is logged and the connection
  dropped, never a plaintext fallback); the `permit` concurrency slot is held
  only so dropping it releases the slot panic-safely alongside `tcp`.
  `MitmCtx` is the shared per-proxy context built once by the listener behind
  an `Arc`: `forward_state`, `metrics`, `warn_once`, `upstream_origin` (real
  Anthropic origin), `reinject_base` (routectl's own loopback listener),
  `tested_cc_version` + `cc_version_warn_guard`, and the `seam_nonce` (the
  SAME `Arc<ingress::MitmSeamNonce>` on `AppState`, so the proxy stamper and
  the ingress checkers agree without re-generating it)
- `src/proxy/split.rs` -- request classification + split-leg dispatch.
  `handle_request(ctx, req)` (generic over `B: http_body::Body` so tests drive
  a synthetic body without a real TLS / HTTP1 connection; production
  instantiates `hyper::body::Incoming`) decides per decrypted request whether
  it is Anthropic-dialect inference traffic (re-inject over loopback into
  routectl's own listener, which carries the credential-swap seam) or anything
  else (forward verbatim to the real Anthropic origin).
  `ANTHROPIC_INFERENCE_PATHS` is the single source of truth for that decision
  (exposed via `anthropic_inference_paths()`), deliberately excluding
  `/v1/chat/completions` and `/v1/responses` (direct-client ingress dialects
  never reached through this proxy)

### commands

- `src/commands/mod.rs` -- groups CLI subcommand entry points (init, test,
  prompt_size, config, migrate, provider add, provider probe, doctor, login,
  logout, refresh, whoami, usage, catalog, catalog_import; the shared
  `capability_legacy` legacy-key helper lives here too; `serve` lives in
  `crate::server`)
- `src/commands/config.rs` -- `routectl config check/show/example` (secret
  resolution, alias chain validation, the shared `collect_config_validation`
  suite). On `check`, each semantic error is passed through `locate` ->
  `locate_dotted_path` (fed the raw config text) to gain a `(line N): ` prefix
  pointing at the `config.toml` line that produced it; `derive_dotted_path`
  conservatively recognizes leading `[a.b.c]` headers and
  ``alias/model/provider `X` `` clauses, and unresolvable messages fall back
  unchanged (a display aid only, never a taxonomy change). `EXAMPLE_CONFIG`
  is the ONE `include_str!` of `examples/config.toml`, named by `config
  example` and by `init --scaffold`'s `STARTER_CONFIG`; an ungated test
  gate-loads it so no build can emit an example it cannot itself parse
- `src/commands/config_edit.rs` -- `routectl config set/unset`: the write
  pipeline through the shared gate. Order: RAW version preflight FIRST
  (refuses `version < CURRENT_CONFIG_VERSION` byte-identically BEFORE any
  shared-loader call -- the loader preflight-rejects a too-old file and only
  `config migrate` migrates it; also rejects too-new) + legacy-key preflight;
  `validate_config_path` (unknown segments error with siblings pre-mutation;
  `Table` targets allowed only for unset); scalar inference
  (bool/int/float/else string -- the re-parse gate backstops mistypes);
  in-memory candidate gate (`parse_config` + `collect_config_validation`, full
  diagnostic rendering, any failure writes nothing); high-consequence confirm
  BEFORE the lock (`collect_high_consequence_changes`, `--yes` bypass); then
  `edit_config_toml` (lock + revision check + atomic write). unset removes the
  key and recursively prunes now-empty parent tables (empty == absent);
  missing key = NoChange, no write. Success prints the exact restart-required
  field names and emits ONE value-free `tracing::info` audit
  (surface/verb/path/restart_required/high_consequence); a no-op writes and
  logs nothing
- `src/commands/config_migrate_cmd.rs` -- `routectl config migrate`: brings a
  legacy `config.toml` forward to `CURRENT_CONFIG_VERSION` through the shared
  `routectl_router::plan_migration` PURE planner, committing through the same
  `edit_config_toml` write primitive as `config set`. Order
  (check-before-write end to end): snapshot + raw `version` read; PLAN the
  migration purely (`plan_migration` -> `MigrationPlan`, no disk mutation -- a
  `Refusal` / future-version file surfaces here with a truthful "nothing was
  written"); a `NoChange` plan (`config_candidate()` -> `None`) short-circuits
  to `AlreadyCurrent`; gate the plan's config candidate through the shared
  `parse_config` + `validation_report` suite (a parse failure is stripped of
  its verbatim source-line preview via
  `redact_parse_error`/`is_source_snippet_row` first -- the offending config
  line may carry a `literal:` credential, so only the header line/column +
  message kind are kept); `--dry-run` renders the candidate + the plan's
  `removed_keys` summary and writes nothing (no temp copy -- planning never
  touched the real files); otherwise ACKNOWLEDGE every real write
  (`confirm_migration` -- interactive `y` / `--yes`, with `--force` kept one
  release as a deprecated hidden alias for `--yes`;
  non-interactive-without-acknowledgement refuses), including a same-version
  v3 normalization, AFTER the gate and BEFORE any write; then `commit_plan`
  (takes the plan BY VALUE, moving the overlay cells) writes the overlay FIRST
  via `commit_overlay` -> `with_overlay_write_lock` (the revision check runs
  INSIDE the advisory lock, so a concurrent `catalog` writer can neither slip
  between the check and the rename nor be silently overwritten) and
  `config.toml` LAST as the visible completion marker (via `edit_config_toml`,
  whose closure re-applies the SAME `apply_config_transforms` the plan gated,
  under the write lock against the original snapshot). Two-file commit is
  recoverable, not atomic: a config-side failure AFTER the overlay landed is
  reported as resumable (`resumable_commit_error`/`CommitFailure`, outcome
  `incomplete`) and NEVER claims "nothing was written"; a rerun re-plans
  (overlay fold now a no-op) and completes.
  `MigrateResult::{AlreadyCurrent,DryRun,Migrated{from_version},Aborted}`; a
  `Refusal` / future-version file / gate failure / conflict surfaces as `Err`.
  Emits ONE value-free audit event (surface/verb/from/to
  version/dry_run/ack/force/outcome/refusal_kind/path -- never candidate bytes
  or a config value); `outcome` is one of
  `no_change`/`refused`/`version_too_new`/`v1_migration_failed`/`invalid`/`dry_run`/`aborted`/`written`/`incomplete`/`conflict`/`write_failed`
  (`CommitFailure.outcome` labels the commit-phase failure by variant:
  `incomplete` once the overlay landed, `conflict` for a genuine revision /
  base-bytes conflict, the neutral `write_failed` otherwise) and
  `acknowledged` is true ONLY after a real interactive `y` (never synthesized
  -- a `--yes` write records `acknowledged=false`). `run_at` takes the overlay
  path explicitly so tests point both files at a temp dir
- `src/commands/config_effective.rs` -- `routectl config show --effective`
  render layer over `routectl_router::derive_effective_view`: the plain
  redacted dump (unchanged), then provenance-annotated sections for the two
  genuinely layered surfaces -- per-model catalog cells
  (source=baked/import/user/disabled + verified_at) and per-class retry policy
  (source=config vs baked-default). No generic per-key provenance tree
- `src/commands/edit_pipeline.rs` -- building blocks shared by the
  config.toml-mutating commands (`config set/unset`, `provider add`): the raw
  version/legacy preflights, the in-memory `parse_config` +
  `collect_config_validation` gate plus its error rendering, and the pre-lock
  high-consequence confirmation prompt. One home so every mutating command
  refuses stale/legacy files, re-validates candidates, and prompts on
  egress-defining edits identically
- `src/commands/provider_env.rs` -- conventional per-provider-kind credential
  env-var table (`anthropic-api`->`ANTHROPIC_API_KEY`,
  `openai-compat`/`openai-responses`->`OPENAI_API_KEY`), gated on
  `routectl_router::is_cataloged_provider_kind`. `env_var_for_kind(kind)`
  returns the single var an onboarding flow may OFFER as `env://VAR`; an
  explicit `EXCLUDED_KINDS` list documents cataloged kinds with no single
  conventional var (bedrock: multi-var), and a drift test forces every
  cataloged kind into exactly one of the table/exclusion set. Suggest-only:
  never reads the environment, never auto-routes
- `src/commands/provider_add/mod.rs` -- `routectl provider add` entry +
  orchestration: the `ProviderAddArgs` flag surface, the injectable `AddIo`
  seam (stdin / hidden prompt / env offer / oauth login) with production
  `RealAddIo` (std + rpassword + the `login` command) so the pipeline is
  testable without a TTY or a browser, and `run`/`run_with_io` -- adds or
  overwrites a `[providers.<name>]` block (same-name overwrite requires
  `--overwrite`; `--yes` skips the egress-defining confirmation) through the
  same single write-path (`edit_config_toml`) as `config set`, via the shared
  `edit_pipeline`. ORDER discipline: the ref STRING is computed pre-confirm;
  the actual `put`/login runs AFTER the high-consequence confirm and BEFORE
  the locked write, so a declined confirm captures nothing and the advisory
  lock is never held across a prompt/login/env-probe/put. A post-capture write
  conflict reports an explicit recovery (secret/login persists, config
  unchanged). Emits ONE value-free audit event
  (surface/verb/name/kind/credential class -- never the value or full ref);
  owns `AddResult` {Written/NoChange/Aborted/Rotated} and the pinned
  `credential rotated; config unchanged` line. On the `Written` path only,
  offers a scoped capability probe against the just-added provider: `--probe`
  dispatches without prompting, `--no-probe` suppresses it, and the
  interactive default asks the new `AddIo::confirm_probe` seam AFTER printing
  the cost line; the offer runs strictly after the commit + secret put and
  writes only the capability ledger (never rolls the add back), and silently
  skips when no single model routes to the provider yet (delegates to
  `probe::capabilities::offer_scoped_probe`)
- `src/commands/provider_add/build.rs` -- provider-entry construction from
  args: `build_entry` validates the kind against the supported set and
  dispatches to the direct api-key path (`resolve_secret` -> `env://` /
  `--secret-ref` verbatim / stdin / interactive), `build_forwarded`
  (anthropic-api only; no secret, base pinned to api.anthropic.com), or
  `build_oauth` (`anthropic` -> `oauth://<provider>` + oauth-bearer, login
  deferred). Returns the `ProviderEntry` + credential CLASS label + the
  `PendingSecret` owed post-confirm
- `src/commands/provider_add/capture.rs` -- interactive/stdin secret capture +
  deferred execution: the `PendingSecret` enum {None/File/OAuth} and
  `execute_pending` (managed-store `put` / oauth login, run post-confirm),
  plus `capture_from_stdin` (piped `--api-key-stdin`; errors immediately if
  stdin is a TTY), `resolve_interactive` (`env_var_for_kind` env-detect OFFER,
  never auto-routed, then a hidden `rpassword` prompt), and `capture_value`
  which opens the managed 0600 `file://` store ONCE and computes the ref via
  `ManagedSecretStore::ref_path` so the pre-confirm ref string and the
  post-confirm `put` share one canonical base
- `src/commands/provider_add/toml_edit.rs` -- config.toml provider-block edit
  + commit: `provider_table` serializes a `ProviderEntry` to a minimal
  non-inline table (prunes serde-default empties), `insert_provider_block`
  surgically inserts/replaces `[providers.<name>]` preserving comments +
  ordering, and `commit` re-reads under the advisory lock + revision check and
  writes the same deterministic insert atomically through
  `routectl_router::edit_config_toml` (re-gated; a stale snapshot or gate
  failure writes nothing)
- `src/commands/init/mod.rs` -- `routectl init` guided first-run setup.
  Defines the interactive seam (`InitIo: AddIo` -- one fake drives every
  wizard prompt AND the inherited credential seams; `RealInitIo` delegates the
  `AddIo` half to `RealAddIo`), the `InitArgs` flag surface, the leaf types
  passed between steps (`Offer` + `OfferSource`
  {Oauth/Env/Forwarded/ApiKeyPrompt} + `ModelWiring`;
  `Offer::provider_add_kind` maps an oauth offer to the `anthropic` login
  sentinel so it is never mis-routed into the api-key path), the
  `CredentialCapture` choice enum {OauthLogin/ApiKey/Skip}, the single-sourced
  `next_steps` renderer (run-doctor hint + `SERVE_COMMAND` + sample curl,
  shared with a later doctor path) and its `SERVE_COMMAND`/`DOCTOR_NEXT_HINT`
  consts, and the command core: `run`/`run_with_io` (load-or-default config,
  probe the oauth store, compose the sorted offer inventory) plus the
  write-ordered `orchestrate` (scaffold fast-path vs guided wizard,
  `--forwarded` opt-in synthesis, the EMPTY-OFFER capture branch,
  `collect_answers` -> `build_plan` -> the ONE wizard-level ack ->
  `apply_plan`); the empty-offer branch closes the credential-less first-run
  dead-end: interactively it offers oauth login or a hidden api-key prompt
  (`capture_missing_credential` -> `offer_credential_capture`) and synthesizes
  ONE `Offer` (`OfferSource::Oauth`/`ApiKeyPrompt`) wired through the same
  `provider add` seams, while a `--yes`/declined run prints the actionable
  `missing_credential_next_steps` (`routectl login anthropic` / set
  `ANTHROPIC_API_KEY`, then re-run) and exits cleanly rather than surfacing
  the raw `MissingDefaultRoute`; `apply_plan` seeds a base config on a fresh
  machine, composes each provider via `provider add` (atomic + gated +
  idempotent), then the single models/aliases write, wrapping any post-ack
  failure with the explicit re-run recovery message (config on disk stays
  valid, captured credentials persist and are reused); AFTER routing lands it
  offers the scoped capability probe (via
  `probe::capabilities::offer_scoped_probe`, same
  `--probe`/`--no-probe`/interactive semantics) for each provider written THIS
  run whose lane did not exist during the in-loop `provider add` hook -- a
  provider whose selectable model pre-existed was offered in-loop, so it is
  skipped here, yielding at most one offer per provider per run
- `src/commands/init/detect.rs` -- the detect step: `detect_offers(config,
  probes)` composes the sorted `Offer` inventory (deterministic `(source,
  kind, provider_name)` order), PURE (no lock/network/mutation, reads env +
  the passed config/probes only). Forks no new detection logic: the oauth arm
  feeds shipped local probes through `routectl_router::compute_activation`
  (the SAME inventory the server activation path consumes, one `Activated`
  entry -> one offer), the env arm reuses the conventional-var table
  (`ENV_OFFERABLE_KINDS` restricted to `anthropic-api`, offered only when
  `env_ref` resolves non-empty NOW), the forwarded arm keys off a `[mitm]`
  block's presence; `forwarded_offer()` is `pub(super)` so detection and the
  orchestrator's `--forwarded` synthesis name the provider/kind identically
- `src/commands/init/plan.rs` -- the wizard's PURE decision engine:
  `build_plan(&WizardAnswers, existing, offers) -> Result<WizardPlan,
  PlanError>` turns collected answers into the ordered `ProviderAddArgs` list
  (one per selected provider, `yes: true` since init owns the one ack, and the
  operator's `--probe`/`--no-probe` choice threaded onto each so the post-add
  offer fires through the inherited `AddIo` seam), one `ModelWiring` per
  provider, and the single `aliases.default` nickname -- routing lands ONLY
  through `default_alias`. `selected_in_order` filters the canonical offer
  list for a total stable order; `assign_nick` picks the provider-name
  nickname disambiguated with a numeric suffix only against a same-plan
  collision or an existing `[models.<nick>]` targeting a DIFFERENT provider (a
  same-provider existing model is reused verbatim -- the byte-identical
  re-init contract). `PlanError`
  (`MissingModelId`/`MissingDefaultRoute`/`DefaultRouteNotSelected`) is typed
  + actionable, surfaced before any side effect
- `src/commands/init/scaffold.rs` -- the `--scaffold` fast-path plus the
  wizard's fresh-machine seed. `scaffold_fresh` drops the committed starter
  `examples/config.toml` (single-sourced with `config example` off the one
  `EXAMPLE_CONFIG` embed); `scaffold_seed` lays down the minimal
  `version`-only anchor
  the wizard needs on disk before `provider add`/the final write can edit
  through the one write path. Both go through `scaffold_from_text`:
  shared-gate-validate the text, then publish via fsynced temp file +
  no-clobber (`persist_noclobber`, O_EXCL) atomic rename + parent-dir fsync,
  so neither a gate failure nor a mid-write error leaves a partial/invalid
  file and a racing file routes to the typed `ScaffoldError::AlreadyExists`
  the orchestrator matches onto the existing-config walk (`ScaffoldError`:
  `AlreadyExists`/`Gate`/`Io`)
- `src/commands/init/write.rs` -- the ONE final init config write:
  `commit_models_aliases` re-reads `config.toml` under the advisory lock +
  base-bytes revision check and commits the deterministic
  `insert_models_and_default_alias` (each `[models.<nick>]` block = `provider`
  + `upstream` only, no serde defaults; `aliases.default = <nick>`) atomically
  through the shared `routectl_router::edit_config_toml`, mirroring
  `provider_add`'s insert+commit exactly. Idempotent: `EditOutcome::Unchanged`
  (nothing written) only when every planned model block AND the default alias
  already match byte-for-byte; a partial match writes the missing pieces while
  leaving matching ones untouched. A stale snapshot or a gate failure writes
  nothing
- `src/config_classify.rs` -- pure config-diff classifiers shared by
  hot-reload logging and `config set/unset`:
  `collect_restart_required_changes(prev, next)`
  (bind/listener-auth/body-limit, the `[log]` knobs, `usage.db_path`,
  `usage.retention_days`, `[mitm]`, and per-provider `codex_version` -- all
  startup-only state; `codex_version` is stamped into the process-global codex
  identity once at boot) and `collect_high_consequence_changes(prev, next)`
  (provider `base_url`, `credential_source`, `[mitm]` egress fields -- the
  confirm-before-write set). A coverage-tripwire test walks the schema's
  top-level properties and fails on any unclassified new `Config` section; the
  `[capability]` section classifies as plain hot-reloadable (neither
  restart-required nor high-consequence)
- `src/commands/test.rs` -- `routectl test <target>` one-shot completion
  against an alias or model nickname
- `src/commands/prompt_size.rs` -- `routectl prompt-size --alias <X> --request
  <fixture.json>` offline report of a request fixture's per-tier (SYSTEM /
  TOOLS / MESSAGES / TOTAL) byte + approx-token footprint and the projected
  auto-emit decision (caller-supplied / would-inject / globally_disabled /
  no_capability / volatile_vetoed / indeterminate) + reduction outcome. Pure
  `build_report(&ChatRequest, Option<bool>, auto_emit_enabled: bool,
  reduction_enabled: bool) -> Report`; the auto-emit and reduction projections
  reflect the current `[cache]` / `[reduction]` config switches; config-only
  alias->provider cache-capability resolution (no secret resolution, no
  provider build, no network), sharing the router's
  `ALIAS_MAX_RECURSION_DEPTH`. Runs the same cheap config guards as `test`.
  OPTIONAL fourth section -- the advisory cache-break economics projection --
  is emitted ONLY when `--hypothetical-d <TOKENS>` is supplied (no-flag output
  stays byte-identical): `resolve_target` reuses the alias resolution to get
  `(provider_kind = kind_str(), model = upstream)` offline,
  `build_economics(total_tokens, target, &ProjectionArgs)` resolves the
  two-layer catalog merge itself via `lookup_baked_with_overrides` + `merge`
  (the same entry the router's chain-build pass uses, since this offline
  command has no resolved-target chain to ride a precomputed `EffectiveRow`
  on) and calls the `cost_gate` (`break_even_k` / `evaluate`), and
  `print_economics` renders the resolved cell, its `trust_label` (`priced` /
  `unpriced (NEEDS-LIVE-PROBE)` -- named for the merge result, not a per-row
  `verified` flag, which is gone), the break-even K* (suppressed -> `KEEP
  (insufficient data)` for an unpriced/sentinel cell), and -- when
  `--hypothetical-k <COUNT>` given -- the KEEP/BREAK verdict with its stable
  `strategy_str()` ledger token. Flags: `--hypothetical-d` (u64, turns the
  section on), `--hypothetical-k` (f64), `--c-after` (u64, defaults to C),
  `--ttl-tier` (`5m`|`1h`, default `5m`). C = the report's TOTAL approx-token
  count (the command has no separate cacheable-prefix slice).
  `EconomicsProjection`/`Report` are `PartialEq` (not `Eq`: the projection
  carries `f64`). Still offline-only -- no router.rs / context_reduction.rs
  touch
- `src/commands/login.rs` -- `routectl login <provider> [--label <name>]` runs
  the OAuth 2.0 PKCE flow (anthropic, codex), persists tokens via
  `OAuthStore`; `--label` registers an additional seat without overwriting the
  default; `--print-url` headless flow guarded against providers without a
  paste-back landing page
- `src/commands/logout.rs` -- `routectl logout <provider> [--label <name>]` --
  removes one seat (`--label` removes only the named seat; no label removes
  the default) from the credentials store; first-time logout reported but not
  an error
- `src/commands/refresh.rs` -- `routectl refresh <provider> [--label <name>]`
  -- forces a refresh of one seat through the per-seat single-flight gate,
  regardless of expiry
- `src/commands/whoami.rs` -- `routectl whoami` -- prints OAuth seat state
  grouped by provider (default seat as `<provider> (default)`, labeled seats
  as `<provider>#<label>`), each with its own expiry; exits 0 when at least
  one seat is logged in, 2 otherwise
- `src/commands/seat.rs` -- shared `--label` validation for the seat-aware
  OAuth commands (rejects empty/whitespace labels, mirroring the `oauth://`
  ref parser)
- `src/commands/staleness_hint.rs` -- catalog-overlay staleness nudge for the
  human CLI verbs. Pure `staleness_hint_line(verified_at, threshold_days,
  today_epoch_days)` (delegates the strict-greater-than age check to
  `routectl_router::is_stale_days`) + `freshest_verified_at(&CatalogOverlay)`
  (lexicographic max stamp over present cells) + the `emit_staleness_hint`
  seam that takes every gate (is_tty / is_ci / kill_switch / is_json) as an
  injected boolean and writes ONLY to the passed stderr handle. `main.rs`'s
  `emit_staleness_hint_for` binds the seam to the live environment (stderr
  terminal-ness, `CI`, the `ROUTECTL_NO_STALENESS_HINT` kill switch,
  `[capability] staleness_hint_days`) at `doctor`, `catalog list`, and `config
  show` (non-JSON verbs only; never `serve`, `--json`, or an empty overlay)
- `src/commands/usage.rs` -- `routectl usage` read surface over the usage DB
  (read-only). Calendar windows (`--today`/`--this-week` (Monday-start ISO
  week)/`--this-month`/`--all`) and ad-hoc `--since D [--until E]` ranges
  computed in LOCAL time against an injectable `now` (testable window-math
  fns); no flag + no `--since` prints a multi-window summary.
  `build_window_report` aggregates, rolls fine `AggRow`s up to `--by
  model|provider|alias` (or a single total), and bifurcates cost: a provider
  whose `[providers.X] api_key_ref` starts `oauth://` is subscription (`n/a
  (subscription)`, no $), an API-key provider prices its summed tokens via
  `Config::pricing_for` -> `estimate_cost_tokens` (`$X.XX` or `n/a` when
  unpriced) through `cost_for_row`, which yields the usage crate's `RowCost`
  tri-state and is shared with `/status/query`'s pricing closure so the two
  surfaces can never disagree about what a row costs. `--detail` adds cache-write split + nearest-rank p95/max latency
  + wall-time + server-tool counts. Both the per-group and the footer
  cache-hit-rate flow through ONE shared denominator rule --
  `cache_prompt_den` (`input + cache_read_billed + cache_write_5m +
  cache_write_1h`, the cache-INCLUSIVE prompt total, summed only over rows
  with `cache_read_present > 0`) fed to `cache_hit_ratio(num, den)` (`None` on
  a degenerate `den <= 0`, rendered `-` not `0%`), so a mixed-provider window
  can never show two contradictory hit%. Footer also reports the total error
  count. `OpenError::NoData` -> friendly stdout + exit 0; `VersionTooNew` ->
  hard error. Also the project's LOCAL-CALENDAR AUTHORITY for the read surfaces:
  beside the window math it owns `MAX_BUCKETS` and `resolve_bucket(unit, from,
  to, first_row, now)`, the pure DB-free resolver `/status/query`'s series mode
  calls -- it re-anchors an all-time window to the earliest row's local midnight
  (clamped never below the window's own lower bound), then widens the requested
  `hour`/`day` width by a whole multiple (i128 intermediates) until the count fits
  the cap, so the grid always covers the window and never exceeds it
- `src/commands/catalog/` -- `routectl catalog` (hidden alias `pricing`,
  dropped at 1.0), split into a command-entry facade plus three concern
  modules; every original
  `commands::catalog::{list,verify,set,disable,export,build_list_data,render_table,print_pickup_note,verify_at,set_at,PricingVerifications,load_verifications,merge_verifications_into,load_and_merge_verifications,CatalogWriteError}`
  path is preserved via re-exports.
  - `mod.rs` -- command entry + module doc (subcommand overview +
    legacy-sidecar rationale) and the re-export facade; owns
    `today_verified_at` (the one shared UTC `verified_at` stamp every writer
    reads) and `verifications_path` (legacy sidecar location).
  - `verifications.rs` -- legacy `pricing_verifications.json` READ side only
    (`PricingVerifications`, `load_verifications`, `merge_verifications_into`,
    `load_and_merge_verifications`), migration input consumed by the v1 -> v2
    config migration (`config migrate` folds historical sidecar stamps into
    `config.cache_pricing` before the migrator moves them into the catalog
    overlay; `server::load_effective_config` no longer migrates -- it
    preflight-rejects a too-old config); nothing writes the sidecar anymore.
  - `render.rs` -- `list` rendering: `overlay_summary_line` header (revision +
    counts by source + disabled count) then the EFFECTIVE catalog (the
    two-layer merge of the baked table with `catalog_overlay.json`,
    `routectl_router::merge`) via `build_list_data` as an aligned ASCII table
    (columns: provider_kind, model_glob, status, tier, wm, rm, ttl(s),
    min_prefix, auto, max_ctx, source, verified_at, stale); every row renders
    PRESENT (with derived provenance + a staleness marker) or DISABLED
    (overlay `null`). `render_table` is `pub(crate)` so
    `commands::catalog_import` reuses the same aligner for its diff table.
  - `write.rs` -- overlay write verbs. `verify <selector>` stamps an EXISTING
    overlay cell's `verified_at` to today and flips its `source` to `user`
    (verifying is a user act) via the revision-checked writer; a selector with
    no overlay cell (baked-only or unknown) errors -- creating a cell is a
    `set` concern. `set <selector> <field>=<value>...` writes a `source: user`
    cell for a selector KNOWN to the catalog (exact baked-table key, or
    existing overlay cell of either provenance) -- an unknown selector is a
    hard `CatalogWriteError::UnknownSelector`, the synthetic-row poisoning
    guard; fields are
    `wm`/`rm`/`ttl_seconds`/`min_prefix_tokens`/`max_context_tokens`/`input_cost_per_token`/`output_cost_per_token`
    plus
    `cap:<name>=true|false` flags;
    `auto_cacher`/`has_storage_rent`/`storage_rent`/`verified_at` hard-reject;
    value validation reuses `CachePricingOverride::validate` against ONLY the
    fields this call touches (`validate_updates`). `disable <selector>` writes
    JSON `null` for a known selector, discarding prior fields. `export`
    serializes the on-disk overlay to pretty JSON (read-only, no credential
    material). Both `set`/`disable` stamp today (UTC) and print the same
    serve-pickup note `catalog import` does. `verify_at`/`set_at` are
    re-exported `pub(crate)` (test-only) so `commands::catalog_import` reuses
    the writer cores in its own tests.
- `src/commands/catalog_import.rs` -- `routectl catalog import`: the opt-in,
  never-at-startup overlay refresh from the litellm + models.dev sources. This
  is the CLI fetch boundary (`reqwest`, ~10s timeout, one retry;
  `--litellm-file`/`--models-dev-file` read both sources from disk instead,
  both-or-neither) -- `routectl-router` itself stays reqwest-free. Flow: fetch
  both sources -> `build_import_candidate` (one run-wide UTC `verified_at`) ->
  `shrink_guard` vs the persisted `catalog_import_state.json` baseline
  (`--allow-shrink` bypasses ONLY the shrink floors; the shrink-refusal report
  `shrink_verdict_report` also prints the count of selectors skipped for an
  EXPECTED cross-check disagreement, which count as present toward the totals)
  -> `diff_overlay` against the overlay loaded before any lock is taken ->
  render the impact-labeled diff (reuses `catalog::render_table`; sections
  applied/skipped/conflicted/cleared, a `cheaper?` flag, and an "identical (user cell
  preserved)" note for a display-only conflict against an already-matching
  user cell) -> y/N confirm (`--yes` skips it) ->
  `confirm_and_apply`/`apply_diff` acquire `with_overlay_write_lock` ONLY at
  this point, merge `diff.applied` rows and REMOVE `diff.cleared`'s stale
  import cells (never conflicted/skipped; each clear re-checks
  `is_import_cell` under the lock so a removal can never take out a
  `source: user` cell), and
  on a revision conflict release, recompute ONE fresh diff + confirm against
  the latest overlay (a second conflict aborts, no retry loop); a diff with
  nothing to write (empty `cleared` and no `applied` row that differs from
  disk) skips the lock entirely rather than pay for a no-op revision
  bump. Any source-level fetch failure (non-200 / invalid JSON / non-object
  top-level shape) aborts before the overlay is ever opened, so it stays
  byte-identical. `catalog_import_state.json`'s baseline is persisted only
  after `confirm_and_apply` returns `Ok`. Structured `tracing` events per
  phase (start/fetch/cross-check/shrink/diff/commit); prints that a running
  `serve` picks up the change via the overlay watch. Tests include an
  import-vs-`catalog::set_at` writer-serialization case alongside the
  import-vs-`verify_at` one.
- `src/commands/probe/mod.rs` -- `routectl provider probe [<name>]`: the
  read-only, free-only reachability report. `probe_all(config, store,
  deadline)` is THE shared orchestration (reused by the doctor aggregator) --
  the per-credential branch lives here once so both surfaces classify a
  provider identically: `credential_source = forwarded` -> `Skipped` (no
  build, no upstream call); an `oauth://` ref -> the in-memory-only
  `OAuthStore::probe_local` (never a resolving `get`, so no near-expiry
  refresh, credentials byte-identical); `env://`/`file://`/`literal:`/bedrock
  -> `build_provider` + its free `probe()`. Bounded by one shared wall-clock
  deadline (`PROBE_DEADLINE = 20s`, above the 10s per-probe timeout) with an
  8-way concurrency cap; an overrun collapses to `Unreachable`.
  `probe_finding(name, &ProbeOutcome) -> Finding` is THE shared
  outcome->finding seam (also called by `doctor`'s probe section):
  `outcome_status` maps `Reachable`/`Skipped` -> `Pass`,
  `AuthFailed`/`Unreachable` -> `Fail`, `UnsupportedFreeProbe` (and any future
  variant) -> `Warn`; the remediation text lives here (attached once for both
  callers), never baked into the outcome reason. Results sort by name for
  deterministic output + exit code. `run` degrades a config-load failure to
  defaults, errors on an unknown `<name>`, renders human or `--json`
  (`{schema_version, providers:[{name, outcome}]}`, `SCHEMA_VERSION = 1`,
  UNSTABLE pre-1.0), and returns `overall_exit`
- `src/commands/probe/capture.rs` -- hidden `routectl provider
  capture-envelope` (feature `bedrock`): env-gated Bedrock envelope-capture
  harness, CLI-only, never reachable from the serving listener. Two pre-IO
  gates: `ROUTECTL_BEDROCK_ENVELOPE_CAPTURE=1` (`capture_enabled`) and exactly
  one explicit `--provider`/`--alias` target (`require_scoped`). Builds three
  Invoke-shape canaries (`CanaryKind::{UnknownBeta, UnknownBodyField,
  AdvisorTool}`), resolves the target (via the shared
  `probe::resolve::resolve_provider_and_model`) to a Bedrock Invoke provider,
  signs+sends each via the providers-crate `signing`/`endpoint` seams, and
  `classify_validation` hard-fails unless each is HTTP 400 with a flat AWS
  `ValidationException` (`{"__type","message"}`). Before persisting,
  `assert_no_credential_echo` scans each body for the request's own credential
  material (`configured_secret_material` raw key id/secret/token/bearer key +
  `signed_header_secrets` Authorization / `x-amz-security-token`) and
  hard-fails naming the unwritten file if an endpoint echoed any back -- never
  logging the value. On full success `write_bodies` persists the byte-exact
  raw response bodies to the operator `--out` directory; writes nothing else
  (config/catalog/usage DB/breaker state untouched)
- `src/commands/probe/resolve.rs` -- shared config resolution for the CLI
  probe surfaces. `resolve_probe_target(config, provider, alias) ->
  ResolvedProbeTarget { state_key, provider, model_id }` maps a scoped
  `--alias` (a `[models]` nickname, which is itself the routing `state_key`)
  or a bare `--provider` (resolved from the single selectable model
  referencing it; errors on zero or many) to the routing state key plus the
  provider name and upstream model id -- the `state_key` is the `[models]`
  nickname the learned-capability ledger keys on, so a capability probe emits
  on the SAME lane live traffic would. `resolve_provider_and_model(config,
  provider, alias) -> (provider, model_id)` is the thinner pair view consumed
  by the envelope-capture harness
- `src/commands/probe/capabilities.rs` -- `routectl probe --capabilities`: a
  LIB-SHAPED core (`run_capability_probe`) plus a thin CLI wrapper (`run`).
  The core takes only a `CanaryDispatch` seam (impl'd for `Arc<dyn Provider>`,
  forwarding to a BARE `Provider::complete` -- never `Router`, the structural
  isolation boundary; drivable with a fake in tests) plus a resolved
  `CapabilityProbePlan { state_key, provider_kind, model, catalog_version,
  overlay_revision, rates }`, and returns a `CapabilityProbeReport { estimate,
  cells, events }` WITHOUT writing -- dispatch/classification is decoupled
  from persistence, the wrapper writes `events` synchronously via
  `routectl_usage::insert_capability_event` on an `open_rw` connection.
  `ProbeCapability::{StructuredOutput, WebSearch, PromptCaching, Thinking}`
  (`ALL`, fixed probe order) drives per-cell canary dispatch; each success
  classifies through the SAME `routectl_router::detect` path (scoped to the
  cell's capability), each failure through `classify` + the shared
  `resolve_requested_capability` matcher. Emission per outcome: Verified ->
  `cleared(probe)` THEN `verified(probe)` with the cleared event stamped
  strictly earlier (resurrect-proof replay ordering); SuspectAbsence ->
  `suspect(probe)` (F3); a deterministic capability-naming rejection ->
  `broken(probe)` at F1/F2; a clean-stop-gate reject or any
  transport/availability failure -> NO event. Every event stamps
  `(CATALOG_VERSION, catalog_overlay::overlay_revision(overlay))` computed
  exactly as the serve/reload boot boundary does, so `should_replay` never
  silently drops them. Within-run lane health: a 429/timeout/5xx/auth/network
  failure marks the lane unhealthy and the remaining cells render "skipped:
  lane unhealthy"; a capability-level 400 is evidence, not lane health, and
  never trips the lane. `estimate_probe_cost` (always computed pre-dispatch,
  `total_calls * PROBE_PROFILE_V1.max_tokens` priced against the model's
  `Rates`) gates the run behind an operator confirmation (unless `--yes`).
  `run` and the wizard's `offer_scoped_probe` (the post-`provider add`/`init`
  hook, scoped to one provider, caller-supplied confirm) share the
  `dispatch_and_persist` tail (build bare provider + open ledger + run core +
  persist + render)
- `src/commands/probe/canary.rs` -- capability-probe canary builders + the
  baked `PROBE_PROFILE_V1` ceiling. Four pure builders each author one minimal
  `ChatRequest` and derive the matching `DetectorContext` (struct literal) so
  a canary response classifies through the SAME `routectl_router::detect` path
  a live response does: `structured_output_canary` (strict `json_schema`, one
  required top-level key), `web_search_canary` (single search forced via
  `tool_choice`), `prompt_caching_canary` (ordered `CachingCanary { prime,
  read }` sharing one `cache_control`-marked prefix), `thinking_canary`
  (extended reasoning at the minimum budget). `PROBE_PROFILE_V1` is THE single
  const carrying the baked `max_tokens` ceiling and per-capability canary
  counts (prompt_caching = 2, others = 1); no flag/env/config path raises
  them, and exact-value unit tests pin every field
- `src/commands/doctor/mod.rs` -- `routectl doctor` entry (`run`) + report
  assembly. Read-only posture: a doctor run mutates nothing
  (config/credentials/overlay/usage DB byte-identical). The aggregator is a
  FIXED ordered `SECTIONS` sequence of pure `fn(&DoctorContext) ->
  Vec<Finding>` producers (`inventory`, `version`, `config`, `auth`,
  `secrets`, `probe`, `capability`, `freshness`) -- one extension point per
  new section (producer + `section_title`); `NO_NETWORK_SECTIONS` is that list
  MINUS `probe` for the offline status surface. `DoctorContext` (plus the
  per-layer `CapabilityInputs`/`CapabilityConfig`/`PriorCell` types and the
  `CapabilityMatrixSource` availability tri-state -- `Available { entries,
  now, now_ms } | Empty | Unavailable(class)`, consumed by the capability
  matrix panel, plus the `FreshnessInputs` bundle -- baked catalog stamp,
  freshest overlay `verified_at`, staleness hint, pinned `today_epoch_day`,
  last successful import, plus a reserved `Option<ImportResult>` for the
  future durable import outcome that renders nothing today) holds every
  read-only input, gathered once. `build_report`/`build_report_no_network` run
  the producers over a context, flatten, and deterministically sort (section,
  name, status) via `build_report_over` into `DoctorReport { schema_version =
  4 (UNSTABLE pre-1.0; any structural/semantic change, incl. additive, bumps
  it -- v4 supersedes the capability override/prior/learned findings with the
  `capability_matrix` panel and adds the freshness section), findings, panels
  }`; `run` renders human or `--json` and returns `overall_exit`. Data
  collection lives in `gather.rs`, the section producers in `sections.rs`, the
  capability matrix panel builder in `matrix.rs`, human rendering in
  `render.rs`
- `src/commands/doctor/matrix.rs` -- capability matrix panel builder:
  `build_capability_matrix_panel(&DoctorContext) -> CapabilityMatrixPanel`.
  Merges the three capability signal layers onto CONFIG MODEL NICKNAMES as the
  lane identity -- learned entries join on `state_key == nickname`, priors on
  nickname, overrides consulted per lane for BOTH `provider:nickname` and bare
  `provider` specs (model beats provider, via `OverrideRegistry::resolve`); a
  learned state key with no config entry renders as an extra `routed: false`
  lane rather than being dropped. Columns are the five well-known keys then
  observed others capped at 10 (`(+N more)` overflow). Each cell runs the
  shared `resolve_display_verdict`, then layers on the display-only age (`now
  - last_seen` for a learned/verified cell) and stale flag (a verified cell
  past the operator staleness hint, or a prior stamp past the same threshold
  via `is_stale_days`)
- `src/commands/doctor/gather.rs` -- doctor data collection.
  `gather_context_no_network` is the SINGLE shared gather body (per-layer
  `server::parse_config_only` + `server::load_overlay_default` so the
  capability panel degrades one layer without the other, raw-bytes read for
  the version preflight that never stamps the file, auth, secret checks,
  orphan scan, would-trim panel, and the freshness inputs); `gather_context` =
  that body PLUS one `gather_probe_results` `probe_all` pass (the only
  `CompositeStore` dial), so the two entry points cannot drift. `gather_auth`
  takes read-only oauth probes + seats; `sanitize_store_open_error` reduces a
  store-open failure to a path-free class message. Secret presence:
  `gather_secret_checks`/`classify_secret_ref`/`classify_file`/`scheme_label`
  (SCHEME label only, never a value/path/env var) -> discriminant-only
  `SecretCheck`/`SecretPresence`.
  `gather_orphan_secrets`/`referenced_secret_files` is a read-only
  managed-secret-dir vs `file://`-refs diff (never deletes).
  `build_capability_inputs` resolves the config-derived capability inputs
  (legacy keys + `derive_prior_cells` -- one prior per model whose
  `EffectiveRow::Present` carries capability data, retaining its `verified_at`
  so the matrix panel flags staleness; NO stale-filter at derivation); a
  config parse error -> a redacted "panel unavailable" (via
  `parse_error_redaction`), an unreadable overlay -> priors absent while the
  matrix panel's learned + override cells still render.
  `gather_capability_matrix` builds the `CapabilityMatrixSource` tri-state: an
  unparseable config -> `Unavailable("config_unavailable")`, else it resolves
  this run's replay boundary (baked `CATALOG_VERSION` + the loaded overlay
  revision) via `server::ledger_reader::classify_boundary` and either rebuilds
  a bare, config-sized `LearnedCapabilityRegistry` through
  `rebuild_capabilities_into` + `snapshot` (`Available`, or honest `Empty` on
  a matched-but-zero-row slice) or reports `Unavailable(class)` --
  honest-empty ONLY on a readable, revision-matched, zero-row ledger, never a
  silent empty. Read-only throughout (usage DB byte-identical).
  `freshest_overlay_verified_at` walks the same `derive_effective_view` path
  for the freshest OVERLAY-sourced (import/user, never baked) `verified_at`;
  the freshness inputs also pin `today_epoch_day` and load the last successful
  import via `load_last_import`
- `src/commands/doctor/sections.rs` -- the fixed `section_*` producers plus
  their `*_finding`/`*_remediation` helpers, each a pure mapping of the
  gathered `DoctorContext` to findings: inventory via `compute_activation`
  (route-referenced-but-unusable -> WARN + login remediation), version via
  `preflight_config_version` (too-old -> FAIL + `config migrate`, too-new ->
  FAIL + upgrade, present-but-broken -> FAIL not all-PASS), config via the
  shared `validation_report` (short-circuited to one `Warn` "validation
  skipped" + the secret checks when the typed load failed, so a broken file
  never emits a spurious validation `Pass`) + the leak-safe secret-presence
  scan, auth (no seats/expired -> WARN, store-open error -> FAIL), secrets
  (orphan managed file -> WARN, never auto-deleted), probe via the shared
  `probe_finding` seam, and `capability` -- reduced to the config-unavailable
  degradation line plus the guarded legacy-key migrate nudge (the override /
  prior / learned cells are now on the capability matrix panel), every finding
  `Pass`/`Warn` so it NEVER flips the exit code.
  `section_freshness`/`freshness_findings` map the `FreshnessInputs` to three
  findings-shaped rows (baked catalog version + snapshot date; overlay
  verification age via `epoch_day_age`/`is_stale_days` past the staleness
  hint, honest "no overlay verified stamp" when running on baked defaults;
  last SUCCESSFUL import date/age/counts, honest "no successful import
  recorded" when the sidecar is missing), all `Pass`/`Warn`, NEVER `Fail`.
  NEVER auto-fixes: every WARN/FAIL names the fix
- `src/commands/doctor/render.rs` -- human-text rendering of the
  `DoctorReport`: `render_human` walks `SECTIONS` in render order (per-section
  battery via `render_section`/`section_title`/`status_label`, then the
  capability matrix panel block and the would-trim panel block, then a
  `summary: PASS n WARN n FAIL n` line via `render_summary`)
- `src/commands/doctor_panels.rs` -- doctor's read-only would-trim opportunity
  panel, kept apart from the aggregator so the compute/map/render seam is
  unit-testable against a temp DB. `compute_would_trim_panel(config, now) ->
  Option<WouldTrimPanel>` opens the usage DB via `open_readonly` and
  summarizes the all-time window (best-effort: a missing/unmigrated DB or any
  read failure -> `None`, never fails the surrounding diagnostic; a present DB
  with zero candidates -> a `Some` all-zero panel); `now` is injected for
  deterministic windowing. `panel_from_summary` maps the usage-crate summary
  field-for-field onto the router-side `WouldTrimPanel`;
  `render_would_trim_panel` renders the human block (an all-zero panel -> a
  single "no opportunity" advisory line; otherwise counts +
  `met/unmet/cold/unpriced` verdict split + a `prompt-size --steady-state`
  inspect hint). A test pins that `docs/CONFIGURATION.md` keeps the
  `--steady-state` flag and every `would_trim_*`/verdict field documented
  (`include_str!` also makes a moved docs file a compile error). Also
  `render_capability_matrix_panel` renders the `CapabilityMatrixPanel` as a
  lane-by-capability grid: a distinct honest state line for the learned
  availability tri-state (Available / Empty / Unavailable(code)),
  `render_table` alignment, compact `verdict[source]` cells with a `(stale)`
  marker, an `(unrouted)` lane tag, and a `(+N more)` column-overflow note
- `src/commands/capability_legacy.rs` -- shared detection of the deprecated
  capability-list keys (`unsupported_features`, `allowed_betas`,
  `allowed_body_fields`) superseded by `[capability.overrides]`.
  `present_legacy_capability_keys(&Config) -> Vec<&'static str>` returns the
  present key NAMES only (never the operator's list VALUES, which can sit next
  to secrets) in a stable order; a key counts as present only when its list is
  non-empty. Consumed by BOTH `server`'s deprecation WARN
  (`warn_deprecated_capability_lists`) and `doctor`'s capability migrate
  nudge, so the two surfaces never diverge on which keys count

### Tests

- `tests/common/mod.rs` -- thin re-export shim of `routectl_core::test_utils`
  (single source of truth for the canonical scenario builders) plus the
  cli-only `replay` harness submodule; the builders are enabled via the
  `test-utils` dev-dependency feature on core
- `tests/server.rs` -- end-to-end axum server tests with wiremock upstreams
- `tests/hot_reload.rs` -- file-watch + SIGHUP hot-reload integration tests;
  boots `serve_on_listener` against a tempdir-rooted config.toml +
  credentials.json and polls for the live `Router` swap
- `tests/commands.rs` -- `test` / `config` / `login` subcommand integration
  tests
- `tests/provider_add.rs` -- integration floor for `provider add`: drives the
  REAL command into a temp v3 config for each credential shape (`env://`,
  managed `file://`, `oauth://`, `forwarded`) and asserts the
  learnings-mandated pair -- `config check` passes AND the provider factory
  builds -- plus a serve-boot `/health` smoke; also the non-interactive
  no-hang guarantee, comment/section-order preservation on a round-trip add,
  and a secret-never-leaks scan across the capture paths. Every config is a
  temp copy with XDG-scoped credential + usage-DB isolation so no live file is
  touched
- `tests/init.rs` -- integration floor for `routectl init`: drives the REAL
  guided-setup command into a temp fresh-machine config via both the wizard (a
  non-interactive `StubInitIo`) and the `--yes` flag path, then asserts the
  learnings-mandated trio (`config check` passes, the provider factory builds,
  the server boots to a live `/health`), plus the idempotence floor
  (byte-identical re-run no-op + a partial-failure re-run that duplicates
  nothing and mints no new secret), the `--scaffold` refusal/output shape, the
  pre-side-effect `--yes` error, the no-hang guarantee, and the forwarded
  end-to-end path. XDG scoped to a fresh tempdir (isolating the managed secret
  store + credentials.json) with an isolated usage DB; env-mutating cases are
  serialized and readiness is polled off `/health`
- `tests/anthropic_ingress.rs` -- `/v1/messages` end-to-end (cache_control
  round-trip, forward-compat, listener auth)
- `tests/responses_ingress.rs` -- `/v1/responses` end-to-end (Responses body
  -> openai-compat upstream -> Responses-shaped completion,
  `previous_response_id` -> 400, `store:true` accepted, listener auth)
- `tests/tool_choice_egress_e2e.rs` -- chained tool_choice path: a flat
  Responses `{type:function,name:X}` through the `/v1/responses` ingress
  reaches the Anthropic egress as `{type:tool,name:X}` and the openai-compat
  egress as `{type:function,function:{name:X}}` (asserts the upstream body)
- `tests/contract_ingress.rs` -- request wire body -> canonical `ChatRequest`
  shape per ingress
- `tests/contract_response_ingress.rs` -- canonical `ChatResponse` ->
  Anthropic wire body via `render_response`
- `tests/contract_stream_ingress.rs` -- canonical chunk sequences -> Anthropic
  SSE events (asserts terminal-event ordering)
- `tests/replay_egress.rs` -- replay-driven egress contract test; walks
  `tests/fixtures/captured/`, drives each captured ingress request through the
  matching egress provider's `normalize_request`, and structurally diffs the
  upstream-bound body against the on-disk `outgoing_request.json` (anthropic /
  openai-compat / openai-responses)
- `tests/replay_ingress.rs` -- replay-driven ingress contract test; walks
  captured fixtures, mounts the upstream response in wiremock, drives egress
  `complete()`, renders the canonical `ChatResponse` via
  `AnthropicIngress::render_response`, and asserts it matches the captured
  egress response structurally (anthropic ingress, non-stream scope)
- `tests/cross_dialect_render.rs` -- pins the per-egress-allowlist contract;
  asserts that a foreign upstream (openai-compat DeepSeek dialect) through
  canonical normalize and Anthropic ingress render does not leak vendor
  envelope keys or a `signature:null` thinking block into the Anthropic-shape
  response
- `tests/cross_ingress_reasoning_roundtrip.rs` -- pins that routectl accepts
  its OWN output across dialects: captures an assistant turn from
  `AnthropicIngress::render_response` and replays it into
  `OpenAiIngress::parse_request` unmodified, both as content blocks and as a
  `reasoning_details` array still spelled in Anthropic vocabulary. Also pins
  the closing lap (re-render to Anthropic blocks with text and signature
  intact) and that the inbound aliases never leak back onto the OpenAI wire
- `tests/e2e_reasoning.rs` -- end-to-end reasoning round-trip across DeepSeek
  / vLLM / Anthropic dialects
- `tests/live_matrix.rs` -- shared harness for the live provider matrix
  (request builders, `run_complete` / `run_stream` / `run_matrix`,
  `sanitize_provider_name`) plus `#[path]` wiring of the per-scenario
  submodules; one test binary, gated by the `live-integration` feature
- `tests/live_matrix/openai_compat.rs` -- openai-compat matrices (OpenRouter /
  opencode-go / NIM)
- `tests/live_matrix/bedrock_invoke.rs` -- Anthropic-on-Bedrock via
  InvokeModel + bearer key, plus ingress-through-bedrock end-to-end rows
- `tests/live_matrix/bedrock_converse.rs` -- Bedrock Converse matrix
  (api_shape = Converse)
- `tests/live_matrix/mantle_anthropic.rs` -- Bedrock mantle lane matrix
  (Anthropic Messages vocabulary over
  `bedrock-mantle.<region>.api.aws/anthropic`, bearer-signed): sync complete,
  stream, and count_tokens on one current Claude model with a bare model id;
  gated on `AWS_BEARER_TOKEN_BEDROCK` (region from `AWS_REGION`, defaults
  `us-east-1`)
- `tests/live_matrix/mantle_responses.rs` -- Bedrock mantle lane matrix
  (OpenAI Responses vocabulary over
  `bedrock-mantle.<region>.api.aws/openai/v1`, bearer-signed): complete +
  stream on a bare gpt-oss model id; same env gating as `mantle_anthropic.rs`
- `tests/live_matrix/mantle_chat_completions.rs` -- Bedrock mantle lane matrix
  (OpenAI Chat Completions vocabulary over
  `bedrock-mantle.<region>.api.aws/openai/v1`, bearer-signed): complete +
  stream on a bare gpt-oss model id; same env gating as `mantle_anthropic.rs`
- `tests/live_matrix/openai_responses.rs` -- OpenAI Responses matrix against
  the chatgpt-oauth Codex endpoint
- `tests/live_matrix/gemini.rs` -- native Google Gemini matrix (x-goog-api-key
  header)
- `tests/live_matrix/oauth_codex.rs` -- OpenAI Responses via routectl-managed
  `oauth://codex` bearer source
- `tests/live_matrix/responses_ingress_live.rs` -- `POST /v1/responses`
  ingress over HTTP via the real axum server
- `tests/live_matrix/oauth_antigravity.rs` -- Cloud Code Gemini via
  routectl-managed `oauth://antigravity` bearer source
- `tests/live_anthropic_oauth.rs` -- live OAuth-bearer test against
  `api.anthropic.com`; gated by env token file
- `tests/anthropic_forward_compat_stream.rs` -- full-pipeline integration
  tests for the Anthropic SSE forward-compat opaque-events fix; hand-crafted
  SSE wire-byte fixtures driven egress -> canonical -> ingress, asserting
  verbatim re-emission of unknown content_block types
