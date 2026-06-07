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

# Live matrix against real providers. Each provider's tests skip
# cleanly when their env key is absent, so set keys for whatever you
# want to exercise:
#   OPENROUTER_API_KEY / OPENCODE_GO_API_KEY / NIM_API_KEY
#                                  -- openai-compat matrix (5 tests)
#   AWS_BEARER_TOKEN_BEDROCK (+ AWS_REGION)
#                                  -- bedrock invoke + converse (7 tests)
#   OPENAI_BEARER_KEY / OPENAI_ACCOUNT_ID
#                                  -- openai-responses (2 tests)
# There is no single absolute pass count: only the providers whose keys
# are present run. Match the per-provider PASS rows against the baseline
# in docs/TESTED_MODELS.md -- a missing key SKIPS that provider's tests
# rather than failing them.
cargo test -p routectl-cli --features live-integration --release \
  --test live_matrix -- --nocapture --test-threads=1

# Lean build for downstream library consumers who don't want the
# AWS dependency tree. Scoped to the providers library: a full
# --workspace build can never be AWS-free because routectl-cli
# always links the bedrock feature, and Cargo feature unification
# then re-enables bedrock (and the AWS SDK) for the whole graph.
# This providers-scoped check is what the pre-commit hook runs.
cargo check -p routectl-providers --no-default-features \
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
`replay_ingress.rs`) drives wire-shape regression tests off captures
under `crates/routectl-cli/tests/fixtures/captured/`. That directory
is gitignored: each contributor maintains their own corpus locally,
relevant to their own development and regression-testing needs. The
repo ships the harness and the capture script; the corpus is yours.

For the per-fixture directory layout and the `meta.json` schema, see
[REPLAY-FIXTURES.md](REPLAY-FIXTURES.md). The recipe below walks the
day-to-day capture flow.

1. **Enable TRACE knobs in your routectl env file** (or for a
   foreground `routectl serve` run):

   ```
   ROUTECTL_LOG=routectl=info,routectl_core::log_safe=trace
   ROUTECTL_TRACE_HEADERS=1
   ROUTECTL_TRACE_BODY_BYTES=2097152
   ```

   The default service env at `~/.config/routectl/routectl.env`
   documents these (commented by default).

2. **Restart the daemon.** Send traffic through it via your normal
   clients (claude-code, codex, custom scripts, etc.). The capture rig
   only sees completed requests, so let some real exchanges flow.

3. **Bridge the systemd journal to a flat trace log** (the capture
   script reads from a file path; the daemon writes to journal):

   ```
   journalctl --user -u routectl --since "10 minutes ago" --no-pager -o cat \
     | sed 's/\x1b\[[0-9;]*[a-zA-Z]//g' \
     > /tmp/routectl-trace.log
   ```

   The `sed` strips ANSI color codes that the tracing-subscriber
   emits.

4. **Run the capture script:**

   ```
   scripts/capture_fixtures.sh --log /tmp/routectl-trace.log
   ```

   Fixtures land under
   `crates/routectl-cli/tests/fixtures/captured/<request_id>/`. The
   directory is gitignored: never commit it.

5. **Run the replay tests against the local corpus:**

   ```
   cargo test -p routectl-cli --release --test replay_egress -- --nocapture
   cargo test -p routectl-cli --release --test replay_ingress -- --nocapture
   ```

   `--nocapture` surfaces the `[replay_*]` summary plus per-fixture
   skip reasons on stderr; without it cargo swallows them and you
   only see the asserted/skipped/failed counts when something blows
   up.

The replay corpus is per-contributor and ephemeral. Recapture freely
when routectl's wire output changes. The harness and the capture
script are the shared contract; the corpus is yours.

## Style notes

- ASCII-only in code, comments, and commit messages. No em-dashes,
  curly quotes, emoji, or arrows. `--`, `->`, straight quotes.
- Keep functions under 50 lines, files under 800.
- Prefer one file per dialect / one row per quirk. The matrix proves
  the wiring; tight files keep edits surgical.
- Don't add backwards-compatibility shims. If a schema changes,
  change it; the live matrix catches regressions.
- Inline `#[cfg(test)] mod tests` for unit tests; `tests/*.rs` for
  integration tests that need the crate's public API or external
  services.
- Extract a sidecar test file (`*_tests.rs` imported from the parent)
  when inline tests exceed ~200 LOC.
- Feature-gate tests that depend on optional features with
  `#[cfg(feature = "X")]` at the module level, not per-function.

## Provider internal convention

Every provider follows the same seam layout:

| Seam | Name | File |
|---|---|---|
| Request entry (trait) | `normalize_request` | `mod.rs` |
| Per-shape request mappers | `translate_*` | `request.rs` |
| Response entry (trait) | `normalize_response` | `mod.rs` |
| Per-shape response mappers | `translate_*` | `response.rs` |
| SSE decoder entry | `parse_event` | `sse.rs` |
| Auth/signing attach | `apply` | `auth.rs` or `signing.rs` |
| Header construction (returns HeaderMap) | `build_headers` | `mod.rs` |

Two naming layers:
- Contract layer (trait + error variants): `normalize_*`
- Structural layer (per-shape canonical<->wire): `translate_*`

Providers with two API surfaces (e.g. Bedrock invoke/converse): each
sub-module exposes the same seam names; the parent `mod.rs` dispatches.
Dialects (e.g. openai_compat DeepSeek/vLLM reasoning quirks) are a
separate `Dialect` strategy trait, not a request/response fork.
