# routectl roadmap

routectl is a single-binary LLM router. This file lists shipped
releases, planned work, deferred items, and explicit non-goals. For
per-feature change history see [CHANGELOG.md](CHANGELOG.md).

## Released

- **v0.7.0** (2026-05-30) -- routectl-managed OAuth (Anthropic +
  Codex) with runtime refresh and 401 recovery; claude-code as a
  first-class gateway client (`forward_client_headers`,
  `count_tokens` proxy, per-dialect error envelopes); server-side
  `context-management-2025-06-27` beta emulation; forward-compat for
  unknown Anthropic SSE blocks; per-provider capability filter.
  BREAKING: `thinking`/`effort` on `[models.X]` replaced by
  declarative `supports_adaptive_thinking`/`effort_levels`/
  `max_thinking_budget`; `fallback_on_status` replaced by
  `retry_allowlist`/`retry_denylist`.

- **v0.6.0** (2026-05-20) -- Layered `[providers.X]` + `[models.X]`
  config schema with dispatch-time merge; unified `[aliases]` table
  with suffix-glob keys + chain values; `openai-responses` provider
  (ChatGPT Codex chatgpt-oauth).

- **v0.5.0** -- Translation hardening: `tool_choice` shape coercion,
  top-level `system` lower for strict openai-compat hosts, per-
  provider `history_reasoning`, operator-owned Bedrock allowlists,
  [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md).

- **v0.4.0** -- API-spec independence: two ingress dialects (OpenAI
  + Anthropic) feeding one canonical request shape, forward-compat
  catchalls, listener-side auth, adaptive thinking for Opus 4.7+.

- **v0.3.0** -- Native AWS Bedrock provider (SigV4, full credential
  chain, InvokeModel + Converse).

- **v0.2.0** -- Reliability tier: per-attempt timeouts, jittered
  backoff, per-error-class retry caps, per-provider RPM token
  bucket, passive circuit breaker; Anthropic OAuth bearer auth.

- **v0.1.0** (2026-05-06) -- Initial release: `Provider` trait +
  canonical schema, `openai-compat` (5 dialects) + `anthropic-api`
  egresses, alias resolver, fallback walker.

## Planned

### v0.8.0+ -- Replay testing + auth ergonomics

- **Replay-based integration tests** against the captured-fixture
  corpus in `crates/routectl-cli/tests/fixtures/captured/`
  (gitignored). Deterministic wire-shape assertions without network;
  the live matrix stays as the final gate.

- **File-watch self-reexec** for `credentials.json` and
  `config.toml`. v0.7.0 shipped lazy refresh; the next step is to
  pick up external rotation without a daemon restart.

- **Bedrock Converse `stop_sequence` round-trip** via
  `additionalModelResponseFieldPaths`. Completes the v0.6.0
  stop-sequence preservation fix.

### v0.9.0+ -- Token reduction (themed)

Driven by long-session cost pressure. Concrete scopes TBD before
the milestone opens; the workstream covers tool-output truncation
past N turns, reasoning-history compaction, `encrypted_content`
aging-out on the openai-responses path, and operator-side
`cache_control` hint emission on long boilerplate.

## Deferred (might do, might never)

- Cost-aware routing (overlap with [LMSYS
  RouteLLM](https://github.com/lm-sys/RouteLLM)).
- Latency-aware routing (sliding-window p95 across healthy chain
  entries, weighted random).
- Spend tracking / `/v1/metrics` Prometheus exposition. Data
  already exists in `tracing` logs (token counts per response,
  structured WARNs at every fallback hop); an in-process exporter
  is ~250-300 LOC when prioritized.
- `cargo dist` / Homebrew tap (manual `cargo build --release` for
  now).
- Live matrix in CI (currently runs on demand against real provider
  keys).

## Never (explicitly out of scope)

- SSO / LDAP / OIDC / SAML auth -- use a sidecar proxy.
- Multi-tenancy, per-user routing, persistent server state -- fork
  if needed.
- Web UI / config editor (CLI-only by design).
- Caching layer (use an HTTP cache proxy).
- Atropos / RL trajectory hooks (different product space).
- Compliance certifications (SOC 2, HIPAA, audit logging).
- Plugin system / dynamic dialect loading -- build it in or fork.
