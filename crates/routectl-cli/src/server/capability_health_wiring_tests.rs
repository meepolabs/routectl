// Wiring: which Routers carry the capability-persistence health read, what the
// read reports as the writer's own counters move, and whether both rebuild paths
// keep it.
//
// Every health assertion drives a REAL usage writer over a REAL file-backed
// database, so what moves the counters is a genuine write rather than a hand-set
// number. The adapter reads the writer's own state, which is the whole reason it
// exists -- an assertion against a fabricated counter would be an assertion about
// this test's arithmetic.

use super::*;

use std::path::Path;

use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

use crate::server::test_support::live_usage_writer as live_writer;

/// A Router built the way the library and every non-daemon path builds one.
async fn built_router(config: &Arc<routectl_router::Config>) -> Router {
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(config.clone(), secrets)
        .await
        .expect("router build")
}

/// A config whose usage ledger is the given path.
fn config_at(db_path: &Path) -> Arc<routectl_router::Config> {
    let mut config = routectl_router::Config::default();
    config.usage.db_path = db_path.to_path_buf();
    Arc::new(config)
}

/// One usage ROW, for moving the usage-record counters without touching any
/// capability counter -- the exclusion the health predicate depends on.
fn usage_row() -> routectl_usage::UsageRecord {
    routectl_usage::UsageRecord {
        request_id: "health-exclusion".to_string(),
        ..Default::default()
    }
}

/// One capability event, for moving the writer's counters through a real write.
fn event(capability: &str) -> routectl_usage::CapabilityEvent {
    routectl_usage::CapabilityEvent {
        ts: 1_000,
        lane_key: "nick".to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: 8,
        overlay_revision: 1,
    }
}

#[tokio::test]
async fn an_uninstalled_router_reports_writes_as_not_guaranteed() {
    // THE FAIL-SAFE DEFAULT at the wiring level: a Router the ordinary builder
    // produced carries no health read, and reads as not guaranteed. So a library
    // embedder, a test, or a boot path that forgot the wiring suspends learned
    // pre-flight rather than permitting it.
    let (_dir, path, handle, writer) = live_writer();
    let router = built_router(&config_at(&path)).await;

    assert!(
        !router.capability_writes_durable_for_tests(),
        "a Router nobody installed a health read on must read as NOT guaranteed",
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn an_installed_router_reports_durable_on_a_healthy_writer() {
    // The positive control for the default above, on a writer that is genuinely
    // healthy: a real database, opened, with nothing having failed.
    let (_dir, path, handle, writer) = live_writer();
    let router = install_capability_health(built_router(&config_at(&path)).await, &handle);

    assert!(
        router.capability_writes_durable_for_tests(),
        "a healthy writer must report durable, or the gate would suspend \
         pre-flight on every daemon",
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn a_real_write_failure_turns_the_read_unhealthy() {
    // THE STORE-FAULT SIGNAL, driven by a real failure rather than a set counter: the
    // writer opens against a DIRECTORY where its database file must be, so it runs
    // degraded with no connection and every write it is handed fails.
    //
    // The read is unhealthy from the FIRST call, because the failed open already
    // moved the counter -- and that is the corrected behavior. An install-time
    // baseline forgave the open and answered durable until a SECOND failure arrived;
    // a boot whose store will not open is the strongest reason to keep verdicts off
    // the pre-flight path, not a reason to wait for confirmation.
    //
    // The healthy control lives in `an_installed_router_reports_durable_on_a_healthy_writer`
    // above, on a writer whose store DOES open -- it cannot live here, since this
    // fixture has already failed by construction.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let blocked = dir.path().join("usage.db");
    std::fs::create_dir(&blocked).expect("create the blocking directory");
    let (handle, writer) = UsageWriter::start(blocked.clone(), CHANNEL_CAPACITY, 0, true);
    // AWAITED, not assumed: the open runs on the writer's own thread, so the failure
    // it records is not visible the instant `start` returns. A test that read the
    // counter immediately would race the thread and pass or fail by scheduling.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().write_errors() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        handle.counters().write_errors() >= 1,
        "premise: a store that will not open records a failure at start",
    );

    let router = install_capability_health(built_router(&config_at(&blocked)).await, &handle);

    assert!(
        !router.capability_writes_durable_for_tests(),
        "a store fault that happened BEFORE the install must still suspend learned \
         pre-flight: a verdict made eligible now would rest on a row that is not on \
         disk, and no second failure is needed to notice the first",
    );

    // And a later write on the same writer keeps it unhealthy -- monotonic, so this
    // can only stay false.
    handle.try_send_capability_event_in_generation(event("web_search"), 1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().write_errors() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        handle.counters().write_errors() >= 2,
        "premise: the later write failed too",
    );
    assert!(
        !router.capability_writes_durable_for_tests(),
        "and it stays unhealthy: the counters are monotonic, so the gate never \
         self-clears within a process",
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn a_transient_pre_install_boot_write_failure_stays_unhealthy() {
    // THE CASE THE BASELINE FORGAVE, and the reason it was removed. A TRANSIENT
    // failure at boot -- one write that did not land, on a store that is otherwise
    // fine and never fails again -- must keep learned pre-flight suspended for the
    // life of the process.
    //
    // This is the boot tombstone's shape: the startup warm commits a boundary batch
    // BEFORE this adapter is installed, so a tombstone refused at the channel or
    // failed in its transaction is a pre-install failure on a healthy-looking writer.
    // Under an install-time baseline the very first health read answered DURABLE and
    // only a SECOND, later failure would have been noticed -- so a boot whose replay
    // boundary never landed would have gone on to pre-flight traffic against a
    // registry whose boundary is not where it thinks it is.
    //
    // The failure is made transient deliberately: exactly ONE refused capability
    // write before the install, and nothing whatsoever after it. So the only thing
    // that can make this assertion true is reading the counter against ZERO.
    //
    // Mutation check: restore install-time baselining (sample the counters in
    // `capability_health`) -> red here, while every other test in this file stays
    // green. That asymmetry is the point -- this is the only case that discriminates
    // the two designs.
    let (tx, _rx) = tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(1);
    let counters = Arc::new(routectl_usage::UsageCounters::default());
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));

    // ONE pre-install refusal: the slot is taken, then a second write is refused.
    // This stands in for the boot tombstone whose channel was full.
    handle.try_send_capability_event_in_generation(event("boot-tombstone"), 1);
    handle.try_send_capability_event_in_generation(event("boot-tombstone"), 1);
    assert_eq!(
        counters.capability_events_dropped_full(),
        1,
        "premise: exactly ONE capability write was refused, before any install",
    );

    let dir = tempfile::TempDir::new().expect("tempdir");
    let router = install_capability_health(
        built_router(&config_at(&dir.path().join("u.db"))).await,
        &handle,
    );

    assert!(
        !router.capability_writes_durable_for_tests(),
        "a SINGLE transient failure before the install must leave the read unhealthy \
         with no second failure to corroborate it -- which is only true if the \
         predicate reads the counters against zero rather than against an \
         install-time baseline",
    );
    assert_eq!(
        counters.capability_events_dropped_full(),
        1,
        "and nothing further failed: the verdict above rests on the ONE pre-install \
         refusal alone",
    );
}

#[tokio::test]
async fn a_refused_capability_event_also_turns_the_read_unhealthy() {
    // The BACK-PRESSURE signal, and it is not subsumed by the store fault: an event
    // the bounded channel refused never reaches the database at all, so its insert
    // never fails and the write-error counter never moves. For the router's question
    // -- can a write be expected to land -- a dropped event is as absent as a failed
    // one.
    //
    // Mutation check: drop the `capability_events_dropped_full` term from the
    // adapter's predicate -> red here while the store-fault test stays green.
    let (tx, _rx) = tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(1);
    let counters = Arc::new(routectl_usage::UsageCounters::default());
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
    let dir = tempfile::TempDir::new().expect("tempdir");
    let router = install_capability_health(
        built_router(&config_at(&dir.path().join("u.db"))).await,
        &handle,
    );
    assert!(
        router.capability_writes_durable_for_tests(),
        "control: nothing refused yet",
    );

    // Saturate the one-slot channel, then refuse.
    handle.try_send_capability_event_in_generation(event("a"), 1);
    handle.try_send_capability_event_in_generation(event("b"), 1);
    assert!(
        counters.capability_events_dropped_full() >= 1,
        "premise: the bounded channel must have refused an event",
    );
    assert_eq!(
        counters.capability_writer_unavailable(),
        0,
        "premise: a FULL channel is not a closed one -- the two are counted apart, \
         so this case must move only the full counter",
    );

    assert!(
        !router.capability_writes_durable_for_tests(),
        "a refused capability event must suspend learned pre-flight too -- it never \
         reached the database, so no verdict may become eligible on it",
    );
}

#[tokio::test]
async fn a_closed_writer_channel_turns_the_read_unhealthy_on_its_own_counter() {
    // The TEARDOWN signal, the third of three, and the one neither other covers: a
    // closed channel records no store fault (nothing reached the writer) and no
    // full-channel drop (the channel was not full, it was gone).
    //
    // Counted on its OWN counter rather than as a full-channel drop, which is what
    // this asserts in both directions: a shutting-down daemon must not be reported as
    // a saturated one, and health must still notice it.
    //
    // Mutation check: drop the `capability_writer_unavailable` term from the
    // predicate -> red here while the other two signal tests stay green. Count a
    // closed channel on the full counter instead -> the `dropped_full == 0`
    // assertion below reds.
    let handle = routectl_usage::handle_with_closed_channel();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let router = install_capability_health(
        built_router(&config_at(&dir.path().join("u.db"))).await,
        &handle,
    );
    assert!(
        router.capability_writes_durable_for_tests(),
        "control: nothing refused yet, even on a closed channel -- the counter moves \
         when a write is ATTEMPTED, not when the channel is created",
    );

    handle.try_send_capability_event_in_generation(event("a"), 1);

    assert_eq!(
        handle.counters().capability_writer_unavailable(),
        1,
        "a CLOSED channel lands on the unavailable counter",
    );
    assert_eq!(
        handle.counters().capability_events_dropped_full(),
        0,
        "and NOT on the full one: teardown and back-pressure are different operator \
         situations, so reporting a closed channel as full would misdirect whoever \
         reads it",
    );
    assert_eq!(
        handle.counters().write_errors(),
        0,
        "and no store fault was recorded, since nothing reached the writer",
    );
    assert!(
        !router.capability_writes_durable_for_tests(),
        "health must notice a closed writer: no capability write can land at all, so \
         no verdict may become pre-flight eligible",
    );
}

#[tokio::test]
async fn an_ordinary_usage_record_drop_does_not_suspend_preflight() {
    // THE EXCLUSION, asserted rather than assumed. A usage ROW the channel refused is
    // accounting telemetry falling behind; it says nothing about whether a capability
    // write would land, so folding it into health would suspend pre-flight on any
    // busy daemon for no safety gain.
    //
    // Mutation check: add `dropped_full()` (the usage-record counter) to the
    // predicate -> red here.
    let (tx, _rx) = tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(1);
    let counters = Arc::new(routectl_usage::UsageCounters::default());
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
    let dir = tempfile::TempDir::new().expect("tempdir");
    let router = install_capability_health(
        built_router(&config_at(&dir.path().join("u.db"))).await,
        &handle,
    );

    // Saturate with usage ROWS, not capability events, and refuse one.
    handle.try_send(usage_row());
    handle.try_send(usage_row());
    assert!(
        counters.dropped_full() >= 1,
        "premise: a usage RECORD must have been refused",
    );
    assert_eq!(
        counters.capability_events_dropped_full(),
        0,
        "premise: and no CAPABILITY write was refused",
    );

    assert!(
        router.capability_writes_durable_for_tests(),
        "an ordinary usage-record drop must NOT suspend learned pre-flight: it is \
         telemetry falling behind, not evidence about capability persistence",
    );
}

#[tokio::test]
async fn the_health_read_survives_a_rebuild() {
    // The reload half: both generations submit capability events to the SAME
    // writer, so its health is one fact. A replacement that lost the read would
    // suspend learned pre-flight on every reload until a boot path reinstalled it.
    let (_dir, path, handle, writer) = live_writer();
    let previous = install_capability_health(built_router(&config_at(&path)).await, &handle);
    let mut replacement = built_router(&config_at(&path)).await;
    assert!(
        !replacement.capability_writes_durable_for_tests(),
        "premise: a freshly-built replacement carries no read of its own",
    );

    replacement.carry_over_learned_from(&previous);

    assert!(
        replacement.capability_writes_durable_for_tests(),
        "the replacement must gate on the same writer health the outgoing router \
         did, or a reload would suspend pre-flight for no reason",
    );

    drop(handle);
    writer.shutdown();
}

#[test]
fn the_boot_installs_the_health_read_only_after_the_usage_writer_exists() {
    // A source-text guard over the boot sequence, for the same reason its sibling
    // in `paid_probe_ledger_wiring_tests.rs` is one: the ordering is a property of
    // the sequence rather than of any value, so installing against a not-yet-started
    // writer would compile and leave every behavioral test green. An ordered TRIPLE,
    // not mere presence -- presence alone would pass on exactly the swaps that break
    // it.
    let src = include_str!("serve.rs");
    let writer_at = src
        .find("build_usage_writer(&config)")
        .expect("the boot must start the usage writer");
    let install_at = src
        .find("install_capability_health(")
        .expect("the boot must install the capability-persistence health read");
    let publish_at = src
        .find("ArcSwap::from_pointee(router)")
        .expect("the boot must publish the Router behind its ArcSwap");
    assert!(
        writer_at < install_at,
        "the health read must be installed after the writer exists, or it baselines \
         against counters for a writer that is not there",
    );
    assert!(
        install_at < publish_at,
        "the health read must be installed while the Router is still owned, before \
         publication -- the installation is a consuming builder",
    );
}

// ---------------------------------------------------------------------------
// Every capability-write CLASS is observable to health, on both refusal reasons
// ---------------------------------------------------------------------------
//
// The three classes reach the writer by three different admissions: a best-effort
// event through `try_send_capability_event_at`, an acknowledged single event through
// `admit_acknowledged_capability_event`, and an acknowledged BATCH through
// `admit_capability_batch_at` -- which is the one a purge, a canary's durable clear,
// and the boot tombstone all take. A refused batch previously incremented NOTHING,
// so exactly the writes whose durability the gate exists to protect were the ones it
// could not see.
//
// Driven through the admission APIs the production callers use rather than through
// those callers' own wrappers, because what is under test is the accounting at the
// admission: `handlers::control`'s purge, `server::capability_rebuild`'s boot
// tombstone, and the canary clear all pass through one of these two batch entry
// points, and a per-caller test would prove the same fact three times while still
// missing a fourth caller.

/// The batch a purge admits: one `cleared` row for a key, at a purge floor.
fn purge_batch() -> Vec<routectl_usage::CapabilityEvent> {
    let mut cleared = event("thinking.enabled.display");
    cleared.verdict = "cleared".to_string();
    vec![cleared]
}

/// The batch the boot warm admits: a boundary tombstone.
fn tombstone_batch() -> Vec<routectl_usage::CapabilityEvent> {
    vec![routectl_usage::CapabilityEvent::tombstone(900, 8, 1)]
}

#[tokio::test]
async fn a_full_channel_refusal_of_any_capability_write_class_turns_the_read_unhealthy() {
    // EVERY class, on a FULL channel, each on its own fresh fixture so one class's
    // refusal cannot be credited to another's.
    //
    // The batch cases are the ones that were invisible: a purge whose batch was
    // refused, a canary clear whose batch was refused, and a boot tombstone whose
    // batch was refused all left the gate reporting durable.
    //
    // Mutation check: delete the `note_capability_refusal` call from
    // `admit_capability_batch_at` -> the two batch rows red; delete it from
    // `admit_acknowledged_capability_event` -> the acknowledged row reds; delete it
    // from `try_send_capability_event_at` -> the best-effort row reds.
    for (class, refuse) in capability_write_classes() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(1);
        let counters = Arc::new(routectl_usage::UsageCounters::default());
        let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
        let dir = tempfile::TempDir::new().expect("tempdir");
        let router = install_capability_health(
            built_router(&config_at(&dir.path().join("u.db"))).await,
            &handle,
        );
        assert!(
            router.capability_writes_durable_for_tests(),
            "{class}: control -- nothing refused yet",
        );

        // Take the one slot with a write of the SAME class, then refuse one.
        refuse(&handle);
        refuse(&handle);

        assert_eq!(
            counters.capability_events_dropped_full(),
            1,
            "{class}: a FULL channel must land on the full counter",
        );
        assert_eq!(
            counters.capability_writer_unavailable(),
            0,
            "{class}: and not on the unavailable one -- the channel was full, not closed",
        );
        assert!(
            !router.capability_writes_durable_for_tests(),
            "{class}: a refused capability write must suspend learned pre-flight, \
             whichever class it belongs to -- the row is not on disk either way",
        );
    }
}

#[tokio::test]
async fn a_closed_channel_refusal_of_any_capability_write_class_turns_the_read_unhealthy() {
    // The same sweep on a CLOSED channel: the teardown reason, counted on its own
    // counter for every class alike.
    //
    // Mutation check: count a closed channel as a full-channel drop -> the
    // `dropped_full == 0` assertion reds for every class.
    for (class, refuse) in capability_write_classes() {
        let handle = routectl_usage::handle_with_closed_channel();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let router = install_capability_health(
            built_router(&config_at(&dir.path().join("u.db"))).await,
            &handle,
        );
        assert!(
            router.capability_writes_durable_for_tests(),
            "{class}: control -- nothing attempted yet",
        );

        refuse(&handle);

        assert_eq!(
            handle.counters().capability_writer_unavailable(),
            1,
            "{class}: a CLOSED channel must land on the unavailable counter",
        );
        assert_eq!(
            handle.counters().capability_events_dropped_full(),
            0,
            "{class}: and NOT on the full one -- a shutting-down daemon must not be \
             reported as a saturated one",
        );
        assert!(
            !router.capability_writes_durable_for_tests(),
            "{class}: a closed writer means no capability write can land at all",
        );
    }
}

/// One refusal driver per capability-write CLASS, named.
///
/// A table rather than four copied test bodies, so a new class is added in one place
/// -- and so the two sweeps above (full and closed) provably cover the same set
/// rather than two lists somebody has to keep in step.
///
/// The batch entries are the ones that matter most: `purge` is what
/// `handlers::control` admits, `canary clear` is what a disproving canary's
/// settlement admits, and `boot tombstone` is what `server::capability_rebuild`
/// admits at startup, all through `admit_capability_batch_at`.
#[expect(
    clippy::type_complexity,
    reason = "a named table of per-class drivers reads better at the two call sites \
              than a trait or four duplicated test bodies would"
)]
fn capability_write_classes() -> Vec<(&'static str, Box<dyn Fn(&routectl_usage::UsageHandle)>)> {
    vec![
        (
            "best-effort event",
            Box::new(|handle: &routectl_usage::UsageHandle| {
                handle.try_send_capability_event_in_generation(event("web_search"), 1);
            }),
        ),
        (
            "acknowledged single event",
            Box::new(|handle: &routectl_usage::UsageHandle| {
                // The receipt is dropped: what is under test is the ADMISSION's
                // accounting, and an awaited outcome would additionally need a writer.
                let _ = handle.admit_acknowledged_capability_event(
                    event("thinking.enabled.display"),
                    1,
                    1,
                );
            }),
        ),
        (
            "purge batch",
            Box::new(|handle: &routectl_usage::UsageHandle| {
                let _ = handle.admit_capability_batch_at(purge_batch(), 1, 4);
            }),
        ),
        (
            "canary clear batch",
            Box::new(|handle: &routectl_usage::UsageHandle| {
                let _ = handle.admit_capability_batch(purge_batch(), 1);
            }),
        ),
        (
            "boot tombstone batch",
            Box::new(|handle: &routectl_usage::UsageHandle| {
                let _ = handle.admit_capability_batch(tombstone_batch(), 1);
            }),
        ),
    ]
}
