// Parser-unlocalized counter and its class gate.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

// ---------------------------------------------------------------------------
// The parser-unlocalized count
// ---------------------------------------------------------------------------

/// A field-ELIGIBLE rejection the parser localized no path from bumps the
/// am-I-flying-blind counter exactly once.
///
/// This is production's ONLY state today -- the envelope-to-path parser is
/// inert, so every real caller-shaped rejection lands here -- which is exactly
/// why the count matters: without it, "the parser knows no shape" and "no such
/// rejection arrived" read identically on the status surface.
#[tokio::test]
async fn a_field_eligible_rejection_the_parser_cannot_localize_counts_once() {
    // No injection in scope: the production-inert parser resolves nothing, so
    // this drives the real unlocalized path rather than a simulated one.
    let (router, _seen) = single_seat(ALIAS, Answer::AlwaysRejectField);
    assert_eq!(
        router.field_repair_counters().parser_unlocalized,
        0,
        "premise: nothing has been rejected yet",
    );

    let served = router.complete(req_on(ALIAS)).await;

    assert!(served.is_err(), "premise: the fixture seat rejects");
    assert_eq!(
        router.field_repair_counters().parser_unlocalized,
        1,
        "one field-eligible rejection no path was localized from counts once",
    );
}

/// A rejection the parser DID localize does not bump the blind counter.
///
/// The paired control, and the one that makes the count a signal rather than a
/// rejection tally: a counter that rose on every caller-shaped 4xx regardless
/// would report the parser as permanently blind even on the traffic it reads
/// perfectly.
///
/// `AlwaysRejectField` rather than `ServeRepaired` on purpose. A served repair
/// leaves the walk via the repair arm's `continue`, so the observer is never
/// REACHED and the test would pass without the predicate existing at all. Here
/// the repaired retry is rejected too, so a localized rejection genuinely
/// arrives at the observer and the predicate is what declines it.
#[tokio::test]
async fn a_localized_rejection_leaves_the_parser_blind_counter_untouched() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);

    let served = router.complete(req_on(ALIAS)).await;

    assert!(served.is_err(), "premise: every attempt is rejected");
    assert_eq!(
        seen.repairs(),
        1,
        "premise: the parser localized the field, so a repaired attempt was dispatched \
         and its own rejection reached the observer",
    );
    assert_eq!(
        router.field_repair_counters().parser_unlocalized,
        0,
        "a rejection the parser localized is not evidence of blindness",
    );
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
    let rejection = Error::upstream("p", 400, FIELD_REJECT_BODY);

    // The admitted class, as the control: without it, a predicate that refused
    // every class would satisfy the refusals below while making the counter dead.
    // Asserted with the injection in scope so the resolver genuinely answers --
    // the predicate is about the CLASS, and a `None` resolution would make the
    // whole thing true for the wrong reason.
    {
        let _injection = provisional::inject(REJECTED_PATH);
        assert!(
            !Router::rejection_localizes_no_field(
                &FailureClass::BadRequest,
                &rejection,
                ANTHROPIC_API_KIND,
            ),
            "premise: a BadRequest whose path the parser DID localize is not blindness",
        );
    }
    assert!(
        Router::rejection_localizes_no_field(
            &FailureClass::BadRequest,
            &rejection,
            ANTHROPIC_API_KIND,
        ),
        "and the same class with nothing localized IS blindness -- so the predicate \
         genuinely turns on the resolution for an admitted class",
    );

    // Every class that says nothing about the envelope is refused on the class
    // alone, whatever the resolver would answer.
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
        let _injection = provisional::inject(REJECTED_PATH);
        assert!(
            !Router::rejection_localizes_no_field(&class, &rejection, ANTHROPIC_API_KIND),
            "{class:?} says nothing about the request envelope, so it is not evidence \
             about the parser",
        );
    }
}
