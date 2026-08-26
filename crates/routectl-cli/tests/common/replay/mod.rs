//! Replay-harness machinery: fixture loader + structural comparators.
//!
//! Replay tests load a fixture from one of TWO roots, with two
//! different policies (see [`harness::local_root`] /
//! [`harness::driver_root`]): live-box captures, which are
//! per-contributor, gitignored, populated by
//! `scripts/capture_fixtures.sh`, and REPORT-ONLY; and
//! driver-generated fixtures, the only ones eligible for gating. The
//! repo ships the harness, never the live-box data.
//!
//! For the per-fixture directory layout and the `meta.json` schema,
//! see [`docs/REPLAY-FIXTURES.md`](../../../../../../docs/REPLAY-FIXTURES.md).
//!
//! Sub-modules:
//! - [`loader`] -- on-disk fixture format + `load_fixture` /
//!   `discover_fixtures`.
//! - [`json_diff`] -- structural JSON comparator + header comparator.
//! - [`lane`] -- derived lane class + the wire-conservation exception
//!   table (which ingress-vs-outgoing divergences are explained
//!   routectl transforms).
//! - [`sse_diff`] -- SSE event-sequence parser + comparator.
//! - [`gated_lanes`] -- the plain-text gated-lane list + its
//!   fail-closed reader.
//! - [`harness`] -- shared scaffolding (the two fixture-root locators,
//!   header-vec to `HeaderMap` bridge, per-fixture outcome enum,
//!   `meta.ingress_kind` adapter lookup, per-model enrichment rebuild)
//!   used by the `replay_egress.rs` / `replay_ingress.rs` test drivers.

#![allow(dead_code, unused_imports)]

pub mod gated_lanes;
pub mod harness;
pub mod json_diff;
pub mod lane;
pub mod loader;
pub mod sse_diff;

pub use gated_lanes::{
    GATED_LANES_FILE, GatedLaneError, gated_lanes_path, is_lane_gated, parse_gated_lanes,
    read_gated_lanes, read_gated_lanes_at,
};
pub use harness::{
    ADAPTIVE_THINKING_MODELS, ENRICHMENT_DEPENDENT_MODELS, FixtureOutcome, bounded_body_diff,
    divergence_count, diverges_only_in_messages, driver_root, enrichment_skip_reason,
    headers_from_pairs, ingress_for_kind, local_root, parse_enriched_canonical,
    replay_resolved_model, system_turn_lift_skip_reason, unpinned_ingress_skip_reason,
    with_replay_enrichment,
};
pub use json_diff::{
    DEFAULT_HEADER_ALLOW_SKIP, Divergence, DivergenceKind, assert_headers_equal,
    assert_json_equal_structural, diff_all,
};
pub use lane::{
    ANTHROPIC_FIDELITY_LANE, BEDROCK_API_SHAPES, Dialect, EGRESS_KINDS, EgressLane, Exception,
    ExceptionKind, INGRESS_IDS, LaneClass, LaneError, LaneKey, SymbolError, Transform,
    all_exceptions, class_for_dialects, egress_lane_from_fixture_kind, egress_lane_from_token,
    exceptions_for_lane, ingress_dialect, lane_class, normalize_ingress_for_lane, resolve_egress,
    resolve_site_symbol, unexplained, workspace_root,
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
