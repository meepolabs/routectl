# Provider Configuration Guide

Per-model config tips for routectl operators. Most upstream LLMs work out of
the box with default routectl config -- the entries below cover the cases
where you need to flip a knob.

If your model isn't listed and works fine, you don't need anything from this
doc. If it 4xxs or behaves weirdly, find the matching row.

## Quick reference

| If you're using... | Set on... |
|---|---|
| **Claude Opus 4.7+ / Sonnet 4.7+** | `[models.X] adaptive_thinking = true` |
| **Any thinking model + high-effort latency** | `[providers.X] stream_first_byte_timeout_ms = 300000` (every alias hitting this provider inherits) |
| **DeepSeek v4 / v4.1 (any host)** | `[providers.X] history_reasoning = "preserve"` |
| **DeepSeek v3 / older vLLM** | (default `history_reasoning = "auto"` strips for you) |
| **NVIDIA NIM hosting DeepSeek** | callers must send `reasoning_effort = "high"` per request (the operator-side `default_extras` knob is deferred -- callers can still set effort via wire `reasoning.effort`) |
| **NIM cold-start streaming** | `[providers.X] stream_first_byte_timeout_ms = 180000` (3 min) |
| **Anthropic + 1M-context beta** | `[providers.X] header_extras = { "anthropic-beta" = "context-1m-2025-08-07" }` |
| **OAuth bearer to Anthropic** | `[providers.X] auth_kind = "oauth-bearer"` + the matching beta header |
| **DeepSeek /anthropic endpoint + claude-code context-management** | `[providers.X] context_management = true` |

## Per-model config

### Anthropic Opus 4.7+ (and any future adaptive-thinking models)

The 4.7+ generation rejects the legacy `thinking: {type: "enabled", budget_tokens: N}` shape with a `400 thinking.type.enabled is not supported`. Anthropic moved budget control out of the `thinking` block; the model picks budget from `output_config.effort`.

**Required:**

```toml
[providers.bedrock]
kind   = "bedrock"
region = "us-east-1"
creds  = { kind = "default-chain" }

[models.opus47]
provider          = "bedrock"
upstream          = "us.anthropic.claude-opus-4-7-v1:0"
adaptive_thinking = true        # rewrites to {type:"adaptive"} + output_config.effort
thinking          = "high"
```

**Recommended when using `reasoning.effort = "high"` / `"xhigh"` / `"max"`:** Opus 4.7 + max-effort regularly takes 60-90 seconds before first SSE byte. Bump the timeouts on the parent provider so every model routing through it inherits:

```toml
[providers.bedrock]
# ...the fields above, plus:
stream_first_byte_timeout_ms = 300000   # 5 min, applies to every model
request_timeout_ms           = 600000   # 10 min, applies to every model
```

Need different timeouts for different models on the same upstream? Split into separate `[providers.X]` entries (e.g. `bedrock-fast`, `bedrock-heavy`) with their own runtime knobs and route each `[models.X]` at the right one.

Resolution priority: `[providers.X].X` > `[retry].X` > unset (no cap).

**Why opt-in per model, not auto-detect by name:** Anthropic is rolling adaptive thinking out gradually with no clean naming pattern. `opus-4-7` matches today's model but misses `opus-5` / `sonnet-4-7`; `opus-4-` catches the still-legacy `4-5`/`4-6`. TOML opt-in lets you flip the day a new model lands.

### DeepSeek v4 / v4.1 (api.deepseek.com, example-deepseek-host, NIM, vLLM-hosted, anywhere)

DeepSeek v4 inverted v3's contract on multi-turn echo-back. v3: 400 if you echo `reasoning_content` in assistant history. v4: `400 reasoning_content in the thinking mode must be passed back to the API` if you don't.

**Required:**

```toml
[providers.example-deepseek-host]
kind = "openai-compat"
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
kind = "openai-compat"
base_url = "http://localhost:8000/v1"
api_key_ref = "literal:not-needed"
reasoning_dialect = "vllm"
history_reasoning = "preserve"          # for vLLM 0.7+
```

For older vLLM (≤ 0.6) leave `history_reasoning` unset (defaults to strip).

### NVIDIA NIM (integrate.api.nvidia.com)

NIM's DeepSeek v4 Flash / Pro defaults to **non-thinking mode**. Thinking is enabled per request via top-level `reasoning_effort: "none" | "high" | "max"`. routectl's OpenAI dialect already maps canonical `reasoning.effort` -> `reasoning_effort`; clients that send `reasoning.effort` land thinking on the request.

**To enable thinking on every NIM request:** the operator-side
`default_extras` knob (which would unconditionally inject
`reasoning_effort` into the body) is deferred to a future release.
Until then, callers must send `reasoning.effort = "high"` (or set
the equivalent on the client side); the OpenAI-dialect translator
forwards it as `reasoning_effort` to NIM.

```toml
[providers.nim]
kind              = "openai-compat"
base_url          = "https://integrate.api.nvidia.com/v1"
api_key_ref       = "env://NIM_API_KEY"
reasoning_dialect = "openai"
```

**Heads-up: NIM cold-start latency on streaming.** First-byte timeouts of 60s+ are common on NIM. Set the bump on the provider so every model hitting it inherits:

```toml
[providers.nim]
# ...the fields above, plus:
stream_first_byte_timeout_ms = 180000   # 3 min, all NIM-routed models benefit
```

Need different timeouts on different routes? Split into separate `[providers.X]` entries.

### Anthropic (api.anthropic.com)

Default config works for Claude 4.5/4.6 / Haiku 4.5. The two times you need extras:

**1M-context beta** (Sonnet 4.5 with 1M context window):

```toml
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
header_extras = { "anthropic-beta" = "context-1m-2025-08-07" }
```

**OAuth bearer auth** (Claude Code subscription tokens, `sk-ant-oat01-...`):

```toml
[providers.anthropic-oauth]
kind = "anthropic-api"
api_key_ref = "file:///path/to/oauth-token"
auth_kind = "oauth-bearer"
header_extras = { "anthropic-beta" = "oauth-2025-04-20,context-1m-2025-08-07" }
```

routectl does not auto-inject beta gates -- declare the ones you need.

### claude-code attribution headers (`X-Claude-Code-*`)

Anthropic documents three gateway-mandatory passthrough headers at
<https://code.claude.com/docs/en/llm-gateway> --
`X-Claude-Code-Session-Id`, `X-Claude-Code-Agent-Id`, and
`X-Claude-Code-Parent-Agent-Id` -- so cost and trace attribution work
when claude-code traffic is fronted by a proxy. The Anthropic ingress
greedy-captures the entire `x-claude-code-*` namespace into the
canonical request; the Anthropic-API egress forwards a name upstream
only when it appears on the per-provider `forward_client_headers`
allowlist:

```toml
[providers.anthropic-managed]
kind        = "anthropic-api"
api_key_ref = "oauth://anthropic"
forward_client_headers = [
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
]
```

Default is empty (drop everything captured) -- safe-by-default for
new providers. Future Anthropic-namespace additions are NOT
auto-forwarded; the operator opts each name in. Synthesizing
client-supplied identifiers in routectl is intentionally out of
scope: synthesizing identifiers on a personal-use claude.ai proxy is
TOS-adjacent, and the operator can already inject any static
identifier via `header_extras` if they want a static value.

### Bedrock (any region)

Both `api_shape = "invoke"` (Anthropic Messages body) and
`api_shape = "converse"` (AWS Converse) are wired for Anthropic
models on Sonnet/Haiku/Opus. Set `adaptive_thinking = true` on Opus
4.7+ models regardless of api_shape.

**Bedrock allowlist (optional, recommended in production).** AWS
strict-schema validation 400s any unrecognized `anthropic_beta` flag
or top-level body field. routectl ships no built-in default; populate
the operator-supplied lists in TOML to gate which entries reach AWS:

```toml
[bedrock]
allowed_betas       = ["context-1m-2025-08-07", ...]
allowed_body_fields = ["anthropic_version", "messages", ...]
```

Empty lists (or omitted `[bedrock]` section) = pass-through (no
filter applied). Use `ROUTECTL_LOG=routectl_providers::bedrock=trace`
to capture sent flags/fields when building the lists. See
`examples/bedrock.toml` for the empirical 2026-05-12 baseline.

**RPM bucket semantics for shared Bedrock providers.** Runtime state
(circuit breaker + RPM token bucket) is keyed by `[models.X]`
nickname, not by `[providers.X]` name. This is the right semantic for
breakers (a flaky model on one Bedrock provider should not trip the
breaker for siblings), but it means `rpm_limit` is per-model:
`rpm_limit = 60` on a provider serving 3 `[models.X]` rows admits up
to 180 RPM in aggregate to the underlying Bedrock account. Operators
with tight Bedrock service quotas should size `rpm_limit` per the
per-model budget, or split sensitive models across multiple
`[providers.X]` blocks (e.g. distinct `[providers.bedrock-sonnet]` /
`[providers.bedrock-haiku]` entries) to keep buckets isolated.

**Example provider + model:**

```toml
[providers.bedrock]
kind      = "bedrock"
region    = "us-east-1"
api_shape = "invoke"
creds     = { kind = "default-chain" }

[models.opus47]
provider          = "bedrock"
upstream          = "us.anthropic.claude-opus-4-7-v1:0"
adaptive_thinking = true
thinking          = "high"
```

### OpenRouter

Default works. Two niceties to set:

```toml
[providers.openrouter]
kind = "openai-compat"
base_url = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"
reasoning_dialect = "openrouter"
header_extras = {
  "HTTP-Referer" = "https://github.com/your/project",
  "X-Title" = "your-project-name"
}
```

The HTTP-Referer / X-Title headers improve OpenRouter's analytics and can affect rate limits on free tiers.

### OpenAI / o-series / GPT-5

Default works. The OpenAI dialect maps canonical `reasoning.effort` -> wire `reasoning_effort` and drops sampling params on reasoning models automatically (driven by `model_profile.rs`).

```toml
[providers.openai]
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
api_key_ref = "env://OPENAI_API_KEY"
reasoning_dialect = "openai"
```

## Cross-cutting timing notes

### `stream_first_byte_timeout_ms`

Default is 10s (set in `[retry]`). Fine for most non-thinking models, too aggressive for any thinking model with high effort or any cold-start-prone host.

| Model class | Suggested timeout |
|---|---|
| Non-thinking, normal endpoints | 10000 (default) |
| Thinking-capable, low effort | 30000-60000 |
| Thinking-capable, high effort | 180000-300000 |
| Thinking-capable, max effort | 300000-600000 |
| NIM cold-start (any model) | 180000+ |

**Resolution priority** (provider > global):

1. `[providers.Y] stream_first_byte_timeout_ms` -- per-provider default (use when an upstream is uniformly slow; every model routing through it inherits)
2. `[retry] stream_first_byte_timeout_ms` -- workspace default (keep tight to surface real timeouts on routine calls)

Per-route timeouts: split into separate `[providers.X]` entries with their own runtime knobs and route each `[models.X]` accordingly.

### `request_timeout_ms`

Default is unset (no cap; relies on reqwest's default). Same provider > global resolution as the first-byte timeout. Bump alongside the first-byte timeout for long-thinking responses:

```toml
[providers.bedrock]
stream_first_byte_timeout_ms = 300000   # 5 min until first byte
request_timeout_ms           = 600000   # 10 min full request
```

## Multi-host fallback chains

When you want a model with multiple hosts as fallback, declare each host as
its own `[models.X]` row and chain the nicknames in `[aliases]`:

```toml
[providers.example-deepseek-host]
kind        = "openai-compat"
base_url    = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"

[providers.openrouter]
kind        = "openai-compat"
base_url    = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"

[providers.nim]
kind        = "openai-compat"
base_url    = "https://integrate.api.nvidia.com/v1"
api_key_ref = "env://NIM_API_KEY"

[models.ds-go]
provider = "example-deepseek-host"
upstream = "deepseek-v4-flash"

[models.ds-or]
provider = "openrouter"
upstream = "deepseek/deepseek-v4-flash"

[models.ds-nim]
provider = "nim"
upstream = "deepseek-ai/deepseek-v4-flash"

[aliases]
deepseek-flash = ["ds-go", "ds-or", "ds-nim"]   # primary -> fallback -> fallback
```

Each provider's `history_reasoning` config applies independently -- the chain just picks who answers. The `routectl_provider` field on every response tells you which one actually answered.

## DeepSeek /anthropic and similar: context_management beta emulation

claude-code 1.x sends two artefacts when the context-management beta is active:

1. An `anthropic-beta: context-management-2025-06-27` request header.
2. A `context_management` top-level body key containing an `edits` array with
   a `clear_thinking_20251015` entry that tells the server which thinking blocks
   to re-send with the response.

Real Anthropic handles both natively. Non-Anthropic anthropic-api providers
(DeepSeek `/anthropic`, vLLM, LM Studio, etc.) reject the beta header and/or
the body key with a 400. They still require thinking echo-back for multi-turn
continuity (see the `history_reasoning = "preserve"` row above), but they need
to receive the thinking blocks directly in the message history -- not via the
Anthropic-proprietary edit mechanism.

**What routectl does when `context_management = true`:**

- Strips `context-management-2025-06-27` from the outgoing `anthropic-beta`
  header so the upstream never sees the beta it doesn't honour.
- Strips the `context_management` top-level body key so the upstream does not
  400 on an unknown field.
- Re-injects the cached thinking blocks before each qualifying ToolUse block in
  the outgoing assistant messages, emulating what the real Anthropic server would
  have done. The injection follows the `keep` policy from the
  `clear_thinking_20251015` edit:
  - `"keep": "all"` -- inject into every assistant turn that has a ToolUse.
  - `"keep": {"type": "thinking_turns", "value": N}` -- inject only the most
    recent N turns (0 means no injection).
  - Unknown shapes default to `"all"` with a debug log.
- **Soft-fail on cache miss**: if the cache has no entry for a tool_use id
  (cold-start or TTL eviction after 60 minutes), routectl strips the `thinking`
  body key and emits a structured `WARN` log so the request completes without a
  400. The operator can see the miss via `grep missed_tool_ids` in the logs.

**Required config:**

```toml
[providers.deepseek-anthropic]
kind               = "anthropic-api"
base_url           = "https://api.deepseek.com"
api_key_ref        = "env://DEEPSEEK_API_KEY"
context_management = true
history_reasoning  = "preserve"
```

Note: `history_reasoning = "preserve"` is still required (see the DeepSeek v4
section above) because thinking echo-back and context-management emulation are
complementary, not alternatives. `history_reasoning` controls how thinking
tokens in the INCOMING ChatRequest history are forwarded; `context_management`
controls how the outgoing request is shaped for the beta-aware edit workflow.

**Troubleshooting:**

| Symptom | Cause |
|---|---|
| `400 context_management is not allowed` | Provider rejects the body key -- set `context_management = true` |
| `400 anthropic-beta header not recognised` | Provider rejects the beta header -- set `context_management = true` |
| WARN `context_management: cache miss for tool_use ids` in logs | Cold-start or TTL gap; thinking was stripped for that turn. The next turn refills the cache and injection resumes. |

## Troubleshooting matrix routectl logs a 200B-truncated `body_excerpt` at WARN on every 4xx/5xx; flip `ROUTECTL_LOG=routectl=debug` for the full body.

| Symptom | Likely cause |
|---|---|
| `400 thinking.type.enabled is not supported` | Need `adaptive_thinking = true` on `[models.X]` (Opus 4.7+) |
| `400 reasoning_content in the thinking mode must be passed back to the API` | Need `history_reasoning = "preserve"` on `[providers.X]` (DeepSeek v4) |
| `stream first-byte timeout after 10000ms` on a thinking model | Bump `stream_first_byte_timeout_ms` per the table above |
| Empty `content` + non-zero `reasoning_tokens` | Model used full `max_tokens` budget on reasoning. Increase `max_tokens` |
| `400 thinking enabled requires temperature 1.0` | Don't set `temperature` when reasoning is enabled (routectl auto-forces 1.0 if you do) |
| `400 context_management is not allowed` or `400 anthropic-beta header not recognised` | Set `context_management = true` on the provider (DeepSeek /anthropic or similar) |

## When in doubt

`./routectl config check --config <path>` validates the schema before serve.
`./routectl config show` prints the resolved config (for inspection).

Combine with `ROUTECTL_LOG=routectl=debug` and a `grep request_id=<id>` workflow when triaging a specific failure -- see [LOGGING.md](LOGGING.md) for the full triage recipes.
