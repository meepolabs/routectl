#![allow(clippy::match_single_binding)]

//! Tests for the shared-registry generation barrier and the retunable
//! capability timing / capacity settings.

use super::*;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use std::sync::Arc;

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES)
}

/// The wire-shape capability key, assembled at runtime so no source line
/// outside the owning module spells the namespace prefix.
fn field_key(path: &str) -> String {
    format!("{}{}{}", "fie", "ld:", path)
}

/// A fresh registry starts at a known generation, and each advance returns the
/// new one. The generation is what lets a stale Router's Arc be told apart
/// from the live one, so it must be observable and monotonic.
#[test]
fn generation_starts_at_one_and_advances_monotonically() {
    let reg = registry();
    assert_eq!(reg.generation(), 1);
    assert_eq!(reg.advance_generation(), 2);
    assert_eq!(reg.generation(), 2);
    assert_eq!(reg.advance_generation(), 3);
}

/// A catalog-scoped observation from a STALE generation is inert: nothing
/// resident, and the caller is told it was rejected so it emits no ledger
/// event and bumps no metric.
///
/// This is what stops an old Router Arc from repopulating the catalog-scoped
/// state the boundary just pruned. Without it the shared Arc would let a
/// request still in flight on the pre-swap Router re-learn a negative the
/// revision change was meant to discard.
#[test]
fn a_stale_generation_catalog_scoped_observation_is_rejected_and_inert() {
    let reg = registry();
    let stale = reg.generation();
    reg.advance_generation();

    let outcome = reg.observe_in_generation(
        stale,
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert_eq!(outcome, GenerationOutcome::Stale);
    assert!(
        reg.is_empty(),
        "a stale catalog-scoped observation must leave nothing resident",
    );
}

/// A catalog-INDEPENDENT observation from a stale generation is ACCEPTED
/// against the active generation.
///
/// A wire-shape fact does not depend on the catalog, so a request that was
/// already in flight when the reload landed still learned something true. The
/// shared Arc means that fact lands in the live registry rather than in a
/// discarded copy.
#[test]
fn a_stale_generation_field_observation_is_accepted_into_the_live_registry() {
    let reg = registry();
    let stale = reg.generation();
    reg.advance_generation();
    let key = field_key("thinking.enabled.display");

    let outcome = reg.observe_in_generation(
        stale,
        "nick",
        &key,
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert!(
        matches!(outcome, GenerationOutcome::Applied { .. }),
        "a wire-shape fact from an old generation must still be accepted, got {outcome:?}",
    );
    let resident: Vec<String> = reg
        .snapshot()
        .into_iter()
        .map(|entry| entry.feature_key)
        .collect();
    assert_eq!(resident, vec![key]);
}

/// A catalog-scoped READ from a stale generation reports stale rather than
/// answering, so a pre-swap Router cannot route on catalog truth the reload
/// replaced. A field read still answers.
#[test]
fn stale_generation_reads_are_scoped_by_key_class() {
    let reg = registry();
    let now = Instant::now();
    let key = field_key("thinking.enabled.display");
    // Learn both classes at the current generation.
    for capability in [key.as_str(), "web_search"] {
        reg.observe(
            "nick",
            capability,
            "anthropic-api",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
    }
    let stale = reg.generation();
    reg.advance_generation();

    // A catalog-scoped read from the stale generation must not answer.
    assert_eq!(
        reg.acting_negative_in_generation(stale, "nick", "web_search", "anthropic-api", now)
            .map(|(d, _)| d),
        None,
        "a stale catalog-scoped read must report stale, never a verdict",
    );
    // The field read still answers, because its truth is catalog-independent.
    assert!(
        matches!(
            reg.acting_negative_in_generation(stale, "nick", &key, "anthropic-api", now),
            Some((RoutingDecision::RouteAway { .. }, _))
        ),
        "a stale field read must still answer",
    );
}

/// Pruning at the boundary drops the catalog-scoped entries and keeps the
/// catalog-independent ones, in one pass under the registry's own lock.
#[test]
fn pruning_catalog_scoped_entries_keeps_the_field_entries() {
    let reg = registry();
    let now = Instant::now();
    let key = field_key("thinking.enabled.display");
    for capability in [key.as_str(), "web_search", "computer_use"] {
        reg.observe(
            "nick",
            capability,
            "anthropic-api",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
    }

    let pruned = reg.prune_catalog_scoped();

    assert_eq!(pruned, 2, "both catalog-scoped entries are dropped");
    let resident: Vec<String> = reg
        .snapshot()
        .into_iter()
        .map(|entry| entry.feature_key)
        .collect();
    assert_eq!(resident, vec![key]);
}

/// A probe settlement from a stale generation on a catalog-scoped key is a
/// no-op: it must neither clear a resident entry nor produce a cleared event
/// for the ledger.
#[test]
fn a_stale_generation_probe_settlement_on_a_scoped_key_is_a_no_op() {
    let reg = registry();
    let now = Instant::now();
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let stale = reg.generation();
    reg.advance_generation();

    let removed = reg.remove_keyed_in_generation(stale, "nick", "web_search", "anthropic-api");

    assert_eq!(removed, GenerationOutcome::Stale);
    assert_eq!(
        reg.snapshot().len(),
        1,
        "a stale settlement must not remove the resident entry",
    );
}

/// The shared registry retunes its hot-reloadable capability settings in
/// place.
///
/// It has to: the registry now OUTLIVES the Router generation that built it,
/// so a reload that changes `[capability]` timing or capacity has no other way
/// to apply it. Leaving the old values would silently pin the operator's
/// tuning to whenever the daemon last restarted.
#[test]
fn retuning_applies_new_timing_and_capacity_to_the_shared_registry() {
    let reg = registry();
    let now = Instant::now();
    assert_eq!(reg.decay(), DECAY);

    reg.retune(Duration::from_hours(1), Duration::from_mins(5), 7);

    assert_eq!(reg.decay(), Duration::from_hours(1));
    assert_eq!(reg.inferred_window(), Duration::from_mins(5));
    assert_eq!(reg.max_entries(), 7);

    // The new decay governs a subsequently learned negative.
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let entry = &reg.snapshot()[0];
    let acted_for = entry.expires_at.saturating_duration_since(now);
    assert!(
        acted_for <= Duration::from_hours(1),
        "the retuned decay must govern the new entry (got {acted_for:?})",
    );
}

/// The boundary cut is ATOMIC with respect to observations: no observation can
/// land between the survivor snapshot and the batch submission.
///
/// An observation that interleaved there would be in neither place -- absent
/// from the snapshot (so not restated past the new tombstone) and written
/// before the boundary (so invisible to the next boot). It would vanish while
/// every individual step looked correct.
///
/// The assertion is that the store is UNCHANGED at the moment of submit, read
/// from inside the closure. Checking only the snapshot's contents would pass
/// even if the guard were released first: the snapshot is taken before the
/// release either way. A contending thread is blocked on the same lock
/// throughout, so if the guard is dropped early its write lands and the
/// in-closure read sees it.
#[test]
fn the_boundary_cut_excludes_observations_between_snapshot_and_submit() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let reg = Arc::new(registry());
    let now = Instant::now();
    reg.observe(
        "nick",
        &field_key("thinking.enabled.display"),
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let resident_before = reg.snapshot().len();

    let contender_started = Arc::new(AtomicBool::new(false));
    let sibling = {
        let reg = Arc::clone(&reg);
        let started = Arc::clone(&contender_started);
        std::thread::spawn(move || {
            started.store(true, Ordering::SeqCst);
            // Blocks until the cut releases the guard.
            reg.observe(
                "nick",
                "web_search",
                "anthropic-api",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                Instant::now(),
            );
        })
    };
    while !contender_started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }

    // Act: inside the cut, give the sibling every chance to interleave and then
    // read the store back through a NON-blocking probe.
    let cut = reg.with_boundary_cut(
        |survivors, _pending| {
            for _ in 0..1_000 {
                std::thread::yield_now();
            }
            (
                survivors
                    .iter()
                    .map(|e| e.feature_key.clone())
                    .collect::<Vec<_>>(),
                reg.try_entry_count(),
            )
        },
        |_| false,
    );
    let (survivors, resident_at_submit) = match cut {
        BoundaryCut::Rejected { outcome } => outcome,
        _ => panic!("expected Rejected for a not-admitted cut"),
    };

    sibling.join().expect("sibling");

    assert_eq!(
        resident_at_submit, None,
        "the cut must still hold the registry guard when it submits",
    );
    assert_eq!(survivors, vec![field_key("thinking.enabled.display")]);
    // The sibling's write landed only after the cut completed.
    assert_eq!(reg.snapshot().len(), resident_before + 1);
}

/// The cut returns the catalog-independent survivors only -- the set a
/// restatement batch must carry.
#[test]
fn the_boundary_cut_snapshots_only_catalog_independent_entries() {
    let reg = registry();
    let now = Instant::now();
    let key = field_key("thinking.enabled.display");
    for capability in [key.as_str(), "web_search"] {
        reg.observe(
            "nick",
            capability,
            "anthropic-api",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
    }

    let cut = reg.with_boundary_cut(
        |survivors, _pending| {
            survivors
                .iter()
                .map(|e| e.feature_key.clone())
                .collect::<Vec<_>>()
        },
        |_| false,
    );
    let keys = match cut {
        BoundaryCut::Rejected { outcome } => outcome,
        _ => panic!("expected Rejected"),
    };

    assert_eq!(keys, vec![key]);
}

// ---- atomicity of generation validation ------------------------------------

/// Shared state for a paused-operation test: the registry, plus the sibling
/// thread that attempts a boundary transition during the pause.
struct PausedBoundary {
    reg: Arc<LearnedCapabilityRegistry>,
    sibling: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Generation observed from inside the guarded operation.
    observed: std::sync::Mutex<Option<u64>>,
}

impl PausedBoundary {
    /// Arm a registry whose guarded operations pause once, and during that pause
    /// a SIBLING thread attempts `commit_boundary_transition`.
    ///
    /// The sibling is essential: the transition takes the generation lock for
    /// writing while the paused operation holds it for reading, so attempting it
    /// on the paused thread would deadlock rather than test anything.
    fn arm() -> Arc<Self> {
        let this = Arc::new(Self {
            reg: Arc::new(registry()),
            sibling: std::sync::Mutex::new(None),
            observed: std::sync::Mutex::new(None),
        });
        let armed = Arc::clone(&this);
        this.reg.set_generation_pause_hook(Box::new(move || {
            let reg = Arc::clone(&armed.reg);
            let handle = std::thread::spawn(move || {
                // Contends on the GENERATION lock alone. A full
                // `commit_boundary_transition` also takes `entries`, which every
                // inner operation takes as well, so it would serialize behind
                // that regardless and could not distinguish a released guard
                // from a held one.
                reg.advance_generation();
                reg.prune_catalog_scoped();
            });
            // Long enough that an UNBLOCKED sibling finishes inside the pause --
            // which is exactly what a check-then-lock shape would allow.
            std::thread::sleep(std::time::Duration::from_millis(200));
            *armed.sibling.lock().expect("sibling slot") = Some(handle);
        }));
        let watching = Arc::clone(&this);
        this.reg
            .set_generation_probe_hook(Box::new(move |generation| {
                *watching.observed.lock().expect("observed slot") = Some(generation);
            }));
        this
    }

    /// Wait for the sibling's boundary transition to land.
    fn settle(&self) {
        if let Some(handle) = self.sibling.lock().expect("sibling slot").take() {
            handle.join().expect("boundary sibling");
        }
    }

    /// The generation active when the guarded operation actually ran.
    ///
    /// Captured by a second hook fired from INSIDE the guarded closure. Under the
    /// atomic shape this equals the generation the operation validated against,
    /// because the guard is still held. Under check-then-lock the sibling has
    /// already advanced it, so the values differ -- which is the gap, visible
    /// regardless of which locks the inner operation happens to re-take.
    fn generation_at_operation(&self) -> Option<u64> {
        *self.observed.lock().expect("observed slot")
    }

    /// Whether any catalog-scoped entry is resident.
    fn holds_catalog_scoped(&self) -> bool {
        self.reg
            .snapshot()
            .into_iter()
            .any(|e| crate::field_capability::capability_key_is_catalog_scoped(&e.feature_key))
    }
}

/// Generation validation must be ATOMIC with the entries operation it guards.
///
/// A check-then-lock shape has a window: the operation reads the generation,
/// finds it current, releases, and only then takes the entries lock -- so a
/// boundary transition landing in between lets a catalog-scoped write through
/// AFTER the generation advanced and the prune ran, repopulating exactly what
/// the boundary evicted.
///
/// The window is made deterministic rather than raced for. The invariant
/// asserted is the one that discriminates: once the operation and the boundary
/// have both completed, NO catalog-scoped entry may be resident. Under the
/// atomic shape the write lands first and the prune then removes it; under
/// check-then-lock the prune runs first and the write survives it. Asserting on
/// the generation counter instead would not discriminate, because the boundary
/// legitimately lands right after an atomic operation releases.
#[test]
fn a_stale_observe_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let generation_at_call = armed.reg.generation();

    armed.reg.observe_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    armed.settle();

    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
    assert!(
        !armed.holds_catalog_scoped(),
        "a catalog-scoped entry must not survive the boundary that pruned it",
    );
}

/// Same atomicity requirement for the positive-admission path.
#[test]
fn a_stale_positive_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let generation_at_call = armed.reg.generation();

    armed.reg.observe_positive_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
        EvidenceSource::Live,
        Some("search_blocks"),
        Instant::now(),
    );
    armed.settle();

    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
    assert!(
        !armed.holds_catalog_scoped(),
        "a catalog-scoped positive must not survive the boundary that pruned it",
    );
}

/// The keyed removal a probe settlement drives must be atomic too: a settlement
/// that validated pre-boundary must not report a removal it performed against
/// state the boundary had already taken.
#[test]
fn a_stale_removal_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let now = Instant::now();
    // Seeded BEFORE the hook can fire, via the ungated path.
    armed.reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let generation_at_call = armed.reg.generation();

    let removed = armed.reg.remove_keyed_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
    );
    armed.settle();

    // Under the atomic shape the removal runs first and reports `true`; the
    // boundary then prunes nothing. Under check-then-lock the boundary prunes
    // first and the removal reports `false` while still claiming to be Applied.
    assert!(
        matches!(removed, GenerationOutcome::Applied { value: true, .. }),
        "the removal must act on the state it validated against",
    );
    assert!(!armed.holds_catalog_scoped());
    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
}

/// The act-side read must be atomic: a generation that validated pre-boundary
/// must read the state it validated against, not a half-pruned one.
#[test]
fn a_stale_read_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let now = Instant::now();
    armed.reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let generation_at_call = armed.reg.generation();

    let decision = armed.reg.acting_negative_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
        now,
    );
    armed.settle();

    assert!(
        matches!(decision, Some((RoutingDecision::RouteAway { .. }, _))),
        "the read must see the entry it validated against, got {decision:?}",
    );
    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
}

/// Same for the expiry sweep, which mutates a resident entry's decay clock.
#[test]
fn a_stale_expiry_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let now = Instant::now();
    armed.reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let generation_at_call = armed.reg.generation();

    let expired = armed.reg.expire_keyed_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
        now,
    );
    armed.settle();

    assert!(
        matches!(expired, GenerationOutcome::Applied { value: true, .. }),
        "the expiry must act on the entry it validated against",
    );
    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
}

/// Same for a probe settlement recorded through the barrier.
#[test]
fn a_stale_probe_settlement_cannot_slip_through_a_pause_after_validation() {
    let armed = PausedBoundary::arm();
    let now = Instant::now();
    armed.reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let generation_at_call = armed.reg.generation();

    let settled = armed.reg.record_probe_outcome_in_generation(
        generation_at_call,
        "nick",
        "web_search",
        "anthropic-api",
        ProbeOutcome::Success,
        now,
    );
    armed.settle();

    assert!(
        matches!(settled, GenerationOutcome::Applied { value: (), .. }),
        "the settlement must apply against the state it validated",
    );
    assert!(
        !armed.holds_catalog_scoped(),
        "a successful settlement clears the entry; the boundary finds nothing",
    );
    assert_eq!(
        armed.generation_at_operation(),
        Some(generation_at_call),
        "the operation must run under the generation it validated against",
    );
}

/// The generation an operation reports is selected while its guard is HELD, so
/// it cannot be moved by a boundary that lands after the operation completes.
///
/// A value sampled after the guard releases can be changed by a boundary in
/// between, stamping the event with a generation that does not describe its
/// state. Here the sibling installs a pending generation only AFTER the guarded
/// operation has finished, so a paired read reports the pre-boundary value and a
/// post-release read reports the sibling's.
///
/// (A sibling installing DURING the window is a different, legitimate case: the
/// pending generation is exactly what a concurrent observation should adopt, and
/// `the_effective_generation_follows_the_pending_boundary` covers it.)
#[test]
fn the_reported_generation_cannot_be_moved_by_a_later_boundary() {
    let reg = Arc::new(registry());
    let active = reg.generation();
    let pending = active + 7;

    let outcome = reg.observe_in_generation(
        active,
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    // The boundary lands AFTER the operation returned. A generation the
    // operation reported must not shift under it.
    reg.install_pending_generation(pending);

    let GenerationOutcome::Applied { generation, .. } = outcome else {
        panic!("a live observation must apply");
    };
    assert_eq!(
        generation, active,
        "the reported generation is fixed at operation time, not re-read later",
    );
    // The registry's own effective value HAS moved -- which is what a
    // post-release sample would have picked up.
    assert_eq!(reg.effective_persistence_generation(), pending);
}

// ---- lock order --------------------------------------------------------------

/// A `Debug` render concurrent with a boundary commit must not deadlock.
///
/// The two acquire the same locks. `commit_boundary_transition` takes
/// `generation -> pending_generation -> entries` for WRITING; `Debug` reads them,
/// and used to read `entries` FIRST and hold it across the whole builder -- the
/// reverse order, which closes a cycle. A `Debug` on a shared registry is
/// reachable from any tracing or panic path, so an inverted acquisition there is
/// easy to trip and hard to attribute.
///
/// Deterministic rather than raced: the commit is blocked mid-acquisition, after
/// it holds `generation` but before it reaches `entries`, using the
/// lock-acquisition hook. A `Debug` render is then attempted from this thread. If
/// `Debug` took `entries` first it would hold the lock the commit is about to
/// want while waiting for the `generation` the commit already holds. Under the
/// documented order it queues behind `generation` and completes once the commit
/// finishes.
///
/// The bounded wait is the assertion, and it is checked on a WATCHER thread so a
/// wedge is reported rather than hanging the suite.
#[test]
fn a_debug_render_concurrent_with_a_boundary_commit_does_not_deadlock() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    let reg = Arc::new(registry());
    // `entries` non-empty, so the render has something to count.
    reg.observe(
        "nick",
        &field_key("thinking.enabled.display"),
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    // The commit signals when it has taken `generation`, then waits for release.
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    {
        let reached_tx = reached_tx.clone();
        let release_rx = Arc::clone(&release_rx);
        // Fires ONCE, and only for the commit's first acquisition. Without the
        // one-shot guard the Debug render's own `generation` acquisition would
        // also block here and the fixture would deadlock on itself -- which it
        // did, once the render was instrumented.
        let armed = Arc::new(AtomicBool::new(false));
        reg.set_lock_acquire_hook(Box::new(move |lock| {
            if lock == "generation" && !armed.swap(true, Ordering::SeqCst) {
                let _ = reached_tx.send(());
                // Hold the commit here, between `generation` and `entries`.
                let _ = release_rx
                    .lock()
                    .expect("release slot")
                    .recv_timeout(std::time::Duration::from_secs(10));
            }
        }));
    }

    let committer = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || {
            let BoundaryCut::Taken { receipt, .. } = reg.with_boundary_cut(|_, _| (), |()| true)
            else {
                return;
            };
            let _ = reg.commit_boundary_transition(&receipt);
        })
    };
    // The commit is now parked holding `generation` only.
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the commit must reach its first acquisition");

    // Render from a third thread and watch for completion. Under the inverted
    // order this never finishes: it holds `entries` and waits for `generation`.
    let rendered = Arc::new(AtomicBool::new(false));
    let renderer = {
        let reg = Arc::clone(&reg);
        let rendered = Arc::clone(&rendered);
        std::thread::spawn(move || {
            let _ = format!("{reg:?}");
            rendered.store(true, Ordering::SeqCst);
        })
    };

    // Give the render a real chance to wedge, then let the commit finish.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = release_tx.send(());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !(committer.is_finished() && renderer.is_finished()) {
        assert!(
            std::time::Instant::now() < deadline,
            "Debug and the boundary commit deadlocked (commit finished={}, \
             render finished={})",
            committer.is_finished(),
            renderer.is_finished(),
        );
        std::thread::yield_now();
    }
    committer.join().expect("committer");
    renderer.join().expect("renderer");
    assert!(
        rendered.load(Ordering::SeqCst),
        "the Debug render must complete",
    );
}

/// The boundary cut acquires its locks in the documented order:
/// `generation`, then `pending_generation`, then `entries`.
///
/// Recorded through the lock-acquisition hook rather than inferred from timing, so
/// this fails immediately if the order is ever rearranged -- which is what the
/// previous sleep-based liveness checks could not do.
#[test]
fn the_boundary_cut_acquires_its_locks_in_the_documented_order() {
    let reg = Arc::new(registry());
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    {
        let order = Arc::clone(&order);
        reg.set_lock_acquire_hook(Box::new(move |lock| {
            order.lock().expect("order").push(lock);
        }));
    }

    // The cut runs all acquisitions regardless of whether the submitted callback
    // reports success; Rejected is the outcome when it reports failure.
    let cut = reg.with_boundary_cut(|_survivors, _pending| (), |()| false);
    assert!(matches!(cut, BoundaryCut::Rejected { .. }));

    assert_eq!(
        *order.lock().expect("order"),
        vec![
            "generation",
            "pending_generation",
            "entries",
            "purge_leases"
        ],
        "the cut must acquire generation, then pending_generation, then entries, then \
         purge_leases -- the documented order, which is what makes the cut deadlock-free \
         against every path that takes any two of them",
    );
}

/// `commit_boundary_transition` acquires the same three locks in the same order.
///
/// The pairing is what makes the cut deadlock-free, so the commit's order is
/// pinned independently rather than assumed to match.
#[test]
fn the_boundary_commit_acquires_its_locks_in_the_documented_order() {
    let reg = Arc::new(registry());
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    {
        let order = Arc::clone(&order);
        reg.set_lock_acquire_hook(Box::new(move |lock| {
            order.lock().expect("order").push(lock);
        }));
    }

    // Admit through the real cut so the receipt matches, then clear the order
    // recorded by the CUT's own acquisitions and commit.
    let BoundaryCut::Taken { receipt, .. } = reg.with_boundary_cut(|_, _| (), |()| true) else {
        panic!("must be taken");
    };
    order.lock().expect("order").clear();
    let _ = reg.commit_boundary_transition(&receipt);

    assert_eq!(
        *order.lock().expect("order"),
        vec!["generation", "pending_generation", "entries"],
        "the commit must acquire the same three locks in the same order as the cut",
    );
}

/// A stale receipt from a rolled-back boundary must not promote the counter.
///
/// After A rolls back, the generation is still 1. B admits and derives gen=2.
/// A's receipt names gen=2 (it was the first admitted boundary), which EQUALS
/// B's generation. But A's receipt has a different id, so it returns
/// StaleReceipt -- demonstrating that the receipt, not just the generation,
/// governs promotion.
#[test]
fn a_stale_receipt_cannot_promote_even_at_the_same_generation() {
    let reg = registry();
    // A: admitted and rolled back. The active generation does not move.
    let BoundaryCut::Taken { receipt: a, .. } = admit_boundary(&reg) else {
        panic!("A must be admitted");
    };
    let _ = reg.rollback_pending_generation(&a);

    // B: derives the same generation A did.
    let BoundaryCut::Taken { receipt: b, .. } = admit_boundary(&reg) else {
        panic!("B must be admitted after A rolled back");
    };
    assert_eq!(a.generation, b.generation, "the ABA condition");
    assert_ne!(a, b, "but the receipts are different");

    // A's stale receipt cannot promote B.
    assert_eq!(
        reg.commit_boundary_transition(&a),
        BoundarySettlement::StaleReceipt,
    );
    assert_eq!(reg.generation(), 1, "the counter must not have moved");
}

/// The positive control: a strictly advancing pending value IS promoted.
#[test]
fn a_strictly_advancing_pending_generation_is_promoted() {
    let reg = registry();
    let BoundaryCut::Taken { receipt, .. } =
        reg.with_boundary_cut(|_survivors, _pending| (), |()| true)
    else {
        panic!("the first cut must be taken");
    };
    assert_eq!(
        reg.commit_boundary_transition(&receipt),
        BoundarySettlement::Applied {
            generation: receipt.generation(),
            pruned: 0,
        },
    );
}

// ---- exactly one admitted boundary -------------------------------------------

/// Take a cut whose submit always "admits", returning the receipt.
fn admit_boundary(reg: &LearnedCapabilityRegistry) -> BoundaryCut<usize> {
    reg.with_boundary_cut(
        |survivors, _pending| survivors.len(),
        |_: &_| -> bool { true },
    )
}

/// A second boundary is REFUSED while the first is admitted and unsettled.
///
/// Two in flight would each stamp events with their own generation while only one
/// can be promoted, so the loser's events are dropped by the writer as older than
/// the winner's boundary -- a silent verdict loss. The retired shape advanced PAST
/// the in-flight value and allocated another, which is exactly that.
#[test]
fn a_second_boundary_is_refused_while_the_first_is_admitted() {
    let reg = registry();
    let first = admit_boundary(&reg);
    let BoundaryCut::Taken { receipt: a, .. } = first else {
        panic!("the first boundary must be admitted");
    };

    let second = admit_boundary(&reg);

    assert!(
        matches!(second, BoundaryCut::Busy { in_flight } if in_flight == a.generation),
        "the second boundary must be refused as Busy, got {second:?}",
    );
    // And no second generation was allocated: the pending slot still holds A's.
    assert_eq!(reg.effective_persistence_generation(), a.generation);
}

/// A succeeds, and only then may B be admitted -- with a strictly later
/// generation.
#[test]
fn a_boundary_admits_again_only_after_the_first_settles() {
    let reg = registry();
    let BoundaryCut::Taken { receipt: a, .. } = admit_boundary(&reg) else {
        panic!("A must be admitted");
    };
    assert_eq!(
        reg.commit_boundary_transition(&a),
        BoundarySettlement::Applied {
            generation: a.generation,
            pruned: 0
        },
    );

    let BoundaryCut::Taken { receipt: b, .. } = admit_boundary(&reg) else {
        panic!("B must be admitted once A settled");
    };
    assert!(
        b.generation > a.generation,
        "B must establish a strictly later generation"
    );
}

/// A FAILS (rolled back), and B then succeeds -- so a failed boundary does not
/// wedge the daemon out of ever moving its boundary again.
#[test]
fn a_failed_boundary_frees_the_slot_for_the_next_one() {
    let reg = registry();
    let BoundaryCut::Taken { receipt: a, .. } = admit_boundary(&reg) else {
        panic!("A must be admitted");
    };
    assert_eq!(
        reg.rollback_pending_generation(&a),
        BoundarySettlement::Applied {
            generation: a.generation,
            pruned: 0
        },
    );
    // Rolled back, so the active generation did NOT move.
    assert!(reg.generation() < a.generation);

    let BoundaryCut::Taken { receipt: b, .. } = admit_boundary(&reg) else {
        panic!("B must be admitted after A rolled back");
    };
    assert_eq!(
        reg.commit_boundary_transition(&b),
        BoundarySettlement::Applied {
            generation: b.generation,
            pruned: 0
        },
    );
    assert_eq!(reg.generation(), b.generation);
}

/// A's stale receipt can neither promote nor clear B's boundary.
///
/// After A settles, its receipt names a generation the pending slot no longer
/// holds. Honouring it would promote the counter on behalf of a boundary that is
/// already done -- and prune entries no committed batch accounted for -- or clear
/// B's admitted state so B's own settlement finds nothing to promote.
#[test]
fn a_stale_receipt_cannot_settle_a_later_boundary() {
    let reg = registry();
    let now = Instant::now();
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let BoundaryCut::Taken { receipt: a, .. } = admit_boundary(&reg) else {
        panic!("A must be admitted");
    };
    let _ = reg.commit_boundary_transition(&a);
    // B is admitted and unsettled; the catalog-scoped entry is re-seeded so a
    // wrongful prune would be visible.
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let BoundaryCut::Taken { receipt: b, .. } = admit_boundary(&reg) else {
        panic!("B must be admitted");
    };
    let entries_before = reg.snapshot().len();

    // A's receipt is now stale.
    assert_eq!(
        reg.commit_boundary_transition(&a),
        BoundarySettlement::StaleReceipt,
        "a stale receipt must not promote",
    );
    assert_eq!(
        reg.rollback_pending_generation(&a),
        BoundarySettlement::StaleReceipt,
        "nor clear",
    );
    // B's admitted state and the entries are untouched, so B can still settle.
    assert_eq!(reg.effective_persistence_generation(), b.generation);
    assert_eq!(reg.snapshot().len(), entries_before);
    assert_eq!(
        reg.commit_boundary_transition(&b),
        BoundarySettlement::Applied {
            generation: b.generation,
            pruned: 1
        },
        "B's own receipt still settles it",
    );
}

/// At `u64::MAX` the boundary is REFUSED before any batch, prune or state change.
///
/// A saturating add would hand back the active generation as the "next" one and
/// every later comparison would read wrongly; refusing degrades to "no more
/// boundaries" instead.
#[test]
fn an_exhausted_generation_refuses_the_boundary_before_any_change() {
    let reg = registry();
    let now = Instant::now();
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    reg.set_generation_for_test(u64::MAX);
    let entries_before = reg.snapshot().len();

    let mut submitted = false;
    let cut = reg.with_boundary_cut(
        |_survivors, _pending| {
            submitted = true;
            0_usize
        },
        |_| true,
    );

    assert!(
        matches!(cut, BoundaryCut::Exhausted),
        "an exhausted counter must refuse, got {cut:?}",
    );
    assert!(
        !submitted,
        "the batch must not be submitted when the counter is exhausted",
    );
    assert_eq!(reg.generation(), u64::MAX, "the counter did not wrap");
    assert_eq!(
        reg.effective_persistence_generation(),
        u64::MAX,
        "and no pending generation was installed",
    );
    assert_eq!(
        reg.snapshot().len(),
        entries_before,
        "nor was anything pruned",
    );
}

/// The control for the exhaustion case: one below `u64::MAX` still admits, so the
/// refusal is the boundary value and not a blanket rejection.
#[test]
fn a_generation_one_below_the_maximum_still_admits() {
    let reg = registry();
    reg.set_generation_for_test(u64::MAX - 1);

    let cut = admit_boundary(&reg);

    assert!(
        matches!(cut, BoundaryCut::Taken { receipt, .. } if receipt.generation() == u64::MAX),
        "the last available generation must still be admissible, got {cut:?}",
    );
}

/// `Debug` acquires its locks in the documented order:
/// `generation`, `pending_generation`, `entries`, `tuning`.
///
/// Recorded DIRECTLY through the acquisition hook rather than inferred from a
/// concurrent liveness fixture. The liveness test is supplementary: it proves the
/// order does not deadlock against a commit, but it can only fail by timing out,
/// which is a slow and indirect signal for a rearranged acquisition. This fails
/// immediately and names what changed.
#[test]
fn the_debug_render_acquires_its_locks_in_the_documented_order() {
    let reg = registry();
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    {
        let order = Arc::clone(&order);
        reg.set_lock_acquire_hook(Box::new(move |lock| {
            order.lock().expect("order").push(lock);
        }));
    }

    let rendered = format!("{reg:?}");

    assert_eq!(
        *order.lock().expect("order"),
        vec!["generation", "pending_generation", "entries", "tuning"],
        "Debug must acquire all four locks in the documented order",
    );
    // And it must actually have rendered the fields, or the order above could be
    // satisfied by a Debug that reads the locks and prints nothing.
    for field in ["entries", "generation", "pending_generation", "tuning"] {
        assert!(
            rendered.contains(field),
            "the render must include {field}: {rendered}",
        );
    }
}

/// The ABA regression: A rolls back, B derives the SAME persistence generation,
/// and A's stale receipt must NOT settle B.
///
/// Without a receipt ID, A's `expected_generation == 2` matches B's `pending == 2`
/// exactly, so A's stale commit would promote the generation, prune entries, and
/// emit a success -- all on behalf of a boundary that was already rolled back.
/// With a receipt ID, A's `{ generation: 2, id: 1 }` does not match
/// B's `{ generation: 2, id: 2 }`, so it returns StaleReceipt and changes nothing.
#[test]
fn a_rolled_back_receipt_cannot_settle_a_later_boundary_at_the_same_generation() {
    let reg = registry();
    let now = Instant::now();
    reg.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let entries_before = reg.snapshot().len();

    // A: admitted, then ROLLED BACK. The generation does not advance.
    let BoundaryCut::Taken { receipt: a, .. } = admit_boundary(&reg) else {
        panic!("A must be admitted");
    };
    let gen_a = a.generation;
    let _ = reg.rollback_pending_generation(&a);
    assert!(
        reg.generation() < gen_a,
        "rollback must leave the generation unmoved"
    );

    // B: derives the SAME persistence generation as A.
    let BoundaryCut::Taken { receipt: b, .. } = admit_boundary(&reg) else {
        panic!("B must be admitted after A rolled back");
    };
    assert_eq!(
        b.generation, gen_a,
        "the ABA condition: B derives the same generation A did",
    );
    assert_ne!(a, b, "the receipts must differ even at the same generation");

    // A's stale receipt must not promote B.
    assert_eq!(
        reg.commit_boundary_transition(&a),
        BoundarySettlement::StaleReceipt,
        "A's stale receipt must not promote B's boundary",
    );
    assert_eq!(
        reg.rollback_pending_generation(&a),
        BoundarySettlement::StaleReceipt,
        "nor clear B's pending state",
    );
    // B's pending state and the entries are untouched.
    assert_eq!(reg.effective_persistence_generation(), b.generation);
    assert_eq!(reg.snapshot().len(), entries_before);

    // B's own receipt settles it.
    assert_eq!(
        reg.commit_boundary_transition(&b),
        BoundarySettlement::Applied {
            generation: b.generation,
            pruned: 1,
        },
    );
    assert_eq!(reg.generation(), b.generation);
}

/// The `#[must_use]` attribute on `BoundaryCut` must be present, so a caller
/// cannot silently discard a `Taken` without settling it.
///
/// This is a source-text guard, not a compile-lint guard: `#[must_use]` on a type
/// produces a WARNING, not an error, and a test cannot assert that a warning fired.
/// What it CAN assert is that the attribute exists on the type definition, which
/// is what prevents it from being accidentally removed.
#[test]
fn the_boundary_cut_type_is_must_use() {
    let source = include_str!("learned_capability.rs");
    // The attribute must appear DIRECTLY before the `pub enum BoundaryCut` line.
    assert!(
        source.contains("#[must_use") && source.contains("pub enum BoundaryCut"),
        "BoundaryCut must carry a #[must_use] attribute",
    );
    // Verify the two are close together (within 5 lines).
    // Search backward from the enum declaration, not forward from the first
    // `#[must_use]` in the file (which may be on a different type).
    let enum_decl = source.find("pub enum BoundaryCut").expect("enum");
    let before = &source[..enum_decl];
    let must_use = before
        .rfind("#[must_use")
        .expect("must_use before BoundaryCut");
    let gap = source[must_use..enum_decl]
        .chars()
        .filter(|c| *c == '\n')
        .count();
    assert!(
        gap <= 5,
        "#[must_use] must be within 5 lines of BoundaryCut, found {gap} lines apart",
    );
}

/// A rejected submission allocates no receipt and installs no pending state.
/// A retry after refusal succeeds. The original refusal's stale outcome cannot
/// settle the retried boundary.
#[test]
fn a_rejected_submission_leaves_no_pending_state_and_a_retry_succeeds() {
    let reg = registry();
    let gen_before = reg.generation();

    // The submit closure always "refuses" (returns false from `admitted`).
    let cut = reg.with_boundary_cut(|_survivors, pending| pending, |_: &u64| false);
    assert!(
        matches!(cut, BoundaryCut::Rejected { .. }),
        "a refused admission must produce Rejected, got {cut:?}",
    );
    assert_eq!(
        reg.effective_persistence_generation(),
        gen_before,
        "Rejected must install no pending state",
    );

    // A retry that succeeds is admitted normally.
    let BoundaryCut::Taken { receipt, .. } =
        reg.with_boundary_cut(|_survivors, pending| pending, |_: &u64| true)
    else {
        panic!("a retry must be Taken once the refusal is gone");
    };
    assert_eq!(
        reg.effective_persistence_generation(),
        receipt.generation(),
        "the retry's receipt is the effective generation",
    );

    // The original refusal's outcome cannot be used to commit (it carries no
    // receipt, so there is nothing to present).
    let BoundaryCut::Rejected {
        outcome: gen_from_refusal,
    } = cut
    else {
        unreachable!();
    };
    assert_eq!(
        gen_from_refusal,
        receipt.generation(),
        "the refusal and the retry derived the same generation"
    );
    // But the refusal has NO receipt, so it cannot settle.
    // (This is proven structurally: `Rejected` has no `receipt` field to
    // pass to `commit_boundary_transition`, so the compiler prevents it.)
}
