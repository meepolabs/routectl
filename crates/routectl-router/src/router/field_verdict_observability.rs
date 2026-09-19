//! Live-visibility surface for acting field verdicts: the process-lifetime
//! repair counters and the per-entry provenance status and doctor read, so a
//! field repair or a durably purged verdict stays observable at INFO without
//! any response-body change.

use routectl_core::{EvidenceSource, FailurePhase, Verdict};

use crate::field_capability::capability_key_is_catalog_scoped;
use crate::learned_capability::LearnedRegistryEntry;

use super::Router;

/// Snapshot of the process-lifetime envelope-field repair counters.
///
/// Read fresh from the live [`Router`] on every call rather than cached, so
/// a status/doctor read never lags the counters it reports.
///
/// `#[non_exhaustive]`: a growth type -- each new observable the field pipeline
/// gains lands here as another counter, so adding one stays a non-breaking
/// change for readers. Adding one DOES oblige updating the places that enumerate
/// the counters by hand, none of which compilation will force: the status
/// reporter (`log_field_verdict_snapshot` in the CLI), the `rc_field_*` table in
/// `docs/LOGGING.md`, and the CODEMAP passages describing this set
/// (`rg -i field_repair_counters docs/CODEMAP.md` -- several passages describe
/// it, so grep for them rather than trusting a remembered list). Miss them and
/// the counter exists but is never reported and nowhere documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldRepairCounters {
    /// Cumulative fired envelope-field repairs.
    pub repair_attempted: u64,
    /// Cumulative fired repairs whose re-dispatch succeeded.
    pub repair_succeeded: u64,
    /// Cumulative envelope-field verdicts persisted after a confirmed repair.
    pub verdicts_learned: u64,
    /// Requests currently riding on a verdict no canary has re-confirmed yet,
    /// summed over every resident identity -- the OUTSTANDING half of the
    /// wrong-repair alarm.
    ///
    /// Exposure that might later be disproved. Reported beside the lifetime
    /// total below rather than alone, because either number on its own reads as
    /// the other.
    pub outstanding_unconfirmed: u64,
    /// Lifetime count of requests that applied a pre-flight repair a canary
    /// later DISPROVED. Monotonic, and counts REQUESTS AFFECTED rather than
    /// canary attempts: one disproof of a verdict that had repaired forty
    /// requests charges forty.
    pub disproved_requests: u64,
}

impl Router {
    /// Read the live envelope-field repair counters, for the status/doctor
    /// surfaces to log at INFO. `&self` delegate over the private metrics
    /// counters, mirroring [`Router::learned_capability_snapshot`].
    pub fn field_repair_counters(&self) -> FieldRepairCounters {
        let canaries = self.field_verdicts().canaries();
        FieldRepairCounters {
            repair_attempted: self.metrics.field_repair_attempted_total(),
            repair_succeeded: self.metrics.field_repair_succeeded_total(),
            verdicts_learned: self.metrics.field_verdicts_learned_total(),
            outstanding_unconfirmed: canaries.outstanding_unconfirmed_total(),
            disproved_requests: canaries.disproved_requests_total(),
        }
    }
}

/// One acting envelope-field verdict: the provenance status/doctor need to
/// explain why a field is currently being soft-tailed for a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActingFieldVerdict {
    /// The routing state key (provider + model) this verdict applies to.
    pub state_key: String,
    /// The `field:`-namespaced capability key this verdict was minted for.
    pub feature_key: String,
    /// The detection phase that attributed this verdict.
    pub phase: FailurePhase,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    pub source: EvidenceSource,
}

/// Every ACTING (currently soft-tailing) field verdict among `entries`.
///
/// Filters on three conditions: the entry sits in the field namespace --
/// tested through the namespace owner's own predicate rather than a second
/// spelling of the prefix; its verdict is `LearnedBroken`, the only
/// discriminator chain preference and repair actually act on; and its
/// (phase, source) is not the one combination the learned-capability
/// registry's own routing decision treats as advisory-only -- an F3
/// negative sourced from live traffic, which awaits a probe rather than
/// routing away on its own. A verified positive, a cleared
/// row, every catalog-scoped entry, and that advisory F3+Live row are
/// excluded; an F3 negative sourced from a probe carries routing authority
/// and is included like any other acting negative.
pub fn acting_field_verdicts(entries: &[LearnedRegistryEntry]) -> Vec<ActingFieldVerdict> {
    entries
        .iter()
        .filter(|entry| !capability_key_is_catalog_scoped(&entry.feature_key))
        .filter_map(|entry| match entry.verdict {
            Verdict::LearnedBroken(FailurePhase::F3) if entry.source == EvidenceSource::Live => {
                None
            }
            Verdict::LearnedBroken(phase) => Some(ActingFieldVerdict {
                state_key: entry.state_key.clone(),
                feature_key: entry.feature_key.clone(),
                phase,
                source: entry.source,
            }),
            _ => None,
        })
        .collect()
}
