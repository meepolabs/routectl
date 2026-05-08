# routectl

A local LLM router. One Rust binary, listening on `127.0.0.1`, that proxies OpenAI-compatible (`/v1/chat/completions`) and Anthropic Messages (`/v1/messages`) requests across multiple backends with fallback, retry, and unified reasoning normalization.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![Workspace tests](https://img.shields.io/badge/tests-313%2B%20passing-brightgreen.svg)](#testing)

## Features

- **Two ingress dialects** -- OpenAI Chat Completions and Anthropic Messages, both feeding one canonical internal request shape.
- **Three egress provider classes**:
  - `openai-compat` -- any host that speaks the OpenAI body shape (OpenAI, DeepSeek, OpenRouter, Groq, NIM, vLLM, llama.cpp, etc.).
  - `anthropic-api` -- native Anthropic Messages API with `x-api-key` or `Authorization: Bearer` auth.
  - `bedrock` -- native AWS Bedrock with SigV4 signing, full credential chain (env / static / profile / SSO / IRSA / IMDS) or short-term bearer keys, InvokeModel body shape today.
- **Unified reasoning surface** -- OpenRouter-shape `reasoning_details[]` with provider-tagged `format`. Six dialects on the openai-compat side; Anthropic thinking blocks (with `signature`) preserved across multi-turn tool use.
- **Cache control round-trip** -- Anthropic prompt-caching `cache_control` and `anthropic_beta` flags pass through losslessly on Anthropic-in -> Anthropic-out and Anthropic-in -> Bedrock-Invoke-out paths. Verified live: cache miss writes N tokens, cache hit reads the same N back.
- **Reliability** -- per-error-class retry caps (429 / 5xx / network), per-attempt timeouts, jittered backoff, RPM token bucket per provider, passive circuit breaker with single-probe half-open.
- **Secrets** -- `env://VAR`, `file:///abs/path` (chmod-600 / 400 enforced on Unix, TOCTOU-safe), `literal:value`. No keychain integration, no auto-discovery.
- **Local-first** -- binds to `127.0.0.1` by default; refuses non-loopback bind without explicit `--unsafe-public`.

## Install

Requires Rust 1.75+.

```bash
git clone https://github.com/meepolabs/routectl
cd routectl
cargo build --release
./target/release/routectl --help
```

The release binary is ~6.5 MB stripped with default features (includes the AWS SDK dependency tree for Bedrock). For a lean build without AWS deps:

```bash
cargo build --release \
  --no-default-features \
  --features openai-compat,anthropic-api \
  -p routectl-providers
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
type = "openai-compat"
base_url = "https://api.deepseek.com/v1"
api_key_ref = "env://DEEPSEEK_API_KEY"
reasoning_dialect = "deepseek"

[providers.anthropic]
type = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[providers.bedrock]
type = "bedrock"
region = "us-east-1"
model_id = "us.anthropic.claude-haiku-4-5-20251001-v1:0"
creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

[aliases.fast]
chain = ["deepseek:deepseek-chat"]

[aliases.heavy]
chain = [
  "bedrock:us.anthropic.claude-opus-4-20250514-v1:0",
  "anthropic:claude-opus-4-20250514",
]
```

See [`examples/config.toml`](examples/config.toml) for the full surface, including per-alias retry overrides, RPM limits, circuit breakers, and ingress alias maps.

### Secret references

Routectl resolves credentials through one of three URI schemes per provider. There is no auto-discovery; you choose the source explicitly.

| Scheme | Meaning |
|---|---|
| `env://VAR_NAME` | Process env var. |
| `file:///abs/path` | File contents (trailing whitespace trimmed). On Unix, refused if group/other have any permissions; `chmod 600` or `400` recommended. Compatible with sops, age, doppler-cli, vault-agent, etc. Windows skips the bit-check (use NTFS ACLs there). |
| `literal:VALUE` | Inline plaintext. For placeholders like `literal:not-needed` (llama.cpp without auth) and tests. Avoid for real secrets in version-controlled config. |

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
- `oauth-bearer` -- `Authorization: Bearer <token>` header.

Beta gates (`anthropic-beta` flags for prompt caching, 1M context, extended thinking) are independent of `auth_kind`. Declare them via `extra_headers`:

```toml
[providers.anthropic.extra_headers]
"anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31"
```

`extra_headers` cannot override auth-bearing headers (`authorization`, `x-api-key`, `host`); collisions are dropped with a `tracing::warn!`.

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

`api_shape = "invoke"` (default) sends the per-vendor body shape -- Anthropic Messages JSON for Claude. `api_shape = "converse"` for the AWS vendor-neutral envelope is wired (auth, transport, eventstream framing) but body translation is staged for v0.5.

Bedrock-specific knobs:

- `user_agent` -- per-provider UA override. Required when an IAM policy gates access via the `aws:UserAgent` condition key.
- `anthropic_beta` -- list of beta flags merged into the request body's top-level `anthropic_beta` array (Invoke).
- `additional_model_request_fields` -- free-form JSON merged into the request body for vendor-specific knobs.

`BedrockCreds` redacts secret material in `Debug` output (no leaks via panic messages or `tracing` events).

## Routing

Aliases map a name to a fallback chain. When the primary fails (rate limit, 5xx, network error), the router falls through to the next entry.

```toml
[aliases.heavy]
chain = [
  "bedrock:us.anthropic.claude-opus-4-20250514-v1:0",
  "anthropic:claude-opus-4-20250514",
  "openrouter:anthropic/claude-opus-4-20250514",
]

[aliases.heavy.retry]
max_attempts = 2
initial_backoff_ms = 250
backoff_multiplier = 2.0
jitter_ms = 50
fallback_on_status = [408, 429, 500, 502, 503, 504]
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

### Ingress alias maps

When a client can't override the `model` field directly, map model IDs to routectl aliases server-side:

```toml
[ingress.anthropic.aliases]
"claude-opus-4-20250514"      = "heavy"
"claude-sonnet-4-5-20250929"  = "default"
"claude-haiku-4-5-20251001"   = "fast"

[ingress.openai.aliases]
"gpt-5"        = "heavy"
"gpt-4o-mini"  = "fast"
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
                         anthropic_api (api-key + oauth-bearer)
                         bedrock (SigV4 + InvokeModel + eventstream)
  routectl-router/       alias resolution + fallback chain
                         + tier-1 retry (per-error-class caps, timeouts, jitter)
                         + tier-2 RPM bucket + circuit breaker
                         + provider factory
  routectl-auth/         SecretStore: env:// / file:// (TOCTOU-safe) / literal:
  routectl-cli/          axum HTTP server (/v1/chat/completions + /v1/messages)
                         + clap CLI (serve, test, config)
                         + IngressAdapter trait (one file per ingress dialect)
```

The hub-and-spoke design means N+M translators, not N×M: a new ingress dialect is one file under `routectl-cli/src/ingress/`; a new egress provider is one `Provider` impl in `routectl-providers/`. Neither side knows about the other.

## Testing

```bash
# Unit + integration tests, no network.
cargo test --workspace --release

# Live integration matrix (opt-in; skips per-provider when its env key is absent).
export OPENROUTER_API_KEY=...
export NIM_API_KEY=...
export ANTHROPIC_API_KEY=...
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

routectl speaks several wire protocols and forwards whatever credentials you supply. It does not vouch for whether a particular credential is permitted to be used a particular way. Read the upstream provider's terms before pointing routectl at production traffic.

## Contributing

Issues and PRs welcome. See [`CLAUDE.md`](CLAUDE.md) for the development runbook (tests, where to put new code, common failure-mode gotchas), and [`ROADMAP.md`](ROADMAP.md) for the milestone-by-milestone trajectory.

Conventions:

- ASCII-only in source, comments, and commit messages (no em-dashes, curly quotes, emoji, or arrows).
- Functions under 50 lines, files under 800.
- One file per dialect, one row per quirk in the model-profile table.
- The live matrix proves wiring; tight files keep edits surgical.

## License

MIT. See [`LICENSE`](LICENSE).
