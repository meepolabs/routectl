# routectl Configuration Reference

TOML configuration schema reference: every knob, how the layered
overlays merge, what's reserved.

> **In a hurry:** run `routectl init` (guided setup wizard), or copy
> [`examples/config.toml`](../examples/config.toml)
> for a working end-to-end config (print one anytime with
> `routectl config example`). Then jump to
> [Getting started](#getting-started-provider--model--alias),
> [claude-code as a gateway
> client](#claude-code-as-a-gateway-client), or
> [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) for upstream-specific
> tuning.

## Contents

**Basics**
- [Getting started: provider + model + alias](#getting-started-provider--model--alias)
- [Top-level shape](#top-level-shape)
- [Config schema version (`version`)](#config-schema-version-version)
- [Editor autocomplete (JSON Schema)](#editor-autocomplete-json-schema)
- [Listener auth + routing](#listener-auth--routing)
- [Validating config](#validating-config)

**Providers**
- [Per-provider runtime gates](#per-provider-runtime-gates) (RPM, circuit breaker, timeouts)
- [Per-provider capability filter](#per-provider-capability-filter-unsupported_features)
- [Gemini](#providersx-gemini-kind--gemini) -
  [ChatGPT / Codex](#chatgpt--codex-provider) -
  [xAI (Grok)](#xai-grok-provider)
- Bedrock: [api_shape](#providersx-api_shape----bedrock-api-selector) -
  [beta-flag controls](#bedrock-and-anthropic-api-beta-flag-controls) -
  [body-field allowlist](#bedrock-allowed_body_fields----global-bedrock-body-field-allowlist) -
  [mantle Anthropic lane](#providersxbedrock_mantle----bedrock-mantle-anthropic-lane) -
  [mantle OpenAI lanes](#providersxbedrock_mantle----bedrock-mantle-openai-lanes)
- anthropic-api flags: [context_management](#context_management-anthropic-api-provider-flag) -
  [credential_source (forwarded)](#credential_source-anthropic-api-provider-flag----forwarded-credential)

**Models and routing**
- [Model routing (`[aliases]`)](#model-routing-aliases)
- [Model directory (`[models]`)](#model-directory-models)
- [Per-model knobs](#per-model-knobs) (reasoning, effort, output caps)
- [history_reasoning](#history_reasoning-reasoning-echo-back)
- [Retry and fallback defaults](#retry-and-fallback-defaults)
- [Proactive context-window gate (`[window_gate]`)](#proactive-context-window-gate-window_gate)
- [Learned token-estimate correction (`[calibration]`)](#learned-token-estimate-correction-calibration)
- [Quota-aware seat placement (`[seat_quota]`)](#quota-aware-seat-placement-seat_quota)

**Caching and cost**
- [Prompt-cache auto-emission (`[cache]`)](#prompt-cache-auto-emission-cache)
  ([break-even emission gate](#break-even-emission-gate-k_gated_emission))
- [Context reduction (`[reduction]`)](#context-reduction-reduction)
  ([kill switch + recovery](#kill-switch-reduction-enabled-is-the-live-off-switch))
- [Steady-state advisory trim (`[trim]`)](#steady-state-advisory-trim-trim)
- [Pricing registry](#pricing-registry-registrypatternpricing)
- [Catalog: prompt-cache economics](#catalog-prompt-cache-economics-routectl-catalog)
  ([retired `[cache_pricing]`](#retired-cache_pricing))
- [Learned capability tempo (`[capability]`)](#learned-capability-tempo-capability)

**Operating the daemon**
- [Log knobs (`[log]`)](#log-knobs-log)
- [Usage accounting (`[usage]`)](#usage-accounting-usage)
- [Reading usage (`routectl usage`)](#reading-usage-routectl-usage)
- [Diagnostics (doctor / probe)](#diagnostics-routectl-doctor-and-routectl-provider-probe)
- [Inspecting a request offline (`prompt-size`)](#inspecting-a-request-offline-routectl-prompt-size)
- [Inspecting the effective config](#inspecting-the-effective-config-config-show---effective)
- [Editing config from the CLI](#editing-config-from-the-cli-config-set--config-unset)
- [MITM front-proxy (`[mitm]`)](#mitm-front-proxy-mitm)

**Client integrations**
- [claude-code as a gateway client](#claude-code-as-a-gateway-client)

**Advanced: overlay merge internals**
- [Field-assignment table](#field-assignment-table)
- [header_extras merge](#header_extras-merge) /
  [payload_extras merge](#payload_extras-merge) /
  [Reserved-header buckets](#reserved-header-buckets)
- [Worked example: three-source anthropic-beta compose](#worked-example-three-source-anthropic-beta-compose)

## Getting started: provider + model + alias

Three blocks route your first request. A **provider** says how to
reach an upstream (transport + auth), a **model** names one upstream
model on that provider, and an **alias** maps the model string your
client sends to that model (or to a fallback chain of them):

```toml
version = 3           # config schema version; routectl refuses older
                      # files until `routectl config migrate` runs

[providers.anthropic]
kind        = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.heavy]
provider = "anthropic"
upstream = "claude-opus-4-20250514"

[aliases]
heavy   = "heavy"
default = "heavy"     # catch-all for unmatched model strings
```

Save as `~/.config/routectl/config.toml`, then `routectl config check`
and `routectl serve`. Credentials always arrive as references --
`env://VAR`, `file:///abs/path` (owner-only, 0600), or
`oauth://<provider>` (populated by `routectl login <provider>`);
inline `literal:` values are rejected at parse and resolve. To add a
provider interactively (with secret capture) use
`routectl provider add`; the rest of this document is the full
reference for everything the wizard does not cover.

## Top-level shape

A routectl config is a single TOML file with the following top-level
sections:

```toml
version = 3           # config schema version; see "Config schema version"

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
                      # (auto_emit_top_level_breakpoint,
                      # normalize_tools, k_gated_emission). Optional;
                      # emission and normalization default on, the
                      # break-even emission gate defaults off.

[reduction]           # dispatch-time context-reduction policy
                      # (enabled). Optional; default on.

[trim]                # steady-state advisory-trim knobs (trigger_tokens,
                      # clear_at_least_tokens, head_keep_messages,
                      # keep_recent_messages). Optional; default
                      # 100000 / 20000 / 2 / 6. Advisory only -- never
                      # mutates a dispatched request.

[window_gate]         # proactive context-window gate kill switch
                      # (enabled). Optional; default on.

[calibration]         # learned per-lane token-estimate correction kill
                      # switch (enabled). Optional; default on.

[seat_quota]          # quota-aware seat placement kill switch
                      # (enabled). Optional; default on.

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

# Prompt-cache economics (wm/rm/ttl/...) live outside config.toml as of
# schema v2: a baked table plus catalog_overlay.json, managed via
# `routectl catalog`. A v1 config's [cache_pricing] table is a hard
# startup error, never migrated on load; `routectl config migrate` is
# what folds it into the overlay. See "Retired: [cache_pricing]" below.

[mitm]                # MITM front-proxy: local TLS-terminating
                      # listener fronting a first-party upstream.
                      # Optional; presence gates the feature on
                      # (absence = zero proxy startup).
```

[`examples/config.toml`](../examples/config.toml) is a working
end-to-end reference; [`examples/bedrock.toml`](../examples/bedrock.toml)
ships an empirical Bedrock allowlist baseline (16 betas + 16 body
fields). Copy and edit; do not re-derive. Carrying a v1
`[cache_pricing]` table: see
[Retired: `[cache_pricing]`](#retired-cache_pricing).

## Config schema version (`version`)

```toml
version = 3
```

- **`version`** (u32, required in practice) -- the config schema version
  this file is written against. The current version is `3`
  (`CURRENT_CONFIG_VERSION` in
  `crates/routectl-router/src/config/validate.rs`); a file omitting the
  key reads as the legacy `1`.

The version is checked in a preflight that reads the key straight off the
raw TOML text, BEFORE the typed deserialize runs. That ordering is what
makes the two out-of-range verdicts actionable instead of surfacing as a
confusing `unknown field` error:

- **Too old** (`version` below the current one, or absent) -- load fails
  naming `config migrate`. The loader NEVER migrates on load and never
  writes to the file: `routectl config migrate` is the only thing that
  rewrites it, and it does so format-preservingly (operator comments
  survive) in a two-phase commit that stamps the new `version` last.
- **Too new** (`version` above what this build supports) -- load fails
  naming a binary upgrade, so a config written by a newer routectl is
  never partially interpreted by an older one.

The preflight only ever speaks about the `version` key. TOML that does not
parse at all, or a `version` that is present but is not a plain
non-negative integer, falls through to the normal typed deserialize so the
precise syntax or type error is the one reported.

Two keys are retired by the ladder rather than removed silently: v1's
`[cache_pricing]` table folds into the catalog overlay (see
[Retired: `[cache_pricing]`](#retired-cache_pricing)), and v2's raw-status
retry allow/deny escape hatch becomes per-class policy (see
[Per-class retry and fallback policy](#per-class-retry-and-fallback-policy-retryclasses)).
`version` is hot-reloadable: a live swap to a config carrying an
out-of-range version is REJECTED and the prior router keeps serving.

## Editor autocomplete (JSON Schema)

routectl commits a JSON Schema for the `config.toml` surface at
[`routectl.schema.json`](../routectl.schema.json) in the repo root. Point
a schema-aware TOML editor at it for inline field completion, hover
descriptions, and type checking as you edit. With the Even Better TOML
VS Code extension you can either add a first-line directive to the config
file itself:

```toml
#:schema ./routectl.schema.json
```

or map the file to the schema in your editor settings
(`evenBetterToml.schema.associations`).

The schema is generated from routectl's own config types (`cargo run
--bin gen_schema`) and a golden test fails the build if the committed
file drifts from those types, so it always matches the binary you built.
Its root carries an `x-routectl-config-version` marker naming the config
schema version it was generated against. The schema deliberately does NOT
pin the `version` field to a single value -- editors must keep accepting
older, still-migratable configs the binary still loads (see "Catalog:
prompt-cache economics" below for the v1 -> v2 auto-migration).

## Listener auth + routing

```toml
[server]
host = "127.0.0.1"
port = 8787
strict_translation = false      # set true for production CI
max_body_bytes = 33554432       # 32 MiB default; larger bodies are rejected with HTTP 413
allow_disable_fallbacks = true  # set false for hardened multi-tenant deployments

[server.auth]
tokens = ["env://ROUTECTL_LISTENER_TOKEN", "file:///etc/routectl/listener-token"]

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

`tokens` entries are secret-refs (`env://`, `file://`) resolved at
startup. Inline `literal:` refs are rejected at parse -- the resolver
names `env://` and `file://` as the alternatives. Bind non-loopback only
with `--unsafe-public` on the CLI.

On a non-loopback bind, routectl refuses to start unless at least one
`[server.auth].tokens` entry is configured. The startup error names
the bind address: `"refusing to serve on public bind '<addr>' without
[server.auth].tokens"`. Loopback binds (127.x.x.x, ::1,
::ffff:127.0.0.1) are exempt so the default local-dev workflow
requires no auth.

**Incompatible with the `[mitm]` Remote Control feature:** enabling
`[server.auth].tokens` alongside `[mitm]` breaks Remote Control. The
MITM proxy re-injects the Anthropic inference request into routectl's
own listener carrying the client's claude.ai session token, verbatim,
as `Authorization` -- listener auth would reject that token since it
is not one of the configured `[server.auth]` tokens. See
[REMOTE-CONTROL.md](REMOTE-CONTROL.md) "Limitation: Remote Control
requires listener auth OFF".

### Request routes

routectl exposes three ingress dialects, all behind listener auth (and
the `max_body_bytes` cap):

- `POST /v1/chat/completions` -- OpenAI Chat Completions requests.
- `POST /v1/messages` (+ `POST /v1/messages/count_tokens`) -- Anthropic
  Messages requests.
- `POST /v1/responses` -- OpenAI Responses API requests (the shape a
  Codex client sends). routectl is stateless, so the Responses
  server-side conversation state is handled deterministically:
  - `previous_response_id` -> **HTTP 400** (`invalid_request_error`).
    The reference points at a prior turn routectl never stored;
    answering anyway would be a silent wrong answer. Configure the
    client to send the full conversation `input` each turn.
  - `store: true` without a `previous_response_id` -> **accepted**. The
    current turn is self-contained so the answer is correct; the
    persistence intent is ignored and logged at WARN (a later
    retrieval-by-id against this stateless proxy will find nothing).
  - `store: false` / absent -> normal stateless path.

`GET /v1/models` lists the configured aliases; `GET /health` is the
only route outside the auth layer (so liveness probes work under
`--unsafe-public`).

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

## Model routing (`[aliases]`)

`[aliases]` is one flat table mapping the wire `model` string a client
sends to a `[models.X]` nickname, or to an ordered fallback chain of
nicknames. A single string is a one-entry chain; a list is a chain walked
in order.

```toml
[aliases]
"claude-opus-*"   = "heavy"                 # suffix-glob key
"claude-haiku-*"  = ["fast", "fast-backup"] # fallback chain
"gpt-4o"          = "compat-4o"             # exact key
default           = "heavy"                 # catch-all
```

**Key grammar.** A key is either an exact string or a prefix followed by a
single trailing `*`. Anything else is rejected at `config check`: a bare
`"*"` (use `default` instead), an embedded asterisk (`"foo-*-bar"`), and
multiple asterisks are all errors naming the offending key verbatim.

**Resolution order** for an incoming wire model:

1. exact key in `[aliases]`
2. longest-prefix suffix-glob key
3. direct `[models.X]` nickname (the wire model IS a nickname)
4. the `default` key
5. otherwise an unknown-alias error

**Shadowing:** when a string is both an alias key and a model nickname,
the ALIAS wins -- that is what lets an operator put a fallback chain
behind an existing nickname (`foo = ["foo-primary", "foo-backup"]`). Glob
keys shadow direct nicknames too. The `default` catch-all is consulted
LAST, after a direct-nickname hit, so a known nickname is never captured
by the default.

**Chain entries may themselves be alias keys.** They expand inline,
depth-first, preserving the operator's stated order: with
`A = ["B", "C"]` and `B = ["X", "Y"]`, `A` resolves to `[X, Y, C]`.
Recursion is capped (depth 8) as a defensive net; genuine cycles and
unknown targets are caught at startup by `config check`, which also
rejects an empty chain and a chain referencing a `selectable = false`
model.

**Header override.** A client may send `x-routectl-alias: <key>`, which
wins over the body's `model` field entirely. `[aliases]` is
hot-reloadable: an edit applies to the next request with no restart.

## Model directory (`[models]`)

Each `[models.X]` entry binds a logical nickname (the table key) to one
transport and one upstream model id. `[aliases]` references entries by
nickname; nothing else in the config does.

```toml
[models.heavy]
provider   = "anthropic"                  # required; a [providers] key
upstream   = "claude-opus-4-20250514"     # required; the upstream wire id
selectable = true                         # default true
```

- **`provider`** (string, required) -- the `[providers.X]` key this model
  dispatches through. Validated at startup against the provider table.
- **`upstream`** (string, required) -- the model id forwarded to the
  upstream verbatim. For Bedrock this is the inference-profile id (e.g.
  `us.anthropic.claude-haiku-4-5-20251001-v1:0`); for OpenAI-shaped
  egresses it is the wire `model` field.
- **`selectable`** (bool, default true) -- set `false` to keep an entry
  around while wiring without making it servable. A disabled entry still
  loads, but `config check` errors if an alias chain references it.

Everything else on `[models.X]` is a per-model behavior knob -- reasoning
declaration, output caps, header and payload extras, response labels --
documented under [Per-model knobs](#per-model-knobs), with the full merge
semantics in the [field-assignment table](#field-assignment-table).
`[models]` is hot-reloadable.

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
| `credential_source`            | `[providers.X]` AnthropicApi    | string, default `"own"`; `"forwarded"` requires empty `api_key_ref` + `base_url` pinned to `api.anthropic.com` (see "credential_source" below) |
| `user_agent`                   | `[providers.X]`     | provider-only                                                          |
| `runtime` (RPM, breaker, timeouts, `unsupported_features`) | `[providers.X]` | provider-only                                                          |
| `allowed_betas`                | `[providers.X]` AnthropicApi    | provider-only; allowlist for `anthropic_beta` flags to `api.anthropic.com`; empty = pass-through |
| `allowed_betas`                | `[bedrock]` global              | global filter for Bedrock-accepted `anthropic_beta` values; empty = pass-through (see `[bedrock]`) |
| `anthropic_beta`               | `[providers.X]` Bedrock         | provider-only; operator-asserted floor always sent, bypasses `[bedrock] allowed_betas`            |
| `max_body_bytes`               | `[server]`                      | u32 bytes, default 33554432 (32 MiB); caps inbound body size; HTTP 413 on excess; restart required |
| `allow_disable_fallbacks`      | `[server]`                      | bool, default true; when false the `x-routectl-disable-fallbacks` per-request header is ignored   |
| `auto_emit_top_level_breakpoint` | `[cache]` global              | bool, default true; master switch for dispatch-path auto-cache (see `[cache]`)                    |
| `normalize_tools`              | `[cache]` global                | bool, default true; stable-sorts the tool array on the OAuth Anthropic path for cache stability (see `[cache]`) |
| `k_gated_emission`             | `[cache]` global                | bool, default **false**; when true, withholds auto-emitted markers on a session whose measured reuse sits below the marker's break-even (see `[cache]`) |
| `auto_emit_top_level_breakpoint` | `[providers.X]`               | `Option<bool>`, default None (inherits global); `false` disables auto-cache for this provider     |
| `auto_emit_per_block_breakpoints` | `[providers.X]`              | `Option<bool>`, default None (-> per-kind default: true for default-base `anthropic-api`, false elsewhere); gates the per-block FRONT marker only, and is inert on kinds whose egress cannot carry one |
| `cache_capability`             | `[providers.X]`                 | `Option<{supports_top_level_cache_control, cache_hit_observable}>`, default None (-> conservative per-kind default) |
| `enabled`                      | `[reduction]` global            | bool, default true; master switch for dispatch-path context reduction (see `[reduction]`)         |
| `reduction_enabled`            | `[providers.X]`                 | `Option<bool>`, default None (inherits global); `false` disables reduction for this provider      |
| `trigger_tokens`               | `[trim]` global                 | u64, default 100000; see `[trim]`                                                                 |
| `clear_at_least_tokens`        | `[trim]` global                 | u64, default 20000; see `[trim]`                                                                  |
| `head_keep_messages`           | `[trim]` global                 | usize, default 2; see `[trim]`                                                                    |
| `keep_recent_messages`         | `[trim]` global                 | usize, default 6; see `[trim]`                                                                    |
| `enabled`                      | `[window_gate]` global          | bool, default true; kill switch for the proactive context-window gate (see `[window_gate]`)       |
| `enabled`                      | `[calibration]` global          | bool, default true; kill switch for the learned per-lane estimate correction (see `[calibration]`)|
| `enabled`                      | `[seat_quota]` global           | bool, default true; kill switch for quota-aware seat placement (see `[seat_quota]`)               |
| `enabled`                      | `[capability]` global           | bool, default true; master switch for the learned-capability subsystem (see `[capability]`)       |
| `decay_hours`                  | `[capability]` global           | u64, default 48; hours a learned negative acts before a re-probe (see `[capability]`)             |
| `inferred_window_hours`        | `[capability]` global           | u64, default 1; window a pending inferred signal waits for confirmation (see `[capability]`)      |
| `staleness_hint_days`          | `[capability]` global           | u64, default 14; display-only age past which a capability reads as stale in diagnostics (see `[capability]`) |

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

Two flags bypass this filter unconditionally: see
[the capability-beta carve-out](#allowed_betas-carve-out-the-capability-betas).

### `[bedrock] allowed_body_fields` -- global Bedrock body-field allowlist

The `[bedrock]` block's second list, and the only other key on it. Bedrock's
strict-schema validator 400s any unrecognized field with "Extra inputs are
not permitted", and routectl's forward-compat sweep forwards
quarterly-added Anthropic body fields (`context_management`,
`context_hint`, `speed`, ...) it does not itself model. This list is what
keeps those from reaching an account whose Bedrock schema has not caught
up.

```toml
[bedrock]
allowed_body_fields = ["messages", "anthropic_version", "max_tokens",
                       "system", "tools", "anthropic_beta"]
```

- **Empty / omitted = pass-through (the default).** No filtering; the
  assembled body and the Converse extras bag are forwarded as-is. This is
  discovery mode: bring routectl up, observe what is actually sent with
  `ROUTECTL_LOG=routectl_providers::bedrock=trace`, then populate the
  list. The empirical baseline is in
  [`examples/bedrock.toml`](../examples/bedrock.toml) -- copy it rather
  than re-deriving.
- **Non-empty = allowlist.** Every key not on the list is dropped before
  egress, logged at `debug` (not `warn`: the forward-compat sweep produces
  forwarded keys on every request, so a WARN would flood the log).
- **Two surfaces, one list.** On `api_shape = "invoke"` it filters the
  top-level Anthropic Messages body, so the structural keys routectl
  writes (`messages`, `system`, `tools`, ...) must be ON the list or the
  assembled body is malformed. On Converse those keys live at the AWS top
  level and never appear in the filtered
  `additionalModelRequestFields` bag.

Two coherence checks run at startup, reload, and `config check`, both only
when the list is non-empty:

- The routectl-mandatory keys `messages`, `anthropic_version`, and
  `max_tokens` must be present -- but only when some provider uses
  `api_shape = "invoke"`; a Converse-only deployment is unaffected.
- `anthropic_beta` must be present when any Bedrock provider sets an
  `anthropic_beta` floor, or the filter would silently drop the
  operator-asserted always-send value.

A partial allowlist silently breaks every Bedrock request, so both are
hard errors at startup rather than a runtime 400. `[bedrock]` is
hot-reloadable.

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
anthropic_beta = ["computer-use-2025-01-24"]
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

On an `auth_kind = "oauth-bearer"` provider talking to `api.anthropic.com`,
routectl also injects a 9-flag model-agnostic floor
(`default_claude_code_anthropic_betas()`: `claude-code-20250219`,
`oauth-2025-04-20`, `interleaved-thinking-2025-05-14`,
`context-management-2025-06-27`, `prompt-caching-scope-2026-01-05`,
`structured-outputs-2025-12-15`, `fast-mode-2026-02-01`,
`redact-thinking-2026-02-12`, `token-efficient-tools-2026-03-28`). The floor
bypasses this allowlist -- those nine are operator-equivalent pins.

The floor carries ONLY model-agnostic flags. Model-gated ones
(`context-1m-2025-08-07`, `effort-2025-11-24`,
`thinking-token-count-2026-05-13`, `mid-conversation-system-2026-04-07`,
`advisor-tool-2026-03-01`) are NOT injected, because forcing them 400s models
that do not support them. They travel as ordinary client-driven flags, which
means `allowed_betas` now APPLIES to them where the floor previously bypassed
it: a non-empty `allowed_betas` that omits `context-1m-2025-08-07` drops a
caller's request for it.

### `allowed_betas` carve-out: the capability betas

Two flags are exempt from BOTH allowlists above. Each is force-added
whenever the request's ASSEMBLED body carries the field that flag gates:

| Body field | Beta force-added | Lanes |
| --- | --- | --- |
| `output_config.format` (structured outputs) | `structured-outputs-2025-12-15` | `anthropic-api` (`anthropic-beta` header) and Bedrock (body `anthropic_beta` array) |
| `output_config.effort` (adaptive thinking) | `effort-2025-11-24` | own-OAuth to `api.anthropic.com` only |

Both bypass `[providers.X] allowed_betas` and `[bedrock] allowed_betas`.

Neither is a client-opted beta: each is a routectl-derived capability
signal implied by the feature the request is already using. A 2026-08-11
live capture on `api.anthropic.com` (one lane, one seat, one model)
accepted `output_config.format` both with and without
`structured-outputs-2025-12-15`, so that union is retained as
belt-and-braces rather than a proven hard requirement; the
`output_config.effort` case remains unmeasured. Dropping either flag is
therefore not a reliable way to constrain a request -- an older account or
model tier may still gate the field.

Both carve-outs are ONE-WAY -- the body's field adds the flag; a
caller-supplied flag with no matching field is passed through untouched
(routectl never manufactures the field to match a flag).

An operator who wants to deny either feature should deny the FEATURE --
declare it in the provider's `unsupported_features` so requests using it
are never routed to that provider -- rather than relying on the beta
allowlist.

### OAuth lane: `temperature` and `top_p` are dropped

Anthropic's OAuth seat rejects a `/v1/messages` body carrying
`temperature` or `top_p`. On an `auth_kind = "oauth-bearer"` provider
talking to `api.anthropic.com` (excluding the forwarded / pure-proxy
leg), routectl removes both from the outbound body and logs one
structured `WARN` per affected request naming the dropped keys.
`stop_sequences` is unaffected.

This gate is the LANE, not the cloak setting: **`cloak.mode = "never"` on
such a provider STILL drops these params.** That is intended -- the 400 is
a property of the credential, not of the disguise, so honouring the knob
would mean failing the request instead. If you need `temperature` or
`top_p` honoured, route the request to an API-key provider or a
non-Anthropic host.

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

## `[providers.X.bedrock_mantle]` -- Bedrock mantle Anthropic lane

An `anthropic-api`-kind provider reaches AWS Bedrock's managed mantle
endpoint (Anthropic Messages vocabulary, AWS-authenticated) by adding a
`[providers.X.bedrock_mantle]` sub-table. The mere PRESENCE of the
sub-table selects the lane. The provider speaks the ordinary Anthropic
Messages API (`complete`, `stream`, `count_tokens`), but every request is
SigV4/bearer-signed under the `bedrock-mantle` service scope and NO
`x-api-key` is sent.

```toml
[providers.bedrock-mantle]
kind = "anthropic-api"

[providers.bedrock-mantle.bedrock_mantle]
region = "us-east-1"
creds  = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

[models.claude-mantle]
provider = "bedrock-mantle"
upstream = "claude-haiku-4-5-20251001-v1:0"   # bare model id, no "us." prefix
```

Fields:

- `region` (required, non-empty) -- the AWS region the mantle endpoint
  lives in (e.g. `us-east-1`). It is the SINGLE source of truth: the
  factory derives both the endpoint host
  (`https://bedrock-mantle.<region>.api.aws/anthropic`) and the SigV4
  signing scope from it. Do NOT set `base_url` on a mantle provider.
- `creds` (required) -- the credential descriptor. A `kind` tag selects
  one of four shapes:
  - `{ kind = "bearer-key", key_ref = "<secret-uri>" }` -- a long-term
    Bedrock API key sent as `Authorization: Bearer`. `key_ref` resolves a
    secret URI (`env://`, `file://`).
  - `{ kind = "static", access_key_ref = "<uri>", secret_key_ref = "<uri>", session_token_ref = "<uri>" }`
    -- static AWS access/secret keys signed with SigV4. `session_token_ref`
    is optional (set it for short-term STS credentials).
  - `{ kind = "profile", name = "<profile>" }` -- a named profile from the
    AWS shared-credentials file; SigV4-signed.
  - `{ kind = "default-chain" }` -- the AWS default credential provider
    chain (environment, profile, SSO, container/instance roles);
    SigV4-signed.

Validation (enforced on build, reload, and `config check`) rejects an
incoherent mantle entry:

- `auth_kind = "oauth-bearer"` -- REJECTED. The lane never carries a
  Claude Code OAuth token (whose identity headers and User-Agent must
  never reach AWS); leave `auth_kind` at its `"api-key"` default.
- `credential_source` not `"own"` -- REJECTED. The credential comes from
  `bedrock_mantle.creds`; `own` (the default) is the only coherent value.
- a non-empty `api_key_ref` -- REJECTED. `creds` is the single credential
  source, so a stray `api_key_ref` is dead config.
- a non-default `base_url` -- REJECTED. `region` derives the endpoint, so a
  manual `base_url` would drift from it.
- an empty / whitespace-only `region` -- REJECTED.

The mantle lane uses a no-redirect client: any 3xx from the upstream is a
fault to surface, never followed (auto-following a signed POST would
replay the SigV4 signature cross-host). AWS-shaped error envelopes
(`SignatureDoesNotMatch`, `ThrottlingException`, `RequestTimeTooSkewed`)
are lifted into the classified failure so a bad credential surfaces as an
auth failure and a throttle as rate-limited. The first-party lane (no
`bedrock_mantle` sub-table) uses the same no-redirect client: `x-api-key`
and the Claude-Code identity headers are not covered by reqwest's
default cross-host header strip list, so a 3xx from the configured host
is likewise surfaced as a fault rather than chased to a different host.
Every credentialed egress lane in routectl (Anthropic, OpenAI-compat,
OpenAI Responses, Gemini, native Bedrock) shares this posture.

Production credential guidance: use a SigV4 source (`static` with
long-term keys, `profile`, or `default-chain`) or a long-term Bedrock API
key (`bearer-key`). Short-term keys (a `bearer-key` from the console or
`static` with a `session_token_ref`) expire and are DEV-ONLY -- the lane
does not refresh them, so they are unsuitable for a long-running daemon.

`bedrock_mantle` requires the `bedrock` build feature. If the key is
present in TOML but the binary was built without `bedrock`, config load
fails with a clean feature-gated-field error.

## `[providers.X.bedrock_mantle]` -- Bedrock mantle OpenAI lanes

The same `[providers.X.bedrock_mantle]` sub-table reaches the mantle
endpoint's OpenAI vocabularies from an `openai-responses`-kind or
`openai-compat`-kind provider. As on the Anthropic lane, the mere PRESENCE
of the sub-table selects the lane: every request is SigV4/bearer-signed
under the `bedrock-mantle` service scope, NO first-party `Authorization:
Bearer` is sent, and the client follows no redirects. `region` is the
single source of truth -- the factory derives the endpoint host
(`https://bedrock-mantle.<region>.api.aws/openai/v1`) and the SigV4 scope
from it; do NOT set `base_url`. The `creds` descriptor takes the same four
`kind` shapes documented for the Anthropic lane above (`bearer-key`,
`static`, `profile`, `default-chain`).

### `openai-responses` mantle lane

```toml
[providers.mantle-responses]
kind        = "openai-responses"
api_key_ref = ""   # required key, must be empty; the sub-table carries the credential

[providers.mantle-responses.bedrock_mantle]
region = "us-east-1"
creds  = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

[models.gpt-oss-mantle]
provider = "mantle-responses"
upstream = "openai.gpt-oss-120b"   # bare model id
```

The Responses lane persists nothing: `store` is forced `false` on every
request (and the `reasoning.encrypted_content` include is forced on so
reasoning survives across turns without server-side storage). A `store`
key in `payload_extras` is REJECTED at config load -- the flag is not a
knob on this lane.

Validation (build, reload, `config check`) rejects an incoherent entry:

- a non-empty `api_key_ref` -- REJECTED (`creds` is the single credential
  source). The key is REQUIRED on this variant, so write it as an empty
  string rather than omitting it -- an omitted key fails the parse with
  `missing field api_key_ref` before validation runs.
- a set `account_id_ref` -- REJECTED (a ChatGPT-account id has no meaning
  on the mantle lane).
- a non-default `base_url` -- REJECTED (`region` derives the endpoint).
- an empty / whitespace-only `region` -- REJECTED.
- a `store` key in `payload_extras` -- REJECTED (see above).
- `auth_kind = "bedrock-mantle"` WITHOUT the sub-table -- REJECTED with a
  hard error naming the block form. See the migration note below.

Migration from the legacy `auth_kind = "bedrock-mantle"` form: earlier
builds selected a bearer-only mantle Responses lane with
`auth_kind = "bedrock-mantle"` plus an `api_key_ref`. That form is closed
-- it silently defaulted the region and could not meet the SigV4 posture.
Replace it with the `[providers.X.bedrock_mantle]` sub-table carrying
`region` and `creds`:

```toml
# OLD (rejected):
# [providers.mantle-responses]
# kind        = "openai-responses"
# auth_kind   = "bedrock-mantle"
# api_key_ref = "env://AWS_BEARER_TOKEN_BEDROCK"

# NEW:
[providers.mantle-responses]
kind        = "openai-responses"
api_key_ref = ""

[providers.mantle-responses.bedrock_mantle]
region = "us-east-1"
creds  = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }
```

Stating `auth_kind = "bedrock-mantle"` ALONGSIDE the sub-table is
redundant but accepted (the factory sets the runtime marker from the
block's presence).

### `openai-compat` mantle lane

```toml
[providers.mantle-compat]
kind        = "openai-compat"
api_key_ref = ""   # empty; the sub-table carries the credential

[providers.mantle-compat.bedrock_mantle]
region = "us-east-1"
creds  = { kind = "default-chain" }

[models.gpt-oss-compat]
provider = "mantle-compat"
upstream = "openai.gpt-oss-20b"   # bare model id
```

`base_url` is optional on an `openai-compat` provider only when the
mantle sub-table is present (the region derives the endpoint). A
non-mantle `openai-compat` provider still requires a non-empty `base_url`.

Validation rejects an incoherent entry:

- a non-empty `api_key_ref` -- REJECTED (`creds` is the single credential
  source).
- a non-empty `base_url` -- REJECTED (`region` derives the endpoint).
- an empty / whitespace-only `region` -- REJECTED.

`count_tokens` on the mantle compat lane returns a deterministic 501
(`NotImplemented`): the router never walks the compat lane for token
counting, so it never dials the signed endpoint for it.

### Shared behavior and production guidance

Both OpenAI lanes share the Anthropic lane's no-redirect posture and
AWS-error handling: a 3xx on the signed POST is surfaced as an upstream
fault (never followed), and AWS-shaped error envelopes are lifted into the
classified failure (a 403 -> Auth, a `ThrottlingException` 429 ->
RateLimited with the `Retry-After` reset preserved). A 403 free-text body
is scrubbed to the IAM action only -- the principal ARN, account id, and
resource ARN never reach the client body or the logs.

Production credential guidance matches the Anthropic lane: use a SigV4
source (`static` with long-term keys, `profile`, or `default-chain`) or a
long-term Bedrock API key (`bearer-key`). Short-term credentials (a
console `bearer-key` or a `static` entry with `session_token_ref`) expire
and are DEV-ONLY -- the lane does not refresh them.

Both lanes require the `bedrock` build feature; the `bedrock_mantle` key
on an OpenAI provider fails config load with a clean feature-gated-field
error when the binary was built without it.

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
  `oauth://`) resolving to a Google AI Studio API key. The
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
  canonical schema does not carry natively -- notably `safetySettings`.
  The merge is top-level and shallow, so `generationConfig` (which
  routectl assembles itself) is dropped with a WARN rather than merged,
  and no field inside it can be set this way. Merged per the
  [payload_extras merge](#payload_extras-merge) rules.
- `user_agent` (optional) -- override the outbound `User-Agent`.
- `auth_mode` (optional, default `"api-key"`) -- selects how the
  provider authenticates:
  - `"api-key"` (default) -- the API-key path described above:
    `api_key_ref` resolves to a Google AI Studio key, sent as
    `x-goog-api-key` against the `generativelanguage` base.
  - `"cloud-code"` -- the Cloud Code ("antigravity") OAuth-bearer path:
    `api_key_ref` MUST be an `oauth://` ref, the resolved bearer is sent
    as `Authorization: Bearer` (NOT `x-goog-api-key`), and the base
    defaults to the `cloudcode-pa` `/v1internal:*` endpoint. The Cloud
    Code project id is auto-resolved on first use and cached in the
    credential record. See the cloud-code stanza below and
    [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#cloud-code-antigravity-egress-mode-auth_mode--cloud-code).

Cloud Code (OAuth) stanza:

```toml
[providers.gemini-cloud-code]
kind        = "gemini"
auth_mode   = "cloud-code"
api_key_ref = "oauth://antigravity"
# base_url defaults to the cloudcode-pa endpoint in cloud-code mode;
# omit it. The Cloud Code project id is auto-resolved and persisted.

[models.gemini-flash]
provider = "gemini-cloud-code"
upstream = "gemini-2.5-flash"
```

Auth decision: by default Gemini auth is API-key only, via the
`x-goog-api-key` header. Setting `auth_mode = "cloud-code"` adds an
OAuth-bearer path against the Cloud Code ("antigravity") surface: it
requires a one-time `routectl login antigravity` (live Google consent in
a browser) and an `oauth://antigravity` `api_key_ref`. In that mode the
base defaults to the `cloudcode-pa` endpoint and the Cloud Code project
id is auto-resolved (via loadCodeAssist, falling back to onboardUser) and
cached in the credential record. Vertex AI / Google service-account ADC
is still NOT implemented; it is reachable later by pointing `base_url` at
a Vertex endpoint without a new provider kind.

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
| `stream_first_byte_timeout_ms` | Option<u64> | None            | Per-provider first-content timeout for streaming responses (content-free leading chunks do not satisfy it). Resolution order: per-model > per-provider > global `[retry] stream_first_byte_timeout_ms`. |

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
response: fallback is decided per failure CLASS, and the baked class
matrix (see below) marks every class fallbackable except `unknown`.
This is the safest default for Cloudflare-fronted upstreams
(opencode.ai, openrouter.ai, etc.), which surface upstream-origin
failures through extended 5xx codes (520-527, 530). Operators with
bespoke upstream behavior narrow or widen a specific class through
`[retry.classes.<class>]` (a global per-class overlay) or a
`[providers.X.class_overrides]` per-provider status remap -- the
raw-status `retry_allowlist` / `retry_denylist` escape hatch was
retired in config schema v3.

```toml
[retry]
max_attempts                  = 2
initial_backoff_ms            = 250
backoff_multiplier            = 2.0
jitter_ms                     = 50
request_timeout_ms            = 300000        # 5 min per attempt
stream_first_byte_timeout_ms  = 90000         # 90s -- thinking models stall
probe_max_tokens              = 1             # fast-fail availability probes
# Per-error-class caps (each overrides max_attempts for that class only):
# retry_on_429                = 1             # rate-limits usually clear in one retry
# retry_on_network            = 2             # flaky DNS/TLS/connect
# To pin a class terminal (never fall back), set a per-class overlay:
# [retry.classes.bad-request]
# fallback = false
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

Resolution lives in `RetryPolicy::resolved_class`
(`crates/routectl-router/src/class_policy.rs`): each knob, when `Some`,
feeds the retry-same cap for its class family. A class pinned
non-fallbackable via `[retry.classes.<class>].fallback = false` is
terminal regardless of its `retry_on_*` cap. They ship commented in
[`examples/config.toml`](../examples/config.toml).

Each `retry_on_*` knob above is exactly the retry-same cap source the
wider per-class policy below consults for its class family -- see
`[retry.classes]`.

### Per-class retry and fallback policy (`[retry.classes]`)

95% of deployments need nothing here: the baked class matrix below IS
the policy. `[retry.classes]` and `[providers.X.class_overrides]`
(next section) are escape hatches over that matrix, not the primary
way to tune retry/fallback behavior -- reach for `retry_on_429` /
`retry_on_5xx` / `retry_on_network` above first.

Every upstream failure classifies into one of ten config-facing
classes (`crates/routectl-router/src/class_policy.rs`). Each class
resolves to a `(retry_cap, falls_back)` pair via
`RetryPolicy::resolved_class`, and separately either debits the
per-seat circuit breaker's health accounting or does not -- a fixed
set, independent of the retry/fallback decision:

| Class (kebab key) | Retry-same cap source | Falls back? | Debits breaker? |
|--------------------|------------------------|--------------|-------------------|
| `rate-limited` | `retry_on_429` (else `max_attempts`) | yes | yes |
| `server-error` | `retry_on_5xx` (else `max_attempts`) | yes | yes |
| `overloaded` | `retry_on_5xx` (else `max_attempts`) | yes | yes |
| `timeout` | `retry_on_network` (else `max_attempts`) | yes | yes |
| `network-error` | `retry_on_network` (else `max_attempts`) | yes | yes |
| `auth` | 0 (fixed) | yes | no |
| `bad-request` | 0 (fixed) | yes | no |
| `content-policy` | 0 (fixed) | yes | no |
| `context-window` | 0 (fixed) | yes | no |
| `feature-unsupported` | 0 (fixed, reserved -- see below) | yes | no |

**Breaker debit is not configurable.** The "debits breaker?" column is
a fixed set (`RateLimited`, `ServerError`, `Timeout`, `NetworkError`,
`Overloaded`) baked into the router's `class_debits` predicate
(`crates/routectl-router/src/router.rs`); no `[retry.classes]` leaf,
and no `[providers.X.class_overrides]` remap, changes whether a class
counts against a seat's health. Routing (retry cap, fallback) and
health accounting are deliberately separate concerns.

Every class above defaults to `fallback = true`.
`[retry.classes.<class>]` overrides one or both leaves, sparsely:

```toml
[retry.classes.server-error]
retry = 3    # same-provider retries above retry_on_5xx / max_attempts

[retry.classes.overloaded]
retry = 0    # stop retrying same-provider; fall over to the next chain entry immediately
```

A class with no `[retry.classes.<class>]` block keeps the baked row
above verbatim. Within a present block, an absent leaf (`retry` or
`fallback`) still inherits the baked default for that leaf alone --
`ClassPolicy`'s two leaves are independently optional, so setting
`retry` does not force an implicit `fallback` value.

Setting `[retry.classes.bad-request] fallback = false` is valid but
flagged by an advisory warning (`class_policy_warnings`, never fails
the load): the baked `bad-request` fallback is what walks a
capability-filter rejection to a capable target, so turning it off also
disables structured-output rescue -- a request needing a capability the
target lacks then hard-fails instead of falling over.

`feature-unsupported` is RESERVED: `[retry.classes.feature-unsupported]`,
even empty, is rejected at config load (`validate_class_policy`,
`crates/routectl-router/src/factory.rs`). The baked row above already
governs it; there is no override path for this class yet.

#### `[providers.X.class_overrides]` -- remapping a raw status to a class

Each `[providers.X]` table (flattened, not a `[providers.X.runtime]`
sub-table -- see "Per-provider runtime gates" above) accepts an
optional `class_overrides` map from a raw upstream HTTP status to one
of the ten kebab class keys:

```toml
[providers.bedrock]
kind   = "bedrock"
region = "us-west-2"
creds  = { kind = "default-chain" }

[providers.bedrock.class_overrides]
400 = "feature-unsupported"
```

The remap TARGET is restricted to four terminal, non-retrying classes
-- `bad-request`, `content-policy`, `context-window`,
`feature-unsupported` -- rejected at load otherwise
(`validate_class_policy`). A remap may only make behavior LESS
aggressive: move a status into one of those four, never into a class
the router retries or debits for health.

The example above relabels a Bedrock 400 as `feature-unsupported`
rather than the classifier's default `bad-request`. Both classes
resolve to the identical baked row (0 retry, falls back, no breaker
debit), so the remap changes nothing about routing -- a 400 was
already non-debiting. What it buys is capability-aware telemetry: the
relabeled failure carries the stable `operator-remap` capability token
on the `routectl::feature_unsupported` observability event instead of
surfacing as a generic bad-request, so capability gaps are
distinguishable from caller-shaped 400s for dashboards and any future
per-capability handling.

The remap SOURCE has no such restriction, but remapping a breaker
health-signal status (408, 429, or any `500..=599`) away from its
native class is flagged by an advisory warning
(`class_policy_warnings`, never fails the load) since it diverts a
real health signal away from the breaker -- an outage-masking risk if
the upstream is actually unhealthy.

#### Precedence

Class policy (`resolved_class`, including any `[retry.classes]` overlay
and any `class_overrides` remap) governs the retry/fallback decision
for every status. The probe fast-fail and the `UnknownProvider`
always-fallback short-circuits are the only checks ahead of it. (The
raw-status `retry_allowlist` / `retry_denylist` escape hatch that
formerly took precedence per code was retired in config schema v3 --
pin a class terminal with `[retry.classes.<class>].fallback = false`
or remap a specific status with `[providers.X.class_overrides]`
instead.)

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



## Proactive context-window gate (`[window_gate]`)

Before dispatch, routectl estimates the request's token size and skips
fallback-chain targets whose catalog context window clearly cannot hold
it, so an oversized request avoids a doomed round trip on a small-window
entry. This runs after the capability pre-filter, as the second chain
filter pass.

It is a best-effort filter, not a guarantee. A skip requires a
CONFIRMED catalog window and a non-final target, and the estimate is
deliberately margined, so several cases still dispatch: a target whose
window is unconfirmed, a chain of one, a chain whose every target
overflows, and any request the estimate undercounts. Each of those falls
through to the reactive path below.

The gate can never be the reason a request has nowhere to go: it has no
empty-chain error path. A chain of one is returned untouched before any
estimate is computed, a target whose catalog window is unconfirmed is
kept (an unknown fact never enables a skip), and a chain whose every
target overflows is returned unchanged so the caller sees exactly
today's upstream error rather than a routectl-invented one.

The optional `[window_gate]` block is the **global** kill switch. A
missing block keeps the default: the gate enabled.

```toml
[window_gate]
# Master switch for the proactive context-window gate. Default true.
enabled = true
```

- **`enabled`** (bool, default true) -- the master switch. When
  `false` the gate returns the resolved chain before it computes
  anything: no estimate, no chain reordering, no `window_gate_skip`
  WARN, and no movement in the skip counter. Turning it off is
  byte-identical to running with no gate at all, which is what makes it
  a safe first move when you suspect the gate is mis-routing.

**The gate is a fast path, not the safety net.** Disabling it leaves
reactive classification and fallback fully intact: an upstream that
rejects an oversized request is still classified and still falls over to
the next chain entry per the
[retry and fallback](#retry-and-fallback-defaults) table. With the gate
off you pay one doomed round trip for the rejection instead of skipping
it in advance; you do not lose the recovery.

Which class the rejection lands in depends on what the upstream sent.
The `context-window` class is assigned only when the error envelope
carries a recognized context-window token (for example the
OpenAI-compatible `context_length_exceeded`); a 4xx that carries no such
token is classified `bad-request`. Note the Anthropic-family token set
is empty today, so an Anthropic oversized-request rejection arrives as
`bad-request`. This matters if you have narrowed fallback per class:
retaining `context-window` fallback while disabling `bad-request`
fallback does not, on its own, guarantee an oversized request falls
over.

**One field deliberately.** The gate's safety margin against estimator
error (a request is skipped only once the estimate passes a fixed
fraction of the target's window) is a baked constant, not an operator
knob. A margin tuned per deployment turns a routing decision into a
support surface, and both error directions are already survivable: an
underestimate misses a skip and costs one round trip, while an
overestimate is a re-route among confirmed windows rather than a denial.
The switch above is the whole operator surface; if the gate misbehaves,
the answer is off, not retuned.

**Hot-reloadable.** `[window_gate]` is classified alongside the other
hot-reloadable sections
(`crates/routectl-cli/src/config_classify.rs`): the flag is read per
chain resolution, so a live config swap -- an editor save the daemon's
file watcher picks up, or a `routectl config set window_gate.enabled
false` -- applies to the next request with no restart and no
confirmation prompt.

An unknown key inside the block fails config load rather than being
silently ignored, so a typo is reported by `routectl config check`
instead of leaving the gate quietly at its default.

Whether the gate is acting is observable in the log: a throttled
`window_gate_skip` WARN (at most one line per minute per process) names
the first skipped target with the estimate, the figure the decision
actually used, the target's catalog window, and the running skip total.
Routectl-internal identifiers and catalog figures only -- never anything
from the request body.

## Learned token-estimate correction (`[calibration]`)

The gate above compares its estimate against a target's catalog window,
and that estimate is serialized bytes over four -- wrong per model, and
wrong *differently* per model. routectl measures that per-model error
from traffic it has already served: a request that completes
successfully and whose upstream reported a nonzero prompt total
contributes the estimate routectl used paired with that
cache-inclusive total, and those pairs reduce to one multiplicative
correction per lane. A lane is `(provider kind, served model nickname)`
-- the nickname you declared under `[models.X]`, never the upstream wire
id. A failed or partial request contributes nothing: there is no
trustworthy pairing between what was estimated and what the upstream
actually charged for.

The correction has exactly ONE consumer: the proactive context-window
gate's comparison. It never changes the bytes sent upstream, never
changes the estimate figure persisted to the usage ledger, and never
feeds the advisory trim or the reduction path. Above 1.0 it means the
estimator under-counts this lane, so the corrected figure is larger and
the gate is more willing to skip a small-window target; below 1.0 it
means the estimator over-counts, so the gate is more willing to admit a
target it would otherwise have skipped.

The optional `[calibration]` block is the **global** kill switch. A
missing block keeps the default: the correction enabled.

```toml
[calibration]
# Master switch for applying the learned per-lane correction. Default true.
enabled = true
```

- **`enabled`** (bool, default true) -- the master switch. When `false`
  the context-window gate compares the raw, uncorrected estimate exactly
  as it did before the correction existed: no lane is looked up and no
  correction is applied.

**Default on is safe because a cold lane corrects nothing.** A lane
produces no correction until it has accumulated real evidence, so a
fresh install behaves exactly as it would with the switch off. Every
refusal collapses to the same uncorrected path: an unseen lane, a lane
with too few recent samples, one whose samples are all too old, one
whose distinct callers are too few, and one whose reduced ratio landed
outside the sane band. That last case is **refused, not clamped** -- a
ratio that extreme is evidence a lane is mis-keyed or fed garbage, not
evidence of a genuinely extreme correction, and clamping it to the bound
would let a mis-keyed lane still move a routing decision.

**Off still collects.** Turning the switch off stops the correction from
being APPLIED and nothing else. The gate keeps gating on the uncorrected
estimate (the switch does not disable the gate -- that is
[`[window_gate]`](#proactive-context-window-gate-window_gate)), routectl
keeps recording evidence into the per-lane store and keeps persisting
the estimate/actual pairs to the usage ledger, and the evidence already
collected is retained. Switching back on therefore applies on the next
request rather than after a re-learn period. The reverse pairing matters
too: with `[window_gate] enabled = false` the correction is never
consulted at all, because the gate returns the chain before it computes
any estimate -- so turning the gate off already turns the correction off.

**One field deliberately.** The reduction's sample floors (a minimum
count of recent samples and a minimum count of distinct callers before a
lane may produce a correction), the age bound past which a sample stops
counting, and the sane band a reduced ratio must land inside are all
**baked constants, not operator knobs**. A band tuned per deployment
turns a routing decision into a support surface, and the fallback in
every direction is the uncorrected estimate rather than a denial. The
switch above is the whole operator surface; if a lane's correction
misbehaves, the answer is off, not retuned.

Two properties of the reduction are worth knowing even though neither is
configurable. It groups samples by caller and takes a median of the
per-caller medians, so one long-running conversation cannot define a
lane's correction by its volume alone -- and because every request
arriving without a session key shares a single caller group, a lane fed
only keyless traffic never clears the distinct-caller floor and stays
uncorrected. And the evidence survives a restart: the daemon warms the
lane store from the usage ledger at startup (bounded to samples young
enough to still count), while a hot reload carries the live in-memory
store across instead of re-reading history.

**Hot-reloadable.** `[calibration]` is classified alongside the other
hot-reloadable sections
(`crates/routectl-cli/src/config_classify.rs`): the flag is read per
chain resolution, so a live config swap -- an editor save the daemon's
file watcher picks up, or a `routectl config set calibration.enabled
false` -- applies to the next request with no restart and no
confirmation prompt. The lane store is preserved across that swap
(lanes whose nickname the new config no longer declares are dropped).

An unknown key inside the block fails config load rather than being
silently ignored, so a typo is reported by `routectl config check`
instead of leaving the correction quietly at its default.

Whether a correction is moving a decision is observable in the gate's
own throttled `window_gate_skip` WARN, which reports both the raw
estimate and the corrected figure the decision actually used. They are
equal on an uncorrected lane, and their divergence is the only way to
see a learned correction move a skip. The startup warm logs its own
tally: rows loaded, rows accepted, rows rejected, and how many lanes
came back calibrated.

## Quota-aware seat placement (`[seat_quota]`)

On a multi-seat OAuth pool running `sticky-least-loaded`, a NEW
conversation's birth seat is ordered by each credential account's
remaining short-window subscription budget, read from the quota headers
upstreams already return on every response. This is what makes a pool of
two subscription accounts actually spread new conversations instead of
tying on available RPM (which is unlimited on a subscription seat) and
falling through to the anti-herd rotation.

`sticky-least-loaded` is the only strategy this applies to. A pool left on
the `fill-first` default, or set to `round-robin`, places exactly as it
always has -- neither reads a seat's budget, and the switch below changes
nothing for them. `fill-first` in particular is unchanged *deliberately*:
draining one seat is what that setting asks for. Adopting quota-aware
placement is therefore an explicit
`seat_selection = "sticky-least-loaded"` on the pool.

Nothing here can move an established conversation. Placement is
consulted for a birth pick only, so a soft cap never evicts or migrates a
session off the seat holding its warm prompt cache -- a pinned session
over its budget runs to actual exhaustion and is rescued by the reactive
path (an upstream refusal trips the per-seat breaker and the seat drops
out of the dispatch filter), exactly as it is today.

It is best-effort and cap-dormant by construction. An untrustworthy
reading never NARROWS a placement -- a seat routectl has no fresh reading
for is never preferred on quota grounds, and never counted as having
budget it did not report. When the evidence is too thin to act on,
placement falls back to the pre-quota capacity ranking and an unobserved
seat can still be picked there, which is what makes these cases resolve
exactly as they did before quota placement existed: a fresh process
before any response has been observed, a seat whose reading has lapsed
past its own window, a provider routectl curates no short recovering
window for (the Codex subscription egress is one -- it reports a
seven-day window and no short one), and a pool where some seats are
observed and the rest are not.

Concretely, among the seats the existing health and rate-limit filters
already admit:

- any seat with a fresh reading below its budget threshold -> the pick is
  restricted to those, taking the one with the most left;
- every admitted seat observed and every one over its threshold -> the
  pick takes the one with the most left, and the request is **never**
  failed over a budget;
- anything else -> the unchanged capacity ranking decides.

Ties inside the chosen group still fall to the existing deterministic
anti-herd rotation, so a burst of new conversations spreads rather than
herding onto the emptiest seat.

The optional `[seat_quota]` block is the **global** kill switch. A
missing block keeps the default: placement enabled.

```toml
[seat_quota]
# Master switch for quota-aware seat placement. Default true.
enabled = true
```

- **`enabled`** (bool, default true) -- the master switch. When `false`,
  an unpinned conversation's birth seat resolves exactly as it did before
  quota placement existed: no quota state is read, no budget orders the
  pick, no quota placement log line is emitted, and the health preference,
  the rate-limit eligibility filter, the RPM ranking and the anti-herd
  rotation all decide as before. Conversation pins are preserved and a
  one-time migration off an unhealthy seat still happens -- the switch
  gates WHICH seat a birth picks, never whether conversations pin.

**Off still observes.** Turning placement off stops readings from being
APPLIED and nothing else: routectl keeps collecting and aging them, so
turning it back on takes effect on the next birth pick rather than after
a re-observe period. This follows the same shape as the learned-estimate
correction's switch.

**One field deliberately.** The per-provider budget thresholds, the
long-window guard and the freshness bounds are constants grounded in
captured upstream evidence, not operator knobs: a threshold tuned per
deployment turns a routing decision into a support surface, and the
fallback in every direction is the pre-quota chooser rather than a denial.
The switch above is the whole operator surface.

**Hot-reloadable.** `[seat_quota]` is classified alongside the other
hot-reloadable sections
(`crates/routectl-cli/src/config_classify.rs`): the flag is read per
birth pick, so a live config swap -- an editor save the daemon's file
watcher picks up, or a `routectl config set seat_quota.enabled false` --
applies to the next new conversation with no restart and no confirmation
prompt.

An unknown key inside the block fails config load rather than being
silently ignored, so a typo is reported by `routectl config check`
instead of leaving placement quietly at its default.

Whether placement is acting is observable in the log: a throttled line
(at most one per five minutes per process) names the model and which arm
decided, with the running per-arm totals. A DEBUG line when a budget
chose the seat, a WARN when the evidence was incomplete and the
unchanged capacity ranking decided instead -- a steady stream of the
latter on a busy pool means readings are not reaching routectl for some
seat. Routectl-internal identifiers and counters only: no session key, no
account identity, no credential, no header, nothing from a body.

The per-request seat-selection decision vocabulary
(`birth_pick` / `sticky_stay` / ... , see
[credential pool](#credential-pool-multiple-seats-per-provider)) is
UNCHANGED by this feature: quota changes which seat a birth picks, never
how that decision is named.

## Per-model knobs

### Reasoning capability declaration

Three fields on `[models.X]` declare what reasoning a model supports.
The router and egresses read them at dispatch time; callers never set
these fields directly.

#### `supports_adaptive_thinking` (bool, default false)

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

#### `effort_levels` (array of strings, default ["low","medium","high"])

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

#### `max_thinking_budget` (u32, default 0)

Declares the model's maximum thinking-token budget in tokens. `0` means
"not a budget-capped model" -- the egress falls back to its own
inference-time defaults. Non-zero values are forwarded as the ceiling
for the egress's budget negotiation. Only relevant on the legacy
`supports_adaptive_thinking = false` path; the adaptive path uses effort
strings and has no budget field.

#### `max_output_tokens` (Option<u32>, default None)

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
`openai-responses`, `bedrock-converse`) forward a caller-supplied
`max_tokens` under their own wire name (`openai-responses` emits it as
`max_output_tokens`) and forward caller omission cleanly without
injection (good-translator principle: do not inject where the upstream
already handles it). Exception: the `openai-responses` codex OAuth lane
(`chatgpt.com/backend-api/codex`) omits `max_output_tokens` entirely --
codex's request contract has no such field and the backend rejects the
drift.

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

#### `reported_model` (Option<String>, default None) -- response `model` label

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

#### `visible_routectl_provider` (bool, default true) -- response `routectl_provider` field

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

## credential_source (anthropic-api provider flag) -- forwarded credential

`credential_source` on `[providers.X]` (kind = "anthropic-api") picks which
credential that provider's Anthropic egress authenticates with. Default
`"own"` -- the provider authenticates with `api_key_ref`/`auth_kind` exactly
as every provider always has; this default is byte-for-byte unchanged
behavior.

Set `credential_source = "forwarded"` to make the provider a pure passthrough
for the client's own captured claude.ai bearer instead of a routectl-managed
credential:

```toml
[providers.anthropic-forwarded]
kind              = "anthropic-api"
base_url          = "https://api.anthropic.com"
credential_source = "forwarded"
```

- Omit `api_key_ref` entirely -- a forwarded provider has no configured
  credential of its own. Validation REJECTS a forwarded entry that
  carries a non-empty `api_key_ref` (the two are mutually exclusive: a
  forwarded provider's credential comes from the client, never from config).
- `base_url`'s host must be exactly `api.anthropic.com` (case-insensitive;
  a path, port, or `user:pass@` prefix on the URL is ignored by the
  check and does not smuggle in a different host). Validation REJECTS
  `credential_source = "forwarded"` on any other host -- a hard
  containment guarantee: the forwarded credential carries the client's
  full-scope claude.ai bearer, which must never be sent to a non-Anthropic
  egress.
- On this leg the dispatched request keeps the client's requested
  model verbatim instead of being rewritten to the target's configured
  upstream model, and `GET /v1/models` proxies through to Anthropic's
  real model list rather than returning routectl's local alias list
  (falling back to that local list on any other request, or a
  proxy-side failure). See
  [REMOTE-CONTROL.md](REMOTE-CONTROL.md#pure-proxy-mode) for the
  full admission and failure-handling model.

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
  directions. Directions 1 (ingress), 2 (outgoing), and 3 (upstream
  response) redact secret-bearing header values before emission;
  direction 4 (egress response) stays raw. Default off. See
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

## MITM front-proxy (`[mitm]`)

The optional `[mitm]` block gates on a local TLS-terminating listener
that fronts a first-party upstream (e.g. Claude Code talking to
`api.anthropic.com`), the same presence-gates-the-feature convention as
`[server.auth]`. A missing block keeps every default below AND zero
proxy startup -- routectl's behavior is unchanged until an operator
declares `[mitm]`.

```toml
[mitm]
upstream_origin   = "https://api.anthropic.com"
listen_port       = 8443
cert_dir          = "/home/you/.config/routectl/mitm-certs"
mitm_host         = "api.anthropic.com"
tested_cc_version = "2.1.143"
```

| Knob                | Default                                | Reload  |
|---------------------|-----------------------------------------|---------|
| `upstream_origin`   | `https://api.anthropic.com`             | restart |
| `listen_port`       | `8443`                                  | restart |
| `cert_dir`          | `<config-dir>/routectl/mitm-certs`      | restart |
| `mitm_host`         | `api.anthropic.com`                     | restart |
| `tested_cc_version` | unset                                   | restart |

- `upstream_origin` -- the first-party origin the proxy forwards
  decrypted requests to. Must be EXACTLY `https://api.anthropic.com`
  (no userinfo, path, query, fragment, or explicit non-default port,
  and no other host); startup validation rejects anything else. This
  pins the MITM egress to first-party Anthropic so the client's
  full-scope claude.ai token can never be forwarded to a non-Anthropic
  origin.
- `listen_port` -- the local TCP port the MITM listener binds. Must
  differ from `[server] port` -- the two are separate bound sockets on
  the same host; startup validation rejects a collision.
- `cert_dir` -- directory holding the locally-generated MITM CA + leaf
  certificates. Defaults under the same user config dir as `usage.db`
  and `config.toml` (`XDG_CONFIG_HOME` else `$HOME/.config`).
- `mitm_host` -- the TLS SNI / `Host` header the proxy expects from the
  client and presents to the upstream. Must be EXACTLY
  `api.anthropic.com` (a subdomain like `evil.api.anthropic.com` is
  rejected, not matched as a suffix); startup validation rejects any
  other value.
- `tested_cc_version` -- the Claude Code version this `[mitm]` config
  was last verified against. Consulted by the proxy at runtime: on a
  decrypted request whose `User-Agent` reports a different Claude Code
  version, the proxy logs a WARNING once per distinct observed version
  but never refuses the request. Unset (the default) disables the
  check entirely. The whole `[mitm]` block, including this field, is
  read once at startup -- editing it takes effect only on the next
  restart, since there is no reload path that respawns the proxy
  listener.

`[mitm]` is transport-only: it carries no credential knob. Which
credential a forwarded egress uses is a per-provider choice -- see
[credential_source (anthropic-api provider flag)](#credential_source-anthropic-api-provider-flag----forwarded-credential)
above.

**Migrating an old config.** A `[mitm]` block that still carries
`credential_source = "forwarded"` (or `"own"`) is REJECTED at startup
with an actionable error naming the exact replacement. Delete the key
and add a provider block instead:

```toml
[providers.anthropic-forwarded]
kind              = "anthropic-api"
base_url          = "https://api.anthropic.com"
credential_source = "forwarded"
```

No `api_key_ref` line -- a forwarded provider has no configured
credential of its own.

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

When you pass `--hypothetical-d` (or `--steady-state`), the report gains
a fourth section: an ADVISORY projection of whether breaking a warm
prompt-cache to apply a proposed prefix cut would be net-positive. It is
offline-only and never mutates a request, resolves a secret, or touches
the network -- it is advice, computed from the baked
per-`(provider_kind, model, tier)` cache pricing table.

```sh
# Just the break-even threshold K* for a 50k-token cut, 5m tier:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000

# Price the REAL steady-state trim candidate instead of a hypothetical cut:
routectl prompt-size --alias heavy --request ./fixture.json --steady-state

# Plus a keep/break verdict at an assumed reuse count, 1h tier:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000 --hypothetical-k 60 --ttl-tier 1h
```

- `--hypothetical-d <TOKENS>` -- the size of the proposed cache-prefix
  cut. Supplying this flag is what turns ON the projection; omit it and
  the report is byte-for-byte the three-section output above.
- `--steady-state` -- price the REAL deterministic steady-state trim
  candidate routectl's advisory trimmer would propose for this request
  (front-anchored old-tool-content elision), instead of a hypothetical
  cut. Turns ON the projection section and leads with the trimmer's
  would-trim yes/no decision before the priced candidate. Mutually
  exclusive with `--hypothetical-d`.
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

## Catalog: prompt-cache economics (`routectl catalog`)

routectl ships a two-layer catalog of prompt-cache *economics*
multipliers -- the write multiplier (`wm`), read multiplier (`rm`), TTL,
minimum cacheable prefix, and a small set of capability flags -- per
`(provider_kind, model)`. These feed the cache-break break-even
reasoning (see `prompt-size --hypothetical-d` above); they are distinct
from the `[registry]` dollar prices above, which feed usage-cost
estimation.

**Layer 1: the baked table.** Every routectl build carries a table
derived from vendor-published pricing snapshots, re-verified on each
release. You do not normally need to touch it.

**Layer 2: the overlay.** `catalog_overlay.json` -- next to
`config.toml` and `credentials.json`, under
`$XDG_CONFIG_HOME/routectl/` (falling back to `~/.config/routectl/`) --
holds per-selector overrides on top of the baked table. An overlay cell
wins over the baked row field by field: a field it sets wins, a field it
leaves unset inherits the baked value. A JSON `null` value for a
selector key DISABLES that row outright, overriding even the baked
table's own default; a selector absent from the overlay falls through
to the baked row unchanged. A running `routectl serve` watches the
overlay file and picks up a write automatically -- no restart needed.

### Retired: `[cache_pricing]`

Config schema v1's `[cache_pricing]` TOML table (and its
`pricing_verifications.json` sidecar) is retired: at the current schema
version a non-empty `[cache_pricing]` table is a hard startup error, not
silently-ignored data.

`routectl config migrate` is what folds it forward -- the loader NEVER
migrates on load. The migrator plans the whole transform in memory first,
then commits in two phases: every `[cache_pricing]` entry (already merged
with any sidecar verification) is written into `catalog_overlay.json` as a
`source: import` cell, and only then is `config.toml` rewritten
format-preservingly with the new `version` stamped and `[cache_pricing]`
dropped. Config last, deliberately: a crash between the phases leaves the
file still reporting the old version, so a rerun re-plans from scratch.

The migration is idempotent (a cell whose value already matches what a
prior run wrote is a silent no-op) and fails closed on any conflict
between a migrated entry and a DIFFERENT pre-existing overlay value --
nothing is written for ANY key until the conflict is resolved by hand.
`pricing_verifications.json` is read only as part of this migration;
nothing writes to it anymore.

### `routectl catalog list`

Prints the EFFECTIVE catalog (the baked table merged with the overlay)
as an aligned ASCII table, headed by a summary line (overlay revision,
cell counts by source, disabled count). Columns: `provider_kind`,
`model_glob`, `status`, `tier`, `wm`, `rm`, `ttl(s)`, `min_prefix`,
`auto`, `max_ctx`, `source`, `verified_at`, `stale`. Every row renders
`PRESENT` (with its provenance and a staleness marker) or `DISABLED`
(overlay `null`); a trailing punch-list names any selector with an
unknown `max_context_tokens` (its context-fraction advisory falls back
to absolute tokens for those).

### `routectl catalog verify <selector>`

Stamps an EXISTING overlay cell's `verified_at` to today (UTC) and flips
its `source` to `user` -- verifying is a user act, even for a cell an
import last wrote. Every other field on the cell carries through
unchanged. `selector` is `"provider_kind:model_glob"` (e.g.
`openai-compat:grok-*`). A selector with no overlay cell yet (baked-only,
or unknown to the catalog), or one that is explicitly disabled, has
nothing to stamp and is rejected -- creating a new overlay cell is a
`set` concern.

### `routectl catalog import`

Opt-in bulk refresh of the overlay from the two vendored economics
sources (litellm + models.dev): fetches both over the network (or reads
both from disk via the file flags below), derives one candidate, prints
an impact-labeled diff, and -- on confirmation -- applies it.

```sh
routectl catalog import
routectl catalog import --litellm-file ./litellm.json \
  --models-dev-file ./models_dev.json --yes
```

- `--litellm-file <PATH>` / `--models-dev-file <PATH>` -- read a source
  from disk instead of the network. Must be given together (both, or
  neither).
- `--yes` -- skip the y/N confirmation prompt (scripting).
- `--allow-shrink` -- bypass ONLY the shrink guard's per-source /
  per-family floors (a candidate whose row counts dropped too far
  relative to the last successful import). Never bypasses a fetch
  failure, a cross-check disagreement, a `source: user` conflict, or a
  revision conflict.

The printed diff sorts every candidate selector into exactly one bucket:
`applied` (a fresh selector, or one whose existing overlay cell is
itself `source: import`), `skipped` (a per-selector cross-check
disagreement between the two sources, or a selector unknown to the
baked table), or `conflicted` (an existing `source: user` cell, or an
explicit disable). USER-WINS: a conflicted row's existing value is
always preserved untouched, regardless of what the candidate proposed.
Each `applied` / `conflicted` row is labeled `display-only`,
`cost-affecting`, or `routing-affecting`, and flags whether the change
trends toward a lower cache-break break-even reuse count.

The apply is TRANSACTIONAL: a fetch failure on either source aborts
before a candidate is even built, leaving the overlay byte-identical.
Once a candidate is built and confirmed, only the `applied` rows are
written, through the same revision-checked, advisory-locked writer every
overlay mutation goes through -- a write from a concurrent `catalog`
invocation is detected and recomputes one bounded fresh diff rather than
silently overwriting or losing an update.

### `routectl catalog set` / `routectl catalog disable`

`set <selector> <field>=<value>...` writes a `source: user` cell for a
KNOWN selector (an existing baked row, or an existing overlay cell of
either provenance), field by field; a field left unnamed inherits the
prior cell's value. `disable <selector>` writes a JSON-null cell for a
KNOWN selector, discarding whatever it previously carried. Both REJECT a
selector unknown to the catalog (no baked row, no existing overlay
cell) -- creating a brand-new selector is not supported by either verb.

Supported `set` fields: `wm` (f32), `rm` (f32), `ttl_seconds` (u32),
`min_prefix_tokens` (u32), `max_context_tokens` (u32), and a capability
flag via `cap:<name>=true|false`. `auto_cacher` / `has_storage_rent` /
`storage_rent` are hard-rejected -- they live only on the baked table;
`verified_at` is hard-rejected too, since `set` and `verify` alike
always stamp it automatically to today (UTC).

A `wm` set below the conservative sentinel (`2.0`) is rejected unless
the call also carries `--acknowledge-cost-risk`: a too-cheap write
multiplier can make a cache break look falsely profitable. `rm` must be
`> 0`; `max_context_tokens` must not be `0`.

```sh
routectl catalog set openai-compat:my-cheap-host-* wm=1.0 --acknowledge-cost-risk
routectl catalog disable openai-compat:retired-model-*
```

### `routectl catalog export`

Serialize the on-disk overlay (`catalog_overlay.json`) to pretty JSON,
printed to stdout or written to a file with `--out <path>`. It is
read-only: the overlay file is left byte-identical.

```sh
routectl catalog export
routectl catalog export --out ./catalog_overlay.backup.json
```

The export is CATALOG CELLS ONLY. It does NOT back up credentials --
provider keys, OAuth tokens, and every other secret live in separate
files (`config.toml`, the OAuth credentials store) that this command
never reads, so a leaked export can never disclose one.

There is no separate overlay-import format to pair with it: to restore an
export, place the JSON back at the overlay path (`catalog_overlay.json`
next to `config.toml`), where the next load picks it up. `catalog import`
consumes the VENDOR economics snapshots (litellm + models.dev), not an
overlay dump.

**`pricing` alias.** `routectl pricing ...` is a hidden alias for
`routectl catalog ...`, kept for muscle memory; it is dropped at 1.0.

## Inspecting the effective config (`config show --effective`)

`routectl config show` dumps `config.toml` with secrets redacted: inline
`literal:` key references become sentinels, and every provider
`base_url` is reduced to its ORIGIN (`scheme://host:port`), dropping any
path, query, or embedded `user:pass@` credential. That reduction means
the output is NOT round-trippable back into a config file, which the
dump says in a leading `#` comment -- your own `config.toml` remains the
authoritative source for verbatim `base_url` values.
Adding `--effective` appends a provenance-annotated view of the two
surfaces where MORE THAN ONE LAYER can supply a value -- so you can see
which layer actually won without cross-referencing the baked table, the
overlay, and the retry defaults by hand. Every other config field is
trivially whatever `config.toml` says, so it is not re-annotated (that
would be noise).

The derivation is pure: it runs the SAME `(provider_kind, upstream)`
catalog lookups the router runs at chain-build, over the loaded config
and `catalog_overlay.json`. It resolves no secrets, builds no provider,
and makes no network call, so it is safe to run against any config.

Two sections are printed:

- **model catalog cells** -- one row per `[models.X]` entry, showing its
  `provider_kind/upstream` selector and the merged catalog cell. The
  `source=` tag reads the same as `catalog list`:
  - `baked` -- the value came from the compiled-in baked table (no
    overlay cell for this selector);
  - `import` -- an overlay cell written by `catalog import` (or migrated
    from a legacy `[cache_pricing]` entry);
  - `user` -- an overlay cell an operator wrote (`catalog set` /
    `catalog verify`);
  - `disabled` -- the overlay explicitly disabled this selector
    (overlay `null`); pricing falls back to the conservative sentinel;
  - `missing` -- neither layer has a row (an unpriced upstream).
- **retry class policy** -- one row per failure class, showing the
  resolved `retry`/`fallback` pair and a `source=` tag:
  - `config` -- a `[retry.classes.<class>]` leaf set this class;
  - `baked-default` -- no operator leaf, so the baked class default
    applies (see "Per-class retry and fallback policy").

```sh
routectl config show --effective
```

## Editing config from the CLI (`config set` / `config unset`)

`routectl config set <path> <value>` edits one scalar leaf of
`config.toml` in place, addressed by its dotted path:

```sh
routectl config set server.port 8788
routectl config set retry.classes.server-error.retry 4
routectl config set usage.enabled true
```

The value's scalar type is inferred: `true`/`false` become a boolean,
an integer parses to an int, a decimal to a float, and anything else is
a string. A mistyped value is caught by the validation gate below, not
by inference.

Every edit runs the same pipeline before a single byte reaches disk, so
a rejected edit leaves the file byte-identical:

- **Version preflight.** A config older than the schema version this
  binary writes is refused outright (run `config migrate`, or use a
  matching-version binary) -- `config set` never migrates a
  file as a side effect of editing it.
- **Path validation.** The dotted path is checked against the config
  schema before any mutation. An unknown key is rejected with the valid
  sibling keys named at that level; a path into an array is rejected
  (array values are hand-edit-only in this version); a path that names a
  whole table rather than a scalar leaf is rejected.
- **Full re-validation.** The edited document is re-parsed and run
  through the SAME validator suite `config check` and `serve` use. Any
  error (a bad type, an alias pointing at an unknown model, an
  incoherent cross-field combination) is rendered and the write is
  abandoned.
- **High-consequence confirmation.** An egress-defining change -- a
  provider `base_url` or `credential_source`, a `[mitm]` origin or host
  -- prompts for confirmation first. `--yes` bypasses the prompt for
  scripting.
- **Atomic, format-preserving write.** The write goes through a
  sibling advisory lock and a base-bytes revision check (an out-of-band
  edit between read and commit is reported as a conflict, with nothing
  written), then an atomic rename. Comments and key ordering survive.

A running daemon's file watcher picks up the rename and hot-reloads
automatically. Fields that only apply at startup (`[server]` bind and
listener auth, the `[log]` knobs, `usage.db_path`,
`usage.retention_days`, and the whole `[mitm]` block) are still written,
and `config set` prints exactly which ones need a restart to take
effect. A no-op set (the value already matches) writes nothing and
prints no restart notice.

### Removing an override (`config unset`)

`routectl config unset <path>` removes one override so the value falls
back to whatever it inherits -- a shared knob, or the baked catalog
default -- rather than assigning a new one:

```sh
routectl config unset retry.classes.server-error.retry
routectl config unset retry.classes.server-error
```

It runs the identical pipeline as `config set` (version preflight, path
validation, full re-validation, high-consequence confirmation, atomic
format-preserving write, restart-required reporting, one audit event),
differing only in the mutation and in two path rules:

- **Recursive empty-parent prune.** Removing a key that leaves its
  parent table empty removes that table too, and so on up the chain --
  an empty override table is treated as absent (a leftover `[retry.classes.server-error]`
  with nothing under it would be ambiguous). A parent that keeps any
  sibling key or sub-table is left in place. In the first example above,
  removing the last leaf under `[retry.classes.server-error]` drops that
  table; if it was the only class override, `[retry.classes]` and an
  otherwise-empty `[retry]` go with it.
- **A whole table may be the target.** Unlike `set`, `unset` accepts a
  path that names a table node (the second example), dropping the entire
  override block in one call.

Removing a key that is not set writes nothing (there is no override to
remove) and reports no change. Comments and key ordering elsewhere in
the file survive, exactly as with `set`.

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
block keeps the default: auto-emit enabled, and the break-even emission
gate off.

```toml
[cache]
# Master switch for dispatch-path auto-emission of a top-level
# cache_control breakpoint. Default true.
auto_emit_top_level_breakpoint = true
# Stabilize the tool-array order on the non-Claude-Code OAuth Anthropic
# path so a random client tool order can still hit the prompt cache.
# Default true.
normalize_tools = true
# Withhold auto-emitted markers on a session whose measured per-turn
# reuse sits below the marker's break-even point. Default FALSE -- see
# "Break-even emission gate" below before turning it on.
k_gated_emission = false
```

Auto-emit applies to completions and streaming. It is **not** applied to
`count_tokens` (`/v1/messages/count_tokens`), which is a probe and never
writes a cache entry.

### Per-provider switch (`auto_emit_top_level_breakpoint`)

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
api_key_ref = "env://SOME_ANTHROPIC_API_KEY"
# Opt this provider out of auto-cache while leaving the global default on.
auto_emit_top_level_breakpoint = false
```

### Per-provider capability (`cache_capability`)

routectl only auto-emits a breakpoint to a provider it knows honors one.
Each provider kind has a **conservative** default capability:

| `kind`              | `supports_top_level_cache_control` | `cache_hit_observable` |
|---------------------|------------------------------------|------------------------|
| `anthropic-api`     | true (default base URL only -- see below) | true            |
| `bedrock`           | true on `api_shape = "invoke"`, false on `"converse"` -- see below | true  |
| `openai-responses`  | false (server-side auto-cache; no explicit breakpoint) | true |
| `openai-compat`     | false                              | false                  |
| any unknown kind    | false                              | false                  |

When `supports_top_level_cache_control` is false, auto-emit is skipped
for that provider regardless of the switches above. An operator can
override the default per entry with an explicit `cache_capability`:

```toml
# An anthropic-compatible third-party host that DOES honor a top-level
# breakpoint and DOES report cache hits.
[providers.compat-anthropic]
kind = "anthropic-api"
api_key_ref = "env://COMPAT_ANTHROPIC_API_KEY"
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

**bedrock defaults split by `api_shape`.** Bedrock honors prompt caching
only via per-block markers -- a `cachePoint` block on Converse, a
per-block `cache_control` on Invoke -- never a routectl-injected
top-level marker on the wire. The two shapes therefore default
differently:

- `api_shape = "invoke"` (the default) gets `true/true`, so auto-emit
  runs. The bedrock-invoke egress lowers a routectl-injected top-level
  `cache_control` onto the last cache-eligible content block -- the
  per-block form Invoke honors -- and re-validates the resulting
  breakpoint sequence before the body ships. When no eligible block
  exists, or the lowered arrangement would violate the breakpoint
  invariants, the top-level marker is dropped and the request goes out
  uncached (logged at WARN) instead of 400ing.
- `api_shape = "converse"` gets `false/true`: a top-level marker lands
  in `additionalModelRequestFields` and never becomes a `cachePoint`, so
  auto-emit is skipped (`auto_skipped:no_capability`) rather than
  silently no-op'd, while hit usage is still reported back
  (`cache_hit_observable = true`).

Caller-supplied per-block markers are unaffected on either shape and
still cache normally; an operator may override per entry.

### Per-block front marker (`auto_emit_per_block_breakpoints`)

Each `[providers.X]` entry carries an optional
`auto_emit_per_block_breakpoints` (bool). It gates dispatch-path
auto-emission of a **per-block** cache breakpoint at the FRONT of the
cacheable prefix -- the Anthropic per-block `cache_control` marker and the
Bedrock Converse `cachePoint` block. It does **not** govern the terminal
top-level marker; that stays entirely under
`auto_emit_top_level_breakpoint` and the global `[cache]` switch. The two
knobs are independent, so turning per-block emission off never disturbs
the terminal marker an existing deployment already gets.

When the key is omitted, the kind-level default applies:

| Provider entry                            | Default |
|-------------------------------------------|---------|
| `anthropic-api` on the default base URL   | true    |
| `anthropic-api` on any other base URL     | false   |
| `bedrock`, `api_shape = "converse"`       | false   |
| `bedrock`, `api_shape = "invoke"`         | false (key is inert -- see below) |
| every other kind                          | false (key is inert -- see below) |

Only default-base `anthropic-api` defaults on: that is the population
whose terminal marker is already auto-emitted, so the front marker adds
no new upstream shape. Everywhere else per-block emission is opt-in, so
the feature fails toward current behavior.

**The knob is operator INTENT, not a capability override.** Emission also
requires that the target's egress can actually carry a per-block marker to
the wire, which only `anthropic-api` and `bedrock` with `api_shape =
"converse"` can. Setting the key to `true` on any other kind is accepted
and INERT: no marker is placed, and the recorded decision is
`auto_skipped:no_capability` rather than `auto_emitted`, so the decision
ledger never claims a marker that never shipped. This is deliberate
fail-closed behavior -- on `openai-compat` an emitted per-block marker
would be dropped with a WARN, and under `[server] strict_translation`
would fail the whole request with a 400.

```toml
# Opt a Bedrock Converse provider in to front-marker emission.
[providers.bedrock-converse]
kind = "bedrock"
region = "us-east-1"
api_shape = "converse"
creds = { kind = "default-chain" }
auto_emit_per_block_breakpoints = true

# Opt the real Anthropic surface back out without touching the terminal
# marker.
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
auto_emit_per_block_breakpoints = false
```

**Inert where the wire cannot carry the marker.** Setting the key (to
either value) on a `bedrock` entry with `api_shape = "invoke"`, or on any
`openai-compat` / `openai-responses` / `gemini` entry, is accepted -- the
config still loads -- but changes nothing, and the loader emits a startup
WARN naming the provider so the key is not mistaken for an active setting.
Invoke has no front-marker path (its egress lowers the *top-level* marker
to per-block form itself); the other kinds have no per-block breakpoint
surface at all.

**A cachePoint below the upstream's minimum prefix is a silent no-op.**
AWS (and Anthropic) only cache a prefix that clears a per-model minimum
token count; a marker emitted on a shorter prefix is accepted and simply
never produces a cache entry. routectl does not gate emission on an
estimated token count -- the estimate would be wrong often enough to
withhold markers that would have cached -- so a small-prefix request may
record `auto_emitted` while the upstream reports no cache write. Read the
decision alongside the reported cache-write tokens, not on its own.

The custom-base `anthropic-api` fail-closed posture above is unchanged:
that entry is gated by `cache_capability` for the terminal marker and by
this default of `false` for the front marker; both need an explicit
operator opt-in.

### Tool-array normalization (`normalize_tools`)

`normalize_tools` (bool, default true) sorts the outbound `tools` array
by each tool's final wire name on the non-Claude-Code OAuth Anthropic
path. Many clients emit their tool definitions in a nondeterministic
order from run to run; because the tool block sits inside the cacheable
prefix, a reshuffled order breaks the prompt-cache prefix even though the
tool set is identical. Sorting by wire name gives that prefix a stable
byte shape so it keeps hitting the cache regardless of client emission
order.

The sort is applied only when it is provably safe: every entry must be a
named custom tool with a unique name. Any opaque or server-side tool
entry, or a duplicate name, disables the sort for that request (the array
is passed through untouched), so normalization can never reorder a set it
cannot fully reason about.

```toml
[cache]
# Kill switch: set false to pass the client's tool order through
# verbatim on the OAuth Anthropic path.
normalize_tools = false
```

A missing `[cache]` block keeps the default: normalization enabled.

### Break-even emission gate (`k_gated_emission`)

`k_gated_emission` (bool, **default false**) is the master switch for
withholding an auto-emitted cache marker on a session whose *measured*
per-turn reuse is too low for the marker to pay for itself. It is a
cost knob only: with it on, an affected request goes out uncached and
still returns a correct HTTP 200. Nothing about the response changes.

```toml
[cache]
# Opt in to withholding markers on measurably low-reuse sessions.
# Default false.
k_gated_emission = true
```

It is hot-reloadable, like the rest of `[cache]`: write the value and
let the running daemon reload, no restart. A reload that CHANGED it
stamps `k_gated_emission_before` / `k_gated_emission_after` on the reload
success line, and stamps neither when the value was already what the
file says -- so the pair's presence is the confirmation that the flip
landed. A reload that fails parse or validation keeps the previous
config and logs no success line at all, so the gate never half-applies.
See [LOGGING.md](LOGGING.md), "Config-reload transition fields".

Turning it back off restores the ungated emission behavior immediately
for everything admitted after the swap, and the reuse evidence keeps
accumulating while the switch is off -- so enabling it later does not
start from a cold estimator.

#### When suppression fires: the K\* arithmetic

Emitting a marker trades a one-time write premium for a per-read
discount. Over one write plus `K` subsequent reads of that prefix, with
`C` the cost of the same prefix sent uncached, `Wm` the tier's write
multiplier and `Rm` its read multiplier:

```
no marker:    (1 + K) * C * 1.0
with marker:  C*Wm + K * C * Rm

marker wins iff  Wm + K*Rm < 1 + K
              -> K* = (Wm - 1) / (1 - Rm)
```

`C` cancels, so the threshold depends only on the tier's multipliers --
no token counting, no prefix size. `K*` is the reuse count at which the
marker breaks even; suppression fires only when the session's *measured*
reuse floor sits below it.

| Tier                            | `Wm`  | `Rm` | `K*`   |
|---------------------------------|-------|------|--------|
| Anthropic / Bedrock 5-minute    | 1.25  | 0.10 | ~0.278 |
| Anthropic / Bedrock 1-hour      | 2.0   | 0.10 | ~1.11  |
| Any server-side auto-cacher     | (exempt) | -- | never suppress |

Two conditions exempt a tier from suppression regardless of `K*`. A
server-side auto-cacher is exempt by an explicit flag check, not by its
`Wm` -- these providers cache for free, so a marker is never withheld on
economic grounds even though a row like `openai-responses` carries a
`Wm` of 1.25. Separately, a `Wm` of 1.0 or below means the write carries
no premium, so no reuse is needed to justify it and suppression cannot
fire there either.

**Read the 5-minute number as the operating point: `K*` of ~0.278 means
suppression should almost never fire.** It takes a session whose
lower-confidence-bound reuse estimate sits below roughly a quarter of a
reuse per turn -- a near-all-miss session, which is exactly the
write-premium-for-nothing case the gate exists to stop. Reuse evidence
also has to be *calibrated* before it can suppress at all: a thin-sampled
session reads as low-confidence and always emits. If suppression fires
broadly on your traffic with the switch on, the estimate or its keying is
wrong, not the traffic -- turn the switch back off and treat it as a bug
report.

#### Why the default is off

Three reasons, in weight order:

1. **Stale evidence predates the caching it is measuring.** Front-marker
   emission is recent. Every reuse window recorded before it shipped
   describes a world with LESS caching in it, so a large share of
   otherwise-calibrated windows read as all-miss for *structural*
   reasons rather than economic ones. Turning suppression on at upgrade
   time would withhold markers from sessions on evidence gathered before
   those sessions could have cached at all. Default-off is the whole
   mitigation: the windows re-fill under emission first.
2. **The calibration threshold is provisional.** A window is treated as
   calibrated after 8 samples, a value documented in the estimator as
   provisional and not yet tuned by the calibration harness. The
   estimator-coverage bar for acting on reuse evidence is not met yet.
3. **A wrong-low estimate costs money at HTTP 200.** The estimator is
   deliberately biased low -- correct when the number authorizes a
   destructive trim, wrong when it withholds a beneficial marker. A
   false suppression produces no error, no latency change, and no alert:
   just a higher bill. Regret is bounded only by how fast it is noticed
   and this switch is flipped back.

Default-off makes an upgrade a no-op on live traffic. You are not blind
before flipping it: routectl already WARNs on cache thrash -- sessions
recording an auto-emitted marker with cache writes and no reads, i.e.
precisely the sessions paying the write premium for nothing (see
[LOGGING.md](LOGGING.md)). Read that warning first; it tells you whether
there is anything for this gate to suppress.

There is no per-provider override. The global switch already restores the
previous behavior instantly, and a narrower knob has no stated need.

## Learned capability tempo (`[capability]`)

routectl learns per-provider capability signals (which upstreams honor
which features) and lets a negative decay back into a re-probe over time.
The optional `[capability]` block is the **global** control surface for
that subsystem: a kill switch plus the tempo knobs. A missing block keeps
the defaults below.

```toml
[capability]
# Master switch for the learned-capability subsystem. Default true.
enabled = true
# Hours a learned negative acts before it lapses into a single re-probe.
# Default 48.
decay_hours = 48
# Hours a pending single-observation inferred signal waits for a
# confirming second observation before it resets. Default 1.
inferred_window_hours = 1
# Days past which a verified capability reads as stale in diagnostics.
# Display-only -- surfaced in doctor / CLI hints, never wired into the
# act path. Default 14.
staleness_hint_days = 14
```

- **`enabled`** (bool, default true) -- the master switch. When off, any
  learned entries stay resident but inert: both the learn path and the
  act path are skipped.
- **`decay_hours`** (u64, default 48) -- how long a learned negative acts
  before it lapses into a single re-probe.
- **`inferred_window_hours`** (u64, default 1) -- how long a pending
  single-observation inferred signal waits for a confirming second
  observation before it resets.
- **`staleness_hint_days`** (u64, default 14) -- the age past which a
  verified capability reads as stale in diagnostics. This is
  **display-only**: it drives the stale-cell flag in the `routectl
  doctor` capability matrix and the CLI staleness hint, and is never
  wired into router construction or the act path.

Operator capability overrides nest under this parent as
`[capability.overrides]` (keyed by `"provider"` or `"provider:nickname"`)
rather than a new top-level section. A missing `[capability]` block
deserializes to the defaults above, and an unknown key inside the block
fails config load rather than being silently ignored.

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

Reduction is applied to completions and streaming. It is **on by
default**; an operator opts out globally or per provider.

The optional `[reduction]` block is the **global** master switch. A
missing block keeps the default: reduction enabled.

```toml
[reduction]
# Master switch for dispatch-path context reduction (whitespace-only
# minify of JSON-valued string tool content in the mutable tail).
# Default true; set false to opt out.
enabled = false
```

### Per-provider switch (`reduction_enabled`)

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
api_key_ref = "env://COMPAT_A_API_KEY"
base_url = "https://example.invalid/v1"
# Inherits the global switch (reduction ON for this provider).

[providers.compat-b]
kind = "openai-compat"
api_key_ref = "env://COMPAT_B_API_KEY"
base_url = "https://example.invalid/v1"
# Opt this provider out while leaving the global default on.
reduction_enabled = false
```

The per-request decision token is not persisted, and it is only partially
observable: when reduction actually strips bytes a `context_reduction`
line is logged at DEBUG (counts only -- no bodies), and that is the only
outcome that surfaces anywhere. A request whose reduction was skipped
(disabled, no mutable tail, nothing to strip) logs nothing. The usage DB's
`reduction_strategy` column is write-stopped (retained in the schema, NULL
for every row written by this version onward). See
[LOGGING.md](LOGGING.md).

### Kill switch: `[reduction] enabled` is the live off switch

`[reduction] enabled` is hot-reloadable, and it IS the kill switch. There
is no separate emergency knob: setting it to `false` and letting the
running daemon reload turns the reducer off without a restart.

- **Scope** -- the global switch governs every provider. A per-provider
  `reduction_enabled = false` narrows reduction for one provider; only the
  global switch turns the whole transform off.
- **Effect on the next request** -- each HTTP handler pins ONE router
  snapshot at request entry and dispatch reads the reduction switch off
  that snapshot. So the first request admitted after the swap egresses
  byte-identical passthrough bytes.
- **In-flight requests keep the old state** -- a request admitted BEFORE
  the swap uses its pinned snapshot for its WHOLE fallback chain, so a
  chain entry prepared after the flip still reduces. This is documented
  behavior, not a defect: pinning is what keeps one request's bytes
  self-consistent across retries and fallbacks. In-flight requests drain
  in seconds; the switch is immediate for everything admitted after it.
- **A rejected reload changes nothing** -- if the candidate config fails
  parse or validation, the running router (and its reduction state) stays
  live and the daemon logs a WARN naming the failure. The switch does not
  half-apply.
- **One-time cache churn is expected in BOTH directions.** Reduction runs
  before auto-cache, so a session whose prefix was cached with the other
  setting re-writes its cache once after the flip, then is stable (the
  reducer is deterministic). That is churn, not corruption.

#### Recovery runbook

Run these in order; stop as soon as the transition log confirms the flip.

1. **Write the config.** Set `enabled = false` under `[reduction]` in
   `config.toml` (or `routectl config set reduction.enabled false`). Any
   editor works: the watcher handles atomic-rename writes.
2. **Verify the transition log.** Watch the daemon log for the reload
   success line carrying the before/after pair:

   ```bash
   ROUTECTL_LOG=routectl_cli::server=info ./routectl serve
   # config reloaded; router rebuilt and swapped ...
   #   reduction_enabled_before=true reduction_enabled_after=false
   ```

   Those two fields appear ONLY when the value actually changed, so their
   presence is the confirmation that the flip landed. See
   [LOGGING.md](LOGGING.md), "Config-reload transition fields".
3. **SIGHUP if the watcher did not fire.** No reload line at all means the
   filesystem event was missed, not that the config was rejected. Send
   `SIGHUP` to the daemon to drive the same reload coordinator directly:

   ```bash
   kill -HUP <routectl-pid>
   ```

4. **Restart if reload validation repeatedly fails.** A WARN naming a
   parse or validation error means the candidate config is bad -- fix what
   the message names first. If a config that `routectl config check`
   accepts still fails to reload, restart the daemon; a cold start runs the
   same loader with no prior state to carry over.

#### Independence from advisory trim (`[trim]`)

The lossless minifier's switch and the advisory trimmer's controls are
SEPARATE mechanisms and never share a switch:

- `[reduction] enabled` is the **global master switch for the lossless
  minifier**, and each `[providers.X] reduction_enabled` is a per-provider
  override of that one transform.
- The advisory trimmer's live-cut switch, when trimming gains one, is a
  NEW key under `[trim]` -- distinct from `[reduction] enabled` and from
  every per-provider `reduction_enabled`.

This is deliberate. Killing a risky lossy cut must not silently kill the
safe lossless minifier, and all four on/off combinations stay legal:

| `[reduction]` lossless | lossy trim cut | Meaning |
|---|---|---|
| on | on | Both transforms active. |
| on | off | Minify only; trimming stays advisory (observations still recorded). |
| off | on | Lossy cut without the minifier. |
| off | off | Neither transform mutates a request. |

Two consequences follow directly, and neither may be traded away:

- A provider's lossless opt-out (`reduction_enabled = false`) must NOT
  disable lossy trimming for that provider.
- Disabling lossy mutation must NOT suppress advisory trim observations --
  the would-trim recorder is a measurement surface, independent of whether
  anything acts on it.

## Steady-state advisory trim (`[trim]`)

routectl carries a deterministic, front-anchored steady-state context
trimmer that is **advisory only** in this release: it computes what a
cache-coherent prefix cut WOULD look like for a long-running
conversation, but never mutates a dispatched request. The `[trim]`
block tunes the four knobs the trimmer's proposal is a pure function
of; it carries no `enabled` switch and no per-provider override, because
an always-on advisory recorder that never mutates has no "off" state to
represent.

```toml
[trim]
# Estimated total tokens at or below which no trim is proposed.
trigger_tokens = 100000
# Minimum tokens the elided span must free for a trim to be proposed.
clear_at_least_tokens = 20000
# Leading messages kept fully intact (never elided).
head_keep_messages = 2
# Trailing messages protected from elision.
keep_recent_messages = 6
```

A missing `[trim]` block keeps these same conservative defaults. All
four fields are required to have sane values -- an unknown key inside
the block fails config load rather than being silently ignored.

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

### Steady-state would-trim opportunity

Under `--detail`, and in the `routectl doctor` would-trim panel, the
report surfaces the advisory steady-state would-trim opportunity recorded
in the usage ledger -- how much cacheable context routectl's trimmer
flagged as a would-cut candidate WITHOUT applying it. The recording is
non-mutating (nothing is trimmed from a live request); it measures the
opportunity so an operator can decide whether to enable trimming. The
`doctor` panel reads the same figures read-only over all recorded
history, and its remediation hint points at `prompt-size --steady-state`
for a per-request inspection.

- **`candidate_requests`** -- count of requests in the window that
  carried a would-cut candidate.
- **`would_trim_tokens`** -- summed candidate freed-token count over
  those `candidate_requests`.
- **`verdict`** -- how the candidate rows partition by the K-estimator's
  confidence call:
  - `met` -- priced and calibrated, and the reuse floor cleared the
    break-even threshold: a real cut would have been authorized.
  - `unmet` -- priced and calibrated, but the floor fell short of
    break-even: not enough predicted reuse to justify the cut.
  - `cold` -- priced but not yet calibrated (no floor stamped): too few
    samples for a confidence call.
  - `unpriced` -- no verified pricing row, so the break-even threshold
    could not be computed.

## Diagnostics (`routectl doctor` and `routectl provider probe`)

`routectl doctor` and `routectl provider probe` are the two read-only
health surfaces. Both mutate NOTHING -- config, credentials, the catalog
overlay, and the usage DB are byte-identical after a run -- and both share
one exit-code contract and one probe-classification seam, so the two
never disagree about a provider.

### Exit-code contract (STABLE)

Both commands map their findings to a process exit code the same way, and
this mapping is the one part of the surface pinned pre-1.0:

- **PASS or WARN -> exit 0.** A warning is advisory (something to look at,
  not a failure). A run whose findings are all PASS and WARN exits 0.
- **FAIL -> nonzero exit.** A single FAIL finding makes the whole run exit
  nonzero. The code is order-independent: it depends only on whether any
  finding failed, never on provider ordering.

Script against the exit code, not against the rendered text or the
`--json` shape (below). `provider probe <name>` with an unconfigured name
is a separate usage error that also exits nonzero.

### `--json` is UNSTABLE pre-1.0

Both commands take `--json` for a machine-readable report, and both
payloads carry a top-level `schema_version`. The two version independently
off their own constants -- `SCHEMA_VERSION` in
`crates/routectl-cli/src/commands/doctor/mod.rs` for `doctor`, and the one
in `crates/routectl-cli/src/commands/probe/mod.rs` for `provider probe` --
so read the number off the payload you actually receive rather than
hardcoding one. The JSON shape is UNSTABLE before 1.0: fields may be added,
renamed, or restructured, and `schema_version` bumps when they do,
INCLUDING on a purely additive change (the report is explicitly
human-facing and non-contractual). Only the exit-code contract above is
pinned -- do not build a durable integration on the exact JSON shape.

`provider probe --json`:

```json
{
  "schema_version": 1,
  "providers": [
    { "name": "anthropic", "outcome": "Reachable" },
    { "name": "compat", "outcome": { "AuthFailed": "not logged in" } }
  ]
}
```

`doctor --json` carries the flat findings list plus the structured panels
(`schema_version` elided here -- read it off the payload):

```json
{
  "findings": [
    {
      "section": "auth",
      "name": "anthropic",
      "status": "Warn",
      "detail": "no oauth providers are logged in",
      "remediation": "run `routectl login <provider>` to authenticate"
    }
  ],
  "panels": { "would_trim": null, "capability_matrix": null }
}
```

`status` is one of `"Pass"`, `"Warn"`, `"Fail"`; `remediation` is `null`
on a clean finding and a fix string on every WARN/FAIL. Each panel is
`null` when it could not be computed: `would_trim` when there is no usage
data to summarize (its fields are documented under
[Steady-state would-trim opportunity](#steady-state-would-trim-opportunity)),
`capability_matrix` when the config layer it draws lanes from could not be
loaded.

### `/status/query` is UNSTABLE pre-1.0

The running server's `/status/query` endpoint answers a grouped, windowed
aggregate over the local usage ledger. Like `--json` above, its payload
carries a `schema_version` (currently `1`) and its shape is UNSTABLE before
1.0 -- metric names may be added and the envelope may be restructured. The
`schema_version` bumps on any semantic change or removal; purely additive
changes (a new metric, a new grouping or window token) do not bump it.

It answers the `QUERY` method only; every other method on that path is a
405. The request body is a closed vocabulary -- an unknown key or token is
refused with HTTP 400 and the fixed code `invalid_query`, and a body the
server can read but a ledger it cannot is HTTP 200 with an unavailable
panel, never a 400.

```
QUERY /status/query
{"window": "week", "group_by": "model"}
```

`window` is one of `today`, `week`, `month`, `all`; `group_by` is one of
`model`, `provider`, `alias`; optional `alias` and `provider` keys narrow
the result to one routing alias or served provider. Do not build a durable
integration on the response shape before 1.0.

### `provider probe [<name>]` -- reachability, free-only

`routectl provider probe` reports one reachability outcome per configured
provider (or just `<name>` when given). It is FREE-ONLY by construction:
it never makes a billed upstream call and never mutates a credential.

- **No free endpoint -> WARN, never a silent charge.** A provider kind
  with no cheap reachability check reports WARN with a "cannot verify
  without a billed call" reason -- routectl will not spend money to turn a
  WARN into a PASS. This is a warning (exit 0), not a failure.
- **Forwarded providers are SKIPPED.** A `credential_source = "forwarded"`
  provider (see
  [credential_source](#credential_source-anthropic-api-provider-flag----forwarded-credential))
  short-circuits before any build or upstream call and renders an
  informational PASS line -- there is no routectl-managed credential to
  probe.
- **OAuth is probed read-only.** An `oauth://` provider is checked against
  the in-memory credential cache only: a present seat reports reachable, a
  missing or expired one reports a FAIL with a `routectl login`
  remediation. The probe never refreshes a near-expiry token, so the
  credentials store is byte-identical afterward.
- **Static-credential and bedrock providers** (`env://` / `file://`,
  bedrock) build the provider and call its free reachability
  check.
- **Unreachable -> FAIL.** A provider that cannot be reached (network
  failure, a credential store that will not open, or a probe that overruns
  the shared deadline) reports FAIL with a remediation.

The probe is a one-shot process: it reports static reachability, not live
runtime state. The per-seat circuit-breaker readout an operator watches
during traffic is NOT part of `provider probe` -- that state belongs to a
running `serve` instance, not a one-shot command.

Outcome-to-status summary:

| Outcome | Status | Exit |
|---|---|---|
| reachable | PASS | 0 |
| skipped (forwarded) | PASS | 0 |
| no free endpoint / cannot verify | WARN | 0 |
| auth failed (not logged in / expired) | FAIL | nonzero |
| unreachable (network / store / deadline) | FAIL | nonzero |

### `routectl doctor` -- the health battery

`routectl doctor` runs a fixed ordered battery of read-only sections and
prints a finding list plus a summary line (`summary: PASS n  WARN n  FAIL
n`). It loads config through the never-migrating loader and reads the raw
bytes for a schema-version preflight that never stamps the file.

`doctor` NEVER auto-fixes anything. Every WARN or FAIL finding carries a
remediation that NAMES the fix (`run \`routectl login anthropic\``, `run
\`routectl config migrate\``, ...) for the operator to run -- the command
diagnoses, it does not mutate.

The battery sections, in render order:

| Section | Checks |
|---|---|
| Provider activation (`inventory`) | Whether each known provider's credential is present and usable; a configured route that depends on an unusable provider is a WARN. |
| Config schema version (`version`) | The config's schema version against the binary; a too-old config FAILs with a `config migrate` remediation, a too-new one FAILs with an upgrade remediation, a present-but-broken config FAILs rather than reporting all-PASS. |
| Config validation (`config`) | The static validator suite (the same one `config check` runs) plus a read-only secret-presence scan. Every message names the ref SCHEME, never the secret value, path, or env var name. |
| OAuth credentials (`auth`) | Stored OAuth seats and their expiry; no seats logged in is a WARN, an expired seat is a WARN, a credential store that will not open is a FAIL. |
| OAuth seat pools (`pools`) | One purely informational PASS row per `oauth://` reference per provider entry, describing how many stored seats it resolves to and which `seat_selection` strategy applies. A bare `oauth://<provider>` pool ref lists the seats it expands to (the default seat as `default`, then labelled siblings; a long list is truncated with an exact remaining count); a labelled ref reports that it pins one seat and that `seat_selection` does not apply to it. Never advises, carries no remediation, and can never move the exit code; when the credential store will not open the count reads as unknown while the strategy still renders (the `auth` section owns that FAIL). |
| OAuth seats (`seats`) | Stored OAuth seats no provider entry's `oauth://` ref reaches surface as a WARN naming the seat. Matching is by full seat identity: a labelled ref (`oauth://<provider>#<label>`) pins that one seat and covers no sibling, while a bare `oauth://<provider>` pool ref covers every stored seat of that provider. Read-only -- a stored credential is NEVER auto-deleted or refreshed, and the finding names only the seat key, never token material or a storage path. |
| Managed secrets (`secrets`) | Managed secret files not referenced by any provider surface as a WARN. The scan is a read-only directory diff; a stored secret is NEVER auto-deleted. |
| Provider reachability (`probe`) | One finding per provider through the SAME probe seam `provider probe` uses, so the two surfaces never diverge on status, detail, or remediation. |
| Capability (`capability`) | The learned-capability findings NOT absorbed by the capability matrix panel below: a WARN when the config layer could not be parsed (so the panel is honestly degraded rather than silently empty), and a WARN nudging `config migrate` when deprecated capability-list keys are still set. Never emits a FAIL, so it can never flip the exit code. |
| Catalog freshness (`freshness`) | Three advisory rows on how current the catalog data is: the baked catalog version and snapshot date, the freshest overlay verification stamp and its age, and the last SUCCESSFUL `catalog import` with its row counts. A stale overlay or import is a WARN pointing at `catalog import`; never a FAIL. |

Two structured panels render after the sections:

- **`panels.capability_matrix`** -- the learned-capability truth matrix:
  lanes (config model nicknames, plus any stale learned lane, marked
  `(unrouted)` when the loaded config no longer maps it) by capability
  keys, one resolved display cell each. Every cell merges the three signal
  layers -- operator overrides, the learned ledger-replay registry, and
  catalog priors -- through the same resolver the router uses, so the panel
  cannot drift from dispatch precedence; each cell carries its winning
  layer, its age, and a staleness flag. The ledger source reports a
  first-class tri-state (available / an honest empty / unavailable with a
  path-free class code) so "could not read" is never collapsed into
  "nothing learned".
- **`panels.would_trim`** -- the steady-state would-trim opportunity,
  read-only over all recorded history. Its fields are documented under
  [Steady-state would-trim opportunity](#steady-state-would-trim-opportunity)
  and its remediation hint points at `prompt-size --steady-state`.

The ordered section list is the command's single extension point
(`SECTIONS` in `crates/routectl-cli/src/commands/doctor/mod.rs`); the
offline status surface renders the same list minus `probe`.

Neither command reads a secret it does not need: like
[`catalog export`](#routectl-catalog-export), a probe or doctor run reads
only what it classifies and never discloses a credential value.

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

`config show` prints the post-merge view: `env://`, `file://`, and
`oauth://` references remain as opaque URIs (they are non-secret
pointers, not credential values); defaults are filled in; layered
overlays NOT yet applied
(those compose per request, not at startup). Key references need no
redaction here -- a config carrying an inline `literal:` secret is
rejected at validation before this view is ever produced -- but
`base_url` DOES get reduced to its origin, because a `base_url` may
legitimately carry a credential and is not rejected for it. Useful when
chasing
"why is my model picking provider Y instead of Z" without flipping
trace logging.

For active triage of a specific failing request, combine `config show`
with `ROUTECTL_LOG=routectl=debug` and the `request_id` correlation
workflow -- see [LOGGING.md](LOGGING.md) for the full triage recipes.

### Error messages: did-you-mean + source lines

routectl parses `config.toml` through a single funnel, so the same
diagnostics reach every surface that loads config -- `config check`,
`serve`, and hot reload.

- **Did-you-mean suggestions.** An unknown field, or an unknown enum
  variant (a mistyped `[providers.X] kind`, a bad `[retry.classes.<class>]`
  key), is rejected with the offending token AND a `did you mean `Y`?`
  hint naming the closest real name -- when one is close enough. A token
  far from every known name gets no guess rather than a misleading one.
  The candidate list is exactly the one the TOML/serde parser already
  emits, so it can never drift from the real fields. These fire wherever
  config is parsed (check / serve / hot reload).
- **Source lines on `config check`.** Semantic validation errors -- an
  alias chain naming a missing nickname, a model referencing an unknown
  provider, a reserved `[retry.classes.feature-unsupported]` override --
  are
  prefixed with `(line N): ` pointing at the line in your `config.toml`
  that produced them. This locating runs on `config check` only; it is a
  display aid that never changes which configs are accepted or rejected,
  and falls back to the plain message when a line cannot be resolved.

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
   # plus a [providers.anthropic-managed.header_extras] sub-table --
   # see "Header pack" below
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
   `~/.zshrc`, etc.). Use whatever `[server] port` your config sets
   (the examples in this section assume `port = 9100`; the quick-start
   default is `8787`):

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

Seats are NOT declared in this file. They live in the credential store
(`credentials.json`), and the only way to add one is to log in:
`routectl login <provider> --label <name>`. The config's `api_key_ref`
selects among the seats already stored; it never creates one. `routectl
config check` and `routectl doctor` both report, per provider entry, how
many stored seats its ref resolves to and which labels they carry.

**Seat selection.** A `seat_selection` knob picks how dispatch chooses
among a pool's seats. It is set on the `[pools.<name>]` block that groups
the accounts, because the strategy is a property of the SET, not of one
transport. It takes effect only when a pool resolves to more than one
stored seat -- with a single seat (or a `#label`-pinned ref) there is
nothing to choose between, and every strategy behaves identically:

```toml
[providers.anthropic-managed]
kind          = "anthropic-api"
api_key_ref   = "oauth://anthropic"
auth_kind     = "oauth-bearer"

[pools.anthropic]
members        = ["anthropic-managed"]
seat_selection = "round-robin"   # "fill-first" (default) / "round-robin" / "sticky-least-loaded"
```

- `fill-first` (default) -- drain one seat before advancing to the
  next. A single-seat provider (the common case) keeps today's
  behavior with no config. The drain IS this strategy's contract, not a
  rough edge on it: holding one seat keeps that seat's prompt cache warm,
  and running it down until the upstream refuses is the price of the
  locality. Quota-aware placement deliberately does NOT apply here --
  choosing `fill-first` is choosing that trade, and overriding it would
  override a stated preference. An operator who wants budget-aware
  spreading sets `sticky-least-loaded` instead.
- `round-robin` -- rotate across seats to spread load, advancing the
  start seat once per request. Quota-aware placement does not apply here
  either.
- `sticky-least-loaded` -- the ONLY strategy quota-aware placement and
  the session-affinity layer apply to. Pin each conversation to one seat
  so its warm prompt cache is preserved, while balancing NEW conversations
  across seats by available capacity. A conversation's first request
  picks the least-loaded healthy seat (preferring seats with a closed
  breaker, then -- on a subscription pool with observed budgets -- the
  seat with the most remaining short-window budget, otherwise the most
  RPM headroom, with a deterministic tiebreak so a burst of new
  conversations does not herd onto one seat; see
  [`[seat_quota]`](#quota-aware-seat-placement-seat_quota)); every
  later request for that conversation routes back to the same seat. If
  that seat later goes unhealthy (rate-limited or breaker-open), the
  conversation migrates ONCE to a healthy sibling and does not flap back
  when the original recovers. Requires an inbound per-conversation key
  (the `x-claude-code-session-id` header Claude Code sends, or body
  `metadata.session_id`). A request without one mints no pin, since there
  is nothing to pin under; it still places by remaining subscription
  budget when `[seat_quota]` is on and the evidence is complete enough to
  act on, because the only thing that outranks quota fairness is
  protecting a warm prompt cache and a keyless request has none. "Complete
  enough" is the same bar the keyed pick uses: at least one eligible seat
  fresh and below its threshold, or every eligible seat fresh-known and
  over it. Mixed evidence -- some seats observed and over threshold, the
  rest unobserved -- falls back to `fill-first`, as do a pool with no
  readings at all and the switch off. The per-request decision (birth_pick
  / sticky_stay / overflow_repin / defer_no_healthy / keyless_quota /
  keyless_fill_first) is not
  persisted, and only partially logged: a DEBUG line marks a birth pick
  and an overflow repin, while `sticky_stay`, `defer_no_healthy` and the
  keyless fall-through emit no selection-decision line of their own. Note
  an incomplete-evidence fallback is separately visible in the throttled
  quota-placement diagnostic when `[seat_quota]` is on. The usage ledger's
  `selection_decision` column is write-stopped (retained in the schema,
  NULL for every row written by this version onward).

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
from the bundled `@anthropic-ai/sdk`. Written as a sub-table -- a
multi-line inline table (`{ ... }` spanning lines) is not legal TOML:

```toml
[providers.anthropic-managed.header_extras]
"anthropic-beta"                            = "claude-code-20250219,oauth-2025-04-20"
"x-app"                                     = "cli"
"anthropic-dangerous-direct-browser-access" = "true"
"x-stainless-arch"                          = "x64"
"x-stainless-lang"                          = "js"
"x-stainless-os"                            = "Linux"
"x-stainless-package-version"               = "0.94.0"
"x-stainless-runtime"                       = "node"
"x-stainless-runtime-version"               = "v24.3.0"
"x-stainless-timeout"                       = "600"
"x-stainless-retry-count"                   = "0"
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

### routectl-managed OAuth for Codex (recommended)

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
account_id_ref = "env://OPENAI_ACCOUNT_ID"
```

`account_id_ref` is REQUIRED on this path -- there is no OAuth session
for routectl to derive the UUID from. `env://` and `file://` refs work
for both fields. routectl never refreshes a
static bearer; rotation is the operator's job.

### `codex_version` (client-identity override)

The ChatGPT backend rejects a session whose HTTP client identity drifts
from what a real codex CLI install emits, so routectl mimics a specific
codex CLI version on the wire. That version is baked into the binary (a
pinned default that ships current); `codex_version` overrides it without
rebuilding, so an operator can track a newer upstream codex release the
moment it lands rather than waiting for a routectl release.

```toml
[providers.codex]
kind          = "openai-responses"
auth_kind     = "chatgpt-oauth"
api_key_ref   = "oauth://codex"
# codex_version = "0.145.0"   # default: the version pinned into this build
```

- **Default is the pinned version.** Omit the knob and routectl uses its
  baked-in default (the codex CLI version this build was cut against). The
  knob is only for tracking a version newer than the pin.
- **One derivation point.** The value flows into every codex fingerprint
  surface together -- the outbound `User-Agent`, the `version` identity
  header, and the OAuth token-refresh `User-Agent` -- so they can never
  drift from each other.
- **Restart-required.** `codex_version` is classified restart-required:
  a hot config reload does NOT change the running process's identity. The
  reload logs a "requires a daemon restart to take effect" warning and
  keeps serving the boot value until the daemon restarts.
- **Divergent values are an error.** If two `openai-responses` providers
  set DIFFERENT `codex_version` values, config load fails fast -- the
  process claims one codex identity, so a silent winner is forbidden.
  Providers that omit the knob inherit the resolved value.
- **Used verbatim, never sanitized** -- a transformed value is a different
  fingerprint than the operator asked for. The value must be bounded,
  header-legal ASCII with no whitespace or control bytes. An illegal value
  fails config validation and the serve / `routectl test` load aborts with a
  precise error (it is never silently sanitized). Only the unvalidated
  diagnostic path (`provider probe` / `doctor`, which skips config
  validation) degrades a stray illegal value to the pinned default with a
  warning rather than crashing the diagnostic.
- Overriding `version` or `user-agent` through `header_extras` still wins
  the merge, but doing so on a chatgpt-oauth provider with a value that
  diverges from the derived identity logs a warning -- prefer
  `codex_version` to keep the fingerprint coherent.

## xAI (Grok) provider

routectl can route to xAI's OpenAI-compatible API (`https://api.x.ai/v1`) using
an xAI OAuth bearer. The credential is managed the same way as the Codex flow --
PKCE, local callback server, credentials persisted to
`~/.config/routectl/credentials.json`.

### routectl-managed OAuth for xAI (recommended)

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
base_url    = "https://api.x.ai/v1"   # REQUIRED non-empty on this kind
api_key_ref = "oauth://xai"
```

The `openai-compat` variant carries NO auth-selector field -- entries are
`deny_unknown_fields`, so writing an `auth_kind` here fails config load with
`unknown field`. The `oauth://` scheme on `api_key_ref` is what selects the
bearer surface; `base_url` must be spelled out because validation rejects an
empty one on this kind. `routectl login xai` prints exactly this block on
success.

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
