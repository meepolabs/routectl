# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

  Note: Converse adapter is wired but body translation for non-Anthropic
  vendors is staged for v0.4.0; using `api_shape = "converse"` today
  returns a clear "not implemented" error.

- **`extra_headers` and `user_agent` on `AnthropicApiConfig`** and
  `[providers.X]` of type `anthropic-api`. Mirrors the existing fields
  on `OpenAiCompatConfig`. Use `extra_headers` to declare any
  `anthropic-beta` flags (e.g. `context-1m-2025-08-07`,
  `prompt-caching-2024-07-31`). Use `user_agent` to override the
  outbound UA, useful for IAM-gated upstreams whose policy condition
  matches on `aws:UserAgent`.

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
