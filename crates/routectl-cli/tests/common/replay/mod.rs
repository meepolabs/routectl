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
//!   `discover_fixtures` (flat, live-box) / `discover_driver_fixtures`
//!   (`<root>/<lane>/<case_id>`).
//! - [`plant`] -- the one writer of a synthetic corpus in a tempdir,
//!   shared by every test that needs one to walk.
//! - [`conservation`] -- captured-ingress vs captured-outgoing
//!   adjudication over the lane class and the exception table, plus the
//!   translation-lane divergence baseline.
//! - [`front_proxy`] -- front-proxy reachability per ingress, derived
//!   from the MITM host pin read out of the router's source.
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

pub mod conservation;
pub mod front_proxy;
pub mod gated_lanes;
pub mod harness;
pub mod json_diff;
pub mod lane;
pub mod loader;
pub mod plant;
pub mod sse_diff;

pub use conservation::{
    BaselineEntry, BaselineError, ConservationRun, CorpusSlice, ExceptionHits, GatedLanes,
    LaneSummary, TRANSLATION_BASELINE_FILE, UNPINNED_INGRESS_LABEL, Verdict, adjudicate,
    parse_translation_baseline, read_translation_baseline, read_translation_baseline_at,
    resolve_gated_lanes, resolve_gated_lanes_at, translation_baseline_path,
};
pub use front_proxy::{
    MITM_PIN_CONST, MITM_PIN_SITE_PATH, MITM_VALIDATOR_SYMBOL, PinError, Reachability, SETTLED_PIN,
    front_proxy_reachability, mitm_pin_site_path, mitm_pinned_host, mitm_pinned_host_at,
    parse_mitm_pinned_host, reachability_for_pin,
};
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
    ExceptionKind, INGRESS_IDS, LaneClass, LaneError, LaneKey, MCP_TOOL_RENAME_ID, SymbolError,
    ToolIdentityError, Transform, all_exceptions, class_for_dialects,
    egress_lane_from_fixture_kind, egress_lane_from_token, exceptions_for_lane,
    in_band_system_turns, ingress_dialect, lane_class, mcp_tool_rename_explained,
    normalize_ingress_for_lane, normalize_ingress_for_pair, resolve_egress, resolve_site_symbol,
    system_turns_were_lifted, tool_identity_preserved, unexplained, unexplained_for_fixture,
    workspace_root,
};
pub use loader::{
    FIXTURE_SCHEMA_VERSION, Fixture, FixtureClient, FixtureMeta, LoadedCorpus, ReplayError,
    discover_driver_fixtures, discover_fixtures, load_fixture,
};
pub use plant::{
    current_meta, make_conserved, plant_driver_case, plant_fixture, plant_unloadable_driver_case,
    write_required_files,
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
