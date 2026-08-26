//! Replay-harness machinery: fixture loader + structural comparators.
//!
//! Replay tests load a fixture from
//! `crates/routectl-cli/tests/fixtures/captured/<request_id>/`, drive
//! the relevant code path, and assert the result matches the on-disk
//! bytes structurally. The corpus is per-contributor, gitignored, and
//! populated by `scripts/capture_fixtures.sh`; the repo ships the
//! harness, never the data.
//!
//! For the per-fixture directory layout and the `meta.json` schema,
//! see [`docs/REPLAY-FIXTURES.md`](../../../../../../docs/REPLAY-FIXTURES.md).
//!
//! Sub-modules:
//! - [`loader`] -- on-disk fixture format + `load_fixture` /
//!   `discover_fixtures`.
//! - [`json_diff`] -- structural JSON comparator + header comparator.
//! - [`sse_diff`] -- SSE event-sequence parser + comparator.
//! - [`harness`] -- shared scaffolding (captured root locator,
//!   header-vec to `HeaderMap` bridge, per-fixture outcome enum) used
//!   by the `replay_egress.rs` / `replay_ingress.rs` test drivers.

#![allow(dead_code, unused_imports)]

pub mod harness;
pub mod json_diff;
pub mod loader;
pub mod sse_diff;

pub use harness::{
    ENRICHMENT_DEPENDENT_MODELS, FixtureOutcome, captured_root, enrichment_skip_reason,
    headers_from_pairs,
};
pub use json_diff::{
    DEFAULT_HEADER_ALLOW_SKIP, Divergence, DivergenceKind, assert_headers_equal,
    assert_json_equal_structural, diff_all,
};
pub use loader::{
    FIXTURE_SCHEMA_VERSION, Fixture, FixtureClient, FixtureMeta, LoadedCorpus, ReplayError,
    discover_fixtures, load_fixture,
};
pub use sse_diff::{ParseError, SseEventCmp, assert_sse_equal, parse_sse_events};

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
