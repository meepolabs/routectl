# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0]

The big v0.6 release: layered provider + model config, dispatch
hygiene fixes, openai-compat normalization, and a wave of dogfood
fixes from daily live use.

### Added

- **Layered provider + model config**. `[providers.X]` carries
  transport-wide knobs (auth, base URL, runtime gates) and
  `[models.X]` carries per-model behavior (reasoning, dialect,
  quirks). Two fields live on BOTH layers and merge at dispatch
  time -- `header_extras` and `payload_extras` -- with model winning
  on key collision and `anthropic-beta` comma-unioning across all
  sources. The router's `apply_layered_overlays` helper runs the
  merge before calling `provider.complete()` / `provider.stream()`
  so the `Provider` trait surface stays stable across all four
  concrete providers (openai-compat, anthropic-api, bedrock,
  openai-responses).

- **`[models.X]` first-class TOML table**. Required: `provider`
  (key in `[providers.X]`), `upstream` (wire model id). Optional:
  `thinking` (bool or `"adaptive"`), `effort` (enum), `reasoning_dialect`,
  `history_reasoning`, `additional_request_fields`, `anthropic_beta`,
  `stream_first_byte_timeout_ms`, `header_extras`, `payload_extras`,
  `selectable`.

- **Suffix-glob alias keys** in the unified `[aliases]` table:
  `"claude-opus-*" = "heavy"` matches any wire model starting with
  `claude-opus-`. Lookup precedence: exact match > longest matching
  prefix > `default`. Alias values are `String | Vec<String>` --
  single string is a one-entry chain, list is a fallback chain.

- **`Router::new` precomputes** a `BTreeMap<String, Arc<ResolvedModel>>`
  from `[models]`, so dispatch is one O(1) lookup per hop. Unknown
  nicknames in alias chains fail at startup, not at first request.

- **Tracing dispatch events** carry `model = <nickname>` alongside
  `provider = <provider_name>` for per-model triage.

- **`anthropic-beta` HTTP header lifted into canonical
  `req.anthropic_beta`**. The Anthropic TypeScript SDK translates
  the `betas: [...]` SDK option into an `anthropic-beta: a,b,c` HTTP
  header (not a body field); claude-code uses this surface for
  first-party betas (context-management, prompt-cache-1h,
  adaptive-thinking, ...). routectl now lifts the header so the
  egress emits it in the upstream body (Anthropic accepts either
  surface).

- **`POST /v1/messages` openai-responses provider** (default-on
  `openai-responses` Cargo feature). ChatGPT Codex endpoint via
  `chatgpt-oauth` bearer JWT. Stream-only (`complete()` forces
  `stream:true` and drains SSE to `response.completed`). Flat
  Responses-shape tools and `tool_choice`. `instructions` field
  always serialized (the server 400s if absent).

- **Operator-owned Bedrock allowlists** -- `[bedrock] allowed_betas`
  and `[bedrock] allowed_body_fields` in TOML. Filters the body's
  `anthropic_beta` array (Invoke and Converse) and any forward-compat
  body fields the Anthropic ingress sweeps in (`mcp_servers`,
  `diagnostics`, `context_hint`, `speed`, ...). routectl ships no
  built-in default; AWS schema drift is operator-tracked. Empty list
  (or omitted `[bedrock]` section) is pass-through for discovery --
  bring up routectl, observe sent flags via
  `ROUTECTL_LOG=routectl_providers::bedrock=trace`, populate the
  lists. `examples/bedrock.toml` ships the empirical 2026-05-12
  baseline (16 betas + 16 body fields).

- **`history_reasoning` per-provider knob** on `[providers.X]` of
  type `openai-compat`. Three values: `auto` (default; defer to the
  dialect's strip-vs-preserve default), `strip` (required for
  DeepSeek v3 and vLLM <= 0.6), `preserve` (required for DeepSeek
  v4+ and vLLM 0.7+ hosts that 400 on missing echo-back).
  Per-dialect preserve impls: DeepSeek and vLLM render
  `reasoning_content` scalars; OpenRouter renders typed
  `reasoning_details[]`; OpenAI and Passthrough are no-ops.

- **Per-provider and per-model timeout overrides**:
  `request_timeout_ms` and `stream_first_byte_timeout_ms`. Resolution
  priority: per-model > per-provider > global `[retry]`. Eliminates
  alias-level repetition (e.g. NIM cold-start, Opus 4.7
  high-effort).

- **CF extended 5xx range in default `fallback_on_status`**:
  `[408, 429, 500, 502, 503, 504, 520-527, 530]`. Cloudflare-fronted
  upstreams (opencode.ai, openrouter.ai) surface upstream-origin
  failures via 520-527; without these in the default list, a single
  520 would kill a request even when a sibling provider in the chain
  could have served it.

- **`ROUTECTL_TRACE_BODY_BYTES` env var** to override the 16 KB
  TRACE body cap at process start. Set to 1 MB (`1048576`) for
  live-traffic fixture capture; real claude-code requests routinely
  exceed 16 KB. Resolved cap is announced once at server boot.

- **`scripts/capture_fixtures.sh`** -- operator script that drains
  the TRACE log into per-request fixture directories under
  `crates/routectl-cli/tests/fixtures/captured/` (gitignored). Atomic
  writes via `.tmp.<id>.XXXXXX` rename pattern.

- **`docs/PROVIDER-QUIRKS.md`** -- operator-facing config guide.
  Per-model rows for Anthropic Opus 4.7+ (adaptive thinking),
  DeepSeek v4 (echo-back), vLLM 0.7+, NIM (reasoning_effort gate +
  cold-start cushion), Anthropic / Bedrock / OpenRouter / OpenAI.
  Cross-cutting timing notes, multi-host fallback chain examples,
  troubleshooting matrix.

- **`SECURITY.md`** -- vulnerability disclosure policy.

### Fixed

- **Per-model circuit breaker isolation**. Two `[models.X]` rows
  pointing at the same `[providers.X]` now have independent breaker
  counters and RPM buckets. State is keyed by `[models.X]` nickname,
  not by provider name -- a single flaky model no longer trips the
  breaker for every healthy sibling on the same transport.

- **Bedrock SSO probe deduplication** across models on one
  `[providers.X]`. The factory now caches resolved AWS credentials
  per provider name; building 5 Bedrock models on one provider hits
  the credential chain once instead of 5x.

- **Alias chain validation at startup**. `serve` and `routectl test`
  reject `[aliases]` chains pointing at unknown OR
  `selectable = false` `[models.X]` nicknames before the server
  binds, instead of silently returning `UnknownAlias` at first
  request time. Validator accumulates every offending alias/nickname
  pair into one consolidated error.

- **Per-model `header_extras` reaches the wire**. The merged value
  (provider + model + ingress) lands on `req.anthropic_beta` and the
  Anthropic egress emits one comma-unioned `anthropic-beta` HTTP
  header.

- **Anthropic legacy thinking budget clamp**. Drop legacy
  `thinking: Enabled` when `req.max_tokens <= 1024` (Anthropic
  requires `max > budget`, floor `budget >= 1024`); enforce both the
  1024 floor and the `max - 1` ceiling on every Enabled emission
  path. Caught live on probe-sized requests (e.g. title-generation,
  topic-summary, "continue?" prompts with `max_tokens=64`) when the
  operator's per-model config carried `thinking = true effort =
  high`. Bedrock Invoke + Converse share the helper transitively.

- **openai-compat: strip vendor envelope + lift usage sub-bags**.
  Anthropic-shape ingress + openai-compat upstream was bleeding
  envelope fields (`object`, `system_fingerprint`, `cost`) and four
  DeepSeek/OpenAI usage sub-bags (`prompt_cache_hit_tokens`,
  `prompt_cache_miss_tokens`, `prompt_tokens_details`,
  `completion_tokens_details`) back to the Anthropic-shape response.
  Now lifted to canonical `Usage.reasoning_tokens` and
  `Usage.cache_read_input_tokens` and stripped from the extras
  catchall. Mirrored on the SSE path before serde (`UsageDelta` has
  no extras flatten).

- **`tool_choice` shape mismatch: OpenAI bare-string -> Anthropic
  tagged-enum**. Anthropic's Messages API and Bedrock-Invoke reject
  `tool_choice: "auto"` with a 400. The Anthropic-API egress now
  translates `"auto" | "none" | "required"` and the OpenAI
  `{"type":"function","function":{"name":"X"}}` object map to
  Anthropic-shape `{"type":...}`. Anthropic-shape inputs pass
  through unchanged.

- **Top-level `system` leaks onto openai-compat wire**. The OpenAI
  ingress lifts wire `role: "system"` into canonical `req.system`
  (Anthropic-shape top-level field). The openai-compat egress now
  performs the inverse lower: prepends a synthetic
  `role: "system"` message and strips the top-level `system` key.
  Strict hosts (NVIDIA NIM) used to 400 with
  `Validation: Unsupported parameter(s): system`.

- **OpenAI ingress: `reasoning_content` keys coalesced before
  schema deserialization**. DeepSeek-shape `reasoning_content` was
  arriving unmerged on `messages[].reasoning_content`, missing the
  canonical `reasoning` lift on multi-turn echo-back. Added
  pre-deserialization coalescer mirroring the response-side
  `merge_reasoning_keys`.

- **`prompt_tokens` translation: cache_creation/cache_read summed
  into Anthropic streaming usage**. The Anthropic SSE response now
  captures `message_start` input usage, sums `input_tokens +
  cache_creation_input_tokens + cache_read_input_tokens` into the
  closing `message_delta` `UsageDelta`, and exposes per-TTL cache
  breakdown via field-level merge.

- **Stop-sequence end-to-end**. Preserve the matched stop sequence
  through canonical so the Anthropic ingress can emit
  `stop_reason: "stop_sequence"` + `stop_sequence: "<value>"` instead
  of collapsing to `end_turn`. Previously broke claude-code
  structured-output flows (stop_sequence fences the output) by
  flagging `is_error: true` on the result envelope. Bedrock Converse
  is a known follow-up -- AWS surfaces the matched sequence via
  `additionalModelResponseFields` only when the request opts in.

- **Router log clarity: chain-exhausted vs fallback hop**. Both
  `complete()` and `stream()` previously WARNed "fallback to next" on
  every fallbackable terminal error including the LAST chain entry.
  Now emits "chain exhausted; no fallback target available; request
  will fail" when no next target exists.

- **WARN at egress when canonical reasoning is silently stripped**.
  Operator visibility for the `history_reasoning = "auto"` + strip
  dialect case where the request actually carried reasoning.

- **Alias glob double-parse**. `Router::new` parsed each `*`-bearing
  alias key twice; pattern is now reused via let-binding.

- **`routectl test` help text and module docstring** referenced the
  removed `provider:model` direct-target form. Now references the
  alias key / model nickname inputs the v0.6.0 router accepts.

### Security

- **Log redaction at TRACE level**. `ROUTECTL_LOG_REDACT_PROMPTS=1`
  walks every traced body and replaces known prompt-bearing fields
  (text blocks, system, instructions, tool_use input,
  function_call arguments, refusal blocks, image source data,
  image_url data URIs, Bedrock Converse `toolUse.input`) with
  `<redacted len=N>` placeholders while preserving structural
  fields (model, tools, sampling params, finish_reason, usage).
  Read once on first traced body; one-shot startup log line
  reports the resolved value.

- **gitleaks workflow + `.gitleaks.toml`** -- secret scan on every
  PR + push + weekly full-history sweep. Inherits the default rule
  set; allowlists Cargo.lock, target/, and the captured/ fixture
  directory.

- **CI hygiene**: pinned every third-party action to a commit SHA
  with a version comment (floating tags can be retroactively moved
  by an attacker), added `permissions: contents: read` at the
  workflow level.

### Removed (BREAKING)

- `enabled` on `[models.X]` -- renamed to `selectable` to free the
  TOML key for the flattened `ReasoningDefaults::enabled` (reasoning
  on/off). Operators wanting per-model reasoning-off semantics now
  write `enabled = false`; `selectable = false` is the routing-
  disable knob.
- `[aliases.X.retry]` per-alias retry overrides -- removed when
  `[aliases]` collapsed into a flat wire-string -> nickname-or-chain
  map. Use the global `[retry]` table; per-error-class caps
  (`retry_on_429`, `retry_on_5xx`, `retry_on_network`) cover the
  knobs operators previously set per alias.
- `type` field on `[providers.X]` -- renamed to `kind` to disambiguate
  from the `type` Rust keyword.
- `model_id` on `[providers.bedrock-X]` -- moves to
  `[models.X].upstream`. Bedrock providers are no longer 1:1 with a
  model.
- `thinking`, `enabled`, `adaptive_thinking` on `[providers.X]` --
  move to `[models.X]`. Per-provider was the wrong granularity; two
  models on one provider can now carry different reasoning floors.
- `additional_model_request_fields` on `[providers.bedrock-X]` --
  renamed to `additional_request_fields` and moved to `[models.X]`.
- `default_extras` on `[providers.X]` -- moves to `[models.X]`.
- `[ingress.X.aliases]` per-ingress alias maps -- collapsed into the
  unified top-level `[aliases]` table.
- `[aliases.X] chain = [...]` sub-tables -- chains live as list
  values directly in `[aliases]`: `heavy = ["opus", "sonnet"]`.
- top-level `default_model = "..."` -- replaced by `default = "..."`
  inside `[aliases]`.
- `[bedrock] anthropic_beta` -- renamed to `[bedrock] allowed_betas`.

### Deferred

- Per-model `default_extras` and `chat_template_kwargs` deferred
  until the egress wiring lands; they will return as `[models.X]`
  fields in a future release. The provider-side fields
  (`OpenAiCompatConfig::default_extras`, `chat_template_kwargs` on
  the wire) are unaffected -- callers continue to send them
  per-request via `provider_extras`.
- Bedrock Converse stop_sequence round-trip -- AWS surfaces the
  matched sequence only when the request opts into
  `additionalModelResponseFieldPaths`. Tracked as a follow-up.
- OAuth token hot-rotation. routectl reads `ROUTECTL_ANTHROPIC` once
  at startup; a credentials.json rotation by claude-code requires a
  routectl restart. Manual snapshot + restart workflow today;
  inotify-based file-watch is staged for a future release.

### Migration

No automated migration tool; old configs hit raw serde errors at
startup. Hand-edit your TOML against the new shape -- see
`examples/config.toml` for a complete reference.

## [0.4.0] - 2026-04-XX

### Added

- **Native AWS Bedrock provider** (`type = "bedrock"`). Speaks SigV4
  directly to `bedrock-runtime.<region>.amazonaws.com`, with both
  `InvokeModel` (per-vendor body shape, default) and `Converse`
  (vendor-neutral envelope) request paths selectable via
  `api_shape = "invoke" | "converse"`. Streaming responses are
  decoded from the AWS eventstream binary frame format and re-emitted
  as routectl `ChatChunk`s; in-stream Anthropic `error` events
  (`overloaded_error`, `rate_limit_error`, etc.) surface as `Error::Upstream`
  with mapped HTTP status codes rather than silently truncating.

  Credentials resolve via four mutually exclusive `creds.kind` shapes:

  - `bearer-key` -- short-term Bedrock API key from the AWS console.
    Skips SigV4 entirely and sends `Authorization: Bearer <key>`.
  - `static` -- raw `access_key_ref` / `secret_key_ref` /
    optional `session_token_ref`, each via routectl `SecretRef` URIs.
  - `profile` -- a named profile in `~/.aws/credentials`, with SSO
    auto-refresh via `aws-config`.
  - `default-chain` -- standard AWS provider chain (env -> profile ->
    SSO -> web identity / IRSA -> EC2/ECS metadata).

  Gated behind a `bedrock` Cargo feature (default on for the binary;
  library consumers can opt out with `--no-default-features` to skip
  the `aws-config` / `aws-sigv4` / `aws-smithy-eventstream` dep tree).

  Per-provider `user_agent` override is supported and recommended for
  IAM policies that gate access via the `aws:UserAgent` condition key.
  Per-provider `anthropic_beta` flags route into the request body's
  top-level `anthropic_beta` array (Invoke) or
  `additionalModelRequestFields.anthropic_beta` (Converse).
  `additional_model_request_fields` is a free-form merge point for
  vendor-specific knobs.

  Note: For Anthropic models, both `Invoke` and `Converse` adapters
  are wired and live-tested. Converse for non-Anthropic Bedrock
  vendors (Mistral, Llama, Cohere) is staged for a later cut.

- **`POST /v1/messages` Anthropic ingress**, full tool-call
  round-trip, thinking blocks + signature preservation, typed SSE
  events (`message_start` / `content_block_*` / `message_delta` /
  `message_stop`), server-side model-id -> alias mapping
  (`[ingress.anthropic.aliases]`), and `x-routectl-alias` header
  override. Two ingress dialects (OpenAI + Anthropic) feeding one
  canonical request shape; any client speaking either wire format
  routes through any backend.

- **Canonical internal shape** absorbs Anthropic features
  losslessly: typed `ContentPart` (Text / Image / ImageUrl /
  Document / ToolUse / ToolResult / Thinking / RedactedThinking /
  Other), typed `SystemContent` (Text or Blocks), typed `ToolDef`
  (Custom / Other), top-level `cache_control` and `anthropic_beta`,
  and `Usage` cache stats. Forward-compat catchalls
  (`ContentPart::Other`, `ToolDef::Other`, `ContentBlock::Other`)
  pass unknown Anthropic block types through verbatim on the
  all-Anthropic path. `cache_control::validate` enforces the
  4-breakpoint cap and 1h-before-5m TTL ordering at ingress.

- **Listener-side auth** via static config tokens (`[server.auth]
  tokens = [...]`) accepts both `x-api-key` and `Authorization:
  Bearer`. Inbound auth is fully decoupled from upstream credentials
  (no bridging, no token storage).

- **`strict_translation`** server flag. Default `false` emits
  `tracing::warn!` on lossy seams (cache_control dropped on
  openai-compat egress, ContentPart::Other forward-compat blocks on
  egresses that don't carry them, Anthropic builtin tools dropped).
  `[server] strict_translation = true` upgrades all of these to a
  400 Bad Request, rejecting the request before it hits upstream.

- **Adaptive thinking** for Anthropic Opus 4.7+. Per-provider
  `adaptive_thinking = true` on `[providers.X]` of type
  `anthropic-api` or `bedrock` rewrites the request to the new
  `thinking: {type: "adaptive"}` + `output_config: {effort: "..."}`
  shape; budget is no longer caller-provided.

- **`extra_headers` and `user_agent` on `AnthropicApiConfig`** and
  `[providers.X]` of type `anthropic-api`. Mirrors the existing fields
  on `OpenAiCompatConfig`. Use `extra_headers` to declare any
  `anthropic-beta` flags (e.g. `context-1m-2025-08-07`,
  `prompt-caching-2024-07-31`). Use `user_agent` to override the
  outbound UA, useful for IAM-gated upstreams whose policy condition
  matches on `aws:UserAgent`.

- **Universal 4xx/5xx self-diagnosing logging**. Outgoing request
  body at `tracing::trace!`, ingress body at `tracing::trace!`,
  full upstream error body at `tracing::debug!` (cap 4 KB) on every
  4xx/5xx from any provider. Request-id correlation across the chain
  so `grep request_id=<id>` shows ingress -> egress -> upstream
  response in one shot.

### Security

- **`extra_headers` cannot override auth-bearing headers**. TOML-supplied
  `extra_headers` entries that case-insensitively match `authorization`,
  `x-api-key`, or `host` are now ignored with a `tracing::warn!`
  instead of silently overwriting the provider's auth header. This
  applies to both `anthropic-api` and `bedrock` providers.
- **`BedrockCreds` redacts secret material in `Debug` output**.
  `secret_access_key`, `session_token`, and bearer keys never appear
  in `tracing` events, panic messages, or test failures.
  `access_key_id` is shown as a 4-character prefix so operators can
  identify the active key. `BedrockConfig` is safe by transitivity.
- **Eventstream parser caps single-frame size at 8 MB**. Defends
  against a malicious or compromised upstream advertising a giant
  `total_length` to drive the inbound buffer toward OOM. Real Bedrock
  chunks are KB-scale.

### Changed

- **BREAKING (config-level)**: `auth_kind = "oauth-bearer"` no longer
  auto-injects `anthropic-beta: oauth-2025-04-20`. Beta flags are now
  declared explicitly in `extra_headers`, decoupling auth method from
  capability gates. This unblocks API-key-auth users from setting
  `context-1m-*`, `prompt-caching-*`, and `extended-thinking-*` gates
  via the same channel.

  Migration -- if you used `auth_kind = "oauth-bearer"`, add to your
  TOML:
  ```toml
  [providers.<your-anthropic-provider>.extra_headers]
  "anthropic-beta" = "oauth-2025-04-20"
  ```
  Or, if you want the OAuth gate alongside other beta flags, comma-join:
  ```toml
  "anthropic-beta" = "oauth-2025-04-20,context-1m-2025-08-07"
  ```

### Fixed

- **Bedrock eventstream prelude drain on `Incomplete`**. Multi-chunk
  HTTP body responses (any long Opus stream) hit `InvalidUtf8String`
  mid-stream because the decoder consumed the 12-byte prelude into
  state but the caller didn't drain the cursor on `Incomplete`, so
  the next iteration reread the prelude bytes as headers. Fixed by
  draining `cursor.position()` from the buffer before breaking.

## [0.2.0] - 2026-05-06

### Added

- **Tier-1 retry/timeout policy**: per-error-class retry caps
  (`retry_on_429`, `retry_on_5xx`, `retry_on_network`), `request_timeout_ms`
  per attempt, `stream_first_byte_timeout_ms`, jitter on backoff.
- **Tier-2 routing gates**: per-provider `rpm_limit` (token-bucket), passive
  circuit breaker (`circuit_failures` + `circuit_cooldown_ms`),
  per-request `x-routectl-disable-fallbacks` header.
- **Anthropic OAuth bearer auth**: `auth_kind = "oauth-bearer"` on
  `[providers.X]` of type `anthropic-api`. Sends
  `Authorization: Bearer ...` plus `anthropic-beta: oauth-2025-04-20`
  against the same `/v1/messages` endpoint, for callers that prefer
  that wire format over an `x-api-key` header.
- **Opinionated alias groups** in `examples/config.toml`:
  `heavy`, `med`, `cheap`, `local`, `reasoning`.
- **`ModelProfile` registry** for per-model quirks
  (`drops_sampling_params`, `requires_reasoning_effort`,
  `supports_adaptive_thinking`, `uses_chat_template_kwargs`).
- **`Dialect` trait** and `openai_compat/dialects/` per-dialect modules
  routing request/response/SSE through static dispatch.
- **`file://` SecretRef variant**: TOCTOU-safe (open-once + fd-based
  `fstat`), refuses non-regular files, refuses world/group-readable
  files, requires absolute paths.

### Changed

- **Public config types are `#[non_exhaustive]`**: `ProviderEntry`
  (enum + every variant), `ProviderRuntimePolicy`, `AliasEntry`,
  `RetryPolicy`, `RouterOptions`. External callers construct via the
  per-variant `ProviderEntry::*` factories and the chainable
  `with_runtime` / `with_extra_headers` / `with_default_extras` /
  `with_reasoning_dialect` / `with_base_url` / `with_anthropic_version`
  / `with_organization_id` / `with_auth_kind` setters. Variant-specific
  setters panic on wrong-variant misuse rather than silently dropping
  values.
- **Stream cancellation** now distinguishes half-open probe drop
  (records failure, releases the in-flight slot, re-trips circuit)
  from steady-state drop (records success, healthy provider doesn't
  flap on client cancel).
- **Half-open circuit breaker is single-probe under concurrent load**.
  An explicit `half_open_in_flight` flag gates concurrent requests so
  exactly one probe runs at a time after cooldown.
- **Per-attempt gate accounting**: RPM and breaker now debit on every
  upstream call (was per-request), so retries can't bypass the
  per-provider rate limit.
- **`ProviderEntry::redact_secrets()` and `secret_uris()`** are
  exhaustive methods on the type. The CLI delegates rather than
  matching on the variants itself, so any future variant fails to
  compile until redaction is wired up (closes the silent-secret-leak
  footgun).

### Removed

- **OS keychain support** is gone. `SecretRef` is now `env://`,
  `file://`, or `literal:` only. Rationale: routectl is not a
  credential-discovery tool and we don't want the keychain-permission
  prompt giving the wrong impression.

### Fixed

- `file://` reads no longer have a TOCTOU window between permission
  check and read; the open file descriptor is `fstat`-ed and read
  from in one go.
- `file://` URIs reject non-absolute paths instead of resolving them
  cwd-dependently.
- Stream-mid-failure now charges the circuit breaker; previously a
  provider that consistently emitted one byte and died would never be
  quarantined.
- Drop-time mutex poisoning in the stream breaker is recovered via
  `into_inner()` instead of panicking, so cancellation cleanup stays
  non-aborting.

## [0.1.0] - 2026-05-06

Initial release.

- Single binary, OpenAI-compatible HTTP server bound to `127.0.0.1`
  by default.
- Two provider classes: `openai-compat` (5 reasoning dialects:
  `openai`, `deepseek`, `vllm`, `raw-think-tag`, `openrouter`,
  `passthrough`) and `anthropic-api` (api-key auth; `thinking` blocks
  with `signature` preserved across multi-turn tool use).
- Reasoning normalization to OpenRouter-shape `reasoning_details[]`
  with provider-tagged `format`.
- Streaming SSE both directions, including stateful `<think>` tag
  handling for tags split across chunk boundaries.
- Fallback chain on 408/429/5xx/timeout (no fallback once first chunk
  has streamed).
- Per-provider retry with exponential backoff.
- TOML config in `~/.config/routectl/config.toml`.

[0.6.0]: https://github.com/meepolabs/routectl/compare/v0.4.0...v0.6.0
[0.4.0]: https://github.com/meepolabs/routectl/compare/v0.2.0...v0.4.0
[0.2.0]: https://github.com/meepolabs/routectl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/meepolabs/routectl/releases/tag/v0.1.0
