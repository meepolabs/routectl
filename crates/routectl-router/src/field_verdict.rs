//! Envelope-field verdict lifecycle: keying, two-phase learn, single-flight
//! admission, and the loopback-target mint suppression.
//!
//! The concrete sibling of the reasoning-replay lifecycle, deliberately NOT a
//! shared generic over both. The two identities have no field in common -- a
//! replay truth is keyed on a lane discriminant plus an artifact scheme, a
//! field truth on a target plus a qualified envelope path -- and one of them
//! carries an admission predicate the other has no notion of. A parameterized
//! guard over two callers that share only their shape would put the shape
//! under one roof and leave every actual rule in the caller.
//!
//! The learned-capability registry ([`LearnedCapabilityRegistry`]) owns storage
//! and decay, so warm rebuild, doctor surfacing and the events ledger all
//! apply to a field verdict unchanged: it rides the existing row shape as a
//! new string VALUE in an already open-set column, with no schema change and
//! no second store.
//!
//! - **Keying.** `(state_key, field capability key, provider_kind)`. The
//!   capability half is minted by the namespace owner
//!   ([`field_capability_key`]) from the qualified dotted path the upstream
//!   named, so this module cannot spell a key the grammar would refuse, and a
//!   path the grammar refuses mints no identity at all.
//! - **Two-phase learn.** A rejection alone persists NOTHING. It opens
//!   request-local provisional state; the verdict is persisted only once the
//!   repaired retry actually succeeds ([`FieldRepairGuard::commit`]). A repair
//!   that failed, or an error unrelated to the field, settles without learning
//!   ([`FieldRepairGuard::release`], and the same path on an unsettled
//!   `Drop`). This is the first verdict class that MOVES TRAFFIC rather than
//!   only logging a signal, so a single misread or transient upstream fault
//!   must not be able to mint a permanent negative.
//! - **Single-flight.** Only ONE in-flight request repairs an unknown or
//!   lapsed identity; concurrent callers are refused. Otherwise N parallel
//!   requests each get rejected and each repair, N times the cost for one
//!   fact.
//! - **Loopback suppression.** A target whose `base_url` is loopback can never
//!   mint. The rejection a local hop returns is not attributable to the wire
//!   format that hop was configured with: the configured kind answers "what
//!   dialect did the operator write", not "what actually rejected this".
//!   Suppression therefore keys on the base URL, never on the kind and never
//!   on "the base differs from the kind default" -- a remote mirror on a
//!   custom base does reject with its own envelope and is not suppressed.
//!   Classification is SYNTACTIC and bounded: address literals in every
//!   spelling, the reserved local name and its subtree, and a closed set of
//!   stock hosts-file aliases. It resolves nothing, so an arbitrary DNS name
//!   that a resolver points at a local address is deliberately outside the
//!   contract -- no resolver or network dependency enters the dispatch path.
//!   Inferring an address from a name's SHAPE was tried and removed: it is
//!   wrong in both directions at once, suppressing legitimate remote domains
//!   with numeric labels while still missing the other spellings the same
//!   wildcard-DNS services accept.
//!
//! # Emission
//!
//! A committed verdict returns a [`CapabilityLearnEvent`] and a cleared one a
//! [`CapabilityClearedEvent`], the same rows the replay lifecycle emits, for
//! the dispatch layer to push onto the usage-capture drain. Every string on
//! them is a normalized key or a closed-set token; nothing in this module's
//! API can accept a request body.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, normalize_capability_key,
};

use crate::field_canary::FieldCanaryRegistry;
use crate::field_capability::field_capability_key;
use crate::learned_capability::{LearnedCapabilityRegistry, NegativeState};
use crate::router::{CapabilityClearedEvent, CapabilityLearnEvent};

/// The identity of one learned envelope-field truth: a qualified wire path
/// rejected by one configured target.
///
/// Identity is `(state_key, field capability key, provider_kind)`. The
/// capability half is minted by the namespace owner from the path, so a
/// malformed path yields no key rather than a permanent token nobody can
/// attribute; `provider_kind` rides along because every registry call
/// normalizes the capability key with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldVerdictKey {
    state_key: String,
    capability_key: String,
    provider_kind: String,
}

impl FieldVerdictKey {
    /// Build the identity for the qualified dotted `field_path` an upstream
    /// rejection named on the target `state_key`, or `None` when no identity
    /// can be built for it.
    ///
    /// Two independent refusals, both upstream of every mint path:
    ///
    /// - the field namespace does not accept `field_path` as a qualified path;
    /// - this provider kind's capability-key normalization would REWRITE the
    ///   minted key. A lane whose normalizer reduces a dotted key to a shorter
    ///   form would collapse structurally distinct fields onto one token, and
    ///   because the token is permanent that collision could never be
    ///   un-minted. Refusing the identity excludes such a lane without
    ///   changing the shared normalizer, whose behavior other capability
    ///   classes depend on. A lane whose normalization is a pass-through for
    ///   this key is unaffected.
    #[must_use]
    pub fn new(state_key: &str, field_path: &str, provider_kind: &str) -> Option<Self> {
        let minted = field_capability_key(field_path)?;
        // Normalized once at construction so every registry call and the
        // emitted row meet on one canonical string -- and compared against the
        // minted bytes, so a lane that would not carry them acquires no
        // identity at all.
        if normalize_capability_key(&minted, provider_kind) != minted {
            return None;
        }
        Some(Self {
            state_key: state_key.to_string(),
            capability_key: minted,
            provider_kind: provider_kind.to_string(),
        })
    }

    /// Rebuild the identity from components already minted and normalized
    /// by an earlier call to [`Self::new`] -- a cold-rebuild seed or a
    /// purge-finalization read, both of which already hold the exact
    /// resident row's key rather than a raw unqualified path. Skips the
    /// namespace mint and the normalization re-check `new` performs, since
    /// re-running the grammar on an already-resident key can only ever
    /// agree with the check that admitted it the first time.
    #[must_use]
    pub(crate) const fn from_capability_key(
        state_key: String,
        capability_key: String,
        provider_kind: String,
    ) -> Self {
        Self {
            state_key,
            capability_key,
            provider_kind,
        }
    }

    /// The routing state key this identity is keyed on.
    ///
    /// Test-only, like its two siblings below: the dispatch path passes the
    /// identity whole and never reads a half out of it, so these accessors exist
    /// for the tests that assert the key's composition. Gated rather than
    /// blanket-allowed, so a future production reader has to ungate one
    /// deliberately.
    #[cfg(test)]
    #[must_use]
    pub fn state_key(&self) -> &str {
        &self.state_key
    }

    /// The normalized field capability key this identity is keyed on.
    ///
    /// Ungated (unlike its two siblings): the pre-flight planner reads it to
    /// consult the operator `force_supported` resolver, which keys on the
    /// capability token. Reading it off the identity rather than re-minting
    /// the key at the call site is what makes the mask consult the SAME
    /// normalized token the registry lookup uses, so a mask cannot be honored
    /// against one spelling and missed against another.
    #[must_use]
    pub fn capability_key(&self) -> &str {
        &self.capability_key
    }

    /// The provider-kind token this identity is keyed on.
    /// Test-only -- see [`Self::state_key`].
    #[cfg(test)]
    #[must_use]
    pub fn provider_kind(&self) -> &str {
        &self.provider_kind
    }
}

/// Two-phase, single-flight lifecycle over the learned-capability registry for
/// envelope-field verdicts.
#[derive(Debug)]
pub struct FieldVerdictRegistry {
    learned: Arc<LearnedCapabilityRegistry>,
    /// Identities whose repair is unresolved. Purely request-local
    /// coordination: nothing here is persisted, and every settlement path --
    /// including a dropped guard -- clears its entry.
    ///
    /// Shared behind an `Arc` so it survives a reload: a repair outstanding when
    /// the router swap lands still holds a guard that settles against the
    /// REPLACEMENT facade, and a fresh empty set there would admit a second
    /// concurrent repair for the same identity -- exactly the duplicate-repair
    /// cost single-flight exists to prevent.
    in_flight: Arc<Mutex<HashSet<FieldVerdictKey>>>,
    /// Per-identity canary claim, cadence countdown, outstanding-repair
    /// count, and confirmation-quorum state -- see [`FieldCanaryRegistry`].
    ///
    /// Shared behind an `Arc` for the same reason as `in_flight`: a canary
    /// claimed or a countdown mid-cycle when the router swap lands must stay
    /// visible to the replacement facade, or a reload would silently admit a
    /// second concurrent canary or double-count an eligible request.
    canaries: Arc<FieldCanaryRegistry>,
}

impl FieldVerdictRegistry {
    /// Wrap the shared learned-capability registry.
    #[must_use]
    pub fn new(learned: Arc<LearnedCapabilityRegistry>) -> Self {
        Self {
            learned,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            canaries: Arc::new(FieldCanaryRegistry::new()),
        }
    }

    /// Rebuild this facade onto a (possibly new) shared registry, CARRYING the
    /// in-flight identities and the canary/quorum state.
    ///
    /// Both sets move by `Arc::clone`, not by copy: an old guard's release must
    /// be visible to the replacement facade's admission check, or the two would
    /// each believe the identity free and admit a duplicate repair or canary.
    #[must_use]
    pub fn rebuilt_on(&self, learned: Arc<LearnedCapabilityRegistry>) -> Self {
        Self {
            learned,
            in_flight: Arc::clone(&self.in_flight),
            canaries: Arc::clone(&self.canaries),
        }
    }

    /// Whether this facade shares its in-flight set with `other`. Test-only: it
    /// is what makes "the set survived the rebuild" an assertion about identity
    /// rather than about contents.
    #[cfg(test)]
    pub fn shares_in_flight_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.in_flight, &other.in_flight)
    }

    /// Whether this facade shares its canary/quorum state with `other`.
    /// Test-only, mirroring [`Self::shares_in_flight_with`].
    #[cfg(test)]
    pub fn shares_canaries_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.canaries, &other.canaries)
    }

    /// The shared canary/quorum registry, so downstream eligibility and
    /// canary-dispatch logic reads and mutates the same state this facade's
    /// settlements reconcile.
    ///
    /// Not yet read outside tests: the dispatch-side eligibility rule that
    /// consumes it lands in a follow-up change; this module is state-only.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn canaries(&self) -> &Arc<FieldCanaryRegistry> {
        &self.canaries
    }

    /// Whether `key` is currently pre-flight eligible: resident, ACTING,
    /// and its incarnation has at least one acknowledged confirmation in
    /// the shared canary registry.
    ///
    /// Read-only, and deliberately NOT routed through [`Self::admit_provisional`]:
    /// that call claims the single-flight REACTIVE repair slot and refuses
    /// outright on an acting verdict, because reacting to an already-acting
    /// verdict is a routing decision, not a repair. A pre-flight rewrite acts
    /// on the SAME acting verdict from the other side -- before dispatch, not
    /// after a rejection -- so it reads the verdict directly rather than
    /// competing with the reactive path for its slot.
    ///
    /// The confirmation count is compared against the ACTING entry's own
    /// incarnation, never merely "some confirmations exist": a canary state
    /// left over from a since-relearned incarnation (the verdict lapsed,
    /// cleared, and was re-learned) must never be read as backing the
    /// current one. Today, live traffic can only produce a non-zero count
    /// through a cold-rebuild seed (see [`FieldCanaryRegistry::seed_from_rebuild`]);
    /// [`FieldCanaryRegistry::acknowledge_confirmation`] is reserved for a
    /// durable-writer-ack caller that lands separately, so this predicate
    /// naturally stays dormant on live traffic until that caller exists.
    ///
    /// # Why the verdict is read TWICE
    ///
    /// The two reads this predicate needs -- the acting verdict and the
    /// canary confirmation -- take different locks, so nothing holds them
    /// still together. A concurrent clear, purge, lapse, relearn, or
    /// generation boundary landing BETWEEN them yields a decision assembled
    /// from two states that never coexisted: the confirmation matched an
    /// incarnation the entry had already left. The re-read closes that by
    /// requiring the acting incarnation to be unchanged AFTER the
    /// confirmation was observed, which makes the authorization rest on one
    /// consistent (generation, incarnation) pair rather than on the ordering
    /// of two independent reads. It cannot manufacture a false negative that
    /// matters: a verdict that moved under the read is exactly a verdict this
    /// request has no settled grounds to act on, so refusing is the correct
    /// answer rather than a lost opportunity.
    ///
    /// This is a compare-recheck, not a lock: it does not prevent a change
    /// landing after the return, and it does not need to -- the caller's
    /// decision is per-attempt and fails open.
    #[must_use]
    pub fn preflight_eligible(&self, key: &FieldVerdictKey, generation: u64, now: Instant) -> bool {
        let acting_incarnation = |()| {
            self.learned.field_acting_incarnation_in_generation(
                generation,
                &key.state_key,
                &key.capability_key,
                &key.provider_kind,
                now,
            )
        };
        let Some(before) = acting_incarnation(()) else {
            return false;
        };
        let confirmed = self
            .canaries
            .snapshot(key)
            .is_some_and(|snap| snap.incarnation == before && snap.confirmations >= 1);
        if !confirmed {
            return false;
        }
        between_eligibility_reads();
        // Re-read under the same generation: the confirmation above is only
        // authorization if the verdict it backs is STILL the acting one.
        acting_incarnation(()) == Some(before)
    }

    /// Claim the single-flight repair slot for `key` on a target reached at
    /// `target_base_url`.
    ///
    /// `Some(guard)` means THIS request may repair and settle the identity.
    /// `None` means it may not: the target is loopback and can never mint, an
    /// acting verdict is already resident, or another request holds the slot
    /// with its repair unresolved.
    ///
    /// A lapsed verdict is admissible: exactly one caller gets the guard and
    /// re-verifies the field against live upstream behavior.
    pub fn admit_provisional(
        &self,
        key: &FieldVerdictKey,
        target_base_url: &str,
        generation: u64,
        now: Instant,
    ) -> Option<FieldRepairGuard<'_>> {
        // Checked before the slot is claimed: a suppressed target must not
        // even hold a slot, or it would refuse a sibling request that could
        // legitimately mint.
        if loopback_target_suppresses_minting(target_base_url) {
            return None;
        }
        // The claim is taken under the in-flight lock together with the decay
        // read, so two callers racing an unknown identity cannot both observe
        // "absent" and both repair.
        let mut in_flight = self.in_flight.lock();
        if in_flight.contains(key) {
            return None;
        }
        // Validated atomically with the decay read: a superseded Router must
        // not decide to strip on state belonging to the replacement generation.
        let (state, admitted_generation) = self.learned.negative_state_in_generation(
            generation,
            &key.state_key,
            &key.capability_key,
            &key.provider_kind,
            now,
        )?;
        match state {
            NegativeState::Acting => None,
            NegativeState::Absent | NegativeState::Lapsed => {
                in_flight.insert(key.clone());
                Some(FieldRepairGuard {
                    registry: self,
                    key: key.clone(),
                    settled: false,
                    generation: admitted_generation,
                })
            }
        }
    }

    /// Whether an acting verdict currently applies to this identity,
    /// independent of any in-flight repair. Read-only: it never claims the
    /// slot. Test-only, because the dispatch path settles through
    /// `admit_provisional`, which reads the same state while claiming.
    #[cfg(test)]
    pub fn is_negative_acting(&self, key: &FieldVerdictKey, now: Instant) -> bool {
        matches!(
            self.learned.negative_state_in_generation(
                self.learned.generation(),
                &key.state_key,
                &key.capability_key,
                &key.provider_kind,
                now,
            ),
            Some((NegativeState::Acting, _))
        )
    }

    /// The wrapped registry as a shared handle, so a rebuild can be constructed
    /// onto the same store. Test-only: production gets the `Arc` from the Router.
    #[cfg(test)]
    pub const fn learned_arc(&self) -> &Arc<LearnedCapabilityRegistry> {
        &self.learned
    }

    /// The wrapped registry, so a test can plant a resident verdict through
    /// the registry's own carry-over seam rather than through this lifecycle.
    #[cfg(test)]
    pub fn learned(&self) -> &LearnedCapabilityRegistry {
        &self.learned
    }

    /// How many entries are resident in the wrapped registry. Test-only: it is
    /// what makes "persists nothing" an assertion about the store rather than
    /// only about one key's acting state.
    #[cfg(test)]
    pub fn snapshot_len(&self) -> usize {
        self.learned.snapshot().len()
    }

    fn release_slot(&self, key: &FieldVerdictKey) {
        self.in_flight.lock().remove(key);
    }
}

/// The single-flight repair claim for one envelope-field identity.
///
/// Holding this guard is the PROVISIONAL phase: no verdict is persisted while
/// it lives. Exactly one settlement applies:
///
/// - [`commit`](FieldRepairGuard::commit) -- the request was rejected AND the
///   repaired retry succeeded: persist (or refresh) the verdict.
/// - [`clear`](FieldRepairGuard::clear) -- the field was ACCEPTED: drop any
///   resident verdict.
/// - [`release`](FieldRepairGuard::release) -- the repair failed, or the
///   request hit an unrelated error: learn nothing, leave any resident entry
///   exactly as it was.
///
/// Dropping the guard without settling releases the slot as `release` would,
/// so an early return, a `?` propagation, or a client disconnect can never
/// strand an identity behind a permanently claimed slot -- and can never learn
/// by omission either.
#[derive(Debug)]
pub struct FieldRepairGuard<'a> {
    registry: &'a FieldVerdictRegistry,
    key: FieldVerdictKey,
    /// Set by whichever settlement runs, so the subsequent `Drop` cannot free
    /// a slot a different request has since claimed.
    settled: bool,
    /// The generation TOKEN this guard presents to the guarded registry
    /// operations at settlement -- what the barrier validates its admission
    /// against, captured from the guarded negative-state read.
    ///
    /// NOT the event's persistence generation. That always comes from the
    /// settlement's own `GenerationOutcome::Applied`, and the two legitimately
    /// DIFFER once a boundary has moved in between: a boundary admitted after
    /// this guard makes `Applied` report the pending generation, and a boundary
    /// that rolls back makes it report the active one again while this token
    /// still names the discarded value. Stamping a row from this field would
    /// write a generation no boundary committed, and the writer would drop it.
    ///
    /// A valid `field:` capability key is catalog-INDEPENDENT (see
    /// `field_capability`), so the barrier admits this token from any generation
    /// and a settlement crossing a catalog revision still applies -- correct,
    /// because an upstream statement about its own request envelope is not
    /// invalidated by a catalog revision.
    generation: u64,
}

impl FieldRepairGuard<'_> {
    /// Phase two: the repaired retry succeeded, so the rejection is confirmed
    /// as a real envelope-field incompatibility. Persists the verdict
    /// (refreshing a resident or lapsed one) and returns the emission row for
    /// the capability-event sink.
    ///
    /// A refresh re-stamps the base decay window rather than applying the
    /// registry's geometric re-probe backoff: this is a corroborated
    /// self-identifying observation, not a failed probe, so a chronically
    /// rejecting field re-verifies once per base decay by design. The backoff
    /// ladder stays reserved for the registry's own re-probe path.
    ///
    /// `request_features` is the request's derived in-flight feature set; no
    /// request body can enter the row.
    #[must_use]
    /// `None` covers any refused guarded mutation reported by the shared
    /// generation API -- Stale, Reserved, or Exhausted, not stale alone. In
    /// every case the slot is released, nothing is recorded, and no event
    /// rides out -- the caller must never emit a learn row for a verdict it
    /// did not persist.
    ///
    /// For a valid `field:` key that arm is DEFENSIVE rather than expected: the
    /// key class is catalog-independent, so the barrier admits it from any
    /// generation. It exists because this lifecycle calls the same generic
    /// registry API the catalog-scoped lifecycles use, and silently treating a
    /// refusal as success there would emit a row for a mutation that never
    /// happened.
    pub fn commit(
        mut self,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> Option<CapabilityLearnEvent> {
        self.settled = true;
        let key = self.key.clone();
        // Through the generation barrier with the admission's own generation.
        // The persistence_generation comes from the Applied outcome, atomically
        // paired with the mutation it describes. `observations` rides out of
        // the SAME critical section the mutation ran under, via
        // `observe_in_generation_with_observations`, rather than a second,
        // unguarded `snapshot()` call after the guard releases -- a
        // concurrent purge or sibling mutation between those two calls could
        // otherwise report a count this mutation never produced.
        let observed = self
            .registry
            .learned
            .observe_in_generation_with_observations(
                self.generation,
                &key.state_key,
                &key.capability_key,
                &key.provider_kind,
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                now,
            );
        let (observations, persistence_generation, incarnation) = match observed {
            crate::learned_capability::GenerationOutcome::Applied {
                value: (_, observations),
                incarnation,
                generation,
            } => (observations, generation, incarnation),
            crate::learned_capability::GenerationOutcome::Stale => {
                self.registry.release_slot(&key);
                tracing::debug!(
                    event = "field_verdict_commit_stale",
                    state_key = %routectl_core::sanitize_for_log(&key.state_key),
                    capability_key = %key.capability_key,
                    "field-verdict commit refused: its admission predates the live \
                     capability generation"
                );
                return None;
            }
            crate::learned_capability::GenerationOutcome::Reserved => {
                self.registry.release_slot(&key);
                tracing::debug!(
                    event = "field_verdict_commit_reserved",
                    state_key = %routectl_core::sanitize_for_log(&key.state_key),
                    capability_key = %key.capability_key,
                    "field-verdict commit refused: an operator purge holds this \
                     key's lease"
                );
                return None;
            }
            crate::learned_capability::GenerationOutcome::Exhausted => {
                self.registry.release_slot(&key);
                tracing::debug!(
                    event = "field_verdict_commit_exhausted",
                    state_key = %routectl_core::sanitize_for_log(&key.state_key),
                    capability_key = %key.capability_key,
                    "field-verdict commit refused: the incarnation sequence is \
                     exhausted"
                );
                return None;
            }
        };
        self.registry.release_slot(&key);
        // This admission is IN-MEMORY only: `Applied` means the shared
        // registry accepted the mutation through the generation barrier,
        // not that the event below has been durably written. The
        // confirmation count a later eligibility check reads is reconciled
        // separately, by a caller holding that durable acknowledgment (see
        // `FieldCanaryRegistry::acknowledge_confirmation`) -- never from
        // this in-memory admission alone.
        tracing::info!(
            event = "field_verdict_commit",
            state_key = %routectl_core::sanitize_for_log(&key.state_key),
            capability_key = %key.capability_key,
            upstream_status,
            observations,
            "envelope-field verdict persisted after a successful repaired retry",
        );
        Some(CapabilityLearnEvent {
            persistence_generation,
            incarnation,
            state_key: key.state_key,
            capability_key: key.capability_key,
            provider_kind: key.provider_kind,
            signal_tier: SignalTier::SelfIdentifying,
            observations,
            upstream_status,
            remapped: false,
            request_features,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        })
    }

    /// The field was ACCEPTED: drop any resident verdict so the request shape
    /// is re-enabled at once rather than after the remaining decay.
    ///
    /// Returns a [`CapabilityClearedEvent`] when a resident entry was actually
    /// removed, so the caller rides the clear out on the dispatch meta and a
    /// warm rebuild does not resurrect the verdict from the ledger. An identity
    /// that had no resident entry clears nothing and returns `None`, and the
    /// event's stamp comes from the removal's own `Applied` outcome.
    ///
    /// A `Stale`, `Reserved`, or `Exhausted` removal is likewise inert and
    /// returns `None`, each logged with a diagnostic naming its own refusal
    /// rather than a generic one. As with `commit`, this is DEFENSIVE
    /// generic-API handling rather than expected `field:` behavior: a
    /// catalog-independent key is admitted from any generation.
    pub fn clear(mut self) -> Option<CapabilityClearedEvent> {
        self.settled = true;
        let removed = self.registry.learned.remove_keyed_in_generation(
            self.generation,
            &self.key.state_key,
            &self.key.capability_key,
            &self.key.provider_kind,
        );
        // A lease-refused, stale, or exhausted removal clears nothing and
        // emits nothing: all three are refusals, so none may produce an
        // event.
        let (cleared, persistence_generation, incarnation) = match removed {
            crate::learned_capability::GenerationOutcome::Applied {
                value,
                incarnation,
                generation,
            } => (value, generation, incarnation),
            crate::learned_capability::GenerationOutcome::Stale => {
                self.registry.release_slot(&self.key);
                tracing::debug!(
                    event = "field_verdict_clear_stale",
                    state_key = %routectl_core::sanitize_for_log(&self.key.state_key),
                    capability_key = %self.key.capability_key,
                    "field-verdict clear refused: its admission predates the live \
                     capability generation"
                );
                return None;
            }
            crate::learned_capability::GenerationOutcome::Reserved => {
                self.registry.release_slot(&self.key);
                tracing::debug!(
                    event = "field_verdict_clear_reserved",
                    state_key = %routectl_core::sanitize_for_log(&self.key.state_key),
                    capability_key = %self.key.capability_key,
                    "field-verdict clear refused: an operator purge holds this \
                     key's lease"
                );
                return None;
            }
            crate::learned_capability::GenerationOutcome::Exhausted => {
                self.registry.release_slot(&self.key);
                tracing::debug!(
                    event = "field_verdict_clear_exhausted",
                    state_key = %routectl_core::sanitize_for_log(&self.key.state_key),
                    capability_key = %self.key.capability_key,
                    "field-verdict clear refused: the incarnation sequence is \
                     exhausted"
                );
                return None;
            }
        };
        self.registry.release_slot(&self.key);
        // The identity's verdict is gone: drop its canary/quorum state too,
        // so a later re-learn starts a clean incarnation rather than
        // inheriting a stale cadence, claim, or confirmation count.
        self.registry.canaries.reset(&self.key);
        if !cleared {
            return None;
        }
        tracing::info!(
            event = "field_verdict_clear",
            state_key = %routectl_core::sanitize_for_log(&self.key.state_key),
            capability_key = %self.key.capability_key,
            "envelope-field verdict cleared by an accepted request",
        );
        Some(CapabilityClearedEvent {
            persistence_generation,
            incarnation,
            state_key: self.key.state_key.clone(),
            capability_key: self.key.capability_key.clone(),
            provider_kind: self.key.provider_kind.clone(),
        })
    }

    /// Settle WITHOUT learning: the repair failed, or the request hit an error
    /// unrelated to the field. Any resident entry is left exactly as it was,
    /// so the next request re-verifies rather than inheriting a conclusion
    /// nothing proved.
    ///
    /// Test-only as an explicit CALL, because the dispatch path reaches this
    /// exact settlement by dropping an unsettled guard (see [`Drop`]) rather
    /// than by naming it -- which is what makes an early return, a `?`, or a
    /// client disconnect settle correctly without a call site to forget. The
    /// tests call it directly to pin that the explicit and implicit paths agree.
    #[cfg(test)]
    pub fn release(mut self) {
        self.settled = true;
        self.registry.release_slot(&self.key);
    }
}

impl Drop for FieldRepairGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.registry.release_slot(&self.key);
        }
    }
}

/// The reserved name whose whole subtree names a local destination
/// (RFC 6761): `localhost` itself, and anything under it.
const LOCALHOST_NAME: &str = "localhost";

/// Names a stock hosts file maps to a local address. A CLOSED set, matched
/// EXACTLY: the Debian-family entries plus the RHEL/Fedora ones. Exact match is
/// the whole discipline here -- a suffix or prefix rule over these would
/// suppress remote names that merely resemble one, which is the failure mode
/// that got a name-shape heuristic removed from this module.
const LOCAL_HOST_ALIASES: &[&str] = &[
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "localhost4",
    "localhost6",
    "localhost4.localdomain4",
    "localhost6.localdomain6",
];

/// True when a target reached at `base_url` must never mint an envelope-field
/// verdict, because that base URL names a LOCAL destination rather than a
/// remote upstream.
///
/// The predicate keys on the BASE URL, and on nothing else. A local hop is
/// configured with whatever wire format routectl speaks to it, while the real
/// upstream behind it may be a different dialect entirely, so the configured
/// kind cannot identify one -- and "the base differs from this kind's default"
/// cannot either, because a remote mirror also runs on a custom base and its
/// rejections ARE attributable.
///
/// Local means any of:
///
/// - a loopback address, in every spelling an operator can write: the whole
///   `127.0.0.0/8` range, `::1`, and the IPv4-mapped / IPv4-compatible forms;
/// - an UNSPECIFIED wildcard address (`0.0.0.0`, `::`), which is not a remote
///   host at all and reaches a local listener in practice;
/// - the RFC 6761 reserved name `localhost` or any subdomain of it;
/// - an exact match against the closed set of stock hosts-file aliases.
///
/// Terminal DNS root dots are trimmed before any name comparison, so a fully
/// qualified or malformed-but-local spelling classifies with its bare form.
///
/// Fails toward suppression: a scheme that is not http(s), and a base URL that
/// names no host (including a malformed address literal the parser refuses), are
/// all treated as local. Minting is the irreversible direction -- the token is
/// permanent and steers routing -- so a target this predicate cannot positively
/// identify as a remote upstream must not mint.
///
/// # What is deliberately OUT of scope
///
/// An arbitrary DNS name that RESOLVES to a loopback address is not caught, and
/// that is a design boundary rather than a gap to close incrementally. This
/// predicate is synchronous, on the dispatch path, and reads only the configured
/// string: no resolver, no network dependency, no clock. A name-shape heuristic
/// was tried here and removed, because approximating resolution from spelling is
/// wrong in BOTH directions at once -- it suppressed legitimate remote domains
/// carrying numeric labels, while still missing the prefixed, dashed and hex
/// spellings the same wildcard-DNS services accept. A partial classifier that
/// silently misroutes both ways is worse than a stated boundary, and the
/// residual exposure is bounded: one spurious verdict on a target the operator
/// deliberately aliased to a local listener.
pub fn loopback_target_suppresses_minting(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return true;
    };
    // Classified before the host: an egress is http(s), so any other scheme
    // names something this predicate cannot attribute a rejection to -- and it
    // may still carry an ordinary-looking remote hostname.
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }
    match url.host() {
        Some(url::Host::Domain(domain)) => is_local_domain(domain),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        // The native loopback check is REQUIRED for `::1`, not merely ordered
        // before the reduction: that address matches the IPv4-compatible prefix
        // (`::/96`) with an embedded quad of `0.0.0.1`, which is neither
        // loopback nor unspecified, so the reduction below cannot classify it at
        // all. Removing this check makes `::1` read as remote. The native
        // UNSPECIFIED check is redundant against the reduction (`::` reduces to
        // `0.0.0.0`, which the fallback accepts) and is kept to state the intent
        // at the point of decision rather than leave it resting on an
        // arithmetic coincidence.
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip
                    .to_ipv4_mapped()
                    .or_else(|| crate::factory::ipv4_compatible_embedded(&ip))
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        None => true,
    }
}

/// True when `domain` names a local destination by NAME rather than by address
/// literal.
///
/// Every terminal DNS root dot is trimmed first. A single-dot strip would leave
/// `localhost..` with an empty last label, which is not the reserved name and
/// would mint -- so a malformed local spelling has to fail closed, and trimming
/// the whole run is what does it.
///
/// Two rules, both anchored, and deliberately no third:
///
/// - the RFC 6761 reserved name, or any subdomain of it. Compared on whole
///   LABELS, never a byte suffix: `notlocalhost` and `mylocalhost.example` are
///   ordinary remote hosts that merely contain those bytes, and
///   `localhost.upstream.example` is a remote name whose FIRST label happens to
///   be the reserved one.
/// - an exact match against the closed alias set. Exact, so it cannot creep into
///   a suffix heuristic that swallows `localhost4.upstream.example`.
///
/// Nothing here infers an address from the SHAPE of a name. See
/// [`loopback_target_suppresses_minting`] for why that inference was removed
/// rather than refined.
fn is_local_domain(domain: &str) -> bool {
    // The `url` crate already lowercases a parsed domain; the explicit
    // case-insensitive comparisons keep this correct for a direct caller too.
    let name = domain.trim_end_matches('.');
    name.eq_ignore_ascii_case(LOCALHOST_NAME)
        || name
            .rsplit_once('.')
            .is_some_and(|(_, last_label)| last_label.eq_ignore_ascii_case(LOCALHOST_NAME))
        || LOCAL_HOST_ALIASES
            .iter()
            .any(|alias| name.eq_ignore_ascii_case(alias))
}

/// Production build: nothing sits between the two eligibility reads.
///
/// The re-read in [`FieldVerdictRegistry::preflight_eligible`] closes a real
/// race, and a race is only demonstrably closed by a test that can land a
/// mutation INSIDE the window. This seam is that interposition point and
/// nothing else: the production body is empty, so the window is exactly as
/// wide as the two reads make it and no production path can widen it.
#[cfg(not(test))]
const fn between_eligibility_reads() {}

/// Test build: run whatever this thread parked in the interposition slot.
///
/// Substituted at exactly this seam and nowhere else, the same shape the
/// reactive arm's absent-parser seam uses. Thread-local, so a barrier a test
/// installs is visible to the dispatch that test drives and invisible to every
/// sibling test running concurrently.
#[cfg(test)]
fn between_eligibility_reads() {
    eligibility_interpose::run();
}

/// Test-only interposition between the two eligibility reads.
///
/// The window this parks in is invisible from outside: a concurrency test that
/// merely spawns threads and hopes to hit it proves nothing when it passes,
/// because a green run and an unreachable window are indistinguishable. A
/// barrier installed HERE makes the interleaving deterministic, so the
/// regression test fails every run against the single-read version rather
/// than occasionally.
#[cfg(test)]
pub mod eligibility_interpose {
    use std::cell::RefCell;

    type Hook = Box<dyn Fn()>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run this thread's installed hook, if any.
    pub(super) fn run() {
        // The hook is taken out for the call so a hook that itself reaches
        // `preflight_eligible` cannot recurse into itself, and restored
        // afterwards so one installation serves repeated reads.
        let hook = HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
            HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        }
    }

    /// Install `hook` to run between the two reads, for this thread, until the
    /// returned guard drops.
    pub fn install(hook: impl Fn() + 'static) -> Guard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    /// Clears the installed hook on drop, so one test cannot leak its
    /// interposition into another running on the same thread.
    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
#[path = "field_verdict_tests.rs"]
mod tests;
