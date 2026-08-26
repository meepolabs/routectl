# Replay fixtures

This document is the on-disk format reference for the local
replay-fixture corpus at `crates/routectl-cli/tests/fixtures/captured/`.
You only need it when writing or debugging replay tests; for the
day-to-day capture recipe see [DEVELOPMENT.md](DEVELOPMENT.md)
"Adding a replay fixture from a real session".

The directory is gitignored. Fixtures are captured locally by
`scripts/capture_fixtures.sh` from a TRACE-level routectl session and
consumed by the replay test drivers in `crates/routectl-cli/tests/`.
**They never ship in the repo.** Each contributor maintains their
own corpus relevant to their own development and regression-testing
needs; the repo provides the harness, not the data.

The live matrix at `crates/routectl-cli/tests/live_matrix.rs` stays
the final regression gate. Replay catches wire-shape regressions
cheaply between matrix runs against whatever corpus the contributor
has on hand.

## Captured bodies that do NOT live in the corpus

A captured upstream body that a shipping unit test must pin belongs in
that test as an inline constant, not here. Because the corpus is
gitignored, a corpus-backed assertion is unrunnable for every
contributor but the one who captured it -- the test would silently skip
or fail on a fresh checkout and in CI, which is exactly where a pinned
regression body needs to hold.

The rule of thumb: the corpus holds bodies a DRIVER iterates over
(whatever the contributor happens to have), while a body a NAMED test
asserts against goes inline. Inlining is only appropriate for a small,
secret-free body -- a rejection envelope, not a full response.

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
`mod.rs`, with `loader.rs`, `json_diff.rs`, `sse_diff.rs`, and
`harness.rs` as sub-modules. `harness.rs` holds shared scaffolding
(`captured_root`, `headers_from_pairs`, `enrichment_skip_reason`,
`ENRICHMENT_DEPENDENT_MODELS`) used by the `replay_egress.rs` and
`replay_ingress.rs` drivers. For the day-to-day capture + replay flow see
[DEVELOPMENT.md](DEVELOPMENT.md) "Adding a replay fixture".

## Per-fixture directory layout

Each fixture lives at:

    crates/routectl-cli/tests/fixtures/captured/<request_id>/

Inside the request directory, `meta.json` and the two request halves are
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
      "client": {
        "name": String,
        "version": String,
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
  the current major: the key arrived alongside purely additive fields,
  so a pre-versioning directory is a valid fixture with those fields
  empty. This is the ONLY compatibility gate.
- `ingress_kind` -- which ingress dialect parsed the inbound body, in
  the vocabulary of `IngressAdapter::id()`
  (`crates/routectl-cli/src/ingress/mod.rs`), so a consumer dispatches
  on the value directly with no mapping table. EMPTY when the capture
  could not extract the token -- never a sentinel word outside that
  vocabulary, matching how `lane` reports its unmapped case and the
  empty-means-unpinned convention the rest of the schema uses.
- `provider_kind` -- which egress provider produced the outgoing body,
  in the `PROVIDER_KIND` const vocabulary of `routectl-providers` --
  in particular `"anthropic"` (not `"anthropic-api"`) for the
  api.anthropic.com client. The replay drivers select the matching
  translator from this.
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
  `ROUTECTL_FIXTURE_CASE_ID`; empty for an unpinned live-box capture.
- `config_sha` -- hash of the config in force at capture time, so a
  rerun under a drifted config does not read as client drift. Written
  from `ROUTECTL_FIXTURE_CONFIG_SHA`.
- `client` -- which client produced the request. `name` / `version`
  come from the captured ingress `user-agent`; `connection_mode` comes
  from `ROUTECTL_FIXTURE_CONNECTION_MODE`, because the trace cannot
  observe it. The mode matters: Claude Code sends `role:"system"` turns
  in `messages[]` through a MITM front proxy but inlines the same
  content as system-reminder text with zero system turns in base-url
  mode, so an unpinned mode makes a cross-mode comparison read as
  drift.
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

### Backward compatibility

The loader TOLERATES a fixture captured before this schema settled: the
added fields carry serde defaults, so an existing per-contributor corpus
keeps loading. That is deliberate -- a clean break would zero out the
only wire evidence anyone has on disk, and a past session cannot be
recaptured. Tolerance is not permission to run an unpinned fixture
through a GATED comparison: a consumer that needs a pinned lane, case,
client, or config refuses the individual fixture that lacks it. Only
`schema_version` is a hard gate, because a major bump means the
directory shape itself changed and no per-field default can rescue it.

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
