// What the ONE paid body carries on the wire: the exact seat's upstream id, the
// retained payload's field and value, both beta carriers on their own fields, the
// Claude Code classification, the background marker, and the profile's own
// `max_tokens` under both thinking shapes.
//
// An `include!`d FRAGMENT of `paid_probe_dial_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason --
// imports live in the host.

// ---------------------------------------------------------------------------
// The canonical body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_paid_body_carries_the_exact_seat_and_the_retained_payload() {
    // The body is built from the AUTHORIZATION's own seat and payload, so the
    // paid call asks the question the admitted request posed, of the account the
    // reservation was committed for.
    //
    // The wire model id rather than the nickname: this request goes straight to
    // the provider, past the alias/model resolution that would otherwise
    // translate it.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::Completed);
    let body = dial.provider.only_body();
    assert_eq!(
        body.model, UPSTREAM,
        "the body must name the seat's UPSTREAM wire id, not the nickname",
    );
    assert_eq!(
        body.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("summarized"),
        "the closed-table field under test must be present with the captured \
         value, or the call asks nothing about the field",
    );
    assert!(
        body.reasoning.is_some(),
        "the serializer drops the whole thinking object without a reasoning \
         state, so the field would never reach the wire",
    );
    assert!(
        !body.messages.is_empty(),
        "a completion needs at least one turn to be well-formed",
    );
}

#[tokio::test]
async fn the_paid_body_keeps_each_beta_source_on_its_own_carrier() {
    // Each source back onto ITS OWN carrier. The egress filters `anthropic_beta`
    // through `allowed_betas` and exempts `operator_betas`, so crossing them over
    // would either smuggle a filtered client flag past the allowlist or subject an
    // operator-pinned flag to it -- either way the paid call's header would differ
    // from the one under test.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    dialed(&dial).await;

    let body = dial.provider.only_body();
    assert_eq!(
        body.anthropic_beta,
        vec![CLIENT_BETA.to_string()],
        "the CLIENT set, alone, on the client carrier",
    );
    assert_eq!(
        body.routectl_internal.operator_betas,
        vec![OPERATOR_BETA.to_string()],
        "the OPERATOR set, alone, on the operator carrier",
    );
    // Stated as absences too, so a UNION on either carrier reds rather than
    // passing on the containment the two assertions above already imply.
    assert!(
        !body.anthropic_beta.contains(&OPERATOR_BETA.to_string()),
        "an operator flag on the client carrier would be filtered by an allowlist \
         it is exempt from",
    );
    assert!(
        !body
            .routectl_internal
            .operator_betas
            .contains(&CLIENT_BETA.to_string()),
        "a client flag on the operator carrier would bypass the very allowlist \
         that filtered it",
    );
}

#[tokio::test]
async fn the_paid_body_carries_the_origin_bit_and_the_background_marker() {
    // The originating classification drives the egress's `is_non_cc` call, and so
    // whether the Claude Code beta floor applies: a paid body classified
    // differently from the request it probes for would travel under a different
    // header. The background marker excludes this call from the CLIENT-traffic
    // census, which exists to report what clients send.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    dialed(&dial).await;

    let body = dial.provider.only_body();
    assert_eq!(
        body.routectl_internal.originating_claude_code_session,
        Some(true),
        "the captured origin bit must ride along, or the beta floor differs",
    );
    assert!(
        body.routectl_internal.background_probe,
        "routectl's own background traffic must be marked as such",
    );
}

// ---------------------------------------------------------------------------
// The allowance is the profile's, not the free path's default
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_paid_body_max_tokens_is_the_profiles_own_allowance_per_wire_shape() {
    // The two wire shapes need different minimum viable allowances, and the
    // profile is what derived them from the catalog-confirmed ceiling. The free
    // path's payload application fills a 2048 default when `max_tokens` is ABSENT,
    // so the profile's value must be set BEFORE it -- otherwise every paid body
    // would carry the free default and the derivation would be inert.
    for (shape, dial, expected) in [
        (
            "adaptive",
            Dial::adaptive(CompleteAnswer::Ok),
            ADAPTIVE_MIN_VIABLE_MAX_TOKENS,
        ),
        (
            "legacy",
            Dial::legacy(CompleteAnswer::Ok),
            LEGACY_MIN_VIABLE_MAX_TOKENS,
        ),
    ] {
        dialed(&dial).await;

        let body = dial.provider.only_body();
        assert_eq!(
            body.max_tokens,
            Some(expected),
            "{shape}: the body must carry the PROFILE's allowance",
        );
    }
    // Stated as distinct values, so a fixture change collapsing the two shapes
    // would not leave the case above asserting one number twice.
    assert_ne!(
        ADAPTIVE_MIN_VIABLE_MAX_TOKENS, LEGACY_MIN_VIABLE_MAX_TOKENS,
        "premise: the two shapes must size differently, or the case above \
         discriminates nothing",
    );
}

#[tokio::test]
async fn the_paid_body_allowance_is_never_the_free_paths_default() {
    // The specific overwrite this ordering exists to prevent, named rather than
    // left implicit in the numbers above: the free count-token path's 2048 must
    // not appear on a paid body under either shape.
    for (shape, dial) in [
        ("adaptive", Dial::adaptive(CompleteAnswer::Ok)),
        ("legacy", Dial::legacy(CompleteAnswer::Ok)),
    ] {
        dialed(&dial).await;

        let body = dial.provider.only_body();
        assert_ne!(
            body.max_tokens,
            Some(FREE_PROBE_DEFAULT_MAX_TOKENS),
            "{shape}: the free path's default overwrote the profile's allowance",
        );
    }
}

/// The free count-token path's `max_tokens` fill, spelled here so the case above
/// names the value it forbids.
///
/// A LITERAL rather than an import: the producing constant is private to the
/// field-repair module, and the assertion's subject is that this number does not
/// appear on a paid body -- which is a property of the paid path, not a fact to
/// be kept in step with the free one. The paired case above asserts the positive
/// direction against the profile's own constants, so a free-path retune shows up
/// there rather than silently satisfying this.
const FREE_PROBE_DEFAULT_MAX_TOKENS: u32 = 2048;

// ---------------------------------------------------------------------------
// Diagnostics carry no request content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_paid_dial_log_line_carries_payload_beta_or_response_content() {
    // This path spends money, so it is exactly what a diagnostic reaches for. The
    // captured field value and both beta sets are CLIENT-supplied, the seat names
    // an account, and the credential material sits behind the provider entry --
    // none of it may reach a log line that asked only whether a call was made.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    let (outcome, lines) =
        routectl_testkit::capture_lines(async { dial.router.run_paid_probe().await }).await;

    assert!(
        matches!(
            outcome,
            PaidProbePass::Dialed(PaidProbeDialOutcome::Completed)
        ),
        "premise: the pass must have dialed, or there are no lines to inspect: \
         {outcome:?}",
    );
    let rendered = lines.join("\n");
    // POSITIVE control: the pass must actually EMIT something, or "absent from
    // the output" is free and this test proves nothing.
    assert!(
        !lines.is_empty(),
        "premise: the paid path must emit at least one line for these assertions \
         to be about redaction",
    );
    for secret in [CLIENT_BETA, OPERATOR_BETA, "summarized", "literal:k"] {
        assert!(
            !rendered.contains(secret),
            "a paid-dial log line leaked {secret}: {rendered}",
        );
    }
}
