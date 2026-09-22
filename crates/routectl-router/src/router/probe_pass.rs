//! `Router::run_probe_pass`: the one driver-facing entry point that runs the
//! free due batch to completion, then attempts at most one paid probe, and
//! records how the paid half SETTLED.
//!
//! # Why free runs to completion before the paid half is even asked
//!
//! A free settlement is what can turn an identity into the paid candidate
//! the paid half claims from. Running the halves in the other order could
//! attempt a paid probe against a candidate list this same tick was about
//! to populate, so the ordering is structural rather than a tuning choice.
//!
//! # Why this function records only the SETTLEMENT
//!
//! A paid pass can be cancelled at any await -- shutdown drops the whole
//! pass future -- and the milestones it has already passed are irreversible:
//! a claimed candidate is off the list, a committed unit is spent with no
//! refund, and a dispatched call may have reached the upstream. Each of those
//! is therefore counted WHERE IT HAPPENS, by the stage that performs it, so a
//! cancelled pass still reports them. What is left for this function is the
//! terminal outcome, which by definition only exists once the pass returns.
//!
//! # Why the internal pass/failure vocabularies stay internal
//!
//! `PaidProbePass`, `PaidProbeDialOutcome`, and `PaidProbeRefusal` are the
//! dial stage's own closed sets, detailed enough to drive its settlement and
//! its diagnostics. A driver tick needs neither: it logs one pass and reads
//! the scheduler's counters for the rest, so this module translates the
//! dial's outcome into the scheduler's own [`PaidPassSettlement`] before
//! recording it, and hands the caller only [`ProbePassSummary`].

use super::Router;
use super::paid_probe_authorize::PaidProbeRefusal;
use super::paid_probe_dial::{PaidProbeDialOutcome, PaidProbePass};
use crate::probe_scheduler::PaidPassSettlement;

/// What one whole probe pass did, for the driver to log.
///
/// `non_exhaustive` so a later field stays additive for `routectl-cli`, the
/// only outside consumer -- adding one here must never break that build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbePassSummary {
    /// How many free validators this pass ran to settlement.
    pub free_validators_run: usize,
    /// Whether a paid probe was attempted at all: true for every dialed or
    /// refused outcome except an empty candidate list, which claimed
    /// nothing to attempt.
    pub paid_probe_attempted: bool,
}

impl Router {
    /// Run the due free batch to completion, then attempt at most one paid
    /// probe. Called once per driver tick.
    pub async fn run_probe_pass(&self) -> ProbePassSummary {
        let free_validators_run = self.run_due_probes().await;
        let pass = self.run_paid_probe().await;
        let settlement = paid_pass_settlement(&pass);
        if let Some(settlement) = settlement {
            self.probe_scheduler.record_paid_settlement(settlement);
        }
        ProbePassSummary {
            free_validators_run,
            paid_probe_attempted: settlement.is_some(),
        }
    }
}

/// Translate the dial stage's own vocabulary into the scheduler's closed
/// settlement set, or `None` for the one pass that settles nothing: an empty
/// candidate list claimed nothing to settle.
///
/// One exhaustive match, so a new dial-stage variant fails this build rather
/// than landing silently uncounted.
const fn paid_pass_settlement(pass: &PaidProbePass) -> Option<PaidPassSettlement> {
    match pass {
        PaidProbePass::Refused(PaidProbeRefusal::NoCandidate) => None,
        PaidProbePass::Refused(_) => Some(PaidPassSettlement::Refused),
        PaidProbePass::Dialed(PaidProbeDialOutcome::GateDeferred) => {
            Some(PaidPassSettlement::GateDeferred)
        }
        PaidProbePass::Dialed(PaidProbeDialOutcome::Completed) => {
            Some(PaidPassSettlement::Completed)
        }
        PaidProbePass::Dialed(PaidProbeDialOutcome::ProviderFailed(_)) => {
            Some(PaidPassSettlement::ProviderFailed)
        }
        PaidProbePass::Dialed(PaidProbeDialOutcome::TimedOut) => Some(PaidPassSettlement::TimedOut),
    }
}
