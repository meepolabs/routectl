# routectl Configuration Reference

This doc covers routectl's TOML configuration schema -- what each knob
does, how the layered overlays merge, and which keys are reserved. For
per-upstream tuning recipes (DeepSeek echo-back, NIM cold-start, Bedrock
allowlists, OpenRouter analytics headers) see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md). For the rationale behind the
two-layer split (provider vs model) and the hub-and-spoke contract see
[ARCHITECTURE.md](ARCHITECTURE.md).

## Top-level shape

A routectl config is a single TOML file with up to six top-level
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
```

A working end-to-end example lives at
[`examples/config.toml`](../examples/config.toml). The Bedrock-specific
allowlist baseline (16 betas + 16 body fields, empirically verified
against `bedrock-runtime.us-west-2.amazonaws.com` on 2026-05-12) lives
at [`examples/bedrock.toml`](../examples/bedrock.toml). Copy and edit;
do not re-derive.

## Listener auth + routing

```toml
[server]
host = "127.0.0.1"
port = 8787
strict_translation = false   # set true for production CI

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

## Field-assignment table

| Field                          | Lives on            | Merge semantics                                                        |
|--------------------------------|---------------------|------------------------------------------------------------------------|
| `provider`                     | `[models.X]`        | required; refs a `[providers]` key                                     |
| `upstream`                     | `[models.X]`        | required                                                               |
| `selectable`                   | `[models.X]`        | default true                                                           |
| `thinking` (bool or "adaptive")| `[models.X]`        | model-only; caller `reasoning.enabled=false` always wins               |
| `effort` (enum)                | `[models.X]`        | model-only; caller `reasoning.effort` always wins                      |
| `reasoning_dialect`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `history_reasoning`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `additional_request_fields`    | `[models.X]`        | model-only (Bedrock Converse / Invoke bag)                             |
| `stream_first_byte_timeout_ms` | `[models.X]`        | model > provider > global                                              |
| `header_extras`                | BOTH                | model wins on key collision; `anthropic-beta` comma-unions (see below) |
| `payload_extras`               | BOTH                | deep recursive merge; model wins on leaf collision                     |
| `base_url`, `api_key_ref`, etc.| `[providers.X]`     | provider-only                                                          |
| `auth_kind`, `anthropic_version`| `[providers.X]`    | provider-only                                                          |
| `user_agent`                   | `[providers.X]`     | provider-only                                                          |
| `runtime` (RPM, breaker, timeouts) | `[providers.X]` | provider-only                                                          |
| `allowed_betas`                | `[providers.X]` Anthropic / `[bedrock]` global | provider-only                              |

Caller request shape > model defaults > provider/internal defaults. An
incoming `reasoning.effort = "minimal"` always wins over an
operator-configured `[models.X] thinking = "high"`. Use model defaults
to set a floor when the caller is silent.

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

## Retry and fallback defaults

The default `RetryPolicy.fallback_on_status` list (`[retry]` table in
TOML, `default_fallback_status` in `crates/routectl-router/src/config.rs`)
includes the Cloudflare extended 5xx range (520-527, 530) alongside the
standard `[408, 429, 500, 502, 503, 504]`. Cloudflare-fronted upstreams
(opencode.ai, openrouter.ai, etc.) surface upstream-origin failures via
this range; without them in the default list, a single 520 from a
Cloudflare-fronted provider would kill the request even when a sibling
provider in the chain could have served it. Operators with bespoke
upstream behavior can still override `fallback_on_status` in `[retry]`.

```toml
[retry]
max_attempts                  = 2
initial_backoff_ms            = 250
backoff_multiplier            = 2.0
jitter_ms                     = 50
fallback_on_status            = [408, 429, 500, 502, 503, 504]   # override
request_timeout_ms            = 300000        # 5 min per attempt
stream_first_byte_timeout_ms  = 90000         # 90s -- thinking models stall
```

Workspace defaults are tight on purpose -- surface real timeouts on
routine calls. Bump per-provider for known-slow upstreams (see the
`stream_first_byte_timeout_ms` table in
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#stream_first_byte_timeout_ms))
rather than loosening the global.

## Per-model knobs

Two per-model overrides live on `[models.X]`:

- `anthropic_beta = [...]` -- lifted onto `req.anthropic_beta` at
  dispatch time, deduplicated against client-supplied entries
  (client wins on order). Use when a provider serves multiple
  Claude models and only some support a given beta (e.g.
  `context-1m-2025-08-07` works on opus/sonnet but is rejected
  for haiku) -- saves duplicating the entire provider config.
  IMPORTANT: the merge is **additive**. A model entry's
  `anthropic_beta = []` does NOT suppress a beta the provider
  already sets via `extra_headers["anthropic-beta"]` or
  `[providers.X] anthropic_beta`. To make a model opt OUT of a
  provider-shipped beta, REMOVE the beta from the provider config
  and add it back to each `[models.X] anthropic_beta` that needs
  it. Comparison is exact-string (case sensitive) to match
  Anthropic's beta-name semantics; a casing typo propagates so
  the upstream's 400 surfaces the misconfig.

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
adaptive_thinking            = true
thinking                     = "high"
anthropic_beta               = ["context-1m-2025-08-07"]
stream_first_byte_timeout_ms = 300000         # opus override

[models.haiku45]
provider = "bedrock"
upstream = "us.anthropic.claude-haiku-4-5-v1:0"
# inherits provider's 60s; no per-model override
```

## Validating config

```bash
./routectl config check --config <path>    # validates schema before serve
./routectl config show                     # prints resolved config (inspection)
```

`config check` runs the same startup validation `serve` does -- secret
refs resolve, provider kinds map to known impls, alias chains reference
existing model nicknames, Bedrock allowlists include the
routectl-mandatory keys (`messages`, `anthropic_version`, `max_tokens`)
when set. A partial Bedrock allowlist silently breaks every Bedrock
request, so the validator surfaces it as a clean `Error::Config` at
startup rather than a runtime 400.

`config show` prints the post-merge view: secret refs resolved (values
redacted), defaults filled in, layered overlays NOT yet applied
(those compose per request, not at startup). Useful when chasing
"why is my model picking provider Y instead of Z" without flipping
trace logging.

For active triage of a specific failing request, combine `config show`
with `ROUTECTL_LOG=routectl=debug` and the `request_id` correlation
workflow -- see [LOGGING.md](LOGGING.md) for the full triage recipes.
