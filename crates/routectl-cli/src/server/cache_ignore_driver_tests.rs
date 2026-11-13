//! Tests for the cache-ignore driver: the pure transition function, and the
//! driver loop on paused time over a fixture ledger, counting captured log
//! lines.

use super::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use routectl_core::cache_decision::CALLER_SUPPLIED;
use routectl_core::provider_kind::config::{ANTHROPIC_API, BEDROCK};
use routectl_testkit::CapturedEvent;
use routectl_usage::{UsageDb, open};

use crate::cache_ignore::{KEY_SHAPED_DIGITS, key_shaped_model};
use crate::test_usage_dir::usage_dir;

const NOW_MS: i64 = 10_000_000_000;
const HOUR_MS: i64 = 3_600_000;
/// Fixture turns sit an hour before `NOW_MS`, well inside the window.
const TURN_BASE_MS: i64 = NOW_MS - HOUR_MS;
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
        evaluated: BTreeSet::from(["direct".to_owned()]),
    }
}

const EVALUATED: CacheIgnoreStatus = CacheIgnoreStatus::Evaluated;
const LOCKED: CacheIgnoreStatus = CacheIgnoreStatus::Unavailable(UNAVAILABLE_DB);

#[test]
fn transitions_report_only_what_changed() {
    let rows: [(&str, DriverState, DriverState, Vec<Event>); 9] = [
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
                verdict: ClearedVerdict::Evaluated(LaneVerdict::Insufficient),
            }],
        ),
        (
            "{L} -> {} with the provider no longer evaluated clears as not evaluated",
            state(EVALUATED, &["l"]),
            DriverState {
                evaluated: BTreeSet::new(),
                ..state(EVALUATED, &[])
            },
            vec![Event::LaneCleared {
                lane: key("l"),
                verdict: ClearedVerdict::NotEvaluated,
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
                verdict: ClearedVerdict::Evaluated(LaneVerdict::Clear),
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
        evaluated: vec!["direct".to_owned()],
        not_evaluated: Vec::new(),
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

/// The `direct` provider (anthropic-api) when present and the `aws`
/// provider (bedrock) on `aws_shape`, beside an openai-responses provider
/// that is never evaluated.
struct Providers {
    direct: bool,
    aws_shape: &'static str,
}

const ALL_PROVIDERS: Providers = Providers {
    direct: true,
    aws_shape: "invoke",
};

fn config_with(db_path: &Path, providers: &Providers) -> Config {
    let direct = if providers.direct {
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
         [providers.aws]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         api_shape = \"{shape}\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         {direct}",
        db = toml::Value::String(db_path.display().to_string()),
        shape = providers.aws_shape,
    );
    toml::from_str(&text).expect("fixture config parses")
}

fn router_swap(db_path: &Path) -> Arc<ArcSwap<Router>> {
    swap_with(db_path, &ALL_PROVIDERS)
}

fn swap_with(db_path: &Path, providers: &Providers) -> Arc<ArcSwap<Router>> {
    Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(config_with(
        db_path, providers,
    )))))
}

fn fresh_ledger() -> (PathBuf, UsageDb) {
    let path = usage_dir().join("usage.db");
    let db = open(&path).expect("open ledger");
    (path, db)
}

fn insert_turn(db: &UsageDb, request_id: &str, model: &str, ts_start: i64, cache_read: i64) {
    insert_provider_turn(
        db,
        ("direct", ANTHROPIC_API),
        request_id,
        model,
        ts_start,
        cache_read,
    );
}

fn insert_provider_turn(
    db: &UsageDb,
    (provider, kind): (&str, &str),
    request_id: &str,
    model: &str,
    ts_start: i64,
    cache_read: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, seat, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, cache_read, cache_write_tokens, \
             cache_front_decision, prefix_epoch_event, operation, probe) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, ?4, ?5, NULL, ?6, 0, \
             'ok', 0, 0, 0, 1, 0, 5000, ?7, 0, ?8, 0, 'inference', 0)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider,
                kind,
                SESSION_ID,
                cache_read,
                CALLER_SUPPLIED,
            ],
        )
        .expect("insert turn");
}

/// Four consecutive zero-cache turns on `provider` from `base_ms`: three
/// evidence turns, so `ignoring`.
fn seed_lane_at(db: &UsageDb, provider: (&str, &str), model: &str, base_ms: i64) {
    for i in 0..4 {
        insert_provider_turn(
            db,
            provider,
            &format!("{}-{model}-{base_ms}-{i}", provider.0),
            model,
            base_ms + i * STEP_MS,
            0,
        );
    }
}

fn seed_ignoring_lane(db: &UsageDb, model: &str) {
    seed_lane_at(db, ("direct", ANTHROPIC_API), model, TURN_BASE_MS);
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

/// Run the driver over the ledger at `db_path` beside `script` on this
/// thread, then shut it down and return every captured event. `script`
/// starts after the boot evaluation and sits half an interval off the tick
/// grid.
async fn run_with<F: Future<Output = ()>>(
    swap: Arc<ArcSwap<Router>>,
    db_path: &Path,
    clock_ms: Arc<AtomicI64>,
    script: F,
) -> Vec<CapturedEvent> {
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = drive(
        swap,
        shutdown_rx,
        move || clock_ms.load(Ordering::SeqCst),
        ledger_read(db_path.to_path_buf()),
    );
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
    let events = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {
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

/// Boot one driver on `path` at `clock_ms`, let one tick pass after a
/// checkpoint, and return its two segments: boot, then that tick.
async fn boot_then_one_tick(
    path: &Path,
    clock_ms: i64,
) -> (Vec<CapturedEvent>, Vec<CapturedEvent>) {
    let events = run_with(router_swap(path), path, clock_at(clock_ms), async {
        checkpoint(1);
        past_next_tick().await;
    })
    .await;
    let seg = segments(&events);
    let [boot, tick] = seg.as_slice() else {
        panic!("expected two segments, got {}", seg.len());
    };
    (boot.clone(), tick.clone())
}

#[tokio::test(start_paused = true)]
async fn a_restart_re_announces_a_lane_still_ignoring_once() {
    // Arrange: a first run sees the lane enter ignoring, then stops.
    let (path, db) = fresh_ledger();
    let first = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {
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
    let (boot, tick) = boot_then_one_tick(&path, NOW_MS + 20 * 60_000).await;

    // Assert
    let baseline = named(&boot, EVENT_BASELINE);
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].level, tracing::Level::INFO);
    assert_eq!(baseline[0].field("lanes_ignoring"), Some("1"));
    let warned = named(&boot, EVENT_IGNORING);
    assert_eq!(
        warned.len(),
        1,
        "one WARN per lane still ignoring: {boot:?}"
    );
    assert_eq!(warned[0].field("model"), Some("m-a"));
    assert_eq!(warns(&boot), 1);
    assert!(
        tick.is_empty(),
        "an identical tick after boot is silent: {tick:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_lane_that_started_ignoring_during_a_long_downtime_warns_at_boot() {
    // Arrange: a first run on an empty ledger; the lane's turns land an hour
    // into a three-hour downtime.
    let (path, db) = fresh_ledger();
    let first = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {}).await;
    assert_eq!(
        warns(&first),
        0,
        "premise: nothing ignoring before the stop"
    );
    seed_lane_at(&db, ("direct", ANTHROPIC_API), "m-a", NOW_MS + HOUR_MS);

    // Act
    let (boot, tick) = boot_then_one_tick(&path, NOW_MS + 3 * HOUR_MS).await;

    // Assert
    assert_eq!(
        named(&boot, EVENT_BASELINE)[0].field("lanes_ignoring"),
        Some("1")
    );
    assert_eq!(named(&boot, EVENT_IGNORING).len(), 1, "{boot:?}");
    assert!(tick.is_empty(), "{tick:?}");
}

#[tokio::test(start_paused = true)]
async fn the_driver_logs_lane_identifiers_as_every_surface_renders_them() {
    // Arrange: a served model nickname carrying an account-shaped run.
    let (path, db) = fresh_ledger();
    let model = key_shaped_model();
    seed_ignoring_lane(&db, &model);
    let expected = LaneIdentifiers::render("direct", None, Some(&model));

    // Act
    let events = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {}).await;

    // Assert
    let warned = named(&events, EVENT_IGNORING);
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_eq!(warned[0].field("model"), expected.model.as_deref());
    assert_eq!(
        warned[0].field("provider"),
        Some(expected.provider.as_str())
    );
    for e in events.iter().filter(|e| is_driver(e)) {
        for (name, value) in &e.fields {
            assert!(
                !value.contains(KEY_SHAPED_DIGITS),
                "{name} carries the digits: {value}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_ledger_warns_once_across_three_ticks() {
    // Arrange: a file that is not a SQLite database.
    let path = usage_dir().join("usage.db");
    std::fs::write(&path, b"not a database, just bytes long enough to read").unwrap();

    // Act
    let events = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {
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

    let events = run_with(router_swap(&path), &path, clock_at(NOW_MS), async {
        past_next_tick().await;
    })
    .await;

    let unavailable = named(&events, EVENT_UNAVAILABLE);
    assert_eq!(unavailable.len(), 1, "{events:?}");
    assert_eq!(unavailable[0].level, tracing::Level::INFO);
    assert_eq!(unavailable[0].field("class"), Some(UNAVAILABLE_NO_DATA));
    assert_eq!(warns(&events), 0);
}

/// Boot on `before` with one ignoring lane on `lane_provider`, publish
/// `after` (its ledger path `after_db` when set), and return the driver's
/// lines from the next tick.
async fn tick_after_reload(
    lane_provider: (&str, &str),
    before: &Providers,
    after: &Providers,
    after_db: Option<&Path>,
) -> Vec<CapturedEvent> {
    let (path, db) = fresh_ledger();
    seed_lane_at(&db, lane_provider, "m-a", TURN_BASE_MS);
    let swap = swap_with(&path, before);
    let reloaded = Router::new(Arc::new(config_with(after_db.unwrap_or(&path), after)));
    let events = run_with(swap.clone(), &path, clock_at(NOW_MS), async {
        checkpoint(1);
        swap.store(Arc::new(reloaded));
        past_next_tick().await;
    })
    .await;
    let seg = segments(&events);
    assert_eq!(seg.len(), 2, "{events:?}");
    seg[1].clone()
}

/// What a tick after a reload logs: `(event, provider, verdict)`.
type ReloadLine<'a> = (&'a str, &'a str, Option<&'a str>);

/// (row, lane, config before, config after, reloaded ledger path, expected line)
type ReloadRow<'a> = (
    &'a str,
    (&'a str, &'a str),
    &'a Providers,
    &'a Providers,
    Option<&'a Path>,
    Option<ReloadLine<'a>>,
);

#[tokio::test(start_paused = true)]
async fn a_hot_reload_changes_which_providers_are_evaluated() {
    const DIRECT: (&str, &str) = ("direct", ANTHROPIC_API);
    const AWS: (&str, &str) = ("aws", BEDROCK);
    const CONVERSE: Providers = Providers {
        direct: true,
        aws_shape: "converse",
    };
    const NO_DIRECT: Providers = Providers {
        direct: false,
        aws_shape: "invoke",
    };
    let elsewhere = usage_dir().join("usage.db");
    let rows: [ReloadRow<'_>; 4] = [
        (
            "provider removed: cleared as not evaluated",
            DIRECT,
            &ALL_PROVIDERS,
            &NO_DIRECT,
            None,
            Some((EVENT_IGNORING_CLEARED, "direct", Some("not_evaluated"))),
        ),
        (
            "bedrock flips to converse: cleared as not evaluated",
            AWS,
            &ALL_PROVIDERS,
            &CONVERSE,
            None,
            Some((EVENT_IGNORING_CLEARED, "aws", Some("not_evaluated"))),
        ),
        (
            "bedrock flips to invoke: the lane is now evaluated and warns",
            AWS,
            &CONVERSE,
            &ALL_PROVIDERS,
            None,
            Some((EVENT_IGNORING, "aws", None)),
        ),
        (
            "a changed db_path is restart-required: the boot ledger is still read",
            DIRECT,
            &ALL_PROVIDERS,
            &ALL_PROVIDERS,
            Some(elsewhere.as_path()),
            None,
        ),
    ];

    for (name, lane, before, after, after_db, expected) in rows {
        let tick = tick_after_reload(lane, before, after, after_db).await;
        let got = tick
            .iter()
            .map(|e| {
                (
                    e.field("event").unwrap_or(""),
                    e.field("provider").unwrap_or(""),
                    e.field("verdict"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(got, expected.into_iter().collect::<Vec<_>>(), "row: {name}");
    }
}

/// A read that returns at once on every call but `blocked_call`, which
/// holds until the gate is dropped (or five seconds pass).
fn gated_read(
    blocked_call: usize,
    gate: std::sync::mpsc::Receiver<()>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> impl Fn(&Config, i64) -> CacheIgnoreReport + Send + Sync + 'static {
    let gate = std::sync::Mutex::new(gate);
    move |_config, _now_ms| {
        if calls.fetch_add(1, Ordering::SeqCst) == blocked_call {
            let _ = gate
                .lock()
                .expect("gate lock")
                .recv_timeout(Duration::from_secs(5));
        }
        report(EVALUATED, false, Vec::new())
    }
}

/// Shut the driver down while its `blocked_call`-th read is held and return
/// how long the driver took to return, in real time.
async fn shutdown_while_read_blocked(blocked_call: usize) -> Duration {
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(drive(
        router_swap(&usage_dir().join("absent.db")),
        shutdown_rx,
        || NOW_MS,
        gated_read(blocked_call, gate_rx, Arc::clone(&calls)),
    ));
    let mut advanced = false;
    let started_waiting = Instant::now();
    while calls.load(Ordering::SeqCst) <= blocked_call {
        assert!(
            started_waiting.elapsed() < Duration::from_secs(5),
            "the read never started"
        );
        if calls.load(Ordering::SeqCst) == blocked_call && !advanced && blocked_call > 0 {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(CACHE_IGNORE_DRIVER_INTERVAL).await;
            advanced = true;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !driver.is_finished(),
        "premise: the evaluation is in flight"
    );

    let started = Instant::now();
    shutdown_tx.send(()).expect("driver listening");
    driver.await.expect("driver task");
    let elapsed = started.elapsed();
    drop(gate_tx);
    elapsed
}

/// Shutdown while an evaluation is blocked, at boot and at a tick: the
/// driver returns at once rather than waiting out the read.
#[tokio::test(start_paused = true)]
async fn shutdown_during_an_evaluation_returns_promptly() {
    for (name, blocked_call) in [("boot evaluation", 0), ("tick evaluation", 1)] {
        let elapsed = shutdown_while_read_blocked(blocked_call).await;
        assert!(
            elapsed < Duration::from_secs(1),
            "row {name}: took {elapsed:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicked_evaluation_logs_one_error_and_reads_as_unavailable() {
    // Arrange
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let read = |_config: &Config, _now_ms: i64| -> CacheIgnoreReport {
        panic!("evaluation fault");
    };
    let driver = drive(
        router_swap(&usage_dir().join("absent.db")),
        shutdown_rx,
        || NOW_MS,
        read,
    );
    let scripted = async move {
        tokio::time::sleep(CACHE_IGNORE_DRIVER_INTERVAL / 2).await;
        shutdown_tx.send(()).expect("driver listening");
    };

    // Act
    let (((), ()), events) =
        routectl_testkit::with_capture(async { tokio::join!(driver, scripted) }).await;

    // Assert
    let panicked = named(&events, EVENT_PANICKED);
    assert_eq!(panicked.len(), 1, "{events:?}");
    assert_eq!(panicked[0].level, tracing::Level::ERROR);
    assert_eq!(
        named(&events, EVENT_UNAVAILABLE)[0].field("class"),
        Some(UNAVAILABLE_DB)
    );
}
