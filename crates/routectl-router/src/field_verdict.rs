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

    /// The routing state key this identity is keyed on, for the probe
    /// worker's target resolution.
    ///
    /// A probe must reach the SAME lane the identity was minted against,
    /// and the state key is what names it. Exposed as its own accessor
    /// (rather than ungating the test-only `state_key` below) so this one
    /// production reader is explicit about why it needs the half.
    #[must_use]
    pub(crate) fn probe_state_key(&self) -> &str {
        &self.state_key
    }

    /// The routing state key this identity is keyed on, for the read-only
    /// status/doctor projection's target-spec resolution.
    ///
    /// Its own accessor rather than ungating the test-only `state_key` below, for
    /// the same reason [`Self::probe_state_key`] is: each production reader is
    /// explicit about why it needs the half. This one resolves the state key to a
    /// `(provider, nickname)` pair to answer whether the operator opted the target
    /// into prefix-impacting pre-flight -- a pure read, on a surface that must
    /// mutate nothing.
    #[must_use]
    pub(crate) fn status_state_key(&self) -> &str {
        &self.state_key
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

/// The floor every transform class shares before ANY pre-flight rewrite may
/// act: one acknowledged confirmation. A class needing more compares
/// [`PreflightAuthorization::confirmations`] against its own quorum -- the
/// floor is not any class's whole gate.
///
/// Its RELATION to the per-class quorums (`MINIMUM_CONFIRMATIONS <=
/// ENVELOPE_QUORUM < PREFIX_QUORUM`) is enforced beside those two, in
/// `config::schema`, by anonymous `const _: () = assert!(...)` items. Rustc
/// evaluates the initializer of every `const` item in a crate it compiles, so
/// those assertions run with no consumer and no reference at all. The floor
/// assertion reads THIS constant by path rather than a copy of its value, so
/// there is one definition and nothing can drift.
///
/// What that catches is narrower than every edit: RAISING this above
/// `ENVELOPE_QUORUM` violates the inequality and fails the build
/// (`error[E0080]: evaluation panicked`), while an order-preserving change --
/// lowering it to `0` -- compiles. The exact value is pinned by the named test
/// `the_three_quorum_values_are_exactly_one_one_and_two`.
///
/// `pub(crate)` and no wider: the floor is an internal eligibility parameter, and
/// the operator-facing values are `config::ENVELOPE_QUORUM` and
/// `config::PREFIX_QUORUM`.
#[expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so pub(crate) reads as redundant -- but \
              `config::schema`'s floor assertion reads this path, so the \
              visibility is load-bearing rather than cosmetic"
)]
pub(crate) const MINIMUM_CONFIRMATIONS: u32 = 1;

/// What one consistent pre-flight eligibility read established.
///
/// The incarnation and the confirmation count travel TOGETHER rather than
/// through two calls, because a transform class's quorum is a threshold on the
/// same count eligibility was decided from: a caller re-reading it could gate
/// one row against a state the eligibility decision never saw.
///
/// The PROVENANCE and the CANARY POSTURE ride along for the same reason, one
/// step further: a decision record that reports which evidence authorized a
/// rewrite, and where that identity's re-verification stood when it did, must
/// report the snapshot that ACTUALLY permitted the action. A caller re-reading
/// either afterwards would report a state that may have moved between the
/// authorization and the record -- which is the one way a diagnostic on this
/// surface can make a false claim about a rewrite that already went upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightAuthorization {
    /// The verdict incarnation the authorization rests on. A canary claim and
    /// its later settlement must both carry this value.
    pub incarnation: u64,
    /// The acknowledged confirmation count resident for that incarnation, at
    /// least [`MINIMUM_CONFIRMATIONS`].
    pub confirmations: u32,
    /// The detection phase that attributed the acting verdict, from the SAME
    /// guarded read the incarnation came from.
    pub phase: FailurePhase,
    /// Whether the acting verdict's evidence came from live traffic or an
    /// out-of-band probe, from that same guarded read.
    pub source: EvidenceSource,
    /// Where this identity's canary stood in the ONE canary snapshot that
    /// backed the eligibility decision -- counting, due, or in flight.
    pub canary: crate::router::CanaryPosture,
    /// The last settled canary outcome in that same snapshot, or `None` when
    /// none has settled for this incarnation.
    pub canary_last_outcome: Option<crate::field_canary::CanaryOutcome>,
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
    #[must_use]
    pub const fn canaries(&self) -> &Arc<FieldCanaryRegistry> {
        &self.canaries
    }

    /// Whether `key` is currently pre-flight eligible at all, discarding the
    /// rest of [`Self::preflight_authorization`]'s answer.
    ///
    /// Test-only: the production planner needs the incarnation (a canary claim
    /// and its settlement must carry the one the authorization was validated
    /// against) and the confirmation count (the transform class's quorum is
    /// read from the same snapshot), so it calls the full form directly. Gated
    /// rather than blanket-allowed, so a future production reader has to ungate
    /// it deliberately and think about which of the two it wants.
    #[cfg(test)]
    #[must_use]
    pub fn preflight_eligible(&self, key: &FieldVerdictKey, generation: u64, now: Instant) -> bool {
        self.preflight_authorization(key, generation, now).is_some()
    }

    /// The pre-flight authorization for `key`: the incarnation the decision
    /// rests on plus the acknowledged confirmation count backing it, read from
    /// ONE snapshot of the shared canary state -- `None` when there is no
    /// settled, acknowledged, acting verdict to authorize anything.
    ///
    /// The incarnation is what a canary claim and its later settlement must
    /// carry: a settlement stamped with any other value would be indisputably
    /// stale, and one stamped with a value read separately after this decision
    /// could name an incarnation the authorization was never checked against.
    /// Returning it from the same consistent (generation, incarnation) pair the
    /// re-read validated is what keeps the two from disagreeing.
    ///
    /// The confirmation count rides along for the same reason rather than
    /// through a second read: the transform class's quorum (the router's
    /// `field_repair::TransformClass::required_quorum`) is
    /// a threshold on THIS count, and a caller that re-read it would be
    /// answering "is there an acting verdict" and "does it carry enough
    /// cycles" against two states that never coexisted -- which is how an
    /// eligible-and-confirmed identity comes to report a confirmation
    /// shortfall it never had. One snapshot means eligibility and every
    /// class's gate are decided from the same facts.
    ///
    /// ELIGIBLE means: resident, ACTING, not canary-suspended, and its
    /// incarnation has at least one acknowledged confirmation in the shared
    /// canary registry. A class needing MORE than one compares
    /// [`PreflightAuthorization::confirmations`] against its own quorum; this
    /// predicate is the floor every class shares, not any class's whole gate.
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
    /// canary snapshot -- take different locks, so nothing holds them
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
    pub fn preflight_authorization(
        &self,
        key: &FieldVerdictKey,
        generation: u64,
        now: Instant,
    ) -> Option<PreflightAuthorization> {
        let acting_facts = |()| {
            self.learned.field_acting_facts_in_generation(
                generation,
                &key.state_key,
                &key.capability_key,
                &key.provider_kind,
                now,
            )
        };
        let before = acting_facts(())?;
        // SUSPENSION, read before the confirmation: a canary that accepted the
        // unrepaired field proved this verdict wrong, and the durable clear
        // that removes it can be REFUSED (a purge lease, a stale generation).
        // Until the row is actually gone it is still resident, still acting
        // and still confirmed, so nothing else in this predicate would refuse
        // it -- and every request admitted in that window is one more modified
        // by a verdict already known to be false.
        //
        // Deliberately NOT incarnation-scoped, unlike the confirmation check
        // below. Scoping it would be dead logic: a snapshot whose incarnation
        // differs from the acting one already fails that check, so the comparison
        // could never change an outcome.
        //
        // A retained suspension is therefore lifted only by dropping the
        // identity's state outright, NOT by a relearn -- the residual case and
        // its three recovery paths are spelled out on
        // [`Self::record_canary_disproof`].
        //
        // ONE snapshot backs both this check and the confirmation count the
        // caller's quorum reads, so no gate downstream can be decided against a
        // different state than eligibility was.
        let snapshot = self.canaries.snapshot(key)?;
        if snapshot.preflight_suspended {
            return None;
        }
        if snapshot.incarnation != before.incarnation
            || snapshot.confirmations < MINIMUM_CONFIRMATIONS
        {
            return None;
        }
        between_eligibility_reads();
        // Re-read under the same generation: the confirmation above is only
        // authorization if the verdict it backs is STILL the acting one. Compared
        // on the whole facts value, not the incarnation alone: phase and source
        // are carried into the decision record, so a lifecycle that moved under
        // the read must refuse rather than report the pre-read provenance.
        (acting_facts(()) == Some(before)).then_some(PreflightAuthorization {
            incarnation: before.incarnation,
            confirmations: snapshot.confirmations,
            phase: before.phase,
            source: before.source,
            // Derived through the status surface's OWN posture function, from the
            // same snapshot the two eligibility checks above read. One
            // derivation, so the posture a decision record reports and the
            // posture a status row reports for one identity cannot disagree.
            canary: crate::router::fidelity_status::canary_posture(Some(snapshot)),
            canary_last_outcome: snapshot.last_outcome,
        })
    }

    /// Persist a canary CONFIRMATION for `key` and SETTLE the caller's claim:
    /// the tested field drew the same structured rejection unrepaired and the
    /// repaired retry then succeeded, so the resident verdict is corroborated
    /// afresh.
    ///
    /// Takes the claim by value because recording and settling are ONE
    /// operation, not two a caller sequences. The confirmation's own
    /// re-observation mints a new incarnation for the same verdict, and the
    /// resident canary state moves onto it in the same critical section that
    /// releases the claim -- see
    /// [`FieldCanaryRegistry::settle_confirmed_and_carry`] for why a released
    /// claim plus a not-yet-carried incarnation must never be observable.
    ///
    /// The observation that mints the incarnation necessarily precedes that
    /// critical section, so a window exists in which the learned row is minted
    /// and the canary state has not moved. Nothing reads across it destructively:
    /// a request planned in that window carries the OLD incarnation and is
    /// refused by the monotonic incarnation checks on every canary write path
    /// rather than being allowed to reset the identity's state.
    ///
    /// A verdict whose canary state stays on the superseded incarnation is not
    /// merely slower: pre-flight refuses it on the incarnation match, and the
    /// reactive repair path refuses it too because the verdict is ACTING, so the
    /// field reaches the upstream unrepaired and the request TERMINATES on the
    /// rejection the verdict exists to avoid.
    ///
    /// Reuses the SAME registry observe call [`FieldRepairGuard::commit`] makes
    /// -- one observation path, one event row shape -- and deliberately claims
    /// no single-flight repair slot: the caller already holds this identity's
    /// canary claim, which is what makes the confirmation single-flight. Routing
    /// it through `admit_provisional` instead is impossible by construction,
    /// since that call refuses an ACTING verdict, and a canary only ever runs on
    /// one.
    ///
    /// Like `commit`, this is an IN-MEMORY admission: it returns the row for the
    /// caller to drain to the ledger and does NOT advance the canary
    /// confirmation count, which only a caller holding a durable writer
    /// acknowledgment may do (see
    /// [`FieldCanaryRegistry::acknowledge_confirmation`]).
    ///
    /// `None` for any refused guarded mutation -- Stale, Reserved, or Exhausted.
    /// Nothing was recorded and no event may ride out, and the claim settles
    /// INCONCLUSIVE rather than confirmed: a confirmation that did not persist
    /// vouches for nothing, so the identity's `modified_since_confirmation`
    /// tally must survive for a later disproof to charge to the alarm.
    ///
    /// `None` also for a claim whose incarnation has been SUPERSEDED, and that
    /// check runs BEFORE the observation: observing refreshes the learned row's
    /// decay and increments its `observations`, so a straggler reaching it would
    /// corroborate a lifecycle it never tested. Such a claim only releases.
    pub fn record_canary_confirmation(
        &self,
        key: &FieldVerdictKey,
        claim: crate::field_canary::CanaryClaimGuard<'_>,
        generation: u64,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> Option<CapabilityLearnEvent> {
        // Ownership first: nothing observable may happen on behalf of a claim
        // whose lifecycle has already been carried forward.
        if !claim.owns_current_incarnation() {
            tracing::debug!(
                event = "field_canary_confirmation_stale",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                "canary confirmation abandoned: the identity moved to a new \
                 incarnation while this canary was in flight"
            );
            drop(claim);
            return None;
        }
        let observed = self.learned.observe_in_generation_with_observations(
            generation,
            &key.state_key,
            &key.capability_key,
            &key.provider_kind,
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
        let crate::learned_capability::GenerationOutcome::Applied {
            value: (_, observations),
            incarnation,
            generation: persistence_generation,
        } = observed
        else {
            tracing::debug!(
                event = "field_canary_confirmation_refused",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                "canary confirmation refused by the generation barrier: nothing recorded"
            );
            // Nothing persisted, so nothing is vouched for: INCONCLUSIVE keeps
            // the affected-request tally resident for a later disproof to charge.
            claim.settle(crate::field_canary::CanaryOutcome::Inconclusive);
            return None;
        };
        tracing::info!(
            event = "field_canary_confirmed",
            state_key = %routectl_core::sanitize_for_log(&key.state_key),
            capability_key = %key.capability_key,
            upstream_status,
            observations,
            "envelope-field verdict re-confirmed by a canary's repaired retry",
        );
        // Release the claim and move the lifecycle onto the incarnation this
        // observation minted, in one critical section. The guard is settled
        // through the carrying variant rather than the plain one, because the
        // confirmed transition and the carry must not be separately observable.
        claim.settle_confirmed_and_carry(incarnation);
        Some(CapabilityLearnEvent {
            persistence_generation,
            incarnation,
            state_key: key.state_key.clone(),
            capability_key: key.capability_key.clone(),
            provider_kind: key.provider_kind.clone(),
            signal_tier: SignalTier::SelfIdentifying,
            observations,
            upstream_status,
            remapped: false,
            request_features,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        })
    }

    /// Durably clear `key` because a canary DISPROVED its verdict: the field
    /// the verdict said was refused was accepted unrepaired.
    ///
    /// Takes the claim by value because SETTLING and CLEARING are one operation.
    /// The settlement is incarnation-scoped, but the removal names only the
    /// identity -- no generation or incarnation token makes a guarded removal
    /// select a lifecycle -- so a claim whose lifecycle was carried forward while
    /// it was in flight would remove the row belonging to the lifecycle that
    /// REPLACED the one it tested. A canary has evidence only about the verdict it
    /// actually probed.
    ///
    /// The claim's settlement therefore reports whether it owned the current
    /// lifecycle, and `None` comes back for a superseded one: it touches nothing
    /// at all -- it suspends nothing, charges nothing, removes nothing, and does
    /// not free the slot, which by then belongs to whatever canary the current
    /// lifecycle is running.
    ///
    /// ORDER, for an owning claim: the settlement runs FIRST, because it is what
    /// suspends pre-flight and transfers the affected-request tally into the
    /// alarm, and both must be true before the durable clear is attempted -- the
    /// clear can be refused, and a verdict known to be wrong must stop moving
    /// traffic regardless.
    ///
    /// The removal itself is the same one [`FieldRepairGuard::clear`] performs,
    /// for the same reason and through the same guarded call, so a disproof and an
    /// accepted unrepaired reactive attempt cannot diverge in what they remove or
    /// in what the ledger records. `Some` when a resident entry was actually
    /// removed -- the caller drains it so a warm rebuild cannot resurrect the
    /// verdict this canary disproved.
    ///
    /// The identity's canary state is dropped ONLY on a removal that actually
    /// removed a row. Two distinct outcomes leave it resident, for the same
    /// reason: a REFUSED removal (Stale, Reserved, Exhausted -- the guarded
    /// mutation never ran) and an APPLIED removal that found nothing resident
    /// (`Applied { value: false }`). In both, the settlement has already
    /// suspended pre-flight for the identity and charged its affected-request
    /// tally to the alarm; dropping the state would lift that suspension while
    /// the row it describes may still be resident and still acting.
    ///
    /// A retained suspension is STICKY, and deliberately so -- but the residual
    /// case is narrow and worth naming precisely, because a relearn does NOT
    /// clear it on its own. Learned incarnations are per-entry and start at
    /// zero, so a row removed and relearned typically returns on the SAME
    /// incarnation the suspension was recorded against; the monotonic
    /// admission then reads the relearn as `Current`, not `Newer`, and reseeds
    /// nothing. The suspension is lifted only by dropping the state outright:
    /// a clear that actually applies ([`Self::clear`] /
    /// [`FieldRepairGuard::clear`]), a completed operator purge
    /// (`Router::finalize_learned_capability_purge`), or a process restart,
    /// whose cold rebuild seeds canary state afresh from the replayed ledger.
    /// Residual exposure: an identity whose clear kept failing stays out of
    /// pre-flight until one of those happens. That is the safe direction --
    /// reactive forward-and-repair still serves every request -- but it is
    /// stickiness, not self-healing, and a caller must not assume a relearn
    /// restores pre-flight.
    pub fn record_canary_disproof(
        &self,
        key: &FieldVerdictKey,
        claim: crate::field_canary::CanaryClaimGuard<'_>,
        generation: u64,
    ) -> Option<CapabilityClearedEvent> {
        // Ownership and the effects it authorizes, in one critical section:
        // nothing durable may happen on behalf of a claim whose lifecycle was
        // already carried forward.
        if !claim.settle_disproved_if_current() {
            tracing::debug!(
                event = "field_canary_disproof_stale",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                "canary disproof abandoned: the identity moved to a new \
                 incarnation while this canary was in flight"
            );
            return None;
        }
        let removed = self.learned.remove_keyed_in_generation(
            generation,
            &key.state_key,
            &key.capability_key,
            &key.provider_kind,
        );
        let crate::learned_capability::GenerationOutcome::Applied {
            value: cleared,
            incarnation,
            generation: persistence_generation,
        } = removed
        else {
            tracing::debug!(
                event = "field_canary_clear_refused",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                "canary clear refused by the generation barrier: pre-flight stays \
                 suspended for this identity"
            );
            return None;
        };
        if !cleared {
            return None;
        }
        // Only a removal that ACTUALLY removed a row drops the canary state.
        // `Applied { value: false }` means the guarded mutation ran but found
        // nothing resident to remove, so the identity's suspension -- and the
        // tally its disproof charged -- must stay exactly where the settlement
        // put them.
        self.canaries.reset(key);
        tracing::info!(
            event = "field_canary_disproved",
            state_key = %routectl_core::sanitize_for_log(&key.state_key),
            capability_key = %key.capability_key,
            "envelope-field verdict cleared: a canary's unrepaired request was accepted",
        );
        Some(CapabilityClearedEvent {
            persistence_generation,
            incarnation,
            state_key: key.state_key.clone(),
            capability_key: key.capability_key.clone(),
            provider_kind: key.provider_kind.clone(),
        })
    }

    /// Reconcile the acknowledged confirmation count for `key` from a capability
    /// event whose ledger write has DURABLY LANDED.
    ///
    /// THE production writer of the confirmation half of pre-flight eligibility,
    /// and the one that closes the live-acknowledgment gap: a reactive repair
    /// that mints a verdict emits a [`CapabilityLearnEvent`] carrying the
    /// observation count the guarded mutation produced, and once the durable
    /// writer acknowledges THAT event's row, the count it names is acknowledged
    /// evidence rather than an in-memory tally. So the verdict becomes pre-flight
    /// eligible during the same process, without a restart.
    ///
    /// # Why the acknowledgment cannot be inferred from the mutation
    ///
    /// The in-memory admission ([`FieldRepairGuard::commit`],
    /// [`Self::record_canary_confirmation`]) reports
    /// `GenerationOutcome::Applied`, which means the shared registry accepted the
    /// mutation through the generation barrier -- it says nothing about whether
    /// the event row describing it was written. Advancing the count on `Applied`
    /// would make a verdict pre-flight eligible whose event the writer then
    /// dropped (a full channel, a degraded database, a purge that superseded it),
    /// and the next boot's replay would find no evidence for the very verdict that
    /// had been rewriting traffic. So the count moves on the ACKNOWLEDGMENT and
    /// nowhere else.
    ///
    /// # What a caller must present, and why each half is checked
    ///
    /// `generation` and `incarnation` are the event's OWN stamps, from the guarded
    /// mutation that produced it, and BOTH are validated against live state before
    /// anything moves:
    ///
    /// - the GENERATION, because an event stamped before a boundary describes a
    ///   catalog revision the daemon has left. Acknowledging it would raise a
    ///   count against a lifecycle the boundary evicted.
    /// - the INCARNATION, because a purge and a later relearn of one key both
    ///   happen inside one generation. A delayed pre-purge acknowledgment carries
    ///   the superseded incarnation, and raising the post-purge lifecycle's count
    ///   from it would credit the new verdict with the old one's evidence. The
    ///   monotonic admission inside
    ///   [`FieldCanaryRegistry::acknowledge_confirmation`] refuses a superseded
    ///   one; the ACTING check here additionally refuses an acknowledgment for an
    ///   identity whose row is no longer resident or no longer acting at all.
    ///
    /// `false` for every refusal, and nothing is written on any of them: an
    /// unacknowledged, failed, timed-out, or stale write must not advance
    /// eligibility, because each leaves the durable record the count claims to
    /// describe absent.
    ///
    /// `now` is the instant the acting check reads decay against, passed in rather
    /// than sampled here so the caller's own consistency window governs.
    pub fn acknowledge_durable_confirmation(
        &self,
        key: &FieldVerdictKey,
        generation: u64,
        incarnation: u64,
        observations: u32,
        now: Instant,
    ) -> bool {
        // The ACTING facts under the event's own generation. A `None` here covers
        // every way the identity can have moved on: absent, lapsed, no longer
        // acting, or a generation this event may not write against.
        let Some(facts) = self.learned.field_acting_facts_in_generation(
            generation,
            &key.state_key,
            &key.capability_key,
            &key.provider_kind,
            now,
        ) else {
            tracing::debug!(
                event = "field_confirmation_ack_refused",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                reason = "not_acting_in_generation",
                "durable confirmation acknowledgment refused: no acting verdict \
                 for this identity under the event's own generation"
            );
            return false;
        };
        // THE INCARNATION MATCH. A delayed acknowledgment from a superseded
        // lifecycle must not credit the lifecycle that replaced it, and one from a
        // FUTURE incarnation must not be admitted either -- it would reseed the
        // canary state (dropping a live cadence and claim) on the strength of an
        // event whose own mutation this registry has not seen.
        if facts.incarnation != incarnation {
            tracing::debug!(
                event = "field_confirmation_ack_refused",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                reason = "incarnation_superseded",
                "durable confirmation acknowledgment refused: the identity's \
                 lifecycle moved while this event was in flight"
            );
            return false;
        }
        // THE CANARY LAYER'S OWN VERDICT, propagated rather than assumed.
        //
        // The check above validates against the LEARNED row; this validates against
        // the resident CANARY state, and a straggler can be current by one and
        // superseded by the other -- the canary state is reseeded by a rebuild, a
        // clear, and a confirmation carry, none of which the learned incarnation
        // tracks. So this call can refuse an acknowledgement the check above admitted.
        //
        // Reporting `true` regardless (the previous shape) was a FALSE SUCCESS: it
        // logged "acknowledged" and told the caller the count now backs eligibility
        // for a call that wrote nothing. The count alone could not have caught it
        // either -- a refusal reports the standing count, so a refusal at three and an
        // acceptance at three are the same number.
        let ack = self
            .canaries
            .acknowledge_confirmation(key, incarnation, observations);
        if !ack.accepted {
            tracing::debug!(
                event = "field_confirmation_ack_refused",
                state_key = %routectl_core::sanitize_for_log(&key.state_key),
                capability_key = %key.capability_key,
                reason = "canary_state_superseded",
                standing_observations = ack.confirmations,
                "durable confirmation acknowledgment refused: the identity's canary \
                 state names a newer lifecycle, so nothing was written"
            );
            return false;
        }
        tracing::debug!(
            event = "field_confirmation_acknowledged",
            state_key = %routectl_core::sanitize_for_log(&key.state_key),
            capability_key = %key.capability_key,
            observations = ack.confirmations,
            "durable confirmation acknowledged: this verdict's acknowledged count \
             now backs pre-flight eligibility"
        );
        true
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
/// The re-read in [`FieldVerdictRegistry::preflight_eligible_incarnation`] closes a real
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
