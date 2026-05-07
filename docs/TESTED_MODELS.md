# Tested Models

This is the running matrix of models routectl has been verified against,
end-to-end through `routectl serve` and the `Router` core. The list lives
alongside the `crates/routectl-cli/tests/live_matrix.rs` integration test
so the README and the test stay in sync.

## How to run the matrix

```bash
# Set keys once (they're optional -- tests skip cleanly when missing).
export OPENROUTER_API_KEY=sk-or-v1-...
export OPENCODE_GO_API_KEY=sk-...
export NIM_API_KEY=nvapi-...

cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

`--test-threads=1` keeps the per-provider report blocks together so you
can read them in order. Each test prints a PASS/FAIL row per model with
latency, content preview, and reasoning-shape signals (`rd=N` is the
number of `reasoning_details[]` entries; `fmt=...` is the format tag).

The suite is feature-gated behind `live-integration`, so a regular
`cargo test --workspace` does NOT hit the network.

## Coverage philosophy

- One representative model per major provider org -- not an exhaustive
  sweep of all 367+ OpenRouter models.
- Both completion and streaming for at least one model per provider.
- Reasoning-capable models included on every provider so the
  `reasoning_details[]` lift is exercised.
- Tests are non-strict on individual model failures (rate-limited free
  tiers, deprecated models, transient outages), but assert
  `pass > 0` per provider as a sanity gate.

## OpenRouter (`reasoning_dialect = "openrouter"`)

Hits `https://openrouter.ai/api/v1`. Adds the `HTTP-Referer` and `X-Title`
headers OpenRouter expects.

| Model | Mode | Status | Notes |
|---|---|---|---|
| `openai/gpt-4o-mini` | complete + stream | PASS | Reference baseline |
| `openai/gpt-oss-120b:free` | complete | PASS | Reasoning lifted |
| `anthropic/claude-haiku-4-5` | complete + stream | PASS | |
| `google/gemma-3n-e4b-it` | complete + stream | flaky | Occasional 503 from upstream |
| `meta-llama/llama-3.2-3b-instruct:free` | complete | upstream-429 | Free tier rate-limit |
| `mistralai/mistral-nemo` | complete | PASS | |
| `deepseek/deepseek-v4-flash` | complete | PASS | |
| `deepseek/deepseek-r1` | complete + stream | PASS | Reasoning lifted, ~17+ reasoning chunks streamed |
| `qwen/qwen3-coder:free` | complete | upstream-429 | Free tier rate-limit |
| `x-ai/grok-3-mini` | complete | PASS | `format = xai-responses-v1` |
| `nvidia/nemotron-nano-9b-v2:free` | complete | PASS | Returns `content: null`; lifted via `MessageContent::Null` |
| `microsoft/phi-4` | complete | PASS | |
| `cohere/command-r7b-12-2024` | complete | PASS | |
| `minimax/minimax-m2.5:free` | complete | PASS | Slow (~7s) but completes |
| `moonshotai/kimi-k2-0905` | complete | PASS | |
| `z-ai/glm-4.5-air:free` | complete | PASS | |
| `amazon/nova-micro-v1` | complete | PASS | |
| `perplexity/sonar` | complete | PASS | |
| `arcee-ai/trinity-mini` | complete | PASS | Returns `content: null` |
| `nousresearch/hermes-3-llama-3.1-405b:free` | complete | upstream-429 | Free tier rate-limit |

## OpenCode-Go (`reasoning_dialect = "deepseek"`)

Hits `https://opencode.ai/zen/go/v1`. The Zen Go subscription tier --
distinct from the pay-as-you-go Zen API at `/zen/v1`.

All 14 models on the subscription have been verified end-to-end:

| Model | Mode | Status |
|---|---|---|
| `minimax-m2.7` | complete | PASS |
| `minimax-m2.5` | complete | PASS |
| `kimi-k2.6` | complete + stream | PASS (~59 reasoning chunks streamed) |
| `kimi-k2.5` | complete | PASS |
| `glm-5.1` | complete + stream | PASS |
| `glm-5` | complete | PASS |
| `deepseek-v4-pro` | complete | PASS |
| `deepseek-v4-flash` | complete + stream | PASS (~43 reasoning chunks streamed) |
| `qwen3.6-plus` | complete + stream | PASS |
| `qwen3.5-plus` | complete | PASS (occasional Alibaba 429 upstream) |
| `mimo-v2-pro` | complete | PASS |
| `mimo-v2-omni` | complete | PASS |
| `mimo-v2.5-pro` | complete | PASS |
| `mimo-v2.5` | complete | PASS |

OpenCode-Go-specific behaviors routectl handles:

- Emits a `data: {"choices":[],"cost":"0"}` cost-trailer chunk **after**
  the `[DONE]` SSE terminator. Routectl correctly stops parsing at
  `[DONE]` so the trailer doesn't fail chunk deserialization.
- DeepSeek-style `reasoning_content` field on responses; lifted into
  `reasoning_details[format="deepseek-v1"]` by the deepseek dialect.

## NIM -- NVIDIA Inference Microservices (`reasoning_dialect = "openai"`)

Hits `https://integrate.api.nvidia.com/v1`.

| Model | Mode | Status | Notes |
|---|---|---|---|
| `meta/llama-3.1-8b-instruct` | complete | PASS | |
| `meta/llama-3.3-70b-instruct` | complete | PASS | Returns both `reasoning` and `reasoning_content` keys (often null); coalesced before deserialize |
| `meta/llama-4-maverick-17b-128e-instruct` | complete | PASS | |
| `google/gemma-3-12b-it` | complete | PASS | Returns no `created` field; tolerated via `#[serde(default)]` |
| `qwen/qwen3-coder-480b-a35b-instruct` | complete | PASS | |

NIM has 137+ models in its catalog. The matrix picks 5 across providers
and architectures. Lots of NIM models are guarded behind feature
endpoints that 410 ("end of life") -- the matrix avoids those by using
freshly-validated names.

## Anthropic API (untested live)

`reasoning_dialect = "anthropic-api"`, hits `https://api.anthropic.com`
(with `x-api-key` and `anthropic-version: 2023-06-01` headers). Wire
format -- thinking blocks, signature preservation, system-message lift,
tools shape -- is covered by 20 unit tests. No live key has been wired
into the matrix yet.

## Cookie-auth providers (deferred to v0.2)

`claude-cookie` and `chatgpt-cookie` providers are scaffolded but
feature-gated. The CLI returns a clean "not enabled in this build (v0.2
feature)" error when configured. v0.2 will enable a `wry`-based webview
login flow.

## Adding a new model

If you find a model not in the matrix that you want covered:

1. Add the `provider:model` target to the appropriate `*_MODELS` constant
   in `crates/routectl-cli/tests/live_matrix.rs`.
2. Run the matrix locally with the relevant key set.
3. Update this doc with the result.

If the model fails with a routectl error (not an upstream 4xx/5xx), it's
a normalization gap -- the matrix log will show the exact serde failure,
which is usually a 5-10 line schema fix in
`crates/routectl-core/src/schema.rs`.
