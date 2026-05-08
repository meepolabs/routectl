# routectl

A tiny local LLM router. Single Rust binary. Localhost-only by default. Drop-in OpenAI-compatible API with a unified reasoning surface (OpenRouter-shape) so any client that speaks OpenAI or OpenRouter speaks routectl.

Status: **v0.3.0** (in flight on `feat/v0.3-bedrock`). Three default-on provider classes: `openai-compat`, `anthropic-api` (api-key + OAuth-bearer auth), and native `bedrock` (SigV4 + eventstream streaming, full AWS credential chain). Tier-1 retry policy + tier-2 per-provider rate limit and circuit breaker. Cookie-auth providers (`claude-cookie`, `chatgpt-cookie`) are scaffolded and feature-gated.

## Why

You're using Claude Code, opencode, Codex, Cursor, or any other OpenAI-compatible client and you want it to round-robin or fall back across multiple backends -- some hosted (DeepSeek, OpenRouter, Groq, NIM, OpenAI), some local (llama.cpp, vLLM). You want a single small binary, on `127.0.0.1`, that does just that:

- Routes requests across providers with fallback chains
- Loads secrets from env vars or chmod-600 files (no auto-discovery, no keychain)
- Speaks OpenAI on the wire so every client just works
- Handles reasoning tokens cleanly across OpenAI o-series, Anthropic extended thinking, DeepSeek R1, Qwen3 thinking, vLLM-served reasoners, and raw `<think>...</think>` emitters

routectl is that binary. Nothing more.

## Scope (v0.3)

**In:**

- OpenAI-compatible HTTP server on `127.0.0.1:<port>` (refuses non-loopback bind without explicit `--unsafe-public`)
- Three default-on provider classes:
  - `openai-compat` (covers DeepSeek, OpenRouter, OpenAI, OpenCode Go, NVIDIA NIM, llama.cpp, Together, Groq, anything OpenAI-shaped) with 6 reasoning dialects: `openai`, `deepseek`, `vllm`, `raw-think-tag`, `openrouter`, `passthrough`
  - `anthropic-api` (api.anthropic.com Messages API; `thinking` blocks with `signature` preserved across multi-turn tool use; `x-api-key` or `Authorization: Bearer` auth; per-provider `extra_headers` and `user_agent` overrides)
  - `bedrock` (native AWS Bedrock; SigV4 signing or short-term bearer keys; full AWS credential chain via `aws-config`; `InvokeModel` shape today, `Converse` adapter planned for v0.4.0; eventstream binary frame decoder for streaming)
- Reasoning normalization to OpenRouter-shape `reasoning_details[]` array with provider-tagged `format`
- Streaming SSE both directions, including stateful `<think>` tag handling for tags split across chunk boundaries
- Fallback chain on 408/429/5xx/timeout (no fallback once first chunk has streamed)
- Tier-1 retry: per-error-class caps (`retry_on_429` / `retry_on_5xx` / `retry_on_network`), per-attempt `request_timeout_ms`, `stream_first_byte_timeout_ms`, jittered backoff
- Tier-2 routing gates: per-provider `rpm_limit` (token bucket), passive circuit breaker (`circuit_failures` + cooldown, single-probe half-open under concurrent load), per-request `x-routectl-disable-fallbacks` header
- TOML config in `~/.config/routectl/config.toml`
- Secret resolution from `env://`, `file://` (chmod-600 / 400, TOCTOU-safe, 1 MiB cap), or inline `literal:` URIs

**Scaffolded but feature-gated:**

- `claude-cookie` (claude.ai consumer session)
- `chatgpt-cookie` (chatgpt.com consumer session)

**Out of scope** -- if you need any of the below, reach for [LiteLLM](https://github.com/BerriAI/litellm) or a dedicated proxy:

- Multi-user auth, TLS, persistent token store
- Web UI / dashboard
- Caching layer
- Cost-aware routing (that's [LMSYS RouteLLM](https://github.com/lm-sys/RouteLLM)'s lane)

## Schema

routectl's outward shape mirrors [OpenRouter's reasoning normalization](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens):

- Request: standard OpenAI chat completion + optional `reasoning: {effort, max_tokens, exclude, enabled}` + optional `chat_template_kwargs` + optional `provider_extras`
- Response: standard OpenAI choices + `message.reasoning` (legacy plaintext) + `message.reasoning_details[]` (typed blocks with provider-tagged `format` + optional `signature`)
- Streaming: same, on `delta.reasoning` / `delta.reasoning_details`

Set `legacy_compat = "openai"` in config to strip extensions for clients that gag on extra fields.

## Build

Requires Rust 1.75+.

```bash
git clone https://github.com/meepolabs/routectl
cd routectl
cargo build --release
./target/release/routectl --help
```

The release binary is roughly 6.5 MB stripped with default features (which include the `bedrock` provider and its `aws-config` dependency tree), or about 5.6 MB if you build with `--no-default-features --features openai-compat,anthropic-api`. Single-file, no system-library dependencies. Place it on your PATH.

## Quickstart

```bash
# Initialize a config from the example
mkdir -p ~/.config/routectl
routectl config example > ~/.config/routectl/config.toml

# Edit it: add your providers, aliases, secrets

# Validate
routectl config check

# Run
routectl serve
# -> routectl listening on http://127.0.0.1:8787

# In another terminal, hit it like any OpenAI-compatible endpoint:
curl -N -X POST http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "fast",
    "messages": [{"role":"user","content":"say hi"}],
    "stream": true
  }'
```

Reasoning model alias:

```bash
curl -X POST http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "reasoning",
    "messages": [{"role":"user","content":"What is 17 * 23?"}],
    "reasoning": {"effort": "high"}
  }'
# Response includes choices[0].message.reasoning_details[] with format-tagged blocks.
```

One-shot completion via the CLI (no curl needed):

```bash
routectl test fast --prompt "say hi in five words"
```

## Config

See [`examples/config.toml`](examples/config.toml) for the full surface. Minimal example:

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

[aliases.fast]
chain = ["deepseek:deepseek-chat"]

[aliases.heavy]
chain = ["anthropic:claude-opus-4-7", "deepseek:deepseek-reasoner"]
```

Secret references (the `*_ref` fields). Routectl never auto-discovers credentials from other tools; you pick the source per provider:

- `env://VAR_NAME` -- process env var. Silent on every platform. Trade-off: the value is visible to anything that can read this process's env (e.g. `/proc/<pid>/environ`). Fine for local dev with shell-managed keys.
- `file:///abs/path/to/key` -- file contents (trimmed of trailing whitespace). On Unix, refused if group or other have any permissions (`chmod 600` or `400` recommended). Windows skips this check -- the permission-bit enforcement is Unix-only, since NTFS ACLs are not portably enforceable from Rust; secure the file via filesystem ACLs there. Compatible with sops, age, doppler-cli, vault-agent, or any tool that drops a token into a file.
- `literal:hunter2` -- inline plaintext. Use for placeholders like `literal:not-needed` (llama.cpp where no auth is required) and tests. Avoid for real secrets in version-controlled config.

Routectl does not bundle an OS-keychain integration. Most managed-secret tools can drop a file or set an env var as part of their workflow, which is the integration boundary.

## Anthropic auth: api-key vs OAuth bearer

The `anthropic-api` provider supports two wire formats, both pointed at `https://api.anthropic.com/v1/messages`:

- `auth_kind = "api-key"` (default) -- standard `x-api-key: <key>` header. Use with API keys provisioned through the Anthropic Console (`sk-ant-api03-...`).
- `auth_kind = "oauth-bearer"` -- sends `Authorization: Bearer <token>`. Use when you have an Anthropic OAuth access token to present.

Beta gates are not coupled to the auth kind. To enable `anthropic-beta` flags (1M context, prompt caching, extended thinking, OAuth gates, etc.) declare them yourself via `extra_headers`:

```toml
[providers.anthropic-oauth]
type = "anthropic-api"
api_key_ref = "file:///abs/path/to/anthropic-oauth-token"
auth_kind = "oauth-bearer"
extra_headers = { "anthropic-beta" = "oauth-2025-04-20,context-1m-2025-08-07" }

[providers.anthropic-api]
type = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
extra_headers = { "anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31" }
```

`extra_headers` cannot override auth-bearing headers (`authorization`, `x-api-key`, `host`); collisions are dropped with a `tracing::warn!`.

The credential is resolved once at startup and held in process memory. To rotate a token on disk, restart routectl. Live OAuth refresh (re-read on rotation, plus full refresh-token round-trip against the OAuth provider) is on the v0.4.0 roadmap.

> OAuth access tokens may carry usage restrictions imposed by their issuer. routectl just speaks the wire protocol; check the upstream provider's terms before pointing it at production traffic.

## Bedrock provider

`type = "bedrock"` talks to `bedrock-runtime.<region>.amazonaws.com` directly, no gateway in the middle. Two request shapes, selected per-provider via `api_shape`:

- `invoke` (default) -- per-vendor body shape; today this is the Anthropic Messages JSON shape with `anthropic_version = "bedrock-2023-05-31"`. Reuses the `anthropic-api` request/response normalization, so multi-turn tool use, thinking signatures, and cache breakpoints round-trip unchanged.
- `converse` -- AWS's vendor-neutral envelope. The transport, auth, and streaming framing are wired; body translation lands in v0.4.0.

Credentials resolve through one of four mutually exclusive `creds.kind` shapes:

```toml
# Short-term Bedrock API key from the AWS console. Skips SigV4 entirely
# and sends Authorization: Bearer <key>. Useful for quick demos; expires
# in hours.
[providers.bedrock-bearer]
type = "bedrock"
region = "us-east-1"
model_id = "us.anthropic.claude-opus-4-7"
creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" }

# Raw access key + secret key (+ optional session token). Each value
# resolves through a routectl SecretRef URI.
[providers.bedrock-static.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
session_token_ref = "env://AWS_SESSION_TOKEN"  # optional
[providers.bedrock-static]
type = "bedrock"
region = "us-east-1"
model_id = "us.anthropic.claude-opus-4-7"

# Named profile from ~/.aws/credentials / ~/.aws/config. SSO sessions
# auto-refresh via aws-config.
[providers.bedrock-profile]
type = "bedrock"
region = "us-east-1"
model_id = "us.anthropic.claude-opus-4-7"
creds = { kind = "profile", name = "my-bedrock-profile" }

# Standard AWS chain: env -> profile -> SSO -> web identity / IRSA ->
# EC2/ECS metadata. Picks whichever first resolves.
[providers.bedrock-default]
type = "bedrock"
region = "us-east-1"
model_id = "us.anthropic.claude-opus-4-7"
creds = { kind = "default-chain" }
```

A few Bedrock-specific knobs that come up in practice:

- `user_agent` -- per-provider UA override. Required when an IAM policy gates access via the `aws:UserAgent` condition key.
- `anthropic_beta` -- list of beta flags merged into the request body's top-level `anthropic_beta` array (Invoke) or `additionalModelRequestFields.anthropic_beta` (Converse). Independent of `extra_headers`.
- `additional_model_request_fields` -- free-form JSON merged into the request body, for vendor-specific knobs that don't yet have a dedicated config field.

The `bedrock` feature is on by default; library consumers who want to skip the AWS dependency tree can build with `--no-default-features --features openai-compat,anthropic-api`. AWS credential refresh (SSO token rotation, role-credential refresh, IMDS rotation) is handled by `aws-config` itself; routectl calls `provide_credentials()` per signing request and the SDK does the right thing.

## Model groups (recommended pattern)

Define one alias per cost/capability tier so callers can ask for `heavy`
or `cheap` without knowing which model is current. Aliases ARE the
group mechanism -- there's no separate group syntax.

```toml
[aliases.heavy]
chain = [
  "anthropic:claude-opus-4-7",
  "openai:gpt-5",
  "openrouter:anthropic/claude-opus-4-7",            # last-ditch
]

[aliases.med]
chain = [
  "opencode-go:deepseek-v4-pro",
  "nim:meta/llama-3.3-70b-instruct",
  "openrouter:meta-llama/llama-3.3-70b-instruct",
]

[aliases.cheap]
chain = [
  "opencode-go:deepseek-v4-flash",
  "openrouter:deepseek/deepseek-v4-flash",
  "llama-local:qwen3.6",
]
```

When a tier's primary fails (rate limit, 5xx, network error), the
router falls through to the next entry. See `examples/config.toml`
for the full set with retry overrides per tier.

## Routing policy knobs

Per-alias `[aliases.<name>.retry]` block (and a global `[retry]` for
defaults):

| Field | Default | Effect |
|---|---|---|
| `max_attempts` | 2 | Retry cap per provider in chain |
| `initial_backoff_ms` | 250 | Exponential backoff start |
| `backoff_multiplier` | 2.0 | Per-attempt growth |
| `jitter_ms` | 0 | Random extra ms on each sleep -- avoids thundering-herd retries |
| `fallback_on_status` | `[408,429,500,502,503,504]` | Status codes that trigger fallback |
| `retry_on_429` | (`max_attempts`) | Override retry cap for 429 specifically |
| `retry_on_5xx` | (`max_attempts`) | Override for 5xx |
| `retry_on_network` | (`max_attempts`) | Override for status 0 (DNS, connect, TLS, timeout) |
| `request_timeout_ms` | none | Cap each attempt; expiry treated as network error |
| `stream_first_byte_timeout_ms` | none | Abandon stream if no chunk arrives in this window |

Per-provider runtime gates (set inline on the provider definition):

| Field | Effect |
|---|---|
| `rpm_limit` | Token-bucket cap; over-limit falls through to next chain entry |
| `circuit_failures` | Trip breaker after N consecutive failures |
| `circuit_cooldown_ms` | Keep breaker open this long once tripped (default 30000) |

Per-request:

- Header `x-routectl-disable-fallbacks: 1` -- only the first chain entry is tried, the first failure propagates verbatim.

## Reasoning dialects (per-provider)

| Dialect | Wire signal | Format tag |
|---|---|---|
| `openai` | `reasoning_effort: high` (request); reasoning hidden in chat-completions | `openai-responses-v1` |
| `deepseek` | `reasoning_content` field | `deepseek-v1` |
| `vllm` | `chat_template_kwargs.enable_thinking` + `reasoning_content` | `vllm-reasoning-v1` |
| `raw-think-tag` | `<think>...</think>` inline in `content` | `raw-think-tag-v1` |
| `openrouter` | already-normalized `reasoning_details` | `openrouter-passthrough-v1` |
| `passthrough` | none | `passthrough-v1` |

Anthropic extended thinking is handled by the `anthropic-api` provider directly: `thinking` blocks (with `signature`) are normalized to `reasoning_details[format="anthropic-claude-v1"]`, and the round-trip preserves signatures during tool-use loops.

## Architecture

```
crates/
  routectl-core/         Provider trait + OpenRouter-shape schema (request/response/chunk types)
  routectl-providers/    openai_compat (6 dialects), anthropic_api (api-key + oauth-bearer),
                         bedrock (SigV4 + InvokeModel + eventstream; Converse stubbed for v0.4.0),
                         claude_cookie + chatgpt_cookie (feature-gated stubs)
  routectl-router/       alias resolution + fallback chain + tier-1 retry + tier-2 RPM/breaker + provider factory
  routectl-auth/         SecretStore trait + MemoryStore resolving env:// / file:// (TOCTOU-safe) / literal:
  routectl-cli/          axum HTTP server + clap subcommands (serve, test, config, login)
```

Run `cargo test --workspace --features bedrock` for the full suite. The live integration matrix is opt-in via the `live-integration` feature and skips per-provider when its env key is absent.

## Tested models

A live integration matrix in `crates/routectl-cli/tests/live_matrix.rs` exercises representative models across OpenRouter, OpenCode-Go, and NIM end-to-end through the router. (Bedrock entries land in v0.4.0 alongside the doctor command.) Run with:

```bash
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

Tests skip cleanly when their key is absent. See [`docs/TESTED_MODELS.md`](docs/TESTED_MODELS.md) for the full per-provider list with status and any provider-specific quirks routectl handles.

## Responsible-use note

The `claude-cookie` and `chatgpt-cookie` providers (post-v0.3) reuse your existing browser session against `claude.ai` and `chatgpt.com`. **You are responsible for compliance with the upstream provider's Terms of Service.** routectl makes no representations about the legality or sanctioned-ness of consumer-session use; treat it as you would any reverse-engineered consumer API client.

These providers are feature-gated and **not enabled in default builds**. You opt in when building from source.

The same general principle applies to any auth path routectl exposes: the binary speaks several wire formats; it does not vouch for whether a particular credential is permitted to be used a particular way. Read the upstream provider's terms before pointing routectl at production traffic.

## License

MIT. See [`LICENSE`](LICENSE).
