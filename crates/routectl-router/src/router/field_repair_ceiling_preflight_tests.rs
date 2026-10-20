// A committed ceiling on a Bedrock Converse seat applied pre-flight on the
// next request, read off the body an upstream would receive.
// An `include!`d FRAGMENT of the `converse_ceiling` module in
// `field_repair_ceiling_tests.rs`.

/// Serve one request that repairs and commits a ceiling, then acknowledge its
/// durable write as the daemon does, so the verdict is pre-flight eligible.
async fn learn_and_acknowledge(router: &Router) -> crate::router::CapabilityLearnEvent {
    let (first, served) = dispatch(router, false, client_req(CLIENT_MAX_TOKENS)).await;
    assert!(served, "premise: the clamped retry serves");
    let event = first
        .learned_capabilities
        .first()
        .expect("premise: the repair committed a ceiling")
        .clone();
    assert!(
        router.acknowledge_durable_field_confirmation(
            &event.state_key,
            &event.capability_key,
            &event.provider_kind,
            event.persistence_generation,
            event.incarnation,
            event.observations,
        ),
        "premise: the durable write acknowledges the confirmation",
    );
    event
}

/// The pre-flight record this request planned for the ceiling row.
fn ceiling_record(meta: &crate::router::DispatchMeta) -> &crate::router::FieldPreflight {
    meta.field_preflight
        .iter()
        .find(|record| record.field_path == Some("max_tokens"))
        .expect("the request carried max_tokens, so the planner decided it")
}

/// The first request repairs and learns; the second goes out at the learned
/// ceiling with no rejection, on both dispatch surfaces.
///
/// Mutation check: make `preflight_edit` refuse a ceiling row again
/// (`FieldTransform::Ceiling => Err(FIELD_PREFLIGHT_NO_STORED_VALUE)`) -> red
/// (the second request ships the client's limit and draws an unrepaired
/// refusal, since reactive repair refuses a lane with an acting verdict).
#[tokio::test]
async fn the_request_after_a_committed_ceiling_goes_out_at_the_ceiling() {
    for stream in [false, true] {
        let seat = ConverseSeat::new(Seat::CapsThenServes);
        let router = converse_router(&[&seat]);
        let event = learn_and_acknowledge(&router).await;

        let (second, served) = dispatch(&router, stream, client_req(CLIENT_MAX_TOKENS)).await;

        assert!(served, "stream={stream}");
        assert_eq!(
            seat.sent_limits(),
            vec![
                json!(CLIENT_MAX_TOKENS),
                json!(NAMED_LIMIT),
                json!(NAMED_LIMIT)
            ],
            "stream={stream}: the second request is sent once, at the ceiling",
        );
        assert!(
            second.field_repairs.is_empty(),
            "stream={stream}: no rejection, so no reactive repair",
        );
        let record = ceiling_record(&second);
        assert!(record.acted, "stream={stream}");
        assert_eq!(
            record.reason,
            crate::router::field_preflight::FIELD_PREFLIGHT_ACTION_CEILING,
            "stream={stream}",
        );
        assert!(second.cleared_capabilities.is_empty(), "stream={stream}");
        assert!(second.learned_capabilities.is_empty(), "stream={stream}");
        assert_eq!(
            router
                .field_verdicts()
                .verdict_value(&event_key(&event), event.incarnation),
            Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
            "stream={stream}: a served pre-flighted request leaves the verdict as it was",
        );
    }
}

/// A client limit already at or below the learned ceiling ships unchanged.
#[tokio::test]
async fn a_client_limit_below_a_committed_ceiling_ships_unchanged() {
    const CLIENT_BELOW: u32 = 4096;
    let seat = ConverseSeat::new(Seat::CapsThenServes);
    let router = converse_router(&[&seat]);
    learn_and_acknowledge(&router).await;

    let (second, served) = dispatch(&router, false, client_req(CLIENT_BELOW)).await;

    assert!(served);
    assert_eq!(
        seat.sent_limits(),
        vec![
            json!(CLIENT_MAX_TOKENS),
            json!(NAMED_LIMIT),
            json!(CLIENT_BELOW)
        ],
        "the stored ceiling never raises a limit",
    );
    let record = ceiling_record(&second);
    assert!(!record.acted);
    assert_eq!(
        record.reason,
        crate::router::field_preflight::FIELD_PREFLIGHT_WITHIN_CEILING,
    );
}

/// A pre-flighted stream whose content precedes a later error neither confirms
/// nor disproves the resident ceiling: the verdict and its value stand, and
/// nothing is learned, cleared, or retried.
///
/// Mutation check: settle the field carry on a stream's first content by
/// clearing every resident verdict the request carries (make
/// `admit_provisional` admit an ACTING verdict, so `settle_success` clears it)
/// -> red (the verdict is cleared).
#[tokio::test]
async fn content_before_a_later_error_leaves_a_resident_ceiling_standing() {
    let seat = ConverseSeat::new(Seat::CapsThenStreamsThenRefuses);
    let router = converse_router(&[&seat]);
    let event = learn_and_acknowledge(&router).await;
    let key = event_key(&event);

    let opened = router
        .stream_with_options(client_req(CLIENT_MAX_TOKENS), RouterOptions::new())
        .await;
    let meta = opened.meta;
    let frames: Vec<Result<ChatChunk>> = opened
        .result
        .expect("content committed the stream")
        .collect()
        .await;

    assert!(frames.first().is_some_and(std::result::Result::is_ok));
    assert!(
        frames.last().is_some_and(std::result::Result::is_err),
        "premise: the post-content error ends the stream",
    );
    assert_eq!(
        seat.sent_limits(),
        vec![
            json!(CLIENT_MAX_TOKENS),
            json!(NAMED_LIMIT),
            json!(NAMED_LIMIT)
        ],
        "the stream went out pre-flighted and was not retried",
    );
    assert!(
        ceiling_record(&meta).acted,
        "premise: pre-flight clamped it"
    );
    assert!(meta.field_repairs.is_empty());
    assert!(meta.learned_capabilities.is_empty(), "nothing confirmed");
    assert!(meta.cleared_capabilities.is_empty(), "nothing disproved");
    assert_eq!(router.field_verdicts().snapshot_len(), 1);
    assert_eq!(
        router
            .field_verdicts()
            .verdict_value(&key, event.incarnation),
        Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
        "the resident ceiling stands",
    );
    assert!(
        router.field_verdicts().preflight_eligible(
            &key,
            router.registry_generation(),
            std::time::Instant::now(),
        ),
        "and is still applied to the next request",
    );
}
