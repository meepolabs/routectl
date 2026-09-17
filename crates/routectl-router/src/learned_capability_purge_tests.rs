//! Tests for the per-key PURGE RESERVATION: the single-flight lease that keeps
//! an operator purge's in-memory removal consistent with its durable clear.
//!
//! The reservation exists because the two halves of a purge cannot be made
//! atomic: the removal is a memory mutation under a lock, and the clear is a
//! SQLite transaction that must not be awaited under that lock. Without a lease
//! the window between them is observable -- a concurrent observation could
//! refresh the entry that is about to be removed, or a failed commit could leave
//! memory cleared while the ledger still holds the negative, so the next boot
//! would resurrect a verdict the operator was told was gone.
//!
//! The lease closes that window by making the key's state immovable for its
//! duration: prepare captures the exact resident entry, the commit happens with
//! no lock held, and finalize or restore then acts on state nothing else could
//! have touched.

use super::*;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use std::sync::Arc;

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES)
}

/// Plant one acting self-identifying negative.
fn plant(reg: &LearnedCapabilityRegistry, state_key: &str, capability: &str) {
    reg.observe(
        state_key,
        capability,
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
}

/// Observe once and report the outcome, for the incarnation assertions.
fn observe_reporting(
    reg: &LearnedCapabilityRegistry,
    state_key: &str,
    capability: &str,
    now: Instant,
) -> AppliedMutation<ObserveOutcome> {
    reg.observe_in_generation(
        reg.generation(),
        state_key,
        capability,
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    )
    .applied()
    .expect("the live generation admits the observation")
}

/// Every PRODUCTION mutation path that can produce a persisted event, as data.
///
/// Enumerated so the lease contract is asserted per call site. Each variant is a
/// real entry point the dispatch or probe path calls -- a list of names would
/// prove nothing, so each one runs the actual method.
#[derive(Debug, Clone, Copy)]
enum MutationPath {
    /// A learned negative from the dispatch error arm.
    ObserveNegative,
    /// A response-evidence positive.
    ObservePositive,
    /// A probe settlement (the re-probe's outcome).
    SettleProbe,
    /// The keyed removal a replay/field clear and a probe-settled clear use.
    RemoveKeyed,
    /// The lapse/backoff bookkeeping a re-probe admission performs.
    ExpireKeyed,
}

impl MutationPath {
    const ALL: [Self; 5] = [
        Self::ObserveNegative,
        Self::ObservePositive,
        Self::SettleProbe,
        Self::RemoveKeyed,
        Self::ExpireKeyed,
    ];

    /// Run this path against `reg` and report whether the lease refused it.
    fn run(
        self,
        reg: &LearnedCapabilityRegistry,
        state_key: &str,
        capability: &str,
        now: Instant,
    ) -> PathOutcome {
        let generation = reg.generation();
        match self {
            Self::ObserveNegative => PathOutcome::from(reg.observe_in_generation(
                generation,
                state_key,
                capability,
                "",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                now,
            )),
            Self::ObservePositive => PathOutcome::from(reg.observe_positive_in_generation(
                generation,
                state_key,
                capability,
                "",
                EvidenceSource::Live,
                Some("schema_mismatch"),
                now,
            )),
            Self::SettleProbe => PathOutcome::from(reg.record_probe_outcome_in_generation(
                generation,
                state_key,
                capability,
                "",
                ProbeOutcome::SameCapabilityRejection,
                now,
            )),
            Self::RemoveKeyed => PathOutcome::from(
                reg.remove_keyed_in_generation(generation, state_key, capability, ""),
            ),
            Self::ExpireKeyed => PathOutcome::from(
                reg.expire_keyed_in_generation(generation, state_key, capability, "", now),
            ),
        }
    }
}

/// Whether a path was refused by the lease, flattened across the paths' own
/// return types so one assertion covers all of them.
enum PathOutcome {
    Reserved,
    Other,
}

impl PathOutcome {
    const fn is_reserved(&self) -> bool {
        matches!(self, Self::Reserved)
    }
}

impl<T> From<GenerationOutcome<T>> for PathOutcome {
    fn from(outcome: GenerationOutcome<T>) -> Self {
        match outcome {
            GenerationOutcome::Reserved => Self::Reserved,
            _ => Self::Other,
        }
    }
}

/// Whether an entry is resident for the key.
fn resident(reg: &LearnedCapabilityRegistry, state_key: &str, capability: &str) -> bool {
    reg.snapshot()
        .iter()
        .any(|e| e.state_key == state_key && e.feature_key == capability)
}

// --- prepare captures, and the lease is single-flight ---------------------

/// Preparing a purge captures the EXACT resident entry and the effective
/// generation, and leaves the entry in place until the caller finalizes.
///
/// Capturing rather than removing is what makes a failed commit recoverable: the
/// restore path needs the original entry byte for byte, including its
/// observation count and decay clock, so a rolled-back purge is indistinguishable
/// from one that never happened.
#[test]
fn preparing_a_purge_captures_the_entry_and_leaves_it_resident() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let before = reg.snapshot();
    let planted = before
        .iter()
        .find(|e| e.feature_key == "web_search")
        .expect("premise: the entry must be resident");

    // Act
    let prepared = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(reserved) => reserved,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Assert: captured, and still resident (nothing is removed until finalize).
    assert_eq!(prepared.generation(), reg.generation());
    assert!(
        prepared.captured_entry().is_some(),
        "the entry was captured"
    );
    assert!(
        resident(&reg, "nick", "web_search"),
        "prepare must not remove the entry -- the removal is only durable once \
         the clear commits"
    );
    let captured = prepared.captured_entry().expect("captured");
    assert_eq!(captured.observations, planted.observations);
    assert_eq!(captured.signal_tier, planted.signal_tier);
}

/// A second purge for the SAME key is refused while the first lease is open.
///
/// Two purges in flight would each capture the entry and each try to settle it;
/// whichever finalized second would act on a capture the first had already
/// invalidated, and a failure on either could restore an entry the other
/// legitimately removed.
#[test]
fn a_second_purge_for_the_same_key_is_refused_while_a_lease_is_open() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let _first = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(reserved) => reserved,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    let second = reg.prepare_purge(reg.generation(), "nick", "web_search", "");

    // Assert
    assert!(
        matches!(second, PurgePreparation::Busy),
        "a same-key purge must be refused while a lease is open, got {second:?}"
    );
}

/// A DIFFERENT key is unaffected: the lease is per-key, not a registry-wide
/// stop-the-world.
#[test]
fn a_purge_on_another_key_proceeds_alongside() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    plant(&reg, "nick", "thinking");
    let _first = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(reserved) => reserved,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    let other = reg.prepare_purge(reg.generation(), "nick", "thinking", "");

    // Assert
    assert!(
        matches!(other, PurgePreparation::Reserved(_)),
        "a purge on a different key must not be blocked, got {other:?}"
    );
}

/// Preparing a purge for an ABSENT key reserves nothing and says so, so the
/// caller answers a clean no-op without a commit or a lease to settle.
#[test]
fn preparing_a_purge_for_an_absent_key_reports_absent() {
    // Arrange: a different key resident, so "absent" is about the request.
    let reg = registry();
    plant(&reg, "nick", "web_search");

    // Act
    let prepared = reg.prepare_purge(reg.generation(), "nick", "thinking", "");

    // Assert
    assert!(
        matches!(prepared, PurgePreparation::Absent),
        "an absent key must report Absent -- no capture, no lease, no commit, \
         got {prepared:?}"
    );
    assert!(resident(&reg, "nick", "web_search"));
}

/// A STALE generation is refused distinguishably, and refusing is not the same
/// answer as absent.
///
/// A purge arriving through a superseded Router would mutate a registry the
/// published Router no longer reads, so reporting it as a clean removal would
/// tell the operator something happened that did not. Reporting it as ABSENT
/// would be just as wrong -- it claims the entry is gone.
#[test]
fn a_stale_generation_purge_is_refused_distinguishably() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let stale = reg.generation();
    reg.advance_generation();

    // Act
    let prepared = reg.prepare_purge(stale, "nick", "web_search", "");

    // Assert
    assert!(
        matches!(prepared, PurgePreparation::Stale),
        "a stale-generation purge must be refused as Stale, never as Absent or \
         as a clean removal, got {prepared:?}"
    );
    assert!(
        resident(&reg, "nick", "web_search"),
        "a refused purge must change nothing"
    );
}

// --- the lease blocks same-key mutation while it is open ------------------

/// A same-key OBSERVATION cannot change the entry while a lease is open.
///
/// This is what the lease is for. Without it, an observation landing between the
/// capture and the commit would refresh the entry's decay clock, and the
/// finalize would then remove state the operator never saw -- or a restore would
/// put back a capture that is older than what the registry now held.
#[test]
fn a_same_key_observation_is_refused_while_a_lease_is_open() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let before = reg.snapshot();
    let planted = before
        .iter()
        .find(|e| e.feature_key == "web_search")
        .expect("resident")
        .clone();
    let reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(r) => r,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act: a fresh observation on the reserved key.
    let outcome = reg.observe_in_generation(
        reg.generation(),
        "nick",
        "web_search",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    // Assert: refused, and the entry is byte-identical to the capture.
    assert!(
        outcome.is_reserved(),
        "an observation on a reserved key must be refused, got {outcome:?}"
    );
    let after = reg.snapshot();
    let now = after
        .iter()
        .find(|e| e.feature_key == "web_search")
        .expect("still resident");
    assert_eq!(
        now.observations, planted.observations,
        "the reserved entry must not have been refreshed"
    );
    assert_eq!(now.expires_at, planted.expires_at, "decay clock untouched");
    drop(reserved);
}

/// After the lease is RELEASED, a legitimate observation relearns normally --
/// the lease is a brief mutual exclusion, not a permanent block on the key.
#[test]
fn an_observation_after_release_relearns_normally() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(r) => r,
        other => panic!("expected a reservation, got {other:?}"),
    };
    // The purge fails, so the lease releases with the entry restored.
    reg.restore_purge(reserved);

    // Act
    let outcome = reg.observe_in_generation(
        reg.generation(),
        "nick",
        "web_search",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    // Assert
    assert!(
        outcome.applied().is_some(),
        "an observation after release must be applied, got {outcome:?}"
    );
}

// --- settle: finalize removes, restore puts back exactly -----------------

/// Finalizing removes the entry. This is the only path that makes the removal
/// visible, and it runs only after the durable clear has committed.
#[test]
fn finalizing_a_purge_removes_the_entry_and_releases_the_lease() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(r) => r,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    let removed = reg.finalize_purge(reserved);

    // Assert
    assert!(removed, "finalize reports the removal it performed");
    assert!(
        !resident(&reg, "nick", "web_search"),
        "the entry is gone only after finalize"
    );
    // The lease released, so the key is purgeable again (now absent).
    assert!(matches!(
        reg.prepare_purge(reg.generation(), "nick", "web_search", ""),
        PurgePreparation::Absent
    ));
}

/// Restoring leaves the entry EXACTLY as it was -- same observation count, same
/// tier, same decay clock -- so a failed purge is indistinguishable from one
/// that never ran.
///
/// Anything less would make a failed purge destructive in its own right: an
/// entry restored with a reset decay clock would act for a fresh window, and one
/// restored with a lower observation count could fall below its acting threshold.
#[test]
fn restoring_a_purge_leaves_the_entry_byte_identical() {
    // Arrange: two observations, so the count is not the default.
    let reg = registry();
    plant(&reg, "nick", "web_search");
    plant(&reg, "nick", "web_search");
    let before = reg
        .snapshot()
        .into_iter()
        .find(|e| e.feature_key == "web_search")
        .expect("resident");

    let reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(r) => r,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    reg.restore_purge(reserved);

    // Assert
    let after = reg
        .snapshot()
        .into_iter()
        .find(|e| e.feature_key == "web_search")
        .expect("the entry must be restored");
    assert_eq!(after.observations, before.observations);
    assert_eq!(after.signal_tier, before.signal_tier);
    assert_eq!(after.expires_at, before.expires_at);
    assert_eq!(after.first_seen, before.first_seen);
    assert_eq!(after.last_seen, before.last_seen);
    assert_eq!(after.verdict.as_str(), before.verdict.as_str());
}

/// Restore is also correct for the case where nothing was captured: an absent
/// key never produces a reservation, so there is nothing to put back.
#[test]
fn a_restored_purge_releases_the_lease_for_the_next_caller() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    let reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
        PurgePreparation::Reserved(r) => r,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    reg.restore_purge(reserved);

    // Assert: a fresh purge can now reserve the same key.
    assert!(matches!(
        reg.prepare_purge(reg.generation(), "nick", "web_search", ""),
        PurgePreparation::Reserved(_)
    ));
}

/// A lease dropped WITHOUT settling restores the entry rather than losing it.
///
/// This is the shutdown-abandonment path: a handler that is cancelled mid-commit
/// never reaches finalize or restore. The safe default is to keep the negative
/// acting -- the ledger still holds it, so dropping the entry from memory would
/// disagree with the ledger until the next boot.
#[test]
fn a_dropped_lease_restores_the_entry() {
    // Arrange
    let reg = registry();
    plant(&reg, "nick", "web_search");
    {
        let _reserved = match reg.prepare_purge(reg.generation(), "nick", "web_search", "") {
            PurgePreparation::Reserved(r) => r,
            other => panic!("expected a reservation, got {other:?}"),
        };
        // Dropped here without finalize or restore.
    }

    // Assert
    assert!(
        resident(&reg, "nick", "web_search"),
        "an abandoned lease must leave the entry acting: the ledger still holds \
         the negative, so removing it from memory alone would disagree"
    );
    assert!(
        matches!(
            reg.prepare_purge(reg.generation(), "nick", "web_search", ""),
            PurgePreparation::Reserved(_)
        ),
        "and the lease must have been released"
    );
}

// --- concurrency: the lease is single-flight under real threads ----------

/// Under concurrent threads, exactly ONE purge reserves a given key and the
/// rest are refused -- the property a check-then-lock shape breaks.
#[test]
fn exactly_one_concurrent_purge_reserves_a_key() {
    // Arrange
    let reg = Arc::new(registry());
    plant(&reg, "nick", "web_search");
    let generation = reg.generation();

    // Act: many threads race for the same key.
    let reserved_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let busy_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let mut handles = Vec::new();
    for _ in 0..16 {
        let reg = Arc::clone(&reg);
        let reserved_count = Arc::clone(&reserved_count);
        let busy_count = Arc::clone(&busy_count);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            match reg.prepare_purge(generation, "nick", "web_search", "") {
                PurgePreparation::Reserved(reserved) => {
                    reserved_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Hold the lease briefly so the losers genuinely contend.
                    std::thread::sleep(Duration::from_millis(20));
                    reg.restore_purge(reserved);
                }
                PurgePreparation::Busy => {
                    busy_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }));
    }
    for handle in handles {
        handle.join().expect("thread");
    }

    // Assert: every thread got a definite answer, and the winners serialized.
    let reserved = reserved_count.load(std::sync::atomic::Ordering::Relaxed);
    let busy = busy_count.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        reserved + busy,
        16,
        "every thread must get a definite answer"
    );
    assert!(reserved >= 1, "at least one thread must win the lease");
    assert!(
        busy >= 1,
        "with 16 threads contending on one key and the winner holding the lease, \
         at least one must be refused -- otherwise the lease is not exclusive"
    );
    // And the entry survived the whole race (every winner restored).
    assert!(resident(&reg, "nick", "web_search"));
}

// ---------------------------------------------------------------------------
// Incarnations: a checked monotonic stamp on every persistence-producing
// mutation, and the identity a purge finalizes against
// ---------------------------------------------------------------------------

/// Every persistence-producing operation reports the incarnation it produced.
///
/// The incarnation is what lets a LATER consumer tell one version of a key's
/// truth from another. A generation cannot do it: a reload advances the
/// generation for every key at once, while a purge-and-relearn of ONE key
/// happens entirely inside one generation -- so an event queued before a purge
/// and an event produced by a genuine relearn after it are indistinguishable by
/// generation alone.
#[test]
fn a_negative_observation_reports_a_monotonic_incarnation() {
    let reg = registry();
    let now = Instant::now();

    let first = reg
        .observe_in_generation(
            reg.generation(),
            "t",
            "web_search",
            "",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        )
        .applied()
        .expect("the live generation admits the observation");
    let second = reg
        .observe_in_generation(
            reg.generation(),
            "t",
            "web_search",
            "",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        )
        .applied()
        .expect("a second observation is admitted too");

    assert!(
        second.incarnation > first.incarnation,
        "each mutation of a key must allocate a STRICTLY greater incarnation \
         ({} then {}), or two versions of the key's truth cannot be ordered",
        first.incarnation,
        second.incarnation,
    );
}

/// The incarnation advances per MUTATION, not per key, so two keys cannot
/// collide on a value and a consumer comparing them is comparing the same
/// sequence.
#[test]
fn distinct_keys_draw_from_one_monotonic_sequence() {
    let reg = registry();
    let now = Instant::now();

    let first = observe_reporting(&reg, "t", "web_search", now);
    let other = observe_reporting(&reg, "t", "computer_use", now);

    assert_ne!(
        first.incarnation, other.incarnation,
        "two mutations must never share an incarnation, whatever keys they touch",
    );
}

/// A purge finalizes ONLY against the exact entry it captured.
///
/// The reservation carries the captured incarnation, and finalize compares it to
/// what is resident. A mismatch means newer state arrived under the lease, and
/// deleting it would destroy a truth the operator never saw -- so the mismatch
/// refuses rather than removes.
#[test]
fn a_finalize_whose_entry_changed_under_the_lease_refuses_to_delete() {
    let reg = registry();
    plant(&reg, "t", "web_search");
    let lease = match reg.prepare_purge(reg.generation(), "t", "web_search", "") {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("a resident entry must reserve; got {other:?}"),
    };

    // Newer state lands under the lease. Nothing in production can do this --
    // the lease refuses every mutation path -- so it is forced directly, which
    // is the only way to reach the mismatch branch at all.
    reg.bump_incarnation_for_tests("t", "web_search", "");

    let removed = reg.finalize_purge(lease);

    assert!(
        !removed,
        "a finalize whose captured incarnation no longer matches must NOT delete: the resident \
         entry is newer than what the operator saw and approved removing",
    );
    assert!(
        resident(&reg, "t", "web_search"),
        "and the newer entry must survive",
    );
}

/// The matching case, so the refusal above is about the MISMATCH rather than
/// about a finalize that never removes anything.
#[test]
fn a_finalize_whose_entry_is_unchanged_removes_it() {
    let reg = registry();
    plant(&reg, "t", "web_search");
    let lease = match reg.prepare_purge(reg.generation(), "t", "web_search", "") {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("a resident entry must reserve; got {other:?}"),
    };

    assert!(
        reg.finalize_purge(lease),
        "an unchanged entry is removed, so the mismatch refusal is not vacuous",
    );
    assert!(!resident(&reg, "t", "web_search"));
}

/// A leased key is excluded from capacity eviction.
///
/// Eviction under a lease would delete the captured entry behind the purge's
/// back: the finalize would then find nothing, and a caller reading `removed`
/// could not tell a successful purge from an entry evicted for capacity.
#[test]
fn a_leased_entry_is_not_evicted_for_capacity() {
    // A registry with room for exactly one entry, so the next insert must evict.
    let reg = LearnedCapabilityRegistry::new(DECAY, WINDOW, 1);
    plant(&reg, "t", "web_search");
    let lease = match reg.prepare_purge(reg.generation(), "t", "web_search", "") {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("a resident entry must reserve; got {other:?}"),
    };

    // Another key arrives and needs the slot.
    plant(&reg, "other", "computer_use");

    assert!(
        resident(&reg, "t", "web_search"),
        "a leased entry must survive capacity pressure: evicting it would delete the state a \
         purge captured, and the finalize could no longer tell a removal from an eviction",
    );
    reg.restore_purge(lease);
}

// ---------------------------------------------------------------------------
// Lease validation lives INSIDE the atomic primitive
// ---------------------------------------------------------------------------

/// Every production mutation path yields to an open lease, and none of them
/// produces a value a caller could turn into a ledger event.
///
/// Driven per PATH rather than once, because each is its own call site: a fix
/// applied to one leaves the others live, and a single-path test would report the
/// class as closed while the rest still refreshed a captured entry.
#[test]
fn every_persistence_producing_path_is_refused_under_a_lease() {
    let now = Instant::now();
    for path in MutationPath::ALL {
        let reg = registry();
        plant(&reg, "t", "web_search");
        let lease = match reg.prepare_purge(reg.generation(), "t", "web_search", "") {
            PurgePreparation::Reserved(lease) => lease,
            other => panic!("{path:?}: a resident entry must reserve; got {other:?}"),
        };
        let before = reg.snapshot();

        let outcome = path.run(&reg, "t", "web_search", now);

        assert!(
            outcome.is_reserved(),
            "{path:?}: a mutation on a leased key must report Reserved -- applying it would \
             refresh the entry the purge captured, so the operator would be told a purge \
             succeeded on state that had since been relearned",
        );
        assert_eq!(
            reg.snapshot().len(),
            before.len(),
            "{path:?}: and must leave the registry unchanged",
        );
        reg.restore_purge(lease);

        // The control: the SAME path applies once the lease is gone, so the
        // refusal above is about the lease and not about a dead call.
        assert!(
            !path.run(&reg, "t", "web_search", now).is_reserved(),
            "{path:?}: after release the path must apply again, or a released lease would \
             permanently freeze the key",
        );
    }
}

// ---------------------------------------------------------------------------
// Purge / reload mutual exclusion
// ---------------------------------------------------------------------------

/// A reload boundary yields to an OPEN PURGE LEASE.
///
/// Mutual exclusion in this direction because the boundary's own work would
/// otherwise race the lease: it snapshots the catalog-independent survivors and
/// restates them past the new tombstone, and a leased entry is one an operator is
/// in the middle of removing. Restating it would re-append the very verdict the
/// purge is clearing -- so the durable clear would commit and the boundary would
/// then put the entry back, past the boundary, where the next boot reads it.
#[test]
fn a_boundary_cut_is_refused_while_a_purge_lease_is_open() {
    let reg = registry();
    plant(&reg, "t", "web_search");
    let lease = match reg.prepare_purge(reg.generation(), "t", "web_search", "") {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("a resident entry must reserve; got {other:?}"),
    };

    // A cut whose submit closure must never run: refusing has to happen BEFORE
    // the snapshot, or a survivor has already been captured for restatement.
    let mut submitted = false;
    let cut = reg.with_boundary_cut(
        |_survivors, _gen| {
            submitted = true;
            true
        },
        |admitted| *admitted,
    );

    assert!(
        matches!(cut, BoundaryCut::Busy { .. }),
        "a boundary must refuse as Busy while any purge lease is open",
    );
    assert!(
        !submitted,
        "and must refuse BEFORE snapshotting or submitting: a captured survivor is already a \
         restatement of state the purge is removing",
    );
    reg.restore_purge(lease);

    // The control: once the lease releases, the same cut is taken. Without it, a
    // boundary that always refused would satisfy the assertion above.
    let mut submitted_after = false;
    let after = reg.with_boundary_cut(
        |_survivors, _gen| {
            submitted_after = true;
            true
        },
        |admitted| *admitted,
    );
    assert!(
        matches!(after, BoundaryCut::Taken { .. }),
        "after the lease releases the boundary proceeds, so the refusal is about the lease",
    );
    assert!(submitted_after, "and its submit closure runs");
}

/// A purge yields to an ADMITTED-BUT-UNSETTLED boundary.
///
/// The other direction, and it is not symmetric reasoning: a boundary in flight
/// has already submitted its tombstone batch, so a purge admitted now would
/// commit its clear into a generation whose promotion is still undecided. If the
/// boundary then fails and rolls back, the clear is stamped with a generation the
/// writer will reject -- the purge would report success on a row nothing reads.
#[test]
fn a_purge_is_refused_as_busy_while_a_boundary_is_admitted() {
    let reg = registry();
    // A CATALOG-INDEPENDENT key (`field:`), because the control below commits the
    // boundary: a catalog-scoped entry is evicted by that commit, so the retry
    // would find nothing and report absent for a reason unrelated to the lease.
    // The exclusion under test is about the in-flight boundary, not about which
    // keys survive one.
    // MINTED through the namespace owner, not spelled out: the prefix is
    // permanent and has exactly one compiled spelling, which a lexer-backed scan
    // enforces across every source file -- a literal here would be the second
    // spelling that scan exists to forbid.
    let key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");
    let key = key.as_str();
    plant(&reg, "t", key);

    // Admit a boundary and leave it unsettled.
    let cut = reg.with_boundary_cut(|_survivors, _gen| true, |admitted| *admitted);
    let receipt = match cut {
        BoundaryCut::Taken { receipt, .. } => receipt,
        other => panic!("the cut must be taken; got {other:?}"),
    };

    let prepared = reg.prepare_purge(reg.generation(), "t", key, "");

    assert!(
        matches!(prepared, PurgePreparation::Busy),
        "a purge must be refused as BUSY while a boundary is admitted and unsettled -- and \
         busy rather than absent, because telling an operator the entry is gone is the answer \
         a refusal must never give; got {prepared:?}",
    );
    assert!(
        resident(&reg, "t", key),
        "and the entry stays exactly where it was",
    );

    // The control: once the boundary settles, the purge is admitted.
    let _ = reg.commit_boundary_transition(&receipt);
    assert!(
        matches!(
            reg.prepare_purge(reg.generation(), "t", key, ""),
            PurgePreparation::Reserved(_)
        ),
        "after the boundary commits the purge reserves normally, so the refusal is about the \
         in-flight boundary",
    );
}

// ---------------------------------------------------------------------------
// One atomic critical section: no check-then-act, no mutation on exhaustion
// ---------------------------------------------------------------------------

/// A purge attempting to take the lease MID-MUTATION cannot interleave.
///
/// Deterministic rather than timing-based: the hook fires between the guard's
/// validation and its operation, and from inside it a purge reservation is
/// attempted. Under the atomic shape the guards are still held, so
/// `prepare_purge` -- which needs the WRITE lock -- cannot proceed, and the
/// attempt from this thread would deadlock rather than interleave. So the hook
/// instead asserts what it CAN observe without blocking: that the lease set is
/// not writable while the mutation is in flight.
///
/// A check-then-act shape (the previous one) released the lease guard before the
/// operation, which is precisely the window this proves closed.
#[test]
fn a_purge_cannot_take_the_lease_between_validation_and_the_mutation() {
    let reg = Arc::new(registry());
    plant(&reg, "t", "web_search");
    let observed_writable = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let reg_inner = Arc::clone(&reg);
        let observed = Arc::clone(&observed_writable);
        reg.set_generation_pause_hook(Box::new(move || {
            // `try_write` rather than `write`: the point is that the lease set is
            // NOT available here. Blocking would hang this test, which is the
            // same fact stated less usefully.
            if reg_inner.purge_leases_try_write_for_tests() {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }));
    }

    let outcome = reg.observe_in_generation(
        reg.generation(),
        "t",
        "web_search",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert!(
        outcome.applied().is_some(),
        "premise: the mutation itself must be admitted, or the hook never ran inside it",
    );
    assert!(
        !observed_writable.load(std::sync::atomic::Ordering::SeqCst),
        "the lease set must NOT be writable between validation and the mutation: a purge that \
         could take the lease there would capture an entry this mutation is about to refresh, \
         and the operator would be told the purge removed state that had since been relearned",
    );
}

/// A guarded mutation paused mid-flight (holding `entries` then
/// `purge_leases`, the documented order) must not deadlock a concurrent
/// `prepare_purge` for the SAME key.
///
/// The two paths took those two locks in OPPOSITE orders before this fix:
/// `guarded_keyed` held `purge_leases` for read and its `op` reacquired
/// `entries` for write, while `prepare_purge` held `entries` for read then
/// `purge_leases` for write. A mutation paused between the two acquisitions
/// while a purge is mid-flight on the same key is exactly the interleaving
/// that produced the AB/BA cycle: the purge parks on `purge_leases` (held by
/// the mutation) while still holding `entries`, and the mutation's `op` then
/// parks on `entries` (held by the purge) -- neither side can ever resume.
/// This test pauses a mutation AFTER it holds both locks (mirroring the fixed
/// code's actual acquisition point) and starts a purge attempt on the same
/// key while it is paused; if the two locks were still taken in reversed
/// order this would reconstruct the cycle and the `recv_timeout` below would
/// starve, failing the test instead of hanging it.
#[test]
fn a_paused_mutation_does_not_deadlock_a_concurrent_purge_on_the_same_key() {
    use std::sync::mpsc;

    // Arrange
    let reg = Arc::new(registry());
    plant(&reg, "nick", "web_search");
    let generation = reg.generation();

    let (paused_tx, paused_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    reg.set_generation_pause_hook(Box::new(move || {
        let _ = paused_tx.send(());
        let _ = release_rx
            .lock()
            .expect("release slot")
            .recv_timeout(Duration::from_secs(10));
    }));

    // Act: a mutation pauses holding `entries` and `purge_leases`; a purge
    // for the same key starts while it is paused.
    let mutator = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || {
            reg.observe_in_generation(
                generation,
                "nick",
                "web_search",
                "",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                Instant::now(),
            )
        })
    };
    paused_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the mutation must reach the pause point holding both locks");

    let purger = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || reg.prepare_purge(generation, "nick", "web_search", ""))
    };
    // Give the purge thread time to park on `entries` before releasing the
    // mutation, so the two genuinely overlap rather than running in sequence.
    std::thread::sleep(Duration::from_millis(50));
    release_tx.send(()).expect("release the paused mutation");

    // Assert: both sides complete -- a deadlocked run would hang the join
    // past the pause hook's own 10s cap instead of returning here.
    let mutate_outcome = mutator.join().expect("mutator thread must not panic");
    assert!(
        mutate_outcome.applied().is_some(),
        "the paused mutation must still complete once released"
    );
    let purge_outcome = purger.join().expect("purge thread must not panic");
    assert!(
        matches!(purge_outcome, PurgePreparation::Reserved(_)),
        "the purge must complete once the mutation releases the locks it needs"
    );
}

/// An EXHAUSTED incarnation sequence mutates nothing and reports distinctly.
///
/// The value is reserved before any entry is touched, so exhaustion is a refusal
/// rather than a silent mutation. The previous shape allocated afterwards: the
/// entry moved, the caller was told nothing did, and no event described it.
#[test]
fn an_exhausted_incarnation_sequence_refuses_without_mutating() {
    let reg = registry();
    plant(&reg, "t", "web_search");
    let before = reg.snapshot();
    reg.force_incarnation_ceiling_for_tests();

    let outcome = reg.observe_in_generation(
        reg.generation(),
        "t",
        "web_search",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert!(
        outcome.is_exhausted(),
        "exhaustion must be its OWN outcome: it is not a stale generation, not a lease refusal, \
         and not a successful mutation, and a caller that could not tell them apart would \
         either emit an event for a mutation that never happened or retry forever",
    );
    assert!(
        outcome.applied().is_none(),
        "and must carry no value, so no event can be derived from it",
    );
    let after = reg.snapshot();
    assert_eq!(
        after.len(),
        before.len(),
        "and must mutate nothing: the reservation happens before the entry is touched",
    );
    let observations = |snap: &[LearnedRegistryEntry]| {
        snap.iter()
            .find(|e| e.state_key == "t" && e.feature_key == "web_search")
            .map(|e| e.observations)
    };
    assert_eq!(
        observations(&after),
        observations(&before),
        "not even the observation count may move: the mutation did not run",
    );
}

/// A READ allocates no incarnation.
///
/// An incarnation names a VERSION of a key's state. A read produces no new
/// version, so minting one would advance the sequence for nothing -- and on a
/// read-heavy daemon it would exhaust a sequence that only mutations should
/// consume.
#[test]
fn a_read_does_not_consume_an_incarnation() {
    let reg = registry();
    plant(&reg, "t", "web_search");
    let now = Instant::now();
    let baseline = reg
        .negative_state_in_generation(reg.generation(), "t", "web_search", "", now)
        .expect("the live generation may read");

    // Many reads.
    for _ in 0..50 {
        let _ = reg.negative_state_in_generation(reg.generation(), "t", "web_search", "", now);
    }

    // The next MUTATION's incarnation is one past the planting mutation's, not
    // fifty-one past it.
    let mutated = observe_reporting(&reg, "t", "web_search", now);
    assert_eq!(
        mutated.incarnation,
        reg.resident_incarnation_for_tests("t", "web_search", ""),
        "the mutation's reported incarnation is the one stamped on the entry",
    );
    let _ = baseline;
    assert!(
        mutated.incarnation <= 2,
        "fifty reads must not advance the sequence: the first mutation took 1 and this one \
         takes 2, so a value above 2 means reads are consuming ordering space; got {}",
        mutated.incarnation,
    );
}
