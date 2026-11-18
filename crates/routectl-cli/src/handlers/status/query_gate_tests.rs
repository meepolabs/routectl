// The ledger-read gate and request cancellation for `/status/query`: a held
// gate answers busy at once, a dropped request frees the gate, a spent budget
// never opens the ledger, and the live GET panels keep their own capacity.
// `include!`d into the `query_tests.rs` module; every import comes from there.

/// A status state over `db_path` whose query reads stall on their blocking
/// worker, holding the gate, until their cancel flag or deadline stops them.
fn stalling_state(db_path: PathBuf) -> Arc<StatusState> {
    let router = Router::new(Arc::new(Config::default()));
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let hooks = StatusTestHooks {
        stall_query_read: true,
        ..StatusTestHooks::default()
    };
    let mut status =
        StatusState::from_app(&app, None, DaemonMeta::for_test()).with_test_hooks(hooks);
    status.usage_db_path = db_path;
    Arc::new(status)
}

/// One GET through the real status router.
async fn get(state: Arc<StatusState>, uri: &str) -> Value {
    let app = super::super::status_router().with_state(state);
    let resp = app
        .oneshot(HttpRequest::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Start a query whose read stalls holding the gate, and return once it holds
/// it. The returned handle owns the request future.
async fn hold_gate_with_a_stalled_read(state: &Arc<StatusState>) -> tokio::task::JoinHandle<()> {
    let held = tokio::spawn({
        let state = Arc::clone(state);
        async move {
            send(state, QUERY_METHOD, VALID_BODY).await;
        }
    });
    let gate_taken = wait_until(Duration::from_secs(10), || {
        state.ledger_read.available_permits() == 0
    })
    .await;
    assert!(gate_taken, "the stalled read never took the gate");
    held
}

/// Poll `ready` until it holds or `bound` passes; `bound` is only a hang bound.
async fn wait_until(bound: Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    ready()
}

/// Comfortably under the query budget: an answer slower than this waited.
fn well_inside_the_budget() -> Duration {
    Duration::from_millis(QUERY_BUDGET_MS / 4)
}

/// Scheduler tolerance for an answer that must not wait at all: it covers a
/// loaded test runner's task scheduling, not any waiting the gate might do.
/// Far below the budget, so a gate that queued or waited for the held read
/// would overrun it.
const NO_WAIT_TOLERANCE: Duration = Duration::from_millis(250);

fn already_expired() -> ReadLimit {
    ReadLimit::from(
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("the process has been up for at least a millisecond"),
    )
}

fn cancelled_with_time_left() -> ReadLimit {
    let limit = ReadLimit::from(far_deadline());
    limit.cancel.cancel();
    limit
}

#[tokio::test]
async fn a_held_gate_answers_db_busy_at_once_and_health_stays_available() {
    // Arrange: one read holds the gate.
    let dir = TempDir::new().unwrap();
    let state = stalling_state(dir.path().join("usage.db"));
    let held = hold_gate_with_a_stalled_read(&state).await;

    // Act: a second query, given only scheduler tolerance to answer.
    let second = tokio::time::timeout(
        NO_WAIT_TOLERANCE,
        send(Arc::clone(&state), QUERY_METHOD, VALID_BODY),
    )
    .await;
    let health = get(Arc::clone(&state), "/status/health").await;
    held.abort();

    // Assert
    let (status, json) = second.expect("a held gate must answer without waiting");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["unavailable"], codes::DB_BUSY, "{json}");
    assert!(health["unavailable"].is_null(), "health: {health}");
}

#[tokio::test]
async fn live_get_panels_keep_every_builder_permit_while_a_query_reads() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let state = stalling_state(dir.path().join("usage.db"));
    let held = hold_gate_with_a_stalled_read(&state).await;

    // Act
    let free_builders = state.builder_capacity.available_permits();
    let aggregate = get(Arc::clone(&state), "/status").await;
    held.abort();

    // Assert
    assert_eq!(
        free_builders,
        crate::server::status_gate::STATUS_MAX_INFLIGHT,
        "a query read must not hold a GET panel's builder permit"
    );
    assert!(
        aggregate["panels"]["health"]["unavailable"].is_null(),
        "{aggregate}"
    );
}

#[tokio::test]
async fn dropping_a_request_mid_read_frees_the_gate_well_before_its_deadline() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let state = stalling_state(dir.path().join("usage.db"));
    let held = hold_gate_with_a_stalled_read(&state).await;

    // Act: the client goes away mid-read.
    let dropped_at = Instant::now();
    held.abort();
    assert!(
        held.await.is_err(),
        "the request future must have been dropped"
    );
    let freed = wait_until(Duration::from_secs(10), || {
        state.ledger_read.available_permits() == 1
    })
    .await;
    let took = dropped_at.elapsed();

    // Assert: freed by the cancel, not by the budget running out.
    assert!(freed, "the gate was never freed");
    assert!(
        took < well_inside_the_budget(),
        "the gate was freed {took:?} after the drop; the budget is {QUERY_BUDGET_MS} ms"
    );
}

#[test]
fn a_stopped_limit_answers_query_timeout_without_opening_the_ledger() {
    // A missing ledger answers `no_data` once it is opened, so `query_timeout`
    // shows the ledger was never touched.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("absent.db");
    let pricer = state_with_ledger(path.clone()).pricer().pin();
    let zone = crate::local_time::LocalZone::utc();
    let now = OffsetDateTime::now_utc();
    let cases = [
        (
            "control: time left, not cancelled",
            ReadLimit::from(far_deadline()),
            codes::NO_DATA,
        ),
        (
            "deadline already past",
            already_expired(),
            codes::QUERY_TIMEOUT,
        ),
        (
            "cancelled with time left",
            cancelled_with_time_left(),
            codes::QUERY_TIMEOUT,
        ),
    ];
    for (name, limit, expected) in cases {
        let (spec, bucket) = parse_body(VALID_BODY.as_bytes())
            .unwrap()
            .resolve(&zone, now);

        let panel = build_panel(
            &path,
            spec,
            bucket,
            &pricer,
            now_utc_rfc3339(),
            &zone,
            now,
            &limit,
        );

        assert_eq!(panel.unavailable.as_deref(), Some(expected), "{name}");
    }
}

#[tokio::test]
async fn busy_and_timeout_answers_land_on_the_detector_for_their_mode() {
    let aggregate = r#"{"window":"all","group_by":"model"}"#;
    let series = r#"{"window":"all","group_by":"model","bucket":"day"}"#;
    let cases = [
        ("busy, aggregate", aggregate, true, codes::DB_BUSY),
        ("busy, series", series, true, codes::DB_BUSY),
        ("timeout, aggregate", aggregate, false, codes::QUERY_TIMEOUT),
        ("timeout, series", series, false, codes::QUERY_TIMEOUT),
    ];
    for (name, body, gate_held, expected) in cases {
        // Arrange: a fresh state per case, so each detector starts unobserved.
        let dir = TempDir::new().unwrap();
        let state = state_with_ledger(dir.path().join("absent.db"));
        let parsed = parse_body(body.as_bytes()).unwrap();
        let is_series = parsed.bucket.is_some();
        let held = gate_held.then(|| state.ledger_read.try_acquire().unwrap());
        let limit = if gate_held {
            ReadLimit::from(far_deadline())
        } else {
            already_expired()
        };

        // Act
        let panel = build(&state, parsed, OffsetDateTime::now_utc(), limit).await;
        drop(held);

        // Assert
        let observability = &state.observability;
        let (hit, missed) = if is_series {
            (&observability.query_series, &observability.query)
        } else {
            (&observability.query, &observability.query_series)
        };
        assert_eq!(panel.unavailable.as_deref(), Some(expected), "{name}");
        assert_eq!(hit.shed_count.load(Ordering::Relaxed), 1, "{name}");
        assert!(
            hit.availability.load(Ordering::Relaxed) == Availability::Unavailable.as_u8(),
            "{name}: the edge was not recorded"
        );
        assert_eq!(missed.shed_count.load(Ordering::Relaxed), 0, "{name}");
        assert_eq!(state.ledger_read.available_permits(), 1, "{name}");
    }
}
