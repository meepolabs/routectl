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
# is what runs them; CI runs it too. It is NOT in the local commit gate,
# which carries no test legs.
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
# This providers-scoped check is what the commit gate runs.
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

## Commit gate (one-time setup)

The local commit gate is a [pre-commit](https://pre-commit.com) config,
[`.pre-commit-config.yaml`](../.pre-commit-config.yaml). Install it once
per clone:

```bash
bash scripts/bootstrap.sh
```

That installs the `pre-commit`, `commit-msg` and `pre-push` stages and
pre-builds each hook's environment. Installing from a linked worktree
writes the clone's shared hooks directory, so one run covers every
worktree. Re-run it after any change to the stage list, since a stage
added to the config is not installed until something installs it.

A clone that previously used the retired hand-rolled hooks will also
carry `.git/hooks/*.legacy` files: pre-commit preserves whatever hook it
displaced. They point at scripts that no longer exist and nothing
invokes them, so they are inert -- delete them or ignore them.

The full detail of every leg -- exact command, flags, and skip/fail
conditions -- lives in the config itself, each leg named as it runs;
treat the list below as a same-order summary, not a substitute for
reading it. In order: a toolchain preflight, the gitleaks staged-secret
scan (its `rev` pins the binary, in lockstep with `GITLEAKS_VERSION` in
`.github/workflows/gitleaks.yml`), an internal-identifier scan, a
log-display scan (flags `%`-rendered wire data in tracing fields),
`cargo fmt --check`, a separate leg that runs rustfmt on `include!`d
fragments `cargo fmt` never opens, `cargo clippy`, a lean
providers-only `cargo check`, a public-api-baseline check that skips
with a warning instead of blocking when `cargo-public-api` or its
pinned nightly isn't installed (CI runs the same check
unconditionally, so the local leg is an early warning rather than the
guarantee), and `cargo doc` with rustdoc
lints denied. The `commit-msg` stage applies the same identifier scan to
the commit message, plus a subject-line length check.

`cargo test` is deliberately not a COMMIT-stage leg: it was 275 of the
305 seconds a commit used to take, and a multi-minute release suite on
every commit is what drives people to bypass the gate wholesale, taking
the fast legs with it. It runs at the `pre-push` stage instead, once
per push rather than once per commit -- the same protection against
pushing a broken branch, at a fraction of the cost.

The three stages divide by a single rule: a check belongs at the
earliest stage where it is cheap and the latest stage where it is
authoritative. The secret scan is the clearest case for the commit
stage and stays there, because catching a secret after it is pushed is
already too late -- it is in history and needs rotating. The test suite
is authoritative about a branch, not about one commit, so it sits at
pre-push. Everything with no local counterpart -- the advisory and
licence scans, the public-API baseline -- is authoritative in CI.

One thing the pre-push stage does NOT do: it does not prevent a
non-bisectable individual commit. It runs against the branch tip, so a
mid-branch commit can be broken while the tip is green. Nothing here
closes that gap.


Every leg runs against the STAGED tree, not the working tree --
pre-commit stashes unstaged changes for the duration and restores them
afterwards. A partially staged commit is therefore checked on exactly
the code it contains. To skip one leg for one commit:
`SKIP=<hook-id> git commit`; the ids are in the config.

Every gate leg invokes `cargo` and `rustfmt` bare and relies on the
rustup shim to honor the exact-patch pin in `rust-toolchain.toml`.
`scripts/assert-toolchain.sh` verifies that reliance held before any leg
runs: it asserts the effective `rustc` reports the pinned version and
that `rustfmt` comes from the same toolchain build, so an exported
`RUSTUP_TOOLCHAIN`, a stale `rustup override`, or a system toolchain
ahead of the shim on `PATH` fails the hook instead of silently changing
which compiler the gates cleared against. It is a version check, not a
rustup check -- a toolchain that IS the pinned version passes however it
was installed. It runs as the commit gate's first leg and from
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
target. It runs as its own commit-gate leg and CI step, and takes no
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

   Both of these legs are known-red against a fixture corpus the
   replay path cannot yet fully reproduce -- see
   [REPLAY-FIXTURES.md](REPLAY-FIXTURES.md) for the per-class
   breakdown. They drive real routectl code, so a
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
which config was in force, how the client connected, or which wire shape
the scenario was meant to exercise. That mode stays tolerant of all four
being unset, because an empty pin is honest there.

`scripts/capture_fixtures.sh --driver-mode` is the other mode, for a
hermetic capture where a driver KNOWS every pin. It changes six things:

- **All five pins become mandatory.** `ROUTECTL_FIXTURE_CASE_ID`,
  `ROUTECTL_FIXTURE_CONFIG_SHA`, `ROUTECTL_FIXTURE_CONNECTION_MODE`,
  `ROUTECTL_FIXTURE_WIRE_PATTERN`, and
  `ROUTECTL_FIXTURE_EXPECTED_INGRESS` must be set; an unset one aborts the
  run naming the variable. An empty case id would collapse every case in
  the lane onto one landing directory and the corpus would quietly
  overwrite itself, and an empty wire pattern would land a fixture whose
  coverage claim nothing downstream can tell from a claim nobody recorded.

  **There is no `--wire-pattern` flag, and looking for one is the wrong
  move.** The runner DERIVES that pin from the case file, by reading
  `wire_pattern` out of `scripts/drivers/cases/<case-id>.json` through
  `scripts/drivers/lib/validate_case.py --field wire_pattern`. A flag
  would let a caller declare a pattern the case does not claim, which is
  the coverage lie one layer earlier than the fixture. If you drive the
  rig directly rather than through `scripts/capture_driver.sh`, you export
  the variable yourself; if you go through the runner, it is already set
  for you. Read the value the same way the runner does:

  ```
  python3 scripts/drivers/lib/validate_case.py --field wire_pattern \
    scripts/drivers/cases/plain-turn-01.json
  ```

  `ROUTECTL_FIXTURE_EXPECTED_INGRESS` is the exception that DOES arrive on
  argv, as the runner's `--expected-ingress`. Nothing the runner reads
  names the value: a lane config declares the EGRESS provider and says
  nothing about the client's inbound dialect (a translation lane exists so
  the two differ), and a case describes a dialect-agnostic interaction on
  purpose. The pin is a property of the (driver, lane) pairing the caller
  chose, so the caller supplies it -- with no default, because a default
  of `anthropic` would be silently wrong for exactly the non-Anthropic
  client the gate below exists to catch.

- **The expected ingress dialect is enforced against the TRACED one.**
  `meta.ingress_kind` is parsed out of the daemon's own trace, so it says
  which adapter really handled the request; the pin says which one the run
  was set up to reach. A client that accepts the runner's connection
  carriers and then talks its own dialect anyway lands a fixture that is
  evidence for the wrong dialect, and no environment check can see that --
  the environment recorded the intent faithfully. A disagreement refuses
  the promotion. The dialect vocabulary lives in
  `scripts/drivers/lib/ingress_kinds.sh` (a replica of `IngressAdapter::id()`,
  welded to it by the shell self-tests), and a pin outside it is a usage
  error rather than a guaranteed mismatch.
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
- **The lane must be CLASSIFIED by the scrub gate.** The rig asks
  `scrub-fixture.sh --lane-known <lane>` and refuses to promote on a
  non-zero answer. A `--check` pass says the fixture carries no residue of
  the credential shapes the gate knows; on a lane whose credential shape
  nobody has classified that proof is vacuous, so an unclassified lane
  fails closed. A lane the gate records as having no prefix-detectable
  shape -- with the reason written beside it, as bedrock's prefix-less AWS
  secret is -- counts as classified: that is a verdict, not ignorance. The
  table lives in `scripts/scrub-fixture.sh` and nowhere else, so adding a
  provider lane means adding its row there.

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
  --expected-ingress anthropic \
  -- scripts/drivers/<driver-script> [args...]
```

`--expected-ingress` is required and names the dialect the driven client is
expected to reach routectl on. It is validated against
`scripts/drivers/lib/ingress_kinds.sh` before any daemon boots and compared
against the TRACED dialect before any fixture lands.

Before any daemon boots, the run reads the LANE CONFIG and the CASE FILE.
The lane config it copies into the run's config root; the case file it reads
for the wire-pattern pin. Both are fail-closed: a lane with no committed
config, or a case whose file is missing or declares no valid `wire_pattern`,
is a usage error (exit 2) rather than a run that boots and lands an unpinned
fixture. Reading the case file at all is new -- the runner used to treat
`--case` as an opaque scalar and leave the case file entirely to the drivers.

What each run then does, in order:

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
   URL plus all four fixture pins exported. `--help` lists the exact
   variable names -- that block is the contract a driver script codes
   against.
5. Stops the daemon by the pid it captured from its own `$!` (never by
   name, and never under `setsid`, which would capture the wrapper), then
   runs the rig in `--driver-mode` against the trace, landing under
   `<out>/<lane>/<case-id>/`. With no `--out`, that is
   `.routectl-driver-scratch/<lane>/<case-id>/` at the repo root.

A cleanup trap runs on every exit path, so an interrupted or failed run
never leaves a daemon holding its port. Exit codes: 2 usage, 3 unhealthy
daemon, 4 driver failure, 5 rig refusal, 6 no free port, 7 the rig ran
clean but landed no fixture, 8 (front-proxy only) the daemon is healthy
but its MITM listener never became ready. 5 and 7 are separate because a
refusal means routectl produced a fixture the gate rejected (a defect,
never retry) while a zero landing means the case produced no completed
request (retryable). 8 is separate from 3 because MITM startup failure
is non-fatal to the daemon -- `/health` stays green while the proxied
CONNECT has nothing to hit -- so an operator debugging "healthy daemon,
dead proxy" from the unhealthy-daemon message would look at the wrong
layer.

`meta.config_sha` is the sha256 of the COMMITTED lane config
(`scripts/drivers/config/<lane>.toml`), not of the copy the run boots
from. The port arrives on the serve command line rather than as a rewrite
of that file precisely so the hashed identity stays stable: a per-run sha
could not distinguish config drift from a fresh run, which is the one
question the field exists to answer.

`scripts/capture_driver.test.sh` covers the runner against a STUB daemon
(`ROUTECTL_BIN` selects the binary) -- a real boot needs a credential and
CI has none.

### Where a driver capture lands: scratch, then promotion

A driver fixture lands in a SCRATCH tree by default and reaches the
reviewed corpus only through a second, deliberate step. Two roots, and
they are not interchangeable:

| Root | Path | What it is |
|---|---|---|
| Scratch (default `--out`) | `.routectl-driver-scratch/` at the repo root | gitignored by definition; nothing here is ever committed |
| Corpus | `crates/routectl-cli/tests/fixtures/driver/` | the reviewed destination, reachable only through the promote script |

Unlike the live-box `captured/` corpus, a driver fixture is hermetic by
construction -- no real prompts, a synthetic git identity, an empty HOME --
which is what makes it eligible for review and for gating in the first
place. Check `git status` after a promotion: the corpus is committed, so a
promoted fixture shows up as an addition staged for review.

The runner keys a landing on `(lane, case_id)`, so a rerun of a case
replaces that case's fixture wholesale. With the corpus as the default,
every exploratory rerun would therefore overwrite a reviewed fixture in
place. The scratch default is what buys you a free rerun: capture, look at
it, capture again, and nothing tracked has moved.

Two flags govern the destination:

- `--out <dir>` -- where the fixture lands. Defaults to the scratch root
  above.
- `--out-root <dir>` -- the confinement root `--out` must live under. It
  WIDENS what `--out` may name, which is why it is itself restricted to a
  closed set: the scratch root, or the value of `ROUTECTL_DRIVER_OUT_ROOT`
  when the run sets it. A root taken on trust would leave `--out` compared
  against a path the same caller chose, which confines nothing.

Both refusals are usage errors (exit 2) and both happen before a daemon
boots, so a refused run costs nothing and leaves no directory behind.
`--out-root` outside the closed set is refused as "not an allowed landing
root"; an `--out` that resolves outside the root in force is refused for
containment, including via a symlinked component anywhere along the path.
The scratch root cannot be assumed present inside a run that mounts this
repo read-only, which is why the allowed set has an environment seam at
all rather than being a single repo-relative constant.

The resume marker the rig keeps is scoped to the landing root, so a
scratch run never suppresses a later corpus recapture of the same case.

**Promotion into the corpus is `scripts/promote_fixture.sh`, never a hand
`mv`.**

```
bash scripts/promote_fixture.sh \
  --from .routectl-driver-scratch/anthropic-api/plain-turn-01 \
  --scratch-root .routectl-driver-scratch \
  --expected-ingress anthropic
```

`--from`, `--scratch-root`, and `--expected-ingress` are all required.
`--expected-ingress` is the second boundary of the ingress pin: the rig
checked it at capture time, and this check is what covers the window the
scratch root exists for -- a fixture inspected and hand-edited before it
lands. It arrives on argv rather than being read off the fixture because
`meta.ingress_kind` is the traced FACT this gate compares against, and a
fact checked against itself asserts nothing; `--scratch-root` exists
because the scratch tree can sit outside the repo entirely, so there is no
constant to derive it from, and a root guessed from `--from` would confine
nothing. `--to` selects the corpus root and defaults to
`crates/routectl-cli/tests/fixtures/driver`, confined to that default -- it
can narrow the destination but never leave the corpus. `--from` must name
a path exactly two components under the scratch root
(`<scratch-root>/<lane>/<case-id>`), because that pair IS the corpus key.

Two things the script does that a `mv` does not:

- It lands by RENAME-ASIDE-THEN-DELETE. A `mv` of one fixture directory
  over an existing one MERGES: files the previous capture wrote survive
  beside the new ones. File presence is part of the fixture schema -- an
  `upstream_response.json` present means the run was non-stream, absent
  means it streamed -- so a merged directory is a fixture no single
  capture ever produced, and every drift signal read off it is read off
  evidence that does not exist. Promoting instead stages a copy beside the
  destination, renames the old fixture aside, renames the new one into
  place, and only then deletes the old: a reader sees the whole old
  fixture or the whole new one, never a union.
- It re-runs `scrub-fixture.sh --check` on the STAGED COPY before any
  rename. The rig already checked at capture time, but a scratch fixture is
  by design hand-inspectable and hand-editable in between -- that is what
  scratch is FOR -- so this re-check is the only thing between an edited
  scratch fixture and the corpus.

Exit codes: 0 promoted, 1 the staged content failed the scrub gate
(nothing was promoted and the destination is untouched), 2 usage, a
confinement refusal, a `--from` that is not `<lane>/<case-id>` deep, or a
scrub gate that could not run at all.

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

A case DECLARES a wire pattern; whether the captured bytes exhibit it is a
separate question, answered by `scripts/drivers/lib/verify_pattern.py`. It
takes a fixture directory plus the pattern it claims and exits non-zero
with the failing clause on stderr:

```
python3 scripts/drivers/lib/verify_pattern.py \
  crates/routectl-cli/tests/fixtures/driver/anthropic-api/plain-turn-01 baseline
```

Every predicate keys on the INGRESS side only -- the case controls what the
client sends, not the provider dialect the request is translated into.
`baseline`, `thinking`, and `cache-breakpoints` read the ingress structural
summary line from `structural.txt`; `tool-use-multiturn` and
`large-context` read `ingress_request.json` (a resent tool-call / result
pair, and a body byte floor). A pattern token with no predicate
is REFUSED rather than waved through, so extending the closed vocabulary
without extending the table cannot promote an unverified fixture.

Ingress side does not mean Anthropic side. `tool-use-multiturn` runs ONE
census over every ingress dialect: it picks the turn list by which key the
captured body carries (`messages` for the Anthropic and chat-completions
shapes, `input` for the Responses shape) and matches the pair in each
dialect's own spelling -- `tool_use` / `tool_result` blocks, a `tool_calls`
array plus a `role: "tool"` turn, or `function_call` /
`function_call_output` items. The turn list is selected by PRESENCE, never
by the recorded `meta.ingress_kind`: that claim sits beside the
`wire_pattern` claim the predicate exists to check. A body carrying neither
key is refused naming both.

`baseline` is the deliberate exception: it is ANTHROPIC-ONLY, scoped off
the ingress line's own `id` token, and a claim on any other dialect is
refused by name. A non-Anthropic client's floor request carries tools its
own runtime requires rather than tools a case permitted, so `tools_len == 0`
describes a request that client cannot send -- and a per-dialect tool-count
floor keyed on a measured client tool count would pin that client's VERSION
into a predicate and lie at its next release. An absent `id` token is
refused too rather than defaulted, on the same rule as an absent count.

The three structural predicates exist twice, in Python here and as
reference logic in the Rust test suite, and
`scripts/drivers/lib/wire_pattern_classification.tsv` is the shared
classification set that keeps them honest: one structural line per record,
paired with the patterns it does and does not satisfy. BOTH sides assert
against it -- the Python predicates through `scripts/drivers.test.sh` and
the Rust reference logic through
`crates/routectl-cli/tests/wire_pattern_weld.rs` -- so agreement with one
recorded verdict on both sides is the two implementations agreeing with
each other, and a divergence is red rather than silent. Scoped to those
three predicates -- the two body-census predicates have no Rust counterpart
to drift from, and a structural line carries nothing that decides them.

That same Rust test carries the other weld: it parses `WIRE_PATTERNS` out
of `validate_case.py` and the predicate table out of `verify_pattern.py`,
both as text from sentinel-delimited blocks, and asserts the covered set is
exactly the vocabulary minus an explicit deferred list (`mcp-tools` today,
the one token with a closed-set entry and no case). Extending the
vocabulary therefore turns it red until a predicate or a recorded deferral
lands -- the review moment each addition deserves. Renaming a sentinel
without updating the test makes the parse fail loudly; an empty parse is a
failure, never a coverage claim over nothing.

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

Every command in this section drives a real client against a real upstream:
it needs a working credential and it SPENDS TOKENS. Nothing below is safe to
run as a smoke test.

```
scripts/capture_driver.sh --lane anthropic-api --case tools-multiturn-01 \
  --expected-ingress anthropic -- scripts/drivers/claude-code-print.sh
```

That lands `.routectl-driver-scratch/anthropic-api/tools-multiturn-01/` --
scratch, not the corpus; promote it with `scripts/promote_fixture.sh` once
you have reviewed it. Add `--keep` to retain the run workspace, which holds
the trace, the client's own output, and `client.txt` (the version the driver
read from the binary at run time). A rerun of the same case re-lands on the
same path and produces a diff.

The MITM mode selects its own second port, exports the two carriers
(`ROUTECTL_DRIVER_PROXY_URL`, `ROUTECTL_DRIVER_PROXY_CA`) into the driver
environment itself, and gates the run on the proxy listener actually
being ready (exit 8 when it is not). The CA it points a client at is the
one the daemon mints at listener start, under the run's throwaway config
root at `mitm-certs/current/mitm-ca-cert.pem` -- `current` is the
generation symlink the cert store swaps on re-mint. The lane must be the
`[mitm]`-carrying twin; the mode and the lane config are checked against
each other before any daemon boots:

```
scripts/capture_driver.sh --lane anthropic-api.front-proxy --case thinking-01 \
  --connection-mode front-proxy --expected-ingress anthropic \
  -- scripts/drivers/claude-code.sh
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
  --expected-ingress anthropic -- scripts/drivers/external-agent-cli.sh
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

### Running a capture in a container

`scripts/container/` holds a second WAY TO RUN the same capture: a
Dockerfile, a build script, a host wrapper, and their self-tests. The
wrapper is a CALLER of `scripts/capture_driver.sh`, never a mode inside it.
Neither the runner nor the rig has a container-conditional branch (a
self-test asserts that), so the host path stays fully supported and
abandoning the container path costs nothing but deleting a directory.

Build the image locally. There is no registry and no published artifact --
the image is reproducible from the committed Dockerfile alone, and the base
is pinned by digest so two contributors cannot silently differ on glibc:

```
bash scripts/container/build.sh
```

That writes `routectl-capture:default`. `--version <client-version>` bakes a
different client version in as a build ARG and tags
`routectl-capture:<version>`; `--tag <tag>` names the tag outright. Never a
bare `:latest`, which would let a rebuild replace the image a fixture was
captured under. Exit codes: 0 built, 1 build failed, 2 usage, 3 docker
missing. The script reports the version it observes by running the client in
the finished image, because that observed value -- not the requested ARG --
is what a fixture records.

The image carries the client and its runtime dependencies. It carries NO
routectl binary and no Rust toolchain: build routectl on the host first
(`cargo build --release`) and the wrapper bind-mounts it in. A toolchain
layer would make the image a build environment and a fixture's provenance a
question about which compiler ran where.

Then run a capture through the wrapper. This spends real tokens and needs a
real credential on the host, exactly like a host-path run:

```
bash scripts/container/run_capture.sh --scratch /var/tmp/routectl-cell -- \
  --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
  -- scripts/drivers/claude-code-print.sh
```

Two `--` separators, and that is deliberate: the wrapper's own flags stop at
the first one, everything after it is the runner's argv verbatim, and the
runner's own `--` still introduces the driver command. `--image <tag>`
selects a non-default image.

What crosses the boundary, and nothing else:

| Host side | In-container | Mode |
|---|---|---|
| this repo | `/workspace` | READ-ONLY |
| the host routectl binary | `/usr/local/lib/routectl/bin/routectl` | READ-ONLY |
| `--scratch <dir>` | `/scratch` | writable, the only one |
| the upstream token | `ROUTECTL_DRIVER_ANTHROPIC_API_KEY` | forwarded BY NAME |

- **The read-only repo is load-bearing**, not tidiness. A driven agent runs
  with file tools and permission prompts disabled; a writable repo mount
  would let the thing under capture edit tracked source, and a capture is
  evidence only if the tree that produced it did not move under it. That is
  also why `--scratch` is required and must be outside the repo: with the
  repo read-only there is nowhere inside it a fixture could be written. The
  wrapper passes `--out /scratch` and `--out-root /scratch` to the runner and
  sets `ROUTECTL_DRIVER_OUT_ROOT` so `/scratch` is a member of the runner's
  closed set of allowed landing roots.
- **NO SEAT FILE IS MOUNTED**, and none exists inside the container. The
  wrapper reads the access token on the HOST and forwards it under the
  environment variable name the lane config already resolves, passing it to
  `docker run` by NAME rather than as a value in an argument vector. Nothing
  in the container can refresh, rewrite, or perturb the operator's seat
  store, and mid-run token expiry is therefore a HARD STOP rather than a
  refresh: a clean upstream 401, the rig lands no fixture, and the wrapper
  reports the runner's exit 7. Recapture with a fresh token. Mounting the
  seat read-only was measured to be worse than not mounting it, so do not
  reintroduce a mount for it.
  `ROUTECTL_CAPTURE_CELL_SEAT` overrides which seat file is read, and
  `ROUTECTL_BIN` which host binary is mounted; both exist so the self-test
  can drive this script end to end with no credential and no daemon.
- **No other host environment is forwarded.** That is a property, not an
  omission: an inherited `ANTHROPIC_BASE_URL` is how a capture once recorded
  a daemon nobody meant to capture, and a container starting from an empty
  environment cannot inherit one.
- **Default bridge networking, stated explicitly** even though it is
  docker's default. The isolation the cell provides rests on the container
  NOT being on the host's loopback, and a daemon whose default network had
  been reconfigured would change that property while every assertion about
  it still passed.
- `--user` carries the host uid/gid, so the fixture lands owned by you. A
  root-owned fixture could not be promoted or scrubbed without sudo.

**The wrapper's exit code is the runner's, verbatim.** 0 and 2 through 8
mean exactly what they mean on the host path, so a caller reads one contract
either way. The wrapper's own refusals occupy a disjoint range, one code
each, and every one of them fires before docker is consulted at all -- so a
refusal reads identically on a box with no docker installed:

| Code | Refusal |
|---|---|
| 10 | `--network host` or `--network=host` in the caller's argv |
| 11 | `--net host` or `--net=host` |
| 12 | `--privileged` |
| 13 | `--pid host` or `--pid=host` |
| 14 | any caller-added mount flag (`-v`, `--volume`, `--mount`, `--tmpfs`, `--volumes-from`) |
| 15 | the host routectl binary is missing or not executable |
| 16 | the scratch root is inside this repo |
| 17 | docker is not installed or not on PATH |
| 18 | the image is not present locally |
| 19 | the seat file is unreadable |
| 20 | the seat carries no usable access token |

The first four get their own codes rather than sharing one because each
independently dissolves the isolation the cell exists for, and a caller who
reads only the number must be able to tell them apart. The mount refusal is
deliberately an over-approximation: those tokens are refused anywhere in the
argv, including after the driver separator. The image is never pulled from a
registry (18 rather than a pull), because a run against a registry image
would capture under a layer set nobody in this repo committed.

Both scripts have shell self-tests in the same directory, which skip BY NAME
when docker is absent rather than silently:

```
bash scripts/container/run_capture.test.sh
bash scripts/container/image_scan.test.sh
```

The wrapper's refusal legs need no docker at all -- the wrapper decides every
one of them from its argv and the host filesystem -- so they run everywhere;
only the legs that actually start a container skip. Each property the
refusals protect is then asserted from INSIDE a container by a driver stub
that attempts the write and reports what happened, which is the only honest
place to assert it. The layer scan runs this repo's own
`scripts/scrub-fixture.sh` over the extracted content of every image layer,
so credential vocabulary keeps exactly one owner, and it carries a positive
control proving the scan can fail.


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
