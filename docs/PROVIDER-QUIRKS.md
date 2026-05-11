# Provider Configuration Guide

Per-model config tips for routectl operators. Most upstream LLMs work out of
the box with default routectl config -- the entries below cover the cases
where you need to flip a knob.

If your model isn't listed and works fine, you don't need anything from this
doc. If it 4xxs or behaves weirdly, find the matching row.

## Quick reference

| If you're using... | Set on `[providers.X]` | Set on `[aliases.X.retry]` |
|---|---|---|
| **Claude Opus 4.7+ / Sonnet 4.7+** | `adaptive_thinking = true` | `stream_first_byte_timeout_ms = 300000` (when using `effort = "high"` / `"xhigh"` / `"max"`) |
| **DeepSeek v4 / v4.1 (any host)** | `history_reasoning = "preserve"` | -- |
| **DeepSeek v3 / older vLLM** | -- (default `history_reasoning = "auto"` strips for you) | -- |
| **NVIDIA NIM hosting DeepSeek** | `default_extras = { reasoning_effort = "high" }` (only if you want thinking on by default) | -- |
| **Any reasoning / thinking model with high effort** | -- | `stream_first_byte_timeout_ms = 300000` |
| **Anthropic + 1M-context beta** | `extra_headers = { "anthropic-beta" = "context-1m-2025-08-07" }` | -- |
| **OAuth bearer to Anthropic** | `auth_kind = "oauth-bearer"` + the matching beta header | -- |

## Per-model config

### Anthropic Opus 4.7+ (and any future adaptive-thinking models)

The 4.7+ generation rejects the legacy `thinking: {type: "enabled", budget_tokens: N}` shape with a `400 thinking.type.enabled is not supported`. Anthropic moved budget control out of the `thinking` block; the model picks budget from `output_config.effort`.

**Required:**

```toml
[providers.bedrock-opus47]
type = "bedrock"
model_id = "global.anthropic.claude-opus-4-7-v1:0"
adaptive_thinking = true        # rewrites to {type:"adaptive"} + output_config.effort
```

**Recommended when using `reasoning.effort = "high"` / `"xhigh"` / `"max"`:** Opus 4.7 + max-effort regularly takes 60-90 seconds before first SSE byte. Bump the per-alias timeout or it fires spuriously and your client sees a failed request:

```toml
[aliases.heavy.retry]
stream_first_byte_timeout_ms = 300000   # 5 min
request_timeout_ms = 600000             # 10 min
```

**Why opt-in per provider, not auto-detect by name:** Anthropic is rolling adaptive thinking out gradually with no clean naming pattern. `opus-4-7` matches today's model but misses `opus-5` / `sonnet-4-7`; `opus-4-` catches the still-legacy `4-5`/`4-6`. TOML opt-in lets you flip the day a new model lands.

### DeepSeek v4 / v4.1 (api.deepseek.com, opencode-go, NIM, vLLM-hosted, anywhere)

DeepSeek v4 inverted v3's contract on multi-turn echo-back. v3: 400 if you echo `reasoning_content` in assistant history. v4: `400 reasoning_content in the thinking mode must be passed back to the API` if you don't.

**Required:**

```toml
[providers.opencode-go]
type = "openai-compat"
base_url = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"
reasoning_dialect = "deepseek"          # so response reasoning lifts correctly
history_reasoning = "preserve"          # echoes reasoning_content back to upstream
```

**Default behavior (without the knob):** routectl strips reasoning fields from outgoing assistant history (correct for DeepSeek v3). When you upgrade to v4, the strip is wrong and the upstream 400s. Routectl warns at the strip site so you see the loss in logs:

```
WARN openai-compat egress: assistant reasoning_content stripped from outgoing history.
     Set history_reasoning = "preserve" on the provider if your upstream requires
     echo-back (DeepSeek v4+, recent vLLM).
```

### vLLM (recent versions)

vLLM 0.7+ matches DeepSeek v4's echo-back contract. Same fix:

```toml
[providers.my-vllm]
type = "openai-compat"
base_url = "http://localhost:8000/v1"
api_key_ref = "literal:not-needed"
reasoning_dialect = "vllm"
history_reasoning = "preserve"          # for vLLM 0.7+
```

For older vLLM (≤ 0.6) leave `history_reasoning` unset (defaults to strip).

### NVIDIA NIM (integrate.api.nvidia.com)

NIM's DeepSeek v4 Flash / Pro defaults to **non-thinking mode**. Thinking is enabled per request via top-level `reasoning_effort: "none" | "high" | "max"`. routectl's OpenAI dialect already maps canonical `reasoning.effort` -> `reasoning_effort`, but most clients (e.g. opencode) don't send `reasoning.effort` so you land on NIM's default `none`.

**To enable thinking on every NIM request:**

```toml
[providers.nim]
type = "openai-compat"
base_url = "https://integrate.api.nvidia.com/v1"
api_key_ref = "env://NIM_API_KEY"
reasoning_dialect = "openai"
default_extras = { reasoning_effort = "high" }   # always-on thinking
```

**Heads-up: NIM cold-start latency on streaming.** First-byte timeouts of 60s+ are common on NIM. Bump per-alias:

```toml
[aliases.nim-heavy.retry]
stream_first_byte_timeout_ms = 180000   # 3 min
```

### Anthropic (api.anthropic.com)

Default config works for Claude 4.5/4.6 / Haiku 4.5. The two times you need extras:

**1M-context beta** (Sonnet 4.5 with 1M context window):

```toml
[providers.anthropic]
type = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
extra_headers = { "anthropic-beta" = "context-1m-2025-08-07" }
```

**OAuth bearer auth** (Claude Code subscription tokens, `sk-ant-oat01-...`):

```toml
[providers.anthropic-oauth]
type = "anthropic-api"
api_key_ref = "file:///path/to/oauth-token"
auth_kind = "oauth-bearer"
extra_headers = { "anthropic-beta" = "oauth-2025-04-20,context-1m-2025-08-07" }
```

routectl no longer auto-injects beta gates -- declare the ones you need.

### Bedrock (any region)

Default works for `invoke` shape (Anthropic Messages body) on Sonnet/Haiku/Opus. Same notes apply for Opus 4.7+ -- set `adaptive_thinking = true` on the provider.

The **`converse` api_shape** is accepted in TOML but the body translator is stubbed (M2.7). Use `api_shape = "invoke"` until the adapter ships.

**Example:**

```toml
[providers.bedrock-opus47]
type = "bedrock"
region = "us-east-1"
model_id = "global.anthropic.claude-opus-4-7-v1:0"
api_shape = "invoke"
adaptive_thinking = true
creds = { kind = "default-chain" }
```

### OpenRouter

Default works. Two niceties to set:

```toml
[providers.openrouter]
type = "openai-compat"
base_url = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"
reasoning_dialect = "openrouter"
extra_headers = {
  "HTTP-Referer" = "https://github.com/your/project",
  "X-Title" = "your-project-name"
}
```

The HTTP-Referer / X-Title headers improve OpenRouter's analytics and can affect rate limits on free tiers.

### OpenAI / o-series / GPT-5

Default works. The OpenAI dialect maps canonical `reasoning.effort` -> wire `reasoning_effort` and drops sampling params on reasoning models automatically (driven by `model_profile.rs`).

```toml
[providers.openai]
type = "openai-compat"
base_url = "https://api.openai.com/v1"
api_key_ref = "env://OPENAI_API_KEY"
reasoning_dialect = "openai"
```

## Cross-cutting timing notes

### `stream_first_byte_timeout_ms`

Default is 10s (set in `[retry]`). Fine for most non-thinking models, too aggressive for any thinking model with high effort. Bump per-alias when needed.

| Model class | Suggested timeout |
|---|---|
| Non-thinking, normal endpoints | 10000 (default) |
| Thinking-capable, low effort | 30000-60000 |
| Thinking-capable, high effort | 180000-300000 |
| Thinking-capable, max effort | 300000-600000 |
| NIM cold-start (any model) | 180000+ |

These belong on the alias's `[aliases.X.retry]` block, not on `[retry]` -- the global default should stay tight to surface real timeouts on routine calls.

**Open design question (not yet implemented):** should routectl support per-provider or per-model timeout config? Today timeouts are alias-level only. Per-provider would reduce repetition when many aliases share an upstream. Per-model would be overkill -- model_id is just a string in `provider:model` literals. **Provider-level is the most likely future addition.** Until then, set per-alias.

### `request_timeout_ms`

Default is 60s. Long-thinking responses can run 5-10 min on max effort. Bump alongside the first-byte timeout:

```toml
[aliases.heavy.retry]
stream_first_byte_timeout_ms = 300000   # 5 min until first byte
request_timeout_ms = 600000             # 10 min full request
```

## Multi-host fallback chains

When you want a model with multiple hosts as fallback, put them in chain order:

```toml
[aliases.deepseek-flash]
chain = [
  "opencode-go:deepseek-v4-flash",                     # cheapest, primary
  "openrouter:deepseek/deepseek-v4-flash",             # if opencode-go down
  "nim:deepseek-ai/deepseek-v4-flash",                 # third-party fallback
]
```

Each provider's `history_reasoning` config applies independently -- the chain just picks who answers. The `routectl_provider` field on every response tells you which one actually answered.

## Troubleshooting matrix

When a request fails, the upstream's error body is the truth source. routectl logs a 200B-truncated `body_excerpt` at WARN on every 4xx/5xx; flip `ROUTECTL_LOG=routectl=debug` for the full body.

| Symptom | Likely cause |
|---|---|
| `400 thinking.type.enabled is not supported` | Need `adaptive_thinking = true` on provider (Opus 4.7+) |
| `400 reasoning_content in the thinking mode must be passed back to the API` | Need `history_reasoning = "preserve"` on provider (DeepSeek v4) |
| `400 unknown variant 'auto', expected 'function'` | Old routectl version -- upgrade to one with the `tool_choice` egress translator |
| `stream first-byte timeout after 10000ms` on a thinking model | Bump `stream_first_byte_timeout_ms` per the table above |
| Empty `content` + non-zero `reasoning_tokens` | Model used full `max_tokens` budget on reasoning. Increase `max_tokens` |
| `400 thinking enabled requires temperature 1.0` | Don't set `temperature` when reasoning is enabled (routectl auto-forces 1.0 if you do) |
| `prompt_tokens` smaller than expected | You're on a routectl version older than v0.5.x -- upgrade so the cache_creation/cache_read sum lands |

## When in doubt

`./routectl config check --config <path>` validates the schema before serve.
`./routectl config show` prints the resolved config (for inspection).

Combine with `ROUTECTL_LOG=routectl=debug` and a `grep request_id=<id>` workflow when triaging a specific failure -- see `CLAUDE.md` "Triage recipes" for full examples.
