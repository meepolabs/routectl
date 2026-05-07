# routectl

A tiny local LLM router. Single Rust binary. Localhost-only by default. Drop-in OpenAI-compatible API with a unified reasoning surface (OpenRouter-shape) so any client that speaks OpenAI or OpenRouter speaks routectl.

Status: **v0.1.0-dev**. Two providers shipped (`openai-compat`, `anthropic-api`). Cookie-auth providers (`claude-cookie`, `chatgpt-cookie`) are scaffolded and feature-gated; the `wry` login flow lands in v0.2.

## Why

LiteLLM is huge and ships a server's worth of features (multi-tenant auth, dashboards, accounting, RBAC). For local dev you want one binary that:

- Routes requests across providers with fallback chains
- Stores secrets in your OS keychain
- Speaks OpenAI on the wire so every client just works
- Handles reasoning tokens cleanly across OpenAI o-series, Anthropic extended thinking, DeepSeek R1, Qwen3 thinking, vLLM-served reasoners, and raw `<think>...</think>` emitters

routectl is that binary. Nothing more.

## Scope (v0.1)

**In:**

- OpenAI-compatible HTTP server on `127.0.0.1:<port>` (refuses non-loopback bind without explicit `--unsafe-public`)
- Two provider classes built and tested:
  - `openai-compat` (covers DeepSeek, OpenRouter, OpenAI, OpenCode Go, NVIDIA NIM, llama.cpp, Together, Groq, anything OpenAI-shaped) with 5 reasoning dialects: `openai`, `deepseek`, `vllm`, `raw-think-tag`, `openrouter`, `passthrough`
  - `anthropic-api` (api.anthropic.com Messages API; `thinking` blocks with `signature` preserved across multi-turn tool use)
- Reasoning normalization to OpenRouter-shape `reasoning_details[]` array with provider-tagged `format`
- Streaming SSE both directions, including stateful `<think>` tag handling for tags split across chunk boundaries
- Fallback chain on 408/429/5xx/timeout (no fallback once first chunk has streamed)
- Per-provider retry with exponential backoff
- TOML config in `~/.config/routectl/config.toml`
- OS keychain via the `keyring` crate (mac/linux/win)

**Scaffolded but feature-gated (v0.2 lands the impl):**

- `claude-cookie` (claude.ai consumer session)
- `chatgpt-cookie` (chatgpt.com consumer session)
- `wry` webview popup for `routectl login`

**Out of scope (use LiteLLM if you want any of these):**

- Multi-user auth, TLS, persistent token store
- Web UI / dashboard
- Caching layer
- Cost-aware routing (that's [LMSYS RouteLLM](https://github.com/lm-sys/RouteLLM)'s lane)
- Rate limiting

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

The release binary is ~5.6MB stripped, single-file, no system dependencies beyond `libsecret`/`gnome-keyring` on Linux for the OS keychain.

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
- `file:///abs/path/to/key` -- file contents (trimmed of trailing whitespace). Refused unless `chmod 600` or `400`. Compatible with sops, age, doppler-cli, vault-agent, or any tool that drops a token into a file. Good if you keep secrets in `~/.secrets/*` mode 600.
- `literal:hunter2` -- inline plaintext. Use for placeholders like `literal:not-needed` (llama.cpp where no auth is required) and tests. Avoid for real secrets in version-controlled config.

Routectl does not bundle an OS-keychain integration. Most managed-secret tools can drop a file or set an env var as part of their workflow, which is the integration boundary.

## Model groups (recommended pattern)

Define one alias per cost/capability tier so callers can ask for `heavy`
or `cheap` without knowing which model is current. Aliases ARE the
group mechanism -- there's no separate group syntax.

```toml
[aliases.heavy]
chain = [
  "claude-pro:claude-opus-4-7",                      # subscription
  "anthropic:claude-opus-4-7",                       # API key
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
  routectl-providers/    openai_compat (5 dialects), anthropic_api, claude_cookie + chatgpt_cookie (stubs)
  routectl-router/       alias resolution + fallback chain + retry policy + provider factory
  routectl-auth/         SecretStore trait + KeyringStore (OS keychain) + MemoryStore (test mock)
  routectl-cli/          axum HTTP server + clap subcommands (serve, test, config, login)
```

128 tests pass across the workspace. `cargo test --workspace` -- 0 failures, 0 warnings.

## Tested models

A live integration matrix in `crates/routectl-cli/tests/live_matrix.rs` exercises representative models across OpenRouter, OpenCode-Go, and NIM end-to-end through the router. Run with:

```bash
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

Tests skip cleanly when their key is absent. See [`docs/TESTED_MODELS.md`](docs/TESTED_MODELS.md) for the full per-provider list with status and any provider-specific quirks routectl handles.

## Responsible-use note

The `claude-cookie` and `chatgpt-cookie` providers (v0.2) reuse your existing browser session against `claude.ai` and `chatgpt.com`. **You are responsible for compliance with the upstream provider's Terms of Service.** routectl makes no representations about the legality or sanctioned-ness of consumer-session use; treat it as you would any reverse-engineered consumer API client.

These providers are feature-gated and **not enabled in default builds**. You opt in when building from source.

## License

MIT. See [`LICENSE`](LICENSE).
