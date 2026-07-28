# routectl docs

Deep documentation for [routectl](../README.md). Start with the README
for what routectl is, install, and quick start; come here when you
need the full story on a specific surface.

## Using routectl

| Read this... | ...when you want to |
|---|---|
| [CONFIGURATION.md](CONFIGURATION.md) | Configure anything: providers, models, aliases, retry policy, prompt-cache knobs, managed OAuth login, CLI config commands. The authoritative TOML reference. |
| [PROVIDER-QUIRKS.md](PROVIDER-QUIRKS.md) | Tune a specific upstream (Anthropic, DeepSeek, Bedrock, Gemini, vLLM, ...) or debug a 4xx with the troubleshooting matrix. |
| [LOGGING.md](LOGGING.md) | Triage a failing request: log levels, body tracing, redaction guarantees, the event catalog. |
| [REMOTE-CONTROL.md](REMOTE-CONTROL.md) | Run Claude Code through routectl while keeping Remote Control working (the optional front-proxy). |
| [TESTED_MODELS.md](TESTED_MODELS.md) | See which models are verified against the live integration matrix. |

## Contributing to routectl

| Read this... | ...when you want to |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Understand the workspace: crates, the hub-and-spoke contract, config layering. |
| [CODEMAP.md](CODEMAP.md) | Find which file owns what, per file. |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Build, test, and debug: the verification gate, runbooks, add-a-model and add-a-provider recipes. |
| [WIRE-GOTCHAS.md](WIRE-GOTCHAS.md) | Understand upstream wire-shape weirdness routectl handles (and debug new breakage). |
| [REPLAY-FIXTURES.md](REPLAY-FIXTURES.md) | Work with the replay-fixture corpus format (niche; only for replay-test debugging). |

Project-level docs live at the repo root:
[README](../README.md), [ROADMAP](../ROADMAP.md),
[CHANGELOG](../CHANGELOG.md), [SECURITY](../SECURITY.md),
[CLAUDE.md](../CLAUDE.md) (the contributor quick-router, also used by
coding agents).
