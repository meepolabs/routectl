//! Reasoning-replay learned lifecycle: keying, two-phase learn,
//! single-flight admission, and the decay settlement paths.
//!
//! The learned-capability registry ([`LearnedCapabilityRegistry`]) owns
//! storage and decay, so warm rebuild, doctor surfacing, the
//! override-change expiry sweep and the events ledger all apply to a replay
//! negative unchanged. A committed replay negative is refreshed on a flat
//! `decay` window (it re-acts through `observe`, not the registry's
//! geometric re-probe backoff, which only `record_probe_outcome` drives).
//! This module owns the DYNAMICS specific to reasoning
//! replay, which the generic registry cannot express on its own:
//!
//! - **Keying.** A learned replay truth is per-`(scheme_tag, target_lane)`,
//!   never per-model. The replay validator is a property of the LANE, so
//!   sibling models on one lane share ONE learned entry; a model-level key
//!   would cost one learned retry per sibling to converge on a single
//!   lane-level fact. The lane discriminant is the lane's [`ReplayScheme`]
//!   (derived from its auth kind) plus the configured provider-level target
//!   key -- the caller-controlled model string never enters a key.
//! - **Two-phase learn.** The upstream rejection alone does NOT persist a
//!   negative. It opens PROVISIONAL, request-local state; the negative is
//!   persisted only once the stripped repair actually succeeds
//!   ([`ReplayProbeGuard::commit`]). A repair that fails, or an unrelated
//!   error, drops the provisional state without learning
//!   (`ReplayProbeGuard::release`). Persisting on the rejection alone
//!   would let one misread request fault silently disable working
//!   reasoning continuity until decay.
//! - **Single-flight.** Only ONE in-flight request carries artifacts for an
//!   unknown or lapsed pair; concurrent callers strip while that probe is
//!   unresolved. Otherwise N parallel requests each carry, each get
//!   rejected, and each repair -- N times the cost for one fact.
//! - **Decay round-trip.** A lapsed entry admits exactly one carry; that
//!   carry succeeding CLEARS the entry (an upstream fix re-enables
//!   continuity), the same rejection REFRESHES it, an unrelated error
//!   RELEASES it unchanged.
//!
//! # Emission
//!
//! A committed negative returns a [`CapabilityLearnEvent`], which the
//! dispatch layer pushes onto `meta.learned_capabilities` for the
//! best-effort usage-capture drain. No ledger schema change is involved:
//! the row's verdict / phase / source / tier are open-set tolerant tokens
//! and it carries only normalized keys -- never a request body, a reasoning
//! artifact, or an artifact id. Nothing in this module's API can accept
//! one.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use routectl_core::ReplayScheme;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, REASONING_REPLAY, SignalTier, normalize_capability_key,
};

use crate::learned_capability::{LearnedCapabilityRegistry, NegativeState};
use crate::router::{CapabilityClearedEvent, CapabilityLearnEvent};

/// Separator between the provider-level target key and the lane token
/// inside a lane discriminant.
const LANE_SEPARATOR: char = '#';

/// Separator between the replay capability key and the artifact scheme
/// token inside a learned replay capability key.
const SCHEME_SEPARATOR: char = ':';

/// Stable token for a validator family. It lands inside registry keys and
/// the emitted ledger row, so the mapping is fixed forever: a changed token
/// re-partitions historical rows.
const fn scheme_token(scheme: ReplayScheme) -> &'static str {
    match scheme {
        ReplayScheme::Codex => "codex",
        ReplayScheme::Mantle => "mantle",
        ReplayScheme::Gray => "gray",
    }
}

/// The identity of one learned replay truth: an artifact scheme replayed
/// onto a target lane.
///
/// Identity is `(scheme_tag, target_lane)`, split across the underlying
/// registry's two key halves:
///
/// - the lane half is `<lane_state_key>#<lane scheme token>`, where
///   `lane_state_key` is the PROVIDER-level configured target key. It is
///   deliberately not the per-model nickname the breaker keys on: sibling
///   models on one provider are several nicknames but ONE replay validator,
///   and lane keying exists precisely so they converge on a single learned
///   fact. Pinning the lane's scheme into the key means a reconfigured auth
///   kind mints a fresh identity rather than silently inheriting a truth
///   proven about the old lane.
/// - the capability half is `reasoning_replay:<artifact scheme token>`, so
///   artifacts of different provenance replayed onto one lane settle
///   independently.
///
/// `provider_kind` rides along because every registry call normalizes the
/// capability key with it; it is a property of the same provider the lane
/// key names, so it never splits the identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplayLearnKey {
    lane_key: String,
    capability_key: String,
    provider_kind: String,
}

impl ReplayLearnKey {
    /// Build the key for replaying `artifact`-scheme reasoning onto the
    /// `lane`-scheme lane of the provider named by `lane_state_key`.
    #[must_use]
    pub fn new(
        lane_state_key: &str,
        provider_kind: &str,
        lane: ReplayScheme,
        artifact: ReplayScheme,
    ) -> Self {
        let lane_key = format!("{lane_state_key}{LANE_SEPARATOR}{}", scheme_token(lane));
        let capability_key = format!(
            "{REASONING_REPLAY}{SCHEME_SEPARATOR}{}",
            scheme_token(artifact)
        );
        Self {
            // Normalize once at construction so every registry call and the
            // emitted row meet on one canonical string.
            capability_key: normalize_capability_key(&capability_key, provider_kind),
            lane_key,
            provider_kind: provider_kind.to_string(),
        }
    }

    /// The lane discriminant this entry is keyed on. Only the tests that pin
    /// key normalization read it back out; the lifecycle itself passes the
    /// whole key around.
    #[cfg(test)]
    pub fn lane_key(&self) -> &str {
        &self.lane_key
    }

    /// The normalized capability key this entry is keyed on. Test-only for the
    /// same reason as [`Self::lane_key`].
    #[cfg(test)]
    pub fn capability_key(&self) -> &str {
        &self.capability_key
    }
}

/// Two-phase, single-flight lifecycle over the learned-capability registry
/// for reasoning-replay pairs.
#[derive(Debug)]
pub struct ReplayLearnRegistry {
    learned: Arc<LearnedCapabilityRegistry>,
    /// Pairs whose carry is unresolved. Purely request-local coordination:
    /// nothing here is persisted, and every settlement path -- including a
    /// dropped guard -- clears its entry.
    in_flight: Mutex<HashSet<ReplayLearnKey>>,
}

impl ReplayLearnRegistry {
    /// Wrap the shared learned-capability registry.
    #[must_use]
    pub fn new(learned: Arc<LearnedCapabilityRegistry>) -> Self {
        Self {
            learned,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Claim the single-flight carry slot for `key`.
    ///
    /// `Some(guard)` means THIS request is the probe: carry the reasoning
    /// artifacts and settle the guard with the outcome. `None` means strip
    /// the artifacts -- either an acting negative is resident, or another
    /// request already carries this pair and its probe is unresolved.
    ///
    /// A lapsed negative is admissible: exactly one caller gets the guard
    /// and re-verifies the pair against live upstream behavior.
    pub fn admit_provisional(
        &self,
        key: &ReplayLearnKey,
        now: Instant,
    ) -> Option<ReplayProbeGuard<'_>> {
        // The claim is taken under the in-flight lock together with the
        // decay read, so two callers racing an unknown pair cannot both
        // observe "absent" and both carry.
        let mut in_flight = self.in_flight.lock();
        if in_flight.contains(key) {
            return None;
        }
        match self.negative_state(key, now) {
            NegativeState::Acting => None,
            NegativeState::Absent | NegativeState::Lapsed => {
                in_flight.insert(key.clone());
                Some(ReplayProbeGuard {
                    registry: self,
                    key: key.clone(),
                    settled: false,
                })
            }
        }
    }

    /// Whether an acting learned negative currently forces a strip for this
    /// pair, independent of any in-flight probe. Read-only: it never claims
    /// the carry slot. Test-only: the dispatch path settles through
    /// `admit_provisional`, which reads the same state while claiming.
    #[cfg(test)]
    pub fn is_negative_acting(&self, key: &ReplayLearnKey, now: Instant) -> bool {
        matches!(self.negative_state(key, now), NegativeState::Acting)
    }

    fn negative_state(&self, key: &ReplayLearnKey, now: Instant) -> NegativeState {
        self.learned
            .negative_state(&key.lane_key, &key.capability_key, &key.provider_kind, now)
    }

    fn release_slot(&self, key: &ReplayLearnKey) {
        self.in_flight.lock().remove(key);
    }
}

/// The single-flight carry claim for one `(scheme_tag, target_lane)` pair.
///
/// Holding this guard is the PROVISIONAL phase: no negative is persisted
/// while it lives. Exactly one settlement applies:
///
/// - [`commit`](ReplayProbeGuard::commit) -- the carry was rejected AND the
///   stripped repair succeeded: persist (or refresh) the negative.
/// - [`clear`](ReplayProbeGuard::clear) -- the carry SUCCEEDED: the pair
///   works; drop any resident negative.
/// - `release` -- the repair failed, or the request hit an unrelated error:
///   learn nothing, leave any resident entry exactly as it was.
///
/// Dropping the guard without settling releases the slot as `release`
/// would, so an early return, a `?` propagation, or a client disconnect can
/// never strand a pair behind a permanently claimed slot -- and can never
/// learn by omission either.
#[derive(Debug)]
pub struct ReplayProbeGuard<'a> {
    registry: &'a ReplayLearnRegistry,
    key: ReplayLearnKey,
    /// Set by whichever settlement runs, so the subsequent `Drop` cannot
    /// free a slot a different request has since claimed.
    settled: bool,
}

impl ReplayProbeGuard<'_> {
    /// Phase two: the stripped repair succeeded, so the rejection is
    /// confirmed as a real replay incompatibility. Persists the negative
    /// (refreshing a resident or lapsed one) and returns the emission row
    /// for `meta.learned_capabilities`.
    ///
    /// A refresh re-stamps the base decay window rather than applying the
    /// registry's geometric re-probe backoff: this is a corroborated
    /// self-identifying observation, not a failed probe, so a chronically
    /// broken pair re-verifies once per base decay by design (spec: the same
    /// rejection REFRESHES). The backoff ladder is reserved for the
    /// registry's own `record_probe_outcome` re-probe path.
    ///
    /// `request_features` is the request's derived in-flight feature set;
    /// no request body, artifact, or artifact id can enter the row.
    #[must_use]
    pub fn commit(
        mut self,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> CapabilityLearnEvent {
        self.settled = true;
        let key = self.key.clone();
        // A rejection corroborated by a successful stripped repair is
        // direct proof, not an inference: it acts on this one observation.
        self.registry.learned.observe(
            &key.lane_key,
            &key.capability_key,
            &key.provider_kind,
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            now,
        );
        let observations = self
            .registry
            .learned
            .snapshot()
            .into_iter()
            .find(|entry| {
                entry.state_key == key.lane_key && entry.feature_key == key.capability_key
            })
            .map_or(0, |entry| entry.observations);
        self.registry.release_slot(&key);
        tracing::info!(
            event = "replay_learn_commit",
            state_key = %key.lane_key,
            capability_key = %key.capability_key,
            upstream_status,
            observations,
            "reasoning-replay negative persisted after a successful stripped repair",
        );
        CapabilityLearnEvent {
            state_key: key.lane_key,
            capability_key: key.capability_key,
            provider_kind: key.provider_kind,
            signal_tier: SignalTier::SelfIdentifying,
            observations,
            upstream_status,
            remapped: false,
            request_features,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        }
    }

    /// The carried request SUCCEEDED: the pair replays cleanly. Drops any
    /// resident negative so continuity is re-enabled at once rather than
    /// after the remaining decay.
    ///
    /// Returns a [`CapabilityClearedEvent`] when a resident (lapsed) entry was
    /// actually removed, so the caller rides the clear out on the dispatch meta
    /// and a warm rebuild does not resurrect the negative from the ledger. A
    /// carry that never had a resident entry (an absent pair admitted for its
    /// first probe) clears nothing and returns `None`.
    pub fn clear(mut self) -> Option<CapabilityClearedEvent> {
        self.settled = true;
        let cleared = self.registry.learned.remove_keyed(
            &self.key.lane_key,
            &self.key.capability_key,
            &self.key.provider_kind,
        );
        self.registry.release_slot(&self.key);
        if cleared {
            tracing::info!(
                event = "replay_learn_clear",
                state_key = %self.key.lane_key,
                capability_key = %self.key.capability_key,
                "lapsed reasoning-replay negative cleared by a successful carry",
            );
            Some(CapabilityClearedEvent {
                state_key: self.key.lane_key.clone(),
                capability_key: self.key.capability_key.clone(),
                provider_kind: self.key.provider_kind.clone(),
            })
        } else {
            None
        }
    }

    /// Settle WITHOUT learning: the stripped repair failed, or the request
    /// hit an error unrelated to replay. Any resident entry is left exactly
    /// as it was, so the next request re-verifies rather than inheriting a
    /// conclusion nothing proved. The dispatch path reaches this same
    /// no-learn settlement by dropping an unsettled guard (see [`Drop`]); an
    /// explicit call is exercised only by the tests.
    #[cfg(test)]
    pub fn release(mut self) {
        self.settled = true;
        self.registry.release_slot(&self.key);
    }
}

impl Drop for ReplayProbeGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.registry.release_slot(&self.key);
        }
    }
}

#[cfg(test)]
#[path = "learned_replay_tests.rs"]
mod tests;
