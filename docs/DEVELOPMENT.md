# Development workflow

Contributor workflow for routectl: the verification gate, debug
runbooks, and the worked examples for adding a new model or egress
provider. For repo layout see [ARCHITECTURE.md](ARCHITECTURE.md);
for TOML configuration see [CONFIGURATION.md](CONFIGURATION.md).

## Verification gate

Every change must keep two things green:

```bash
# Unit + integration tests across the whole workspace.
cargo test --workspace --features bedrock --release

# Some context-management integration tests are gated on
# `#[cfg(feature = "test-utils")]` to keep the production API surface
# clean. To include them:
cargo test --workspace --features bedrock,test-utils --release

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
   `ROUTECTL_LOG=routectl_providers=debug` or by running the
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
     `routectl-cli/src/ingress/anthropic/parse.rs::translate_request`.
   - Content-block translation (canonical -> Anthropic wire):
     `routectl-providers/src/anthropic_api/request.rs::translate_content_part`.
   - Missing wire field on the response side: extend
     `routectl-providers/src/anthropic_api/types.rs::AnthropicResponse`
     and `walk_content_blocks`.
   - SSE event ordering (e.g. Anthropic emits a new event type):
     `routectl-cli/src/ingress/anthropic/stream.rs::render_chunk_internal`
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
   ROUTECTL_LOG=routectl_providers=debug cargo test -p routectl-cli \
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

## Adding a new egress provider

A new egress implements `routectl_core::Provider` in
`routectl-providers`. Beyond the body traces, it MUST wire dir-2
(outgoing) and dir-3 (upstream -- on BOTH the `complete()` AND
`stream()` paths) header tracing via
`routectl_providers::header_trace::{outgoing, upstream}`; both helpers
are gated on `ROUTECTL_TRACE_HEADERS` (build nothing when off) and the
fixture capture in `scripts/capture_fixtures.sh` depends on dir-3
firing on the stream path too.

## Adding a replay fixture from a real session

The replay harness (`crates/routectl-cli/tests/replay_egress.rs` +
`replay_ingress.rs`) drives wire-shape regression tests off
hand-curated fixtures under `crates/routectl-cli/tests/fixtures/canon/`.
For the per-fixture directory layout, the `meta.json` schema, and the
full redaction policy, see [REPLAY-FIXTURES.md](REPLAY-FIXTURES.md).
The recipe below walks the day-to-day capture flow.

1. **Run routectl with body + header tracing** so the request you want
   to capture lands in the log:

   ```
   ROUTECTL_LOG=routectl=info,routectl_core::log_safe=trace
   ROUTECTL_TRACE_HEADERS=1
   ROUTECTL_TRACE_BODY_BYTES=2097152
   ```

   The default service env file at `~/.config/routectl/routectl.env`
   documents these flags. They are commented out by default; uncomment
   them for the duration of a capture session.

2. **Reproduce the request through routectl.** The daemon writes
   TRACE lines to stdout (captured by systemd journal under
   `routectl` for service installs, or directly to your terminal for a
   foreground `routectl serve` run).

3. **Bridge the journal to a flat log** if you are running under
   systemd:

   ```
   journalctl --user -u routectl --since "10 minutes ago" \
     --no-pager -o cat \
     | sed 's/\x1b\[[0-9;]*[a-zA-Z]//g' \
     > /tmp/routectl-trace.log
   ```

   The `sed` filter strips the ANSI color codes that the
   tracing-subscriber emits.

4. **Run the capture script:**

   ```
   scripts/capture_fixtures.sh --log /tmp/routectl-trace.log --limit 4
   ```

   Fixtures land under
   `crates/routectl-cli/tests/fixtures/captured/<request_id>/`.
   That directory is gitignored by design -- raw headers carry auth
   tokens and the bodies may carry personal or internal info.

5. **Sanitize.** For each fixture you want to keep:

   - Inspect `meta.json`. Confirm `provider_kind`, `stream`, alias,
     and finish_reason match the scenario you intended to capture.
     Keep small, interesting cases.
   - `provider_kind` is the in-code `PROVIDER_KIND` constant from
     the relevant egress -- in particular `"anthropic"` (not
     `"anthropic-api"`) for the api.anthropic.com client. The
     capture rig writes it verbatim.
   - Phase one replay drivers DO NOT yet exercise stream fixtures
     end-to-end (the capture rig writes empty stream bodies and
     `replay_ingress` skips them; `replay_egress` would also see
     drift on the body `stream` key). Prefer non-stream fixtures
     for the seed corpus. NOTE: revisit this caveat once
     stream-replay support lands.
   - Open every `*.headers.json`. Replace the value of every
     `Authorization`, `x-api-key`, `x-amz-*`, `anthropic-api-key`,
     `proxy-authorization`, `cookie`, and `set-cookie` header with
     the literal string `<REDACTED>`.
   - Open `ingress_request.json` and `outgoing_request.json`. Replace
     every prompt-content text with a deterministic stub
     (e.g. `reply with: pong`). Replace any system-prompt /
     assistant-history that contains personal or session info.
   - Open `upstream_response.json` and `egress_response.json` (when
     present). Replace response text with the matching stub.
   - Add `"router_overlay": false` to `meta.json` (phase-one hard
     requirement).
   - Forward-compat scenarios may set
     `"expected_unknown_block_count": <n>` so `replay_ingress.rs`
     can later assert event counts.

6. **Move the sanitized directory** to
   `crates/routectl-cli/tests/fixtures/canon/<scenario_name>/`. Use a
   meaningful name (e.g. `anthropic_api_stream_basic`,
   `anthropic_api_complete_tools`).

7. **Run gitleaks:**

   ```
   gitleaks detect --config .gitleaks.toml \
     --source crates/routectl-cli/tests/fixtures/canon
   ```

   If anything is reported, scrub it before continuing.

8. **Hand-review the diff** (`git diff`). Confirm zero auth tokens,
   zero personal info, zero internal references.

9. **Commit and exercise the new fixture:**

   ```
   cargo test -p routectl-cli --release --test replay_egress -- --nocapture
   cargo test -p routectl-cli --release --test replay_ingress -- --nocapture
   ```

   `--nocapture` surfaces the `[replay_*]` summary plus per-fixture
   skip reasons on stderr; without it cargo swallows them and you
   only see the asserted/skipped/failed counts when something blows
   up.

## Style notes

- ASCII-only in code, comments, and commit messages. No em-dashes,
  curly quotes, emoji, or arrows. `--`, `->`, straight quotes.
- Keep functions under 50 lines, files under 800.
- Prefer one file per dialect / one row per quirk. The matrix proves
  the wiring; tight files keep edits surgical.
- Don't add backwards-compatibility shims. If a schema changes,
  change it; the live matrix catches regressions.
