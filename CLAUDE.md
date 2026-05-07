# Working on routectl with Claude

This file is a runbook for autonomous agents (Claude Code, qwen via
opencode-delegate, etc.) working on this repo. Read it once before
making changes; refer back when a model fails the live matrix.

## Repo map

- `crates/routectl-core/` -- `Provider` trait + OpenRouter-shape schema
  (`ChatRequest`, `ChatResponse`, `ChatChunk`, `Message`,
  `ReasoningDetail`). Wire shapes only; no provider code.
- `crates/routectl-providers/` -- concrete provider impls. Two are
  feature-on by default: `openai_compat` (covers OpenAI, OpenRouter,
  DeepSeek, vLLM, NIM, llama.cpp, OpenCode-Go, ...) and `anthropic_api`.
  Cookie-auth providers stub for v0.2.
  - `model_profile.rs` -- per-model quirks table. **Edit here when a
    model needs new behavior** (drops sampling params, requires
    reasoning effort, etc.).
  - `openai_compat/dialects/*.rs` -- one file per reasoning dialect.
    **Edit here when a new wire format appears**.
- `crates/routectl-router/` -- alias resolution, fallback chain, retry
  policy, provider factory.
- `crates/routectl-auth/` -- `SecretStore` trait, OS keychain backend,
  in-memory test store, secret-ref URI parser.
- `crates/routectl-cli/` -- axum HTTP server, clap subcommands
  (serve/test/config/login), live matrix integration tests.

## Verification gate

Every change must keep two things green:

```bash
# Unit + integration tests, ~140 of them.
cargo test --workspace --release

# Live matrix against real providers. Requires OPENROUTER_API_KEY,
# OPENCODE_GO_API_KEY, NIM_API_KEY in env (skips per-provider when
# missing). 5/5 tests must pass; per-provider PASS counts must match
# the baseline in docs/TESTED_MODELS.md.
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1
```

The live matrix is slow (~30s) and costs cents per run. Use it as a
final gate, not a tight inner loop.

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

- **Post-`[DONE]` SSE trailers**. OpenCode-Go emits a
  `data: {"choices":[],"cost":"0"}` cost-tracker chunk after the
  `[DONE]` terminator. Handled by an explicit `return` in the SSE
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
