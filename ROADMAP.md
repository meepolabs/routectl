# routectl roadmap

routectl is a single-binary LLM router. This file lists shipped
releases, planned work, deferred items, and explicit non-goals. For
per-feature change history see [CHANGELOG.md](CHANGELOG.md).

## On develop (unreleased since v0.9.0)

A large body of work has landed on `develop` and awaits the next
release cut. Highlights, roughly in dependency order:

- **Production hardening** -- auth-store lock ordering + atomic
  fsync-policy writes, network-exposure fail-closed middleware,
  request-body caps and disconnect-cancel semantics, retry-boundary
  fixes (no retry after first content chunk), usage-ledger accuracy
  (`http_status`, error classes), OAuth refresh cooldown, log
  redaction tightening.
- **Per-failure-class retry policy** -- a stable failure-class
  taxonomy (`rate-limited`, `auth`, `bad-request`, ...) with
  `[retry.classes.<class>]` per-class retry/fallback overrides and
  `[providers.X.class_overrides]` status remaps.
- **Model catalog** -- baked cache-economics catalog + user overlay
  with provenance and verification stamps; `routectl catalog
  list/verify/import/set/disable`.
- **Config schema v3 + `config migrate`** -- versioned config with a
  deterministic migration ladder; `config set/show/check` with
  dotted-key paths; a committed JSON Schema (`routectl.schema.json`).
- **Onboarding** -- `routectl init` wizard, `provider add` with
  secret capture (`env://`/managed `file://`; inline `literal:` refs
  now rejected), `doctor` diagnostics with exit-code contract,
  reachability probes.
- **Learned capability system** -- per-target capability registry
  learned from live rejections and response evidence, persisted in
  the usage ledger and rebuilt at boot; consented active probes
  (`probe --capabilities`); operator overrides
  (`[capability.overrides]`); a doctor truth-matrix panel and
  staleness hints.
- **Read-only status dashboard** -- `GET /` single-file dashboard +
  `/status/{usage,health,config,doctor}` JSON panels; structurally
  read-only.
- **Streaming reliability** -- early-response commit with a
  flush-first grace window; interim usage semantics.
- **Bedrock surface expansion** -- Amazon Bedrock "mantle"
  bearer-auth lanes on the Anthropic and OpenAI provider classes;
  Converse request-side gap closure.
- **Native Gemini egress** -- `generateContent` API-key and Cloud
  Code OAuth auth modes.
- **OpenAI Responses ingress** -- third ingress dialect
  (`POST /v1/responses`).
- **Performance** -- criterion bench harness + allocation reductions
  on the hot path (zero-alloc token estimates, copy-on-write message
  sharing, byte-oriented ingress).
- **First-party passthrough** -- optional `[mitm]` front-proxy so
  Claude Code can route inference through routectl while Remote
  Control keeps working against `api.anthropic.com`; per-target
  forwarded-credential mode with strict transparency guarantees.
- **Codex identity currency** -- config-overridable codex client
  version (`codex_version`) reaching every fingerprint surface from
  one derivation, plus a persistent installation id.
- **Cache stability guardrails** -- advisory warning when a
  caller-cached prefix carries volatile content; optional tool-array
  normalization (`[cache] normalize_tools`).
- **Advisory context reduction** -- lossless dedup/supersession
  analysis with per-request would-save accounting and a
  confidence-bounded cache-hit estimator (observation-only; no
  request mutation).

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

### Token reduction, active phase

The observation layer shipped on develop (advisory dedup/
supersession analysis + cache-hit estimator, see above). The
remaining planned phase is the live request-mutating cut, designed
as adaptive self-gating (dormant where trimming is uneconomical,
active where a workload earns it). Structural/size heuristics and
estimator warm-start follow behind it.

## Deferred (might do, might never)

- Cost- and latency-aware routing: explored and closed as an
  in-proxy feature -- the proxy sees only wire bytes, so its role is
  to expose cache/latency/cost signals a client-side harness can
  route on, not to make the routing decision itself.
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
- Multi-tenancy, per-user routing, multi-tenant server-session
  state -- fork if needed. (Persistent LOCAL state is in scope and
  shipped: the usage ledger, learned-capability registry, catalog
  overlay.)
- Config-editing / interactive admin web UI -- the read-only status
  dashboard (GET / and the /status JSON family) is deliberate; all
  mutation stays config.toml + CLI.
- Caching layer (use an HTTP cache proxy).
- Atropos / RL trajectory hooks (different product space).
- Compliance certifications (SOC 2, HIPAA, audit logging).
- Plugin system / dynamic dialect loading -- build it in or fork.
