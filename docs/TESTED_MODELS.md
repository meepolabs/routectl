# Tested Models

This is the running matrix of models routectl has been verified against,
end-to-end through `routectl serve` and the `Router` core. The list lives
alongside the `crates/routectl-cli/tests/live_matrix.rs` integration test
so the README and the test stay in sync.

PASS/flaky statuses are point-in-time observations from the most
recent matrix run, not guarantees -- free tiers rate-limit, hosts
deprecate models, upstreams have outages. Re-run the matrix (below)
for current truth.

## How to run the matrix

```bash
# Set keys for the providers you want to exercise. Tests skip cleanly
# when a key is absent.
export OPENROUTER_API_KEY=sk-or-v1-...
export OPENCODE_GO_API_KEY=...
export NIM_API_KEY=nvapi-...
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
| `x-ai/grok-3-mini` | complete | PASS | `format = openrouter-passthrough-v1` |
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

## opencode-go (`reasoning_dialect = "deepseek"` for DeepSeek; "openai" otherwise)

Hits `https://opencode.ai/zen/go/v1` with a Cloudflare front. Skipped
unless `OPENCODE_GO_API_KEY` is set.

| Model | Mode | Notes |
|---|---|---|
| `minimax-m2.7` | complete | |
| `minimax-m2.5` | complete | |
| `kimi-k2.6` | complete | |
| `kimi-k2.5` | complete | |
| `glm-5.1` | complete | |
| `glm-5` | complete | |
| `deepseek-v4-pro` | complete | DeepSeek dialect; reasoning lifted |
| `deepseek-v4-flash` | complete | DeepSeek dialect; reasoning lifted |
| `qwen3.6-plus` | complete | |
| `qwen3.5-plus` | complete | |
| `mimo-v2-pro` | complete | |
| `mimo-v2-omni` | complete | |
| `mimo-v2.5-pro` | complete | |
| `mimo-v2.5` | complete | |

Cloudflare-fronted: 5xx responses in the 520-527/530 range are
covered by the default `[retry]` policy (every 4xx/5xx classifies to a
fallbackable class under the baked class matrix) so a sibling host can
take over without killing the request.

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

The OAuth-bearer auth path is exercised by a separate live test
`crates/routectl-cli/tests/live_anthropic_oauth.rs`, run with
`ROUTECTL_TEST_CLAUDE_OAUTH_TOKEN_FILE=<path>` pointing to a file that
contains a raw Anthropic OAuth bearer access token (one line).

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

Three additional ingress-through-Bedrock tests verify the
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

### Bedrock Converse (`api_shape = "converse"`)

The Converse path runs as a separate sub-matrix against the AWS
Converse API (`/model/{id}/converse`) rather than InvokeModel. Same
auth surfaces, distinct request/response shape and binary
eventstream decoder.

| Model | Mode | Notes |
|---|---|---|
| `us.anthropic.claude-haiku-4-5-20251001-v1:0` | complete + stream | |
| `us.anthropic.claude-3-5-haiku-20241022-v1:0` | complete + stream | |

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

Wire-shape notes for the chatgpt-oauth surface:

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
  and treated as no-op.

| Model | Mode | Status | Notes |
|---|---|---|---|
| `gpt-5.3-codex` | complete + stream | PASS | Default codex CLI model |
| `gpt-5.4` | complete + stream | PASS | General-purpose flagship |
| `gpt-5.4-mini` | complete + stream | PASS | Faster/cheaper variant |
| `gpt-5.4-mini` | complete (oauth://codex) | PASS | Bearer resolved through `OAuthStore` (tempdir credentials.json); ChatGPT account id auto-derived from the JWT `chatgpt_account_id` claim (no `account_id_ref`). Skipped when `OPENAI_OAUTH_ACCESS_TOKEN` is unset. |

## Native Google Gemini (`kind = "gemini"`)

Hits the public Gemini REST endpoint
(`https://generativelanguage.googleapis.com/v1beta`) with an API key
sent as the `x-goog-api-key` header. This exercises the native provider
(`generateContent` / `streamGenerateContent`), NOT the openai-compat
shim. Skipped unless `GEMINI_API_KEY` is set.

Required env var for the live matrix:
```bash
export GEMINI_API_KEY=...   # Google AI Studio API key
```

Run the Gemini matrix:
```bash
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix gemini -- --nocapture --test-threads=1
```

| Model | Mode | Notes |
|---|---|---|
| `gemini-2.5-flash` | complete + stream | Fast / cheap reference; thinkingConfig + usageMetadata exercised |
| `gemini-2.5-pro` | complete + stream | Reasoning-capable; `thoughtsTokenCount` -> `reasoning_tokens`, `thoughtSignature` replay |

Wire-shape notes for the native Gemini surface:

- **systemInstruction**: canonical system content is lifted into the
  native top-level `systemInstruction.parts` (no role), not a synthetic
  `system`-role chat message.
- **thinkingConfig**: canonical `reasoning` controls map to
  `generationConfig.thinkingConfig` (explicit budget verbatim / effort
  table / dynamic `-1`); `includeThoughts` follows `reasoning.exclude`.
- **functionDeclarations**: tools emitted as native
  `tools[].functionDeclarations[]`, not OpenAI `{type,function}` shape.
- **usageMetadata**: `cachedContentTokenCount` ->
  `cache_read_input_tokens`, `thoughtsTokenCount` -> `reasoning_tokens`.

See [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#gemini-native-kind--gemini)
for the full mapping and the before/after fidelity note.

## Cloud Code Gemini (`kind = "gemini"`, `auth_mode = "cloud-code"`, antigravity OAuth)

Hits the Cloud Code ("antigravity") surface: the daily Cloud Code host's
`/v1internal:{generate,stream}Content` with an OAuth bearer
(`Authorization: Bearer <token>`, NOT `x-goog-api-key`); `base_url` moves
the whole lane to the production host when a seat needs it. The inner
Gemini translation is reused unchanged from the api-key path; only the
transport wrapper, auth, base, and project resolution differ. See
[PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md#cloud-code-antigravity-egress-mode-auth_mode--cloud-code)
for the wire details.

Human gate (one-time): the operator runs `routectl login antigravity`
once -- a live Google consent in a browser -- to mint the
`oauth://antigravity` credential. After that the bearer is reused (and
refreshed) without further interaction.

Required env var for the live matrix:
```bash
export GEMINI_OAUTH_ACCESS_TOKEN="$(jq -r '.providers.antigravity.access_token' \
  ~/.config/routectl/credentials.json)"
```

Run the Cloud Code Gemini matrix:
```bash
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix oauth_antigravity -- --nocapture --test-threads=1
```

| Model | Mode | Status | Notes |
|---|---|---|---|
| `gemini-2.5-flash` | complete + stream (oauth://antigravity) | PENDING | Bearer resolved through `OAuthStore` (tempdir credentials.json); project id auto-resolved live via loadCodeAssist / onboardUser. Skipped (clean) until the operator runs `routectl login antigravity` and sets `GEMINI_OAUTH_ACCESS_TOKEN`. |
| `gemini-2.5-pro` | complete + stream (oauth://antigravity) | PENDING | Same path; reasoning-capable. Skipped until the operator login + env var are present. |

Deterministic coverage (GREEN in CI now): the Cloud Code transport is
pinned by wiremock tests in `crates/routectl-providers/src/gemini/mod.rs`
that run keyless in CI today --
`envelope_wrap_and_response_unwrap` (non-stream), `stream_unwraps_response_envelope`
(stream), `onboards_via_loadcodeassist`, `onboards_via_onboarduser`, and
`preserves_reasoning_and_structured_output`. The live rows above are
PENDING the one-time operator login and skip cleanly until then.

## Client x provider compatibility matrix

Coverage = which call modes are reachable end-to-end through routectl.
Fidelity = how faithfully the native wire shape is preserved on each
provider's distinguishing features. PASS/flaky/skip statuses live in the
per-provider tables above; this table is the at-a-glance summary.

| Provider (`kind`) | Non-stream | Stream | Fidelity highlights |
|---|---|---|---|
| openai-compat | yes | yes | OpenAI Chat Completions wire shape; reasoning lifted per dialect |
| anthropic-api | yes | yes | Native Messages blocks, thinking, cache_control, unified rate-limit headers |
| bedrock (invoke + converse) | yes | yes | Anthropic body on InvokeModel; vendor-neutral Converse |
| openai-responses | stream-only (`complete` force-streams) | yes | Flat Responses tool shape, encrypted_content reasoning replay |
| **gemini (native)** | yes | yes | Full on the four named features: systemInstruction, thinkingConfig, functionDeclarations, usageMetadata cached-content + thoughts tokens |
| **gemini (Cloud Code, OAuth)** | yes | yes | `auth_mode = "cloud-code"`: Bearer against the daily Cloud Code host's `/v1internal`; `{project,request,model}` envelope + `response` unwrap; inner Gemini translation reused unchanged. Wiremock-pinned in CI; live rows PENDING `routectl login antigravity` |

## Adding a new model

If you find a model not in the matrix that you want covered:

1. Add the bare model ID string to the appropriate `*_MODELS` constant
   in `crates/routectl-cli/tests/live_matrix.rs`.
2. Run the matrix locally with the relevant key set.
3. Update this doc with the result.

If the model fails with a routectl error (not an upstream 4xx/5xx), it's
a normalization gap -- the matrix log will show the exact serde failure,
which is usually a 5-10 line schema fix in
`crates/routectl-core/src/schema.rs`.
