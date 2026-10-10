// The attribution tier of a reactive repair's commit, persisted on the ledger
// row the daemon writes for it. `include!`d into preflight_daemon_tests.rs; the
// fixtures and imports live there, so do not add `use` lines here.

/// A rejection the live parser attributes to the field at the INFERRED tier: the
/// quoted not-supported form refuses the field, but its continuation is not the
/// grounded one, so the attribution is prose rather than a captured template.
const INFERRED_FIELD_REJECT_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"\"thinking.enabled.display\" is not supported on this deployment."}}"#;

/// The `(capability, tier)` of every ledger row for this test's identity.
fn field_ledger_tiers(db_path: &std::path::Path) -> Vec<String> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT capability, tier FROM capability_events WHERE lane_key = ?1 ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([STATE_KEY], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .expect("query")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("rows")
    .into_iter()
    .filter(|(capability, _)| *capability == field_key())
    .map(|(_, tier)| tier)
    .collect()
}

/// An inferred reactive repair reaches the daemon's ledger stamped `inferred`.
///
/// Driven end to end: a real HTTP request, the live parser reading the upstream
/// rejection, the repair and its commit, and the production acknowledged drain
/// writing the row. Nothing here constructs the event.
///
/// Mutation check: hardcode `SignalTier::SelfIdentifying` on the event row in
/// `FieldRepairGuard::commit` -> the persisted token reads `self-identifying`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inferred_reactive_repair_persists_its_tier_on_the_ledger_row() {
    let (config, dir) = daemon_config();
    let db_path = config.usage.db_path.clone();
    let recorder = Arc::new(Recorder {
        carried_rejection: Some(INFERRED_FIELD_REJECT_BODY),
        ..Recorder::default()
    });
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;

    let status = post_inference(&daemon, &body_with_field()).await;

    assert_eq!(status, reqwest::StatusCode::OK, "the repaired retry served");
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true, false],
        "premise: the field was rejected and the repair dropped it",
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let tiers = loop {
        let tiers = field_ledger_tiers(&db_path);
        if !tiers.is_empty() && daemon.confirmations().in_flight() == 0 {
            break tiers;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the committed verdict's row never reached the ledger",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        tiers,
        vec!["inferred".to_string()],
        "exactly one row, carrying the tier the rejection was attributed at",
    );

    daemon.shutdown().await;
}
