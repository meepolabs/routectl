// A canary answered with a refused VALUE, and the canary's share of the
// request repair allowance. No injection: the live parser reads the committed
// display rejection, which names the restored row and a refused value.
// An `include!`d FRAGMENT of `field_canary_settlement_tests.rs`.

/// The committed anthropic-api display rejection.
const DISPLAY_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/anthropic-api/thinking-display-enum.json");

fn display_rejection() -> Error {
    let fixture: serde_json::Value =
        serde_json::from_str(DISPLAY_FIXTURE).expect("the fixture is JSON");
    let status =
        u16::try_from(fixture["status"].as_u64().expect("a status")).expect("an HTTP status");
    Error::upstream(
        "display-mock",
        status,
        fixture["body"]
            .as_str()
            .expect("the fixture carries its body"),
    )
}

/// A seat answering with the display rejection: only while the request carries
/// the field (`serve_repaired`), or on every attempt.
struct DisplaySeat {
    serve_repaired: bool,
    observed: Arc<Observed>,
}

impl DisplaySeat {
    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let carried = self.observed.record(req);
        if carried || !self.serve_repaired {
            return Err(display_rejection());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for DisplaySeat {
    fn id(&self) -> &'static str {
        "display-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("display-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
}

fn install_display_seats(seats: usize, serve_repaired: bool) -> (Router, Vec<Arc<Observed>>) {
    let mut router = Router::new(Arc::new(chain_config(seats)));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut observed = Vec::with_capacity(seats);
    for idx in 0..seats {
        let seen = Arc::new(Observed::default());
        observed.push(Arc::clone(&seen));
        let seat: Arc<dyn Provider> = Arc::new(DisplaySeat {
            serve_repaired,
            observed: seen,
        });
        models.insert(
            format!("m{idx}"),
            Arc::new(ResolvedModel::new(
                format!("m{idx}"),
                format!("p{idx}"),
                seat,
                format!("wire-{idx}"),
            )),
        );
    }
    router.install_resolved_models(models);
    (
        router.with_capability_writes_assumed_durable_for_tests(),
        observed,
    )
}

/// A ledger holding one persisted refused-field verdict for the display row.
struct DisplayVerdictLedger {
    state_key: String,
}

impl crate::capability_rebuild::CapabilityLedgerReader for DisplayVerdictLedger {
    fn tombstone(&self) -> Option<crate::capability_rebuild::ReplayTombstone> {
        Some(crate::capability_rebuild::ReplayTombstone::new(0, 0, 0))
    }

    fn read_events(&self) -> Vec<crate::capability_rebuild::CapabilityEventRow> {
        vec![
            crate::capability_rebuild::CapabilityEventRow::new(
                1,
                Instant::now(),
                "broken".to_string(),
                Some("f1".to_string()),
                "live".to_string(),
                Some("self-identifying".to_string()),
                None,
                grounded_key(),
                self.state_key.clone(),
                ANTHROPIC.to_string(),
                0,
                0,
            )
            .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION)),
        ]
    }
}

/// How the display seat answers the canary's repaired retry, and what the
/// request then reports.
struct RepairedRetryRow {
    name: &'static str,
    retry_serves: bool,
}

/// A verdict replayed from the ledger is resident, acting, and seeded due by
/// the warm rebuild itself -- the state a restart leaves every persisted
/// display verdict in. The canary restores the field and the upstream refuses
/// its VALUE. That proves nothing about the verdict, whatever the field-free
/// retry then does: the attempt is repaired and recorded as a lossy repair,
/// nothing confirms, nothing clears, the verdict stays resident, and the slot
/// is released inconclusive. Only the retry's own outcome moves the
/// succeeded count and the record's `repair_succeeded`.
///
/// Mutation checks: make `canary_repaired_retry` settle the canary disproved
/// -> red (a cleared row, the verdict gone); delete the lossy recorder call at
/// the canary call site -> red (no repair summary); make
/// `canary_repaired_retry` decline an `InvalidValue` rejection -> red (one
/// attempt, the canary's 400 reaches the client); report the canary's lossy
/// repair as succeeded whether or not the retry served -> red on the
/// `retry fails` row.
#[tokio::test]
async fn a_canary_answered_with_a_refused_value_keeps_the_replayed_verdict() {
    for row in [
        RepairedRetryRow {
            name: "retry serves",
            retry_serves: true,
        },
        RepairedRetryRow {
            name: "retry fails",
            retry_serves: false,
        },
    ] {
        assert_refused_value_canary_row(&row).await;
    }
}

async fn assert_refused_value_canary_row(row: &RepairedRetryRow) {
    let name = row.name;
    let (router, observed) = install_display_seats(1, row.retry_serves);
    let lane = crate::router::probe_test_support::seat_chain_lane("m0");
    let summary = router.rebuild_learned_from_ledger(&DisplayVerdictLedger {
        state_key: lane.as_lane_key().to_string(),
    });
    assert_eq!(
        summary.replayed_negative, 1,
        "{name}: premise: the verdict replayed"
    );
    assert_eq!(
        canary_snapshot(&router, "m0").map(|snap| snap.cadence),
        Some(1),
        "{name}: premise: the rebuild seeded the replayed verdict's canary due",
    );

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert_eq!(
        dispatched.result.is_ok(),
        row.retry_serves,
        "{name}: only a served retry reaches the client as a success: {:?}",
        dispatched.result.as_ref().err(),
    );
    assert_eq!(
        observed[0].carried_per_attempt(),
        vec![true, false],
        "{name}: the canary carried the field, then one repaired retry went out on the \
         same target",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "{name}: a refused value confirms nothing",
    );
    assert!(
        dispatched.meta.cleared_capabilities.is_empty(),
        "{name}: a refused value clears nothing either",
    );
    assert_eq!(
        router.learned_capability_snapshot().len(),
        1,
        "{name}: the replayed verdict is still resident",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(
        snap.last_outcome,
        Some(CanaryOutcome::Inconclusive),
        "{name}: the canary settled inconclusive",
    );
    assert!(!snap.canary_claimed, "{name}: and released its slot");
    let [repair] = dispatched.meta.field_repairs.as_slice() else {
        panic!(
            "{name}: exactly the canary's repair is recorded: {:?}",
            dispatched.meta.field_repairs
        );
    };
    assert_eq!(
        repair.action,
        super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR,
        "{name}"
    );
    assert_eq!(repair.field_path, "thinking.enabled.display", "{name}");
    assert_eq!(
        repair.repair_succeeded, row.retry_serves,
        "{name}: the record reports the retry's own outcome",
    );
    assert!(!repair.learned, "{name}: and nothing was learned");
    let counters = router.field_repair_counters();
    assert_eq!(counters.repair_attempted, 1, "{name}");
    assert_eq!(counters.repair_lossy, 1, "{name}");
    assert_eq!(
        counters.repair_succeeded,
        u64::from(row.retry_serves),
        "{name}"
    );
    assert_eq!(counters.verdicts_learned, 0, "{name}");
}

/// The canary's repaired retry is a repair like any other: once earlier
/// targets have spent the request's allowance, a due canary later in the
/// chain gets no repaired redispatch.
///
/// Every seat refuses the display value on every attempt, so the first two
/// seats each spend one reactive repair and then fall back, and the third --
/// whose verdict is acting with its canary due -- restores the field and draws
/// the same rejection with nothing left to spend.
///
/// Mutation check: hand `canary_repaired_retry` a fresh budget at the call
/// site -> red (the canary seat dispatches a third repaired attempt).
#[tokio::test]
async fn a_due_canary_after_the_request_allowance_is_spent_redispatches_nothing() {
    let (router, observed) = install_display_seats(3, false);
    plant_eligible_with_canary_due(&router, "m2");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: no seat serves");
    assert_eq!(
        observed[0].carried_per_attempt(),
        vec![true, false],
        "premise: the first seat spent one reactive repair",
    );
    assert_eq!(
        observed[1].carried_per_attempt(),
        vec![true, false],
        "premise: the second seat spent the other",
    );
    assert_eq!(
        observed[2].carried_per_attempt(),
        vec![true],
        "the canary seat restored the field and, with the allowance spent, dispatched \
         no repaired retry",
    );
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m2"), Instant::now()),
        "a refused value leaves the verdict acting, repaired or not",
    );
}

// --- two repairs in one walk ----------------------------------------------

/// The canary seat answers the display rejection while the field is carried and
/// is unavailable otherwise; the fallback seat refuses the test-only field
/// outright and serves a request without it; the seat that does both refuses
/// the display value first, then the test-only field, then serves.
#[derive(Clone, Copy)]
enum TwoRepairRole {
    Canary,
    Fallback,
    Both,
}

struct TwoRepairSeat {
    role: TwoRepairRole,
    observed: Arc<Observed>,
}

impl TwoRepairSeat {
    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let carried_display = self.observed.record(req);
        match self.role {
            TwoRepairRole::Canary if carried_display => Err(display_rejection()),
            TwoRepairRole::Canary => Err(Error::upstream(
                "two-repair-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
            TwoRepairRole::Fallback if carries_refused_field(req) => Err(Error::upstream(
                "two-repair-mock",
                400,
                refused_field_body(),
            )),
            TwoRepairRole::Fallback => Ok(()),
            TwoRepairRole::Both if carried_display => Err(display_rejection()),
            TwoRepairRole::Both if carries_refused_field(req) => Err(Error::upstream(
                "two-repair-mock",
                400,
                refused_field_body(),
            )),
            TwoRepairRole::Both => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for TwoRepairSeat {
    fn id(&self) -> &'static str {
        "two-repair-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("two-repair-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
}

fn carries_refused_field(req: &ChatRequest) -> bool {
    req.user.as_deref() == Some(super::field_repair::TEST_UNSUPPORTED_SENTINEL)
}

/// The location-prefix "Extra inputs are not permitted" wording, naming the
/// test-only refused-field row.
fn refused_field_body() -> String {
    json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "{}: Extra inputs are not permitted",
                super::field_repair::TEST_UNSUPPORTED_PATH,
            ),
        },
    })
    .to_string()
}

fn install_two_repair_seats(roles: &[TwoRepairRole]) -> (Router, Vec<Arc<Observed>>) {
    let mut router = Router::new(Arc::new(chain_config(roles.len())));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut observed = Vec::with_capacity(roles.len());
    for (idx, &role) in roles.iter().enumerate() {
        let seen = Arc::new(Observed::default());
        observed.push(Arc::clone(&seen));
        let seat: Arc<dyn Provider> = Arc::new(TwoRepairSeat {
            role,
            observed: seen,
        });
        models.insert(
            format!("m{idx}"),
            Arc::new(ResolvedModel::new(
                format!("m{idx}"),
                format!("p{idx}"),
                seat,
                format!("wire-{idx}"),
            )),
        );
    }
    router.install_resolved_models(models);
    (
        router.with_capability_writes_assumed_durable_for_tests(),
        observed,
    )
}

/// Two repairs in one walk on two targets each keep their own record: the
/// canary's lossy repair on the first target, whose repaired retry was
/// unavailable, and the reactive refused-field repair on the fallback target,
/// whose repaired retry served and committed. Both reach the request-level
/// WARN, one line each, in the order they fired.
///
/// The fallback seat carries a resident display verdict only so the chain
/// filter demotes it beside the canary seat and keeps the configured order;
/// it is neither eligible nor due, so it neither pre-flights nor canaries.
///
/// Mutation checks: let the recorder replace the request's previous record
/// instead of appending -> red (one record, the canary's outcome lost); emit
/// only the first record -> red (one WARN). Settling by handle rather than by
/// recency is pinned by `a_success_settles_each_repair_on_its_own_record`:
/// here the only success settles the latest record either way.
#[test]
fn a_canary_lossy_repair_and_a_fallback_repair_each_keep_their_record() {
    let (router, observed) =
        install_two_repair_seats(&[TwoRepairRole::Canary, TwoRepairRole::Fallback]);
    plant_eligible_with_canary_due(&router, "m0");
    plant_verdict(&router, "m1");
    let mut req = req_on(ALIAS);
    req.user = Some(super::field_repair::TEST_UNSUPPORTED_SENTINEL.to_string());

    let mut dispatched = None;
    let events = routectl_testkit::capture_events(|| {
        dispatched = Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime with timers")
                .block_on(router.complete_with_options(req, RouterOptions::new())),
        );
    });
    let dispatched = dispatched.expect("the walk ran");

    assert!(
        dispatched.result.is_ok(),
        "premise: the fallback's repaired retry serves: {:?}",
        dispatched.result.as_ref().err(),
    );
    assert_eq!(
        observed[0].carried_per_attempt(),
        vec![true, false],
        "premise: the canary carried the display field, then its repaired retry went out",
    );
    assert_eq!(
        observed[1]
            .dispatched
            .lock()
            .iter()
            .map(carries_refused_field)
            .collect::<Vec<_>>(),
        vec![true, false],
        "premise: the fallback carried the refused field, then its repaired retry went out",
    );
    let records: Vec<_> = dispatched
        .meta
        .field_repairs
        .iter()
        .map(|r| {
            (
                r.action,
                r.state_key.as_str(),
                r.field_path,
                r.repair_succeeded,
                r.learned,
            )
        })
        .collect();
    assert_eq!(
        records,
        vec![
            (
                super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR,
                "m0",
                GROUNDED_PATH,
                false,
                false,
            ),
            (
                super::field_repair::FIELD_ACTION_DROP_REPAIR,
                "m1",
                super::field_repair::TEST_UNSUPPORTED_PATH,
                true,
                true,
            ),
        ],
        "both repairs survive, in firing order, each with its own outcome",
    );
    let counters = router.field_repair_counters();
    assert_eq!(counters.repair_attempted, 2);
    assert_eq!(counters.repair_lossy, 1);
    assert_eq!(counters.repair_succeeded, 1);
    assert_eq!(counters.verdicts_learned, 1);

    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
        .map(|e| {
            (
                e.field("action"),
                e.field("field_path"),
                e.field("repair_succeeded"),
                e.field("learned"),
            )
        })
        .collect();
    assert_eq!(
        warns,
        vec![
            (
                Some(super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR),
                Some(GROUNDED_PATH),
                Some("false"),
                Some("false"),
            ),
            (
                Some(super::field_repair::FIELD_ACTION_DROP_REPAIR),
                Some(super::field_repair::TEST_UNSUPPORTED_PATH),
                Some("true"),
                Some("true"),
            ),
        ],
        "one WARN per repair, each carrying its own record",
    );
}

/// Two repairs on ONE target, settled by one success: the canary's lossy repair
/// fires first and the reactive refused-field repair second, so the success
/// settles a record that is no longer the latest. Each settlement reaches its
/// own record.
///
/// Mutation check: settle the most recent record instead of the handle's ->
/// red (the canary record stays unsucceeded while the later one absorbs both
/// settlements).
#[tokio::test]
async fn a_success_settles_each_repair_on_its_own_record() {
    let (router, observed) = install_two_repair_seats(&[TwoRepairRole::Both]);
    plant_eligible_with_canary_due(&router, "m0");
    let mut req = req_on(ALIAS);
    req.user = Some(super::field_repair::TEST_UNSUPPORTED_SENTINEL.to_string());

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "premise: the twice-repaired attempt serves: {:?}",
        dispatched.result.as_ref().err(),
    );
    assert_eq!(
        observed[0]
            .dispatched
            .lock()
            .iter()
            .map(|r| (carries_field(r), carries_refused_field(r)))
            .collect::<Vec<_>>(),
        vec![(true, true), (false, true), (false, false)],
        "premise: the canary attempt, its lossy repaired retry, then the reactive one",
    );
    let records: Vec<_> = dispatched
        .meta
        .field_repairs
        .iter()
        .map(|r| (r.action, r.repair_succeeded, r.learned))
        .collect();
    assert_eq!(
        records,
        vec![
            (
                super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR,
                true,
                false
            ),
            (super::field_repair::FIELD_ACTION_DROP_REPAIR, true, true),
        ],
        "both records settled succeeded, and only the refused-field one learned",
    );
    let counters = router.field_repair_counters();
    assert_eq!(counters.repair_attempted, 2);
    assert_eq!(counters.repair_lossy, 1);
    assert_eq!(counters.repair_succeeded, 2);
    assert_eq!(counters.verdicts_learned, 1);
}
