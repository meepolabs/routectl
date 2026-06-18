# routectl

A local LLM router. One Rust binary, listening on `127.0.0.1`, that proxies OpenAI-compatible (`/v1/chat/completions`) and Anthropic Messages (`/v1/messages`) requests across multiple backends with fallback, retry, and unified reasoning normalization.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![Workspace tests](https://img.shields.io/badge/tests-2500%2B%20passing-brightgreen.svg)](#testing)

## Features

- **Two ingress dialects** -- OpenAI Chat Completions and Anthropic Messages, both feeding one canonical internal request shape.
- **Four egress provider classes**:
  - `openai-compat` -- any host that speaks the OpenAI body shape (OpenAI, DeepSeek, OpenRouter, Groq, NIM, vLLM, llama.cpp, etc.).
  - `anthropic-api` -- native Anthropic Messages API with `x-api-key` or `Authorization: Bearer` auth.
  - `bedrock` -- native AWS Bedrock with SigV4 signing, full credential chain (env / static / profile / SSO / IRSA / IMDS) or short-term bearer keys; both InvokeModel and Converse body shapes for Anthropic models.
  - `openai-responses` -- ChatGPT Codex (`chatgpt-oauth` bearer JWT, stream-only).
- **Unified reasoning surface** -- OpenRouter-shape `reasoning_details[]` with provider-tagged `format`. Six dialects on the openai-compat side; Anthropic thinking blocks (with `signature`) preserved across multi-turn tool use.
- **Cache control round-trip** -- Anthropic prompt-caching `cache_control` and `anthropic_beta` flags pass through losslessly on Anthropic-in -> Anthropic-out and Anthropic-in -> Bedrock-Invoke-out paths. Verified live: cache miss writes N tokens, cache hit reads the same N back.
- **claude-code as a gateway client** -- per Anthropic's [published gateway pattern](https://code.claude.com/docs/en/llm-gateway), with `forward_client_headers` for `x-claude-code-*` attribution, a `POST /v1/messages/count_tokens` proxy, per-dialect error-envelope shapes, and forward-compat for unknown Anthropic SSE block types. See [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) "claude-code as a gateway client" for setup; "Responsible use" below for the operating envelope.
- **Reliability** -- per-error-class retry caps (429 / 5xx / network), per-attempt timeouts, jittered backoff, RPM token bucket per provider, passive circuit breaker with single-probe half-open. `retry_allowlist` / `retry_denylist` schema for fallback selection. `probe_max_tokens` fast-fails small availability probes on rate-limit instead of walking the chain.
- **Secrets** -- `env://VAR`, `file:///abs/path` (chmod-600 / 400 enforced on Unix, TOCTOU-safe), `literal:value`, and `oauth://<provider>` (routectl-managed PKCE login for Anthropic and Codex with runtime refresh and 401 recovery). No OS-keychain integration, no auto-discovery.
- **Local-first** -- binds to `127.0.0.1` by default; refuses non-loopback bind without explicit `--unsafe-public`.
- **Usage accounting** -- per-request SQLite ledger (default `~/.config/routectl/usage.db`); inspect with `routectl usage` over calendar windows (today / this-week / this-month / all-time) or a custom date range, optionally grouped by model, provider, or alias; configured via the `[usage]` block (`db_path` overridable).
- **Per-model response-echo knobs** -- `reported_model` (optional `[models.X]` field; pins the `model` string echoed to clients to a stable label regardless of the alias or fallback target; default = the client's requested alias) and `visible_routectl_provider` (default `true`; set `false` on a `[models.X]` entry to suppress the `routectl_provider` field in that model's responses).

## Install

### Pre-built binaries

Bare binaries are published per release for linux x86_64, linux aarch64, macos aarch64, and windows x86_64. Download with `curl`, mark executable, drop into `PATH`:

```bash
# linux x86_64
curl -fL https://github.com/meepolabs/routectl/releases/latest/download/routectl-$(curl -s https://api.github.com/repos/meepolabs/routectl/releases/latest | grep tag_name | cut -d'"' -f4 | sed 's/^v//')-linux-x86_64 -o /usr/local/bin/routectl

# macos aarch64 (apple silicon)
curl -fL https://github.com/meepolabs/routectl/releases/latest/download/routectl-$(curl -s https://api.github.com/repos/meepolabs/routectl/releases/latest | grep tag_name | cut -d'"' -f4 | sed 's/^v//')-macos-aarch64 -o /usr/local/bin/routectl

chmod +x /usr/local/bin/routectl
routectl --help
```

`curl` does not set the `com.apple.quarantine` xattr, so macOS Gatekeeper does not prompt. If you downloaded via a browser (Safari / Chrome / Firefox) instead, run once:

```bash
xattr -d com.apple.quarantine /usr/local/bin/routectl
```

Verify the download against the signed `SHA256SUMS` file at the same release URL:

```bash
cosign verify-blob \
  --certificate-identity-regexp '^https://github\.com/meepolabs/routectl/\.github/workflows/release\.yml@refs/tags/v[0-9]' \
  --certificate-github-workflow-repository meepolabs/routectl \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --bundle SHA256SUMS.cosign.bundle SHA256SUMS

sha256sum -c SHA256SUMS
```

### From source

Requires Rust 1.75+.

```bash
git clone https://github.com/meepolabs/routectl
cd routectl
cargo build --release
./target/release/routectl --help
```

The release binary is ~6.5 MB stripped with default features (includes the AWS SDK dependency tree for Bedrock). The shipped `routectl` binary always links the AWS SDK -- `routectl-cli` hardcodes the `bedrock` feature, and Cargo feature unification re-enables it across the whole workspace, so there is no AWS-free build of the binary itself. The lean command below builds the `routectl-providers` **library** without the AWS deps, for downstream embedders that depend on the crate directly:

```bash
cargo check -p routectl-providers \
  --no-default-features \
  --features openai-compat,anthropic-api
```

## Quick start

```bash
mkdir -p ~/.config/routectl
routectl config example > ~/.config/routectl/config.toml
# edit: add provider keys + aliases
routectl config check
routectl serve
# routectl listening on http://127.0.0.1:8787
```

OpenAI Chat Completions:

```bash
curl -N -X POST http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "fast",
    "messages": [{"role": "user", "content": "say hi"}],
    "stream": true
  }'
```

Anthropic Messages:

```bash
curl -N -X POST http://127.0.0.1:8787/v1/messages \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "fast",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "say hi"}],
    "stream": true
  }'
```

One-shot via CLI:

```bash
routectl test fast --prompt "say hi in five words"
```

## Configuration

Minimal example:

```toml
[server]
host = "127.0.0.1"
port = 8787

[providers.deepseek]
kind = "openai-compat"
base_url = "https://api.deepseek.com/v1"
api_key_ref = "env://DEEPSEEK_API_KEY"
reasoning_dialect = "deepseek"

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[providers.bedrock]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

[models.fast]
provider = "deepseek"
upstream = "deepseek-chat"

[models.heavy-bedrock]
provider = "bedrock"
upstream = "us.anthropic.claude-opus-4-20250514-v1:0"
supports_adaptive_thinking = true
effort_levels = ["low", "medium", "high"]

[models.heavy-anthropic]
provider = "anthropic"
upstream = "claude-opus-4-20250514"
supports_adaptive_thinking = true
effort_levels = ["low", "medium", "high"]

[aliases]
fast  = "fast"
heavy = ["heavy-bedrock", "heavy-anthropic"]   # fallback chain
"claude-opus-*" = "heavy"                       # suffix-glob routing
default = "fast"
```

See [`examples/config.toml`](examples/config.toml) for the full surface, including the global `[retry]` table, per-provider RPM limits, and circuit breakers. v0.6 collapsed per-alias retry overrides into the workspace-global `[retry]` -- per-error-class caps (`retry_on_429`, `retry_on_5xx`, `retry_on_network`) cover the same operator knobs.

### Secret references

Routectl resolves credentials through one of four URI schemes per provider. There is no auto-discovery; you choose the source explicitly.

| Scheme | Meaning |
|---|---|
| `env://VAR_NAME` | Process env var. |
| `file:///abs/path` | File contents (trailing whitespace trimmed). On Unix, refused if group/other have any permissions; `chmod 600` or `400` recommended. Compatible with sops, age, doppler-cli, vault-agent, etc. Windows skips the bit-check (use NTFS ACLs there). |
| `literal:VALUE` | Inline plaintext. For placeholders like `literal:not-needed` (llama.cpp without auth) and tests. Avoid for real secrets in version-controlled config. |
| `oauth://<provider>` or `oauth://<provider>#<label>` | routectl-managed OAuth credential, populated by `routectl login <provider>` (Anthropic and Codex supported). The bare form addresses the default seat; `#label` selects a named seat from the provider's credential pool (see `routectl login --label`). Tokens persist in `~/.config/routectl/credentials.json` (chmod 0600); resolution checks near-expiry, refreshes under a per-provider single-flight mutex, and survives upstream 401 via `Provider::on_auth_failure`. See "Managed OAuth login" below. |

### Managed OAuth login

> Read [Responsible use](#responsible-use) below before pointing routectl-managed OAuth at production traffic. Per the Anthropic Agent SDK overview, claude.ai OAuth tokens may not be embedded in third-party products; the `oauth://anthropic` ref is for personal-use proxying with the operator's own subscription token.

For Anthropic (claude.ai) and OpenAI Codex (ChatGPT), routectl owns the full OAuth lifecycle so the operator does not have to snapshot or rotate JWTs by hand:

```bash
routectl login anthropic        # opens browser, runs PKCE, persists tokens
routectl login codex            # same, against the ChatGPT auth endpoint
routectl whoami                 # prints stored expiry per provider
routectl refresh anthropic      # force a refresh, regardless of expiry
routectl logout anthropic       # remove tokens for one provider
```

`login`, `logout`, and `refresh` accept `--label <name>` for multi-seat pools:

```bash
routectl login anthropic --label seat-b  # add a named seat without overwriting the default
```

Reference a named seat via `api_key_ref = "oauth://anthropic#seat-b"`.

Reference the credential in `[providers.X]` via `api_key_ref = "oauth://anthropic"` (or `"oauth://codex"`) plus `auth_kind = "oauth-bearer"`. Headless / SSH operators use `routectl login anthropic --print-url` (Anthropic only); Codex requires a browser-reachable callback and operators port-forward port 1455 instead. See [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) "claude-code as a gateway client" for the full operator setup including header packs and `forward_client_headers`.

## Provider classes

### `openai-compat`

Any host speaking `POST /v1/chat/completions` with the OpenAI body shape. Reasoning dialect is per-provider:

| Dialect | Wire signal | Format tag |
|---|---|---|
| `openai` | `reasoning_effort` request; reasoning hidden on response | `openai-responses-v1` |
| `deepseek` | `reasoning_content` field | `deepseek-v1` |
| `vllm` | `chat_template_kwargs.enable_thinking` + `reasoning_content` | `vllm-reasoning-v1` |
| `raw-think-tag` | `<think>...</think>` inline in `content` | `raw-think-tag-v1` |
| `openrouter` | already-normalized `reasoning_details` | `openrouter-passthrough-v1` |
| `passthrough` | none | `passthrough-v1` |

The `<think>` tag accumulator handles tags split across SSE chunk boundaries (state lives outside the dispatch path).

### `anthropic-api`

Hits `https://api.anthropic.com/v1/messages`. Two auth modes via `auth_kind`:

- `api-key` (default) -- `x-api-key: <key>` header.
- `oauth-bearer` -- `Authorization: Bearer <token>` header. Pair with `api_key_ref = "oauth://anthropic"` for routectl-managed login (see "Managed OAuth login" above).

Beta gates (`anthropic-beta` flags for prompt caching, 1M context, extended thinking) are independent of `auth_kind`. Declare them via `header_extras`:

```toml
[providers.anthropic]
header_extras = { "anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31" }
```

`header_extras` cannot override auth-bearing headers (`authorization`, `x-api-key`, `host`); collisions are dropped with a `tracing::warn!`.

Provider-level knobs relevant to claude-code-as-a-gateway use:

- `forward_client_headers: Vec<String>` -- allowlist of incoming client headers (typically `x-claude-code-session-id`, `x-claude-code-agent-id`, `x-claude-code-parent-agent-id`) that pass through to the upstream. Defaults to empty (drop everything).
- `context_management = true` -- routectl emulates Anthropic's `context-management-2025-06-27` beta server-side for upstreams that demand thinking-block echoback but do not implement the beta natively (e.g. DeepSeek `/anthropic`). Bounded LRU + TTL cache; strips the beta header and body field on egress. See [`docs/PROVIDER-QUIRKS.md`](docs/PROVIDER-QUIRKS.md) for when to flip it.

### `bedrock`

Native AWS Bedrock at `bedrock-runtime.<region>.amazonaws.com`. Four credential modes; pick one:

```toml
# Short-term Bedrock API key from the AWS console. Skips SigV4 entirely
# and sends Authorization: Bearer <key>.
creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

# Raw access key + secret key (+ optional session token).
[providers.bedrock-static.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
session_token_ref = "env://AWS_SESSION_TOKEN"  # optional

# Named profile. SSO sessions auto-refresh via aws-config.
creds = { kind = "profile", name = "my-bedrock-profile" }

# Standard AWS chain: env -> profile -> SSO -> IRSA -> IMDS.
creds = { kind = "default-chain" }
```

`api_shape = "invoke"` (default) sends the per-vendor body shape -- Anthropic Messages JSON for Claude. `api_shape = "converse"` uses the AWS vendor-neutral envelope; both are wired and live-tested for Anthropic models. Converse for non-Anthropic vendors (Mistral, Llama, Cohere) is staged.

Bedrock-specific knobs:

- `user_agent` -- per-provider UA override. Required when an IAM policy gates access via the `aws:UserAgent` condition key.
- `anthropic_beta` -- list of beta flags always sent on requests from this provider (operator-asserted floor; bypasses the global allowlist filter below).
- `additional_model_request_fields` -- free-form JSON merged into the request body for vendor-specific knobs.

**`[bedrock]` allowlists (optional, recommended in production).** AWS strict-schema validation 400s any unrecognized `anthropic_beta` flag or top-level body field. routectl ships no built-in default; populate operator-supplied lists in TOML to gate which entries reach AWS:

```toml
[bedrock]
allowed_betas       = ["context-1m-2025-08-07", "claude-code-20250219", ...]
allowed_body_fields = ["anthropic_version", "messages", "max_tokens", ...]
```

Empty lists (or omitted `[bedrock]` section) = pass-through (no filter applied) -- the discovery default. Run with `ROUTECTL_LOG=routectl_providers::bedrock=trace` to capture sent flags/fields, then populate the lists. See `examples/bedrock.toml` for the empirical 2026-05-12 baseline.

`BedrockCreds` redacts secret material in `Debug` output (no leaks via panic messages or `tracing` events).

## Routing

`[aliases]` maps incoming wire model strings to model nicknames declared in `[models.X]`. List values are fallback chains -- when the primary model's provider fails (rate limit, 5xx, network error), the router falls through to the next entry. Global retry policy lives in the top-level `[retry]` table (shown below).

```toml
[models.opus-bedrock]
provider = "bedrock"
upstream = "us.anthropic.claude-opus-4-20250514-v1:0"

[models.opus-anthropic]
provider = "anthropic"
upstream = "claude-opus-4-20250514"

[models.opus-or]
provider = "openrouter"
upstream = "anthropic/claude-opus-4-20250514"

[aliases]
heavy = ["opus-bedrock", "opus-anthropic", "opus-or"]

[retry]
max_attempts = 2
initial_backoff_ms = 250
backoff_multiplier = 2.0
jitter_ms = 50
retry_allowlist = [408, 429, 500, 502, 503, 504]
retry_on_429 = 4
retry_on_5xx = 1
request_timeout_ms = 60000
stream_first_byte_timeout_ms = 15000
```

Per-provider runtime gates (set inline on the provider definition):

| Field | Effect |
|---|---|
| `rpm_limit` | Token-bucket cap; over-limit falls through to next chain entry. |
| `circuit_failures` | Trip breaker after N consecutive failures. |
| `circuit_cooldown_ms` | Keep breaker open this long once tripped (default 30000). |

Per-request: `x-routectl-disable-fallbacks: 1` skips the chain walk; the first failure propagates verbatim.

### Routing wire models

The unified `[aliases]` table maps incoming wire `model` strings to model nicknames declared in `[models.X]`. Single string = one entry; list = fallback chain. Suffix-globs collapse per-version sprawl. `default = "..."` is the catch-all.

```toml
[aliases]
"claude-opus-*"   = "heavy"
"claude-sonnet-*" = "default"
"claude-haiku-*"  = "fast"
"gpt-5*"          = ["heavy", "default"]   # OpenAI-side fallback
default           = "default"
```

Or send `x-routectl-alias: heavy` -- the header always wins.

## Listener auth

Optional. When set, all ingress routes require a matching token via `x-api-key` or `Authorization: Bearer`. Comparison is constant-time.

```toml
[server.auth]
tokens = ["env://ROUTECTL_LISTENER_TOKEN"]
```

When omitted, the listener accepts unauthenticated requests (suitable for `127.0.0.1` dev).

## Architecture

```
crates/
  routectl-core/         Provider trait + canonical schema
                         (ChatRequest, ChatResponse, ChatChunk, Message,
                          ContentPart, SystemContent, ToolDef, CacheControl)
  routectl-providers/    openai_compat (6 dialects)
                         anthropic_api (api-key + oauth-bearer
                                        + context_management emulation)
                         bedrock (SigV4 + InvokeModel + Converse +
                                  eventstream)
                         openai_responses (ChatGPT Codex chatgpt-oauth)
  routectl-router/       alias resolution + fallback chain
                         + tier-1 retry (per-error-class caps, timeouts, jitter)
                         + tier-2 RPM bucket + circuit breaker
                         + capability filter (unsupported_features)
                         + provider factory
  routectl-auth/         SecretStore: env:// / file:// (TOCTOU-safe) /
                         literal: / oauth:// (PKCE login + atomic
                         credentials.json + lazy refresh)
  routectl-usage/        SQLite usage accounting (UsageRecord, UsageWriter,
                         UsageHandle, cost estimation, retention) + the
                         query layer behind the `routectl usage` CLI
  routectl-cli/          axum HTTP server (/v1/chat/completions + /v1/messages
                                          + /v1/messages/count_tokens)
                         + clap CLI (serve, login, logout, refresh,
                         whoami, test, config, usage)
                         + IngressAdapter trait (one file per ingress dialect)
```

The hub-and-spoke design means N+M translators, not NxM: a new ingress dialect is one file under `routectl-cli/src/ingress/`; a new egress provider is one `Provider` impl in `routectl-providers/`. Neither side knows about the other.

## Testing

```bash
# Unit + integration tests, no network.
cargo test --workspace --release

# Live integration matrix (opt-in; skips per-provider when its env key is absent).
export OPENROUTER_API_KEY=...
export OPENCODE_GO_API_KEY=...
export NIM_API_KEY=...
export AWS_BEARER_TOKEN_BEDROCK=...
export AWS_REGION=us-east-1

cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

See [`docs/TESTED_MODELS.md`](docs/TESTED_MODELS.md) for the verified model matrix.

## Out of scope

- Multi-user auth, TLS, persistent token store. Use a real proxy.
- Web UI / dashboard. CLI-only by design.
- Caching layer. Use a proxy if you want this.
- Cost-aware routing (overlap with [LMSYS RouteLLM](https://github.com/lm-sys/RouteLLM); different product).

If you need any of those, reach for [LiteLLM](https://github.com/BerriAI/litellm) or a dedicated proxy.

## Responsible use

routectl is a translation pipe. It speaks several wire protocols and forwards whatever credentials you supply, and it does not vouch for whether a particular credential is permitted to be used a particular way. **routectl does not support or condone gateway usage beyond what the upstream provider permits.** Read the upstream provider's terms before pointing routectl at production traffic.

Specifically:

- Anthropic publishes a gateway pattern at <https://code.claude.com/docs/en/llm-gateway> for first-party deployments. routectl's claude-code-as-a-gateway support implements that pattern.
- Per the Anthropic Agent SDK overview, claude.ai OAuth tokens may NOT be embedded in third-party products. The `oauth://anthropic` ref is for personal-use proxying with the operator's own subscription token; do not deploy a routectl instance that resolves your `oauth://anthropic` ref under other users' requests.
- Read Anthropic's [Acceptable Use Policy](https://www.anthropic.com/legal/aup) and [Usage Policy](https://www.anthropic.com/legal/usage-policy) (Anthropic API + Claude.ai) before production traffic.
- For Codex / ChatGPT credentials, read OpenAI's [Terms of Use](https://openai.com/policies/terms-of-use) and [Service Terms](https://openai.com/policies/service-terms).
- For Bedrock, the AWS service terms and the underlying foundation-model vendor's terms both apply.

See [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) "claude-code as a gateway client" for the full operating envelope and operator-setup checklist.

## Contributing

Issues and PRs welcome. [`CLAUDE.md`](CLAUDE.md) is the slim entry point with a routing index; deep references live under [`docs/`](docs/) (architecture, codemap, configuration, logging, wire gotchas, development workflow, per-provider quirks, tested models). [`ROADMAP.md`](ROADMAP.md) tracks the milestone-by-milestone trajectory.

Conventions:

- ASCII-only in source, comments, and commit messages (no em-dashes, curly quotes, emoji, or arrows).
- Functions under 50 lines, files under 800.
- One file per dialect, one row per quirk in the model-profile table.
- The live matrix proves wiring; tight files keep edits surgical.

## License

MIT. See [`LICENSE`](LICENSE).
