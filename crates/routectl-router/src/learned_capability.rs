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
// Only the test-only hook slots need a Mutex.
#[cfg(test)]
use parking_lot::Mutex;
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
    /// The pinned observation-evidence token this entry was admitted on, when
    /// its verdict carries one (`verified` / `suspect` always do; `broken`
    /// never does).
    ///
    /// Held because the warm rebuild REQUIRES a recognized class for those
    /// verdicts and fails closed without one: an entry re-appended to the
    /// ledger without its class is skipped on the next boot, so dropping it
    /// here would silently evict the verdict rather than merely lose forensic
    /// detail.
    evidence_class: Option<String>,
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
        /// Signal tier of the negative that is acting.
        signal: SignalTier,
        /// Detection phase that attributed it, read directly by the strip site.
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
    /// The pinned observation-evidence token, when the verdict carries one.
    pub evidence_class: Option<String>,
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
    /// The pinned observation-evidence token, when the verdict carries one.
    /// Round-tripped at full fidelity: the warm rebuild fails closed on a
    /// `verified` / `suspect` row without a recognized class, so losing it
    /// across a carry-over would evict the entry at the next boot.
    pub evidence_class: Option<String>,
}

/// In-memory, interior-locked learned-capability store. Mutated through
/// `&self`; held behind an `Arc` on the router.
pub struct LearnedCapabilityRegistry {
    entries: RwLock<HashMap<RegistryKey, LearnedEntry>>,
    /// Hot-reloadable tempo and capacity.
    ///
    /// Behind the same lock as `entries` rather than immutable fields,
    /// because this registry now OUTLIVES the Router generation that built
    /// it: one shared instance spans reloads, so a reload that changes the
    /// `[capability]` knobs has no other way to apply them. Immutable fields
    /// would silently pin the operator's tuning to whenever the daemon last
    /// restarted.
    tuning: RwLock<RegistryTuning>,
    /// The ACTIVE router generation.
    ///
    /// One registry is shared across Router generations, so an operation can
    /// arrive from a Router that has already been replaced. Catalog-scoped
    /// truth belongs to the generation that learned it and must not survive a
    /// revision change; a wire-shape fact is catalog-independent and stays
    /// true regardless. The generation is what tells the two cases apart at
    /// the moment of the call -- see [`LearnedCapabilityRegistry::generation`].
    ///
    /// # Lock order (the ONE order every path uses)
    ///
    /// `generation` -> `pending_generation` -> `entries` -> `tuning`.
    ///
    /// Every generation-validated operation acquires `generation` FIRST and holds
    /// it across the `entries` work, so validation and the operation it guards
    /// are one atomic step and a boundary transition cannot land between them.
    /// `commit_boundary_transition` takes the same locks in the same order, which
    /// is what makes the pairing deadlock-free.
    ///
    /// A path that needs only a subset still takes what it needs in this
    /// sequence -- notably `effective_persistence_generation`, which reads
    /// `generation` before `pending_generation` even though it wants the latter.
    /// Reversing that pair was a live deadlock against
    /// `commit_boundary_transition`, which write-locks both. No path takes any
    /// two of these in reverse.
    generation: RwLock<u64>,
    /// The generation a boundary has ADMITTED but not yet committed, if any.
    ///
    /// Installed under the boundary cut before the guard is released, so from
    /// that instant a catalog-independent operation arriving through the
    /// still-published old Router is stamped with the PENDING generation. Its
    /// ledger event then sorts after the boundary being committed instead of
    /// being dropped as older than it -- which is what makes the observation
    /// survive rather than merely be accepted in memory.
    ///
    /// Rolled back atomically on boundary failure or shutdown abandonment, and
    /// promoted on commit. Held under the same `generation` lock so the pending
    /// value can never be read apart from the active one.
    pending_generation: RwLock<Option<BoundaryReceipt>>,
    /// Monotonic receipt counter. Incremented on every successful admission, so
    /// no two boundaries share a receipt even if they derive the same persistence
    /// generation (the ABA case after a rollback).
    next_receipt_id: RwLock<u64>,
    /// Test-only hook fired between generation validation and the entries
    /// operation, to prove the two are ATOMIC.
    ///
    /// Under the single-acquisition shape a competing boundary cannot make
    /// progress here -- it blocks on the held generation lock -- so a hook that
    /// advances the generation simply waits. A check-then-lock shape would let
    /// it through, which is the window being tested.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pause_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Test-only hook fired from INSIDE the guarded operation, carrying the
    /// generation active at that moment. Lets a test assert the operation ran
    /// under the generation it validated against -- the property a
    /// check-then-lock shape breaks, independently of which locks the inner
    /// operation re-takes.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    probe_hook: Mutex<Option<Box<dyn Fn(u64) + Send + Sync>>>,
    /// Test-only hook fired immediately AFTER each named lock is acquired,
    /// carrying that lock's name.
    ///
    /// Exists so an ordering test can synchronize on the precise acquisition
    /// point instead of sleeping: a sleep-based fixture passes whenever the
    /// timing happens to work out, which is exactly the shape that made the
    /// earlier lock-order tests non-discriminating. A hook lets the test block a
    /// thread between two acquisitions deterministically.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    acquire_hook: Mutex<Option<Box<dyn Fn(&'static str) + Send + Sync>>>,
}

impl std::fmt::Debug for LearnedCapabilityRegistry {
    /// Hand-rolled because the test-only hooks are boxed closures, which cannot
    /// derive `Debug`. Reports the observable state a reader wants (size,
    /// generation, tuning) and never the hooks.
    ///
    /// Every value is SNAPSHOTTED first, in the documented
    /// `generation -> pending_generation -> entries -> tuning` order, and every
    /// guard is dropped before the formatter runs. Reading them inline inside
    /// `debug_struct` took `entries` before `generation` and held both across the
    /// builder -- the reverse of the order every other path uses, and so a cycle
    /// against `commit_boundary_transition`, which takes them in order for
    /// writing. A `Debug` on a shared registry is reachable from any tracing or
    /// panic path, which makes an inverted acquisition there especially easy to
    /// trip and especially hard to attribute.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (generation, pending, entries, tuning) = {
            let generation = self.generation.read();
            self.note_acquired("generation");
            let pending = self.pending_generation.read();
            self.note_acquired("pending_generation");
            let entries = self.entries.read();
            self.note_acquired("entries");
            let tuning = self.tuning.read();
            self.note_acquired("tuning");
            (*generation, *pending, entries.len(), *tuning)
        };
        f.debug_struct("LearnedCapabilityRegistry")
            .field("entries", &entries)
            .field("generation", &generation)
            .field("pending_generation", &pending)
            .field("tuning", &tuning)
            .finish()
    }
}

/// An opaque, non-reused boundary receipt.
///
/// Every admitted boundary gets a receipt that is unique for the lifetime of the
/// registry, so a rolled-back boundary's receipt can never match a later one
/// that happens to derive the same persistence generation. The receipt counter is
/// monotonic and separate from the persistence generation: the generation may
/// reappear after a rollback, but the receipt never does.
///
/// `Copy + Eq` so it can be carried on the `AdmittedBoundary` and compared at
/// settlement cheaply. `#[must_use]` because silently discarding a receipt
/// instead of settling it would leave the pending slot occupied, blocking every
/// later boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a boundary receipt must be settled (committed or rolled back)"]
pub struct BoundaryReceipt {
    /// The persistence generation this boundary establishes.
    generation: u64,
    /// An opaque, monotonic, non-reused identifier.
    id: u64,
}

/// The result of asking for a boundary cut.
///
/// Four outcomes, because the caller acts differently on each and collapsing any
/// two loses information it needs.
#[derive(Debug)]
#[must_use = "a Taken cut must be settled (committed or rolled back); \
              a Rejected/Busy/Exhausted cut must not be silently ignored"]
pub enum BoundaryCut<T> {
    /// The cut was taken. Carries the closure's outcome and the generation this
    /// boundary establishes -- the receipt that must be presented at settlement.
    Taken {
        /// The submit closure's own result.
        outcome: T,
        /// The opaque receipt that must be presented at settlement.
        receipt: BoundaryReceipt,
    },
    /// A boundary is ALREADY admitted and unsettled, so this one is refused.
    ///
    /// Exactly one boundary may be in flight: two would each stamp events with
    /// their own generation while only one can be promoted, so the loser's events
    /// are dropped by the writer as older than the winner's boundary. Refusing is
    /// the only outcome that cannot silently lose a verdict. Carries the
    /// in-flight generation for the diagnostic.
    Busy {
        /// The already-admitted generation this cut yielded to.
        in_flight: u64,
    },
    /// The generation counter cannot advance: it is at `u64::MAX`.
    ///
    /// Refused BEFORE any batch submission, prune, or state change, so an
    /// exhausted counter degrades to "no more boundaries" rather than to a
    /// wrapped generation that would make every later event compare wrongly.
    Exhausted,
    /// The submit closure ran but reported that admission FAILED (the batch was
    /// not queued). No receipt was allocated, no pending state was installed, and
    /// the caller has NO settlement obligation.
    ///
    /// `Taken` exists ONLY when the batch was admitted, because a receipt without
    /// a queued batch would leave the pending slot occupied with nothing to settle
    /// it. Moving the receipt allocation after the admission check is what
    /// prevents that: a refused admission never consumes a receipt ID.
    Rejected {
        /// The submit closure's own result, so the caller can log the refusal
        /// reason without re-deriving it.
        outcome: T,
    },
}

impl BoundaryReceipt {
    /// The persistence generation this boundary establishes.
    ///
    /// The only public surface: the CLI needs it to stamp the tombstone row and
    /// for diagnostic fields. Everything else about the receipt is opaque.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The result of settling an admitted boundary.
///
/// A settlement presents the generation it was admitted at; a value that no
/// longer matches the pending slot belongs to a boundary that has already been
/// settled, so applying it would promote or clear state belonging to a different
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a StaleReceipt must not be silently ignored"]
pub enum BoundarySettlement {
    /// The receipt matched: the generation was promoted and the catalog-scoped
    /// entries pruned.
    Applied {
        /// The generation now active.
        generation: u64,
        /// Catalog-scoped entries evicted by the transition.
        pruned: usize,
    },
    /// The receipt did not match the pending slot. Nothing changed.
    StaleReceipt,
}

/// Hot-reloadable registry tempo and capacity.
#[derive(Debug, Clone, Copy)]
struct RegistryTuning {
    decay: Duration,
    inferred_window: Duration,
    max_entries: usize,
}

/// Whether an operation submitted against a router generation was applied to
/// the shared registry, or refused because that generation is stale.
///
/// `Stale` is not an error: it is the barrier working. The caller must then
/// emit no ledger event and bump no metric, because the operation describes
/// truth from a catalog revision the daemon has already left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationOutcome<T> {
    /// Applied, carrying the inner outcome and the EFFECTIVE persistence
    /// generation the operation ran under.
    Applied {
        /// The operation's own result.
        value: T,
        /// The generation any persistence metadata for this operation must be
        /// stamped with.
        ///
        /// Selected under the SAME guard as the read or mutation, never sampled
        /// before or after it. A separate read cannot be trusted: a boundary
        /// landing between the mutation and the sample would stamp the event
        /// with a generation that does not match the state it describes -- and a
        /// single request legitimately spans a boundary, so each event carries
        /// its own value rather than one request-wide figure.
        generation: u64,
    },
    /// Refused: the submitting generation is stale and the key is
    /// catalog-scoped.
    Stale,
}

impl<T> GenerationOutcome<T> {
    /// The inner value when applied.
    pub fn applied(self) -> Option<T> {
        match self {
            Self::Applied { value, .. } => Some(value),
            Self::Stale => None,
        }
    }

    /// Whether the operation was refused as stale.
    pub const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale)
    }
}

impl LearnedCapabilityRegistry {
    /// Build an empty registry. `decay` is how long a negative acts before
    /// lapsing into a single re-probe; `inferred_window` bounds how long a
    /// pending single-observation inferred signal waits for its
    /// confirming second observation; `max_entries` caps resident entries.
    pub fn new(decay: Duration, inferred_window: Duration, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            tuning: RwLock::new(RegistryTuning {
                decay,
                inferred_window,
                max_entries,
            }),
            // Generations are 1-based so that zero is never a valid live
            // generation: a default-constructed token cannot pass as current.
            generation: RwLock::new(1),
            pending_generation: RwLock::new(None),
            next_receipt_id: RwLock::new(1),
            #[cfg(test)]
            pause_hook: Mutex::new(None),
            #[cfg(test)]
            probe_hook: Mutex::new(None),
            #[cfg(test)]
            acquire_hook: Mutex::new(None),
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
        evidence_class: Option<&str>,
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
                        let (entry, outcome) =
                            self.fresh_entry(tier, phase, source, evidence_class, now);
                        *existing = entry;
                        outcome
                    }
                    SignalTier::Inferred => ObserveOutcome::Pending,
                },
            };
        }
        self.evict_if_full(&mut entries);
        let (entry, outcome) = self.fresh_entry(tier, phase, source, evidence_class, now);
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
        evidence_class: Option<&str>,
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
        entries.insert(key, Self::fresh_positive(source, evidence_class, now));
        PositiveOutcome::Recorded
    }

    /// Dispatch-path query. Returns the routing decision for this target
    /// and feature, admitting exactly one re-probe when the decay window
    /// has lapsed.
    pub(crate) fn acting_negative_for(
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
                evidence_class: entry.evidence_class.clone(),
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
                evidence_class: entry.evidence_class.clone(),
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
                    evidence_class: exported.evidence_class,
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

    /// The ACTIVE router generation.
    ///
    /// One registry instance is shared across Router generations (a reload
    /// attaches the replacement Router to the SAME `Arc` rather than copying
    /// entries into a fresh one), so an in-flight request can submit an
    /// operation from a Router that has since been replaced. This counter is
    /// how such an operation is recognized: a Router carries the generation it
    /// was published at, and the `*_in_generation` entry points compare it
    /// against this value.
    pub fn generation(&self) -> u64 {
        *self.generation.read()
    }

    /// Advance to the next generation and return it.
    ///
    /// Called once per successful boundary, AFTER the tombstone batch is
    /// durable and BEFORE the replacement Router is published, so there is no
    /// window in which the new generation is active but its boundary is not
    /// recorded.
    #[cfg(test)]
    pub(crate) fn advance_generation(&self) -> u64 {
        let mut generation = self.generation.write();
        *generation = generation.saturating_add(1);
        *generation
    }

    /// The current decay window.
    pub fn decay(&self) -> Duration {
        self.tuning.read().decay
    }

    /// The current inferred-corroboration window.
    pub fn inferred_window(&self) -> Duration {
        self.tuning.read().inferred_window
    }

    /// The current resident-entry cap.
    pub fn max_entries(&self) -> usize {
        self.tuning.read().max_entries
    }

    /// Apply hot-reloaded `[capability]` tempo and capacity in place.
    ///
    /// Resident entries keep the `expires_at` they were stamped with; the new
    /// decay governs subsequent observations. Re-stamping live entries would
    /// let a reload extend or truncate verdicts that were already acting,
    /// which is a routing change the operator did not ask for.
    pub fn retune(&self, decay: Duration, inferred_window: Duration, max_entries: usize) {
        *self.tuning.write() = RegistryTuning {
            decay,
            inferred_window,
            max_entries,
        };
    }

    /// Run `op` under the generation guard, refusing a stale catalog-scoped
    /// operation.
    ///
    /// THE single validated-operation shape. The generation READ lock is
    /// acquired first and held across `op`, so validation and the entries work it
    /// guards are one atomic step: a boundary transition (which takes the same
    /// lock for WRITING, in the same order) cannot land between them. A
    /// check-then-lock shape would leave a window where a catalog-scoped write
    /// lands after the generation advanced and the prune ran -- repopulating
    /// exactly what the boundary evicted.
    ///
    /// A catalog-independent key is always admissible: its truth does not depend
    /// on the catalog revision, so an older generation observing one is still
    /// observing a fact. A catalog-scoped key is admissible only from the active
    /// generation, or from the PENDING generation a boundary has installed (see
    /// `install_pending_generation`, which is test-only).
    fn guarded<T>(
        &self,
        generation: u64,
        feature_key: &str,
        op: impl FnOnce() -> T,
    ) -> GenerationOutcome<T> {
        let active = self.generation.read();
        let admitted = !crate::field_capability::capability_key_is_catalog_scoped(feature_key)
            || generation == *active
            // A caller stamped with the admitted-but-uncommitted generation is
            // acting for the boundary that is landing, not against it.
            || self.pending_generation.read().is_some_and(|r| r.generation == generation);
        // Between validation and the operation. Inert under this shape: a
        // competing boundary blocks on the lock still held above.
        #[cfg(test)]
        if let Some(hook) = self.pause_hook.lock().as_ref() {
            hook();
        }
        if !admitted {
            return GenerationOutcome::Stale;
        }
        // Reports the generation active AT operation time; still under the
        // guard, so it equals what was validated.
        #[cfg(test)]
        if let Some(probe) = self.probe_hook.lock().as_ref() {
            probe(*active);
        }
        let out = op();
        // The effective generation, chosen while the guard still holds: pending
        // when a boundary is admitted-but-uncommitted, otherwise active. Read
        // here rather than by the caller so it cannot drift from the state this
        // operation just produced.
        let generation = self
            .pending_generation
            .read()
            .map_or(*active, |r| r.generation);
        drop(active);
        GenerationOutcome::Applied {
            value: out,
            generation,
        }
    }

    /// Install a test hook fired between generation validation and the guarded
    /// operation. Test-only; see [`Self::guarded`].
    #[cfg(test)]
    pub(crate) fn set_generation_pause_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self.pause_hook.lock() = Some(hook);
    }

    /// Force the active generation, for tests that must reach a boundary value
    /// (`u64::MAX`) that no realistic number of reloads would produce.
    #[cfg(test)]
    pub(crate) fn set_generation_for_test(&self, generation: u64) {
        let mut slot = self.generation.write();
        *slot = generation;
    }

    /// Install a test hook fired immediately after each named lock is acquired.
    /// Test-only; see the `acquire_hook` field.
    ///
    /// The callback must not touch this registry: it runs with at least one guard
    /// held, so re-entering would self-deadlock. Signalling a channel or barrier
    /// is the intended use.
    #[cfg(test)]
    pub(crate) fn set_lock_acquire_hook(&self, hook: Box<dyn Fn(&'static str) + Send + Sync>) {
        *self.acquire_hook.lock() = Some(hook);
    }

    /// Fire the acquisition hook for `lock`, if one is installed.
    #[cfg(test)]
    fn note_acquired(&self, lock: &'static str) {
        // Cloned out from under its own lock first: holding the hook slot while
        // the callback runs would deadlock a callback that installs another hook.
        let hook = self.acquire_hook.lock();
        if let Some(hook) = hook.as_ref() {
            hook(lock);
        }
    }

    /// No-op when not testing.
    #[cfg(not(test))]
    #[inline]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    const fn note_acquired(&self, _lock: &'static str) {}

    /// Install a test hook fired from inside the guarded operation with the
    /// generation active at that moment. Test-only; see [`Self::guarded`].
    #[cfg(test)]
    pub(crate) fn set_generation_probe_hook(&self, hook: Box<dyn Fn(u64) + Send + Sync>) {
        *self.probe_hook.lock() = Some(hook);
    }

    /// Record a negative observation on behalf of `generation`.
    ///
    /// Returns [`GenerationOutcome::Stale`] when the submitting generation has
    /// been superseded and the key is catalog-scoped: the caller must then emit
    /// no ledger event and bump no metric, because the observation describes a
    /// catalog revision the daemon has left.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        tier: SignalTier,
        phase: FailurePhase,
        source: EvidenceSource,
        evidence_class: Option<&str>,
        now: Instant,
    ) -> GenerationOutcome<ObserveOutcome> {
        self.guarded(generation, feature_key_raw, || {
            self.observe(
                state_key,
                feature_key_raw,
                provider_kind,
                tier,
                phase,
                source,
                evidence_class,
                now,
            )
        })
    }

    /// Record a positive observation on behalf of `generation`, with the same
    /// staleness rule as [`Self::observe_in_generation`].
    // Mirrors `observe_positive` plus the generation; grouping the arguments
    // would only introduce a type that exists to satisfy a lint.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_positive_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        source: EvidenceSource,
        evidence_class: Option<&str>,
        now: Instant,
    ) -> GenerationOutcome<PositiveOutcome> {
        self.guarded(generation, feature_key_raw, || {
            self.observe_positive(
                state_key,
                feature_key_raw,
                provider_kind,
                source,
                evidence_class,
                now,
            )
        })
    }

    /// The routing decision for `generation`, or `None` when that generation
    /// may not read this key.
    ///
    /// `None` means STALE, not "allow": a caller holding a superseded Router
    /// must fall through to its ordinary no-verdict path rather than route on
    /// catalog truth the reload replaced.
    pub(crate) fn acting_negative_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> Option<(RoutingDecision, u64)> {
        match self.guarded(generation, feature_key_raw, || {
            self.acting_negative_for(state_key, feature_key_raw, provider_kind, now)
        }) {
            GenerationOutcome::Applied { value, generation } => Some((value, generation)),
            GenerationOutcome::Stale => None,
        }
    }

    /// Remove a keyed entry on behalf of `generation` (the probe-settlement
    /// clear), with the same staleness rule.
    ///
    /// A stale settlement is a no-op AND emits nothing: the probe it settles
    /// was issued against a catalog revision the daemon has left, so treating
    /// it as authoritative would clear an entry the live generation still
    /// believes.
    pub fn remove_keyed_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
    ) -> GenerationOutcome<bool> {
        self.guarded(generation, feature_key_raw, || {
            self.remove_keyed(state_key, feature_key_raw, provider_kind)
        })
    }

    /// The generation an event produced NOW must be stamped with to survive.
    ///
    /// The pending generation when a boundary is admitted-but-uncommitted,
    /// otherwise the active one. Every producer of a ledger event asks this rather
    /// than reading `generation()` directly: an event stamped with the active
    /// generation during an admitted boundary would be older than the boundary the
    /// writer is about to commit, and would be dropped.
    ///
    /// Takes `generation` before `pending_generation`, the documented order.
    pub fn effective_persistence_generation(&self) -> u64 {
        let active = self.generation.read();
        let pending = self.pending_generation.read();
        pending.map_or(*active, |r| r.generation)
    }

    /// Install the generation a boundary has admitted.
    ///
    /// Production installs it INSIDE [`Self::with_boundary_cut`], which is the only
    /// window where the install and the survivor snapshot are indivisible. This
    /// entry point exists for tests that stage a pending generation directly;
    /// it takes `generation` before `pending_generation` like every other path.
    #[cfg(test)]
    pub(crate) fn install_pending_generation(&self, pending: u64) {
        let _generation = self.generation.read();
        let mut next = self.next_receipt_id.write();
        let id = *next;
        *next = next.saturating_add(1);
        *self.pending_generation.write() = Some(BoundaryReceipt {
            generation: pending,
            id,
        });
    }

    /// Discard an admitted-but-uncommitted generation: the boundary failed or was
    /// abandoned at shutdown. Events produced after this revert to stamping the
    /// active generation, which is still the newest committed boundary.
    ///
    /// Takes `generation` before `pending_generation`, the documented order, even
    /// though it only writes the latter.
    pub fn rollback_pending_generation(&self, receipt: &BoundaryReceipt) -> BoundarySettlement {
        let _generation = self.generation.read();
        let mut pending = self.pending_generation.write();
        if *pending != Some(*receipt) {
            tracing::warn!(
                expected_receipt = ?receipt,
                pending = ?*pending,
                "capability boundary rollback ignored: the receipt does not match"
            );
            return BoundarySettlement::StaleReceipt;
        }
        *pending = None;
        BoundarySettlement::Applied {
            generation: receipt.generation,
            pruned: 0,
        }
    }

    /// Take the boundary cut: DERIVE the next pending generation, snapshot the
    /// catalog-independent survivors, let `submit` admit them, and install the
    /// pending generation on success -- all under one acquisition of
    /// `generation -> pending_generation -> entries`, in the documented order.
    ///
    /// Returns `(outcome, pending)` so the caller knows which generation was
    /// established without re-reading it.
    ///
    /// # Why the pending generation is derived HERE
    ///
    /// A caller that read `generation()` and handed back `+ 1` computed it outside
    /// this lock, so two concurrent boundaries could derive the SAME pending value
    /// and the second would silently reuse the first's -- and even a single caller
    /// races a `commit_boundary_transition` landing between its read and this cut.
    /// The old signature also rested on an unenforced single-writer assumption
    /// (only the reload coordinator ever calls it), which nothing in the type
    /// system or this module checks. Deriving under the guard removes the
    /// assumption instead of documenting it.
    ///
    /// # Why the snapshot, admission and install are one operation
    ///
    /// The snapshot and the admission must be indivisible: an observation landing
    /// between them would be in NEITHER place -- absent from the snapshot, so
    /// never restated past the new tombstone, and written before the boundary, so
    /// invisible to the next boot. It would disappear while every individual step
    /// still looked correct.
    ///
    /// Installing the pending generation must be inside the same window, or an
    /// observation arriving between admission and installation is stamped with the
    /// pre-boundary generation and the writer drops it as older than the boundary
    /// being committed.
    ///
    /// The retired shape held `entries` in a callback that then called
    /// `install_pending_generation`, acquiring `entries -> pending_generation` --
    /// the reverse of the documented order, and so a LATENT INVERSION HAZARD
    /// against `commit_boundary_transition`, which takes them in order for
    /// writing. It was never demonstrated as a live deadlock, but a future caller
    /// holding both would have closed the cycle.
    ///
    /// `submit` must not block on I/O: it performs the (non-blocking) batch
    /// ADMISSION only, and every lock here is released before the caller awaits
    /// the writer's outcome. Holding these across SQLite would stall every
    /// dispatching request for the length of a transaction.
    ///
    /// `pending` is installed only when `admitted` reports success, so a refused
    /// admission leaves the registry byte-identical.
    pub fn with_boundary_cut<T>(
        &self,
        submit: impl FnOnce(&[LearnedRegistryEntry], u64) -> T,
        admitted: impl FnOnce(&T) -> bool,
    ) -> BoundaryCut<T> {
        // Take the receipt counter's lock in step, between pending and entries,
        // as an inner component of the pending acquisition order.
        // (It is always taken together with pending_generation, never alone.)
        // The documented order, taken once, top to bottom.
        let generation = self.generation.read();
        self.note_acquired("generation");
        let mut pending_slot = self.pending_generation.write();
        self.note_acquired("pending_generation");

        // REFUSE before any snapshot, submission or state change.
        //
        // An already-admitted boundary means two would be in flight at once, each
        // stamping events with its own generation while only one can be promoted
        // -- the loser's events are then dropped by the writer as older than the
        // winner's boundary. The previous shape advanced PAST the in-flight value
        // and allocated another, which is exactly that loss.
        if let Some(in_flight) = *pending_slot {
            tracing::warn!(
                in_flight_receipt = ?in_flight,
                "capability boundary refused: another boundary is admitted and \
                 unsettled"
            );
            return BoundaryCut::Busy {
                in_flight: in_flight.generation,
            };
        }
        // `checked_add`, not saturating: at `u64::MAX` a saturating add would hand
        // back the active generation as the "next" one, and every later comparison
        // would read wrongly. Refused before the batch is built.
        let mut next_receipt_id = self.next_receipt_id.write();
        let Some(pending_gen) = generation.checked_add(1) else {
            tracing::error!(
                active_generation = *generation,
                "capability boundary generation exhausted; refusing the boundary"
            );
            return BoundaryCut::Exhausted;
        };
        // Receipt counter exhaustion: also refuse before any change.
        let Some(receipt_id) = next_receipt_id.checked_add(1) else {
            tracing::error!("capability boundary receipt counter exhausted");
            return BoundaryCut::Exhausted;
        };

        // The entries WRITE lock excludes readers too, which is what makes the
        // cut a true quiescent point rather than merely serializing writers.
        let entries = self.entries.write();
        self.note_acquired("entries");

        let survivors: Vec<LearnedRegistryEntry> = entries
            .iter()
            .filter(|(key, _)| {
                !crate::field_capability::capability_key_is_catalog_scoped(&key.feature_key)
            })
            .map(|(key, entry)| LearnedRegistryEntry {
                state_key: key.state_key.clone(),
                feature_key: key.feature_key.clone(),
                verdict: entry.read_verdict(),
                signal_tier: entry.signal,
                observations: entry.observations,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                expires_at: entry.expires_at,
                evidence_class: entry.evidence_class.clone(),
                phase: entry.phase,
                source: entry.source,
            })
            .collect();

        let outcome = submit(&survivors, pending_gen);
        if !admitted(&outcome) {
            // The batch was NOT queued. No receipt is allocated, no pending state
            // installed, and the caller has no settlement obligation. The receipt
            // counter stays untouched so a retry does not waste IDs.
            return BoundaryCut::Rejected { outcome };
        }
        let receipt = BoundaryReceipt {
            generation: pending_gen,
            id: receipt_id,
        };
        *pending_slot = Some(receipt);
        *next_receipt_id = receipt_id;
        BoundaryCut::Taken { outcome, receipt }
    }

    /// Resident entry count, or `None` when the entries lock is held for writing.
    ///
    /// A non-blocking probe for the boundary-cut exclusion test: called from
    /// inside the cut it must return `None`, which is direct evidence the cut
    /// still holds the write lock. A blocking read there would deadlock and prove
    /// nothing.
    #[cfg(test)]
    pub(crate) fn try_entry_count(&self) -> Option<usize> {
        self.entries.try_read().map(|entries| entries.len())
    }

    /// Advance the generation and evict the catalog-scoped entries as ONE
    /// transition, returning `(new_generation, pruned)`.
    ///
    /// Paired under a single lock acquisition so there is no window in which the
    /// generation has advanced but the stale entries are still readable, nor one
    /// in which they are gone while an old generation is still admitted to
    /// re-learn them.
    pub fn commit_boundary_transition(&self, receipt: &BoundaryReceipt) -> BoundarySettlement {
        let mut generation = self.generation.write();
        self.note_acquired("generation");
        let mut pending = self.pending_generation.write();
        self.note_acquired("pending_generation");

        // Bound to the receipt, BEFORE the entries lock and before any mutation: a
        // mismatched receipt belongs to a boundary already settled, and promoting
        // it would move the generation on behalf of a different boundary and prune
        // entries no committed batch accounted for.
        if *pending != Some(*receipt) {
            tracing::warn!(
                expected_receipt = ?receipt,
                pending = ?*pending,
                active_generation = *generation,
                "capability boundary commit ignored: the receipt does not match"
            );
            return BoundarySettlement::StaleReceipt;
        }
        // The promotion must be a STRICT ADVANCE. A pending value at or below the
        // active generation would move the counter backwards or leave it still,
        // and then events already stamped with the active generation would read as
        // belonging to the new boundary. Only reachable through a hand-installed
        // value, so it is refused rather than allowed to corrupt the counter.
        if receipt.generation <= *generation {
            tracing::error!(
                active_generation = *generation,
                rejected_receipt = ?receipt,
                "pending capability generation did not strictly advance; refusing \
                 the transition"
            );
            *pending = None;
            return BoundarySettlement::StaleReceipt;
        }

        let mut entries = self.entries.write();
        self.note_acquired("entries");
        *pending = None;
        *generation = receipt.generation;
        let before = entries.len();
        entries.retain(|key, _| {
            !crate::field_capability::capability_key_is_catalog_scoped(&key.feature_key)
        });
        BoundarySettlement::Applied {
            generation: *generation,
            pruned: before - entries.len(),
        }
    }

    /// The decay state of an entry for `generation`, or `None` when that
    /// generation may not read the key.
    ///
    /// Validated atomically with the read, so a superseded Router cannot decide
    /// to strip on state belonging to the replacement generation.
    pub fn negative_state_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> Option<(NegativeState, u64)> {
        match self.guarded(generation, feature_key_raw, || {
            self.negative_state(state_key, feature_key_raw, provider_kind, now)
        }) {
            GenerationOutcome::Applied { value, generation } => Some((value, generation)),
            GenerationOutcome::Stale => None,
        }
    }

    /// Lapse an entry into a single re-probe on behalf of `generation`, under
    /// the same atomic guard. A stale catalog-scoped expiry is refused: the
    /// entry belongs to a catalog revision the daemon has left, so resetting its
    /// decay clock would extend a verdict the boundary is discarding.
    pub fn expire_keyed_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> GenerationOutcome<bool> {
        self.guarded(generation, feature_key_raw, || {
            self.expire_keyed(state_key, feature_key_raw, provider_kind, now)
        })
    }

    /// Settle a re-probe on behalf of `generation`, under the same atomic guard.
    ///
    /// A stale catalog-scoped settlement is refused, so the caller emits no
    /// cleared event and bumps no metric: the probe was issued against a catalog
    /// revision the daemon has left, and treating its result as authoritative
    /// would clear or back off an entry the live generation still believes.
    pub fn record_probe_outcome_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        outcome: ProbeOutcome,
        now: Instant,
    ) -> GenerationOutcome<()> {
        self.guarded(generation, feature_key_raw, || {
            self.record_probe_outcome(state_key, feature_key_raw, provider_kind, outcome, now);
        })
    }

    /// True when `feature_key` is verified-working for `generation`, or `None`
    /// when that generation may not read the key.
    pub fn is_verified_working_in_generation(
        &self,
        generation: u64,
        state_key: &str,
        feature_key_raw: &str,
        provider_kind: &str,
        now: Instant,
    ) -> Option<bool> {
        match self.guarded(generation, feature_key_raw, || {
            self.is_verified_working(state_key, feature_key_raw, provider_kind, now)
        }) {
            GenerationOutcome::Applied { value, .. } => Some(value),
            GenerationOutcome::Stale => None,
        }
    }

    /// Drop every catalog-scoped entry, keeping the catalog-independent ones.
    /// Returns how many were removed.
    ///
    /// The eviction half of the boundary: paired with
    /// [`Self::advance_generation`] under one caller-held transition so no
    /// window exists where the generation advanced but the stale entries are
    /// still resident.
    #[cfg(test)]
    pub(crate) fn prune_catalog_scoped(&self) -> usize {
        let mut entries = self.entries.write();
        let before = entries.len();
        entries.retain(|key, _| {
            !crate::field_capability::capability_key_is_catalog_scoped(&key.feature_key)
        });
        before - entries.len()
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
            entry.expires_at = now + self.decay();
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
                entry.expires_at = now + self.decay();
                entry.consecutive_failed_probes = 0;
                ObserveOutcome::Acting
            }
            SignalTier::Inferred => {
                let within_window =
                    now.saturating_duration_since(entry.first_seen) <= self.inferred_window();
                if within_window {
                    entry.observations = 2;
                    entry.last_seen = now;
                    entry.expires_at = now + self.decay();
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
        evidence_class: Option<&str>,
        now: Instant,
    ) -> (LearnedEntry, ObserveOutcome) {
        let (expires_at, outcome) = match tier {
            // Self-identifying acts immediately; stamp the decay window.
            SignalTier::SelfIdentifying => (now + self.decay(), ObserveOutcome::Acting),
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
            evidence_class: evidence_class.map(str::to_string),
        };
        (entry, outcome)
    }

    /// Build a brand-new VerifiedWorking positive: self-identifying
    /// (structural proof acts on a single observation), phase F3 (the
    /// positive-detection phase). `source` attributes the evidence.
    /// `expires_at` is set to `now` but carries no decay meaning --
    /// `is_expired` excludes a positive, so it never lapses into a re-probe.
    fn fresh_positive(
        source: EvidenceSource,
        evidence_class: Option<&str>,
        now: Instant,
    ) -> LearnedEntry {
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
            evidence_class: evidence_class.map(str::to_string),
        }
    }

    /// Evict the entry with the oldest `last_seen` when the map is at cap,
    /// emitting a structured WARN. A safety valve, not a cache policy.
    fn evict_if_full(&self, map: &mut HashMap<RegistryKey, LearnedEntry>) {
        if map.len() < self.max_entries() {
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
                max_entries = self.max_entries(),
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
        let base = self.decay().saturating_mul(multiple);

        let span = (self.decay().as_nanos() / u128::from(JITTER_DIVISOR)) as i128;
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            t0,
        );
        reg.observe(
            "n",
            "cap_b",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
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
                None,
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
            None,
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
            None,
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
            None,
            t0,
        );
        reg.observe(
            "n2",
            "computer_use",
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
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
            evidence_class: None,
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
            evidence_class: None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            t0,
        );

        // Act -- a passive positive must NOT clear the negative.
        let outcome = reg.observe_positive(
            "nick",
            "web_search",
            "openai-compat",
            EvidenceSource::Live,
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            t0,
        );
        reg.observe(
            "nn",
            "computer_use",
            "openai-compat",
            SignalTier::SelfIdentifying,
            FailurePhase::F2,
            EvidenceSource::Live,
            None,
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
                None,
                t0,
            );
            reg.observe(
                "nn",
                "structured_output",
                "openai-compat",
                SignalTier::Inferred,
                FailurePhase::F3,
                EvidenceSource::Live,
                None,
                t0,
            );
            reg.observe(
                "nn",
                "structured_output",
                "openai-compat",
                SignalTier::Inferred,
                FailurePhase::F3,
                EvidenceSource::Live,
                None,
                t0 + WINDOW / 2,
            );
            reg.observe(
                "ns",
                "computer_use",
                "openai-compat",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
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

#[cfg(test)]
#[path = "learned_capability_generation_tests.rs"]
mod generation_tests;
