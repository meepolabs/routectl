//! The RAII lease over one in-flight probe slot, and what releasing one did.
//!
//! Split from the scheduler module so each file holds one concern: the
//! scheduler owns the job table and its transitions, this owns the guard a
//! worker holds while an operation runs. Both halves name `super::` types, so
//! the split is an organizing choice and changes no surface.

use std::time::Instant;

use super::{ProbeScheduler, ProbeSettlement, ProbeValidator};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbePayload;

/// What one [`ProbeScheduler::release`] did.
///
/// `free_spent` is reported by the scheduler rather than decided by the
/// caller because only the settling critical section can know it: it is the
/// answer to "did advancing the cursor run the plan out", and the cursor is
/// shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The settlement did not apply: a retired or cancelled job, or one
    /// already settled and re-leased. Nothing was mutated or scheduled.
    Stale,
    /// The settlement applied.
    Committed {
        /// `true` when this settlement spent the LAST free step, so the paid
        /// class is now a candidate for this identity. Only a
        /// [`ProbeSettlement::SpentFreeStep`] that ran the plan out sets it.
        free_spent: bool,
    },
}

impl ReleaseOutcome {
    /// Whether the settlement applied against current state.
    ///
    /// Test-only: the production worker keys on
    /// [`Self::exhausted_free_plan`], which already implies commitment, so a
    /// separate commitment read has no production caller. The tests need it
    /// to tell "did not commit" from "committed but spent no step" -- two
    /// outcomes the stronger predicate cannot distinguish.
    #[cfg(test)]
    #[must_use]
    pub const fn committed(self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Whether this settlement spent the last free step in the plan.
    #[must_use]
    pub const fn exhausted_free_plan(self) -> bool {
        matches!(self, Self::Committed { free_spent: true })
    }
}

/// RAII lease over one in-flight probe slot.
///
/// [`settle`](Self::settle) is the only way to record an outcome. Dropping
/// the guard unsettled -- an early return, a `?`, a cancelled future, a
/// shutdown -- releases the slot on the same terms, so there is no call
/// site that can forget to.
pub struct ProbeLease<'a> {
    scheduler: &'a ProbeScheduler,
    key: FieldVerdictKey,
    payload: ProbePayload,
    generation: u64,
    validator: ProbeValidator,
    lease_seq: u64,
    settled: bool,
}

impl<'a> ProbeLease<'a> {
    /// Hand a freshly-marked in-flight job to its worker.
    ///
    /// `pub(super)` with private fields rather than a visible struct literal:
    /// the scheduler's leasing critical section is the only place that may mint
    /// one, because a lease minted anywhere else would hold a slot the job
    /// table does not know is held.
    pub(super) const fn new(
        scheduler: &'a ProbeScheduler,
        key: FieldVerdictKey,
        payload: ProbePayload,
        generation: u64,
        validator: ProbeValidator,
        lease_seq: u64,
    ) -> Self {
        Self {
            scheduler,
            key,
            payload,
            generation,
            validator,
            lease_seq,
            settled: false,
        }
    }
}

/// Hand-written: a derive would recurse into the scheduler and render its
/// whole job table, and the key and payload carry operator identifiers and
/// client-supplied beta context. Only closed-set tokens and counters print, and
/// the scheduler lock is never taken from here.
impl std::fmt::Debug for ProbeLease<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeLease")
            .field("validator", &self.validator.as_str())
            .field("field_path", &self.payload.field_path())
            .field("generation", &self.generation)
            .field("lease_seq", &self.lease_seq)
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl ProbeLease<'_> {
    /// The identity this lease runs a probe for.
    #[must_use]
    pub const fn key(&self) -> &FieldVerdictKey {
        &self.key
    }

    /// The router generation the leased job was activated at. Carried so
    /// every observation a probe produces is stamped with the generation
    /// that selected it, never a freshly sampled one.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The validator class this lease runs.
    #[must_use]
    pub const fn validator(&self) -> ProbeValidator {
        self.validator
    }

    /// The bounded capability payload this job was activated with.
    #[must_use]
    pub const fn payload(&self) -> &ProbePayload {
        &self.payload
    }

    /// Settle the lease at `now`, releasing the slot.
    ///
    /// The returned [`ReleaseOutcome`] carries both facts a caller needs and
    /// neither can derive itself: whether the settlement COMMITTED against
    /// current state, and whether it spent the plan's last free step. A
    /// caller may only publish a consequence of the settlement (a paid
    /// candidate, say) after it commits, or a lease settling into a scheduler
    /// that no longer tracks it would leave a candidate describing retired
    /// router state.
    #[must_use]
    pub fn settle(mut self, settlement: ProbeSettlement, now: Instant) -> ReleaseOutcome {
        self.settled = true;
        self.scheduler.release(
            &self.key,
            self.generation,
            self.lease_seq,
            Some(settlement),
            now,
        )
    }
}

impl Drop for ProbeLease<'_> {
    fn drop(&mut self) {
        if !self.settled {
            // No clock is available on a drop path, so the backoff is
            // measured from now. A drop is a cancellation, not a result:
            // the only thing that matters is that the slot comes back and
            // the job cannot immediately re-lease.
            let _outcome = self.scheduler.release(
                &self.key,
                self.generation,
                self.lease_seq,
                None,
                Instant::now(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::super::{ProbePayload, ProbeScheduler, ProbeValidator};
    use crate::field_verdict::FieldVerdictKey;

    fn sentinel_key(state_key: &str, field_path: &str, provider_kind: &str) -> FieldVerdictKey {
        let lane = crate::state_key::StateKey::parse(state_key).expect("a sentinel lane");
        FieldVerdictKey::new(&lane, field_path, provider_kind)
            .expect("a well-formed dotted path mints an identity")
    }

    fn sentinel_payload(
        field_path: &'static str,
        field_value: &str,
        client_beta: &str,
        operator_beta: &str,
    ) -> ProbePayload {
        ProbePayload::new(
            field_path,
            field_value.to_string(),
            &[client_beta.to_string()],
            &[operator_beta.to_string()],
            true,
        )
        .expect("sentinel tokens are within the retention bound")
    }

    #[test]
    fn lease_debug_renders_identity_tokens_and_no_lane_payloads() {
        // Arrange: the own lane is activated first, so it is the one leased;
        // the sibling stays queued in the table behind the scheduler reference.
        let scheduler = ProbeScheduler::new();
        let own_key = sentinel_key(
            "own-state-sentinel#own-seat-sentinel",
            "ownpathsentinel.leaf",
            "own-provider-sentinel",
        );
        let sibling_key = sentinel_key(
            "sibling-state-sentinel#sibling-seat-sentinel",
            "siblingpathsentinel.leaf",
            "sibling-provider-sentinel",
        );
        let own_payload = sentinel_payload(
            "thinking.enabled.display",
            "omitted",
            "own-client-beta-sentinel",
            "own-operator-beta-sentinel",
        );
        let sibling_payload = sentinel_payload(
            "siblingfieldsentinel.display",
            "updates",
            "sibling-client-beta-sentinel",
            "sibling-operator-beta-sentinel",
        );
        scheduler.activate(&own_key, 7, vec![ProbeValidator::CountTokens], own_payload);
        scheduler.activate(
            &sibling_key,
            7,
            vec![ProbeValidator::CountTokens],
            sibling_payload,
        );
        let lease = scheduler
            .lease_due(Instant::now())
            .expect("a queued job is leasable");

        // Act
        let rendered = format!("{lease:?}");

        // Assert
        for sentinel in [
            "own-state-sentinel",
            "own-seat-sentinel",
            "ownpathsentinel",
            "own-provider-sentinel",
            "own-client-beta-sentinel",
            "own-operator-beta-sentinel",
            "omitted",
            "sibling-state-sentinel",
            "sibling-seat-sentinel",
            "siblingpathsentinel",
            "sibling-provider-sentinel",
            "siblingfieldsentinel",
            "sibling-client-beta-sentinel",
            "sibling-operator-beta-sentinel",
            "updates",
            "jobs",
            "tombstones",
        ] {
            assert!(
                !rendered.contains(sentinel),
                "lease debug leaked {sentinel:?}: {rendered}"
            );
        }
        for identity in [
            "ProbeLease",
            "validator: \"count_tokens\"",
            "field_path: \"thinking.enabled.display\"",
            "generation: 7",
            "lease_seq: 1",
            "settled: false",
        ] {
            assert!(
                rendered.contains(identity),
                "lease debug is missing {identity:?}: {rendered}"
            );
        }
    }
}
