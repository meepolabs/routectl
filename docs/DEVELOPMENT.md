# Development workflow

Contributor workflow for routectl: the verification gate, debug
runbooks, and the worked examples for adding a new model or egress
provider. For repo layout see [ARCHITECTURE.md](ARCHITECTURE.md);
for TOML configuration see [CONFIGURATION.md](CONFIGURATION.md).

## Verification gate

Every change must keep all of the following green:

```bash
# Unit + integration tests across the whole workspace.
cargo test --workspace --features bedrock --release

# Some context-management integration tests are gated on
# `#[cfg(feature = "test-utils")]` to keep the production API surface
# clean. To include them:
cargo test --workspace --features bedrock,test-utils --release

# The two catalog-codegen tests -- the selectors/snapshot flag weld and
# the `catalog_baked.rs` drift guard -- are `#[cfg(feature =
# "gen-catalog")]`, so they compile out of every command above. This leg
# is what runs them; it is in both the pre-commit hook and CI.
cargo test -p routectl-router --features gen-catalog --lib

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
# hardcodes the bedrock provider on its routectl-providers dependency,
# and Cargo feature unification then re-enables bedrock (and the AWS
# SDK) for the whole graph. The routectl CLI itself declares no
# provider-gating features -- it ships every provider by design.
# This providers-scoped check is what the pre-commit hook runs.
cargo check -p routectl-providers --no-default-features \
  --features openai-compat,anthropic-api

# Rustdoc lints, denied. Catches broken intra-doc links and public docs
# that link PRIVATE items -- such a link resolves locally but renders as
# dead text in the published docs. Deliberately NOT run with
# --document-private-items: that flag makes private targets resolvable,
# which would green-light exactly the links this gate rejects.
RUSTDOCFLAGS="-D rustdoc::all" cargo doc --workspace --all-features --no-deps
```

The workspace has seven crates: `routectl-core`, `routectl-auth`,
`routectl-providers`, `routectl-router`, `routectl-usage` (SQLite
per-request usage accounting + the `routectl usage` CLI subcommand),
`routectl-cli`, and `routectl-testkit` (dev-only shared test
doubles). All seven are covered by the `--workspace` test
commands above.

The live matrix is slow (~30s) and costs cents per run. Use it as a
final gate, not a tight inner loop.

## Git hooks (one-time setup)

The repo ships its hooks in `tools/git-hooks/`. Install them once per
clone:

```bash
bash tools/git-hooks/install.sh
```

This symlinks `pre-commit` and `commit-msg` into `.git/hooks/`. The
full detail of every pre-commit leg -- exact commands, flags, and
skip/fail conditions -- lives in
[`tools/git-hooks/pre-commit`](../tools/git-hooks/pre-commit) itself,
each one named by its own `echo` as the hook runs; treat the list below
as a same-order summary, not a substitute for reading the script. In
order: a toolchain preflight, the gitleaks staged-secret scan, an
internal-identifier scan, a log-display scan (flags `%`-rendered wire
data in tracing fields), `cargo fmt --check`, a separate leg that runs
rustfmt on `include!`d fragments `cargo fmt` never opens, `cargo
clippy`, a lean providers-only `cargo check`, a local-only
public-api-baseline check that skips with a warning instead of blocking
when `cargo-public-api` or its pinned nightly isn't installed, `cargo
doc` with rustdoc lints denied, the `--workspace` test suite CI also
runs (except the two replay suites, which need a contributor's local
fixture corpus and so only run unfiltered in CI), and a `gen-catalog`-
gated router-test leg no other leg compiles. The `commit-msg` hook
applies the same identifier scan to the commit message, plus a
subject-line length check. Set `ROUTECTL_SKIP_PRECOMMIT=1` to bypass
the pre-commit gate while iterating; CI enforces the same rules
fail-closed regardless.

Every gate leg invokes `cargo` and `rustfmt` bare and relies on the
rustup shim to honor the exact-patch pin in `rust-toolchain.toml`.
`scripts/assert-toolchain.sh` verifies that reliance held before any leg
runs: it asserts the effective `rustc` reports the pinned version and
that `rustfmt` comes from the same toolchain build, so an exported
`RUSTUP_TOOLCHAIN`, a stale `rustup override`, or a system toolchain
ahead of the shim on `PATH` fails the hook instead of silently changing
which compiler the gates cleared against. It is a version check, not a
rustup check -- a toolchain that IS the pinned version passes however it
was installed. It runs from the pre-commit hook and from
`scripts/fmt-fragments.sh` (which is also invoked standalone); CI's rust
jobs select their toolchain through the setup action and so do not need
it, but CI does run its self-test:

```bash
bash scripts/assert-toolchain.sh
bash scripts/assert-toolchain.test.sh
```

`cargo fmt` walks the module tree, so it never opens a file pulled in by
`include!` -- the fmt gate passes vacuously on those fragments, and this
repo uses them to keep large test modules under the file-size ceiling.
`scripts/fmt-fragments.sh` closes that hole: it resolves every
`include!` call site and runs `rustfmt --edition 2024 --check` on each
target. It runs as its own pre-commit and CI step, and takes no
arguments:

```bash
bash scripts/fmt-fragments.sh
```

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
     `routectl-providers/src/anthropic_api/messages.rs::translate_content_part`.
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
   `crates/routectl-cli/tests/live_matrix/openai_compat.rs`. The const
   arrays are `OPENROUTER_MODELS`, `OPENCODE_GO_MODELS`, `NIM_MODELS`.
   One string per row; the test harness handles the rest.

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
   `crates/routectl-cli/tests/live_matrix/openai_compat.rs`:
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

   If you run routectl as a service, put these in the service's env
   file; for a foreground run, export them in the shell.

2. **Restart the daemon.** Send traffic through it via your normal
   clients (claude-code, codex, custom scripts, etc.). The capture rig
   only sees completed requests, so let some real exchanges flow.

3. **Point the capture script at a flat trace log written by a FILE
   SINK.** The script reads from a file path, and that file must be one
   the daemon's stderr was redirected into:

   ```
   routectl serve 2>/tmp/routectl-trace.log
   ```

   **The journal is not an option for capture.** Do not bridge
   `journalctl` output into the log the rig reads. journald's default
   `LineMax` (~48 KiB) silently truncates a long trace line wherever the
   cut lands -- in practice mid JSON-string-escape -- and the daemon's own
   `... [truncated at N bytes]` marker is never emitted, so the rig writes
   a malformed body to disk with nothing signalling the damage. Draining a
   multi-hour session through the journal also stalls on long streams.
   A corpus captured that way is unrecoverable: the bytes are gone, not
   reorderable.

   This is why `scripts/capture_driver.sh` redirects
   `routectl serve --port <port> 2> "$RUN/trace.log"` -- that redirect IS
   the driver runner's capture sink, and the driven daemon is not a service
   unit, so nothing truncates or journals it.

4. **Run the capture script:**

   ```
   scripts/capture_fixtures.sh --log /tmp/routectl-trace.log
   ```

   Fixtures land under
   `crates/routectl-cli/tests/fixtures/captured/<request_id>/`. The
   directory is gitignored: never commit it.

   Before promoting each fixture the rig runs
   `scripts/scrub-fixture.sh --write` over it, which rewrites your own
   home path (both `$HOME/...` and the dash-encoded `-home-...` form
   that appears in `.claude/projects/` dir names) to a neutral
   `/home/user` placeholder, and replaces the VALUE of every
   credential-shaped header with a redaction placeholder while keeping
   the header NAME. Auth redaction happens at write time on purpose: a
   capture against an OAuth lane records a live bearer token, and a
   corpus that ever held one is unpublishable no matter what a later
   scan says.

   Everything the write pass cannot safely rewrite is REFUSED rather
   than guessed at. Run the gate over a fresh capture:

   ```
   scripts/scrub-fixture.sh --check \
     crates/routectl-cli/tests/fixtures/captured/<request_id>
   ```

   It exits non-zero and names the pattern class (never the matched
   value) when a fixture still carries your git author name or email, a
   third party's `/home/<name>` prefix (plain or dash-encoded), your
   hostname, an `ls -l` / `ls -l@` / `ls -o` owner column naming a real
   account, an unredacted credential header, a `bearer <token>` value,
   or a raw vendor key (`sk-ant-api03-...`, `ghp_...`, `AKIA...`). The
   last two scan raw bytes anywhere in the fixture, not just header
   files: a body that captured a `cat .env` or a
   `~/.claude/.credentials.json` transcript carries a live credential
   with no header structure to key on.

   There is no automatic rewrite for those: remove the content by hand,
   or recapture the request without it. The deny set is derived at
   runtime from `$HOME`, `git config user.name` / `user.email`, and the
   hostname, so it works on any contributor's machine; a value the gate
   cannot read prints a warning naming the dropped class rather than
   silently narrowing.

5. **Run the replay tests against the local corpus:**

   ```
   cargo test -p routectl-cli --release --test replay_egress -- --nocapture
   cargo test -p routectl-cli --release --test replay_ingress -- --nocapture
   ```

   `--nocapture` surfaces the `[replay_*]` summary plus per-fixture
   skip reasons on stderr; without it cargo swallows them and you
   only see the asserted/skipped/failed counts when something blows
   up.

   Both of these legs are `--skip`ped by the pre-commit hook, which
   states why at the call site. They drive real routectl code, so a
   fixture the replay path cannot yet reproduce (an unresolved model
   alias, a stale capture predating an invariant) fails rather than
   skips. The leg that adjudicates the whole corpus today is
   conservation, which re-runs no routectl code at all:

   ```
   cargo test -p routectl-cli --release --test conservation -- --nocapture
   ```

   It compares each fixture's captured ingress body against its captured
   outgoing body through the lane class and the exception table, and
   prints one bounded line per lane plus a `PASS|FAIL|DEGRADED` verdict.
   Run it with `--nocapture` always: over an empty corpus it passes while
   proving nothing, and the verdict line is what tells the two apart.

The replay corpus is per-contributor and ephemeral. Recapture freely
when routectl's wire output changes. The harness and the capture
script are the shared contract; the corpus is yours.

### Driver-mode capture

The recipe above is the LIVE-BOX mode: a trace drained from your own real
session, where the rig cannot observe which scenario produced the request,
which config was in force, or how the client connected. That mode stays
tolerant of all three being unset, because an empty pin is honest there.

`scripts/capture_fixtures.sh --driver-mode` is the other mode, for a
hermetic capture where a driver KNOWS all three. It changes four things:

- **All three pins become mandatory.** `ROUTECTL_FIXTURE_CASE_ID`,
  `ROUTECTL_FIXTURE_CONFIG_SHA`, and `ROUTECTL_FIXTURE_CONNECTION_MODE`
  must be set; an unset one aborts the run naming the variable. An empty
  case id would collapse every case in the lane onto one landing
  directory and the corpus would quietly overwrite itself.
- **Landing keys on `(lane, case_id)`**, at `<out>/<lane>/<case_id>/`
  rather than `<out>/<request_id>/`, so a rerun of the same case produces
  a DIFF instead of a fresh sibling. The rerun replaces the previous
  directory wholesale. Keep case ids neutral scenario names
  (`tools-multiturn-01`) and never derive one from the environment: the
  scrub check below reads `meta.json` too, so a hostname or a real path
  in a case id refuses the fixture.
- **A missing structural summary on either request-side direction fails
  that fixture** rather than warning. A capture with half its structural
  evidence is not a canonical fixture.
- **`scrub-fixture.sh --check` runs after `--write`** and a non-zero exit
  refuses the promotion, so a driver fixture is canonical by construction
  or it is not landed.

Any refusal exits non-zero, which is the signal a runner reads as "this
case produced no fixture". The full policy split is in the script header
(`scripts/capture_fixtures.sh --help`); the on-disk layout is in
[REPLAY-FIXTURES.md](REPLAY-FIXTURES.md).

### The hermetic driver runner

`scripts/capture_driver.sh` is what actually produces a driver-mode
capture. It boots a hermetic routectl, hands the daemon to a driver
command, and feeds the resulting trace to the rig:

```
scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
  -- scripts/drivers/<driver-script> [args...]
```

What each run does, in order:

1. Builds a throwaway workspace: a fresh `HOME` (empty), a fresh cwd that
   is a git repo with a SYNTHETIC author (`Fixture Driver
   <driver@fixtures.invalid>`), and a fresh `XDG_CONFIG_HOME` carrying
   `routectl/config.toml` copied from the lane's committed config. A
   driven client runs tools and reads their output back into its own
   request bodies, so hermeticity is what keeps anything personal out of
   a fixture in the first place -- the scrub gate is the proof half of
   the same story, not a substitute for it.
2. Picks a free port AFTER probing `ss -ltn`, then starts
   `routectl serve --port <port> 2> "$RUN/trace.log"`. That redirect IS
   the capture sink: `init_tracing` writes to stderr and the driven
   daemon is not a service unit, so nothing truncates or journals it.
3. Polls `/health` as a PRECONDITION, requiring both that the pid it
   captured is alive and that the endpoint answers -- an occupied port
   leaves someone else's listener answering, so "something responds" is
   not proof. A daemon that never comes up aborts the run (exit 3) after
   printing the tail of the trace.
4. Runs the driver command with cwd in the throwaway repo and the base
   URL plus all three fixture pins exported. `--help` lists the exact
   variable names -- that block is the contract a driver script codes
   against.
5. Stops the daemon by the pid it captured from its own `$!` (never by
   name, and never under `setsid`, which would capture the wrapper), then
   runs the rig in `--driver-mode` against the trace, landing under
   `crates/routectl-cli/tests/fixtures/driver/<lane>/<case-id>/`.

A cleanup trap runs on every exit path, so an interrupted or failed run
never leaves a daemon holding its port. Exit codes: 2 usage, 3 unhealthy
daemon, 4 driver failure, 5 rig refusal, 6 no free port.

`meta.config_sha` is the sha256 of the COMMITTED lane config
(`scripts/drivers/config/<lane>.toml`), not of the copy the run boots
from. The port arrives on the serve command line rather than as a rewrite
of that file precisely so the hashed identity stays stable: a per-run sha
could not distinguish config drift from a fresh run, which is the one
question the field exists to answer.

`scripts/capture_driver.test.sh` covers the runner against a STUB daemon
(`ROUTECTL_BIN` selects the binary) -- a real boot needs a credential and
CI has none.

### The canonical interaction set

What a driver run captures is one CASE: a file under
`scripts/drivers/cases/`, `<case_id>.json`, describing one interaction.
The set covers wire PATTERNS rather than models -- multi-turn tool loops,
cache breakpoints, thinking, large contexts, plus a plain-turn baseline to
diff the others against. A case file names no binary and no flag: mapping
it onto a client's argv is a driver's job, which is what lets the same case
be replayed through every drivable harness.

The schema is documented in one place,
[../scripts/drivers/cases/README.md](../scripts/drivers/cases/README.md),
and enforced in one place,
`scripts/drivers/lib/validate_case.py` -- the drivers read their case
through it on every run, so a malformed case fails before a client opens a
session:

```
python3 scripts/drivers/lib/validate_case.py --check scripts/drivers/cases/thinking-01.json
```

### The harness drivers

One FILE per harness under `scripts/drivers/`, never a dispatch statement:
a harness this box cannot drive has NO file, because a dead branch still
reads as coverage and a harness whose name cannot be committed could not be
driven at all if the enumeration lived in tracked content. Shared behavior
(reading the case, seeding the throwaway cwd, the daemon precondition, the
client-version read) lives in `scripts/drivers/lib/common.sh`.

| Driver | Client | Selected by |
|---|---|---|
| `claude-code.sh` | interactive Claude Code | `ROUTECTL_DRIVER_CLAUDE_BIN`, default `claude` |
| `claude-code-print.sh` | `claude -p` / Agent SDK print mode | same variable; the same binary, a different wire shape |
| `external-agent-cli.sh` | any third-party Anthropic-dialect CLI | `ROUTECTL_DRIVER_AGENT_BIN`, REQUIRED, no default |

### Running a case

```
scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
  -- scripts/drivers/claude-code-print.sh
```

That lands `crates/routectl-cli/tests/fixtures/driver/anthropic-api/tools-multiturn-01/`.
Add `--keep` to retain the run workspace, which holds the trace, the
client's own output, and `client.txt` (the version the driver read from the
binary at run time). A rerun of the same case re-lands on the same path and
produces a diff.

The MITM mode needs its two carriers, and an unset one is a refusal rather
than a fallback to `base-url`:

```
ROUTECTL_DRIVER_PROXY_URL=http://127.0.0.1:8443 \
ROUTECTL_DRIVER_PROXY_CA=$XDG_CONFIG_HOME/routectl/mitm-certs/ca.pem \
scripts/capture_driver.sh --lane anthropic-api --case thinking-01 \
  --connection-mode front-proxy -- scripts/drivers/claude-code.sh
```

Both modes matter because they emit different wire shapes: a front proxy
carries `role:"system"` turns inside `messages[]` while `base-url` inlines
the same content as system-reminder text and sends zero system turns. A
silent fallback would land a fixture labelled `front-proxy` whose shape is
`base-url`, and every later cross-mode diff would read as client drift.

Driving the third harness supplies its binary and the flags it answers to;
the driver's header lists them. A multi-turn case through it requires
`ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG`, because N independent one-shots
would land a fixture labelled multi-turn whose trace holds N first turns.

```
ROUTECTL_DRIVER_AGENT_BIN=<binary> \
ROUTECTL_DRIVER_AGENT_CONTINUE_FLAG=--continue \
ROUTECTL_DRIVER_AGENT_MODEL_FLAG=-m \
scripts/capture_driver.sh --lane anthropic-api --case thinking-01 \
  -- scripts/drivers/external-agent-cli.sh
```

### A driver corpus is a snapshot of a client VERSION

`meta.client.version` is the decay clock. Claude Code auto-updated 2.1.169
-> 2.1.245 across one restart and changed its request shape mid-week; every
driver reads the version from the binary at run time and fails the run when
the binary cannot state it, because a fixture with no version cannot say
which client shape it pins. Case keying is what converts that decay from
silent rot into a visible diff.

`scripts/drivers.test.sh` covers the case set and every driver against a
stub daemon AND a stub client, injected through the same binary overrides
listed above -- a real run needs a credential and spends tokens.


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
