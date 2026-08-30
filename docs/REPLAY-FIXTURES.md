# Replay fixtures

This document is the on-disk format reference for the replay-fixture
corpora under `crates/routectl-cli/tests/fixtures/`. You only need it
when writing or debugging replay tests; for the day-to-day capture
recipe see [DEVELOPMENT.md](DEVELOPMENT.md) "Adding a replay fixture
from a real session".

The LIVE-BOX corpus at `captured/` is gitignored. Its fixtures are
captured locally by `scripts/capture_fixtures.sh` from a TRACE-level
routectl session and consumed by the replay test drivers in
`crates/routectl-cli/tests/`. **Those fixtures never ship in the
repo.** Each contributor maintains their own such corpus relevant to
their own development and regression-testing needs; the repo provides
the harness, not the data.

Its sibling `driver/` holds the hermetic driver corpus, which IS
committed and review-gated -- see [Two corpus roots, two policies,
never mixed](#two-corpus-roots-two-policies-never-mixed) below for the
split.

The live matrix at `crates/routectl-cli/tests/live_matrix.rs` stays
the final regression gate. Replay catches wire-shape regressions
cheaply between matrix runs against whatever corpus the contributor
has on hand.

## Two corpus roots, two policies, never mixed

There are two fixture roots, siblings under
`crates/routectl-cli/tests/fixtures/`, resolved by `local_root()` and
`driver_root()` in `harness.rs`:

- `captured/` -- LIVE-BOX captures, written by
  `scripts/capture_fixtures.sh`. Per-contributor, gitignored,
  **report-only**: the bodies carry the operator's real prompts and
  model outputs, so this corpus may never be committed and a
  comparison over it may never become a commit gate.
- `driver/` -- fixtures produced by the hermetic fixture drivers.
  Hermetic by construction (nothing personal in them), so this is the
  only root eligible for gating, and it is COMMITTED: the corpus is
  tracked in the repo, and a fixture reaches it only through
  `scripts/promote_fixture.sh`, which re-runs the scrub gate on the way
  in so every tracked fixture arrives review-gated. A driver capture
  does NOT land here by default -- `scripts/capture_driver.sh` writes
  into a gitignored SCRATCH root (`.routectl-driver-scratch/` at the
  repo root unless `--out` says otherwise), because a rerun of a case
  overwrites that case's fixture and a corpus default would replace a
  reviewed fixture in place. Fixtures land two levels deep under this
  root (`<root>/<lane>/<case_id>`), which is why the loader walks it
  through `discover_driver_fixtures` rather than the single-level
  `discover_fixtures` the flat `captured/` root uses.

The separation is a DIRECTORY boundary rather than a naming
convention: with one root, "never gate the live-box corpus" is a
discipline someone has to keep, and the first lapse turns private
traffic into a commit gate.

Which lanes of the driver corpus a consumer may gate on comes from
`crates/routectl-cli/tests/fixtures/gated_lanes.txt` -- a plain text
file of lane ids in the `kind_str()` vocabulary, one per line, with
`#` comments and blank lines tolerated. Its reader
(`common/replay/gated_lanes.rs`) is FAIL-CLOSED: an unreadable,
malformed, or lane-less file errors rather than yielding an empty
gated set, because an empty set is indistinguishable from a passing
gate. Presence under `driver/` makes a fixture ELIGIBLE for gating;
only this file makes its lane gated.

The conservation harness reads that list through
`conservation::resolve_gated_lanes`, which maps exactly ONE error variant
-- the deliberately-empty `NoLanesListed` -- onto "no lane is gated", and
propagates every other. That is not fail-open: "the list was read and
parsed and names nothing yet" and "the list could not be read" are
different facts, and only the first one is knowledge. An empty gated set
stays unrepresentable from a parse failure.

## Truncated bodies are refused

`truncate_json_for_log` in `routectl-core/src/log_safe.rs` appends
`... [truncated at <cap> bytes]` when a traced body exceeds the trace
body cap. The loader refuses any fixture file ending in that marker:
such a file is a PREFIX of the wire body and would diff as drift.

The detector matches the full marker anchored at end-of-file (head
literal, decimal cap, tail literal), never the bare phrase `truncated
at`. Derivation, measured against a 250-fixture live-box corpus: the
bare phrase matches 12 files, all of them legitimate prompt content
(a captured system-reminder reading "...which was truncated at 27748
chars") and all valid JSON, while the full marker matches 0. A
bare-phrase detector would therefore refuse healthy fixtures at a 100%
false-positive rate. Refusal flows through the normal skip-and-count
path, so one clipped fixture never blinds the run to the rest.

Recapture a refused fixture with a larger `ROUTECTL_TRACE_BODY_BYTES`.

## Captured bodies that do NOT live in the LIVE-BOX corpus

A captured upstream body that a shipping unit test must pin belongs in
that test as an inline constant, not in `captured/`. Because the
live-box corpus is gitignored, an assertion backed by it is unrunnable
for every contributor but the one who captured it -- the test would
silently skip or fail on a fresh checkout and in CI, which is exactly
where a pinned regression body needs to hold.

The rule of thumb, for the LIVE-BOX corpus: it holds bodies a DRIVER
iterates over (whatever the contributor happens to have), while a body a
NAMED test asserts against goes inline. Inlining is only appropriate for
a small, secret-free body -- a rejection envelope, not a full response.

The COMMITTED driver corpus is the exception: it ships in the repo, so a
named test may assert directly against a driver fixture and that
assertion runs on every checkout -- the pinned-metadata assertion in
`tests/conservation.rs` does exactly this over every fixture under
`driver/`. Such a test must SKIP BY NAME when the corpus is absent (print
a NOT RUN line naming what is missing and return) rather than fail: a
checkout without the corpus is missing evidence, not exhibiting a
regression. State it over the corpus rather than against one named
fixture: a per-fixture assertion goes on passing while a second fixture
lands unpinned.

Current inline captures:

- The reasoning-replay rejection envelope (a 400 whose `error.message`
  reports an unrecognized encrypted-content prefix), pinned in the
  `failure_class` tests in `routectl-core` as the regression fixture for
  the closed replay-rejection matcher. 166 bytes, no secret: it carries
  only the upstream's own validation tokens and its prose message, no
  request content and no artifact bytes.

For the loader and structural comparators (`load_fixture`,
`assert_json_equal_structural`, `assert_sse_equal`, ...) see
`crates/routectl-cli/tests/common/replay/` -- the entry point is
`mod.rs`, with `loader.rs`, `json_diff.rs`, `sse_diff.rs`,
`gated_lanes.rs`, `lane.rs`, `conservation.rs`, and `harness.rs` as
sub-modules. `harness.rs` holds
shared scaffolding (`local_root`, `driver_root`, `headers_from_pairs`,
`enrichment_skip_reason`, `ENRICHMENT_DEPENDENT_MODELS`) used by the
`replay_egress.rs` and `replay_ingress.rs` drivers. For the day-to-day
capture + replay flow see
[DEVELOPMENT.md](DEVELOPMENT.md) "Adding a replay fixture".

## Per-fixture directory layout

The landing path depends on which CAPTURE MODE produced the fixture, and
the two modes never mix (`scripts/capture_fixtures.sh --help` states the
full policy split).

A live-box capture -- the default mode, drained from a real session --
lands at:

    crates/routectl-cli/tests/fixtures/captured/<request_id>/

A DRIVER capture (`--driver-mode`) lands keyed on `(lane, case_id)`, two
components under the run's output root:

    <out>/<lane>/<case_id>/

`<out>` is the gitignored SCRATCH root by default
(`.routectl-driver-scratch/` at the repo root; `scripts/capture_driver.sh
--out` may name another path, confined to `--out-root`), never the
committed corpus: an exploratory rerun of a case would otherwise replace
a reviewed fixture in place. A scratch fixture enters the corpus at
`crates/routectl-cli/tests/fixtures/driver/<lane>/<case_id>/` only
through `scripts/promote_fixture.sh`, which requires the source to sit
exactly two components under its `--scratch-root`, requires the dialect the
fixture is being landed as (`--expected-ingress`), re-runs
`scrub-fixture.sh --check` on the STAGED copy (a scratch fixture is
hand-editable between capture and promotion, so the rig's own capture-time
scrub is no longer sufficient evidence), and refuses the promotion
outright on a non-zero check.

The lane component is also the key to the per-lane HERMETIC CONFIG the
driver run booted under: `scripts/drivers/config/<lane>.toml`, committed,
one file per lane. `scripts/capture_driver.sh` copies it into the run's
throwaway `$XDG_CONFIG_HOME/routectl/config.toml` and records its sha256
as `meta.config_sha` -- the sha of the COMMITTED file, not of the booted
copy, so the field names the scenario's config identity rather than
changing per run. Editing a lane config therefore invalidates the
comparability of every fixture previously captured on that lane, which is
the point: without a pinned config, a rerun under a drifted one reads as
client drift.

Case keying is what makes a driver corpus diffable. A UUID-keyed corpus
grows a fresh sibling on every rerun and has nothing to compare against;
case-keyed, a rerun of the same scenario RE-LANDS on the same path, so it
either matches or diffs. The lane component is the NORMALIZED lane
(`kind_str()` vocabulary, as in `meta.lane`), and driver mode refuses to
promote a fixture whose provider kind did not map to one -- an empty lane
would collapse the path to `<out>/<case_id>` and put a fixture nothing can
gate into the canonical corpus.

A rerun REPLACES the previous directory rather than merging into it: the
old directory is renamed aside, the new one moves into place, and only
then is the old one removed, so a reader sees one whole fixture or the
other. Merging would leave files the new capture never observed -- a
non-stream run's `upstream_response.json` surviving into a stream rerun --
and since file presence IS the schema (below), the drift signal would be
read off a directory no single capture ever produced. `request_id` stays
in `meta.json` for traceability; it just no longer names the directory.

A `case_id` therefore has to be a single path-safe SCENARIO name
(`tools-multiturn-01`), never a value derived from the environment: in
driver mode the rig runs `scrub-fixture.sh --check` over the staged
fixture -- `meta.json` included -- before promoting it, so a case id
carrying a hostname or a real home path is refused by the landing gate
itself. A case id holding a path separator or a traversal segment is
refused outright, since it names a directory.

The set of case ids a driver corpus can hold is itself committed DATA:
`scripts/drivers/cases/<case_id>.json`, one file per case, each describing
one interaction and naming no harness. The schema is documented in
`scripts/drivers/cases/README.md` and enforced by
`scripts/drivers/lib/validate_case.py`, which every driver runs against its
case before a client opens a session -- so the id charset rule above is
guaranteed at the source rather than only caught at the landing gate. The
set covers wire PATTERNS (multi-turn tool use, cache breakpoints, thinking,
large contexts, plus a plain-turn baseline), not named models: which model
serves a pattern is the lane config's business. How to run one is in
[DEVELOPMENT.md](DEVELOPMENT.md).

Inside the fixture directory, `meta.json` and the two request halves are
always present; the response halves are present when the capture
observed them. The nine files the loader reads:

    meta.json
    ingress_request.json
    ingress_request.headers.json
    outgoing_request.json
    outgoing_request.headers.json
    upstream_response.json
    upstream_response.headers.json
    egress_response.json
    egress_response.headers.json

The rig also writes two triage aids the loader does NOT read and does
not require:

    structural.txt    the captured `structural summary` trace lines
    stream.txt        the captured `stream summary` trace lines

`structural.txt` holds at most two lines -- the ingress one first, then
the outgoing one. Each is selected by the emitter target plus the event
name plus its own `direction=` field, so a request body quoting the phrase
`structural summary` (routine traffic for a coding session about
routectl's own logging) cannot be selected in place of a real summary. An
absent direction FAILS the fixture in driver mode -- half the structural
evidence is not a canonical fixture -- and only warns on the live-box
path, where a drained log is whatever the daemon happened to emit.

`meta.json` is always present. The two request halves --
`ingress_request.json` + `ingress_request.headers.json` and
`outgoing_request.json` + `outgoing_request.headers.json` -- are
ALWAYS required; the loader errors (naming the missing file) if any
of the four is absent.

**`ROUTECTL_TRACE_HEADERS=1` on the daemon is therefore a hard
prerequisite for capture.** With header tracing off the rig writes no
header files at all -- including the two REQUIRED ones -- so every
fixture in the batch is refused with
`MissingFile: ingress_request.headers.json` and the whole capture is
wasted.

The four response files are optional and **their presence is read from
the directory listing**. `meta.json` carries no file-presence flags:
the filesystem is the only record, so there is no second copy of the
fact to disagree with it. Each of the four combinations per response
slot is valid, and the loader yields empty bytes / an empty header
vector for whichever file is absent:

| body | headers | what it means |
|---|---|---|
| present | present | that direction was fully observed |
| absent | present | STREAM capture -- the body arrived as SSE frames and was never logged as one JSON value |
| present | absent | that direction's response headers were not logged |
| absent | absent | that direction was not observed |

The rig's tmp-then-rename promotion (see the atomicity contract in
`scripts/capture_fixtures.sh`) means a fixture directory is never
half-populated, so the listing is trustworthy.

## meta.json schema

ONE schema, in the direction that matters: `FixtureMeta` carries no key
the rig does not produce. (The rig writes four triage-only fields the
loader ignores -- `request_id`, `captured_at_ts`, `alias`,
`finish_reason` -- plus the three token counts. That asymmetry is
harmless; the reverse one, a loader field no producer wrote, is what
broke the corpus.)

    {
      "schema_version": u32,
      "request_id": String,
      "captured_at_ts": String,
      "routectl_version": String,
      "alias": String,
      "model": String,
      "case_id": String,
      "config_sha": String,
      "wire_pattern": "baseline" | "tool-use-multiturn" | "cache-breakpoints" | "thinking" | "large-context",
      "client": {
        "name": String,
        "version": String,
        "binary_version": String,
        "connection_mode": String
      },
      "ingress_kind": "anthropic" | "openai" | "openai-responses",
      "provider_kind": "anthropic" | "openai-compat" | "openai-responses" | ...,
      "lane": "anthropic-api" | "openai-compat" | "openai-responses" | "bedrock" | "gemini",
      "stream": bool,
      "finish_reason": String,
      "input_tokens": u64,
      "output_tokens": u64,
      "total_tokens": u64
    }

Fields:

- `schema_version` -- fixture-format MAJOR. The loader reads exactly
  one major (`FIXTURE_SCHEMA_VERSION` in `loader.rs`) and refuses any
  other with a named error rather than half-loading a shape it does not
  understand. A fixture captured before the key existed is treated as
  major 1: the key arrived alongside purely additive fields, so a
  pre-versioning directory is a valid major-1 fixture with those fields
  empty. This is the only VERSION gate, and the rule governing it is
  "no minor, ever -- recapture is the migration" (see [Format
  evolution](#format-evolution-no-minor-ever)).
- `ingress_kind` -- which ingress dialect parsed the inbound body, in
  the vocabulary of `IngressAdapter::id()`
  (`crates/routectl-cli/src/ingress/mod.rs`), so a consumer dispatches
  on the value directly with no mapping table. EMPTY when the capture
  could not extract the token -- never a sentinel word outside that
  vocabulary, matching how `lane` reports its unmapped case and the
  empty-means-unpinned convention the rest of the schema uses.

  A TRACED FACT, not a claim: it is parsed out of the daemon's own
  `ingress request body` line, so it says which adapter really handled the
  request. That is what makes it usable as the enforcement side of a gate.

  **The dialect is ENFORCED AT PROMOTION, against a pin the caller
  supplies.** A driver run passes `--expected-ingress <dialect>`
  (`scripts/capture_driver.sh`), which becomes the mandatory
  `ROUTECTL_FIXTURE_EXPECTED_INGRESS` pin, and both boundaries refuse a
  fixture whose traced `ingress_kind` differs -- the capture rig in driver
  mode, in the same slot the wire-pattern and seam gates occupy, and
  `scripts/promote_fixture.sh` on the staged copy, which also takes the
  dialect on argv. The failure this catches is a client that ACCEPTS the
  runner's connection carriers and then reaches routectl on its own dialect
  anyway: the environment recorded the intent faithfully, so no
  environment check can see it, and the fixture would land as evidence for
  a dialect it never carried. An empty traced token is refused with the
  rest, because the driver corpus is never the live-box case where an
  unpinned dialect is honest. The pin's vocabulary lives in
  `scripts/drivers/lib/ingress_kinds.sh`, a replica of the `id()` set that
  the shell self-tests weld to the Rust source.

  Deliberately NOT re-derived alongside it: a lane-equality check (the lane
  comes FROM the same trace, so it cannot disagree with itself) and a
  `config_sha` equality check (the runner computes it from the file it
  booted). Nor an exactly-one-completed-request check: one agentic turn
  legitimately produces several, and the wire-pattern selector below is
  what decides which of them a case pins.
- `provider_kind` -- which egress provider produced the outgoing body,
  in the `PROVIDER_KIND` const vocabulary of `routectl-providers` --
  in particular `"anthropic"` (not `"anthropic-api"`) for the
  api.anthropic.com client. The replay drivers select the matching
  translator from this. It is also the API-SHAPE carrier: `lane`
  collapses `bedrock-invoke` and `bedrock-converse` onto the single
  `bedrock` token, while this raw provider token keeps them apart. There
  is deliberately no separate `api_shape` field, because the raw token
  already carries the distinction and `egress_lane_from_token`
  (`common/replay/lane.rs`) resolves both Bedrock spellings straight
  from it -- a second field would be a duplicate of a fact this one
  already states.
- `lane` -- the same egress concept in the `kind_str()` vocabulary of
  `ProviderEntry` (`crates/routectl-router/src/config/schema.rs`),
  which is the vocabulary a lane's class derives from. The rig
  NORMALIZES at write time (`anthropic` -> `anthropic-api`; both
  Bedrock api_shapes -> `bedrock`, which `kind_str()` does not split).
  An unmapped provider kind leaves this EMPTY and warns, rather than
  passing an unknown spelling through as if it were a lane token.
- `case_id` -- stable identity of the SCENARIO, as opposed to the
  one-off `request_id`. A rerun of the same case re-lands on the same
  identity, so it either matches or diffs. Written from
  `ROUTECTL_FIXTURE_CASE_ID`; empty for an unpinned live-box capture. In
  driver mode it also NAMES the landing directory (see the layout above),
  so it must be a single path-safe scenario name and it is mandatory.
- `config_sha` -- hash of the config in force at capture time, so a
  rerun under a drifted config does not read as client drift. Written
  from `ROUTECTL_FIXTURE_CONFIG_SHA`.
- `wire_pattern` -- the wire shape the driven case CLAIMS to cover, a
  string in the closed `WIRE_PATTERNS` vocabulary enforced by
  `scripts/drivers/lib/validate_case.py` (`baseline`,
  `tool-use-multiturn`, `cache-breakpoints`, `thinking`,
  `large-context`). DERIVED by the driver runner from the case file
  (`scripts/capture_driver.sh` reads it through `validate_case.py --field
  wire_pattern` and exports `ROUTECTL_FIXTURE_WIRE_PATTERN`) and never
  accepted on argv: a flag would let a caller declare a pattern its case
  does not claim, and the recorded claim is the only on-disk evidence of
  which shape a fixture was captured for. Mandatory in driver mode with
  the other environment pins; EMPTY on a live-box capture, which cannot
  observe it, and on any fixture predating the key.

  **The claim is a SELECTOR, enforced at promotion.**
  `scripts/drivers/lib/verify_pattern.py` holds one predicate per pattern in
  the closed vocabulary, read off the fixture's own ingress structural line
  or ingress body, and the capture rig invokes it in driver mode after
  `scrub-fixture.sh --check` and before the promotion `mv`.

  ONE agentic turn produces SEVERAL completed upstream requests, and the
  case's claim is the statement of WHICH of them the case means. Each
  completed request in the trace is a CANDIDATE; a candidate whose captured
  bytes do not exhibit the claim is DISCARDED and SKIPPED, and the next one
  is considered. Candidates are examined in first-`ingress request body`
  order -- the beginning of the request, not its completion marker, because
  stream flush timing can otherwise reorder two requests whose initiation
  order was fixed, and a selector over a nondeterministic order is a
  nondeterministic selector. **The FIRST candidate satisfying the claim is
  the one the case pins:** first-match encodes no assumption about whether a
  claim grows or shrinks across a turn, and it is the minimal witness --
  "richest" and "last" are timing properties rather than wire shapes, and
  would pin a fixture to client chattiness so two deployments' fixtures for
  one case would stop being comparable.

  **STAGE ALL, VALIDATE ALL, PROMOTE ONCE.** Driver mode stages, scrubs,
  `--check`s and validates EVERY candidate, retains only the selected staged
  directory, and performs a single promotion plus a single manifest append
  after the whole scan succeeds. Every landing gate is computed from the
  candidate's own bytes and trace lines -- the lane from its
  `provider_kind`, the seam check from its header capture, the
  expected-ingress pin from its traced dialect -- so a LATER candidate can
  refuse the run for a reason the selected candidate passed. Promoting as
  the scan ran meant such a refusal destroyed the correct fixture the run
  had already landed; with nothing promoted while candidates are still being
  examined, that is structurally impossible rather than merely unwound.
  Live-box mode is unchanged: it keys on `request_id`, so each of its
  captures is independent evidence and promotes as it is written.

  A LATER CANDIDATE THAT ALSO SATISFIES THE CLAIM IS REDUNDANT, not
  ambiguous: a tool loop resends its `tool_use` / `tool_result` pair in every
  later request, so a monotone claim is satisfied by every candidate after
  the first witness. It is counted separately from a skip
  (`candidates_redundant`), because a reader seeing two skips would conclude
  two requests failed the claim when one failed and one was a redundant
  witness. A redundant match is accepted only as a strict CONTINUATION of
  the selected one -- same traced ingress kind, same normalized lane and raw
  provider kind, same model, and a turn list strictly LONGER. A client
  retry, a fallback attempt carrying the same history, and a stray
  side-request are none of those, so a trace holding two genuinely different
  interactions under one case id is REFUSED: that is what keeps "one case id
  pins one interaction" enforced by something.

  PER-REQUEST FACTS MAY SKIP; PER-RUN FACTS ABORT. The wire pattern is the
  only per-request fact among the landing gates -- a missing structural
  summary, an unclassified lane, seam/mode incoherence, an unexpected
  ingress dialect and an empty lane are properties of the RUN, identical
  for every request in the trace, so aborting on them is correct and
  stricter. ONE deliberate exception: scrub `--check` residue is
  per-request and stays FATAL, because silently skipping past a body that
  carried a credential shape would turn the loudest safety signal in the
  pipeline into a per-request footnote. Scanning every candidate is what
  keeps that exception meaningful: stopping at the first match would leave
  the rest of the turn unscrubbed and unchecked.

  Landing zero fixtures in driver mode therefore splits two ways, with
  opposite retry verdicts and NO new exit code: candidates existed and none
  matched is a case defect (the rig's refusal exit, never retryable), while
  zero candidates is a transient (the zero-landing exit, retryable). A
  driver run ending in a refusal leaves no promoted fixture and no manifest
  line.

  The rig emits ONE structured selection line per driver run naming the
  case id, the selected request id, how many candidates were examined, how
  many were skipped, how many were redundant witnesses, and the ordering
  basis. It logs no bodies. This is observability, not identity, so it is a
  log line and deliberately NOT a meta field: under the selector the ordinal
  is not the selection basis, so a recorded one would preserve a number
  nothing reads.

  `scripts/promote_fixture.sh` re-runs the same predicate on the staged
  bytes, against the pattern the staged `meta.json` records, because a
  scratch fixture is hand-editable between capture and promotion -- there
  it is a GATE rather than a selector, since a staged directory is one
  request. An absent predicate script is a hard failure in both, never an
  unverified promotion.

  **Dialect scope, per pattern.** `tool-use-multiturn` is dialect-tolerant:
  one census picks the turn list by which key the captured body carries
  (`messages` or `input`) and matches the tool-call / result pair in each
  dialect's own spelling, so an `openai-responses` or `openai` capture
  promotes through the same predicate an `anthropic` one does. `baseline` is
  ANTHROPIC-ONLY and refuses a claim on any other dialect BY NAME: another
  client's floor request carries tools its own runtime requires, so
  `tools_len == 0` describes a request it cannot send, and a per-dialect
  tool-count floor would pin that client's version into the predicate. A
  pattern not capturable on a client is a property of the client, not a gap
  in the corpus.

  Run it by hand against a staged or committed fixture directory:

  ```
  python3 scripts/drivers/lib/verify_pattern.py \
    crates/routectl-cli/tests/fixtures/driver/anthropic-api/plain-turn-01 baseline
  ```

  What the LOADER does with the value is unchanged: it stores the string
  and compares nothing. The enforcement is at the promotion boundary, which
  is the only point at which a fixture can still be refused.

  Also executed on every checkout: the weld between the predicates and the
  vocabulary they answer for
  (`crates/routectl-cli/tests/wire_pattern_weld.rs`): every pattern token
  has a predicate or an explicit deferral, and the Python and Rust
  implementations of the three structural predicates classify the shared
  record set identically.
- `client` -- which client produced the request. `name` / `version`
  come from the captured ingress `user-agent`; `connection_mode` comes
  from `ROUTECTL_FIXTURE_CONNECTION_MODE`, because the trace cannot
  observe it. The mode matters: Claude Code sends `role:"system"` turns
  in `messages[]` through a MITM front proxy but inlines the same
  content as system-reminder text with zero system turns in base-url
  mode, so an unpinned mode makes a cross-mode comparison read as
  drift.

  `binary_version` is the SECOND, independent statement of the same
  version: the driver reads it off the RUNNING client binary before any
  session opens, and the runner forwards it to the rig as
  `ROUTECTL_FIXTURE_CLIENT_BINARY_VERSION` -- which is why the read has to
  cross back out of the run workspace, since that workspace is removed on
  exit unless `--keep`. `version` is the client's own self-report on the
  WIRE and is therefore CLIENT-CONTROLLED; `binary_version` is not.

  A DISAGREEMENT between the two REFUSES PROMOTION at both boundaries
  (`scripts/drivers/lib/client_version.py`, invoked from the capture rig
  and again from `scripts/promote_fixture.sh`): a client whose binary and
  user-agent report different versions is not evidence about either, and a
  mid-run auto-update is exactly what produces one. The comparison is on
  DOTTED-NUMERIC VERSION TOKENS, not on strings, because the two sources
  spell one version differently by construction (`2.1.246 (Claude Code)`
  from a binary against a bare `2.1.246` extracted from
  `claude-cli/2.1.246 (external, cli)`); the reduction is applied to both
  sides by the same code, so neither side's decoration decides the outcome.
  EITHER SIDE ABSENT is NOT COMPARABLE and promotes with the absence
  recorded as an empty field -- a live-box capture has no binary to read,
  and a fixture predating the key reaches the same arm, which is what keeps
  it additive. Nothing ever backfills one field from the other: a field
  that mirrored its counterpart could not contradict it.

  The mode is ENFORCED AT PROMOTION too, against evidence no environment
  carrier can provide. An env var states the client's INTENT; the MITM seam
  header (`x-routectl-mitm-proxied`, retained by name in captured ingress
  headers because it is in the redaction list in
  `crates/routectl-core/src/log_safe.rs`) is the only proof of TRANSIT. So
  a `front-proxy` fixture whose captured ingress headers lack the seam
  header name is refused -- a client that silently fell back to a direct
  connection cannot produce it, whatever its environment said -- and a
  `base-url` fixture that carries it is refused as well. Both directions,
  in the capture rig and again in `scripts/promote_fixture.sh`. The
  comparison is on the header NAME, case-insensitively; the value is a
  redaction placeholder by the time the gate runs.

  All four environment-sourced pins recorded in meta.json (`case_id`,
  `config_sha`, `client.connection_mode`, `wire_pattern`) are EMPTY when
  unset on the live-box path, where a trace genuinely cannot observe them,
  and MANDATORY in driver mode, where an unset pin is a bug in the driver
  rather than a fact about the capture. Driver mode aborts naming the
  missing variable. `ROUTECTL_FIXTURE_EXPECTED_INGRESS` is the fifth
  mandatory driver-mode pin and is NOT recorded: the value it pins is
  already on disk as the traced `ingress_kind`, so a second copy could
  only ever disagree with the fact it was checked against.
- `stream` -- `true` for SSE-bytes responses, `false` for JSON
  bodies. Stream fixtures are currently skipped by the replay
  drivers (stream-body replay is deferred -- the capture rig does
  not yet write stream bodies). `assert_sse_equal` exists as harness
  scaffolding for future stream replay and has no driver caller today;
  the exercised non-stream path uses `assert_json_equal_structural`.
- `model` -- post-alias provider model id from the trace. Used by the
  replay drivers to apply the corpus scope filter described below.
- `routectl_version` -- workspace package version stamped by
  `scripts/capture_fixtures.sh` at capture time. PURELY
  INFORMATIONAL: it lets a contributor recognize a stale capture, and
  nothing reads it as a compatibility signal. `schema_version` is the
  only gate.

### Format evolution: no minor, ever

Recapture IS the migration. Three clauses, and they are the whole rule:

1. **Adding an OPTIONAL key is the only evolution permitted at a fixed
   major.** A reader ignores keys it does not know (`FixtureMeta` carries
   no `deny_unknown_fields`, and every additive field carries a serde
   default), so a checkout that writes a new key stays readable by a
   checkout that has never heard of it -- which is what makes a
   contributor's branch readable by CI's base loader. A consumer that
   NEEDS a key refuses the individual fixture lacking it; the loader does
   not refuse the corpus.
2. **Anything that is not "add an optional key" bumps the MAJOR, and the
   bumping change RECAPTURES the committed driver corpus in the same
   commit.** Driver fixtures are synthetic and reproducible by
   construction, so a single tree never holds a mixed-major committed
   corpus. That is why the loader gates on `!=` and NOT on `>`, and the
   `!=` is CORRECT rather than a limitation: a fixture at a LOWER major
   is a directory shape this loader cannot read either, and half-loading
   it is the exact failure the gate exists to prevent. Do not weaken the
   comparison. Both directions are pinned by tests in `loader.rs`.
3. **There is no minor version, of any spelling.** A second integer
   replicated across `scripts/capture_fixtures.sh`, the Rust constant and
   this document, with nothing enforcing it, reads as a guarantee and is
   not one. Clause 2's recapture is the entire migration story.

The live-box corpus described in the next section is EXEMPT from clause 2
and cannot be recaptured, which is why the loader's
`default_schema_version()` returns the literal 1 rather than tracking
`FIXTURE_SCHEMA_VERSION`.

### Backward compatibility with the pre-schema live-box corpus

This section describes tolerance for ONE unrepeatable population -- the
per-contributor live-box captures taken before the schema settled. It is
not the rule for the committed driver corpus; that is the previous
section.

The loader TOLERATES such a fixture: the added fields carry serde
defaults, so an existing per-contributor corpus keeps loading. That is
deliberate -- a clean break would zero out the only wire evidence anyone
has on disk, and a past session cannot be recaptured. Tolerance is not
permission to run an unpinned fixture through a GATED comparison: a
consumer that needs a pinned lane, case, client, or config refuses the
individual fixture that lacks it. `schema_version` stays a hard gate even
here, because a major bump means the directory shape itself changed and no
per-field default can rescue it.

The rig's self-test (`scripts/capture_fixtures.test.sh`, run in CI)
drives the real script over a synthetic trace and pins the emitted
schema, the presence of every required file, and that `meta.json` stays
valid JSON when a value carries a quote. It exists because the corpus is
gitignored, so a rig regression would otherwise only surface on a
contributor's next capture -- against evidence that cannot be
recaptured.

The rig emits `meta.json` and `manifest.jsonl` by hand (no jq
dependency), so every interpolated string value passes through its
`json_escape` helper. Two of those values are outside the rig's control:
the environment pins are set programmatically by a driver, and
`client.name` / `client.version` are parsed from a client-controlled
`user-agent`. An unescaped quote in any of them would produce invalid
JSON that the rig promotes at exit 0 and the loader then skips forever.

## Corpus scope

### Ingress selection

Both replay drivers select the ingress adapter from `meta.ingress_kind`,
whose vocabulary IS `IngressAdapter::id()` (`anthropic`, `openai`,
`openai-responses`) -- so the lookup needs no mapping table. A value
outside that vocabulary fails the driver naming it, the same way an
unknown `provider_kind` does. The EMPTY value means the capture could not
observe the ingress token; that fixture is skipped with a reason rather
than defaulted to any dialect, because a wrong dialect would replay the
body through the wrong parser and read as wire drift.

The ingress-side response render is still Anthropic-only, so a fixture
captured on another dialect currently compares an Anthropic-shape render
against its own dialect's captured response.

### Per-model enrichment

The replay drivers rebuild the per-model knobs the router would have
overlaid onto `routectl_internal` before the egress saw the canonical
request. The rebuild constructs a `ResolvedModel` through its existing
`with_*` builders and projects the four egress-read knobs
(`supports_adaptive_thinking`, `max_thinking_budget`, `effort_levels`,
`reasoning_dialect` / `history_reasoning`) onto the parsed request. Without
it, every fixture replayed at the library-consumer defaults
(`supports_adaptive_thinking=false`, `max_thinking_budget=0`, both dialect
knobs `None`) and any capture taken under a non-default knob diverged for a
reason that was not a bug.

Two knobs are reconstructed differently because their derivability
differs:

- `supports_adaptive_thinking` IS derivable per fixture. Production makes
  it an explicit `[models.X]` opt-in (Anthropic's adaptive rollout has no
  clean naming pattern), but the adaptive generation rejects the legacy
  `thinking.type=enabled` shape with a 400, and the capture rig only emits
  a fixture for a request whose trace shows a successful upstream
  response. A fixture on an adaptive-generation model was therefore
  necessarily captured with the flag on. The driver's
  `ADAPTIVE_THINKING_MODELS` list carries those model substrings.
- `history_reasoning` is NOT derivable. `Auto` strips outgoing reasoning
  history and `Preserve` emits it; the captured body shows which was in
  force but no `meta.json` field records the operator's choice, so
  guessing would turn a config difference into a wire-shape failure.

`ENRICHMENT_DEPENDENT_MODELS` is the residual skip list for exactly that
undeterminable case -- today the DeepSeek reasoning family (`deepseek`).
Skipped fixtures land in the `skipped` count of the test summary, not
`failed`. A model belongs on that list only when a knob it needs is a free
operator choice `meta.json` does not pin; anything the rebuild can
reconstruct is not grounds for a skip.

### Failure reporting is value-bounded

A replay failure reports the divergence PATH, the KIND, and a SHAPE
summary of each side (type, length, object key names) -- never a whole
value or subtree. Captured fixtures are real prompt traffic and a test log
is not a confidential sink: it reaches CI output, terminal scrollback, and
pasted bug reports.

A string value prints verbatim only when its leaf field is on an
ALLOWLIST of wire identifiers and enums (`model`, `role`, `type`,
`effort`, ...) and is short. The allowlist direction is deliberate -- a
denylist of prose fields fails OPEN on any key nobody enumerated, and the
ingress's forward-compat sweep puts arbitrary client keys into a captured
body. Over-cap and non-allowlisted values are reported as
`string(len=N, elided)`; a truncated PREFIX is never emitted, since the
opening bytes of a prompt are the system preamble. Path and kind are never
abridged: they are the diagnostic and carry no payload.

### Divergence classes on the current corpus

Measured over the 250 loadable fixtures on the anthropic-ingress lane,
the egress leg resolves to **2 asserted, 133 skipped, 115 failed**. The
divergences behind those numbers fall into the classes below -- the first
is a harness gap, the rest are findings about the code under test.

Note that a fixture typically carries divergences from SEVERAL classes at
once, so the classes do not partition the fixture count. What decides a
fixture's outcome is whether anything outside `messages[]` diverges:

- **`messages[]` positional shift** -- the system-turn lift. Positional
  array pairing reports one divergence per element after a removed middle
  turn, so this class dominates by divergence count. Its pre-diff
  normalizer is not built yet, so the egress driver SKIPS a fixture whose
  divergences are ALL inside `messages[]`, with a reason naming the missing
  normalizer. That accounts for the 133 skips.
- **`model`** -- the alias-to-upstream rewrite, which is router dispatch
  rather than an egress concern and the bare replay path does not perform.
- **`output_config`** -- captured bodies carrying
  `output_config: {"effort": "xhigh"}` alongside
  `thinking: {"type": "disabled"}`. Current code cannot emit that pairing
  from any input: `reconcile_output_config_effort` is a late enforcer that
  drops `output_config.effort` unless the assembled body carries
  `thinking.type == "adaptive"`, and it reads the final body rather than
  any flag. Verified by driving the captured ingress shape through
  `normalize_request` directly under both `supports_adaptive_thinking`
  values and with a populated `effort_levels`: `output_config` is dropped
  in every combination (a sibling `output_config.format` does survive).
  These fixtures therefore predate the effort invariant and are EVIDENCE
  of an intentional behavior change, not a replay-path gap -- they must
  not be normalized away, and the enrichment rebuild cannot fix them.
  The invariant is authoritative and the captures are stale; reconciling
  the corpus against it is a provenance question, tracked separately.

The 115 failures are the fixtures that carry a `model` or `output_config`
divergence ALONGSIDE their `messages[]` ones. The narrow skip scope is
deliberate and load-bearing: a lift-affected fixture is not excused for
being lift-affected when something else also diverged.

Do not derive per-class counts by grepping the test log. Most failing
lines hit the "first N shown" cap, so a class appearing only past the cap
is absent from the log and counting there undercounts. Read the fixtures
and call `diff_all` for a complete set; the cap bounds the log, not the
comparison.

### Conservation: captured ingress vs captured outgoing

A second, independent axis, driven by `tests/conservation.rs` over
`common/replay/conservation.rs`. It compares a fixture's
`ingress_request.json` against its `outgoing_request.json` -- two files
captured from the SAME real request -- so no routectl code re-runs and no
enrichment is rebuilt. Every divergence is either an explained routectl
transform (the exception table in `lane.rs`) or wire loss.

The harness normalizes BEFORE it diffs: the lane's normalizer entries
rewrite the ingress side first, then `diff_all(outgoing, normalized
ingress, ..)`. That orientation is what the exception predicates are
written against; swapping it inverts Added/Removed and un-matches every
matcher.

Measured over the 250 loadable live-box fixtures, all on the
`anthropic` -> `anthropic-api` FIDELITY lane, the corpus reduces to
exactly four explained classes with ZERO unexplained:

| class | fixtures / divergences | exception |
|---|---|---|
| `messages[]` length shrink (system-turn lift) | 238 bodies rewritten | `system-turn-lift` (NORMALIZER) |
| `.temperature` ADDED as `1.0` | 133 | `thinking-temperature-clamp` |
| `.model` VALUE change (bracketed alias resolved) | 6 | `model-alias-suffix-resolved` |
| whole `thinking` key REMOVED | 4 | `disabled-thinking-dropped` |

Note the `output_config` stale fixtures do NOT diverge on this axis:
`output_config` is present on BOTH captured sides, so conservation reads
them clean. They remain a finding on the egress-replay axis above.

Verdicts: a FIDELITY lane FAILS on any divergence no exception explains. A
TRANSLATION lane is report-only against
`crates/routectl-cli/tests/fixtures/translation_baseline.txt`, one
`<ingress> <egress> <divergence-path>` triple per line -- the signal is
CHANGE, so a path ABSENT from the baseline fails. An exception matching
ZERO divergences on a POPULATED lane fails (an unexercised matcher is an
untested claim). A gated lane with zero asserted fixtures, or with any
skip, fails. DEGRADED prints loudly but exits 0: nothing asserted, or the
corpus held entries that would not load.

An EMPTY translation baseline is legal, in deliberate contrast to
`gated_lanes.txt`: an empty gated set would make every gated comparison
silently report-only (fail-open), whereas an empty baseline makes every
translation divergence a failure (fail-closed). A MISSING baseline file is
still an error -- an unknown baseline adjudicates nothing.

**Stated limit.** `resign_cch_in_place`
(`routectl-providers/src/claude_signing.rs`) rewrites five lowercase hex
characters of one `cch=` token inside the `system` billing block AFTER the
outgoing-body trace is emitted -- length-preserving, a silent no-op when
the token is absent, and present in 133 of the 250 outgoing bodies. So the
captured outgoing body differs from the true transmitted bytes by exactly
those five characters, and conservation cannot see them. It gets no
exception entry, because it produces no ingress-vs-outgoing divergence at
all and the zero-match rule would correctly fail such an entry. A
byte-identical deletion gate inherits this limit.

Two further conventions hold for the current corpus. These are
capture-rig conventions, NOT loader- or driver-enforced -- the
loader stores no HTTP status and performs no model comparison, so
nothing rejects a fixture that violates them:

- Current-scope fixtures reflect a 2xx upstream response. The capture rig
  only emits a fixture for a request whose trace carries an
  `upstream success body` (or `stream summary`) line, so non-2xx
  responses are not produced in the first place; the loader itself
  does not inspect or reject on status.
- Current-scope fixtures carry no client-side alias resolution
  (`ingress_request.model` matches the post-alias `meta.model`).
  Aliased fixtures would need router enrichment that the replay
  drivers do not yet thread; the capture rig does not produce them,
  and the loader does not validate the relationship.
