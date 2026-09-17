//! The capability replay boundary a revision-changing reload must commit.
//!
//! # Why a reload has to WRITE to preserve state
//!
//! The cold-boot replayer reads only rows appended after the newest
//! tombstone. So moving the boundary is not a neutral bookkeeping act: every
//! row before the new tombstone becomes invisible to every later boot,
//! whatever the replay predicate would have decided about it.
//!
//! That makes "carry the catalog-independent entries across the reload" a
//! half-measure on its own. The in-memory registry keeps them, so they act for
//! the rest of the process's life -- and then vanish at the next restart, when
//! the replayer starts after a tombstone their original rows sit before. The
//! symptom is a verdict that survives every reload and disappears at a restart
//! weeks later: a warning that quietly stops appearing.
//!
//! The fix is to RE-APPEND them past the new boundary. A reload that moves the
//! boundary therefore commits one batch:
//!
//! 1. the new tombstone, stamped this reload's revision, then
//! 2. a restatement of every carried catalog-independent entry.
//!
//! # Why the batch must be atomic and acknowledged
//!
//! The two halves are one indivisible fact. A committed tombstone whose
//! restatements were dropped is strictly WORSE than writing nothing: it
//! evicts exactly the verdicts the batch existed to preserve. So the rows go
//! through one transaction
//! ([`routectl_usage::insert_capability_events_atomic`]), and the caller waits
//! for the acknowledgement -- publishing a router whose verdicts the next boot
//! cannot see is the failure being prevented, so the swap must not happen
//! until the boundary is durable.
//!
//! A restatement re-states an EXISTING fact, so each row is stamped from the
//! entry's own `last_seen`, never from the reload instant: stamping "now"
//! would silently extend every wire-shape verdict's life on every config
//! reload, and a verdict could never lapse on a daemon that reloads
//! regularly.

use std::sync::Arc;

use routectl_router::{LearnedCapabilityRegistry, Router};
use routectl_usage::{BatchCommit, BatchReceipt, CapabilityEvent, UsageHandle};

/// The outcome of committing a reload's replay boundary.
///
/// `Failed` carries the underlying [`BatchCommit`] so the caller can log WHICH
/// way it failed (an unavailable writer, a full channel, a DB fault) without
/// re-deriving it. `Abandoned` is shutdown winning the race against the wait:
/// no verdict on the write, and deliberately nothing published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundaryOutcomeReport {
    /// The tombstone and every survivor restatement are durable, the
    /// generation advanced, and the catalog-scoped entries were evicted.
    Committed {
        /// Number of catalog-independent entries restated past the boundary.
        survivors: usize,
        /// The generation now active on the shared registry.
        generation: u64,
        /// Catalog-scoped entries evicted by the transition.
        pruned: usize,
    },
    /// Nothing durable was written; the caller must keep the previous router
    /// active rather than publish one whose verdicts a restart cannot see.
    Failed(BatchCommit),
    /// Shutdown fired while the admitted batch was still in flight. The rows
    /// may or may not commit, but nothing is published on the strength of them
    /// and the process must not resume serving.
    Abandoned,
}

/// The admitted half of a boundary: the batch is queued, the outcome pending.
///
/// Separating admission from the wait is what keeps the cut atomic without ever
/// holding the registry lock across SQLite. The snapshot and the submission
/// happen together under the registry guard; this value is what survives the
/// guard's release.
pub(crate) struct AdmittedBoundary {
    batch_receipt: BatchReceipt,
    registry: Arc<LearnedCapabilityRegistry>,
    /// Held so the deferred capability retune can be applied at publication --
    /// and only then.
    router: Arc<Router>,
    survivors: usize,
    receipt: routectl_router::BoundaryReceipt,
}

/// Take the boundary cut: snapshot the catalog-independent survivors and ADMIT
/// the atomic tombstone-plus-restatement batch, with observations excluded for
/// the duration.
///
/// `router` is the REPLACEMENT router, after `carry_over_learned_from` has
/// attached it to the shared registry -- so the survivors read here are exactly
/// the entries that must outlive the boundary.
///
/// Nothing is mutated on failure: the generation does not move and no entry is
/// pruned, so an admission refusal leaves the live state byte-identical.
pub(crate) fn admit_capability_boundary(
    usage: &UsageHandle,
    router: &Arc<Router>,
) -> Result<AdmittedBoundary, BatchCommit> {
    let catalog_version = router.catalog_version();
    let overlay_revision = router.overlay_revision();
    let registry = Arc::clone(router.learned_registry());

    let now_ms = super::ledger_reader::epoch_ms_now();
    let signed_catalog = i64::from(catalog_version);
    let signed_overlay = i64::try_from(overlay_revision).unwrap_or(i64::MAX);

    // The registry DERIVES the generation this batch establishes, under its own
    // guard, and hands it to the closure. Computing it out here would race a
    // concurrent commit landing between the read and the cut.
    let cut = registry.with_boundary_cut(
        |survivors, pending_generation| {
            let mut batch = Vec::with_capacity(survivors.len() + 1);
            batch.push(CapabilityEvent::tombstone(
                now_ms,
                signed_catalog,
                signed_overlay,
            ));
            for survivor in survivors {
                // An INFERRED negative acts only once corroborated, and its
                // corroborating observation must arrive INSIDE the inferred
                // window -- a later one RESETS the entry to a fresh pending
                // observation instead. That makes the row timestamps
                // load-bearing:
                //
                //  - 1 observation (pending): one row at `last_seen`.
                //  - exactly 2 (corroborated): rows at `first_seen` and
                //    `last_seen`, which are inside the window by construction,
                //    since that is how the entry became corroborated.
                //  - more than 2 (corroborated, then reconfirmed later):
                //    `last_seen` may be far past the window, so a
                //    first_seen/last_seen pair replays the second row as TOO LATE
                //    and resets the entry to pending -- resident but silently not
                //    acting. Corroborate at `first_seen` TWICE so that pair lands
                //    inside the window, then reconfirm at `last_seen`, which an
                //    already-acting entry accepts.
                //
                // Bounded at three either way: replay only needs to cross the
                // acts-or-not threshold and land the current decay stamp, so no
                // further history is emitted.
                let inferred = matches!(
                    survivor.signal_tier,
                    routectl_core::capability::SignalTier::Inferred
                );
                let observed_at: &[std::time::Instant] = if !inferred || survivor.observations < 2 {
                    &[survivor.last_seen]
                } else if survivor.observations == 2 {
                    &[survivor.first_seen, survivor.last_seen]
                } else {
                    &[survivor.first_seen, survivor.first_seen, survivor.last_seen]
                };
                for &observed in observed_at {
                    batch.push(CapabilityEvent {
                        // Stamped from the entry's own age, not from this reload: a
                        // restatement must not reset the decay clock.
                        ts: now_ms.saturating_sub(
                            i64::try_from(observed.elapsed().as_millis()).unwrap_or(i64::MAX),
                        ),
                        lane_key: survivor.state_key.clone(),
                        capability: survivor.feature_key.clone(),
                        verdict: survivor.verdict.as_str().to_string(),
                        phase: survivor.phase.as_str().to_string(),
                        source: survivor.source.as_str().to_string(),
                        tier: survivor.signal_tier.as_str().to_string(),
                        // Carried verbatim: the rebuild fails closed on a `verified` /
                        // `suspect` row without a recognized class, so omitting it
                        // would make the next boot skip the row and evict the verdict.
                        evidence_class: survivor.evidence_class.clone(),
                        upstream_token: None,
                        // Stamped truthfully with the POST-reload revision: the row is
                        // appended now, under this revision. The read side decides
                        // relevance per key class; no sentinel stamp is involved.
                        catalog_version: signed_catalog,
                        overlay_revision: signed_overlay,
                    });
                }
            }
            // Admission only -- non-blocking, so no lock is held across I/O. The
            // registry installs the pending generation itself, inside this same
            // ordered acquisition, when the admission below reports success.
            let admitted = usage.admit_capability_batch(batch, pending_generation);
            (admitted, survivors.len())
        },
        |(admitted, _survivors)| admitted.is_ok(),
    );
    // Exactly one boundary may be in flight, and the counter must be able to
    // advance. Both refusals happen before any batch is queued, so the registry is
    // byte-identical and the caller simply keeps the previous router.
    let ((admitted, survivors), boundary_receipt) = match cut {
        routectl_router::BoundaryCut::Taken { outcome, receipt } => (outcome, receipt),
        routectl_router::BoundaryCut::Busy { in_flight } => {
            tracing::error!(
                in_flight_generation = in_flight,
                "capability replay boundary NOT admitted; keeping the previous router"
            );
            return Err(BatchCommit::Unavailable);
        }
        routectl_router::BoundaryCut::Exhausted => {
            tracing::error!("capability replay boundary NOT admitted; keeping the previous router");
            return Err(BatchCommit::Unavailable);
        }
        routectl_router::BoundaryCut::Rejected {
            outcome: (admitted, _),
        } => {
            let failure = match admitted {
                Err(e) => e,
                Ok(_) => unreachable!("Rejected implies the admitted callback returned false"),
            };
            tracing::error!(
                "capability replay boundary batch not queued; keeping the previous router"
            );
            return Err(failure);
        }
    };

    match admitted {
        Ok(batch_receipt) => Ok(AdmittedBoundary {
            batch_receipt,
            registry,
            router: Arc::clone(router),
            survivors,
            receipt: boundary_receipt,
        }),
        Err(failure) => {
            tracing::error!(
                catalog_version,
                overlay_revision,
                pending_survivors = survivors,
                reason = ?failure,
                "capability replay boundary NOT admitted; keeping the previous router"
            );
            Err(failure)
        }
    }
}

impl AdmittedBoundary {
    /// The generation this boundary will establish if it commits. Field
    /// observations made after admission are stamped with it, so their events
    /// sort after the boundary rather than being rejected by it.
    /// Commit this boundary's transition, for tests that need a boundary to
    /// SETTLE mid-request (the reload race the purge retry exists for).
    ///
    /// Production settles inside the reload pipeline, which owns the publication
    /// ordering; this exposes only the registry half.
    #[cfg(test)]
    pub(crate) fn commit_for_tests(&self) {
        let _ = self.registry.commit_boundary_transition(&self.receipt);
    }

    pub(super) const fn pending_generation(&self) -> u64 {
        self.receipt.generation()
    }

    /// Await the writer's definitive outcome, then -- only on commit -- advance
    /// the generation and evict the catalog-scoped entries as one transition.
    ///
    /// `shutdown` is selected against the wait: if it fires first the receipt is
    /// dropped, nothing is published, and the caller must not resume serving.
    /// The registry lock is NOT held here; the cut released it before this ran.
    pub(super) async fn settle(
        self,
        shutdown: &mut tokio::sync::watch::Receiver<()>,
    ) -> BoundaryOutcomeReport {
        let receipt = self.receipt;
        let registry = self.registry.clone();
        let router = self.router;
        let survivors = self.survivors;
        let outcome = tokio::select! {
            outcome = self.batch_receipt.await_outcome() => outcome,
            _ = shutdown.changed() => {
                let _ = registry.rollback_pending_generation(&receipt);
                tracing::warn!(
                    pending_survivors = survivors,
                    "shutdown during a capability boundary write; publishing nothing"
                );
                return BoundaryOutcomeReport::Abandoned;
            }
        };

        match outcome {
            BatchCommit::Committed { .. } => {
                // Promotes the pending generation (so the events stamped with it
                // remain valid) and evicts the catalog-scoped entries as one
                // transition.
                let routectl_router::BoundarySettlement::Applied { generation, pruned } =
                    self.registry.commit_boundary_transition(&self.receipt)
                else {
                    // The receipt no longer matches: another settlement already
                    // took this boundary. Publish nothing.
                    tracing::error!(
                        expected_generation = receipt.generation(),
                        "capability replay boundary NOT committed; keeping the previous router"
                    );
                    return BoundaryOutcomeReport::Failed(BatchCommit::Unavailable);
                };
                // Publication: only now does the reload's capability tuning
                // apply. A failed or abandoned boundary leaves the previous
                // Router live, and it must keep the settings it was serving
                // under.
                router.apply_capability_tuning();
                tracing::info!(
                    generation,
                    pruned_catalog_scoped = pruned,
                    restated_survivors = survivors,
                    "capability replay boundary committed; catalog-independent verdicts restated"
                );
                BoundaryOutcomeReport::Committed {
                    survivors: self.survivors,
                    generation,
                    pruned,
                }
            }
            failure => {
                // Nothing is promoted, nothing pruned, nothing retuned: the
                // boundary was never recorded, so the live state must read
                // exactly as it did before admission.
                let _ = registry.rollback_pending_generation(&receipt);
                tracing::error!(
                    pending_survivors = survivors,
                    reason = ?failure,
                    "capability replay boundary NOT committed; keeping the previous router"
                );
                BoundaryOutcomeReport::Failed(failure)
            }
        }
    }
}

#[cfg(test)]
#[path = "capability_boundary_tests.rs"]
mod tests;
