//! The `capability_events` row shape, its insert, and the tombstone
//! constructor.
//!
//! A capability event is one append-only entry in the unified admission
//! ledger the warm-rebuild replayer reads on boot: a learned negative, a
//! verified/suspect observation, a probe-settled clear, or a
//! reload/boot tombstone that marks a correctness boundary. It rides the
//! single usage-writer actor exactly like a `UsageRecord`, but lands in
//! its own table. This crate has no internal crate dependencies by
//! design, so the row carries only plain types -- the producer normalizes
//! the lane / capability keys and stringifies the verdict, phase, source,
//! and tier before handing the event over.

use rusqlite::Connection;

/// Verdict token stamped on a tombstone row. The read side
/// (`query::latest_tombstone`) carries its own copy of this literal; the
/// two are pinned in agreement by the tombstone round-trip test.
const TOMBSTONE_VERDICT: &str = "tombstone";

/// One `capability_events` row bound for insertion (see
/// `schema::CREATE_CAPABILITY_EVENTS_TABLE`).
///
/// Plain types only -- no dependency on the router / core capability
/// types. `lane_key` / `capability` are expected already NORMALIZED by the
/// producer (a tombstone row carries them empty). `verdict` / `phase` /
/// `source` / `tier` are open-set tokens the replayer parses tolerantly;
/// `tier` is persisted so live-vs-rebuild equivalence can distinguish
/// self-identifying from inferred negatives. `evidence_class` carries the
/// pinned observation tokens and is `None` for rows that have none.
/// `upstream_token` is forensic / display only -- never consulted by
/// admission or replay. `catalog_version` / `overlay_revision` stamp the
/// boundary revision the row was written under. NEVER carries a body /
/// message / prompt.
#[derive(Debug, Clone)]
pub struct CapabilityEvent {
    /// Capture time (epoch milliseconds).
    pub ts: i64,
    /// The NORMALIZED lane key (empty on a tombstone row).
    pub lane_key: String,
    /// The NORMALIZED capability key (empty on a tombstone row).
    pub capability: String,
    /// Open-set admission verdict token (e.g. `"broken"`, `"verified"`,
    /// `"suspect"`, `"cleared"`, `"tombstone"`).
    pub verdict: String,
    /// Open-set phase token identifying the admission stage.
    pub phase: String,
    /// Open-set source token (e.g. `"live"`, `"probe"`).
    pub source: String,
    /// Open-set signal-tier token (e.g. `"self-identifying"`,
    /// `"inferred"`).
    pub tier: String,
    /// The pinned observation-evidence token, or `None`.
    pub evidence_class: Option<String>,
    /// The raw upstream wire token, forensic / display only, or `None`.
    pub upstream_token: Option<String>,
    /// Catalog version the row was stamped under.
    pub catalog_version: i64,
    /// Overlay revision the row was stamped under.
    pub overlay_revision: i64,
}

impl CapabilityEvent {
    /// Build a tombstone: a plain row stamped with the boundary revision,
    /// marking the point past which the replayer trusts the ledger. It
    /// carries the tombstone verdict, empty lane / capability keys, and no
    /// phase / source / tier / evidence (a tombstone is a boundary marker,
    /// not an observation). Its `id` primary key (the rowid alias) is the
    /// boundary key.
    pub fn tombstone(ts: i64, catalog_version: i64, overlay_revision: i64) -> Self {
        Self {
            ts,
            lane_key: String::new(),
            capability: String::new(),
            verdict: TOMBSTONE_VERDICT.to_string(),
            phase: String::new(),
            source: String::new(),
            tier: String::new(),
            evidence_class: None,
            upstream_token: None,
            catalog_version,
            overlay_revision,
        }
    }
}

/// Insert one capability event. Append-only (no dedup / no `OR IGNORE`):
/// each event over the ledger's lifetime is a distinct row and the
/// implicit rowid preserves insertion order. All values are bound
/// parameters. Returns the number of rows inserted (always 1 on success).
pub fn insert_capability_event(
    conn: &Connection,
    e: &CapabilityEvent,
) -> Result<usize, rusqlite::Error> {
    conn.execute(
        INSERT_SQL,
        rusqlite::params![
            e.ts,
            e.lane_key,
            e.capability,
            e.verdict,
            e.phase,
            e.source,
            e.tier,
            e.evidence_class,
            e.upstream_token,
            e.catalog_version,
            e.overlay_revision,
        ],
    )
}

/// The bound `INSERT`. Names every writable column of
/// `schema::CREATE_CAPABILITY_EVENTS_TABLE` (the `id` primary key is
/// auto-assigned, so it is omitted); `?1..?11` positions match the params
/// list above.
const INSERT_SQL: &str = "\
INSERT INTO capability_events (
    ts, lane_key, capability, verdict, phase, source, tier,
    evidence_class, upstream_token, catalog_version, overlay_revision
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11
)";

#[cfg(test)]
#[path = "capability_event_tests.rs"]
mod tests;
