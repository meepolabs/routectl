//! The `capability_learn_events` row shape and its insert.
//!
//! DEPRECATED: superseded by the unified `capability_events` ledger
//! (`capability_event.rs`). The request path no longer produces these rows --
//! learned negatives now land as `broken` rows in `capability_events`. This
//! module and the table remain (no DROP) so the legacy write path stays
//! compilable and existing rows survive; removal is a later change.
//!
//! A learn event is one confirmed observation that a routing target does
//! not support a capability. It is captured off the hot path and rides the
//! single usage-writer actor, exactly like a `UsageRecord`, but lands in
//! its own table (learn events are not requests). This crate has no
//! internal crate dependencies by design, so the row carries only plain
//! types -- the producer normalizes the capability key and stringifies the
//! tier before handing the event over.

use rusqlite::Connection;

/// One `capability_learn_events` row (see `schema::CREATE_CAPABILITY_LEARN_EVENTS_TABLE`).
///
/// Plain types only -- no dependency on the router / core capability
/// types. `capability_key` is expected already NORMALIZED by the producer;
/// `signal_tier` is one of `"self-identifying"` or `"inferred"` (the DDL
/// CHECK enforces the closed set). `remapped` is always false by
/// construction but persisted so a replayer can filter defensively.
/// `request_features` is the in-flight derived feature set, stored as a
/// JSON array TEXT. NEVER carries a body / message / prompt.
#[derive(Debug, Clone)]
pub struct CapabilityLearnEvent {
    /// Capture time (epoch milliseconds).
    pub ts: i64,
    /// The breaker's nickname-or-provider-fallback target key.
    pub state_key: String,
    /// The NORMALIZED capability key.
    pub capability_key: String,
    /// The provider kind that rejected the capability.
    pub provider_kind: String,
    /// Producer tier: `"self-identifying"` or `"inferred"`.
    pub signal_tier: String,
    /// Observation count at capture time.
    pub observations: u32,
    /// The upstream request-fault HTTP status (e.g. 400 / 422).
    pub upstream_status: u16,
    /// Always false by construction; persisted for defensive replay.
    pub remapped: bool,
    /// The request's derived in-flight feature set.
    pub request_features: Vec<String>,
}

/// Insert one learn event. Append-only (no dedup / no `OR IGNORE`):
/// multiple observations over the target's lifetime are expected and each
/// is a distinct row. All values are bound parameters. Returns the number
/// of rows inserted (always 1 on success).
pub fn insert_learn_event(
    conn: &Connection,
    e: &CapabilityLearnEvent,
) -> Result<usize, rusqlite::Error> {
    let request_features =
        serde_json::to_string(&e.request_features).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        INSERT_SQL,
        rusqlite::params![
            e.ts,
            e.state_key,
            e.capability_key,
            e.provider_kind,
            e.signal_tier,
            i64::from(e.observations),
            i64::from(e.upstream_status),
            i64::from(e.remapped),
            request_features,
        ],
    )
}

/// The bound `INSERT`. Column order mirrors
/// `schema::CREATE_CAPABILITY_LEARN_EVENTS_TABLE`; `?1..?9` positions match
/// the params list above.
const INSERT_SQL: &str = "\
INSERT INTO capability_learn_events (
    ts, state_key, capability_key, provider_kind, signal_tier,
    observations, upstream_status, remapped, request_features
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
)";
