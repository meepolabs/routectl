// The live rejection parser behind the repair arm, the parser-unlocalized
// counter, and its class gate. No test here injects a resolution unless it
// says so: the parser reads each mock rejection's carried parse.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

// ---------------------------------------------------------------------------
// The captured display rejection: repaired for the attempt, never learned
// ---------------------------------------------------------------------------

/// Every walk served by the captured display rejection: the parser names the
/// display row and a refused VALUE, so each walk drops the field once, serves
/// the repaired request, commits no verdict, and records a lossy repair.
///
/// Mutation checks: make `parse_rejection` return `None` -> red (one attempt,
/// the rejection reaches the client); let `FieldRepairPlan::commit` persist on
/// an `InvalidValue` repair -> red (one learned row, the lossy summary claims
/// `learned`).
#[tokio::test]
async fn the_captured_display_rejection_repairs_every_walk_and_learns_nothing() {
    let (complete_router, complete_seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let (stream_router, stream_seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let (count_router, count_seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let completed = complete_router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let streamed = stream_router
        .stream_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let counted = count_router.count_tokens_with_meta(req_on(ALIAS)).await;

    assert!(
        completed.result.is_ok(),
        "complete: {:?}",
        completed.result.err()
    );
    assert!(
        streamed.result.is_ok(),
        "stream: {:?}",
        streamed.result.err()
    );
    assert!(
        counted.result.is_ok(),
        "count_tokens: {:?}",
        counted.result.err()
    );
    for (walk, router, seen, meta) in [
        (
            "complete",
            &complete_router,
            &complete_seen,
            &completed.meta,
        ),
        ("stream", &stream_router, &stream_seen, &streamed.meta),
        ("count_tokens", &count_router, &count_seen, &counted.meta),
    ] {
        assert_eq!(
            seen.carried_per_attempt(),
            vec![true, false],
            "{walk}: one carried attempt, then one repaired retry without the field",
        );
        assert!(
            meta.learned_capabilities.is_empty(),
            "{walk}: a refused value commits no verdict",
        );
        let summary = meta
            .field_repairs
            .first()
            .unwrap_or_else(|| panic!("{walk}: the repair records a summary"));
        assert_eq!(
            summary.action,
            super::FIELD_ACTION_LOSSY_DROP_REPAIR,
            "{walk}"
        );
        assert_eq!(summary.field_path, REJECTED_PATH, "{walk}");
        assert!(
            summary.repair_succeeded,
            "{walk}: the repaired retry served"
        );
        assert!(!summary.learned, "{walk}: and nothing was learned");
        assert_eq!(
            router.metrics.field_repair_lossy_total(),
            1,
            "{walk}: the lossy repair is counted once",
        );
        let counters = router.field_repair_counters();
        assert_eq!(counters.repair_attempted, 1, "{walk}");
        assert_eq!(counters.repair_succeeded, 1, "{walk}");
        assert_eq!(counters.verdicts_learned, 0, "{walk}");
        assert_eq!(
            counters.parser_unlocalized, 0,
            "{walk}: a named row is not parser blindness",
        );
        assert!(
            !verdict_resident(router, "m0"),
            "{walk}: and the store holds no verdict for the field",
        );
    }
}

/// The lossy repair reaches the operator as the per-request WARN, under its
/// own action token.
///
/// Mutation checks: record the lossy repair under `FIELD_ACTION_DROP_REPAIR`
/// -> red; log a ranked-pick field as `?` (rendering `None`) -> red on the
/// field list.
#[test]
fn the_display_repair_warns_once_as_a_lossy_repair() {
    let (router, _seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let events = routectl_testkit::capture_events(|| {
        let served = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime with timers")
            .block_on(router.complete(req_on(ALIAS)));
        assert!(served.is_ok(), "premise: the repaired request serves");
    });

    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per repaired request: {warns:?}");
    assert_eq!(warns[0].field("action"), Some("field_lossy_drop_repair"));
    assert_eq!(warns[0].field("field_path"), Some(REJECTED_PATH));
    assert_eq!(warns[0].field("learned"), Some("false"));
    let names: Vec<_> = warns[0]
        .fields
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "action",
            "state_key",
            "lane",
            "field_path",
            "reason",
            "repair_attempted",
            "repair_succeeded",
            "learned",
        ],
        "a repair with no ranked pick keeps exactly the fields it always had",
    );
}

// ---------------------------------------------------------------------------
// A parsed refused-FIELD rejection commits one verdict
// ---------------------------------------------------------------------------

/// The rejection the refused-field seat answers with: the location-prefix
/// "Extra inputs are not permitted" wording, naming the test-only row.
fn refused_field_body() -> String {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "{}: Extra inputs are not permitted",
                super::TEST_UNSUPPORTED_PATH,
            ),
        },
    })
    .to_string()
}

/// A seat that refuses the test-only `user` field outright and serves a
/// request without it.
#[derive(Default)]
struct RefusedFieldSeat {
    carried: Mutex<Vec<bool>>,
}

impl RefusedFieldSeat {
    fn carries(req: &ChatRequest) -> bool {
        req.user.as_deref() == Some(super::TEST_UNSUPPORTED_SENTINEL)
    }

    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let carried = Self::carries(req);
        self.carried.lock().push(carried);
        if carried {
            return Err(Error::upstream("field-mock", 400, refused_field_body()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for RefusedFieldSeat {
    fn id(&self) -> &'static str {
        "field-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("field-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.answer(&req).map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// A plain user request carrying only the test-only refused field.
fn req_with_refused_field() -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.reasoning = None;
    req.routectl_internal.anthropic_thinking_display = None;
    req.user = Some(super::TEST_UNSUPPORTED_SENTINEL.to_string());
    req
}

/// No injection in scope: the parser names the test-only row and a refused
/// field, so the repaired retry serves and its success commits one verdict.
///
/// Mutation check: make `parse_rejection` return `None` -> red (one attempt,
/// nothing learned).
#[tokio::test]
async fn a_parsed_refused_field_rejection_repairs_and_commits_one_verdict() {
    let seat = Arc::new(RefusedFieldSeat::default());
    let mut router = Router::new(Arc::new(chain_config(ALIAS, 1)));
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0".to_string(),
            "p0".to_string(),
            Arc::clone(&seat) as Arc<dyn Provider>,
            "wire-0".to_string(),
        )),
    )]));

    let dispatched = router
        .complete_with_options(req_with_refused_field(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(
        *seat.carried.lock(),
        vec![true, false],
        "one carried attempt, then one repaired retry without the field",
    );
    let learned: Vec<_> = dispatched
        .meta
        .learned_capabilities
        .iter()
        .map(|event| event.capability_key.clone())
        .collect();
    assert_eq!(
        learned,
        vec![
            crate::field_capability::field_capability_key(super::TEST_UNSUPPORTED_PATH)
                .expect("the test path is a qualified path"),
        ],
        "exactly one verdict, for the refused field",
    );
    let summary = dispatched
        .meta
        .field_repairs
        .first()
        .expect("the repair records a summary");
    assert_eq!(summary.action, super::FIELD_ACTION_DROP_REPAIR);
    assert!(summary.learned, "the summary reports the committed verdict");
    assert_eq!(router.metrics.field_repair_lossy_total(), 0);
    assert_eq!(router.field_repair_counters().verdicts_learned, 1);
}

// ---------------------------------------------------------------------------
// The parser-unlocalized count
// ---------------------------------------------------------------------------

/// A Bedrock seat answering every request with the committed
/// `unknown-body-field` validation canary.
#[cfg(feature = "bedrock")]
struct BedrockCanarySeat {
    calls: Mutex<Vec<ChatRequest>>,
}

#[cfg(feature = "bedrock")]
impl BedrockCanarySeat {
    const CAPTURE: &'static str =
        include_str!("../../tests/fixtures/bedrock_validation_capture.json");

    fn rejection() -> Error {
        let capture: serde_json::Value =
            serde_json::from_str(Self::CAPTURE).expect("the capture is JSON");
        let canary = &capture["canaries"]["unknown-body-field"];
        let status =
            u16::try_from(canary["status"].as_u64().expect("a status")).expect("an HTTP status");
        Error::upstream(
            "bedrock-mock",
            status,
            canary["body"].as_str().expect("a body"),
        )
    }
}

#[cfg(feature = "bedrock")]
#[async_trait::async_trait]
impl Provider for BedrockCanarySeat {
    fn id(&self) -> &'static str {
        "bedrock-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("bedrock-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.lock().push(req);
        Err(Self::rejection())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.lock().push(req);
        Err(Self::rejection())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.calls.lock().push(req);
        Err(Self::rejection())
    }
}

/// A one-seat Bedrock router whose seat answers with the committed canary.
///
/// The upstream id is Anthropic-family so the token-count walk admits the seat;
/// a non-family id would never be dialed there and the row would pass vacuously.
#[cfg(feature = "bedrock")]
fn bedrock_canary_router() -> (Router, Arc<BedrockCanarySeat>) {
    let mut config: Config = toml::from_str(
        "\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"bedrock\"\nregion = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\n",
    )
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let seat = Arc::new(BedrockCanarySeat {
        calls: Mutex::new(Vec::new()),
    });
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0".to_string(),
            "p0".to_string(),
            Arc::clone(&seat) as Arc<dyn Provider>,
            "anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
        )),
    )]));
    (router, seat)
}

/// The committed Bedrock `unknown-body-field` canary localizes a path the
/// closed table has no row for: on every walk that can reach it, it counts once
/// as parser-unlocalized, dispatches once, mutates nothing, and learns nothing.
///
/// The token-count row is the one that matters most: that walk never reaches
/// the learn path, so the count there comes from its own call site.
///
/// Mutation check: delete the observer call in `count_tokens_try_seat` -> the
/// `count_tokens` row reds on the count (0, not 1).
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn an_unresolved_bedrock_field_rejection_counts_once_and_changes_nothing() {
    for walk in ["complete", "count_tokens"] {
        let (router, seat) = bedrock_canary_router();
        let sent = req_on(ALIAS);

        let meta = match walk {
            "complete" => {
                let dispatched = router
                    .complete_with_options(sent.clone(), RouterOptions::new())
                    .await;
                assert!(
                    dispatched.result.is_err(),
                    "{walk}: premise: the seat rejects"
                );
                dispatched.meta
            }
            _ => {
                let counted = router.count_tokens_with_meta(sent.clone()).await;
                assert!(counted.result.is_err(), "{walk}: premise: the seat rejects");
                counted.meta
            }
        };

        assert_eq!(
            router.field_repair_counters().parser_unlocalized,
            1,
            "{walk}: the unresolved field-eligible rejection counts once",
        );
        let calls = seat.calls.lock();
        assert_eq!(calls.len(), 1, "{walk}: no repaired retry was dispatched");
        assert!(
            carries_field(&sent) && carries_field(&calls[0]),
            "{walk}: the request reached the seat with every closed-table carrier the \
             client sent",
        );
        assert!(meta.field_repairs.is_empty(), "{walk}: no repair fired");
        assert!(
            meta.learned_capabilities.is_empty(),
            "{walk}: and nothing was learned",
        );
        assert!(
            router.learned_capability_snapshot().is_empty(),
            "{walk}: and the store holds no verdict",
        );
    }
}

/// A rejection the parser DID name a row from does not bump the blind counter.
///
/// The paired control, and the one that makes the count a signal rather than a
/// rejection tally: a counter that rose on every caller-shaped 4xx regardless
/// would report the parser as permanently blind even on the traffic it reads
/// perfectly.
///
/// `AlwaysRejectField` rather than `ServeRepaired` on purpose. A served repair
/// leaves the walk via the repair arm's `continue`, so the observer is never
/// REACHED and the test would pass without the predicate existing at all. Here
/// the repaired retry is rejected too, so a named rejection genuinely arrives
/// at the observer and the predicate is what declines it. The token-count row
/// matters separately: that walk has its own observer call site.
///
/// Mutation check: make `rejection_localizes_no_field` ignore the parser
/// (class gate only) -> red on every row.
#[tokio::test]
async fn a_localized_rejection_leaves_the_parser_blind_counter_untouched() {
    for walk in ["complete", "count_tokens"] {
        let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);

        let rejected = match walk {
            "complete" => router.complete(req_on(ALIAS)).await.is_err(),
            _ => router
                .count_tokens_with_meta(req_on(ALIAS))
                .await
                .result
                .is_err(),
        };

        assert!(rejected, "{walk}: premise: every attempt is rejected");
        assert_eq!(
            seen.repairs(),
            1,
            "{walk}: premise: the parser named the field, so a repaired attempt was \
             dispatched and its own rejection reached the observer",
        );
        assert_eq!(
            router.field_repair_counters().parser_unlocalized,
            0,
            "{walk}: a rejection the parser named a row from is not evidence of blindness",
        );
    }
}

/// An unlocalized token-count rejection counts as parser blindness on an
/// own-credential target and not at all on one that authenticates with the
/// client's forwarded credential: routectl owns no credential there, so the
/// rejection is not parser evidence. The own-credential row is the control
/// that proves the fixture reaches the observer.
///
/// Mutation check: drop the forwarded-credential refusal from
/// `observe_unlocalized_field_rejection` -> the forwarded row reds (1, not 0).
#[tokio::test]
async fn a_forwarded_token_count_rejection_counts_no_parser_blindness() {
    for (row, forwarded, expected) in [("own credential", false, 1), ("forwarded", true, 0)] {
        let (router, seen, req) = if forwarded {
            forwarded_seat(Answer::UnlocalizedBadRequest)
        } else {
            let (router, seen) = single_seat(ALIAS, Answer::UnlocalizedBadRequest);
            (router, seen, req_on(ALIAS))
        };

        let counted = router.count_tokens_with_meta(req).await;

        assert!(counted.result.is_err(), "{row}: premise: the seat rejects");
        assert_eq!(seen.calls(), 1, "{row}: premise: nothing is repaired");
        assert_eq!(
            router.field_repair_counters().parser_unlocalized,
            expected,
            "{row}",
        );
    }
}

/// A rejection whose CLASS cannot name a field at all is not counted as blind.
///
/// A 503 says nothing about the request envelope, so counting it would put the
/// whole rate of transient upstream failure into a gauge an operator reads as
/// "how much wire shape am I failing to parse" -- swamping the signal with noise
/// from a different subsystem.
///
/// Driven at the PREDICATE rather than through a dispatch, and the reason is a
/// measured one: on this fixture's lane the gate is behaviorally UNREACHABLE.
/// The learn path only reaches the observer for a 400 or a 422, and the
/// `anthropic-api` family token table carries no context-window or
/// feature-unsupported entries, so every rejection that gets that far classes as
/// `BadRequest` -- which the gate admits. A dispatch-driven test therefore passes
/// with the gate deleted (verified), because its 503 never reaches the predicate
/// at all. The gate is still correct to keep: it is what stops a lane whose
/// table DOES lift a 4xx (openai-compat has both token sets) from booking a
/// context-window rejection as an unparsed envelope.
#[test]
fn a_rejection_whose_class_cannot_name_a_field_is_not_counted_as_blind() {
    let named = Error::upstream("p", 400, field_reject_body());
    let unnamed = Error::upstream("p", 400, "{}");

    // The admitted class, as the control: without it, a predicate that refused
    // every class would satisfy the refusals below while making the counter dead.
    assert!(
        !Router::rejection_localizes_no_field(
            crate::router::FieldRepairTable::default(),
            &FailureClass::BadRequest,
            &named,
            ANTHROPIC_API_KIND,
        ),
        "premise: a BadRequest the parser named a row from is not blindness",
    );
    assert!(
        Router::rejection_localizes_no_field(
            crate::router::FieldRepairTable::default(),
            &FailureClass::BadRequest,
            &unnamed,
            ANTHROPIC_API_KIND,
        ),
        "and the same class with nothing named IS blindness -- so the predicate \
         genuinely turns on the resolution for an admitted class",
    );

    // Every class that says nothing about the envelope is refused on the class
    // alone, whatever the parser would answer -- asserted on the body the
    // parser DOES name a row from, so a refusal cannot come from the parse.
    // EVERY non-BadRequest variant, enumerated rather than sampled: the gate's
    // contract is that exactly one class is admitted, so a variant left out here
    // is a class nobody has decided about -- and a `#[non_exhaustive]` enum makes
    // that decision easy to skip.
    for class in [
        FailureClass::RateLimited,
        FailureClass::Auth,
        FailureClass::ContentPolicy,
        FailureClass::ContextWindow,
        FailureClass::ServerError,
        FailureClass::Timeout,
        FailureClass::NetworkError,
        FailureClass::Overloaded,
        FailureClass::FeatureUnsupported {
            capability: "web_search".to_string(),
        },
        FailureClass::Unknown,
    ] {
        assert!(
            !Router::rejection_localizes_no_field(
                crate::router::FieldRepairTable::default(),
                &class,
                &named,
                ANTHROPIC_API_KIND
            ),
            "{class:?} says nothing about the request envelope, so it is not evidence \
             about the parser",
        );
    }
}
