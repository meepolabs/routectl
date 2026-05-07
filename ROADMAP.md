# routectl roadmap

## v0.1 (DONE -- 2026-05-06)

Goal: a binary you can `cargo install`, point clients at, and have it route real LLM calls with fallback + reasoning normalization. No cookie-auth in default build.

128 tests pass across the workspace. Release binary 5.6MB stripped.

### Implementation status

1. **`routectl-core`** trait + schema -- DONE
2. **`routectl-providers::openai_compat`** -- DONE (5 dialects, 34 tests including SSE state machine)
3. **`routectl-providers::anthropic_api`** -- DONE (signature preservation across multi-turn + streaming, 20 tests)
4. **`routectl-router`** -- DONE (alias resolver + fallback walker + retry with exponential backoff + provider factory, 17 tests)
5. **`routectl-auth`** -- DONE (`SecretStore` trait, `KeyringStore`, `MemoryStore`, 26 tests)
6. **`routectl-cli::serve`** -- DONE (axum server, streaming + non-streaming, bind safety, 10 tests)
7. **`routectl-cli::test`** -- DONE (one-shot via Router with reasoning pretty-print)
8. **`routectl-cli::config`** -- DONE (check/show/example, 8 tests)
9. **Distribution** -- DEFERRED (no `cargo dist` / Homebrew tap yet; manual `cargo build --release` for now)

## v0.2 (cookie-auth)

Out of the default build, opt-in via cargo features.

1. **`routectl-cli::login`** with `wry` webview
   - Tiny embedded browser, navigate to upstream login URL, capture cookies on success
   - Persist to OS keychain
2. **`routectl-providers::claude_cookie`**
   - Port logic from OpenClaw/Hermes Agent
   - Map claude.ai internal envelope to normalized response
3. **`routectl-providers::chatgpt_cookie`**
   - Port from `revChatGPT`-style references
   - cf-clearance handling
4. **Session refresh** when cookies expire

Responsible-use disclosure on every login flow. ToS-on-user.

## Post-v0.2 (deferred / never)

- Caching layer (use a proxy if you want this)
- Web UI / config editor (CLI-only by design)
- Server mode (multi-user, TLS, persistent state) -- out of scope, fork if you want it
- Cost-aware routing (overlap with LMSYS RouteLLM, different product)
- Atropos/RL trajectory hooks (overlap with Hermes Agent, different product)
