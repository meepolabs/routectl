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

A routectl config is a single TOML file with up to seven top-level
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

[bedrock]             # global Bedrock allowlists (allowed_betas,
                      # allowed_body_fields). Optional.

[log]                 # operator-configurable runtime log knobs
                      # (trace_headers, trace_body_bytes,
                      # redact_prompts). Optional. The env-filter
                      # directive ROUTECTL_LOG is intentionally
                      # env-only and is NOT part of this block.
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
caller sent. This floor value bypasses the `[bedrock] allowed_betas`
post-filter -- it is written unconditionally AFTER the filter runs.

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

## Per-provider capability filter (`unsupported_features`)

Some upstreams reject specific built-in tool shapes (Bedrock, for
example, currently 400s on Anthropic's `web_search_*` tool families).
The legacy behavior was tried-and-fallback: dispatch to Bedrock, get
a 400, walk to the next chain entry. That burns latency, surfaces a
400 in operator dashboards, and counts the failure against Bedrock's
breaker even though the request never had a chance.

`unsupported_features` is a declarative, operator-supplied list on
each provider's runtime block. The router derives feature keys from
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
supports feature \`web_search\``.

Per-skip events log at DEBUG (`provider skipped: feature in
unsupported_features list`); the terminal empty-chain event logs at
WARN. INFO would flood; the codebase precedent is "fallback events
at WARN, retry-same-provider at DEBUG".

## Per-provider runtime gates

Each `[providers.X]` entry accepts a `[providers.X.runtime]` block with
five knobs. All accounting is per-attempt (not per-request).

| Field                          | Type       | Default         | Effect |
|--------------------------------|------------|-----------------|--------|
| `rpm_limit`                    | Option<u32> | None (disabled) | Maximum requests per minute to this provider. When exceeded the router treats this provider as a fallbackable failure and tries the next chain entry. |
| `circuit_failures`             | Option<u32> | None (disabled) | Trip the circuit breaker after this many consecutive failed attempts. Once tripped, the router skips this provider for `circuit_cooldown_ms`. |
| `circuit_cooldown_ms`          | Option<u64> | 30000 (30s) when `circuit_failures` is set; unused otherwise | How long to keep the circuit open once tripped. |
| `request_timeout_ms`           | Option<u64> | None (no cap)   | Per-attempt request timeout. Alias-level `[aliases.X.retry] request_timeout_ms` always wins; this is the per-provider fallback when the alias-level field is unset. Resolution order: provider > global `[retry] request_timeout_ms` > None. |
| `stream_first_byte_timeout_ms` | Option<u64> | None            | Per-provider first-byte timeout for streaming responses. Resolution order: per-model > per-provider > global `[retry] stream_first_byte_timeout_ms`. |

`rpm_limit` and circuit breaker are both `None` (disabled) when omitted.
`circuit_cooldown_ms` is only meaningful when `circuit_failures` is set;
its default of 30s applies when `circuit_failures` is present but
`circuit_cooldown_ms` is absent.

Example:

```toml
[providers.bedrock.runtime]
rpm_limit              = 60
circuit_failures       = 5
circuit_cooldown_ms    = 60000
request_timeout_ms     = 120000
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

Workspace defaults are tight; bump per-provider for known-slow
upstreams via the `stream_first_byte_timeout_ms` table in
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#stream_first_byte_timeout_ms)
rather than loosening the global.

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
