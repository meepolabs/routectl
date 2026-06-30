# Provider Configuration Guide

Per-model config tips for routectl operators. Most upstream LLMs work out of
the box with default routectl config -- the entries below cover the cases
where you need to flip a knob.

If your model isn't listed and works fine, you don't need anything from this
doc. If it 4xxs or behaves weirdly, find the matching row.

## Quick reference

| If you're using... | Set on... |
|---|---|
| **Claude Opus 4.7+ / Sonnet 4.7+** | `[models.X] supports_adaptive_thinking = true` |
| **Any thinking model + high-effort latency** | `[providers.X] stream_first_byte_timeout_ms = 300000` (every alias hitting this provider inherits) |
| **DeepSeek v4 / v4.1 (any host)** | `[models.X] history_reasoning = "preserve"` |
| **DeepSeek v3 / older vLLM** | (default `history_reasoning = "auto"` strips for you) |
| **NVIDIA NIM hosting DeepSeek** | callers must send `reasoning_effort = "high"` per request (the operator-side `payload_extras` knob is deferred -- callers can still set effort via wire `reasoning.effort`) |
| **NIM cold-start streaming** | `[providers.X] stream_first_byte_timeout_ms = 180000` (3 min) |
| **Anthropic + 1M-context beta** | `[providers.X] header_extras = { "anthropic-beta" = "context-1m-2025-08-07" }` |
| **OAuth bearer to Anthropic** | `[providers.X] auth_kind = "oauth-bearer"` + the matching beta header |
| **DeepSeek /anthropic endpoint + claude-code context-management** | `[providers.X] context_management = true` |
| **Google Gemini (native)** | `[providers.X] kind = "gemini"` + `api_key_ref = "env://GEMINI_API_KEY"`; safetySettings / topK flow through `payload_extras` |

## Per-model config

### Anthropic Opus 4.7+ (and any future adaptive-thinking models)

The 4.7+ generation rejects the legacy `thinking: {type: "enabled", budget_tokens: N}` shape with a `400 thinking.type.enabled is not supported`. Anthropic moved budget control out of the `thinking` block; the model picks budget from `output_config.effort`.

**Required:**

```toml
[providers.bedrock]
kind   = "bedrock"
region = "us-east-1"
creds  = { kind = "default-chain" }

[models.opus47]
provider          = "bedrock"
upstream          = "us.anthropic.claude-opus-4-7-v1:0"
supports_adaptive_thinking = true        # rewrites to {type:"adaptive"} + output_config.effort
effort_levels     = ["low", "medium", "high", "xhigh", "max"]
```

**Recommended when using `reasoning.effort = "high"` / `"xhigh"` / `"max"`:** Opus 4.7 + max-effort regularly takes 60-90 seconds before first SSE byte. Bump the timeouts on the parent provider so every model routing through it inherits:

```toml
[providers.bedrock]
# ...the fields above, plus:
stream_first_byte_timeout_ms = 300000   # 5 min, applies to every model
request_timeout_ms           = 600000   # 10 min, applies to every model
```

Need different timeouts for different models on the same upstream? For `stream_first_byte_timeout_ms` you can set a per-model override directly: `[models.X] stream_first_byte_timeout_ms = 300000` (resolution is `[models.X]` > `[providers.X]` > `[retry]` > unset). For `request_timeout_ms` there is no per-model tier; split into separate `[providers.X]` entries (e.g. `bedrock-fast`, `bedrock-heavy`) with their own runtime knobs and route each `[models.X]` at the right one.

Resolution priority:
- `stream_first_byte_timeout_ms`: `[models.X]` > `[providers.X]` > `[retry]` > unset (no cap)
- `request_timeout_ms`: `[providers.X]` > `[retry]` > unset (no per-model tier)

**Why opt-in per model, not auto-detect by name:** Anthropic's adaptive-thinking rollout has no clean naming pattern, so a TOML opt-in is more reliable than a regex match.

### DeepSeek v4 / v4.1 (api.deepseek.com, example-deepseek-host, NIM, vLLM-hosted, anywhere)

DeepSeek v4 inverted v3's contract on multi-turn echo-back. v3: 400 if you echo `reasoning_content` in assistant history. v4: `400 reasoning_content in the thinking mode must be passed back to the API` if you don't.

**Required:**

```toml
[providers.example-deepseek-host]
kind = "openai-compat"
base_url = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"

[models.ds-v4]
provider          = "example-deepseek-host"
upstream          = "deepseek-v4-flash"
reasoning_dialect = "deepseek"          # so response reasoning lifts correctly
history_reasoning = "preserve"          # echoes reasoning_content back to upstream
```

**Default behavior (without the knob):** routectl strips reasoning fields from outgoing assistant history (correct for DeepSeek v3). When you upgrade to v4, the strip is wrong and the upstream 400s. Routectl warns at the strip site so you see the loss in logs:

```
WARN openai-compat egress: assistant reasoning_content stripped from outgoing history.
     Set history_reasoning = "preserve" on the provider if your upstream requires
     echo-back (DeepSeek v4+, recent vLLM).
```

### vLLM (recent versions)

vLLM 0.7+ matches DeepSeek v4's echo-back contract. Same fix:

```toml
[providers.my-vllm]
kind = "openai-compat"
base_url = "http://localhost:8000/v1"
api_key_ref = "literal:not-needed"

[models.my-vllm-model]
provider          = "my-vllm"
upstream          = "Qwen3-32B"
reasoning_dialect = "vllm"
history_reasoning = "preserve"          # for vLLM 0.7+
```

For older vLLM (<= 0.6) leave `history_reasoning` unset (defaults to strip).

### NVIDIA NIM (integrate.api.nvidia.com)

NIM's DeepSeek v4 Flash / Pro defaults to **non-thinking mode**. Thinking is enabled per request via top-level `reasoning_effort: "none" | "high" | "max"`. routectl's OpenAI dialect already maps canonical `reasoning.effort` -> `reasoning_effort`; clients that send `reasoning.effort` land thinking on the request.

**To enable thinking on every NIM request:** routectl does not yet
inject `reasoning_effort` into the body unconditionally; callers must
send `reasoning.effort = "high"` (or set the equivalent on the client
side) and the OpenAI-dialect translator forwards it as
`reasoning_effort` to NIM.

```toml
[providers.nim]
kind              = "openai-compat"
base_url          = "https://integrate.api.nvidia.com/v1"
api_key_ref       = "env://NIM_API_KEY"

[models.ds-nim-flash]
provider          = "nim"
upstream          = "deepseek-ai/deepseek-v4-flash"
reasoning_dialect = "openai"
```

**Heads-up: NIM cold-start latency on streaming.** First-byte timeouts of 60s+ are common on NIM. Set the bump on the provider so every model hitting it inherits:

```toml
[providers.nim]
# ...the fields above, plus:
stream_first_byte_timeout_ms = 180000   # 3 min, all NIM-routed models benefit
```

Need different timeouts on different routes? Split into separate `[providers.X]` entries.

### Anthropic (api.anthropic.com)

Default config works for Claude 4.5/4.6 / Haiku 4.5. The two times you need extras:

**1M-context beta** (Sonnet 4.5 with 1M context window):

```toml
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
header_extras = { "anthropic-beta" = "context-1m-2025-08-07" }
```

**OAuth bearer auth** (Claude Code subscription tokens, `sk-ant-oat01-...`):

```toml
[providers.anthropic-oauth]
kind = "anthropic-api"
api_key_ref = "file:///path/to/oauth-token"
auth_kind = "oauth-bearer"
```

For `auth_kind = "api-key"` (default), routectl does not auto-inject beta gates -- declare the ones you need in `header_extras`. For `auth_kind = "oauth-bearer"` on `api.anthropic.com`, the full Claude Code beta set (incl. `oauth-2025-04-20` and `context-1m-2025-08-07`) is auto-injected from `default_claude_code_anthropic_betas()`, so no manual `header_extras` beta list is needed.

### claude-code attribution headers (`X-Claude-Code-*`)

Anthropic documents three gateway-mandatory passthrough headers at
<https://code.claude.com/docs/en/llm-gateway> --
`X-Claude-Code-Session-Id`, `X-Claude-Code-Agent-Id`, and
`X-Claude-Code-Parent-Agent-Id` -- so cost and trace attribution work
when claude-code traffic is fronted by a proxy. The Anthropic ingress
greedy-captures the entire `x-claude-code-*` namespace into the
canonical request; the Anthropic-API egress forwards a name upstream
only when it appears on the per-provider `forward_client_headers`
allowlist:

```toml
[providers.anthropic-managed]
kind        = "anthropic-api"
api_key_ref = "oauth://anthropic"
forward_client_headers = [
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
]
```

Default is empty (drop everything captured) -- safe-by-default for
new providers. Future Anthropic-namespace additions are NOT
auto-forwarded; the operator opts each name in. Synthesizing
client-supplied identifiers in routectl is intentionally out of
scope: synthesizing identifiers on a personal-use claude.ai proxy is
TOS-adjacent, and the operator can already inject any static
identifier via `header_extras` if they want a static value.

### Bedrock (any region)

Both `api_shape = "invoke"` (Anthropic Messages body) and
`api_shape = "converse"` (AWS Converse) are wired for Anthropic
models on Sonnet/Haiku/Opus. Set `supports_adaptive_thinking = true` on Opus
4.7+ models regardless of api_shape.

**Bedrock allowlist (optional, recommended in production).** AWS
strict-schema validation 400s any unrecognized `anthropic_beta` flag
or top-level body field. routectl ships no built-in default; populate
the operator-supplied lists in TOML to gate which entries reach AWS:

```toml
[bedrock]
allowed_betas       = ["context-1m-2025-08-07", ...]
allowed_body_fields = ["anthropic_version", "messages", ...]
```

Empty lists (or omitted `[bedrock]` section) = pass-through (no
filter applied). Use `ROUTECTL_LOG=routectl_providers::bedrock=trace`
to capture sent flags/fields when building the lists. See
`examples/bedrock.toml` for the empirical 2026-05-12 baseline.

**RPM bucket semantics for shared Bedrock providers.** Runtime state
(circuit breaker + RPM token bucket) is keyed by `[models.X]`
nickname, not by `[providers.X]` name. This is the right semantic for
breakers (a flaky model on one Bedrock provider should not trip the
breaker for siblings), but it means `rpm_limit` is per-model:
`rpm_limit = 60` on a provider serving 3 `[models.X]` rows admits up
to 180 RPM in aggregate to the underlying Bedrock account. Operators
with tight Bedrock service quotas should size `rpm_limit` per the
per-model budget, or split sensitive models across multiple
`[providers.X]` blocks (e.g. distinct `[providers.bedrock-sonnet]` /
`[providers.bedrock-haiku]` entries) to keep buckets isolated.

**Example provider + model:**

```toml
[providers.bedrock]
kind      = "bedrock"
region    = "us-east-1"
api_shape = "invoke"
creds     = { kind = "default-chain" }

[models.opus47]
provider          = "bedrock"
upstream          = "us.anthropic.claude-opus-4-7-v1:0"
supports_adaptive_thinking = true
effort_levels     = ["low", "medium", "high", "xhigh", "max"]
```

### OpenRouter

Default works. Two niceties to set:

```toml
[providers.openrouter]
kind = "openai-compat"
base_url = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"
header_extras = {
  "HTTP-Referer" = "https://github.com/your/project",
  "X-Title" = "your-project-name"
}

[models.ds-or]
provider          = "openrouter"
upstream          = "deepseek/deepseek-v4-flash"
reasoning_dialect = "openrouter"
```

The HTTP-Referer / X-Title headers improve OpenRouter's analytics and can affect rate limits on free tiers.

### OpenAI / o-series / GPT-5

Default works. The OpenAI dialect maps canonical `reasoning.effort` -> wire `reasoning_effort` and drops sampling params on reasoning models automatically (driven by `model_profile.rs`).

```toml
[providers.openai]
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
api_key_ref = "env://OPENAI_API_KEY"

[models.o3]
provider          = "openai"
upstream          = "o3"
reasoning_dialect = "openai"
```

### chatgpt-oauth (`openai-responses` provider, `chatgpt.com/backend-api/codex` surface)

The `openai-responses` provider in `chatgpt-oauth` mode **pins the codex CLI client header contract**. The chatgpt.com backend requires requests to the Codex / ChatGPT-Pro responses API to match that contract: the User-Agent literal, the `originator` value, the per-process identity headers, and the OAuth refresh client's own header set must all match what the codex CLI sends. If any of these values drift, the upstream may reject requests or require re-authentication -- this is a client-compatibility constraint, not a recoverable build error.

**Pinned client version (source of truth)**:

- Tag: `rust-v0.136.0` (most recent codex Rust release tag at adoption)
- Commit: `d36a3ead3c896d0552207763ef483262bce9ac73`
- Source: the `version` field on `codex-rs/cli/Cargo.toml` in the upstream codex repo (workspace `version = "0.0.0"` is the tip-of-tree dev placeholder; the tag pin above is what routectl encodes against)

Keep this pin in sync with the codex CLI version routectl targets; treat it as locked.

**Headers that MUST stay in lockstep with codex** (any deviation breaks the client-compatibility contract):

| Header                              | Source in codex-rs                                            | Notes                                                                |
|-------------------------------------|---------------------------------------------------------------|----------------------------------------------------------------------|
| `Authorization`                     | injected by routectl's auth layer per request                 | OAuth bearer JWT (`Bearer <jwt>`); resolved per request from the token store. Redacted to `Bearer [REDACTED]` in TRACE-level outgoing-headers logs. NOT a pinned constant -- the value rotates on every refresh -- but the header is mandatory and absence triggers a 401 immediately. |
| `ChatGPT-Account-Id`                | injected by routectl's auth layer per request                 | The `chatgpt_account_id` claim parsed out of the bearer JWT; mandatory account-routing header. Stable per account. |
| `User-Agent`                        | `login/src/auth/default_client.rs::get_codex_user_agent`      | `<originator>/<build_version> (<os_type> <os_version>; <arch>) <terminal>`; build_version is `CARGO_PKG_VERSION` of the codex binary, not routectl's. |
| `originator`                        | `login/src/auth/default_client.rs::DEFAULT_ORIGINATOR`        | Constant `"codex_cli_rs"` for first-party CLI traffic.               |
| `version` / per-request build tag   | passed through `CodexRequestBuilder` per call                 | Matches the targeted codex CLI build version.                        |
| `session_id`                        | `core/src/client.rs::ModelClientState`                        | Stable per process; never reset within a routectl process lifetime.  |
| `x-codex-installation-id`           | `core/src/client.rs::X_CODEX_INSTALLATION_ID_HEADER`          | Stable per install (persisted under `~/.config/routectl/`).          |
| `x-codex-window-id`                 | `core/src/client.rs::X_CODEX_WINDOW_ID_HEADER`                | Per-window correlation; codex bumps on each new shell window.        |
| `thread-id`                         | `core/src/client.rs::X_CODEX_PARENT_THREAD_ID_HEADER` family  | Per-conversation; new value on every fresh `ChatRequest`.            |
| `x-client-request-id`               | `codex-api/src/endpoint/responses.rs:92`                      | Per-request UUID; carries the `thread_id` for upstream correlation.  |
| `x-openai-internal-codex-residency` | `login/src/auth/default_client.rs::RESIDENCY_HEADER_NAME`     | Set when `--residency us` is configured; absent otherwise.           |

The OAuth refresh client (used for `grant_type=refresh_token` POSTs to `https://auth.openai.com/oauth/token`) carries its OWN header set distinct from the responses-API client; both must carry the pinned codex client headers.

**Defense-in-depth for the bearer JWT**: the `authorization` header on every outgoing request to `chatgpt.com/backend-api/codex` carries an OAuth access-token JWT that embeds `chatgpt_account_id`, `email`, `session_id`, `jti`, and `plan_type`. Routectl's outgoing-headers TRACE log redacts this value to `Bearer [REDACTED]` before any line is emitted (see `routectl_core::log_safe::redact_outgoing_header_values`); the same redaction applies to `x-api-key` and `proxy-authorization`. Operators keep the value local; no log destination ever sees it.

**Cloudflare cookie jar**: the `chatgpt.com` surface sits behind Cloudflare and pins `__cf_bm`, `_cfuvid`, `cf_clearance`, etc. on first contact. The provider attaches a persistent jar (default path `~/.config/routectl/cookies/chatgpt.json`, mode `0600`; override via `ROUTECTL_COOKIE_FILE`) so a cold-start does not pay the challenge cycle on every request. The jar is allowlist-filtered to Cloudflare service-cookie names on both load and save, so a stale on-disk file or a hostile Set-Cookie cannot smuggle account / session cookies into the persistence slot. Mirrors the codex CLI's `with_chatgpt_cloudflare_cookie_store`.

**Refresh tracing**: the OAuth refresh path emits `tracing::debug!` lines tagged with `refresh_token_sha8` (8 hex chars from `sha256(token)[..4]`) so operators can correlate a 401 across logs without ever surfacing a token VALUE. The response leg adds `new_refresh_token_present` and `expires_in`; on failure, `tracing::error!` carries `status`, `error_kind` (`refresh_expired` / `token_endpoint` / `network` / `other`), and `prior_refresh_token_sha8` (the same correlation tag as the pre-POST event, so an interleaving operator can pin which credential triggered which 401). The failure path deliberately does NOT echo any portion of the upstream response body: token-endpoint error envelopes can echo the submitted refresh_token (or mint a new one), and logging the body verbatim would defeat the bearer-redaction contract above. The human-readable error returned to the caller still carries the truncated body for non-refresh paths.

**Cross-link**: when adjusting any of the above, compare against the corresponding upstream source paths under the codex repo's `codex-rs/`:

- Auth client and originator: `login/src/auth/default_client.rs`
- Cookie jar:                  `codex-client/src/chatgpt_cloudflare_cookies.rs`
- Header constants:            `core/src/client.rs`
- Responses-API request:       `codex-api/src/endpoint/responses.rs`

**Compatibility-contract risk**: future deviations (a UA bump on the routectl side that codex did not ship; a missing identity header; a refresh-client header re-ordering) are NOT debuggable as build / wire errors. The first symptom is a refresh endpoint that 401s on every retry. A mismatch with the targeted codex client contract can cause the upstream to reject the refresh token and require the operator to re-authenticate through the ChatGPT web UI. Treat any change here with the same gravity as a database migration on a production system.

## Gemini (native, `kind = "gemini"`)

The native Gemini egress (`generateContent` / `streamGenerateContent`)
replaces the older openai-compat shim. It talks the Gemini REST wire
shape directly, so it does not coerce requests through the OpenAI
schema first.

**Auth model (decision):** API key only, sent as the `x-goog-api-key`
HTTP header (NOT `Authorization: Bearer`). The key is resolved per
request from `api_key_ref`, so a routectl-managed `oauth://` or
`file://` source can rotate it without a daemon restart. Vertex AI /
Google OAuth (ADC, service accounts) is explicitly NOT implemented. It
is reachable later by pointing `base_url` at a Vertex endpoint -- no
new provider kind required.

**Model path shape:** the provider appends the model id and method to
`base_url`:

- non-stream: `{base_url}/models/{model}:generateContent`
- stream:     `{base_url}/models/{model}:streamGenerateContent?alt=sse`

`base_url` defaults to
`https://generativelanguage.googleapis.com/v1beta`.

**thinkingConfig budget mapping:** the canonical `reasoning` controls
map to `generationConfig.thinkingConfig`:

| Canonical input | `thinkingBudget` |
|---|---|
| `reasoning.enabled = false` | (no thinkingConfig emitted -- thinking off) |
| explicit `reasoning.max_tokens = N` | `N` verbatim |
| `reasoning.effort = "<level>"` | budget via the effort table (`minimal=512`, `low=1024`, `medium=8192`, `high=24576`, `xhigh=32768`, `max=128000`) |
| reasoning present, neither set | `-1` (dynamic -- the model picks) |

`includeThoughts` is set to `true` whenever thinking is on and
`reasoning.exclude` is not `true`, so thought summaries stream back and
lift into canonical `reasoning` / `reasoning_details[]`.

**thoughtSignature reasoning replay:** Gemini returns an opaque
`thoughtSignature` on thinking parts. routectl carries it on the
emitted `reasoning_details[]` entry (format tag `gemini-v1`). On a
follow-up turn the request translator replays prior-turn reasoning as
`thought` parts carrying that signature ahead of the assistant text, so
multi-turn thinking continuity is preserved.

**Structured-output mapping:** the canonical OpenAI-shape
`response_format` maps to `generationConfig`:

- `{type: "json_schema", json_schema: {schema}}` ->
  `responseMimeType: "application/json"` + `responseSchema: <schema>`
- `{type: "json_object"}` -> `responseMimeType: "application/json"`
  (no schema)
- anything else / absent -> neither field emitted

**payload_extras / safetySettings:** knobs the canonical schema does
not carry natively flow through `[providers.X] payload_extras` (merged
into the outbound body). The two common ones are `safetySettings` (the
per-category harm-block thresholds) and `generationConfig.topK`.

```toml
[providers.gemini]
kind        = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
payload_extras = { safetySettings = [
  { category = "HARM_CATEGORY_HARASSMENT", threshold = "BLOCK_NONE" },
], generationConfig = { topK = 40 } }
```

**usageMetadata token accounting:** the Gemini `usageMetadata` block
maps to canonical `Usage`:

- `cachedContentTokenCount` -> `cache_read_input_tokens` (surfaced when
  non-zero; this is the implicit-prefix-cache read count Gemini bills at
  a discount)
- `thoughtsTokenCount` -> `reasoning_tokens` (surfaced when non-zero)
- `cache_creation_input_tokens` is left `None` -- Gemini's prefix cache
  is automatic with free writes, so there is no write count to surface.

### Before/after fidelity: native Gemini vs the openai-compat shim

The native provider beats the prior openai-compat shim on four named
features:

| Feature | openai-compat shim (before) | native gemini (after) |
|---|---|---|
| **systemInstruction** | system prompt folded into a synthetic `system`-role chat message; the model treats it as a conversation turn, not a system directive | system content lifted into the native top-level `systemInstruction.parts` (no role), the shape Gemini expects |
| **thinkingConfig** | no native thinking control -- effort / budget either dropped or coerced through an OpenAI-shape field Gemini ignores | `generationConfig.thinkingConfig` with explicit `thinkingBudget` (verbatim / effort-table / dynamic `-1`) and `includeThoughts` |
| **functionDeclarations** | tools coerced into OpenAI `{type:"function",function:{...}}` shape, then best-effort re-mapped; schema-drift risk | tools emitted as native `tools[].functionDeclarations[]`, the Gemini-native tool schema |
| **usageMetadata cache** | `cachedContentTokenCount` lost -- the shim's OpenAI-shape usage parse has no slot for it, so cache-read savings were invisible to usage accounting | `cachedContentTokenCount` surfaced as `cache_read_input_tokens`; `thoughtsTokenCount` surfaced as `reasoning_tokens` |

Net: the shim loses the system-prompt role distinction, all native
thinking control, the exact tool schema, and the cache-read token
count. Native gains all four, end to end.

### Cloud Code (antigravity) egress mode (`auth_mode = "cloud-code"`)

Setting `auth_mode = "cloud-code"` on a `gemini`-kind provider switches
the egress from the API-key `generativelanguage` surface to the Cloud
Code ("antigravity") surface. The inner Gemini request/response/SSE
translation is REUSED UNCHANGED from the api-key path; only the
transport wrapper, auth, base, and project resolution differ.

**Auth model:** OAuth bearer, sent as `Authorization: Bearer <token>`
(NOT `x-goog-api-key`). The bearer is resolved per request from an
`oauth://antigravity` `api_key_ref`, so a refreshed token rotates in
without a daemon restart. A one-time `routectl login antigravity` (live
Google consent in a browser) mints the credential; the factory rejects
any non-`oauth://` ref in this mode.

**Base + path shape:** the base defaults to the `cloudcode-pa` endpoint
and the provider appends the `v1internal` methods:

- non-stream: `{base_url}/v1internal:generateContent`
- stream:     `{base_url}/v1internal:streamGenerateContent`

**Request envelope + response unwrap:** the inner Gemini request body is
wrapped in a Cloud Code envelope `{project, request, model}` before
send. On the response, the Cloud Code surface wraps the payload in a
`response` field: routectl unwraps it on the non-stream body, and
unwraps it per SSE chunk on the stream path, so downstream translation
sees the same shape it would on the api-key path.

**Project-id resolution:** the `project` field is resolved on first use
via `loadCodeAssist`, falling back to `onboardUser` when loadCodeAssist
does not yield a usable project. The resolved id is cached persistently
in the OAuth credential record (`cloud_project_id`), so subsequent
startups skip the resolution round trip. The onboarding calls carry the
antigravity `User-Agent` and the Cloud Code control-plane headers.

**Reused unchanged from the api-key path:** `thinkingConfig` budget
mapping, `thoughtSignature` reasoning replay, `functionDeclarations`
tool schema, structured-output (`responseMimeType` / `responseSchema`)
mapping, and the `usageMetadata` cached-content + thoughts token
accounting all behave identically -- the cloud-code mode only changes
the outer transport.

## Cross-cutting timing notes

### `stream_first_byte_timeout_ms`

Default is 10s (set in `[retry]`). Fine for most non-thinking models, too aggressive for any thinking model with high effort or any cold-start-prone host.

| Model class | Suggested timeout |
|---|---|
| Non-thinking, normal endpoints | 10000 (default) |
| Thinking-capable, low effort | 30000-60000 |
| Thinking-capable, high effort | 180000-300000 |
| Thinking-capable, max effort | 300000-600000 |
| NIM cold-start (any model) | 180000+ |

**Resolution priority** (model > provider > global):

1. `[models.X] stream_first_byte_timeout_ms` -- per-model override (pin opus xhigh without forcing haiku to wait 5 min on a dead upstream)
2. `[providers.Y] stream_first_byte_timeout_ms` -- per-provider default (use when an upstream is uniformly slow; every model routing through it inherits)
3. `[retry] stream_first_byte_timeout_ms` -- workspace default (keep tight to surface real timeouts on routine calls)

Per-route timeouts: split into separate `[providers.X]` entries with their own runtime knobs and route each `[models.X]` accordingly.

### `request_timeout_ms`

Default is unset (no cap; relies on reqwest's default). Supports **two tiers
only** -- per-model override is NOT supported for this knob:

1. `[providers.Y] request_timeout_ms` -- per-provider ceiling; fills in when
   the global tier left the field unset.
2. `[retry] request_timeout_ms` -- workspace global; lowest priority.

There is no `[models.X] request_timeout_ms` field. To vary the request timeout
per model, route those models through separate `[providers.X]` entries with
distinct `runtime.request_timeout_ms` values. Bump alongside
`stream_first_byte_timeout_ms` for long-thinking responses:

```toml
[providers.bedrock]
stream_first_byte_timeout_ms = 300000   # 5 min until first byte
request_timeout_ms           = 600000   # 10 min full request
```

### Honoring upstream rate-limit resets (`Retry-After` / `resets_at`)

When an upstream rate-limits or overloads (429/503/529) and reports
when it will reset, routectl honors that reset instead of re-probing on
the flat backoff schedule. The reset is read from the standard
`Retry-After` header (seconds or HTTP-date) and, on the Codex
`openai-responses` surface, from the `usage_limit_reached`
`resets_at` / `resets_in_seconds` fields (the 5-hour-cap quota window).

What happens with the honored value:

- A reset at or below 5s is folded into the next same-provider retry
  sleep (it never blocks the request thread longer than that).
- A larger reset parks the provider's circuit breaker open until the
  reset elapses, so the fallback chain skips that exhausted seat rather
  than re-hitting it every flat-schedule retry. The request itself
  falls over to the next chain entry immediately.
- The honored duration is clamped to `[retry] max_honored_retry_after_ms`
  (default 1h) so a hostile or buggy upstream cannot pin a seat open
  indefinitely. See [CONFIGURATION.md](./CONFIGURATION.md) "Honoring
  upstream resets".

Availability probes (`max_tokens <= probe_max_tokens`) are exempt: a
probe that 429/529s still fast-fails immediately with no retry, no
fallback, and no park.

## Multi-host fallback chains

When you want a model with multiple hosts as fallback, declare each host as
its own `[models.X]` row and chain the nicknames in `[aliases]`:

```toml
[providers.example-deepseek-host]
kind        = "openai-compat"
base_url    = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"

[providers.openrouter]
kind        = "openai-compat"
base_url    = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"

[providers.nim]
kind        = "openai-compat"
base_url    = "https://integrate.api.nvidia.com/v1"
api_key_ref = "env://NIM_API_KEY"

[models.ds-go]
provider = "example-deepseek-host"
upstream = "deepseek-v4-flash"

[models.ds-or]
provider = "openrouter"
upstream = "deepseek/deepseek-v4-flash"

[models.ds-nim]
provider = "nim"
upstream = "deepseek-ai/deepseek-v4-flash"

[aliases]
deepseek-flash = ["ds-go", "ds-or", "ds-nim"]   # primary -> fallback -> fallback
```

Each model's `history_reasoning` config applies independently -- the chain just picks who answers. The `routectl_provider` field on every response tells you which one actually answered.

## DeepSeek /anthropic and similar: context_management beta emulation

claude-code 1.x sends two artefacts when the context-management beta is active:

1. An `anthropic-beta: context-management-2025-06-27` request header.
2. A `context_management` top-level body key containing an `edits` array with
   a `clear_thinking_20251015` entry that tells the server which thinking blocks
   to re-send with the response.

Real Anthropic handles both natively. Non-Anthropic anthropic-api providers
(DeepSeek `/anthropic`, vLLM, LM Studio, etc.) reject the beta header and/or
the body key with a 400. They still require thinking echo-back for multi-turn
continuity (see the `history_reasoning = "preserve"` row above), but they need
to receive the thinking blocks directly in the message history -- not via the
Anthropic-proprietary edit mechanism.

**What routectl does when `context_management = true`:**

- Strips `context-management-2025-06-27` from the outgoing `anthropic-beta`
  header so the upstream never sees the beta it doesn't honour.
- Strips the `context_management` top-level body key so the upstream does not
  400 on an unknown field.
- Re-injects the cached thinking blocks before each qualifying ToolUse block in
  the outgoing assistant messages, emulating what the real Anthropic server would
  have done. The injection follows the `keep` policy from the
  `clear_thinking_20251015` edit:
  - `"keep": "all"` -- inject into every assistant turn that has a ToolUse.
  - `"keep": {"type": "thinking_turns", "value": N}` -- inject only the most
    recent N turns (0 means no injection).
  - Unknown shapes default to `"all"` with a debug log.
- **Soft-fail on cache miss**: if the cache has no entry for a tool_use id
  (cold-start or TTL eviction after 60 minutes), routectl strips the `thinking`
  body key and emits a structured `WARN` log so the request completes without a
  400. The operator can see the miss via `grep missed_tool_ids` in the logs.

**Required config:**

```toml
[providers.deepseek-anthropic]
kind               = "anthropic-api"
base_url           = "https://api.deepseek.com/anthropic"
api_key_ref        = "env://DEEPSEEK_API_KEY"
context_management = true

[models.ds-claude]
provider          = "deepseek-anthropic"
upstream          = "deepseek-reasoner"
history_reasoning = "preserve"
```

Note: `history_reasoning = "preserve"` is still required on the `[models.X]`
entry (see the DeepSeek v4 section above) because thinking echo-back and
context-management emulation are complementary, not alternatives.
`history_reasoning` controls how thinking tokens in the INCOMING ChatRequest
history are forwarded; `context_management` controls how the outgoing request
is shaped for the beta-aware edit workflow.

**Troubleshooting:**

| Symptom | Cause |
|---|---|
| `400 context_management is not allowed` | Provider rejects the body key -- set `context_management = true` |
| `400 anthropic-beta header not recognised` | Provider rejects the beta header -- set `context_management = true` |
| WARN `context_management: cache miss for tool_use ids` in logs | Cold-start or TTL gap; thinking was stripped for that turn. The next turn refills the cache and injection resumes. |

## Troubleshooting matrix

routectl logs a 512-chars-truncated `body_excerpt` (`sanitize_for_log`)
at WARN on every 4xx/5xx; flip `ROUTECTL_LOG=routectl=debug` for the
full body.

| Symptom | Likely cause |
|---|---|
| `400 thinking.type.enabled is not supported` | Need `supports_adaptive_thinking = true` on `[models.X]` (Opus 4.7+) |
| `400 reasoning_content in the thinking mode must be passed back to the API` | Need `history_reasoning = "preserve"` on `[models.X]` (DeepSeek v4) |
| `stream first-byte timeout after 10000ms` on a thinking model | Bump `stream_first_byte_timeout_ms` per the table above |
| Empty `content` + non-zero `reasoning_tokens` | Model used full `max_tokens` budget on reasoning. Increase `max_tokens` |
| `400 thinking enabled requires temperature 1.0` | Don't set `temperature` when reasoning is enabled (routectl auto-forces 1.0 if you do) |
| `400 context_management is not allowed` or `400 anthropic-beta header not recognised` | Set `context_management = true` on the provider (DeepSeek /anthropic or similar) |

## When in doubt

`./routectl config check --config <path>` validates the schema before serve.
`./routectl config show` prints the resolved config (for inspection).

Combine with `ROUTECTL_LOG=routectl=debug` and a `grep request_id=<id>` workflow when triaging a specific failure -- see [LOGGING.md](LOGGING.md) for the full triage recipes.
