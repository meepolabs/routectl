# Working on routectl

This file is a runbook for contributors (humans and autonomous agents)
working on this repo. Read it once before making changes; refer back
when a model fails the live matrix.

## Repo map

- `crates/routectl-core/` -- `Provider` trait + OpenRouter-shape schema
  (`ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`,
  `ReasoningDetail`). Wire shapes only; no provider code.
- `crates/routectl-providers/` -- concrete provider impls. Four ship
  on by default: `openai_compat` (covers OpenAI, OpenRouter, DeepSeek,
  Groq, vLLM, NIM, llama.cpp, and any OpenAI-shaped host), `anthropic_api`
  (api-key + OAuth-bearer auth), `bedrock` (default-on `bedrock`
  Cargo feature; opt out with `--no-default-features` for a lean build
  without the AWS SDK tree), and `openai_responses` (default-on
  `openai-responses` Cargo feature; ChatGPT Codex endpoint via
  `chatgpt-oauth` bearer JWT).
  - `model_profile.rs` -- per-model quirks table. **Edit here when a
    model needs new behavior** (drops sampling params, requires
    reasoning effort, etc.).
  - `openai_compat/dialects/*.rs` -- one file per reasoning dialect.
    **Edit here when a new wire format appears**.
  - `bedrock/` -- AWS Bedrock provider. `auth.rs` resolves credentials
    through `aws-config`'s chain (or short-term bearer keys);
    `signing.rs` wraps `aws-sigv4`; `invoke.rs` reuses
    `anthropic_api::request/response` for the Anthropic Messages
    body shape; `eventstream.rs` decodes the AWS binary frame format
    for streaming.
  - `openai_responses/` -- OpenAI Responses API provider. Three auth
    surfaces: `chatgpt-oauth` (operational; ChatGPT subscription bearer
    JWT at `chatgpt.com/backend-api/codex`), `api-key` (deferred; standard
    `api.openai.com/v1`), `bedrock-mantle` (deferred; AWS Mantle proxy).
    Wire-shape notes: the chatgpt-oauth endpoint is stream-only (`complete()`
    forces `stream:true` and drains SSE to `response.completed`); tool
    definitions use the flat Responses shape (`{type,name,description,
    parameters}`) NOT the nested chat-completions shape; `tool_choice`
    named-function uses `{"type":"function","name":"X"}` NOT the nested
    `function.name` form; `instructions` must always be serialized (even
    when empty -- the server 400s if the field is absent). Module files:
    `auth.rs` (header injection), `messages.rs` (reasoning replay +
    encrypted_content), `extras.rs` (store/prompt_cache_key/text
    controls), `request.rs` (top-level body assembly), `types.rs`
    (request wire types), `response.rs` + `response_types.rs` (response
    normalization), `sse.rs` (streaming state machine), `tools.rs`
    (tool + tool_choice translation).
- `crates/routectl-router/` -- alias resolution, fallback chain, retry
  policy, provider factory.
- `crates/routectl-auth/` -- `SecretStore` trait + default impl that
  resolves `env://`, `file://`, and `literal:` secret references.
  No OS-keychain integration.
- `crates/routectl-cli/` -- axum HTTP server, clap subcommands
  (serve/test/config/login), live matrix integration tests. Two
  ingress dialects in `src/ingress/`:
  - `openai.rs` -- `POST /v1/chat/completions`, canonical wire shape
    pass-through (the existing route, refactored behind the
    `IngressAdapter` trait in v0.4.0).
  - `anthropic.rs` -- `POST /v1/messages` (v0.4.0). Translates
    Anthropic Messages bodies to canonical, runs cache_control
    validation up front, renders Anthropic SSE events
    (`message_start`, `content_block_*`, `message_delta`,
    `message_stop`) through a stateful block-index machine.
  - `mod.rs` -- `IngressAdapter` trait, `SseEvent`,
    `resolve_alias` (header > config map > literal model passthrough).

## Ingress runbook (v0.4.0)

routectl is a translation pipe with two ingress dialects feeding one
canonical `ChatRequest` and N egress providers. The hub-and-spoke
contract:

- New ingress dialect: add a file under `src/ingress/`, implement
  `IngressAdapter`, add a one-line route in `src/server/mod.rs`. Zero
  changes to providers or canonical types.
- New egress provider: implement `Provider` in `routectl-providers`.
  Zero changes to ingress adapters.
- New canonical-shape feature (e.g. an Anthropic-introduced field
  that needs to round-trip): extend `routectl-core` schema first,
  then teach the relevant ingress and egress to read/write it.
  Forward-compat catchalls (`ContentPart::Other`, `ToolDef::Other`,
  `ContentBlock::Other` on the wire) make most new Anthropic block
  types ship without code edits on the all-Anthropic path.

### When the Anthropic ingress breaks (a real client sending a real body)

1. **Reproduce against routectl directly** with a captured request
   body:

   ```bash
   curl -sN http://127.0.0.1:8787/v1/messages \
     -H "x-api-key: $ROUTECTL_TOKEN" \
     -H "content-type: application/json" \
     -d @failing-body.json | tee out.log
   ```

2. **Inspect what the egress sent upstream** with
   `RUST_LOG=routectl_providers=debug` or by running the
   `anthropic_ingress` integration test against a wiremock that
   captures the body. The failing dimension is usually one of:
   - cache_control dropped on a position routectl doesn't yet handle
     (system block / tool def / message block).
   - Unknown content block type that ContentBlock::Other should pass
     through but doesn't (custom Deserialize edge case).
   - thinking signature missing on a multi-turn assistant message
     (callers must echo `reasoning_details` with the
     `anthropic-claude-v1` format tag verbatim).

3. **Pick the right fix site**:
   - Body translation issue (Anthropic Messages -> canonical):
     `routectl-cli/src/ingress/anthropic.rs::translate_request`.
   - Content-block translation (canonical -> Anthropic wire):
     `routectl-providers/src/anthropic_api/request.rs::translate_content_part`.
   - Missing wire field on the response side: extend
     `routectl-providers/src/anthropic_api/types.rs::AnthropicResponse`
     and `walk_content_blocks`.
   - SSE event ordering (e.g. Anthropic emits a new event type):
     `routectl-cli/src/ingress/anthropic.rs::render_chunk_internal`
     state machine; mirror the wire decoder in
     `routectl-providers/src/anthropic_api/sse.rs::SseState`.

4. **Add an integration test** in
   `crates/routectl-cli/tests/anthropic_ingress.rs` that drives the
   server with the failing body and asserts on the upstream-side
   wiremock body. Re-run the live matrix.

### Configuring listener auth + ingress aliases

```toml
[server]
host = "127.0.0.1"
port = 8787
strict_translation = false   # set true for production CI

[server.auth]
tokens = ["env://ROUTECTL_LISTENER_TOKEN", "literal:sk-routectl-dev"]

# Map upstream model IDs to routectl aliases. Useful when a client
# can't override the `model` field directly, so the mapping happens
# server-side.
[ingress.anthropic.aliases]
"claude-opus-4-20250514"      = "heavy"
"claude-sonnet-4-5-20250929"  = "default"
"claude-haiku-4-5-20251001"   = "fast"

# Alternative: client sets `x-routectl-alias: heavy` and the model
# field is ignored. Header always wins over the aliases map.
```

## Verification gate

Every change must keep two things green:

```bash
# Unit + integration tests across the whole workspace.
cargo test --workspace --features bedrock --release

# Live matrix against real providers. Requires OPENROUTER_API_KEY,
# OPENCODE_GO_API_KEY, NIM_API_KEY in env (skips per-provider when
# missing). 5/5 tests must pass; per-provider PASS counts must match
# the baseline in docs/TESTED_MODELS.md.
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1

# Lean build for downstream library consumers who don't want the
# AWS dependency tree:
cargo check --workspace --no-default-features \
  --features openai-compat,anthropic-api
```

The live matrix is slow (~30s) and costs cents per run. Use it as a
final gate, not a tight inner loop.

## Logging recipes

routectl uses `tracing` with the env filter `ROUTECTL_LOG` (NOT the
default `RUST_LOG`, since we don't want stray `RUST_LOG=debug` exports
turning routectl into a firehose).

Default level is `info`. Every log line carries the module path
(`routectl_router::router`, `routectl_providers::bedrock`, etc.) and,
inside an HTTP request, the `request_id` field for correlation across
fallback hops.

```bash
# Full debug across all routectl crates.
ROUTECTL_LOG=routectl=debug,routectl_providers=debug,routectl_router=debug \
  ./routectl serve

# Bedrock-only deep dive (SigV4 inputs + eventstream frames).
ROUTECTL_LOG=routectl=info,routectl_providers::bedrock=trace ./routectl serve

# Auth tracing only (secret resolution + credential failures + listener
# rejections + upstream 401/403).
ROUTECTL_LOG=routectl_auth=warn,routectl_providers::bedrock::auth=warn,\
routectl_providers::bedrock::signing=warn,\
routectl_cli::server::auth=warn ./routectl serve

# Quiet -- only warnings and errors.
ROUTECTL_LOG=warn ./routectl serve
```

### Request correlation

Every request gets a `request_id`. Either supply your own via the
`x-request-id` header (echoed back on the response so your client logs
match) or routectl mints a `Uuid::now_v7()` (sortable by time). All
log lines emitted while processing the request inherit `request_id` as
a span field, so:

```bash
ROUTECTL_LOG=info ./routectl serve 2>&1 | grep request_id=probe-1
```

shows every event for one specific request: ingress parse, alias
resolution, fallback hops, retry attempts, upstream calls, response
shape, errors.

### Triage recipes (full bodies on demand)

When `body_excerpt=...` in a WARN line isn't enough -- typically when an
upstream returns a generic `400 "request not valid"` and you need to
see WHICH wire field tripped it -- flip the log level. The output
includes ingress body, outgoing egress body, and the full upstream
error body, all carrying the same `request_id` so a single grep
correlates them:

```bash
# Full upstream error bodies (4 KB cap, debug):
ROUTECTL_LOG=routectl=debug ./routectl serve

# Also outgoing + ingress bodies (16 KB cap, trace):
ROUTECTL_LOG=routectl=trace ./routectl serve

# Trace one specific request end-to-end:
ROUTECTL_LOG=routectl=trace ./routectl serve 2>&1 | grep request_id=<id>

# Which Bedrock-Invoke beta flags are getting filtered (operator
# suspects AWS allowlist drift; see issues.md::INV-6):
ROUTECTL_LOG=routectl_providers::bedrock=debug ./routectl serve 2>&1 \
  | grep "dropping beta flag"
```

What you get at debug:
- Existing `body_excerpt=...` WARN on every 4xx/5xx (200B truncated,
  scannable in `routectl-warn.log`)
- New `body=...` DEBUG with the full upstream error body (4 KB cap,
  HTML-collapsed)

What you get at trace, additionally:
- `body=...` TRACE with the JSON body routectl sent to the upstream
  (16 KB cap)
- `body=...` TRACE with the body the client sent on
  `/v1/chat/completions` or `/v1/messages` (no cap; the canonical
  schema bounds it via `MAX_INGRESS_BODY_BYTES`)

Sensitivity caveat: bodies contain user prompts. Leave `ROUTECTL_LOG`
at the default `info` level in production. Only flip to debug/trace
during active triage and prefer redirecting the output to a file
(`./routectl serve 2>/tmp/triage.log`) rather than tailing live.

### Auth-failure log shapes (no secret values, ever)

| Surface | Log line |
|---|---|
| Listener auth (wrong `x-api-key` / `Bearer`) | `WARN routectl_cli::server::auth has_x_api_key=<bool> has_bearer=<bool> route=<path> "listener auth rejected"` |
| Bad secret ref (`env://NONEXISTENT`) | `WARN routectl_auth::memory_store scheme=env:// var=<NAME> reason="not set" "secret resolution failed"` |
| Bad secret ref (file perm too open) | `WARN routectl_auth::memory_store scheme=file:// path=<P> mode=<oct> reason="group/other readable; chmod 600 or 400" "secret resolution failed"` |
| Bedrock SigV4 / cred chain failed | `WARN routectl_providers::bedrock::auth variant=Profile\|DefaultChain region=<r> error=... "bedrock credential resolution failed"` |
| Bedrock SigV4 sign failure | `ERROR routectl_providers::bedrock::signing failure_kind=<kind> ... "bedrock auth failed"` -- where `<kind>` is one of `bearer_header_invalid`, `creds_unavailable`, `body_unbuffered`, `signing_params_build`, `non_ascii_header`, `signable_request_build`, `sigv4_sign`, `signed_header_name_invalid`, `signed_header_value_invalid`, `unexpected_query_params` |
| Bedrock 403 (IAM denied) | `WARN routectl_providers::bedrock provider=<id> status=403 action=<bedrock-runtime:InvokeModel...> principal_present=<bool> "bedrock IAM access denied"` -- `action` extracted from the AWS error body so you immediately see WHICH IAM action your role lacks |
| Bedrock in-stream auth event | `WARN routectl_providers::bedrock::eventstream provider=<id> event_type=accessDeniedException\|unauthorizedException\|authentication_error\|permission_error message=... "bedrock in-stream auth/permission exception"` |
| Anthropic upstream 401/403 | `WARN routectl_providers::anthropic_api provider=<id> status=<401\|403> auth_kind=<ApiKey\|OauthBearer> message=... "anthropic upstream auth failed"` |
| OpenAI-compat upstream 401/403 | `WARN routectl_providers::openai_compat provider=<id> status=<401\|403> body_excerpt=... "openai-compat upstream auth failed"` |

### What's never logged

- Resolved secret values (env contents, file contents, OAuth tokens,
  bearer keys, AWS access/secret keys).
- The supplied `x-api-key` / `Authorization: Bearer` value on a
  rejected listener auth (we log only header presence).
- Full upstream request/response bodies. Bodies are only excerpted to
  256 chars on 4xx/5xx upstream paths, intentionally. Wire-level
  body dump is a future opt-in (`ROUTECTL_DEBUG_BODIES=1`) with
  header-redaction tests; it does not exist yet.

## When a model breaks the live matrix

1. **Add the failing target to the matrix** in
   `crates/routectl-cli/tests/live_matrix.rs`. The const arrays are
   `OPENROUTER_MODELS`, `OPENCODE_GO_MODELS`, `NIM_MODELS`. One
   string per row; the test harness handles the rest.

2. **Run the matrix** and capture the failing row:

   ```bash
   RUST_LOG=routectl_providers=debug cargo test -p routectl-cli \
     --features live-integration --release --test live_matrix \
     <test_name> -- --nocapture --test-threads=1
   ```

   Read the `FAIL` line for the target. The error message tells you
   which layer failed (upstream HTTP, response normalization, chunk
   deserialization, ...).

3. **Capture the raw upstream JSON or SSE** with curl. For
   non-streaming, hit the provider directly with the same request body
   the test sends. For streaming, add `"stream": true` and pipe to a
   file. This is the truth source -- match the failure mode against
   the gotchas below.

4. **Pick the right fix site**:

   - **Model has new quirks** (drops some param, requires effort,
     uses adaptive thinking): add a row to
     `crates/routectl-providers/src/model_profile.rs::PROFILES`.
     One row, declarative, compiler validates the shape.

   - **Provider returns a previously-unseen wire format**: drop a new
     file in `crates/routectl-providers/src/openai_compat/dialects/`,
     add one variant to `ReasoningDialect` in
     `crates/routectl-providers/src/openai_compat/dialect.rs`, and add
     one arm to `ReasoningDialect::as_dyn()` in
     `crates/routectl-providers/src/openai_compat/dialects/mod.rs`.
     Three edits in two files.

   - **Schema-shape edge case** (missing field, null value, duplicate
     key): the schema in `crates/routectl-core/src/schema.rs` is
     already defensive. If the bug is a new shape, prefer fixing the
     openai-compat preprocessor in
     `crates/routectl-providers/src/openai_compat/response.rs::merge_reasoning_keys`
     or
     `crates/routectl-providers/src/openai_compat/sse.rs::coalesce_chunk_reasoning_keys`
     before changing the schema.

5. **Re-run the matrix.** Commit only if green.

## Common gotchas already encountered

These all happen in the wild; the codebase already handles them.
Refer back when a similar failure mode shows up.

- **`null` content alongside non-null reasoning**. NIM's
  `meta/llama-3.3-70b-instruct` returns both `reasoning: null` and
  `reasoning_content: null`. Handled by `merge_reasoning_keys` in
  `crates/routectl-providers/src/openai_compat/response.rs` -- it
  drops null `reasoning` and promotes non-null `reasoning_content`.
  `MessageContent::Null` variant in
  `crates/routectl-core/src/schema.rs` lets the response deserialize
  cleanly.

- **Missing `id` / `created` on responses or chunks**. NIM's
  `google/gemma-3-12b-it` omits `created`; some chunks omit `id`.
  Handled by `#[serde(default)]` on the optional fields in
  `ChatResponse` / `ChatChunk`.

- **Post-`[DONE]` SSE trailers**. Some openai-compat hosts emit a
  bookkeeping chunk (e.g. `data: {"choices":[],"cost":"0"}`) after
  the `[DONE]` terminator. Handled by an explicit `return` in the SSE
  loop at `crates/routectl-providers/src/openai_compat/mod.rs` --
  `[DONE]` stops parsing.

- **DeepSeek 400 on `reasoning_content` echoed in history**. The
  DeepSeek dialect's `Dialect::strip_history_reasoning()` returns
  true; the request normalizer strips the field before sending.

- **`<think>` tags split across stream chunks**. Handled by
  `ThinkTagAccumulator` in
  `crates/routectl-providers/src/openai_compat/sse.rs` -- a
  cross-chunk state machine. Lives outside the dispatch path because
  the trait is stateless.

- **HTML upstream error bodies leaking into the JSON envelope**.
  Misconfigured `base_url`s land on marketing 404 pages. Handled by
  `sanitize_upstream_body` in
  `crates/routectl-providers/src/openai_compat/mod.rs`.

- **Strict openai-compat hosts 400 on top-level `system`**. NIM
  rejects with `Validation: Unsupported parameter(s): system` because
  `system` is the Anthropic-shape top-level field; OpenAI carries it
  as a `role: "system"` message. The OpenAI ingress lifts wire
  `role: "system"` into canonical `req.system`; the openai-compat
  egress at
  `crates/routectl-providers/src/openai_compat/request.rs::normalize`
  does the inverse lower (synthetic `role: "system"` message
  prepended; top-level `system` removed) so neither lenient (OpenAI,
  OpenRouter, opencode-go) nor strict (NIM) hosts see the
  Anthropic-shape field.

- **Anthropic `tool_choice` rejects bare-string OpenAI shape**.
  Bedrock validators 400 on `tool_choice: "auto"` (OpenAI) where
  Anthropic expects `{"type":"auto"}`. Handled at the Anthropic-API
  egress by `translate_tool_choice` in
  `crates/routectl-providers/src/anthropic_api/request.rs`. Maps
  `"auto"|"none"|"required"` plus the OpenAI `{"type":"function",...}`
  object form into the Anthropic tagged-enum shape; Anthropic-shape
  inputs pass through unchanged.

- **Anthropic structured-output `output_format` (legacy) silently
  dropped at the ingress.** Anthropic's current wire shape is
  `output_config.format = {type: "json_schema", schema: ...}`. Older
  callers (and the Claude-Code legacy SDK path) still send a
  top-level `output_format` field; serde on `ChatRequest` does not
  know that name and silently dropped it before Layer 1 fix. Now
  `merge_output_format` in
  `crates/routectl-cli/src/ingress/anthropic.rs` rewrites the
  legacy field into `output_config.format` (preserving any existing
  `output_config.effort`); when both shapes arrive on one request it
  prefers the nested form and WARNs, mirroring claude-code's own
  deprecation message. The egresses need no extra translation:
  Bedrock-Invoke for Claude is Anthropic-shape passthrough, and the
  Anthropic-API egress already merges `provider_extras["output_config"]`
  into the body.

- **Forward-compat sweep on the Anthropic ingress.** Anthropic adds
  new top-level body fields on a quarterly cadence (e.g. recent
  additions: `context_management`, `context_hint`, `speed`,
  `diagnostics`, `mcp_servers`). Without an explicit pull-out, serde
  on `ChatRequest` silently drops them at the ingress boundary.
  `translate_request` now sweeps every key NOT in
  `CANONICAL_CHAT_REQUEST_WIRE_FIELDS` into `provider_extras` so the
  egress's `merge_provider_extras` forwards them upstream verbatim.
  When canonical adds a new field, also add it to the
  `CANONICAL_CHAT_REQUEST_WIRE_FIELDS` const in
  `crates/routectl-cli/src/ingress/anthropic.rs`.

- **Inbound `anthropic-beta` HTTP header was dropped.** The
  `@anthropic-ai/sdk` Beta API translates the SDK option `betas:
  [...]` into the `anthropic-beta: a,b,c` HTTP header (not into the
  body's `anthropic_beta` array). claude-code uses this surface for
  first-party betas (context-management, prompt-cache-1h,
  adaptive-thinking, ...). routectl now lifts inbound header values
  into canonical `req.anthropic_beta` (deduplicated, preserving body
  order) so the egress emits them in the upstream body.
  Anthropic accepts either surface.

- **Bedrock-Invoke rejects unsupported `anthropic_beta` values.**
  Bedrock validates each entry of `anthropic_beta` independently and
  400s the entire request on the first unsupported value -- there is
  no per-flag fallback. claude-code's TS SDK ships up to ten betas
  via the `anthropic-beta` HTTP header, only ten of which Bedrock
  has gated for distribution (one global allowlist; verified
  identical across haiku-4-5 / sonnet-4-6 / opus-4-7). The
  `BEDROCK_INVOKE_ACCEPTED_BETAS` const in
  `crates/routectl-providers/src/bedrock/invoke.rs` is the empirical
  set as of 2026-05-10; `filter_bedrock_invoke_betas` drops unknowns
  at DEBUG (not WARN -- the TS SDK reliably sends ~5 unsupported
  flags every request and WARN would flood `routectl-warn.log`).
  Two complementary operator escape hatches:
  - **Global override** -- `[bedrock] anthropic_beta = [...]` at the
    top level of TOML REPLACES the const allowlist for every Bedrock
    provider in the config. Use this to add flags AWS gated after
    your routectl release, or to remove flags AWS deprecated. The
    const is the default when this field is unset.
  - **Per-provider floor** -- `[providers.X] anthropic_beta = [...]`
    is unchanged: those flags are always sent and bypass the filter
    (operator-asserted), independent of the global allowlist.

  When the Bedrock Converse adapter (M2.7) lands, the analogous
  filter belongs there too -- see the `TODO(M5)` in `invoke.rs`.

- **Response `stop_reason` round-trip was lossy for Anthropic-only
  values.** The Anthropic egress maps `stop_reason -> finish_reason`
  for the OpenAI overlap (`end_turn`/`stop_sequence` -> `stop`,
  `max_tokens` -> `length`, `tool_use` -> `tool_calls`) and passes
  everything else through verbatim. The Anthropic ingress used to
  reverse-map only those four and clobber unknown values to
  `end_turn`, breaking claude-code's per-stop-reason error handling
  for `pause_turn`, `refusal`, and `model_context_window_exceeded`.
  Fixed: `openai_finish_to_anthropic_stop` now passes through unknown
  values. The `stop_sequence -> stop -> end_turn` ambiguity remains
  (information lost at the canonical layer); preserving it would
  require an additional native finish-reason field on the canonical
  Choice.

- **OpenAI Responses chatgpt-oauth endpoint is stream-only.** Sending
  `stream:false` returns HTTP 400 `{"detail":"Stream must be set to true"}`.
  `OpenAiResponsesProvider::complete()` forces `stream:true`, drains the SSE
  stream until a `response.completed` (or `response.failed` /
  `response.cancelled`) event, and extracts the `response` field from that
  event as the final body. The streaming tests in `mod.rs::e2e_tests` use
  a wiremock that returns an SSE `response.completed` event rather than a
  plain JSON body.

- **OpenAI Responses tools/tool_choice must use the flat Responses shape.**
  The chatgpt-oauth backend 400s with
  `"Missing required parameter: 'tools[0].name'"` on the nested
  chat-completions shape `{type:"function",function:{name,...}}`. The flat
  shape is `{type:"function",name,description,parameters,strict}` (no
  nested `function` key). Similarly, `tool_choice` named-function must be
  `{"type":"function","name":"X"}` (flat); the nested form
  `{"type":"function","function":{"name":"X"}}` returns
  `"Unknown parameter: 'tool_choice.function'"`. Both are handled in
  `openai_responses/types.rs` (`ResponsesTool::Function` variant) and
  `openai_responses/tools.rs` (`translate_tool_choice_object`).

- **OpenAI Responses `instructions` field must always serialize.** The
  chatgpt-oauth backend returns HTTP 400 `{"detail":"Instructions are
  required"}` when the field is absent. An empty string `""` is accepted.
  The field on `ResponsesRequest` does NOT carry
  `#[serde(skip_serializing_if = "String::is_empty")]` -- it is always
  emitted (possibly as `""`) so the server never sees the field missing.

## Adding a new model to the matrix

Step-by-step example: "OpenAI launches o5-mini on OpenRouter."

1. Append to `OPENROUTER_MODELS` in
   `crates/routectl-cli/tests/live_matrix.rs`:
   ```rust
   "openai/o5-mini",
   ```

2. Append to `PROFILES` in
   `crates/routectl-providers/src/model_profile.rs`:
   ```rust
   ModelProfile {
       pattern: "o5",
       kind: MatchKind::Prefix,
       drops_sampling_params: true,
       requires_reasoning_effort: true,
       ..ModelProfile::DEFAULT
   },
   ```

3. Add a unit test in `model_profile.rs::tests` mirroring
   `openai_o3_mini_matches_prefix`.

4. Run the live matrix gate. Done.

## Style notes

- ASCII-only in code, comments, and commit messages. No em-dashes,
  curly quotes, emoji, or arrows. `--`, `->`, straight quotes.
- Keep functions under 50 lines, files under 800.
- Prefer one file per dialect / one row per quirk. The matrix proves
  the wiring; tight files keep edits surgical.
- Don't add backwards-compatibility shims. If a schema changes,
  change it; the live matrix catches regressions.
