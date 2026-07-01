# routectl Configuration Reference

TOML configuration schema reference: every knob, how the layered
overlays merge, what's reserved.

> **In a hurry:** copy [`examples/config.toml`](../examples/config.toml)
> for a working end-to-end config, or jump to [Adding a
> provider](#provider-block-providersx), [Adding a
> model](#model-block-modelsx), [claude-code as a gateway
> client](#claude-code-as-a-gateway-client), or
> [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) for upstream-specific
> tuning.

## Top-level shape

A routectl config is a single TOML file with the following top-level
sections:

```toml
[server]              # listener: host, port, strict_translation
[server.auth]         # listener auth tokens (when binding non-loopback)

[providers.X]         # transport + auth: how to reach an upstream
[providers.Y]         # one block per upstream (multiple Y allowed)

[models.X]            # per-model behavior: provider ref, upstream id,
                      # reasoning defaults, header/payload extras
[models.Y]            # one block per addressable model nickname

[aliases]             # wire-string -> model nickname routing
                      # one flat table, single value or list per key

[retry]               # workspace-wide retry + fallback policy

[cache]               # prompt-cache auto-emission policy
                      # (auto_emit_top_level_breakpoint). Optional;
                      # default on.

[reduction]           # dispatch-time context-reduction policy
                      # (enabled). Optional; default off.

[bedrock]             # global Bedrock allowlists (allowed_betas,
                      # allowed_body_fields). Optional.

[log]                 # operator-configurable runtime log knobs
                      # (trace_headers, trace_body_bytes,
                      # redact_prompts). Optional. The env-filter
                      # directive ROUTECTL_LOG is intentionally
                      # env-only and is NOT part of this block.

[usage]               # usage-accounting subsystem: enabled, db_path,
                      # retention_days. Optional.

[registry."<glob>"]   # per-upstream pricing for cost estimation.
                      # Optional; no defaults shipped.

[cache_pricing."<sel>"] # field-level overrides for the baked
                      # prompt-cache economics table. Optional; a
                      # verified table ships baked-in.
```

[`examples/config.toml`](../examples/config.toml) is a working
end-to-end reference; [`examples/bedrock.toml`](../examples/bedrock.toml)
ships an empirical Bedrock allowlist baseline (16 betas + 16 body
fields). Copy and edit; do not re-derive.

## Listener auth + routing

```toml
[server]
host = "127.0.0.1"
port = 8787
strict_translation = false      # set true for production CI
max_body_bytes = 33554432       # 32 MiB default; larger bodies are rejected with HTTP 413
allow_disable_fallbacks = true  # set false for hardened multi-tenant deployments

[server.auth]
tokens = ["env://ROUTECTL_LISTENER_TOKEN", "literal:sk-routectl-dev"]

# Unified [aliases] table: wire-string -> model nickname.
# Suffix-globs (`*`) collapse per-version sprawl. Single-string
# values are one-entry chains; list values are fallback chains.
# `default = "..."` is a special key for the catch-all.
[aliases]
"claude-opus-*"   = "heavy"
"claude-sonnet-*" = "default"
"claude-haiku-*"  = "fast"
default           = "default"

# Alternative: client sets `x-routectl-alias: heavy` and the model
# field is ignored. Header always wins over the aliases map.
```

`tokens` entries are secret-refs (`env://`, `file://`, `literal:`)
resolved at startup. Bind non-loopback only with `--unsafe-public` on
the CLI.

On a non-loopback bind, routectl refuses to start unless at least one
`[server.auth].tokens` entry is configured. The startup error names
the bind address: `"refusing to serve on public bind '<addr>' without
[server.auth].tokens"`. Loopback binds (127.x.x.x, ::1,
::ffff:127.0.0.1) are exempt so the default local-dev workflow
requires no auth.

**`max_body_bytes`** (u32, default 33554432 -- 32 MiB)

Caps the inbound HTTP body size for `/v1/messages` and
`/v1/chat/completions`. Bodies exceeding the limit are rejected by
axum's `DefaultBodyLimit` layer before routectl parses them; the client
receives HTTP 413 Payload Too Large with a dialect-correct error
envelope. The pre-v0.8 hardcoded ceiling was 4 MiB, which was too tight
for live-traffic sessions carrying large multi-turn or multimodal
payloads. Changing this knob takes effect only after a restart (the
`DefaultBodyLimit` layer is wired at server startup, not per-reload).

```toml
[server]
max_body_bytes = 67108864   # 64 MiB -- raise for multimodal or long-history sessions
```

**`allow_disable_fallbacks`** (bool, default true)

When true (default), a client may send the request header
`x-routectl-disable-fallbacks: 1` (also accepts `true` or `yes`) to
pin a single request to the first provider in the alias chain with no
fallback. This is useful for testing and per-provider triage. When set
to false, the header is silently ignored so authenticated clients cannot
bypass the gateway HA story or probe per-provider health. Set this to
false for hardened multi-tenant deployments where listener auth is
configured via `[server.auth].tokens`. The `x-routectl-alias` header
(model routing) is not affected by this knob.

```toml
[server]
allow_disable_fallbacks = false   # harden: ignore client-side fallback bypass header
```

## Field-assignment table

| Field                          | Lives on            | Merge semantics                                                        |
|--------------------------------|---------------------|------------------------------------------------------------------------|
| `provider`                     | `[models.X]`        | required; refs a `[providers]` key                                     |
| `upstream`                     | `[models.X]`        | required                                                               |
| `selectable`                   | `[models.X]`        | default true                                                           |
| `supports_adaptive_thinking`   | `[models.X]`        | bool, default false; selects adaptive vs legacy thinking wire shape    |
| `effort_levels`                | `[models.X]`        | array<string>, default ["low","medium","high"]; empty = pass-through   |
| `max_thinking_budget`          | `[models.X]`        | u32, default 0 (no cap); declared model budget ceiling in tokens       |
| `reasoning_dialect`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `history_reasoning`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `additional_request_fields`    | `[models.X]`        | model-only (Bedrock Converse / Invoke bag)                             |
| `stream_first_byte_timeout_ms` | `[models.X]`        | model > provider > global                                              |
| `max_output_tokens`            | `[models.X]`        | Option<u32>, default None (-> 64000 baseline); anthropic-api + bedrock-invoke only |
| `reported_model`               | `[models.X]`        | Option<String>, default None (-> echo client's requested alias); override for the response `model` label |
| `visible_routectl_provider`    | `[models.X]`        | bool, default true; set false to drop the `routectl_provider` field from the client response (opaque proxy) |
| `header_extras`                | BOTH                | model wins on key collision; `anthropic-beta` comma-unions (see below) |
| `payload_extras`               | BOTH                | deep recursive merge; model wins on leaf collision                     |
| `base_url`, `api_key_ref`, etc.| `[providers.X]`     | provider-only                                                          |
| `auth_kind`, `anthropic_version`| `[providers.X]`    | provider-only; `anthropic_version` default `2023-06-01` (anthropic-api only) |
| `user_agent`                   | `[providers.X]`     | provider-only                                                          |
| `runtime` (RPM, breaker, timeouts, `unsupported_features`) | `[providers.X]` | provider-only                                                          |
| `allowed_betas`                | `[providers.X]` AnthropicApi    | provider-only; allowlist for `anthropic_beta` flags to `api.anthropic.com`; empty = pass-through |
| `allowed_betas`                | `[bedrock]` global              | global filter for Bedrock-accepted `anthropic_beta` values; empty = pass-through (see `[bedrock]`) |
| `anthropic_beta`               | `[providers.X]` Bedrock         | provider-only; operator-asserted floor always sent, bypasses `[bedrock] allowed_betas`            |
| `max_body_bytes`               | `[server]`                      | u32 bytes, default 33554432 (32 MiB); caps inbound body size; HTTP 413 on excess; restart required |
| `allow_disable_fallbacks`      | `[server]`                      | bool, default true; when false the `x-routectl-disable-fallbacks` per-request header is ignored   |
| `auto_emit_top_level_breakpoint` | `[cache]` global              | bool, default true; master switch for dispatch-path auto-cache (see `[cache]`)                    |
| `auto_emit_top_level_breakpoint` | `[providers.X]`               | `Option<bool>`, default None (inherits global); `false` disables auto-cache for this provider     |
| `cache_capability`             | `[providers.X]`                 | `Option<{supports_top_level_cache_control, cache_hit_observable}>`, default None (-> conservative per-kind default) |
| `enabled`                      | `[reduction]` global            | bool, default false; master switch for dispatch-path context reduction (see `[reduction]`)        |
| `reduction_enabled`            | `[providers.X]`                 | `Option<bool>`, default None (inherits global); `false` disables reduction for this provider      |

**`base_url` scheme requirement.** `base_url` must use `https://` (or
`http://` for loopback addresses only). Link-local addresses
(IPv4 `169.254.0.0/16` and IPv6 `fe80::/10`) are rejected at provider
build time regardless of scheme to prevent SSRF and cloud-metadata
credential leaks. The startup error names the offending address.

## header_extras merge

The router's `apply_layered_overlays` helper composes provider and
model `header_extras` per request:

1. Clone provider's `header_extras` into a working map.
2. Iterate model's `header_extras`:
   - auth-reserved keys WARN + drop (see "Reserved-header buckets" below)
   - managed-reserved keys DEBUG + drop
   - every other key model-wins on collision (last-writer-wins)
3. List-valued post-pass on `LIST_VALUED_HEADERS` (`anthropic-beta`):
   comma-split + union + dedup + comma-rejoin in visit order
   `req.anthropic_beta -> provider value -> model value`. The unioned
   string lands back on the merged map AND on `req.anthropic_beta`, so
   downstream readers (the Anthropic-API egress's wire header,
   `bedrock::betas::filter_bedrock_betas`) see the same fully-composed
   list.

## payload_extras merge

Deep recursive merge with model > provider. Object values at the same
key merge recursively; scalar / array collisions take the model value
with a DEBUG log naming the key. The merged result lands on
`req.provider_extras`; each egress's existing `merge_provider_extras`
path picks it up.

Existing `req.provider_extras` (from the Anthropic ingress's
forward-compat sweep) is preserved -- the merge layers
provider + model ON TOP. A provider-side `payload_extras = { foo = "p" }`
wins over a swept ingress value at the same key; a model-side value
wins over both.

## Reserved-header buckets

| Bucket                | Keys                                                | Action on `header_extras` entry             |
|-----------------------|-----------------------------------------------------|---------------------------------------------|
| `AUTH_HEADERS`        | `authorization`, `x-api-key`, `anthropic-version`   | WARN + drop (operator misconfig)            |
| `MANAGED_HEADERS`     | `host`, `content-type`, `content-length`            | DEBUG + drop (wire-shape protection)        |
| `LIST_VALUED_HEADERS` | `anthropic-beta`                                    | comma-split-union-rejoin across all sources |

`anthropic-beta` is NOT in `MANAGED` (pre-v0.6 it was). Operators set
per-provider and per-model values via `header_extras`; the list-valued
post-pass unions them with the ingress lift.

Auth secrets belong in `api_key_ref` / `auth_kind` on the provider, not
`header_extras`. The WARN + drop on `authorization` / `x-api-key`
catches the misconfig at request time.

## Worked example: three-source `anthropic-beta` compose

claude-code sends `betas: ["foo"]` as an SDK option. The TypeScript
SDK translates that to the `anthropic-beta: foo` HTTP header. The
Anthropic ingress lifts the header into `req.anthropic_beta = ["foo"]`
(unchanged in v0.6.0). Then:

```toml
[providers.anthropic-oauth]
kind = "anthropic-api"
api_key_ref = "env://ROUTECTL_ANTHROPIC"
auth_kind = "oauth-bearer"
header_extras = { "anthropic-beta" = "claude-code-20250219,oauth-2025-04-20" }

[models.anthropic-opus]
provider = "anthropic-oauth"
upstream = "claude-opus-4-7"
header_extras = { "anthropic-beta" = "context-1m-2025-08-07" }
```

The dispatch-layer compose runs the list-valued post-pass in visit
order:

- `req.anthropic_beta = ["foo"]` (ingress)
- provider `anthropic-beta` -> `"claude-code-20250219,oauth-2025-04-20"`
- model `anthropic-beta` -> `"context-1m-2025-08-07"`

Result: `req.anthropic_beta = ["foo", "claude-code-20250219",
"oauth-2025-04-20", "context-1m-2025-08-07"]`. The Anthropic-API
egress reads `req.anthropic_beta` and emits ONE
`anthropic-beta: foo,claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07`
HTTP header. Bedrock egresses route the same canonical list through
`filter_bedrock_betas`.

## Bedrock and Anthropic API beta-flag controls

Three distinct knobs govern how `anthropic_beta` flags reach each
upstream. They are independent and serve different purposes.

### `[bedrock] allowed_betas` -- global Bedrock post-filter

An allowlist of `anthropic_beta` flag strings accepted by AWS Bedrock.
Applied as a post-filter to every Bedrock-destined request: any flag
NOT in the list is silently dropped before the request goes on the
wire. Omitting the list (empty = default) puts the filter in
pass-through mode -- every flag reaches AWS as-is.

Use this to prevent unknown flags (new Anthropic betas not yet
supported by Bedrock) from causing upstream 400 errors fleet-wide.
routectl ships no built-in default; AWS schema drift is
operator-tracked.

```toml
[bedrock]
allowed_betas        = ["computer-use-2025-01-24", "files-api-2025-04-14"]
# allowed_body_fields  = [...]  # optional body-field allowlist
```

### `[providers.X] anthropic_beta` -- per-provider Bedrock floor

A static `anthropic_beta` value that routectl always injects on
requests destined for this Bedrock provider, regardless of what the
caller sent. The floor is merged into the request BEFORE the
`[bedrock] allowed_betas` filter runs, and the filter preserves it:
flags present in `[providers.X] anthropic_beta` pass through the
filter unconditionally even when they are absent from
`allowed_betas`, so the floor is always present on the wire.

Use this to guarantee a required beta flag is always present (for
example, a model that requires `computer-use-2025-01-24` to operate
correctly).

```toml
[providers.bedrock-computer]
kind           = "bedrock"
region         = "us-west-2"
creds          = { kind = "default-chain" }
anthropic_beta = "computer-use-2025-01-24"
```

### `[providers.X] allowed_betas` -- Anthropic API (non-Bedrock) allowlist

An allowlist of `anthropic_beta` flags accepted by this
`anthropic-api` provider. Applied analogously to the Bedrock
global filter but scoped to one provider. Empty (default) = every
flag the caller requests reaches `api.anthropic.com` unfiltered.

```toml
[providers.anthropic-strict]
kind         = "anthropic-api"
api_key_ref  = "env://ANTHROPIC_API_KEY"
allowed_betas = ["claude-code-20250219", "oauth-2025-04-20"]
```

## `[providers.X] api_shape` -- Bedrock API selector

A `bedrock`-kind provider picks its wire shape with `api_shape`
(string, default `"invoke"`, also accepts `"converse"`):

- `"invoke"` -- vendor-specific InvokeModel body (the Anthropic
  Messages payload, default).
- `"converse"` -- vendor-neutral Converse API.

```toml
[providers.bedrock]
kind      = "bedrock"
region    = "us-west-2"
creds     = { kind = "default-chain" }
api_shape = "converse"   # default "invoke"
```

Both shapes are wired for Anthropic models on Bedrock; see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#bedrock-any-region) for the
adaptive-thinking interaction.

## `[providers.X]` Gemini (`kind = "gemini"`)

A `gemini`-kind provider talks to the native Google Gemini REST API
(`generateContent` / `streamGenerateContent`), NOT the openai-compat
shim. It wins native fidelity on `systemInstruction`, `contents`/`parts`,
`functionDeclarations`, `generationConfig` (incl. `thinkingConfig`), and
the `usageMetadata` cached-content + thoughts token accounting. See
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#gemini-native-kind--gemini) for
the per-feature mapping and the before/after fidelity note.

```toml
[providers.gemini]
kind        = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
# base_url defaults to the public v1beta endpoint; omit unless you are
# pointing at a Vertex-style endpoint (see below).
# base_url  = "https://generativelanguage.googleapis.com/v1beta"

[models.gemini-flash]
provider = "gemini"
upstream = "gemini-2.5-flash"

[aliases]
"gemini-*" = "gemini-flash"
```

Fields:

- `api_key_ref` (required) -- secret-URI (`env://`, `file://`,
  `literal:`, `oauth://`) resolving to a Google AI Studio API key. The
  resolved key is sent as the `x-goog-api-key` request header. A
  routectl-managed token source may rotate the key without a daemon
  restart.
- `base_url` (optional, default
  `https://generativelanguage.googleapis.com/v1beta`) -- the API base.
  The provider appends `/models/{model}:generateContent` (non-stream)
  and `/models/{model}:streamGenerateContent?alt=sse` (stream). Point
  this at a Vertex AI endpoint to reach Gemini through Vertex once that
  surface is exercised; the path shape is the documented seam.
- `header_extras` (optional) -- provider-level extra request headers,
  merged per the [header_extras merge](#header_extras-merge) rules.
- `payload_extras` (optional) -- provider-level JSON merged into the
  outbound request body. This is the flow-through path for knobs the
  canonical schema does not carry natively -- notably `safetySettings`
  and `generationConfig.topK`. Merged per the
  [payload_extras merge](#payload_extras-merge) rules.
- `user_agent` (optional) -- override the outbound `User-Agent`.

Auth decision: Gemini auth is API-key only, via the `x-goog-api-key`
header. Vertex AI / Google OAuth (ADC, service-account) is explicitly
NOT implemented; it is reachable later by pointing `base_url` at a
Vertex endpoint without a new provider kind.

## Per-provider capability filter (`unsupported_features`)

Some upstreams reject specific built-in tool shapes (Bedrock, for
example, currently 400s on Anthropic's `web_search_*` tool families).
The legacy behavior was tried-and-fallback: dispatch to Bedrock, get
a 400, walk to the next chain entry. That burns latency, surfaces a
400 in operator dashboards, and counts the failure against Bedrock's
breaker even though the request never had a chance.

`unsupported_features` is a declarative, operator-supplied list set
directly on each `[providers.X]` table. The router derives feature keys from
the request's `tools` array and pre-filters the alias chain BEFORE
dispatch -- a chain entry whose provider lists ANY of the request's
features is dropped. If every entry gets filtered, the router returns
a 501 `Not Implemented` naming the offending feature.

Feature-key derivation walks `tools[].type` strings on the
canonical `ToolDef::Other` variant (Anthropic builtins, server-side
tools, future shapes). A trailing `-YYYYMMDD` or `_YYYYMMDD` suffix
is stripped so `web_search_20250305` and a future
`web_search_20251102` both reduce to `web_search`. User-defined
custom tools (`ToolDef::Custom`) do not contribute feature keys.

```toml
# Bedrock provider in a chain that also has anthropic-api fallback.
# claude-code's web_search tool fails on Bedrock today; declaring it
# unsupported here means the router skips Bedrock for web-search-using
# requests entirely (no 400, no breaker hit) and goes straight to the
# anthropic-api fallback.
[providers.bedrock]
kind   = "bedrock"
region = "us-west-2"
creds  = { kind = "default-chain" }
unsupported_features = ["web_search"]

[providers.anthropic-api]
kind        = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.bedrock-opus]
provider = "bedrock"
upstream = "us.anthropic.claude-opus-4-7-v1:0"

[models.anthropic-opus]
provider = "anthropic-api"
upstream = "claude-opus-4-7"

[aliases]
"claude-opus-*" = ["bedrock-opus", "anthropic-opus"]
```

A request without built-in tools dispatches to `bedrock-opus` first
per the chain order. A request carrying a `web_search_20250305` tool
skips Bedrock and goes directly to `anthropic-opus`. If BOTH
providers listed `web_search` as unsupported, the router returns
`501 not_implemented` with the message `no provider in chain
supports features: web_search` (the feature keys are comma-joined).

Per-skip events log at DEBUG (`provider skipped: feature in
unsupported_features list`); the terminal empty-chain event logs at
WARN. INFO would flood; the codebase precedent is "fallback events
at WARN, retry-same-provider at DEBUG".

## Per-provider runtime gates

Each `[providers.X]` entry accepts five runtime-gate knobs set
directly on the `[providers.X]` table (the policy is flattened into
the provider entry -- there is no `[providers.X.runtime]` sub-table;
nesting one fails to load with `unknown field \`runtime\``). All
accounting is per-attempt (not per-request).

| Field                          | Type       | Default         | Effect |
|--------------------------------|------------|-----------------|--------|
| `rpm_limit`                    | Option<u32> | None (disabled) | Maximum requests per minute to this provider. When exceeded the router treats this provider as a fallbackable failure and tries the next chain entry. |
| `circuit_failures`             | Option<u32> | None (disabled) | Trip the circuit breaker after this many consecutive failed attempts. Once tripped, the router skips this provider for `circuit_cooldown_ms`. |
| `circuit_cooldown_ms`          | Option<u64> | 30000 (30s) when `circuit_failures` is set; unused otherwise | How long to keep the circuit open once tripped. |
| `request_timeout_ms`           | Option<u64> | None (no cap)   | Per-attempt request timeout. Resolution order: per-provider `request_timeout_ms` > global `[retry] request_timeout_ms` > None (no cap). Per-alias retry overrides were removed in v0.6; to vary timeouts per route, split into distinct `[providers.X]` entries. |
| `stream_first_byte_timeout_ms` | Option<u64> | None            | Per-provider first-byte timeout for streaming responses. Resolution order: per-model > per-provider > global `[retry] stream_first_byte_timeout_ms`. |

`rpm_limit` and circuit breaker are both `None` (disabled) when omitted.
`circuit_cooldown_ms` is only meaningful when `circuit_failures` is set;
its default of 30s applies when `circuit_failures` is present but
`circuit_cooldown_ms` is absent.

Example:

```toml
[providers.bedrock]
kind                   = "bedrock"
region                 = "us-west-2"
creds                  = { kind = "default-chain" }
rpm_limit              = 60
circuit_failures       = 5
circuit_cooldown_ms    = 60000
request_timeout_ms     = 120000
unsupported_features   = ["web_search"]
```

## Retry and fallback defaults

The default `[retry]` block falls back on every 4xx/5xx upstream
response: `retry_allowlist` defaults to `[]` and `retry_denylist`
defaults to `None`, so `RetryPolicy::is_fallbackable_status`
(`crates/routectl-router/src/config.rs`) takes the "every 4xx/5xx"
fall-through branch. This is the safest default for Cloudflare-fronted
upstreams (opencode.ai, openrouter.ai, etc.), which surface
upstream-origin failures through extended 5xx codes (520-527, 530)
and would otherwise need each code listed by hand. Operators with
bespoke upstream behavior can override either `retry_allowlist` (an
explicit set of fallback codes -- everything else is terminal) OR
`retry_denylist` (`400..=599` minus the listed codes) -- the two are
mutually exclusive and setting both is a config-load error.

```toml
[retry]
max_attempts                  = 2
initial_backoff_ms            = 250
backoff_multiplier            = 2.0
jitter_ms                     = 50
# Default: omit both lists -- every 4xx/5xx falls back. To narrow,
# set EITHER an allowlist OR a denylist (not both):
# retry_allowlist             = [408, 429, 500, 502, 503, 504]
# retry_denylist              = [422]
request_timeout_ms            = 300000        # 5 min per attempt
stream_first_byte_timeout_ms  = 90000         # 90s -- thinking models stall
probe_max_tokens              = 1             # fast-fail availability probes
# Per-error-class caps (each overrides max_attempts for that class only):
# retry_on_429                = 1             # rate-limits usually clear in one retry
# retry_on_network            = 2             # flaky DNS/TLS/connect
```

`probe_max_tokens` (default `1`) fast-fails availability probes. A
request whose `max_tokens` is at or below this value is treated as a
probe -- Claude Code sends `max_tokens=1` quota/health checks to
`/v1/messages`. On a rate-limit or overload (429/529) a probe skips
retry+fallback and returns the status immediately: every hop of an
all-Anthropic chain shares the same limit, so walking it is futile and
the probe's tiny output is never read. Set `probe_max_tokens = 0` to
disable (no request is ever treated as a probe). Real requests
(`max_tokens` above the threshold), generic 5xx, network errors, and
every 4xx (including a capability-rejection 400, which a sibling
provider may accept) keep the normal retry+fallback behavior.

### Per-error-class retry caps

Three optional knobs override `max_attempts` for a single error class
each, leaving the other classes on the global ceiling. Rate-limits
often clear in one retry while flaky 5xx may need more, so tuning the
classes independently avoids over- or under-retrying any one of them.

| Field              | Type        | Default | Effect |
|--------------------|-------------|---------|--------|
| `retry_on_429`     | Option<u32> | None    | Cap for 429 (rate-limit) responses. Unset -> falls back to `max_attempts`. |
| `retry_on_5xx`     | Option<u32> | None    | Cap for 5xx responses. Unset -> falls back to `max_attempts`. |
| `retry_on_network` | Option<u32> | None    | Cap for network errors (status 0: DNS, TCP connect, TLS handshake, request body, request timeout). Unset -> effectively `max_attempts`. |

Resolution lives in `RetryPolicy::retries_for_status`
(`crates/routectl-router/src/config.rs`): each knob, when `Some`,
replaces `max_attempts` for THAT class only. Both the 429 arm and the
5xx arm are gated on `is_fallbackable_status` -- a 429 (or 5xx) that
the allowlist excludes or the denylist names is non-retryable and
yields 0 retries regardless of `retry_on_429` / `retry_on_5xx`, so it
propagates to the caller immediately. They ship commented in
[`examples/config.toml`](../examples/config.toml).



### Honoring upstream resets (`max_honored_retry_after_ms`)

When an upstream rate-limits or overloads (429/503/529) and tells
routectl WHEN it resets -- via the `Retry-After` header, or the Codex
`usage_limit_reached` `resets_at` / `resets_in_seconds` fields --
routectl honors that reset instead of re-probing on the flat backoff
schedule. A small reset is folded into the next in-loop retry sleep;
a larger one parks the provider's circuit breaker open until the reset
elapses, so the fallback chain skips that exhausted seat rather than
hammering it.

| Field                       | Type        | Default     | Effect |
|-----------------------------|-------------|-------------|--------|
| `max_honored_retry_after_ms`| Option<u64> | 3600000 (1h)| Ceiling on how long an upstream reset hint can park a provider. Caps BOTH the in-loop honored sleep and the breaker park, so a hostile or buggy upstream cannot pin a seat open indefinitely. |

```toml
[retry]
# Cap an honored upstream reset at 30 minutes (default is 1 hour):
# max_honored_retry_after_ms  = 1800000
```

A reset at or below 5 seconds is honored as the next same-provider
retry sleep (it never blocks the request thread beyond that). A larger
reset, clamped to `max_honored_retry_after_ms`, parks the provider via
the circuit breaker -- the request itself falls over to the next chain
entry immediately rather than waiting. Recovery after the park still
flows through the breaker's single half-open probe. The resolution and
clamp live in `RetryPolicy::max_honored_retry_after` and the router's
`rate_limit_reset_hint` / `park_provider`
(`crates/routectl-router/src/config.rs`,
`crates/routectl-router/src/router.rs`).



## Per-model knobs

### Reasoning capability declaration

Three fields on `[models.X]` declare what reasoning a model supports.
The router and egresses read them at dispatch time; callers never set
these fields directly.

**`supports_adaptive_thinking` (bool, default false)**

When `true`, the Anthropic-API and Bedrock egresses emit the adaptive
thinking wire shape:

```json
{ "thinking": { "type": "adaptive" }, "output_config": { "effort": "high" } }
```

When `false`, they emit the legacy fixed-budget shape:

```json
{ "thinking": { "type": "enabled", "budget_tokens": 16000 } }
```

Set `true` only for models that accept the adaptive shape (Anthropic
Opus 4.7+). Non-adaptive models sent the adaptive shape receive a 400.

**`effort_levels` (array of strings, default ["low","medium","high"])**

Ordered list of effort levels the operator declares this model accepts.
Every element must be one of the full vocabulary:

```
minimal | low | medium | high | xhigh | max
```

The validator at config-load rejects tokens outside that set. An empty
list (`effort_levels = []`) means pass-through: the egress emits whatever
effort the caller supplied without operator-side filtering. This is the
correct default for OpenRouter-style providers that perform their own
effort translation.

Clamping applies on ALL egresses when `effort_levels` is non-empty: when a
caller supplies an effort value not in the model's `effort_levels`, the egress
clamps to the nearest supported level (rounding toward the most capable
supported level when the requested level is above the declared maximum, and to
the least capable when it is below the minimum). When `effort_levels` is empty,
the egress emits whatever effort the caller supplied without operator-side
filtering.

**`max_thinking_budget` (u32, default 0)**

Declares the model's maximum thinking-token budget in tokens. `0` means
"not a budget-capped model" -- the egress falls back to its own
inference-time defaults. Non-zero values are forwarded as the ceiling
for the egress's budget negotiation. Only relevant on the legacy
`supports_adaptive_thinking = false` path; the adaptive path uses effort
strings and has no budget field.

**`max_output_tokens` (Option<u32>, default None)**

Per-model ceiling on the `max_tokens` value the Anthropic-shape egresses
(`anthropic-api`, `bedrock-invoke`) inject when the caller omits the
field. `None` (the default) falls through to a hardcoded baseline of
`64000`.

Resolution chain at dispatch:

1. `request.max_tokens` -- caller-supplied value always wins.
2. `[models.X].max_output_tokens` -- operator override.
3. `64000` -- hardcoded baseline.

Only consumed by Anthropic-shape egresses (`anthropic-api` and
`bedrock-invoke`). The other egresses (`openai-compat`,
`openai-responses`, `bedrock-converse`) forward caller omission cleanly
without injection (good-translator principle: do not inject where the
upstream already handles it).

Set this when an Anthropic-shape egress points at a model whose
upstream `max_tokens` cap is below `64000` -- otherwise a caller that
omits `max_tokens` triggers a 400 from the upstream's per-model
validation. Rare in practice; typical claude-code clients send
`max_tokens` explicitly.

```toml
[models.opus-legacy]
provider          = "anthropic-oauth"
upstream          = "claude-opus-4"
# Older Opus 4 caps max_tokens at 32000; lower the baseline so callers
# omitting max_tokens do not 400.
max_output_tokens = 32000
```

**`reported_model` (Option<String>, default None) -- response `model` label**

The `model` field routectl echoes back in responses (and on every
streaming chunk, including the terminal usage-only chunk) is the
*client-visible label*, decoupled from the upstream wire model id:

- **Default (no `reported_model` set):** the response `model` echoes the
  client's requested alias verbatim -- the exact string the client sent
  (or the `x-routectl-alias` header override). A client asking for
  `deepseek-chat` and routed to upstream `deepseek-v3` sees
  `"model": "deepseek-chat"` in the response, not the upstream id.

- **Override (`reported_model = "label"`):** the served model's
  `reported_model` wins, pinning a fixed public-facing string regardless
  of which alias the client used. An empty string (`reported_model = ""`)
  is treated as unset and falls through to the requested alias.

The label is computed once per request from the model that actually
served it, so a fallback chain or a multi-chunk stream carries one stable
label across every hop and chunk.

```toml
[models.deepseek]
provider       = "deepseek"
upstream       = "deepseek-v3"
# Clients see "fast-chat" no matter which alias routed here.
reported_model = "fast-chat"
```

This affects only the client-visible `model` field. Internal accounting
and observability (usage capture, pricing) key off the served model and
upstream recorded in dispatch metadata, not the response `model` field,
and are unchanged. The `routectl_provider` field remains the intentional
transparency channel naming the provider that answered; it is unaffected
by `reported_model`.

**`visible_routectl_provider` (bool, default true) -- response `routectl_provider` field**

`routectl_provider` is a routectl response extension naming the provider
that actually answered (e.g. `"anthropic-api"`, `"openai-compat:deepseek"`).
It is the intentional transparency channel and is emitted by default on the
OpenAI dialect (the Anthropic dialect omits it). For an opaque-proxy or
white-label deployment that must not disclose its backend, set:

```toml
[models.fast]
provider                  = "deepseek"
upstream                  = "deepseek-v3"
visible_routectl_provider = false   # drop routectl_provider from responses
```

When false, the router clears `routectl_provider` from the client response
(the served model's flag decides, so a fallback chain has one stable
contract). This affects only the client-visible field: internal usage and
cost accounting key off the dispatch record, not this field, and are
unaffected. There is no streaming concern -- streamed chunks never carry
`routectl_provider`.

Two other per-model overrides live on `[models.X]`:

- `header_extras = { "anthropic-beta" = "..." }` -- per-model beta gates.
  Use when a provider serves multiple Claude models and only some support a
  given beta (e.g. `context-1m-2025-08-07` works on opus/sonnet but is
  rejected for haiku) -- saves duplicating the entire provider config.
  The `anthropic-beta` key runs through the comma-split-union-rejoin post-pass
  (see "header_extras merge" above) so provider-level and model-level betas
  union onto one wire header. To have a model omit a beta that the provider
  sets: move the beta off the provider block and add it to each `[models.X]
  header_extras` that needs it.

- `stream_first_byte_timeout_ms = N` -- per-model > per-provider >
  global resolution. Pin opus xhigh adaptive thinking at 300s
  without forcing haiku to wait 5 min on a dead upstream.

Example:

```toml
[providers.bedrock]
kind   = "bedrock"
region = "us-west-2"
creds  = { kind = "default-chain" }
stream_first_byte_timeout_ms = 60000          # provider default

[models.opus47]
provider                     = "bedrock"
upstream                     = "us.anthropic.claude-opus-4-7-v1:0"
supports_adaptive_thinking   = true
effort_levels                = ["low", "medium", "high", "xhigh", "max"]
header_extras                = { "anthropic-beta" = "context-1m-2025-08-07" }
stream_first_byte_timeout_ms = 300000         # opus override

[models.haiku45]
provider = "bedrock"
upstream = "us.anthropic.claude-haiku-4-5-v1:0"
# inherits provider's 60s; no per-model override
# effort_levels defaults to ["low","medium","high"]
```

## history_reasoning (reasoning echo-back)

`history_reasoning` on `[models.X]` controls whether routectl echoes a
model's own prior reasoning back to the upstream on multi-turn replay.

> **NOTE:** Moved from `[providers.X]` to `[models.X]` in v0.6.0;
> provider-level placement now rejects at config-parse time.

Three values:

- `auto` (the unset default) -- the egress decides. For openai-compat
  this follows the per-dialect default; for the anthropic-api egress it
  strips unsigned `thinking` blocks (real-Anthropic-safe: Anthropic
  signs thinking and 400s an unsigned replay).
- `strip` -- always drop reasoning from outgoing history.
- `preserve` -- always keep reasoning on the wire.

`preserve` governs BOTH egresses:

- **openai-compat** -- keeps assistant `reasoning_content` in outgoing
  history (DeepSeek v4+, recent vLLM that 400 without echo-back).
- **anthropic-api** -- keeps UNSIGNED `thinking` blocks on replay
  instead of stripping them. DeepSeek's `/anthropic` endpoint
  (`kind = "anthropic-api"`, base_url `https://api.deepseek.com/anthropic`)
  emits thinking without a signature yet 400s the next turn unless it is
  echoed back: `The content[].thinking in the thinking mode must be
  passed back to the API.` Set `preserve` for those models.

Default (`auto` / unset) still strips for the anthropic-api egress,
which is correct for real Anthropic (`api.anthropic.com`): replayed
thinking there must carry the signature Anthropic issued. Set `preserve`
ONLY for Anthropic-compatible endpoints that require echo-back, never
for real Anthropic. The tool_result / `tool_use_id` validation is
independent of this knob and always applies.

See [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) for full per-upstream
recipes.

## context_management (anthropic-api provider flag)

`context_management` on `[providers.X]` (kind = "anthropic-api") tells routectl
to emulate Anthropic's context-management-2025-06-27 beta server-side for that
provider. Set it for non-Anthropic anthropic-api endpoints that do not honor the
beta natively (e.g. DeepSeek's `/anthropic` surface). Default is `false`:
routectl forwards the body verbatim and the real Anthropic server handles the
beta itself.

```toml
# DeepSeek /anthropic provider: routectl emulates context management because
# DeepSeek does not natively honor the beta header.
[providers.deepseek-anthropic]
kind               = "anthropic-api"
base_url           = "https://api.deepseek.com/anthropic"
api_key_ref        = "env://DS_KEY"
auth_kind          = "oauth-bearer"
context_management = true
```

### `max_thinking_entry_bytes` (per-entry cap on the thinking cache)

When `context_management = true`, routectl caches thinking blocks observed
in upstream responses for re-injection on the next turn (see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) "context_management beta emulation").
The cache bounds entry-count via an LRU; `max_thinking_entry_bytes` bounds
the per-entry serialized JSON size so a misbehaving upstream cannot push
the cache memory footprint to the LRU-cap times the largest single
response size.

Default: 1 MiB per entry. The default gives ~3x headroom over realistic
worst-case Opus 4.6/4.7/4.8 reasoning turns at full 65k thinking-token
budgets (~328 KB at ~5 bytes/token). Override per provider when memory
pressure dictates a tighter cap, or when an upstream legitimately
produces larger thinking turns. Entries whose serialized bytes exceed
the cap are rejected at write time with a structured WARN; the next
turn's cache-miss recovery strips the `thinking` body key just as it
does for a TTL eviction, so the request still completes without a 400.

Bounds: `>= 1024` (1 KiB) and `<= 4 MiB` (4 194 304 bytes). Values
outside the range are clamped at provider build time with a startup
WARN -- a configured `0` falls back to the 1 MiB default (a zero cap
silently disables the cache, which is never the intent), and any
value above 4 MiB clamps to the ceiling.

The cache LRU itself is bounded at `THINKING_CACHE_CAP = 10000`
entries; the worst-case memory footprint is `THINKING_CACHE_CAP * cap`
(10000 * 1 MiB ~ 10 GiB at the default). Operators on memory-
constrained hosts should tune this knob down. The TTL is sliding
(every hit refreshes `expires_at`); idle entries die after the
hardcoded 60-minute window.

```toml
[providers.deepseek-anthropic]
kind                      = "anthropic-api"
base_url                  = "https://api.deepseek.com/anthropic"
api_key_ref               = "env://DS_KEY"
context_management        = true
max_thinking_entry_bytes  = 524288   # tighten or raise from the 1 MiB default
```

## Log knobs (`[log]`)

The optional `[log]` block carries operator-facing fallbacks for the
three runtime log-safe knobs:

```toml
[log]
trace_headers    = false    # ROUTECTL_TRACE_HEADERS fallback
trace_body_bytes = 16384    # ROUTECTL_TRACE_BODY_BYTES fallback (16 KB default)
redact_prompts   = false    # ROUTECTL_LOG_REDACT_PROMPTS fallback
```

Per-knob resolution: env wins when set; otherwise the matching
`[log]` field (when `Some`); otherwise the hardcoded default
(`false` / `16384` / `false`). All fields are optional. A missing
`[log]` block leaves current behavior unchanged (env-only or
hardcoded default for each knob).

Accepted truthy spellings for the boolean env vars
(`ROUTECTL_TRACE_HEADERS`, `ROUTECTL_LOG_REDACT_PROMPTS`): `1`,
`true`, `yes`, `on` (case-insensitive, whitespace-trimmed). Anything
else (including empty string) is treated as false.

What each knob does:

- `trace_headers` -- opt-in for the four `trace_*_headers`
  directions (raw, no redaction). Default off. See
  [LOGGING.md](LOGGING.md) for the per-direction emit contract.
- `trace_body_bytes` -- cap on the serialized body emitted at
  TRACE level by the four body-trace helpers (ingress, outgoing,
  upstream success, egress). Default 16 KB. Bump to ~1 MB
  (`1048576`) when capturing live-traffic fixtures so full
  conversation history with cache_control breakpoints is not
  truncated.
- `redact_prompts` -- opt-in for prompt redaction in TRACE-level
  body logs. Strips known user-content fields (text blocks,
  tool_use input, instructions, refusal blocks, image data URIs,
  Bedrock Converse `toolUse.input`) and replaces them with
  `<redacted len=N>` while preserving structural fields (model,
  tools, sampling params, finish_reason, usage). Default off
  (verbatim bodies in TRACE).

Caveat -- the env-filter directive (`ROUTECTL_LOG`, e.g.
`routectl=info,routectl_core::log_safe=trace`) is intentionally
NOT part of `[log]`. It stays env-only because it must reach the
tracing subscriber BEFORE any config load runs. To raise log level
for one process, export `ROUTECTL_LOG` in the environment that
launches `routectl serve`.

Caveat -- like every routectl log-safe knob, the resolved value
freezes at process startup. Flipping the env var or editing
`[log]` after launch has no effect until the next restart. The
seeder fires a single `info` line per knob at boot
(`ROUTECTL_LOG_REDACT_PROMPTS resolved`,
`ROUTECTL_TRACE_BODY_BYTES resolved`, `ROUTECTL_TRACE_HEADERS
resolved`) so operators can confirm the effective value once.

## Usage accounting (`[usage]`)

The optional `[usage]` block controls the usage-accounting subsystem,
which persists one row per request (token counts, cost) to a local
SQLite database. A missing block keeps every default below.

```toml
[usage]
enabled = true                          # master switch; default true
db_path = "/home/you/.config/routectl/usage.db"  # default resolved path
retention_days = 90                     # prune rows older than this; default 90
```

| Knob             | Default                          | Reload    |
|------------------|----------------------------------|-----------|
| `enabled`        | `true`                           | hot       |
| `db_path`        | `<config-dir>/routectl/usage.db` | restart   |
| `retention_days` | `90`                             | hot       |

- `enabled` -- master switch for the subsystem. Hot-reloads on the
  next config swap; no restart needed to turn accounting on or off.
- `db_path` -- the SQLite database file. The default resolves the user
  config dir from `XDG_CONFIG_HOME` (else `$HOME/.config`), so no
  literal `~` ever reaches SQLite. **Restart-required**: the writer
  opens the database at startup and holds the handle, so a `db_path`
  change is classified restart-required and is reported as
  `usage.db_path` on hot-reload rather than taking effect live.
- `retention_days` -- on daemon startup, rows older than this many days
  are pruned from the database. Hot-reloads; the new value applies at
  the next startup-time prune.

## Inspecting a request offline (`routectl prompt-size`)

`routectl prompt-size` prints an OFFLINE report of a request fixture's
token footprint and what routectl's cache / reduction machinery WOULD do
to it. It never dispatches to any upstream and never resolves secrets or
touches the network, so it is safe to run against any saved request body
without valid credentials.

```sh
routectl prompt-size --alias heavy --request ./fixture.json
```

- `--alias` -- an `[aliases]` key or a `[models.X]` nickname. Its target
  provider's prompt-cache capability is resolved CONFIG-only (alias ->
  first model nickname -> provider entry), with no provider build.
- `--request` -- a JSON request body parsed as a canonical `ChatRequest`.
  Both OpenAI Chat Completions and Anthropic Messages shapes work: a
  `system` prompt may sit at the top level OR as `role: "system"`
  messages -- both are attributed to the SYSTEM tier.

The report has three sections:

1. **Size breakdown** -- bytes and approx tokens per tier (SYSTEM, TOOLS,
   MESSAGES, TOTAL). Approx tokens are `bytes / 4`, a rough estimate for
   sizing only, NOT a billing figure.
2. **Auto-emit** -- whether the dispatch path would inject a top-level
   `cache_control` breakpoint, reflecting the operator's current
   `[cache] auto_emit_top_level_breakpoint` switch: `caller-supplied`
   (the request already carries breakpoints; checked first), `skipped:
   globally_disabled ([cache] auto_emit_top_level_breakpoint = false)`
   (auto-emit is turned off in config), `would inject 1 top-level
   ephemeral_5m breakpoint`, `skipped: no_capability` (target does not
   honor a top-level breakpoint), `skipped: volatile_vetoed` (the stable
   prefix carries high-confidence volatile tokens), or `indeterminate`
   (the alias's capability could not be resolved offline).
3. **Reduction** -- what `[reduction]` json-minify would strip from the
   mutable message tail, reflecting the operator's current `[reduction]
   enabled` switch. When reduction is ENABLED: the strings minified,
   bytes saved, and approx tokens saved (or a no-op reason). When
   reduction is DISABLED in config, the line still reports the available
   headroom but prefixes it with `reduction disabled in config
   ([reduction] enabled = false)` so an operator sees both the savings on
   offer AND the truth about their current config.

A misconfigured alias surfaces the same clean config-validation error
`routectl test` produces.

### Cache-break economics projection (advisory)

When you pass `--hypothetical-d`, the report gains a fourth section: an
ADVISORY projection of whether breaking a warm prompt-cache to apply a
proposed prefix cut would be net-positive. It is offline-only and never
mutates a request, resolves a secret, or touches the network -- it is
advice, computed from the baked per-`(provider_kind, model, tier)` cache
pricing table.

```sh
# Just the break-even threshold K* for a 50k-token cut, 5m tier:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000

# Plus a keep/break verdict at an assumed reuse count, 1h tier:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000 --hypothetical-k 60 --ttl-tier 1h
```

- `--hypothetical-d <TOKENS>` -- the size of the proposed cache-prefix
  cut. Supplying this flag is what turns ON the projection; omit it and
  the report is byte-for-byte the three-section output above.
- `--hypothetical-k <COUNT>` -- an assumed future-reuse count. When
  given, the report also prints a KEEP / BREAK verdict (with the stable
  ledger strategy token). When omitted, only the break-even K* threshold
  is printed.
- `--c-after <TOKENS>` -- cached tokens at/after the edit point that must
  re-write. Defaults to C (the oldest-first conservative case).
- `--ttl-tier <5m|1h>` -- the cache TTL tier to price (Anthropic /
  Bedrock differ; other providers ignore it). Defaults to `5m`.

C (the total cacheable prefix tokens) is taken from the report's TOTAL
approx-token count; the command does not distinguish a separate
cacheable-prefix slice, so the whole prompt-token footprint is the
conservative C. The projection prints the resolved provider kind /
model / tier, the pricing cell's trust label (`verified` or
`unverified (NEEDS-LIVE-PROBE)`), the break-even K*, and -- when
`--hypothetical-k` is supplied -- the verdict. An unverified / sentinel
cell shows no live K* and a `KEEP (insufficient data)` verdict, because
its multipliers are not trusted for a live decision.

## Pricing registry (`[registry."<pattern>".pricing]`)

The optional `[registry]` table supplies per-upstream prices so usage
rows can carry a cost estimate. routectl ships **no price defaults** --
an upstream with no matching entry is simply unpriced. Cost is computed
at **query time**, never persisted, so correcting a price later
retroactively fixes the cost of every historical row priced by that
entry.

```toml
[registry."deepseek-*"]
# optional provider scope; omit to price the upstream for any provider
provider = "my-deepseek"

[registry."deepseek-*".pricing]
input_per_mtok          = 0.27   # USD per million input tokens
output_per_mtok         = 1.10   # USD per million output tokens
cache_read_per_mtok     = 0.07   # USD per million cache-read tokens
cache_write_5m_per_mtok = 0.50   # USD per million 5-minute cache-write tokens
cache_write_1h_per_mtok = 0.90   # USD per million 1-hour cache-write tokens
```

All five `*_per_mtok` fields are optional and expressed in USD per
million tokens. A field left unset means that dimension is unpriced and
contributes nothing to the estimate. There is intentionally no reasoning
rate: reasoning tokens are billed as output upstream, so they are never
a separate cost dimension.

**Key semantics.** Each `[registry]` key is an upstream-id glob, parsed
the same way alias keys are:

- an **exact** id (`"deepseek-chat"`) matches that id only;
- a **trailing-`*` prefix** (`"deepseek-*"`) matches any id starting
  with the prefix.

Bare `*` and embedded asterisks (`"a*b"`) are rejected at startup by
`config check` and by `serve`/`test`. When several keys match one
upstream, the resolver picks the best by:

1. **provider scope first** -- an entry whose `provider` equals the
   request's provider beats a provider-agnostic entry (no `provider`).
   An entry scoped to a *different* provider is never eligible.
2. **longest matching prefix next** -- among entries of equal scope, the
   longest prefix wins. An exact key behaves like a maximal-length
   prefix, so an exact match beats any shorter prefix.

The optional `provider` scope lets the same upstream id served by two
different providers be priced differently -- give each a distinct key
(the table is keyed by pattern string) and set `provider` on the scoped
one.

## Cache-economics pricing overrides (`[cache_pricing."<selector>"]`)

routectl ships a verified, baked-in table of prompt-cache *economics*
multipliers -- the write multiplier (`wm`), read multiplier (`rm`), TTL,
and minimum cacheable prefix -- per `(provider_kind, model)`. These feed
the (later) cache-break break-even reasoning; they are distinct from the
`[registry]` dollar prices above, which feed usage-cost estimation.

You do not normally need to touch this. The table is re-verified against
vendor docs on each routectl release, and unverified cells already fall
back to a conservative sentinel. The `[cache_pricing]` block exists only
to patch a cell that drifted between releases.

```toml
# Selector key is "<provider_kind>:<model_glob>". provider_kind is the
# stable `kind = "..."` token (anthropic-api, bedrock, openai-responses,
# openai-compat); model_glob is an exact id or a trailing-`*` prefix.
[cache_pricing."openai-compat:grok-4-3*"]
rm = 0.05            # correct just the read multiplier; everything else
                     # (wm, ttl_seconds, min_prefix_tokens, ...) inherits
                     # the baked-in value -- an omitted field is NOT reset.
```

**Field-level merge.** Every field is optional. A field you set wins; a
field you omit inherits the baked-in cell value. You never have to
restate the whole row. Overridable fields:

| Field                  | Meaning                                              |
|------------------------|------------------------------------------------------|
| `wm`                   | write multiplier (cost to re-write a cached block)   |
| `rm`                   | read multiplier (cost to read a warm cached block)   |
| `ttl_seconds`          | cache time-to-live in seconds                        |
| `min_prefix_tokens`    | minimum prefix tokens below which caching stops      |
| `has_storage_rent`     | whether the provider charges per-hour cache rent     |
| `storage_rent`         | per-hour storage-rent multiplier                     |
| `auto_cacher`          | whether the upstream caches automatically            |
| `verified_at`          | verification date (`YYYY-MM-DD`); marks the cell verified and refreshes the 90-day staleness clock without re-asserting any multiplier. A pure verification (only this field set) is recorded with source `operator-verified`. |

**Cost-risk acknowledgement.** An override that sets `wm` *below* the
conservative sentinel value (`2.0`) is **rejected** unless it also carries
`override_acknowledges_cost_risk = true`. A too-cheap write multiplier
makes a cache break look falsely profitable, so dropping below the
sentinel requires an explicit operator acknowledgement:

```toml
[cache_pricing."openai-compat:my-cheap-host-*"]
wm = 1.0
override_acknowledges_cost_risk = true   # required: wm < 2.0 sentinel
```

The selector keys are not validated against the baked table -- a
selector that matches no baked cell is simply inert (it overrides
nothing). A baked cell whose `verified_at` is more than 90 days old logs
a startup `WARN` advising re-verification; this is advisory only and
never blocks startup.

### `routectl pricing` -- inspect and stamp the manifest

Two subcommands let you inspect the effective manifest and stamp
individual cells without editing `config.toml` by hand.

**`routectl pricing list`**

Prints every baked cell as an aligned ASCII table, with overrides and
sidecar verifications already merged in. Columns (in order):
`provider_kind`, `model_glob`, `tier`, `wm`, `rm`, `ttl(s)`,
`min_prefix`, `auto`, `verified`, `source`, `verified_at`, and `stale`.
A verified cell whose `verified_at` is more than 90 days before today
shows `STALE` in the last column.

Override selectors that exactly name a baked cell
(`provider_kind:model_glob`) are reflected in the per-row values.
Selectors using a broader or different glob, or a `"*"` provider, still
apply at request-lookup time but are not reflected per-row. A trailing
note counts how many such selectors are present.

**`routectl pricing verify <selector>`**

Stamps a baked cell verified as of today's local date WITHOUT
re-asserting its multipliers. The selector format is
`provider_kind:model_glob` -- for example, `openai-compat:grok-*`.
A selector that names no baked cell is accepted with a note; the stamp
applies by glob at lookup time.

The stamp is saved to the sidecar file (see below). A running server
does NOT pick up a new verification until it is restarted -- the sidecar
is merged at config-load time and is not file-watched.

**Verification sidecar (`pricing_verifications.json`)**

`routectl pricing verify` writes to a machine-managed JSON sidecar at:

```
$XDG_CONFIG_HOME/routectl/pricing_verifications.json
```

(falling back to `~/.config/routectl/pricing_verifications.json`),
next to `config.toml` and `credentials.json`. Do not edit this file
by hand under normal circumstances.

At config load (every subcommand, including `serve`), the sidecar is
merged into the `[cache_pricing]` override table as `verified_at`-only
overrides. The merge is **additive only**: if `config.toml` already
carries a `[cache_pricing]` entry for the same selector, the
`config.toml` entry wins and the sidecar entry is silently skipped.

A malformed sidecar date is dropped at load time with a `WARN` log
entry and never blocks a command. A completely unparseable sidecar JSON
logs a `WARN` and skips the merge.

Because `config show` prints the RESOLVED (effective) config after the
sidecar merge, sidecar-derived verifications appear in its
`[cache_pricing]` output even though the operator did not write them to
`config.toml` by hand.

**Why it matters**

An unverified cell is treated as insufficient-data by the break-even
gate: no actionable break-even K* can be computed, so the advisory path
returns KEEP. Verifying a cell whose baked multipliers you have
confirmed against the vendor documentation unlocks real break-even
economics on the would-trim advisory path.

## Prompt-cache auto-emission (`[cache]`)

When a caller sends no `cache_control` breakpoint of its own, routectl
can add a single top-level ephemeral 5-minute breakpoint over the
stable cacheable prefix (system prompt + tool name/description strings)
on the dispatch path. For a capable upstream this turns an
otherwise-uncached prefix into a prompt-cache hit on the next request
that reuses it, with no client change. The injection is **lossless**:
it is applied to a per-attempt clone, never the original request; it is
skipped entirely whenever the caller already supplied any breakpoint
(so it can never break a caller's own caching); and the injected shape
is re-validated before dispatch and rolled back on any doubt.

The optional `[cache]` block is the **global** master switch. A missing
block keeps the default: auto-emit enabled.

```toml
[cache]
# Master switch for dispatch-path auto-emission of a top-level
# cache_control breakpoint. Default true.
auto_emit_top_level_breakpoint = true
```

Auto-emit applies to completions and streaming. It is **not** applied to
`count_tokens` (`/v1/messages/count_tokens`), which is a probe and never
writes a cache entry.

### Per-provider switch

Each `[providers.X]` entry carries an optional
`auto_emit_top_level_breakpoint`. `None` (omitted) inherits the global
switch (treated as enabled); `false` disables auto-emit for that
provider even when the global switch is on. The effective decision is
"global on AND provider not explicitly off". Use this to turn auto-cache
off for one upstream without touching the global default -- it is the
first-line remedy for a cache-thrash warning (see LOGGING.md).

```toml
[providers.some-anthropic]
kind = "anthropic-api"
api_key_ref = "literal:PLACEHOLDER"
# Opt this provider out of auto-cache while leaving the global default on.
auto_emit_top_level_breakpoint = false
```

### Per-provider capability (`cache_capability`)

routectl only auto-emits a breakpoint to a provider it knows honors one.
Each provider kind has a **conservative** default capability:

| `kind`              | `supports_top_level_cache_control` | `cache_hit_observable` |
|---------------------|------------------------------------|------------------------|
| `anthropic-api`     | true (default base URL only -- see below) | true            |
| `bedrock`           | false (per-block markers only -- see below) | true                 |
| `openai-responses`  | false (server-side auto-cache; no explicit breakpoint) | true |
| `openai-compat`     | false                              | false                  |
| any unknown kind    | false                              | false                  |

When `supports_top_level_cache_control` is false, auto-emit is skipped
for that provider regardless of the switches above. An operator can
override the default per entry with an explicit `cache_capability`:

```toml
# An anthropic-compatible third-party host that DOES honor a top-level
# breakpoint and DOES report cache hits. Use PLACEHOLDER values.
[providers.compat-anthropic]
kind = "anthropic-api"
api_key_ref = "literal:PLACEHOLDER"
base_url = "https://example.invalid/v1"
cache_capability = { supports_top_level_cache_control = true, cache_hit_observable = true }
```

**anthropic-api custom base URL fails closed.** A `kind =
"anthropic-api"` entry pointed at the default `https://api.anthropic.com`
base URL gets the optimistic `true/true` default -- the real Anthropic
server honors a top-level breakpoint. But a `kind = "anthropic-api"`
entry pointed at any **other** base URL is treated as an
Anthropic-compatible third party that may 400 on or silently drop a
top-level breakpoint. With no operator override, such an entry fails
closed (`false/false`) and is never auto-cached. An operator who knows
their custom-base host supports caching must set `cache_capability`
explicitly to opt in (an explicit override always wins, even on a custom
base URL).

**bedrock fails closed for auto-emit.** Bedrock honors prompt caching
only via per-block markers -- a `cachePoint` block on Converse, a
per-block `cache_control` on Invoke -- never a routectl-injected
top-level marker (on Converse it lands in
`additionalModelRequestFields` and never becomes a `cachePoint`; on
Invoke it is an undocumented top-level field AWS does not honor). So the
`bedrock` default is `false/true`: auto-emit is skipped
(`auto_skipped:no_capability`) rather than silently no-op'd, while hit
usage is still reported back (`cache_hit_observable = true`).
Caller-supplied per-block markers are unaffected and still cache
normally; an operator may override per entry.

## Context reduction (`[reduction]`)

routectl can strip insignificant whitespace from JSON-formatted string
tool content on the dispatch path -- `tool_result.content` and
`tool_use.input` when they hold a JSON-valued **string** (for example, a
tool that returns pretty-printed JSON as text). This shrinks the bytes
sent upstream without changing what the model sees, so it can lower the
token bill for clients whose tools emit indented JSON text. It is a
**no-op** for raw-text tool output (nothing to strip).

The transform is **lossless** and **cache-safe**:

- **Lossless** -- a custom whitespace-only lexer drops insignificant
  whitespace between JSON tokens; it never reparses through serde and
  never reformats string/number/literal token text. Anything that is not
  recognizable JSON is left byte-for-byte unchanged.
- **Cache-safe** -- it mutates ONLY the region strictly after the last
  caller `cache_control` breakpoint (the mutable message tail). The
  cacheable prefix every caller relies on is never touched, so reduction
  can never invalidate a caller's prompt cache. It runs on a per-attempt
  clone, after overlays, never the original request.
- routectl does NOT touch structured JSON `Value` tool content -- those
  are already reserialized compactly on the wire. Only whitespace held
  inside a JSON-valued string survives to the model, so that string is
  the sole target.

Reduction is applied to completions and streaming. It is **opt-in**
(default off).

The optional `[reduction]` block is the **global** master switch. A
missing block keeps the default: reduction disabled.

```toml
[reduction]
# Master switch for dispatch-path context reduction (whitespace-only
# minify of JSON-valued string tool content in the mutable tail).
# Default false.
enabled = true
```

### Per-provider switch

Each `[providers.X]` entry carries an optional `reduction_enabled`.
`None` (omitted) inherits the global switch; `false` disables reduction
for that provider even when the global switch is on. The effective
decision is "global `enabled = true` AND provider not explicitly
`false`".

```toml
[reduction]
enabled = true

[providers.compat-a]
kind = "openai-compat"
api_key_ref = "literal:PLACEHOLDER"
base_url = "https://example.invalid/v1"
# Inherits the global switch (reduction ON for this provider).

[providers.compat-b]
kind = "openai-compat"
api_key_ref = "literal:PLACEHOLDER"
base_url = "https://example.invalid/v1"
# Opt this provider out while leaving the global default on.
reduction_enabled = false
```

The per-request decision token is recorded in the usage DB
`reduction_strategy` column and, when reduction actually strips bytes, a
`context_reduction` line is logged at DEBUG (counts only -- no bodies).
See [LOGGING.md](LOGGING.md).

## Reading usage (`routectl usage`)

`routectl usage` is the read surface over the usage database. It opens
the DB **read-only** (never writes, never migrates, safe to run while the
daemon is live) and prints an aligned ASCII report.

```bash
./routectl usage                       # multi-window summary (default)
./routectl usage --today               # one calendar window
./routectl usage --this-week --by provider
./routectl usage --since 2026-06-01 --until 2026-06-07 --detail
./routectl usage --all --db /path/to/usage.db
```

**Windows (LOCAL time).** Exactly one window selector may be given:

| Flag           | Range                                            |
|----------------|--------------------------------------------------|
| `--today`      | local midnight today -> now                      |
| `--this-week`  | Monday 00:00 of the current ISO week -> now      |
| `--this-month` | the 1st at 00:00 -> now                           |
| `--all`        | all recorded rows                                |
| `--since D [--until E]` | `D` 00:00 -> end of `E` (or now if omitted), local |

The ISO week starts **Monday**. With no window flag and no `--since`,
the command prints a multi-window summary (today / this week / this
month / all time) as separate blocks. There are no rolling
1h/24h/7d/30d windows.

**Breakdown.** `--by model|provider|alias` rolls the rows up to that
dimension; omit `--by` for a single total row. `--detail` adds extra
columns: the 5m/1h cache-write split, p95 (nearest-rank) and max
latency, total wall-time (summed `latency_ms`), and server-tool counts.

**Cost.** Cost is derived from `[registry]` pricing at read time. The
dollar column has three states:

- `$X.XX` -- an API-key provider with a matching `[registry]` price.
- `n/a (subscription)` -- a managed-OAuth provider (its
  `[providers.X] api_key_ref` starts with `oauth://`). Subscription usage
  has no per-token dollar cost; the **quota line** under the table is the
  real spend signal.
- `n/a` -- an API-key provider whose upstream has no `[registry]` price.

A footer reports the window's cache-hit-rate
(`cache_read / (cache_read + input)`) and error count.

If the database does not exist yet (or has never been written), the
command prints `no usage data yet (...)` and exits 0 -- it is not an
error. A database written by a newer routectl than the running binary is
refused with a clear error rather than misread. `--db` overrides the
`[usage] db_path` for one invocation.

## Validating config

```bash
./routectl config check --config <path>    # validates schema before serve
./routectl config show                     # prints resolved config (inspection)
```

`config check` runs the same startup validation `serve` does -- secret
refs resolve, provider kinds map to known impls, alias chains reference
existing model nicknames, Bedrock allowlists include the
routectl-mandatory keys (`messages`, `anthropic_version`, `max_tokens`)
when set, and if any `[providers.X]` Bedrock entry sets an
`anthropic_beta` floor, the validator also confirms that
`allowed_body_fields` includes `anthropic_beta` (so the floor value
actually reaches the upstream). A partial Bedrock allowlist silently
breaks every Bedrock request, so the validator surfaces it as a clean
`Error::Config` at startup rather than a runtime 400.

`config show` prints the post-merge view: `literal:`-prefixed secrets
are redacted to `literal:[REDACTED]`; `env://`, `file://`, and
`oauth://` references remain as opaque URIs (they are non-secret
pointers, not credential values); defaults are filled in; layered
overlays NOT yet applied
(those compose per request, not at startup). Useful when chasing
"why is my model picking provider Y instead of Z" without flipping
trace logging.

For active triage of a specific failing request, combine `config show`
with `ROUTECTL_LOG=routectl=debug` and the `request_id` correlation
workflow -- see [LOGGING.md](LOGGING.md) for the full triage recipes.

## claude-code as a gateway client

### Why route claude-code through routectl

claude-code speaks Anthropic Messages on the wire and routectl
forwards it unchanged to `api.anthropic.com`, translates it into
Bedrock Invoke / Converse, or routes it across a fallback chain.
Pointing claude-code at routectl implements the LLM gateway pattern
Anthropic publishes at <https://code.claude.com/docs/en/llm-gateway>.

**Operating envelope.** Per the Anthropic Agent SDK overview,
claude.ai OAuth tokens may not be embedded in third-party products.
The `oauth://anthropic` ref is for personal-use proxying with the
operator's own subscription token; do not deploy a routectl instance
that resolves your token under other users' requests. routectl does
not support or condone gateway usage beyond what the upstream
provider permits -- see the README "Responsible use" section.

### Operator setup checklist

1. Build and run routectl:

   ```bash
   cargo build --release
   ./target/release/routectl serve --config ~/.config/routectl/config.toml
   ```

2. Configure the Anthropic provider in
   `~/.config/routectl/config.toml`. Two options:

   **routectl-managed OAuth (recommended).** Run
   `routectl login anthropic` once -- it opens the browser to the
   claude.ai consent flow, captures the `sk-ant-oat01-...` token, and
   persists it to `~/.config/routectl/credentials.json`. Then in the
   TOML:

   ```toml
   [providers.anthropic-managed]
   kind          = "anthropic-api"
   api_key_ref   = "oauth://anthropic"
   auth_kind     = "oauth-bearer"
   user_agent    = "claude-cli/2.1.143 (external, cli)"
   forward_client_headers = [
       "x-claude-code-session-id",
       "x-claude-code-agent-id",
       "x-claude-code-parent-agent-id",
   ]
   header_extras = { ... }   # see "Header pack" below
   ```

   The `oauth://anthropic` ref resolves at request time against the
   credentials store; the `auth_kind = "oauth-bearer"` flag emits
   `Authorization: Bearer <token>` instead of `x-api-key`.

   **Anthropic API key (no OAuth).** Standard pattern, no `routectl
   login` needed:

   ```toml
   [providers.anthropic-api]
   kind        = "anthropic-api"
   api_key_ref = "env://ANTHROPIC_API_KEY"
   auth_kind   = "api-key"
   ```

3. Add models and aliases that match what claude-code expects on the
   wire. claude-code 2.1.x sends `claude-haiku-4-5-...`,
   `claude-sonnet-4-...`, `claude-opus-4-...` model strings; the
   suffix-glob aliases collapse the per-version churn:

   ```toml
   [models.anthropic-haiku]
   provider = "anthropic-managed"
   upstream = "claude-haiku-4-5-20251001"

   [models.anthropic-sonnet]
   provider = "anthropic-managed"
   upstream = "claude-sonnet-4-6"

   [models.anthropic-opus]
   provider = "anthropic-managed"
   upstream = "claude-opus-4-7"

   [aliases]
   "claude-haiku-*"  = "anthropic-haiku"
   "claude-sonnet-*" = "anthropic-sonnet"
   "claude-opus-*"   = "anthropic-opus"
   ```

4. Set claude-code env vars in your shell profile (`~/.bashrc`,
   `~/.zshrc`, etc.):

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:9100
   export ANTHROPIC_AUTH_TOKEN=placeholder    # claude-code requires it; routectl uses the oauth:// ref to resolve the real token
   export CLAUDE_CODE_ATTRIBUTION_HEADER=0    # documented as recommended for gateway use; better cache hit rate
   ```

   Put these in the shell profile, NOT in `~/.claude/settings.json`'s
   `env` block -- the settings file overrides per-shell exports.

5. Verify the round-trip:

   ```bash
   claude --print "say hi"
   ```

   The `routectl serve` log should show one
   `INFO request{method=POST path=/v1/messages ...}: ... close
   time.busy=N time.idle=N` line per request. If you see a 401, the
   `oauth://anthropic` token has expired -- re-run `routectl login
   anthropic` (auto-refresh is a follow-up).

### Credential pool (multiple seats per provider)

A single OAuth provider can hold more than one credential ("seat") in
the routectl store. The default seat is the bare provider; additional
seats are named with a `#<label>` suffix. This lets an operator pool a
few same-provider subscriptions and (with the round-robin knob below)
spread load across them.

**Seat refs.** A config `api_key_ref` selects which seat resolves at
request time:

- `oauth://anthropic` -- the default (unlabeled) seat, or the whole
  pool when `seat_selection` is set (the bare ref expands to every
  stored seat for that provider).
- `oauth://anthropic#seat-b` -- one specific labeled seat. Pins exactly
  that credential; never widened to the pool.

**Registering seats.** The four OAuth CLI commands take an optional
`--label`. Without it, every command behaves exactly as before and
targets the default seat:

```bash
# Default seat (unchanged behavior).
routectl login anthropic

# Add a second, independent seat. Does NOT overwrite the default.
routectl login anthropic --label seat-b

# Refresh / log out one named seat only.
routectl refresh anthropic --label seat-b
routectl logout  anthropic --label seat-b   # leaves the default intact

# A bare logout removes ONLY the default seat, leaving labeled seats.
routectl logout anthropic
```

Each seat carries its own credential and its own stable per-credential
identity (the openai-responses `session-id` is minted per seat), so
seats refresh and rotate independently. `routectl whoami` lists every
stored seat grouped under its provider -- the default renders as
`<provider> (default)`, labeled seats as `<provider>#<label>` -- each
with its own expiry.

**Seat selection.** A per-provider `seat_selection` knob picks how
dispatch chooses among the pool's seats:

```toml
[providers.anthropic-managed]
kind          = "anthropic-api"
api_key_ref   = "oauth://anthropic"
auth_kind     = "oauth-bearer"
seat_selection = "round-robin"   # "fill-first" (default) / "round-robin" / "sticky-least-loaded"
```

- `fill-first` (default) -- drain one seat before advancing to the
  next. A single-seat provider (the common case) keeps today's
  behavior with no config.
- `round-robin` -- rotate across seats to spread load.
- `sticky-least-loaded` -- pin each conversation to one seat so its
  warm prompt cache is preserved, while balancing NEW conversations
  across seats by available capacity. A conversation's first request
  picks the least-loaded healthy seat (preferring seats with a closed
  breaker, then the most RPM headroom, with a deterministic tiebreak so
  a burst of new conversations does not herd onto one seat); every
  later request for that conversation routes back to the same seat. If
  that seat later goes unhealthy (rate-limited or breaker-open), the
  conversation migrates ONCE to a healthy sibling and does not flap back
  when the original recovers. Requires an inbound per-conversation key
  (the `x-claude-code-session-id` header Claude Code sends, or body
  `metadata.session_id`); a request without one falls back to
  `fill-first`. The per-request decision is recorded in the usage
  ledger's `selection_decision` column (birth_pick / sticky_stay /
  overflow_repin / defer_no_healthy / keyless_fill_first) for
  diagnostics.

These strategies are applied at dispatch time -- `fill-first` always
starts from seat 0 (fixed priority order), `round-robin` advances the
start seat per request (spreading load across the pool), and
`sticky-least-loaded` leads with the conversation's home seat (the rest
of the pool follows in fill-first order as fallback). Seat selection is
a best-effort ordering hint: the per-seat rate-limit gate and the
fallback chain remain authoritative. An empty or whitespace-only
`--label` is rejected with a clear error, matching the
`oauth://<provider>#<label>` ref parser's rule.

### Header pack ("look like claude-code")

Drop this into `header_extras` on the anthropic-managed provider so
the upstream sees the same SDK fingerprint claude-code 2.1.143 sends
from the bundled `@anthropic-ai/sdk`:

```toml
header_extras = {
    "anthropic-beta"                         = "claude-code-20250219,oauth-2025-04-20",
    "x-app"                                  = "cli",
    "anthropic-dangerous-direct-browser-access" = "true",
    "x-stainless-arch"                       = "x64",
    "x-stainless-lang"                       = "js",
    "x-stainless-os"                         = "Linux",
    "x-stainless-package-version"            = "0.94.0",
    "x-stainless-runtime"                    = "node",
    "x-stainless-runtime-version"            = "v24.3.0",
    "x-stainless-timeout"                    = "600",
    "x-stainless-retry-count"                = "0",
}
```

What each family does (one line each):

- `anthropic-beta` -- the beta gates. routectl unions ingress +
  provider + model `header_extras["anthropic-beta"]` per the
  three-source compose described above.
- `x-app`, `x-stainless-*` -- the SDK fingerprint the
  `@anthropic-ai/sdk` pack emits. Some Anthropic-side analytics keys
  off these; matching claude-code's values keeps cache and metrics
  attribution stable.
- `anthropic-dangerous-direct-browser-access` -- Anthropic-side flag
  claude-cli sets when running outside a browser context. Mirror it.

### What works, what doesn't

| Capability | Status | Note |
|---|---|---|
| `/v1/messages` (sync + streaming) | works | full forward + headers union |
| `/v1/messages/count_tokens` | works | proxied to upstream; first-target only (no fallback chain walk) |
| `/v1/models` (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) | works | glob alias keys and `default` skipped |
| OAuth refresh on 401 | not yet | re-run `routectl login anthropic` to refresh today; auto-refresh is a follow-up |
| `WebSearch` tool on Bedrock | upstream-rejected | claude-code's `web_search_<v>` tool isn't supported by Bedrock; declare `unsupported_features = ["web_search"]` on the Bedrock provider so the chain skips it for web-search requests (see "Per-provider capability filter" above) |
| Tool use + streaming | works | end-to-end SSE, multi-turn |
| Subagent / agent-team dispatch | works | every subagent call flows through the same `/v1/messages` route |
| `claude.ai` Routines / `RemoteTrigger` / `PushNotification` / `ShareOnboardingGuide` | bypass routectl | hardcoded `claude.ai` integrations; the gateway has no visibility |
| `WebFetch` / `WebSearch` model-side calls | bypass routectl | claude-code performs these directly to the user-supplied URL or its bundled search provider |

### Recommended env vars (claude-code side)

| Env var | Value | Why |
|---|---|---|
| `ANTHROPIC_BASE_URL` | `http://127.0.0.1:9100` | route to routectl |
| `ANTHROPIC_AUTH_TOKEN` | (placeholder) | claude-code requires it; routectl ignores when `oauth://` resolves the real token |
| `CLAUDE_CODE_ATTRIBUTION_HEADER` | `0` | omit prompt-fingerprint block; better cache hit rate per the gateway doc |
| `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` | unset | only set to `1` as an emergency exit if a beta flag is causing 400s upstream |
| `ENABLE_TOOL_SEARCH` | `true` (optional) | enable claude-code's tool-search beta; routectl forwards the `tool_reference` blocks correctly |

## ChatGPT / Codex provider

routectl can route to OpenAI's Responses API on the ChatGPT subscription
backend (`https://chatgpt.com/backend-api/codex`) using a ChatGPT OAuth
bearer JWT. Two ways to supply that bearer: routectl-managed OAuth
(recommended) or a static bearer file (kept for backwards-compat with
operators who already manage the JWT externally).

### routectl-managed OAuth (recommended)

Run `routectl login codex` once. routectl spawns a local callback server
on port 1455 (the codex public PKCE client registers fixed redirect URIs
against that port), opens the browser to OpenAI's auth flow, exchanges
the authorization code for an access + refresh token pair, and persists
them to `~/.config/routectl/credentials.json` (atomic write, mode 0600
on Unix; same hygiene as the `file://` secret-ref path).

The login flow is browser-only -- `routectl login codex --print-url`
is rejected, because the OpenAI auth flow has no headless paste-back
landing page. SSH / headless operators should port-forward 1455 to the
local box and use the default browser flow.

Then in `~/.config/routectl/config.toml`:

```toml
[providers.codex]
kind        = "openai-responses"
auth_kind   = "chatgpt-oauth"
api_key_ref = "oauth://codex"
# account_id_ref omitted: routectl reads `chatgpt_account_id` off the
# OAuth-session JWT and injects it as the `ChatGPT-Account-Id` header.
```

The `oauth://codex` ref resolves at request time against the credentials
store; rotation is picked up live without restarting routectl. When the
upstream marks the refresh token expired, reused, or invalidated,
routectl surfaces a "re-run `routectl login codex`" error -- re-run the
login and traffic resumes.

### Static bearer (backwards-compat)

For operators who already manage the JWT externally, point routectl
at the file or env var and supply the account UUID explicitly:

```toml
[providers.codex-static]
kind           = "openai-responses"
auth_kind      = "chatgpt-oauth"
api_key_ref    = "env://OPENAI_JWT"
account_id_ref = "literal:00000000-0000-0000-0000-000000000000"
```

`account_id_ref` is REQUIRED on this path -- there is no OAuth session
for routectl to derive the UUID from. `env://`, `file://`, and
`literal:` refs all work for both fields. routectl never refreshes a
static bearer; rotation is the operator's job.

## xAI (Grok) provider

routectl can route to xAI's OpenAI-compatible API (`https://api.x.ai/v1`) using
an xAI OAuth bearer. The credential is managed the same way as the Codex flow --
PKCE, local callback server, credentials persisted to
`~/.config/routectl/credentials.json`.

### routectl-managed OAuth (recommended)

Run `routectl login xai` once. routectl spawns a local callback server on port
**56121** -- the only redirect URI xAI registers for the public PKCE client
(`http://127.0.0.1:56121/callback`, literal `127.0.0.1`, not `localhost`). The
browser opens to xAI's consent flow; on return the authorization code is
exchanged for an access + refresh token pair.

**Port note.** 56121 is the sole registered port. Unlike the codex flow, there is
no secondary fallback port. If 56121 is busy on your machine, the login will fail
with a clear bind-error rather than silently binding an unregistered port and
confusing xAI's redirect validation.

Then in `~/.config/routectl/config.toml`:

```toml
[providers.xai]
kind        = "openai-compat"
base_url    = "https://api.x.ai/v1"
auth_kind   = "oauth-bearer"
api_key_ref = "oauth://xai"
```

The `oauth://xai` ref resolves at request time against the credentials store;
rotation is picked up live without restarting routectl. When the upstream marks
the refresh token dead (`invalid_grant` on 400/401), routectl surfaces a
"re-run `routectl login xai`" error -- re-run the login and traffic resumes.

**Lazy refresh rotation.** xAI's token endpoint commonly omits `refresh_token`
on a successful refresh (it re-validates the prior token rather than issuing a
new one). routectl preserves the prior refresh token when the response body
omits it -- no operator action is needed.

```toml
[models.grok-3]
provider = "xai"
upstream = "grok-3"

[models.grok-3-mini]
provider = "xai"
upstream = "grok-3-mini"
```
