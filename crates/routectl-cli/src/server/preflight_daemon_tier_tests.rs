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

/// The `upstream_token` of every ledger row for this test's identity.
fn field_ledger_values(db_path: &std::path::Path) -> Vec<Option<String>> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT capability, upstream_token FROM capability_events \
             WHERE lane_key = ?1 ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([STATE_KEY], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })
    .expect("query")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("rows")
    .into_iter()
    .filter(|(capability, _)| *capability == field_key())
    .map(|(_, value)| value)
    .collect()
}

/// Write one field-verdict event carrying `value` through the production
/// acknowledged drain and read back the persisted `upstream_token`.
async fn persisted_value_of(value: Option<routectl_router::VerdictValue>) -> Vec<Option<String>> {
    let (config, dir) = daemon_config();
    let db_path = config.usage.db_path.clone();
    let router = router_with(&config, Arc::new(Recorder::default()));
    plant_unacknowledged_verdict(&router);
    let daemon = spawn_daemon(config, dir, router).await;
    let live = daemon.live_router();
    let mut event = earned_learn_event(&live, 1);
    event.value = value;
    let mut meta = routectl_router::DispatchMeta::for_alias(ALIAS);
    meta.learned_capabilities.push(event);

    advance_through_the_daemon(&daemon, &live, &meta).await;

    let values = field_ledger_values(&db_path);
    daemon.shutdown().await;
    values
}

/// A rewrite verdict's row carries its value as `shape:<token>`.
///
/// Mutation check: write `upstream_token: None` in `capability_ack_drain` -> red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewrite_commit_persists_its_shape_in_the_upstream_token() {
    let values = persisted_value_of(Some(routectl_router::VerdictValue::Shape("summarized"))).await;

    assert_eq!(
        values,
        vec![Some("shape:summarized".to_string())],
        "exactly one row, carrying the encoded rewrite value",
    );
}

/// A ceiling verdict's row carries its limit as `ceiling:<n>`.
///
/// Mutation check: write `upstream_token: None` in `capability_ack_drain` -> red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ceiling_commit_persists_its_limit_in_the_upstream_token() {
    let values = persisted_value_of(Some(routectl_router::VerdictValue::Ceiling(8192))).await;

    assert_eq!(
        values,
        vec![Some("ceiling:8192".to_string())],
        "exactly one row, carrying the encoded ceiling",
    );
}

/// A drop verdict's row carries no value: the column is NULL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drop_commit_persists_a_null_upstream_token() {
    let values = persisted_value_of(None).await;

    assert_eq!(values, vec![None], "exactly one row, with a NULL value");
}
