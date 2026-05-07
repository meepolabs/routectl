# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  `RetryPolicy`, `RouterOptions`. External callers use the new
  `ProviderEntry::{openai_compat, anthropic_api, claude_cookie,
  chatgpt_cookie}` constructors plus chainable
  `with_runtime/with_extra_headers/with_default_extras/with_reasoning_dialect/with_base_url/with_anthropic_version/with_organization_id/with_auth_kind`
  setters. Variant-specific setters panic on wrong-variant misuse
  rather than silently dropping values.
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
