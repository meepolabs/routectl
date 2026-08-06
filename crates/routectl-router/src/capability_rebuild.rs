//! Warm rebuild of the learned-capability registry from the usage ledger.
//!
//! On a fresh process start the in-memory [`LearnedCapabilityRegistry`] is
//! empty, so every capability the router previously learned as unsupported
//! (or confirmed working) would have to be re-learned from live traffic. This
//! module replays a bounded slice of the persisted capability-event ledger
//! back through the SAME stage-2 admission calls the live path uses, so the
//! registry answers from history immediately after boot.
//!
//! The ledger lives in a leaf crate this crate does not depend on, so the
//! read is expressed through the [`CapabilityLedgerReader`] dependency-
//! inversion seam (mirroring the K estimator's `LedgerReader`): a concrete
//! reader bridging the usage crate to the router-side row type is injected
//! from the binary that owns both clocks and both crates. That bridge owns
//! the wall-clock-to-`Instant` map, the row cap, and writing the boot
//! tombstone; this module owns replay only.
//!
//! # Replay boundary
//!
//! A tombstone row marks the correctness boundary: only events after it are
//! replayed. Which events survive is owned by the pure [`should_replay`]
//! seam so the replay loop stays oblivious. Survivors are replayed
//! oldest-first; two events sharing an instant tie-break by `rowid`
//! (insertion order), so the negative-then-cleared ordering is
//! deterministic.

use std::time::Instant;

use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, is_known_evidence_class,
};

use crate::learned_capability::LearnedCapabilityRegistry;

/// The replay boundary: the latest tombstone's ledger `rowid` plus the
/// catalog/overlay revision it was stamped with. Events at or before
/// `rowid` predate the boundary; events carrying a different revision are
/// stragglers an old router appended after a reload swap and are not this
/// boot's truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayTombstone {
    /// Implicit ledger rowid of the tombstone row -- the boundary key.
    pub rowid: i64,
    /// Baked catalog version the tombstone was stamped with.
    pub catalog_version: u32,
    /// Catalog-overlay revision the tombstone was stamped with.
    pub overlay_revision: u64,
}

impl ReplayTombstone {
    /// Construct a boundary descriptor from the mapped tombstone columns.
    pub const fn new(rowid: i64, catalog_version: u32, overlay_revision: u64) -> Self {
        Self {
            rowid,
            catalog_version,
            overlay_revision,
        }
    }
}

/// One capability-event ledger row the rebuild consumes, in router-side
/// terms. The verdict/phase/source/tier/evidence-class fields carry the
/// RAW persisted tokens: replay parses them here so an unrecognized token
/// skips the row rather than crashing the boot (open-set tolerance at the
/// rebuild boundary).
///
/// `#[non_exhaustive]` so a later increment can carry an additional column
/// without a breaking change; the cross-crate reader builds rows through
/// [`CapabilityEventRow::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEventRow {
    /// Implicit ledger rowid -- the insertion-order identity used for the
    /// replay boundary and same-instant tie-break.
    pub rowid: i64,
    /// Monotonic instant the event maps to, produced by the bridge's
    /// wall-clock-to-`Instant` clock map. Replay feeds it as `now`.
    pub observed_at: Instant,
    /// Raw persisted verdict token (`verified`/`broken`/`suspect`/
    /// `cleared`/...).
    pub verdict: String,
    /// Raw persisted phase token, absent for data-free verdicts.
    pub phase: Option<String>,
    /// Raw persisted evidence-source token (`live`/`probe`).
    pub source: String,
    /// Raw persisted signal-tier token, absent for events that carry none.
    pub tier: Option<String>,
    /// Raw persisted evidence-class token; forensic/display fidelity only,
    /// not consulted by replay.
    pub evidence_class: Option<String>,
    /// Normalized capability key.
    pub capability: String,
    /// Breaker state key (nickname-or-provider) the event was recorded for.
    pub state_key: String,
    /// Provider-kind token, used to normalize the capability key on replay.
    pub provider_kind: String,
    /// Baked catalog version in force when the event was written.
    pub catalog_version: u32,
    /// Catalog-overlay revision in force when the event was written.
    pub overlay_revision: u64,
}

impl CapabilityEventRow {
    /// Construct a row from the mapped ledger columns. Provided because the
    /// type is `#[non_exhaustive]`: the concrete [`CapabilityLedgerReader`]
    /// lives in a different crate and cannot use a struct literal.
    // The row mirrors a wide ledger schema one-to-one; grouping the columns
    // into sub-structs would only leak intermediate types across the crate
    // boundary the reader spans.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        rowid: i64,
        observed_at: Instant,
        verdict: String,
        phase: Option<String>,
        source: String,
        tier: Option<String>,
        evidence_class: Option<String>,
        capability: String,
        state_key: String,
        provider_kind: String,
        catalog_version: u32,
        overlay_revision: u64,
    ) -> Self {
        Self {
            rowid,
            observed_at,
            verdict,
            phase,
            source,
            tier,
            evidence_class,
            capability,
            state_key,
            provider_kind,
            catalog_version,
            overlay_revision,
        }
    }
}

/// Dependency-inversion seam between the usage ledger and the learned
/// registry. The concrete implementation lives in the binary that depends
/// on both the usage crate and the router crate; this crate (and its tests)
/// only ever sees the trait. `Send + Sync` so a caller may hold one behind
/// an `Arc`.
pub trait CapabilityLedgerReader: Send + Sync {
    /// The replay boundary, or `None` when the ledger carries no tombstone.
    /// A missing boundary is fail-closed: [`rebuild_capabilities_into`]
    /// replays nothing and the caller writes a fresh boot tombstone.
    fn tombstone(&self) -> Option<ReplayTombstone>;

    /// Every candidate event row in any order; the rebuild filters through
    /// `should_replay` and sorts oldest-first itself.
    fn read_events(&self) -> Vec<CapabilityEventRow>;
}

/// Whether one event survives the replay boundary. The single pure owner of
/// "which events survive": the rebuild loop is oblivious. Post-tombstone
/// survival is deliberately NOT unconditional -- a straggler carrying a
/// revision other than the boundary's is skipped -- so no caller may assume
/// an import clears everything after the tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayDecision {
    Replay,
    Skip,
}

/// Pure replay-boundary rule. Skips rows at or before the tombstone rowid,
/// and post-tombstone stragglers whose stamped revision differs from the
/// boundary's (an old router can append stale-revision events after a
/// tombstone during a reload swap).
pub const fn should_replay(
    event: &CapabilityEventRow,
    tombstone: &ReplayTombstone,
) -> ReplayDecision {
    if event.rowid <= tombstone.rowid {
        return ReplayDecision::Skip;
    }
    if event.catalog_version != tombstone.catalog_version
        || event.overlay_revision != tombstone.overlay_revision
    {
        return ReplayDecision::Skip;
    }
    ReplayDecision::Replay
}

/// Per-rebuild tally, surfaced for boot observability and pinned by tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityRebuildSummary {
    /// `verified` events replayed as positive observations.
    pub replayed_verified: usize,
    /// `broken`/`suspect` events replayed as negative observations.
    pub replayed_negative: usize,
    /// `cleared` events that removed a resident entry.
    pub replayed_cleared: usize,
    /// `cleared` events with no resident entry to remove (a no-op, e.g. a
    /// cleared event replayed before its negative).
    pub cleared_noop: usize,
    /// Probe-source events replayed through the shared admission arms. A
    /// by-source tally bumped ALONGSIDE the by-verdict counter each probe
    /// row hits -- a probe `broken` row bumps both `replayed_probe` and
    /// `replayed_negative`.
    pub replayed_probe: usize,
    /// Events skipped because a token (source/verdict/tier/phase) was not
    /// recognized.
    pub skipped_unknown: usize,
}

/// Replay a ledger slice into `registry` through the live stage-2 admission
/// calls. Reads the boundary and rows via `reader`, keeps the survivors,
/// replays them oldest-first (same-instant rows tie-break by rowid), and
/// returns the tally. A missing tombstone replays nothing (fail-closed).
pub fn rebuild_capabilities_into(
    reader: &dyn CapabilityLedgerReader,
    registry: &LearnedCapabilityRegistry,
) -> CapabilityRebuildSummary {
    let mut summary = CapabilityRebuildSummary::default();
    let Some(tombstone) = reader.tombstone() else {
        return summary;
    };

    let mut rows: Vec<CapabilityEventRow> = reader
        .read_events()
        .into_iter()
        .filter(|row| matches!(should_replay(row, &tombstone), ReplayDecision::Replay))
        .collect();
    rows.sort_by(|a, b| {
        a.observed_at
            .cmp(&b.observed_at)
            .then(a.rowid.cmp(&b.rowid))
    });

    for row in &rows {
        replay_row(row, registry, &mut summary);
    }
    summary
}

/// Replay one surviving row through the matching admission call. The parsed
/// evidence source is threaded into the admission so probe and live rows
/// share the same arms; an unrecognized source token skips the row with a
/// counter. Nothing here panics.
fn replay_row(
    row: &CapabilityEventRow,
    registry: &LearnedCapabilityRegistry,
    summary: &mut CapabilityRebuildSummary,
) {
    let Some(source) = EvidenceSource::parse(&row.source) else {
        tracing::warn!(
            event = "rebuild_skip",
            reason = "unknown_source",
            source = %row.source,
            "capability rebuild skipped a row with an unrecognized source token",
        );
        summary.skipped_unknown += 1;
        return;
    };

    // The verdict tokens mirror `Verdict::as_str`; the catch-all arm is the
    // open-set skip.
    match row.verdict.as_str() {
        "verified" => {
            // The live positive path always stamps a recognized evidence
            // class; a missing or unrecognized one is malformed -- fail
            // closed rather than mint a positive on unattributable evidence.
            if !evidence_class_recognized(row) {
                skip_unknown_evidence_class(row, summary);
                return;
            }
            registry.observe_positive(
                &row.state_key,
                &row.capability,
                &row.provider_kind,
                source,
                row.observed_at,
            );
            summary.replayed_verified += 1;
            bump_probe(source, summary);
        }
        "broken" => {
            if let Some((tier, phase)) = parse_tier_phase(row, summary) {
                mint_negative(row, registry, tier, phase, source, summary);
            }
        }
        "suspect" => {
            // A suspect-absence negative is a positive-detection (F3) signal
            // and always carries a recognized evidence class on the live
            // path. Enforce both on replay: any other phase, or an
            // absent/unrecognized class, is malformed -- skip, fail closed.
            if !evidence_class_recognized(row) {
                skip_unknown_evidence_class(row, summary);
                return;
            }
            let Some((tier, phase)) = parse_tier_phase(row, summary) else {
                return;
            };
            if phase != FailurePhase::F3 {
                tracing::warn!(
                    event = "rebuild_skip",
                    reason = "unexpected_suspect_phase",
                    phase = %phase.as_str(),
                    "capability rebuild skipped a suspect row whose phase was not f3",
                );
                summary.skipped_unknown += 1;
                return;
            }
            mint_negative(row, registry, tier, phase, source, summary);
        }
        "cleared" => {
            if registry.remove_keyed(&row.state_key, &row.capability, &row.provider_kind) {
                summary.replayed_cleared += 1;
            } else {
                summary.cleared_noop += 1;
            }
            bump_probe(source, summary);
        }
        other => {
            tracing::warn!(
                event = "rebuild_skip",
                reason = "unknown_verdict",
                verdict = %other,
                "capability rebuild skipped a row with an unrecognized verdict token",
            );
            summary.skipped_unknown += 1;
        }
    }
}

/// Bump the by-source probe tally when the replayed row carried probe
/// evidence. Called alongside each by-verdict counter so a probe row is
/// counted once per arm it reaches.
const fn bump_probe(source: EvidenceSource, summary: &mut CapabilityRebuildSummary) {
    if matches!(source, EvidenceSource::Probe) {
        summary.replayed_probe += 1;
    }
}

/// Whether the row carries a recognized (and present) evidence-class token.
/// A `NULL` class fails the check: the verdicts that reach this predicate
/// (`verified`/`suspect`) always stamp one on the live path.
fn evidence_class_recognized(row: &CapabilityEventRow) -> bool {
    row.evidence_class
        .as_deref()
        .is_some_and(is_known_evidence_class)
}

/// Record the shared WARN + counter for a row skipped on its evidence class.
fn skip_unknown_evidence_class(row: &CapabilityEventRow, summary: &mut CapabilityRebuildSummary) {
    tracing::warn!(
        event = "rebuild_skip",
        reason = "unknown_evidence_class",
        verdict = %row.verdict,
        "capability rebuild skipped a row with a missing or unrecognized evidence class",
    );
    summary.skipped_unknown += 1;
}

/// Parse the tier and phase a negative observation needs, warning and
/// counting a skip on the first missing or unrecognized token. `None` means
/// the caller must skip the row.
fn parse_tier_phase(
    row: &CapabilityEventRow,
    summary: &mut CapabilityRebuildSummary,
) -> Option<(SignalTier, FailurePhase)> {
    let Some(tier) = row.tier.as_deref().and_then(SignalTier::parse) else {
        tracing::warn!(
            event = "rebuild_skip",
            reason = "unknown_tier",
            verdict = %row.verdict,
            "capability rebuild skipped a negative row with a missing or unrecognized tier",
        );
        summary.skipped_unknown += 1;
        return None;
    };
    let Some(phase) = row.phase.as_deref().and_then(FailurePhase::parse) else {
        tracing::warn!(
            event = "rebuild_skip",
            reason = "unknown_phase",
            verdict = %row.verdict,
            "capability rebuild skipped a negative row with a missing or unrecognized phase",
        );
        summary.skipped_unknown += 1;
        return None;
    };
    Some((tier, phase))
}

/// Replay a parsed negative through the shared admission call, attributing
/// the evidence source the row carried.
fn mint_negative(
    row: &CapabilityEventRow,
    registry: &LearnedCapabilityRegistry,
    tier: SignalTier,
    phase: FailurePhase,
    source: EvidenceSource,
    summary: &mut CapabilityRebuildSummary,
) {
    registry.observe(
        &row.state_key,
        &row.capability,
        &row.provider_kind,
        tier,
        phase,
        source,
        row.observed_at,
    );
    summary.replayed_negative += 1;
    bump_probe(source, summary);
}

#[cfg(test)]
#[path = "capability_rebuild_tests.rs"]
mod tests;
