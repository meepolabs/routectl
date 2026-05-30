# Replay fixtures

The replay harness exercises wire-shape correctness without network.
Each fixture pins one routectl request: ingress wire body, the
canonical-to-upstream egress payload, the upstream response (when
captured), and the rendered egress response. Replay tests load a
fixture, drive the relevant code path, and assert the result matches
the on-disk bytes structurally.

The live matrix at `crates/routectl-cli/tests/live_matrix.rs` stays
the final gate. Replay catches wire-shape regressions cheaply between
matrix runs.

For the loader and structural comparators (`load_fixture`,
`assert_json_equal_structural`, `assert_sse_equal`, ...) see
`crates/routectl-cli/tests/common/replay/` -- the entry point is
`mod.rs`, with `loader.rs`, `json_diff.rs`, and `sse_diff.rs` as
sub-modules.

## Per-fixture directory layout

Each fixture lives at:

    crates/routectl-cli/tests/fixtures/canon/<scenario_name>/

Inside the scenario directory, files are present only when
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

`meta.json` is always present. The four bodies and four header
files are optional; the loader cross-checks presence against the
`has_*` flags in `meta.json` and errors on mismatch (missing file
named in the error).

## meta.json schema

    {
      "provider_kind": "anthropic" | "openai-compat" | "openai-responses",
      "stream": bool,
      "has_upstream_response": bool,
      "has_egress_response": bool,
      "router_overlay": bool,
      "expected_unknown_block_count": Option<u32>,
      "model": Option<String>
    }

Fields:

- `provider_kind` -- which egress provider produced the outgoing
  body. The replay test selects the matching translator. The string
  values match the in-code `PROVIDER_KIND` constants in
  `routectl-providers` -- in particular `"anthropic"` (not
  `"anthropic-api"`) for the api.anthropic.com client.
- `stream` -- `true` for SSE-bytes responses, `false` for JSON
  bodies. Drives which comparator the replay test reaches for
  (`assert_sse_equal` vs `assert_json_equal_structural`).
- `has_upstream_response` / `has_egress_response` -- which response
  files are present. Useful for capture sets that did not record
  the upstream side, or response-only fixtures.
- `router_overlay` -- `true` if the outgoing body reflects a
  dispatch-time `header_extras` / `payload_extras` overlay.
  **Must be `false` for now.** Overlay-aware replay is deferred;
  the loader rejects `true` until then.
- `expected_unknown_block_count` -- forward-compat scenarios only.
  Pins the number of unknown content blocks the canonical pipeline
  must opaquely pass through.
- `model` -- post-alias provider model id from the trace. Optional
  in the schema (older fixtures load without it), but the capture
  rig always writes it. Used by the replay drivers to apply the
  Phase 1 corpus scope below.

## Phase 1 corpus scope

The replay drivers exercise the bare ingress -> egress path:
`AnthropicIngress::parse_request` produces a canonical `ChatRequest`
with default `routectl_internal` (`supports_adaptive_thinking=false`,
`history_reasoning=Auto`, `reasoning_dialect=None`,
`max_thinking_budget=0`). In production the router overlays these
fields from `model_profile.rs` and the dispatch-time merge BEFORE the
egress sees the canonical. Phase 1 replay does not yet thread that
enrichment, so any fixture whose model relies on it would diverge on
the outgoing body.

Practical effect:

- `claude-haiku-*` and `claude-sonnet-*` capture rows are typically
  in scope (their profile defaults match the bare canonical).
- Fixtures from `claude-opus-4` and newer (adaptive thinking on) are
  out of scope -- the egress applies adaptive-budget logic the bare
  canonical does not carry.
- Fixtures from DeepSeek (`history_reasoning=Preserve`) are out of
  scope -- the egress preserves reasoning history that the bare
  canonical drops.

The replay drivers enforce this by skipping any fixture whose
`meta.model` contains a denylisted substring (`opus-4`, `deepseek`).
Skipped fixtures land in the `skipped` count of the test summary, not
`failed`. Adaptive-thinking and DeepSeek replay will arrive in a
later phase that threads router enrichment through the test setup.

Additional Phase 1 corpus constraints:

- Phase 1 fixtures must reflect a 2xx upstream response. Non-2xx
  responses are out of scope and will be rejected by the loader.
- Phase 1 fixtures must have `ingress_request.model == meta.model`
  (i.e., no client-side alias resolution). Aliased fixtures need
  router enrichment, which is not yet wired into the replay drivers.

## Redaction policy

Fixtures live in source control. They must contain zero secrets and
zero personal/internal references.

- `Authorization`, `x-api-key`, and any `x-amz-*` header value:
  replace with the literal string `<REDACTED>`. The header name
  stays; only the value is redacted.
- Cookies, session ids, anything that bears a token: same treatment.
- Prompt content and model output: replace anything that mentions
  personal or internal info with the canonical test text
  `reply with: pong`.

The redaction policy applies to BOTH header files and bodies. If a
body field carries a token-bearing value (for example a tool result
echoing back a header), redact it the same way.

## Sanitization recipe (operator-facing)

Adding a new fixture is a deliberate, hand-reviewed step. The
capture rig is a dev-only data-collection tool; nothing it produces
goes straight to the canon corpus.

1. **Reproduce the request against routectl** with header tracing
   on:

       ROUTECTL_LOG=routectl=trace ROUTECTL_TRACE_HEADERS=1 \
         routectl serve ...

   Issue the request that exercises the wire shape you want to
   pin.

2. **Run the capture rig**:

       scripts/capture_fixtures.sh

   This drops a per-request directory under
   `crates/routectl-cli/tests/fixtures/captured/<request_id>/`.
   That directory is gitignored by design -- bodies and headers
   are raw and may carry secrets.

3. **Move the captured directory** into the canon corpus under a
   descriptive name:

       mv crates/routectl-cli/tests/fixtures/captured/<id> \
          crates/routectl-cli/tests/fixtures/canon/<scenario_name>/

   Pick a name that records the wire-shape concern, not the
   request id (e.g. `anthropic_api_thinking_signature_replay`).

4. **Scrub every `*.headers.json` file** by hand. Replace
   `Authorization`, `x-api-key`, every `x-amz-*` header value, any
   cookie, and any other token-bearing value with `<REDACTED>`.
   Keep the header name; only the value changes.

5. **Scrub every body** for prompt content / response output that
   mentions personal or internal info. Replace with the canonical
   test text `reply with: pong`. The replay tests do not depend on
   prompt content -- they assert on wire shape.

6. **Write `meta.json`** with the schema above. Mark
   `router_overlay: false`. Set `expected_unknown_block_count`
   only for forward-compat scenarios.

7. **Run gitleaks** with the repo config:

       gitleaks detect --config .gitleaks.toml \
         --source crates/routectl-cli/tests/fixtures/canon

   Investigate every hit. Real hits are the bug we want to surface
   before commit; the allowlist is conservative on purpose.

8. **Hand-review the diff.** Diff the entire fixture directory
   and read every line as a curious stranger. No developer names,
   no internal hostnames, no internal-doc paths, no internal task
   or decision ids.

9. **Commit only after** the previous eight steps pass. One commit
   per scenario; one fixture is one self-contained change.
