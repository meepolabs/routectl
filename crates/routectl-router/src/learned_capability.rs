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
use routectl_core::capability::{SignalTier, normalize_capability_key};

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

/// A single learned negative. Private storage representation; callers see
/// [`LearnedRegistryEntry`] (snapshot) or [`ExportedEntry`] (carry-over).
#[derive(Debug, Clone)]
struct LearnedEntry {
    signal: SignalTier,
    observations: u32,
    first_seen: Instant,
    last_seen: Instant,
    expires_at: Instant,
    in_flight: bool,
    consecutive_failed_probes: u32,
}

impl LearnedEntry {
    /// A self-identifying signal acts on one observation; an inferred
    /// signal needs corroboration (two observations).
    const fn is_acting(&self) -> bool {
        matches!(self.signal, SignalTier::SelfIdentifying) || self.observations >= 2
    }

    /// The decay window has lapsed and the negative is due for a re-probe.
    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
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

/// Dispatch-path decision for a `(target, feature)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDecision {
    /// No acting learned negative applies; route to this target normally.
    Allow,
    /// An acting learned negative applies; route away from this target.
    RouteAway(SignalTier),
    /// The negative's decay lapsed and this caller claimed the single
    /// re-probe slot: route to the target and report the result with
    /// [`LearnedCapabilityRegistry::record_probe_outcome`]. Concurrent
    /// callers keep routing away until the probe settles.
    ProbeAdmitted,
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
    pub state_key: String,
    pub feature_key: String,
    pub signal_tier: SignalTier,
    pub observations: u32,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub expires_at: Instant,
}

/// Full-fidelity entry for carrying the registry across a hot reload:
/// [`LearnedCapabilityRegistry::export_entries`] then
/// [`LearnedCapabilityRegistry::import_entries`] round-trips identically.
#[derive(Debug, Clone)]
pub struct ExportedEntry {
    pub state_key: String,
    pub feature_key: String,
    pub signal: SignalTier,
    pub observations: u32,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub expires_at: Instant,
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

    /// Record one learn observation for `(state_key, feature_key)`.
    /// Callers are responsible for one observation per request per target
    /// (dedupe upstream); this method treats each call as a distinct
    /// learn event.
    pub fn observe(
        &self,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        tier: SignalTier,
        now: Instant,
    ) -> ObserveOutcome {
        let key = Self::make_key(state_key, feature_key_raw, provider_kind);
        let mut entries = self.entries.write();
        if let Some(existing) = entries.get_mut(&key) {
            return self.observe_existing(existing, tier, now);
        }
        self.evict_if_full(&mut entries);
        let (entry, outcome) = self.fresh_entry(tier, now);
        entries.insert(key, entry);
        outcome
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
                    if !entry.is_expired(now) || entry.in_flight {
                        return RoutingDecision::RouteAway(entry.signal);
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
                    RoutingDecision::Allow
                } else if !entry.is_expired(now) || entry.in_flight {
                    RoutingDecision::RouteAway(entry.signal)
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
                signal_tier: entry.signal,
                observations: entry.observations,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                expires_at: entry.expires_at,
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
                signal: entry.signal,
                observations: entry.observations,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                expires_at: entry.expires_at,
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
                    signal: exported.signal,
                    observations: exported.observations,
                    first_seen: exported.first_seen,
                    last_seen: exported.last_seen,
                    expires_at: exported.expires_at,
                    // A probe settling on the pre-swap router cannot clear a
                    // slot copied onto the new one, so carry across as free.
                    in_flight: false,
                    consecutive_failed_probes: exported.consecutive_failed_probes,
                },
            );
        }
    }

    /// Drop every entry (invalidation on catalog / overlay change).
    #[allow(dead_code)]
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
    /// The targeted counterpart to [`clear_all`](Self::clear_all): used on a
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

    /// Build a brand-new entry for a first observation.
    fn fresh_entry(&self, tier: SignalTier, now: Instant) -> (LearnedEntry, ObserveOutcome) {
        let (expires_at, outcome) = match tier {
            // Self-identifying acts immediately; stamp the decay window.
            SignalTier::SelfIdentifying => (now + self.decay, ObserveOutcome::Acting),
            // Inferred starts pending; no decay window until it is confirmed.
            SignalTier::Inferred => (now, ObserveOutcome::Pending),
        };
        let entry = LearnedEntry {
            signal: tier,
            observations: 1,
            first_seen: now,
            last_seen: now,
            expires_at,
            in_flight: false,
            consecutive_failed_probes: 0,
        };
        (entry, outcome)
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
            t0,
        );

        // Assert
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
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
            t0,
        );
        let within = t0 + WINDOW / 2;

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
            within,
        );

        // Assert
        assert_eq!(outcome, ObserveOutcome::Acting);
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "anthropic-api", within),
            RoutingDecision::RouteAway(SignalTier::Inferred)
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
            t0,
        );
        let after = t0 + WINDOW + Duration::from_secs(1);

        // Act
        let outcome = reg.observe(
            "nick",
            "web_search",
            "anthropic-api",
            SignalTier::Inferred,
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
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
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
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
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
            t0,
        );
        reg.observe(
            "n",
            "cap_b",
            "openai-compat",
            SignalTier::SelfIdentifying,
            t0 + Duration::from_secs(1),
        );

        // Act -- the third insert forces eviction of the oldest (cap_a).
        let events = routectl_testkit::capture_events(|| {
            reg.observe(
                "n",
                "cap_c",
                "openai-compat",
                SignalTier::SelfIdentifying,
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
            t0,
        );

        // Act
        let snap = reg.snapshot();

        // Assert -- stored under the normalized key; both raw and
        // normalized lookups meet the insert.
        assert_eq!(snap[0].feature_key, "anthropic_beta");
        assert_eq!(
            reg.acting_negative_for("nick", "anthropic_beta", "bedrock", t0),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
        );
        assert_eq!(
            reg.acting_negative_for(
                "nick",
                "additionalModelRequestFields.anthropic_beta",
                "bedrock",
                t0
            ),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
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
            t0,
        );
        reg.observe(
            "n2",
            "computer_use",
            "anthropic-api",
            SignalTier::Inferred,
            t0,
        );

        // Act
        let exported = reg.export_entries();
        let reg2 = registry();
        reg2.import_entries(exported);

        // Assert -- snapshots match once sorted for a stable comparison.
        let mut a = reg.snapshot();
        let mut b = reg2.snapshot();
        a.sort_by(|x, y| x.feature_key.cmp(&y.feature_key));
        b.sort_by(|x, y| x.feature_key.cmp(&y.feature_key));
        assert_eq!(a, b);
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
            t0,
        );
        assert_eq!(
            reg.acting_negative_for("nick", "web_search", "openai-compat", t0),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying)
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
}
