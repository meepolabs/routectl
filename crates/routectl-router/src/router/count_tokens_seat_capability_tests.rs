// Seat-capability group: which seats the count_tokens walk admits, and
// what it does with the ones it admits. The admission rule is the whole
// safety argument for walking -- a seat whose tokenizer differs from the
// caller's would answer a wrong number at HTTP 200 -- so each shape gets
// its own test. Imports live in the host `count_tokens_tests.rs`; do not
// add `use` lines here.

/// The predicate itself, over the two static facts a seat carries. Pins
/// that capability is never granted on kind alone: the Bedrock arm must
/// confront the model id, and an id that proves no vendor is refused.
#[test]
fn seat_capability_requires_an_anthropic_family_upstream_for_bedrock() {
    assert!(seat_can_count_tokens(
        Some("anthropic-api"),
        "claude-haiku-4-5"
    ));
    assert!(seat_can_count_tokens(
        Some("bedrock"),
        "us.anthropic.claude-haiku-4-5-20251001-v1:0"
    ));
    assert!(!seat_can_count_tokens(
        Some("bedrock"),
        "us.meta.llama4-scout-17b-instruct-v1:0"
    ));
    assert!(!seat_can_count_tokens(
        Some("bedrock"),
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/some-profile"
    ));
    assert!(!seat_can_count_tokens(
        Some("openai-compat"),
        "us.anthropic.claude-haiku-4-5-20251001-v1:0"
    ));
    assert!(!seat_can_count_tokens(
        None,
        "us.anthropic.claude-haiku-4-5-20251001-v1:0"
    ));
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_seat_on_an_anthropic_model_serves_the_count() {
    // Arrange: chain [bedrock] alone, on an Anthropic-family model id.
    // The seat's tokenizer is the caller's, so the walk must admit and
    // dispatch to it.
    let (router, counters) = build_router(vec![Leg {
        nickname: "bedrock-claude",
        provider_name: "bedrock-prov",
        entry: bedrock_entry(),
        behavior: CountBehavior::Ok(31),
        upstream: Some(ANTHROPIC_BEDROCK_MODEL),
    }]);

    // Act
    let tc = router
        .count_tokens(count_req())
        .await
        .expect("an Anthropic-family bedrock seat is count_tokens-capable");

    // Assert
    assert_eq!(tc.input_tokens, 31);
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "the admitted bedrock seat must be dispatched to exactly once",
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_seat_on_a_non_anthropic_model_is_skipped_and_anthropic_counts() {
    // Arrange: chain [bedrock-on-llama, anthropic-api]. Admitting the
    // bedrock seat would answer a count from a different tokenizer than
    // the caller's request bills against -- a wrong number at HTTP 200 --
    // so it must be skipped with NO upstream call at all.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "bedrock-llama",
            provider_name: "bedrock-prov",
            entry: bedrock_entry(),
            behavior: CountBehavior::Ok(999),
            upstream: Some(NON_ANTHROPIC_BEDROCK_MODEL),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);

    // Act
    let tc = router
        .count_tokens(count_req())
        .await
        .expect("the anthropic-api seat serves the count");

    // Assert
    assert_eq!(
        tc.input_tokens, 42,
        "the count must come from the Anthropic seat, never the llama seat",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "a non-Anthropic-family bedrock seat must never be dispatched to",
    );
    assert_eq!(counters[1].load(Ordering::SeqCst), 1);
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_seat_on_an_inference_profile_arn_yields_a_clean_501() {
    // Arrange: chain [bedrock-on-arn] alone. An ARN proves no vendor, so
    // the tokenizer behind it is unknown; callers size context windows
    // with this number, making a clean 501 the better answer than a
    // plausible wrong count.
    let (router, counters) = build_router(vec![Leg {
        nickname: "bedrock-arn",
        provider_name: "bedrock-prov",
        entry: bedrock_entry(),
        behavior: CountBehavior::Ok(777),
        upstream: Some(ARN_BEDROCK_MODEL),
    }]);

    // Act
    let err = router.count_tokens(count_req()).await.unwrap_err();

    // Assert
    match err {
        Error::NotImplemented(model, msg) => {
            assert_eq!(model, "alias");
            assert!(
                msg.contains("count_tokens"),
                "message must name the operation; got: {msg}",
            );
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "an unprovable model family must be refused before any upstream call",
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_capability_error_advances_the_walk_without_debiting_the_breaker() {
    // Arrange: chain [bedrock-on-claude(capability error), anthropic-api].
    // A seat whose upstream cannot count surfaces a capability error,
    // which is health-neutral: the walk must advance AND the seat's
    // failure count must stay untouched. The breaker threshold is 1, so a
    // single debit would trip it to Open -- Closed proves the count never
    // incremented.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "bedrock-claude",
            provider_name: "bedrock-prov",
            entry: bedrock_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::NotImplemented,
            upstream: Some(ANTHROPIC_BEDROCK_MODEL),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);

    // Act
    let tc = router
        .count_tokens(count_req())
        .await
        .expect("the walk must advance past the capability error");

    // Assert
    assert_eq!(tc.input_tokens, 42, "the next capable seat serves");
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "the bedrock seat is dispatched to exactly once",
    );
    assert_eq!(counters[1].load(Ordering::SeqCst), 1);
    assert_eq!(
        circuit_phase(&router, "bedrock-claude"),
        crate::runtime_state::CircuitPhase::Closed,
        "a capability error must not debit the seat's breaker (threshold 1 \
         would have tripped it to Open on a single debit)",
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn a_bedrock_404_terminates_the_walk_instead_of_visiting_the_next_seat() {
    // Arrange: chain [bedrock-on-claude(404), anthropic-api(ok)]. A
    // Bedrock CountTokens 404 means the model resource was not found (an
    // end-of-life model id), returned by a region that serves the
    // operation for other models. It is NOT a capability signal, so the
    // 404 must reach the caller carrying AWS's own actionable message
    // rather than being swallowed by a walk past a capable seat.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "bedrock-claude",
            provider_name: "bedrock-prov",
            entry: bedrock_entry(),
            behavior: CountBehavior::UpstreamError(404),
            upstream: Some(ANTHROPIC_BEDROCK_MODEL),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);

    // Act
    let err = router.count_tokens(count_req()).await.unwrap_err();

    // Assert
    assert!(
        matches!(err, Error::Upstream { status: 404, .. }),
        "the 404 must surface verbatim; got {err:?}",
    );
    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "a 404 must not be treated as a capability miss -- walking on to \
         the next seat would hide an end-of-life model id behind a count \
         served by a different seat",
    );
}
