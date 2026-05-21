# routectl architecture

This document describes the module-level architecture of the routectl
Rust workspace: how the crates relate, the hub-and-spoke design that
keeps ingress dialects and egress providers decoupled, the
forward-compat catchalls that absorb upstream wire drift without code
edits, and the dispatch-time overlay model that lets configuration
layer cleanly without bloating the `Provider` trait. For per-file
detail see CODEMAP.md; for TOML configuration knobs see
CONFIGURATION.md; for upstream wire weirdness already encountered in
the wild see WIRE-GOTCHAS.md.

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
    pass-through behind the `IngressAdapter` trait.
  - `anthropic.rs` -- `POST /v1/messages`. Translates Anthropic
    Messages bodies to canonical, runs cache_control validation up
    front, renders Anthropic SSE events (`message_start`,
    `content_block_*`, `message_delta`, `message_stop`) through a
    stateful block-index machine.
  - `mod.rs` -- `IngressAdapter` trait, `SseEvent`, and
    `read_alias_header` (the `x-routectl-alias` override surface; the
    alias resolver lives on the router as
    `Router::resolve_v6_alias`, not the ingress).

## Hub-and-spoke contract

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

## Config layering

Configuration splits into two layers that compose at dispatch time:
`[providers.X]` (transport-wide knobs -- auth, base URL, runtime
gates) and `[models.X]` (per-model behavior -- reasoning, dialect,
quirks). Two fields live on BOTH layers and merge per request:
`header_extras` and `payload_extras`. The router's
`apply_layered_overlays` helper (in `routectl-router/src/router.rs`)
runs the merge before calling `provider.complete(req)` /
`provider.stream(req)` -- the `Provider` trait surface stays stable
across all five concrete providers. For the field-assignment table,
header/payload merge semantics, reserved-header buckets, and worked
examples, see CONFIGURATION.md.
