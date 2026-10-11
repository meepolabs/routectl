// The probe-clear acknowledgement through an ASSEMBLED daemon: a re-check probe
// the upstream accepts writes one durable `cleared` row through the daemon's own
// writer, and a warm rebuild from that ledger does not bring the negative back.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, so do not add `use` lines here.

// ---------------------------------------------------------------------------
// A resolved re-check clears DURABLY, across a router rebuild
// ---------------------------------------------------------------------------

/// The capability key of the count-validated re-check row, assembled from parts
/// for the same reason as [`field_key`].
fn recheck_key() -> String {
    format!(
        "{}{}",
        format_args!("{}{}", "fie", "ld:"),
        routectl_router::TEST_RECHECK_PATH
    )
}

/// Plant the re-check negative on the daemon's live router and commit its live
/// `broken` row through the daemon's OWN writer, at the stamps the plant took.
///
/// Planted at ZERO confirmations, so pre-flight never applies it and no canary
/// can clear it on the request path: the probe is the only thing that can. The
/// ledger row is what makes the negative DURABLE -- the plant writes memory only,
/// so without it a warm rebuild that found the key absent would prove nothing.
async fn plant_durable_recheck_negative(daemon: &Daemon) {
    let live = daemon.live_router();
    plant_acting_field_verdict_for_tests(&live, STATE_KEY, routectl_router::TEST_RECHECK_PATH, 0);
    let (generation, incarnation) = routectl_router::field_verdict_event_stamps_for_tests(
        &live,
        STATE_KEY,
        routectl_router::TEST_RECHECK_PATH,
    );
    let row = routectl_usage::CapabilityEvent {
        ts: crate::server::ledger_reader::epoch_ms_now(),
        lane_key: STATE_KEY.to_string(),
        capability: recheck_key(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: i64::from(live.catalog_version()),
        overlay_revision: i64::try_from(live.overlay_revision()).expect("small revision"),
        provider_kind: Some("anthropic-api".to_string()),
        vocab_version: Some(routectl_router::CURRENT_VOCAB_VERSION),
    };
    let usage = daemon.usage().clone();
    let committed = tokio::task::spawn_blocking(move || {
        usage.commit_capability_events_blocking_at(vec![row], generation, incarnation)
    })
    .await
    .expect("seed thread");
    assert!(
        matches!(committed, routectl_usage::BatchCommit::Committed { .. }),
        "premise: the negative's row must land: {committed:?}",
    );
}

/// Whether a FRESH Router warmed from the ledger finds the re-check negative
/// resident -- the state the next boot would start from.
///
/// `usage` is only the writer the warm would commit a fail-closed tombstone
/// through; a matching tombstone commits nothing.
async fn resident_after_warm_rebuild(
    config: &Arc<Config>,
    usage: &routectl_usage::UsageHandle,
) -> bool {
    let config = Arc::clone(config);
    let usage = usage.clone();
    tokio::task::spawn_blocking(move || {
        let router = Router::new(Arc::clone(&config));
        crate::server::capability_rebuild::warm_capability_registry_from_ledger(
            &config.usage.db_path,
            &router,
            &usage,
        );
        router
            .learned_capability_snapshot()
            .iter()
            .any(|entry| entry.state_key == STATE_KEY && entry.feature_key == recheck_key())
    })
    .await
    .expect("warm thread")
}

/// Every `capability_events` verdict recorded for the re-check key, in order.
fn recheck_ledger_verdicts(db_path: &std::path::Path) -> Vec<String> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT verdict FROM capability_events \
             WHERE lane_key = ?1 AND capability = ?2 ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([STATE_KEY, recheck_key().as_str()], |r| {
        r.get::<_, String>(0)
    })
    .expect("query")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("rows")
}

/// The `source` of every `cleared` row recorded for the re-check key, in order.
fn recheck_cleared_sources(db_path: &std::path::Path) -> Vec<String> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT source FROM capability_events \
             WHERE lane_key = ?1 AND capability = ?2 AND verdict = 'cleared' \
             ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([STATE_KEY, recheck_key().as_str()], |r| {
        r.get::<_, String>(0)
    })
    .expect("query")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("rows")
}

/// Whether the daemon's live router has the re-check negative acting.
fn recheck_acting(router: &Router) -> bool {
    router
        .fidelity_snapshot()
        .acting
        .iter()
        .any(|row| row.state_key == STATE_KEY && row.feature_key == recheck_key())
}

/// THE SPAN: a durable negative, a router REBUILD, an HTTP
/// request that queues a re-check, a free validation the upstream accepts, one
/// durable `cleared` row in the daemon's ledger, and a warm rebuild that finds
/// the negative gone.
///
/// The rebuild sits between boot and the probe on purpose: the acknowledgement is
/// installed at boot only, so a replacement that did not carry it would leave
/// the clear unacknowledged and the negative acting.
///
/// Mutation checks, each red here:
///   - remove the `install_probe_clear_ack` call from the serve boot -> the job
///     is never resolved and no row lands;
///   - drop the acknowledgement from `Router::carry_over_learned_from` -> same,
///     on the rebuilt router;
///   - make the warm rebuild skip probe-sourced field `cleared` rows -> the
///     probe's clear is skipped and the negative is resident again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolved_recheck_writes_one_cleared_row_and_a_warm_rebuild_keeps_it_cleared() {
    let (config, dir) = daemon_config();
    let db_path = config.usage.db_path.clone();
    let recorder = Arc::new(Recorder::default());
    let daemon = spawn_daemon(
        Arc::clone(&config),
        dir,
        router_with(&config, Arc::clone(&recorder)),
    )
    .await;
    plant_durable_recheck_negative(&daemon).await;
    assert!(
        recheck_acting(&daemon.live_router()),
        "premise: the planted negative acts",
    );
    assert!(
        resident_after_warm_rebuild(&config, daemon.usage()).await,
        "premise, and the load-bearing one: a warm rebuild finds the negative \
         BEFORE the probe, so its absence afterwards is evidence the clear landed",
    );

    // A ROUTER REBUILD, the way a reload performs one: a fresh build, the
    // carry-over from the published router, and the shared publication step.
    let mut rebuilt = router_with(&config, Arc::clone(&recorder));
    rebuilt.carry_over_learned_from(&daemon.live_router());
    crate::server::router_publish::publish_router(&daemon.router, Arc::new(rebuilt));
    assert!(
        recheck_acting(&daemon.live_router()),
        "premise: the rebuilt router carries the negative",
    );

    let mut body = body_with_field();
    body["chat_template_kwargs"] = json!({ routectl_router::TEST_RECHECK_KWARG: "summarized" });
    assert_eq!(
        post_inference(&daemon, &body).await,
        reqwest::StatusCode::OK
    );
    let live = daemon.live_router();
    assert!(
        recheck_acting(&live) && recheck_ledger_verdicts(&db_path) == ["broken"],
        "premise: nothing on the request path cleared the negative, so whatever \
         clears it below is the probe",
    );
    let summary = live.run_probe_pass().await;

    assert!(
        summary.free_validators_run > 0,
        "the re-check ran: {summary:?}"
    );
    let snap = live.probe_scheduler_snapshot();
    assert_eq!(
        (snap.resolved_total, snap.clear_persistence_failures_total),
        (1, 0),
        "the clear was acknowledged durable and the job resolved",
    );
    assert!(
        !recheck_acting(&live),
        "the disproved negative is cleared in memory"
    );
    assert_eq!(
        recheck_ledger_verdicts(&db_path),
        vec!["broken".to_string(), "cleared".to_string()],
        "exactly one cleared row landed in the daemon's own ledger, after the seed",
    );
    assert_eq!(
        recheck_cleared_sources(&db_path),
        vec!["probe".to_string()],
        "the clear is attributed to the routectl-issued probe that settled it",
    );
    assert!(
        !resident_after_warm_rebuild(&config, daemon.usage()).await,
        "a warm rebuild from the ledger does not resurrect the cleared negative",
    );

    drop(live);
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// A negative already LAPSED at boot is still recovered only by the re-check
// ---------------------------------------------------------------------------

/// Seed the ledger, before the daemon boots, with a boundary tombstone at this
/// config's revision pair and one live `broken` row on the re-check key -- the
/// state a previous session leaves behind. Through a writer of its own that is
/// drained before returning, so the daemon's boot warm is the only reader.
async fn seed_recheck_negative_before_boot(config: &Arc<Config>) {
    let (usage, writer) = routectl_usage::UsageWriter::start(
        config.usage.db_path.clone(),
        routectl_usage::CHANNEL_CAPACITY,
        0,
        true,
    );
    let now = crate::server::ledger_reader::epoch_ms_now();
    let catalog = i64::from(Router::new(Arc::clone(config)).catalog_version());
    let rows = vec![
        routectl_usage::CapabilityEvent::tombstone(now - 5_000, catalog, 0),
        routectl_usage::CapabilityEvent {
            ts: now - 4_000,
            lane_key: STATE_KEY.to_string(),
            capability: recheck_key(),
            verdict: "broken".to_string(),
            phase: "f1".to_string(),
            source: "live".to_string(),
            tier: "self-identifying".to_string(),
            evidence_class: None,
            upstream_token: None,
            catalog_version: catalog,
            overlay_revision: 0,
            provider_kind: Some("anthropic-api".to_string()),
            vocab_version: Some(routectl_router::CURRENT_VOCAB_VERSION),
        },
    ];
    let committer = usage.clone();
    let committed =
        tokio::task::spawn_blocking(move || committer.commit_capability_events_blocking(rows, 1))
            .await
            .expect("seed thread");
    assert!(
        matches!(committed, routectl_usage::BatchCommit::Committed { .. }),
        "premise: the seeded session must land: {committed:?}",
    );
    drop(usage);
    crate::server::test_support::drain_usage_writer_strict(writer).await;
}

/// THE SPAN for a negative whose decay window has already ENDED when the daemon
/// boots: replayed by the boot warm at zero confirmations, it keeps acting
/// through an ordinary successful request carrying the field, and only the free
/// re-check -- acknowledged durable -- clears it.
///
/// A zero decay window makes the replayed row lapsed the moment it is restated,
/// independent of how long the host has been up.
///
/// Mutation check: let a re-check-only negative lapse like any other (drop the
/// re-check rule from the registry's lapse predicate) -> red at the
/// still-acting assertion: the ordinary 200 clears the negative and its
/// `cleared` row lands with no probe having run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recheck_negative_lapsed_at_boot_survives_an_ordinary_success_until_the_recheck_clears_it()
 {
    let (config, dir) = daemon_config();
    let mut lapsing = (*config).clone();
    lapsing.capability.decay_hours = 0;
    let config = Arc::new(lapsing);
    let db_path = config.usage.db_path.clone();
    seed_recheck_negative_before_boot(&config).await;
    let recorder = Arc::new(Recorder::default());
    let daemon = spawn_daemon(
        Arc::clone(&config),
        dir,
        router_with(&config, Arc::clone(&recorder)),
    )
    .await;
    let live = daemon.live_router();
    let replayed = live
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| entry.state_key == STATE_KEY && entry.feature_key == recheck_key())
        .expect("premise: the boot warm replayed the negative");
    assert!(
        replayed.expires_at <= std::time::Instant::now(),
        "premise: the replayed negative's decay window has already ended",
    );
    drop(live);

    let mut body = body_with_field();
    body["chat_template_kwargs"] = json!({ routectl_router::TEST_RECHECK_KWARG: "summarized" });
    assert_eq!(
        post_inference(&daemon, &body).await,
        reqwest::StatusCode::OK
    );

    let live = daemon.live_router();
    assert!(
        recheck_acting(&live),
        "the lapsed negative is still resident after the ordinary success",
    );
    assert_eq!(
        recheck_ledger_verdicts(&db_path),
        ["broken"],
        "the ordinary success wrote no clear",
    );
    assert_eq!(
        live.probe_scheduler_snapshot().activations_total,
        1,
        "the request queued the re-check, which activates only on an ACTING negative",
    );

    let summary = live.run_probe_pass().await;

    assert!(
        summary.free_validators_run > 0,
        "the re-check ran: {summary:?}"
    );
    let snap = live.probe_scheduler_snapshot();
    assert_eq!(
        (snap.resolved_total, snap.clear_persistence_failures_total),
        (1, 0),
        "the clear was acknowledged durable and the job resolved",
    );
    assert!(!recheck_acting(&live), "the re-check cleared the negative");
    assert_eq!(
        recheck_ledger_verdicts(&db_path),
        vec!["broken".to_string(), "cleared".to_string()],
        "exactly one cleared row landed, after the seed",
    );
    assert!(
        !resident_after_warm_rebuild(&config, daemon.usage()).await,
        "a second warm rebuild from the ledger keeps the negative cleared",
    );

    drop(live);
    daemon.shutdown().await;
}
