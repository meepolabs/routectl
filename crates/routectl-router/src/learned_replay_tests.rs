use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use routectl_core::ReplayScheme;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

use super::*;
use crate::learned_capability::DEFAULT_MAX_ENTRIES;

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);
const PROVIDER: &str = "openai-compat";

fn registry() -> Arc<ReplayLearnRegistry> {
    Arc::new(ReplayLearnRegistry::new(Arc::new(
        LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES),
    )))
}

fn key(state_key: &str) -> ReplayLearnKey {
    ReplayLearnKey::new(
        state_key,
        PROVIDER,
        ReplayScheme::Mantle,
        ReplayScheme::Codex,
    )
}

// --- keying ---

#[test]
fn sibling_models_on_one_lane_share_one_learned_entry() {
    // Arrange -- the key is built from the lane, never from the model
    // string, so two callers dispatching different models to the same
    // configured target land on one identity.
    let reg = registry();
    let t0 = Instant::now();
    let sibling_a = key("lane-target");
    let sibling_b = key("lane-target");
    assert_eq!(sibling_a, sibling_b);

    // Act -- the first sibling carries, gets rejected, and its stripped
    // repair succeeds.
    let guard = reg
        .admit_provisional(&sibling_a, 1, t0)
        .admitted()
        .expect("an unknown pair admits one carry");
    let _ = guard
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- the second sibling reads the SAME learned truth: no second
    // carry, no second learned retry.
    assert!(reg.is_negative_acting(&sibling_b, t0));
    assert!(
        reg.admit_provisional(&sibling_b, 1, t0)
            .admitted()
            .is_none()
    );
}

#[test]
fn distinct_lanes_and_artifact_schemes_are_distinct_entries() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let onto_mantle = ReplayLearnKey::new("t", PROVIDER, ReplayScheme::Mantle, ReplayScheme::Codex);
    let onto_codex = ReplayLearnKey::new("t", PROVIDER, ReplayScheme::Codex, ReplayScheme::Codex);
    let other_artifact =
        ReplayLearnKey::new("t", PROVIDER, ReplayScheme::Mantle, ReplayScheme::Mantle);

    // Act -- learn the codex-onto-mantle pair only.
    let _ = reg
        .admit_provisional(&onto_mantle, 1, t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- neither the other lane nor the other artifact scheme
    // inherits that truth.
    assert!(reg.is_negative_acting(&onto_mantle, t0));
    assert!(!reg.is_negative_acting(&onto_codex, t0));
    assert!(!reg.is_negative_acting(&other_artifact, t0));
}

// --- two-phase learn ---

#[test]
fn commit_persists_the_negative_only_after_a_successful_stripped_repair() {
    // Arrange -- the carry was rejected; nothing is learned yet.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits");
    assert!(
        !reg.is_negative_acting(&k, t0),
        "the rejection alone must not persist a negative"
    );

    // Act -- the stripped repair succeeds.
    let event = guard
        .commit(400, vec!["reasoning_replay".to_string()], t0)
        .expect("a live commit emits its row");

    // Assert
    assert!(reg.is_negative_acting(&k, t0));
    assert_eq!(event.state_key, k.lane_key());
    assert_eq!(event.capability_key, k.capability_key());
    assert_eq!(event.observations, 1);
    assert_eq!(event.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(event.phase, FailurePhase::F1);
    assert_eq!(event.source, EvidenceSource::Live);
}

#[test]
fn release_leaves_no_persisted_negative() {
    // Arrange -- the carry was rejected and the stripped repair ALSO
    // failed, so the rejection is unconfirmed.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits");

    // Act
    guard.release();

    // Assert -- nothing learned, and the slot is free for the next request.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, 1, t0).admitted().is_some());
}

#[test]
fn dropping_an_unsettled_guard_releases_the_slot_without_learning() {
    // Arrange -- a dispatch path that returns early never settles.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");

    // Act
    drop(
        reg.admit_provisional(&k, 1, t0)
            .admitted()
            .expect("unknown pair admits"),
    );

    // Assert
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, 1, t0).admitted().is_some());
}

// --- single-flight ---

#[test]
fn exactly_one_of_n_concurrent_callers_carries_an_unknown_pair() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let admitted = Arc::new(AtomicUsize::new(0));
    const CALLERS: usize = 16;

    // Act -- N callers race for the carry slot. A barrier holds every
    // thread past its admit BEFORE any winner releases, so no admission can
    // observe a freed slot: the count is decided purely by the single-flight
    // claim, not by scheduler timing.
    let barrier = Arc::new(Barrier::new(CALLERS));
    let handles: Vec<_> = (0..CALLERS)
        .map(|_| {
            let reg = Arc::clone(&reg);
            let admitted = Arc::clone(&admitted);
            let barrier = Arc::clone(&barrier);
            let k = k.clone();
            thread::spawn(move || {
                let guard = reg.admit_provisional(&k, 1, t0).admitted();
                if guard.is_some() {
                    admitted.fetch_add(1, Ordering::SeqCst);
                }
                // Every thread has now admitted-or-stripped; only past this
                // point may the winner drop its guard and free the slot.
                barrier.wait();
                if let Some(guard) = guard {
                    guard.release();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("no thread panics");
    }

    // Assert -- one carried, N-1 stripped.
    assert_eq!(admitted.load(Ordering::SeqCst), 1);
}

#[test]
fn a_concurrent_caller_strips_while_the_probe_is_unresolved() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits");

    // Act / Assert -- the pair carries no acting negative yet, but the
    // second caller still strips rather than mounting its own carry.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, 1, t0).admitted().is_none());

    // Once the probe settles the slot reopens.
    guard.release();
    assert!(reg.admit_provisional(&k, 1, t0).admitted().is_some());
}

// --- decay settlement paths ---

#[test]
fn expired_negative_admits_exactly_one_carry() {
    // Arrange -- a persisted negative past its decay window.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);

    // Act
    let first = reg.admit_provisional(&k, 1, lapsed).admitted();
    let second = reg.admit_provisional(&k, 1, lapsed).admitted();

    // Assert -- one caller re-verifies, the concurrent one keeps stripping.
    assert!(first.is_some());
    assert!(second.is_none());
}

#[test]
fn a_lapsed_carry_that_succeeds_clears_the_negative() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, 1, lapsed)
        .admitted()
        .expect("a lapsed entry admits one carry");

    // Act -- upstream was fixed: the carried artifacts went through.
    let cleared = guard.clear();

    // Assert -- continuity is re-enabled at once, not after another decay,
    // and the removal emits a cleared event to ride out on the dispatch meta.
    assert!(cleared.is_some());
    assert!(!reg.is_negative_acting(&k, lapsed));
    assert!(reg.admit_provisional(&k, 1, lapsed).admitted().is_some());
}

#[test]
fn a_lapsed_carry_hitting_the_same_rejection_refreshes_the_negative() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, 1, lapsed)
        .admitted()
        .expect("a lapsed entry admits one carry");

    // Act -- the same rejection, again confirmed by a successful repair.
    let event = guard
        .commit(400, vec![], lapsed)
        .expect("a live commit emits its row");

    // Assert -- the entry re-acts on a fresh window with its history intact.
    assert_eq!(event.observations, 2);
    assert!(reg.is_negative_acting(&k, lapsed));
    assert!(reg.admit_provisional(&k, 1, lapsed).admitted().is_none());
}

#[test]
fn a_lapsed_carry_hitting_an_unrelated_error_releases_it_unchanged() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, 1, lapsed)
        .admitted()
        .expect("a lapsed entry admits one carry");

    // Act -- a transient failure proves nothing either way.
    guard.release();

    // Assert -- neither cleared nor refreshed: the entry survives on its
    // ORIGINAL window (still acting mid-window, still lapsed at the probe
    // time), so the next request re-verifies.
    assert!(reg.is_negative_acting(&k, t0 + DECAY / 2));
    assert!(reg.admit_provisional(&k, 1, lapsed).admitted().is_some());
}

// --- emission ---

#[test]
fn the_emission_row_carries_no_body_blob_or_artifact_id() {
    // Arrange -- a request whose reasoning artifacts carry ids and blobs.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("prod-lane");
    let guard = reg
        .admit_provisional(&k, 1, t0)
        .admitted()
        .expect("unknown pair admits");

    // Act
    let event = guard
        .commit(400, vec!["reasoning_replay".to_string()], t0)
        .expect("a live commit emits its row");

    // Assert -- every string field is a normalized key or a closed-set
    // token; the row has no field that could hold an artifact at all.
    assert_eq!(event.state_key, "prod-lane#mantle");
    assert_eq!(event.capability_key, "reasoning_replay:codex");
    assert_eq!(event.provider_kind, PROVIDER);
    assert_eq!(event.request_features, vec!["reasoning_replay".to_string()]);
    assert_eq!(event.upstream_status, 400);
    assert!(!event.remapped);
}

#[test]
fn the_key_never_embeds_the_caller_supplied_model_string() {
    // Arrange -- the same lane, reached while the caller asked for two
    // different models. The lifecycle is never handed the model string, so
    // the identity cannot vary with it.
    let lane_scheme = ReplayScheme::Mantle;
    let artifact = ReplayScheme::Codex;

    // Act
    let k = ReplayLearnKey::new("configured-target", PROVIDER, lane_scheme, artifact);

    // Assert
    assert_eq!(k.lane_key(), "configured-target#mantle");
    assert_eq!(k.capability_key(), "reasoning_replay:codex");
}

// ---- generation barrier ------------------------------------------------------

/// A replay carry settles against the generation it was ADMITTED under.
///
/// `reasoning_replay:<scheme>` is catalog-SCOPED, so a settlement arriving after
/// a reload must persist nothing and emit no row: the negative would be
/// attributed to a catalog revision the daemon has left, and its ledger row
/// would be replayed by a later boot.
#[test]
fn a_stale_replay_commit_persists_nothing_and_emits_nothing() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    let admitted_generation = reg.learned().generation();
    let guard = reg
        .admit_provisional(&key, admitted_generation, t0)
        .admitted()
        .expect("unknown pair admits");

    // The reload lands between the carry and its settlement. Advance only: a
    // full transition would also prune the entry, so "nothing persisted" could
    // not distinguish a refusal from a prune.
    reg.learned().advance_generation();

    let emitted = guard.commit(400, vec![], t0);

    assert!(emitted.is_none(), "a stale commit must emit no learn event");
    assert!(
        reg.learned().snapshot().is_empty(),
        "and must persist no negative",
    );
}

/// The positive control: with no reload in between the same commit persists and
/// emits. Without it the test above would pass against a commit that never
/// records anything.
#[test]
fn a_live_replay_commit_persists_and_emits() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    let guard = reg
        .admit_provisional(&key, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");

    let emitted = guard.commit(400, vec![], t0);

    assert!(emitted.is_some(), "a live commit emits its row");
    assert_eq!(reg.learned().snapshot().len(), 1);
}

/// The emitted event's observation count matches the registry's own
/// resident count at the moment `commit` returns, pinning that the count
/// rides out of the SAME guarded mutation rather than a later, separately
/// taken snapshot. A second commit on the same lapsed pair (see
/// `a_lapsed_carry_hitting_the_same_rejection_refreshes_the_negative` above)
/// already exercises this across repeated commits; this test additionally
/// asserts the emitted count against a snapshot taken immediately after
/// settlement, so a regression that decoupled the two (e.g. reverting to a
/// second, unguarded `snapshot()` lookup) has an assertion that can fail on
/// its own.
#[test]
fn the_emitted_observation_count_matches_the_registrys_resident_count() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");

    // Act
    let event = guard
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert
    let resident = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.lane_key() && entry.feature_key == k.capability_key())
        .expect("the commit left a resident row");
    assert_eq!(event.observations, resident.observations);
}

/// A commit's emitted event carries the count ITS OWN guarded observe
/// captured, even when a sibling observation lands on the same pair after
/// that observe returns but before the event is built. Exercised via a
/// test-only hook fired from exactly that point -- lock-free by then, so
/// the sibling mutation runs synchronously inline rather than needing a
/// spawned thread.
#[test]
fn commit_emits_its_own_captured_count_despite_a_sibling_observation_during_the_pause() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");
    let sibling_reg = Arc::clone(&reg);
    let generation = reg.learned().generation();
    let lane_key = k.lane_key().to_string();
    let capability_key = k.capability_key().to_string();
    reg.learned().set_post_observe_test_hook(Box::new(move || {
        let _ = sibling_reg
            .learned()
            .observe_in_generation_with_observations(
                generation,
                &lane_key,
                &capability_key,
                PROVIDER,
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                t0,
            );
    }));

    // Act
    let event = guard
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- the emitted event keeps its own captured count, while the
    // resident count reflects the sibling's later bump.
    assert_eq!(
        event.observations, 1,
        "the emitted event must retain its own captured count"
    );
    let resident = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.lane_key() && entry.feature_key == k.capability_key())
        .expect("a resident entry after both observations");
    assert_eq!(
        resident.observations, 2,
        "the sibling observation must have bumped the resident count"
    );
}

/// A purge lease taken on the pair's key blocks a concurrent commit: the
/// commit is refused, the leased row is left untouched, and the in-flight
/// slot is still released so the same pair can admit again once the lease
/// clears.
#[test]
fn a_purge_lease_blocks_a_concurrent_replay_commit_and_leaves_the_entry_untouched() {
    // Arrange -- a prior successful commit leaves a resident row for the
    // purge to capture, then lapses so a second carry may admit.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let _ = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let before = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.lane_key() && entry.feature_key == k.capability_key())
        .expect("the commit left a resident row");
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t_lapsed)
        .admitted()
        .expect("a lapsed pair admits one carry");
    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.lane_key(),
        k.capability_key(),
        PROVIDER,
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    // Act
    let outcome = guard.commit(400, vec![], t_lapsed);

    // Assert -- refused, and the leased row is untouched.
    assert!(
        outcome.is_none(),
        "a commit racing a purge lease must emit no event",
    );
    let after = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.lane_key() && entry.feature_key == k.capability_key())
        .expect("the leased row is left resident, not removed");
    assert_eq!(
        before.observations, after.observations,
        "a refused commit must not mutate the leased entry",
    );

    reg.learned().restore_purge(lease);

    // The commit's refusal arm must have released the in-flight slot: the
    // same lapsed pair admits again once the lease is gone.
    assert!(
        reg.admit_provisional(&k, reg.learned().generation(), t_lapsed)
            .admitted()
            .is_some(),
        "the same lapsed pair must admit again once the purge lease is released",
    );
}

/// A commit refused by a held purge lease emits its OWN distinct debug
/// event, not the generic stale one -- Reserved and Stale name different
/// truths and must not share a diagnostic.
#[test]
fn a_reserved_replay_commit_emits_its_own_debug_event() {
    // Arrange -- a resident row for the lease to capture, then a lapsed
    // second carry racing that lease.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let _ = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t_lapsed)
        .admitted()
        .expect("a lapsed pair admits one carry");
    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.lane_key(),
        k.capability_key(),
        PROVIDER,
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    // Act
    let events = routectl_testkit::capture_events(|| {
        let _ = guard.commit(400, vec![], t_lapsed);
    });

    // Assert
    let reserved: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("replay_learn_reserved"))
        .collect();
    assert_eq!(
        reserved.len(),
        1,
        "exactly one reserved-refusal debug event"
    );

    reg.learned().restore_purge(lease);
}

/// A commit refused because the incarnation sequence is exhausted emits its
/// own distinct debug event.
#[test]
fn an_exhausted_replay_commit_emits_its_own_debug_event() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");
    reg.learned().force_incarnation_ceiling_for_tests();

    // Act
    let events = routectl_testkit::capture_events(|| {
        let _ = guard.commit(400, vec![], t0);
    });

    // Assert
    let exhausted: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("replay_learn_exhausted"))
        .collect();
    assert_eq!(
        exhausted.len(),
        1,
        "exactly one exhaustion-refusal debug event"
    );
}

/// A clear refused by a held purge lease emits its own distinct debug
/// event.
#[test]
fn a_reserved_replay_clear_emits_its_own_debug_event() {
    // Arrange -- a lapsed resident row, then a second carry admitted and a
    // purge lease taken on the same key before the first guard clears.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let _ = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t_lapsed)
        .admitted()
        .expect("a lapsed pair admits one carry");
    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.lane_key(),
        k.capability_key(),
        PROVIDER,
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    // Act
    let events = routectl_testkit::capture_events(|| {
        let _ = guard.clear();
    });

    // Assert
    let reserved: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("replay_clear_reserved"))
        .collect();
    assert_eq!(
        reserved.len(),
        1,
        "exactly one reserved-refusal debug event"
    );

    reg.learned().restore_purge(lease);
}

/// A clear refused because the incarnation sequence is exhausted emits its
/// own distinct debug event.
#[test]
fn an_exhausted_replay_clear_emits_its_own_debug_event() {
    // Arrange -- a resident row, lapsed, then a carry admitted for the
    // clear itself.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("lane-a");
    let _ = reg
        .admit_provisional(&k, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, reg.learned().generation(), t_lapsed)
        .admitted()
        .expect("a lapsed pair admits one carry");
    reg.learned().force_incarnation_ceiling_for_tests();

    // Act
    let events = routectl_testkit::capture_events(|| {
        let _ = guard.clear();
    });

    // Assert
    let exhausted: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("replay_clear_exhausted"))
        .collect();
    assert_eq!(
        exhausted.len(),
        1,
        "exactly one exhaustion-refusal debug event"
    );
}

/// A stale CLEAR removes nothing and emits no cleared event.
#[test]
fn a_stale_replay_clear_removes_nothing_and_emits_nothing() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    // Persist a negative first, then lapse it so a later carry is admitted.
    let guard = reg
        .admit_provisional(&key, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");
    let _ = guard.commit(400, vec![], t0);
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let admitted_generation = reg.learned().generation();
    let guard = reg
        .admit_provisional(&key, admitted_generation, t_lapsed)
        .admitted()
        .expect("a lapsed pair admits one carry");

    reg.learned().advance_generation();
    let cleared = guard.clear();

    assert!(
        cleared.is_none(),
        "a stale clear must emit no cleared event",
    );
    assert_eq!(
        reg.learned().snapshot().len(),
        1,
        "and must leave the negative resident",
    );
}

/// A stale settlement still RELEASES the in-flight slot.
///
/// The slot is request-local coordination, not persisted state. Leaking it would
/// latch the pair forever: every later request would strip while nothing could
/// ever settle the claim.
#[test]
fn a_stale_settlement_still_releases_the_in_flight_slot() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    let admitted_generation = reg.learned().generation();
    let guard = reg
        .admit_provisional(&key, admitted_generation, t0)
        .admitted()
        .expect("unknown pair admits");
    reg.learned().advance_generation();

    let _ = guard.commit(400, vec![], t0);

    // The slot is free, so the pair can be carried again on the live generation.
    assert!(
        reg.admit_provisional(&key, reg.learned().generation(), t0)
            .admitted()
            .is_some(),
        "a stale settlement must not leave the carry slot latched",
    );
}

/// An OLD guard releases the claim the REPLACEMENT facade reads.
///
/// The in-flight set is shared as an `Arc` across facade rebuilds precisely for
/// this: a carry outstanding when the swap lands settles through the old facade,
/// and if the two held separate sets the replacement would either admit a second
/// concurrent carry for that pair or never see the release.
#[test]
fn an_old_guard_releases_the_replacement_facades_claim() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    let guard = reg
        .admit_provisional(&key, reg.learned().generation(), t0)
        .admitted()
        .expect("unknown pair admits");

    // The reload rebuilds the facade on the same shared registry.
    let replacement = Arc::new(reg.rebuilt_on(Arc::clone(reg.registry_arc())));
    // The replacement sees the outstanding claim, so it refuses a second carry.
    assert!(
        replacement
            .admit_provisional(&key, reg.learned().generation(), t0)
            .admitted()
            .is_none(),
        "the replacement must observe the old facade's outstanding claim",
    );

    // The OLD guard settles, releasing the shared claim.
    let _ = guard.commit(400, vec![], t0);

    // And the replacement can now admit -- only possible if both address one set.
    assert!(
        replacement
            .admit_provisional(
                &key,
                reg.learned().generation(),
                t0 + DECAY + Duration::from_secs(1)
            )
            .admitted()
            .is_some(),
        "the old guard's release must be visible to the replacement facade",
    );
}

/// A superseded Router's admission reports STALE, distinctly from Acting.
///
/// The distinction is load-bearing: `Acting` makes the caller STRIP, and a
/// stale Router stripping means it acted on the replacement generation's state
/// -- a routing decision it has no standing to make. `Stale` makes it carry
/// nothing and leave the request as the client sent it.
#[test]
fn a_superseded_router_admission_reports_stale_not_acting() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");
    let stale_generation = reg.learned().generation();
    // The reload lands; advance only, so the entry (if any) is not pruned and
    // cannot be confused with the refusal.
    reg.learned().advance_generation();

    let admission = reg.admit_provisional(&key, stale_generation, t0);

    assert!(
        admission.is_stale(),
        "a superseded admission must be Stale, never Acting -- Acting would \
         make the caller strip on state it may not read",
    );
}

/// The positive control: a LIVE admission on an unknown pair is admitted.
#[test]
fn a_live_admission_on_an_unknown_pair_is_admitted() {
    let reg = registry();
    let t0 = Instant::now();
    let key = key("lane-a");

    let admission = reg.admit_provisional(&key, reg.learned().generation(), t0);

    assert!(!admission.is_stale());
    assert!(admission.admitted().is_some(), "an unknown pair admits");
}
