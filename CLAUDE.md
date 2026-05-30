# Working on routectl

routectl is a Rust LLM routing proxy: two HTTP ingress dialects
(OpenAI Chat Completions, Anthropic Messages) feed one canonical
`ChatRequest` and N egress providers (openai-compat, anthropic-api,
bedrock invoke + converse, openai-responses). This file is a slim
runbook for contributors (humans and autonomous agents). Read it
once; jump to the doc that matches your task.

## The 5 crates at a glance

- `routectl-core` -- canonical wire types (`ChatRequest`,
  `ChatResponse`, `ChatChunk`, `Message`, `ReasoningDetail`) and
  the `Provider` trait
- `routectl-providers` -- concrete provider impls (`openai_compat`,
  `anthropic_api`, `bedrock`, `openai_responses`) plus the per-model
  quirks table (`model_profile.rs`), the per-dialect reasoning
  files (`openai_compat/dialects/*.rs`), and the shared
  `effort.rs` / `header_trace.rs` helpers
- `routectl-router` -- alias resolution, fallback chain, retry
  policy, dispatch-time overlay merge
- `routectl-auth` -- `SecretStore` trait + default resolver for
  `env://`, `file://`, and `literal:` references
- `routectl-cli` -- axum HTTP server, clap subcommands
  (serve / login / logout / refresh / whoami / test / config),
  live matrix integration tests

For per-file detail see [docs/CODEMAP.md](docs/CODEMAP.md). For
module-level architecture and the hub-and-spoke design see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Hub-and-spoke contract

routectl is a translation pipe with two ingress dialects feeding
one canonical `ChatRequest` and N egress providers:

- **New ingress dialect**: add a file under `src/ingress/`,
  implement `IngressAdapter`, add a one-line route in
  `src/server/mod.rs`. Zero changes to providers or canonical types.
- **New egress provider**: implement `Provider` in
  `routectl-providers`. Zero changes to ingress adapters.
- **New canonical-shape feature**: extend `routectl-core` schema
  first, then teach the relevant ingress and egress to read/write
  it. Forward-compat catchalls (`ContentPart::Other`,
  `ToolDef::Other`, `ContentBlock::Other` on the wire) make most
  new Anthropic block types ship without code edits on the
  all-Anthropic path.

## Verification gate

Every change must keep the workspace tests, the lean-feature build,
and the live matrix green. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)
"Verification gate" for the exact commands; that doc is the
contributor workflow source of truth.

## When something breaks: where to look

| Task | Doc |
|---|---|
| Find which file does X | [docs/CODEMAP.md](docs/CODEMAP.md) |
| Module-level architecture, hub-and-spoke design, config-layering rationale | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| Add a new model, debug a failing matrix row, extend a provider | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| Configure listener auth, providers, models, aliases, retry, header/payload extras merge | [docs/CONFIGURATION.md](docs/CONFIGURATION.md) |
| Tune a specific upstream (DeepSeek v4 echo-back, NIM cold-start, Opus 4.7+ adaptive thinking, Bedrock allowlist) | [docs/PROVIDER-QUIRKS.md](docs/PROVIDER-QUIRKS.md) |
| Triage a failing request (logs, env filter, redaction, request_id correlation, auth-failure shapes) | [docs/LOGGING.md](docs/LOGGING.md) |
| Investigate an upstream wire-shape bug (does routectl already handle this? where in the code?) | [docs/WIRE-GOTCHAS.md](docs/WIRE-GOTCHAS.md) |
| Verify against the known-good live-matrix baseline | [docs/TESTED_MODELS.md](docs/TESTED_MODELS.md) |

## Style notes

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) "Style notes" -- ASCII-only,
function/file size ceilings, no back-compat shims, one file per dialect.
