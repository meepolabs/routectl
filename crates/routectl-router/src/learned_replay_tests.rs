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
        .admit_provisional(&sibling_a, t0)
        .expect("an unknown pair admits one carry");
    let _ = guard.commit(400, vec![], t0);

    // Assert -- the second sibling reads the SAME learned truth: no second
    // carry, no second learned retry.
    assert!(reg.is_negative_acting(&sibling_b, t0));
    assert!(reg.admit_provisional(&sibling_b, t0).is_none());
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
        .admit_provisional(&onto_mantle, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);

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
    let guard = reg.admit_provisional(&k, t0).expect("unknown pair admits");
    assert!(
        !reg.is_negative_acting(&k, t0),
        "the rejection alone must not persist a negative"
    );

    // Act -- the stripped repair succeeds.
    let event = guard.commit(400, vec!["reasoning_replay".to_string()], t0);

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
    let guard = reg.admit_provisional(&k, t0).expect("unknown pair admits");

    // Act
    guard.release();

    // Assert -- nothing learned, and the slot is free for the next request.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, t0).is_some());
}

#[test]
fn dropping_an_unsettled_guard_releases_the_slot_without_learning() {
    // Arrange -- a dispatch path that returns early never settles.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");

    // Act
    drop(reg.admit_provisional(&k, t0).expect("unknown pair admits"));

    // Assert
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, t0).is_some());
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
                let guard = reg.admit_provisional(&k, t0);
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
    let guard = reg.admit_provisional(&k, t0).expect("unknown pair admits");

    // Act / Assert -- the pair carries no acting negative yet, but the
    // second caller still strips rather than mounting its own carry.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, t0).is_none());

    // Once the probe settles the slot reopens.
    guard.release();
    assert!(reg.admit_provisional(&k, t0).is_some());
}

// --- decay settlement paths ---

#[test]
fn expired_negative_admits_exactly_one_carry() {
    // Arrange -- a persisted negative past its decay window.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);

    // Act
    let first = reg.admit_provisional(&k, lapsed);
    let second = reg.admit_provisional(&k, lapsed);

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
        .admit_provisional(&k, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, lapsed)
        .expect("a lapsed entry admits one carry");

    // Act -- upstream was fixed: the carried artifacts went through.
    let cleared = guard.clear();

    // Assert -- continuity is re-enabled at once, not after another decay.
    assert!(cleared);
    assert!(!reg.is_negative_acting(&k, lapsed));
    assert!(reg.admit_provisional(&k, lapsed).is_some());
}

#[test]
fn a_lapsed_carry_hitting_the_same_rejection_refreshes_the_negative() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, lapsed)
        .expect("a lapsed entry admits one carry");

    // Act -- the same rejection, again confirmed by a successful repair.
    let event = guard.commit(400, vec![], lapsed);

    // Assert -- the entry re-acts on a fresh window with its history intact.
    assert_eq!(event.observations, 2);
    assert!(reg.is_negative_acting(&k, lapsed));
    assert!(reg.admit_provisional(&k, lapsed).is_none());
}

#[test]
fn a_lapsed_carry_hitting_an_unrelated_error_releases_it_unchanged() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, lapsed)
        .expect("a lapsed entry admits one carry");

    // Act -- a transient failure proves nothing either way.
    guard.release();

    // Assert -- neither cleared nor refreshed: the entry survives on its
    // ORIGINAL window (still acting mid-window, still lapsed at the probe
    // time), so the next request re-verifies.
    assert!(reg.is_negative_acting(&k, t0 + DECAY / 2));
    assert!(reg.admit_provisional(&k, lapsed).is_some());
}

// --- emission ---

#[test]
fn the_emission_row_carries_no_body_blob_or_artifact_id() {
    // Arrange -- a request whose reasoning artifacts carry ids and blobs.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("prod-lane");
    let guard = reg.admit_provisional(&k, t0).expect("unknown pair admits");

    // Act
    let event = guard.commit(400, vec!["reasoning_replay".to_string()], t0);

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
