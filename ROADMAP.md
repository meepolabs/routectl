# routectl roadmap

## Released

### v0.1.0 (DONE -- 2026-05-06) -- Initial release

`Provider` trait + canonical schema. `openai-compat` (5 dialects:
openai, deepseek, vllm, raw-think-tag, openrouter, passthrough; SSE
state machine with stateful `<think>` handling) and `anthropic-api`
(signature preservation across multi-turn + streaming) egresses.
Alias resolver, fallback walker, retry with exponential backoff.
`SecretStore` trait (env / file / literal). axum `serve` + clap
`test` / `config`. 128 tests, 5.6 MB stripped binary.

### v0.2.0 (DONE -- 2026-05-06) -- Reliability + second Anthropic auth path

Per-attempt `request_timeout_ms` / `stream_first_byte_timeout_ms`,
`jitter_ms` on backoff, per-error-class retry caps. Per-provider
`rpm_limit` (token bucket), passive circuit breaker, per-request
`x-routectl-disable-fallbacks` header. Anthropic OAuth bearer auth
(`auth_kind = "oauth-bearer"`). `#[non_exhaustive]` public-type
hardening for v0.x stability. 165 tests, 43/47 live matrix.

### v0.3.0 (DONE) -- Native AWS Bedrock + Anthropic header polish

Native `bedrock` provider: SigV4 via `aws-sigv4`, full AWS credential
chain (env / static / profile / SSO / web identity / IRSA / IMDS) +
short-term `bearer-key`. `InvokeModel` (Anthropic Messages body) and
`Converse` transport wired; eventstream binary frames decoded. Gated
behind a default-on `bedrock` Cargo feature. `extra_headers` /
`user_agent` on `[providers.X] anthropic-api`; beta-flag declaration
decoupled from `auth_kind`. `extra_headers` cannot override
auth-bearing headers.

### v0.4.0 (DONE) -- API spec independence

Two ingress dialects (OpenAI Chat Completions + Anthropic Messages),
one canonical internal request shape, N egress providers. Forward-
compat catchalls (`ContentPart::Other`, `ToolDef::Other`,
`ContentBlock::Other`) pass unknown Anthropic block types through
verbatim. Listener-side auth via `[server.auth] tokens = [...]`.
`strict_translation = false` default with WARN on lossy seams.
Adaptive thinking for Opus 4.7+. Universal 4xx/5xx body logging with
`request_id` correlation.

### v0.5.0 (DONE) -- Translation hardening + dogfood fixes

`tool_choice` shape coercion (OpenAI -> Anthropic at egress). Top-
level `system` lower for strict openai-compat hosts. `prompt_tokens`
cache-aware sum. openai-compat `reasoning_content` coalescing. Per-
provider `history_reasoning = auto | strip | preserve` for DeepSeek
v3/v4 + vLLM. Per-provider `request_timeout_ms` /
`stream_first_byte_timeout_ms`. Operator-owned `[bedrock]
allowed_betas` / `allowed_body_fields` (BREAKING rename). Anthropic-
on-Converse body translation. [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md) operator guide.

### v0.6.0 (DONE) -- Layered config + dispatch hygiene

Layered config schema: `[providers.X]` (transport) + `[models.X]`
(per-model behavior). `header_extras` and `payload_extras` live on
both layers and merge at dispatch with `anthropic-beta` comma-union.
Per-model circuit-breaker + RPM-bucket isolation. Unified `[aliases]`
table with suffix-glob keys + `String | Vec<String>` chain values.
`openai-responses` provider (ChatGPT Codex chatgpt-oauth, stream-
only). openai-compat normalization (vendor envelope strip, usage
sub-bag lifts). Anthropic legacy thinking budget hygiene. Stop-
sequence preservation end-to-end (anthropic-api + bedrock-invoke +
openai-compat heuristic). CF extended 5xx range (520-527, 530) in
default `fallback_on_status`. `ROUTECTL_TRACE_BODY_BYTES` for live-
traffic fixture capture. CI: gitleaks workflow + pinned action SHAs +
scoped `permissions: contents: read`.

For full per-version detail see [CHANGELOG.md](CHANGELOG.md).

## Planned

### v0.7.0+ -- Replay-based testing + OAuth ergonomics

1. **Replay-based integration tests from captured live traffic**.
   The captured-fixture corpus in
   `crates/routectl-cli/tests/fixtures/captured/` (gitignored) feeds
   a deterministic test harness that replays request/response pairs
   against routectl, asserting wire-shape invariants without touching
   the network. Becomes the inner loop for translation work; the
   cents-per-run live matrix stays as the final integration gate.

2. **JSON-file auth + file-watch self-reexec**. Read Anthropic OAuth
   bearer credentials from a JSON file directly (no manual snapshot
   into `ROUTECTL_ANTHROPIC`). Watch the file for rotation; on
   change, self-reexec gracefully so the next request picks up the
   new token without operator intervention. The same machinery
   covers `config.toml` reloads -- any watched file change triggers
   a re-exec.

3. **Bedrock Converse `stop_sequence` round-trip**. AWS surfaces the
   matched sequence via `additionalModelResponseFields` only when
   the request opts in via `additionalModelResponseFieldPaths`.
   Completes the v0.6.0 stop-sequence preservation fix.

### v0.8.0+ -- Token reduction (themed)

Driven by long-session cost pressure on growing context windows.
Workstreams (concrete scopes TBD before milestone opens):

- Tool-output truncation past N turns (configurable cap; preserve
  last K full + summarize older).
- `reasoning_details[]` compaction across long thinking history
  (drop or summarize older entries while preserving signature gates
  for replay).
- `encrypted_content` drop after M turns on the openai-responses
  path.
- Existing-research review (claude-code's own compaction strategy,
  prompt compression literature) before designing routectl-side
  reductions.
- Per-message `cache_control` hint emission on long boilerplate
  (system blocks, tool defs) where ingress didn't already mark it.

## Deferred (uncertain; might do, might never)

- Cost-aware routing (overlap with LMSYS RouteLLM)
- Latency-aware routing (sliding-window p95 across healthy chain
  entries, weighted random)
- Spend tracking / `/v1/metrics` Prometheus exposition. Live
  counters reset on restart; external Prometheus scrape stores
  history. Data exists in `tracing` logs today (token counts on
  every response, structured WARNs at every fallback hop) -- a
  Prometheus exporter is in-process glue + `/v1/metrics` handler
  (~250-300 LOC) when we want it.
- `cargo dist` / Homebrew tap (manual `cargo build --release` for
  now)
- Live matrix in CI (currently runs on demand against real provider
  keys)

## Never (explicitly out of scope)

- SSO / LDAP / OIDC / SAML auth -- use a sidecar proxy if you need it
- Multi-tenancy, per-user routing, persistent server state -- fork if
  you want it
- Web UI / config editor (CLI-only by design)
- Caching layer (use an HTTP cache proxy)
- Atropos / RL trajectory hooks (different product space)
- Compliance certifications (SOC 2, HIPAA, audit logging)
- Plugin system / dynamic dialect loading -- build it in or fork
