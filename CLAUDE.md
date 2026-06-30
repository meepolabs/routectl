# Working on routectl

routectl is a Rust LLM routing proxy: two HTTP ingress dialects
(OpenAI Chat Completions, Anthropic Messages) feed one canonical
`ChatRequest` and N egress providers (openai-compat, anthropic-api,
bedrock invoke + converse, openai-responses). This file is a slim
runbook for contributors (humans and autonomous agents). Read it
once; jump to the doc that matches your task.

## The 6 crates at a glance

- `routectl-core` -- canonical wire types (`ChatRequest`,
  `ChatResponse`, `ChatChunk`, `Message`, `ReasoningDetail`) and
  the `Provider` trait; prompt-cache primitives in
  `cache_control.rs` (`CacheControl`, the `CacheBreakpointSource`
  walk, `FrozenFloor` / `compute_frozen_floor`), the
  stable-prefix volatile-content detector (`volatile.rs`), and
  the lossless cache-safe context-reduction primitive
  (`context_reduction.rs`)
- `routectl-providers` -- concrete provider impls (`openai_compat`,
  `anthropic_api`, `bedrock`, `openai_responses`) plus the per-model
  quirks table (`model_profile.rs`), the per-dialect reasoning
  files (`openai_compat/dialects/*.rs`), and the shared
  `effort.rs` / `header_trace.rs` helpers
- `routectl-router` -- alias resolution, fallback chain, retry
  policy, dispatch-time overlay merge. Single-entry alias chains
  (`AliasValue::Single` / one-element `Chain`) have no fallback by
  design; pair a non-Anthropic primary with a fallback target if
  availability matters.
- `routectl-auth` -- `SecretStore` trait + default resolver for
  `env://`, `file://`, `literal:`, and `oauth://` references
- `routectl-cli` -- axum HTTP server, clap subcommands
  (serve / login / logout / refresh / whoami / test / config / usage / pricing),
  live matrix integration tests
- `routectl-usage` -- SQLite per-request usage accounting:
  `UsageRecord`, `UsageHandle`, cost estimation, retention pruning,
  and the query layer backing `routectl usage`

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
| Capture and replay against your local corpus (regression / feature dev) | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) "Adding a replay fixture" + [docs/REPLAY-FIXTURES.md](docs/REPLAY-FIXTURES.md) format |

## Style notes

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) "Style notes" -- ASCII-only,
function/file size ceilings, no back-compat shims, one file per dialect.
