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

Inside the request directory, files are present only when
`meta.json` declares them. The full set:

    meta.json
    ingress_request.json
    ingress_request.headers.json
    outgoing_request.json
    outgoing_request.headers.json
    upstream_response.json
    upstream_response.headers.json
    egress_response.json
    egress_response.headers.json

`meta.json` is always present. The two request halves --
`ingress_request.json` + `ingress_request.headers.json` and
`outgoing_request.json` + `outgoing_request.headers.json` -- are
ALWAYS required; the loader errors (naming the missing file) if any
of the four is absent. Only the two response halves are optional:
`upstream_response.*` is gated by `has_upstream_response` and
`egress_response.*` by `has_egress_response`. For each optional
half the loader cross-checks both files against its flag and errors
on mismatch -- a promised-but-missing file, or a stray file present
when the flag is `false`.

## meta.json schema

There are two views of `meta.json`: the SUPERSET the capture rig
writes, and the SUBSET the replay loader's `FixtureMeta` actually
deserializes. They do not match field-for-field -- the rig records
extra triage metadata the replay drivers never read, and one
loader-known field (`expected_unknown_block_count`) is not produced by
the current rig at all.

### Rig-written superset (`scripts/capture_fixtures.sh`)

The rig emits `meta.json` by hand (no jq dependency). Every key below
is always present:

    {
      "request_id": String,
      "captured_at_ts": String,
      "routectl_version": String,
      "alias": String,
      "model": String,
      "ingress_kind": "anthropic" | "openai" | ...,
      "provider_kind": "anthropic" | "openai-compat" | "openai-responses" | ...,
      "stream": bool,
      "finish_reason": String,
      "input_tokens": u64,
      "output_tokens": u64,
      "total_tokens": u64,
      "has_ingress_body": bool,
      "has_outgoing_body": bool,
      "has_upstream_response": bool,
      "has_egress_response": bool,
      "has_ingress_headers": bool,
      "has_outgoing_headers": bool,
      "has_upstream_headers": bool,
      "has_egress_headers": bool
    }

Note the rig does NOT write `expected_unknown_block_count`; see below.

### Loader-deserialized subset (`FixtureMeta`)

The replay loader only deserializes these fields (everything else in
the rig's superset is ignored on load via serde's default
unknown-field tolerance):

    {
      "provider_kind": String,
      "stream": bool,
      "has_upstream_response": bool,
      "has_egress_response": bool,
      "expected_unknown_block_count": Option<u32>,  // loader-known, NOT rig-written
      "model": Option<String>,
      "routectl_version": Option<String>
    }

Fields:

- `ingress_kind` -- which ingress dialect parsed the inbound body.
  Recorded by the capture rig for forward use; the loader's
  `FixtureMeta` does not deserialize it and neither replay driver
  reads it. The current replay is anthropic-ingress-only: both drivers
  hardcode `AnthropicIngress::parse_request` regardless of this
  value. Common rig-written values: `"anthropic"` (`/v1/messages`),
  `"openai-chat-completions"` (`/chat/completions`). Multi-dialect
  ingress selection arrives in a later expansion.
- `provider_kind` -- which egress provider produced the outgoing
  body. The replay test selects the matching translator. The string
  values match the in-code `PROVIDER_KIND` constants in
  `routectl-providers` -- in particular `"anthropic"` (not
  `"anthropic-api"`) for the api.anthropic.com client.
- `stream` -- `true` for SSE-bytes responses, `false` for JSON
  bodies. Stream fixtures are currently skipped by the replay
  drivers (stream-body replay is deferred -- the capture rig does
  not yet write stream bodies). `assert_sse_equal` exists as harness
  scaffolding for future stream replay and has no driver caller today;
  the exercised non-stream path uses `assert_json_equal_structural`.
- `has_upstream_response` / `has_egress_response` -- which response
  files are present. Useful for capture sets that did not record
  the upstream side, or response-only fixtures.
- `expected_unknown_block_count` -- loader-deserializable but NOT
  produced by the current capture rig: `capture_fixtures.sh` never
  writes this key, so it deserializes to `None` (via `#[serde(default)]`)
  on every real fixture today. Reserved, not yet enforced -- no replay
  driver reads it. Intended for a future forward-compat scenario that
  pins the number of unknown content blocks the canonical pipeline
  must opaquely pass through, written by a future rig pass or by hand.
- `model` -- post-alias provider model id from the trace. Optional
  in the schema (older captures load without it), but the capture
  rig always writes it. Used by the replay drivers to apply the
  corpus scope filter described below.
- `routectl_version` -- workspace package version stamped by
  `scripts/capture_fixtures.sh` at capture time. Optional in the
  schema for forward compat (older captures load without it). Lets
  contributors recognize stale captures after a routectl bump.

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
