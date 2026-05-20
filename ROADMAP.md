# routectl roadmap

## v0.1.0 (DONE -- 2026-05-06)

Goal: a binary you can `cargo install`, point clients at, and have it
route real LLM calls with fallback + reasoning normalization.

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
across OpenRouter / NIM and additional OpenAI-compatible hosts.
Release binary still <6MB.

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

## v0.3.0 (DONE) -- Native AWS Bedrock + Anthropic header polish

Two themes landed together because they share the Anthropic Messages
wire format:

1. **Native `bedrock` provider** -- talks to
   `bedrock-runtime.<region>.amazonaws.com` directly, no gateway in
   the middle. SigV4 signing via `aws-sigv4`, full AWS credential
   chain via `aws-config` (env / static / profile / SSO / web identity
   / IRSA / IMDS), plus a `bearer-key` flavor for the AWS console's
   short-term token. `InvokeModel` body shape (Anthropic Messages
   today; per-vendor as new vendors are added) and `Converse`
   transport are wired; v0.5.x adds full Anthropic-on-Converse body
   translation. Streaming responses decoded from the AWS eventstream
   binary frame format. Gated behind a default-on `bedrock` Cargo
   feature so library consumers can opt out of the AWS dep tree with
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

## v0.4.0 (DONE) -- API spec independence

Two ingress dialects (OpenAI Chat Completions + Anthropic Messages),
one canonical internal request shape, N egress providers. Any client
that speaks either wire format can route through any backend.

1. **`POST /v1/messages` Anthropic ingress** with full tool-call
   round-trip, thinking blocks + signature preservation, typed SSE
   events (`message_start` / `content_block_*` / `message_delta` /
   `message_stop`), server-side model-id -> alias mapping
   (`[ingress.anthropic.aliases]`), and `x-routectl-alias` header
   override.
2. **Canonical internal shape** absorbs Anthropic features
   losslessly: typed `ContentPart` (Text / Image / ImageUrl /
   Document / ToolUse / ToolResult / Thinking / RedactedThinking /
   Other), typed `SystemContent` (Text or Blocks), typed `ToolDef`
   (Custom / Other), top-level `cache_control` and `anthropic_beta`,
   and `Usage` cache stats. Forward-compat catchalls
   (`ContentPart::Other`, `ToolDef::Other`, `ContentBlock::Other`)
   pass unknown Anthropic block types through verbatim on the
   all-Anthropic path. `cache_control::validate` enforces the
   4-breakpoint cap and 1h-before-5m TTL ordering at ingress.
3. **Listener-side auth** via static config tokens (`[server.auth]
   tokens = [...]`) accepts both `x-api-key` and `Authorization:
   Bearer`. Inbound auth is fully decoupled from upstream credentials
   (no bridging, no token storage).
4. **`strict_translation = false` (default)** -- lossy seams emit
   `tracing::warn!`. Set `[server] strict_translation = true` to
   upgrade to 400 Bad Request on dropped fields.
5. **Adaptive thinking** for Anthropic Opus 4.7+ via per-provider
   `adaptive_thinking = true` -- rewrites to the new
   `thinking: {type: "adaptive"}` + `output_config: {effort: "..."}`
   shape.
6. **Universal 4xx/5xx logging** -- ingress / egress / upstream-error
   bodies all available behind tracing levels with `request_id`
   correlation.

## v0.5.0 (DONE) -- Translation-pipe hardening + dogfood fixes

Bug-fix and ergonomics cycle driven by daily dogfood of the v0.4
surface. Real-world wire-shape mismatches surfaced and got pinned;
operator-facing config knobs landed where compiled defaults didn't
fit every host.

1. **Translation-correctness fixes**:
   - `tool_choice` shape coercion in the Anthropic-API egress (OpenAI
     bare-string and OpenAI function-object -> Anthropic tagged enum;
     Anthropic-shape passes through). Closes the Bedrock 400 path.
   - Top-level `system` field lowered back to a synthetic
     `role: "system"` message on the openai-compat egress. Fixes
     strict hosts (NIM) rejecting the Anthropic-shape leak.
   - `prompt_tokens` translation sums `input + cache_creation +
     cache_read` on Anthropic ingress streaming usage.
   - OpenAI ingress coalesces `reasoning_content` keys before schema
     deserialization (mirrors response-side merge).

2. **Per-provider config knobs**:
   - `history_reasoning = auto | strip | preserve` for openai-compat
     hosts. DeepSeek v4 and vLLM 0.7+ require preserve; older
     versions and DeepSeek v3 require strip; opt-in per provider.
   - `request_timeout_ms` and `stream_first_byte_timeout_ms` at the
     provider level, with alias > provider > global resolution.
     Removes alias-level repetition for uniformly slow upstreams.

3. **Operator visibility**:
   - WARN at openai-compat egress when canonical reasoning is
     silently stripped (auto-mode + strip dialect + carrying
     reasoning).
   - `docs/PROVIDER-QUIRKS.md` operator config guide -- per-model
     rows, troubleshooting matrix, alias > provider > global
     resolution explained.

4. **Operator-owned Bedrock allowlists** -- `[bedrock] allowed_betas`
   and `[bedrock] allowed_body_fields`. routectl ships no built-in
   default; AWS schema drift is operator-tracked, not release-bound.
   Empty list = pass-through (discovery default).
   `examples/bedrock.toml` ships the empirical baseline. Replaces
   the prior hardcoded `BEDROCK_ACCEPTED_BETAS` const (BREAKING
   rename: `[bedrock] anthropic_beta` -> `allowed_betas`).

Deferred (not in this milestone):

- `routectl doctor` subcommand (active credential probe + IAM action
  surfacing for Bedrock).
- `Converse` body translation for non-Anthropic Bedrock vendors
  (Mistral, Llama, Cohere). Anthropic-on-Converse landed in v0.5.x.
- WARN on `default_model` fallthrough (currently DEBUG; visibility
  feature request).

## v0.6.0 (DONE) -- Layered config + dispatch hygiene

Schema refactor that splits transport-wide and per-model concerns,
plus a wave of openai-compat normalization fixes from daily
dogfood.

1. **Layered provider + model config**. `[providers.X]` (transport-
   wide: auth, base URL, runtime gates) and `[models.X]` (per-model:
   reasoning, dialect, quirks). `header_extras` and `payload_extras`
   live on both layers and merge at dispatch time, with
   `anthropic-beta` comma-unioning across ingress + provider + model.
2. **Per-model circuit breaker isolation**. Two models on one
   provider now have independent breaker counters and RPM buckets;
   state is keyed by `[models.X]` nickname.
3. **Unified `[aliases]` table** with suffix-glob keys and
   `String | Vec<String>` values (chain or single). `default = "..."`
   is the catch-all.
4. **`POST /v1/messages` openai-responses provider** -- ChatGPT
   Codex endpoint via chatgpt-oauth bearer JWT. Stream-only.
5. **openai-compat normalization**: strip vendor envelope (`object`,
   `system_fingerprint`, `cost`), lift `reasoning_tokens` and
   `cache_read_input_tokens` from usage sub-bags. Default
   `stream_options.include_usage`. Tightened log-shape and trace-
   field visibility.
6. **Anthropic legacy thinking budget hygiene**: clamp to the
   1024 floor and `max - 1` ceiling, drop legacy thinking when
   `max_tokens <= 1024`. Caught probe-sized requests on Opus 4.7+
   adaptive chains.
7. **Stop-sequence preservation end-to-end** so claude-code
   structured-output flows see `stop_reason: "stop_sequence"` +
   `stop_sequence: "<value>"` instead of `end_turn`.
8. **CF extended 5xx range** in default `fallback_on_status`
   (520-527, 530). Cloudflare-fronted upstreams surface origin
   failures via this range.
9. **Per-provider and per-model `request_timeout_ms` /
   `stream_first_byte_timeout_ms`** with resolution priority
   model > provider > global.
10. **`ROUTECTL_TRACE_BODY_BYTES`** env-overridable TRACE body
    cap for live-traffic fixture capture; default 16 KB.
11. **CI: gitleaks workflow + `.gitleaks.toml`** + SECURITY.md +
    pinned action SHAs + scoped `permissions: contents: read`.

## v0.7.0+ (planned) -- Latency-aware routing + observability

Originally scoped for v0.3.1; deferred to focus earlier milestones on
correctness work.

1. **Latency-based routing** across multiple healthy providers in a
   chain (sliding-window p95 tracking, weighted random).
2. **Spend tracking** -- per-provider request count + token usage
   metric, exposed via `/v1/metrics` for Prometheus scrape.
3. **OAuth token hot-rotation** via inotify file-watch +
   401-retry. routectl reads `ROUTECTL_ANTHROPIC` once at startup
   today; a credentials.json rotation by claude-code requires a
   restart. Native file-watch eliminates the manual snapshot
   workflow.
4. **Bedrock Converse stop_sequence round-trip**. AWS surfaces the
   matched sequence via `additionalModelResponseFields` only when
   the request opts into `additionalModelResponseFieldPaths`.

## Post-v0.7 (deferred / never)

- Caching layer (use a proxy if you want this)
- Web UI / config editor (CLI-only by design)
- Server mode (multi-user, TLS, persistent state) -- out of scope, fork if you want it
- Cost-aware routing (overlap with LMSYS RouteLLM, different product)
- Atropos / RL trajectory hooks (overlap with Hermes Agent, different product)
- Distribution: `cargo dist` / Homebrew tap (manual `cargo build --release` for now)
- Live matrix in CI (currently runs on demand against real provider keys)
