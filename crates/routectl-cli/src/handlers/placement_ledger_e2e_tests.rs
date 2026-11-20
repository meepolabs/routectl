//! End-to-end placement records: requests through `ingress_handle` over a
//! sticky pool of login seats, read back from the usage writer's SQLite
//! database with `json_extract`, so the record is pinned to what the router
//! decided and what the writer stored.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_router::Router;
use routectl_router::config::{PoolEntry, SeatSelection};
use serde_json::Value;

use super::opening_rig::*;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

const SEATS: [&str; 3] = ["seat-a", "seat-b", "seat-c"];
const LONG: Duration = Duration::from_mins(10);

fn account(seat: &str) -> String {
    format!("pool-seat#{seat}")
}

fn rfc3339_in(from_now: Duration) -> String {
    (time::OffsetDateTime::now_utc() + from_now)
        .format(&time::format_description::well_known::Rfc3339)
        .expect("an RFC 3339 rendering")
}

/// Land a poll reading `seat` at `percent` of a five-hour window.
fn poll(router: &Router, seat: &str, percent: u32) {
    let body = format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{}"}}, "seven_day": null}}"#,
        rfc3339_in(Duration::from_hours(2))
    );
    let account = account(seat);
    let generation = router.acquire_credential_generation(&account);
    router
        .observe_polled_usage(
            &account,
            routectl_router::UsageVendor::Anthropic,
            body.as_bytes(),
            generation,
        )
        .expect("the poll is accepted");
}

/// A sticky three-seat pool of login seats behind `claude-opus`.
async fn pool_router(upstream: &Upstream) -> Router {
    let providers = SEATS
        .iter()
        .map(|seat| {
            (
                *seat,
                oauth_seat(upstream.base(), &format!("/{seat}"), seat),
            )
        })
        .collect();
    let mut cfg = config(
        providers,
        &[("opus", "pool", "claude-opus-4-7")],
        &[("claude-opus", &["opus"])],
    );
    cfg.pools.insert(
        "pool".to_string(),
        PoolEntry::new(SEATS.iter().map(|s| (*s).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    build(cfg).await
}

fn serve_every_seat(upstream: &Upstream) {
    for seat in SEATS {
        upstream.mount(
            &format!("/{seat}/v1/messages"),
            "",
            Script::sse(anthropic_stream(None, Some(InputUsage::plain(10)))),
        );
    }
}

/// A daemon with a ledger whose database the test reads with SQL.
struct Rig {
    state: Arc<AppState>,
    usage: routectl_usage::UsageHandle,
    _writer: parking_lot::Mutex<routectl_usage::UsageWriter>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(router: Router) -> Self {
        router.publish_probe_incarnation();
        let dir = tempfile::tempdir().expect("ledger dir");
        let path = dir.path().join("usage.db");
        let (usage, writer) =
            routectl_usage::UsageWriter::start(path.clone(), routectl_usage::CHANNEL_CAPACITY, 0);
        let state =
            AppState::for_test_with_usage(Arc::new(ArcSwap::from_pointee(router)), usage.clone());
        Self {
            state,
            usage,
            _writer: parking_lot::Mutex::new(writer),
            path,
            _dir: dir,
        }
    }

    fn router(&self) -> Arc<Router> {
        self.state.router.load_full()
    }

    /// One streamed turn under `request_id`, for `session` or keyless.
    async fn turn(&self, request_id: &str, session: Option<&str>) {
        let turn = send_as(
            &self.state,
            AnthropicIngress,
            session_headers(session),
            &body(&turn_one()),
            Some(request_id),
        )
        .await;
        assert_eq!(turn.status, axum::http::StatusCode::OK, "{}", turn.raw);
    }

    /// `json_extract(extra, path)` and the served seat of `request_id`'s row,
    /// once `rows` rows have landed.
    async fn read(&self, rows: u64, request_id: &str, path: &str) -> (Option<String>, String) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.usage.counters().persisted() < rows {
            assert!(Instant::now() < deadline, "{rows} usage rows never landed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let db = routectl_usage::open_readonly(&self.path).expect("open ledger");
        db.conn()
            .query_row(
                "SELECT json_extract(extra, ?1), seat FROM requests WHERE request_id = ?2",
                rusqlite::params![path, request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the row")
    }

    async fn placement(&self, rows: u64, request_id: &str) -> Option<Value> {
        let (text, _) = self.read(rows, request_id, "$.placement").await;
        text.map(|t| serde_json::from_str(&t).expect("placement json"))
    }
}

fn keys(record: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = record
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn a_keyed_birth_reaches_the_ledger_with_only_its_closed_fields() {
    // Arrange: seat-b paces best.
    let upstream = Upstream::start().await;
    serve_every_seat(&upstream);
    let router = pool_router(&upstream).await;
    poll(&router, "seat-a", 60);
    poll(&router, "seat-b", 10);
    let rig = Rig::new(router);

    // Act
    rig.turn("born", Some("s-birth")).await;

    // Assert
    let (event, seat) = rig.read(1, "born", "$.placement.event").await;
    let (arm, _) = rig.read(1, "born", "$.placement.arm").await;
    let record = rig.placement(1, "born").await.expect("a record");
    assert_eq!(event.as_deref(), Some("birth"));
    assert_eq!(arm.as_deref(), Some("roomy"));
    assert_eq!(record["rule"], "pace_v1");
    assert_eq!(record["seat"], account("seat-b").as_str());
    assert_eq!(
        seat,
        account("seat-b"),
        "the record names the requests.seat key"
    );
    assert_eq!(keys(&record), ["arm", "event", "rule", "seat"]);
}

#[tokio::test]
async fn the_record_is_kept_when_a_sibling_serves_after_the_chosen_seat_fails() {
    // Arrange: seat-b paces best and fails before any content.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/seat-b/v1/messages",
        "",
        Script::sse(anthropic_start(None) + &anthropic_overloaded()),
    );
    serve_every_seat(&upstream);
    let router = pool_router(&upstream).await;
    poll(&router, "seat-a", 60);
    poll(&router, "seat-b", 10);
    let rig = Rig::new(router);

    // Act
    rig.turn("fell-back", Some("s-fallback")).await;

    // Assert
    let record = rig.placement(1, "fell-back").await.expect("a record");
    let (_, served) = rig.read(1, "fell-back", "$.placement").await;
    assert_eq!(
        upstream.hits("/seat-b/v1/messages"),
        1,
        "premise: chosen first"
    );
    assert_ne!(served, account("seat-b"), "premise: a sibling served");
    assert_eq!(record["event"], "birth");
    assert_eq!(record["seat"], account("seat-b").as_str());
}

#[tokio::test]
async fn a_move_writes_its_trigger_and_a_warm_stay_writes_nothing() {
    // Arrange: a conversation born on seat-b.
    let upstream = Upstream::start().await;
    serve_every_seat(&upstream);
    let router = pool_router(&upstream).await;
    poll(&router, "seat-b", 10);
    let rig = Rig::new(router);
    rig.turn("born", Some("s-move")).await;

    // Act
    rig.turn("stayed", Some("s-move")).await;
    assert!(rig.router().force_open_breaker("opus#seat-b", LONG));
    rig.turn("moved", Some("s-move")).await;

    // Assert
    let stayed = rig.placement(3, "stayed").await;
    let moved = rig.placement(3, "moved").await.expect("a move record");
    let (trigger, served) = rig.read(3, "moved", "$.placement.trigger").await;
    assert_eq!(stayed, None, "a warm stay carries no record");
    assert_eq!(moved["event"], "move");
    assert_eq!(trigger.as_deref(), Some("breaker_open"));
    assert_eq!(moved["arm"], "unread", "the moved-to seat's tier");
    assert_eq!(moved["seat"], served.as_str());
    assert_eq!(keys(&moved), ["arm", "event", "rule", "seat", "trigger"]);
}

#[tokio::test]
async fn a_keyless_placement_writes_keyless() {
    // Arrange
    let upstream = Upstream::start().await;
    serve_every_seat(&upstream);
    let router = pool_router(&upstream).await;
    poll(&router, "seat-c", 10);
    let rig = Rig::new(router);

    // Act
    rig.turn("keyless", None).await;

    // Assert
    let (event, served) = rig.read(1, "keyless", "$.placement.event").await;
    assert_eq!(event.as_deref(), Some("keyless"));
    assert_eq!(served, account("seat-c"));
}
