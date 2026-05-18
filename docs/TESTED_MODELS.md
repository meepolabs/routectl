# Tested Models

This is the running matrix of models routectl has been verified against,
end-to-end through `routectl serve` and the `Router` core. The list lives
alongside the `crates/routectl-cli/tests/live_matrix.rs` integration test
so the README and the test stay in sync.

## How to run the matrix

```bash
# Set keys for the providers you want to exercise. Tests skip cleanly
# when a key is absent.
export OPENROUTER_API_KEY=sk-or-v1-...
export NIM_API_KEY=nvapi-...
export ANTHROPIC_API_KEY=sk-ant-api03-...
export AWS_BEARER_TOKEN_BEDROCK=...
export AWS_REGION=us-east-1

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

Notable openai-compat-host behaviors routectl handles, surfaced
during testing across multiple OpenAI-compatible upstreams:

- **Trailing chunks after `[DONE]`**: some hosts emit a final
  `data: {"choices":[],"cost":"0"}` (or similar bookkeeping chunk)
  *after* the SSE `[DONE]` terminator. Routectl stops parsing at
  `[DONE]` so the trailer doesn't fail chunk deserialization.
- **DeepSeek-style `reasoning_content`**: the `deepseek` dialect lifts
  this into `reasoning_details[format="deepseek-v1"]`.

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

## Anthropic API direct (`kind = "anthropic-api"`)

Hits `https://api.anthropic.com` with `x-api-key` (or
`Authorization: Bearer` for OAuth-bearer auth) and
`anthropic-version: 2023-06-01`. Wire-format coverage -- thinking
blocks, signature preservation, system-message lift, tools shape,
cache_control round-trip -- has 20+ unit tests. Live verification
runs when `ANTHROPIC_API_KEY` is set.

## AWS Bedrock (`kind = "bedrock"`, InvokeModel + Anthropic body)

Hits `bedrock-runtime.<region>.amazonaws.com` directly with SigV4
signing or a short-term bearer key
(`AWS_BEARER_TOKEN_BEDROCK`). Live matrix entries cover the
cross-region inference profiles for current Anthropic models on
Bedrock; per-account model-availability varies. Skipped unless
`AWS_BEARER_TOKEN_BEDROCK` (or the standard AWS credential chain) is
available.

| Model | Mode | Notes |
|---|---|---|
| `us.anthropic.claude-3-5-haiku-20241022-v1:0` | complete + stream | |
| `us.anthropic.claude-haiku-4-5-20251001-v1:0` | complete + stream | |
| `us.anthropic.claude-sonnet-4-20250514-v1:0` | complete | |
| `us.anthropic.claude-sonnet-4-5-20250929-v1:0` | complete | Used as the cache_control verification target (1024-token cache minimum) |
| `us.anthropic.claude-opus-4-20250514-v1:0` | complete | |

Three additional ingress-through-Bedrock tests verify the v0.4
hub-and-spoke seam end-to-end:

- `openai_ingress_through_bedrock`: OpenAI Chat Completions wire body
  -> canonical -> Bedrock InvokeModel -> response.
- `anthropic_ingress_through_bedrock_cache_and_beta`: Anthropic
  Messages body with `cache_control` on a system block and
  `anthropic_beta` flags. Sends the same body twice; the second call
  hits the cache. Verified live: cache_create=N on call 1,
  cache_read=N on call 2.
- `anthropic_ingress_streaming_through_bedrock`: streaming through
  the Anthropic ingress, decoding Bedrock's binary eventstream
  frames, asserts the rendered SSE event sequence
  (`message_start` -> `content_block_*` -> `message_stop`).

## OpenAI Responses API (`kind = "openai-responses"`, chatgpt-oauth surface)

Hits `https://chatgpt.com/backend-api/codex/responses` with a ChatGPT
subscription bearer JWT (`Authorization: Bearer <jwt>`) and
`ChatGPT-Account-Id` header. This is the wire surface used by the
OpenAI Codex CLI.

Required env vars for the live matrix:
```bash
export OPENAI_BEARER_KEY="$(jq -r '.openai.access' <your-codex-CLI-auth-store>)"
export OPENAI_ACCOUNT_ID="$(jq -r '.openai.accountId' <your-codex-CLI-auth-store>)"
```

Run the openai-responses matrix:
```bash
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix openai_responses -- --nocapture --test-threads=1
```

Wire-shape notes validated in the relevant stage smoke (2026-05-12):

- **Stream-only**: the endpoint rejects `stream:false` with HTTP 400
  `{"detail":"Stream must be set to true"}`. `complete()` forces
  `stream:true` internally and collects to `response.completed`.
- **Flat tool definitions**: `{type,name,description,parameters,strict}`
  at the top level (NOT nested `{type,function:{...}}`). The nested form
  400s with `"Missing required parameter: 'tools[0].name'"`.
- **Flat tool_choice**: named-function form is `{"type":"function","name":"X"}`
  (NOT `{"type":"function","function":{"name":"X"}}`). The nested form
  400s with `"Unknown parameter: 'tool_choice.function'"`.
- **instructions always required**: the `instructions` field must be
  present in the body (even as `""`). Absent field -> 400
  `{"detail":"Instructions are required"}`.
- **originator header**: `originator: codex_cli_rs` required; present by
  default, no operator action needed.
- **store flag**: `store:false` sent by default on the chatgpt-oauth
  surface (codex parity).
- **prompt_cache_key**: forwarded from `provider_extras["prompt_cache_key"]`
  if present; omitted otherwise. The endpoint auto-assigns a cache key
  when the field is absent.
- **encrypted_content (reasoning replay)**: sent on prior-turn reasoning
  items when `encrypted_content` is non-empty. Empty string is accepted
  and treated as no-op (codex `arc_monitor.rs:325-336`).

| Model | Mode | Status | Notes |
|---|---|---|---|
| `gpt-5.3-codex` | complete + stream | PASS | Default codex CLI model; smoke 2026-05-12 |
| `gpt-5.4` | complete + stream | PASS | General-purpose flagship |
| `gpt-5.4-mini` | complete + stream | PASS | Faster/cheaper variant |

Verified: 4 wire-shape bugs found and fixed in the relevant stage before the matrix ran.
See `CLAUDE.md` "Common gotchas" for the full fix record.

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
