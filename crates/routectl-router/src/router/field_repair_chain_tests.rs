// Repair chaining through the dispatch walks: a repaired retry the upstream
// refuses for a DIFFERENT field commits the earlier steps and repairs the new
// field on the same target; the same field, an inferred or absent-path
// attribution, a declared-edit undo, a target change and an exhausted budget
// each commit only what was proven. Seats run the real egress body assembly,
// so every assertion reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`, sharing the thinking
// seat helpers of `field_repair_thinking_type_tests.rs` and
// `field_repair_thinking_disabled_tests.rs`; its own seat, rejections and
// requests are in `field_repair_chain_support_tests.rs`.

mod repair_chain {
    use super::*;

    include!("field_repair_chain_support_tests.rs");
    include!("field_repair_chain_exit_tests.rs");

    // -----------------------------------------------------------------------
    // Two keys, one request, every walk
    // -----------------------------------------------------------------------

    /// Mutation check: put back the one-step-per-target gate in
    /// `try_field_repair` -> red (the max_tokens refusal reaches the client).
    /// Derive the moved-on commit's features from the refused attempt
    /// instead of the client's request -> red (the rewritten field is gone).
    #[tokio::test]
    async fn two_refused_fields_on_anthropic_api_complete_converge_in_one_request() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(two_key_req(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_two_key_bodies(&seat, "complete");
        assert_two_key_settled(&dispatched.meta, "complete");
        assert_eq!(router.field_verdicts().snapshot_len(), 2);
        assert_eq!(
            rejection_outcomes(&router),
            vec![crate::wire_check_state::FieldRepairOutcome::RepairedLossy; 2],
            "each refusal joins its own repair record",
        );
    }

    #[tokio::test]
    async fn two_refused_fields_converge_on_a_stream_before_content() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .stream_with_options(two_key_req(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_two_key_bodies(&seat, "stream");
        assert_two_key_settled(&dispatched.meta, "stream");
    }

    /// The count leg assembles the messages body, which carries both fields.
    #[tokio::test]
    async fn two_refused_fields_converge_on_a_settling_token_count() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let counted = router.count_tokens_with_meta(two_key_req()).await;

        assert!(counted.result.is_ok(), "{:?}", counted.result.err());
        assert_two_key_bodies(&seat, "count_tokens");
        assert_two_key_settled(&counted.meta, "count_tokens");
    }

    #[tokio::test]
    async fn a_non_settling_token_count_repairs_both_fields_and_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let counted = router.count_tokens(two_key_req()).await;

        assert!(counted.is_ok(), "{:?}", counted.err());
        assert_two_key_bodies(&seat, "non-settling count");
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            0,
            "nothing resident"
        );
        assert!(router.learned_capability_snapshot().is_empty());
    }

    // -----------------------------------------------------------------------
    // Three keys on openai-compat
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn three_refused_parameters_on_openai_compat_converge_in_one_request() {
        let seat = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&THREE_KEYS, body)
        });
        let router = chain_router(&[(OPENAI_COMPAT_TOML, &seat)], &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(compat_req(&THREE_KEYS), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 4, "three refusals and one served retry");
        for (idx, body) in bodies.iter().enumerate() {
            let carried: Vec<&str> = THREE_KEYS
                .into_iter()
                .filter(|key| body.get(*key).is_some())
                .collect();
            assert_eq!(carried, THREE_KEYS[idx..].to_vec(), "attempt {idx}: {body}");
        }
        assert_eq!(
            verdicts(&dispatched.meta),
            THREE_KEYS.map(|k| (key(k), None)).to_vec(),
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            THREE_KEYS.map(|k| (k, true, true)).to_vec(),
        );
    }

    // -----------------------------------------------------------------------
    // What commits nothing
    // -----------------------------------------------------------------------

    /// The client got `expected`'s message, nothing was learned, and the one
    /// repair record did not succeed.
    fn assert_nothing_committed(
        router: &Router,
        dispatched: &crate::router::Dispatched,
        expected: &Error,
        label: &str,
    ) {
        let err = dispatched
            .result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{label}: the refusal reaches the client"));
        assert_eq!(
            err.parsed_body().and_then(|p| p.error_message()),
            expected.parsed_body().and_then(|p| p.error_message()),
            "{label}: the client gets the latest refusal",
        );
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{label}: {:?}",
            verdicts(&dispatched.meta),
        );
        assert_eq!(router.field_verdicts().snapshot_len(), 0, "{label}");
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, false, false)],
            "{label}",
        );
    }

    /// Mutation check: drop the identity-difference condition in
    /// `moved_on_by` -> red (the refused rewrite commits the value it refused).
    #[tokio::test]
    async fn a_refusal_of_the_rewritten_value_stops_and_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, adaptive_refused)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert_eq!(seat.calls(), 2, "no second repair of the same field");
        assert_eq!(dispatched.meta.field_repairs.len(), 1);
        assert_nothing_committed(&router, &dispatched, &adaptive_refused(), "same field");
    }

    /// Mutation check: drop the self-identifying condition in `moved_on_by`
    /// -> red.
    #[tokio::test]
    async fn an_inferred_refusal_of_a_different_field_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, stop_refused_inferred)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
            .await;

        assert_eq!(
            seat.bodies()[1]["stop_sequences"],
            json!([STOP]),
            "premise: the named field went out, so only the tier refuses the commit",
        );
        assert_nothing_committed(&router, &dispatched, &stop_refused_inferred(), "inferred");
    }

    /// Mutation check: drop the `confirms_path_present` check in
    /// `settle_field_moved_on` -> red.
    #[tokio::test]
    async fn a_refusal_naming_a_path_the_body_lacked_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, absent_top_k_refused)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(
            seat.bodies()[1].get("top_k").is_none(),
            "premise: the body lacked the named path"
        );
        assert_nothing_committed(&router, &dispatched, &absent_top_k_refused(), "absent path");
    }

    /// The refused field went out, but the attempt left no record of the
    /// body, so nothing confirms the upstream read past the pending field.
    ///
    /// Mutation check: let `confirms_path_present` accept unknown presence
    /// (`!= Some(false)`) -> red.
    #[tokio::test]
    async fn a_refusal_whose_path_presence_is_unknown_commits_nothing() {
        let seat = ChainSeat::depositing(
            Egress::Anthropic,
            |body, _| disabled_then(body, stop_refused),
            1,
        );
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
            .await;

        assert_eq!(
            seat.bodies()[1]["stop_sequences"],
            json!([STOP]),
            "premise: the named field went out, only its record is missing",
        );
        assert_nothing_committed(&router, &dispatched, &stop_refused(), "unknown presence");
    }

    const OPERATOR_EXTRA: &str = "payload_extras = { metadata = { op_key = \"v\" } }\n";

    /// Refuse disabled thinking, then the operator's extra, then answer the
    /// undone body with a refusal naming nothing.
    fn disabled_then_extra(body: &serde_json::Value, _: usize) -> Option<Error> {
        if thinking_type(body) == Some("disabled") {
            return Some(anthropic_disabled_rejection());
        }
        if body["metadata"].get("op_key").is_some() {
            return Some(anthropic_rejection(
                "metadata.op_key: Extra inputs are not permitted",
            ));
        }
        Some(unlocalized_rejection())
    }

    /// The refusal of the operator's extra is answered by undoing that edit,
    /// so it says nothing about the pending field.
    ///
    /// Mutation check: move the moved-on check above `answer_declared` in
    /// `on_rejection` -> red.
    #[tokio::test]
    async fn a_refusal_a_declared_edit_answers_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, disabled_then_extra);
        let router = chain_router(
            &[(&format!("{ANTHROPIC_API_TOML}{OPERATOR_EXTRA}"), &seat)],
            &wire_checked(ONE_ATTEMPT),
        );
        let mut req = anthropic_req(4096, false);
        let mut client = disabled_client_body(None);
        client["metadata"] = json!({});
        req.provider_extras = Some(json!({"metadata": {}}));
        if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
            wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
        }

        let dispatched = router
            .complete_with_options(req, RouterOptions::new())
            .await;

        let bodies = seat.bodies();
        assert_eq!(
            bodies.len(),
            3,
            "refused, extra refused, undone and refused"
        );
        assert_eq!(bodies[1]["metadata"]["op_key"], json!("v"), "premise");
        assert!(
            bodies[2]["metadata"].get("op_key").is_none(),
            "premise: the undo answered the refusal: {}",
            bodies[2],
        );
        assert_nothing_committed(&router, &dispatched, &unlocalized_rejection(), "declared");
    }

    // -----------------------------------------------------------------------
    // A moved-on commit stands when the next field cannot be repaired
    // -----------------------------------------------------------------------

    /// Mutation check: read the remapped class instead of the native one in
    /// `settle_field_moved_on` -> red on `remapped` (an operator routing
    /// preference would decide what the upstream proved).
    #[tokio::test]
    async fn a_moved_on_refusal_of_a_field_with_no_row_commits_the_earlier_step() {
        let remap = "[providers.p0.class_overrides]\n400 = \"server-error\"\n";
        let rows = [("native", String::new()), ("remapped", remap.to_string())];
        for (name, provider_extra) in rows {
            let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
                disabled_then(body, stop_refused)
            });
            let provider = format!("{ANTHROPIC_API_TOML}{provider_extra}");
            let router = chain_router(&[(&provider, &seat)], &wire_checked(ONE_ATTEMPT));

            let dispatched = router
                .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
                .await;

            assert_eq!(seat.calls(), 2, "{name}");
            let err = dispatched.result.as_ref().expect_err("the refusal");
            assert_eq!(
                err.parsed_body().and_then(|p| p.error_message()),
                Some("stop_sequences: Extra inputs are not permitted"),
                "{name}: the latest refusal reaches the client",
            );
            assert_eq!(
                verdicts(&dispatched.meta),
                vec![(
                    thinking_disabled_key(),
                    Some(VerdictValue::Shape("thinking.type.adaptive")),
                )],
                "{name}",
            );
            assert_eq!(
                succeeded(&dispatched.meta),
                vec![(THINKING_DISABLED_PATH, true, true)],
                "{name}",
            );
        }
    }

    /// Four refused parameters on seat 0, a 503 there, and seat 1 refusing
    /// the first two: five draws in all, so seat 1's second field finds the
    /// budget empty.
    #[tokio::test]
    async fn exhaustion_across_two_seats_keeps_every_proven_step_committed() {
        let seat0 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&FOUR_KEYS, body).or_else(|| Some(unavailable()))
        });
        let seat1 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&FOUR_KEYS, body)
        });
        let router = chain_router(
            &[(OPENAI_COMPAT_TOML, &seat0), (OPENAI_COMPAT_TOML, &seat1)],
            &wire_checked(ONE_ATTEMPT),
        );

        let dispatched = router
            .complete_with_options(compat_req(&FOUR_KEYS), RouterOptions::new())
            .await;

        assert_eq!(seat0.calls(), 5, "four refusals and the 503");
        assert_eq!(seat1.calls(), 2, "one repair, then no budget");
        let err = dispatched.result.as_ref().expect_err("the refusal");
        assert_eq!(
            err.parsed_body().and_then(|p| p.error_param()),
            Some("top_p"),
            "the latest refusal reaches the client",
        );
        let lanes: Vec<(String, String)> = dispatched
            .meta
            .learned_capabilities
            .iter()
            .map(|event| (event.state_key.clone(), event.capability_key.clone()))
            .collect();
        let lane = |idx: usize| format!("p{idx}#{UNPROFILED_MODEL}");
        assert_eq!(
            lanes,
            vec![
                (lane(0), key("temperature")),
                (lane(0), key("top_p")),
                (lane(0), key("presence_penalty")),
                (lane(1), key("temperature")),
            ],
            "the step the 503 left pending on seat 0 is dropped",
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![
                ("temperature", true, true),
                ("top_p", true, true),
                ("presence_penalty", true, true),
                ("frequency_penalty", false, false),
                ("temperature", true, true),
            ],
        );
    }

    // -----------------------------------------------------------------------
    // Target change and same-target transient
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_pending_step_on_a_seat_left_for_a_fallback_commits_nothing() {
        let seat0 = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, unavailable)
        });
        let seat1 = ChainSeat::new(Egress::Anthropic, |_, _| None);
        let router = chain_router(
            &[(ANTHROPIC_API_TOML, &seat0), (ANTHROPIC_API_TOML, &seat1)],
            &wire_checked(ONE_ATTEMPT),
        );

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_eq!((seat0.calls(), seat1.calls()), (2, 1));
        assert_eq!(
            thinking_type(&seat1.bodies()[0]),
            Some("disabled"),
            "the fallback gets the client's own thinking",
        );
        assert!(verdicts(&dispatched.meta).is_empty());
        assert_eq!(router.field_verdicts().snapshot_len(), 0);
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, false, false)],
        );
    }

    /// The retried body is the repaired body, so its success proves the step.
    #[tokio::test]
    async fn a_pending_step_survives_a_same_target_transient_and_commits_on_success() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, position| {
            if thinking_type(body) == Some("disabled") {
                return Some(anthropic_disabled_rejection());
            }
            (position == 1).then(unavailable)
        });
        let router = anthropic_router(&seat, &wire_checked(THREE_ATTEMPTS));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_eq!(seat.calls(), 3, "refused, unavailable, served");
        assert_eq!(
            verdicts(&dispatched.meta),
            vec![(
                thinking_disabled_key(),
                Some(VerdictValue::Shape("thinking.type.adaptive")),
            )],
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, true, true)],
        );
    }
}
