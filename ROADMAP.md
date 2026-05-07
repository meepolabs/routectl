# routectl roadmap

## v0.1.0 (DONE -- 2026-05-06)

Goal: a binary you can `cargo install`, point clients at, and have it
route real LLM calls with fallback + reasoning normalization. Cookie
auth providers scaffolded but feature-gated.

128 tests pass across the workspace. Release binary 5.6MB stripped.

### Implementation status

1. **`routectl-core`** trait + schema -- DONE
2. **`routectl-providers::openai_compat`** -- DONE (5 dialects: openai, deepseek, vllm, raw-think-tag, openrouter, passthrough; SSE state machine with stateful `<think>` handling)
3. **`routectl-providers::anthropic_api`** -- DONE (signature preservation across multi-turn + streaming)
4. **`routectl-router`** -- DONE (alias resolver + fallback walker + retry with exponential backoff + provider factory)
5. **`routectl-auth`** -- DONE (`SecretStore` trait, `MemoryStore` resolving env / file / literal)
6. **`routectl-cli::serve`** -- DONE (axum server, streaming + non-streaming, bind safety)
7. **`routectl-cli::test`** -- DONE (one-shot via Router with reasoning pretty-print)
8. **`routectl-cli::config`** -- DONE (check/show/example)

## v0.2.0 (DONE -- 2026-05-06)

Reliability, ergonomics, and a second Anthropic auth path. Combines
the modularization refactor with the routing-policy work surfaced
during the v0.1 feature audit.

165 tests pass. Live integration matrix: 5/5 tests, 43/47 model rows
across OpenRouter / OpenCode-Go / NIM. Release binary still <6MB.

### Highlights

- **Tier 1**: per-attempt `request_timeout_ms`, `stream_first_byte_timeout_ms`,
  `jitter_ms` on backoff, per-error-class retry caps (`retry_on_429` /
  `retry_on_5xx` / `retry_on_network`).
- **Tier 2**: per-provider `rpm_limit` (token bucket), passive circuit
  breaker (`circuit_failures` + `circuit_cooldown_ms`,
  single-probe half-open under concurrent load), per-request
  `x-routectl-disable-fallbacks` header.
- **Stream cancellation**: probe drop -> failure (releases the
  half-open slot), steady-state drop -> success (no flap on healthy
  providers).
- **Anthropic OAuth bearer auth** (`auth_kind = "oauth-bearer"`):
  send `Authorization: Bearer ...` + the `anthropic-beta` gate
  against `/v1/messages`. Wire format only -- routectl re-presents
  whatever access token the operator supplies; it makes no
  representation about which tokens are permitted to be used which
  ways.
- **Public API hardening for v0.x stability**: `#[non_exhaustive]` on
  `ProviderEntry`, `AliasEntry`, `RetryPolicy`,
  `ProviderRuntimePolicy`, `RouterOptions`. Constructors + chainable
  setters; setters panic on wrong-variant misuse.
- **Auth surface clarified**: dropped OS keychain support; `SecretRef`
  is `env://` | `file://` | `literal:` only. `file://` is TOCTOU-safe
  and absolute-path-only.
- **Modularization**: `ModelProfile` registry for per-model quirks;
  `Dialect` trait + `openai_compat/dialects/` per-dialect modules.

## v0.2.1 (planned) -- Anthropic-shape endpoint

Adds an Anthropic-shape `/v1/messages` endpoint for clients that
prefer that wire format over the OpenAI shape.

1. **`POST /v1/messages` endpoint** -- Anthropic-shape input/output
   with full tool-call round-trip + thinking blocks + signature
   preservation + streaming SSE (typed events).
2. **OAuth refresh-token handling** -- detect 401, re-read the token
   file, retry once. Optional `refresh_token_ref` for full refresh
   round-trip with the OAuth provider.

Default-build providers stay the same (api-key + OAuth-bearer auth via
`anthropic-api`, or any OpenAI-compat endpoint). Cookie auth
(`claude-cookie`, `chatgpt-cookie`) remains opt-in behind cargo
features.

## v0.3.0 (planned) -- Bedrock + load-balancing routing

Phase C.

1. **`bedrock` provider** with AWS Sigv4 signing, region/profile
   config, and Bedrock Anthropic-shape model name mapping.
2. **Latency-based routing** across multiple healthy providers in a
   chain (sliding-window p95 tracking, weighted random).
3. **Spend tracking** -- per-provider request count + token usage
   metric, exposed via `/v1/metrics` for Prometheus scrape.

## Post-v0.3 (deferred / never)

- Caching layer (use a proxy if you want this)
- Web UI / config editor (CLI-only by design)
- Server mode (multi-user, TLS, persistent state) -- out of scope, fork if you want it
- Cost-aware routing (overlap with LMSYS RouteLLM, different product)
- Atropos / RL trajectory hooks (overlap with Hermes Agent, different product)
- Distribution: `cargo dist` / Homebrew tap (manual `cargo build --release` for now)
- Live matrix in CI (currently runs on demand against real provider keys)
