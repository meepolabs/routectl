# Changelog

All notable changes to routectl. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-06-07

### Added

- **Config-overridable identity defaults for codex and anthropic
  egresses.** Both providers now ship compiled identity-header
  defaults that fire on zero-config and let operator `header_extras`
  override any key. Merge order in `build_headers`: auth headers
  (never overridden) -> compiled defaults -> `header_extras` (config
  wins) -> per-request UUIDs (always win). The `chatgpt-oauth` egress
  emits `originator`, `x-openai-internal-codex-residency`, and
  `version` from pinned constants; the `oauth-bearer` anthropic-api
  egress emits `x-stainless-*`, `x-app`, and
  `anthropic-dangerous-direct-browser-access` with dynamic OS/arch
  mapping, and defaults `user_agent` to `claude-cli/<version>` when
  the config field is unset. Superseded `OpenAiResponsesConfig`
  plumbing (`session_id`, `installation_id`, `originator`) is now
  auto-generated or carried via `header_extras` rather than set as
  explicit fields.

- **codex CLI fingerprint mirroring on the chatgpt.com surface.**
  The `openai-responses` codex egress and its OAuth refresh client
  now present every header ChatGPT's risk system can fingerprint as
  codex CLI does, resolving sessions being invalidated within
  6-15 minutes and step-upped to phone verification. New
  `routectl-core::codex_fingerprint` module is the single source of
  truth for the codex `User-Agent` shape, the `originator`/residency
  defaults, and the process-global `x-codex-window-id`. Requests
  stamp `version`, `session-id`, `x-codex-installation-id`,
  `x-codex-window-id`, `thread-id`, and `x-client-request-id`.
  `session_id` is per-credential (persisted on `credentials.json`'s
  token record, minted on `routectl login codex`, lazy-backfilled
  into pre-existing records, preserved across refresh);
  the static identity headers (`version`, `originator`, residency,
  `x-codex-installation-id`) ride the config-overridable identity
  defaults above, so `header_extras` can override any of them;
  `thread-id` and `x-client-request-id` are a fresh UUIDv4 per
  upstream turn. The OAuth refresh client now
  carries the codex UA and default headers. Pinned codex version is
  documented in `docs/PROVIDER-QUIRKS.md` with the
  fingerprint-header lockstep contract.

- **`bedrock-mantle` bearer-token auth path** on the
  `openai-responses` provider. New `auth_kind = "bedrock-mantle"`
  replaces the prior `NotImplemented` stub, unlocking AWS Bedrock
  GPT-5.5 (and future Mantle-hosted Responses-API models) through the
  existing provider with no separate provider kind. The wire shape is
  verbatim OpenAI Responses, so request assembly, the SSE state
  machine, response decoding, and the URL builder are reused
  unchanged; the only delta is the bearer source (typically
  `env://AWS_BEARER_TOKEN_BEDROCK`). The `/openai/v1` path prefix is
  carried in `base_url`.

- **Hot-reload of `credentials.json` and `config.toml` on external
  rewrite.** A single filesystem watcher on the parent directories
  (reacting to `IN_CLOSE_WRITE`/`IN_MOVED_TO`, 250ms debounce) plus a
  `SIGHUP` escape hatch feed one reload coordinator. The pipeline is
  parse-validate-or-keep-old: an invalid TOML/JSON write emits a WARN
  with the parse error and keeps the current config; only successful
  parses swap in. Hot-reloadable: `[providers]`, `[models]`,
  `[aliases]`, `[retry]`, the `[bedrock]` global, and
  `credentials.json`. Restart-required (with a diff-WARN on detected
  change): `server.host`, `server.port`, `server.auth`,
  `server.max_body_bytes`, and the `[log]` knobs. Circuit-breaker
  counters and RPM token buckets are carried across the reload for
  surviving model nicknames; the OAuth token cache and per-provider
  refresh mutexes are preserved.

- **`[server] max_body_bytes` and per-model `[models.X]
  max_output_tokens`, plus raised internal caps.** New
  `max_body_bytes` knob (default 32 MiB, was a hardcoded 4 MiB;
  returns 413 above the cap). `max_output_tokens` caps the
  `max_tokens` value the Anthropic-shape egresses (`anthropic-api`,
  `bedrock-invoke`) inject when the caller omits the field;
  resolution chain `request.max_tokens` -> `[models.X]
  max_output_tokens` -> hardcoded `64000` baseline. The
  `openai-compat`, `openai-responses`, and `bedrock-converse`
  egresses forward omission cleanly without injection. Caps raised
  for parallel-tool-fanout and long-conversation workloads: anthropic
  ingress tool-call index 64 -> 4096, openai-responses output-block
  count 512 -> 4096, anthropic-api thinking-cache entries
  1000 -> 10000.

- **Per-provider `max_thinking_entry_bytes` knob** on `[providers.X]`
  of `kind = "anthropic-api"` (1 KiB to 4 MiB; default 1 MiB). Tunes
  the per-entry byte cap on the `context_management` emulation
  thinking-cache. Zero falls back to the default with a WARN (a zero
  cap would silently disable the cache); values above the 4 MiB
  ceiling clamp with a WARN.

- **Build-time WARN when `context_management = true` without
  `history_reasoning = "preserve"`.** The two settings are
  complementary on non-Anthropic anthropic-api endpoints (DeepSeek
  `/anthropic`, vLLM, LM Studio); without `preserve`, multi-turn
  continuity breaks the next turn with a missing-thinking 400. The
  WARN surfaces the misconfig once at startup instead of on every
  dispatch.

- **`max_completion_tokens` -> `max_tokens` translation on the OpenAI
  Chat ingress.** o-series / gpt-5+ clients send
  `max_completion_tokens`; the canonical request has no such field,
  so it was silently dropped and the egress saw no per-request token
  cap. It is now renamed before deserialization (`max_tokens` wins if
  both are present).

- **`role: "developer"` support on the OpenAI Chat ingress.** Newer
  clients send the system-voice successor `developer` role; it is
  rewritten to `system` before deserialization so it flows through
  the system-message lift normally instead of failing with a 400.

### Changed

- **`Error::Internal` added to the error taxonomy** for unexpected
  runtime failures (serialization bugs, socket / serve-loop IO,
  impossible states). Six sites that misused `Error::Config` for
  non-config failures are reclassified. The HTTP mapping returns a
  generic `internal error` to clients while logging detail at ERROR
  for operators; `Error::Config` is now documented as
  configuration-validation-only.

- **Default retry jitter is now 50ms** (`RetryPolicy::default()
  .jitter_ms`, previously 0), giving retry spread out of the box.

- **Per-entry byte cap on the `context_management` emulation
  thinking-cache raised from 256 KB to 1 MiB.** The 256 KB cap
  rejected cache writes on full-budget Opus 4.6/4.7/4.8 reasoning
  turns (~328 KB at 65k thinking tokens); 1 MiB gives ~3x headroom.
  Operators on memory-constrained hosts can tune down via the new
  `max_thinking_entry_bytes` knob.

- **Thinking-cache TTL switched from set-on-create to sliding:**
  every successful hit refreshes the entry's `expires_at` to
  `ttl-from-now`, matching Anthropic and DeepSeek prompt-cache
  semantics. Idle entries still die after the hardcoded 60-minute
  window.

- Internal: structural decomposition and shared-helper extraction
  across the bedrock and anthropic-api modules (shared eventstream
  framing driver, hoisted HTTP/header helpers, request-builder split,
  consolidated identity module, unified provider seam naming,
  test-builder dedup into `routectl-core`), with no behavior change.

### Fixed

- **Bedrock Converse now completes the `stop_sequence` round-trip.**
  The request always declares `["/stop_sequence"]` in
  `additionalModelResponseFieldPaths`, AWS lifts the matched literal
  into `additionalModelResponseFields`, and routectl reads it back
  gated on `stop_reason == "stop_sequence"` on both the non-streaming
  and streaming (metadata-frame) paths. A schema-drift DEBUG fires
  when the stop reason indicates a match but the lifted field is
  absent.

- **anthropic-api effort caps no longer bypassed via
  `provider_extras`.** On the adaptive-thinking path,
  `merge_provider_extras` overwrote the clamped `output_config.effort`
  with the raw caller-supplied value (since `output_config` is
  intentionally not routectl-managed), so a client sending
  `output_config.effort` shipped the unclamped value (e.g. `max`) even
  when the operator declared `effort_levels`. The post-merge seam now
  re-clamps `output_config.effort` while leaving sibling sub-keys
  (`format`, etc.) untouched. Affects `anthropic-api` and
  `bedrock-invoke`.

- **anthropic-api: probe-shaped `thinking` 400, `signature: null`,
  and openai-compat envelope leak.** Claude Code probes
  (`max_tokens` 48-128) 400'd because the legacy thinking shape
  derived a budget with no floor; `thinking` is now dropped with a
  WARN when `max_tokens` cannot fit a >=1024 budget plus content,
  rather than mutating the caller's `max_tokens`. A canonical
  reasoning detail with no signature rendered as `signature: null`,
  which Anthropic rejects on mid-conversation provider switch; the
  field is now omitted entirely when the source has none.
  openai-compat envelope fields (`object`, `system_fingerprint`,
  `cost`, top-level `role`) and usage sub-bags
  (`prompt_cache_hit_tokens`, `prompt_tokens_details.cached_tokens`,
  `completion_tokens_details.reasoning_tokens`) are filtered at the
  openai-compat parse seam so they stop leaking onto Anthropic-shape
  responses.

- **`count_tokens` body allowlist drops `metadata`.** The
  `/v1/messages/count_tokens` endpoint rejects `metadata` with an
  "Extra inputs are not permitted" 400; it is removed from the
  forwarding allowlist (and `output_config` is documented as accepted
  but token-count-irrelevant).

- **anthropic-api: opaque-block stop sentinel and dropped-reasoning
  observability.** A degraded opaque (unknown-type) content block
  that exceeded the capture cap after its `content_block_start` had
  already been emitted left an unclosed block on the wire; the
  `content_block_stop` is now emitted unconditionally. A structured
  WARN now fires when `emit_reasoning_blocks` drops
  `reasoning_details` whose format is not `anthropic-claude-v1`.

- **anthropic-api `allowed_betas` now enforced on the
  `anthropic-beta` HTTP header.** The allowlist was applied only to
  the request body field (which is stripped before send), so the
  header the upstream inspects carried the unfiltered client list.
  Header and body now share one predicate.

- **OpenAI ingress: tool_use double-render, system `cache_control`
  loss, and stop-sequence strip.** Anthropic-shape egresses carry
  each tool_use in both the canonical `tool_calls` field and the
  parts array; the OpenAI render emitted both, so Chat-Completions
  clients received tool_use content blocks alongside the tool_calls
  they understand. tool_use parts are now stripped at the OpenAI
  render seam when `tool_calls` is present (Anthropic-ingress and
  egress paths untouched). Parts-form system messages were flattened
  to plain text, dropping per-block `cache_control` and citations;
  the lift now emits `SystemContent::Blocks` when any block carries
  them. The internal `matched_stop_sequence` field is stripped from
  every choice on both render paths.

- **anthropic ingress: streaming usage and finalization.** The
  closing `message_delta` omitted `input_tokens` when the upstream
  lacked `prompt_tokens`, emitted a hollow empty usage object when
  every field was `None`, and could double-emit the terminal error
  event after a normal finish; a client-sent `output_format` of JSON
  null was promoted to `output_config.format = null` (rejected with
  400); and the non-streaming render emitted absent cache-usage
  fields as JSON null. Each is fixed (default missing `prompt_tokens`
  to 0, attach usage only when populated, early-return from the
  error-eos path once finished, drop a null `output_format`, build
  the response usage object incrementally).

- **openai-compat: mid-stream errors and hardened lifts.** An
  upstream mid-200 error envelope was reported as a chunk-deserialize
  error rather than an upstream error; a JSON-null usage sub-bag
  blocked the usage lift; a `tool_result` without `tool_use_id` was
  dropped silently (now hard-fails, matching the anthropic-api
  contract); the reasoning-strip WARN missed the `Thinking`
  content-part shape; and stop-sequence text was accumulated even
  with no configured stop. Streamed `reasoning_details` from DeepSeek
  and vLLM now increment `detail_index` per emitted block instead of
  collapsing onto index 0.

- **openai-responses: signature passthrough, cancel semantics, and
  reasoning logging.** The canonical Anthropic-shape signature was
  placed into the OpenAI Responses `encrypted_content` field (which
  only accepts an `openai-responses-v1` token); the signature is now
  forwarded only when the source format matches.
  `response.cancelled` and a `response.completed` missing its payload
  now surface as upstream errors (matching `complete()`) instead of a
  benign terminal chunk. Dropped non-`openai-responses-v1` reasoning
  entries are summarized in a single DEBUG instead of discarded
  silently.

- **bedrock-converse: forward text documents and envelope
  reasoning_details.** Text-source documents (a valid Anthropic
  document shape) were dropped instead of normalized to base64;
  images/documents with an unmapped `media_type` or missing source
  were dropped with no log line. The Converse egress also dropped
  envelope `reasoning_details` entirely, losing multi-turn reasoning
  on replay; it now emits `ReasoningContent` blocks for
  `anthropic-claude-v1` details (skipping unsigned blocks, which
  Bedrock rejects) and no longer emits an orphan `cachePoint` for an
  empty-text system block.

- **bedrock: reserve `x-amz-*` headers and sign the User-Agent.** An
  operator `header_extras` entry carrying an `x-amz-*` header could
  desync the SigV4 signature; the `x-amz-` prefix is now reserved. The
  User-Agent is inserted on the outbound request so it is both
  SigV4-signed and visible in the header trace; a chunk frame missing
  its `bytes` field is skipped (WARN) instead of killing the stream.

- **router: half-open breaker probe slot now released on all exits.**
  Several dispatch exits returned without recording an outcome (probe
  fast-fail on 429/529, auth-refresh failure, auth-refresh success
  before retry, retry-without-fallback, non-fallbackable client
  errors), leaking the single half-open probe slot and holding the
  breaker open for that provider until restart. A slot release that
  frees the flag without a failure debit now runs on every such exit,
  and the gate runs inside the retry loop in `stream` and
  `count_tokens` so per-attempt RPM and breaker accounting match
  `complete()`.

- **router: 429 non-retryable when excluded from fallback, and
  operator beta floor / `header_extras` preserved across dispatch.**
  `retries_for_status` gated its 5xx arm on fallbackability but not
  its 429 arm, so an operator who excluded 429 from fallback still
  got same-provider 429 retries; the 429 arm is now gated the same
  way. A model-level `anthropic-beta` from `[models.X] header_extras`
  was folded into the client beta union and then dropped by the
  per-provider `allowed_betas` filter (meant to gate only
  client-requested betas); it now rides a separate operator-floor
  field and is re-added unconditionally after the allowlist runs.
  `apply_layered_overlays` rebuilt `routectl_internal` from default
  and dropped the composed non-beta `header_extras` map; the composed
  map is now preserved across the rebuild.

- **factory: dedup bedrock credential probe on the failure path, and
  WARN on the bedrock-mantle region-pin fallback.** On
  credential-resolution failure for one Bedrock model, sibling models
  on the same provider re-ran the SSO probe / credential-chain build
  because the failure path never consulted `provider_failed`; the
  chain is now hit at most once per provider. Because the
  `openai-responses` config carries no region field, the factory
  falls back to the `us-east-1` bedrock-mantle endpoint; it now WARNs
  at construction so operators on other regions are not silently
  misdirected.

- **auth: close the OAuth reload/refresh race and harden login
  read.** A `reload_from_disk` could be clobbered by a refresh POST
  that began before it; a generation counter now lets a stale refresh
  detect the reload and discard its result. The unbounded login-line
  read is replaced with a bounded `read_line` loop.

- **file-watch: suppress the misleading Remove WARN on atomic
  rewrites.** Atomic-rewrite editors (vim, safe-save) issue a
  Remove + Create pair within one debounced batch; the handler now
  scans the batch for a sibling Create/Modify with matching basename
  and emits a DEBUG breadcrumb instead of a "watched file was
  removed" WARN. Lone Remove events keep the WARN so genuine
  deletions still alert. (#38)

- **build: feature-isolation fixes.** The `openai-responses` feature
  did not declare `dep:chrono` (built only when a sibling feature
  pulled `chrono` in); the dependency is now declared on the feature.

### Documentation

- Corrected reference drift across `CONFIGURATION.md`, `CODEMAP.md`,
  `ARCHITECTURE.md`, `LOGGING.md`, `PROVIDER-QUIRKS.md`, and
  `WIRE-GOTCHAS.md`: the bedrock framing driver, the two-tier retry
  resolution and per-error-class caps, the bedrock `api_shape`
  selector, the 256-char WARN body-excerpt cap, 6 (not 5) reasoning
  dialects, the 300s OAuth refresh lead, per-provider runtime gates,
  the config-show secret-redaction behavior, the non-loopback bind
  auth requirement, and the per-direction header-trace redaction
  policy. Each verified against the cited code.

- Documented previously-undocumented `[server]` knobs in
  `CONFIGURATION.md`: `max_body_bytes` (32 MiB cap, 413 failure shape,
  restart requirement) and `allow_disable_fallbacks` (plus its
  `x-routectl-disable-fallbacks` request header), and the per-model
  `max_output_tokens` knob with its resolution chain.

- Recast the replay harness around a local-only fixture corpus:
  `docs/REPLAY-FIXTURES.md` is now a format reference (per-fixture
  layout, `meta.json` schema), the "Adding a replay fixture" flow in
  `DEVELOPMENT.md` collapses to a 5-step capture flow, and the
  capture script stamps the workspace version into fixture metadata.

### Security

- **Eventstream payload logging gated behind TRACE on both Bedrock
  decoders.** The Converse decode-error path and `contentBlockStart`
  handler logged decoded, upstream-controlled model output (which may
  carry prompt-derived secrets/PII) at WARN/DEBUG; both now log only
  the 12-byte frame prelude (or top-level key list) at WARN/DEBUG and
  gate the full payload behind TRACE, matching the Invoke decoder and
  the shared framing driver.

- **Bearer JWT redacted from the outgoing-headers TRACE.** The
  outgoing-request-headers TRACE in `routectl-core::log_safe` emitted
  `Authorization: Bearer <jwt>` verbatim, exposing live access tokens
  (which embed account/session identifiers). The fix lives at the
  lowest trace layer so all four providers are covered;
  `Authorization`, `x-api-key`, and `proxy-authorization` values are
  masked while header names are preserved.

- **Cloudflare cookie jar on the chatgpt.com client.** The
  `openai-responses` provider now attaches a persistent cookie jar
  (default `~/.config/routectl/cookies/chatgpt.json`, mode 0600,
  overridable via `ROUTECTL_COOKIE_FILE`), allowlist-filtered to
  Cloudflare service-cookie names on both save and load so account or
  session cookies never land on disk. Persistence runs off the async
  executor on provider drop.

- **OAuth refresh tracing with `sha256[0:8]` hashes.** Pre-POST,
  success, and failure events emit grant type, status, and
  hashed-token correlation fields; token values are never logged, and
  failure events omit upstream body excerpts (some token endpoints
  echo `refresh_token` in error envelopes). A canary test pins that
  no echoed token leaks into any captured field.

- **Upstream error bodies sanitized against log-line forgery.**
  Upstream-controlled 4xx/5xx error bodies and the forward-compat SSE
  `content_block.type` were logged without control-char filtering, so
  CR/LF/ANSI sequences could forge log lines on text-format
  subscribers. openai-compat, openai-responses, bedrock, the shared
  DEBUG full-body helper, and the SSE block-type capture now pass
  through the control-char filter.

- **Secret-ref values redacted from error messages.**
  `SecretRef::parse` and the listener-token resolver embedded the raw
  reference in error strings, so a bare or `literal:` secret could
  reach operator-facing output (config-check stdout, startup logs).
  They now report only a validated scheme prefix (or, for listener
  tokens, the entry position).

- **Operator `header_extras` can no longer override codex fingerprint
  headers** on the ChatGPT-OAuth path, keeping the impersonation
  contract intact.

- **Per-entry size bound on the anthropic-api thinking cache.**
  Oversized writes are rejected with a WARN (the cache-miss recovery
  handles the next turn as it would on a TTL eviction), preventing
  unbounded LRU memory use. Truncation was rejected as an alternative
  because it would corrupt the opaque continuity signature on
  Anthropic thinking blocks.

## [0.7.0] - 2026-05-30

The v0.7 release: routectl-managed OAuth login (Anthropic + Codex)
with runtime refresh and one-shot 401 recovery, claude-code as a
first-class gateway client, forward-compat for unknown Anthropic SSE
block types, server-side emulation of the context-management beta
for non-Anthropic anthropic-api endpoints, and a BREAKING refactor
moving model reasoning capabilities from per-provider floors to
declarative per-model fields.

**Responsible use.** Anthropic publishes a gateway pattern at
<https://code.claude.com/docs/en/llm-gateway> for first-party
deployments. routectl's `oauth://anthropic` ref and gateway support
are for personal-use proxying with the operator's own subscription
token; per the Anthropic Agent SDK overview, claude.ai OAuth tokens
may not be embedded in third-party products. routectl does not
support or condone gateway usage beyond what the upstream provider
permits and does not vouch for whether a particular credential is
permitted to be used a particular way -- read the upstream
provider's terms before pointing routectl at production traffic.
See [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) "claude-code as
a gateway client" for the operator setup, and the README
"Responsible use" section.

### Added

- **routectl-managed OAuth client** (Anthropic + Codex). `routectl
  login <provider>` runs PKCE through a loopback callback, persists
  tokens atomically to `~/.config/routectl/credentials.json` (chmod
  0600, tempfile + fsync + rename). New `oauth://<provider>`
  SecretRef resolves at request time through a `CompositeStore`.
  `--print-url` headless flow on Anthropic; rejected for Codex
  (no headless paste-back; SSH operators port-forward 1455).
  `routectl whoami` reports stored token expiry. `SecretToken`
  newtype zeroizes on drop with a redacted `Debug`.

- **OAuth runtime refresh + 401 recovery.** Lazy refresh at egress:
  `oauth://` resolution checks near-expiry (300s lead -- matches the
  codex CLI 5-minute refresh window), acquires a
  per-provider mutex, double-checks under the lock so concurrent
  gets collapse to one refresh per window, persists atomically.
  New `Provider::on_auth_failure` hook lets a 401 force-rotate the
  token and retry the same provider once. New `routectl logout
  <provider>` and `routectl refresh <provider>` ops subcommands.
  Migration: replace `env://ROUTECTL_ANTHROPIC` with
  `oauth://anthropic` after running `routectl login anthropic`
  once.

- **claude-code as a first-class gateway client.** Implements the
  Anthropic-published gateway contract for Anthropic Messages, so
  claude-code with `ANTHROPIC_BASE_URL=http://routectl` works
  without silent capability downgrades. New per-provider
  `forward_client_headers: Vec<String>` opt-in for `x-claude-code-*`
  attribution headers (defaults to drop). New `POST
  /v1/messages/count_tokens` proxy with an explicit 8-field body
  allowlist (first-target-only dispatch -- tokenizer correctness;
  no fallback walk). `GET /v1/models` skips alias keys with `*` so
  the picker does not show unroutable globs. New per-dialect
  `ErrorEnvelopeShape::{Anthropic, OpenAi}` so each ingress emits
  its native error shape. New `Error::NotImplemented(provider, op)`
  maps to 501 (covers `count_tokens` on Bedrock today).

- **Server-side emulation of `context-management-2025-06-27`.**
  New per-provider `context_management: bool` on `[providers.X]`
  of kind `anthropic-api` (default false). When true, routectl
  caches thinking blocks observed in upstream responses (bounded
  LRU at 1000 entries + 60-minute TTL, keyed by `(provider_id,
  tool_use_id)`), re-injects them on next-turn requests per the
  `clear_thinking_20251015` keep policy, and strips the beta
  header + body field on egress. Unblocks Anthropic-API-shaped
  upstreams (e.g. DeepSeek `/anthropic`) that demand thinking
  echoback but do not implement the beta natively. Composes with
  `history_reasoning`: strip on canonical messages first, inject
  on wire last.

- **Forward-compat for unknown Anthropic SSE block types.**
  `Other(Value)` catchalls on three strict-tagged enums
  (`SseEvent`, `SseContentBlockStart`, `SseDelta`) plus
  `OpenBlockKind::Unknown` in the SSE state machine, so an
  unrecognized `content_block.type` no longer crashes the stream
  and walks the fallback chain. A `#[serde(skip)] opaque_events`
  carrier on `ChatChunk` captures the upstream event bytes
  (value-preserving / semantically lossless for valid JSON); the
  matching Anthropic ingress re-emits them so strict clients (citation
  links, search-status UI) see the full upstream wire. Bounded
  caps (256 KB / 10000 deltas per block) downgrade overflow
  silently with a WARN. Bedrock-Invoke inherits the fix free;
  Bedrock-Converse streaming forward-compat is a separate task.

- **Per-provider `unsupported_features` filter + cross-alias
  resolution.** Operator-declared list on `ProviderRuntimePolicy`;
  the router strips date suffixes from request `tools` to derive
  feature keys and removes providers whose declared list intersects
  request features BEFORE dispatch. Eliminates wasted Bedrock
  round-trips for `web_search_*`-bearing requests; when the filter
  eliminates every provider the request fails 501 instead of
  cascading 400s. Cross-alias resolution lets a chain entry
  reference another alias key (not just a model nickname); cycle
  detection runs at startup with a runtime depth cap of 8.

- **`probe_max_tokens` fast-fail on 429 / 529.** New `[retry]
  probe_max_tokens` knob (default 1, 0 disables). claude-code's
  `max_tokens=1` availability probes used to walk the full fallback
  chain on rate-limit; a request with `max_tokens <=
  probe_max_tokens` now skips retry AND fallback on 429/529 and
  returns the status immediately. Other errors keep the normal
  retry / fallback path; real requests above the threshold are
  unaffected.

- **`[log]` config block** for runtime knob fallbacks.
  `trace_headers`, `trace_body_bytes`, `redact_prompts` gain a
  config-side default. Resolution per knob: env wins when set,
  then `[log]` if set, then the hardcoded default. `ROUTECTL_LOG`
  stays env-only because it must reach the tracing subscriber
  before config loads.

- **Opt-in 4-direction HTTP header tracing**
  (`ROUTECTL_TRACE_HEADERS=1`). Emit headers on all four hops --
  ingress in/out, egress in/out -- routed through the existing
  `log_safe` redaction so bearer JWTs and API keys are masked.
  `scripts/capture_fixtures.sh` consumes the new format.
  `docs/DEVELOPMENT.md` documents the toggle.

- **Stream-error terminator emission.** When an egress stream
  errored mid-stream, the ingress used to drop the SSE channel
  without a terminator; multi-turn SDKs interpret the silent
  disconnect as truncation and retry up to 5 times. Both ingress
  dialects now emit a dialect-appropriate terminal event before
  closing (Anthropic `event: error`; OpenAI `data: {"error":{...}}`
  then `data: [DONE]`). Errors forwarded to clients are sanitized
  via `sanitize_stream_error_for_client`: only `upstream stream
  error (HTTP <status>)` reaches the wire so per-tenant existence
  hints stay out of the client-visible payload.

### Changed

- **anthropic-api egress: STRIP unsigned `thinking` blocks instead
  of REJECTing.** The 400-on-missing-signature check broke
  cross-provider fallback (a turn handled by deepseek with its own
  signature format, then a turn that walks to Anthropic) and SDKs
  that drop `signature` on serialization.
  `validate_replay_invariants` -> `normalize_replay_invariants`,
  returning `Cow<'a, [Message]>` so unmodified requests pay zero
  clone cost. `history_reasoning = "preserve"` opts a model out of
  the strip (required for upstreams like DeepSeek `/anthropic`
  that demand unsigned thinking echoback). One structured WARN
  fires per request when stripping occurs; block content is never
  logged.

- **Effort clamping is now uniform across all egresses** when
  `effort_levels` is non-empty. Anthropic-API and Bedrock now
  consult the model's declared `effort_levels` and clamp the
  caller's effort to the nearest supported value (rounding toward
  the most capable when above the declared maximum, the least
  capable when below the minimum). Empty list keeps the
  pass-through default for OpenRouter-style providers that perform
  their own effort translation. Shared `clamp_effort_to_supported`
  helper in a new `routectl-providers::effort` module.

### Fixed

- **`thinking` stripped when `tool_choice` forces tool use.**
  Anthropic Messages and Bedrock Converse reject `thinking` paired
  with `tool_choice = {type:"any"|"tool"}`. Strip `thinking` (not
  `tool_choice`) so caller intent to force a named tool is
  preserved; `auto`, `none`, absent are unaffected.

- **Stale `extra_headers` doc references renamed to
  `header_extras`.** Field was renamed in v0.6.0; a few snippets
  and the example config still referenced the legacy spelling.

- **`cache_control` system-block drop demoted from WARN to
  DEBUG.** OpenAI Responses has no equivalent surface; the strip
  is correct, the WARN level just trained operators to ignore
  real WARNs.

- **CI clippy gate tightened to `--all-features`.** The clippy
  steps in `ci.yml` and the local pre-commit hook were labeled
  "all features" but omitted the flag, so feature-gated test files
  never type-checked under the strict gate; tightening exposed
  pre-existing breakages in `live_matrix.rs`.

- **Doc-vs-code currency sweep.** `[providers.X]` / `[models.X]`
  field placements, `adaptive_thinking` ->
  `supports_adaptive_thinking` rename references, the
  `stream_first_byte_timeout_ms` resolution table (now three
  tiers: model > provider > global), DeepSeek `context_management`
  example base URL with `/anthropic` suffix, the
  `header_extras["anthropic-beta"]` mechanism, the corrected
  `effort_levels` clamping description, the `anthropic.rs` ->
  `anthropic/{mod,parse,render,stream}` directory split, the
  corrected `MAX_LOG_BODY_EXCERPT` size in `LOGGING.md`. Cross-doc
  links converted from bare backtick text to Markdown link syntax.

### Removed (BREAKING)

- **`thinking` and `effort` on `[models.X]`** -- replaced by three
  declarative capability fields: `supports_adaptive_thinking`
  (bool, selects the adaptive vs legacy thinking wire shape),
  `effort_levels` (array, default `["low","medium","high"]`;
  drives clamping; empty = pass-through), `max_thinking_budget`
  (u32 tokens, default 0 = no cap). Migration: declare
  capabilities explicitly on each `[models.X]` block per the
  vendor docs. The `EffortLevel` enum and the
  `merge_reasoning_defaults_into` helper are deleted.

- **`adaptive_thinking` on `[providers.X]` of kind
  `anthropic-api`** -- the egress now reads
  `supports_adaptive_thinking` from `RoutectlInternal` per request.
  `Bedrock-Invoke` and `Bedrock-Converse` keep the static
  `adaptive_thinking` field on `BedrockConfig` because Bedrock
  model IDs do not carry the same Anthropic-vs-Bedrock split.

- **`fallback_on_status` on `[retry]`** -- replaced by the
  two-field `retry_allowlist` / `retry_denylist` schema (mutually
  exclusive at config-load). With both unset (the new default)
  every 4xx / 5xx falls back, which is strictly more permissive
  than the previous 15-code default and still covers Cloudflare
  extended 5xx codes (520-527, 530); operators wanting the narrow
  behavior set `retry_allowlist` explicitly. No back-compat shim:
  configs using `fallback_on_status` need a one-line rename.

### Security

- **OAuth callback rate limiting** (two-window guard).
  Per-source-port (30 hits / 10s) AND listener-wide (60 hits /
  10s) on the loopback callback, so a co-resident process spraying
  ephemeral ports cannot drown a legitimate browser callback during
  the 120s login window. Memory bounded (256-entry LRU + capped
  VecDeque). State-valid browser callbacks bypass the tracker
  entirely.

- **OAuth refresh hygiene.** The OAuth HTTP client disables
  redirect-following so a 307/308 from the IdP cannot replay the
  refresh-token POST to a different host. Refresh-flow errors and
  JSON parse errors omit upstream body excerpts (some IdPs reflect
  request fields in error envelopes; refresh bodies carry the
  long-lived refresh token).

- **Anthropic upstream-error body excerpts sanitized in WARN
  logs.** The 4xx / 5xx WARN logs in `complete()` and `stream()`
  used to emit `body_excerpt = %msg` directly from the upstream
  message; an upstream returning CRLF in `error.message` could
  forge log lines on text-format tracing subscribers.

- **`capture_fixtures.sh --out` rejects symlink components.** A
  dangling symlink under `captured/` could let fixture writes
  (which carry raw upstream headers) land outside the gitignored
  tree. A per-component `[ -L ]` walk now runs before physical
  resolution. `--allow-unsafe-out` still bypasses the check for
  legitimate symlink-traversal use cases.

## [0.6.0] - 2026-05-20

The big v0.6 release: layered provider + model config, dispatch
hygiene fixes, openai-compat normalization, and a wave of dogfood
fixes from daily live use.

### Added

- **Layered provider + model config**. `[providers.X]` carries
  transport-wide knobs (auth, base URL, runtime gates) and
  `[models.X]` carries per-model behavior (reasoning, dialect,
  quirks). Two fields live on BOTH layers and merge at dispatch
  time -- `header_extras` and `payload_extras` -- with model winning
  on key collision and `anthropic-beta` comma-unioning across all
  sources. The router's `apply_layered_overlays` helper runs the
  merge before calling `provider.complete()` / `provider.stream()`
  so the `Provider` trait surface stays stable across all four
  concrete providers (openai-compat, anthropic-api, bedrock,
  openai-responses).

- **`[models.X]` first-class TOML table**. Required: `provider`
  (key in `[providers.X]`), `upstream` (wire model id). Optional:
  `thinking` (bool or `"adaptive"`), `effort` (enum), `reasoning_dialect`,
  `history_reasoning`, `additional_request_fields`, `anthropic_beta`,
  `stream_first_byte_timeout_ms`, `header_extras`, `payload_extras`,
  `selectable`.

- **Suffix-glob alias keys** in the unified `[aliases]` table:
  `"claude-opus-*" = "heavy"` matches any wire model starting with
  `claude-opus-`. Lookup precedence: exact match > longest matching
  prefix > `default`. Alias values are `String | Vec<String>` --
  single string is a one-entry chain, list is a fallback chain.

- **`Router::new` precomputes** a `BTreeMap<String, Arc<ResolvedModel>>`
  from `[models]`, so dispatch is one O(1) lookup per hop. Unknown
  nicknames in alias chains fail at startup, not at first request.

- **Tracing dispatch events** carry `model = <nickname>` alongside
  `provider = <provider_name>` for per-model triage.

- **`anthropic-beta` HTTP header lifted into canonical
  `req.anthropic_beta`**. The Anthropic TypeScript SDK translates
  the `betas: [...]` SDK option into an `anthropic-beta: a,b,c` HTTP
  header (not a body field); claude-code uses this surface for
  first-party betas (context-management, prompt-cache-1h,
  adaptive-thinking, ...). routectl now lifts the header so the
  egress emits it in the upstream body (Anthropic accepts either
  surface).

- **`POST /v1/messages` openai-responses provider** (default-on
  `openai-responses` Cargo feature). ChatGPT Codex endpoint via
  `chatgpt-oauth` bearer JWT. Stream-only (`complete()` forces
  `stream:true` and drains SSE to `response.completed`). Flat
  Responses-shape tools and `tool_choice`. `instructions` field
  always serialized (the server 400s if absent).

- **Operator-owned Bedrock allowlists** -- `[bedrock] allowed_betas`
  and `[bedrock] allowed_body_fields` in TOML. Filters the body's
  `anthropic_beta` array (Invoke and Converse) and any forward-compat
  body fields the Anthropic ingress sweeps in (`mcp_servers`,
  `diagnostics`, `context_hint`, `speed`, ...). routectl ships no
  built-in default; AWS schema drift is operator-tracked. Empty list
  (or omitted `[bedrock]` section) is pass-through for discovery --
  bring up routectl, observe sent flags via
  `ROUTECTL_LOG=routectl_providers::bedrock=trace`, populate the
  lists. `examples/bedrock.toml` ships the empirical 2026-05-12
  baseline (16 betas + 16 body fields).

- **`history_reasoning` per-provider knob** on `[providers.X]` of
  type `openai-compat`. Three values: `auto` (default; defer to the
  dialect's strip-vs-preserve default), `strip` (required for
  DeepSeek v3 and vLLM <= 0.6), `preserve` (required for DeepSeek
  v4+ and vLLM 0.7+ hosts that 400 on missing echo-back).
  Per-dialect preserve impls: DeepSeek and vLLM render
  `reasoning_content` scalars; OpenRouter renders typed
  `reasoning_details[]`; OpenAI and Passthrough are no-ops.

- **Per-provider and per-model timeout overrides**:
  `request_timeout_ms` and `stream_first_byte_timeout_ms`. Resolution
  priority: per-model > per-provider > global `[retry]`. Eliminates
  alias-level repetition (e.g. NIM cold-start, Opus 4.7
  high-effort).

- **CF extended 5xx range in default `fallback_on_status`**:
  `[408, 429, 500, 502, 503, 504, 520-527, 530]`. Cloudflare-fronted
  upstreams (opencode.ai, openrouter.ai) surface upstream-origin
  failures via 520-527; without these in the default list, a single
  520 would kill a request even when a sibling provider in the chain
  could have served it.

- **`ROUTECTL_TRACE_BODY_BYTES` env var** to override the 16 KB
  TRACE body cap at process start. Set to 1 MB (`1048576`) for
  live-traffic fixture capture; real claude-code requests routinely
  exceed 16 KB. Resolved cap is announced once at server boot.

- **`scripts/capture_fixtures.sh`** -- operator script that drains
  the TRACE log into per-request fixture directories under
  `crates/routectl-cli/tests/fixtures/captured/` (gitignored). Atomic
  writes via `.tmp.<id>.XXXXXX` rename pattern.

- **`docs/PROVIDER-QUIRKS.md`** -- operator-facing config guide.
  Per-model rows for Anthropic Opus 4.7+ (adaptive thinking),
  DeepSeek v4 (echo-back), vLLM 0.7+, NIM (reasoning_effort gate +
  cold-start cushion), Anthropic / Bedrock / OpenRouter / OpenAI.
  Cross-cutting timing notes, multi-host fallback chain examples,
  troubleshooting matrix.

- **`SECURITY.md`** -- vulnerability disclosure policy.

### Fixed

- **Per-model circuit breaker isolation**. Two `[models.X]` rows
  pointing at the same `[providers.X]` now have independent breaker
  counters and RPM buckets. State is keyed by `[models.X]` nickname,
  not by provider name -- a single flaky model no longer trips the
  breaker for every healthy sibling on the same transport.

- **Bedrock SSO probe deduplication** across models on one
  `[providers.X]`. The factory now caches resolved AWS credentials
  per provider name; building 5 Bedrock models on one provider hits
  the credential chain once instead of 5x.

- **Alias chain validation at startup**. `serve` and `routectl test`
  reject `[aliases]` chains pointing at unknown OR
  `selectable = false` `[models.X]` nicknames before the server
  binds, instead of silently returning `UnknownAlias` at first
  request time. Validator accumulates every offending alias/nickname
  pair into one consolidated error.

- **Per-model `header_extras` reaches the wire**. The merged value
  (provider + model + ingress) lands on `req.anthropic_beta` and the
  Anthropic egress emits one comma-unioned `anthropic-beta` HTTP
  header.

- **Anthropic legacy thinking budget clamp**. Drop legacy
  `thinking: Enabled` when `req.max_tokens <= 1024` (Anthropic
  requires `max > budget`, floor `budget >= 1024`); enforce both the
  1024 floor and the `max - 1` ceiling on every Enabled emission
  path. Caught live on probe-sized requests (e.g. title-generation,
  topic-summary, "continue?" prompts with `max_tokens=64`) when the
  operator's per-model config carried `thinking = true effort =
  high`. Bedrock Invoke + Converse share the helper transitively.

- **openai-compat: strip vendor envelope + lift usage sub-bags**.
  Anthropic-shape ingress + openai-compat upstream was bleeding
  envelope fields (`object`, `system_fingerprint`, `cost`) and four
  DeepSeek/OpenAI usage sub-bags (`prompt_cache_hit_tokens`,
  `prompt_cache_miss_tokens`, `prompt_tokens_details`,
  `completion_tokens_details`) back to the Anthropic-shape response.
  Now lifted to canonical `Usage.reasoning_tokens` and
  `Usage.cache_read_input_tokens` and stripped from the extras
  catchall. Mirrored on the SSE path before serde (`UsageDelta` has
  no extras flatten).

- **`tool_choice` shape mismatch: OpenAI bare-string -> Anthropic
  tagged-enum**. Anthropic's Messages API and Bedrock-Invoke reject
  `tool_choice: "auto"` with a 400. The Anthropic-API egress now
  translates `"auto" | "none" | "required"` and the OpenAI
  `{"type":"function","function":{"name":"X"}}` object map to
  Anthropic-shape `{"type":...}`. Anthropic-shape inputs pass
  through unchanged.

- **Top-level `system` leaks onto openai-compat wire**. The OpenAI
  ingress lifts wire `role: "system"` into canonical `req.system`
  (Anthropic-shape top-level field). The openai-compat egress now
  performs the inverse lower: prepends a synthetic
  `role: "system"` message and strips the top-level `system` key.
  Strict hosts (NVIDIA NIM) used to 400 with
  `Validation: Unsupported parameter(s): system`.

- **OpenAI ingress: `reasoning_content` keys coalesced before
  schema deserialization**. DeepSeek-shape `reasoning_content` was
  arriving unmerged on `messages[].reasoning_content`, missing the
  canonical `reasoning` lift on multi-turn echo-back. Added
  pre-deserialization coalescer mirroring the response-side
  `merge_reasoning_keys`.

- **`prompt_tokens` translation: cache_creation/cache_read summed
  into Anthropic streaming usage**. The Anthropic SSE response now
  captures `message_start` input usage, sums `input_tokens +
  cache_creation_input_tokens + cache_read_input_tokens` into the
  closing `message_delta` `UsageDelta`, and exposes per-TTL cache
  breakdown via field-level merge.

- **Stop-sequence end-to-end**. Preserve the matched stop sequence
  through canonical so the Anthropic ingress can emit
  `stop_reason: "stop_sequence"` + `stop_sequence: "<value>"` instead
  of collapsing to `end_turn`. Previously broke claude-code
  structured-output flows (stop_sequence fences the output) by
  flagging `is_error: true` on the result envelope. Bedrock Converse
  is a known follow-up -- AWS surfaces the matched sequence via
  `additionalModelResponseFields` only when the request opts in.

- **Router log clarity: chain-exhausted vs fallback hop**. Both
  `complete()` and `stream()` previously WARNed "fallback to next" on
  every fallbackable terminal error including the LAST chain entry.
  Now emits "chain exhausted; no fallback target available; request
  will fail" when no next target exists.

- **WARN at egress when canonical reasoning is silently stripped**.
  Operator visibility for the `history_reasoning = "auto"` + strip
  dialect case where the request actually carried reasoning.

- **Alias glob double-parse**. `Router::new` parsed each `*`-bearing
  alias key twice; pattern is now reused via let-binding.

- **`routectl test` help text and module docstring** referenced the
  removed `provider:model` direct-target form. Now references the
  alias key / model nickname inputs the v0.6.0 router accepts.

### Security

- **Log redaction at TRACE level**. `ROUTECTL_LOG_REDACT_PROMPTS=1`
  walks every traced body and replaces known prompt-bearing fields
  (text blocks, system, instructions, tool_use input,
  function_call arguments, refusal blocks, image source data,
  image_url data URIs, Bedrock Converse `toolUse.input`) with
  `<redacted len=N>` placeholders while preserving structural
  fields (model, tools, sampling params, finish_reason, usage).
  Read once on first traced body; one-shot startup log line
  reports the resolved value.

- **gitleaks workflow + `.gitleaks.toml`** -- secret scan on every
  PR + push + weekly full-history sweep. Inherits the default rule
  set; allowlists Cargo.lock, target/, and the captured/ fixture
  directory.

- **CI hygiene**: pinned every third-party action to a commit SHA
  with a version comment (floating tags can be retroactively moved
  by an attacker), added `permissions: contents: read` at the
  workflow level.

### Removed (BREAKING)

- `enabled` on `[models.X]` -- renamed to `selectable` to free the
  TOML key for the flattened `ReasoningDefaults::enabled` (reasoning
  on/off). Operators wanting per-model reasoning-off semantics now
  write `enabled = false`; `selectable = false` is the routing-
  disable knob.
- `[aliases.X.retry]` per-alias retry overrides -- removed when
  `[aliases]` collapsed into a flat wire-string -> nickname-or-chain
  map. Use the global `[retry]` table; per-error-class caps
  (`retry_on_429`, `retry_on_5xx`, `retry_on_network`) cover the
  knobs operators previously set per alias.
- `type` field on `[providers.X]` -- renamed to `kind` to disambiguate
  from the `type` Rust keyword.
- `model_id` on `[providers.bedrock-X]` -- moves to
  `[models.X].upstream`. Bedrock providers are no longer 1:1 with a
  model.
- `thinking`, `enabled`, `adaptive_thinking` on `[providers.X]` --
  move to `[models.X]`. Per-provider was the wrong granularity; two
  models on one provider can now carry different reasoning floors.
- `additional_model_request_fields` on `[providers.bedrock-X]` --
  renamed to `additional_request_fields` and moved to `[models.X]`.
- `default_extras` on `[providers.X]` -- moves to `[models.X]`.
- `[ingress.X.aliases]` per-ingress alias maps -- collapsed into the
  unified top-level `[aliases]` table.
- `[aliases.X] chain = [...]` sub-tables -- chains live as list
  values directly in `[aliases]`: `heavy = ["opus", "sonnet"]`.
- top-level `default_model = "..."` -- replaced by `default = "..."`
  inside `[aliases]`.
- `[bedrock] anthropic_beta` -- renamed to `[bedrock] allowed_betas`.

### Deferred

- Per-model `default_extras` and `chat_template_kwargs` deferred
  until the egress wiring lands; they will return as `[models.X]`
  fields in a future release. The provider-side fields
  (`OpenAiCompatConfig::default_extras`, `chat_template_kwargs` on
  the wire) are unaffected -- callers continue to send them
  per-request via `provider_extras`.
- Bedrock Converse stop_sequence round-trip -- AWS surfaces the
  matched sequence only when the request opts into
  `additionalModelResponseFieldPaths`. Tracked as a follow-up.
- OAuth token hot-rotation. routectl reads `ROUTECTL_ANTHROPIC` once
  at startup; a credentials.json rotation by claude-code requires a
  routectl restart. Manual snapshot + restart workflow today;
  inotify-based file-watch is staged for a future release.

### Migration

No automated migration tool; old configs hit raw serde errors at
startup. Hand-edit your TOML against the new shape -- see
`examples/config.toml` for a complete reference.

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
- Two provider classes: `openai-compat` (6 reasoning dialects:
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

[0.8.0]: https://github.com/meepolabs/routectl/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/meepolabs/routectl/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/meepolabs/routectl/compare/v0.4.0...v0.6.0
[0.4.0]: https://github.com/meepolabs/routectl/compare/v0.2.0...v0.4.0
[0.2.0]: https://github.com/meepolabs/routectl/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/meepolabs/routectl/releases/tag/v0.1.0
