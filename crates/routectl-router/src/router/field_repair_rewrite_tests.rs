// The REWRITE transform through the live parser: a rejection naming exactly
// one accepted value rewrites the field to it and commits, anything else
// repairs nothing, and a rewrite draws the same per-request allowance a drop
// does. No test here injects a resolution.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

// ---------------------------------------------------------------------------
// Fixture: a seat refusing the test-only rewrite field's value
// ---------------------------------------------------------------------------

/// The value the client sends in the rewrite field: outside the accepted set.
const CLIENT_REWRITE_VALUE: &str = "routectl-test-client-value";

/// The rewrite field's value on `req`, if it carries one.
fn rewrite_value(req: &ChatRequest) -> Option<String> {
    req.chat_template_kwargs
        .as_ref()
        .and_then(|kwargs| kwargs.get(super::TEST_REWRITE_KWARG))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// A refused-VALUE rejection on the rewrite row whose reason quotes `named`.
fn rewrite_rejection(named: &[&str]) -> Error {
    let quoted: Vec<String> = named.iter().map(|value| format!("'{value}'")).collect();
    let body = json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "{}: Input should be {}",
                super::TEST_REWRITE_PATH,
                quoted.join(", "),
            ),
        },
    });
    Error::upstream("field-mock", 400, body.to_string())
}

/// The refused-FIELD rejection on the test-only drop row.
fn unsupported_rejection() -> Error {
    Error::upstream("field-mock", 400, refused_field_body())
}

/// What a [`RewriteSeat`] answers a request still carrying a refused value.
#[derive(Clone, Copy)]
enum RewriteAnswer {
    /// Name these values, then serve any accepted value.
    Name(&'static [&'static str]),
    /// Name the first accepted value, then answer the rewritten retry with a
    /// 503 so the walk falls back having repaired.
    RewriteThenWalk,
    /// Refuse the test-only drop row's field outright, then answer the
    /// repaired retry with a 503.
    DropThenWalk,
}

/// A seat that refuses the rewrite field while it holds a value outside the
/// accepted set, recording every attempt's request.
struct RewriteSeat {
    answer: RewriteAnswer,
    seen: Mutex<Vec<ChatRequest>>,
}

impl RewriteSeat {
    fn new(answer: RewriteAnswer) -> Self {
        Self {
            answer,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn values(&self) -> Vec<Option<String>> {
        self.seen.lock().iter().map(rewrite_value).collect()
    }

    fn calls(&self) -> usize {
        self.seen.lock().len()
    }

    fn answer(&self, req: &ChatRequest) -> Result<()> {
        self.seen.lock().push(req.clone());
        let refused = rewrite_value(req).as_deref() == Some(CLIENT_REWRITE_VALUE);
        match self.answer {
            RewriteAnswer::Name(named) if refused => Err(rewrite_rejection(named)),
            RewriteAnswer::Name(_) => Ok(()),
            RewriteAnswer::RewriteThenWalk if refused => {
                Err(rewrite_rejection(&super::TEST_REWRITE_ACCEPTED[..1]))
            }
            RewriteAnswer::DropThenWalk if RefusedFieldSeat::carries(req) => {
                Err(unsupported_rejection())
            }
            RewriteAnswer::RewriteThenWalk | RewriteAnswer::DropThenWalk => Err(Error::upstream(
                "field-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
        }
    }
}

#[async_trait::async_trait]
impl Provider for RewriteSeat {
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

/// A router over one [`RewriteSeat`] per entry of `answers`, in chain order.
fn rewrite_chain(answers: &[RewriteAnswer]) -> (Router, Vec<Arc<RewriteSeat>>) {
    let mut router = Router::new(Arc::new(chain_config(ALIAS, answers.len())));
    let seats: Vec<Arc<RewriteSeat>> = answers
        .iter()
        .map(|answer| Arc::new(RewriteSeat::new(*answer)))
        .collect();
    router.install_resolved_models(
        seats
            .iter()
            .enumerate()
            .map(|(idx, seat)| {
                (
                    format!("m{idx}"),
                    Arc::new(ResolvedModel::new(
                        format!("m{idx}"),
                        format!("p{idx}"),
                        Arc::clone(seat) as Arc<dyn Provider>,
                        format!("wire-{idx}"),
                    )),
                )
            })
            .collect(),
    );
    (router, seats)
}

/// A request carrying only the rewrite field, holding the client's value.
fn req_with_rewrite_field() -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.reasoning = None;
    req.routectl_internal.anthropic_thinking_display = None;
    req.chat_template_kwargs = Some(json!({ super::TEST_REWRITE_KWARG: CLIENT_REWRITE_VALUE }));
    req
}

fn rewrite_key() -> String {
    crate::field_capability::field_capability_key(super::TEST_REWRITE_PATH)
        .expect("the test path is a qualified path")
}

// ---------------------------------------------------------------------------
// Rewrite outcomes
// ---------------------------------------------------------------------------

/// Mutation check: make the rewrite edit drop the field instead -> red (the
/// retry carries no value); make `commit` refuse every `InvalidValue` repair
/// -> red (nothing learned).
#[tokio::test]
async fn a_rejection_naming_one_accepted_value_rewrites_to_it_and_commits() {
    let named = &super::TEST_REWRITE_ACCEPTED[1..];
    let (router, seats) = rewrite_chain(&[RewriteAnswer::Name(named)]);

    let dispatched = router
        .complete_with_options(req_with_rewrite_field(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(
        seats[0].values(),
        vec![
            Some(CLIENT_REWRITE_VALUE.to_string()),
            Some(named[0].to_string()),
        ],
        "one carried attempt, then one retry carrying exactly the named literal",
    );
    let learned: Vec<_> = dispatched
        .meta
        .learned_capabilities
        .iter()
        .map(|event| event.capability_key.clone())
        .collect();
    assert_eq!(
        learned,
        vec![rewrite_key()],
        "the rewrite commits one verdict"
    );
    let summary = dispatched
        .meta
        .field_repair
        .as_ref()
        .expect("the repair records a summary");
    assert_eq!(summary.action, super::FIELD_ACTION_REWRITE_REPAIR);
    assert!(summary.repair_succeeded);
    assert!(summary.learned, "the summary reports the committed verdict");
    assert_eq!(router.metrics.field_repair_lossy_total(), 0);
    assert_eq!(router.field_repair_counters().verdicts_learned, 1);
}

/// Mutation check: drop the closed-set membership check in the parser's
/// accepted-value fill -> red (the outside value fills a member and the
/// request is rewritten and served).
#[tokio::test]
async fn a_rejection_naming_a_value_outside_the_closed_set_repairs_nothing() {
    let (router, seats) = rewrite_chain(&[RewriteAnswer::Name(&["routectl-test-not-a-member"])]);

    let dispatched = router
        .complete_with_options(req_with_rewrite_field(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "no repair, so the rejection reaches the client",
    );
    assert_eq!(seats[0].calls(), 1, "no repaired retry was dispatched");
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(dispatched.meta.field_repair.is_none());
    assert_eq!(router.field_repair_counters().repair_attempted, 0);
}

/// Mutation check: let the accepted-value fill take the first of several
/// named members -> red (the request is rewritten and a verdict commits).
#[tokio::test]
async fn a_rejection_naming_two_accepted_values_rewrites_nothing_and_commits_nothing() {
    let (router, seats) = rewrite_chain(&[RewriteAnswer::Name(super::TEST_REWRITE_ACCEPTED)]);
    assert_eq!(
        super::TEST_REWRITE_ACCEPTED.len(),
        2,
        "premise: the rejection names both members of the closed set",
    );

    let dispatched = router
        .complete_with_options(req_with_rewrite_field(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "the rejection reaches the client"
    );
    assert_eq!(
        seats[0].values(),
        vec![Some(CLIENT_REWRITE_VALUE.to_string())],
        "one attempt, carrying the client's own value: no rewrite",
    );
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert_eq!(router.field_repair_counters().repair_attempted, 0);
}

/// A rewrite and a drop draw ONE allowance: seat one rewrites, seat two
/// drops, and seat three's rewritable rejection finds the allowance spent.
///
/// Mutation check: skip the allowance check for a rewrite edit -> red (seat
/// three repairs too).
#[tokio::test]
async fn a_rewrite_and_a_drop_share_the_per_request_ceiling() {
    let (router, seats) = rewrite_chain(&[
        RewriteAnswer::RewriteThenWalk,
        RewriteAnswer::DropThenWalk,
        RewriteAnswer::RewriteThenWalk,
    ]);
    assert_eq!(
        usize::from(REPAIRS_PER_REQUEST),
        2,
        "premise: the third repair in this request is one past the ceiling",
    );
    let mut req = req_with_rewrite_field();
    req.user = Some(super::TEST_UNSUPPORTED_SENTINEL.to_string());

    let result = router.complete(req).await;

    assert!(result.is_err(), "no seat serves");
    let seat_one = seats[0].values();
    assert_eq!(
        seat_one.len(),
        2,
        "seat one: carried attempt plus one repair"
    );
    assert_eq!(
        seat_one[1].as_deref(),
        Some(super::TEST_REWRITE_ACCEPTED[0]),
        "seat one's repair was the rewrite",
    );
    let seat_two = seats[1].seen.lock().clone();
    assert_eq!(
        seat_two.len(),
        2,
        "seat two: carried attempt plus one repair"
    );
    assert!(
        !RefusedFieldSeat::carries(&seat_two[1]),
        "seat two's repair was the drop",
    );
    assert_eq!(
        seats[2].calls(),
        1,
        "seat three: its repairable rejection finds the shared allowance spent",
    );
}
