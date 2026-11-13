//! Tests for the cache-ignore driver: the pure transition function, and the
//! driver loop on paused time over a fixture ledger, counting captured log
//! lines.

use super::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use routectl_core::cache_decision::CALLER_SUPPLIED;
use routectl_core::provider_kind::config::ANTHROPIC_API;
use routectl_testkit::CapturedEvent;
use routectl_usage::{UsageDb, open};

use crate::test_usage_dir::usage_dir;

const NOW_MS: i64 = 10_000_000_000;
/// Fixture turns sit an hour before `NOW_MS`: inside the window ending at
/// now AND inside the window ending one interval earlier.
const TURN_BASE_MS: i64 = NOW_MS - 3_600_000;
const STEP_MS: i64 = 1_000;
const SESSION_ID: &str = "sess-driver-1";
const CHECKPOINT: &str = "test checkpoint";

// ---- pure transitions -------------------------------------------------

fn key(model: &str) -> LaneKey {
    LaneKey {
        provider: "direct".to_owned(),
        seat: None,
        model: Some(model.to_owned()),
        provider_kind: ANTHROPIC_API.to_owned(),
    }
}

const COUNTS: LaneCounts = LaneCounts {
    evidence_turns: 3,
    active_turns: 0,
    sessions: 1,
};

fn state(status: CacheIgnoreStatus, models: &[&str]) -> DriverState {
    DriverState {
        status,
        ignoring: models.iter().map(|m| (key(m), COUNTS)).collect(),
        others: BTreeMap::new(),
    }
}

const EVALUATED: CacheIgnoreStatus = CacheIgnoreStatus::Evaluated;
const LOCKED: CacheIgnoreStatus = CacheIgnoreStatus::Unavailable(UNAVAILABLE_DB);

#[test]
fn transitions_report_only_what_changed() {
    let rows: [(&str, DriverState, DriverState, Vec<Event>); 8] = [
        (
            "{} -> {L} warns once",
            state(EVALUATED, &[]),
            state(EVALUATED, &["l"]),
            vec![Event::LaneIgnoring {
                lane: key("l"),
                counts: COUNTS,
            }],
        ),
        (
            "{L} -> {L} is silent",
            state(EVALUATED, &["l"]),
            state(EVALUATED, &["l"]),
            vec![],
        ),
        (
            "{L} -> {} clears once",
            state(EVALUATED, &["l"]),
            state(EVALUATED, &[]),
            vec![Event::LaneCleared {
                lane: key("l"),
                verdict: LaneVerdict::Insufficient,
            }],
        ),
        (
            "{L} -> {L,M} warns for M only",
            state(EVALUATED, &["l"]),
            state(EVALUATED, &["l", "m"]),
            vec![Event::LaneIgnoring {
                lane: key("m"),
                counts: COUNTS,
            }],
        ),
        (
            "Evaluated -> Unavailable once",
            state(EVALUATED, &[]),
            state(LOCKED, &[]),
            vec![Event::Unavailable {
                class: UNAVAILABLE_DB,
            }],
        ),
        (
            "Unavailable -> Unavailable is silent",
            state(LOCKED, &[]),
            state(LOCKED, &[]),
            vec![],
        ),
        (
            "Unavailable -> Evaluated clears once",
            state(LOCKED, &[]),
            state(EVALUATED, &[]),
            vec![Event::Available {
                previous_class: UNAVAILABLE_DB,
            }],
        ),
        (
            "a new unavailable class is a new entry",
            state(LOCKED, &[]),
            state(CacheIgnoreStatus::Unavailable(UNAVAILABLE_NO_DATA), &[]),
            vec![Event::Unavailable {
                class: UNAVAILABLE_NO_DATA,
            }],
        ),
    ];
    for (name, prev, next, expected) in rows {
        assert_eq!(transitions(&prev, &next), expected, "row: {name}");
    }
}

#[test]
fn event_levels_warn_on_entry_and_inform_on_clear() {
    let rows: [(&str, Event, tracing::Level); 5] = [
        (
            "lane entering ignoring",
            Event::LaneIgnoring {
                lane: key("l"),
                counts: COUNTS,
            },
            tracing::Level::WARN,
        ),
        (
            "lane leaving ignoring",
            Event::LaneCleared {
                lane: key("l"),
                verdict: LaneVerdict::Clear,
            },
            tracing::Level::INFO,
        ),
        (
            "unreadable ledger",
            Event::Unavailable {
                class: UNAVAILABLE_DB,
            },
            tracing::Level::WARN,
        ),
        (
            "no ledger file yet",
            Event::Unavailable {
                class: UNAVAILABLE_NO_DATA,
            },
            tracing::Level::INFO,
        ),
        (
            "available again",
            Event::Available {
                previous_class: UNAVAILABLE_DB,
            },
            tracing::Level::INFO,
        ),
    ];
    for (name, event, level) in rows {
        assert_eq!(event.level(), level, "row: {name}");
    }
}

fn report_lane(model: &str, verdict: LaneVerdict) -> CacheIgnoreLaneReport {
    CacheIgnoreLaneReport {
        provider: "direct".to_owned(),
        seat: None,
        model: Some(model.to_owned()),
        provider_kind: ANTHROPIC_API.to_owned(),
        verdict,
        evidence_turns: 3,
        active_turns: i64::from(verdict == LaneVerdict::Clear),
        sessions: 1,
    }
}

fn report(
    status: CacheIgnoreStatus,
    truncated: bool,
    lanes: Vec<CacheIgnoreLaneReport>,
) -> CacheIgnoreReport {
    CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(NOW_MS),
        status,
        lanes,
        unobservable: Vec::new(),
        truncated,
    }
}

#[test]
fn a_lane_the_report_cannot_speak_for_keeps_its_verdict() {
    let prev = state(EVALUATED, &["l", "m"]);
    let rows: [(&str, CacheIgnoreReport, DriverState); 4] = [
        (
            "truncated report missing a lane: no clear",
            report(
                EVALUATED,
                true,
                vec![report_lane("m", LaneVerdict::Ignoring)],
            ),
            state(EVALUATED, &["l", "m"]),
        ),
        (
            "truncated report naming the lane clear: clears it",
            report(
                EVALUATED,
                true,
                vec![
                    report_lane("l", LaneVerdict::Clear),
                    report_lane("m", LaneVerdict::Ignoring),
                ],
            ),
            state(EVALUATED, &["m"]),
        ),
        (
            "complete report missing a lane: clears it",
            report(
                EVALUATED,
                false,
                vec![report_lane("m", LaneVerdict::Ignoring)],
            ),
            state(EVALUATED, &["m"]),
        ),
        (
            "unavailable report: lanes kept",
            report(LOCKED, false, vec![]),
            state(LOCKED, &["l", "m"]),
        ),
    ];
    for (name, next_report, expected) in rows {
        let got = prev.observe(&next_report);
        assert_eq!(
            (got.status, got.ignoring),
            (expected.status, expected.ignoring),
            "row: {name}"
        );
    }
}

#[test]
fn an_outage_between_two_ignoring_reads_logs_no_lane_line() {
    let ignoring = state(EVALUATED, &["l"]);
    let down = ignoring.observe(&report(LOCKED, false, vec![]));
    let back = down.observe(&report(
        EVALUATED,
        false,
        vec![report_lane("l", LaneVerdict::Ignoring)],
    ));

    assert_eq!(
        transitions(&ignoring, &down),
        vec![Event::Unavailable {
            class: UNAVAILABLE_DB
        }]
    );
    assert_eq!(
        transitions(&down, &back),
        vec![Event::Available {
            previous_class: UNAVAILABLE_DB
        }]
    );
}

// ---- driver on paused time -------------------------------------------

fn config_toml(db_path: &Path, with_direct: bool) -> Config {
    let direct = if with_direct {
        "[providers.direct]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://CACHE_IGNORE_DRIVER_TEST_UNUSED\"\n"
    } else {
        ""
    };
    let text = format!(
        "[usage]\ndb_path = {db}\n\
         [providers.other]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"env://CACHE_IGNORE_DRIVER_TEST_UNUSED\"\n\
         {direct}",
        db = toml::Value::String(db_path.display().to_string()),
    );
    toml::from_str(&text).expect("fixture config parses")
}

fn router_swap(db_path: &Path) -> Arc<ArcSwap<Router>> {
    Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(config_toml(
        db_path, true,
    )))))
}

fn fresh_ledger() -> (PathBuf, UsageDb) {
    let path = usage_dir().join("usage.db");
    let db = open(&path).expect("open ledger");
    (path, db)
}

fn insert_turn(db: &UsageDb, request_id: &str, model: &str, ts_start: i64, cache_read: i64) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, seat, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, cache_read, cache_write_tokens, \
             cache_front_decision, prefix_epoch_event, operation, probe) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, 'direct', ?4, NULL, ?5, 0, \
             'ok', 0, 0, 0, 1, 0, 5000, ?6, 0, ?7, 0, 'inference', 0)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                ANTHROPIC_API,
                SESSION_ID,
                cache_read,
                CALLER_SUPPLIED,
            ],
        )
        .expect("insert turn");
}

/// Four consecutive zero-cache turns: three evidence turns, so `ignoring`.
fn seed_ignoring_lane(db: &UsageDb, model: &str) {
    for i in 0..4 {
        insert_turn(
            db,
            &format!("{model}-{i}"),
            model,
            TURN_BASE_MS + i * STEP_MS,
            0,
        );
    }
}

/// One more turn on the lane that read from the cache, so `clear`.
fn add_active_turn(db: &UsageDb, model: &str) {
    insert_turn(
        db,
        &format!("{model}-active"),
        model,
        TURN_BASE_MS + 10 * STEP_MS,
        100,
    );
}

fn checkpoint(n: u32) {
    tracing::info!(checkpoint = n, "{CHECKPOINT}");
}

/// Sleep past the next driver tick (ticks land at whole intervals).
async fn past_next_tick() {
    tokio::time::sleep(CACHE_IGNORE_DRIVER_INTERVAL).await;
}

/// Run the driver beside `script` on this thread, then shut it down and
/// return every captured event. `script` starts after the boot evaluation
/// and sits half an interval off the tick grid.
async fn run_with<F: Future<Output = ()>>(
    swap: Arc<ArcSwap<Router>>,
    clock_ms: Arc<AtomicI64>,
    script: F,
) -> Vec<CapturedEvent> {
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = drive(swap, shutdown_rx, move || clock_ms.load(Ordering::SeqCst));
    let scripted = async move {
        tokio::time::sleep(CACHE_IGNORE_DRIVER_INTERVAL / 2).await;
        script.await;
        shutdown_tx.send(()).expect("driver still listening");
    };
    let (((), ()), events) =
        routectl_testkit::with_capture(async { tokio::join!(driver, scripted) }).await;
    events
}

fn is_driver(e: &CapturedEvent) -> bool {
    e.target == "routectl_cli::server::cache_ignore_driver"
}

/// The driver's events at INFO and above, split at each checkpoint.
fn segments(events: &[CapturedEvent]) -> Vec<Vec<CapturedEvent>> {
    let mut out = vec![Vec::new()];
    for e in events {
        if e.message == CHECKPOINT {
            out.push(Vec::new());
        } else if is_driver(e) && e.level <= tracing::Level::INFO {
            out.last_mut().expect("never empty").push(e.clone());
        }
    }
    out
}

fn named<'e>(events: &'e [CapturedEvent], event: &str) -> Vec<&'e CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("event") == Some(event))
        .collect()
}

fn warns(events: &[CapturedEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .count()
}

fn clock_at(ms: i64) -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(ms))
}

#[tokio::test(start_paused = true)]
async fn the_driver_warns_once_per_entering_lane_and_once_per_change() {
    // Arrange: an empty ledger at boot.
    let (path, db) = fresh_ledger();

    // Act
    let events = run_with(router_swap(&path), clock_at(NOW_MS), async {
        seed_ignoring_lane(&db, "m-a");
        seed_ignoring_lane(&db, "m-b");
        checkpoint(1);
        past_next_tick().await;
        checkpoint(2);
        past_next_tick().await;
        checkpoint(3);
        add_active_turn(&db, "m-a");
        past_next_tick().await;
    })
    .await;

    // Assert
    let seg = segments(&events);
    let [boot, first, identical, changed] = seg.as_slice() else {
        panic!("expected four segments, got {}", seg.len());
    };
    assert_eq!(
        named(boot, EVENT_BASELINE).len(),
        1,
        "one baseline line at boot"
    );
    assert_eq!(
        named(boot, EVENT_BASELINE)[0].field("lanes_ignoring"),
        Some("0")
    );
    assert_eq!(warns(boot), 0, "an empty ledger warns nothing at boot");

    let entering = named(first, EVENT_IGNORING);
    assert_eq!(entering.len(), 2, "one WARN per entering lane: {first:?}");
    assert_eq!(warns(first), 2);
    let a = entering
        .iter()
        .find(|e| e.field("model") == Some("m-a"))
        .expect("m-a warned");
    assert_eq!(a.level, tracing::Level::WARN);
    assert_eq!(a.message, "markers sent, no cache activity");
    let fields: Vec<(&str, Option<&str>)> = vec![
        ("provider", Some("direct")),
        ("evidence_turns", Some("3")),
        ("active_turns", Some("0")),
        ("sessions", Some("1")),
        ("window_hours", Some("6")),
        ("seat", None),
    ];
    for (name, value) in fields {
        assert_eq!(a.field(name), value, "field {name}");
    }

    assert!(
        identical.is_empty(),
        "an identical tick is silent: {identical:?}"
    );

    assert_eq!(changed.len(), 1, "one change line: {changed:?}");
    assert_eq!(changed[0].field("event"), Some(EVENT_IGNORING_CLEARED));
    assert_eq!(changed[0].level, tracing::Level::INFO);
    assert_eq!(changed[0].field("model"), Some("m-a"));
    assert_eq!(changed[0].field("verdict"), Some("clear"));

    for e in &events {
        for (name, value) in &e.fields {
            assert!(!value.contains(SESSION_ID), "{name} carries a session id");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_restarted_driver_logs_the_baseline_and_no_warn() {
    // Arrange: a first run sees the lane enter ignoring.
    let (path, db) = fresh_ledger();
    let first = run_with(router_swap(&path), clock_at(NOW_MS), async {
        seed_ignoring_lane(&db, "m-a");
        past_next_tick().await;
    })
    .await;
    assert_eq!(
        named(&first, EVENT_IGNORING).len(),
        1,
        "premise: the first run warned"
    );

    // Act: a second process on the same ledger, twenty minutes later.
    let second = run_with(router_swap(&path), clock_at(NOW_MS + 20 * 60_000), async {}).await;

    // Assert
    let seg = segments(&second);
    let baseline = named(&seg[0], EVENT_BASELINE);
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].level, tracing::Level::INFO);
    assert_eq!(baseline[0].field("lanes_ignoring"), Some("1"));
    assert_eq!(warns(&second), 0, "a restart repeats no WARN: {second:?}");
}

#[tokio::test(start_paused = true)]
async fn a_lane_that_started_ignoring_while_down_warns_at_boot() {
    // Arrange: every turn inside the last interval, so the baseline (one
    // interval ago) has not seen the lane.
    let (path, db) = fresh_ledger();
    for i in 0..4 {
        insert_turn(
            &db,
            &format!("t-{i}"),
            "m-a",
            NOW_MS - 60_000 + i * STEP_MS,
            0,
        );
    }

    // Act
    let events = run_with(router_swap(&path), clock_at(NOW_MS), async {}).await;

    // Assert
    let boot = &segments(&events)[0];
    assert_eq!(
        named(boot, EVENT_BASELINE)[0].field("lanes_ignoring"),
        Some("1")
    );
    assert_eq!(named(boot, EVENT_IGNORING).len(), 1, "{boot:?}");
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_ledger_warns_once_across_three_ticks() {
    // Arrange: a file that is not a SQLite database.
    let path = usage_dir().join("usage.db");
    std::fs::write(&path, b"not a database, just bytes long enough to read").unwrap();

    // Act
    let events = run_with(router_swap(&path), clock_at(NOW_MS), async {
        for _ in 0..3 {
            past_next_tick().await;
        }
    })
    .await;

    // Assert
    let evaluated = events
        .iter()
        .filter(|e| e.field("event") == Some(EVENT_EVALUATED))
        .count();
    assert_eq!(evaluated, 3, "premise: three ticks evaluated");
    let unavailable = named(&events, EVENT_UNAVAILABLE);
    assert_eq!(unavailable.len(), 1, "{events:?}");
    assert_eq!(unavailable[0].level, tracing::Level::WARN);
    assert_eq!(warns(&events), 1);
}

#[tokio::test(start_paused = true)]
async fn a_missing_ledger_is_reported_at_info() {
    let path = usage_dir().join("absent.db");

    let events = run_with(router_swap(&path), clock_at(NOW_MS), async {
        past_next_tick().await;
    })
    .await;

    let unavailable = named(&events, EVENT_UNAVAILABLE);
    assert_eq!(unavailable.len(), 1, "{events:?}");
    assert_eq!(unavailable[0].level, tracing::Level::INFO);
    assert_eq!(unavailable[0].field("class"), Some(UNAVAILABLE_NO_DATA));
    assert_eq!(warns(&events), 0);
}

#[tokio::test(start_paused = true)]
async fn a_hot_reload_changes_which_providers_are_evaluated() {
    // Arrange: the lane is ignoring from boot.
    let (path, db) = fresh_ledger();
    seed_ignoring_lane(&db, "m-a");
    let swap = router_swap(&path);
    let reloaded = Router::new(Arc::new(config_toml(&path, false)));

    // Act: publish a config without the provider between two ticks.
    let events = run_with(swap.clone(), clock_at(NOW_MS), async {
        checkpoint(1);
        swap.store(Arc::new(reloaded));
        past_next_tick().await;
    })
    .await;

    // Assert
    let seg = segments(&events);
    assert_eq!(
        named(&seg[0], EVENT_BASELINE)[0].field("lanes_ignoring"),
        Some("1")
    );
    assert_eq!(seg[1].len(), 1, "{:?}", seg[1]);
    assert_eq!(seg[1][0].field("event"), Some(EVENT_IGNORING_CLEARED));
}

/// Shutdown while an evaluation is blocked on a locked ledger: the driver
/// returns at once rather than waiting out the read's busy timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_an_evaluation_returns_promptly() {
    // Arrange: hold the ledger under an exclusive lock.
    let (path, db) = fresh_ledger();
    db.conn()
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
        .expect("take the exclusive lock");
    let probe_path = path.clone();
    let premise = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::task::spawn_blocking(move || open_readonly(&probe_path).map(|_| ())),
    )
    .await;
    assert!(
        premise.is_err(),
        "premise: a read of the locked ledger blocks"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(run_cache_ignore_driver(router_swap(&path), shutdown_rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !driver.is_finished(),
        "premise: the boot evaluation is in flight"
    );

    // Act
    let started = Instant::now();
    shutdown_tx.send(()).expect("driver listening");
    let joined = tokio::time::timeout(Duration::from_secs(1), driver).await;

    // Assert
    assert!(joined.is_ok(), "the driver did not return after shutdown");
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(db);
}
