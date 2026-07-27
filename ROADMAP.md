# routectl roadmap

routectl is a single-binary LLM router. This file lists shipped
releases, planned work, deferred items, and explicit non-goals. For
per-feature change history see [CHANGELOG.md](CHANGELOG.md).

## Released

- **v0.9.0** (2026-06-18) -- Per-request usage accounting
  (`routectl-usage` SQLite crate + `routectl usage` CLI with
  query-time cost pricing); OAuth credential seat pools with
  `--label` per-seat login/logout/refresh and a `seat_selection`
  dispatch knob; managed Claude Code identity on the Anthropic OAuth
  egress (identity system block, stable `x-claude-code-session-id`,
  beta-floor auto-injection, billing-checksum re-sign); unified
  quota/overage observation; OpenAI file/PDF -> document-block
  translation; graceful-shutdown in-flight drain; per-model
  `reported_model` and `visible_routectl_provider` knobs; and a broad
  wire-correctness + log-redaction hardening pass. BREAKING: the
  response `model` field now echoes the client-requested alias
  instead of the upstream's internal id (override with
  `reported_model`).

- **v0.8.0** (2026-06-07) -- Config-overridable identity-header
  defaults and codex CLI client-header parity; `bedrock-mantle`
  bearer auth on the `openai-responses` provider; hot-reload of
  `credentials.json` and `config.toml` (file-watch + SIGHUP,
  parse-validate-or-keep-old); `[server] max_body_bytes`, per-model
  `max_output_tokens`, and raised internal caps; Bedrock Converse
  `stop_sequence` round-trip; replay-based integration test harness
  against the captured-fixture corpus.

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

### v0.10.0+ -- Token reduction (themed)

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
- Spend tracking: per-request SQLite accounting and the `routectl
  usage` CLI shipped in v0.9.0. The remaining deferred scope is the
  `/v1/metrics` Prometheus exposition endpoint -- an in-process
  exporter, smaller now that the data layer exists.
- `cargo dist` / Homebrew tap (manual `cargo build --release` for
  now).
- Live matrix in CI (currently runs on demand against real provider
  keys).

## Never (explicitly out of scope)

- SSO / LDAP / OIDC / SAML auth -- use a sidecar proxy.
- Multi-tenancy, per-user routing, persistent server state -- fork
  if needed.
- Config-editing / interactive admin web UI -- the read-only status
  dashboard (GET / and the /status JSON family) is deliberate; all
  mutation stays config.toml + CLI.
- Caching layer (use an HTTP cache proxy).
- Atropos / RL trajectory hooks (different product space).
- Compliance certifications (SOC 2, HIPAA, audit logging).
- Plugin system / dynamic dialect loading -- build it in or fork.
