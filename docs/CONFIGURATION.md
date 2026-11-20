# routectl Configuration Reference

TOML configuration schema reference: every knob, how the layered
overlays merge, what's reserved.

> **In a hurry:** run `routectl init` (guided setup wizard), or copy
> [`examples/config.toml`](../examples/config.toml)
> for a full reference config -- it passes `routectl config check` as
> shipped, and needs only credentials to serve (print one anytime with
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
- [Provider entries (`[providers.X]`)](#provider-entries-providersx)
- [Config schema version (`version`)](#config-schema-version-version)
- [Retired config keys](#retired-config-keys)
- [Editor autocomplete (JSON Schema)](#editor-autocomplete-json-schema)
- [Listener auth + routing](#listener-auth--routing)
- [Validating config](#validating-config)

**Providers**
- [Per-provider runtime gates](#per-provider-runtime-gates) (RPM, circuit breaker, timeouts) -
  [local refusals](#local-refusals)
- [Per-provider capability filter](#per-provider-capability-filter)
- [Gemini](#gemini-gemini-gemini-cloud-code) -
  [ChatGPT / Codex](#chatgpt--codex-provider) -
  [xAI (Grok)](#xai-grok-provider)
- Bedrock: [formats](#bedrock-formats-bedrock-invoke-bedrock-converse) -
  [beta-flag controls](#bedrock-and-anthropic-api-beta-flag-controls) -
  [body fields](#bedrock-body-fields) -
  [mantle Anthropic lane](#bedrock-mantle-anthropic-lane) -
  [mantle OpenAI lanes](#bedrock-mantle-openai-lanes)
- anthropic-messages: [context_management](#context_management-anthropic-messages-only) -
  [forwarded credential](#forwarded-credential)

**Models and routing**
- [Model routing (`[aliases]`)](#model-routing-aliases)
- [Model directory (`[models]`)](#model-directory-models)
- [Per-model knobs](#per-model-knobs) (removed model keys, output ceiling)
- [history_reasoning](#history_reasoning-reasoning-echo-back)
- [Retry and fallback defaults](#retry-and-fallback-defaults)
- [Proactive context-window gate (`[window_gate]`)](#proactive-context-window-gate-window_gate)
- [Learned token-estimate correction](#learned-token-estimate-correction)
- [Quota-aware seat placement](#quota-aware-seat-placement)

**Caching and cost**
- [Prompt-cache auto-emission (`[cache]`)](#prompt-cache-auto-emission-cache)
  ([break-even emission gate](#break-even-emission-gate-k_gated_emission))
- [Context reduction (`[reduction]`)](#context-reduction-reduction)
  ([kill switch + recovery](#kill-switch-reduction-enabled-is-the-live-off-switch))
- [Steady-state advisory trim](#steady-state-advisory-trim)
- [Model prices (`[models.X.pricing]`)](#model-prices-modelsxpricing)
- [Refreshing the price table (`routectl prices refresh`)](#refreshing-the-price-table-routectl-prices-refresh)
- [Retired: `routectl catalog`](#retired-routectl-catalog)
- [Learned capability tempo (`[capability]`)](#learned-capability-tempo-capability)
- [OpenAI chat-lane rule (`reasoning_dialect`)](#openai-chat-lane-rule-reasoning_dialect)

**Operating the daemon**
- [Log knobs (environment only)](#log-knobs-environment-only)
- [Usage accounting (`[usage]`)](#usage-accounting-usage)
- [Live wire check](#live-wire-check)
- [Reading usage (`routectl usage`)](#reading-usage-routectl-usage)
- [Diagnostics (doctor / probe)](#diagnostics-routectl-doctor-and-routectl-provider-probe)
- [Inspecting a request offline (`prompt-size`)](#inspecting-a-request-offline-routectl-prompt-size)
- [Inspecting the effective config](#inspecting-the-effective-config-config-show---effective)
- [Editing config from the CLI](#editing-config-from-the-cli-config-set--config-unset)
- [Adding a provider (`provider add`)](#adding-a-provider-provider-add)
- [MITM front-proxy (`[mitm]`)](#mitm-front-proxy-mitm)

**Client integrations**
- [claude-code as a gateway client](#claude-code-as-a-gateway-client)

**Advanced: overlay merge internals**
- [Field-assignment table](#field-assignment-table)
- [header_extras merge](#header_extras-merge) /
  [payload_extras merge](#payload_extras-merge)
  ([client extras by dialect](#client-extras-follow-the-clients-dialect)) /
  [Reserved-header buckets](#reserved-header-buckets)
- [Worked example: three-source anthropic-beta compose](#worked-example-three-source-anthropic-beta-compose)

## Getting started: provider + model + alias

Three blocks route your first request. A **provider** says how to
reach an upstream (transport + auth), a **model** names one upstream
model on that provider, and an **alias** maps the model string your
client sends to that model (or to a fallback chain of them):

```toml
version = 5           # config schema version; routectl refuses an older
                      # or versionless file (write a new one with
                      # `routectl init`)

[providers.anthropic-api-key]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth       = "x-api-key"

[models.heavy]
provider = "anthropic-api-key"
upstream = "claude-opus-4-20250514"

[aliases]
"claude-opus-*" = "heavy"
default = "heavy"     # catch-all for unmatched model strings
```

An alias key must differ from the model nickname it points at: alias
resolution walks keys through the alias table before reaching a model, so
`heavy = "heavy"` reads as a one-hop cycle and is refused.

The entry is named `anthropic-api-key`, not the bare family name
`anthropic`: providers, pools and model nicknames share ONE namespace, so
an entry holding the plain family name makes `[pools.anthropic]`
unnameable, and a later `routectl login anthropic` then has nowhere to
put a second subscription seat.

A provider entry writes its keys in one order: `format` (the API it
speaks), then exactly one of `base_url` or `cloud` (where requests go),
then `credential` (which secret) and `auth` (how it goes on the wire).
[Provider entries](#provider-entries-providersx) lists every value of
those keys, which keys apply to which format, and the rules an entry must
meet. `config check`, daemon start and reload report every problem in the
file at once, naming the provider and the key and never printing the
value.

Save as `~/.config/routectl/config.toml` (or
`$XDG_CONFIG_HOME/routectl/config.toml`) on every OS, including macOS, then
`routectl config check` and `routectl serve`. `XDG_CONFIG_HOME` is used only
when it is an absolute path, and the home folder is `HOME` when that is an
absolute path; an empty or relative value counts as unset. On Windows without
an absolute `HOME` that is `%USERPROFILE%\.config\routectl\config.toml`
unless `XDG_CONFIG_HOME` is an absolute path. When neither variable (nor
`USERPROFILE` on Windows) is an absolute path, there is no default config:
a command that reads it stops with `could not resolve an absolute config
folder; set HOME or pass --config`, and nothing under the working directory
is read. When neither `--config` nor `ROUTECTL_CONFIG` names the config and
the new `config.toml` is missing, routectl checks the old folders on every
OS, in order: `%APPDATA%\routectl` when `APPDATA` is an absolute path, then
`~/Library/Application Support/routectl` when `HOME` is an absolute path.
The first that holds a `config.toml` or cannot be inspected stops every
command that reads or writes the config until you move its files or pass
`--config` (or set `ROUTECTL_CONFIG`); `logout`, `refresh`, `whoami`,
`config example`, `prices refresh` and `doctor` still run, and `doctor`
lists the old folder. A `credential` is always a reference:
`env://VAR`, `file:///abs/path` (owner-only, 0600),
`oauth://<vendor>` (populated by `routectl login <vendor>`), an AWS
form, or `forwarded` (see
[Provider entries](#provider-entries-providersx)); inline `literal:`
values are rejected at parse and resolve. To add a
provider from the command line (with secret capture) use
`routectl provider add --format <format>` or
`routectl provider add --login <vendor>` (see
[Adding a provider](#adding-a-provider-provider-add)); the rest of this document is the full
reference for everything the wizard does not cover.

## Top-level shape

A routectl config is a single TOML file with the following top-level
sections:

```toml
version = 5           # config schema version; see "Config schema version"

[server]              # listener: host, port, body limit
[server.auth]         # listener auth tokens (when binding non-loopback)

[providers.X]         # one upstream: format, base_url or cloud,
[providers.Y]         # credential, auth, then tuning (see "Provider
                      # entries"); one block per upstream

[pools.X]             # a group of interchangeable provider entries (OAuth
                      # accounts) dispatch spreads across: members,
                      # seat_selection, accepts_new_logins, plus the
                      # five tuning keys its members inherit (see
                      # "Pool tuning keys"). Optional.

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

[window_gate]         # proactive context-window gate switch
                      # (enabled). Optional; default off.

[usage]               # usage-accounting subsystem: db_path,
                      # retention_days, and the background usage
                      # poll switch (poll). Optional.

[mitm]                # MITM front-proxy: local TLS-terminating
                      # listener fronting api.anthropic.com.
                      # Optional; presence gates the feature on
                      # (absence = zero proxy startup).
```

[`examples/config.toml`](../examples/config.toml) passes `routectl config
check` exactly as shipped, but it is NOT boot-ready: every credential in
it is a REFERENCE, and none resolves until the named env var is set or
the matching `routectl login` has run (an unresolved ref is a warning,
never an error). Copy and edit; do not re-derive.

## Provider entries (`[providers.X]`)

Every `[providers.X]` entry answers the same questions with the same keys,
written in this order:

1. `format` -- the API the upstream speaks.
2. Exactly one of `base_url` (an explicit endpoint) or `cloud` (a cloud
   whose endpoint routectl derives).
3. `credential` -- the one credential reference.
4. `auth` -- how that credential goes on the wire.
5. Companions, only where the format needs them: `account_id_ref`,
   `cloud_project_id`.
6. Tuning keys, each optional with a default.

The four identity keys (`format`, the carrier, `credential`, `auth`) are
always written; nothing defaults them. An unknown key is refused.

```toml
[providers.openrouter]
format     = "openai-chat"
base_url   = "https://openrouter.ai/api/v1"
credential = "env://OPENROUTER_API_KEY"
auth       = "bearer"

[providers.bedrock-east]
format     = "bedrock-converse"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://profile/bedrock"
auth       = "aws-sigv4"
```

### `format`

| `format` | API | Carriers | `auth` values | Family token |
|---|---|---|---|---|
| `anthropic-messages` | Anthropic Messages | `base_url`; `cloud` (AWS Bedrock's managed Anthropic surface) | `base_url`: `x-api-key`, `bearer`; `cloud`: `bearer`, `aws-sigv4` | `anthropic-api` |
| `openai-chat` | OpenAI-compatible Chat Completions | `base_url`; `cloud` (Bedrock's managed OpenAI surface) | `base_url`: `bearer`; `cloud`: `bearer`, `aws-sigv4` | `openai-compat` |
| `openai-responses` | OpenAI Responses (the OpenAI API, or ChatGPT Codex on `chatgpt.com`) | `base_url`; `cloud` (Bedrock's managed OpenAI surface) | `base_url`: `bearer`; `cloud`: `bearer`, `aws-sigv4` | `openai-responses` |
| `bedrock-invoke` | AWS Bedrock InvokeModel (Anthropic Messages body) | `cloud` only | `bearer`, `aws-sigv4` | `bedrock` |
| `bedrock-converse` | AWS Bedrock Converse | `cloud` only | `bearer`, `aws-sigv4` | `bedrock` |
| `gemini` (untested) | Google Gemini `generateContent` with an API key | `base_url` only | `x-goog-api-key` | `gemini` |
| `gemini-cloud-code` (untested) | Gemini through the Cloud Code surface | `base_url` only | `bearer` | `gemini` |

A format with an `auth` value its carrier does not accept is refused,
naming the accepted values. A format over a carrier it does not list, or
whose provider is not compiled into the binary, is refused as not
available over that carrier in this build.

The family token is not a config key. It is the provider kind routectl
derives from `format` and stores with each usage row, learned capability,
calibration lane and price lookup, and it is what diagnostics print as
the provider `kind`. Several formats share one token. Learned capabilities
belong to the entry name and its token, so switching an entry between
`bedrock-invoke` and `bedrock-converse` keeps them, while a reload that
moves the entry to a format with another token drops them (the
`owner_sweep` line in [LOGGING.md](LOGGING.md)). Usage rows keep the
token they were written with.

**Untested formats.** `gemini` and `gemini-cloud-code` parse and route,
and so does the managed xAI login on `openai-chat` (see
[xAI (Grok) provider](#xai-grok-provider)), but none of the three has
been validated against its live upstream for this release. Treat them as
best-effort.

### `base_url` and `cloud`

Set exactly one; both or neither is refused.

`base_url` is a plain http(s) URL: a host and an optional path prefix
(a trailing `/` is fine), with no `user@` userinfo, no `?query` and no
`#fragment`. Request paths are appended to it. Write it in its
canonical form, the same text the client dials:

- no whitespace before or after the URL, inside the quotes;
- printable ASCII only: no spaces, tabs, backslashes or non-ASCII
  letters (give an internationalized host in its `xn--` form);
- no explicit default port (`:443` on https, `:80` on http);
- no `.` or `..` path segments, and no percent-escapes in the host.

Beyond one trailing `/`, only the letter case of the scheme and host may
differ from what is dialed. It must be `https://`, or `http://` on a
loopback host only (see
[`base_url` scheme requirement](#field-assignment-table)). No format has a
default: write the endpoint even for the vendor's public API.

`cloud` names a cloud by `name` plus that cloud's location keys, and
routectl derives the endpoint host and the signing scope from them. The
one cloud this build accepts is `aws`, located by `region`:

```toml
[providers.bedrock-west]
format     = "bedrock-invoke"
cloud      = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth       = "aws-sigv4"
```

`region` must be a canonical AWS region (see
[AWS region values](#aws-region-values)). For `bedrock-invoke` and
`bedrock-converse` the endpoint is Bedrock runtime; for the other three
formats it is Bedrock's managed surface (see
[Bedrock mantle Anthropic lane](#bedrock-mantle-anthropic-lane) and
[Bedrock mantle OpenAI lanes](#bedrock-mantle-openai-lanes)).

Other clouds are planned as further `name` values, each with its own
location keys (not settled yet). They are NOT supported: this build
refuses any `name` other than `aws`, so the shapes below show the
direction only and are not config to copy:

```text
# Not supported yet; refused at load.
cloud = { name = "google-vertex", <Vertex location keys> }
cloud = { name = "azure", <Azure location keys> }
```

### `credential`

One string, always a reference, never the secret itself:

| Value | Meaning |
|---|---|
| `env://VAR` | The value of an environment variable. |
| `file:///abs/path` | The contents of a file (refused on Unix unless owner-only, `chmod 600`). |
| `oauth://<vendor>` or `oauth://<vendor>#<label>` | A routectl-managed login stored by `routectl login <vendor> [--label <label>]`. The bare form is the vendor's default seat; `#<label>` picks one labeled seat. |
| `aws://profile/<name>` | A named profile from the shared AWS config. |
| `aws://chain` | The AWS SDK default credential chain (environment, profile, SSO, container and instance roles). |
| `forwarded` | The client's own credential, passed through (see [Forwarded credential](#forwarded-credential)). |

`literal:` values are refused. An AWS access key pair goes in a profile or
the environment and is reached through `aws://profile/<name>` or
`aws://chain`. A Bedrock API key is an ordinary `env://` or `file://`
reference with `auth = "bearer"`.

### `auth`

| Value | On the wire |
|---|---|
| `x-api-key` | `x-api-key: <credential>` |
| `bearer` | `Authorization: Bearer <credential>` |
| `x-goog-api-key` | `x-goog-api-key: <credential>` |
| `aws-sigv4` | AWS SigV4 request signing with the `aws://` credential |

### Rules an entry must meet

Each rule is checked before any credential is read, at `config check`,
daemon start, hot reload and on every config edit. A refusal names the
provider and the keys, never the base URL or the credential.

- `aws-sigv4` is used exactly when the credential is `aws://profile/<name>`
  or `aws://chain`.
- `forwarded` is accepted only with `format = "anthropic-messages"`,
  `auth = "bearer"` and an https `base_url` whose host is exactly
  `api.anthropic.com`, with no userinfo and no explicit non-default port.
- `gemini-cloud-code` takes only a managed `oauth://antigravity[#label]`
  login.
- A managed `oauth://<vendor>` login is accepted only on its vendor's own
  lane: the vendor's format, `auth = "bearer"`, and an https `base_url` on
  one of the vendor's first-party hosts with no userinfo and no explicit
  non-default port (see
  "Managed token host containment" under
  [Why route claude-code through routectl](#why-route-claude-code-through-routectl)).
  The same check covers `account_id_ref`.
- A key that applies to one format only (table below) is refused on any
  other format, even when written as `false`, an empty string or an empty
  list.
- `base_url` is a plain URL as described above.

**Client identity.** routectl presents a vendor's generated client
identity (the Claude Code header pack, user agent, session ids and body
cloak for Anthropic; the codex CLI headers and ids for Codex; the
Antigravity identity for Cloud Code) only when the entry's `credential`
is that vendor's managed login (`oauth://anthropic`, `oauth://codex` or
`oauth://antigravity`, any label) AND the entry is the vendor's own lane
as defined above. An API key or a pasted token in `env://` or `file://`
never gets one, even on the vendor's host, and neither does the managed
xAI login. Set `user_agent` or `header_extras` yourself where such an
upstream needs them.

### Companions

- `account_id_ref` (`openai-responses` only) -- a reference to the
  ChatGPT account id, for a `base_url` on the Codex host. Optional with an
  `oauth://codex` login (read from the login), required with a static
  credential there, refused anywhere else. See
  [ChatGPT / Codex provider](#chatgpt--codex-provider).
- `cloud_project_id` (`gemini-cloud-code` only) -- the Cloud Code project
  id. See [Gemini](#gemini-gemini-gemini-cloud-code).

### Which keys apply to which format

| Key | Formats |
|---|---|
| `anthropic_version`, `forward_client_headers`, `context_management`, `cloak_mode`, `cloak_strict_mode` | `anthropic-messages` |
| `codex_version`, `account_id_ref` | `openai-responses` |
| `cloud_project_id` | `gemini-cloud-code` |
| `header_extras`, `payload_extras`, `user_agent`, `cache_capability`, `reduction_enabled`, `circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`, `request_timeout_ms`, `stream_first_byte_timeout_ms` | every format |

Each key is described in its own section: `anthropic_version` defaults
to `2023-06-01` (see [Field-assignment table](#field-assignment-table));
`forward_client_headers` and the cloak keys under
[claude-code as a gateway client](#claude-code-as-a-gateway-client);
[context_management](#context_management-anthropic-messages-only);
[`codex_version`](#codex_version-client-identity-override);
[header_extras](#header_extras-merge) and
[payload_extras](#payload_extras-merge);
[`cache_capability`](#per-provider-capability-cache_capability);
[`reduction_enabled`](#per-provider-switch-reduction_enabled); the five
runtime keys under [Per-provider runtime gates](#per-provider-runtime-gates).

## Config schema version (`version`)

```toml
version = 5
```

- **`version`** (u32, required): the config schema version this file is
  written against. The current version is `5` (`CURRENT_CONFIG_VERSION` in
  `crates/routectl-router/src/config/validate.rs`), and it is the only
  version this build reads.

The version is checked in a preflight that reads the key straight off the
raw TOML text, BEFORE the typed deserialize runs, so a file written for
another schema is refused by its version instead of surfacing as a
confusing `unknown field` error. Three verdicts fail closed:

- **Too old** (`version` below 5):

  ```text
  config version 4 is no longer supported; v5 required. Move the old file aside, run `routectl init` to write a new one, and see docs/CONFIGURATION.md for the current schema. Nothing was written.
  ```

- **Missing** (no `version` key at all, which is every config written for
  routectl v0.9.0 or earlier):

  ```text
  config version missing; v5 required. Move the old file aside, run `routectl init` to write a new one, and see docs/CONFIGURATION.md for the current schema. Nothing was written.
  ```

- **Too new** (`version` above what this build supports): load fails
  naming a binary upgrade, so a config written by a newer routectl is
  never partially interpreted by an older one.

The same preflight runs at every entry point that reads the file:
`serve`, `config check`, `doctor` (its `version` section FAILs with the
same remediation), `init`, and every config edit (`config set` /
`config unset`, `provider add`, the `routectl login` config offer). It
never writes to the file, and there is no command that rewrites an older
file to the current schema. `version` is hot-reloadable: a live swap to a
file the preflight refuses is REJECTED with a WARN
(`config reload failed; keeping previous config`) and the running router
keeps serving the config it already loaded.

The preflight only ever speaks about the `version` key. TOML that does not
parse at all, or a `version` that is present but is not a plain
non-negative integer, falls through to the normal typed deserialize so the
precise syntax or type error is the one reported.

**Replacing an older config.** Move the refused file aside (keep it as a
reference), run `routectl init` to write a version 5 file, then carry your
settings over by hand using this reference. `init` refuses to edit a file
it cannot load, which is why the old file has to move first. What each
schema version retired, and where its replacement lives:

- **v1 -> v2:** the `[cache_pricing]` table; set a model's prices under
  `[models.X.pricing]` (see [Retired config keys](#retired-config-keys)).
- **v2 -> v3:** the raw-status retry allow/deny escape hatch; use
  per-class policy (see
  [Same-target retry counts](#same-target-retry-counts-retryclasses)).
- **v3 -> v4:** a bare `oauth://<provider>` ref means the default seat
  alone, and the provider-level `seat_selection` moves onto a
  `[pools.<name>]` block (see
  [Version 4 makes pools explicit](#version-4-makes-pools-explicit)).
- **v4 -> v5:** the capability lists; use `[capability.overrides]` and
  pass-through (see
  [Version 5 retires the capability lists](#version-5-retires-the-capability-lists)).

**Upgrading and rolling back a running daemon.** The binary and the file
move together: this build refuses the old file, and an older build may
not read the new one. Move the old file aside, install the new binary,
write the new file with `routectl init`, then restart the daemon onto the
new binary. Until that restart, a daemon still running the old binary
never sees a valid swap from this procedure: a hot reload that fails
keeps the config it already loaded. Rolling back means restoring the old
file together with the previous binary; swapping the binary back alone
leaves it facing a file it may refuse.

### Version 5 retires the capability lists

Config version 5 removes every per-provider and per-model capability
list. What they did is now covered by `[capability.overrides]`, by
pass-through, or by the router's own learned and seeded verdicts:

| Retired key | What replaces it |
| --- | --- |
| `[providers.X] unsupported_features` | `[capability.overrides.<provider>] unsupported` |
| `[models.X] unsupported_features` | `[capability.overrides."<provider>:<nickname>"] unsupported` |
| `[providers.X] allowed_betas` (anthropic-api) | nothing: client betas pass through, minus the flags the router withholds from the lane |
| `[bedrock] allowed_betas` | nothing: client betas pass through, minus the flags the router withholds from the lane |
| `[bedrock] allowed_body_fields` | nothing: every body field passes through except `mcp_servers` and an orphan `tool_choice` |
| `[bedrock]` table | nothing: it held only the two lists above |

When carrying an older file's lists over by hand:

- An `unsupported_features` list becomes
  `[capability.overrides.<spec>].unsupported`, where `<spec>` is the
  provider name for a provider list and `"<provider>:<nickname>"` for a
  model list.
- An allowlist names what may be sent; it does not convert into the list
  of what must not be, so decide per flag. Dropping a beta list accepts
  pass-through: client betas are forwarded except the ones the router
  withholds from the lane. For a beta that must never be sent, write:

  ```toml
  [capability.overrides.bedrock-west]
  unsupported = ["beta:context-1m-2025-08-07"]
  ```

  A body-field list has no successor; leave it out.

A version 5 file that still carries one of these keys is refused at load;
see [Retired config keys](#retired-config-keys) for the message and the
full list.

## Retired config keys

A config that still carries a retired key is refused at load, hot reload
(the running router keeps serving), `config check`, `doctor` and every
config edit, before the typed parse. One refusal lists every retired key
in the file, sorted by key path, one line each:

```text
`models.gpt.effort_levels` is retired: refused effort levels are learned per lane; delete it
```

Only the key path is shown, never its value. An entry name that is not a
bare key (letters, digits, `_` and `-`) is shown as `<quoted name>`,
since a quoted name can hold any text. `config set` on a retired
key, or on a path under a retired table, prints the same line. Delete the
keys by hand; nothing converts them. A key is written as its dotted path:
`bedrock` is the whole `[bedrock]` table, and `<name>` stands for any
entry name, so `models.<name>.effort_levels` is `effort_levels` in any
`[models.X]` block. A key retired only for one value is written with
that value (`` `<path> = "<value>"` ``); any other value stays valid.

| Retired key | What replaces it |
| --- | --- |
| `bedrock` | nothing replaces it: client betas and body fields pass through, minus what the router withholds from the lane |
| `cache_pricing` | set prices under `[models.X.pricing]` |
| `calibration` | the learned token-estimate correction always applies |
| `capability.staleness_hint_days` | diagnostics use a fixed 14-day hint |
| `fidelity` | prefix-impacting pre-flight repair is never admitted; class and quorum are fixed |
| `log` | set ROUTECTL_TRACE_HEADERS / ROUTECTL_TRACE_BODY_BYTES / ROUTECTL_LOG_REDACT_PROMPTS in the environment |
| `mitm.credential_source` | a forwarded credential is a per-provider choice: add `[providers.anthropic-forwarded]` with `format = "anthropic-messages"`, `base_url = "https://api.anthropic.com"`, `credential = "forwarded"` and `auth = "bearer"` |
| `mitm.mitm_host` | pinned to https://api.anthropic.com |
| `mitm.tested_cc_version` | routectl pins the Claude Code version it mints; nothing replaces the check |
| `mitm.upstream_origin` | pinned to https://api.anthropic.com |
| `models.<name>.additional_request_fields` | use payload_extras |
| `models.<name>.effort_levels` | refused effort levels are learned per lane |
| `models.<name>.max_thinking_budget` | client thinking budgets are forwarded |
| `models.<name>.reported_model` | the response model field echoes the requested model |
| `models.<name>.selectable` | delete the block or comment it out |
| `models.<name>.supports_adaptive_thinking` | client thinking is forwarded; effort alone adds no thinking |
| `models.<name>.unsupported_features` | use `[capability.overrides."<provider>:<nickname>"] unsupported` |
| `models.<name>.visible_routectl_provider` | responses carry no provider field |
| `pools.<name>.seat_selection = "round-robin"` | use sticky-least-loaded |
| `providers.<name>.allowed_betas` | nothing replaces it: client betas pass through, minus the flags the router withholds from the lane |
| `providers.<name>.anthropic_beta` | pin the flag in header_extras "anthropic-beta" |
| `providers.<name>.api_key_ref` | write the reference as `credential` |
| `providers.<name>.api_shape` | the Bedrock API is the format: `invoke` (the default) is `format = "bedrock-invoke"`, `converse` is `format = "bedrock-converse"` |
| `providers.<name>.auth_kind` | set `auth`: `api-key` is `x-api-key` on `anthropic-messages` over `base_url` and `bearer` everywhere else; `oauth-bearer` and `chatgpt-oauth` are `bearer`; `bedrock-mantle` is `cloud = { name = "aws", region = "<region>" }` with `auth = "bearer"` for a key or `auth = "aws-sigv4"` for an `aws://` credential |
| `providers.<name>.auth_mode` | set `format`: `api-key` is `format = "gemini"` with `auth = "x-goog-api-key"`, `cloud-code` is `format = "gemini-cloud-code"` with `auth = "bearer"` and `credential = "oauth://antigravity[#label]"` |
| `providers.<name>.auto_emit_per_block_breakpoints` | use the global [cache] auto_emit_top_level_breakpoint switch |
| `providers.<name>.auto_emit_top_level_breakpoint` | use the global [cache] auto_emit_top_level_breakpoint switch |
| `providers.<name>.bedrock_mantle` | set `cloud = { name = "aws", region = "<region>" }` in place of `base_url`; a `bearer-key` `key_ref` becomes `credential` with `auth = "bearer"`, a profile is `credential = "aws://profile/<name>"` and the default chain `credential = "aws://chain"`, both with `auth = "aws-sigv4"` |
| `providers.<name>.class_overrides` | per-class retry counts live in `[retry.classes]`; a status keeps its built-in class |
| `providers.<name>.cloak` | set `cloak_mode` and `cloak_strict_mode` on the entry itself |
| `providers.<name>.cloak_sensitive_words` | the sensitive-word scanner is removed; nothing replaces it |
| `providers.<name>.cloak_tool_rename` | tool names are normalized to mcp__ only |
| `providers.<name>.credential_source` | `forwarded` is `credential = "forwarded"` with `auth = "bearer"` on `format = "anthropic-messages"` and `base_url = "https://api.anthropic.com"`; `own` is the entry's own `credential` |
| `providers.<name>.creds` | write one `credential`: a `bearer-key` `key_ref` with `auth = "bearer"`, or `aws://profile/<name>` or `aws://chain` with `auth = "aws-sigv4"`; static key pairs move to a profile or the environment (read by `aws://chain`) |
| `providers.<name>.kind` | set `format` and `auth`: `anthropic-api` is `anthropic-messages` with `x-api-key` (`bearer` for an `oauth://` login or over `cloud`), `openai-compat` is `openai-chat` with `bearer`, `openai-responses` stays and takes `bearer`, `gemini` is `gemini` with `x-goog-api-key` (`auth_mode = "cloud-code"`: `gemini-cloud-code` with `bearer`), `bedrock` is `bedrock-invoke` (`api_shape = "converse"`: `bedrock-converse`) over `cloud` with `aws-sigv4` for an `aws://` credential or `bearer` for a key |
| `providers.<name>.max_thinking_entry_bytes` | the thinking-cache entry cap is fixed at 1 MiB |
| `providers.<name>.region` | set `cloud = { name = "aws", region = "<region>" }` |
| `providers.<name>.unsupported_features` | use `[capability.overrides.<provider>] unsupported` |
| `registry` | set prices under `[models.X.pricing]` |
| `retry.classes.<name>.fallback` | chains and pools fall back; an unknown failure stays terminal |
| `retry.classes.<name>.retry` | write the count as `[retry.classes] <class> = <n>` |
| `retry.retry_on_429` | set `[retry.classes] rate-limited = <n>` |
| `retry.retry_on_5xx` | set `[retry.classes] server-error = <n>` and `overloaded = <n>` |
| `retry.retry_on_network` | set `[retry.classes] timeout = <n>` and `network-error = <n>` |
| `seat_quota` | quota-aware placement always applies on sticky-least-loaded pools |
| `server.allow_disable_fallbacks` | the x-routectl-disable-fallbacks header is always honored |
| `server.strict_translation` | lossy seams forward and count |
| `trim` | the advisory trim uses fixed defaults |
| `usage.enabled` | usage recording is always on |
| `wire_check` | sampling is fixed: first 8 per lane and version, then 1 in 100; 24 h status window; set ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN in the environment to change the 1-in-100 rate |

The one value the retired `wire_check` table set that can still change is
the 1-in-100 rate: `ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN` in the environment
that launches `routectl serve` (a whole number from 1 up; see
[Live wire check](#live-wire-check)).

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
schema version it was generated against. The schema does NOT pin the
`version` field to a single value, so an editor will not flag an older
`version`; the load-time preflight is what refuses it (see
[Config schema version](#config-schema-version-version)).

## Listener auth + routing

```toml
[server]
host = "127.0.0.1"
port = 8787
max_body_bytes = 33554432       # 32 MiB default; larger bodies are rejected with HTTP 413

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
  - `thinking.display` -> **forwarded verbatim**. The upstream vocabulary
    is beta-extensible (`"summarized"` and `"omitted"` are long-standing;
    the `thinking-display-updates-2026-08-18` beta added `"updates"`), so
    routectl does not hold an allowlist that a newer client would trip:
    any string value reaches the wire unchanged and upstream rules on it,
    with one **DEBUG** line when the value is one routectl does not model
    canonically. A NON-string `display` is still a **local HTTP 400**
    (`invalid_request_error`), since the carrier that preserves the
    literal is a string and there is no shape to forward. An absent
    `display` key stays absent on the wire -- routectl never injects a
    default, since Anthropic's own default is model-dependent. See
    [WIRE-GOTCHAS.md](WIRE-GOTCHAS.md) for the wire-shape quirks under a
    suppressing value and [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) for how
    Bedrock Converse forwards it (including the beta `"updates"` needs).
- `POST /v1/responses` -- OpenAI Responses API requests (the shape a
  Codex client sends). routectl is stateless, so the Responses
  server-side conversation state is handled deterministically:
  - `previous_response_id` -> **HTTP 400** (`invalid_request_error`).
    The reference points at a prior turn routectl never stored;
    answering anyway would be a silent wrong answer. Configure the
    client to send the full conversation `input` each turn.
  - `conversation` (any non-null value) -> **HTTP 400**
    (`invalid_request_error`), for the same reason: it names a
    conversation routectl never stored.
  - `background: true` -> **HTTP 400** (`invalid_request_error`). It asks
    the server to keep the response for later retrieval, which a
    stateless proxy cannot do. `background: false` or absent is accepted.
  - The 400 message names the member it refused (the first of
    `previous_response_id`, `conversation`, `background` the request
    sets) and asks the client to send the full conversation instead.
  - `store: true` without a `previous_response_id` -> **accepted**. The
    current turn is self-contained so the answer is correct; the
    persistence intent is ignored and logged at WARN once per process (a
    later retrieval-by-id against this stateless proxy will find nothing).
  - `store: false` / absent -> normal stateless path.

On every inference route (including `count_tokens`), a transcript whose
tool calls and tool results carry ids but are not correctly paired is
**forwarded unchanged** to the target. Each tool call must be answered by
exactly one result, matched on its id, in the result turns that follow it
before the next non-result turn or the end of the request; parallel
results may arrive in any order. A tool result with an empty or absent
id that follows a tool call turn is mispaired the same way, since it
cannot name the call it answers. Among the egresses only Gemini, whose
translation needs each result's call, refuses such a pairing defect
locally (`tool-pairing`) and the chain moves to the next target; when no
other target remains the client gets a **local HTTP 400**
(`invalid_request_error` on the Anthropic envelope, `validation_error` on
the OpenAI one) naming the defect class and the offending message index,
never a tool id or tool content. Separately, the `anthropic-api` egress
refuses a tool result turn from a translating ingress (an OpenAI
`role: tool` message or a Responses `function_call_output`) whose tool-call
id is missing or empty locally (`tool-call-id-missing`), since it builds
that result's block from the id, and the chain likewise moves on. A
`tool_result` block a client sends on `/v1/messages` is relayed as is, so
an empty or absent `tool_use_id` there reaches the upstream. Either way,
when the id-less result follows a tool call turn, the pairing defect is
recorded as well. An
upstream 400 or 422 answering a mispaired transcript is returned as is: it is
not retried, does not fall back, is not repaired, and teaches no learned
capability. The one exception, on completions and streams only: when the
rejection names one change routectl itself made to the request (an
operator payload extra or an auto-placed cache marker), routectl undoes
that change and retries the same target once. routectl does not repair a malformed transcript by
synthesizing or dropping turns.

`GET /v1/models` lists the configured aliases; `GET /health` is the
only route outside the auth layer (so liveness probes work under
`--unsafe-public`).

Each `/v1/models` entry also carries `context_length` -- the id's
resolved target's [`context_window`](#context-window-and-output-ceiling),
the same figure the proactive window gate acts on, so a client and the
router never disagree about a model's window. For a chain alias it is
the FIRST configured target's window. The field is OMITTED when that
target sets no `context_window` (never `null`, never `0`).

**`max_body_bytes`** (u32, default 33554432 -- 32 MiB)

Caps the inbound HTTP body size for `/v1/messages` and
`/v1/chat/completions`. Bodies exceeding the limit are rejected by
axum's `DefaultBodyLimit` layer before routectl parses them; the client
receives HTTP 413 Payload Too Large with a dialect-correct error
envelope. The pre-v0.8 hardcoded ceiling was 4 MiB, which was too tight
for live-traffic sessions carrying large multi-turn or multimodal
payloads. Changing this knob takes effect only after a restart (the
`DefaultBodyLimit` layer is wired at server startup, not per-reload).
A value of `0` is refused at load; omit the key for the 32 MiB default.

```toml
[server]
max_body_bytes = 67108864   # 64 MiB -- raise for multimodal or long-history sessions
```

**`x-routectl-disable-fallbacks` request header**

A client may send `x-routectl-disable-fallbacks: 1` (also accepts `true`
or `yes`) to pin a single request to the first provider in the alias
chain with no fallback. This is useful for testing and per-provider
triage. The header is always honored; no config key turns it off. The
`x-routectl-alias` header (model routing) is separate.

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
rejects an empty chain. A chain may name any `[models]` nickname.

**Header override.** A client may send `x-routectl-alias: <key>`, which
wins over the body's `model` field entirely. `[aliases]` is
hot-reloadable: an edit applies to the next request with no restart.

## Model directory (`[models]`)

Each `[models.X]` entry binds a logical nickname (the table key) to one
transport and one upstream model id. `[aliases]` references entries by
nickname; nothing else in the config does.

```toml
[models.heavy]
provider   = "anthropic-api-key"          # required; a [providers] or
                                          # [pools] key
upstream   = "claude-opus-4-20250514"     # required; the upstream wire id
```

- **`provider`** (string, required) -- the `[providers.X]` entry or
  `[pools.X]` block this model dispatches through. Validated at startup
  against both tables, which share one namespace.
- **`upstream`** (string, required) -- the model id forwarded to the
  upstream verbatim. For Bedrock this is the inference-profile id (e.g.
  `us.anthropic.claude-haiku-4-5-20251001-v1:0`); for OpenAI-shaped
  egresses it is the wire `model` field.

Every `[models]` entry is servable and is listed by `GET /v1/models`. To
keep an entry out while wiring it, delete the block or comment it out.

**Reserved names.** Model nicknames, pool seats, and provider entries
share one runtime-state namespace (circuit breaker, rate limit, probes,
learned capability state), so the rules below apply to every `[models]`
and `[providers]` key, referenced or not. `config check`,
`serve`, and hot reload all refuse a config that breaks any of them:

- `#` is reserved. It separates a pool nickname from its member in a
  seat's state key, so neither a model nickname nor a provider name may
  contain it.
- A model nickname may not equal the name of a DIFFERENT provider entry.
  A model named after its own provider (`[models.openrouter]` with
  `provider = "openrouter"`, the shape `routectl init` writes) is still
  valid.
- A provider name is at most 128 bytes (UTF-8). An oversized one is
  refused; the error states the length without echoing the name.

Everything else on `[models.X]` is a per-model behavior knob -- reasoning
declaration, output caps, header and payload extras --
documented under [Per-model knobs](#per-model-knobs), with the full merge
semantics in the [field-assignment table](#field-assignment-table).
`[models]` is hot-reloadable.

## Field-assignment table

| Field                          | Lives on            | Merge semantics                                                        |
|--------------------------------|---------------------|------------------------------------------------------------------------|
| `provider`                     | `[models.X]`        | required; refs a `[providers]` key                                     |
| `upstream`                     | `[models.X]`        | required                                                               |
| `reasoning_dialect`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `history_reasoning`            | `[models.X]`        | model-only (NO provider fallback)                                      |
| `stream_first_byte_timeout_ms` | `[models.X]`        | model > provider > pool > global > default (`0` on a model is ignored) |
| `context_window`               | `[models.X]`        | Option<u32>, `0` refused; total context window for the window gate and `/v1/models` `context_length` |
| `max_output_tokens`            | `[models.X]`        | Option<u32>, `0` refused; `max_tokens` sent when the client omits it (anthropic-api, bedrock-invoke), else 64000 |
| `pricing`                      | `[models.X]`        | Option<table>, per-Mtok prices, each field optional; non-finite or negative refused; per-field precedence in [Model prices](#model-prices-modelsxpricing) |
| `header_extras`                | BOTH                | model wins on key collision; `anthropic-beta` comma-unions (see below) |
| `payload_extras`               | BOTH                | deep recursive merge; model wins on leaf collision                     |
| `format`, `base_url` or `cloud`, `credential`, `auth` | `[providers.X]` | provider-only; required (see [Provider entries](#provider-entries-providersx)) |
| `account_id_ref`, `cloud_project_id` | `[providers.X]` | provider-only companions; one format each (see [Provider entries](#provider-entries-providersx)) |
| `anthropic_version`            | `[providers.X]`     | provider-only, `anthropic-messages` only; default `2023-06-01`         |
| `user_agent`                   | `[providers.X]`     | provider-only                                                          |
| `runtime` (RPM, breaker, timeouts) | `[providers.X]`, `[pools.X]` | provider > pool; the two timeouts then fall back to `[retry]`          |
| `max_body_bytes`               | `[server]`                      | u32 bytes, default 33554432 (32 MiB); caps inbound body size; HTTP 413 on excess; restart required |
| `auto_emit_top_level_breakpoint` | `[cache]` global              | bool, default true; master switch for dispatch-path auto-cache (see `[cache]`)                    |
| `normalize_tools`              | `[cache]` global                | bool, default true; stable-sorts the tool array on the OAuth Anthropic path for cache stability (see `[cache]`) |
| `k_gated_emission`             | `[cache]` global                | bool, default **false**; when true, withholds auto-emitted markers on a session whose measured reuse sits below the marker's break-even (see `[cache]`) |
| `cache_capability`             | `[providers.X]`                 | see [Per-provider capability](#per-provider-capability-cache_capability) |
| `enabled`                      | `[reduction]` global            | bool, default true; master switch for dispatch-path context reduction (see `[reduction]`)         |
| `reduction_enabled`            | `[providers.X]`                 | `Option<bool>`, default None (inherits global); `false` disables reduction for this provider      |
| `enabled`                      | `[window_gate]` global          | bool, default false; master switch for the proactive context-window gate (see `[window_gate]`)    |
| `enabled`                      | `[capability]` global           | bool, default true; master switch for the learned-capability subsystem (see `[capability]`)       |
| `decay_hours`                  | `[capability]` global           | u64, default 48; hours a learned negative acts before a re-probe (see `[capability]`)             |
| `inferred_window_hours`        | `[capability]` global           | u64, default 1; window a pending inferred signal waits for confirmation (see `[capability]`)      |

**`base_url` scheme requirement.** `base_url` must use `https://` (or
`http://` for loopback hosts only: an IPv4 loopback literal such as
`127.0.0.1`, `[::1]`, an IPv4-mapped loopback such as `[::ffff:127.0.0.1]`,
or the exact name `localhost`). Any other DNS name -- even one beginning
with `127.` -- requires `https://`. Link-local addresses
(IPv4 `169.254.0.0/16` and IPv6 `fe80::/10`) are rejected at provider
build time regardless of scheme to prevent SSRF and cloud-metadata
credential leaks. The startup error names the offending address.

**Outbound proxies.** Provider traffic to a non-loopback base URL honors the
standard proxy environment (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
`NO_PROXY`, and their lower-case forms). Traffic to a loopback base URL --
the same hosts the cleartext exemption above accepts -- is always sent
directly, over `http://` and `https://` alike: it carries the provider
credential, so it must not leave the machine.

## header_extras merge

The router's `apply_layered_overlays` helper composes provider and
model `header_extras` per request:

1. Clone provider's `header_extras` into a working map.
2. Iterate model's `header_extras`:
   - auth-reserved keys drop (see "Reserved-header buckets" below). The
     config advisory `auth-reserved header warning` names the model and
     header once at `serve` startup and once per accepted config reload;
     each request logs the drop only at DEBUG
   - managed-reserved keys DEBUG + drop
   - every other key model-wins on collision (last-writer-wins)
3. List-valued post-pass on `LIST_VALUED_HEADERS` (`anthropic-beta`):
   comma-split + union + dedup + comma-rejoin in visit order
   `req.anthropic_beta -> provider value -> model value`. The unioned
   string lands back on the merged map AND on `req.anthropic_beta`, so
   downstream readers (the Anthropic-API egress's wire header, the
   Bedrock egresses' `anthropic_beta` array) see the same fully-composed
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

### Client extras follow the client's dialect

The swept client fields are spelled for the client's own wire, so an
egress forwards them only when the client speaks that egress's
vocabulary:

| Egress | Forwards client extras from |
|---|---|
| `anthropic-api` (Mantle's Anthropic lane included), Bedrock Invoke, the Bedrock Converse bag | the Anthropic Messages ingress |
| `openai-compat` (Mantle's OpenAI-compatible lane included) | the OpenAI Chat Completions ingress |
| `openai-responses` | its own fixed allowlist of keys, whatever the ingress; `prompt_cache_options` and `prompt_cache_retention` only from the Responses ingress, and on the AWS `cloud` only as a captured value (`prompt_cache_retention` `"in_memory"` or `"24h"`, `prompt_cache_options` `{"mode": "explicit"}` with an optional `"ttl": "30m"`), any other value withheld whole. From the Responses ingress, an entry whose `base_url` is not on the ChatGPT Codex host also forwards any other key except one routectl writes itself; the Codex host and the AWS `cloud` withhold every other key and count the request once as `unknown_member_withheld`. Any other key from another ingress counts the request once as `foreign_dialect_extra` |
| `gemini` | no ingress: only the operator layer |

A library caller that builds its own `ChatRequest` keeps forwarding its
`provider_extras` to every egress; Responses treats it as it treats the
Responses ingress. For any other request, an egress keeps only the
keys it handles itself (a key it manages, which its merge drops as
before, or one it translates, such as `output_config` on openai-compat)
and the operator's `payload_extras` with the operator's value. Every
other client extra is dropped, and a request that dropped any is counted
once as the `foreign_dialect_extra` drop class on that lane, with the
sanitized key names (never the values) at `debug`. On openai-compat a
`metadata` object from a client of another dialect is the exception: it
is the client identity block, and counts as `client_fingerprint_stripped`
instead.

The same rule covers the members a client puts below the top level that
routectl does not model: on a message, a content part, a custom tool or a
system block, and on a Responses `function_call` or `reasoning` input
item. For a client of the egress's own dialect (or a library caller)
they are forwarded on the node they came on, beside the typed members,
which win on a name clash. Every other egress has no slot for them: on
`anthropic-api`, Bedrock Invoke, Bedrock Converse, `openai-compat`,
`openai-responses` and `gemini` they are dropped, and a request that
dropped any counts once as `foreign_dialect_extra` on that lane (on
every lane but `gemini`, one count with any top-level key it dropped;
on `gemini` the top-level keys count as their own `ingress_extra_withheld`
policy action). Each dropped top-level key and member is declared
removed at its own path. A Responses client's members, a function tool's
included, are withheld in two more cases, counted once per request as
`unknown_member_withheld` instead: on an `openai-responses` entry over the
AWS `cloud`, which forwards no carried member, and, on any destination,
the members of
a `function_call` or `reasoning` item that does not ship with the
identity the client gave it.

The operator layer is never affected: a key set in `payload_extras` is
sent whatever the client's dialect. To send a field a client of another
dialect cannot reach the upstream with (a Chat client's `top_k` to
Claude, say), set it in `payload_extras`.

Gemini forwards the operator layer alone for all ingress traffic, so a
swept client value never reaches it, even underneath an operator value
at the same key. See the
[Gemini `payload_extras`](#gemini-gemini-gemini-cloud-code) entry.

Whatever the source, openai-compat never sends `mcp_servers`: its
entries can carry a token issued for a remote MCP server, so the field
is removed from the finished body, operator `payload_extras` included,
and counted once per request as the `credential_bearing_field_withheld`
policy action. Bedrock drops it the same way (see
[Bedrock body fields](#bedrock-body-fields)). Gemini, by contrast, still
sends an `mcp_servers` the operator put in its `payload_extras`.

On an openai-compat streaming request, `stream_options.include_usage` is
set to `true` after the merge, so the upstream ends the stream with the
usage chunk routectl's accounting reads. A client value other than
`true` (or a `stream_options` that is not an object) is overridden and
counted once per request as the `client_stream_include_usage_overridden`
policy action. Only an operator `payload_extras` value for
`stream_options.include_usage` is sent as configured.

## Reserved-header buckets

| Bucket                | Keys                                                | Action on `header_extras` entry             |
|-----------------------|-----------------------------------------------------|---------------------------------------------|
| `AUTH_HEADERS`        | `authorization`, `x-api-key`, `anthropic-version`, `chatgpt-account-id`, any `x-amz-*` | drop; see below for which line logs it |
| `MANAGED_HEADERS`     | `host`, `content-type`, `content-length`            | DEBUG + drop (wire-shape protection)        |
| `LIST_VALUED_HEADERS` | `anthropic-beta`                                    | comma-split-union-rejoin across all sources |

`anthropic-beta` is NOT in `MANAGED` (pre-v0.6 it was). Operators set
per-provider and per-model values via `header_extras`; the list-valued
post-pass unions them with the ingress lift.

Auth secrets belong in `credential` / `auth` on the provider, not
`header_extras`. An auth-reserved key never reaches the wire, but where
it is configured decides how the drop is logged:

- `[models.X] header_extras` with `authorization`, `x-api-key` or
  `anthropic-version`: the dispatch merge drops it. The
  `auth-reserved header warning` advisory names it once at `serve`
  startup and once per accepted config reload, and each request logs
  the drop only at DEBUG.
- `[providers.X] header_extras` with any auth-reserved key, and
  `[models.X] header_extras` with `chatgpt-account-id` or an `x-amz-*`
  key: the dispatch merge keeps it, no startup advisory names it, and
  the egress drops it with a WARN on every request (`ignoring
  auth-reserved header from header_extras`). The `x-amz-` prefix is
  reserved because the Bedrock SigV4 signature covers those headers.

## Worked example: three-source `anthropic-beta` compose

claude-code sends `betas: ["foo"]` as an SDK option. The TypeScript
SDK translates that to the `anthropic-beta: foo` HTTP header. The
Anthropic ingress lifts the header into `req.anthropic_beta = ["foo"]`
(unchanged in v0.6.0). Then:

```toml
[providers.anthropic-default]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ROUTECTL_ANTHROPIC"
auth = "bearer"
header_extras = { "anthropic-beta" = "claude-code-20250219,oauth-2025-04-20" }

[models.anthropic-opus]
provider = "anthropic-default"
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
HTTP header. Bedrock egresses carry the same canonical list in the body's
`anthropic_beta` array, minus any flag the router withholds from the
lane (see below).

## Bedrock and Anthropic API beta-flag controls

routectl keeps no allowlist of `anthropic_beta` flags or body fields: a
flag or field Anthropic adds reaches the upstream without a routectl
release or a config change. What still shapes the wire is the operator
floor, the flags the router withholds from a Bedrock lane, the two
unconditional Bedrock body-field drops, and the capability betas a
request's own body implies. Config version 5 retired the earlier
`[bedrock]` and `[providers.X]` allowlists; see
[Version 5 retires the capability lists](#version-5-retires-the-capability-lists).

### Bedrock: which client betas reach AWS

Every client flag reaches AWS except the ones the router decides that
lane must not send, which the Bedrock egress drops. Per lane and flag,
strongest first:

1. **The operator floor sends.** Any flag
   [pinned](#pinning-a-bedrock-beta-floor) through provider or model
   `header_extras["anthropic-beta"]` is always sent and never withheld.
2. **A non-Anthropic model on Converse withholds every client flag.** On
   `format = "bedrock-converse"`, a model whose upstream id is provably not
   Anthropic (`openai.*`, `amazon.nova-*`, `meta.llama*`, ..., with or
   without a region prefix, or a `foundation-model/` ARN embedding one)
   refuses the `anthropic_beta` field itself, so no client flag is sent
   to it. An inference-profile ARN proves no vendor and falls through to
   the steps below.
3. **A capability override decides.** An entry on the flag's
   `beta:<flag>` key under
   [`[capability.overrides]`](#capabilityoverrides-is-the-durable-decision-surface)
   sends it (`force_supported`) or withholds it (`unsupported`). A beta
   flag never routes a target away; it is only withheld.
4. **A learned beta verdict decides** (only with `[capability] enabled`).
   A flag the lane rejected is withheld; once its negative lapses it is
   sent once as a re-probe, and an accepted re-probe records a positive
   that sends it from then on.
5. **The shipped seed withholds.** Three flags AWS rejects on every
   request (`advanced-tool-use-2025-11-20`, `advisor-tool-2026-03-01`,
   `prompt-caching-scope-2026-01-05`) are withheld on every `bedrock`
   lane, even with `[capability] enabled = false`, unless an operator
   lifted the seed for that lane (below).

A request on a forwarded credential withholds nothing: the client owns
that credential and its beta choices. Each withheld flag is logged at
`debug` on both `bedrock-invoke` and `bedrock-converse`, and each carrier
counts the withhold once per request as the `anthropic_beta_rejected_by_bedrock`
drop class.

For a client flag the router let through, the router repairs the
upstream's rejection itself, on both carriers and for streaming,
non-streaming, and token-count calls. Bedrock answers with a
`ValidationException` naming the flags (``Unexpected value(s) `<flag>`,
... for the `anthropic-beta` header. ...``, which Converse prefixes with
`The model returned the following errors: `). When it does, the router's
learning engine re-sends the request once on the same target without
exactly those flags. The repair applies only to that exact message on a
400, and only when every named flag came from the client: it never
removes an operator-floor flag (including a `header_extras`-pinned one),
skips a rejection that names no flags (`invalid beta flag`), skips one
naming a flag the request's own body requires (the structured-outputs or
`thinking.display` `"updates"` beta, which the egress would re-add),
skips a flag already withheld, and does nothing on a forwarded
credential. It is one of the at most five repairs a request may draw,
shared with field repair, and a target repairs a beta
rejection once. The re-send is logged at `debug`, with flag names only.
The same repair runs on an `anthropic-api` provider, Mantle's Anthropic
lane included, where the rejection arrives as an `invalid_request_error`
with the same message; see
[Anthropic API: client betas pass through](#anthropic-api-client-betas-pass-through).

**Learned beta verdicts.** Once an attempt sent without the repaired flags
succeeds, the router records each of them as a learned negative on that
lane (`<provider>#<upstream>`, capability `beta:<flag>`), so later
requests withhold it without another 400. These are ordinary learned
capability facts: they are written to the capability ledger and survive
a restart, decay on the `[capability] decay_hours` window (48 hours by
default) and then revalidate with one re-probe, show in `routectl doctor`
as `beta:<flag>` cells with action `withhold`, and are dropped with
`routectl capability purge <provider#upstream> beta:<flag>`. A lane
holds at most 32 learned beta entries; a new one past that evicts the
lane's most recently learned one, so established entries outlast a burst
of new flags. Nothing is learned from a token-count repair, a
forwarded-credential request, a flag under a `force_supported` override,
or with `[capability] enabled = false`. The live wire check reports a
rejection the repair answered as `repaired_lossy` once the repaired
attempt succeeds (the client asked for the flag), and as unrepaired
otherwise.

**Lifting the seed on one lane.** If your account accepts a seeded flag,
`routectl capability purge <provider#upstream> beta:<flag>` lifts the
seed for that lane: the daemon commits a `cleared` row to its capability
ledger, and the flag is sent from then on, across restarts. The lift is
accepted only for a lane some configured model routes to, and it lasts
until the upstream rejects the flag again on that lane (the rejection is
learned as usual and the seed's withhold returns with it) or the lane's
provider entry is removed or moves to a `format` with another family
token (see [`format`](#format)).
To send a flag permanently, use a `force_supported` override on
`beta:<flag>` instead.

**Library consumers.** The seed and the learned verdicts live in the
router, which hands each request its withheld set. Code that calls the
Bedrock provider directly, without the router, gets no withheld set and
so no longer has the seed applied, and no beta rejection is repaired:
the repair belongs to the router too. Such code must drop rejected
flags itself (or route through the router).

### Bedrock body fields

Every body field reaches Bedrock -- the assembled Invoke body and the
Converse `additionalModelRequestFields` bag alike, including the
quarterly-added Anthropic fields routectl's forward-compat sweep forwards
without modelling them -- except two, which are always dropped with no
operator knob:

- **`mcp_servers`** never ships. Bedrock rejects it on both carriers,
  and its entries can carry a connector credential meant for a remote MCP
  server. It is removed as the last mutation of each carrier's body,
  whichever source supplied it.
- **An orphan `tool_choice`** -- one left with no non-empty `tools` list,
  a pairing Anthropic rejects -- is removed from the Invoke body that
  ships.

Both drops log at `debug`, by field name only. Two more rules decide
what reaches the bag or body:

- **Client fields follow the client's dialect.** A swept client field
  reaches Invoke and the Converse bag only from an Anthropic Messages
  client (or a library caller); one a Chat or Responses client sent is
  dropped and counted as `foreign_dialect_extra` (see
  [Client extras follow the client's dialect](#client-extras-follow-the-clients-dialect)).
  On Converse, `output_config` is a key routectl composes, so a client's
  `output_config` object is not merged whole: its `format` member is
  merged into the composed `output_config`, beside any effort, and wins
  over a `response_format` conversion.
- **A non-Claude model on Converse gets no `output_config`.** For a
  model whose upstream id is provably not Anthropic (see
  [which client betas reach AWS](#bedrock-which-client-betas-reach-aws)),
  the composed `output_config` (its effort and its structured-output
  format alike) is omitted from the bag, because such a model refuses the
  key, and the request counts once as the `output_config_unrepresentable`
  drop class. The structured-outputs beta is then not added either. To
  keep structured-output requests away from such a model, list
  `structured_output` under `unsupported` in
  [`[capability.overrides]`](#capabilityoverrides-is-the-durable-decision-surface).

An operator body field for Bedrock goes in the provider or model
`payload_extras`: it merges onto the top level of the Invoke body and
into the Converse bag, except a key routectl writes itself. On Invoke
that refused set is every field the `anthropic-api` egress manages, and
the body's `anthropic_version` is always `bedrock-2023-05-31`. On
Converse it is the canonical request keys plus the bag keys `thinking`
and `output_config` and the Converse body fields `inferenceConfig`,
`toolConfig` and `additionalModelResponseFieldPaths`.

The client's `metadata` block (its `user_id` / `account_uuid`) never
reaches AWS on Bedrock Invoke, Bedrock Converse, or the
[Bedrock mantle Anthropic lane](#bedrock-mantle-anthropic-lane).
The operator's own `metadata` is sent in its place: the provider or
model `payload_extras` value. Of that value,
only what the request still carries when it is sent ships: a member a
retry removed or reverted stays out. On Converse a request whose
client `metadata` differs from that operator value counts once as the
`client_fingerprint_stripped` policy action.

A field the account's
Bedrock schema has not caught up with is answered with a 400 naming it
(`Extra inputs are not permitted`); route requests that carry it
elsewhere, or have the client stop sending it.

### Pinning a Bedrock beta floor

A flag in a Bedrock provider's or model's
`header_extras["anthropic-beta"]` (comma-separated) is always sent on
requests destined for that provider or model, regardless of what the
caller sent. It rides where Bedrock reads betas: the top-level
`anthropic_beta` array on `bedrock-invoke`, the
`additionalModelRequestFields` bag on `bedrock-converse`. The pin is unioned
after the client's flags, each flag appears once on the wire (a client
flag repeating a pinned flag is not sent twice), and no withholding or
repair ever removes a pinned flag.

Use this to guarantee a required beta flag is always present (for
example, a model that requires `computer-use-2025-01-24` to operate
correctly).

```toml
[providers.bedrock-computer]
format        = "bedrock-invoke"
cloud         = { name = "aws", region = "us-west-2" }
credential    = "aws://chain"
auth          = "aws-sigv4"
header_extras = { "anthropic-beta" = "computer-use-2025-01-24" }
```

### Anthropic API: client betas pass through

An `anthropic-api` provider forwards every client `anthropic-beta` flag to
the upstream except the ones the router withholds from the lane: an
`unsupported` override on the flag's `beta:<flag>` key under
[`[capability.overrides]`](#capabilityoverrides-is-the-durable-decision-surface)
(a `force_supported` one sends it), and a learned or seeded `beta:` verdict
where the lane has one (only Bedrock lanes carry the shipped seed; a
`beta:` negative is learned on any lane whose rejection names the client
flags, see below). The
floor is never withheld: the operator floor (provider and model
`header_extras["anthropic-beta"]`) always ships, as do the own-OAuth flags
below and the [capability betas](#capability-betas-are-always-unioned); a
client flag the floor also carries keeps its client position. A request on
a forwarded credential withholds nothing. Each withheld flag is logged at
`debug` (see [LOGGING.md](LOGGING.md)).

A `beta:<flag>` override matches the client flag case-insensitively
(ASCII): `beta:context-1m-2025-08-07` withholds a client
`Context-1M-2025-08-07`, and an `unsupported` / `force_supported` pair that
differs only in the flag's case is a contradictory override and fails
validation. Matching never rewrites the flag: a forwarded flag keeps the
client's spelling, and learned and seeded `beta:` verdicts stay
case-sensitive.

Because the floor is never withheld, an `unsupported` override on a flag
the egress re-adds has no effect wherever that re-add fires, and `routectl
config check` warns about it, naming the spec, the flag and every reason it
ships. The comparison ignores ASCII case, and only a re-add the cell's
targets can reach is reported:

- The provider's `header_extras["anthropic-beta"]` pin, on every
  `anthropic-api` provider (a forwarded one included).
- A model's own `header_extras["anthropic-beta"]` pin. A
  `"<provider>:<nickname>"` cell checks that model; a provider cell checks
  every model that dispatches to the provider, directly or through a
  `[pools]` block, except a model whose own `"<provider>:<nickname>"` cell
  also names the flag (that cell decides it).
- `structured-outputs-2025-12-15`, re-added whenever the request carries
  `output_config.format`; not on a `credential = "forwarded"` provider,
  whose client betas pass through verbatim.
- On an `auth = "bearer"` provider whose `base_url` host is
  `api.anthropic.com` and whose credential is its own (not `forwarded`):
  `oauth-2025-04-20` (OAuth gate, always).
- On a provider presenting the Claude Code identity (a managed
  `oauth://anthropic` login on `api.anthropic.com`, see
  [Provider entries](#rules-an-entry-must-meet)) only:
  `effort-2025-11-24` (re-added whenever the request carries
  `output_config.effort`) and the Claude Code floor listed below (re-added
  on requests routectl classifies as non-Claude-Code -- normally those
  without a Claude Code session header -- or on every request under
  `cloak_mode = "always"`; never under `cloak_mode = "never"`).

  An `x-api-key` provider or one on another host is not warned about
  these, and there the override does withhold the flag. A forwarded
  provider is not warned either, but there no `beta:` override withholds
  anything: the client's beta set is forwarded as sent.

Two removals run after every re-add and are reflected in the warning:
`context-management-2025-06-27` is never reported on a provider with
`context_management = true`, which strips it from the header whatever
supplied it; and `redact-thinking-2026-02-12` is reported with the exception
"unless the request carries `thinking.display`" (except on a forwarded
provider, where no removal runs). The warning is advisory: the config still
loads and the exit code is unchanged.

When an `anthropic-api` upstream, Mantle's Anthropic lane included,
answers a 400 `invalid_request_error` whose message is ``Unexpected
value(s) `<flag>`, ... for the `anthropic-beta` header. ...``, the router
repairs and learns it exactly as on Bedrock: one re-send without the
named client flags, never a floor or body-implied flag, then a learned
`beta:<flag>` negative once an attempt without them succeeds (see
[which client betas reach AWS](#bedrock-which-client-betas-reach-aws)
for the full rules).

At ingress, client flags from the body and the `anthropic-beta` header are
split on commas, trimmed and deduplicated; a flag holding any byte outside
visible ASCII is dropped, and a request with more than 64 distinct flags is
rejected with 400. A request that drops any flag logs exactly one warning,
carrying the number of dropped flags and the length of the longest one, never
the flag values; this holds when the same request is also rejected for the cap.

On a provider presenting the Claude Code identity, routectl also injects,
on requests it classifies as non-Claude-Code, a 9-flag model-agnostic floor
(`default_claude_code_anthropic_betas()`: `claude-code-20250219`,
`oauth-2025-04-20`, `interleaved-thinking-2025-05-14`,
`context-management-2025-06-27`, `prompt-caching-scope-2026-01-05`,
`structured-outputs-2025-12-15`, `fast-mode-2026-02-01`,
`redact-thinking-2026-02-12`, `token-efficient-tools-2026-03-28`); those
nine are operator-equivalent pins.
`redact-thinking-2026-02-12` is removed again, whatever supplied it, when the
request body carries `thinking.display`: Anthropic lets the redaction win over
the display mode, so the two never ship together (except on a forwarded
credential, whose client betas pass through verbatim).

The floor carries ONLY model-agnostic flags. Model-gated ones
(`context-1m-2025-08-07`, `effort-2025-11-24`,
`thinking-token-count-2026-05-13`, `mid-conversation-system-2026-04-07`,
`advisor-tool-2026-03-01`) are NOT injected, because forcing them 400s models
that do not support them. They travel as ordinary client-driven flags, sent
when the caller sends them.

### Capability betas are always unioned

Two flags are force-added whenever the request's ASSEMBLED body carries
the field that flag gates, after every other beta step, so nothing that
runs earlier can remove them:

| Body field | Beta force-added | Lanes |
| --- | --- | --- |
| `output_config.format` (structured outputs) | `structured-outputs-2025-12-15` | `anthropic-api` (`anthropic-beta` header) and Bedrock (body `anthropic_beta` array) |
| `output_config.effort` (adaptive thinking) | `effort-2025-11-24` | a provider presenting the Claude Code identity only |

Neither is a client-opted beta: each is a routectl-derived capability
signal implied by the feature the request is already using. A 2026-08-11
live capture on `api.anthropic.com` (one lane, one seat, one model)
accepted `output_config.format` both with and without
`structured-outputs-2025-12-15`, and a 2026-10-21 measurement on Bedrock
Invoke and Converse (one model) accepted both `output_config.format` and
`output_config.effort` without either flag. The structured-outputs union
is therefore retained as belt-and-braces rather than a proven hard
requirement, and Bedrock gains no effort beta; the `output_config.effort`
case on `api.anthropic.com` remains unmeasured. Dropping either flag is
therefore not a reliable way to constrain a request -- an older account or
model tier may still gate the field.

Both carve-outs are ONE-WAY -- the body's field adds the flag; a
caller-supplied flag with no matching field is passed through untouched
(routectl never manufactures the field to match a flag).

An operator who wants to deny either feature should deny the FEATURE --
list its capability key (`structured_output` for `output_config.format`)
under `unsupported` in
[`[capability.overrides]`](#capabilityoverrides-is-the-durable-decision-surface)
so requests using it are never routed to that target -- rather than
trying to withhold the beta.

### OAuth lane: `temperature` and `top_p` are dropped

Anthropic's OAuth seat rejects a `/v1/messages` body carrying
`temperature` or `top_p`. On an `auth = "bearer"` provider whose
`base_url` host is `api.anthropic.com` (excluding the forwarded /
pure-proxy leg), routectl removes both from the outbound body and logs one
structured `WARN` per affected request naming the dropped keys.
`stop_sequences` is unaffected.

This gate is the LANE, not the client identity or the cloak setting: **an
`env://` bearer, or `cloak_mode = "never"`, on such a provider STILL drops
these params.** That is intended -- the 400 is
a property of the credential, not of the disguise, so honouring the knob
would mean failing the request instead. If you need `temperature` or
`top_p` honoured, route the request to an `x-api-key` provider or a
non-Anthropic host.

## Bedrock formats (`bedrock-invoke`, `bedrock-converse`)

A native Bedrock provider picks its wire shape with `format`, and always
reaches AWS through `cloud`:

- `"bedrock-invoke"` -- vendor-specific InvokeModel body (the Anthropic
  Messages payload). Every model routed to it must be an Anthropic-family
  model; an upstream id that provably names another vendor is refused at
  load (an inference-profile ARN proves nothing and passes).
- `"bedrock-converse"` -- vendor-neutral Converse API.

```toml
[providers.bedrock]
format     = "bedrock-converse"   # or "bedrock-invoke"
cloud      = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth       = "aws-sigv4"
```

The credential is `aws://profile/<name>` or `aws://chain` with
`auth = "aws-sigv4"`, or a Bedrock API key as an `env://` / `file://`
reference with `auth = "bearer"`. Both formats share the `bedrock`
family token. Both shapes are wired for Anthropic models on Bedrock; see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#bedrock-any-region) for the
adaptive-thinking interaction.

### AWS region values

Every `cloud = { name = "aws", region = "..." }` entry, native Bedrock
and the managed surface alike, builds its endpoint host from the region and
signs under it, so the value must be a canonical AWS region identifier:
lowercase `a-z`, `0-9` and `-` only, at most 32 bytes, in the commercial
(`us-west-2`, `eu-central-1`), GovCloud (`us-gov-west-1`) or China
(`cn-north-1`) partition. Anything else -- empty, whitespace, uppercase,
a dot, or a URL character such as `@`, `/`, `:`, `#` or `?` -- is REJECTED
at config load, and a library caller that constructs a provider directly
with such a region gets a config error before any credential is resolved,
any request is signed, or any connection is opened. The host follows the
partition's DNS suffix: `bedrock-runtime.<region>.amazonaws.com`
(`.amazonaws.com.cn` in China) and `bedrock-mantle.<region>.api.aws`
(`.api.amazonwebservices.com.cn` in China).

## Bedrock mantle Anthropic lane

An `anthropic-messages` provider reaches AWS Bedrock's managed mantle
endpoint (Anthropic Messages vocabulary, AWS-authenticated) by setting
`cloud = { name = "aws", region = "..." }` instead of `base_url`. The
`cloud` key selects the lane. The provider speaks the ordinary Anthropic
Messages API (`complete`, `stream`, `count_tokens`), but every request is
SigV4/bearer-signed under the `bedrock-mantle` service scope and NO
`x-api-key` is sent.

```toml
[providers.bedrock-mantle]
format     = "anthropic-messages"
cloud      = { name = "aws", region = "us-east-1" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth       = "bearer"

[models.claude-mantle]
provider = "bedrock-mantle"
upstream = "claude-haiku-4-5-20251001-v1:0"   # bare model id, no "us." prefix
```

The keys, in order:

- `format = "anthropic-messages"`.
- `cloud = { name = "aws", region = "<region>" }` (a canonical AWS region,
  see [AWS region values](#aws-region-values)) -- the SINGLE source of
  truth for the endpoint: the factory derives both the host
  (`https://bedrock-mantle.<region>.api.aws/anthropic`) and the SigV4
  signing scope from it. There is no `base_url` on a cloud entry.
- `credential` and `auth`, one of:
  - a long-term Bedrock API key, `credential = "env://VAR"` (or
    `file://`) with `auth = "bearer"`, sent as `Authorization: Bearer`;
  - `credential = "aws://profile/<name>"` with `auth = "aws-sigv4"` -- a
    named profile from the shared AWS config, SigV4-signed;
  - `credential = "aws://chain"` with `auth = "aws-sigv4"` -- the AWS
    default credential chain (environment, profile, SSO, container and
    instance roles), SigV4-signed.

Validation (enforced on build, reload, and `config check`) refuses any
other pairing: `x-api-key`, an `oauth://` login (managed logins are
contained to their vendor's first-party host, so no subscription token
and no Claude Code identity ever reaches AWS), `forwarded`, an `aws://`
credential without `aws-sigv4` or the reverse, and a region that is not a
canonical AWS region.

The mantle lane uses a no-redirect client: any 3xx from the upstream is a
fault to surface, never followed (auto-following a signed POST would
replay the SigV4 signature cross-host). AWS-shaped error envelopes
(`SignatureDoesNotMatch`, `ThrottlingException`, `RequestTimeTooSkewed`)
are lifted into the classified failure so a bad credential surfaces as an
auth failure and a throttle as rate-limited. A `base_url` Anthropic entry
uses the same no-redirect client: `x-api-key`
and the Claude-Code identity headers are not covered by reqwest's
default cross-host header strip list, so a 3xx from the configured host
is likewise surfaced as a fault rather than chased to a different host.
Every credentialed egress lane in routectl (Anthropic, OpenAI-compat,
OpenAI Responses, Gemini, native Bedrock) shares this posture.

Production credential guidance: use a SigV4 source (`aws://profile/<name>`
backed by long-term keys or a credential process, or `aws://chain`) or a
long-term Bedrock API key. Short-term keys (a Bedrock API key from the
console, or STS session credentials) expire and are DEV-ONLY -- the lane
does not refresh them, so they are unsuitable for a long-running daemon.

The `cloud` carrier requires the `bedrock` build feature. A binary built
without it refuses a `cloud` entry at config load, naming the provider.

The client's `metadata` block never reaches AWS on this lane; the
operator's own `payload_extras` `metadata`, if any, is sent in its place
(see [Bedrock body fields](#bedrock-body-fields) for the rule shared with
Invoke and Converse).

Field repair on this lane acts only on the rows a refusal captured on
Mantle grounds: the `max_tokens` ceiling. Its thinking and effort rows are
not admitted, and `presence_penalty` / `frequency_penalty` never reach the
wire here, since the Anthropic egress drops both before sending. Some
models refuse `thinking.type: enabled` on Mantle that accept it on the
direct Anthropic endpoint; since that refusal is not repaired here, the
client must send adaptive thinking to such a model (routectl forwards the
client's `thinking` object as sent). Learned
field-repair history is keyed on the provider entry name and upstream
model, not on the region or host, so a `region` change, or switching an
entry between `base_url` and `cloud` under the same name, keeps that
history exactly like a `base_url` repoint; decay re-checks each verdict.

## Bedrock mantle OpenAI lanes

The same `cloud = { name = "aws", region = "..." }` key reaches the mantle
endpoint's OpenAI vocabularies from an `openai-responses` or `openai-chat`
provider. As on the Anthropic lane, the `cloud` key selects the lane:
every request is SigV4/bearer-signed under the `bedrock-mantle` service
scope, NO first-party `Authorization:
Bearer` is sent, and the client follows no redirects. `region` is the
single source of truth -- the factory derives the endpoint host
(`https://bedrock-mantle.<region>.api.aws/openai/v1`) and the SigV4 scope
from it. `credential` and `auth` take the same three pairings documented
for the Anthropic lane above (a Bedrock API key with `bearer`,
`aws://profile/<name>` or `aws://chain` with `aws-sigv4`).

### `openai-responses` mantle lane

```toml
[providers.mantle-responses]
format     = "openai-responses"
cloud      = { name = "aws", region = "us-east-1" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth       = "bearer"

[models.gpt-oss-mantle]
provider = "mantle-responses"
upstream = "openai.gpt-oss-120b"   # bare model id
```

The Responses lane persists nothing: `store` is forced `false` on every
request (and the `reasoning.encrypted_content` include is forced on so
reasoning survives across turns without server-side storage). A `store`
key in `payload_extras` is REJECTED at config load -- the flag is not a
knob on this lane.

Mantle fetches URLs and file references it finds in a request, so this
lane forwards only request parts routectl has reviewed. Function tools
are forwarded, flat, with only their typed members (`type`, `name`,
`description`, `parameters`, `strict`); a function declared inside a
namespace container or an `additional_tools` input item is forwarded the
same way in `tools`, without the container or the item. Every other
tool (hosted tools such as `web_search`, freeform `type:"custom"` tools,
MCP and unknown tool types) is withheld. Of the input items routectl does
not model, only `agent_message`, `local_shell_call`, `compaction`,
`compaction_summary` and `context_compaction` are forwarded, and only
when every member they carry has the reviewed name and shape; every other
such item (for example `custom_tool_call`, `custom_tool_call_output`,
`web_search_call` or `additional_tools`) is withheld. Members a client
carried on a message, content part, function call or reasoning item are
withheld too. Each withheld tool, item or member is declared as a removal
and the request counts once as the `unknown_member_withheld` drop class
(see [LOGGING.md](LOGGING.md)). A `base_url` Responses entry (the OpenAI
API or ChatGPT Codex) forwards all of these as the client sent them,
except a `cache_control` member on a non-function tool, which every
Responses lane removes.

Validation (build, reload, `config check`) rejects an incoherent entry:

- a credential and `auth` outside the three pairings above -- REJECTED.
- a set `account_id_ref` -- REJECTED (a ChatGPT-account id has no meaning
  on the mantle lane).
- a `region` that is not a canonical AWS region identifier (including an
  empty or whitespace-only value) -- REJECTED.
- a `store` key in `payload_extras` -- REJECTED (see above).

### `openai-chat` mantle lane

```toml
[providers.mantle-compat]
format     = "openai-chat"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://chain"
auth       = "aws-sigv4"

[models.gpt-oss-compat]
provider = "mantle-compat"
upstream = "openai.gpt-oss-20b"   # bare model id
```

This example uses the default `openai` reasoning dialect, so the
[OpenAI chat-lane rule](#openai-chat-lane-rule-reasoning_dialect) applies:
`openai.gpt-oss-20b` matches it, so its sampling params are dropped and
`max_tokens` goes out as `max_completion_tokens`. The rule is consulted only
by the `openai` and `deepseek` dialects, and the `deepseek` dialect keeps
`max_tokens`.

Learned field repair does not act on an `openai-chat` provider whose
`base_url` names a loopback host; that refusal is lexical, so an `https://`
name that resolves to a loopback address is treated as remote.

Validation rejects a credential and `auth` outside the three pairings
above and a `region` that is not a canonical AWS region identifier
(including an empty or whitespace-only value).

`count_tokens` on the mantle `openai-chat` lane returns a deterministic 501
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
source (`aws://profile/<name>` or `aws://chain`) or a long-term Bedrock
API key. Short-term credentials (a console Bedrock API key or STS session
credentials) expire and are DEV-ONLY -- the lane does not refresh them.

Both lanes require the `bedrock` build feature (the Responses lane also
needs `openai-responses`); a binary built without it refuses a `cloud`
entry on these formats at config load.

Field repair on these lanes acts only on the rows a refusal captured on
that lane grounds. The Chat Completions lane drops `top_p`,
`presence_penalty` and `frequency_penalty` when the model refuses them.
`temperature` is not admitted there: the model that refuses it answers
with the `unsupported_value` code, which the field repair does not act on
for `openai-compat`, so that refusal reaches the client. Top-level
`reasoning_effort` is not a repairable field on that lane (its refusal
wording has no reader). The Responses lane rewrites a refused
`reasoning.effort` of `minimal` or `max` to the nearest level the refusal
lists, and learns it; its sampling refusals name a parameter the Responses
reader does not localize, so they are forwarded. Learned field-repair history
is keyed on the provider entry name and upstream model, not on the region
or host, so a `region` change, or switching an entry between `base_url`
and `cloud` under the same name, keeps that history exactly like a
`base_url` repoint; decay re-checks each verdict.

## Gemini (`gemini`, `gemini-cloud-code`)

UNTESTED in this release: both formats parse, route and build, but neither
has been validated against its live upstream for this release. Keep a
fallback chain entry behind any model you route here.

A `gemini` provider talks to the native Google Gemini REST API
(`generateContent` / `streamGenerateContent`), NOT the openai-chat
shim. It wins native fidelity on `systemInstruction`, `contents`/`parts`,
`functionDeclarations`, `generationConfig` (incl. `thinkingConfig`), and
the `usageMetadata` cached-content + thoughts token accounting. See
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#gemini-native-format--gemini) for
the per-feature mapping and the before/after fidelity note.

```toml
[providers.gemini]
format     = "gemini"
base_url   = "https://generativelanguage.googleapis.com/v1beta"   # the public v1beta endpoint
credential = "env://GEMINI_API_KEY"
auth       = "x-goog-api-key"

[models.gemini-flash]
provider = "gemini"
upstream = "gemini-2.5-flash"

[aliases]
"gemini-*" = "gemini-flash"
```

The keys, in order:

- `format = "gemini"` -- the API-key surface. `format =
  "gemini-cloud-code"` is the Cloud Code surface described below.
- `base_url` (required) -- the API base; write
  `https://generativelanguage.googleapis.com/v1beta` for the public API.
  The provider appends `/models/{model}:generateContent` (non-stream) and
  `/models/{model}:streamGenerateContent?alt=sse` (stream).
- `credential` -- a reference (`env://`, `file://`) resolving to a Google
  AI Studio API key. It is resolved per request, so a rotated file is
  picked up without a daemon restart.
- `auth = "x-goog-api-key"` -- the key is sent as the `x-goog-api-key`
  request header; it is the only value this format accepts.
- `header_extras` (optional) -- provider-level extra request headers,
  merged per the [header_extras merge](#header_extras-merge) rules.
- `payload_extras` (optional) -- provider-level JSON merged into the
  outbound request body. This is the flow-through path for knobs the
  canonical schema does not carry natively -- notably `safetySettings`.
  The merge is top-level and shallow, so `generationConfig` (which
  routectl assembles itself) is dropped with a WARN rather than merged,
  and no field inside it can be set this way. Merged per the
  [payload_extras merge](#payload_extras-merge) rules.

  Only the operator's `payload_extras` reach Gemini for traffic arriving
  through the Anthropic Messages, OpenAI Chat Completions, or OpenAI
  Responses ingress. Those ingresses sweep unrecognized top-level client
  fields into the request so that same-dialect upstreams can receive
  them, but those fields are addressed to another vendor. On a Gemini
  target they are withheld: `mcp_servers` (which can carry a remote
  server's bearer), `safety_identifier`, `prompt_cache_key`, and any
  future key alike. Where the client and the operator set the same key,
  only the operator's value is sent. Each request that withheld at least
  one such key bumps the `(gemini, ingress_extra_withheld)` policy-action
  counter once and logs one WARN naming the sanitized keys and their
  count, never their values. The client's `metadata` block is always
  withheld, whatever the request's origin, and counts under
  `client_fingerprint_stripped`; an operator-set `metadata` value is
  still sent. A library caller that builds its own `ChatRequest` without
  an ingress keeps forwarding its explicit `provider_extras`.
- `user_agent` (optional) -- override the outbound `User-Agent`.

### Cloud Code (`gemini-cloud-code`)

`format = "gemini-cloud-code"` is the Cloud Code ("antigravity")
OAuth-bearer surface. Its `credential` MUST be a managed
`oauth://antigravity[#label]` login (a one-time `routectl login
antigravity`, live Google consent in a browser); any other credential is
refused. The resolved bearer is sent with `auth = "bearer"` as
`Authorization: Bearer` (NOT `x-goog-api-key`). This lane is EXPERIMENTAL
/ best-effort beyond being untested: it speaks an internal surface with no
published contract, so a change upstream can break it between routectl
releases. See
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#cloud-code-antigravity-egress-mode-format--gemini-cloud-code).

`routectl login antigravity` offers this entry under the convention's
name (`antigravity-default`) plus its pool, so the hand-written form below
is for an entry you want to name or tune yourself:

```toml
[providers.gemini-cloud-code]
format     = "gemini-cloud-code"
# The daily cloud-code host; a seat served on the production host uses
# https://cloudcode-pa.googleapis.com instead, and the one value moves
# generate, loadCodeAssist and onboardUser together.
base_url   = "https://daily-cloudcode-pa.googleapis.com"
credential = "oauth://antigravity"
auth       = "bearer"
# The Cloud Code project id is auto-resolved and persisted; set
# cloud_project_id to the BARE id to skip discovery.
# cloud_project_id = "my-project-1234"

[models.gemini-flash]
provider = "gemini-cloud-code"
upstream = "gemini-3.1-flash-lite"
```

- `base_url` (required) -- the DAILY Cloud Code host
  (`https://daily-cloudcode-pa.googleapis.com`), whose `/v1internal:*`
  methods are what a consumer (non-GCP-ToS) seat is served on, or the
  PRODUCTION host (`https://cloudcode-pa.googleapis.com`) for a seat
  served there. The managed `oauth://antigravity` credential is accepted
  only on those two hosts over `https://`, and this format takes no static
  credential, so an enterprise mirror or a staging host is not reachable
  (see "Managed token host containment" under
  [Why route claude-code through routectl](#why-route-claude-code-through-routectl)).
  There is exactly ONE base for the lane, and it moves every Cloud Code
  call together -- `generateContent`, `loadCodeAssist`, and
  `onboardUser` -- so a pin can never split inference onto one host and
  onboarding onto another. The value is forwarded verbatim, never
  rewritten; `config check` and startup emit a WARNING when an entry pins
  the production host, since consumer seats are served on the daily one.
- `cloud_project_id` (optional, `gemini-cloud-code` only) -- the Cloud
  Code project id for this seat, as the BARE id (`my-project-1234`), never
  the `projects/`-prefixed resource name. Set it to skip discovery
  entirely: it is consulted BEFORE the credential store's cached project
  id and before `loadCodeAssist` / `onboardUser`, so even a cold request
  goes straight to `generateContent`. The value writes through to the
  seat's persisted project id, so later requests -- and other entries on
  the same seat -- read it from the cache; if two entries name the same
  seat with different ids, last writer wins (routectl does not reconcile
  them). If the upstream rejects the configured id as not applying to
  this seat (a `PERMISSION_DENIED` or `NOT_FOUND` project verdict), that
  request fails, routectl logs one WARNING naming the entry and the
  rejected id, and the entry falls back to ordinary discovery for the
  rest of the process (the warning is emitted once per process, and a
  restart re-trusts the configured value). Quota, auth, and 5xx failures
  are not project verdicts and never trigger the fallback. Unset or empty
  leaves discovery (via loadCodeAssist, falling back to onboardUser) as
  the only source; the resolved id is cached in the credential record.

`header_extras`, `payload_extras` and `user_agent` work as on `gemini`.

Cloud-code model ids are the upstream's own, and it churns them; pick one
verified against the live lane (see
[TESTED_MODELS.md](TESTED_MODELS.md#cloud-code-gemini-format--gemini-cloud-code-antigravity-oauth))
rather than reusing a name from the API-key lane. The `gemini-2.5-*` ids
still resolve here as server-side aliases, but routectl no longer documents
them and WARNs when one is configured on this lane.

Vertex AI / Google service-account ADC is NOT implemented: no format
reaches Vertex today, and a Vertex `cloud` value is documentation only
(see [`base_url` and `cloud`](#base_url-and-cloud)).

## Per-provider capability filter

Some upstreams reject specific built-in tool shapes (Bedrock, for
example, currently 400s on Anthropic's `web_search_*` tool families).
Without a declaration the router learns this on its own: the first
such request on each Bedrock lane gets the 400, falls back to the next
chain entry, and records the refused capability, and later requests
carrying it skip that lane (see "Learned capability tempo" below).

When you already know the answer, declare it under `unsupported` in
[`[capability.overrides]`](#capabilityoverrides-is-the-durable-decision-surface),
keyed by the provider (or `"<provider>:<nickname>"` for one model). The
target is skipped before the first dispatch, so even that one miss per
lane -- the latency, the 400 in operator dashboards, the failure counted
against the lane's breaker -- never happens:

```toml
[capability.overrides.bedrock]
unsupported = ["web_search"]
```

Feature keys come from the request's `tools` array: a trailing
`-YYYYMMDD` or `_YYYYMMDD` suffix on a built-in tool's `type` is
stripped, so `web_search_20250305` and a future `web_search_20251102`
both reduce to `web_search`; the computer tool (`computer_20250124`)
keys as `computer_use`, while `computer_toolset_*` keeps its own
`computer_toolset` key; user-defined custom tools contribute no key. If every chain entry is skipped this way, the router returns
`501 not_implemented` with the message `no provider in chain supports
features: <keys>`. Config version 5 retired the per-provider and
per-model `unsupported_features` lists this used to be written as; write
them as `[capability.overrides]` entries instead (see
[Version 5 retires the capability lists](#version-5-retires-the-capability-lists)).

## Per-provider runtime gates

Each `[providers.X]` entry accepts five runtime-gate knobs set
directly on the `[providers.X]` table (the policy is flattened into
the provider entry -- there is no `[providers.X.runtime]` sub-table;
nesting one fails to load with `unknown field \`runtime\``). All
accounting is per-attempt (not per-request).

| Field                          | Type       | Default         | Effect |
|--------------------------------|------------|-----------------|--------|
| `rpm_limit`                    | Option<u32> | None (disabled) | Maximum requests per minute to this provider. When exceeded the router treats this provider as a fallbackable failure and tries the next chain entry. Resolution order: per-provider > claiming pool > None. |
| `circuit_failures`             | Option<u32> | 5               | Trip the circuit breaker after this many consecutive failed attempts. Once tripped, the router skips this provider for `circuit_cooldown_ms`. Resolution order: per-provider > claiming pool > 5. `0` turns the breaker off and stops the walk: a provider `0` wins over its pool's value, and a pool `0` wins over the default. |
| `circuit_cooldown_ms`          | Option<u64> | 60000 (60s)     | How long to keep the circuit open once tripped. Resolution order: per-provider > claiming pool > 60s. Unused while the breaker is off. |
| `request_timeout_ms`           | Option<u64> | 1800000 (30 min) | Per-attempt timeout on complete and count-tokens requests; a stream that has committed content is not cut by it. Resolution order: per-provider `request_timeout_ms` > claiming pool > global `[retry] request_timeout_ms` > 1800000 (30 min). Per-alias retry overrides were removed in v0.6; to vary timeouts per route, split into distinct `[providers.X]` entries. |
| `stream_first_byte_timeout_ms` | Option<u64> | 300000 (5 min)  | Per-provider first-content timeout for streaming responses (content-free leading chunks do not satisfy it). Resolution order: per-model > per-provider > claiming pool > global `[retry] stream_first_byte_timeout_ms` > 300000 (5 min). |

`rpm_limit` is unlimited when no tier sets it. The circuit breaker is
on for every provider and pool seat: with nothing configured, 5
consecutive failed attempts open it for 60s, after which one request
re-tests the seat. Only rate-limit, server-error, overload, timeout and
network-error failures count, and every retry is its own attempt, so a
request that spends its `max_attempts - 1` retries on one seat can
debit it several times. Set `circuit_failures = 0` on a provider or pool
to turn the breaker off there.

A hot reload applies changed `circuit_failures`, `circuit_cooldown_ms`
and `rpm_limit` values (on a provider or a pool) to the running gates
without resetting them: the consecutive-failure count and an open or
half-open breaker carry, and the next failure is judged against the new
threshold. An open window follows a changed cooldown, except one opened
by an upstream reset, which keeps its own length. A raised or lowered
`rpm_limit` keeps the bucket's current level, capped at the new limit;
a newly set limit starts with a full bucket, and removing it makes the
gate unlimited at once.

Example:

```toml
[providers.bedrock]
format                 = "bedrock-invoke"
cloud                  = { name = "aws", region = "us-west-2" }
credential             = "aws://chain"
auth                   = "aws-sigv4"
rpm_limit              = 60
circuit_failures       = 5
circuit_cooldown_ms    = 60000
request_timeout_ms     = 120000
```

### Local refusals

When routectl itself declines to send a request, because every target the
request could use is refused before any upstream contact, the client gets
HTTP 503 on chat completions, messages and responses. `error.type` (and
`error.code` on the OpenAI shapes) is one of these tokens:

| Token | Why the target was refused |
|---|---|
| `credential_unusable` | No usable credential seat (for example an empty pool). |
| `account_parked` | The credential's account is parked after the upstream rejected it. |
| `breaker_open` | The target's circuit breaker is open. |
| `rate_limited` | The target's own `rpm_limit` has no request left this minute. |

When several targets are refused for different reasons, the token is the
first one in that table. A `Retry-After` header carries the earliest known
recovery among the refused targets, rounded up to whole seconds and
clamped to 1-3600 s: the rest of an open breaker window, the time until
the RPM bucket holds one request again, or the rest of an account park.
It is omitted when no recovery time is known (an empty pool, a zero
`rpm_limit`, or a breaker or park whose one re-test request is in flight).
A stream whose SSE head already went out ends with one terminal error
event carrying the same token.

If a target was dialed and failed, its own error is returned instead. An
upstream 429 is passed through unchanged, with the upstream's status,
error type and `Retry-After`. A token count stops at the first target
whose breaker or RPM gate refuses it rather than trying the next one; it
still answers 503 with that target's token.

## Retry and fallback defaults

Every failure class but an unknown one falls back to the next chain
entry or pool seat: chains and pools exist to fall back, so there is no
setting that turns it off. This is the safest default for
Cloudflare-fronted upstreams (opencode.ai, openrouter.ai, etc.), which
surface upstream-origin failures through extended 5xx codes (520-527,
530). A failure routectl cannot classify (an upstream status outside
every class below) stays terminal, and availability probes fail fast by
a built-in rule (`probe_max_tokens`, below). The one per-class setting
is how many times to retry the same target, `[retry.classes]`.

```toml
[retry]
max_attempts                  = 3
initial_backoff_ms            = 250
backoff_multiplier            = 2.0
jitter_ms                     = 50
request_timeout_ms            = 300000        # 5 min per attempt
stream_first_byte_timeout_ms  = 90000         # 90s -- thinking models stall
probe_max_tokens              = 1             # keep availability probes in their pool

# Same-target retry count per failure kind; a class named here takes
# this count in place of max_attempts.
# [retry.classes]
# server-error = 2                            # one same-target retry per request
```

`probe_max_tokens` (default `1`) marks availability probes. A request
whose `max_tokens` is at or below this value is treated as a probe --
Claude Code sends `max_tokens=1` quota/health checks to `/v1/messages`,
and their tiny output is never read. Whatever the failure, a probe is
never retried on the same seat and never leaves the contiguous run of
chain entries for its `[models]` entry: it may move to another seat of
the same pool (a rate-limited seat does not mean the pool is out of
quota), and it never dials the failed seat again, even when the chain
names that pool a second time. Once the run is spent it returns the
last upstream error. A probe's native 429 or 529 is never counted
against the seat's circuit breaker, on completions, streams (before or
after first content) and `count_tokens` alike: the probe frees any
half-open slot it held and takes no failure debit or park. Every other
probe failure (a 5xx, a network error, a timeout) debits the seat like
any request's, so a broken seat is not hidden behind quota checks. Set
`probe_max_tokens = 0` to disable (no request is ever treated as a
probe).

### Same-target retry counts (`[retry.classes]`)

Most deployments need nothing here: the built-in class table below IS
the policy, and `max_attempts` (default 3) is the one knob it reads.
`max_attempts = N` allows at most N - 1 same-target retries per client
request, unless `[retry.classes]` sets another count for that class,
counted across the whole fallback walk: a retry spent on the first
chain entry is not available to a later one, so a single-entry chain
retries N - 1 times and a longer chain never more. A repair resend (the
router undoing or rewriting a field the upstream refused) and the one
401 refresh-and-retry per target have their own allowances and do not
spend this count.

Which failures may be retried at all, and how far one reaches, is the
failure's scope, decided before any class leaf is read. A failure the
upstream returned for the request itself excludes every remaining
chain target it would hit the same way, for the rest of that request:

- A native 400 or 422 the classifier types as an unsupported feature
  or an exceeded context window, or whose message the capability
  matcher recognizes as naming a capability the model lacks (a Bedrock
  `ValidationException` naming a tool type, say), excludes its
  `[models]` entry: the walk can still reach another entry, even on
  the same origin with the same upstream id (a larger-window variant,
  say). This holds whether or not `[capability]` learning is on. The
  exclusion covers that entry's seats on every origin, pool members
  on another base URL included: a capability the model lacks is a
  fact about the model, not the endpoint that answered. An
  Anthropic "prompt is too long" 400 is not recognized as either, so
  it keeps endpoint scope.
- Any other native 400 or 422 excludes its endpoint (the provider's
  normalized origin): pool siblings and every other entry on that
  origin are skipped, and the walk reaches only another origin.
- A 429 the provider recognizes as a request-caused refusal rather
  than a rate limit (an Anthropic OAuth `rate_limit_error` whose
  message is the bare `Error`, with no `Retry-After` and no
  `anthropic-ratelimit-*` header) excludes its endpoint and debits its
  seat once.
- Everything else (5xx, an unrecognized 429, 529, a network error, a
  timeout) is the failed seat's alone: it may be retried within the
  request-wide count, and the walk does not dial that seat again.

None of these is retried on the same target except the last, and a
native 400 or 422 never debits the breaker. A
request whose targets were all excluded returns the last upstream
error it received.

Every upstream failure classifies into one of ten config-facing
classes (`crates/routectl-router/src/class_policy.rs`), or into no class
at all (unknown). Each class resolves to a `(retry_cap, falls_back)`
pair via `RetryPolicy::resolved_class`, and separately either debits
the per-seat circuit breaker's health accounting or does not -- a fixed
set, independent of the retry/fallback decision:

| Class (key) | Retry-same cap | Falls back? | Debits breaker? |
|--------------------|------------------------|--------------|-------------------|
| `rate-limited` | `max_attempts` | yes | yes |
| `server-error` | `max_attempts` | yes | yes |
| `overloaded` | `max_attempts` | yes | yes |
| `timeout` | `max_attempts` | yes | yes |
| `network-error` | `max_attempts` | yes | yes |
| `auth` | 0 | yes | no |
| `bad-request` | 0 | yes | no |
| `content-policy` | 0 | yes | no |
| `context-window` | 0 | yes | no |
| `feature-unsupported` | 0 (reserved -- see below) | yes | no |
| unknown (not nameable) | 0 | no | no |

To change one class's same-target retry count, name it under
`[retry.classes]`:

```toml
[retry.classes]
server-error = 3    # a cap of 3 allows 2 same-target retries per request
overloaded = 0      # never retry the same target; walk on immediately
```

A class not named keeps its cap from the table. A count only narrows
what the scope rule allows: it is compared with the request-wide retry
count, it never re-enables a same-target retry the scope rule forbids,
and it never admits a target a failure excluded, so a raised
`bad-request` count resends nothing. No count changes whether a class
falls back.

**Breaker debit is not configurable per class.** The "debits breaker?"
column is a fixed set (`RateLimited`, `ServerError`, `Timeout`,
`NetworkError`, `Overloaded`) baked into the router's `class_debits`
predicate (`crates/routectl-router/src/router/outcome_rule.rs`); no
`[retry.classes]` count changes whether a class counts against a seat's
health.

`feature-unsupported` is RESERVED: `[retry.classes] feature-unsupported`
is rejected at config load (`validate_class_policy`,
`crates/routectl-router/src/factory/validate.rs`). The built-in row
above already governs it.

The earlier forms (the three per-status `[retry]` caps, and the
`retry` and `fallback` leaves of a per-class sub-table) are refused at
load by name, each line naming what to write instead: see the `retry.*`
rows of [Retired config keys](#retired-config-keys).

#### Precedence

One failure decision governs retry and fallback for every status, in
this order: the failure's scope (above) and the probe rule set what the
request may still do; the class's retry cap (`resolved_class`,
including any `[retry.classes]` count) then only narrows it. A chain
entry with no provider at all is walked past before any attempt,
independent of class.

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

One rule decides what a failed attempt carrying a reset does to the
seat's breaker, and it is the same on completions, streams and
`count_tokens`:

- A reset larger than 5 seconds, clamped to
  `max_honored_retry_after_ms`, parks the provider via the circuit
  breaker -- the request itself falls over to the next chain entry
  immediately rather than waiting (`count_tokens` never falls over on a
  health error; it returns the error).
- A reset at or below 5 seconds takes the ordinary threshold-gated
  failure debit, exactly as an error without a reset does.
- A probe request (`max_tokens` at or below `probe_max_tokens`) never
  parks. A probe's 429 or 529 takes no debit at all, whatever its
  reset; a probe failing any other way follows the normal class-based
  rule (a debiting class takes the ordinary debit, a non-debiting one
  such as a 400 takes none).

Once a stream has delivered content, an error frame arriving later is
settled once, by the same rule as a failure before content and on its
built-in class: a debiting class
(rate-limit, server error, timeout, network error, overload) takes the
ordinary debit or parks on a large reset as above, a non-debiting class
(auth, bad request, content policy, context window, unsupported
feature) takes none, and a probe stream keeps its exceptions (its 429
or 529 leaves the breaker untouched, and it never parks).

Completions additionally fold a reset at or below 5 seconds into the
next same-provider retry sleep (it never blocks the request thread
beyond that). Streams and `count_tokens` do not retry the same
provider, so there the reset only decides park versus debit. Recovery
after a park still flows through the breaker's single half-open probe.
The resolution and clamp live in `RetryPolicy::max_honored_retry_after`
and the router's `rate_limit_reset_hint` / `breaker_outcome`
(`crates/routectl-router/src/config/schema.rs`,
`crates/routectl-router/src/router/outcome_rule.rs`); the router applies
the outcome in `apply_breaker_outcome`
(`crates/routectl-router/src/router/runtime_gate.rs`).



## Proactive context-window gate (`[window_gate]`)

Before dispatch, routectl estimates the request's token size and skips
fallback-chain targets whose configured context window cannot hold it,
so an oversized request avoids a doomed round trip on a small-window
entry. This runs after the capability pre-filter, as the second chain
filter pass.

A target is skipped when

```text
estimate + output reserve + 1024 > context window
```

The context window is the target model's
[`context_window`](#context-window-and-output-ceiling) and nothing else:
routectl never takes a window from outside data. A model without one is
never skipped. Set `context_window` on every model in a gated chain
(the gate only acts on targets that have one), and above all on a
small-window fallback, which is the target the gate exists to step past.

The window is total context, so the upstream sets the output limit aside
before any input fits; the output reserve accounts for that. It is the
request's own `max_tokens` when the client sent one, otherwise the
model's configured `max_output_tokens`, otherwise the 64000 tokens
the `anthropic-api` and `bedrock-invoke` egresses send when the client
sets none, otherwise 0 (an egress that forwards an omitted limit as
omitted). A ceiling learned from an upstream refusal is not part of the
reserve, so the reserve can overstate what is sent, which only makes a
skip more likely, never less. The 1024 tokens are a fixed headroom, not a
fraction of the window, so a 1M-window target is not held to a smaller
effective size than a 200k one: on a 1M window with a 64000-token
reserve, a request estimated at up to 934976 tokens is kept.

It is a best-effort filter, not a guarantee. A skip requires a
configured window and a non-final target, so several cases still
dispatch: a target with no
`context_window`, a chain of one, a chain whose every target
overflows, and any request the estimate undercounts. Each of those falls
through to the reactive path below.

The gate can never be the reason a request has nowhere to go: it has no
empty-chain error path. A chain of one is returned untouched before any
estimate is computed, a target with no configured window is
kept (an unknown fact never enables a skip), and a chain whose every
target overflows is returned unchanged so the caller sees exactly
today's upstream error rather than a routectl-invented one.

The optional `[window_gate]` block is the **global** switch. The gate is
off by default: a missing block, or a block with no `enabled` key,
leaves it disabled. Opt in with `enabled = true`.

```toml
[window_gate]
# Master switch for the proactive context-window gate. Default false.
enabled = true
```

- **`enabled`** (bool, default false) -- the master switch. When
  `false` the gate returns the resolved chain before it computes
  anything: no estimate, no chain reordering, no `window_gate_skip`
  WARN, and no movement in the skip counter. Turning it off is
  byte-identical to running with no gate at all, which is what makes
  switching it back off a safe first move when you suspect the gate is
  mis-routing.

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

**One field deliberately.** The gate's 1024-token headroom is a baked
constant, not an operator knob. A margin tuned per deployment turns a
routing decision into a support surface, and both error directions are
already survivable: an underestimate misses a skip and costs one round
trip, while an overestimate is a re-route among configured windows rather
than a denial. The switch above is the whole operator surface; if the
gate misbehaves, the answer is off, not retuned.

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
the first skipped target with the estimate, the corrected figure the
decision actually used, the output reserve, the target's configured
window, and the running skip total.
Routectl-internal identifiers and token figures only -- never anything
from the request body.

## Learned token-estimate correction

The gate above compares its estimate against a target's configured window,
and that estimate is serialized bytes over four -- wrong per model, and
wrong *differently* per model. routectl measures that per-model error
from traffic it has already served: a request that completes
successfully and whose upstream reported a nonzero prompt total
contributes the estimate routectl used paired with that
cache-inclusive total, and those pairs reduce to one multiplicative
correction per lane. A lane is `(provider kind, upstream model id)`:
the `upstream` the request was sent with, not the nickname you declared
under `[models.X]`. Two nicknames that send the same upstream on the same
provider kind share one lane (a direct entry and a passthrough entry for
one model learn together), every seat of a pool shares its model's lane,
and renaming a nickname keeps the lane's evidence. The same upstream on
two provider kinds is two lanes. A forwarded-credential target sends the
client's requested model, so that is the lane it reads: it uses the
correction an own-credential target serving that same upstream learned.
A request served on a forwarded credential contributes nothing; only
own-credential traffic writes lane evidence. A failed or partial request contributes nothing either:
there is no trustworthy pairing between what was estimated and what the
upstream actually charged for.

The correction has exactly ONE consumer: the proactive context-window
gate's comparison. It never changes the bytes sent upstream, never
changes the estimate figure persisted to the usage ledger, and never
feeds the advisory trim or the reduction path. Above 1.0 it means the
estimator under-counts this lane, so the corrected figure is larger and
the gate is more willing to skip a small-window target; below 1.0 it
means the estimator over-counts, so the gate is more willing to admit a
target it would otherwise have skipped.

The correction always applies; no config key turns it off.

**Always on is safe because a cold lane corrects nothing.** A lane
produces no correction until it has accumulated real evidence, so a
fresh install gates on the raw estimate. Every refusal collapses to the
same uncorrected path: an unseen lane, a lane
with too few recent samples, one whose samples are all too old, one
whose distinct callers are too few, and one whose reduced ratio landed
outside the sane band. That last case is **refused, not clamped** -- a
ratio that extreme is evidence a lane is mis-keyed or fed garbage, not
evidence of a genuinely extreme correction, and clamping it to the bound
would let a mis-keyed lane still move a routing decision.

**The gate switch is the operator surface.** With `[window_gate]
enabled = false` (the default) the correction is never consulted for
routing, because the gate returns the chain before it computes any
estimate. routectl keeps recording evidence into the per-lane store and
persisting the estimate/actual pairs to the usage ledger either way.

**No tuning knobs.** The reduction's sample floors (a minimum count of
recent samples and a minimum count of distinct callers before a lane may
produce a correction), the age bound past which a sample stops counting,
and the sane band a reduced ratio must land inside are all **baked
constants, not operator knobs**. A band tuned per deployment turns a
routing decision into a support surface, and the fallback in every
direction is the uncorrected estimate rather than a denial. If the gate
misbehaves, the answer is `[window_gate] enabled = false`.

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

**Across a reload.** The lane store is preserved across a live config
swap. A
lane is dropped when no own-credential model of its provider kind sends
its upstream under the new config: pointing a nickname at a different
upstream reads that upstream's own (possibly cold) lane, and the old
upstream's lane goes once nothing serves it. The startup warm applies the
same rule to ledger rows, whatever nickname recorded them.

Whether a correction is moving a decision is observable in the gate's
own throttled `window_gate_skip` WARN, which reports both the raw
estimate and the corrected figure the decision actually used. They are
equal on an uncorrected lane, and their divergence is the only way to
see a learned correction move a skip. The startup warm logs its own
tally: rows loaded, rows accepted, rows rejected, and how many lanes
came back calibrated.

## Quota-aware seat placement

On a multi-seat OAuth pool running `sticky-least-loaded`, a NEW
conversation's birth seat is ordered by each credential account's
remaining short-window subscription budget, read from the quota headers
upstreams already return on every response and, for a seat that carries no
traffic, from the vendor's usage endpoint (see
[Background usage poll](#background-usage-poll)). This is what makes a pool of
two subscription accounts actually spread new conversations instead of
tying on available RPM (which is unlimited on a subscription seat) and
falling through to the anti-herd rotation.

`sticky-least-loaded`, the default, is the only strategy this applies to.
A pool set to `fill-first` places exactly as it always has and never reads
a seat's budget, *deliberately*: draining one seat is what that setting
asks for. A pool that omits `seat_selection` gets quota-aware placement.

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
before any response or poll has been observed, a seat whose reading has lapsed
past its own window, a provider routectl curates no short recovering
window for (the Codex subscription egress is one: its readings are
listed on `/status/accounts` but do not place), and a pool where some seats are
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

Placement always applies; no config key turns it off.

**Always on is safe because thin evidence places nothing.** A seat
produces no reading until a real upstream response carries one or the
background usage poll reads one, and a restart starts every seat with no
reading (the stored reading history is never loaded back), so a fresh
process places exactly as it did before quota placement existed until
those readings arrive, and every case above that falls back (no fresh reading, a lapsed one, an
uncurated provider, a partially observed pool) resolves through the
unchanged capacity ranking. Conversation pins and the one-time migration
off an unhealthy seat are never affected. A pool that should not place
by budget at all sets `seat_selection = "fill-first"`.

**No tuning knobs.** The per-provider budget thresholds, the long-window
guard and the freshness bounds are constants grounded in captured
upstream evidence, not operator knobs: a threshold tuned per deployment
turns a routing decision into a support surface, and the fallback in
every direction is the pre-quota chooser rather than a denial.

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
[credential pools](#credential-pools-multiple-accounts-per-provider)) is
UNCHANGED by this feature: quota changes which seat a birth picks, never
how that decision is named.

## Per-model knobs

### Removed model keys

`supports_adaptive_thinking`, `effort_levels` and `max_thinking_budget`
are no longer `[models.X]` keys. A config that still
carries any of them is refused at load, reload and every config edit,
with one message listing every `models.<nick>.<key>` path (values are
never echoed) and what now decides that behavior (see
[Retired config keys](#retired-config-keys)). Delete the lines by hand.

| Removed key                  | What decides it now                                                           |
|------------------------------|-------------------------------------------------------------------------------|
| `supports_adaptive_thinking` | client thinking is forwarded; effort alone adds no thinking                   |
| `effort_levels`              | refused effort levels are learned per lane                                    |
| `max_thinking_budget`        | client thinking budgets are forwarded                                         |

A client's Anthropic `thinking` object (`enabled`, `adaptive`, `disabled`,
`between_tools`, `display` included) is forwarded as sent; a model that
refuses `enabled` thinking is repaired and learned per lane (see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#anthropic-opus-47-and-any-future-adaptive-thinking-models)).
A request with an effort and no `thinking` object ships no thinking. An
effort level reaches the upstream unclamped until a refusal teaches the
lane which level to use instead. An `enabled` budget is forwarded whatever
its size; a budget routectl translates from another dialect is fitted under
the output ceiling below.

#### Context window and output ceiling

Two `[models.X]` keys state the model's token limits. routectl never
takes either from outside data, so set them yourself:

```toml
[models.sonnet]
provider          = "anthropic"
upstream          = "claude-sonnet-4-6"
context_window    = 200000   # total tokens, input plus output
max_output_tokens = 64000    # sent as max_tokens when the client omits it
```

- **`context_window`** (u32, optional, `0` refused at load) -- the
  model's total context window. The
  [window gate](#proactive-context-window-gate-window_gate) skips the
  model when a request cannot fit, and `GET /v1/models` reports it as
  `context_length`. Unset: the gate keeps the model and `/v1/models`
  omits `context_length`. Set it on every model in a gated fallback
  chain, and above all on small-window models.
- **`max_output_tokens`** (u32, optional, `0` refused at load) -- the
  output ceiling used when the client sends no `max_tokens`. Set it on
  any model whose real ceiling is below 64000, so a request is not
  refused, and on any model that allows more than 64000 when you want
  long outputs without a client limit.

A zero for either key is refused at load, reload and every config edit,
naming the dotted `models.<nick>.<key>` path.

The `max_tokens` value the Anthropic-shape egresses (`anthropic-api`,
`bedrock-invoke`) send when the caller omits the field resolves at
dispatch:

1. `request.max_tokens` -- caller-supplied value always wins.
2. `[models.X] max_output_tokens`.
3. `64000` -- hardcoded baseline.

A learned output ceiling then caps the result. When a lane has learned
the model's limit from a refusal that named it (see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#bedrock-any-region)), a value
above that ceiling is lowered to it before the request is sent, whether
it came from the client or, on `anthropic-api` and `bedrock-invoke`,
from this chain. On `bedrock-converse` a learned ceiling lowers only a
client value. With none, Converse sends no `maxTokens` unless the shipped
thinking is the legacy `enabled` shape, where it sends this chain's value
(`max_output_tokens`, else `64000`) uncapped by any learned ceiling;
otherwise no limit appears on the wire. A value at or below the
ceiling ships unchanged: a learned ceiling never raises a limit. The
ceiling recovers by decay only: it lapses after
[`decay_hours`](#learned-capability-tempo-capability) and the next
refusal learns it again. Lowering a client value is recorded as a lossy
repair; lowering the value routectl filled in is not, since the client
asked for no particular limit.

`routectl doctor`'s **Model knobs** section names, for every configured
model, the ceiling it sends and whether that is `model-config` (its
`max_output_tokens`), `learned` (a learned ceiling that lowers what would
otherwise be sent) or `default` (the egress baseline, or unset on an egress
that sends no limit of its own).

**Spend note.** Reasoning tokens bill as output, and on the legacy
`enabled` thinking path a budget routectl translates from canonical
reasoning tracks the resolved ceiling, because it passes through
Anthropic's `budget_tokens < max_tokens` window clamp:
`reasoning.effort = "max"` becomes the ceiling minus one,
`reasoning.enabled = true` with no effort or budget becomes half the
ceiling, and a caller `reasoning.max_tokens` above the ceiling is capped
at `ceiling - 1`. Every other standard effort level (`minimal` through
`xhigh`) resolves through an exact effort-to-budget table and is
unaffected.

Only the Anthropic-shape egresses (`anthropic-api` and `bedrock-invoke`)
send this value as `max_tokens` whenever the caller omits one.
`bedrock-converse` uses it to size the legacy thinking budget and, under
legacy `enabled` thinking only, sends it as `maxTokens` when the caller
omitted one (Claude requires `max_tokens` above the budget); otherwise it
sends no `maxTokens` the client did not send. The other egresses (`openai-compat`, `openai-responses`)
forward a caller-supplied `max_tokens` under their own wire name
(`openai-responses` emits it as `max_output_tokens`) and forward caller
omission cleanly without injection (good-translator principle: do not
inject where the upstream already handles it). Exception: the
`openai-responses` codex OAuth lane (`chatgpt.com/backend-api/codex`)
omits `max_output_tokens` entirely -- codex's request contract has no
such field and the backend rejects the drift.

A caller that omits `max_tokens` on a model whose real cap is below the
resolved ceiling draws one 400 from the upstream's per-model validation.
On a lane field repair acts on, routectl retries that request at the
limit the 400 names and learns it, so later requests go out at the
learned ceiling; `[models.X] max_output_tokens` saves even the first
refusal. On a lane field repair does not act on (a loopback `base_url`; a
forwarded client credential), set `max_output_tokens`. On a
Bedrock Mantle lane field repair acts on the rows a refusal captured on
that sub-lane grounds: the `max_tokens` ceiling on the Anthropic lane;
`top_p`, `presence_penalty` and `frequency_penalty` on the Chat
Completions lane; the `minimal` and `max` effort levels on the Responses
lane. No ceiling row is admitted on the Chat Completions and Responses
Mantle lanes, so a token-limit 400 there reaches the client; their
egresses never inject a limit and send only the caller's own value, so
the remedy is for the caller to lower its `max_tokens`. Chat Completions
`reasoning_effort` is a known gap there: its refusal wording has no
reader, so it is forwarded.

#### The response `model` field

The `model` field routectl echoes back in responses (and on every
streaming chunk, including the terminal usage-only chunk) is the model
or alias the client requested, verbatim: the exact string the client
sent, or the `x-routectl-alias` header override. A client asking for
`deepseek-chat` and routed to upstream `deepseek-v3` sees
`"model": "deepseek-chat"`, not the upstream id, across every hop of a
fallback chain and every chunk of a stream. Internal accounting and
observability (usage capture, pricing) key off the served model and
upstream recorded in dispatch metadata.

Client responses never carry a provider field, on any ingress dialect or
streaming mode. To see which provider served a request, read the
`provider` column of the usage ledger (`routectl usage`), the request's
finished log line, or the status dashboard.

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
  per-pool > global > default resolution. Pin opus xhigh adaptive
  thinking at 300s without forcing haiku to wait 5 min on a dead
  upstream. `0` is ignored with a warning and the next tier applies.

Example:

```toml
[providers.bedrock]
format     = "bedrock-invoke"
cloud      = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth       = "aws-sigv4"
stream_first_byte_timeout_ms = 60000          # provider default

[models.opus47]
provider                     = "bedrock"
upstream                     = "us.anthropic.claude-opus-4-7-v1:0"
header_extras                = { "anthropic-beta" = "context-1m-2025-08-07" }
stream_first_byte_timeout_ms = 300000         # opus override

[models.haiku45]
provider = "bedrock"
upstream = "us.anthropic.claude-haiku-4-5-v1:0"
# inherits provider's 60s; no per-model override
```

## OpenAI chat-lane rule (`reasoning_dialect`)

On an `openai-compat` model whose `reasoning_dialect` is `openai` (the
default) or `deepseek`, one fixed rule decides which upstream ids get the
modern chat-completions shape. An id matches when, lowercased with at most
one leading `openai.` removed, it starts with `gpt-`, `o1`, `o3` or `o4`,
or when the lowercased id contains `reasoner`. A matching id leaves with
`temperature`, `top_p`, `presence_penalty`, `frequency_penalty`,
`logprobs` and `top_logprobs` removed; on the `openai` dialect its
`max_tokens` also goes out as `max_completion_tokens` (the `deepseek`
dialect keeps `max_tokens`).

| upstream id | modern shape |
|---|---|
| `o1-mini`, `o3`, `o4-mini` | yes |
| `gpt-4o`, `gpt-5-chat-latest`, `gpt-5.4`, `gpt-5.5`, `gpt-6-sol` | yes |
| `gpt-oss-120b`, `openai.gpt-5.6-terra`, `OPENAI.GPT-5` | yes |
| `deepseek-reasoner` | yes |
| `gpt5`, `openai/gpt-5`, `openai.openai.gpt-5` | no |
| `deepseek-chat`, `llama-3.3-70b`, `claude-opus-4-7` | no |

There is no key to change the rule. The `vllm`, `raw-think-tag`,
`openrouter` and `passthrough` dialects never apply it, so a self-hosted
`gpt-oss-*` that should keep its sampling params sets
`reasoning_dialect = "vllm"` on its model:

```toml
[providers.local-vllm]
format     = "openai-chat"
base_url   = "http://127.0.0.1:8000/v1"
credential = "env://LOCAL_VLLM_KEY"
auth       = "bearer"

[models.gpt-oss-local]
provider          = "local-vllm"
upstream          = "gpt-oss-120b"
reasoning_dialect = "vllm"
```

For gpt-5.6 and later, prefer a Responses lane (`format = "openai-responses"`)
over chat completions. An id outside the rule that refuses a sampling
parameter is handled by learned field repair instead; see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#openai--o-series--gpt-5).

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
  (`format = "anthropic-messages"`, base_url `https://api.deepseek.com/anthropic`)
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

## context_management (`anthropic-messages` only)

`context_management` is a tuning key on an `anthropic-messages` entry (any
other format refuses it). `true` tells routectl to emulate Anthropic's
context-management-2025-06-27 beta server-side for that provider. Set it
for non-Anthropic `anthropic-messages` endpoints that do not honor the beta
natively (e.g. DeepSeek's `/anthropic` surface). Default is `false`:
routectl forwards the body verbatim and the real Anthropic server handles
the beta itself.

```toml
# DeepSeek /anthropic provider: routectl emulates context management because
# DeepSeek does not natively honor the beta header.
[providers.deepseek-anthropic]
format             = "anthropic-messages"
base_url           = "https://api.deepseek.com/anthropic"
credential         = "env://DS_KEY"
auth               = "bearer"
context_management = true
```

### Thinking-cache bounds

When `context_management = true`, routectl caches thinking blocks observed
in upstream responses for re-injection on the next turn (see
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) "context_management beta emulation").
Each cached entry is capped at 1 MiB of serialized JSON, ~3x headroom over
realistic worst-case Opus 4.6/4.7/4.8 reasoning turns at full 65k
thinking-token budgets (~328 KB at ~5 bytes/token). The cap is fixed; no
key changes it. An entry over the cap is not cached and a structured WARN
is logged; the next turn's cache-miss recovery strips the `thinking` body
key just as it does for a TTL eviction, so the request still completes
without a 400.

The cache LRU holds at most `THINKING_CACHE_CAP = 10000` entries, so its
worst-case memory footprint is 10000 * 1 MiB ~ 10 GiB. The TTL is sliding
(every hit refreshes `expires_at`); idle entries die after the hardcoded
60-minute window.

## Forwarded credential

`credential = "forwarded"` makes an `anthropic-messages` provider a pure
passthrough for the client's own captured claude.ai bearer instead of a
credential routectl holds. Every other credential value is the provider's
own: it authenticates exactly as configured.

```toml
[providers.anthropic-forwarded]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "forwarded"
auth       = "bearer"
```

- `forwarded` is the whole credential -- the provider holds no secret of
  its own; the client supplies it on each request.
- The entry is accepted only with `format = "anthropic-messages"`,
  `auth = "bearer"` and a `base_url` that is `https://` on exactly
  `api.anthropic.com` (case-insensitive), with no `user:pass@` userinfo
  and no explicit non-default port. Any other format, auth, scheme, host or port is
  REJECTED -- a hard containment guarantee: the forwarded credential is
  the client's full-scope claude.ai bearer, which must never be sent to a
  non-Anthropic egress. The error withholds the configured `base_url`.
- On this leg the dispatched request keeps the client's requested
  model verbatim instead of being rewritten to the target's configured
  upstream model, and `GET /v1/models` proxies through to Anthropic's
  real model list rather than returning routectl's local alias list
  (falling back to that local list on any other request, or a
  proxy-side failure). See
  [REMOTE-CONTROL.md](REMOTE-CONTROL.md#pure-proxy-mode) for the
  full admission and failure-handling model.

## Log knobs (environment only)

The three runtime log-safe knobs are set only in the environment that
launches `routectl serve`; `config.toml` has no key for them (a `log`
table is refused, see [Retired config keys](#retired-config-keys)):

```sh
ROUTECTL_TRACE_HEADERS=1          # default off
ROUTECTL_TRACE_BODY_BYTES=1048576 # default 16384 (16 KB)
ROUTECTL_LOG_REDACT_PROMPTS=1     # default off
```

Each knob takes its env var when set, otherwise the hardcoded default
(`false` / `16384` / `false`). A `ROUTECTL_TRACE_BODY_BYTES` that is
zero or not a positive integer falls back to the default.

Accepted truthy spellings for the boolean env vars
(`ROUTECTL_TRACE_HEADERS`, `ROUTECTL_LOG_REDACT_PROMPTS`): `1`,
`true`, `yes`, `on` (case-insensitive, whitespace-trimmed). Anything
else (including empty string) is treated as false.

What each knob does:

- `ROUTECTL_TRACE_HEADERS` -- opt-in for the four `trace_*_headers`
  directions. Directions 1 (ingress), 2 (outgoing), and 3 (upstream
  response) redact secret-bearing header values before emission;
  direction 4 (egress response) stays raw. Default off. See
  [LOGGING.md](LOGGING.md) for the per-direction emit contract.
- `ROUTECTL_TRACE_BODY_BYTES` -- cap on the serialized body emitted at
  TRACE level by the four body-trace helpers (ingress, outgoing,
  upstream success, egress). Default 16 KB. Bump to ~1 MB
  (`1048576`) when capturing live-traffic fixtures so full
  conversation history with cache_control breakpoints is not
  truncated.
- `ROUTECTL_LOG_REDACT_PROMPTS` -- opt-in for prompt redaction in TRACE-level
  body logs. Strips known user-content fields (text blocks,
  tool_use input, instructions, refusal blocks, image data URIs,
  Bedrock Converse `toolUse.input`) and replaces them with
  `<redacted len=N>` while preserving structural fields (model,
  tools, sampling params, finish_reason, usage). Default off
  (verbatim bodies in TRACE).

Caveat -- the env-filter directive (`ROUTECTL_LOG`, e.g.
`routectl=info,routectl_core::log_safe=trace`) is env-only too: it
must reach the tracing subscriber BEFORE any config load runs. To
raise log level for one process, export `ROUTECTL_LOG` in the
environment that launches `routectl serve`.

Caveat -- like every routectl log-safe knob, the resolved value
freezes at process startup. Flipping the env var after launch has
no effect until the next restart. `serve` logs a single `info` line
per knob at boot
(`ROUTECTL_LOG_REDACT_PROMPTS resolved`,
`ROUTECTL_TRACE_BODY_BYTES resolved`, `ROUTECTL_TRACE_HEADERS
resolved`) so operators can confirm the effective value once.

## Usage accounting (`[usage]`)

The optional `[usage]` block controls the usage-accounting subsystem,
which persists one row per request (token counts, cost) to a local
SQLite database. Recording is always on; nothing turns it off. A missing
block keeps every default below.

```toml
[usage]
db_path = "/home/you/.config/routectl/usage.db"  # default resolved path
retention_days = 90                     # prune rows older than this; default 90
poll = true                             # background usage poll; default true
```

| Knob             | Default                          | Reload    |
|------------------|----------------------------------|-----------|
| `db_path`        | `<config-dir>/routectl/usage.db` | restart   |
| `retention_days` | `90`                             | hot       |
| `poll`           | `true`                           | hot       |

- `db_path` -- the SQLite database file. The default resolves the user
  config dir from `XDG_CONFIG_HOME` (else `$HOME/.config`), so no
  literal `~` ever reaches SQLite; each variable is used only when it is
  an absolute path (an empty or relative value counts as unset). With
  neither absolute the default is empty, never a path under the working
  directory: `serve`, `usage` and `usage downgrade` then refuse with
  `no config folder for usage.db_path: set HOME, or set usage.db_path`
  (`usage --db` needs no folder). **Restart-required**: the writer
  opens the database at startup and holds the handle, so a `db_path`
  change is classified restart-required and is reported as
  `usage.db_path` on hot-reload rather than taking effect live.
- `retention_days` -- on daemon startup, rows older than this many days
  are pruned from the database. Hot-reloads; the new value applies at
  the next startup-time prune.
- `poll` -- whether `routectl serve` runs the
  [background usage poll](#background-usage-poll), a boolean (`config
  check` refuses any other value). With `false` the daemon sends no usage
  poll request for any managed login; the token refresh is a separate job
  and is unaffected. Hot-reloads: see the poll's section for what a switch
  does.

**Token-count requests.** Each `POST /v1/messages/count_tokens` request
whose body parses writes one row with `operation = count_tokens`. The
row carries the request's `request_id` (the `x-request-id` the client
received), the identity of the target that served the count (or, on
failure, the last target tried), and the outcome: `ok`, or the
dispatch-error outcome (`upstream_error`, `gate_blocked`). The token
columns (`input_tokens`, `output_tokens`, `reasoning_tokens`,
`cache_read`, `cache_write_5m`, `cache_write_1h`) stay NULL: a count
consumes no tokens. `session_id` stays NULL too: unlike an inference
row, a count row never records the client's session id. These rows are left out of every inference reader
(the `routectl usage` aggregates and cost, `/status/usage` totals and
groups), so counting traffic never inflates request totals or spend;
the per-window observation totals include them. A body that fails to
parse writes no row.

**Walk counts on the status surface.** The `observation` block on
`/status/usage` (usage panel schema 7, for the requested window) and on
`/status/doctor` (doctor schema 32, for today) sums five walk counts over
every `requests` row in the window, token-count rows included:
`request_caused_failures` (failed attempts the request itself caused,
scoped to the endpoint or the model) and `excluded_endpoint`,
`excluded_model`, `excluded_target` and `excluded_probe_run` (targets
passed over because an earlier failure's exclusion of that scope covered
them). A row written before the usage database reached schema 20 carries
NULL in these columns and adds 0. The counts sit beside the block's
`gap_rows`, `gap_dropped` and `complete` fields: when `complete` is false
the window lost rows, so each count is a lower bound. They are not part of
the skip counts and never reconcile against attempt counts.

## Live wire check

routectl can compare the body a client sent with the body it sent upstream
on the same attempt, and count every difference no routectl transform
declared. It has no config table (a `wire_check` table is refused, see
[Retired config keys](#retired-config-keys)); sampling is fixed:

- **First 8 per lane and client version.** A lane is one ingress dialect
  feeding one egress kind (for example `anthropic->anthropic-api`). Each
  lane checks its first 8 successful attempts per client version after
  boot, so a new client release is checked straight away. At most 8
  client versions are tracked per lane; attempts from any further version
  are checked at the sample rate only.
- **Then 1 in 100.** Past the first 8, one successful attempt in 100 is
  checked. The environment variable `ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN`
  replaces the 100 (`1` checks every successful attempt):

  ```sh
  ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN=1   # default 100
  ```

  It is read once when `serve` starts and kept for the life of the
  process; a config reload does not read it again. Surrounding whitespace
  is ignored. A value that is set but is not a whole number from 1 to
  4294967295 (empty, `0`, negative, not a number, too large) refuses to
  start with an error naming the variable; the value itself is not
  printed. `serve` logs the resolved rate and whether it came from the
  variable or the default once at boot, at INFO (see
  [LOGGING.md](LOGGING.md#startup-env-knob-status-lines)).
- **24 h status window.** The status surface shows a loss or local-refusal
  counter only while its last sighting is inside the last 86400 seconds.
  The counters themselves are cumulative since boot and survive a config
  reload.
- **There is no on/off switch.** The check never changes what is sent and
  never fails a request; the only choice is how much it costs. An attempt
  the upstream rejects is always checked, whatever the sample rate.
- **What it reports.** Three counters ride the router's metrics snapshot
  log line: `rc_wire_loss_total{lane,path_class}`,
  `rc_wire_check_total{lane,outcome}` (outcome `clean`, `loss`, `budget`
  or `incomplete` -- a check that ran out of budget or had nothing
  complete to compare is never counted clean) and
  `rc_request_refused_local_total{lane,reason}` (`reason` is the local
  refusal's token, such as `tool-pairing` or `unparseable`). A loss, or an upstream
  rejection field repair could not fix, logs one WARN per (lane, path,
  kind) at target `routectl_router::wire_check`. Every label and path is
  content-free: paths are sanitized JSON pointers, never body values.
- **Reloads.** The per-(lane, version) counts and every counter carry
  across a config reload.

## MITM front-proxy (`[mitm]`)

The optional `[mitm]` block gates on a local TLS-terminating listener
that fronts `api.anthropic.com` (e.g. Claude Code's own traffic), the
same presence-gates-the-feature convention as `[server.auth]`. A
missing block keeps every default below AND zero proxy startup --
routectl's behavior is unchanged until an operator declares `[mitm]`.

```toml
[mitm]
listen_port = 8443
cert_dir    = "/home/you/.config/routectl/mitm-certs"
```

| Knob          | Default                                 | Reload  |
|---------------|-----------------------------------------|---------|
| `listen_port` | `8443`                                  | restart |
| `cert_dir`    | `<config-dir>/routectl/mitm-certs`      | restart |

- `listen_port` -- the local TCP port the MITM listener binds. Must
  differ from `[server] port` -- the two are separate bound sockets on
  the same host; startup validation rejects a collision.
  `serve --mitm-port <n>` overrides this value for one run without
  touching the config file.
- `cert_dir` -- directory holding the locally-generated MITM CA + leaf
  certificates. Defaults under the same user config dir as `usage.db`
  and `config.toml` (`XDG_CONFIG_HOME` else `$HOME/.config`, each used
  only when it is an absolute path; an empty or relative value counts as
  unset). With no config folder the default is empty, and `serve` and
  `rc` refuse with `no config folder for mitm.cert_dir: set HOME, or set
  mitm.cert_dir` until the key is set.

The host the proxy terminates and the origin it forwards to are fixed:
the proxy decrypts only a CONNECT to `api.anthropic.com` (an exact
match, never a suffix) and forwards to `https://api.anthropic.com`, so
the client's full-scope claude.ai token can never reach a non-Anthropic
origin. No config key names either one; see
[Retired config keys](#retired-config-keys).

The whole `[mitm]` block is read once at startup -- editing it takes
effect only on the next restart, since there is no reload path that
respawns the proxy listener.

`[mitm]` is transport-only: it carries no credential knob. Whether an
egress uses the client's own credential is a per-provider choice -- see
[Forwarded credential](#forwarded-credential) above.

**Migrating an old config.** A `[mitm]` block that still carries its
retired credential key is refused at load with a line naming the exact
replacement (see [Retired config keys](#retired-config-keys)). Delete
the key and add a provider block instead:

```toml
[providers.anthropic-forwarded]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "forwarded"
auth       = "bearer"
```

`credential = "forwarded"` is the whole credential -- a forwarded
provider has no configured secret of its own.

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
the network -- it is advice, computed from fixed cache rules per
`[providers.X] kind` (the same rules the would-trim advisory and the
K-gate read): `anthropic-api` and `bedrock` write at 1.25x and read at
0.1x with a 5-minute TTL and a 4096-token floor; `openai-compat` caches
automatically (1.0x / 0.1x, 5 minutes, 4096 tokens); `openai-responses`
caches automatically (0.1x reads, 24 hours, 1024 tokens); `gemini` has no
rules. The model and any per-model prices play no part.

```sh
# Just the break-even threshold K* for a 50k-token cut:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000

# Price the REAL steady-state trim candidate instead of a hypothetical cut:
routectl prompt-size --alias heavy --request ./fixture.json --steady-state

# Plus a keep/break verdict at an assumed reuse count:
routectl prompt-size --alias heavy --request ./fixture.json \
  --hypothetical-d 50000 --hypothetical-k 60
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

C (the total cacheable prefix tokens) is taken from the report's TOTAL
approx-token count; the command does not distinguish a separate
cacheable-prefix slice, so the whole prompt-token footprint is the
conservative C. The projection prints the resolved provider kind and
model, the `cache rules:` line it priced with (write and read
multipliers, TTL, prefix floor, and `(auto-cached)` for a provider that
caches on its own), the break-even K*, and -- when `--hypothetical-k` is
supplied -- the verdict. A target with no rules (a `gemini` provider, or
an alias that does not resolve offline) prints `cache rules: unpriced
(no cache rules for this target)`, `break-even K*: n/a (insufficient
data)` and a `KEEP (insufficient data)` verdict.

## Model prices (`[models.X.pricing]`)

Usage cost and the API-equivalent value of subscription traffic are
display figures; nothing in routing reads a price. Input, output, cache
read and the cache-write price of each cache lifetime resolve on their
own, in this order, and the first source that gives a field wins:

1. **`model-config`**: `[models.X.pricing]` on the served model, while
   that entry still names the served upstream.
2. **`refreshed`**: the table `routectl prices refresh` wrote into the
   config folder.
3. **`shipped`**: the table shipped with the release.
4. **`derived`**: for the Anthropic family only, the 1-hour cache-write
   price is twice the effective input price, when no source above gives
   it and the refreshed or shipped row came from the table's `anthropic`
   provider (or the target is `bedrock` with an Anthropic model id). An
   `anthropic-api` gateway priced from another table provider's row
   (OpenRouter, MiniMax) gets no derived figure.
5. **`unknown`**: shown as unknown, never as zero.

A managed-subscription (`oauth://`) provider resolves through the same
order; it is still billed by seat, and the figure is its API-equivalent.
A model on a `[pools]` entry prices through the pool's first member, the
provider entry its usage rows bill under.

The table provider for a target is chosen from the target's base URL:
the table providers whose `api_host` has the same origin (scheme, host,
port) are ranked by the longest common path prefix, then by how many path
segments they share in any position; a tie on both is unknown (whether a
provider lists the upstream id never breaks it). An operator whose target
host ties this way prices it through `[models.X.pricing]`. A loopback base URL never
matches. With no origin match, `anthropic-api`, `openai-responses`,
`gemini` and `bedrock` fall back to the vendor's own table provider.
A usage row whose provider entry was since renamed or removed prices a
first-party upstream (Anthropic, OpenAI, Google, Bedrock) through the
provider kind recorded with the row.

```toml
[models.sonnet]
provider = "anthropic"
upstream = "claude-sonnet-4-6"

[models.sonnet.pricing]
cache_write_1h_per_mtok = 6.0   # fixes one figure; the rest come from the tables
```

The fields are in USD per million tokens: `input_per_mtok`, `output_per_mtok`, `cache_read_per_mtok`,
`cache_write_5m_per_mtok`, `cache_write_1h_per_mtok` and
`cache_write_per_mtok`. A non-finite or negative value is refused at
load, reload and every config edit, naming the dotted
`models.<nick>.pricing.<field>` path; a known `0` is kept. A field left
unset is priced by the tables below it, or stays unknown. There is no
reasoning rate: reasoning tokens are billed as output upstream, so they
are never a separate cost dimension.

`routectl doctor`'s **Cost pricing** section names which source priced
each field for every configured model. Cost is computed at **query
time**, never persisted, so correcting a price later retroactively fixes
the cost of every historical row it prices.

`cache_write_per_mtok` prices cache writes per lifetime. Each key is a
cache lifetime written as a positive whole number and one unit (`s`,
`m`, `h` or `d`, lowercase): `"30m"`, `"90m"`, `"24h"`. A key outside
that grammar (`"thirty"`, `"0m"`, `"5m5m"`), or two keys for the same
lifetime (`"60m"` and `"1h"`), is refused at load naming the dotted
`models.<nick>.pricing.cache_write_per_mtok.<key>` path.
`cache_write_5m_per_mtok` and `cache_write_1h_per_mtok` stay valid and
price the 5-minute and 1-hour lifetimes; when one of them and a `"5m"` or
`"1h"` map key are both set, the legacy key wins that lifetime. A
lifetime no source prices stays unknown.

```toml
[models.sonnet.pricing]
cache_write_per_mtok = { "30m" = 4.5 }   # one more lifetime; 5m and 1h come from the tables
```

## Refreshing the price table (`routectl prices refresh`)

`routectl prices refresh` downloads the models.dev price list
(`https://models.dev/api.json`), converts and validates it, and replaces
`prices.json` in the config folder (`$XDG_CONFIG_HOME/routectl`, else
`~/.config/routectl`). It is the only network call routectl makes for
prices, and nothing runs it automatically: the daemon only reads the file.

```sh
routectl prices refresh                        # default source
routectl prices refresh --from ./api.json      # a local copy
routectl prices refresh --from https://mirror.example/api.json
```

- The fetch is a plain GET on its own HTTP client: no credentials (userinfo
  in a `--from` URL is never sent), no cookies, no redirects (a 3xx is an
  error naming only the target's `scheme://host[:port]`), 5 s to connect
  and 15 s in total. The usual `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`
  variables apply.
- A missing config folder is created owner-only (`0700`).
- A source over 32 MiB is refused, whether a URL or a file.
- A source that is not valid JSON, repeats a provider or a model, is over
  100000 rows or providers, or holds no usable price, exits non-zero and
  leaves the previous `prices.json` exactly as it was. A good one is
  written owner-only to a temp file beside it, flushed, and renamed into
  place.
- A single row is never a reason to refuse the table: a provider or model
  id that is empty, too long or not printable ASCII is skipped (that
  model stays unknown and the id is never printed), and a provider `api`
  URL that is not a usable http(s) URL leaves the provider reachable only
  through the first-party fallback.
- Cache-write prices are kept per cache lifetime. The 5-minute and 1-hour
  prices are written as `cache_write_5m` and `cache_write_1h`, so an older
  routectl still reads them; any other lifetime goes in a `cache_write`
  object keyed by seconds (`"cache_write": {"1800": 1.5}`). A key that is
  not a positive whole number of seconds prices nothing. A row whose
  `cache_write` object holds more than 16 entries, or prices the 5-minute
  or 1-hour lifetime differently from its `cache_write_5m` /
  `cache_write_1h` key, refuses the whole table; a running daemon keeps the
  table it already holds and logs one WARN. The models.dev `cache_write`
  price is the 5-minute price.
- The command prints the model and provider counts and the table date,
  and how many source rows were skipped when any were.
  The fetched URL is logged at INFO without userinfo, query or fragment.

To keep prices current, schedule the command yourself. A daily cron entry:

```cron
17 4 * * * /usr/local/bin/routectl prices refresh
```

Or a systemd user timer (`~/.config/systemd/user/routectl-prices.service`
and `routectl-prices.timer`, then `systemctl --user enable --now
routectl-prices.timer`):

```ini
# routectl-prices.service
[Unit]
Description=Refresh the routectl price table

[Service]
Type=oneshot
ExecStart=/usr/local/bin/routectl prices refresh

# routectl-prices.timer
[Unit]
Description=Refresh the routectl price table daily

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
```

## Retired: `routectl catalog`

Earlier releases kept prompt-cache economics and per-model limits in a
separate catalog: a table baked into the binary plus a
`catalog_overlay.json` of hand-entered cells, managed with `routectl
catalog`. That catalog is gone. What replaced each part:

- **Prices** resolve per field as described in
  [Model prices](#model-prices-modelsxpricing): `[models.X.pricing]`, then
  the table
  [`routectl prices refresh`](#refreshing-the-price-table-routectl-prices-refresh)
  wrote, then the table shipped with the release; a price none of them
  sets is unknown, never zero.
- **Context window and output ceiling** come only from `[models.X]
  context_window` and `max_output_tokens`. With no `max_output_tokens`, an
  egress that must send `max_tokens` fills 64000.
- **Cache rules** (write and read multipliers, TTL, minimum cacheable
  prefix) are fixed per `[providers.X] kind`; see the
  [cache-break economics projection](#cache-break-economics-projection-advisory).

Cells in a leftover `catalog_overlay.json` are **not migrated**: they stop
applying, and the daemon ignores the file at boot and on reload. To keep
a price one of them carried, set it under `[models.X.pricing]`; to keep a
window or output limit, set `context_window` / `max_output_tokens` on the
model. `routectl doctor` names any leftover catalog file in the config
folder (`catalog_overlay.json`, `catalog_overlay.json.lock`,
`catalog_state.json`, `catalog_import_state.json`) so you can remove it.

Config schema v1's `[cache_pricing]` table is in
[Retired config keys](#retired-config-keys). Nothing reads its
`pricing_verifications.json` sidecar any more.

**`routectl catalog`.** Every subcommand (`list`, `verify`, `import`,
`set`, `disable`, `export`) is gone. Running any of them prints a pointer
to `routectl prices refresh` and `[models.X.pricing]` and exits 2.

## Inspecting the effective config (`config show --effective`)

`routectl config show` dumps `config.toml` with secrets redacted:

- every secret reference (a provider's `credential` and `account_id_ref`,
  and each `[server.auth] tokens` entry) prints as written only when it
  is an `env://`, `file://` or `oauth://` pointer; a provider
  `credential` of `aws://profile/<name>`, `aws://chain` or `forwarded`
  also prints as written (it names no secret); a `literal:` value prints
  as `literal:[REDACTED]` and any other value as `[REDACTED]`;
- every `header_extras` value, on providers and models, prints as
  `[REDACTED]` (header names still print);
- every string inside `payload_extras` (providers and models), at any
  depth, prints as
  `[REDACTED]` (keys, numbers and booleans still print);
- every provider `base_url` is reduced to its ORIGIN
  (`scheme://host:port`), dropping any path, query, or embedded
  `user:pass@` credential.

That reduction means
the output is NOT round-trippable back into a config file, which the
dump says in a leading `#` comment -- your own `config.toml` remains the
authoritative source for verbatim `base_url` values.
Adding `--effective` appends a provenance-annotated view: every setting
with the layer it comes from (the file, the code default, an environment
variable, or another entry), all computed from the one read of the
config file that produced the dump, then
the surfaces where MORE THAN ONE LAYER can supply a value -- so you can
see which layer actually won without cross-referencing the price tables
and the retry defaults by hand.

The derivation is pure: it runs the same price precedence usage cost
uses, over the loaded config, `prices.json` in the config folder and the
table shipped with the release. It resolves no secrets, builds no provider, and makes no
network call, so it is safe to run against any config.

Five sections are printed:

- **settings** -- one row per setting, sorted by its dotted key path,
  as `path=value(source)`, where the value is the one the runtime
  resolves and `source` is one of three, computed by the same resolution
  the runtime uses:
  - `file` -- `config.toml` writes the key (as a `[section]` key, a
    dotted key or an inline table: all three read alike);
  - `default` -- nothing sets it and the value is the one in code;
  - `from <path>` -- the key is unset here and another entry's value
    applies: an `openai-responses` entry with no `codex_version` runs
    under the one another entry sets (`from
    providers.<name>.codex_version`); with none set it reads `default`,
    the version pinned in this build.

  Values come from the same redacted tree as the plain dump, so every
  value redacted there (listed above) prints its sentinel here too and a
  `base_url` prints only its origin. An array prints as one row. A key
  that has no value until you set one is not printed unless its default
  is a fixed value in code: `[retry] max_honored_retry_after_ms`, and
  on each `openai-responses` entry `codex_version`. The five
  tiered tuning keys an entry leaves unset resolve in **resolved
  tuning** below; the rest of these keys mean off, or that the value
  comes from another table or section (for example a model's prices
  under **model facts**).
- **model facts** -- a header naming the refreshed and shipped table
  dates (`none` when that table is absent), then one row per
  `[models.X]` entry with `context_window`, `fill` and the five price
  fields (`input_per_mtok`, `output_per_mtok`, `cache_read_per_mtok`,
  `cache_write_5m_per_mtok`, `cache_write_1h_per_mtok`), then one
  `cache_write_per_mtok.<duration>` field (`cache_write_per_mtok.30m`)
  per other cache-write lifetime a source prices, shortest first, as
  `key=value(source)`. The `/status/config` panel (schema 6) carries the
  same fields, in the same order, as each model's `prices` rows (`field`,
  `per_mtok`, `source`, `table_date`):
  - `context_window` comes only from `[models.X] context_window`
    (`model-config`); unset renders `none`.
  - `fill` is the `max_tokens` sent when a client omits one, on an egress
    that must send `max_tokens`: `model-config` for `[models.X]
    max_output_tokens`, else `default` (64000). `none` when the egress
    forwards an omitted limit as omitted, even with `max_output_tokens`
    set (that value then only sizes the window-gate reserve). With no
    running router this view cannot see a learned ceiling, so it never
    shows `learned`; its header says so, and `routectl doctor` and the
    status config panel show a learned ceiling lowering the fill.
  - each price names its source: `model-config` (`[models.X.pricing]`),
    `refreshed` or `shipped`
    (followed by `:YYYY-MM-DD`, the table date), or `derived` (the
    Anthropic-family 1h cache write, twice the input price). A field no
    source prices renders `unknown`, never `0`. Sources are fixed tokens:
    no provider id or host from a downloaded table is printed.
- **retry class policy** -- one row per failure class, showing the
  resolved `retry`/`fallback` pair and a `source=` tag:
  - `config` -- a `[retry.classes]` count set this class's retry cap;
  - `baked-default` -- no count, so the baked class default applies
    (see "Same-target retry counts"). Fallback is always the built-in
    value.
- **resolved tuning** -- one row per `[providers.X]` entry (pool members
  included), then the rows of each `[models.X]` entry that sets its own
  `stream_first_byte_timeout_ms`: one row under the nickname for a model
  on a provider, and one row per seat for a model on a pool, labelled
  `nickname#member` (the seat's runtime state key) and resolved through
  that member, so a member's own key beats the pool's. Each row prints the five tiered keys
  (`circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`,
  `request_timeout_ms`, `stream_first_byte_timeout_ms`) as
  `key=value(source)`, where `source` is the tier that supplied the
  value: `model`, `provider`, `pool`, `global` or `default` (the shipped
  default). A breaker turned off by `circuit_failures = 0` renders `off`;
  an unlimited RPM or an absent timeout renders `none`. These are the
  same values dispatch runs under, from the same resolver. The
  `/status/config` provider rows keep each entry's own configured
  `rpm_limit`, so a cap a member inherits from its pool shows here (and
  in `routectl doctor`), not there. An entry that sets none carries
  `null` there, which the dashboard reads as "not set here": a pool or
  default may still apply, so it is not a claim that no cap runs.
- **capability cells** -- one row per config-derived capability
  override, with its verdict and `source=override`. Override keys are
  shown normalized, not as written: a `beta:` key's flag prints in
  lowercase (`beta:Foo-Bar` shows as `beta:foo-bar`, the form overrides
  match on), a Bedrock request-bag key prints as its capability leaf,
  and rows are sorted by target, then by that normalized key.

```sh
routectl config show --effective
```

## Editing config from the CLI (`config set` / `config unset`)

`routectl config set <path> <value>` edits one scalar leaf of
`config.toml` in place, addressed by its dotted path:

```sh
routectl config set server.port 8788
routectl config set retry.classes.server-error 4
routectl config set usage.retention_days 30
```

The value's scalar type is inferred: `true`/`false` become a boolean,
an integer parses to an int, a decimal to a float, and anything else is
a string. A mistyped value is caught by the validation gate below, not
by inference.

Every edit runs the same pipeline before a single byte reaches disk, so
a rejected edit leaves the file byte-identical:

- **Version preflight.** A config older than the schema version this
  binary writes, or one with no `version` key, is refused outright with
  the same message the loader prints (see
  [Config schema version](#config-schema-version-version)); an edit never
  rewrites the file's schema as a side effect.
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
  provider's `format`, `base_url`, `cloud`, `credential`, `auth`,
  `account_id_ref` or `cloud_project_id`, or a pool's members -- prompts
  for confirmation first. `--yes` bypasses the prompt for
  scripting. With no terminal on stdin the prompt is DECLINED rather
  than read, so an automated caller neither hangs nor applies the edit
  by accident: the field list still prints, naming what was declined,
  and `--yes` is the non-interactive way to apply it. The same contract
  covers `provider add` and the `login` config offer.
- **Atomic, format-preserving write.** The write goes through a
  sibling advisory lock and a base-bytes revision check (an out-of-band
  edit between read and commit is reported as a conflict, with nothing
  written), then an atomic rename. Comments and key ordering survive.

A running daemon's file watcher picks up the rename and hot-reloads
automatically. Fields that only apply at startup (`[server]` bind and
listener auth, `usage.db_path`,
`usage.retention_days`, and the whole `[mitm]` block) are still written,
and `config set` prints exactly which ones need a restart to take
effect. A no-op set (the value already matches) writes nothing and
prints no restart notice.

### Removing an override (`config unset`)

`routectl config unset <path>` removes one override so the value falls
back to whatever it inherits -- a shared knob, or the built-in default
-- rather than assigning a new one:

```sh
routectl config unset retry.classes.server-error
routectl config unset retry.classes
```

It runs the identical pipeline as `config set` (version preflight, path
validation, full re-validation, high-consequence confirmation, atomic
format-preserving write, restart-required reporting, one audit event),
differing only in the mutation and in two path rules:

- **Recursive empty-parent prune.** Removing a key that leaves its
  parent table empty removes that table too, and so on up the chain --
  an empty override table is treated as absent (a leftover `[retry.classes]`
  with nothing under it would be ambiguous). A parent that keeps any
  sibling key or sub-table is left in place. In the first example above,
  removing the last count under `[retry.classes]` drops that table, and
  an otherwise-empty `[retry]` goes with it.
- **A whole table may be the target.** Unlike `set`, `unset` accepts a
  path that names a table node (the second example), dropping the entire
  override block in one call.

Removing a key that is not set writes nothing (there is no override to
remove) and reports no change. Comments and key ordering elsewhere in
the file survive, exactly as with `set`.

### Adding a provider (`provider add`)

`routectl provider add` writes one `[providers.<name>]` block through
the same pipeline. Name what the provider speaks with `--format`, or a
routectl-managed login with `--login`:

```sh
routectl provider add --format openai-chat --name grok \
  --base-url https://api.x.ai/v1 --api-key-env XAI_API_KEY
routectl provider add --format anthropic-messages --name claude-key \
  --api-key-env ANTHROPIC_API_KEY
routectl provider add --format anthropic-messages --name claude-fwd \
  --credential-source forwarded
routectl provider add --login codex --name codex
routectl provider add --login anthropic --label work --name anthropic-work
routectl provider add --login anthropic --label team-b --name anthropic-team-b \
  --pool anthropic
routectl provider add --format bedrock-converse --name bedrock \
  --region us-west-2 --secret-ref aws://profile/work
routectl provider add --format anthropic-messages --name mantle \
  --region us-east-1 --api-key-env AWS_BEARER_TOKEN_BEDROCK
```

- **`--format`** is `anthropic-messages`, `openai-chat`,
  `openai-responses`, `bedrock-invoke`, `bedrock-converse` or `gemini`.
  `openai-chat` and `openai-responses` need `--base-url` or `--region`;
  `anthropic-messages` defaults to `https://api.anthropic.com`;
  `bedrock-invoke` and `bedrock-converse` need `--region`; `gemini` always
  uses the public Gemini REST base.
- **`--region`** puts the entry on the AWS cloud instead of a base URL:
  it writes `cloud = { name = "aws", region = "<region>" }`. The
  `bedrock-*` formats reach Bedrock; `anthropic-messages`, `openai-chat`
  and `openai-responses` reach Bedrock's managed surface (Mantle). It is
  refused together with `--base-url`, `--login`,
  `--credential-source forwarded` or `--format gemini`.
- **`--auth`** is optional. Left out, it is `aws-sigv4` for an `aws://`
  credential, `x-api-key` for `anthropic-messages` over a base URL,
  `x-goog-api-key` for `gemini` and `bearer` for the others (a forwarded
  credential is always `bearer`, and so is a key on the AWS cloud). The
  chosen value is written to the entry and shown in the confirmation, and
  a combination the router refuses is refused with nothing written.
- **`--login`** is `anthropic` or `codex`. It runs the `login` flow after
  the confirmation and writes the vendor's own `format`, `base_url` and
  `auth` with `credential = "oauth://<vendor>"`; it takes no secret
  flag, `--base-url`, `--auth` or `--credential-source`.
- **`--label`** (only with `--login`) logs in that seat and writes
  `credential = "oauth://<vendor>#<label>"`. Without it the login is the
  default seat. If the config write fails after the login (another writer
  changed the file), the login is kept, nothing is written, and the error
  names the exact `routectl provider add --login ... --label ...` command
  to run again, with any value the shell would split single-quoted and
  `--config <path>` when the file was chosen with `--config` or
  `ROUTECTL_CONFIG`. When a value holds a control character, no command is
  printed: the error says to re-run the same `routectl provider add`
  command instead.
- **`--pool <name>`** (only with `--login`) puts the new entry in a
  pool in the same write: the provider block and the pool edit are one
  candidate, validated, confirmed and committed together, so either both
  land or neither does. An existing pool grows by the entry (an explicit
  edit, so its `accepts_new_logins` marker is not required and not
  changed); a missing pool is created with `members = ["<name>"]` and
  `accepts_new_logins = true`. Joining a pool whose first member sets
  `cloak_mode` or `cloak_strict_mode` copies those keys onto the new
  entry, and the confirmation shows them. A pool the entry cannot join
  (a different format, destination, auth or vendor) or a `pools` value
  that is not a table is refused before the login runs, with nothing
  written. If the write fails after the login, the rerun command the
  error names includes `--pool <name>`, under the same quoting rule as
  `--label`. `--pool` is refused without `--login`, and with `--format`
  or `--region`.
- The credential comes from `--api-key-env`, `--secret-ref`,
  `--api-key-stdin` (captured to the managed `file://` store) or, on a
  terminal, the matching conventional environment variable (not offered
  for an entry on the AWS cloud) or a hidden prompt. `--secret-ref` takes
  `env://`, `file://`, `oauth://`, `aws://profile/<name>` or `aws://chain`;
  anything else, `literal:` included, is refused without repeating the
  value.

The old `--kind` flag is refused with a message naming `--format` and
`--login`; nothing is written.

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

The optional `[cache]` block is the only operator switch for
auto-emission; past it, each provider's `cache_capability` decides. A
missing block keeps the default: auto-emit enabled, and the break-even
emission gate off.

```toml
[cache]
# Master switch for dispatch-path auto-emission of both cache
# breakpoints (front and terminal). Default true.
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

### Per-provider capability (`cache_capability`)

routectl only auto-emits a breakpoint to a provider it knows honors one.
`cache_capability` is a tuning key every format takes; an entry that
omits it gets its format's **conservative** default:

| `format` | `supports_top_level_cache_control` | `cache_hit_observable` |
|---|---|---|
| `anthropic-messages`, `base_url` on `api.anthropic.com` or the AWS `cloud` | true | true |
| `anthropic-messages`, any other `base_url` | false | false |
| `bedrock-invoke` | true -- see below | true |
| `bedrock-converse` | false -- see below | true |
| `openai-responses` | false (server-side auto-cache; no explicit breakpoint) | true |
| `gemini`, `gemini-cloud-code` | false (implicit prefix caching; no explicit breakpoint) | true |
| `openai-chat` | false | false |

When `supports_top_level_cache_control` is false, auto-emit of the
terminal marker is skipped for that provider. `cache_hit_observable`
says whether the upstream reports cache reads back, which the
cache-ignore report needs. An explicit `cache_capability` replaces the
default whole, on any format and any destination:

```toml
# An anthropic-compatible third-party host that DOES honor a top-level
# breakpoint and DOES report cache hits.
[providers.compat-anthropic]
format = "anthropic-messages"
base_url = "https://example.invalid/v1"
credential = "env://COMPAT_ANTHROPIC_API_KEY"
auth = "x-api-key"
cache_capability = { supports_top_level_cache_control = true, cache_hit_observable = true }
```

Both keys of the inline table are required.

**anthropic-messages on another host fails closed.** An
`anthropic-messages` entry whose `base_url` is on `api.anthropic.com`
(or that reaches Bedrock's managed Anthropic surface through `cloud`)
gets the optimistic `true/true` default -- those servers honor a
top-level breakpoint. An entry pointed at any **other** host is treated
as an Anthropic-compatible third party that may 400 on or silently drop
a top-level breakpoint. With no operator override, such an entry fails
closed (`false/false`) and is never auto-cached. An operator who knows
their host supports caching sets `cache_capability` explicitly to opt in.

**The two Bedrock formats default differently.** Bedrock honors prompt
caching only via per-block markers -- a `cachePoint` block on Converse, a
per-block `cache_control` on Invoke -- never a routectl-injected
top-level marker on the wire:

- `bedrock-invoke` gets `true/true`, so auto-emit
  runs. The bedrock-invoke egress lowers a routectl-injected top-level
  `cache_control` onto the last cache-eligible content block -- the
  per-block form Invoke honors -- and re-validates the resulting
  breakpoint sequence before the body ships. When no eligible block
  exists, the top-level marker is dropped and the request goes out
  uncached (logged at WARN) instead of 400ing. When the lowered
  arrangement would break the breakpoint rules (more than four, or a 5m
  ahead of a 1h), the target refuses the request locally
  (`cache-breakpoint-cap` / `cache-ttl-order`) and the chain moves to its
  next target, as anthropic-messages does. Only a caller's own top-level
  marker can reach that refusal: routectl auto-emits its marker only when
  the request carries no breakpoint at all.
- `bedrock-converse` gets `false/true`: a top-level marker lands
  in `additionalModelRequestFields` and never becomes a `cachePoint`, so
  auto-emit is skipped (`auto_skipped:no_capability`) rather than
  silently no-op'd, while hit usage is still reported back
  (`cache_hit_observable = true`).

Caller-supplied per-block markers are unaffected on either format and
still cache normally.

### Per-block front marker

Besides the terminal top-level marker, the dispatch path can place a
**per-block** cache breakpoint at the FRONT of the cacheable prefix (the
last wire-eligible system block, else the last custom tool definition).
Whether it does is fixed by the entry's format and destination; there is
no per-provider setting:

| Provider entry | Front marker |
|---|---|
| `anthropic-messages` on `api.anthropic.com` or the AWS `cloud` | emitted |
| `anthropic-messages` on any other `base_url` | not emitted |
| `bedrock-invoke`, `bedrock-converse` | not emitted |
| every other format | not emitted |

Only those `anthropic-messages` entries take it: that is the population whose
terminal marker is already auto-emitted, so the front marker adds no new
upstream shape. Everywhere else the front decision records
`auto_skipped:provider_disabled`. The global `[cache]` switch withholds
both markers.

**A cachePoint below the upstream's minimum prefix is a silent no-op.**
AWS (and Anthropic) only cache a prefix that clears a per-model minimum
token count; a marker emitted on a shorter prefix is accepted and simply
never produces a cache entry. routectl does not gate emission on an
estimated token count -- the estimate would be wrong often enough to
withhold markers that would have cached -- so a small-prefix request may
record `auto_emitted` while the upstream reports no cache write. Read the
decision alongside the reported cache-write tokens, not on its own.

An `anthropic-messages` entry on another host can opt in to the terminal
marker with `cache_capability` (above); it never receives a front marker.

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
# Capability keys declared ESSENTIAL globally: a target that only fails on
# one of them is routed away from rather than having the capability
# stripped in place. Default empty (the baked strip-vs-route table applies).
essential = []
```

- **`enabled`** (bool, default true) -- the master switch. When off, any
  learned entries stay resident but inert: both the learn path and the
  act path are skipped.
- **`decay_hours`** (u64, default 48) -- how long a learned negative acts
  before it lapses into a single re-probe. A field negative a free token
  count re-checks (on a lane that has such a count) does not lapse at that
  deadline: only that re-check, once its clear is durable, retracts it. A
  fixed 48h backstop past the decay deadline bounds this; a negative the
  re-check has not retracted by then lapses into the ordinary re-probe.
- **`inferred_window_hours`** (u64, default 1) -- how long a pending
  single-observation inferred signal waits for a confirming second
  observation before it resets.

The `routectl doctor` capability matrix flags a verified capability as
stale once it was last seen more than 14 days ago. The hint is fixed and
display-only: it never affects routing.

### Global essential capabilities (`essential`)

Some capabilities routectl knows how to **strip**: when a target only
fails on one of them, the request proceeds against that target with the
capability removed rather than routing elsewhere. `essential` is the
operator's veto over that choice. A key listed here is treated as
essential everywhere: a learned negative on it routes the request away to
a target that supports it, and nothing is stripped.

```toml
[capability]
essential = ["advisor", "context_management"]
```

- **Accepted keys** -- any capability key routectl knows: the well-known
  keys (`web_search`, `computer_use`, `structured_output`,
  `prompt_caching`, `thinking`) plus every key the strip table can act on
  (`advisor`, `context_management`). An unknown key fails config load
  naming the key, so a typo surfaces instead of sitting inert. Listing a
  key that already routes away is accepted as an idempotent declaration.
- **Rejections quote the key** -- the offending entry is rendered inside
  backticks (`` `advisor ` ``) so a defect you cannot see in prose, such
  as a stray leading or trailing space, shows its exact bounds. When the
  entry is an exact whitespace-or-case variant of an accepted key
  (`"advisor "`, `"Advisor"`), the error adds a `did you mean `advisor`?`
  hint naming which difference it is. A plain misspelling is already
  visible in the quoted key and gets no guess.
- **`reasoning_replay` is not accepted** and is rejected naming the
  carve-out. Replayed reasoning artifacts are dropped by the target
  lane's own replay path rather than by this key-only consult, so replay
  negatives never reach it -- accepting the key would pin a
  meaningful-looking no-op into the config vocabulary.
- **Tightening only** -- this list can turn a strip into a route-away; it
  can never turn a route-away into a strip. There is deliberately no
  config path to the loosening direction: making a capability droppable
  requires a strip recipe in code, since a strip with no recipe would
  dispatch a request that is guaranteed to fail.
- **Not a support override** -- `essential` changes only the RESPONSE to a
  learned negative. It does not mask a negative the way
  `[capability.overrides].force_supported` does, and it does not
  hard-drop a target the way `unsupported` does. The negative itself
  stays exactly as learned.
- **Global, unlike `[capability.overrides]`** -- the absence of a
  target-spec path segment is the point: droppability is operator intent
  that holds for every target, while support is a per-target fact.
- **Hot-reloadable, and it keeps learned state** -- the list is re-read
  on every router build, so adding or removing a key takes effect on
  reload. Flipping it does NOT expire the learned negatives it affects:
  they are still true, and re-probing one would send a request the
  operator just declared should route away.
- **When every lane refuses: the request fails** -- on a complete or
  stream request, if every target in the chain refuses a requested
  capability outright, routectl answers 400 `invalid_request_error` naming
  the capability before contacting any upstream, rather than walking the
  chain into the same rejection on each lane. A target refuses outright
  when its learned negative is self-identifying (the upstream named the
  capability; an inferred negative never counts), from the F1 or F2
  phase, and routes the target away: the capability is not strippable
  there, or it is listed in `essential`. So listing a droppable key here
  is what lets it fail the request rather than be stripped. A lane the
  operator pins (a beta floor that re-adds the capability's token) or
  masks with `force_supported` never counts, and count_tokens requests
  keep the soft tail. A lapsed negative whose re-probe slot is free sends
  that probe instead of failing; the refusal lifts on its own once a
  re-probe succeeds.
- **Escape hatches** -- `[capability.overrides.<spec>].force_supported`
  on one lane makes that lane serve the capability again (and so ends the
  refusal), and `routectl capability purge` removes a wrong learned
  negative. The refusal WARNs (`event=hard_exclusion`, see LOGGING.md)
  and counts on `rc_capability_hard_excluded_total`.

Operator capability overrides nest under this parent as
`[capability.overrides]` (keyed by `"provider"` or `"provider:nickname"`)
rather than a new top-level section. A missing `[capability]` block
deserializes to the defaults above, and an unknown key inside the block
fails config load rather than being silently ignored.

### `[capability.overrides]` IS the durable-decision surface

`[capability.overrides]` is where a capability decision **persists**.
Everything the daemon learns from live traffic is an OBSERVATION: it
lives in the running process's registry, is replayed from the usage
ledger at boot, and decays on its own schedule. An entry in
`[capability.overrides]` is an operator DECISION: it is re-read on every
router build, it never decays, and no amount of traffic can teach around
it.

That distinction is what the purge command below is scoped by, and it is
deliberate that there is no runtime store on the other side of it.
A second place to record the same intent would be a second place for it
to disagree with the config file, and the config file is the one an
operator can read, review, and put in version control.

```toml
# Pin a decision: this target does not support this capability, forever,
# regardless of what live traffic later suggests.
[capability.overrides."anthropic:sonnet"]
unsupported = ["web_search"]

# Or the other direction, on another target: mask a learned negative you
# believe is wrong. Both lists may also sit in one entry.
[capability.overrides."anthropic:opus"]
force_supported = ["prompt_caching"]
```

**Masking a learned verdict you read in `doctor` or `/status/health`.**
Copy the capability key exactly as the matrix column (or the status row's
`capability_key`) shows it; that string is the override key, envelope-field
verdicts included. The override's target is the `[providers]` entry, the
part of the lane before `#`, not the lane itself: for a verdict learned on
lane `anthropic#claude-opus-5-5` write `[capability.overrides."anthropic"]`
(or `"anthropic:<nickname>"` to scope it to one model). A learned verdict
carries a value (`shape:<v>` for a rewrite, `ceiling:<n>` for an output
limit; `-` for a drop or a catalog key), shown beside the key; the override
names only the key. Effort levels are learned one key per level, so to send
every effort level as the client wrote it on a lane, list each level's key
the lane learned:

```toml
[capability.overrides."anthropic"]
force_supported = [
  "field:output_config.effort.max",
  "field:output_config.effort.xhigh",
]
```

Besides the tool-type and well-known keys, the request side derives
`forced_tool_choice` whenever `tool_choice` forces a tool call (Anthropic
`any` / a named `tool`, OpenAI `required` / a named `function`, Converse
`any` / a named `tool`; never `auto` or `none`). Listing it under
`unsupported` routes forced requests away from that target before dispatch
while leaving every other request on it. The key always routes away and is
never stripped: routectl does not remove a forced `tool_choice` to make a
target accept the request.

### Dropping ONE learned observation (`routectl capability purge`)

To make the daemon forget one thing it learned -- without pinning
anything -- ask the running daemon:

```bash
routectl capability purge <lane> <capability>
# e.g.
routectl capability purge anthropic#claude-sonnet-4-5 web_search
```

`<lane>` is the learned lane exactly as `routectl doctor` prints it:
`<provider>#<upstream>`, the `[providers]` entry that egresses the
request and the upstream model id it sends.
Learned facts key on that pair rather than on a `[models]` nickname, so
every nickname for the same upstream on the same provider shares one
entry, and one purge clears it for all of them. For a pooled model the
provider half is the member entry of the seat that learned it. A value
that is not of that form (a bare nickname, for instance) is refused
locally with an error and a non-zero exit -- never reported as "nothing
to purge".

What it does and does not do:

- **It removes ONE observation, not a decision.** The next request
  re-evaluates that capability from scratch, and traffic that provokes
  the same rejection teaches the same negative again. If you want the
  decision to stick, write it under `[capability.overrides]` above --
  the command's own output says so.
- **It succeeds only once the removal is DURABLE.** The daemon writes the
  matching `cleared` row to its capability ledger and waits for that
  write to commit *before* it removes the entry from memory and reports
  success. That ordering is the whole guarantee: a purge reported as done
  has already been persisted, so the next restart cannot bring the
  verdict back. When the write cannot commit -- the usage database is
  unavailable or failing, or the command is interrupted mid-commit --
  nothing is removed, the entry keeps acting, and the command exits
  non-zero telling you to retry. Three outcomes are reported distinctly,
  because they need different responses from you:
  - **could not persist** -- check the daemon's usage database, then run
    the command again. Nothing changed.
  - **busy** -- another purge of the same key is in flight. Wait for it,
    then run the command again if the entry is still there. Nothing
    changed.
  - **configuration reloaded** -- the daemon reloaded while the command
    ran, so it acted on configuration that is no longer live. Run it
    again. Nothing changed.
  - **superseded** -- the entry changed while the command ran, so the
    daemon refused to delete a newer observation than the one you asked
    about. The clear was written, but nothing was removed.
  Retrying is always safe: every one of those leaves the entry exactly as
  it was, and a purge of an already-purged key is a clean no-op rather
  than a second removal.
- **Interrupting the command does not interrupt the purge.** Once the
  daemon has accepted the work it owns it: closing the connection, or
  Ctrl-C-ing the CLI, leaves the daemon to finish committing and to
  either complete the removal or leave the entry untouched. You lose the
  answer, not the outcome -- run the command again to see where it
  landed. A purge in flight briefly reports *busy* for that same key,
  which is the daemon telling you its own settlement still holds it.
- **A purge and a configuration reload exclude each other.** Whichever
  starts first wins; the other is told to retry, and neither leaves
  partial state. This is why a reload during a purge (or the reverse) can
  report busy rather than simply queueing.
- **It goes through the daemon, and requires one to be running.** The
  verdict lives in the daemon's memory, so an offline edit of the usage
  database would remove nothing that is currently steering routing, and
  writing to a database the daemon holds open risks the database itself.
  The command never opens it. With no daemon reachable, the command
  reports that clearly and exits non-zero, having changed nothing.
- **It is loopback-only, on the daemon's existing auth.** The control
  route sits on the same server, the same port, and the same
  `[server.auth]` gate as `/v1/*` -- it introduces no credential scheme
  of its own. Three checks narrow it further, none of them
  authentication, and each buying a DIFFERENT property (no one of them
  substitutes for another):
  - **a non-loopback caller is refused outright**, even on an
    `--unsafe-public` bind -- that flag exposes inference, not
    administration;
  - **an authority that does not name a loopback endpoint is refused.**
    This is the anti-DNS-rebinding check, the same one the `/status*`
    subtree carries. It is what stops an attacker who controls a hostname
    from pointing it at `127.0.0.1`: a page on that name is then
    *same-origin* with the daemon, so it needs no preflight, its caller
    really is loopback, and nothing about its body distinguishes it --
    only the authority it claims does. Every claim is checked and each
    must pass: every `Host` header value (a request may send the header
    more than once), and the request URI's authority (HTTP/2 sends
    `:authority` there and no `Host` at all). A `Host` value routectl
    cannot read as text is refused rather than ignored -- a claim it
    cannot evaluate has not been checked. A request carrying no authority
    at all -- plain origin-form from a hand-rolled client -- is
    permitted, because it makes no claim and the rebinding vector is a
    browser, which always sends one. Bracketed authorities must hold a
    real IPv6 literal, and a ZONE-SCOPED one (`[fe80::1%eth0]`) is
    deliberately unsupported: a zone index names a local interface rather
    than an endpoint, so it cannot be compared against the address
    routectl bound. Such a request is refused -- reach the daemon by its
    loopback or bound literal instead. The read-only `/status*` subtree
    checks the same claim sites with the same fail-closed handling, but
    against a WIDER allowlist: it also admits the address routectl is
    bound to, and under a wildcard bind (`0.0.0.0` / `::`) it degrades to
    accepting any well-formed authority naming the bound port -- on such
    a bind the listener token above carries the real access decision;
  - **a non-JSON `content-type` is refused.** This buys exactly one
    thing: a browser's preflight-free *simple* cross-origin request
    (`text/plain`, a form encoding) cannot reach the mutation, because
    requiring JSON forces a preflight the daemon never answers. It says
    nothing about the rebound same-origin case above.
- **The command always talks to loopback.** `[server] host` names what
  the daemon BINDS, so the command derives its destination from it rather
  than dialing it as written: a literal loopback address is used exactly
  as bound, a wildcard bind (`0.0.0.0` / `::`) becomes the matching
  family's loopback address. Anything else is refused locally, before your
  listener token is even read, and nothing is sent. That includes a
  specific non-loopback bind -- a daemon bound only to `10.20.30.40` is
  not listening on loopback, so quietly retargeting the call would send
  your token to whatever *other* process holds that port -- and it
  includes every hostname, `localhost` among them: a name picks no
  address family (`localhost` may be `127.0.0.1`, `::1`, or both), and
  resolving it would hand the choice of destination to a resolver. Set
  `[server] host` to an explicit `127.0.0.1` / `::1`, or to a wildcard. The command also
  bypasses any configured HTTP proxy and never follows a redirect, for
  the same reason: a control call is a conversation with a process on
  this machine.
- **Purging something that is not there is a clean no-op**, reported as
  such and exiting zero, so re-running it is safe.
- **It touches learned entries only.** It never edits your config, your
  `[capability.overrides]` cells.

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
format = "openai-chat"
base_url = "https://example.invalid/v1"
credential = "env://COMPAT_A_API_KEY"
auth = "bearer"
# Inherits the global switch (reduction ON for this provider).

[providers.compat-b]
format = "openai-chat"
base_url = "https://example.invalid/v1"
credential = "env://COMPAT_B_API_KEY"
auth = "bearer"
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

#### Independence from advisory trim

The lossless minifier's switch and the advisory trimmer's controls are
SEPARATE mechanisms and never share a switch:

- `[reduction] enabled` is the **global master switch for the lossless
  minifier**, and each `[providers.X] reduction_enabled` is a per-provider
  override of that one transform.
- The advisory trimmer's live-cut switch, when trimming gains one, is a
  NEW key, distinct from `[reduction] enabled` and from every
  per-provider `reduction_enabled`.

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

## Steady-state advisory trim

routectl carries a deterministic, front-anchored steady-state context
trimmer that is **advisory only** in this release: it computes what a
cache-coherent prefix cut WOULD look like for a long-running
conversation, but never mutates a dispatched request. Its proposal is a
pure function of four fixed defaults; no config key tunes them, and an
always-on advisory recorder that never mutates has no "off" state:

| Default | Value | Meaning |
|---|---|---|
| trigger | 100000 tokens | estimated total at or below which no trim is proposed |
| minimum cut | 20000 tokens | the elided span must free at least this much |
| head kept | 2 messages | leading messages never elided |
| tail kept | 6 messages | trailing messages never elided |

## Reading usage (`routectl usage`)

`routectl usage` is the read surface over the usage database. It opens
the DB **read-only** (never writes, never migrates, safe to run while the
daemon is live) and prints an aligned ASCII report.

```bash
./routectl usage                       # multi-window summary (default)
./routectl usage --today               # one calendar window
./routectl usage --this-week --by provider
./routectl usage --last-24h            # the 24 hours ending now
./routectl usage --since 2026-06-01 --until 2026-06-07 --detail
./routectl usage --all --db /path/to/usage.db
```

**Windows.** Exactly one window selector may be given:

| Flag           | Range                                            |
|----------------|--------------------------------------------------|
| `--today`      | local midnight today -> now                      |
| `--this-week`  | Monday 00:00 of the current ISO week -> now      |
| `--this-month` | the 1st at 00:00 -> now                           |
| `--last-24h`   | exactly 24 hours ending now                      |
| `--last-7d`    | exactly 7 days (168 hours) ending now            |
| `--all`        | all recorded rows                                |
| `--since D [--until E]` | `D` 00:00 -> end of `E` (or now if omitted), local |

"Local" is the host's time zone, read when the command starts (and per
request on `/status/usage`, `/status/query` and `/status/doctor`): the `TZ`
environment variable when set, where an empty value means UTC, otherwise
`/etc/localtime` (Windows uses the system zone). If the zone cannot be read,
the windows and printed timestamps use UTC and one WARN line says so. On a
day with a daylight-saving change, "today" spans its real 23 or 25 hours; a
midnight that falls in a skipped hour starts at the first time that exists.
`--last-24h` and `--last-7d` are trailing windows: fixed durations ending
now that never read the time zone, so they stay exactly 24 and 168 hours
across a daylight-saving change.

The ISO week starts **Monday**. With no window flag and no `--since`,
the command prints a multi-window summary (today / this week / this
month / all time) as separate blocks. The only trailing windows are
`--last-24h` and `--last-7d`.

**Breakdown.** `--by model|alias|provider|seat` rolls the rows up to
that dimension; omit `--by` to group by model. Every report ends with a
`total` row. `seat` is the credential seat recorded with each request;
requests with no recorded seat group as `(unattributed)`. The report reads
the same grouped query as `/status/query`, so for the same window and
grouping its groups and figures match the dashboard's. `--detail` adds
extra columns: cost, `ctx_peak` and `ctx_avg` (the largest and the mean
per-request prompt: fresh input plus cache read plus cache write, over
requests that reported any of the three; `-` when none did), `cache_wr`
(cache-write tokens of every lifetime), `ttft_p50` and `ttft_p95`
(nearest-rank time to first token over streamed successes), `tok/s`
(output tokens of streamed successes over their summed generation time)
and server-tool counts.

**Cache writes.** The `input` column, the `cache_wr` column and the
cache-hit rate count cache writes from every row: a row written before
usage schema 21 contributes its 5-minute and 1-hour columns, a newer row
its recorded write total, and no write is counted twice. `cache_wr`
reads `-` when no row in the group reported a write and `0` when one
reported an observed zero.

**Cost.** Cost is derived at read time from the effective prices for
each row, field by field, in the order [Model
prices](#model-prices-modelsxpricing) gives: `[models.X.pricing]`, the
refreshed table, the shipped table. The tables are read once per `routectl usage` run and once per
dashboard query, so a table `routectl prices refresh` writes while the
daemon runs prices the next query with no restart; a copy that fails
validation keeps the previous table. The dollar column has three states:

- `$X.XX` -- an API-key provider whose upstream resolved a price for
  every dimension the row used. A dimension with nonzero usage and no
  price makes the row `n/a` instead, whether the rates come from a price
  table (`refreshed`, `shipped` or `derived`) or from a partial
  `[models.X.pricing]` block: a source that lists an input price but no
  cache-read price says nothing about cache reads being free. Reasoning is
  billed by the provider kind
  **persisted with the row**, never by what the provider is configured as
  now, so re-kinding a provider in place cannot reprice history.
- `n/a (subscription)` -- a managed-OAuth provider (its
  `[providers.X] credential` starts with `oauth://`). Subscription usage
  has no per-token dollar cost; the **quota line** under the table is the
  real spend signal.
- `n/a` -- an API-key provider whose upstream no source prices: no
  `[models.X.pricing]` and no table row
  (the tables do not list it, the provider's host is ambiguous, or the
  provider entry is gone and the row carries no persisted provider kind);
  or a priced row that used a dimension with no rate. Never shown as `$0`.

**Subscription-equivalent value.** Where the effective rates cover every
dimension a subscription row actually used, the cell also reports what
that usage would have cost at API rates: `n/a (sub ~$12.34)`, or
`$5.00 (+sub ~$12.34)` in a group that mixes both. The two figures are
never added together -- the dollar amount is real spend, the `~$` amount
is current replacement value.

The equivalent is **complete or absent**: if any dimension with a nonzero
count on the row (input, output, separately-billed reasoning, billed cache
reads, either cache-write bucket) has no price, the cell falls back to the
plain `n/a (subscription)` / `+sub` string instead of showing a partial
figure. A known `$0` price counts as a price: that dimension adds nothing
and the figure stays complete. A model no source prices shows no
equivalent, never `~$0.00`. A group
whose subscription rows recorded no tokens at all resolves a measured
`~$0.00`: nothing was used, so nothing is the honest equivalent.

**Cache writes with no lifetime.** Cache writes are priced per lifetime,
so written tokens the upstream reported with no lifetime cannot be
priced, and routectl never guesses one. Cost is resolved per group of
rows sharing a model, provider, upstream, alias and provider kind in
the window, so one such row leaves that whole group without a price or
an API-equivalent value. A `routectl usage` line made only of such
groups reads `n/a` (API key) or plain `n/a (subscription)` (no `~$`);
a line that also covers priced groups shows only their figure. On the
dashboard such an API-key group reads `unpriced`, and a view that mixes
it with priced groups reads `partial`. The tokens still count in
`cache_wr` and the dashboard's cache-write figure. A streamed Anthropic turn in which a server tool
such as web search adds content partway through the turn produces such
a row: the closing event reports the turn's whole cache-write total
with no per-lifetime split, and the split on the opening event covers
only the writes made before the tool ran, so the row records the total
with an unknown lifetime.

A footer reports the window's cache-hit-rate
(`cache_read / (cache_read + input)`) and error count, then one `walk:`
line with the window's dispatch walk counts summed over its request rows:
`request-caused failures` (failed attempts the request itself caused) and
the targets passed over by an `excluded endpoint`, `model`, `target` or
`probe-run` exclusion. A row recorded before usage schema 20 adds 0. When
the ledger recorded a gap inside the window (request rows dropped before
they were written), the line ends with a `lower bound` clause naming the
gap rows and dropped rows: every count on it is then a minimum.

The `--detail` reduction and k-suppressed-session blocks end with a
`validity:` note: they report counts seen, and a ratio or cost claim needs
the running service's usage-channel `dropped_full`, `dropped_closed`
and `write_errors` counters flat across the window.

`--detail` also ends the output with one `cache-ignore:` block, the same
report the daemon logs and `routectl doctor` shows in its `cache`
section. Its window is always the last 6 hours ending now: `--today`,
`--since` and the other window flags do not change it, and a
multi-window summary prints it once. One line per lane (provider, seat,
model, provider kind) on an `anthropic-api` or `bedrock` provider whose
cache hits are observable, with its verdict (`ignoring`, `clear` or
`insufficient`) and its `evidence`, `active` and `sessions` counts; an
`ignoring` lane is marked "markers sent, no cache activity", which says
only what the ledger shows. When no lane had a qualifying turn (consecutive
marked turns within the cache lifetime) the block says so in one line. A
final line per reason names the providers that are not evaluated: those
whose cache hits are not observable, and `bedrock-converse` providers, where a caller's top-level-only cache marker is
forwarded inert. When the ledger cannot answer within 2 s the block shows
`unavailable: <class>` and the command still exits 0.

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

### Context meter opening accuracy (`--opening-accuracy`)

`routectl usage --opening-accuracy` reports how close the Anthropic context
meter's anchored opening count came to the terminal input the upstream
later reported. It reads the opening diagnostics each Anthropic-ingress
streaming row carries in its `extra` column (see
[LOGGING.md](LOGGING.md) "Usage accounting log shapes"), opens the
database **read-only**, and needs an explicit window:

```bash
# after a deploy: a window that starts after BOTH hops run the new build;
# rerun at any time, it writes nothing
./routectl usage --opening-accuracy --since 2026-09-25
./routectl usage --opening-accuracy --since 2026-09-25 --until 2026-09-26
./routectl usage --opening-accuracy --today --db /path/to/usage.db
echo "exit status: $?"
```

With no window flag and no `--since` it refuses rather than guessing. It
cannot be combined with `--by`, `--detail` or `--k-calibration`, and it
leaves the default `routectl usage` output unchanged.

**Universe.** Every Anthropic-ingress streaming row whose start falls in
the window, each counted in exactly one category; the report prints the
sum so the categories reconcile to the total:

- `legacy` -- no opening-diagnostics key at all (written before they
  existed);
- `unclassifiable` -- diagnostics present but unreadable, by a fixed kind
  (`oversized` past 16 KiB, `invalid_json`, `not_an_object`,
  `opening_present_missing`, `opening_present_not_bool`,
  `no_opening_contradicted`, `opening_source_missing`,
  `opening_source_unrecognized`);
- `no opening` -- `opening_present = false` and nothing else;
- `known` -- by `opening_source`.

Only the producer's own labels are ever printed: an unknown reason or
terminal label on a known row reads `<unrecognized>` and is counted as a
data defect, never echoed. Lane keys (`kind:provider/model@upstream`, from
the row's persisted `provider_kind`, `provider`, `model` and `upstream`
columns, so a repointed nickname stays a separate lane) are reduced to
printable ASCII and truncated.

**Anchored cohort and rate.** The cohort is exactly the rows whose
`opening_source` is `anchor`, including one whose `opening_present` marker
is missing or wrong (a non-pass, `opening_marker_inconsistent`). The source
is chosen before the terminal usage arrives, so no outcome removes a row.
A row passes only when its marker is consistent, the turn completed
(`outcome = ok`), its opening count is stated, its `terminal_source` is
input evidence (`explicit_final` or `vendor_opening`), its cache-inclusive
`terminal_input` is positive, and `|opening - terminal| * 100 <= 5 *
terminal`. Every other anchored row is a non-pass with one reason
(`outcome_not_ok`, `opening_unstated`, `opening_malformed`,
`terminal_missing`, `terminal_malformed`, `terminal_unsupported`,
`terminal_zero`, `outside_5pct`). The rate is `pass * 100 >= 95 *
anchored`, in integers (19 of 20 passes, 18 of 20 fails), over the cohort
alone; the ledger's cache-exclusive `input_tokens` column is never used.

**Verdict and exit status.** One `verdict:` line, followed by the
provenance notice:

| Verdict | Exit | Meaning |
|---|---|---|
| `PASS` | 0 | numerical pass, complete window, no data defect, every anchored terminal vendor-verified |
| `FAIL` | 1 | numerical rate below 95% |
| `INSUFFICIENT` | 2 | no anchored rows, or legacy rows in the window (choose a post-deploy window) |
| `NO_LEDGER` | 2 | no database, or one this binary cannot read yet |
| `INDETERMINATE` | 3 | numerical pass, but unclassifiable rows or data defects veto it |
| `BACKEND_PENDING` | 4 | numerical pass, but some anchored row's terminal was relayed by a compatible endpoint |
| error | 5 | no window given, or the database cannot be read |

**Provenance.** A terminal relayed by an Anthropic-compatible endpoint --
a routectl back hop included -- is only what that endpoint reported
(`terminal_vendor_verified = false`). The report counts every such
terminal, whatever the turn's outcome, globally, within the cohort and per
lane. It cannot establish how a back hop's backend measured its counts;
that needs a separate, operator-run check of both hops. Exit status 0 is
not an authorization to deploy.

Also printed: opening-to-terminal error buckets (0-5%, 5-10%, 10-20%,
>20%, and `n/a` where no comparison is possible) per opening source, the
same buckets for cold-start rows (anchor reason `cold`), and per lane the
row categories, anchored/pass counts, unverified reports, defect rows and
source, reason and `terminal_source` counts.


### Rolling the usage database back (`routectl usage downgrade`)

A binary refuses a usage database stamped with a newer schema version than
it understands, so rolling a host back to an earlier release needs the
database re-stamped first. Each schema step it undoes only appended
nullable columns or created a table or index that the earlier binary never
names, so the rollback is a re-stamp, not a rewrite:

- `--to 22` undoes v23: the `account_readings` table (the account usage
  reading history), its rows and its index stay in place. A v22 binary never
  reads or writes it. Starting the v23 binary again reads the history back.
- `--to 21` undoes v23 (when the file is at v23) and then v22: the three
  read indexes on `requests` (`idx_requests_agg`, `idx_requests_errors`,
  `idx_requests_quota_seat`) stay in place. A v21 binary never names them,
  and SQLite keeps them up to date on its inserts, so starting a v22 or
  later binary again needs no rebuild.
- `--to 20` undoes every step above the file's version down to v21, then
  v21: the two cache-write columns on `requests` (`cache_write_tokens`,
  `cache_write_lifetimes`) and their values stay in place. A v20 binary
  names every `requests` column it writes, so its rows leave them NULL, and
  it reports only the older `cache_write_5m` / `cache_write_1h` columns, so
  cache writes recorded at v21 do not show in its reports. Starting a v21 or
  later binary again reads them back.
- `--to 19` undoes every step above the file's version down to v20, then
  v20: the five walk-count columns on `requests` (`request_caused_failures`,
  `excluded_endpoint`, `excluded_model`, `excluded_target`,
  `excluded_probe_run`) stay in place. A v19 binary names every `requests`
  column it writes. A file already stamped 20 that still carries the
  cache-write columns is accepted.
- `--to 18` undoes every step above the file's version down to v19, then
  v19: the six skip-count columns on `requests` stay in place as well. A
  v18 binary names every `requests` column it writes. A file already stamped
  19 that still carries the walk-count and cache-write columns is accepted.
- `--to 17` undoes every step above the file's version down to v18, then
  v18: the eight request-observation columns on `requests` and the
  `ledger_gaps` table stay in place. A v17 binary names every `requests`
  column it writes and never reads `ledger_gaps`. A file already stamped 18
  that still carries the skip-count, walk-count and cache-write columns is
  accepted.
- `--to 16` undoes every step above the file's version down to v17, then
  v17: the two capability-ledger columns stay in place as well.

```bash
systemctl --user stop routectl        # or however the daemon runs
./routectl usage downgrade --to 22    # [--db /path/to/usage.db]; --to 21, 20, 19, 18, 17 or 16 for an older binary
# install and start the previous binary
```

The command is **offline only**: it refuses while the daemon or any other
process (a `routectl usage` viewer, a status poll) has the database open.
Before re-stamping, each step checks that the tables it covers are exactly
the earlier shape plus the columns that step added, and refuses anything
else. It reads only the schema, never the rows, so it takes the same time
on any database size. All steps commit in one transaction, so a refusal
at any step leaves the file exactly as it was. Nothing is dropped, so
starting the newer binary again migrates the file straight back to the
current version without re-adding anything; rows the older binary wrote
in between carry NULL in the newer columns and read as legacy rows. Use
this command rather than editing the version with a hand-run `sqlite3`,
which can disturb the database's WAL sidecar files.

The first start of a release carrying the legacy capability cleanup runs a
one-time data migration: it deletes the legacy capability observations
(rows with no vocabulary version) that precede the newest replay boundary,
which the replayer already ignores, and logs the count once. Boundary
markers, rows at or after the newest boundary, and rows carrying a
vocabulary version are kept. It changes no schema: the schema version is
unchanged, and `routectl usage downgrade` still works. The deleted rows can be recovered only from a backup taken
before the upgrade, so snapshot the database first if they matter.

## Diagnostics (`routectl doctor` and `routectl provider probe`)

`routectl doctor` and `routectl provider probe` are the two read-only
health surfaces. Both mutate NOTHING -- config, credentials, and the usage
DB are byte-identical after a run -- and both share
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
carries a `schema_version` (currently `4`) and its shape is UNSTABLE before
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

`window` is one of `today`, `week`, `month`, `last24h`, `last7d`, `all`
(`last24h` and `last7d` are the trailing 24 hours and 7 days ending at the
request, the rest local calendar windows, as for `routectl usage`; a series
on a trailing window starts its buckets at the window's start); `group_by` is one of
`model`, `alias`, `provider`, `seat` (`seat` groups by the credential
identity each request was served under; a request with none recorded reads
as `(unattributed)`); optional `alias` and `provider` keys narrow
the result to one routing alias or served provider.

Each group carries its `label` and `metrics`. A `seat` group also carries
three identity fields, resolved from the served config the same way
`/status/accounts` resolves them: `account` (the recorded seat, the key
`/status/accounts` lists it under), `provider_entry` (the `[providers]` entry
it is reported under) and `login_label` (the `<label>` of an
`oauth://<provider>#<label>` login, null for an unlabeled one). A recorded
seat the config no longer names keeps its `account` with null
`provider_entry` and `login_label`, and the `(unattributed)` group carries
null for all three. Read these fields; never split the label. Groups of the
other groupings carry no identity fields. Do not build a durable
integration on the response shape before 1.0.

The server runs one query read at a time. A query that arrives while another
is still reading answers at once with an unavailable panel carrying `db_busy`;
retry it once the first answers. The read has 10 s; a read that runs longer
answers `query_timeout`. The budget starts when the body
has been read, so time spent waiting for a worker counts against it, and a
read whose budget ran out before it started answers `query_timeout` without
touching the ledger. A client that disconnects mid-read stops its read, so the
next query does not wait out the abandoned one. The other `/status` panels do
not share this limit and keep answering while a query reads.

Every metric set (groups, totals, and each series bucket) carries both cost
channels side by side: `cost_usd` is real spend under `cost_status`, and
`equivalent_cost_usd` is the API-equivalent value of the set's subscription
rows under the same complete-or-absent rule the CLI cell applies. Both are
`null` rather than `0` when nothing contributed, and neither is ever folded
into the other.

The groups and `totals` are priced over the whole window, and are the same
whether or not a `bucket` series was requested. Each series bucket is priced
and derived over its own requests alone. The counts of the buckets (requests,
tokens, errors) sum to `totals`, but their costs and cache-hit shares need
not. A cache write with no recorded lifetime leaves the window's cost of its
requests unpriced, while a bucket that holds none of that write prices the
same requests' share in it; and a set of requests sharing the keys listed
under `cache_hit_pct` below counts toward a bucket's hit share only when it
reported cache reads in that bucket.

Every metric set also carries `cache_write_tokens`, the cache-write tokens
of every lifetime summed over both ledger eras, and `cache_write_present`,
the count of requests that reported a cache-write value (an observed zero
counts, a request reporting nothing does not). `cache_write_5m` and
`cache_write_1h` sum only rows written before usage schema 21; newer rows
leave those columns empty, so read `cache_write_tokens` for the write
volume. The `/status/usage` panel (schema 7) carries the same two fields,
with the same meaning, on its `totals` and on every group, and the
dashboard's cache-write figures read `cache_write_tokens`.

Every metric set also carries `errors_by_class`, an object mapping each
failure class to its count of error requests. Its counts sum exactly to
that set's `errors`, under the same `alias` / `provider` narrowing and in
each series bucket. An error request with no recorded class counts under
`unclassified`, and a class token this build does not know keeps its own
key. A set with no errors carries `{}`. The class is a measure, never a
`group_by` token. The `/status/usage` panel's `errors_by_class` uses the
same class vocabulary.

The timing, throughput, context and cache-hit figures of every metric set
mean what their names say. `ttft_avg_ms` and `ttft_max_ms` are the mean and
the largest time to first token over streamed successes; `latency_avg_ms` and
`latency_max_ms` the mean and the largest end-to-end latency over every
request. None of them is a percentile. `throughput_tok_s` is the output
tokens of streamed successes divided by their summed generation time (the
latency after the first token), so a long slow stream weighs as much as its
tokens. `ctx_avg` and `ctx_peak` are the mean and the largest per-request
prompt: fresh input plus cache read plus cache write, over requests that
reported any of the three. `cache_hit_pct` is the share of prompt tokens
served from cache, weighted by tokens, the same rule as the `routectl usage`
hit rate. It is decided per group of requests that share a model, provider,
upstream, alias, provider kind and seat: when at least one request in such a
group reported cache reads, every prompt token of the whole group counts,
including requests that reported none; a group with no cache-reporting
request is left out. Each set
also counts `cache_read_present` and `server_tool_present` (requests that
reported a cache read or a server-tool map, a reported zero included) and
`client_disconnect_pre_dispatch` (client hangups before a model was served).

Every metric set also carries `missing_price_dimensions`, a sorted list of
the dimensions its requests used that had no price: the reason `cost_usd`
or `equivalent_cost_usd` is absent or covers only part of the set. It is
`[]` when every used dimension was priced. The names are the ones
`routectl doctor` uses: `input`, `output`, `cache read` and
`cache write <duration>` (for example `cache write 30m`). Reasoning tokens
billed at the output rate count as `output`. Three other reasons have their
own names: when no price source covers a model at all, every dimension its
requests used is listed; cache writes with no recorded lifetime, or a
lifetime split above its total, read `cache write (unknown lifetime)`; and
reasoning tokens on a request whose provider kind is unknown read
`reasoning`. A request with no recorded provider adds nothing. Totals and
series buckets carry the union of their requests' names. Naming a missing
dimension never adds any API-equivalent amount to `cost_usd`.

### `provider probe [<name>]` -- reachability, free-only

`routectl provider probe` reports one reachability outcome per configured
provider (or just `<name>` when given). It is FREE-ONLY by construction:
it never makes a billed upstream call and never mutates a credential.

- **No free endpoint -> WARN, never a silent charge.** A provider kind
  with no cheap reachability check reports WARN with a "cannot verify
  without a billed call" reason -- routectl will not spend money to turn a
  WARN into a PASS. This is a warning (exit 0), not a failure.
- **Forwarded providers are SKIPPED.** A `credential = "forwarded"`
  provider (see
  [Forwarded credential](#forwarded-credential))
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
remediation that NAMES the fix (`` run `routectl login anthropic` ``,
`` run `routectl init` ``, ...) for the operator to run -- the command
diagnoses, it does not mutate.

The battery sections, in render order:

| Section | Checks |
|---|---|
| Provider activation (`inventory`) | Whether each known provider's credential is present and usable; a configured route that depends on an unusable provider is a WARN. |
| Config schema version (`version`) | The config's schema version against the binary; a too-old or versionless config FAILs with a move-aside-and-`routectl init` remediation, a too-new one FAILs with an upgrade remediation, a present-but-broken config FAILs rather than reporting all-PASS. |
| Config validation (`config`) | The static validator suite (the same one `config check` runs) plus a read-only secret-presence scan. Every message names the ref SCHEME, never the secret value, path, or env var name. When the config folder still holds any of `catalog_overlay.json`, `catalog_overlay.json.lock`, `catalog_state.json` or `catalog_import_state.json`, one PASS `leftover catalog files` row names the ones present: routectl no longer reads them, prices come from `routectl prices refresh` and `[models.X.pricing]`, and the files can be removed. The row is informational and never moves the exit code. |
| OAuth credentials (`auth`) | Stored OAuth seats and their expiry; no seats logged in is a WARN, an expired seat is a WARN, a credential store that will not open is a FAIL. On the daemon's `/status/doctor` only, each dispatch seat whose credential account the upstream rejected (and the router parked) is one WARN `parked seat <state_key>` naming the reason (`credential-expired` / `credential-invalid`) and when the park lapses. A park covers the whole account (an OAuth seat, or one `[providers]` entry for any other credential), so N models on one parked account are N rows; the CLI cannot see daemon state and reports no parks. A park lifts on a success through any model on the account, after its hour lapses, or on a reload that may have changed its credential: a credentials reload lifts each OAuth seat whose stored record changed (routectl's own token refresh writes the rotated record and then updates its in-memory copy to match, so the reload that write triggers sees no change for that seat), and an accepted config reload lifts the non-OAuth account of each `[providers]` entry it added, removed or changed, and no OAuth seat (an unchanged entry keeps its park, so a secret rotated behind an unchanged `env://` or `file://` ref waits for the re-test). Also on `/status/doctor` only, each dispatch seat whose credential the daemon holds as unusable is one WARN `unusable seat <state_key>`: its detail names the reason (`credential_unusable`), the cause (`not_logged_in`, `unreadable`, `expired` or `revoked`) and the UTC time the seat was first marked, and its remediation is the fix: `routectl login <provider> --label <label>` for a labeled OAuth seat, `routectl login <provider>` for an unlabeled one, and for an `env://` or `file://` credential the provider entry and scheme to correct before a config reload (never the variable name, path or value). A label with any character outside `A-Z a-z 0-9 _ . -` is single-quoted so the command pastes into a shell as one word, and an `oauth://` ref whose provider `routectl login` does not accept gets a correction naming the entry and the provider token instead of a login command. Like a park, the mark covers the whole account, so N models on one unusable account are N rows. The mark lives only in the running daemon's memory, so the CLI `routectl doctor` cannot see it and reports no unusable seats. |
| OAuth seat pools (`pools`) | One row per `[pools.<name>]` block naming its members and the seat each one's ref names, its `seat_selection`, whether it accepts new logins, and any member the credential store holds no seat for; then one purely informational PASS row per `oauth://` reference on a provider entry NO pool claims (a bare ref pins the default seat, a `#label` ref that one seat; neither carries a strategy). A pool missing a member's stored credential is a WARN naming the member, and a pool no member of which has one is a FAIL -- requests to every model naming it are refused locally with `credential_unusable` until a member has a credential. When the credential store will not open, presence reads as unknown and the section claims neither (the `auth` section owns that FAIL). LIMIT: this section builds no router, so it reports CONFIG-and-STORE presence -- a member whose credential is stored but which the daemon could not use is visible only to a surface that has a built router: the daemon's `/status/doctor` names each such seat in the `auth` section as `unusable seat <state_key>`. |
| OAuth seats (`seats`) | Stored OAuth seats no provider entry's `oauth://` ref reaches surface as a WARN naming the seat. Matching is by full seat identity: every ref names exactly one seat, so a bare `oauth://<provider>` ref vouches for the DEFAULT seat alone and a labeled seat is reached only by a ref naming it -- in a pooled configuration, that pool member's own pinned ref. Read-only -- a stored credential is NEVER auto-deleted or refreshed, and the finding names only the seat key, never token material or a storage path. |
| Managed secrets (`secrets`) | Managed secret files not referenced by any provider surface as a WARN. The scan is a read-only directory diff; a stored secret is NEVER auto-deleted. |
| Provider reachability (`probe`) | One finding per provider through the SAME probe seam `provider probe` uses, so the two surfaces never diverge on status, detail, or remediation. |
| Capability (`capability`) | The learned-capability findings NOT absorbed by the capability matrix panel below: a WARN when the config layer could not be parsed (so the panel is honestly degraded rather than silently empty), and, for a config that loads with `[capability] enabled = false`, one WARN `learning` saying nothing is learned and no learned verdict or limit is applied (the daemon's `/status/doctor` panel, and so the dashboard doctor tab, carries the same row for the config it accepted). Never emits a FAIL, so it can never flip the exit code. |
| Cost pricing (`pricing`) | One row per configured model listing its five prices (input, output, cache read, cache write 5m, cache write 1h), then a `cache write <duration>` price for each other cache-write lifetime a source prices (`cache write 30m`), each with the source that gave it: `model-config`, `refreshed`, `shipped`, `derived` or `unknown`, resolved as in [Model prices](#model-prices-modelsxpricing) and as `routectl usage` prices. An unknown price reads `unknown`, never `$0`; a known zero reads `$0.00`. The row ends with the date of each price table read (`none` for an absent one). A model no source prices reads unpriced. A model on a pool is priced through the pool's first member, as usage bills it. When a `[models.X.pricing]` block sets the 5m or 1h write under both `cache_write_5m_per_mtok` / `cache_write_1h_per_mtok` and a `cache_write_per_mtok` key, at different values, the row notes that the legacy key's value is used; the row stays PASS. A managed subscription (`oauth://`) row is billed by seat; its prices are the basis of the API-equivalent value, and it adds whether that value is complete or which dimensions it lacks. The CLI reads `prices.json` once per run and `/status/doctor` uses the running daemon's. Nothing from a price table but its prices and date is shown. Never FAILs, so it can never move the exit code. |
| Model knobs (`knobs`) | One row per configured model naming the `max_tokens` fill its egress sends when the caller omits one and WHERE it comes from, in the same `model-config`, `learned`, `default` vocabulary `config show --effective` and the status config panel use: `model-config` when `[models.X] max_output_tokens` is set, `learned` when the resident ceiling learned from an upstream refusal on one of its lanes lowers that value (or the egress baseline), else `default` (the Anthropic-shape egress baseline). A learned value is shown only when capability learning is enabled, the lane's kind is one field repair admits, and no `force_supported` override masks `max_tokens` for the model. An egress that forwards caller omission untouched has no fill: with `max_output_tokens` unset the row reads `unset`; with it set the row names the value as `model-config` and says it sizes the window-gate reserve (on `bedrock`, also the limit Converse sends under legacy `enabled` thinking), not a fill. The learned value is the resident ceiling; the pre-flight's per-request gates (writer health, verdict quorum) are not reflected, so a given request may still go unclamped. Every row names the model's `(kind, upstream)`. Never FAILs, so it can never move the exit code. |
| Resolved tuning (`tuning`) | One PASS row per `[providers.X]` entry (pool members included), named `provider <id>`, then one per `[models.X]` entry that sets its own `stream_first_byte_timeout_ms`, named `model <nickname>` (one row per seat, named `model <nickname>#<member>`, for a model on a pool). The detail lists `circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`, `request_timeout_ms` and `stream_first_byte_timeout_ms` as `key=value(source)`, the same cells and tier tokens (`model`, `provider`, `pool`, `global`, `default`) `config show --effective` prints; a breaker turned off renders `off`, an unlimited RPM or absent timeout `none`. Read from the config alone. Never WARNs or FAILs, so it can never move the exit code. |
| Cache markers (`cache`) | Read-only from the usage ledger's last 6 hours, on the CLI and on the daemon's `/status/doctor` alike. A lane is a provider, seat and model on an `anthropic-api` or `bedrock` provider whose cache hits are observable. One WARN `lane <provider>[#<seat>] <model>` per lane where at least 3 turns carried the caller's own cache markers and none read or wrote any cache: "markers sent, no cache activity". That says only what the ledger shows; routectl does not claim the upstream ignored the markers. Then one PASS `cache markers` summary counting the lanes evaluated, or `unavailable: <class>` (`no_data`, `query_timeout`, `db_unavailable`, or an open class) when the ledger could not answer, one PASS `unobservable providers` naming each `anthropic-api` or `bedrock` provider whose cache hits are not observable (an `anthropic-messages` provider on a `base_url` other than `api.anthropic.com` with no `cache_capability` override, for example), and one PASS `converse providers` naming each `bedrock-converse` provider; the lanes of both are not evaluated. A remediation that sets `cache_capability` names both of its keys and keeps `supports_top_level_cache_control` at its current value (doctor prints it; `false` for an `anthropic-messages` provider on another host with no override). The daemon's read gives up after 250 ms, the CLI's after 2 s. Never FAILs, so it can never move the exit code. |

Two structured panels render after the sections:

- **`panels.capability_matrix`** -- the learned-capability truth matrix:
  learned lanes by capability keys, one resolved display cell each. A lane
  is `<provider>#<upstream>` -- the `[providers]` entry that egresses and
  the upstream model id -- printed exactly as `routectl capability purge`
  accepts it; each row also names its provider kind and the model nicknames
  that map to it (two nicknames on one upstream of one provider entry share
  a lane, and a pooled model has one lane per member entry). A learned lane
  the loaded config no longer maps is marked `(unrouted)`. Every cell merges
  the signal layers -- an operator `header_extras` `anthropic-beta`
  wire pin, whether the lane can carry the flag at all, operator overrides,
  the learned ledger-replay registry, and the shipped beta seed -- through
  the same precedence resolution dispatch uses, so the panel cannot drift
  from dispatch; each cell carries its shown layer
  (`pin` / `representability` / `override` / `learned` / `seed`)
  and evidence source, `overrode_layer` (the next layer below the deciding
  one that had an opinion, or null), the routing action dispatch takes
  (`drop` / `route_away` / `strip` / `withhold` / `reprobe` / `allow` /
  `none`), the learned entry's first-seen / last-seen / expiry times, its
  age, a staleness flag, and `value`: the value the learned verdict applies
  (`shape:<v>` / `ceiling:<n>`), null when no learned entry backs the cell or
  its verdict writes none (a drop, a catalog key). The text grid ends each
  cell a learned entry backs in ` value=<token>` (`value=-` when there is
  none); its age and times are in the JSON (`--json`) and the dashboard
  only. A pinned beta flag reads `forced_supported`
  from layer `pin` with action `allow` whatever the learned layer holds; a
  client beta flag on a Converse lane whose model refuses the beta field
  reads `unrepresentable` from layer `representability` with action
  `withhold`. The ledger source
  reports a first-class tri-state (available / an honest empty / unavailable
  with a path-free class code, including `open_failed` / `query_failed`
  when the slice after the replay boundary could not be read) so "could not
  read" is never collapsed into "nothing learned"; whenever the replay ran,
  `replay` reports the rows read, replayed, and skipped by reason
  (`skipped_vocab`, `skipped_owner`, `skipped_revision`, `skipped_lane`,
  `skipped_unknown`; `skipped_revision` is always 0, since no verdict is
  dropped for its catalog or overlay revision). On a `bedrock` lane every
  flag the shipped beta seed withholds appears as a `beta:<flag>` column
  even with no learned row: the
  cell reads `broken` from layer `seed` with action `withhold` (the seed acts
  even with `[capability] enabled = false`), or `cleared` with action
  `allow` once a successful re-probe or `routectl capability purge` lifted
  it for that lane. A learned verdict on the flag outranks the seed, and a
  learned `beta:` negative shows action `withhold` rather than `strip` or
  `route_away`.

  The running daemon's `/status/health` (schema 9) lists the same resident
  learned entries under `learned_negatives`, one row per entry (a
  `verified` positive included), each resolved the same way against the
  served config. Beside `state_key`, `capability_key`, `verdict`,
  `signal_tier`, `observations`, `phase`, `source` and `last_seen_ms`, a
  row carries `action`, `layer` and `overrode_layer` in the matrix's own
  vocabulary (`mixed` action and null layer when the lane's nicknames
  disagree), `age_ms` (time since the entry was FIRST seen; the matrix's
  `age_ms` counts from the last sighting), and `expires_at_ms`: when the
  negative lapses into a re-verification under the runtime rule, its decay
  deadline or, for a negative only the free token-count re-check can
  clear, that deadline plus a 48-hour backstop; null for a `verified`
  positive; and `value`, the same token the matrix cell carries. The matrix's `expires_at_ms` stays the raw decay deadline.
  Each `/status/health` target row also carries `account`, the credential
  account its park and unusable mark cover (`<provider>[#<label>]` for an
  `oauth://` seat, `#<entry>` for any other own credential, null for a
  forwarded one), `login_label`, the `<label>` of an
  `oauth://<provider>#<label>` credential (null for an unlabeled seat, any
  other credential and a forwarded one; `seat_label` is the pool member's
  provider entry, not the login), and `eligibility`: `reason` is why a request cannot
  dispatch to the target right now (`credential_unusable`, `account_parked`,
  `breaker_open` or `rate_limited`, read from the same side-effect-free
  snapshot placement reads; null when it can), `cause` the unusable cause
  (`not_logged_in`, `unreadable`, `expired` or `revoked`), `since_ms` when
  the reason began (the first unusable mark or the breaker opening; null for
  a park or a rate limit, whose start is not recorded), and `fix` the exact
  command that readmits an unusable seat: `routectl login <provider> --label
  <label>` for a labeled `oauth://` seat, `routectl login <provider>` for an
  unlabeled one, and for an `env://` or `file://` credential the provider
  entry and scheme (never the variable name, path or value). The label is
  quoted and an unknown OAuth provider handled as in the doctor's `auth`
  section above. Every key is
  always present. `pools` lists each pool's `configured` and `eligible` seat
  counts and `by_reason` (`reason`, `seats`) for the others, highest
  priority first; `seat_driver` is the background refresh driver's health (see
  [Background refresh of managed logins](#background-refresh-of-managed-logins))
  and `usage_poll_driver` the background usage poll's, with the same keys
  (see [Background usage poll](#background-usage-poll)).
  Reading the panel spends no rate-limit token and claims no half-open
  probe slot.
- **`panels.would_trim`** -- the steady-state would-trim opportunity,
  read-only over all recorded history. Its fields are documented under
  [Steady-state would-trim opportunity](#steady-state-would-trim-opportunity)
  and its remediation hint points at `prompt-size --steady-state`.

The ordered section list is the command's single extension point
(`SECTIONS` in `crates/routectl-cli/src/commands/doctor/mod.rs`); the
offline status surface renders the same list minus `probe`.

#### The seat-pool readout (`pools`), shared with `config check`

`doctor`'s `pools` section and the `oauth seat pools:` block `routectl
config check` prints are the SAME rows rendered two ways, so the two
surfaces can never disagree about a pool. Read them for the answer to
"which account will actually serve this model, and is it logged in":

- **One line per `[pools.<name>]` block** -- its members and the seat each
  member's `credential` names (`<entry>=<seat>`, `=none` for a member
  declaring no `oauth://` ref), its `seat_selection`, and whether it
  `accepts_new_logins`. A member the credential store holds no seat for is
  named explicitly.
- **Then one line per `oauth://` provider entry NO pool claims.** A bare
  ref reads as "pins the default seat", a `#<label>` ref as "pins 1 seat";
  neither carries a strategy, and the line says so -- `seat_selection` is a
  property of a pool, so a standalone entry has nothing to select between.
- **Member entries are not repeated as standalone rows.** The pool line
  already names every member and its seat; a second per-member line would
  read as an independent dispatch target.

In `doctor` the same rows carry a status: a pool whose every member has a
stored credential is PASS, a pool missing SOME member's credential is a
WARN naming the members to log in (or remove), and a pool NO member of
which has one is a FAIL -- requests to every model naming that pool are
refused locally with `credential_unusable` until one member has a
credential. The per-entry rows are always informational
PASS. When the credential store will not open, presence is reported as
unknown and neither status is claimed (the `auth` section owns that FAIL);
`config check` renders the same rows with the counts as unknown and its
own exit stays governed by the validator suite, not by this block.

The block is omitted entirely when the config declares no pool and no
provider entry carries an `oauth://` ref, so an api-key-only config gets no
header noise.

LIMIT worth knowing before trusting it: neither surface builds a router, so
both report CONFIG-and-STORE presence only. A member whose credential is
stored but whose provider the factory could not construct is visible only
to a surface that has a built router (`serve`, or `/status/health` on a
running daemon).

Neither command reads a secret it does not need: a probe or doctor run
reads only what it classifies and never discloses a credential value.

## Validating config

```bash
./routectl config check --config <path>    # validates schema before serve
./routectl config show                     # prints resolved config (inspection)
```

`config check` runs the same startup validation `serve` does -- secret
refs resolve, provider kinds map to known impls, alias chains reference
existing model nicknames.

On a config declaring pools or `oauth://` refs, `config check` also prints
the seat-pool readout ahead of its warnings and errors -- the same rows
`doctor`'s `pools` section renders, documented under
[The seat-pool readout](#the-seat-pool-readout-pools-shared-with-config-check).

**How names appear in diagnostics.** One rule covers `config check`,
`doctor`, the WARN a declined hot reload logs, and the daemon's activation
log lines:

- Names you write (provider entries, pools, pool members, model
  nicknames) are shown filtered and capped: control characters are
  removed, an ANSI escape sequence goes with its ESC, and a name longer
  than 128 bytes is replaced by `<N-byte name withheld>`, so a name never
  spans or recolors a line.
- Seat labels are shown. A pool line names each member with the seat its
  `credential` pins (`work-seat=work`), filtered the same way.
- An `env://` or `file://` credential appears only by its scheme: the
  variable name, file path or value behind it is never printed. An
  `oauth://` reference may be shown with its vendor and seat label
  (`oauth://anthropic#work`); neither is secret, and the token itself is
  never printed.
- Each finding is one line. A name holding a newline cannot split a
  finding in two, and `provider add`, `login` and `init` print a rejected
  edit's findings with the same filter as `config check`.

`config show` prints the post-merge view: `env://`, `file://`, and
`oauth://` references remain as opaque URIs (they are non-secret
pointers, not credential values); defaults are filled in; layered
overlays NOT yet applied
(those compose per request, not at startup). Credential-bearing values
are redacted and `base_url` is reduced to its origin, as listed under
[Inspecting the effective config](#inspecting-the-effective-config-config-show---effective).
Useful when
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
  variant (a mistyped `[providers.X] kind`, a bad class name under
  `[retry.classes]`), is rejected with the offending token AND a `did you mean `Y`?`
  hint naming the closest real name -- when one is close enough. A token
  far from every known name gets no guess rather than a misleading one.
  The candidate list is exactly the one the TOML/serde parser already
  emits, so it can never drift from the real fields. These fire wherever
  config is parsed (check / serve / hot reload).
- **Source lines on `config check`.** Semantic validation errors -- an
  alias chain naming a missing nickname, a model referencing an unknown
  provider, a reserved `[retry.classes] feature-unsupported` count --
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

**Managed token host containment.** A `credential` (or `account_id_ref`)
of `oauth://anthropic` or `oauth://anthropic#<label>` is the subscription
token, and it is accepted only on an entry with
`format = "anthropic-messages"`, `auth = "bearer"`, and a `base_url` that
is `https://` on exactly `api.anthropic.com` with no `user:pass@` prefix
and no explicit non-default port. Any other format, a `cloud` carrier, and any other
host (a gateway, a passthrough, a loopback address, a lookalike or
subdomain of the Anthropic host) are rejected. Static `env://` and
`file://` credentials are not affected, so an `anthropic-messages`
provider on `https://gateway.example/api` with an `env://` key, or on
`http://127.0.0.1:18080` with a `file://` key, stays valid.

- **Symptom.** `routectl serve` refuses to start, and `routectl config
  check` reports an error, naming the provider and the vendor's rule
  (`provider `<name>`: the managed `oauth://anthropic` credential is
  accepted only with ...`). The message withholds
  the `base_url` and the credential ref; read them from your own
  `config.toml`. On a hot reload the same error makes the reload fail:
  the daemon logs `config reload failed; keeping previous config` at
  WARN and keeps serving the previous config. A stored-seat change that
  rebuilds the router against a config carrying the shape is likewise
  refused and keeps the previous router.
- **Fix.** For a gateway or passthrough, give that provider its own
  static credential (`credential = "env://VAR"` or `"file:///abs/path"`)
  issued for that destination. Never copy the subscription token into an
  env var or file to send it elsewhere. To use the subscription itself,
  set `base_url = "https://api.anthropic.com"` and `auth = "bearer"`, as
  `routectl login anthropic` prints.
- **No override.** There is no flag, config key, or loopback exception
  that relaxes this check; loopback is not treated as local because a
  loopback port can be a tunnel to another machine.

The managed Codex, xAI, and Antigravity subscription tokens follow the
same rule, each pinned to its own first-party lane:

- `oauth://codex` or `oauth://codex#<label>` (in `credential` or
  `account_id_ref`) is accepted only with `format = "openai-responses"`,
  `auth = "bearer"`, and a `base_url` that is `https://` on exactly
  `chatgpt.com`.
- `oauth://xai` or `oauth://xai#<label>` is accepted only with
  `format = "openai-chat"`, `auth = "bearer"`, and a `base_url` that is
  `https://` on exactly `api.x.ai` (the path is free, so both
  `https://api.x.ai/v1`, which `routectl login xai` writes, and
  `https://api.x.ai` are valid).
- `oauth://antigravity` or `oauth://antigravity#<label>` is accepted only
  with `format = "gemini-cloud-code"`, `auth = "bearer"`, and a
  `base_url` that is `https://` on exactly `cloudcode-pa.googleapis.com`
  or `daily-cloudcode-pa.googleapis.com`.

For each, a `user:pass@` prefix, an explicit non-default port, a trailing
dot, a lookalike or subdomain host, `http://`, and loopback are all
rejected. The error names the provider and the rule (`provider `<name>`:
the managed `oauth://codex` credential is accepted only with ...`, and
the same for `oauth://xai` and `oauth://antigravity`) and withholds the
`base_url`, the ref, and the seat label; the startup, `config check`, and
hot-reload behavior is the same as above. A staging shard, mirror, or
mock for the Responses lane takes a static credential (`credential =
"env://OPENAI_JWT"`), and an xAI-compatible gateway
or mock takes an `env://` or `file://` key; both stay valid on any host.
There is no override.

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
   persists it to `~/.config/routectl/credentials.json`. It then OFFERS
   the config change that makes the new seat reachable, prints that exact
   delta, and writes it on confirmation:

   ```text
   Credential stored. This config change would route to it:

   [providers.anthropic-default]
   format     = "anthropic-messages"
   base_url   = "https://api.anthropic.com"
   credential = "oauth://anthropic"
   auth       = "bearer"

   [pools.anthropic]
   members = ["anthropic-default"]
   accepts_new_logins = true
   ```

   Answer the confirmation (or pass `--yes`, which skips the PROMPT but
   never the printed delta) and the entry plus its pool land in the file.
   Declining writes nothing and exits 0 -- the credential is stored
   either way, so the block above is also what to paste by hand. Points
   worth knowing:

   - The entry is `anthropic-default`, not the bare `anthropic`: the
     plain family name belongs to the POOL, and providers, pools and
     model nicknames share one namespace.
   - `accepts_new_logins = true` is written only for a pool this login
     CREATES. It is what lets a later
     `routectl login anthropic --label <name>` offer to join its new seat
     to the same pool; login never flips an existing marker.
   - A re-login for a seat the config already reaches proposes nothing.
   - Login never CREATES `config.toml` (that is `routectl config init`),
     and never edits one this build must not write -- it prints the block
     and exits 0 instead.
   - After the write it names whatever routing is still missing: a
     `[models.X]` naming the pool, or an `[aliases]` entry reaching that
     model. The upstream model id is never guessed.

   The `oauth://anthropic` ref resolves at request time against the
   credentials store; `auth = "bearer"` emits
   `Authorization: Bearer <token>` instead of `x-api-key`. Because the
   credential is the managed login and the host is `api.anthropic.com`,
   the entry also emits the built-in Claude Code identity (stainless header
   pack, default `user_agent`, session and request ids, the body cloak).
   Any other credential (an `env://` or `file://` bearer included) or any
   other host gets none of it, and only what you set explicitly in
   `header_extras` or `user_agent` is sent.

   For the claude-code client identity (`user_agent`, forwarded session
   headers, the full stainless header pack) the offered entry is a
   starting point rather than the finished article -- add those fields to
   `[providers.anthropic-default]`:

   ```toml
   [providers.anthropic-default]
   format        = "anthropic-messages"
   base_url      = "https://api.anthropic.com"
   credential    = "oauth://anthropic"
   auth          = "bearer"
   user_agent    = "claude-cli/2.1.294 (external, cli)"
   forward_client_headers = [
       "x-claude-code-session-id",
       "x-claude-code-agent-id",
       "x-claude-code-parent-agent-id",
   ]
   # plus a [providers.anthropic-default.header_extras] sub-table --
   # see "Header pack" below
   ```

   Keep it as ONE entry: a second entry naming the same
   `oauth://anthropic` seat makes a later login ambiguous, and it refuses
   to guess which of the two a pool should grow around.

   **Anthropic API key (no OAuth).** Standard pattern, no `routectl
   login` needed:

   ```toml
   [providers.anthropic-api-key]
   format     = "anthropic-messages"
   base_url   = "https://api.anthropic.com"
   credential = "env://ANTHROPIC_API_KEY"
   auth       = "x-api-key"
   ```

3. Add models and aliases that match what claude-code expects on the
   wire. claude-code 2.1.x sends `claude-haiku-4-5-...`,
   `claude-sonnet-4-...`, `claude-opus-4-...` model strings; the
   suffix-glob aliases collapse the per-version churn. Point them at the
   POOL rather than the member entry, so a second subscription seat added
   by a later login serves the same models with no further edit:

   ```toml
   [models.anthropic-haiku]
   provider = "anthropic"
   upstream = "claude-haiku-4-5-20251001"

   [models.anthropic-sonnet]
   provider = "anthropic"
   upstream = "claude-sonnet-4-6"

   [models.anthropic-opus]
   provider = "anthropic"
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
   time.busy=N time.idle=N` line per request. routectl refreshes the
   `oauth://anthropic` token itself (see
   [Background refresh of managed logins](#background-refresh-of-managed-logins)),
   and on an upstream 401 it refreshes once and retries. A 401 that
   persists, or a `credential_unusable` refusal, means the stored login
   was refused: re-run `routectl login anthropic`.

### Background refresh of managed logins

`routectl serve` runs two background jobs over the managed logins: this
refresh, and the [usage poll](#background-usage-poll) that reads each idle
seat's subscription usage. Both run only when the daemon has a credential
store.

While `routectl serve` runs, a background seat driver checks once a minute
every Anthropic and Codex managed login (`oauth://anthropic[#label]`,
`oauth://codex[#label]`) that a `[models]` entry reaches, directly or through
a `[pools]` member. Logins no entry names, and the other managed providers
(xAI, antigravity), are left alone. A login is refreshed only when it is due:
its access token expires within the same 5-minute lead window a request-time
refresh uses. Otherwise the check sends nothing.

- A refused refresh token, or a configured seat with no stored login, makes the
  seat unusable at once: every model on it answers `credential_unusable` and is
  skipped. It rejoins after `routectl login <provider> [--label <label>]`, with
  no restart. Until then the driver does not retry it.
- Network errors, 429 and 5xx keep the seat in service until its access token
  actually expires; then it is marked `expired` until a later refresh succeeds.
- A revocation is only seen when a refresh is attempted, so it can surface up to
  one access-token lifetime after it happened.
- The driver's health (running, last sweep, last sweep with no failure, sweep
  and failure counts) is held in memory only and starts from zero on restart.
  `/status/health` shows it as `seat_driver`: `running`, `last_sweep_ms`,
  `last_good_sweep_ms` (epoch ms, null before the first such sweep), `sweeps`,
  `transient` and `definitive`. `running` is false on a daemon with no
  credential store, which runs no driver.

Each background refresh rewrites `credentials.json`, so the credentials watcher
reloads once per seat per token lifetime.

### Background usage poll

While `routectl serve` runs, a second background job reads the subscription
usage of the same managed logins the refresh covers, for the seats that have
no fresh accepted reading. Anthropic seats are read from `https://api.anthropic.com/api/oauth/usage`
and Codex seats from `https://chatgpt.com/backend-api/wham/usage`, each with the
seat's own access token (and, for Codex, its ChatGPT account id). No other host
is contacted and redirects are never followed.

- An Anthropic seat's usage call sends the `user_agent` of the `[providers]`
  entry the seat is reported under (for a login several entries name, the
  lexicographically smallest, the same entry `/status/accounts` shows as
  `provider_entry`), or the built-in Claude Code `User-Agent` when that entry
  sets none. Each poll reads the running config, so a reload that changes the
  value applies to the next poll. A value that is not a valid HTTP header value
  is not sent: the poll sends the built-in default instead and logs a WARN (see
  [LOGGING.md](LOGGING.md#background-usage-poll-lines)). Codex usage calls
  always send the built-in Codex `User-Agent`.

- A seat is polled only after 5 minutes (plus a per-seat random jitter of up to
  a minute) without an accepted reading from either a response or an earlier
  poll. It is an accepted reading that suppresses the poll, not traffic: a
  seat whose responses carry no quota headers, or whose header reading is
  refused, is still polled. A fresh daemon gives each seat a
  random first due time within its first 5 minutes. The poll sweeps once a
  minute and polls at most one seat per vendor per sweep, so seats of one vendor
  that are due together are polled one per minute, and a larger set of one
  vendor's seats takes correspondingly longer than 5 minutes to be read.
- A seat whose access token is about to expire is skipped: the poll never
  refreshes a token, it waits for the refresh job.
- A failed poll backs that seat off for 5, then 10, then 15 minutes (the cap)
  before it is tried again. A 429 pauses the whole vendor for the
  `Retry-After` it names, capped at 1 hour, or 15 minutes when it names none.
- A poll changes only the seat's reading and the poll's own state. It never
  marks a seat unusable, opens a breaker or touches a credential, whatever the
  endpoint answers.
- An accepted reading feeds [quota-aware placement](#quota-aware-seat-placement)
  exactly as a response header reading does, and is written to the usage
  database's `account_readings` history. The poll's state and the meter are
  memory-only: after a restart every seat starts with no reading. A seat also
  starts again with no reading when the credentials file reloads with any
  change to its stored login record: a `routectl login` of another account, a
  hand edit, or a token refresh by another routectl process (a `routectl
  refresh`, a second daemon). This happens even when the router rebuild that
  reload needs fails. A refresh the running daemon made itself normally keeps
  the reading, but one that lands while the file is being reloaded can drop it
  too; a dropped reading costs only one extra poll.
- `/status/health` shows the poll driver's health as `usage_poll_driver`, with
  the same keys as `seat_driver`; a poll failure counts as `transient` and never
  as `definitive`.
- `[usage] poll = false` turns the poll off for every managed login: no usage
  request is sent at all. It is a standing setting, not a fault: the poll
  calls vendor endpoints that are not a public API, and this is how an
  operator declines that traffic. **The trade-off:** with the poll off an
  account gets a reading only from its own responses' headers, so an idle
  account has no fresh reading, and placement then steers new conversations
  toward the busy accounts whose readings are fresh. The switch hot-reloads:
  turning it off stops polling from the next sweep, and a poll already in
  flight when it went off is discarded when it returns, without changing the
  reading, the history or the poll's state. Turning it off also clears each
  seat's poll schedule (its last attempt, result and failure run; vendor
  pauses are kept), so once it is back on each seat is polled as soon as it
  has gone 5 minutes without an accepted reading. `/status/accounts` reports
  each managed login as `off` meanwhile, and the poll driver keeps running
  (`usage_poll_driver.running` stays true).

#### Account readings on `/status/accounts`

The running daemon's `/status/accounts` panel (schema 1, also under
`panels.accounts` on the `/status` aggregate) shows every configured
credential account with what the account meter holds for it and the usage
poll's state. It is read from memory only: no usage database read and no
vendor request, so it is cheap at the dashboard's poll rate. Every key is
always present; times are epoch ms and durations ms.

- `accounts`: one row per credential account a model dispatches on, in
  account order: each `oauth://` login once, however many entries or models
  name it, and each other own credential (an `env://` or `file://` key, an
  AWS cloud credential) as `#<entry>`. A forwarded credential is not an
  account, and an account the config no longer names is not listed, even
  while the meter still counts it. Each row carries:
  - `account`, the key other panels carry (the `/status/health` targets'
    `account`); `provider_entry`, the `[providers]` entry it is reported
    under (for a login two entries name, the lexicographically smallest);
    and `login_label`, the `<label>` of an `oauth://<provider>#<label>`
    login (null for an unlabeled login and any other credential). The three
    are separate keys: never split `account` to find the other two.
  - `report`: `no_reading` (nothing accepted since the daemon started),
    `windows`, or `no_windows` (the latest complete poll said the account
    has no usage windows). An API-key or cloud account always reads
    `no_reading` with no windows.
  - `windows`: each held window, ordered by length then scope, with `class`
    (`short` up to 24 hours, `long` beyond), `length_secs`, `scope` (null for
    an account-wide window), `used` (fraction 0 to 1), `remaining` (`1 -
    used`, null while the reading is stale, so a stale reading never shows
    room), `reset_at_ms` (null when not reported; for an unstarted window,
    where it would end if it began at the reading), `started` (`yes`, `no`
    or `unknown`), `source` (`response` for a response header, `poll` for
    the usage poll), `observed_at_ms`, `age_ms` (null when the clocks cannot
    order it) and `stale`. A reading is stale once it is more than 7 minutes
    old, once a running window's reset has passed, or when its age cannot be
    read.
  - `last_accepted`: `observed_at_ms` and `source` of the latest accepted
    reading, or null.
  - `poll`: `applicability`, one of a closed set: `polled` for a managed
    Anthropic or Codex login the running poll reads; `awaiting_login` for
    one the router holds as revoked or not logged in, which the poll skips
    until a fresh login (its schedule state is dropped, so the fields below
    read null until it is polled again); `off` for a managed Anthropic or
    Codex login while `[usage] poll = false` (a setting, not a fault; the
    fields below read null, and it takes precedence over `awaiting_login`);
    `not_polled` for any other
    credential or a daemon running no poll. `vendor` (`anthropic`, `codex`,
    or null for `not_polled`), `last_attempt_ms`, `last_result` (the last poll's
    outcome: `accepted`, `no_windows`, `token_not_ready`, `paused`,
    `redirect`, `auth`, `http`, `timeout`, `transport`, `too_large`,
    `malformed` or `superseded`) and `next_due_ms`, each null before the
    poll has attempted or scheduled the seat. `failure_since_ms` is when the
    seat's current run of failed polls began: set by the first `paused`,
    `redirect`, `auth`, `http`, `timeout`, `transport`, `too_large` or
    `malformed` result after an accepted poll (or since the daemon started),
    kept through further failures, cleared to null by an `accepted` or
    `no_windows` result or by any accepted reading newer than it (a response
    header reading included, which also holds the poll off), and left as it
    was by `token_not_ready` and `superseded`. It is the input for "a usage poll failing for 15 minutes":
    `now - failure_since_ms`. A backward wall-clock step pulls it back to
    the new time, never into the future.
- `vendor_pauses`: each vendor whose polling is paused after a 429, with
  `vendor` and `paused_until_ms`.
- `rejections`: the meter's running refusal counts by reason
  (`invalid_utilization`, `expired_reset`, `implausible_reset`, `overflow`,
  `contradictory`, `unknown_length`, `invalid_started`, `malformed_body`),
  and `refused_by_admission`, readings refused because their account is not
  configured.
- `history`: the `account_readings` history writer's counts of accepted
  readings, `enqueued`, `dropped_full`, `dropped_closed`, `persisted` and
  `write_errors`.
- `poll_driver`: the usage poll driver's health: `running`,
  `last_sweep_ms`, `last_good_sweep_ms`, `sweeps`, `transient` and
  `definitive`, today the same values as `/status/health`
  `usage_poll_driver`.

Every count resets on restart. The panel carries no token, credential
reference, response body or header, and no vendor account id.

The `quota` list on `/status/usage` (one latest ledger snapshot per seat) and
the quota lines under the `routectl usage` table follow the same configured
rule: a seat appears only while the config names it, and a row with no seat
recorded is not shown. A retired seat's ledger rows stay and still count in
the traffic and cost figures. Each `/status/usage` `quota` row carries `seat`
(the `account` key), `provider_entry` and `login_label`, with the values this
panel gives that account.

### Credential pools (multiple accounts per provider)

A single OAuth provider can hold more than one credential ("seat") in
the routectl store. The default seat is the bare provider; additional
seats are named with a `#<label>` suffix. `[pools.<name>]` is how an
operator pools a few same-provider subscriptions and spreads load across
them.

**The resolution hierarchy** (stated once, referenced everywhere else in
this document):

```
alias -> model -> provider entry OR pool -> credential
```

An `[aliases]` entry resolves to a `[models.X]` nickname; the model's
`provider` value names EITHER one `[providers.X]` entry OR one
`[pools.X]` block; a provider entry names exactly ONE credential through
its `credential`; a pool names one credential per member. A pool is
therefore the only multi-account dispatch target -- a provider entry has
one credential and nothing to select between.

**Seat refs.** A config `credential` names exactly one seat:

- `oauth://anthropic` -- the DEFAULT (unlabeled) seat, and only that
  seat. It is not a set and carries no strategy.
- `oauth://anthropic#seat-b` -- one specific labeled seat.

Under config schema version 3 a bare ref quietly stood for every stored
seat of its provider. Version 4 retired that reading: reaching several
accounts is what `[pools.<name>]` is for, and each account gets its own
`[providers.X]` entry naming its own seat. See
[Version 4 makes pools explicit](#version-4-makes-pools-explicit).

**The pool block.** A pool lists the provider entries (accounts) it
groups. The members must be interchangeable seats of one upstream:

```toml
[providers.anthropic-default]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "oauth://anthropic"
auth       = "bearer"

[providers.anthropic-seat-b]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "oauth://anthropic#seat-b"
auth       = "bearer"

[pools.anthropic]
members            = ["anthropic-default", "anthropic-seat-b"]
seat_selection     = "sticky-least-loaded"   # the default; or "fill-first"
accepts_new_logins = false           # the ONLY thing that grows a pool

[models.anthropic-sonnet]
provider = "anthropic"               # the POOL, so dispatch spreads across both accounts
upstream = "claude-sonnet-4-6"
```

| Key | Meaning |
|---|---|
| `members` | `[providers]` table keys, required and non-empty, at most 32. Every member must exist, all must share format, destination, auth, OAuth vendor and cloak settings (see "What pool members share" below), and a provider entry belongs to at most one pool. This release's members are OAuth accounts. |
| `seat_selection` | How dispatch picks among the members for one request. Documented per strategy below. Set on the POOL, because the strategy is a property of the SET, not of one transport. |
| `accepts_new_logins` | Whether a future `routectl login` for this family may propose joining the new account to this pool. `false` (the default) pins the membership list, so only an explicit config edit grows it. There is no wildcard member token. |
| `circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`, `request_timeout_ms`, `stream_first_byte_timeout_ms` | Tuning every member inherits. See "Pool tuning keys" below. |

**What pool members share.** Every member must agree with the pool's
first member on:

- `format`;
- the destination: the `base_url`, compared after parsing (scheme and host
  case do not matter) and without trailing `/`, or
  for a `cloud` entry the cloud and its region;
- `auth`;
- the OAuth vendor of `credential` (`oauth://anthropic` and
  `oauth://anthropic#seat-b` share the vendor `anthropic`; the label is
  per seat);
- `cloak_mode` and `cloak_strict_mode`, where an unset key equals its
  default (`auto` and `false`).

A member that differs gets one error per differing setting, naming the
pool, the two members and the setting, never its value. Per-seat
tuning (`rpm_limit`, `circuit_failures`, `circuit_cooldown_ms`,
`request_timeout_ms`, `stream_first_byte_timeout_ms`) may differ. For
ordered fallback across different upstreams, use an alias chain.

**Pool tuning keys.** A pool accepts the five runtime-gate keys of
[Per-provider runtime gates](#per-provider-runtime-gates), with the same
names and meanings. Each applies to every member of the pool and to a
pooled model's own runtime slot. A member's own value wins over the
pool's, key by key, and the two timeouts fall back to `[retry]` when
neither sets them:

```
circuit_failures, circuit_cooldown_ms, rpm_limit:
    member provider > pool > default
request_timeout_ms:
    member provider > pool > [retry] > default
stream_first_byte_timeout_ms:
    [models.X] > member provider > pool > [retry] > default
```

A provider entry that is not a member of the pool never reads it. A
pool `circuit_failures = 0` turns the breaker off for every member that
sets no value of its own; a member's own `0` turns it off for that seat
alone.

```toml
[pools.anthropic]
members          = ["anthropic-default", "anthropic-seat-b"]
circuit_failures = 3        # every seat trips after 3 failures...

[providers.anthropic-seat-b]
format           = "anthropic-messages"
base_url         = "https://api.anthropic.com"
credential       = "oauth://anthropic#seat-b"
auth             = "bearer"
circuit_failures = 5        # ...except seat-b, whose own value wins
```

A pool name must not collide with a `[providers]` key or a `[models]`
nickname -- `config check`, the `serve` gate, and the hot-reload gate all
reject that, along with members that do not share the settings below, an unknown member, a member
claimed by two pools, and an empty member list. They also reject a pool
member with no credential reference or one that does not parse (every
member must authenticate with an `oauth://` reference), and a member
carrying a managed credential outside the lane that credential is
accepted on. A member whose credential reads but whose provider cannot
be built from its entry (for example a Codex login with no stored
account id) refuses the config too: `serve` refuses to start, a hot
reload is rejected, and `config check` reports it, because check runs
the same member build offline (it reads the stored credentials as they
are and never refreshes or writes one, adopts an existing installation
id without creating a missing one, and reads the ChatGPT cookie jar
without writing it back). Each refusal names the
pool, the member and the rule, never a credential value or path. A rejected
hot reload leaves the running router serving the previous config. A
member that is only logged out or whose credential cannot be read is
not refused; see "Pool seats are kept" below.

**Naming convention.** `routectl login` derives names the same way every
time, so a name it generated is recognized on the next login: the POOL
takes the plain family name (`anthropic`), and each account entry takes a
suffixed one -- `<family>-default` for the default seat,
`<family>-<label>` for a labelled seat. A generated name that collides
with an unrelated existing entry is refused rather than resolved by
guesswork.

**Registering seats.** The four OAuth CLI commands take an optional
`--label`. Without it, every command targets the default seat:

```bash
# Default seat.
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
`routectl login <provider> --label <name>`. The config's `credential`
selects among the seats already stored; it never creates one. What the
login DOES do to this file is offer the provider entry (and, on a first
login for the family, the pool) that consumes the new seat -- printed
before anything is written, applied on confirmation or with `--yes`,
skipped entirely when there is no config file to edit or the file fails
the validation `config check` runs (the errors are printed instead). A
pool without `accepts_new_logins = true` is never grown that way. A login
that joins a pool copies the `cloak_mode` and `cloak_strict_mode` keys its
first listed member sets onto the new entry, and the printed entry shows
them; a key that member leaves unset stays unset, so no default is
written as an override. `routectl
config check` and `routectl doctor` both report each pool -- its members
and the seat each names, its strategy, whether it accepts new logins, and
any member the credential store holds no seat for -- plus one line per
provider entry no pool claims.

**Pool seats are kept.** A member whose credential cannot be read when
the router is built (a seat that is not logged in, or a credential that
cannot be read or refreshed) stays in the pool as an unusable seat
instead of being dropped. Its account is marked unusable with a cause
(`not_logged_in` for a missing login, `unreadable` otherwise), dispatch
walks past it without dialing it, and a config reload that still cannot
read it keeps the mark and the time it was first set. A daemon whose
every seat is unusable starts normally and answers each request, on all
three ingresses and on `count_tokens`, with HTTP 503 and `error.type`
`credential_unusable`, with no `Retry-After` (no recovery time is
known). When an unusable seat shares the chain with a parked one, the
refusal stays `credential_unusable` and carries the park's wait.

**Live recovery after re-login.** `routectl login <provider> --label
<name>` readmits an unusable seat with no restart, whether it was
unusable at startup or became unusable later. The daemon watches the
credential store, and a reload that sees the seat's stored record added
or changed acts on it at once. A seat that was built and later found
unusable (for example a refused refresh token) is readmitted in place:
its mark is cleared and the router is not rebuilt. A seat kept as an
unusable placeholder at build (never logged in, or a stored credential
that could not be read or refreshed) has nothing to dial, so the reload
rebuilds the router for it even when the set of stored seats is
unchanged; every other seat keeps its breaker, rate and park state
across that rebuild, and a seat still not logged in keeps its mark.
routectl's own token refresh writes the rotated record and then updates
its in-memory copy to match, so the reload that write triggers sees no
changed seat and does not rebuild.

**Seat selection.** `seat_selection` takes effect only when a pool
resolves to more than one usable member; with one member there is nothing
to choose between and every strategy behaves identically:

- `fill-first` -- drain one seat before advancing to the
  next. The drain IS this strategy's contract, not a
  rough edge on it: holding one seat keeps that seat's prompt cache warm,
  and running it down until the upstream refuses is the price of the
  locality. Quota-aware placement deliberately does NOT apply here --
  choosing `fill-first` is choosing that trade, and overriding it would
  override a stated preference. An operator who wants budget-aware
  spreading leaves the key out.
- `sticky-least-loaded` (default) -- the ONLY strategy quota-aware
  placement and the session-affinity layer apply to. Pin each conversation to one seat
  so its warm prompt cache is preserved, while balancing NEW conversations
  across seats by available capacity. A conversation's first request
  picks the least-loaded healthy seat (preferring seats with a closed
  breaker, then -- on a subscription pool with observed budgets -- the
  seat with the most remaining short-window budget, otherwise the most
  RPM headroom, with a deterministic tiebreak so a burst of new
  conversations does not herd onto one seat; see
  [quota-aware seat placement](#quota-aware-seat-placement)); every
  later request for that conversation routes back to the same seat. If
  that seat later goes unhealthy (rate-limited or breaker-open), the
  conversation migrates ONCE to a healthy sibling and does not flap back
  when the original recovers. Requires an inbound per-conversation key
  (the `x-claude-code-session-id` header Claude Code sends, or body
  `metadata.session_id`). A request without one mints no pin, since there
  is nothing to pin under; it still places by remaining subscription
  budget when the evidence is complete enough to act on, because the only
  thing that outranks quota fairness is protecting a warm prompt cache
  and a keyless request has none. "Complete
  enough" is the same bar the keyed pick uses: at least one eligible seat
  fresh and below its threshold, or every eligible seat fresh-known and
  over it. Mixed evidence -- some seats observed and over threshold, the
  rest unobserved -- falls back to `fill-first`, as do a pool with no
  readings at all. The per-request seat decision is not
  persisted, and only partially logged: a DEBUG line marks a birth pick
  and an overflow repin, while a sticky stay, a pick deferred for lack of
  a healthy seat, and the keyless fall-through emit no selection line of
  their own. Note
  an incomplete-evidence fallback is separately visible in the throttled
  quota-placement diagnostic. The usage ledger's
  `selection_decision` column is write-stopped (retained in the schema,
  NULL for every row written by this version onward).

These strategies are applied at dispatch time -- `fill-first` always
starts from seat 0 (fixed priority order), and
`sticky-least-loaded` leads with the conversation's home seat (the rest
of the pool follows in fill-first order as fallback). Seat selection is
a best-effort ordering hint: the per-seat rate-limit gate and the
fallback chain remain authoritative. An empty or whitespace-only
`--label` is rejected with a clear error, matching the
`oauth://<provider>#<label>` ref parser's rule.

### Version 4 makes pools explicit

Config schema version 4 makes `[pools.<name>]` blocks the ONLY multi-seat
shape. Under version 3 a bare `oauth://<provider>` ref quietly stood for
EVERY stored seat of that provider; under version 4 and later it means
the DEFAULT SEAT alone, and `#<label>` remains the single-seat pin. A
version 3 file is refused like any older file (see
[Config schema version](#config-schema-version-version)).

When writing the new file, give each stored seat you dispatched through
its own `[providers.X]` entry (`<family>-<label>`, carrying
`oauth://<family>#<label>`), list those entries in a `[pools.<name>]`
block that carries the `seat_selection` the provider entry used to, and
point each `[models.X] provider` that should spread across the seats at
the POOL (see the pool block above). `routectl login <provider> --label
<name>` offers the entry and, on a first login for the family, the pool,
so logging in again per seat after `routectl init` writes the same
shape. A model left on a single entry's bare ref reaches the default seat
alone.

### Header pack ("look like claude-code")

Drop this into `header_extras` on the OAuth-bearer Anthropic entry (the
one `routectl login anthropic` wrote) so the upstream sees the same SDK
fingerprint a stock claude-code 2.1.294 sends from the bundled
`@anthropic-ai/sdk` (a client run with `API_TIMEOUT_MS` set reports its
configured `x-stainless-timeout` instead of the default `600`). Written as a
sub-table -- a multi-line inline table
(`{ ... }` spanning lines) is not legal TOML:

```toml
[providers.anthropic-default.header_extras]
"anthropic-beta"                            = "claude-code-20250219,oauth-2025-04-20"
"x-app"                                     = "cli"
"anthropic-dangerous-direct-browser-access" = "true"
"x-stainless-arch"                          = "x64"
"x-stainless-lang"                          = "js"
"x-stainless-os"                            = "Linux"
"x-stainless-package-version"               = "0.128.0"
"x-stainless-runtime"                       = "node"
"x-stainless-runtime-version"               = "v26.3.0"
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
| OAuth refresh on 401 | works | an upstream 401 forces one token refresh and one retry; the background seat driver also refreshes a due login; a refused refresh token needs `routectl login anthropic` |
| `WebSearch` tool on Bedrock | upstream-rejected | claude-code's `web_search_<v>` tool isn't supported by Bedrock; the router learns the rejection after the first miss per Bedrock lane and skips that lane for later web-search requests. Optionally declare `unsupported = ["web_search"]` under `[capability.overrides.<bedrock-provider>]` to skip even that first miss (see "Per-provider capability filter" above) |
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
bearer JWT. The entry is `format = "openai-responses"` with a `base_url`
on `chatgpt.com` and `auth = "bearer"`; the destination selects the
ChatGPT request dialect. Two ways to supply the bearer: routectl-managed
OAuth (recommended) or a static bearer you manage yourself.

### routectl-managed OAuth for Codex (recommended)

Run `routectl login codex` once. routectl spawns a local callback server
on port 1455 (the codex public PKCE client registers fixed redirect URIs
against that port), opens the browser to OpenAI's auth flow, exchanges
the authorization code for an access + refresh token pair, and persists
them to `~/.config/routectl/credentials.json` (atomic write, mode 0600
on Unix; same hygiene as the `file://` secret-ref path). Windows has no
mode check: the file relies on the user-profile directory ACL to stay
owner-only.

The login flow is browser-only -- `routectl login codex --print-url`
is rejected, because the OpenAI auth flow has no headless paste-back
landing page. SSH / headless operators should port-forward 1455 to the
local box and use the default browser flow.

The login then OFFERS the config change that consumes the new seat and
writes it on confirmation (`--yes` skips the prompt, never the printed
delta), so the block below usually lands without hand-editing:

```toml
[providers.codex-default]
format     = "openai-responses"
base_url   = "https://chatgpt.com/backend-api/codex"
credential = "oauth://codex"
auth       = "bearer"
# account_id_ref omitted: routectl reads `chatgpt_account_id` off the
# OAuth-session JWT and injects it as the `ChatGPT-Account-Id` header.

[pools.codex]
members = ["codex-default"]
accepts_new_logins = true
```

The entry takes the `-default` suffix because the plain family name
`codex` is the POOL's; `accepts_new_logins = true` on a pool this login
created is what lets `routectl login codex --label <name>` offer to join
a second ChatGPT account later. Declining writes nothing and exits 0 --
paste the block by hand instead.

The `oauth://codex` ref resolves at request time against the credentials
store; rotation is picked up live without restarting routectl. When the
upstream marks the refresh token expired, reused, or invalidated,
routectl surfaces a "re-run `routectl login codex`" error -- re-run the
login and traffic resumes.

### Static bearer

For operators who already manage the JWT externally, point routectl
at the file or env var and supply the account UUID explicitly:

```toml
[providers.codex-static]
format         = "openai-responses"
base_url       = "https://chatgpt.com/backend-api/codex"
credential     = "env://OPENAI_JWT"
auth           = "bearer"
account_id_ref = "env://OPENAI_ACCOUNT_ID"
```

`account_id_ref` is REQUIRED on this path -- there is no OAuth session
for routectl to derive the UUID from. `env://` and `file://` refs work
for both fields. routectl never refreshes a
static bearer; rotation is the operator's job.

`config check`, `serve` startup, and hot reload all enforce the
`account_id_ref` rule without reading either secret: an entry on the
Codex host with a static bearer and no `account_id_ref` is refused, and
so is an `account_id_ref` on any other `openai-responses` entry (another
host, or the AWS `cloud`) and on any other format. That rule itself reads no secret. Once the
config has no errors, `config check` then resolves `env://` and
`file://` refs locally and reports an env var that is unset or a file
that is missing as a warning.

### `codex_version` (client-identity override)

The ChatGPT backend rejects a session whose HTTP client identity drifts
from what a real codex CLI install emits, so routectl mimics a specific
codex CLI version on the wire. That version is baked into the binary (a
pinned default that ships current); `codex_version` overrides it without
rebuilding, so an operator can track a newer upstream codex release the
moment it lands rather than waiting for a routectl release.

```toml
[providers.codex-default]
format        = "openai-responses"
base_url      = "https://chatgpt.com/backend-api/codex"
credential    = "oauth://codex"
auth          = "bearer"
# codex_version = "0.167.0"   # default: the version pinned into this build
```

- **Default is the pinned version.** Omit the knob and routectl uses its
  baked-in default (the codex CLI version this build was cut against). The
  knob is only for tracking a version newer than the pin.
- **Only the managed codex login presents it.** The codex identity (the
  `User-Agent` default, `originator`, residency, `version`, session,
  installation, thread, request and window ids) is sent only by a provider
  whose `credential` is `oauth://codex[#label]` with a `base_url` on
  `chatgpt.com` (see [Client identity](#rules-an-entry-must-meet)). An
  entry on another host, an AWS `cloud` entry, or a `chatgpt.com` entry
  with an `env://` / `file://` token sends none of it; set `user_agent` or
  `header_extras` there if the upstream needs them.
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
  the merge, but doing so on a `chatgpt.com` provider with a value that
  diverges from the derived identity logs a warning -- prefer
  `codex_version` to keep the fingerprint coherent.

## xAI (Grok) provider

UNTESTED in this release: the managed xAI login parses, is host-contained
and routes, but it has not been validated against the live upstream for
this release.

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

The login then OFFERS this config change and writes it on confirmation
(`--yes` skips the prompt, never the printed delta):

```toml
[providers.xai-default]
format     = "openai-chat"
base_url   = "https://api.x.ai/v1"
credential = "oauth://xai"
auth       = "bearer"

[pools.xai]
members = ["xai-default"]
accepts_new_logins = true
```

The `oauth://xai` credential is sent as a bearer, so the entry writes
`auth = "bearer"`; `base_url` must be spelled out because every entry
names its destination. Declining the offer writes nothing and exits 0 --
`routectl login xai` prints exactly this block either way, so it can be
pasted by hand.

The `oauth://xai` ref resolves at request time against the credentials store;
rotation is picked up live without restarting routectl. An upstream 401 makes
routectl refresh the login and retry the request once. When the upstream marks
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
