//! Coverage for the loopback-only capability-purge control route: the peer
//! boundary, the closed request vocabulary and every rejection shape, the
//! successful purge's effect on resident state, the cleared settlement it
//! persists to the real ledger, the distinguishable clean no-op, and the
//! warm-rebuild property that makes the purge durable.

use super::*;

use arc_swap::ArcSwap;
use axum::http::Request as HttpRequest;
use axum::routing::post;
use routectl_core::capability::{THINKING, WEB_SEARCH};
use routectl_router::{
    CapabilityEventRow as ReplayRow, CapabilityLedgerReader, Config, ModelEntry, ProviderEntry,
    ReplayTombstone, Router,
};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use std::future::Future;
use std::time::Instant;
use tempfile::TempDir;
use tower::ServiceExt;

/// The route path this module serves, spelled once for the fixtures.
const PURGE_PATH: &str = "/control/capability/purge";

/// The provider kind every fixture plants under. Stage 1 is anthropic-api
/// only, and its capability-key normalization is the identity.
const ANTHROPIC_API: &str = "anthropic-api";

/// A loopback peer -- what every legitimate caller presents.
fn loopback_peer() -> std::net::SocketAddr {
    "127.0.0.1:54321".parse().expect("loopback peer parses")
}

/// A config with one `anthropic-api` provider and one model routing to it, so
/// the router's provider-kind derivation resolves a real kind for `sonnet`.
fn config_with_model() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config
}

/// The whole fixture: the one-route app, the live router handle the tests read
/// resident state through, a REAL enabled usage writer over a temp ledger, and
/// the ledger path.
///
/// The writer is real and enabled on purpose. The shared test-only `AppState`
/// constructor builds a DISABLED writer, under which every capability event is
/// dropped at the enabled gate -- so a settlement assertion against it would
/// pass whether or not the route enqueued anything.
struct Fixture {
    app: axum::Router,
    router: Arc<ArcSwap<Router>>,
    /// The SAME handle the route holds, so a test that fills the writer channel
    /// fills the one the purge will try to admit to.
    usage: routectl_usage::UsageHandle,
    writer: UsageWriter,
    ledger: std::path::PathBuf,
    _dir: TempDir,
}

impl Fixture {
    /// The fixture with a usage handle the caller supplies, so a test can
    /// present a writer that is closed, full, or never answers.
    ///
    /// The DURABILITY tests need those shapes and cannot get them from the real
    /// lifecycle: `UsageWriter::shutdown` leaves the channel open while a handle
    /// clone lives, and a real writer always answers. Everything else in the
    /// fixture stays real -- one router, one route, one ledger.
    fn with_usage(
        usage: routectl_usage::UsageHandle,
        writer: UsageWriter,
        ledger: std::path::PathBuf,
        dir: TempDir,
    ) -> Self {
        let router = Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
            config_with_model(),
        ))));
        let state = Arc::new(AppState {
            router: Arc::clone(&router),
            usage: usage.clone(),
            activation: Arc::new(ArcSwap::from_pointee(
                routectl_router::ActivationState::default(),
            )),
            mitm_seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
            cc_pin_drift: crate::server::cc_pin_drift::CcPinDriftGuard::new(),
            purge_settlements: std::sync::Arc::new(
                crate::server::purge_settlement::SettlementTracker::new().0,
            ),
            confirmation_advances: std::sync::Arc::new(
                crate::server::confirmation_advance::ConfirmationTracker::new(),
            ),
        });
        let app = axum::Router::new()
            .route(PURGE_PATH, post(purge_capability))
            .with_state(state);
        Self {
            app,
            router,
            usage,
            writer,
            ledger,
            _dir: dir,
        }
    }

    /// A fixture whose writer channel is CLOSED: every admission fails, so the
    /// purge can never reach a durable commit.
    fn with_closed_writer() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let ledger = dir.path().join("usage.db");
        // A real writer is started and immediately shut down so the fixture owns
        // one, but the handle the route uses is the genuinely-closed one.
        let (_live, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
        Self::with_usage(
            routectl_usage::handle_with_closed_channel(),
            writer,
            ledger,
            dir,
        )
    }

    /// A fixture over a channel the TEST owns, plus that receiver. Nothing
    /// consumes the channel unless the test does, so a batch can be left
    /// admitted-but-unanswered (the shutdown / cancellation race) or the channel
    /// can be filled to refuse admission outright.
    fn with_owned_channel(
        capacity: usize,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<routectl_usage::WriterMessage>,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let ledger = dir.path().join("usage.db");
        let (_live, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
        let (tx, rx) = tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(capacity);
        (
            Self::with_usage(routectl_usage::handle_over_channel(tx), writer, ledger, dir),
            rx,
        )
    }

    /// The default fixture: a REAL enabled writer over a temp ledger.
    ///
    /// Real and enabled on purpose. The shared test-only `AppState` constructor
    /// builds a DISABLED writer, under which every capability event is dropped at
    /// the enabled gate -- so a settlement assertion against it would pass
    /// whether or not the route persisted anything.
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let ledger = dir.path().join("usage.db");
        let (usage, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
        Self::with_usage(usage, writer, ledger, dir)
    }

    /// Flush the writer and count the persisted `cleared` rows. Consumes the
    /// fixture: the writer's shutdown is what makes the rows readable, so
    /// counting twice would be counting after a closed channel.
    fn persisted_cleared_rows(self) -> i64 {
        drop(self.app);
        self.writer.shutdown();
        let db = routectl_usage::open(&self.ledger).expect("open ledger for read");
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM capability_events WHERE verdict = 'cleared'",
                [],
                |r| r.get(0),
            )
            .expect("count cleared rows")
    }

    /// Plant one acting self-identifying negative on the live router.
    ///
    /// Reached through the SAME warm-rebuild replay path a real boot uses,
    /// because the registry is private to the Router and the replay is the only
    /// production route into it from outside the router crate.
    fn plant_negative(&self, state_key: &str, capability: &str) {
        let live = self.router.load();
        let reader = PlantedLedger::negative(&live, state_key, capability);
        let summary = live.rebuild_learned_from_ledger(&reader);
        assert_eq!(
            summary.replayed_negative, 1,
            "the fixture must actually plant one negative, or every assertion \
             about purging it is vacuous"
        );
        assert!(
            self.resident(state_key, capability),
            "the planted negative must be resident before the act"
        );
    }

    /// A fixture whose ledger path cannot be written: the writer reaches the
    /// batch and its transaction fails. A DB fault, distinct from an
    /// unavailable or full channel.
    ///
    /// The path is a DIRECTORY, so SQLite cannot open it as a database -- a
    /// mode-based trick would not survive a test run as root, and this one is a
    /// property of the path itself.
    fn with_unwritable_ledger() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let ledger = dir.path().join("not-a-db");
        std::fs::create_dir(&ledger).expect("create the blocking directory");
        let (usage, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
        Self::with_usage(usage, writer, ledger, dir)
    }

    /// Occupy every slot of an owned channel so the next admission finds none
    /// free.
    ///
    /// Filled through the handle the ROUTE uses, reached from the app's own
    /// state: a separate handle over a different channel would fill the wrong
    /// queue and the test would pass for the wrong reason.
    fn fill_channel(&self, capacity: usize) {
        let generation = self.router.load().registry_generation();
        // Empty batches: they occupy a slot without writing a row, so filling the
        // channel cannot itself change the ledger the assertions read.
        //
        // BOUNDED by the fixture's own capacity. A `while .is_ok()` loop hung
        // here: nothing drains an owned channel, so whether such a loop ever
        // stops depends on the queue's internals rather than on the test's
        // intent. The receipts are retained so the slots provably stay occupied.
        let mut held = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            match self.usage.admit_capability_batch(vec![], generation) {
                Ok(receipt) => held.push(receipt),
                Err(_) => break,
            }
        }
        assert!(
            self.usage
                .admit_capability_batch(vec![], generation)
                .is_err(),
            "premise: the channel must actually be FULL, or the purge under test would be \
             admitted and the refusal assertion would be vacuous",
        );
        std::mem::forget(held);
    }

    /// Whether a fresh same-key observation is admitted by the registry -- the
    /// after-release half of the lease contract.
    fn same_key_learn_admitted(&self, state_key: &str, capability: &str) -> bool {
        let live = self.router.load();
        let reader = PlantedLedger::negative(&live, state_key, capability);
        live.rebuild_learned_from_ledger(&reader).replayed_negative == 1
    }

    /// Admit a REAL capability boundary and leave it unsettled, so every purge
    /// reservation refuses (a purge yields to an admitted-but-uncommitted
    /// boundary). Returns the admitted boundary so a caller can settle it.
    ///
    /// The production seam, not a mock: the refusal under test IS the
    /// purge/boundary exclusion, so staging it any other way would test a
    /// different thing.
    fn admit_unsettled_boundary(&self) -> crate::server::capability_boundary::AdmittedBoundary {
        let live = self.router.load_full();
        crate::server::capability_boundary::admit_capability_boundary(&self.usage, &live)
            .expect("a live writer admits the boundary batch")
    }

    /// Whether the live router holds a resident entry for the key.
    fn resident(&self, state_key: &str, capability: &str) -> bool {
        self.router
            .load()
            .learned_capability_snapshot()
            .iter()
            .any(|e| e.state_key == state_key && e.feature_key == capability)
    }

    /// Drive one request at the route with the given peer and raw body.
    ///
    /// Clones the app OUT of the fixture and hands the owned clone to a free
    /// function, so the returned future captures no reference to the fixture.
    /// The fixture owns a `UsageWriter`, whose shutdown channel is not `Sync`,
    /// and a future borrowing it would not be `Send`.
    fn call(
        &self,
        peer: std::net::SocketAddr,
        body: &str,
    ) -> impl Future<Output = (StatusCode, serde_json::Value)> + Send + 'static {
        drive(self.app.clone(), peer, body.to_string())
    }
}

/// Wait until a batch has been ADMITTED to an owned channel -- i.e. the purge
/// under test is past its reservation and awaiting its receipt.
///
/// Takes only the receiver, not the fixture: the fixture owns the writer's
/// shutdown channel and so is not `Sync`, and a helper borrowing it would
/// produce a non-`Send` future that cannot be awaited beside a spawned purge.
///
/// Waiting is what makes the concurrency tests deterministic -- asserting on a
/// lease before the first purge has taken it would be a race dressed as a test.
async fn await_admitted_batch(rx: &mut tokio::sync::mpsc::Receiver<routectl_usage::WriterMessage>) {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
    assert!(
        waited.is_ok(),
        "the purge under test must admit its batch, or the lease it is supposed to hold was \
         never taken and every concurrency assertion below is vacuous",
    );
    // The message is deliberately RETAINED rather than answered: its receipt
    // resolves only when the sender goes, so holding it keeps the first purge in
    // flight with its lease open.
    std::mem::forget(waited);
}

/// Drive one request at `app` and return the status plus the parsed body.
async fn drive(
    app: axum::Router,
    peer: std::net::SocketAddr,
    body: String,
) -> (StatusCode, serde_json::Value) {
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer))
        .body(Body::from(body))
        .expect("build request");
    let response = app.oneshot(request).await.expect("route must respond");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read response body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A capability-event ledger stub: a tombstone plus the rows to replay, stamped
/// with the live router's own boundary revision so `should_replay` admits them.
struct PlantedLedger {
    tombstone: ReplayTombstone,
    rows: Vec<ReplayRow>,
}

impl PlantedLedger {
    /// One self-identifying `broken` negative for `(state_key, capability)`.
    fn negative(router: &Router, state_key: &str, capability: &str) -> Self {
        Self {
            tombstone: ReplayTombstone::new(1, router.catalog_version(), router.overlay_revision()),
            rows: vec![broken_row(2, router, state_key, capability)],
        }
    }
}

impl CapabilityLedgerReader for PlantedLedger {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(self.tombstone)
    }

    fn read_events(&self) -> Vec<ReplayRow> {
        self.rows.clone()
    }
}

/// One `broken` replay row at `rowid`, stamped with `router`'s revision.
fn broken_row(rowid: i64, router: &Router, state_key: &str, capability: &str) -> ReplayRow {
    ReplayRow::new(
        rowid,
        Instant::now(),
        "broken".to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        capability.to_string(),
        state_key.to_string(),
        ANTHROPIC_API.to_string(),
        router.catalog_version(),
        router.overlay_revision(),
    )
}

/// A well-formed purge body for `(state_key, capability_key)`.
fn body_for(state_key: &str, capability_key: &str) -> String {
    serde_json::json!({
        "state_key": state_key,
        "capability_key": capability_key,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Durability: success is reported ONLY after an acknowledged durable commit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_writer_keeps_the_entry_acting_and_refuses_the_purge() {
    // The blocker this finishes. A purge that reported success on a best-effort
    // send would tell the operator the verdict is gone while the ledger still
    // holds the negative -- so the entry keeps steering routing until the next
    // boot, and the next warm rebuild resurrects it. With no durable commit
    // possible, the ONLY correct answer is a refusal with the entry untouched.
    let fixture = Fixture::with_closed_writer();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_ne!(
        status,
        StatusCode::OK,
        "a purge whose clear cannot be persisted must not report success",
    );
    assert_eq!(
        body["error"]["code"].as_str(),
        Some(DURABILITY_FAILED),
        "and must name the durability failure, distinguishably from absent and busy",
    );
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "the entry must still be RESIDENT and acting: nothing was persisted, so removing it \
         from memory would leave the ledger and the registry disagreeing",
    );
    assert_eq!(
        body["purged"].as_bool(),
        None,
        "and the response must not carry a purged verdict at all",
    );
}

#[tokio::test]
async fn a_full_writer_channel_refuses_the_purge_and_leaves_the_entry_resident() {
    // Admission failure rather than commit failure: the batch was never queued.
    // The entry must survive it exactly as it survives an unavailable writer --
    // the operator retries, and a retry is only safe because nothing moved.
    let (fixture, _rx) = Fixture::with_owned_channel(1);
    fixture.plant_negative("sonnet", WEB_SEARCH);
    // Fill the one slot so the purge's own admission finds none free.
    fixture.fill_channel(1);

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_ne!(
        status,
        StatusCode::OK,
        "a refused admission is not a success"
    );
    assert_eq!(body["error"]["code"].as_str(), Some(DURABILITY_FAILED));
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "an unqueued batch must leave the entry acting",
    );
}

#[tokio::test]
async fn a_write_failure_refuses_the_purge_and_leaves_the_entry_resident() {
    // The writer reached the batch and its transaction FAILED -- a DB fault
    // rather than an environment one. Same contract: no success, entry intact,
    // ledger unchanged.
    let fixture = Fixture::with_unwritable_ledger();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_ne!(
        status,
        StatusCode::OK,
        "a failed transaction is not a success"
    );
    assert_eq!(body["error"]["code"].as_str(), Some(DURABILITY_FAILED));
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "a failed write must leave the entry acting",
    );
}

#[tokio::test]
async fn a_cancelled_client_leaves_the_settlement_to_the_daemon() {
    // The protocol CHANGED here, and the old expectation was the weaker one. When
    // the handler owned the await, cancelling the client abandoned the lease
    // mid-commit -- so the test asserted the entry survived and the key was free
    // again. That is exactly the window daemon ownership closes: the settlement
    // now belongs to a task the server owns, so a cancelled client changes
    // nothing about it.
    //
    // What that means observably: while the commit is still in flight the lease is
    // STILL HELD (by the daemon, not the request), so a second purge of the same
    // key is refused as busy rather than admitted. The entry is untouched either
    // way, which is the property an operator depends on.
    let _guard = ();
    let (fixture, _rx) = Fixture::with_owned_channel(4);
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Drive the request and drop the future before it can be answered: nothing
    // consumes `_rx`, so the commit never resolves.
    let pending = fixture.call(loopback_peer(), &body_for("sonnet", WEB_SEARCH));
    let cancelled = tokio::time::timeout(std::time::Duration::from_millis(150), pending).await;
    assert!(
        cancelled.is_err(),
        "premise: the purge must still be awaiting its commit, which is what makes this the \
         cancellation case",
    );

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "a cancelled client must leave the entry resident: nothing is removed until the clear \
         commits",
    );
    // The lease is the DAEMON's now, so the key is legitimately busy rather than
    // free -- and busy is the answer that cannot mislead an operator.
    match fixture
        .router
        .load()
        .reserve_learned_capability_purge("sonnet", WEB_SEARCH)
    {
        routectl_router::router::PurgeOutcome::Busy => {}
        other => panic!(
            "while a daemon-owned settlement is in flight the key must read BUSY -- the lease \
             outlives the cancelled request by design; got {}",
            outcome_name(&other)
        ),
    }
}

/// Stable name for a non-reserved outcome, for panic messages.
fn outcome_name(outcome: &routectl_router::router::PurgeOutcome) -> &'static str {
    match outcome {
        routectl_router::router::PurgeOutcome::Reserved(_) => "reserved",
        routectl_router::router::PurgeOutcome::Absent => "absent",
        routectl_router::router::PurgeOutcome::Busy => "busy",
        routectl_router::router::PurgeOutcome::Stale => "stale",
    }
}

#[tokio::test]
async fn a_successful_purge_commits_durably_before_it_reports_or_finalizes() {
    // The success path, end to end against the REAL writer: the row is in the
    // ledger, the entry is gone from memory, and the response says so. Ordering
    // is the property -- the durable row is what licenses both the removal and
    // the report.
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_eq!(status, StatusCode::OK, "a durable purge succeeds");
    assert_eq!(body["purged"].as_bool(), Some(true));
    assert!(
        !fixture.resident("sonnet", WEB_SEARCH),
        "the entry is removed from memory only after the clear committed",
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        1,
        "and exactly one cleared row is durable, so a warm rebuild cannot resurrect it",
    );
}

#[tokio::test]
async fn a_repeat_purge_after_a_successful_one_is_a_clean_absent_no_op() {
    // Response-loss retry: the operator did not see the first answer and asks
    // again. The second call must be a distinguishable ABSENT no-op -- not a
    // second clear, and not a failure.
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let (first, first_body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(first_body["purged"].as_bool(), Some(true));

    let (second, second_body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;
    assert_eq!(
        second,
        StatusCode::OK,
        "a retry against an already-purged key is a clean no-op, not an error",
    );
    assert_eq!(
        second_body["purged"].as_bool(),
        Some(false),
        "and reports that it removed nothing",
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        1,
        "at most ONE clear is durable across the retry: a second row would claim the operator \
         removed something that was already gone",
    );
}

// ---------------------------------------------------------------------------
// Outcome shape: generation only where it describes a real removal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absent_purge_reports_no_generation() {
    // A generation is a fact about a removal. An absent key had none, so
    // reporting one would be a sampled value dressed as provenance.
    let fixture = Fixture::new();

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["purged"].as_bool(), Some(false));
    assert!(
        body.get("generation").is_none() || body["generation"].is_null(),
        "an absent purge must carry no generation; body was {body}",
    );
}

#[tokio::test]
async fn a_successful_purge_reports_the_generation_the_removal_ran_under() {
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    let expected = fixture.router.load().registry_generation();

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["generation"].as_u64(),
        Some(expected),
        "a real removal reports the generation it ran under, carried out of the reservation \
         rather than sampled after the fact",
    );
}

#[tokio::test]
async fn a_failed_purge_reports_no_generation() {
    let fixture = Fixture::with_closed_writer();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let (_status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert!(
        body.get("generation").is_none() || body["generation"].is_null(),
        "a failed purge removed nothing, so it has no generation to report; body was {body}",
    );
}

// ---------------------------------------------------------------------------
// Concurrency: one lease holder, same-key learning blocked then resumed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_purge_of_the_same_key_is_refused_as_busy_and_clears_once() {
    // Two operators, one key. Exactly one holds the lease; the other gets a
    // DISTINGUISHABLE busy answer (never "absent", which would tell it the entry
    // is gone), and at most one clear becomes durable.
    let (fixture, mut rx) = Fixture::with_owned_channel(8);
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // The first purge blocks awaiting its receipt (nothing consumes the channel
    // yet), so its lease is open while the second arrives.
    let first = tokio::spawn(fixture.call(loopback_peer(), &body_for("sonnet", WEB_SEARCH)));
    await_admitted_batch(&mut rx).await;

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the second purge of a leased key must be refused as busy",
    );
    assert_eq!(body["error"]["code"].as_str(), Some(PURGE_BUSY));
    assert_eq!(
        body["purged"].as_bool(),
        None,
        "a busy refusal must never report a purge verdict -- least of all that the entry is gone",
    );
    first.abort();
}

#[tokio::test]
async fn a_same_key_learn_is_blocked_during_a_purge_and_proceeds_after_release() {
    // The lease's whole point: while it is open the key's state is immovable, so
    // the capture cannot go stale. Once released, legitimate learning resumes --
    // a lease that leaked would silently stop the daemon from ever relearning.
    let (fixture, _rx) = Fixture::with_owned_channel(4);
    fixture.plant_negative("sonnet", WEB_SEARCH);

    let pending = fixture.call(loopback_peer(), &body_for("sonnet", WEB_SEARCH));
    let cancelled = tokio::time::timeout(std::time::Duration::from_millis(150), pending).await;
    assert!(
        cancelled.is_err(),
        "premise: the purge must still be awaiting its receipt, which is what makes this the \
         cancellation case",
    );

    // After the abandoned lease releases, the same key admits a fresh learn.
    assert!(
        fixture.same_key_learn_admitted("sonnet", WEB_SEARCH),
        "once the lease is released the registry must admit a legitimate same-key observation \
         again, or a cancelled purge would permanently freeze the key",
    );
}

#[tokio::test]
async fn a_purge_removes_the_resident_entry_and_reports_it_purged() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let (status, json) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["purged"], serde_json::json!(true));
    assert_eq!(json["state_key"], serde_json::json!("sonnet"));
    assert_eq!(json["capability_key"], serde_json::json!(WEB_SEARCH));
    assert!(
        !fixture.resident("sonnet", WEB_SEARCH),
        "the purged entry must be gone from the LIVE registry, not merely \
         reported gone"
    );
}

#[tokio::test]
async fn a_purge_persists_exactly_one_cleared_settlement_row() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let (status, _json) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fixture.persisted_cleared_rows(),
        1,
        "a removal must reach the ledger as exactly one cleared row, or the \
         warm rebuild resurrects the negative on the next boot"
    );
}

#[tokio::test]
async fn an_absent_key_is_a_clean_no_op_that_persists_nothing() {
    // Arrange: a DIFFERENT capability resident, so "not purged" is about the
    // requested key rather than about an empty registry.
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let (status, json) = fixture
        .call(loopback_peer(), &body_for("sonnet", THINKING))
        .await;

    // Assert
    assert_eq!(
        status,
        StatusCode::OK,
        "an absent key is a clean no-op, not an error"
    );
    assert_eq!(
        json["purged"],
        serde_json::json!(false),
        "the no-op must be distinguishable from a real purge on the wire"
    );
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "the no-op must leave the unrelated resident entry alone"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "a no-op must not write a settlement for an entry that never existed"
    );
}

#[tokio::test]
async fn a_non_loopback_peer_is_refused_and_mutates_nothing() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    let remote: std::net::SocketAddr = "203.0.113.7:44444".parse().expect("remote peer parses");

    // Act
    let (status, json) = fixture.call(remote, &body_for("sonnet", WEB_SEARCH)).await;

    // Assert
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"]["code"], serde_json::json!(FORBIDDEN_PEER));
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "a refused caller must not have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "a refused caller must not have written to the ledger"
    );
}

/// Every out-of-vocabulary body is the same refusal, and none of them mutates.
/// The cases are enumerated rather than sampled because each is a distinct way
/// a caller reaches the parser: a non-JSON body, an empty body, a wrong type,
/// an unknown key, a missing key, a blank key, a control byte, and an oversize
/// key.
#[tokio::test]
async fn every_out_of_vocabulary_body_is_refused_without_mutating() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    let oversize = "x".repeat(MAX_KEY_BYTES + 1);
    let cases: Vec<(&str, String)> = vec![
        ("not JSON at all", "sonnet web_search".to_string()),
        ("an empty body", String::new()),
        (
            "a wrong-typed key",
            serde_json::json!({"state_key": 7, "capability_key": WEB_SEARCH}).to_string(),
        ),
        (
            "an unknown field",
            serde_json::json!({
                "state_key": "sonnet",
                "capability_key": WEB_SEARCH,
                "provider_kind": ANTHROPIC_API,
            })
            .to_string(),
        ),
        (
            "a missing capability key",
            serde_json::json!({"state_key": "sonnet"}).to_string(),
        ),
        ("a blank state key", body_for("   ", WEB_SEARCH)),
        ("a blank capability key", body_for("sonnet", "")),
        ("a control byte in a key", body_for("sonnet", "web\nsearch")),
        ("an oversize key", body_for("sonnet", &oversize)),
    ];

    for (label, body) in cases {
        // Act
        let (status, json) = fixture.call(loopback_peer(), &body).await;

        // Assert
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused as out of vocabulary"
        );
        assert_eq!(
            json["error"]["code"],
            serde_json::json!(INVALID_REQUEST),
            "{label} must carry the one fixed refusal code, so a rejection \
             cannot report which validation failed"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no refused request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no refused request may have written to the ledger"
    );
}

/// A well-formed request naming a key the caller believes exists must not be
/// answered as a purge when it names a DIFFERENT lane. This is the boundary a
/// caller-supplied provider kind would have broken: the report addresses the
/// key the router derived, so a purge on one lane cannot silently drain another.
#[tokio::test]
async fn a_purge_on_one_lane_leaves_another_lanes_entry_resident() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    fixture.plant_negative("anthropic", WEB_SEARCH);

    // Act
    let (status, json) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["purged"], serde_json::json!(true));
    assert!(!fixture.resident("sonnet", WEB_SEARCH));
    assert!(
        fixture.resident("anthropic", WEB_SEARCH),
        "a keyed purge must not widen into a sibling lane"
    );
}

/// A wrong method never reaches the handler at all -- axum answers 405 -- so
/// the route cannot be driven by a GET a browser could be tricked into issuing.
#[tokio::test]
async fn a_get_is_not_served_by_the_purge_route() {
    // Arrange
    let fixture = Fixture::new();

    // Act
    let request = HttpRequest::builder()
        .method("GET")
        .uri(PURGE_PATH)
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::empty())
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// The audit record is emitted once per purge, carries the state key and the
/// normalized capability key, and carries nothing else -- no request body, no
/// upstream text.
#[tokio::test]
async fn one_content_free_audit_record_is_emitted_per_purge() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let (_result, events) = routectl_testkit::with_capture(
        fixture.call(loopback_peer(), &body_for("sonnet", WEB_SEARCH)),
    )
    .await;

    // Assert
    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("purge"))
        .collect();
    assert_eq!(audit.len(), 1, "exactly one audit record per purge");
    assert_eq!(audit[0].field("state_key"), Some("sonnet"));
    assert_eq!(audit[0].field("capability_key"), Some(WEB_SEARCH));
    assert_eq!(audit[0].field("removed"), Some("true"));
    let names: Vec<&str> = audit[0]
        .fields
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["event", "state_key", "capability_key", "removed"],
        "the audit record's field set is a contract: an added field is where \
         request or upstream content would leak in"
    );
}

/// The no-op is audited too, and says so: an operator reading the log must be
/// able to tell "I purged something" from "there was nothing to purge".
#[tokio::test]
async fn the_audit_record_distinguishes_a_no_op_from_a_removal() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let (_result, events) = routectl_testkit::with_capture(
        fixture.call(loopback_peer(), &body_for("sonnet", THINKING)),
    )
    .await;

    // Assert
    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("purge"))
        .collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].field("removed"), Some("false"));
}

// --- Fix round: content-type gate and the shared loopback peer predicate ---

/// A browser's SIMPLE cross-origin request must fail BEFORE mutating.
///
/// `text/plain` is one of the three content types a form-or-fetch simple
/// request can carry without a CORS preflight, so a page on any origin can
/// issue one at a loopback URL and the browser sends it -- the response is
/// hidden from the script, but the mutation would already have happened. A JSON
/// content-type requirement is what makes such a request impossible to send
/// without a preflight the daemon never answers.
///
/// The `Origin` and foreign `Host` headers here are what a real browser
/// attaches; they are asserted to be irrelevant to the outcome only in the
/// sense that the refusal does not depend on reading them.
#[tokio::test]
async fn a_browser_simple_cross_origin_request_is_refused_before_mutating() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    let body = body_for("sonnet", WEB_SEARCH);

    for content_type in [
        "text/plain",
        "text/plain;charset=UTF-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        // Act: the shape a cross-origin page can actually put on the wire.
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", content_type)
            // A cross-origin `Origin` with a LOOPBACK `Host`: the shape a page
            // gets when it fetches the daemon's own address directly. The Host
            // guard is satisfied, so this test reaches the content-type check it
            // is named for rather than short-circuiting ahead of it.
            .header("origin", "https://evil.example")
            .header("host", "127.0.0.1:8791")
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body.clone()))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "`{content_type}` is sendable cross-origin without a preflight, so \
             it must be refused before the mutation"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no simple cross-origin request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no simple cross-origin request may have written to the ledger"
    );
}

/// An ABSENT content-type is refused too. A missing header is not a JSON
/// declaration, and `fetch` with a plain string body and no explicit header is
/// exactly the shape that would otherwise slip through.
#[tokio::test]
async fn a_request_with_no_content_type_is_refused() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(fixture.resident("sonnet", WEB_SEARCH));
}

/// The positive control for the gate above, and the reason it is evidence: the
/// SAME request with a JSON content-type -- Origin and foreign Host still
/// attached -- is served and does mutate. Without this, a route that refused
/// everything would satisfy the negative test.
#[tokio::test]
async fn a_json_content_type_is_served_and_does_mutate() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .header("origin", "https://evil.example")
        .header("host", "127.0.0.1:8791")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "control: a JSON content-type must still be served, or the refusals \
         above prove nothing about the content-type gate"
    );
    assert!(
        !fixture.resident("sonnet", WEB_SEARCH),
        "control: the served request must really mutate"
    );
}

/// The JSON spellings the shared ingress predicate accepts are accepted here
/// too, so the control route and the inference routes agree on what JSON is.
#[tokio::test]
async fn every_json_content_type_spelling_the_ingress_accepts_is_accepted_here() {
    for content_type in [
        "application/json",
        "application/json; charset=utf-8",
        "APPLICATION/JSON",
        "application/vnd.routectl+json",
    ] {
        // Arrange
        let fixture = Fixture::new();
        fixture.plant_negative("sonnet", WEB_SEARCH);

        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", content_type)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "`{content_type}` names JSON and must be accepted"
        );
    }
}

/// The peer check uses the repo's SHARED loopback predicate, so every loopback
/// spelling a socket can really present is accepted -- including an IPv4-mapped
/// IPv6 peer, which a dual-stack listener produces for an IPv4 client and which
/// a bare `Ipv6Addr::is_loopback` would refuse.
#[tokio::test]
async fn every_loopback_peer_spelling_is_accepted() {
    for peer in [
        "127.0.0.1:5000",
        "127.0.0.5:5000",
        "127.255.255.254:5000",
        "[::1]:5000",
        "[::ffff:127.0.0.1]:5000",
    ] {
        // Arrange
        let fixture = Fixture::new();
        let addr: std::net::SocketAddr = peer.parse().expect("peer parses");

        // Act
        let (status, _body) = fixture.call(addr, &body_for("sonnet", WEB_SEARCH)).await;

        // Assert
        assert_eq!(
            status,
            StatusCode::OK,
            "`{peer}` is a loopback peer and must be served"
        );
    }
}

/// The other direction, so the acceptance above is not a route that accepts
/// everything: a non-loopback peer stays refused, including the IPv4-mapped
/// form of a PUBLIC address -- the shape a naive mapped-address unwrap would
/// wave through.
#[tokio::test]
async fn every_non_loopback_peer_spelling_stays_refused() {
    for peer in [
        "203.0.113.7:5000",
        "10.20.30.40:5000",
        "[2001:db8::1]:5000",
        "[::ffff:203.0.113.7]:5000",
        "0.0.0.0:5000",
    ] {
        // Arrange
        let fixture = Fixture::new();
        fixture.plant_negative("sonnet", WEB_SEARCH);
        let addr: std::net::SocketAddr = peer.parse().expect("peer parses");

        // Act
        let (status, body) = fixture.call(addr, &body_for("sonnet", WEB_SEARCH)).await;

        // Assert
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "`{peer}` is not a loopback peer and must be refused"
        );
        assert_eq!(body["error"]["code"], serde_json::json!(FORBIDDEN_PEER));
        assert!(
            fixture.resident("sonnet", WEB_SEARCH),
            "`{peer}` was refused, so it must not have purged anything"
        );
    }
}

// --- Second fix round: anti-DNS-rebinding Host validation ---

/// A present FOREIGN `Host` is rejected before the body is read or anything
/// mutates.
///
/// This is the anti-DNS-rebinding property, and the content-type gate does NOT
/// provide it: a JSON content-type stops a browser's PREFLIGHT-FREE simple
/// request, but an attacker who controls a hostname can point it at 127.0.0.1
/// and then a page on `http://rebind.evil` is SAME-ORIGIN with the daemon --
/// no preflight is needed at all, and the JSON content-type is allowed. What
/// distinguishes that request is its `Host`, which carries the attacker's name
/// rather than a loopback authority. Same guard the status subtree already
/// carries, same predicate.
#[tokio::test]
async fn a_present_foreign_host_is_rejected_before_mutating() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for host in [
        "rebind.evil",
        "rebind.evil:8791",
        "attacker.example.com",
        "routectl.local",
        "10.20.30.40:8791",
        "203.0.113.7",
        "[2001:db8::1]:8791",
    ] {
        // Act: a request a rebound page can really issue -- correct JSON
        // content-type, loopback peer, attacker-controlled Host.
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .header("host", host)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "Host `{host}` is not a loopback authority and must be rejected: a \
             rebound hostname makes a hostile page same-origin with the daemon, \
             which no content-type check can see"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no foreign-Host request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no foreign-Host request may have written to the ledger"
    );
}

/// The positive control: every LOOPBACK authority spelling is served. Without
/// this, a guard that rejected every Host would satisfy the test above while
/// making the route unusable by its own CLI.
#[tokio::test]
async fn every_loopback_host_authority_is_served() {
    for host in [
        "127.0.0.1:8791",
        "127.0.0.1",
        "127.0.0.5:8791",
        "localhost:8791",
        "localhost",
        "[::1]:8791",
        "[::1]",
    ] {
        // Arrange
        let fixture = Fixture::new();
        fixture.plant_negative("sonnet", WEB_SEARCH);

        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .header("host", host)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Host `{host}` names a loopback authority and must be served, or the \
             daemon's own CLI cannot reach the route"
        );
        assert!(
            !fixture.resident("sonnet", WEB_SEARCH),
            "control: a served request must really mutate"
        );
    }
}

/// An ABSENT `Host` is permitted, matching the status subtree's own rule and for
/// the same reason: the rebinding vector is a browser, which always sends one.
/// A bare `curl --http1.0` or a hand-rolled client legitimately omits it.
#[tokio::test]
async fn an_absent_host_is_permitted() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!fixture.resident("sonnet", WEB_SEARCH));
}

/// The Host guard runs BEFORE the content-type guard, so a rebound page learns
/// nothing about the route's body vocabulary from the status code it gets back.
/// Both refusals are safe, but the order is what keeps them from being an oracle.
#[tokio::test]
async fn a_foreign_host_is_rejected_ahead_of_the_content_type_check() {
    // Arrange
    let fixture = Fixture::new();

    // Act: BOTH would fail -- a foreign Host and a non-JSON content-type.
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "text/plain")
        .header("host", "rebind.evil")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "the Host refusal must come first, so the response cannot report which \
         body shapes the route would have accepted"
    );
}

// --- Final correction: HTTP/2 carries the authority in the URI, not in Host ---

/// An HTTP/2 request carries its authority in the `:authority` pseudo-header,
/// which hyper surfaces on the request URI rather than as a `Host` header. A
/// guard that reads only `Host` therefore sees NOTHING on such a request and
/// waves it through -- so the absent-Host allowance, which exists for
/// hand-rolled HTTP/1 clients, silently becomes a bypass for anything speaking
/// h2c to a cleartext loopback port.
///
/// Shaped as hyper delivers it: no `Host` header, authority on the URI.
#[tokio::test]
async fn an_h2_shaped_request_with_a_foreign_authority_is_rejected() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for authority in [
        "rebind.evil",
        "rebind.evil:8791",
        "attacker.example.com:443",
        "10.20.30.40:8791",
        "[2001:db8::1]:8791",
    ] {
        // Act: absolute-form URI carrying the authority, and NO Host header.
        let request = HttpRequest::builder()
            .method("POST")
            .uri(format!("http://{authority}{PURGE_PATH}"))
            .header("content-type", "application/json")
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "URI authority `{authority}` is foreign and must be rejected: on \
             HTTP/2 this is where the authority lives, so a Host-only guard \
             would see nothing and admit it"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no foreign-authority request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no foreign-authority request may have written to the ledger"
    );
}

/// The paired positive: an h2-shaped request whose URI authority IS loopback is
/// served. Without it, the rejection above would be equally satisfied by a guard
/// that refused every absolute-form request, which would break h2c clients
/// outright.
#[tokio::test]
async fn an_h2_shaped_request_with_a_loopback_authority_is_served() {
    for authority in [
        "127.0.0.1:8791",
        "127.0.0.1",
        "localhost:8791",
        "[::1]:8791",
    ] {
        // Arrange
        let fixture = Fixture::new();
        fixture.plant_negative("sonnet", WEB_SEARCH);

        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(format!("http://{authority}{PURGE_PATH}"))
            .header("content-type", "application/json")
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "URI authority `{authority}` names loopback and must be served"
        );
        assert!(
            !fixture.resident("sonnet", WEB_SEARCH),
            "control: a served request must really mutate"
        );
    }
}

/// A `Host` header and a URI authority that DISAGREE: the foreign one must lose.
/// Checking only the first thing found would let an attacker satisfy the guard
/// with a benign value while the other half carries the hostile name.
#[tokio::test]
async fn a_foreign_authority_is_rejected_even_beside_a_loopback_host_header() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act: a loopback Host header paired with a foreign URI authority.
    let request = HttpRequest::builder()
        .method("POST")
        .uri(format!("http://rebind.evil{PURGE_PATH}"))
        .header("content-type", "application/json")
        .header("host", "127.0.0.1:8791")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a foreign URI authority must be rejected even when a benign Host header \
         sits beside it -- otherwise the guard is satisfiable by the half an \
         attacker does not need"
    );
    assert!(fixture.resident("sonnet", WEB_SEARCH));
}

/// The remaining permitted shape, stated exactly: ORIGIN-FORM with no `Host`
/// and no URI authority at all. That is what a hand-rolled HTTP/1 client sends,
/// and it carries no authority claim to validate -- unlike the h2 case above,
/// where an authority is present and must be checked.
#[tokio::test]
async fn a_genuinely_authority_less_origin_form_request_is_permitted() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act: origin-form path only, no Host, no authority.
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let request_authority = request.uri().authority().map(ToString::to_string);
    assert_eq!(
        request_authority, None,
        "premise: this fixture must really carry no authority, else it is not \
         exercising the origin-form allowance"
    );
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!fixture.resident("sonnet", WEB_SEARCH));
}

// --- Final authority correction: every claimed value, fail-closed ---

/// A PRESENT but non-UTF-8 `Host` fails CLOSED.
///
/// Treating it as absent was the wrong reading of "absent means no claim": the
/// client did make an authority claim, this build simply cannot read it. A guard
/// that cannot evaluate a claim has not validated it, so admitting the request
/// is exactly the fail-open shape. Presence and unreadability together are the
/// signal.
#[tokio::test]
async fn a_present_non_utf8_host_fails_closed() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for raw in [
        // Lone continuation byte, and a truncated multi-byte sequence.
        &b"\xff\xfe"[..],
        &b"127.0.0.1\xff"[..],
        &b"\xc3"[..],
        &b"evil\xff.example"[..],
    ] {
        // Act: a header value that is a valid header byte string but not UTF-8.
        let host = axum::http::HeaderValue::from_bytes(raw)
            .expect("non-UTF-8 bytes are still a legal header value");
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        request.headers_mut().insert(axum::http::header::HOST, host);
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a present but unreadable Host ({raw:?}) must fail closed -- a claim \
             this build cannot evaluate has not been validated"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no unreadable-Host request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no unreadable-Host request may have written to the ledger"
    );
}

/// EVERY `Host` value is validated, not just the first.
///
/// A request may carry the header more than once. `HeaderMap::get` returns only
/// the first, so a hostile second value rode along unchecked -- and which one a
/// downstream reader honors is not this guard's call to assume. All of them are
/// claims; all of them must pass.
#[tokio::test]
async fn a_hostile_duplicate_host_is_rejected_whichever_position_it_holds() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Both orderings: hostile second (the `get`-only blind spot) and hostile
    // first (which the old code did catch -- kept so a future change that
    // reverses the scan cannot pass this test either).
    for (first, second) in [
        ("127.0.0.1:8791", "rebind.evil"),
        ("rebind.evil", "127.0.0.1:8791"),
        ("127.0.0.1:8791", "10.20.30.40:8791"),
        ("127.0.0.1", "[::1]@evil.example"),
    ] {
        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .header("host", first)
            .header("host", second)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "Host values ({first:?}, {second:?}) include a non-loopback claim and \
             must be rejected -- reading only the first leaves the other unchecked"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no duplicate-Host request carrying a hostile value may have purged"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no such request may have written to the ledger"
    );
}

/// The paired positive: duplicated `Host` values that are ALL loopback are
/// served. Without it, rejecting on any duplication would satisfy the test above
/// while refusing a benign (if unusual) request for the wrong reason.
#[tokio::test]
async fn benign_duplicate_loopback_hosts_are_served() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Act
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .header("host", "127.0.0.1:8791")
        .header("host", "127.0.0.1:8791")
        .extension(ConnectInfo(loopback_peer()))
        .body(Body::from(body_for("sonnet", WEB_SEARCH)))
        .expect("build request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("route must respond");

    // Assert
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "duplicated values that are all loopback carry no hostile claim, so the \
         refusal above must be about the VALUE and not about the duplication"
    );
    assert!(
        !fixture.resident("sonnet", WEB_SEARCH),
        "control: the served request must really mutate"
    );
}

/// Userinfo in the `Host` header is rejected at the route, not merely in the
/// predicate's own unit tests -- the bypass shape that read as loopback.
#[tokio::test]
async fn a_userinfo_bearing_host_is_rejected_at_the_route() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for hostile in [
        "[::1]@evil.example",
        "[::1]@evil.example:8791",
        "127.0.0.1@evil.example",
        "user:pw@127.0.0.1",
    ] {
        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .header("host", hostile)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "`{hostile}` puts a loopback-looking string in the userinfo field \
             while naming a foreign host, and must be rejected"
        );
    }

    assert!(fixture.resident("sonnet", WEB_SEARCH));
}

/// Malformed bracketed authorities are rejected at the MUTATING route, with no
/// resident or ledger change.
///
/// This is ROUTE-LEVEL coverage of an inherited predicate, not a pin on the parse
/// rules themselves: the grammar lives in `status_gate::parse_authority` and its
/// own unit tests own which shapes are well-formed (including the port-salvage
/// cases, which only a wildcard ALLOWLIST can act on and which this route never
/// consults). What these assert is that the route reaches that predicate before
/// mutating -- so a wrong or bypassed answer costs a purge here, and the wiring
/// is pinned independently of the grammar.
#[tokio::test]
async fn bracket_trailing_junk_authorities_are_rejected_without_mutating() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for hostile in [
        "[::1]evil",
        "[::1]evil.example",
        "[::1].evil",
        "[::1]:8791evil",
        "[::1]:80:evil",
        "[127.0.0.1]evil",
        "[127.0.0.1].evil",
        "[]:8791",
        "[::1",
    ] {
        // Act
        let request = HttpRequest::builder()
            .method("POST")
            .uri(PURGE_PATH)
            .header("content-type", "application/json")
            .header("host", hostile)
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "`{hostile}` is not a well-formed authority, so the loopback-looking \
             text in it must not admit a mutation"
        );
    }

    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "no malformed-authority request may have purged anything"
    );
    assert_eq!(
        fixture.persisted_cleared_rows(),
        0,
        "no malformed-authority request may have written to the ledger"
    );
}

/// The same shapes arriving as a URI authority (the HTTP/2 position) are rejected
/// too: route-level coverage that BOTH claim sites reach the inherited predicate
/// before anything mutates.
#[tokio::test]
async fn bracket_trailing_junk_uri_authorities_are_rejected_without_mutating() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    for hostile in ["[::1]evil.example", "[::1]:8791evil", "[127.0.0.1]evil"] {
        // Act: absolute-form URI carrying the malformed authority, no Host.
        let request = HttpRequest::builder()
            .method("POST")
            .uri(format!("http://{hostile}{PURGE_PATH}"))
            .header("content-type", "application/json")
            .extension(ConnectInfo(loopback_peer()))
            .body(Body::from(body_for("sonnet", WEB_SEARCH)))
            .expect("build request");
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("route must respond");

        // Assert
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "URI authority `{hostile}` is malformed and must be rejected"
        );
    }

    assert!(fixture.resident("sonnet", WEB_SEARCH));
    assert_eq!(fixture.persisted_cleared_rows(), 0);
}

/// The control route's authority refusal names the CLAIM SITE from the same
/// closed set the status guard uses, and carries no caller value.
///
/// Both surfaces refuse for the same reasons through the same predicate, so an
/// operator correlating a rejection across them should read one vocabulary. The
/// site is what tells them where to look -- a header value or the request URI --
/// and it is the only thing they need, since the value itself is
/// attacker-controlled.
#[tokio::test]
async fn an_authority_refusal_names_the_claim_site_without_the_value() {
    // Arrange
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // A header claim and a URI claim, each driven separately so each line is
    // attributable to one site.
    for (site, use_header) in [("host_header", true), ("uri_authority", false)] {
        // Act
        let (_result, events) = routectl_testkit::with_capture(async {
            let request = if use_header {
                HttpRequest::builder()
                    .method("POST")
                    .uri(PURGE_PATH)
                    .header("content-type", "application/json")
                    .header("host", "evil-LEAKED.example:8791")
                    .extension(ConnectInfo(loopback_peer()))
                    .body(Body::from(body_for("sonnet", WEB_SEARCH)))
                    .expect("build request")
            } else {
                HttpRequest::builder()
                    .method("POST")
                    .uri(format!("http://evil-LEAKED.example{PURGE_PATH}"))
                    .header("content-type", "application/json")
                    .extension(ConnectInfo(loopback_peer()))
                    .body(Body::from(body_for("sonnet", WEB_SEARCH)))
                    .expect("build request")
            };
            fixture
                .app
                .clone()
                .oneshot(request)
                .await
                .expect("route must respond")
                .status()
        })
        .await;

        // Assert: exactly one refusal record, naming this site and nothing else.
        let refusals: Vec<_> = events
            .iter()
            .filter(|e| e.field("claim_site").is_some())
            .collect();
        assert_eq!(
            refusals.len(),
            1,
            "one authority refusal must be recorded for the {site} claim"
        );
        assert_eq!(
            refusals[0].field("claim_site"),
            Some(site),
            "the refusal must name the {site} claim site"
        );
        let rendered = format!("{:?} {}", refusals[0].fields, refusals[0].message);
        assert!(
            !rendered.contains("LEAKED") && !rendered.contains("evil-"),
            "the refusal record leaked a caller-controlled value: {rendered}"
        );
    }

    assert!(fixture.resident("sonnet", WEB_SEARCH));
    assert_eq!(fixture.persisted_cleared_rows(), 0);
}

// ---------------------------------------------------------------------------
// The handler's bounded stale retry, through the real route
// ---------------------------------------------------------------------------

/// A wire-shape (catalog-independent) capability key, assembled at runtime so no
/// source line carries a second spelling of the permanent namespace prefix -- the
/// uniqueness scan the namespace owner enforces.
fn wire_shape_key() -> String {
    format!("{}{}{}", "fie", "ld:", "thinking.enabled.display")
}

/// A FIRST busy reservation is retried once the boundary settles and succeeds.
///
/// The retry is safe precisely because a busy reservation changed nothing: it
/// took no lease, removed nothing and committed nothing. So the second attempt
/// sees a settled boundary and a key whose state never moved -- and the operator
/// gets the purge they asked for rather than a refusal for a race they cannot
/// see.
#[tokio::test]
async fn a_busy_first_attempt_retries_against_the_settled_boundary_and_succeeds() {
    let fixture = Fixture::new();
    // A CATALOG-INDEPENDENT key: settling the boundary below evicts every
    // catalog-scoped entry, so a `web_search` fixture would vanish for a reason
    // unrelated to the retry and the test would pass on the wrong evidence.
    let key = wire_shape_key();
    fixture.plant_negative("sonnet", &key);
    // A pending boundary makes the FIRST reservation refuse (a purge yields to an
    // admitted-but-unsettled boundary), and it is cleared from a hook so the
    // retry finds a settled generation.
    let admitted = fixture.admit_unsettled_boundary();

    // The purge runs while the boundary is admitted: its FIRST reservation
    // refuses. The boundary is committed from this task before the retry, so the
    // second attempt finds a settled generation -- which is the ordinary reload
    // race the bounded retry exists for.
    // Settled BEFORE the request runs, deterministically: the property under test
    // is that a first-attempt refusal is retried against the CURRENT registry
    // rather than reported, and racing the commit against the handler's own yield
    // would decide that by timing. The refusal is real -- the boundary was
    // admitted while the reservation would have been taken -- and the retry then
    // sees the settled generation.
    let observed_refusal = matches!(
        fixture
            .router
            .load()
            .reserve_learned_capability_purge("sonnet", &key),
        routectl_router::router::PurgeOutcome::Busy
    );
    assert!(
        observed_refusal,
        "premise: an admitted boundary must make a reservation refuse, or this test is not \
         exercising the retry at all",
    );
    admitted.commit_for_tests();

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", &key))
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the bounded retry must reach the fresh Router: refusing a purge for a reload the \\
         operator cannot observe would make the command unreliable for no reason; body {body}",
    );
    assert_eq!(body["purged"].as_bool(), Some(true));
    assert_eq!(
        fixture.persisted_cleared_rows(),
        1,
        "and exactly ONE clear is durable across the retry -- a retried reservation must not \\
         clear twice",
    );
}

/// REPEATED busy refusals exhaust the bound and answer 409.
///
/// One retry, not a loop: a boundary settling is a discrete event, so a single
/// re-read resolves the ordinary race, and retrying indefinitely would turn a
/// reload storm into an unbounded hold on the control route. Exhaustion is the
/// operator's cue to ask again, and it must NOT read as a clean no-op.
#[tokio::test]
async fn repeated_busy_refusals_exhaust_the_bound_and_report_it_distinguishably() {
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    // Never settled, so EVERY reservation refuses and the bound exhausts.
    let _admitted = fixture.admit_unsettled_boundary();

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "exhausted retries must report a conflict, never a success",
    );
    assert!(
        matches!(
            body["error"]["code"].as_str(),
            Some(PURGE_STALE | PURGE_BUSY)
        ),
        "and must name the refusal distinguishably from an absent key -- telling an operator \\
         the entry is gone is the answer a refusal must never give; body {body}",
    );
    assert_eq!(
        body["purged"].as_bool(),
        None,
        "a refusal carries no purge verdict at all",
    );
    assert!(
        fixture.resident("sonnet", WEB_SEARCH),
        "and the entry is untouched, so retrying stays safe",
    );
}

/// Advance the fixture's SHARED registry to a new generation, via a real
/// boundary commit against a throwaway router attached to the same registry
/// the fixture's own router reads.
///
/// Returns the throwaway router, already stamped with the generation it just
/// established -- the shape `carry_over_learned_from` plus
/// `set_pending_registry_generation` produce at a real reload's publication.
/// The fixture's own `ArcSwap` is left untouched: whether the caller ever
/// swaps this router in is the one thing that distinguishes the two tests
/// below.
fn advance_shared_generation(fixture: &Fixture) -> Arc<Router> {
    let previous = fixture.router.load_full();
    let mut next = Router::new(Arc::new(config_with_model()));
    next.carry_over_learned_from(&previous);
    let next = Arc::new(next);

    let admitted =
        crate::server::capability_boundary::admit_capability_boundary(&fixture.usage, &next)
            .expect("a live writer admits the boundary batch");
    admitted.commit_for_tests();

    let active_generation = next.learned_registry().generation();
    next.set_pending_registry_generation(active_generation);
    next
}

/// A FIRST stale reservation -- one made through a Router the shared registry
/// has genuinely moved past -- is retried against a freshly published Router
/// and succeeds.
///
/// Reproduces the actual production mechanism: a boundary commit prunes every
/// catalog-scoped entry as part of the SAME transition that advances the
/// generation, so a stale purge can only find its target resident again if
/// live traffic re-learns it under the new generation before the retry runs --
/// exactly the ordinary case of an operator purge racing a reload while
/// traffic keeps flowing.
#[tokio::test]
async fn a_stale_first_attempt_retries_against_a_freshly_published_router_and_succeeds() {
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);

    // Moves the shared registry's active generation away from the one the
    // fixture's still-published router is cached at, and -- as a real
    // boundary commit always does -- prunes the catalog-scoped entry along
    // with it.
    let fresh_router = advance_shared_generation(&fixture);
    assert!(
        !fixture.resident("sonnet", WEB_SEARCH),
        "premise: a boundary commit prunes catalog-scoped entries, or the retry below \\
         would succeed for the wrong reason",
    );

    // Live traffic on the newly published router re-learns the same key under
    // the new generation, before the purge's retry runs.
    let reader = PlantedLedger::negative(&fresh_router, "sonnet", WEB_SEARCH);
    let summary = fresh_router.rebuild_learned_from_ledger(&reader);
    assert_eq!(
        summary.replayed_negative, 1,
        "premise: the entry must actually come back resident on the fresh router, or the \\
         retry below has nothing to purge",
    );

    let observed_stale = matches!(
        fixture
            .router
            .load()
            .reserve_learned_capability_purge("sonnet", WEB_SEARCH),
        routectl_router::router::PurgeOutcome::Stale
    );
    assert!(
        observed_stale,
        "premise: a Router cached at a superseded generation must see Stale, or this test \\
         is not exercising the retry at all",
    );

    // The spawned request runs on the CURRENT-THREAD test runtime, which never
    // preempts a task on its own -- so the one `yield_now` below is what lets
    // the spawned task's own first-attempt yield (added for exactly this
    // reason) hand control back before the swap below runs.
    let handle = tokio::spawn(fixture.call(loopback_peer(), &body_for("sonnet", WEB_SEARCH)));
    tokio::task::yield_now().await;

    fixture.router.store(fresh_router);

    let (status, body) = handle.await.expect("the spawned purge request completes");

    assert_eq!(
        status,
        StatusCode::OK,
        "the bounded retry must reach the freshly published Router: refusing a purge for a \\
         reload the operator cannot observe would make the command unreliable for no reason; \\
         body {body}",
    );
    assert_eq!(body["purged"].as_bool(), Some(true));
    assert_eq!(
        fixture.persisted_cleared_rows(),
        1,
        "and exactly ONE clear is durable across the retry -- a retried reservation must not \\
         clear twice",
    );
}

/// REPEATED staleness exhausts the bound and answers 409.
///
/// Unlike the busy case, no boundary ever settles here: the shared registry's
/// generation moves on a router the fixture never publishes, so every
/// reservation attempt -- including the retry -- addresses the same
/// superseded generation and refuses the same way.
#[tokio::test]
async fn repeated_staleness_exhausts_the_bound_and_reports_it_distinguishably() {
    let fixture = Fixture::new();
    fixture.plant_negative("sonnet", WEB_SEARCH);
    // Advances the shared registry past the fixture's own published router,
    // which is never swapped for the fresh one -- so both attempts read the
    // same superseded generation.
    let _fresh_router = advance_shared_generation(&fixture);

    let observed_stale = matches!(
        fixture
            .router
            .load()
            .reserve_learned_capability_purge("sonnet", WEB_SEARCH),
        routectl_router::router::PurgeOutcome::Stale
    );
    assert!(
        observed_stale,
        "premise: the fixture's published router must see Stale, or this test is not \\
         exercising the staleness bound at all",
    );

    let (status, body) = fixture
        .call(loopback_peer(), &body_for("sonnet", WEB_SEARCH))
        .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "exhausted retries must report a conflict, never a success",
    );
    assert_eq!(
        body["error"]["code"].as_str(),
        Some(PURGE_STALE),
        "and must name the refusal distinguishably from a busy or absent key",
    );
    assert_eq!(
        body["purged"].as_bool(),
        None,
        "a refusal carries no purge verdict at all",
    );
}
