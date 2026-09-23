//! The acknowledged drain for ENVELOPE-FIELD verdict events, and the
//! eligibility advance it authorizes.
//!
//! # Why one event class leaves the best-effort drain
//!
//! Every other capability row is best effort: a dropped one costs the next warm
//! rebuild a little evidence and nothing else. A field verdict's row is different,
//! because that verdict can go on to rewrite client requests BEFORE any rejection
//! -- and every way it can be taken back out of service (the durable clear a
//! disproving canary performs, an operator purge, a later confirmation) is itself a
//! capability-event write. A verdict made pre-flight eligible on a row that never
//! landed is one the next boot cannot see: it would rewrite traffic today and be
//! unexplainable tomorrow.
//!
//! So a field verdict's row rides the ACKNOWLEDGED write path
//! ([`UsageHandle::admit_acknowledged_capability_event`]), and only a committed
//! answer advances the registry's acknowledged confirmation count. That is what
//! makes a newly confirmed reactive repair pre-flight eligible within the SAME
//! process -- no restart -- while an unacknowledged, failed, superseded, or
//! timed-out write advances nothing.
//!
//! # The acknowledgment is NOT awaited on the request path
//!
//! It cannot be, for two independent reasons, and the second is the sharper:
//!
//! - an HTTP handler's future is CANCELLED when the client goes away, and awaiting a
//!   commit is a cancellation point. Cancelling there drops the receipt mid-commit:
//!   the transaction still lands, so the process holds a durable confirmation it
//!   never applied, and the verdict stays dormant until a restart -- silently, and
//!   only for the requests whose clients hung up;
//! - no client response should wait on SQLite for work the client did not ask for.
//!
//! So this function ADMITS the row and hands ownership of the receipt to a
//! daemon-owned task (`server::confirmation_advance`), which awaits the outcome and
//! calls the router's generation- and incarnation-checked acknowledgment. The task is
//! tracked and bounded at shutdown rather than fire-and-forget; abandoning one is
//! safe because the row is already durable and the next boot's ledger replay restores
//! the count.
//!
//! # Every other row is untouched
//!
//! Observations, clears, and non-field learned negatives stay on the best-effort
//! drain. Nothing here widens the acknowledged path beyond the one class whose
//! eligibility depends on it.

use std::sync::Arc;

use routectl_core::capability::Verdict;
use routectl_router::router::CapabilityLearnEvent;
use routectl_router::{DispatchMeta, Router, capability_key_is_field_verdict};
use routectl_usage::{CapabilityEvent, UsageHandle};

use super::usage_capture::epoch_ms_now;
use crate::server::confirmation_advance::{ConfirmationIdentity, ConfirmationTracker};

/// Whether `event` is the one class this module owns: a learned negative on an
/// envelope-field capability key.
///
/// Read through the namespace's OWN published predicate rather than by testing a
/// prefix here: the prefix has exactly one spelling, in the module that owns it,
/// and a second copy could drift and re-partition persisted history.
pub(crate) fn is_field_verdict_event(event: &CapabilityLearnEvent) -> bool {
    capability_key_is_field_verdict(&event.capability_key)
}

/// Admit every field-verdict event on `meta` through the ACKNOWLEDGED path and hand
/// each admitted row to `tracker` for the daemon-owned advancement.
///
/// The row shape is IDENTICAL to the one the best-effort drain builds for a learned
/// negative -- same verdict, phase, source, tier, stamps -- because it is the same
/// fact reaching the same table. What differs is only that this one's outcome is
/// AWAITED, by the daemon rather than by the request.
///
/// SYNCHRONOUS, and that is the whole correction: nothing here awaits, so a cancelled
/// request cannot interrupt it and no response waits on SQLite. The admission itself
/// is a non-blocking `try_send`, so a refusal is known immediately.
///
/// A refused ADMISSION advances nothing and is reported at DEBUG. So is a refused
/// tracker claim, which is what shutdown produces. Both are the fail-safe direction:
/// the verdict stays resident, the reactive arm keeps repairing on rejection, and the
/// next boot's ledger replay restores the count.
///
/// No-op when `meta` carries no field-verdict event, which is the overwhelmingly
/// common case: a request that rejected no envelope field mints none.
pub(crate) fn acknowledge_field_confirmations(
    router: &Arc<Router>,
    usage: &UsageHandle,
    tracker: &ConfirmationTracker,
    meta: &DispatchMeta,
    catalog_version: u32,
    overlay_revision: u64,
) {
    let catalog_version = i64::from(catalog_version);
    let overlay_revision = i64::try_from(overlay_revision).unwrap_or(i64::MAX);
    let ts = epoch_ms_now();
    for ev in meta
        .learned_capabilities
        .iter()
        .filter(|ev| is_field_verdict_event(ev))
    {
        // An UNSTAMPED event is dropped by the best-effort drain's own guard for the
        // same reason, and it must not reach the acknowledged path either: generation
        // zero is never a live generation, so the writer would refuse the row and the
        // advance would be refused after the fact rather than before. Checked here as
        // well because this path does not go through that drain.
        if ev.persistence_generation == 0 {
            continue;
        }
        // THE CLAIM, before the admission. That ordering is what makes shutdown
        // correct: a successful claim means `close_and_wait` is already waiting for
        // this row's advancement, and a refused one means nothing is admitted at all.
        // Admitting first and claiming second would leave a committed row with no
        // task to apply it and nothing accounting for its absence.
        let Some(claim) = tracker.claim() else {
            tracing::debug!(
                "field-verdict confirmation not admitted: the daemon is shutting \
                 down, so the count stays as it is and the next boot's ledger replay \
                 restores it"
            );
            continue;
        };
        let event = CapabilityEvent {
            ts,
            lane_key: ev.state_key.clone(),
            capability: ev.capability_key.clone(),
            verdict: Verdict::LearnedBroken(ev.phase).as_str().to_string(),
            phase: ev.phase.as_str().to_string(),
            source: ev.source.as_str().to_string(),
            tier: ev.signal_tier.as_str().to_string(),
            evidence_class: None,
            upstream_token: None,
            catalog_version,
            overlay_revision,
        };
        match usage.admit_acknowledged_capability_event(
            event,
            ev.persistence_generation,
            ev.incarnation,
        ) {
            // ADMITTED: ownership of the receipt transfers to the daemon's task,
            // which owns the await from here. The claim goes with it, so the slot
            // this counted is the slot that task releases.
            Ok(receipt) => tracker.advance(
                claim,
                Arc::clone(router),
                ConfirmationIdentity {
                    state_key: ev.state_key.clone(),
                    capability_key: ev.capability_key.clone(),
                    provider_kind: ev.provider_kind.clone(),
                    generation: ev.persistence_generation,
                    incarnation: ev.incarnation,
                    observations: ev.observations,
                },
                receipt,
            ),
            // Refused at admission (a saturated channel, capture disabled, a closed
            // writer): nothing was queued, so nothing can become durable and the
            // claim releases on drop.
            Err(refusal) => {
                drop(claim);
                tracing::debug!(
                    outcome = refusal.as_str(),
                    "field-verdict event was not admitted; its confirmation count is \
                     not advanced and pre-flight stays dormant for this identity"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "capability_ack_drain_tests.rs"]
mod tests;
