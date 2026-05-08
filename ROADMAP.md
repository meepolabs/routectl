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
  send `Authorization: Bearer ...` against `/v1/messages`. Wire
  format only -- routectl re-presents whatever access token the
  operator supplies; it makes no representation about which tokens
  are permitted to be used which ways. Beta gates were originally
  auto-injected on this auth kind; v0.3.0 decoupled them so any
  auth method can declare gates via `extra_headers`.
- **Public API hardening for v0.x stability**: `#[non_exhaustive]` on
  `ProviderEntry`, `AliasEntry`, `RetryPolicy`,
  `ProviderRuntimePolicy`, `RouterOptions`. Constructors + chainable
  setters; setters panic on wrong-variant misuse.
- **Auth surface clarified**: dropped OS keychain support; `SecretRef`
  is `env://` | `file://` | `literal:` only. `file://` is TOCTOU-safe
  and absolute-path-only.
- **Modularization**: `ModelProfile` registry for per-model quirks;
  `Dialect` trait + `openai_compat/dialects/` per-dialect modules.

## v0.3.0 (in flight on `feat/v0.3-bedrock`) -- Native AWS Bedrock + Anthropic header polish

Two themes landed together because they share the Anthropic Messages
wire format:

1. **Native `bedrock` provider** -- talks to
   `bedrock-runtime.<region>.amazonaws.com` directly, no gateway in
   the middle. SigV4 signing via `aws-sigv4`, full AWS credential
   chain via `aws-config` (env / static / profile / SSO / web identity
   / IRSA / IMDS), plus a `bearer-key` flavor for the AWS console's
   short-term token. `InvokeModel` body shape (Anthropic Messages
   today; per-vendor as new vendors are added) and `Converse`
   transport are wired; `Converse` body translation deferred to v0.4.0.
   Streaming responses decoded from the AWS eventstream binary frame
   format. Gated behind a default-on `bedrock` Cargo feature so
   library consumers can opt out of the AWS dep tree with
   `--no-default-features`.
2. **Anthropic header polish** -- `extra_headers` and `user_agent`
   on `[providers.X]` of type `anthropic-api`, mirroring the
   existing `openai-compat` fields. Beta-flag declaration is now
   decoupled from `auth_kind`; both api-key and OAuth-bearer paths
   declare `anthropic-beta` via `extra_headers` (BREAKING for
   v0.2.0 OAuth-bearer users -- see CHANGELOG).
3. **Auth surface fixes** -- `extra_headers` cannot override
   auth-bearing headers; reserved-header collisions are dropped with
   a `tracing::warn!`. `BedrockCreds` redacts secret material in
   `Debug` output. Eventstream parser caps single-frame size at 8 MiB
   to defend against advertised-length OOM. Secret-file reads cap at
   1 MiB.

Deferred from this milestone into v0.4.0:

- `routectl doctor` subcommand (active credential probe + IAM action
  surfacing for Bedrock).
- `Converse` body translation for non-Anthropic Bedrock vendors.
- Live integration matrix entries for Bedrock.

## v0.4.0 (planned) -- API spec independence

Two ingress dialects (OpenAI Chat Completions + Anthropic Messages),
one canonical internal request shape, N egress providers. Lets any
harness (Claude Code, opencode, Codex, raw curl) pick either wire
format and route through any backend.

1. **`POST /v1/messages` Anthropic ingress** with full tool-call
   round-trip, thinking blocks + signature preservation, typed SSE
   events (`message_start` / `content_block_*` / `message_stop`),
   server-side model-id -> alias mapping, and `x-routectl-alias`
   header override.
2. **Canonical internal shape** as a strict superset of OpenAI and
   Anthropic. Each provider translates canonical -> its wire format;
   each ingress translates its wire format -> canonical. N+M
   translators, not N*M. Lossy seams (cache_control / thinking
   signatures dropped on canonical -> OpenAI-compat) surface as
   `tracing::warn!` and an opt-in `strict_translation` policy.
3. **Listener-side auth** via static config tokens (`[server.auth]
   tokens = [...]`); inbound auth is decoupled from upstream
   credentials.
4. **OAuth refresh, two stages**:
   - *Per-request file re-read*: opt-in `refresh_per_request = true`
     on `file://`-backed providers picks up rotated tokens within
     one request, no restart needed. Cheap to implement, useful when
     an external sidecar handles the actual refresh dance.
   - *Full OAuth refresh-token round-trip*: routectl holds the
     refresh token, calls the OAuth provider's `/token` endpoint
     before access-token expiry, swaps the new access token into
     memory, and atomically writes any rotated refresh token back to
     disk. The "real" answer for OAuth bearer; replaces the need
     for an external rotator.
5. **Bedrock follow-ons**:
   - `routectl doctor` subcommand (active credential probe; IAM
     action surfacing including the
     `bedrock:InvokeModelWithResponseStream` streaming-permission
     case; clock-skew check).
   - `Converse` body translation (vendor-neutral) for non-Anthropic
     Bedrock vendors.
   - Live matrix Bedrock entries (Invoke + Converse).

## v0.5.0 (planned) -- Latency-aware routing + observability

Originally scoped for v0.3.1; deferred to focus v0.4.0 on the API
spec work.

1. **Latency-based routing** across multiple healthy providers in a
   chain (sliding-window p95 tracking, weighted random).
2. **Spend tracking** -- per-provider request count + token usage
   metric, exposed via `/v1/metrics` for Prometheus scrape.

## Post-v0.5 (deferred / never)

- Caching layer (use a proxy if you want this)
- Web UI / config editor (CLI-only by design)
- Server mode (multi-user, TLS, persistent state) -- out of scope, fork if you want it
- Cost-aware routing (overlap with LMSYS RouteLLM, different product)
- Atropos / RL trajectory hooks (overlap with Hermes Agent, different product)
- Distribution: `cargo dist` / Homebrew tap (manual `cargo build --release` for now)
- Live matrix in CI (currently runs on demand against real provider keys)
