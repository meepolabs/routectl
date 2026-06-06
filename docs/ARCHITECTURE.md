# routectl architecture

Module-level architecture of the routectl Rust workspace: how the
crates relate, the hub-and-spoke design, and the dispatch-time
overlay model. For per-file detail see [CODEMAP.md](CODEMAP.md);
for TOML configuration see [CONFIGURATION.md](CONFIGURATION.md).

## Repo map

- `crates/routectl-core/` -- `Provider` trait + canonical schema
  (`ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`,
  `ReasoningDetail`). Wire shapes only; no provider code.

- `crates/routectl-providers/` -- four concrete provider impls,
  default-on:
  - `openai_compat` -- OpenAI-shape hosts (OpenAI, OpenRouter,
    DeepSeek, Groq, vLLM, NIM, llama.cpp, ...). Per-dialect files in
    `dialects/`.
  - `anthropic_api` -- native Anthropic Messages API (api-key +
    OAuth-bearer); ships server-side `context-management-2025-06-27`
    beta emulation behind the per-provider `context_management` knob.
  - `bedrock` -- AWS Bedrock with SigV4 + InvokeModel + Converse.
    Behind a `bedrock` Cargo feature; the providers library can opt
    out with `cargo check -p routectl-providers --no-default-features`
    for an AWS-SDK-free build. The shipped `routectl` binary always
    links the AWS SDK (routectl-cli hardcodes `bedrock`).
  - `openai_responses` -- OpenAI Responses API (chatgpt-oauth JWT,
    OpenAI api-key, or AWS bedrock-mantle bearer); `complete()`
    force-streams.

  `model_profile.rs` is the per-model quirks table (edit here when a
  model needs new behavior). `effort.rs` and `header_trace.rs` are
  the shared helpers for effort clamping and 4-direction header
  tracing.

- `crates/routectl-router/` -- alias resolution, fallback chain
  walker, retry policy, capability filter (`unsupported_features`),
  provider factory.

- `crates/routectl-auth/` -- `SecretStore` trait + resolvers for
  `env://`, `file://`, `literal:`, and `oauth://<provider>` (PKCE
  login + atomic credentials.json + lazy refresh). No OS-keychain
  integration.

- `crates/routectl-cli/` -- axum HTTP server, clap CLI (`serve`,
  `login`, `logout`, `refresh`, `whoami`, `test`, `config`), and
  the two ingress dialects (`openai.rs` for
  `POST /v1/chat/completions`, `anthropic/` for `POST /v1/messages`
  + `POST /v1/messages/count_tokens`). Live matrix integration tests
  live here.

## Hub-and-spoke contract

See [`CLAUDE.md`](../CLAUDE.md) "Hub-and-spoke contract" -- the
canonical statement of what changes when a new ingress dialect, a
new egress provider, or a new canonical-shape feature lands.

## Config layering

Configuration splits into two layers that compose at dispatch time:
`[providers.X]` (transport-wide knobs -- auth, base URL, runtime
gates) and `[models.X]` (per-model behavior -- reasoning, dialect,
quirks). Two fields live on BOTH layers and merge per request:
`header_extras` and `payload_extras`. The router's
`apply_layered_overlays` helper (in `routectl-router/src/router.rs`)
runs the merge before calling `provider.complete(req)` /
`provider.stream(req)` -- the `Provider` trait surface stays stable
across all four concrete providers. For the field-assignment table,
header/payload merge semantics, reserved-header buckets, and worked
examples, see [CONFIGURATION.md](CONFIGURATION.md).
