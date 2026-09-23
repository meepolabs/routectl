// The capability-persistence health seam: the fail-safe default, the installed
// answers, and the carry-over across a rebuild.
//
// What is NOT here: the gate's effect on the planner and on the status surface.
// Those live with the surfaces they gate (`field_preflight_tests.rs` and
// `fidelity_status_tests.rs`), because a gate asserted only where it is declared
// proves the predicate and not the refusal.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::CapabilityPersistenceHealth;
use crate::config::Config;
use crate::router::Router;

/// A health read whose answer the test controls, and which COUNTS its reads.
///
/// The count is what distinguishes "the router asked and got false" from "the
/// router never asked" -- two states a boolean assertion on the predicate's
/// output cannot tell apart.
#[derive(Default)]
struct ControlledHealth {
    durable: AtomicBool,
    reads: std::sync::atomic::AtomicUsize,
}

impl ControlledHealth {
    fn answering(durable: bool) -> Arc<Self> {
        Arc::new(Self {
            durable: AtomicBool::new(durable),
            reads: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Acquire)
    }

    fn set(&self, durable: bool) {
        self.durable.store(durable, Ordering::Release);
    }
}

impl CapabilityPersistenceHealth for ControlledHealth {
    fn capability_writes_durable(&self) -> bool {
        self.reads.fetch_add(1, Ordering::AcqRel);
        self.durable.load(Ordering::Acquire)
    }
}

fn bare_router() -> Router {
    Router::new(Arc::new(Config::default()))
}

#[test]
fn an_uninstalled_health_read_reports_writes_as_not_guaranteed() {
    // THE FAIL-SAFE DEFAULT, and the one that makes every wiring mistake land on
    // the safe side: a Router nobody installed a health read on must not permit
    // learned pre-flight. The permissive state takes an explicit installation.
    //
    // Mutation check: make the predicate `is_none_or` instead of `is_some_and`
    // (i.e. absent reads as healthy) -> red here.
    let router = bare_router();

    assert!(
        router.capability_persistence_health().is_none(),
        "premise: Router::new installs no health read",
    );
    assert!(
        !router.capability_writes_durable(),
        "an absent health read must read as NOT GUARANTEED, so a build path that \
         forgot to wire one suspends learned pre-flight rather than permitting it",
    );
}

#[test]
fn an_installed_read_is_actually_consulted_for_each_answer() {
    // The paired control for the default above, and it asserts the CALL as well as
    // the answer: a predicate that returned a cached or hardcoded value would
    // satisfy an answer-only assertion on the healthy case.
    let health = ControlledHealth::answering(true);
    let router = bare_router().with_capability_persistence_health(health.clone());

    assert!(router.capability_writes_durable());
    assert_eq!(
        health.reads(),
        1,
        "the installed read must actually be consulted, not answered from a cache",
    );

    // The SAME router, the read now answering false: the predicate must follow the
    // live answer rather than a value sampled at install time.
    health.set(false);
    assert!(
        !router.capability_writes_durable(),
        "an unhealthy answer must suspend, on the same router that permitted a \
         moment earlier -- the health is live state, not an install-time constant",
    );
    assert_eq!(health.reads(), 2, "and each decision is its own read");
}

#[test]
fn a_second_installation_replaces_the_first() {
    // What a boot path installing its own read over a carried-over one needs. A
    // second installation that ACCUMULATED would leave two reads whose answers
    // could disagree, and nothing would say which one gates traffic.
    let stale = ControlledHealth::answering(true);
    let live = ControlledHealth::answering(false);
    let router = bare_router()
        .with_capability_persistence_health(stale.clone())
        .with_capability_persistence_health(live.clone());

    assert!(
        !router.capability_writes_durable(),
        "the SECOND installation is the one that answers",
    );
    assert_eq!(live.reads(), 1, "and it is the one consulted");
    assert_eq!(
        stale.reads(),
        0,
        "while the replaced one is never asked again",
    );
}

#[test]
fn the_health_read_rides_across_a_rebuild() {
    // The reload half: both generations submit capability events to the SAME
    // writer, so its health is one fact. A replacement that lost the read would
    // suspend learned pre-flight on every reload until a boot path reinstalled it
    // -- a reload-shaped outage of the feature.
    //
    // Mutation check: delete the `capability_health` line from
    // `carry_over_learned_from` -> red here.
    let health = ControlledHealth::answering(true);
    let previous = bare_router().with_capability_persistence_health(health.clone());
    let mut replacement = bare_router();
    assert!(
        !replacement.capability_writes_durable(),
        "premise: a freshly-built replacement has no read of its own",
    );

    replacement.carry_over_learned_from(&previous);

    assert!(
        replacement.capability_writes_durable(),
        "the replacement must reserve its pre-flight decision against the SAME \
         writer health the outgoing router did",
    );
    // Identity, not merely value: a replacement holding a DIFFERENT read that
    // happens to answer the same way would pass a value-only assertion and then
    // report a different writer's health as the reload aged.
    health.set(false);
    assert!(
        !replacement.capability_writes_durable(),
        "and it must be the same read -- a live change on the carried-over one \
         must be visible through the replacement",
    );
}

#[test]
fn the_test_only_builder_installs_a_read_that_reports_durable() {
    // The library-test opt-in. Its existence is what keeps the production default
    // and the library-test default the SAME value: a test that needs pre-flight to
    // fire says so through this builder rather than relying on a `cfg(test)` flip
    // that would make every test exercise a gate production never has.
    let router = bare_router().with_capability_writes_assumed_durable_for_tests();

    assert!(
        router.capability_writes_durable(),
        "the named opt-in must actually install a read reporting durable writes",
    );
    assert!(
        !bare_router().capability_writes_durable(),
        "and the un-opted default on the same constructor stays fail-safe, which \
         is what makes the opt-in a statement rather than a no-op",
    );
}
