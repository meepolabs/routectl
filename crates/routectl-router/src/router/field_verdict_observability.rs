//! Live-visibility surface for acting field verdicts: the process-lifetime
//! repair counters and the per-entry provenance status and doctor read, so a
//! field repair or a durably purged verdict stays observable at INFO without
//! any response-body change.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use routectl_core::{EvidenceSource, FailurePhase, Verdict, sanitize_for_log};
use serde::Serialize;

use crate::field_capability::capability_key_is_catalog_scoped;
use crate::learned_capability::LearnedRegistryEntry;
use crate::state_key::StateKey;

use super::Router;
use super::repair_budget::REPAIRS_PER_REQUEST;

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
    /// Cumulative envelope-field verdicts inserted into the live learned
    /// registry after a confirmed repair. Counts the insert, not the durable
    /// write, which can still be refused.
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
    /// Chained repair steps settled as PROVEN: the repaired retry succeeded, or
    /// was refused for a different field the body carried. One per step, on a
    /// walk that can settle; a non-settling token count proves nothing it can
    /// keep and is not counted.
    ///
    /// Not the same count as `verdicts_learned`, and not a subset of it either
    /// way round: a proven step whose repair teaches nothing (a refused VALUE
    /// repaired by a drop) is committed here and learns no verdict, while
    /// `verdicts_learned` counts only the steps whose commit put a verdict in
    /// the learned registry. Neither claims durability: a verdict counted there
    /// can still fail its durable write, which is reported on the capability
    /// write counters, never here.
    pub chain_steps_committed: u64,
    /// Chained repair steps still pending when their plan ended unproven: the
    /// same field refused again, an unrelated error, a fallback to another
    /// target, or a request that ended there. Each learned nothing. Settling
    /// walks only, like `chain_steps_committed`; the two sum to the steps
    /// applied on settling walks.
    pub chain_steps_dropped: u64,
    /// Rejections a repair proposal answered but the request's shared repair
    /// allowance could not pay for, counted once per rejection. A rejection no
    /// repair proposed for is not counted, empty allowance or not.
    pub budget_exhausted: u64,
}

/// Most learned lanes a per-lane tally names individually. A lane first seen
/// once this many are named is counted under [`LANE_TALLY_OTHER`].
///
/// The lane set is operator configuration, but a reload that renames provider
/// entries or upstreams mints new lanes for the life of the process, so the map
/// needs a ceiling of its own. A code constant, never a config knob.
pub(super) const LANE_TALLY_CAP: usize = 256;

/// The overflow bucket of a per-lane tally: lanes past [`LANE_TALLY_CAP`], and
/// a target with no learned lane. Cannot collide with a real lane, every
/// serialized one of which carries the `#` separator.
pub(super) const LANE_TALLY_OTHER: &str = "other";

/// A process-lifetime count, in total and per learned lane.
///
/// Total and map move under ONE lock, so a quiescent read always has the
/// per-lane rows summing to the total.
#[derive(Debug, Default)]
pub(super) struct LaneTally {
    inner: Mutex<LaneCounts>,
}

#[derive(Debug, Default)]
struct LaneCounts {
    total: u64,
    /// Keyed by the serialized lane, unsanitized: the raw form is injective,
    /// a sanitized one is not. Sanitized when projected into rows.
    by_lane: BTreeMap<String, u64>,
    other: u64,
}

impl LaneTally {
    /// Count one event on `lane`.
    pub(super) fn record(&self, lane: Option<&StateKey>) {
        let mut counts = self.inner.lock();
        counts.total += 1;
        let Some(lane) = lane else {
            counts.other += 1;
            return;
        };
        if let Some(count) = counts.by_lane.get_mut(lane.as_lane_key()) {
            *count += 1;
        } else if counts.by_lane.len() < LANE_TALLY_CAP {
            counts.by_lane.insert(lane.as_lane_key().to_string(), 1);
        } else {
            counts.other += 1;
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.inner.lock().total
    }

    /// One `(sanitized lane, count)` per named lane in lane order, then the
    /// overflow bucket when it has counted anything.
    fn rows(&self) -> Vec<(String, u64)> {
        let counts = self.inner.lock();
        let mut rows: Vec<(String, u64)> = counts
            .by_lane
            .iter()
            .map(|(lane, &count)| (sanitize_for_log(lane), count))
            .collect();
        if counts.other > 0 {
            rows.push((LANE_TALLY_OTHER.to_string(), counts.other));
        }
        rows
    }
}

/// The settlement exits of chained field-repair steps, counted on settling
/// plans only.
#[derive(Debug, Default)]
pub(super) struct ChainStepTally {
    committed: AtomicU64,
    dropped: AtomicU64,
}

impl ChainStepTally {
    pub(super) fn add_committed(&self, steps: usize) {
        self.committed.fetch_add(steps as u64, Ordering::Relaxed);
    }

    pub(super) fn add_dropped(&self, steps: usize) {
        self.dropped.fetch_add(steps as u64, Ordering::Relaxed);
    }

    fn committed(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Buckets of the repairs-per-request histogram: one per possible draw count,
/// zero through the whole per-request allowance.
pub(super) const REPAIR_DRAW_BUCKETS: usize = REPAIRS_PER_REQUEST as usize + 1;

/// How many repair draws each finished request spent, one bucket per count.
#[derive(Debug, Default)]
pub(super) struct RepairDrawHistogram {
    buckets: [AtomicU64; REPAIR_DRAW_BUCKETS],
}

impl RepairDrawHistogram {
    /// Count one finished request that spent `draws` repairs.
    pub(super) fn record(&self, draws: u8) {
        let bucket = usize::from(draws).min(REPAIR_DRAW_BUCKETS - 1);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect()
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

/// One learned lane's share of the repair-budget exhaustion count.
///
/// `lane` is sanitized at construction like [`LaneLossyRepairs::lane`], or the
/// overflow token `other`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LaneBudgetExhaustions {
    /// The sanitized learned lane key, or `other`.
    pub lane: String,
    /// Rejections on the lane a repair was proposed for and the request's
    /// allowance could not pay, over the process lifetime.
    pub exhausted: u64,
}

impl Router {
    /// The lossy field-repair count split by learned lane: at most
    /// 256 named lanes plus the `other` bucket, whose
    /// counts sum to [`FieldRepairCounters::repair_lossy`] when no repair lands
    /// between the two reads.
    pub fn lossy_repairs_by_lane(&self) -> Vec<LaneLossyRepairs> {
        self.metrics
            .field_repair_lossy
            .rows()
            .into_iter()
            .map(|(lane, lossy)| LaneLossyRepairs { lane, lossy })
            .collect()
    }

    /// The repair-budget exhaustion count split by learned lane: at most 256
    /// named lanes plus the `other` bucket, whose counts sum to
    /// [`FieldRepairCounters::budget_exhausted`] when no rejection lands
    /// between the two reads.
    pub fn budget_exhaustions_by_lane(&self) -> Vec<LaneBudgetExhaustions> {
        self.metrics
            .repair_budget_exhausted
            .rows()
            .into_iter()
            .map(|(lane, exhausted)| LaneBudgetExhaustions { lane, exhausted })
            .collect()
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
            chain_steps_committed: self.metrics.field_chain_steps.committed(),
            chain_steps_dropped: self.metrics.field_chain_steps.dropped(),
            budget_exhausted: self.metrics.repair_budget_exhausted.total(),
        }
    }

    /// Finished requests by how many repairs each spent: index `n` counts the
    /// requests that drew exactly `n`, from zero up to the whole per-request
    /// allowance, so the list has one more entry than that allowance. Recorded
    /// once per request on every walk.
    pub fn repairs_per_request(&self) -> Vec<u64> {
        self.metrics.repairs_per_request.snapshot()
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
