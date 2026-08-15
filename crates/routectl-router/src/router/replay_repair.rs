//! Reasoning-replay carry admission and the strip-repair settlement, the
//! router half of the two-phase learned lifecycle.
//!
//! Before a target is dispatched, [`Router::plan_replay_carry`] claims the
//! per-pair single-flight slot for every non-portable artifact scheme the
//! request carries toward the target's lane:
//!
//! - Admitted (absent or lapsed negative) -- the artifacts stay on the
//!   attempt (the optimistically CARRIED variant) and the guards ride back
//!   in a [`ReplayCarryPlan`] for the dispatch arm to settle.
//! - Refused (an acting negative is resident, or a concurrent probe already
//!   carries the pair) -- the artifacts are stripped in place BEFORE
//!   dispatch and no plan is returned. This is the proactive strip that a
//!   persisted negative or a peer probe forces; the target then dispatches
//!   the stripped variant with no carried attempt.
//!
//! The dispatch arm drives the second phase as a FIXED correctness branch:
//! when the carried variant draws the proven replay rejection it switches to
//! the pre-stripped variant and re-dispatches the SAME target exactly once,
//! then settles the plan -- [`ReplayCarryPlan::commit`] on a successful
//! stripped repair, release (explicit or by drop) on a repeat rejection or
//! any unrelated failure.
//!
//! The strip and carry decisions read the deterministic scheme ladder
//! ([`is_replayable`] over [`scheme_of`]), never any client-supplied claim,
//! so a hostile or incompatible pairing is stripped regardless of what an
//! envelope asserted.
//!
//! Both strip moments -- the proactive one here and the on-retry one in the
//! dispatch arm -- go through [`strip_replay_artifacts_recalibrating`], which
//! also re-stamps the calibration estimate so the evidence numerator always
//! describes the bytes that actually went upstream.

use std::time::Instant;

use routectl_core::capability::REASONING_REPLAY;
use routectl_core::failure_class::{FailureClass, ReplayAttempt};
use routectl_core::{ChatRequest, ReplayScheme, Replayability, is_replayable, scheme_of};

use crate::capability_strip::strip_replay_artifacts;
use crate::context_trim::estimate_total_tokens;
use crate::learned_replay::{ReplayLearnKey, ReplayProbeGuard};

use super::{CapabilityClearedEvent, CapabilityLearnEvent, DispatchMeta, DispatchTarget, Router};

/// The carried-artifact admission for one dispatch target: the single-flight
/// guards held while the optimistically-carried variant is in flight, the
/// target lane the repair strips against, and the count of carried
/// non-portable artifacts the classifier gates its replay-rejection lift on.
pub(super) struct ReplayCarryPlan<'a> {
    lane: ReplayScheme,
    gray_count: usize,
    schemes: Vec<ReplayScheme>,
    guards: Vec<ReplayProbeGuard<'a>>,
}

impl ReplayCarryPlan<'_> {
    /// The target lane the stripped repair removes rejected artifacts against.
    pub(super) const fn lane(&self) -> ReplayScheme {
        self.lane
    }

    /// The distinct source schemes of the carried non-portable artifacts,
    /// first-seen order -- closed-set tokens for the degradation WARN.
    pub(super) fn source_schemes(&self) -> &[ReplayScheme] {
        &self.schemes
    }

    /// The count of carried non-portable reasoning artifacts -- the
    /// degradation WARN's artifact count.
    pub(super) const fn artifact_count(&self) -> usize {
        self.gray_count
    }

    /// The replay signal to hand the classifier: the count of non-portable
    /// artifacts this attempt carried, so a proven replay rejection lifts to
    /// [`FailureClass::FeatureUnsupported`] instead of a plain bad request.
    pub(super) const fn attempt(&self) -> ReplayAttempt {
        ReplayAttempt::with_gray_artifacts(self.gray_count)
    }

    /// Phase two after a successful stripped repair: persist the negative for
    /// every carried pair and return the emission rows for
    /// `meta.learned_capabilities`.
    pub(super) fn commit(
        self,
        upstream_status: u16,
        request_features: &[String],
        now: Instant,
    ) -> Vec<CapabilityLearnEvent> {
        self.guards
            .into_iter()
            .map(|guard| guard.commit(upstream_status, request_features.to_vec(), now))
            .collect()
    }

    /// Phase two after the carried (unstripped) variant succeeded upstream:
    /// the pair replays cleanly, so drop any resident (lapsed) negative for
    /// every carried pair and return the cleared-event rows for
    /// `meta.cleared_capabilities`, so the ledger records each clear and a warm
    /// rebuild does not resurrect the negative. A pair that had no resident
    /// entry (admitted for its first probe) clears nothing and emits no row.
    ///
    /// An unrelated failure instead settles by DROP: the plan is never
    /// `take`n on an error path, so each guard's `Drop` releases its slot
    /// without touching the resident entry.
    pub(super) fn settle_success(self) -> Vec<CapabilityClearedEvent> {
        self.guards
            .into_iter()
            .filter_map(ReplayProbeGuard::clear)
            .collect()
    }
}

/// Strip the lane-rejected reasoning artifacts off a per-attempt request AND
/// bring the calibration estimate back in line with the payload that will
/// actually go upstream. THE dispatch-path entry point for the strip: every
/// site that mutates a live `attempt_req` goes through here.
///
/// `record_would_trim` stamps `meta.calib_estimated_tokens` once, before the
/// retry loop, from the request as it stood then. A strip -- proactive
/// (a resident negative or a peer probe) or on-retry (the strip repair) --
/// makes the dispatched payload SMALLER than that stamp describes, while the
/// provider's reported prompt total reflects the smaller payload. Left
/// uncorrected the evidence ratio comes out too low, and a low correction
/// factor shrinks a corrected estimate until the window gate admits requests
/// the static estimate had correctly judged too large.
///
/// The re-estimate runs ONLY when the strip actually removed something, so
/// the overwhelming majority of dispatches -- which carry no artifact the
/// lane rejects -- pay no extra serialization. Taking `meta` as a required
/// parameter is deliberate: a future strip site cannot be added on this path
/// without deciding what happens to the estimate.
pub(super) fn strip_replay_artifacts_recalibrating(
    attempt_req: &mut ChatRequest,
    lane: ReplayScheme,
    meta: &mut DispatchMeta,
) -> bool {
    if !strip_replay_artifacts(attempt_req, lane) {
        return false;
    }
    meta.calib_estimated_tokens = Some(estimate_total_tokens(attempt_req));
    true
}

impl Router {
    /// Whether an effective failure class is the proven reasoning-replay
    /// rejection this arm repairs.
    pub(super) fn is_replay_rejection_class(class: &FailureClass) -> bool {
        matches!(
            class,
            FailureClass::FeatureUnsupported { capability } if capability == REASONING_REPLAY
        )
    }

    /// Claim the carry slot for every non-portable artifact scheme the
    /// request carries toward `target`'s lane. See the module docs for the
    /// admit / proactive-strip split. Returns `None` (leaving `attempt_req`
    /// carried as-is) when the lane is unestablished or no non-portable
    /// artifact is present, and `None` (after stripping `attempt_req` in
    /// place, and re-stamping the calibration estimate to match the stripped
    /// payload) when any pair is acting-negative or already under a peer
    /// probe.
    pub(super) fn plan_replay_carry<'a>(
        &'a self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        now: Instant,
    ) -> Option<ReplayCarryPlan<'a>> {
        let lane = target.provider.as_ref()?.replay_lane();
        if lane == ReplayScheme::Gray {
            return None;
        }
        let schemes = nonportable_schemes(attempt_req, lane);
        if schemes.is_empty() {
            return None;
        }
        let provider_kind = target.provider_kind.unwrap_or("");
        let mut guards: Vec<ReplayProbeGuard<'a>> = Vec::with_capacity(schemes.len());
        for &scheme in &schemes {
            let key = ReplayLearnKey::new(&target.provider_name, provider_kind, lane, scheme);
            match self.learned_replay().admit_provisional(&key, now) {
                Some(guard) => guards.push(guard),
                None => {
                    // Acting negative or peer probe: carry nothing. Dropping
                    // the guards already taken releases their slots without
                    // learning, then strip proactively before dispatch.
                    drop(guards);
                    strip_replay_artifacts_recalibrating(attempt_req, lane, meta);
                    return None;
                }
            }
        }
        let gray_count = nonportable_count(attempt_req, lane);
        Some(ReplayCarryPlan {
            lane,
            gray_count,
            schemes,
            guards,
        })
    }
}

/// Count the carried reasoning artifacts the lane's validator does not prove
/// portable (proven-incompatible or unestablished) -- the classifier's
/// gray-artifact signal.
fn nonportable_count(req: &ChatRequest, lane: ReplayScheme) -> usize {
    req.messages
        .iter()
        .flat_map(|message| message.reasoning_details.iter())
        .filter(|detail| !is_portable(detail.format.as_deref(), lane))
        .count()
}

/// The distinct schemes of the carried non-portable artifacts, in
/// first-seen order -- one learned pair per scheme.
fn nonportable_schemes(req: &ChatRequest, lane: ReplayScheme) -> Vec<ReplayScheme> {
    let mut out: Vec<ReplayScheme> = Vec::new();
    for message in req.messages.iter() {
        for detail in &message.reasoning_details {
            let scheme = scheme_of(detail.format.as_deref());
            if !is_portable(detail.format.as_deref(), lane) && !out.contains(&scheme) {
                out.push(scheme);
            }
        }
    }
    out
}

fn is_portable(format: Option<&str>, lane: ReplayScheme) -> bool {
    matches!(is_replayable(scheme_of(format), lane), Replayability::Carry)
}

#[cfg(test)]
#[path = "replay_repair_tests.rs"]
mod replay_repair_tests;

#[cfg(test)]
#[path = "replay_strip_calibration_tests.rs"]
mod replay_strip_calibration_tests;
