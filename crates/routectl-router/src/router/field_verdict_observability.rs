//! Live-visibility surface for acting field verdicts: the process-lifetime
//! repair counters and the per-entry provenance status and doctor read, so a
//! field repair or a durably purged verdict stays observable at INFO without
//! any response-body change.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use routectl_core::{EvidenceSource, FailurePhase, Verdict, sanitize_for_log};
use serde::Serialize;

use crate::field_capability::capability_key_is_catalog_scoped;
use crate::learned_capability::LearnedRegistryEntry;
use crate::state_key::StateKey;

use super::Router;

/// Snapshot of the process-lifetime envelope-field repair counters.
///
/// Read fresh from the live [`Router`] on every call rather than cached, so
/// a status/doctor read never lags the counters it reports.
///
/// `#[non_exhaustive]`: each new observable the field pipeline gains lands
/// here as another counter, so a reader outside this crate never breaks when
/// one is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldRepairCounters {
    /// Cumulative fired envelope-field repairs.
    pub repair_attempted: u64,
    /// The subset of `repair_attempted` that served the request carrying
    /// something other than what the client sent: a drop of a field whose
    /// VALUE was refused, a rewrite of the field to the accepted value the
    /// rejection named, or a client token limit lowered to the named ceiling.
    /// A drop of a field the lane does not take, and a lowered limit routectl
    /// filled in itself, are not lossy.
    ///
    /// Read beside `verdicts_learned`: only the value-refused drop learns
    /// nothing, so a lossy count rising with the attempted count while nothing
    /// is learned is the steady state of a lane refusing a value, not a
    /// fault, and without this count it reads as one.
    pub repair_lossy: u64,
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
    /// Cumulative requests a PRE-FLIGHT rewrite modified before dispatch.
    ///
    /// The count that answers "is pre-flight acting at all", and the one whose
    /// zero an operator must be able to distinguish from an absent field. Read
    /// beside `repair_attempted` rather than instead of it: that one counts the
    /// REACTIVE arm, so a rising pre-flight count against a flat reactive one
    /// means the upstream is no longer seeing the field -- the feature working.
    pub preflight_actions: u64,
    /// Cumulative field-eligible upstream rejections the parser localized no
    /// field path from -- the am-I-flying-blind metric.
    ///
    /// Expected NON-ZERO on real traffic even when everything is healthy: most
    /// caller-shaped 4xxs are not field rejections. What carries the signal is
    /// its ratio against the learned and repaired counts above, which is why it
    /// is reported beside them.
    pub parser_unlocalized: u64,
}

/// Most learned lanes the lossy-repair tally names individually. A lane first
/// seen once this many are named is counted under [`LOSSY_REPAIR_OTHER_LANE`].
///
/// The lane set is operator configuration, but a reload that renames provider
/// entries or upstreams mints new lanes for the life of the process, so the map
/// needs a ceiling of its own. A code constant, never a config knob.
pub(super) const LOSSY_REPAIR_LANE_CAP: usize = 256;

/// The overflow bucket of the lossy-repair tally: lanes past
/// [`LOSSY_REPAIR_LANE_CAP`], and a target with no learned lane. Cannot collide
/// with a real lane, every serialized one of which carries the `#` separator.
pub(super) const LOSSY_REPAIR_OTHER_LANE: &str = "other";

/// The process-lifetime lossy field-repair count, in total and per learned lane.
///
/// Total and map move under ONE lock, so a quiescent read always has the
/// per-lane rows summing to the total.
#[derive(Debug, Default)]
pub(super) struct LossyRepairTally {
    inner: Mutex<LossyRepairCounts>,
}

#[derive(Debug, Default)]
struct LossyRepairCounts {
    total: u64,
    /// Keyed by the serialized lane, unsanitized: the raw form is injective,
    /// a sanitized one is not. Sanitized when projected into rows.
    by_lane: BTreeMap<String, u64>,
    other: u64,
}

impl LossyRepairTally {
    /// Count one lossy repair on `lane`.
    pub(super) fn record(&self, lane: Option<&StateKey>) {
        let mut counts = self.inner.lock();
        counts.total += 1;
        let Some(lane) = lane else {
            counts.other += 1;
            return;
        };
        if let Some(count) = counts.by_lane.get_mut(lane.as_lane_key()) {
            *count += 1;
        } else if counts.by_lane.len() < LOSSY_REPAIR_LANE_CAP {
            counts.by_lane.insert(lane.as_lane_key().to_string(), 1);
        } else {
            counts.other += 1;
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.inner.lock().total
    }

    /// One row per named lane in lane order, then the overflow bucket when it
    /// has counted anything.
    fn rows(&self) -> Vec<LaneLossyRepairs> {
        let counts = self.inner.lock();
        let mut rows: Vec<LaneLossyRepairs> = counts
            .by_lane
            .iter()
            .map(|(lane, &lossy)| LaneLossyRepairs {
                lane: sanitize_for_log(lane),
                lossy,
            })
            .collect();
        if counts.other > 0 {
            rows.push(LaneLossyRepairs {
                lane: LOSSY_REPAIR_OTHER_LANE.to_string(),
                lossy: counts.other,
            });
        }
        rows
    }
}

/// One learned lane's share of the lossy field-repair count.
///
/// `lane` is the `provider_entry#upstream` lane key, `sanitize_for_log`-SANITIZED
/// at construction for the same reason as [`ActingFieldVerdict`]: its only
/// destinations are operator surfaces. Or the overflow token `other`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LaneLossyRepairs {
    /// The sanitized learned lane key, or `other`.
    pub lane: String,
    /// Lossy repairs counted on the lane over the process lifetime.
    pub lossy: u64,
}

impl Router {
    /// The lossy field-repair count split by learned lane: at most
    /// 256 named lanes plus the `other` bucket, whose
    /// counts sum to [`FieldRepairCounters::repair_lossy`] when no repair lands
    /// between the two reads.
    pub fn lossy_repairs_by_lane(&self) -> Vec<LaneLossyRepairs> {
        self.metrics.field_repair_lossy.rows()
    }

    /// Read the live envelope-field repair counters, for the status/doctor
    /// surfaces to log at INFO. `&self` delegate over the private metrics
    /// counters, mirroring [`Router::learned_capability_snapshot`].
    pub fn field_repair_counters(&self) -> FieldRepairCounters {
        let canaries = self.field_verdicts().canaries();
        FieldRepairCounters {
            repair_attempted: self.metrics.field_repair_attempted_total(),
            repair_lossy: self.metrics.field_repair_lossy_total(),
            repair_succeeded: self.metrics.field_repair_succeeded_total(),
            verdicts_learned: self.metrics.field_verdicts_learned_total(),
            outstanding_unconfirmed: canaries.outstanding_unconfirmed_total(),
            disproved_requests: canaries.disproved_requests_total(),
            preflight_actions: self.metrics.field_preflight_actions_total(),
            parser_unlocalized: self.metrics.parser_unlocalized_total(),
        }
    }
}

/// One acting envelope-field verdict: the provenance status/doctor need to
/// explain why a field is currently being soft-tailed for a target.
///
/// Both string fields are `sanitize_for_log`-SANITIZED at construction, and that
/// is a property of this type rather than of its consumers: the value goes
/// straight onto an operator-facing log line, and a control byte there is how a
/// forged second line is injected while an unbounded one prints a whole registry
/// key into a journal. Sanitizing here means no consumer can forget -- and there
/// is more than one consumer.
///
/// SANITIZING IS NOT REDACTING. The filter bounds length and strips
/// non-printables; it does not hide a value. Nothing here needs it to: a state
/// key comes from the operator's own `[providers]` table and a capability key is
/// a normalized token minted from a closed-table path, so neither carries request
/// or response content in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActingFieldVerdict {
    /// The routing state key (provider + model) this verdict applies to,
    /// sanitized.
    pub state_key: String,
    /// The `field:`-namespaced capability key this verdict was minted for,
    /// sanitized.
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
                // Sanitized HERE rather than at each log site: this value's only
                // destination is an operator-facing line, and a second consumer
                // that forgot would reintroduce the hole silently.
                state_key: sanitize_for_log(&entry.state_key),
                feature_key: sanitize_for_log(&entry.feature_key),
                phase,
                source: entry.source,
            }),
            _ => None,
        })
        .collect()
}
