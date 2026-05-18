# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0-rc.1]

### Removed (BREAKING)

- `type` field on `[providers.X]` -- renamed to `kind` to disambiguate
  from the `type` Rust keyword and match `BedrockCredsConfig.kind`.
- `model_id` on `[providers.bedrock-X]` -- moves to
  `[models.X].upstream`. Bedrock providers are no longer 1:1 with a
  model; the factory builds one provider instance per `[models.X]`
  row that points at a Bedrock provider, so each model gets its own
  circuit breaker.
- `thinking`, `enabled`, `adaptive_thinking` on `[providers.X]` --
  move to `[models.X]`. Per-provider was the wrong granularity; two
  models on one provider can now carry different reasoning floors.
- `additional_model_request_fields` on `[providers.bedrock-X]` --
  renamed to `additional_request_fields` and moved to `[models.X]`.
- `default_extras` on `[providers.X]` -- moves to `[models.X]`.
- `[ingress.X.aliases]` per-ingress alias maps -- collapsed into the
  unified top-level `[aliases]` table. The wire-string keyspaces
  don't collide in practice (claude-* vs gpt-* vs deepseek-*).
- `[aliases.X] chain = [...]` sub-tables -- chains live as list
  values directly in `[aliases]`: `heavy = ["opus", "sonnet"]`.
- top-level `default_model = "..."` -- replaced by `default = "..."`
  inside `[aliases]`.

### Added

- `[models.X]` first-class TOML table. Required fields: `provider`
  (key in `[providers.X]`), `upstream` (wire model id). Optional:
  `thinking`, `enabled`, `adaptive_thinking`, `additional_request_fields`,
  `chat_template_kwargs`, `default_extras`.
- Suffix-glob alias keys: `"claude-opus-*" = "opus-heavy"` matches
  any wire model starting with `claude-opus-`. Lookup precedence
  is exact match > longest matching prefix > `default`.
- Alias values are `String | Vec<String>` (untagged enum). Single
  string is a one-entry chain; list is a fallback chain.
- `Router::new` precomputes a `BTreeMap<String, Arc<ResolvedModel>>`
  from the `[models]` table, so dispatch is one O(1) lookup per
  hop and unknown nicknames in alias chains fail at startup, not
  at first request.
- Tracing dispatch events now carry `model = <nickname>` alongside
  `provider = <provider_name>` for per-model triage.

### Migration

No automated migration tool; old configs hit raw serde errors at
startup. Hand-edit your TOML against the new shape -- see
`examples/config.toml` for a complete reference.

## [Unreleased]

### Added

- **Operator-owned Bedrock allowlists** -- `[bedrock] allowed_betas`
  and `[bedrock] allowed_body_fields` in TOML. Filters the body's
  `anthropic_beta` array (Invoke and Converse) and any forward-compat
  body fields the Anthropic ingress sweeps in (`mcp_servers`,
  `diagnostics`, `context_hint`, `speed`, ...). routectl ships no
  built-in default; AWS schema drift is operator-tracked. Empty list
  (or omitted `[bedrock]` section) puts the corresponding filter in
  pass-through mode for discovery -- bring up routectl, observe sent
  fields/flags via `ROUTECTL_LOG=routectl_providers::bedrock=trace`,
  populate the lists. `examples/bedrock.toml` ships the empirical
  2026-05-12 baseline (16 betas + 16 body fields). Closes
  `issues.md::INV-6` and `INV-7`.

  BREAKING: `[bedrock] anthropic_beta` is renamed to
  `[bedrock] allowed_betas`. Configs using the old name need a rename.

- **`history_reasoning` per-provider TOML knob** on `[providers.X]` of
  type `openai-compat`. Three values: `auto` (default; defer to the
  reasoning dialect's strip vs preserve default), `strip` (always
  strip canonical reasoning fields from outgoing assistant history --
  required for DeepSeek v3 and vLLM <= 0.6 hosts that 400 on
  echo-back), `preserve` (echo canonical reasoning back to the
  upstream in the dialect-native shape -- required for DeepSeek v4+
  and vLLM 0.7+ hosts that 400 on missing echo-back). Per-dialect
  preserve impls: DeepSeek and vLLM render `reasoning_content`
  scalars; OpenRouter renders typed `reasoning_details[]`; OpenAI and
  Passthrough are no-ops (no preserve shape on the wire).

- **Per-provider timeout overrides** on `[providers.X]` of type
  `openai-compat`, `anthropic-api`, and `bedrock`:
  `request_timeout_ms` and `stream_first_byte_timeout_ms`. Resolution
  priority is `[aliases.X.retry] > [providers.X] > [retry]`. Eliminates
  alias-level repetition when an entire upstream is uniformly slow
  (e.g. NIM cold-start, Opus 4.7 high-effort).

- **`docs/PROVIDER-QUIRKS.md`** -- operator-facing config guide.
  Per-model rows for Anthropic Opus 4.7+ (adaptive thinking),
  DeepSeek v4 (echo-back), vLLM 0.7+, NIM (reasoning_effort gate +
  cold-start cushion), Anthropic / Bedrock / OpenRouter / OpenAI.
  Cross-cutting timing notes, multi-host fallback chain examples,
  troubleshooting matrix.

### Fixed

- **`tool_choice` shape mismatch: OpenAI bare-string -> Anthropic
  tagged-enum**. Anthropic's Messages API and Bedrock-Invoke reject
  `tool_choice: "auto"` with a 400 (the validator names the field on
  Opus 4.7+ but the Bedrock generic 400 is opaque). The Anthropic-API
  egress now translates: `"auto" | "none" | "required"` and the
  OpenAI `{"type":"function","function":{"name":"X"}}` object map to
  Anthropic-shape `{"type":...}`. Anthropic-shape inputs pass through
  unchanged.

- **Top-level `system` leaks onto openai-compat wire**. The OpenAI
  ingress lifts wire `role: "system"` into canonical `req.system`
  (Anthropic-shape top-level field). The openai-compat egress now
  performs the inverse lower: prepends a synthetic
  `role: "system"` message to the messages array and strips the
  top-level `system` key. Lenient hosts (OpenAI, OpenRouter,
  opencode-go) silently ignored the unknown field; strict hosts
  (NVIDIA NIM) 400'd with `Validation: Unsupported parameter(s):
  system`.

- **OpenAI ingress: `reasoning_content` keys not coalesced before
  schema deserialization**. DeepSeek-shape `reasoning_content` arrived
  unmerged on `messages[].reasoning_content`, missing the canonical
  `reasoning` lift on multi-turn echo-back. Added pre-deserialization
  coalescer in
  `crates/routectl-cli/src/ingress/openai.rs::coalesce_message_reasoning_keys`
  that mirrors the response-side `merge_reasoning_keys`.

- **`prompt_tokens` translation: cache_creation/cache_read not summed
  into Anthropic streaming usage**. The Anthropic SSE response now
  captures `message_start` input usage, sums `input_tokens +
  cache_creation_input_tokens + cache_read_input_tokens` into the
  closing `message_delta` `UsageDelta`, and exposes per-TTL cache
  breakdown via field-level merge. Closes the gap where streaming
  responses underreported `prompt_tokens` by the cache contribution.

- **WARN at egress when canonical reasoning is silently stripped**.
  Operator visibility for the case where `history_reasoning = "auto"`
  resolves to strip but the request actually carried reasoning -- the
  config choice is logged so DeepSeek-v4-style upstreams 400ing on
  missing echo-back are diagnosable from the routectl side without
  enabling trace-level body logging.

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

[Unreleased]: https://github.com/meepolabs/routectl/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/meepolabs/routectl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/meepolabs/routectl/releases/tag/v0.1.0
