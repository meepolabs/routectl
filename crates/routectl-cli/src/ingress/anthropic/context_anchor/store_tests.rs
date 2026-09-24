//! Publication, ordering and capacity rules of the anchor store.

use std::sync::{Arc, Barrier};
use std::thread;

use routectl_core::ChatRequest;
use routectl_core::test_utils::{assistant_text_msg, user_msg};

use super::super::{MAX_ANCHOR_MODEL_BYTES, MAX_ANCHOR_SESSION_KEY_BYTES};
use super::{
    ANCHOR_CAPACITY, AnchorKey, AnchorLane, ContextAnchorStore, RequestIdentity, SettleOutcome,
    TurnOutcome,
};

fn lane() -> AnchorLane {
    AnchorLane {
        provider_kind: "openai-compat".into(),
        model: "glm".into(),
        upstream_model: "glm-4.6".into(),
        generation: 1,
    }
}

fn key() -> AnchorKey {
    AnchorKey::new("session-a", "claude-opus").expect("key within bounds")
}

fn request_with(turns: usize) -> ChatRequest {
    let messages: Vec<_> = (0..turns)
        .map(|i| {
            if i % 2 == 0 {
                user_msg(&format!("question {i}"))
            } else {
                assistant_text_msg(&format!("answer {i}"))
            }
        })
        .collect();
    ChatRequest {
        model: "claude-opus".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

fn completed(actual: u64) -> TurnOutcome {
    TurnOutcome::Completed {
        served_lane: lane(),
        cache_inclusive_input: Some(actual),
    }
}

fn settle_turn(store: &ContextAnchorStore, key: AnchorKey, turns: usize, actual: u64) {
    let pending = store.reserve_turn(key).measure(&request_with(turns), None);
    assert_eq!(pending.settle(completed(actual)), SettleOutcome::Published);
}

#[test]
fn a_measured_success_publishes_a_record() {
    // Arrange
    let store = ContextAnchorStore::new();
    let req = request_with(3);
    let pending = store.reserve_turn(key()).measure(&req, None);

    // Act
    let outcome = pending.settle(completed(900));

    // Assert
    assert_eq!(outcome, SettleOutcome::Published);
    let record = store.get(&key()).expect("published");
    assert_eq!(record.actual_input(), 900);
    assert_eq!(record.message_count(), 3);
    assert_eq!(
        record.normalized_estimate(),
        RequestIdentity::measure(&req, None).normalized_estimate()
    );
    assert_eq!(record.lane(), &lane());
}

#[test]
fn success_without_terminal_usage_does_not_publish() {
    let store = ContextAnchorStore::new();
    let pending = store.reserve_turn(key()).measure(&request_with(3), None);

    let outcome = pending.settle(TurnOutcome::Completed {
        served_lane: lane(),
        cache_inclusive_input: None,
    });

    assert_eq!(outcome, SettleOutcome::NotPublished);
    assert!(store.get(&key()).is_none());
}

#[test]
fn an_error_does_not_publish_or_disturb_the_anchor() {
    // Arrange
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 3, 900);
    let pending = store.reserve_turn(key()).measure(&request_with(5), None);

    // Act
    let outcome = pending.settle(TurnOutcome::Failed);

    // Assert
    assert_eq!(outcome, SettleOutcome::NotPublished);
    let record = store.get(&key()).expect("earlier anchor kept");
    assert_eq!(record.message_count(), 3);
}

#[test]
fn a_cancellation_does_not_publish_or_disturb_the_anchor() {
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 3, 900);
    let pending = store.reserve_turn(key()).measure(&request_with(5), None);

    let outcome = pending.settle(TurnOutcome::Cancelled);

    assert_eq!(outcome, SettleOutcome::NotPublished);
    assert_eq!(store.get(&key()).expect("kept").message_count(), 3);
}

#[test]
fn a_newer_success_replaces_the_record() {
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 3, 900);
    let before = store.get(&key()).expect("first");

    settle_turn(&store, key(), 5, 1_400);

    let after = store.get(&key()).expect("second");
    assert_eq!(after.message_count(), 5);
    assert_eq!(after.actual_input(), 1_400);
    assert_eq!(
        before.message_count(),
        3,
        "an earlier snapshot is never edited"
    );
}

#[test]
fn a_compacted_request_can_establish_its_own_anchor() {
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 9, 5_000);

    settle_turn(&store, key(), 2, 300);

    assert_eq!(store.get(&key()).expect("replaced").message_count(), 2);
}

#[test]
fn an_older_turn_finishing_late_cannot_overwrite_a_newer_anchor() {
    // Arrange: turn A starts, turn B starts, B finishes, then A finishes.
    let store = ContextAnchorStore::new();
    let older = store.reserve_turn(key()).measure(&request_with(3), None);
    let newer = store.reserve_turn(key()).measure(&request_with(5), None);
    assert_eq!(newer.settle(completed(1_400)), SettleOutcome::Published);

    // Act
    let late = older.settle(completed(900));

    // Assert
    assert_eq!(late, SettleOutcome::Superseded);
    let record = store.get(&key()).expect("newer kept");
    assert_eq!(record.message_count(), 5);
    assert_eq!(record.actual_input(), 1_400);
}

#[test]
fn turn_ordering_is_per_key_in_effect() {
    // A late turn on one session is not superseded by a newer turn on
    // another session.
    let store = ContextAnchorStore::new();
    let other = AnchorKey::new("session-b", "claude-opus").expect("key within bounds");
    let older = store.reserve_turn(key()).measure(&request_with(3), None);
    settle_turn(&store, other.clone(), 5, 1_400);

    let late = older.settle(completed(900));

    assert_eq!(late, SettleOutcome::Published);
    assert_eq!(store.len(), 2);
}

#[test]
fn a_fresh_store_is_cold_for_a_session_anchored_elsewhere() {
    let before_restart = ContextAnchorStore::new();
    settle_turn(&before_restart, key(), 3, 900);

    let after_restart = ContextAnchorStore::new();

    assert!(before_restart.get(&key()).is_some(), "positive control");
    assert!(after_restart.get(&key()).is_none());
    assert!(after_restart.is_empty());
}

#[test]
fn capacity_is_bounded_and_evicts_the_least_recent_session() {
    // Arrange
    let store = ContextAnchorStore::new();
    let req = request_with(1);
    let overflow = 3;

    // Act
    for i in 0..ANCHOR_CAPACITY + overflow {
        let key = AnchorKey::new(&format!("s-{i}"), "m").expect("key within bounds");
        store
            .reserve_turn(key)
            .measure(&req, None)
            .settle(completed(1));
    }

    // Assert
    assert_eq!(store.len(), ANCHOR_CAPACITY);
    assert!(
        store
            .get(&AnchorKey::new("s-0", "m").expect("key within bounds"))
            .is_none()
    );
    assert!(
        store
            .get(
                &AnchorKey::new(&format!("s-{}", ANCHOR_CAPACITY + overflow - 1), "m")
                    .expect("key within bounds")
            )
            .is_some()
    );
}

#[test]
fn concurrent_settles_on_one_session_keep_the_newest_turn() {
    // Arrange: every thread begins in a known order, then all settle at
    // once in whatever order the scheduler picks.
    const THREADS: usize = 32;
    let store = Arc::new(ContextAnchorStore::new());
    let pendings: Vec<_> = (0..THREADS)
        .map(|i| {
            store
                .reserve_turn(key())
                .measure(&request_with(i + 1), None)
        })
        .collect();
    let barrier = Arc::new(Barrier::new(THREADS));

    // Act
    let handles: Vec<_> = pendings
        .into_iter()
        .enumerate()
        .map(|(i, pending)| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                pending.settle(completed(i as u64 + 1))
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("settle thread"))
        .collect();

    // Assert
    let record = store.get(&key()).expect("published");
    assert_eq!(record.message_count(), THREADS);
    assert_eq!(record.actual_input(), THREADS as u64);
    assert!(outcomes.contains(&SettleOutcome::Published));
    assert_eq!(store.len(), 1);
}

#[test]
fn concurrent_sessions_do_not_interfere() {
    const THREADS: usize = 16;
    let store = Arc::new(ContextAnchorStore::new());
    let barrier = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let session = AnchorKey::new(&format!("s-{i}"), "m").expect("key within bounds");
                barrier.wait();
                for turns in 1..=4 {
                    settle_turn(&store, session.clone(), turns, turns as u64 * 100);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("session thread");
    }

    assert_eq!(store.len(), THREADS);
    for i in 0..THREADS {
        let record = store
            .get(&AnchorKey::new(&format!("s-{i}"), "m").expect("key within bounds"))
            .expect("each session anchored");
        assert_eq!(record.message_count(), 4);
        assert_eq!(record.actual_input(), 400);
    }
}

// ------------------------------------------------------------- ordering

#[test]
fn a_slow_to_measure_older_turn_stays_older_than_a_fast_newer_one() {
    // Arrange: the older turn reserves first, but the newer turn measures
    // and publishes before the older one finishes hashing its large body.
    let store = ContextAnchorStore::new();
    let older_ticket = store.reserve_turn(key());
    let newer = store.reserve_turn(key()).measure(&request_with(5), None);
    assert_eq!(newer.settle(completed(1_400)), SettleOutcome::Published);
    let older = older_ticket.measure(&request_with(300), None);

    // Act
    let late = older.settle(completed(90_000));

    // Assert
    assert_eq!(late, SettleOutcome::Superseded);
    assert_eq!(store.get(&key()).expect("newer kept").message_count(), 5);
}

#[test]
fn an_older_turn_cannot_republish_after_the_newer_record_was_evicted() {
    // Arrange: a newer turn anchors the session, then enough other sessions
    // arrive to evict it.
    let store = ContextAnchorStore::new();
    let older = store.reserve_turn(key()).measure(&request_with(3), None);
    settle_turn(&store, key(), 5, 1_400);
    for i in 0..ANCHOR_CAPACITY {
        settle_turn(
            &store,
            AnchorKey::new(&format!("filler-{i}"), "m").expect("key"),
            1,
            1,
        );
    }
    assert!(
        store.get(&key()).is_none(),
        "positive control: newer record evicted"
    );

    // Act
    let late = older.settle(completed(900));

    // Assert
    assert_eq!(late, SettleOutcome::Superseded);
    assert!(store.get(&key()).is_none());
}

#[test]
fn a_turn_reserved_after_an_eviction_can_re_enter_the_session() {
    // Arrange
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 5, 1_400);
    for i in 0..ANCHOR_CAPACITY {
        settle_turn(
            &store,
            AnchorKey::new(&format!("filler-{i}"), "m").expect("key"),
            1,
            1,
        );
    }
    assert!(
        store.get(&key()).is_none(),
        "positive control: record evicted"
    );

    // Act
    let fresh = store.reserve_turn(key()).measure(&request_with(7), None);
    let outcome = fresh.settle(completed(2_000));

    // Assert
    assert_eq!(outcome, SettleOutcome::Published);
    assert_eq!(store.get(&key()).expect("re-entered").message_count(), 7);
}

#[test]
fn an_unrelated_eviction_does_not_block_a_turn_reserved_later() {
    // A turn reserved after the last eviction is unaffected by it.
    let store = ContextAnchorStore::new();
    for i in 0..=ANCHOR_CAPACITY {
        settle_turn(
            &store,
            AnchorKey::new(&format!("filler-{i}"), "m").expect("key"),
            1,
            1,
        );
    }
    let later = store.reserve_turn(key()).measure(&request_with(3), None);

    let outcome = later.settle(completed(900));

    assert_eq!(outcome, SettleOutcome::Published);
}

// ---------------------------------------------------------- key bounds

#[test]
fn a_requested_model_at_the_byte_bound_is_accepted() {
    let model = "m".repeat(MAX_ANCHOR_MODEL_BYTES);

    assert!(AnchorKey::new("session-a", &model).is_some());
}

#[test]
fn a_requested_model_one_byte_over_the_bound_is_rejected() {
    let model = "m".repeat(MAX_ANCHOR_MODEL_BYTES + 1);

    assert!(AnchorKey::new("session-a", &model).is_none());
}

#[test]
fn a_request_body_sized_model_name_is_rejected() {
    // The largest body the server accepts by default is 32 MiB, so a
    // client could send a model name of about that size.
    let model = "m".repeat(32 * 1024 * 1024);

    assert!(AnchorKey::new("session-a", &model).is_none());
}

#[test]
fn a_multibyte_model_name_is_bounded_in_bytes_not_chars() {
    let chars = MAX_ANCHOR_MODEL_BYTES / 2 + 1;
    let model = "\u{e9}".repeat(chars);

    assert!(
        model.chars().count() <= MAX_ANCHOR_MODEL_BYTES,
        "positive control"
    );
    assert!(AnchorKey::new("session-a", &model).is_none());
}

#[test]
fn an_overlong_session_key_is_rejected() {
    let session = "s".repeat(MAX_ANCHOR_SESSION_KEY_BYTES + 1);
    let at_bound = "s".repeat(MAX_ANCHOR_SESSION_KEY_BYTES);

    assert!(AnchorKey::new(&at_bound, "m").is_some());
    assert!(AnchorKey::new(&session, "m").is_none());
}

// ------------------------------------------------------- settlement binding

#[test]
fn a_turn_publishes_only_under_the_key_it_was_reserved_for() {
    // Arrange
    let store = ContextAnchorStore::new();
    let other = AnchorKey::new("session-b", "claude-opus").expect("key");
    let pending = store.reserve_turn(key()).measure(&request_with(3), None);

    // Act
    let outcome = pending.settle(completed(900));

    // Assert
    assert_eq!(outcome, SettleOutcome::Published);
    assert!(store.get(&key()).is_some());
    assert!(store.get(&other).is_none());
    assert_eq!(store.len(), 1);
}

#[test]
fn a_turn_publishes_only_into_the_store_that_reserved_it() {
    // Arrange: a second store (e.g. one built after a restart) is present.
    let issuing = ContextAnchorStore::new();
    let other = ContextAnchorStore::new();
    let pending = issuing.reserve_turn(key()).measure(&request_with(3), None);

    // Act
    let outcome = pending.settle(completed(900));

    // Assert
    assert_eq!(outcome, SettleOutcome::Published);
    assert!(issuing.get(&key()).is_some());
    assert!(other.get(&key()).is_none());
    assert!(other.is_empty());
}

#[test]
fn a_turn_outliving_its_store_publishes_nowhere_else() {
    // Arrange
    let issuing = ContextAnchorStore::new();
    let pending = issuing.reserve_turn(key()).measure(&request_with(3), None);
    drop(issuing);
    let replacement = ContextAnchorStore::new();

    // Act
    let _ = pending.settle(completed(900));

    // Assert
    assert!(replacement.get(&key()).is_none());
}

#[test]
fn the_current_record_is_read_for_the_reserved_key() {
    let store = ContextAnchorStore::new();
    settle_turn(&store, key(), 3, 900);
    let other = AnchorKey::new("session-b", "claude-opus").expect("key");

    let own = store.reserve_turn(key()).current_record();
    let foreign = store.reserve_turn(other).current_record();

    assert_eq!(own.expect("anchored").message_count(), 3);
    assert!(foreign.is_none());
}
