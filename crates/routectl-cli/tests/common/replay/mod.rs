//! Replay-harness machinery: fixture loader + structural comparators.
//!
//! Replay tests load a hand-curated fixture from
//! `crates/routectl-cli/tests/fixtures/canon/<scenario_name>/`, drive
//! the relevant code path, and assert the result matches the on-disk
//! bytes structurally. Initial scope is egress-only (canonical
//! `ChatRequest` -> upstream-bound bytes); the actual replay tests
//! arrive in a follow-up wave.
//!
//! For the per-fixture directory layout, the `meta.json` schema, the
//! redaction policy, and the operator-facing sanitization recipe, see
//! [`docs/REPLAY-FIXTURES.md`](../../../../../../docs/REPLAY-FIXTURES.md).
//!
//! Sub-modules:
//! - [`loader`] -- on-disk fixture format + `load_fixture` /
//!   `discover_fixtures`.
//! - [`json_diff`] -- structural JSON comparator + header comparator.
//! - [`sse_diff`] -- SSE event-sequence parser + comparator.
//! - [`harness`] -- shared scaffolding (canon root locator, header-vec
//!   to `HeaderMap` bridge, per-fixture outcome enum) used by the
//!   `replay_egress.rs` / `replay_ingress.rs` test drivers.

#![allow(dead_code, unused_imports)]

pub mod harness;
pub mod json_diff;
pub mod loader;
pub mod sse_diff;

pub use harness::{canon_root, headers_from_pairs, FixtureOutcome};
pub use json_diff::{
    assert_headers_equal, assert_json_equal_structural, DEFAULT_HEADER_ALLOW_SKIP,
};
pub use loader::{discover_fixtures, load_fixture, Fixture, FixtureMeta, ReplayError};
pub use sse_diff::{assert_sse_equal, parse_sse_events, ParseError, SseEventCmp};

/// Wrapped error message returned by every structural comparator.
/// Tests print it via `Display`; it is never re-parsed, so a bare
/// `String` payload is enough.
#[derive(Debug, Clone)]
pub struct DiffMessage(pub String);

impl std::fmt::Display for DiffMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DiffMessage {}
