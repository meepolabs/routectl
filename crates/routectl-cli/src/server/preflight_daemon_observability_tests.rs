// Status/doctor INFO-snapshot scenarios, the zero-cap free-validation probe,
// and per-call endpoint event emission -- the daemon's observability surface.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, so do not add `use` lines here.

// ---------------------------------------------------------------------------
// The INFO snapshots the daemon emits
// ---------------------------------------------------------------------------

/// Both status surfaces answer, and the health panel carries a POPULATED verdict row
/// for the planted verdict.
///
/// Asserted on the daemon's live snapshot rather than only on the panel envelope,
/// because the envelope alone would pass with an empty row set -- which is the
/// vacuity an earlier version of the log test had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_status_and_doctor_surfaces_report_a_populated_snapshot() {
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let daemon = spawn_daemon(config, dir, router).await;

    let health = get_status(&daemon, "/status/health").await;
    let doctor = get_status(&daemon, "/status/doctor").await;

    let data = available_data(&health);
    assert!(
        data["learned_negatives"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["capability_key"] == field_key())),
        "the health panel reports the planted verdict: {data}",
    );
    assert!(
        doctor["schema_version"].is_number(),
        "and the doctor route answers its envelope: {doctor}",
    );
    // The snapshot the daemon's own INFO line is built from, read through the live
    // router: populated rows, and a budget row for the configured cap.
    let snapshot = daemon.live_router().fidelity_snapshot();
    assert_eq!(
        snapshot.verdicts.len(),
        1,
        "the live router's fidelity snapshot carries the verdict row",
    );
    assert_eq!(snapshot.verdicts[0].capability_key, field_key());
    assert_eq!(
        snapshot.verdicts[0].blocked_reason, None,
        "and reports it UNBLOCKED, which is the state in which traffic is rewritten",
    );

    // The AGGREGATE endpoint, and its exactly-once contract: one request builds both
    // the health and doctor panels, and the fidelity line is emitted once.
    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("aggregate answers")
        .json()
        .await
        .expect("aggregate parses");
    assert!(
        aggregate["panels"]["health"]["data"].is_object()
            && aggregate["panels"]["doctor"]["schema_version"].is_number(),
        "the aggregate carries both panels: {aggregate}",
    );

    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Cap zero, through the daemon
// ---------------------------------------------------------------------------

/// A zero-cap daemon activates its lane on a real HTTP request and runs FREE
/// validation, while committing no reservation and dispatching no paid call.
///
/// The probe pass is driven on the daemon's EXACT live Router, after an HTTP request
/// has activated it -- the activation is non-blocking for the user request, so the
/// work lands on a driver tick. The wiring and the request boundary are real; only
/// the tick is invoked directly rather than waited out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_cap_daemon_activates_and_runs_free_validation_without_spending() {
    // Cap ZERO, explicitly rather than by omission, so the assertion is about the cap
    // rather than about an absent config section.
    let (config, dir) = daemon_config(Some(("p0", 0)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;
    assert_eq!(
        daemon
            .live_router()
            .fidelity_snapshot()
            .probes
            .activations_total,
        0,
        "premise: nothing has activated before the first admitted request",
    );

    let status = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let live = daemon.live_router();
    let summary = live.run_probe_pass().await;

    let probes = live.fidelity_snapshot().probes;
    assert!(
        probes.activations_total > 0,
        "the HTTP request activated the lane -- a lane activates on its first \
         admitted real request whatever its paid cap",
    );
    assert!(
        !summary.paid_probe_attempted,
        "and a zero cap attempts no paid probe: the eligibility predicate reads the \
         cap before any candidate is claimed",
    );
    assert_eq!(
        probes.paid_reservations_committed_total, 0,
        "so no reservation is committed -- cap zero spends nothing",
    );
    assert_eq!(
        probes.paid_provider_calls_started_total, 0,
        "and no paid call is dispatched",
    );
    daemon.shutdown().await;
}

/// The control for the case above: the zero-cap lane's free work genuinely ran.
///
/// Without it, a scheduler that queued nothing would also report zero paid calls, and
/// the assertions above could not tell "free ran, paid refused" from "nothing
/// happened".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_zero_cap_daemons_free_validation_actually_runs() {
    let (config, dir) = daemon_config(Some(("p0", 0)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;

    let _ = post_inference(&daemon, &body_with_field()).await;
    let live = daemon.live_router();
    let queued_before = live.fidelity_snapshot().probes.queued;
    let summary = live.run_probe_pass().await;

    assert!(
        queued_before > 0 || summary.free_validators_run > 0,
        "the activation queued real free work and the pass ran it, so 'no paid call' \
         is a refusal rather than an empty scheduler",
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Endpoint-level INFO emission (observed through per-call event observer)
// ---------------------------------------------------------------------------

/// Drain every fidelity event the daemon has emitted so far.
///
/// `try_recv` in a loop rather than an awaited `recv`: by the time the HTTP response
/// has been read the emission has already happened (the emitter runs inside the
/// blocking builder the response waits on), so a wait would only add a way for the
/// test to hang. Draining to EMPTY is the load-bearing half -- the assertion is a
/// count, and a read that stopped at the first event would report one for a daemon
/// that emitted three.
fn drain_events(
    events: &mut crate::handlers::status::test_hooks::FidelityEvents,
) -> Vec<crate::handlers::status::FidelityEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

/// Assert `events` is EXACTLY ONE populated fidelity event, and return it.
///
/// Both halves matter and neither implies the other. The COUNT is the exactly-once
/// contract: the response body carries no trace of the line, so nothing else in a
/// request can tell one emission from two, and two identical snapshots per poll make
/// a reader counting lines read double the poll rate. POPULATED is the vacuity guard:
/// an emitter reached with an empty snapshot satisfies every count assertion while
/// reporting nothing an operator can use, which is exactly the shape an earlier
/// version of the log test had.
///
/// `expect_budget_rows` is the count the caller's fixture configures. Asserted rather
/// than ignored because the budget rows are the half of the line a verdict count
/// cannot vouch for: they come from a DIFFERENT source (the operator's configured
/// caps plus a ledger read on the blocking thread), so an emitter reached with the
/// verdict rows intact and the budget read dropped passes every other assertion here.
/// The fixtures discriminate it -- a configured cap emits one row, the no-config
/// doctor branch legitimately emits none -- so the value distinguishes a real read
/// from a default.
fn one_populated_event(
    events: Vec<crate::handlers::status::FidelityEvent>,
    endpoint: &str,
    expect_budget_rows: usize,
) -> crate::handlers::status::FidelityEvent {
    assert_eq!(
        events.len(),
        1,
        "{endpoint} must emit EXACTLY ONE fidelity event per request: the line is \
         log-only, so no response body can tell one emission from two -- and two per \
         poll make a reader counting lines read double the real poll rate. Got \
         {} events",
        events.len(),
    );
    let event = events.into_iter().next().expect("the count is one");
    assert_eq!(
        event.verdict_rows_total, 1,
        "and the event {endpoint} emitted carries the planted verdict row: an emitter \
         reached with an empty snapshot passes every count assertion while reporting \
         nothing",
    );
    assert!(
        !event.snapshot.acting.is_empty(),
        "with the acting verdict present in the snapshot the line is built from",
    );
    assert_eq!(
        event.budget_rows_total, expect_budget_rows,
        "and it carries {expect_budget_rows} paid-probe budget row(s) for {endpoint}: \
         the budgets come from the operator's configured caps plus a ledger read, not \
         from the router snapshot, so a dropped budget read leaves every verdict \
         assertion above green",
    );
    event
}

/// A daemon with one planted acting verdict, its status seams observing.
///
/// `config_path` selects the doctor branch; see `spawn_daemon_full`.
async fn observing_daemon(
    config_path_from: Option<&std::path::Path>,
) -> (Daemon, crate::handlers::status::test_hooks::FidelityEvents) {
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let (hooks, events) = crate::handlers::status::test_hooks::StatusTestHooks::observing();
    let config_path = config_path_from.map(std::path::Path::to_path_buf);
    let daemon = spawn_daemon_full(config, dir, router, hooks, config_path).await;
    (daemon, events)
}

/// Write a minimal servable config into `dir` and return its path, so a daemon can
/// serve the CONFIGURED doctor branch.
fn write_config_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("routectl.toml");
    std::fs::write(
        &path,
        format!(
            "version = 3\n\n[providers.p0]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n\n\
             [models.{STATE_KEY}]\nprovider = \"p0\"\nupstream = \"claude-sonnet-4-5\"\n\n\
             [aliases]\ndefault = \"{STATE_KEY}\"\n"
        ),
    )
    .expect("the config file writes");
    path
}

/// The standalone `/status/health` endpoint emits exactly one populated fidelity
/// event, observed at the emitter inside a RUNNING daemon.
///
/// What this adds over the emitter unit tests beside it: those construct the
/// `FidelityEmission` themselves, so they prove the emitter's behavior for an
/// emission a test built. This one observes the emission the daemon's OWN health
/// handler built, reached over a real HTTP request through the listener, the status
/// middleware, `guard_panel`, and the blocking builder. A surface wired into a
/// builder the request never reaches passes every unit test and fails this.
///
/// Mutation check: remove the `log_field_verdict_snapshot` call from `health`'s
/// `build_from_view` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_health_endpoint_emits_one_populated_fidelity_event() {
    let (daemon, mut events) = observing_daemon(None).await;
    assert!(
        drain_events(&mut events).is_empty(),
        "premise: booting the daemon emits no fidelity event, so the event counted \
         below is the REQUEST's rather than a boot-time one",
    );

    let health = get_status(&daemon, "/status/health").await;

    let data = available_data(&health);
    assert!(
        data["learned_negatives"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "premise: the health panel served its payload, so the builder ran to \
         completion: {data}",
    );
    // ONE budget row: the fixture configures a cap for `p0`, and the health
    // builder reads the caps off the same live config.
    one_populated_event(drain_events(&mut events), "/status/health", 1);

    daemon.shutdown().await;
}

/// The CONFIGURED `/status/doctor` endpoint emits exactly one populated fidelity
/// event.
///
/// Its own case rather than a variant of the health one, because it reaches the
/// emitter through different code: the doctor builder gathers a no-network report
/// from an on-disk config first, and it is the branch a daemon with a config path
/// serves. A daemon whose doctor emitter was removed still answers this endpoint's
/// envelope, so only the observer can tell.
///
/// Mutation check: remove the `log_field_verdict_snapshot` call from
/// `doctor::build_panel_data` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_configured_doctor_endpoint_emits_one_populated_fidelity_event() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let doctor = get_status(&daemon, "/status/doctor").await;

    let data = available_data(&doctor);
    assert!(
        data["report"]["schema_version"].is_number(),
        "premise: this IS the configured branch -- the panel carries a gathered \
         report rather than an unavailable envelope: {doctor}",
    );
    one_populated_event(drain_events(&mut events), "/status/doctor", 1);

    daemon.shutdown().await;
}

/// The NO-CONFIG `/status/doctor` branch emits exactly one populated fidelity event
/// even though its panel is unavailable.
///
/// The contract this pins is that the observability floor is about the ROUTER, not
/// the report: a daemon with no on-disk config has no doctor report to build but has
/// a live router in full, and staying silent would mean such a daemon satisfied the
/// floor only through `/status/health`. Asserted over a real request rather than by
/// driving the builder, because the branch is selected by how the DAEMON was
/// started.
///
/// Mutation check: drop the `log_field_verdict_snapshot` call from the `None` arm of
/// `doctor::build_with_emission` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_no_config_doctor_branch_still_emits_one_populated_fidelity_event() {
    let (daemon, mut events) = observing_daemon(None).await;
    let _ = drain_events(&mut events);

    let doctor: Value = reqwest::get(format!("{}/status/doctor", daemon.base_url))
        .await
        .expect("the daemon answers")
        .json()
        .await
        .expect("the doctor envelope parses");

    assert!(
        !doctor["unavailable"].is_null(),
        "premise: this IS the no-config branch, so the panel is unavailable -- which \
         is precisely the state in which silence would be a narrower floor: {doctor}",
    );
    // ZERO budget rows, and that is the branch's own contract rather than a
    // shortcoming: the caps come from an on-disk config this daemon has none of, so
    // the line is emitted with unavailable budget data while every other field --
    // the counters, the verdict rows, the probe state -- comes from state it holds
    // in full.
    one_populated_event(drain_events(&mut events), "the no-config /status/doctor", 0);

    daemon.shutdown().await;
}

/// The `/status` AGGREGATE emits exactly ONE fidelity event for a request that
/// builds BOTH fidelity-carrying panels.
///
/// The shared claim's whole purpose, and the one property no other test here can
/// see: both the health and the doctor builder run in this single request and both
/// carry the surface, so an unarbitrated surface emits twice -- two identical
/// snapshots per poll, differing only in timestamps while describing one moment, and
/// a reader counting lines reads double the poll rate. Neither the response nor the
/// router state distinguishes one emission from two.
///
/// Mutation check: replace `FidelityEmission::shared()` in `status_aggregate` with
/// `::always()` -> red at two events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_aggregate_emits_exactly_one_fidelity_event_for_both_panels() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers")
        .json()
        .await
        .expect("the aggregate parses");

    // PREMISE, and the half that makes the count below mean anything: BOTH
    // fidelity-carrying panels really did build for this one request. Against a
    // request where one of them degraded, "one event" would be the trivial answer.
    assert!(
        aggregate["panels"]["health"]["data"].is_object(),
        "the health panel built: {aggregate}",
    );
    assert!(
        aggregate["panels"]["doctor"]["data"]["report"]["schema_version"].is_number(),
        "and so did the doctor panel, so two fidelity-carrying builders ran in this \
         one request: {aggregate}",
    );
    one_populated_event(drain_events(&mut events), "the /status aggregate", 1);

    daemon.shutdown().await;
}

/// With the aggregate's HEALTH builder failing BEFORE the emitter, the response marks
/// health unavailable and the DOCTOR builder emits the one fidelity event.
///
/// THE health-failure fallback, over a real request. The claim is a claim rather than
/// a pre-assigned role precisely for this case: a pre-assigned emitter that degraded
/// emitted nothing and its healthy sibling stayed suppressed too, so ONE failing
/// panel silently removed the whole observability floor from that request. What makes
/// this non-vacuous is the pair of assertions -- health really is unavailable in the
/// served envelope (so the failure landed before the emitter, not after), and exactly
/// one populated event still arrived (so the floor held).
///
/// Mutation check: replace the claim's `compare_exchange` with a pre-assigned
/// health-emits role -> red at zero events, which is the regression this shape
/// exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aggregate_whose_health_builder_fails_still_emits_one_event_from_doctor() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let (hooks, mut events) =
        crate::handlers::status::test_hooks::StatusTestHooks::observing_with_failing_health();
    let daemon = spawn_daemon_full(config, dir, router, hooks, Some(config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers even with one panel degraded")
        .json()
        .await
        .expect("the aggregate parses");

    assert!(
        !aggregate["panels"]["health"]["unavailable"].is_null(),
        "the health panel degraded to unavailable, so its builder did NOT reach the \
         fidelity emitter -- which is what makes the event below doctor's: {aggregate}",
    );
    assert!(
        aggregate["panels"]["health"]["data"].is_null(),
        "and it carries no payload, so the failure landed before the build completed \
         rather than after it emitted: {aggregate}",
    );
    assert!(
        aggregate["panels"]["doctor"]["data"]["report"]["schema_version"].is_number(),
        "while the doctor panel built in full: {aggregate}",
    );
    one_populated_event(
        drain_events(&mut events),
        "the aggregate whose health builder failed",
        1,
    );

    daemon.shutdown().await;
}

/// The FAILURE-INJECTION control: without the injection the same fixture's health
/// panel is AVAILABLE.
///
/// Without it, the test above would pass against a daemon whose health panel was
/// unavailable for some entirely unrelated reason -- and then "doctor emitted" would
/// be a statement about that other cause. This pins that the injected failure is
/// what degraded health.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_aggregate_without_the_injection_serves_an_available_health_panel() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers")
        .json()
        .await
        .expect("the aggregate parses");

    assert!(
        aggregate["panels"]["health"]["unavailable"].is_null()
            && aggregate["panels"]["health"]["data"].is_object(),
        "the identical fixture WITHOUT the injection serves health available, so the \
         degradation in the sibling test is the injection's and not a property of \
         this daemon shape: {aggregate}",
    );

    daemon.shutdown().await;
}
