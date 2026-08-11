//! Learned-capability registry: in-memory, interior-locked store of the
//! per-(target, feature) negatives the router learns from upstream
//! request faults.
//!
//! A target that rejects a capability (either by naming it outright --
//! self-identifying -- or via a corroborated free-text inference) earns a
//! learned negative here; the dispatch path consults the registry to
//! route away from that target for that feature until the negative decays
//! into a single re-probe. The registry owns only the data structure and
//! its state machine: capture points, the ledger, and the dispatch wiring
//! live in the router.
//!
//! # Keys and normalization
//!
//! Entries are keyed by `(state_key, feature_key)`, where `state_key` is
//! the breaker's nickname-or-provider string and `feature_key` is the
//! capability key AFTER [`normalize_capability_key`]. Every mutating and
//! querying entry point runs the raw feature key through normalization
//! with the caller's provider kind, so an inserted negative and a later
//! lookup meet on identical keys regardless of the raw provider token
//! shape.
//!
//! # Concurrency
//!
//! State lives behind a single [`RwLock`]. The dispatch hot path
//! ([`LearnedCapabilityRegistry::acting_negative_for`]) takes a shared
//! read lock for the overwhelmingly common non-expired case and never
//! contends; it upgrades to an exclusive write only to claim the single
//! re-probe slot on the rare lapse, mirroring the circuit breaker's
//! half-open discipline.
//!
//! # Time
//!
//! Every method takes `now: Instant` so tests drive the decay / window /
//! backoff state machine deterministically, matching the per-model
//! runtime gate's now-parameter style.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, Verdict, normalize_capability_key,
};

/// Default resident-entry ceiling. A safety valve, not a cache policy:
/// eviction should never fire at solo-local volume.
pub const DEFAULT_MAX_ENTRIES: usize = 1024;

/// A re-probe's backoff window never exceeds this multiple of the base
/// decay, no matter how many consecutive probes have failed.
const MAX_BACKOFF_MULTIPLE: u32 = 30;

/// Backoff jitter is bounded to `+/- (decay / JITTER_DIVISOR)`.
const JITTER_DIVISOR: u32 = 8;

/// Internal map key. `state_key` is used verbatim; `feature_key` is
/// always the normalized capability key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RegistryKey {
    state_key: String,
    feature_key: String,
}

/// Which side of the capability-truth ledger a resident entry records: a
/// positive confirmed by structural detection, or a learned negative.
///
/// The discriminator is what a phase alone cannot express: an `F3` entry
/// is a positive-detection phase on BOTH sides -- a VerifiedWorking
/// positive AND an inferred suspect-absence negative both carry `F3` --
/// so the read-model verdict is derived from this discriminator plus the
/// phase, mirroring [`Verdict::from_parts`]: `Verified` maps to
/// `VerifiedWorking`, `Negative` to `LearnedBroken(phase)`. Snapshot and
/// export carry it so a reader reconstructs the verdict without a second
/// registry lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryVerdict {
    /// A capability confirmed working by positive detection. Constructed
    /// only by the positive-admission path.
    Verified,
    /// A learned negative, attributed to the entry's [`FailurePhase`].
    Negative,
}

/// A single resident entry -- a VerifiedWorking positive or a learned
/// negative, discriminated by [`EntryVerdict`]. Private storage
/// representation; callers see [`LearnedRegistryEntry`] (snapshot) or
/// [`ExportedEntry`] (carry-over).
#[derive(Debug, Clone)]
struct LearnedEntry {
    /// Which side of the ledger this entry records. A `Verified` positive
    /// never decays, never claims a re-probe slot, and routes nothing;
    /// a `Negative` runs the full decay / re-probe / backoff machinery.
    verdict: EntryVerdict,
    signal: SignalTier,
    observations: u32,
    first_seen: Instant,
    last_seen: Instant,
    expires_at: Instant,
    in_flight: bool,
    consecutive_failed_probes: u32,
    /// The detection phase that attributed this entry. For a negative this
    /// is F1/F2/F3; for a `Verified` positive it is always F3 (the
    /// positive-detection phase). Threaded through every contract surface;
    /// the derived read-model verdict reads it together with `verdict`.
    phase: FailurePhase,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    source: EvidenceSource,
}

impl LearnedEntry {
    /// A self-identifying signal acts on one observation; an inferred
    /// signal needs corroboration (two observations). A `Verified` positive
    /// is always self-identifying, so it acts on its single observation.
    const fn is_acting(&self) -> bool {
        matches!(self.signal, SignalTier::SelfIdentifying) || self.observations >= 2
    }

    /// The decay window has lapsed and the negative is due for a re-probe.
    /// A `Verified` positive never decays within a revision, so it is never
    /// expired -- it can never claim a re-probe slot.
    fn is_expired(&self, now: Instant) -> bool {
        matches!(self.verdict, EntryVerdict::Negative) && now >= self.expires_at
    }

    /// The routing decision for this entry when it `is_acting`, keyed on
    /// (verdict, phase, source):
    ///
    /// - `Verified` -> `Allow` (a positive routes nothing);
    /// - `Negative` F3 + Live -> `Allow` (advisory-only: visible in the
    ///   snapshot for the status surface, but it routes nothing on its own
    ///   -- a probe settles it);
    /// - every other negative (F1/F2 live, and the F3 + Probe authority a
    ///   later probe pass owns) -> `RouteAway`, carrying its phase to the
    ///   strip site so no second registry lookup is needed.
    const fn acting_decision(&self) -> RoutingDecision {
        match self.verdict {
            EntryVerdict::Verified => RoutingDecision::Allow,
            EntryVerdict::Negative => match (self.phase, self.source) {
                (FailurePhase::F3, EvidenceSource::Live) => RoutingDecision::Allow,
                _ => RoutingDecision::RouteAway {
                    signal: self.signal,
                    phase: self.phase,
                },
            },
        }
    }

    /// The derived read-model verdict, mirroring [`Verdict::from_parts`]:
    /// a `Verified` entry is `VerifiedWorking`; a `Negative` is
    /// `LearnedBroken(phase)`.
    const fn read_verdict(&self) -> Verdict {
        match self.verdict {
            EntryVerdict::Verified => Verdict::VerifiedWorking,
            EntryVerdict::Negative => Verdict::LearnedBroken(self.phase),
        }
    }
}

/// Outcome of [`LearnedCapabilityRegistry::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveOutcome {
    /// An inferred first (or window-reset) observation: stored, but not
    /// yet acting -- it awaits a confirming second observation.
    Pending,
    /// The entry is acting after this observation (self-identifying
    /// immediately; inferred on the confirming second observation within
    /// the window).
    Acting,
}

/// Outcome of [`LearnedCapabilityRegistry::observe_positive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositiveOutcome {
    /// The positive was recorded (a fresh VerifiedWorking entry, or a
    /// refresh of a resident one): VerifiedWorking now acts for this key.
    Recorded,
    /// A learned negative owns the key; the passive positive is a no-op.
    /// The negative's decay / re-probe lifecycle owns clearing -- a passive
    /// positive never clears a resident negative.
    SuppressedByNegative,
}

/// Dispatch-path decision for a `(target, feature)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDecision {
    /// No acting learned negative applies; route to this target normally.
    Allow,
    /// An acting learned negative applies; route away from this target,
    /// carrying the detection phase so the strip site reads it directly
    /// without a second registry lookup.
    RouteAway {
        signal: SignalTier,
        phase: FailurePhase,
    },
    /// The negative's decay lapsed and this caller claimed the single
    /// re-probe slot: route to the target and report the result with
    /// [`LearnedCapabilityRegistry::record_probe_outcome`]. Concurrent
    /// callers keep routing away until the probe settles.
    ProbeAdmitted,
}

/// Non-claiming view of the resident negative for a key.
///
/// The read-only counterpart to
/// [`LearnedCapabilityRegistry::acting_negative_for`]: it never claims the
/// re-probe slot, so a caller that runs its own admission discipline (the
/// reasoning-replay lifecycle's single-flight) reads the decay state
/// without mutating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeState {
    /// No acting learned negative applies: nothing resident, a pending
    /// (uncorroborated) observation, or a VerifiedWorking positive.
    Absent,
    /// An acting negative inside its decay window.
    Acting,
    /// An acting negative whose decay window has lapsed: due for exactly
    /// one re-verification.
    Lapsed,
}

/// Result of a re-probe dispatch, reported to settle the in-flight slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe succeeded (2xx): the negative is stale; clear the entry.
    Success,
    /// The probe hit the SAME capability rejection again: refresh with
    /// capped geometric backoff and keep acting.
    SameCapabilityRejection,
    /// The probe hit some OTHER error (network, 5xx): a transient must not
    /// clear a valid negative; release the slot and leave the entry
    /// expired so the next request re-probes.
    OtherError,
}

/// Snapshot row -- the shape consumed by later features (status / doctor)
/// without a retrofit, so the field set is fixed by contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedRegistryEntry {
    /// The routing state key (provider + model) this entry applies to.
    pub state_key: String,
    /// The capability feature key this entry records.
    pub feature_key: String,
    /// The derived read-model verdict: `VerifiedWorking` for a positive,
    /// `LearnedBroken(phase)` for a negative. Derived at read from the
    /// entry's discriminator, consistent with [`Verdict::from_parts`].
    pub verdict: Verdict,
    /// The signal tier of the entry.
    pub signal_tier: SignalTier,
    /// How many observations have accrued.
    pub observations: u32,
    /// When the entry was first observed.
    pub first_seen: Instant,
    /// When the entry was most recently observed.
    pub last_seen: Instant,
    /// When the negative's decay window lapses. For a VerifiedWorking
    /// positive this carries no decay meaning (a positive never decays);
    /// read the `verdict` discriminator, not this field, to tell them apart.
    pub expires_at: Instant,
    /// The detection phase that attributed this entry.
    pub phase: FailurePhase,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    pub source: EvidenceSource,
}

/// Full-fidelity entry for carrying the registry across a hot reload:
/// [`LearnedCapabilityRegistry::export_entries`] then
/// [`LearnedCapabilityRegistry::import_entries`] round-trips identically.
#[derive(Debug, Clone)]
pub struct ExportedEntry {
    pub state_key: String,
    pub feature_key: String,
    /// Which side of the ledger this entry records. Carried at full
    /// fidelity so the import round-trip reconstructs the identical entry.
    pub verdict: EntryVerdict,
    pub signal: SignalTier,
    pub observations: u32,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub expires_at: Instant,
    pub phase: FailurePhase,
    pub source: EvidenceSource,
    // Populated on export for test observation; import intentionally
    // discards it (a probe slot cannot carry across a hot reload).
    #[cfg_attr(not(test), allow(dead_code))]
    pub in_flight: bool,
    pub consecutive_failed_probes: u32,
}

/// In-memory, interior-locked learned-capability store. Mutated through
/// `&self`; held behind an `Arc` on the router.
#[derive(Debug)]
pub struct LearnedCapabilityRegistry {
    entries: RwLock<HashMap<RegistryKey, LearnedEntry>>,
    decay: Duration,
    inferred_window: Duration,
    max_entries: usize,
}

impl LearnedCapabilityRegistry {
    /// Build an empty registry. `decay` is how long a negative acts before
    /// lapsing into a single re-probe; `inferred_window` bounds how long a
    /// pending single-observation inferred signal waits for its
    /// confirming second observation; `max_entries` caps resident entries.
    pub fn new(decay: Duration, inferred_window: Duration, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            decay,
            inferred_window,
            max_entries,
        }
    }

    /// Build the registry sized from the `[capability]` knobs: the decay and
    /// inferred-observation windows from the configured hours, the resident
    /// cap from `DEFAULT_MAX_ENTRIES`. Shared by the router constructor and
    /// the doctor's one-shot read-only ledger rebuild so both size an
    /// otherwise-bare registry identically.
    pub fn from_capability_config(capability: &crate::config::CapabilityConfig) -> Self {
        Self::new(
            Duration::from_hours(capability.decay_hours),
            Duration::from_hours(capability.inferred_window_hours),
            DEFAULT_MAX_ENTRIES,
        )
    }

    /// Record one learn observation for `(state_key, feature_key)`.
    /// Callers are responsible for one observation per request per target
    /// (dedupe upstream); this method treats each call as a distinct
    /// learn event.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        tier: SignalTier,
        phase: FailurePhase,
        source: EvidenceSource,
        now: Instant,
    ) -> ObserveOutcome {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let mut entries = self.entries.write();
        if let Some(existing) = entries.get_mut(&key) {
            return match existing.verdict {
                // A resident negative runs the normal observe path.
                EntryVerdict::Negative => self.observe_existing(existing, tier, now),
                // Recency (settled rule): only a SELF-IDENTIFYING negative
                // supersedes a resident VerifiedWorking positive -- a directly
                // named failure is fresher, stronger evidence than the
                // structural positive, so it replaces and acts at once. An
                // INFERRED negative is sub-threshold evidence weaker than the
                // positive, so it is DROPPED and the verified entry stays
                // resident (the no-passive-clear philosophy: weak signal never
                // overturns strong). A later self-identifying negative still
                // replaces. The dropped inferred observation produces no acting
                // negative, hence `Pending`.
                EntryVerdict::Verified => match tier {
                    SignalTier::SelfIdentifying => {
                        let (entry, outcome) = self.fresh_entry(tier, phase, source, now);
                        *existing = entry;
                        outcome
                    }
                    SignalTier::Inferred => ObserveOutcome::Pending,
                },
            };
        }
        self.evict_if_full(&mut entries);
        let (entry, outcome) = self.fresh_entry(tier, phase, source, now);
        entries.insert(key, entry);
        outcome
    }

    /// Record one positive (VerifiedWorking) observation for
    /// `(state_key, feature_key)`. A structural positive acts on a single
    /// observation (self-identifying proof), never decays within a
    /// revision, never claims a re-probe slot, and never backs off.
    ///
    /// A no-op when any learned negative resides for the key: a passive
    /// positive never clears a negative -- the negative's decay / re-probe
    /// lifecycle owns clearing. A VerifiedWorking entry therefore lands only
    /// on keys with no resident negative.
    ///
    /// Stage-two admission: pure over its arguments plus `now`, consulting
    /// only the resident registry state -- no internal clock.
    pub fn observe_positive(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        source: EvidenceSource,
        now: Instant,
    ) -> PositiveOutcome {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let mut entries = self.entries.write();
        if let Some(existing) = entries.get_mut(&key) {
            return match existing.verdict {
                EntryVerdict::Negative => PositiveOutcome::SuppressedByNegative,
                EntryVerdict::Verified => {
                    existing.observations = existing.observations.saturating_add(1);
                    existing.last_seen = now;
                    PositiveOutcome::Recorded
                }
            };
        }
        self.evict_if_full(&mut entries);
        entries.insert(key, Self::fresh_positive(source, now));
        PositiveOutcome::Recorded
    }

    /// Dispatch-path query. Returns the routing decision for this target
    /// and feature, admitting exactly one re-probe when the decay window
    /// has lapsed.
    pub fn acting_negative_for(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> RoutingDecision {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);

        // Fast path: a shared read lock covers the common non-expired case
        // and never blocks concurrent dispatch. Only a lapsed, unclaimed
        // negative needs the write lock below.
        {
            let entries = self.entries.read();
            match entries.get(&key) {
                None => return RoutingDecision::Allow,
                Some(entry) => {
                    if !entry.is_acting() {
                        return RoutingDecision::Allow;
                    }
                    let decision = entry.acting_decision();
                    // A Verified positive or an advisory F3+Live negative
                    // routes nothing and never claims a re-probe slot.
                    if matches!(decision, RoutingDecision::Allow) {
                        return RoutingDecision::Allow;
                    }
                    if !entry.is_expired(now) || entry.in_flight {
                        return decision;
                    }
                    // Lapsed and unclaimed: fall through to claim the probe.
                }
            }
        }

        // Slow path: claim the single re-probe slot, re-checking under the
        // exclusive lock (the entry may have changed since the read).
        let mut entries = self.entries.write();
        match entries.get_mut(&key) {
            None => RoutingDecision::Allow,
            Some(entry) => {
                if !entry.is_acting() {
                    return RoutingDecision::Allow;
                }
                let decision = entry.acting_decision();
                if matches!(decision, RoutingDecision::Allow) {
                    RoutingDecision::Allow
                } else if !entry.is_expired(now) || entry.in_flight {
                    decision
                } else {
                    entry.in_flight = true;
                    tracing::info!(
                        event = "expire_probe",
                        state_key = %key.state_key,
                        capability_key = %key.feature_key,
                        signal_tier = entry.signal.as_str(),
                        "lapsed learned negative admitted for its single re-probe",
                    );
                    RoutingDecision::ProbeAdmitted
                }
            }
        }
    }

    /// Read the resident negative's decay state WITHOUT claiming the
    /// re-probe slot. For callers that own their own admission discipline;
    /// the ordinary dispatch path uses [`Self::acting_negative_for`], which
    /// both reads and claims.
    pub(crate) fn negative_state(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> NegativeState {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let entries = self.entries.read();
        let Some(entry) = entries.get(&key) else {
            return NegativeState::Absent;
        };
        if !matches!(entry.verdict, EntryVerdict::Negative) || !entry.is_acting() {
            return NegativeState::Absent;
        }
        if entry.is_expired(now) {
            NegativeState::Lapsed
        } else {
            NegativeState::Acting
        }
    }

    /// Whether a resident acting VerifiedWorking positive owns this key.
    /// The filter's prior pass consults it: a verified positive masks a
    /// catalog `prior=false` demotion (precedence: override > learned >
    /// verified-working > catalog prior > unknown). A positive never decays,
    /// so `now` is unused; it is kept for query-surface symmetry with the
    /// rest of the registry.
    pub fn is_verified_working(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        _now: Instant,
    ) -> bool {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        self.entries.read().get(&key).is_some_and(|entry| {
            matches!(entry.verdict, EntryVerdict::Verified) && entry.is_acting()
        })
    }

    /// Settle an in-flight re-probe with its outcome.
    pub fn record_probe_outcome(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        outcome: ProbeOutcome,
        now: Instant,
    ) {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let mut entries = self.entries.write();
        match outcome {
            ProbeOutcome::Success => {
                if let Some(entry) = entries.remove(&key) {
                    tracing::info!(
                        event = "clear",
                        state_key = %key.state_key,
                        capability_key = %key.feature_key,
                        signal_tier = entry.signal.as_str(),
                        "learned-capability negative cleared by successful re-probe",
                    );
                }
            }
            ProbeOutcome::SameCapabilityRejection => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.consecutive_failed_probes =
                        entry.consecutive_failed_probes.saturating_add(1);
                    entry.observations = entry.observations.saturating_add(1);
                    entry.last_seen = now;
                    entry.in_flight = false;
                    let window = self.backoff_window(&key, entry.consecutive_failed_probes);
                    entry.expires_at = now + window;
                }
            }
            ProbeOutcome::OtherError => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.in_flight = false;
                }
            }
        }
    }

    /// Snapshot every resident entry in the fixed contract shape.
    pub fn snapshot(&self) -> Vec<LearnedRegistryEntry> {
        self.entries
            .read()
            .iter()
            .map(|(key, entry)| LearnedRegistryEntry {
                state_key: key.state_key.clone(),
                feature_key: key.feature_key.clone(),
                verdict: entry.read_verdict(),
                signal_tier: entry.signal,
                observations: entry.observations,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                expires_at: entry.expires_at,
                phase: entry.phase,
                source: entry.source,
            })
            .collect()
    }

    /// Export every entry at full fidelity for hot-reload carry-over.
    pub fn export_entries(&self) -> Vec<ExportedEntry> {
        self.entries
            .read()
            .iter()
            .map(|(key, entry)| ExportedEntry {
                state_key: key.state_key.clone(),
                feature_key: key.feature_key.clone(),
                verdict: entry.verdict,
                signal: entry.signal,
                observations: entry.observations,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                expires_at: entry.expires_at,
                phase: entry.phase,
                source: entry.source,
                in_flight: entry.in_flight,
                consecutive_failed_probes: entry.consecutive_failed_probes,
            })
            .collect()
    }

    /// Bulk-load previously exported entries, honoring the cap.
    pub fn import_entries(&self, entries: Vec<ExportedEntry>) {
        let mut map = self.entries.write();
        for exported in entries {
            // The exported feature key is already normalized (every insert
            // path runs `normalize_capability_key`, which is idempotent),
            // so the round-trip preserves the canonical key.
            let key = RegistryKey {
                state_key: exported.state_key,
                feature_key: exported.feature_key,
            };
            if !map.contains_key(&key) {
                self.evict_if_full(&mut map);
            }
            map.insert(
                key,
                LearnedEntry {
                    verdict: exported.verdict,
                    signal: exported.signal,
                    observations: exported.observations,
                    first_seen: exported.first_seen,
                    last_seen: exported.last_seen,
                    expires_at: exported.expires_at,
                    phase: exported.phase,
                    source: exported.source,
                    // A probe settling on the pre-swap router cannot clear a
                    // slot copied onto the new one, so carry across as free.
                    in_flight: false,
                    consecutive_failed_probes: exported.consecutive_failed_probes,
                },
            );
        }
    }

    /// Drop every entry (invalidation on catalog / overlay change).
    #[cfg(test)]
    pub fn clear_all(&self) {
        self.entries.write().clear();
    }

    /// Lapse the entry keyed by `(state_key, feature_key)` into a single
    /// re-probe: set its `expires_at` to `now` so the next dispatch admits
    /// a probe, WITHOUT touching observation count, signal tier, or backoff
    /// history. Also releases any in-flight slot so the lapse takes effect
    /// immediately. A no-op when no such entry is resident. Returns whether
    /// an entry was expired.
    ///
    /// The targeted counterpart to `clear_all`: used on a
    /// hot-reload when the operator override cell governing this key changed,
    /// so the resident verdict is re-verified against live upstream behavior
    /// rather than either trusted blindly or dropped along with every
    /// unrelated negative.
    pub fn expire_keyed(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> bool {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let mut entries = self.entries.write();
        match entries.get_mut(&key) {
            Some(entry) => {
                entry.expires_at = now;
                entry.in_flight = false;
                true
            }
            None => false,
        }
    }

    /// Whether the registry holds no entries.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Remove the resident entry keyed by `(state_key, feature_key)`
    /// outright, returning whether one was present. The keyed counterpart
    /// to the `record_probe_outcome(Success)` clear: a warm rebuild replays
    /// a persisted `cleared` settlement event through here so a
    /// probe-settled negative does not resurrect across a restart. Unlike
    /// `expire_keyed`, this drops the entry entirely rather than lapsing it
    /// into a single re-probe -- the settlement already proved the target
    /// works, so there is nothing to re-verify.
    pub fn remove_keyed(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
    ) -> bool {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        self.entries.write().remove(&key).is_some()
    }

    /// Build the map key, normalizing the raw capability key so an insert
    /// and a later lookup meet on identical strings.
    fn make_key(state_key: &str, feature_key_raw: &str, provider_kind: &str) -> RegistryKey {
        RegistryKey {
            state_key: state_key.to_string(),
            feature_key: normalize_capability_key(feature_key_raw, provider_kind),
        }
    }

    /// Apply a fresh observation to an entry that already exists.
    fn observe_existing(
        &self,
        entry: &mut LearnedEntry,
        tier: SignalTier,
        now: Instant,
    ) -> ObserveOutcome {
        // An already-acting entry (self-identifying, or a confirmed
        // inferred) is simply reconfirmed: refresh the negative and keep
        // acting.
        if entry.is_acting() {
            entry.observations = entry.observations.saturating_add(1);
            entry.last_seen = now;
            entry.expires_at = now + self.decay;
            if matches!(tier, SignalTier::SelfIdentifying) {
                entry.signal = SignalTier::SelfIdentifying;
                entry.consecutive_failed_probes = 0;
            }
            return ObserveOutcome::Acting;
        }

        // Otherwise the entry is a pending inferred signal awaiting
        // corroboration.
        match tier {
            SignalTier::SelfIdentifying => {
                // A self-identifying signal supersedes the pending inference
                // and acts at once.
                entry.signal = SignalTier::SelfIdentifying;
                entry.observations = entry.observations.saturating_add(1);
                entry.last_seen = now;
                entry.expires_at = now + self.decay;
                entry.consecutive_failed_probes = 0;
                ObserveOutcome::Acting
            }
            SignalTier::Inferred => {
                let within_window =
                    now.saturating_duration_since(entry.first_seen) <= self.inferred_window;
                if within_window {
                    entry.observations = 2;
                    entry.last_seen = now;
                    entry.expires_at = now + self.decay;
                    ObserveOutcome::Acting
                } else {
                    // The confirming observation arrived too late: reset to a
                    // fresh pending observation.
                    entry.observations = 1;
                    entry.first_seen = now;
                    entry.last_seen = now;
                    entry.expires_at = now;
                    ObserveOutcome::Pending
                }
            }
        }
    }

    /// Build a brand-new entry for a first observation. `source` attributes
    /// the evidence (a real in-flight request or a routectl-issued probe);
    /// `phase` is the caller's attribution.
    fn fresh_entry(
        &self,
        tier: SignalTier,
        phase: FailurePhase,
        source: EvidenceSource,
        now: Instant,
    ) -> (LearnedEntry, ObserveOutcome) {
        let (expires_at, outcome) = match tier {
            // Self-identifying acts immediately; stamp the decay window.
            SignalTier::SelfIdentifying => (now + self.decay, ObserveOutcome::Acting),
            // Inferred starts pending; no decay window until it is confirmed.
            SignalTier::Inferred => (now, ObserveOutcome::Pending),
        };
        let entry = LearnedEntry {
            verdict: EntryVerdict::Negative,
            signal: tier,
            observations: 1,
            first_seen: now,
            last_seen: now,
            expires_at,
            in_flight: false,
            consecutive_failed_probes: 0,
            phase,
            source,
        };
        (entry, outcome)
    }

    /// Build a brand-new VerifiedWorking positive: self-identifying
    /// (structural proof acts on a single observation), phase F3 (the
    /// positive-detection phase). `source` attributes the evidence.
    /// `expires_at` is set to `now` but carries no decay meaning --
    /// `is_expired` excludes a positive, so it never lapses into a re-probe.
    const fn fresh_positive(source: EvidenceSource, now: Instant) -> LearnedEntry {
        LearnedEntry {
            verdict: EntryVerdict::Verified,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: now,
            last_seen: now,
            expires_at: now,
            in_flight: false,
            consecutive_failed_probes: 0,
            phase: FailurePhase::F3,
            source,
        }
    }

    /// Evict the entry with the oldest `last_seen` when the map is at cap,
    /// emitting a structured WARN. A safety valve, not a cache policy.
    fn evict_if_full(&self, map: &mut HashMap<RegistryKey, LearnedEntry>) {
        if map.len() < self.max_entries {
            return;
        }
        let victim = map
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(key, _)| key.clone());
        if let Some(key) = victim {
            tracing::warn!(
                event = "evict",
                state_key = %key.state_key,
                capability_key = %key.feature_key,
                max_entries = self.max_entries,
                "learned-capability registry at capacity; evicted oldest entry",
            );
            map.remove(&key);
        }
    }

    /// Capped geometric backoff window for the next re-probe: base decay
    /// doubled per consecutive failure, ceilinged at `MAX_BACKOFF_MULTIPLE`
    /// times decay, with deterministic per-key jitter bounded to
    /// `+/- decay / JITTER_DIVISOR`.
    fn backoff_window(&self, key: &RegistryKey, consecutive_failed_probes: u32) -> Duration {
        let multiple = 2u64
            .saturating_pow(consecutive_failed_probes)
            .min(u64::from(MAX_BACKOFF_MULTIPLE)) as u32;
        let base = self.decay.saturating_mul(multiple);

        let span = (self.decay.as_nanos() / u128::from(JITTER_DIVISOR)) as i128;
        let jitter = if span == 0 {
            0
        } else {
            jitter_offset(key, consecutive_failed_probes, span)
        };
        let total = (base.as_nanos() as i128 + jitter).max(0) as u128;
        Duration::from_nanos(total.min(u128::from(u64::MAX)) as u64)
    }
}

/// Deterministic jitter in `[-span, span]` derived from the entry key and
/// consecutive-failure count. Deterministic (no RNG dependency) yet keyed,
/// so distinct entries spread their re-probes apart rather than stampeding.
fn jitter_offset(key: &RegistryKey, consecutive_failed_probes: u32, span: i128) -> i128 {
    let mut hasher = DefaultHasher::new();
    key.state_key.hash(&mut hasher);
    key.feature_key.hash(&mut hasher);
    consecutive_failed_probes.hash(&mut hasher);
    let hash = hasher.finish();

    let modulus = (2 * span + 1) as u128;
    let magnitude = (u128::from(hash) % modulus) as i128;
    magnitude - span
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECAY: Duration = Duration::from_hours(48);
    const WINDOW: Duration = Duration::from_hours(1);

    fn registry() -> LearnedCapabilityRegistry {
        LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES)
    }

    #[test]
    fn self_identifying_entry_acts_on_first_observation() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Assert
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
    }

    #[test]
    fn inferred_first_observation_is_pending_not_acting() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Assert
        assert_eq!(outcome, ObserveOutcome::Pending);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "anthropic-api", t0),
            RoutingDecision::Allow
        );
    }

    #[test]
    fn inferred_second_observation_within_window_becomes_acting() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let within = t0 + WINDOW / 2;

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            within,
        );

        // Assert
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "anthropic-api", within),
            RoutingDecision::RouteAway {
                signal: SignalTier::Inferred,
                phase: FailurePhase::F1,
            }
        );
    }

    #[test]
    fn inferred_second_observation_after_window_resets_to_pending() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let after = t0 + WINDOW + Duration::from_secs(1);

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            after,
        );

        // Assert -- reset to a fresh pending observation, not acting.
        assert_eq!(outcome, ObserveOutcome::Pending);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "anthropic-api", after),
            RoutingDecision::Allow
        );
        let snap = reg.snapshot();
        assert_eq!(snap[0].observations, 1);
        assert_eq!(snap[0].first_seen, after);
    }

    #[test]
    fn expired_negative_admits_exactly_one_probe() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);

        // Act -- first caller claims the single probe slot.
        let first = reg.acting_negative_for("nick", "web_search", "openai-compat", expired);
        // Concurrent caller sees the claimed slot.
        let second = reg.acting_negative_for("nick", "web_search", "openai-compat", expired);

        // Assert
        assert_eq!(first, RoutingDecision::ProbeAdmitted);
        assert_eq!(
            second,
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
    }

    #[test]
    fn probe_success_clears_the_entry() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", expired),
            RoutingDecision::ProbeAdmitted
        );

        // Act
        reg.record_probe_outcome(
            "nick",
            "web_search",
            "openai-compat",
            ProbeOutcome::Success,
            expired,
        );

        // Assert
        assert!(reg.is_empty());
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", expired),
            RoutingDecision::Allow
        );
    }

    #[test]
    fn probe_same_capability_rejection_backs_off_and_keeps_acting() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);
        reg.acting_negative_for("nick", "web_search", "openai-compat", expired);

        // Act
        reg.record_probe_outcome(
            "nick",
            "web_search",
            "openai-compat",
            ProbeOutcome::SameCapabilityRejection,
            expired,
        );

        // Assert -- slot released, entry re-acts (fresh non-expired window),
        // observation count bumped.
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", expired),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
        assert_eq!(reg.snapshot()[0].observations, 2);
    }

    #[test]
    fn probe_other_error_releases_slot_and_leaves_entry_expired() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);
        reg.acting_negative_for("nick", "web_search", "openai-compat", expired);

        // Act -- a transient failure must not clear the valid negative.
        reg.record_probe_outcome(
            "nick",
            "web_search",
            "openai-compat",
            ProbeOutcome::OtherError,
            expired,
        );

        // Assert -- still expired, slot free: the next request re-probes.
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", expired),
            RoutingDecision::ProbeAdmitted
        );
    }

    #[test]
    fn backoff_grows_geometrically_and_caps_at_ceiling() {
        // Arrange
        let decay = Duration::from_hours(1);
        let reg =
            LearnedCapabilityRegistry::new(decay, Duration::from_mins(10), DEFAULT_MAX_ENTRIES);
        let t0 = Instant::now();
        reg.observe(
            "n",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let now = t0 + decay + Duration::from_secs(1);

        // Act -- hammer consecutive rejections well past the cap threshold.
        for _ in 0..8 {
            reg.record_probe_outcome(
                "n",
                "web_search",
                "openai-compat",
                ProbeOutcome::SameCapabilityRejection,
                now,
            );
        }

        // Assert -- window pinned to the ceiling multiple, within jitter.
        let window = reg.snapshot()[0].expires_at.duration_since(now);
        let base = decay * MAX_BACKOFF_MULTIPLE;
        let span = decay / JITTER_DIVISOR;
        assert!(
            window >= base.saturating_sub(span),
            "window {window:?} below floor"
        );
        assert!(window <= base + span, "window {window:?} above ceiling");
    }

    #[test]
    fn backoff_jitter_stays_within_bound() {
        // Arrange -- 8h decay makes the jitter span exactly 1h.
        let decay = Duration::from_hours(8);
        let reg =
            LearnedCapabilityRegistry::new(decay, Duration::from_mins(10), DEFAULT_MAX_ENTRIES);
        let t0 = Instant::now();
        reg.observe(
            "n",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let now = t0 + decay + Duration::from_secs(1);

        // Act -- a single rejection: base window is 2x decay.
        reg.record_probe_outcome(
            "n",
            "web_search",
            "openai-compat",
            ProbeOutcome::SameCapabilityRejection,
            now,
        );

        // Assert
        let window = reg.snapshot()[0].expires_at.duration_since(now);
        let base = decay * 2;
        let span = decay / JITTER_DIVISOR;
        assert!(
            window >= base.saturating_sub(span),
            "window {window:?} below floor"
        );
        assert!(window <= base + span, "window {window:?} above ceiling");
    }

    #[test]
    fn cap_eviction_removes_oldest_last_seen_and_warns() {
        // Arrange -- cap of 2, three distinct keys with rising last_seen.
        let reg =
            LearnedCapabilityRegistry::new(Duration::from_hours(1), Duration::from_mins(10), 2);
        let t0 = Instant::now();
        reg.observe(
            "n",
            "cap_a",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        reg.observe(
            "n",
            "cap_b",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0 + Duration::from_secs(1),
        );

        // Act -- the third insert forces eviction of the oldest (cap_a).
        let events = routectl_testkit::capture_events(|| {
            reg.observe(
                "n",
                "cap_c",
                "openai-compat",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                t0 + Duration::from_secs(2),
            );
        });

        // Assert
        let keys: Vec<String> = reg.snapshot().into_iter().map(|e| e.feature_key).collect();
        assert!(
            !keys.contains(&"cap_a".to_string()),
            "oldest entry must be evicted"
        );
        assert!(keys.contains(&"cap_b".to_string()));
        assert!(keys.contains(&"cap_c".to_string()));
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("eviction must emit a WARN");
        assert_eq!(warn.field("event"), Some("evict"));
        assert_eq!(warn.field("capability_key"), Some("cap_a"));
        assert_eq!(warn.field("state_key"), Some("n"));
        assert_eq!(warn.field("max_entries"), Some("2"));
    }

    #[test]
    fn snapshot_exposes_contract_fields() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Act
        let snap = reg.snapshot();

        // Assert
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.state_key, "nick");
        assert_eq!(entry.feature_key, "web_search");
        assert_eq!(entry.signal_tier, SignalTier::SelfIdentifying);
        assert_eq!(entry.observations, 1);
        assert_eq!(entry.first_seen, t0);
        assert_eq!(entry.last_seen, t0);
        assert_eq!(entry.expires_at, t0 + DECAY);
        // The observe path mints an F1/Live negative.
        assert_eq!(entry.phase, FailurePhase::F1);
        assert_eq!(entry.source, EvidenceSource::Live);
    }

    #[test]
    fn snapshot_feature_key_is_normalized_for_bedrock() {
        // Arrange -- a raw Bedrock field path is normalized on insert.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "additionalModelRequestFields.anthropic_beta",
            "bedrock",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Act
        let snap = reg.snapshot();

        // Assert -- stored under the normalized key; both raw and
        // normalized lookups meet the insert.
        assert_eq!(snap[0].feature_key, "anthropic_beta");
        assert_eq!(
            reg.acting_negative_for("nick", "anthropic_beta", "bedrock", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
        assert_eq!(
            reg.acting_negative_for(
                "nick",
                "additionalModelRequestFields.anthropic_beta",
                "bedrock",
                t0
            ),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
    }

    #[test]
    fn export_import_round_trips_all_entries() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "n1",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        reg.observe(
            "n2",
            "computer_use",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        // A non-default (phase, source) pair proves both survive the
        // round-trip, not just the F1/Live the observe path mints.
        reg.import_entries(vec![ExportedEntry {
            state_key: "n3".into(),
            feature_key: "prefill".into(),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::Inferred,
            observations: 2,
            first_seen: t0,
            last_seen: t0,
            expires_at: t0 + Duration::from_hours(1),
            phase: FailurePhase::F2,
            source: EvidenceSource::Probe,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);

        // Act
        let exported = reg.export_entries();
        let reg2 = registry();
        reg2.import_entries(exported);

        // Assert -- snapshots match once sorted for a stable comparison
        // (the derived `PartialEq` covers phase + source too).
        let mut a = reg.snapshot();
        let mut b = reg2.snapshot();
        a.sort_by(|x, y| x.feature_key.cmp(&y.feature_key));
        b.sort_by(|x, y| x.feature_key.cmp(&y.feature_key));
        assert_eq!(a, b);

        // The non-default attribution rode across intact.
        let n3 = b
            .iter()
            .find(|e| e.state_key == "n3")
            .expect("the imported entry survives the round-trip");
        assert_eq!(n3.phase, FailurePhase::F2);
        assert_eq!(n3.source, EvidenceSource::Probe);
    }

    #[test]
    fn imported_f2_negative_routes_away_carrying_its_phase() {
        // Arrange -- an acting F2 negative loaded via hot-reload carry-over.
        let reg = registry();
        let t0 = Instant::now();
        reg.import_entries(vec![ExportedEntry {
            state_key: "nick".into(),
            feature_key: "web_search".into(),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: t0,
            last_seen: t0,
            expires_at: t0 + DECAY,
            phase: FailurePhase::F2,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);

        // Act / Assert -- the route-away decision surfaces the F2 phase so
        // the strip site reads it directly, no second registry lookup.
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F2,
            }
        );
    }

    #[test]
    fn clear_all_empties_the_registry() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "n",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        assert!(!reg.is_empty());

        // Act
        reg.clear_all();

        // Assert
        assert!(reg.is_empty());
    }

    #[test]
    fn unknown_key_allows_routing() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();

        // Act / Assert -- nothing learned about this target/feature.
        assert_eq!(
            reg.acting_negative_for("absent", "web_search", "openai-compat", t0),
            RoutingDecision::Allow
        );
    }

    #[test]
    fn expire_keyed_lapses_entry_into_reprobe_without_touching_history() {
        // Arrange -- an acting self-identifying negative well inside decay.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );

        // Act -- keyed-expire at t0.
        let expired = reg.expire_keyed("nick", "web_search", "openai-compat", t0);

        // Assert -- the entry lapsed into a single re-probe, its observation
        // history left intact (only the decay clock reset).
        assert!(expired);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::ProbeAdmitted
        );
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].observations, 1);
        assert_eq!(snap[0].first_seen, t0);
    }

    #[test]
    fn expire_keyed_absent_key_is_a_noop() {
        // Arrange
        let reg = registry();

        // Act / Assert -- nothing to expire for an unknown key.
        assert!(!reg.expire_keyed("nick", "web_search", "openai-compat", Instant::now()));
    }

    #[test]
    fn probe_admission_emits_expire_probe_event() {
        // Arrange -- an acting self-identifying negative past its decay window.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);

        // Act -- the admission of the single re-probe emits the event.
        let events = routectl_testkit::capture_events(|| {
            assert_eq!(
                reg.acting_negative_for("nick", "web_search", "openai-compat", expired),
                RoutingDecision::ProbeAdmitted
            );
        });

        // Assert
        let ev = events
            .iter()
            .find(|e| e.field("event") == Some("expire_probe"))
            .expect("probe admission must emit an expire_probe event");
        assert_eq!(ev.field("state_key"), Some("nick"));
        assert_eq!(ev.field("capability_key"), Some("web_search"));
        assert_eq!(ev.field("signal_tier"), Some("self-identifying"));
    }

    #[test]
    fn successful_reprobe_emits_clear_event() {
        // Arrange -- an acting negative admitted for its single re-probe.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        let expired = t0 + DECAY + Duration::from_secs(1);
        reg.acting_negative_for("nick", "web_search", "openai-compat", expired);

        // Act -- the probe succeeds; the entry is cleared with an event.
        let events = routectl_testkit::capture_events(|| {
            reg.record_probe_outcome(
                "nick",
                "web_search",
                "openai-compat",
                ProbeOutcome::Success,
                expired,
            );
        });

        // Assert -- fields captured from the removed entry.
        assert!(reg.is_empty());
        let ev = events
            .iter()
            .find(|e| e.field("event") == Some("clear"))
            .expect("successful re-probe must emit a clear event");
        assert_eq!(ev.field("state_key"), Some("nick"));
        assert_eq!(ev.field("capability_key"), Some("web_search"));
        assert_eq!(ev.field("signal_tier"), Some("self-identifying"));
    }

    // --- VerifiedWorking coexistence (verdict discriminator) ---

    #[test]
    fn observe_positive_acts_on_first_observation_and_routes_allow() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();

        // Act -- a single structural positive.
        let outcome = reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );

        // Assert -- recorded, acting, but routes NOTHING (a positive never
        // routes away).
        assert_eq!(outcome, PositiveOutcome::Recorded);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::Allow
        );
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].verdict, Verdict::VerifiedWorking);
        assert_eq!(snap[0].signal_tier, SignalTier::SelfIdentifying);
        assert_eq!(snap[0].observations, 1);
    }

    #[test]
    fn verified_positive_never_decays_or_claims_a_probe() {
        // Arrange -- a positive far past any plausible decay window.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );
        let long_after = t0 + DECAY * 100;

        // Act / Assert -- still Allow, never ProbeAdmitted: a positive is
        // excluded from is_expired and can never claim a re-probe slot.
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", long_after),
            RoutingDecision::Allow
        );
    }

    #[test]
    fn passive_positive_no_ops_on_resident_negative() {
        // Arrange -- an acting self-identifying negative resides.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Act -- a passive positive must NOT clear the negative.
        let outcome = reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );

        // Assert -- suppressed; the negative still routes away.
        assert_eq!(outcome, PositiveOutcome::SuppressedByNegative);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
        assert_eq!(
            reg.snapshot()[0].verdict,
            Verdict::LearnedBroken(FailurePhase::F1)
        );
    }

    #[test]
    fn fresh_self_identifying_negative_replaces_resident_verified() {
        // Arrange -- a resident VerifiedWorking positive.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );
        assert_eq!(reg.snapshot()[0].verdict, Verdict::VerifiedWorking);

        // Act -- a fresh self-identifying negative supersedes it.
        let outcome = reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );

        // Assert -- the positive is replaced by an acting negative.
        assert_eq!(outcome, ObserveOutcome::Acting);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].verdict, Verdict::LearnedBroken(FailurePhase::F1));
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
            }
        );
    }

    #[test]
    fn inferred_negative_does_not_replace_resident_verified() {
        // Arrange -- a resident VerifiedWorking positive.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );

        // Act -- a single INFERRED negative is sub-threshold evidence, weaker
        // than the structural positive: it must be dropped, leaving the
        // verified entry resident.
        let outcome = reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::Inferred,
            FailurePhase::F3,
            EvidenceSource::Live,
            t0,
        );

        // Assert -- no acting negative; the verified positive survives intact
        // and routing stays Allow.
        assert_eq!(outcome, ObserveOutcome::Pending);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].verdict, Verdict::VerifiedWorking);
        assert_eq!(snap[0].observations, 1);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::Allow
        );

        // A later SELF-IDENTIFYING negative still replaces the survivor.
        let outcome = reg.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.snapshot()[0].verdict,
            Verdict::LearnedBroken(FailurePhase::F1)
        );
    }

    #[test]
    fn f3_live_acting_negative_is_advisory_and_routes_allow() {
        // Arrange -- an F3 suspect-absence negative via the inferred window
        // reaching N=2 (the existing corroboration path, no new threshold).
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "structured_output",
            "openai-compat",
            SignalTier::Inferred,
            FailurePhase::F3,
            EvidenceSource::Live,
            t0,
        );
        let confirm = t0 + WINDOW / 2;
        let outcome = reg.observe(
            "nick",
            "structured_output",
            "openai-compat",
            SignalTier::Inferred,
            FailurePhase::F3,
            EvidenceSource::Live,
            confirm,
        );

        // Assert -- acting (N=2), but F3+Live routes NOTHING (advisory-only);
        // it stays visible in the snapshot for the status surface.
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "structured_output", "openai-compat", confirm),
            RoutingDecision::Allow
        );
        let snap = reg.snapshot();
        assert_eq!(snap[0].verdict, Verdict::LearnedBroken(FailurePhase::F3));
        assert_eq!(snap[0].phase, FailurePhase::F3);
        assert_eq!(snap[0].source, EvidenceSource::Live);
    }

    #[test]
    fn f3_probe_acting_negative_routes_away() {
        // Arrange -- the same F3 suspect-absence admission as the live case
        // (inferred window reaching N=2), differing ONLY in evidence source.
        // A probe-sourced F3 negative carries routing authority, so it routes
        // away where the live-sourced twin stays advisory.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe(
            "nick",
            "structured_output",
            "openai-compat",
            SignalTier::Inferred,
            FailurePhase::F3,
            EvidenceSource::Probe,
            t0,
        );
        let confirm = t0 + WINDOW / 2;
        let outcome = reg.observe(
            "nick",
            "structured_output",
            "openai-compat",
            SignalTier::Inferred,
            FailurePhase::F3,
            EvidenceSource::Probe,
            confirm,
        );

        // Assert -- acting (N=2), and F3+Probe routes away (not advisory).
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "structured_output", "openai-compat", confirm),
            RoutingDecision::RouteAway {
                signal: SignalTier::Inferred,
                phase: FailurePhase::F3,
            }
        );
        let snap = reg.snapshot();
        assert_eq!(snap[0].source, EvidenceSource::Probe);
    }

    #[test]
    fn is_verified_working_reflects_resident_verdict() {
        // Arrange
        let reg = registry();
        let t0 = Instant::now();

        // Absent key: not verified.
        assert!(!reg.is_verified_working("nick", "web_search", "openai-compat", t0));

        // A positive: verified.
        reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );
        assert!(reg.is_verified_working("nick", "web_search", "openai-compat", t0));

        // A negative on a different key: not verified.
        reg.observe(
            "nick",
            "computer_use",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            t0,
        );
        assert!(!reg.is_verified_working("nick", "computer_use", "openai-compat", t0));
    }

    #[test]
    fn export_import_round_trips_the_verified_discriminator() {
        // Arrange -- a positive and a negative coexisting on distinct keys.
        let reg = registry();
        let t0 = Instant::now();
        reg.observe_positive(
            "np",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            t0,
        );
        reg.observe(
            "nn",
            "computer_use",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F2,
            EvidenceSource::Live,
            t0,
        );

        // Act -- round-trip through export / import.
        let reg2 = registry();
        reg2.import_entries(reg.export_entries());

        // Assert -- the discriminator survives on both sides.
        let verdict_of = |snap: &[LearnedRegistryEntry], sk: &str| {
            snap.iter().find(|e| e.state_key == sk).unwrap().verdict
        };
        let snap = reg2.snapshot();
        assert_eq!(verdict_of(&snap, "np"), Verdict::VerifiedWorking);
        assert_eq!(
            verdict_of(&snap, "nn"),
            Verdict::LearnedBroken(FailurePhase::F2)
        );
    }

    #[test]
    fn admission_is_deterministic_over_same_observations_and_now() {
        // Stage-two admission purity: the same observation sequence replayed with the
        // same `now` timestamps yields an identical registry state. Admission
        // consults only its arguments plus `now` -- no internal clock -- so a
        // shared `t0` drives both replays to a byte-identical snapshot.
        let t0 = Instant::now();
        let apply = |t0: Instant| {
            let reg = registry();
            reg.observe_positive(
                "np",
                "web_search",
                "openai-compat",
                EvidenceSource::Live,
                t0,
            );
            reg.observe(
                "nn",
                "structured_output",
                "openai-compat",
                SignalTier::Inferred,
                FailurePhase::F3,
                EvidenceSource::Live,
                t0,
            );
            reg.observe(
                "nn",
                "structured_output",
                "openai-compat",
                SignalTier::Inferred,
                FailurePhase::F3,
                EvidenceSource::Live,
                t0 + WINDOW / 2,
            );
            reg.observe(
                "ns",
                "computer_use",
                "openai-compat",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                t0,
            );
            let mut snap = reg.snapshot();
            snap.sort_by(|a, b| {
                (a.state_key.clone(), a.feature_key.clone())
                    .cmp(&(b.state_key.clone(), b.feature_key.clone()))
            });
            snap
        };

        // Identical timestamps -> identical entry state, field-for-field
        // (the derived `PartialEq` covers verdict, tier, phase, source, and
        // the monotonic instants alike).
        assert_eq!(apply(t0), apply(t0));
    }
}
