//! The per-verdict INFO status/doctor row: everything an operator needs to
//! explain, audit, or distrust one resident envelope-field verdict, read off
//! live state in one pass.
//!
//! # Why this is its own read rather than fields on the counters snapshot
//!
//! [`super::FieldRepairCounters`] answers "how much has happened", which is a
//! handful of process-wide totals. This answers "what is resident, on what
//! evidence, and what is it waiting for", which is one row PER IDENTITY: the
//! capability key and target it applies to, the transform class and whether that
//! class rewrites a cache prefix, where the evidence came from, how many
//! confirmations back it against how many its class requires, what is currently
//! blocking it if anything, and where its re-verification cadence stands.
//! Folding per-identity rows into a totals struct would force a reader to
//! reconcile them, and the two move on different schedules.
//!
//! # Purity
//!
//! EVERY read here is non-mutating, and that is a hard contract rather than a
//! convention: a status poll runs every few seconds, so a read that ticked a
//! cadence would re-verify a verdict on the operator's dashboard refresh rate
//! instead of on traffic, and one that claimed a canary would consume the slot a
//! real request needs. The registries expose non-mutating snapshot reads for
//! exactly this, and nothing here calls anything else -- no cadence tick, no
//! canary claim, no modified-request accounting, no lane activation, no
//! scheduling. A source guard in the test sidecar refuses the mutating names.
//!
//! # What is deliberately NOT here
//!
//! A next-canary TIMESTAMP. The cadence is a request countdown, so any instant
//! derived from it is a projection of future traffic rather than a schedule --
//! and an operator reading a timestamp treats it as one. The remaining eligible
//! request count is the honest form of the same fact.

use std::time::Instant;

use routectl_core::{EvidenceSource, FailurePhase, sanitize_for_log};

use crate::field_canary::{CanaryOutcome, CanaryStateSnapshot};
use crate::field_capability::capability_key_is_catalog_scoped;
use crate::field_verdict::FieldVerdictKey;
use crate::learned_capability::LearnedRegistryEntry;

use super::Router;
use super::field_repair::TransformClass;
use super::{ActingFieldVerdict, FieldRepairCounters};

/// Why an otherwise-resident verdict is not currently acting pre-flight.
///
/// A CLOSED set with its OWN stable token table, owned in this module. That
/// ownership is a deliberate reversal of the obvious arrangement, so the reason is
/// worth stating: reading the planner's internal constants would look like it
/// prevents drift, but it inverts which surface is stable. The planner's tokens
/// are an INTERNAL diagnostic vocabulary its own module may retune -- a rename
/// there would silently change what every operator dashboard, alert, and log query
/// is matching on, with no signal at either site. A status token is a
/// longer-lived contract than a debug-line token, so it is spelled here and pinned
/// here.
///
/// What keeps the two from MEANING different things is not a shared literal: it is
/// that each variant is produced by consulting the planner's own predicate
/// (`preflight_authorization`, `required_quorum`, the target-spec membership), not
/// by paraphrasing its conditions. A test additionally pins that every token this
/// surface can emit is distinct and stable.
///
/// `None` (rendered as the `none` token) means nothing is blocking: the
/// verdict is eligible and its class's gates are clear.
/// `#[non_exhaustive]`: this set GROWS as the planner gains gates, and a foreign
/// exhaustive `match` on it would then fail to compile on a purely additive change.
/// The attribute forces a `..` arm at foreign call sites, so a new reason lands there
/// as a deliberate decision rather than as a silent fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreflightBlockedReason {
    /// The verdict is not pre-flight eligible at all: not acting, not
    /// acknowledged, or unactionable on this build's closed table.
    NotEligible,
    /// Eligible, but short of this transform class's confirmation quorum.
    BelowQuorum,
    /// At quorum, but the class needs an explicit `[fidelity]` target opt-in
    /// this target does not have.
    NoTargetOptIn,
    /// The capability kill switch is off (`capability.enabled = false`), so no learned
    /// verdict can act at all. Distinct from the per-key reasons below because it is
    /// a GLOBAL setting, and the remedy is flipping it rather than providing evidence.
    CapabilityDisabled,
    /// The target's lane is not one this stage acts on: either it is not an
    /// Anthropic-API provider, or it has no attributable base URL (a forwarded
    /// credential, a local hop, or a Bedrock Mantle entry). A verdict on such a lane
    /// is REAL (the registry holds it from a reactive repair or a ledger replay), but
    /// pre-flight will never fire on it.
    UnsupportedLane,
    /// An operator `[capability.overrides]` cell forces this capability SUPPORTED
    /// for the target, so no learned verdict may act on it.
    ///
    /// First in precedence, matching the planner: the operator has said to send this
    /// field, and a learned verdict does not override that. Reported distinctly
    /// because it is the one blocked reason whose remedy is a CONFIG edit rather than
    /// more evidence -- an operator reading `not_eligible` for a masked cell would go
    /// looking for missing confirmations that would never help.
    MaskedByOverride,
    /// A canary disproved this verdict and pre-flight is suspended for it while
    /// the durable clear is negotiated.
    ///
    /// Distinct from [`Self::NotEligible`], which the suspension also implies:
    /// the planner folds the two together because its decision is the same
    /// either way, but an operator needs the difference -- a suspension is a
    /// verdict known to be WRONG whose clear may be failing, while the rest of
    /// that arm is a verdict not yet known to be right.
    CanarySuspended,
    /// Durable capability-event persistence cannot currently be guaranteed, so
    /// learned pre-flight is suspended for EVERY verdict.
    ///
    /// Reported distinctly from every verdict-level reason, and the distinction is
    /// the operator's whole action: this says nothing about whether the verdict is
    /// right. Every escape hatch a pre-flight rewrite depends on is a
    /// capability-event write -- the durable clear a disproving canary performs,
    /// the operator purge, the confirmation acknowledgment -- so while those
    /// cannot be guaranteed a wrong verdict could not be durably retracted. An
    /// operator seeing `not_eligible` or `below_quorum` here would go looking for
    /// missing evidence; what is actually needed is a healthy writer.
    ///
    /// Reactive forward-and-repair is UNAFFECTED, so a lane reporting this is
    /// still fully served -- it forwards and repairs on rejection rather than
    /// rewriting ahead of one.
    CapabilityWriterUnhealthy,
}

impl PreflightBlockedReason {
    /// The stable operator-facing token for this reason.
    ///
    /// THE table, owned here -- see the type's own docs for why these are spelled
    /// rather than read from the planner's internal constants. Every literal in
    /// this match is a status contract: changing one changes what operator queries
    /// match, so it is a deliberate breaking edit rather than an internal rename.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotEligible => "not_eligible",
            Self::BelowQuorum => "below_quorum",
            Self::NoTargetOptIn => "no_target_opt_in",
            Self::CanarySuspended => "canary_suspended",
            Self::CapabilityDisabled => "capability_disabled",
            Self::UnsupportedLane => "unsupported_lane",
            Self::MaskedByOverride => "masked_by_override",
            Self::CapabilityWriterUnhealthy => "capability_writer_unhealthy",
        }
    }
}

/// Rendered blocked reason for a verdict nothing is blocking.
///
/// A literal rather than an absent field: an absent field on a status surface
/// reads as an unavailable panel, and "unblocked" is precisely the state an
/// operator most needs to see unambiguously -- it is the state in which
/// pre-flight is rewriting their traffic.
///
/// CRATE-INTERNAL. Consumers read it through [`FieldVerdictStatus::blocked_reason_token`],
/// which is the whole reason the accessor exists: a caller that needed the literal
/// separately would be re-deciding what an absent reason means. Publishing it would
/// pin a rendering detail as semver surface for no consumer.
pub(super) const BLOCKED_REASON_NONE: &str = "none";

/// Where a canary stands for one identity, as a closed token.
///
/// Three states rather than two booleans, because the pair (due, claimed) has no
/// meaningful fourth combination and a reader given two booleans has to know
/// that to interpret them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryPosture {
    /// Counting down: neither due nor in flight.
    Counting,
    /// The countdown tripped and no claim has consumed the trip yet, so the next
    /// eligible request claims it.
    Due,
    /// A canary is claimed and its outcome is outstanding.
    InFlight,
}

impl CanaryPosture {
    /// Closed-set token for the status surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counting => "counting",
            Self::Due => "due",
            Self::InFlight => "in_flight",
        }
    }
}

/// Rendered last-canary outcome for an identity no canary has settled yet.
///
/// Distinguished from every real outcome deliberately: reporting an unsettled
/// identity as `inconclusive` would make a re-verification that never ran look
/// like one that ran and proved nothing.
///
/// CRATE-INTERNAL, like `BLOCKED_REASON_NONE` and for the same reason: consumers
/// read it through [`FieldVerdictStatus::canary_outcome_token`].
pub(super) const CANARY_OUTCOME_NONE: &str = "none";

/// The closed token for one settled canary outcome.
///
/// Owned HERE rather than derived from a `Serialize` on the registry's enum, so a
/// new outcome variant is a compile error on this surface rather than a silent
/// wire change -- the same discipline the status health panel applies to the
/// breaker phases.
///
/// CRATE-INTERNAL: its one consumer is the row accessor below. A caller holding a
/// bare `CanaryOutcome` on the status surface would have obtained it from a row,
/// which already renders it.
#[must_use]
pub(super) const fn canary_outcome_token(outcome: CanaryOutcome) -> &'static str {
    match outcome {
        CanaryOutcome::Confirmed => "confirmed",
        CanaryOutcome::Regressed => "regressed",
        CanaryOutcome::Inconclusive => "inconclusive",
    }
}

/// One resident envelope-field verdict, as the INFO status/doctor surface
/// reports it.
///
/// `#[non_exhaustive]`: a growth type, like [`super::FieldRepairCounters`]. Every
/// field is a closed-set token, a code-authored path literal, a
/// `sanitize_for_log`-sanitized state key, or a count -- deliberately NO request
/// value, response body, credential, session key, or upstream text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldVerdictStatus {
    /// Sanitized `[providers]` state key of the target this verdict applies to.
    pub state_key: String,
    /// The normalized `field:`-namespaced capability key, read off the resident
    /// row rather than re-minted (so the token reported is the token the registry
    /// is keyed on) and then SANITIZED.
    ///
    /// Sanitized for the same reason as the state key beside it: a resident key is
    /// normally a closed-table token, but the registry is fed from a ledger this
    /// build did not necessarily write, so the read side must not assume its shape.
    pub capability_key: String,
    /// Closed-set transform-class token, or `None` for a resident verdict whose
    /// path this build's closed table does not carry -- what a verdict persisted
    /// by a build with a wider table looks like here. Reported as absent rather
    /// than guessed: the class decides which gates apply, so inventing one would
    /// misreport what the verdict is waiting for.
    pub transform_class: Option<&'static str>,
    /// Whether this class's transform rewrites content the upstream hashes into
    /// its cache prefix.
    ///
    /// `false` for an unknown class, which is the conservative reading for a
    /// display flag: it claims no prefix cost it cannot substantiate, and the
    /// absent class beside it says why.
    pub prefix_impacting: bool,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    pub source: EvidenceSource,
    /// The detection phase that attributed this verdict.
    pub phase: FailurePhase,
    /// Acknowledged confirmation cycles backing this verdict.
    pub confirmations: u32,
    /// Confirmation cycles this verdict's transform class REQUIRES before a
    /// pre-flight rewrite may run.
    ///
    /// Reported beside the count rather than left to the reader: the required
    /// value is a code constant that differs per class, so a bare count of one is
    /// either sufficient or half-sufficient depending on a fact the row would
    /// otherwise not carry. `None` only when the class is unknown.
    pub required_quorum: Option<u32>,
    /// Why pre-flight is not acting for this verdict, or `None` when nothing is
    /// blocking it.
    pub blocked_reason: Option<PreflightBlockedReason>,
    /// Where this identity's canary stands.
    pub canary: CanaryPosture,
    /// Eligible non-streaming completion requests remaining before the next
    /// canary is due.
    ///
    /// A REQUEST count, never a timestamp: the cadence is driven by traffic, so
    /// any instant derived from it would be a projection an operator reads as a
    /// schedule.
    pub canary_remaining_requests: u32,
    /// The most recently settled canary outcome, or `None` when none has settled
    /// for this incarnation.
    pub canary_last_outcome: Option<CanaryOutcome>,
    /// Requests currently applying this verdict's repair.
    pub requests_in_flight: u64,
    /// Requests this verdict has modified since its last confirmation -- the
    /// exposure a later disproof would charge to the lifetime alarm.
    pub unconfirmed_requests: u64,
}

impl FieldVerdictStatus {
    /// Rendered blocked reason, with the unblocked case spelled explicitly.
    #[must_use]
    pub fn blocked_reason_token(&self) -> &'static str {
        self.blocked_reason
            .map_or(BLOCKED_REASON_NONE, PreflightBlockedReason::as_str)
    }

    /// Rendered last-canary outcome, with the never-settled case spelled
    /// explicitly.
    #[must_use]
    pub fn canary_outcome_token(&self) -> &'static str {
        self.canary_last_outcome
            .map_or(CANARY_OUTCOME_NONE, canary_outcome_token)
    }
}

impl Router {
    /// The whole fidelity surface in ONE pass. See [`FidelitySnapshot`].
    ///
    /// PURE: the learned snapshot, the canary snapshots, the metrics atomics, and
    /// the scheduler snapshot are all non-mutating reads. Nothing here activates a
    /// lane, advances a cadence, claims a slot, or schedules work -- which is what
    /// makes it admissible on a surface an operator polls every few seconds.
    #[must_use]
    pub fn fidelity_snapshot(&self) -> FidelitySnapshot {
        // ONE learned read, feeding BOTH derivations. This is the whole reason the
        // type exists: two reads could disagree, and a reader reconciling the two
        // counts could not tell a documented filter from a race.
        let learned = self.learned_capability_snapshot();
        FidelitySnapshot {
            counters: self.field_repair_counters(),
            acting: super::acting_field_verdicts(&learned),
            verdicts: self.verdict_rows_from(learned),
            probes: self.probe_scheduler_snapshot(),
        }
    }

    /// One status row per resident field-namespace verdict, read off live state
    /// without mutating any of it.
    ///
    /// Reads the learned registry's own snapshot, ONE bulk acting-incarnation
    /// projection, and per row that identity's canary snapshot -- all non-mutating.
    ///
    /// The eligibility facts come from that BULK projection rather than from a
    /// per-row `preflight_authorization` call, and the reason is a measured one: the
    /// per-key guarded read takes an EXCLUSIVE `entries` write lock, because the
    /// guarded primitive is shared with the mutating paths that need lease-awareness
    /// in one critical section. A thousand resident verdicts therefore meant a
    /// thousand exclusive acquisitions per poll, on the lock every dispatch needs.
    /// The bulk projection answers the same question for every key under one shared
    /// acquisition.
    ///
    /// An earlier version of this comment claimed the read "never takes a lock a
    /// dispatch needs". That was false -- it took the dispatch lock once per row --
    /// and it is corrected here rather than deleted, because the claim is exactly
    /// the kind a future reader would otherwise trust.
    ///
    /// Rows are produced for every resident field verdict, acting or not: an
    /// operator debugging why pre-flight is NOT firing needs the row whose
    /// blocked reason explains it, and a surface listing only acting rows would
    /// answer that question with silence.
    #[must_use]
    pub fn field_verdict_status(&self) -> Vec<FieldVerdictStatus> {
        self.verdict_rows_from(self.learned_capability_snapshot())
    }

    /// The detailed rows for an ALREADY-FETCHED learned snapshot.
    ///
    /// Split from the fetch so [`Self::fidelity_snapshot`] can derive these and the
    /// acting list from one read. Both entry points share this body, so the rows a
    /// coherent snapshot carries are the same rows the standalone call returns --
    /// there is no second projection to drift.
    fn verdict_rows_from(&self, learned: Vec<LearnedRegistryEntry>) -> Vec<FieldVerdictStatus> {
        // ONE bulk acquisition for every key's acting incarnation, taken before the
        // per-row walk.
        let acting = self
            .learned_capabilities
            .field_acting_incarnations(Instant::now());
        learned
            .into_iter()
            .filter(|entry| !capability_key_is_catalog_scoped(&entry.feature_key))
            .map(|entry| self.status_row_for(entry, &acting))
            .collect()
    }

    /// Assemble one row. Split out so the per-row read is one named thing and the
    /// projection above stays a projection.
    fn status_row_for(
        &self,
        entry: LearnedRegistryEntry,
        acting: &std::collections::HashMap<(String, String), u64>,
    ) -> FieldVerdictStatus {
        let provider_kind = self
            .provider_kind_for_state_key(&entry.state_key)
            .to_string();
        // Rebuilt from the RESIDENT row's already-normalized key rather than
        // re-minted from a path: re-minting would re-run the namespace grammar on
        // a key that is already persisted, which can only ever agree with the
        // check that admitted it -- and would silently drop a row whose key a
        // stricter grammar no longer accepts, hiding exactly the verdict an
        // operator is looking for.
        let key = FieldVerdictKey::from_capability_key(
            entry.state_key.clone(),
            entry.feature_key.clone(),
            provider_kind,
        );
        let snapshot = self.field_verdicts().canaries().snapshot(&key);
        let class =
            crate::router::field_repair::transform_class_of_capability_key(&entry.feature_key);
        let confirmations = snapshot.map_or(0, |s| s.confirmations);
        FieldVerdictStatus {
            state_key: sanitize_for_log(&entry.state_key),
            capability_key: sanitize_for_log(&entry.feature_key),
            transform_class: class.map(TransformClass::as_str),
            prefix_impacting: class.is_some_and(TransformClass::requires_target_opt_in),
            source: entry.source,
            phase: entry.phase,
            confirmations,
            required_quorum: class.map(TransformClass::required_quorum),
            blocked_reason: self.blocked_reason_for(&key, class, confirmations, snapshot, acting),
            canary: canary_posture(snapshot),
            // An identity with no resident canary state has consumed none of its
            // interval, so the WHOLE interval remains -- which is exactly what a
            // cold boot before the first eligible request means. Reporting zero
            // there would read as "due now" for a verdict whose cadence has not
            // started.
            canary_remaining_requests: snapshot
                .map_or(crate::config::CANARY_INTERVAL, |s| s.cadence),
            canary_last_outcome: snapshot.and_then(|s| s.last_outcome),
            requests_in_flight: snapshot.map_or(0, |s| s.outstanding),
            unconfirmed_requests: snapshot.map_or(0, |s| s.modified_since_confirmation),
        }
    }

    /// Why pre-flight is not acting for `key`, or `None` when nothing blocks it.
    ///
    /// Ordered to match the planner's gate order. A suspension is reported with
    /// its own token after the config, lane, and override gates; the planner folds
    /// that state into not-eligible because its dispatch decision is the same, but
    /// the status surface separates it because the operator action differs.
    ///
    /// Eligibility comes from the bulk acting-incarnation projection assembled for
    /// this snapshot. It carries the same facts the planner reads without taking the
    /// registry's exclusive per-key authorization lock for every status row.
    fn blocked_reason_for(
        &self,
        key: &FieldVerdictKey,
        class: Option<TransformClass>,
        confirmations: u32,
        snapshot: Option<CanaryStateSnapshot>,
        acting: &std::collections::HashMap<(String, String), u64>,
    ) -> Option<PreflightBlockedReason> {
        // CONFIG-LEVEL gates, in the planner's own order. These outrank every
        // per-key check because they refuse the LANE, not the verdict -- a
        // verdict on a refused lane is real but can never fire.
        if !self.config.capability.enabled {
            return Some(PreflightBlockedReason::CapabilityDisabled);
        }
        if !self.lane_supports_preflight(key) {
            return Some(PreflightBlockedReason::UnsupportedLane);
        }
        // The operator MASK, after the lane gates: the mask is about a specific
        // capability on a supported lane, not about a lane the stage would refuse
        // regardless.
        if self.override_masks_capability(key) {
            return Some(PreflightBlockedReason::MaskedByOverride);
        }
        // PERSISTENCE, in the planner's own order: after the operator mask (which
        // is the operative reason at any writer health -- a healthy writer would
        // not change a masked row's fate) and ahead of every evidence gate (it is
        // not a verdict gate, so reporting a confirmation shortfall here would send
        // an operator looking for evidence that would never help).
        if !self.capability_writes_durable() {
            return Some(PreflightBlockedReason::CapabilityWriterUnhealthy);
        }
        if snapshot.is_some_and(|s| s.preflight_suspended) {
            return Some(PreflightBlockedReason::CanarySuspended);
        }
        // The eligibility facts come from the BULK projection rather than from the
        // planner's own `preflight_authorization`, which would take an exclusive
        // dispatch lock per row. The FACTS are the same three the planner reads --
        // the entry is acting under this generation (its presence here), the canary
        // state carries an acknowledged confirmation for that same incarnation, and
        // the count clears the class's quorum -- so this is the same decision from
        // one shared read rather than a paraphrase of a different one.
        //
        // The incarnation match is the half that matters: a confirmation left over
        // from a since-relearned lifecycle must never read as backing the current
        // one, which is exactly what the planner's own check prevents.
        let eligible = acting
            .get(&(
                key.status_state_key().to_string(),
                key.capability_key().to_string(),
            ))
            .is_some_and(|incarnation| {
                snapshot.is_some_and(|s| {
                    s.incarnation == *incarnation
                        && s.confirmations >= crate::field_verdict::MINIMUM_CONFIRMATIONS
                })
            });
        if !eligible {
            return Some(PreflightBlockedReason::NotEligible);
        }
        // A class this build does not carry is UNACTIONABLE, so it is blocked --
        // not unblocked. Returning `None` here (the shape this replaces) reported a
        // verdict nothing can act on as one that is actively rewriting traffic, which
        // is the single most misleading value this field can carry: an operator would
        // read a resident foreign-key row from a wider build's ledger as live.
        //
        // Reported as not-eligible rather than below-quorum because such a verdict is
        // not short of confirmations -- there is no quorum to be short of -- and the
        // absent class beside it is what says which case this is.
        let Some(class) = class else {
            return Some(PreflightBlockedReason::NotEligible);
        };
        if confirmations < class.required_quorum() {
            return Some(PreflightBlockedReason::BelowQuorum);
        }
        if class.requires_target_opt_in() && !self.prefix_impact_opted_in_for_state_key(key) {
            return Some(PreflightBlockedReason::NoTargetOptIn);
        }
        None
    }

    /// Whether the lane this verdict's target sits on is one the pre-flight planner
    /// acts on at all.
    ///
    /// Derived from the same two predicates the planner's own
    /// `preflight_lane_admits` and `preflight_identity` read: the provider kind
    /// must be `anthropic-api`, and the entry must carry an attributable base URL
    /// (not a forwarded credential, a local hop, or none). A status read cannot
    /// check `use_forwarded_credential` because it has no `DispatchTarget` -- what
    /// it has is the state key, which resolves to the provider entry's own base
    /// URL and kind. A forwarded credential is a per-request property the status
    /// surface does not carry, so it is not checked here; the verdict row's
    /// blocked reason may therefore read `not_eligible` on a lane whose REQUESTS
    /// are forwarded, which is the correct conservative reading rather than a
    /// false claim about the lane itself.
    fn lane_supports_preflight(&self, key: &FieldVerdictKey) -> bool {
        let pk = self.provider_kind_for_state_key(key.status_state_key());
        if pk != super::field_repair::ANTHROPIC_API_KIND {
            return false;
        }
        let pn = self.provider_name_for_state_key(key.status_state_key());
        crate::router::field_repair::attributable_anthropic_base_url(&self.config, &pn, false)
            .is_some()
    }

    /// The provider ENTRY name for a state key, for the lane predicate above and the
    /// override mask below.
    ///
    /// MEMBER-FIRST, then the model tables, then a pool fallback. For a pooled seat
    /// `nick#member` the MEMBER suffix is the provider entry a live `DispatchTarget`
    /// carries as its `provider_name` (see `chain::dispatch_target_for_seat`), so it
    /// is resolved first and the base is left to key the MODEL lookup rather than the
    /// provider one. A key that names no member falls through to the resolved models
    /// (a live Router), then the configured ones (cold boot, or a provider that failed
    /// to build), then the base of a `#`-suffixed key whose suffix resolved nothing,
    /// and finally the key itself -- a provider-scoped key with no model.
    ///
    /// The order is load-bearing: a pool-backed model's own `provider_name` is the
    /// POOL, which is not a `[providers]` entry at all, so resolving through it first
    /// would yield a name no provider lookup can answer and every decision keyed on it
    /// would fall through to a default.
    fn provider_name_for_state_key(&self, state_key: &str) -> String {
        // Pooled seat: `nick#member` -> the SUFFIX is the provider entry name.
        if let Some((_base, member)) = state_key.split_once('#')
            && self.config.providers.contains_key(member)
        {
            return member.to_string();
        }
        // Resolved models (a live Router with installed providers).
        if let Some(model) = self.resolved_models.get(state_key) {
            return model.provider_name.clone();
        }
        // Configured model.
        if let Some(model) = self.config.models.get(state_key) {
            return model.provider.clone();
        }
        // Pool fallback: if the base part is a configured model.
        if let Some((base, _)) = state_key.split_once('#')
            && let Some(model) = self.config.models.get(base)
        {
            return model.provider.clone();
        }
        state_key.to_string()
    }

    /// Whether an operator `[capability.overrides]` cell forces this capability
    /// supported for the target `key` names.
    ///
    /// Resolves the state key to its `(provider, nickname)` pair through
    /// `override_identity_for` -- the same shared resolution every other
    /// state-key-keyed decision uses -- and then asks the SAME override registry the
    /// planner asks, with the same normalized capability token. The planner reads the
    /// pair off a live `DispatchTarget`; a status read has only the state key, so the
    /// pair is resolved rather than invented.
    fn override_masks_capability(&self, key: &FieldVerdictKey) -> bool {
        let provider_name = self.provider_name_for_state_key(key.status_state_key());
        // The nickname is the state key itself when the model is a direct config entry,
        // and the base when it is a pooled seat. Both are what a live DispatchTarget
        // carries, and the override resolver checks model-scoped first (matching them)
        // then provider-tier (matching the provider alone).
        let nickname = key.status_state_key().split_once('#').map_or_else(
            || key.status_state_key().to_string(),
            |(base, _)| base.to_string(),
        );
        matches!(
            self.override_registry.resolve(
                &provider_name,
                &nickname,
                key.capability_key(),
                self.provider_kind_for_state_key(key.status_state_key()),
            ),
            Some((crate::override_registry::OverrideVerdict::ForceSupported, _))
        )
    }

    /// Whether the operator opted the target `key` names into prefix-impacting
    /// pre-flight.
    ///
    /// Resolves the state key to its `(provider, nickname)` pair through
    /// `override_identity_for` -- the SAME shared resolution every other
    /// state-key-keyed decision uses -- and then applies the same two-tier
    /// target-spec membership the planner applies. The planner reads the pair off
    /// a live `DispatchTarget` it already holds; a status read has only the state
    /// key, so the pair is resolved rather than invented.
    fn prefix_impact_opted_in_for_state_key(&self, key: &FieldVerdictKey) -> bool {
        let (provider_name, nickname) = self.override_identity_for(key.status_state_key());
        self.config
            .fidelity
            .prefix_impact_opt_in
            .iter()
            .any(|spec| {
                let (spec_provider, spec_model) = crate::override_registry::split_target_spec(spec);
                spec_provider == provider_name
                    && match spec_model {
                        Some(model) => model == nickname,
                        None => true,
                    }
            })
    }
}

/// The canary posture a snapshot describes.
///
/// THE single derivation, shared by the status row above and by the pre-flight
/// authorization (`FieldVerdictRegistry::preflight_authorization`), which carries
/// the posture of the ONE snapshot that permitted an action. A second spelling
/// would let the posture an operator reads on a status row and the posture a
/// decision record reports for the same identity disagree.
///
/// A claim outranks a due flag, and the ORDER of the two arms is not load-bearing
/// today: `claim_canary` clears the due flag in the same critical section that
/// takes the claim, so the two are never both set and either order reports the
/// same posture. The order is written claim-first anyway, because the invariant
/// that makes them exclusive lives in another module: if a future claim path
/// stopped clearing the flag, reporting "due" for an identity whose canary is
/// already in flight would tell an operator to expect a claim that has already
/// happened, and this order degrades to the safe answer instead.
#[expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so pub(crate) reads as redundant -- but \
              `field_verdict::preflight_authorization` calls this so the posture a \
              decision record reports and the posture a status row reports cannot \
              disagree, which makes the visibility load-bearing"
)]
pub(crate) const fn canary_posture(snapshot: Option<CanaryStateSnapshot>) -> CanaryPosture {
    match snapshot {
        Some(s) if s.canary_claimed => CanaryPosture::InFlight,
        Some(s) if s.due => CanaryPosture::Due,
        _ => CanaryPosture::Counting,
    }
}

/// The WHOLE fidelity surface, read from the router in one coherent pass.
///
/// # Why one type rather than four calls
///
/// The caller previously assembled this from a counters read, an acting-verdict
/// derivation over a learned snapshot it fetched itself, a per-verdict projection
/// that fetched its OWN learned snapshot, and a scheduler read. Four reads means
/// four moments: the acting count could describe one registry state while the
/// detailed rows described another, and a reader reconciling "3 acting" against
/// "2 rows" has no way to tell a real filter from a race. Here the acting rows and
/// the detailed rows are derived from the SAME `Vec<LearnedRegistryEntry>`, so the
/// two can disagree only by their documented filters -- never by timing.
///
/// # EXACTLY what this promises, and what it does not
///
/// It promises TWO things and no more: one facade call for the whole surface, and
/// ONE shared learned-registry snapshot behind `acting` and `verdicts`. That second
/// one is the load-bearing one -- those two lists are read against each other (the
/// acting set must be contained in the rows set), so deriving them from two reads
/// would let a reader's reconciliation fail for a reason that is not a filter.
///
/// It does NOT promise a globally consistent instant. The counters are atomics, the
/// canary states are a separate lock read per identity, and the scheduler snapshot
/// is its own lock -- each sampled independently, so a value from one can describe a
/// moment a value from another does not. That is the deliberate trade: a surface an
/// operator polls every few seconds must never hold a lock a dispatch needs, and a
/// globally-pinned read would have to. What a reader gets is a coherent DISPLAY, not
/// a transaction.
///
/// # Why the scheduler snapshot rides along rather than being flattened
///
/// It is an existing, separately-owned, separately-bounded value with its own
/// `#[non_exhaustive]` growth contract. Copying its fields in would be a second
/// place they are enumerated, which is exactly how the two come to drift; and the
/// probe state IS part of the same operator question ("why is this verdict not
/// moving"), so it belongs in the same snapshot.
///
/// `#[non_exhaustive]`: a growth type, like its members.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FidelitySnapshot {
    /// The process-lifetime repair, pre-flight, and parser counters.
    ///
    /// Sampled INDEPENDENTLY of the learned snapshot below -- these are atomics on
    /// the router's own metrics, read at their own instant. See the type docs for
    /// exactly what this snapshot does and does not promise.
    pub counters: FieldRepairCounters,
    /// Every currently ACTING field verdict, derived from the same learned
    /// snapshot as `verdicts` below.
    pub acting: Vec<ActingFieldVerdict>,
    /// One detailed row per RESIDENT field verdict, acting or not.
    ///
    /// A superset of `acting` by construction: the acting list applies the routing
    /// filter (`LearnedBroken`, and not the advisory F3-plus-live combination)
    /// while this one reports every resident row, because an operator debugging why
    /// pre-flight is NOT firing needs the row whose blocked reason explains it.
    pub verdicts: Vec<FieldVerdictStatus>,
    /// The bounded probe scheduler's own snapshot: activation, queue depth,
    /// in-flight and backing-off counts, the aggregate last settlement, and the
    /// earliest next-retry duration.
    ///
    /// Sampled INDEPENDENTLY, under the scheduler's own lock at its own instant.
    pub probes: crate::probe_scheduler::ProbeSchedulerSnapshot,
}

/// The gated fixture, seeding, and read-instrumentation seams this module's tests and
/// the cross-crate status tests need. Compiled only under `cfg(test)` or the
/// non-default `test-utils` feature, so no release build carries any of them.
#[cfg(any(test, feature = "test-utils"))]
#[path = "fidelity_status_test_support.rs"]
mod test_support;
#[cfg(any(test, feature = "test-utils"))]
pub use test_support::{
    FieldVerdictStatusSpec, field_verdict_event_stamps_for_tests, make_field_canary_due_for_tests,
    plant_acting_field_verdict_for_tests, seed_distinct_fidelity_counters_for_tests,
};

#[cfg(test)]
#[path = "fidelity_status_tests.rs"]
mod tests;
