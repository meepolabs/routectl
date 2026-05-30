# canon -- hand-curated replay fixtures

This directory holds the checked-in seed corpus for the replay
harness. Fixtures here are HAND-CURATED, NOT mechanical captures:
every header value bearing a token (`Authorization`, `x-api-key`,
`x-amz-*`, cookies) is replaced with `<REDACTED>`, and every prompt
or response output that could mention personal or internal info is
replaced with the canonical test text `reply with: pong`.

For the per-fixture directory layout, the `meta.json` schema, the
full redaction policy, and the operator-facing sanitization recipe,
see [`docs/REPLAY-FIXTURES.md`](../../../../../docs/REPLAY-FIXTURES.md).

The sibling `captured/` directory is gitignored and holds raw,
unsanitized output from `scripts/capture_fixtures.sh`. Nothing from
`captured/` goes straight into `canon/` -- the sanitization recipe
is a deliberate, hand-reviewed step.
