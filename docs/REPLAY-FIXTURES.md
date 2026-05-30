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
      "provider_kind": "anthropic-api" | "openai-compat" | "openai-responses",
      "stream": bool,
      "has_upstream_response": bool,
      "has_egress_response": bool,
      "router_overlay": bool,
      "expected_unknown_block_count": Option<u32>
    }

Fields:

- `provider_kind` -- which egress provider produced the outgoing
  body. The replay test selects the matching translator.
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
