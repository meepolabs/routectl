//! Real-thread races forced between selection and conditional pin commit.

use super::*;
use std::sync::Barrier;

const THREADS: usize = 12;

/// Every contender selects from the same old pin before ANY can commit.
/// Distinct tiebreaks therefore propose different seats deterministically,
/// rather than relying on scheduler timing or sleeps to expose the race.
fn raced_homes(router: &Router, models: &[&str], session: &str) -> Vec<(String, &'static str)> {
    let barrier = Arc::new(Barrier::new(THREADS));
    router.sticky_pins.set_before_commit(Some(Arc::new(move || {
        barrier.wait();
    })));
    let homes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let model = models[i % models.len()];
                scope.spawn(move || {
                    let chain = router.dispatch_chain(model, Some(session)).unwrap();
                    (
                        chain[0].provider_name.clone(),
                        chain[0].selection_decision.unwrap(),
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    router.sticky_pins.set_before_commit(None);
    homes
}

fn assert_one_winner(homes: &[(String, &'static str)], winning_token: &str) -> String {
    let home = homes[0].0.clone();
    assert!(homes.iter().all(|(member, _)| member == &home), "{homes:?}");
    assert_eq!(
        homes
            .iter()
            .filter(|(_, token)| *token == winning_token)
            .count(),
        1
    );
    assert_eq!(
        homes
            .iter()
            .filter(|(_, token)| *token == "sticky_stay")
            .count(),
        THREADS - 1
    );
    home
}

#[test]
fn repeated_pin_replacements_bound_retries_and_atomically_select_a_real_home() {
    let (router, _) = pooled_router(SeatSelection::StickyLeastLoaded);
    let key = crate::router::chain::sticky_pin_key("contended-session", POOL);
    let pins = router.sticky_pins.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = calls.clone();
    let hook_key = key.clone();
    router.sticky_pins.set_before_commit(Some(Arc::new(move || {
        let attempt = hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        pins.put(
            &hook_key,
            crate::seat_pool::SeatPin {
                member: format!("removed-{attempt}"),
                repinned: false,
            },
        );
    })));
    let chain = router
        .dispatch_chain("opus", Some("contended-session"))
        .unwrap();
    router.sticky_pins.set_before_commit(None);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 8);
    assert_eq!(
        router.sticky_pins.get(&key).unwrap().member,
        chain[0].provider_name
    );
    assert_eq!(chain[0].selection_decision, Some("birth_pick"));
}

#[test]
fn simultaneous_same_session_births_adopt_the_winning_home_and_stay_warm() {
    let (router, _) = pooled_router(SeatSelection::StickyLeastLoaded);
    let homes = raced_homes(&router, &["opus"], "same-session");
    let home = assert_one_winner(&homes, "birth_pick");
    assert_eq!(pinned_member(&router, "same-session").unwrap(), home);
    assert_eq!(router.sticky_pins.len(), 1);
    for _ in 0..24 {
        let chain = router.dispatch_chain("opus", Some("same-session")).unwrap();
        assert_eq!(chain[0].provider_name, home);
        assert_eq!(chain[0].selection_decision, Some("sticky_stay"));
    }
    // Other conversations still spread, rather than inheriting this session's
    // home or losing the anti-herd counter because a same-key race occurred.
    let others: std::collections::BTreeSet<_> = (0..9)
        .map(|i| {
            router
                .dispatch_chain("opus", Some(&format!("other-{i}")))
                .unwrap()[0]
                .provider_name
                .clone()
        })
        .collect();
    assert_eq!(others.len(), 3);
}

#[test]
fn simultaneous_overflow_migrations_commit_once_and_all_adopt_the_winner() {
    let (router, _) = pooled_router(SeatSelection::StickyLeastLoaded);
    let birth = router.dispatch_chain("opus", Some("same-session")).unwrap();
    let old_home = birth[0].provider_name.clone();
    assert!(router.force_open_breaker(&birth[0].state_key, Duration::from_hours(1)));
    let homes = raced_homes(&router, &["opus"], "same-session");
    let home = assert_one_winner(&homes, "overflow_repin");
    assert_ne!(home, old_home);
    let pin = router
        .sticky_pins
        .get(&crate::router::chain::sticky_pin_key("same-session", POOL))
        .unwrap();
    assert_eq!(pin.member, home);
    assert!(pin.repinned);
    router.record_success(&birth[0].state_key);
    assert!(router.force_open_breaker(&opus_state_key(&home), Duration::from_hours(1)));
    // Even a now-nondispatchable winning home stays authoritative after its
    // one-time migration. The fallback/gate machinery, not another repin,
    // handles dispatchability; healing the old home must not flap the pin.
    let chain = router.dispatch_chain("opus", Some("same-session")).unwrap();
    assert_eq!(chain[0].provider_name, home);
    assert_eq!(chain[0].selection_decision, Some("sticky_stay"));
}

#[test]
fn concurrent_births_across_models_share_the_pool_pin_but_other_pools_do_not() {
    let router = one_pool_two_models_router();
    let homes = raced_homes(&router, &["opusShared", "sonnetShared"], "same-session");
    let home = assert_one_winner(&homes, "birth_pick");
    assert_eq!(
        pinned_in_shared_pool(&router, "same-session").unwrap(),
        home
    );
    assert_eq!(router.sticky_pins.len(), 1);
    let other = two_pool_sticky_chain_router();
    let chain = other.dispatch_chain("hot", Some("same-session")).unwrap();
    assert_eq!(
        chain
            .iter()
            .filter(|t| t.selection_decision == Some("birth_pick"))
            .count(),
        2
    );
    assert_eq!(other.sticky_pins.len(), 2);
}
