//! The CLI-side bridge from the router's paid-probe budget seam to the usage
//! writer that can actually promise a durable unit.
//!
//! The router declares the contract (`routectl_router::PaidProbeLedger`) because
//! it CONSUMES it, and the usage crate owns the actor that can make a
//! reservation atomic and durable. Neither depends on the other, so this crate
//! -- the one that already depends on both and owns the daemon's boot -- is
//! where the two meet. Its sibling warms (`k_rebuild`, `calibration_rebuild`,
//! `ledger_reader`) bridge the same two crates in the read direction for the
//! same reason.
//!
//! WHAT THIS MODULE DOES NOT DO, because each absence is load-bearing:
//!
//! - No timeout, no retry, no second attempt. One reservation is one ask. The
//!   caller owns its own bound, and a retry here could submit a second
//!   reservation whose unit -- once committed -- is never given back.
//! - No refund, release, or decrement. There is no such operation on either
//!   side of the bridge; a committed unit is spent.
//! - No fallback that permits. Every outcome the accounting side can report
//!   maps onto a named router outcome, and only its commit maps onto the one
//!   that authorizes a dial.
//! - No accounting detail crosses back. The UTC day, the storage key, the
//!   channel, and the transaction stay on the usage side; what travels is
//!   whether a unit was committed and where that leaves the budget.
//!
//! FAIL CLOSED BY CONSTRUCTION: nothing here is installed by
//! `Router::new`. Every Router this crate builds carries no ledger and
//! therefore cannot make a paid call at all; the daemon's boot is the one place
//! that installs one, after the writer exists.

use std::sync::Arc;

use routectl_router::{PaidProbeLedger, PaidProbeReservation, Router};
use routectl_usage::{PaidProbeAdmission, PaidProbeCommit, UsageHandle};

/// Reserves paid-probe units through the usage writer.
///
/// Holds a producer handle and nothing else: no connection, no path, no cached
/// count. The handle is the whole capability -- it is `Clone`, cheap, and its
/// admission never blocks -- and keeping the adapter this thin is what makes
/// the reservation's atomicity entirely the writer's property rather than
/// something split across two crates.
///
/// BECAUSE IT HOLDS A PRODUCER HANDLE, IT EXTENDS THE WRITER'S CHANNEL
/// LIFETIME. The writer's drain ends only once every producer clone is gone, so
/// whatever holds this adapter has to be released before the drain -- see the
/// shutdown ordering in `serve_on_listener_with_secrets`.
pub(super) struct UsagePaidProbeLedger {
    usage: UsageHandle,
}

#[async_trait::async_trait]
impl PaidProbeLedger for UsagePaidProbeLedger {
    /// Offer EXACTLY ONE reservation and report what it established.
    ///
    /// `provider` and `daily_cap` travel unchanged: no default substituted for a
    /// cap, no widening, no re-read of configuration, no unit conversion. The
    /// caller gated on that number and the reservation must be checked against
    /// that same number, or the cap enforced and the cap consulted are two
    /// different budgets.
    async fn reserve_paid_probe_unit(
        &self,
        provider: &str,
        daily_cap: u32,
    ) -> PaidProbeReservation {
        reservation_for(self.usage.admit_paid_probe_reservation(provider, daily_cap)).await
    }
}

/// Map one admission -- and, where it was accepted, the receipt it yields --
/// onto the router's outcome vocabulary.
///
/// Exhaustive over both enums with no default arm, so an outcome added on the
/// accounting side cannot fall into something permissive: it lands here as a
/// compile error instead.
async fn reservation_for(admission: PaidProbeAdmission) -> PaidProbeReservation {
    match admission {
        PaidProbeAdmission::Admitted(receipt) => {
            reservation_for_commit(receipt.await_outcome().await)
        }
        // Saturation is LOAD, not a fault, and it is reported mechanism-neutrally:
        // the router's vocabulary must not learn that the accounting side rides a
        // bounded channel.
        PaidProbeAdmission::Saturated => PaidProbeReservation::Overloaded,
        PaidProbeAdmission::Unavailable => PaidProbeReservation::Unavailable,
    }
}

/// Map an acknowledged reservation outcome onto the router's outcome.
///
/// The commit's fields are read through `..` because the accounting side's
/// committing variant is `non_exhaustive`: matching stays possible, while
/// constructing one outside the crate that spent the unit does not -- which is
/// what keeps the value that authorizes money obtainable only from the writer
/// that committed it. This function therefore FORWARDS an authorization and can
/// never mint one.
const fn reservation_for_commit(commit: PaidProbeCommit) -> PaidProbeReservation {
    match commit {
        PaidProbeCommit::Committed { used, cap, .. } => {
            PaidProbeReservation::Committed { used, cap }
        }
        PaidProbeCommit::CapExhausted => PaidProbeReservation::CapExhausted,
        PaidProbeCommit::MalformedState => PaidProbeReservation::MalformedState,
        PaidProbeCommit::WriteFailed => PaidProbeReservation::WriteFailed,
        PaidProbeCommit::Unavailable => PaidProbeReservation::Unavailable,
    }
}

/// A reservation adapter over `usage`.
fn paid_probe_ledger(usage: &UsageHandle) -> Arc<UsagePaidProbeLedger> {
    Arc::new(UsagePaidProbeLedger {
        usage: usage.clone(),
    })
}

/// Install the accounting adapter on the owned Router, before publication.
///
/// Consuming, like the builder it calls, so the installation can only happen
/// while the Router is still owned -- which is to say at boot, before the
/// `ArcSwap` makes it shared. A reload needs no call here at all:
/// `Router::carry_over_learned_from` attaches the outgoing installation to the
/// replacement, so both rebuild paths keep reserving against the one accounting
/// actor. That matters because a committed unit is never refunded, so two
/// installations would be two day budgets whose overspend cannot be undone.
pub(super) fn install_paid_probe_ledger(router: Router, usage: &UsageHandle) -> Router {
    router.with_paid_probe_ledger(paid_probe_ledger(usage))
}

#[cfg(test)]
#[path = "paid_probe_ledger_tests.rs"]
mod paid_probe_ledger_tests;

/// The composed end-to-end proof: the real writer, this module's own adapter,
/// and the daemon's probe pass in ONE path down to a single wire call.
///
/// Its own module rather than an `include!` fragment of the sidecar above: it
/// carries a full rig of its own and the two together would put one compiled
/// module well past the repo's size ceiling. Nothing in it needs the host's
/// fixtures directly -- the live writer and the control-row reads it shares with
/// that sidecar live in `server::test_support`.
#[cfg(test)]
#[path = "paid_probe_end_to_end_tests.rs"]
mod paid_probe_end_to_end_tests;
