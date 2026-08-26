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

The replay drivers exercise the bare ingress -> egress path:
`AnthropicIngress::parse_request` produces a canonical `ChatRequest`
with default `routectl_internal` (`supports_adaptive_thinking=false`,
`history_reasoning=None`, `reasoning_dialect=None`,
`max_thinking_budget=0`). In production the router overlays these
fields from `model_profile.rs` and the dispatch-time merge BEFORE the
egress sees the canonical. The current replay does not yet thread that
enrichment, so any fixture whose model relies on it would diverge on
the outgoing body.

Practical effect:

- `claude-haiku-*` and `claude-sonnet-*` captures are typically
  in scope (their profile defaults match the bare canonical).
- Captures from `claude-opus-4` and newer (adaptive thinking on)
  are out of scope -- the egress applies adaptive-budget logic the
  bare canonical does not carry.
- Captures from DeepSeek (`history_reasoning=Preserve`) are out of
  scope -- the egress preserves reasoning history that the bare
  canonical drops.

The replay drivers enforce this by skipping any fixture whose
`meta.model` contains a denylisted substring (`opus-4`, `deepseek`).
Skipped fixtures land in the `skipped` count of the test summary, not
`failed`. Adaptive-thinking and DeepSeek replay will arrive in a
later expansion that threads router enrichment through the test setup.

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
