# routectl roadmap

## v0.1 (DONE -- 2026-05-06)

Goal: a binary you can `cargo install`, point clients at, and have it route real LLM calls with fallback + reasoning normalization. No cookie-auth in default build.

128 tests pass across the workspace. Release binary 5.6MB stripped.

### Implementation status

1. **`routectl-core`** trait + schema -- DONE
2. **`routectl-providers::openai_compat`** -- DONE (5 dialects, 34 tests including SSE state machine)
3. **`routectl-providers::anthropic_api`** -- DONE (signature preservation across multi-turn + streaming, 20 tests)
4. **`routectl-router`** -- DONE (alias resolver + fallback walker + retry with exponential backoff + provider factory, 17 tests)
5. **`routectl-auth`** -- DONE (`SecretStore` trait, `KeyringStore`, `MemoryStore`, 26 tests)
6. **`routectl-cli::serve`** -- DONE (axum server, streaming + non-streaming, bind safety, 10 tests)
7. **`routectl-cli::test`** -- DONE (one-shot via Router with reasoning pretty-print)
8. **`routectl-cli::config`** -- DONE (check/show/example, 8 tests)
9. **Distribution** -- DEFERRED (no `cargo dist` / Homebrew tap yet; manual `cargo build --release` for now)

## v0.1.1 (DONE) -- modularization refactor

ModelProfile registry for per-model quirks; Dialect trait so each
reasoning dialect lives in one file; CLAUDE.md runbook for autonomous
agents. See `7a86f02..bc4618b` in git log.

## v0.2.0 (DONE) -- routing policy + model groups

Phase A from the LiteLLM/OpenRouter feature audit.

- **Tier 1**: per-attempt request_timeout_ms, stream_first_byte_timeout_ms,
  jitter_ms on backoff, per-error-class retry caps (retry_on_429 /
  retry_on_5xx / retry_on_network).
- **Tier 2**: per-provider rpm_limit (token bucket), passive circuit
  breaker (circuit_failures + circuit_cooldown_ms), per-request
  `x-routectl-disable-fallbacks` header.
- **Model groups**: opinionated `[aliases.heavy/med/cheap/local/reasoning]`
  in `examples/config.toml`. Aliases are the group mechanism -- no
  separate group syntax.

## v0.2.1 (planned) -- Anthropic-shape endpoint

Phase B. Aimed squarely at making Claude Code work pointed at routectl.

1. **`POST /v1/messages` endpoint** -- Anthropic-shape input/output
   with full tool-call round-trip + thinking blocks + signature
   preservation + streaming SSE (typed events).

Default-build providers stay the same (api-key auth via OpenAI-compat
or `anthropic-api`). Cookie / OAuth-token-based auth remains opt-in
behind cargo features, with the user explicitly configuring a token
file path -- no auto-discovery of any other tool's credential
storage.

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
- Atropos/RL trajectory hooks (overlap with Hermes Agent, different product)
