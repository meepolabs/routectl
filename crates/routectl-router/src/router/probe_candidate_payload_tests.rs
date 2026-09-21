// What a recorded paid candidate RETAINS, driven through the real worker.
//
// An `include!`d FRAGMENT of `probe_worker_tests.rs`, not a module of its own:
// the host's imports stay in scope and every test here keeps its original fully
// qualified name. Carries no top-level `use` for that reason.

/// A provider whose `count_tokens` answers a well-formed ZERO -- the one shape
/// that spends a free step without settling, and therefore the only way to
/// exhaust the plan through the real worker.
///
/// Distinct from the shared `ZeroCountProvider` only in that it also RECORDS the
/// body it saw, which is how the retained payload is compared against what the
/// probe actually sent rather than against a value this test made up.
struct RecordingZeroCountProvider {
    seen: parking_lot::Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl Provider for RecordingZeroCountProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.seen.lock().push(req);
        Ok(TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

#[tokio::test]
async fn a_recorded_candidate_retains_the_exhausting_jobs_payload_and_incarnation() {
    // The paid body must ask the question the ADMITTED request posed, so the
    // candidate carries the payload the exhausting job ran under rather than
    // leaving a later stage to rebuild one from config.
    //
    // The retained payload is compared against the body the probe ACTUALLY sent,
    // not against a hand-written expectation: a fixture value could agree with a
    // default-constructed payload and the assertion would hold whether retention
    // worked or not. Both beta sources are non-empty for the same reason.
    let provider = Arc::new(RecordingZeroCountProvider {
        seen: parking_lot::Mutex::new(Vec::new()),
    });
    let router = super::probe_test_support::remote_router_with_model_beta_and_paid_cap(
        provider.clone(),
        "context-1m-2025-08-07",
        3,
    );
    let live = router.probe_incarnation();
    let mut admitted = grounding_request();
    admitted.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    admitted.anthropic_beta = vec!["interleaved-thinking-2025-05-14".to_string()];
    let _ = router.complete(admitted).await;

    assert_eq!(router.run_due_probes().await, 1);

    let probe = provider
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the worker must have dialed count_tokens");
    let candidates = router.paid_probe_candidates();
    assert_eq!(
        candidates.len(),
        1,
        "premise: the spent plan must have recorded exactly one candidate",
    );
    let candidate = &candidates[0];

    assert_eq!(
        candidate.incarnation, live,
        "the candidate must carry the incarnation the exhausting work ran under",
    );
    let payload = &candidate.payload;
    assert_eq!(
        payload.field_path(),
        GROUNDED_PATH,
        "the retained payload must name the field under test",
    );
    assert_eq!(payload.field_value(), "summarized");
    // Welded to the body the probe SENT, in both directions, so neither source
    // can be silently dropped or crossed onto the other carrier.
    assert_eq!(
        payload.client_betas(),
        probe.anthropic_beta.as_slice(),
        "the retained client betas must be the ones the probe put on \
         `anthropic_beta`",
    );
    assert_eq!(
        payload.operator_betas(),
        probe.routectl_internal.operator_betas.as_slice(),
        "and the retained operator floor the ones it put on `operator_betas`",
    );
    assert!(
        !payload.client_betas().is_empty(),
        "premise: the client source must be non-empty, or this comparison \
         holds for an unretained payload too",
    );
    assert!(
        !payload.operator_betas().is_empty(),
        "premise: the operator source must be non-empty for the same reason",
    );
    assert_eq!(
        payload.originating_claude_code_session(),
        probe
            .routectl_internal
            .originating_claude_code_session
            .expect("the probe body carries the classification"),
        "and the retained Claude-Code classification the one it travelled under",
    );
}

#[tokio::test]
async fn a_retired_incarnation_discards_every_recorded_candidate_with_its_payload() {
    // The payload is retained state, so it must be released on the same terms
    // the rest of a candidate is: a publication clears the list, because a
    // candidate from a retired incarnation describes router state that no longer
    // serves -- and its payload describes a request that will never be reasked.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider, 3);
    let _ = router.complete(grounding_request()).await;
    router.run_due_probes().await;
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "premise: a candidate with a payload must be on the list",
    );

    router.publish_probe_incarnation();

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a publication must clear the list, payloads included",
    );
}
