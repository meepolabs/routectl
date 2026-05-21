# Development workflow

This document covers the contributor workflow for routectl: the
verification gate every change must clear, the debug runbooks for the
two most common failure modes (Anthropic ingress regressions and live
matrix breakage), the worked example for adding a new model, and the
code style rules. For repo layout and the hub-and-spoke architecture
see `ARCHITECTURE.md`; for TOML configuration see `CONFIGURATION.md`;
for logging recipes see `LOGGING.md`; for known wire-shape gotchas
see `WIRE-GOTCHAS.md`.

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

## When the Anthropic ingress breaks (a real client sending a real body)

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
     `anthropic-claude-v1` format tag verbatim). When the original
     stream had a thinking block whose `signature_delta` Anthropic
     omitted (Claude 4.5 occasionally does this on tool-only thinking
     turns), routectl logs a WARN and skips that detail on replay
     -- partial echo is better than a hard 400, but is a known
     residual seam. See "Anthropic streaming reasoning replay" below.

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
