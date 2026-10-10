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

/// A verdict replayed from the ledger is resident, acting, and seeded due by
/// the warm rebuild itself -- the state a restart leaves every persisted
/// display verdict in. The canary restores the field, the upstream refuses its
/// VALUE, and that proves the field is recognized: the attempt is repaired and
/// served, nothing confirms, and the verdict is cleared durably.
///
/// Mutation check: make `canary_repaired_retry` decline an `InvalidValue`
/// rejection -> red (one attempt, the canary's 400 reaches the client).
#[tokio::test]
async fn a_canary_answered_with_a_refused_value_serves_and_clears_the_replayed_verdict() {
    let (router, observed) = install_display_seats(1, true);
    let lane = crate::router::probe_test_support::seat_chain_lane("m0");
    let summary = router.rebuild_learned_from_ledger(&DisplayVerdictLedger {
        state_key: lane.as_lane_key().to_string(),
    });
    assert_eq!(
        summary.replayed_negative, 1,
        "premise: the verdict replayed"
    );
    assert_eq!(
        canary_snapshot(&router, "m0").map(|snap| snap.cadence),
        Some(1),
        "premise: the rebuild seeded the replayed verdict's canary due",
    );

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "the canary's own rejection must not reach the client: {:?}",
        dispatched.result.err(),
    );
    assert_eq!(
        observed[0].carried_per_attempt(),
        vec![true, false],
        "the canary carried the field, then one repaired retry went out on the same target",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "a refused value confirms nothing",
    );
    let cleared: Vec<_> = dispatched
        .meta
        .cleared_capabilities
        .iter()
        .map(|event| event.capability_key.clone())
        .collect();
    assert_eq!(
        cleared,
        vec![grounded_key()],
        "the clear rides out on the meta so a warm rebuild cannot resurrect the verdict",
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "the resident verdict is gone, not merely suspended",
    );
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
        !router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m2"), Instant::now()),
        "the refused value still disproves the verdict: that evidence needs no retry",
    );
}
