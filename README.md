# routectl

**One local endpoint for every LLM provider you use.** routectl is a
single Rust binary that sits on `127.0.0.1` and speaks three ingress
dialects -- OpenAI Chat Completions, Anthropic Messages, and OpenAI
Responses -- routing every request across five provider classes with
fallback chains, per-failure-class retry, learned capability routing,
and prompt-cache-preserving translation.

Point Claude Code, codex, opencode, or any OpenAI/Anthropic SDK at it
and get provider redundancy, cost observability, and one config file
for all your keys -- without changing the client.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust 1.95](https://img.shields.io/badge/Rust-1.95-orange.svg)](https://www.rust-lang.org)
[![Workspace tests](https://img.shields.io/badge/tests-5900%2B%20passing-brightgreen.svg)](#testing)

## Why routectl

- **Real fallback, not just retries.** Aliases map to fallback chains
  across providers; failures are classified (`rate-limited`, `auth`,
  `bad-request`, ...) and each class has its own retry/fallback
  policy, RPM buckets, and a passive circuit breaker.
- **Translation that preserves the hard parts.** Reasoning blocks
  (six openai-compat dialects + Anthropic thinking signatures),
  prompt-cache `cache_control` round-trips, tool use, and images all
  survive cross-provider hops. Byte-level care keeps upstream prompt
  caches warm.
- **Learns what your providers can actually do.** A capability
  registry learns from live rejections and response evidence, persists
  across restarts, and routes requests away from targets that cannot
  honor them (structured output on a model that lacks it, for
  example). `routectl doctor` renders the truth matrix; `routectl
  probe --capabilities` verifies actively.
- **Local-first and refs-only.** Binds loopback by default and
  refuses public binds without an explicit flag. Config stores secret
  *references* (`env://`, `file://`, `oauth://`) -- never plaintext;
  inline literals are rejected outright.
- **Observability built in.** A per-request SQLite usage ledger, a
  read-only dashboard at `GET /`, `/status` JSON panels, and
  `routectl usage` cost summaries over calendar windows.
- **First-class Claude Code gateway.** Implements Anthropic's
  published gateway pattern, including an optional front-proxy that
  keeps Remote Control working while inference routes through
  routectl. See [Responsible use](#responsible-use).

## Install

### Pre-built binaries

Bare binaries are published per release for linux x86_64, linux
aarch64, macos aarch64, and windows x86_64:

```bash
# linux x86_64
curl -fL https://github.com/meepolabs/routectl/releases/latest/download/routectl-$(curl -s https://api.github.com/repos/meepolabs/routectl/releases/latest | grep tag_name | cut -d'"' -f4 | sed 's/^v//')-linux-x86_64 -o /usr/local/bin/routectl

# macos aarch64 (apple silicon)
curl -fL https://github.com/meepolabs/routectl/releases/latest/download/routectl-$(curl -s https://api.github.com/repos/meepolabs/routectl/releases/latest | grep tag_name | cut -d'"' -f4 | sed 's/^v//')-macos-aarch64 -o /usr/local/bin/routectl

chmod +x /usr/local/bin/routectl
routectl --help
```

`curl` does not set the `com.apple.quarantine` xattr, so macOS
Gatekeeper does not prompt. If you downloaded via a browser instead,
run once: `xattr -d com.apple.quarantine /usr/local/bin/routectl`

Releases ship a cosign-signed `SHA256SUMS`; verify with:

```bash
cosign verify-blob \
  --certificate-identity-regexp '^https://github\.com/meepolabs/routectl/\.github/workflows/release\.yml@refs/tags/v[0-9]' \
  --certificate-github-workflow-repository meepolabs/routectl \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --bundle SHA256SUMS.cosign.bundle SHA256SUMS

sha256sum -c SHA256SUMS
```

### From source

Requires Rust 1.95.

```bash
git clone https://github.com/meepolabs/routectl
cd routectl
cargo build --release
./target/release/routectl --help
```

The release binary includes the AWS SDK for Bedrock (the binary
always links it). Library embedders can build `routectl-providers`
AWS-free: `cargo check -p routectl-providers --no-default-features
--features openai-compat,anthropic-api`.

## Quick start

The guided path -- `routectl init` walks provider setup, secret
capture, and routing, then offers a verification probe:

```bash
routectl init
routectl serve
# routectl listening on http://127.0.0.1:8787
```

Or by hand:

```bash
mkdir -p ~/.config/routectl
routectl config example > ~/.config/routectl/config.toml
# edit: add provider keys + aliases
routectl config check
routectl serve
```

Then send requests in whichever dialect your client speaks:

```bash
# OpenAI Chat Completions
curl -N -X POST http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model": "fast", "messages": [{"role": "user", "content": "say hi"}], "stream": true}'

# Anthropic Messages
curl -N -X POST http://127.0.0.1:8787/v1/messages \
  -H 'Content-Type: application/json' \
  -d '{"model": "fast", "max_tokens": 64, "messages": [{"role": "user", "content": "say hi"}], "stream": true}'

# One-shot via CLI
routectl test fast --prompt "say hi in five words"
```

`routectl doctor` diagnoses a setup end to end; the dashboard at
`http://127.0.0.1:8787/` shows live usage, target health, and the
capability matrix.

## How it works

routectl is a hub-and-spoke translation pipe: three ingress dialects
parse into one canonical request shape, the router resolves the alias
to a fallback chain and applies policy (retry, capability filter,
cache planning), and one of five egress provider classes translates
back out. N+M translators instead of NxM -- a new ingress dialect or
egress provider is one file that knows nothing about the other side.

```
ingress                      router                       egress
  /v1/chat/completions --\                            /--> openai-compat
  /v1/messages ----------+--> ChatRequest --> chain --+--> anthropic-api
  /v1/responses ---------/    (canonical)    walk     +--> bedrock
                                                      +--> openai-responses
                                                      \--> gemini
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the crate map
and [`docs/CODEMAP.md`](docs/CODEMAP.md) for per-file detail.

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

[models.fast]
provider = "deepseek"
upstream = "deepseek-chat"

[models.heavy]
provider = "anthropic"
upstream = "claude-opus-4-20250514"
supports_adaptive_thinking = true

[aliases]
fast  = "fast"
heavy = ["heavy", "fast"]        # fallback chain
"claude-opus-*" = "heavy"        # suffix-glob routing
default = "fast"
```

The full config surface -- per-failure-class retry policy
(`[retry.classes]`), capability overrides, cache knobs, the model
catalog, hot-reload, and `config migrate` for schema upgrades -- is
documented in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md), with
a committed JSON Schema (`routectl.schema.json`) for editor
completion.

### Secret references

Credentials resolve through URI schemes; there is no auto-discovery.

| Scheme | Meaning |
|---|---|
| `env://VAR_NAME` | Process env var. |
| `file:///abs/path` | File contents (trailing whitespace trimmed). On Unix, refused unless owner-only (`chmod 600`/`400`). Compatible with sops, age, doppler-cli, vault-agent, etc. |
| `oauth://<provider>` or `oauth://<provider>#<label>` | routectl-managed OAuth credential, populated by `routectl login <provider>` (anthropic, codex, xai, antigravity). `#label` selects a named seat from a credential pool. Tokens persist to `~/.config/routectl/credentials.json` (0600) with automatic refresh and 401 recovery. |

Inline `literal:` values are rejected at parse and resolve -- pipe
secrets with `--api-key-stdin`, use the hidden interactive prompt, or
reference `env://`.

### Managed OAuth login

> Read [Responsible use](#responsible-use) before pointing
> routectl-managed OAuth at production traffic.

```bash
routectl login anthropic          # browser PKCE flow, tokens persisted
routectl login codex              # same, against the ChatGPT auth endpoint
routectl whoami                   # stored expiry per provider
routectl refresh anthropic        # force a refresh
routectl login anthropic --label seat-b   # add a named seat to the pool
```

Reference the credential via `api_key_ref = "oauth://anthropic"` with
`auth_kind = "oauth-bearer"`.

## Provider classes

| Kind | Speaks | Auth |
|---|---|---|
| `openai-compat` | Any OpenAI-body host (OpenAI, DeepSeek, OpenRouter, Groq, NIM, vLLM, llama.cpp, ...) with six reasoning dialects | API key |
| `anthropic-api` | Native Anthropic Messages, incl. server-side `context-management` emulation and Claude Code gateway support | API key, OAuth bearer, or forwarded client credential |
| `bedrock` | AWS Bedrock InvokeModel + Converse, plus the Bedrock mantle bearer lanes | SigV4 (static / profile / SSO / IRSA / IMDS chain) or short-term bearer key |
| `openai-responses` | OpenAI Responses API / ChatGPT Codex | chatgpt-oauth JWT, API key, or mantle bearer |
| `gemini` | Native Google Gemini `generateContent` | API key or Cloud Code OAuth |

Per-provider knobs (RPM limits, circuit breaker, `header_extras`,
beta gates, Bedrock allowlists) and per-dialect reasoning details are
in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) and
[`docs/PROVIDER-QUIRKS.md`](docs/PROVIDER-QUIRKS.md).

## Routing and reliability

`[aliases]` maps wire model strings to model nicknames; list values
are fallback chains and suffix-globs collapse per-version sprawl. A
failed attempt is classified into a stable failure class, and each
class carries its own retry/fallback policy -- overridable per class
via `[retry.classes.<class>]` and per provider via status remaps.
Rate limiting is a per-provider token bucket; sustained failures trip
a passive circuit breaker with single-probe half-open recovery.

Per-request escape hatches: `x-routectl-alias: <name>` picks the
alias explicitly; `x-routectl-disable-fallbacks: 1` skips the chain
walk.

## Observability

- `GET /` -- single-file read-only dashboard (usage, target health,
  effective config, doctor findings, the capability matrix). No
  mutation routes exist, by construction.
- `/status/{usage,health,config,doctor}` -- the same panels as JSON.
- `routectl usage` -- cost and token summaries from the per-request
  SQLite ledger, over calendar windows or custom ranges, grouped by
  model / provider / alias.
- `routectl doctor [--json]` -- read-only end-to-end diagnosis with a
  stable exit-code contract for scripting.
- Structured `tracing` logs throughout; see
  [`docs/LOGGING.md`](docs/LOGGING.md) for the event catalog and
  redaction guarantees.

## Claude Code as a gateway client

routectl implements Anthropic's [published gateway
pattern](https://code.claude.com/docs/en/llm-gateway):
`forward_client_headers` for attribution headers, a `count_tokens`
proxy, per-dialect error envelopes, and forward-compat for unknown
SSE block types. The optional `[mitm]` front-proxy lets Claude Code
route inference through routectl while Remote Control keeps working
against `api.anthropic.com` -- including a pure-proxy mode where the
client's own credential authenticates inference. See
[`docs/REMOTE-CONTROL.md`](docs/REMOTE-CONTROL.md) and the
"claude-code as a gateway client" section of
[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).

## Testing

```bash
# Unit + integration tests, no network.
cargo test --workspace --release

# Live integration matrix (opt-in; skips per-provider when its env key is absent).
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

See [`docs/TESTED_MODELS.md`](docs/TESTED_MODELS.md) for the verified
model matrix and [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for the
contributor workflow.

## Out of scope

- Multi-user auth, SSO, TLS termination, multi-tenancy. Use a real
  proxy in front, or fork.
- A config-editing web UI. The read-only dashboard is deliberate; all
  mutation stays config.toml + CLI.
- A caching layer.
- In-proxy cost/latency-based routing decisions -- routectl exposes
  the signals; a client-side harness makes the call.

If you need those, reach for
[LiteLLM](https://github.com/BerriAI/litellm) or a dedicated gateway.

## Responsible use

routectl is a translation pipe. It forwards whatever credentials you
supply and does not vouch for whether a particular credential is
permitted to be used a particular way. **routectl does not support or
condone gateway usage beyond what the upstream provider permits.**
Read the upstream provider's terms before pointing routectl at
production traffic.

- Anthropic publishes a gateway pattern at
  <https://code.claude.com/docs/en/llm-gateway> for first-party
  deployments; routectl's claude-code support implements that
  pattern.
- Per the Anthropic Agent SDK overview, claude.ai OAuth tokens may
  NOT be embedded in third-party products. The `oauth://anthropic`
  ref is for personal-use proxying with the operator's own
  subscription token; do not deploy a routectl instance that resolves
  your ref under other users' requests.
- Read Anthropic's [Acceptable Use
  Policy](https://www.anthropic.com/legal/aup) and [Usage
  Policy](https://www.anthropic.com/legal/usage-policy) before
  production traffic.
- For Codex / ChatGPT credentials, read OpenAI's [Terms of
  Use](https://openai.com/policies/terms-of-use) and [Service
  Terms](https://openai.com/policies/service-terms).
- For Bedrock, the AWS service terms and the underlying
  foundation-model vendor's terms both apply.

## Contributing

Issues and PRs welcome. [`CLAUDE.md`](CLAUDE.md) is the contributor
entry point with a routing index; deep references live under
[`docs/`](docs/): architecture, per-file codemap, configuration,
logging, wire gotchas, development workflow, provider quirks, tested
models. [`ROADMAP.md`](ROADMAP.md) tracks shipped and planned work;
[`SECURITY.md`](SECURITY.md) covers the security posture and
vulnerability reporting.

Conventions: ASCII-only in source and commits; functions under 50
lines, files under 800; one file per dialect, one row per quirk in
the model-profile table.

## License

MIT. See [`LICENSE`](LICENSE).
