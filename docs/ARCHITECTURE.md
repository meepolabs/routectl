# routectl architecture

Module-level architecture of the routectl Rust workspace: how the
crates relate, the hub-and-spoke design, and the dispatch-time
overlay model. For per-file detail see [CODEMAP.md](CODEMAP.md);
for TOML configuration see [CONFIGURATION.md](CONFIGURATION.md).

## Repo map

- `crates/routectl-core/` -- `Provider` trait + canonical schema
  (`ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`,
  `ReasoningDetail`). Wire shapes only; no provider code.

- `crates/routectl-providers/` -- five concrete provider impls,
  default-on:
  - `openai_compat` -- OpenAI-shape hosts (OpenAI, OpenRouter,
    DeepSeek, Groq, vLLM, NIM, llama.cpp, ...). Per-dialect files in
    `dialects/`.
  - `anthropic_api` -- native Anthropic Messages API (api-key +
    OAuth-bearer); ships server-side `context-management-2025-06-27`
    beta emulation behind the per-provider `context_management` knob.
  - `bedrock` -- AWS Bedrock with SigV4 + InvokeModel + Converse.
    Behind a `bedrock` Cargo feature; the providers library can opt
    out with `cargo check -p routectl-providers --no-default-features
    --features openai-compat,anthropic-api` for an AWS-SDK-free build
    (at least one provider feature must always be enabled). The
    shipped `routectl` binary always links the AWS SDK (routectl-cli
    hardcodes `bedrock`).
  - `openai_responses` -- OpenAI Responses API (chatgpt-oauth JWT,
    OpenAI api-key, or AWS bedrock-mantle bearer); `complete()`
    force-streams.
  - `gemini` -- native Google Gemini (`generateContent` /
    `streamGenerateContent`) with api-key or Cloud Code OAuth
    (`oauth://antigravity`) auth modes.

  `model_profile.rs` is the per-model quirks table (edit here when a
  model needs new behavior). `effort.rs` and `header_trace.rs` are
  the shared helpers for effort clamping and 4-direction header
  tracing.

- `crates/routectl-router/` -- alias resolution, fallback chain
  walker, retry policy, capability filter (`unsupported_features`),
  provider factory. A `[pools.<name>]` block groups same-kind member
  provider entries; the factory compiles one seat per usable member
  (from that member's OWN credential ref) once per pool, and
  `seat_pool.rs` owns the request-time dispatch order plus the
  pool-keyed rotation and sticky-pin state, slotting those seats into
  the fallback chain as ordinary hops.

- `crates/routectl-auth/` -- `SecretStore` trait + resolvers for
  `env://`, `file://`, and `oauth://<provider>` (PKCE login + atomic
  credentials.json + lazy refresh). `literal:` refs are rejected at
  parse and resolve -- reference a 0600 file instead. Every ref names
  exactly ONE seat: `oauth://<provider>#<label>` pins the labelled
  seat, and a bare `oauth://<provider>` pins the DEFAULT seat
  (reaching several accounts is what a `[pools.<name>]` block is for).
  No OS-keychain integration.

- `crates/routectl-usage/` -- SQLite-backed per-request usage
  accounting: `UsageWriter` (async-producer -> blocking-writer
  bridge), `UsageHandle` (dispatch-time send handle), `cost.rs`
  (request pricing), `query.rs` (the `routectl usage` read path).
  Intentionally thin -- no AWS SDK or axum dependency.

- `crates/routectl-cli/` -- axum HTTP server, clap CLI (`serve`,
  `init`, `provider`, `doctor`, `probe`, `login`, `logout`,
  `refresh`, `whoami`, `test`, `config`, `catalog`, `usage`, `rc`),
  the three ingress dialects (`openai.rs` for
  `POST /v1/chat/completions`, `anthropic/` for `POST /v1/messages`
  + `POST /v1/messages/count_tokens`, `openai_responses/` for
  `POST /v1/responses`), and the read-only status dashboard
  (`GET /` + the `/status/*` JSON panels). Live matrix integration
  tests live here.

- `crates/routectl-testkit/` -- dev-only shared test doubles and
  harnesses (tracing capture, restore-on-drop env guard, the
  two-server cross-host redirect pin every credentialed egress lane's
  redirect regression test drives); depended on by the other crates'
  test targets only, never by shipped code.

## Hub-and-spoke contract

routectl is a translation pipe with three ingress dialects (OpenAI
Chat Completions, Anthropic Messages, OpenAI Responses) feeding one
canonical `ChatRequest` and five egress provider classes. The
contract for extending it:

- **New ingress dialect**: add a file under
  `routectl-cli/src/ingress/`, implement `IngressAdapter`, add a
  one-line route in `src/server/mod.rs`. Zero changes to providers or
  canonical types.
- **New egress provider**: implement `Provider` in
  `routectl-providers`. Zero changes to ingress adapters.
- **New canonical-shape feature**: extend the `routectl-core` schema
  first, then teach the relevant ingress and egress to read/write it.
  Forward-compat catchalls (`ContentPart::Other`, `ToolDef::Other`,
  `ContentBlock::Other` on the wire) make most new Anthropic block
  types ship without code edits on the all-Anthropic path.

## Config layering

Configuration splits into two layers that compose at dispatch time:

- `[providers.X]` -- transport-wide knobs: auth, base URL, runtime
  gates.
- `[models.X]` -- per-model behavior: reasoning, dialect, quirks.

Two fields live on BOTH layers and merge per request:
`header_extras` and `payload_extras`. The router runs the merge
(`apply_layered_overlays` in `routectl-router/src/router.rs`) before
calling `provider.complete(req)` / `provider.stream(req)`, so the
`Provider` trait surface stays stable across all five concrete
providers. For the field-assignment table, merge semantics, and
worked examples, see [CONFIGURATION.md](CONFIGURATION.md).
