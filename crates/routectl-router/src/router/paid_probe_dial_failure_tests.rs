// How a paid dial's upstream failure is CLASSIFIED: one closed class per
// meaningful refusal shape, derived from the shared classifier, so the settlement
// stage never has to replay a paid call to recover what the first one established.
//
// An `include!`d FRAGMENT of `paid_probe_dial_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason --
// imports live in the host.

// ---------------------------------------------------------------------------
// THE mapping table: every current FailureClass variant, one expected row
// ---------------------------------------------------------------------------

/// Every `FailureClass` variant THIS BUILD KNOWS, and the paid class it must map
/// to.
///
/// THE table this build's settlement vocabulary is defined by. Written as data
/// rather than as a set of individual assertions for two reasons: a row per
/// variant means mutating any ONE production arm reds exactly one row (a grouped
/// arm would let a removed decision hide behind its neighbours), and the
/// coverage check below compares this table against a second enumeration rather
/// than against a reader's memory.
///
/// WHAT THIS INVENTORY CANNOT DO, stated because the opposite is the natural
/// assumption: `FailureClass` is `#[non_exhaustive]`, so neither the production
/// match nor anything here can be compile-time exhaustive over it, and BOTH the
/// table and the coverage list below are hand-written. A variant added upstream
/// therefore appears in NEITHER, and no test in this file goes red for it -- it
/// silently takes the production catch-all as `Unclassified`. That is the
/// fail-closed direction (never a capability verdict), but it is unannounced.
/// What these tests do catch is drift WITHIN the known inventory: a mapping
/// dropped, regrouped, or changed for a variant already listed.
fn paid_failure_table() -> Vec<(routectl_core::failure_class::FailureClass, PaidProbeFailure)> {
    use routectl_core::failure_class::FailureClass;
    vec![
        // The one class that is evidence about the FIELD.
        (
            FailureClass::FeatureUnsupported {
                capability: "thinking".to_string(),
            },
            PaidProbeFailure::CapabilityRejected,
        ),
        (FailureClass::Auth, PaidProbeFailure::AuthRejected),
        (FailureClass::RateLimited, PaidProbeFailure::Throttled),
        (FailureClass::Overloaded, PaidProbeFailure::Throttled),
        (FailureClass::ServerError, PaidProbeFailure::Transient),
        (FailureClass::NetworkError, PaidProbeFailure::Transient),
        // Reserved by the classifier and mapped anyway: a deadline is a fault
        // that got no verdict, so it reads transient rather than falling to the
        // unaudited catch-all the day the classifier starts producing it.
        (FailureClass::Timeout, PaidProbeFailure::Transient),
        (FailureClass::BadRequest, PaidProbeFailure::RequestRefused),
        // Both of these are properties of what ROUTECTL sent, so they read with
        // the malformed class -- never as a capability verdict, which is the one
        // reading a settlement would act on the field for.
        (
            FailureClass::ContextWindow,
            PaidProbeFailure::RequestRefused,
        ),
        (
            FailureClass::ContentPolicy,
            PaidProbeFailure::RequestRefused,
        ),
        (FailureClass::Unknown, PaidProbeFailure::Unclassified),
    ]
}

#[test]
fn every_failure_class_maps_to_its_stated_paid_class() {
    // The whole table, row by row, through the PRODUCTION mapping. Mutating any
    // single arm reds exactly its own row and names the class.
    for (class, expected) in paid_failure_table() {
        let mapped =
            crate::router::probe_failure_class::paid_failure_for_class_for_tests(class.clone());
        assert_eq!(
            mapped,
            expected,
            "{class:?} must map to {} -- it mapped to {}",
            expected.as_str(),
            mapped.as_str(),
        );
    }
}

#[test]
fn every_current_failure_class_has_an_explicit_paid_mapping() {
    // CONSISTENCY between two hand-written enumerations of the same inventory:
    // the mapping table above, and the variant list below. A mapping dropped from
    // the table for a variant still listed here reds; so does a variant removed
    // from this list while its table row remains (the count assertion).
    //
    // IT DOES NOT DISCOVER NEW VARIANTS. `FailureClass` is `#[non_exhaustive]`,
    // so nothing here is compile-time exhaustive over it and this list is
    // maintained by hand -- a variant added upstream is absent from BOTH
    // enumerations, so both agree and this test stays green while the production
    // catch-all quietly classifies it `Unclassified`. Fail-closed, but silent.
    // Auditing a core `FailureClass` addition means updating this file by hand;
    // no test here will prompt for it.
    use routectl_core::failure_class::FailureClass;
    let table = paid_failure_table();
    let covered = |probe: &FailureClass| {
        table
            .iter()
            .any(|(class, _)| std::mem::discriminant(class) == std::mem::discriminant(probe))
    };
    for variant in [
        FailureClass::RateLimited,
        FailureClass::Auth,
        FailureClass::BadRequest,
        FailureClass::ContentPolicy,
        FailureClass::ContextWindow,
        FailureClass::ServerError,
        FailureClass::Timeout,
        FailureClass::NetworkError,
        FailureClass::Overloaded,
        FailureClass::FeatureUnsupported {
            capability: String::new(),
        },
        FailureClass::Unknown,
    ] {
        assert!(
            covered(&variant),
            "{variant:?} is listed here but has no row in the paid mapping table; \
             it would take the fail-closed catch-all instead of its own mapping",
        );
    }
    assert_eq!(
        table.len(),
        11,
        "the table and this list must describe the same inventory: one row per \
         variant enumerated above",
    );
}

#[test]
fn no_current_failure_class_reaches_the_unaudited_catch_all() {
    // Each variant THIS FILE LISTS has its own arm in the production mapping,
    // rather than falling through to the catch-all that exists for variants this
    // build has never seen. `Unknown` is in the list and maps to `Unclassified`
    // by DECISION, which is why this asserts the arm EXISTS rather than asserting
    // its value (the table above pins that).
    //
    // Same limit as its siblings: the list is hand-written, so a core variant
    // added upstream is not checked here at all.
    use routectl_core::failure_class::FailureClass;
    let src = include_str!("probe_failure_class.rs");
    for variant in [
        "FailureClass::RateLimited =>",
        "FailureClass::Auth =>",
        "FailureClass::BadRequest =>",
        "FailureClass::ContentPolicy =>",
        "FailureClass::ContextWindow =>",
        "FailureClass::ServerError =>",
        "FailureClass::Timeout =>",
        "FailureClass::NetworkError =>",
        "FailureClass::Overloaded =>",
        "FailureClass::FeatureUnsupported { .. } =>",
        "FailureClass::Unknown =>",
    ] {
        assert!(
            src.contains(variant),
            "{variant} has no explicit arm in the paid mapping; it would reach \
             the catch-all meant for variants this build has never seen",
        );
    }
    // And the arms are UNGROUPED, so a mutation to one cannot hide behind a
    // sibling in the same arm. A grouped arm would spell `A | B =>`.
    let paid_mapping = src
        .split_once("pub(super) fn paid_failure_for_class(")
        .expect("the paid mapping must exist")
        .1
        .split_once("\n}")
        .expect("the paid mapping must terminate")
        .0;
    assert!(
        !paid_mapping.contains("| FailureClass::"),
        "the paid mapping groups two classes in one arm; removing one class's \
         decision would then red nothing: {paid_mapping}",
    );
    // Premise for the source scan: the compiler still agrees these are the
    // variants, so a rename upstream fails to build rather than silently making
    // every `contains` above vacuous.
    let _witness: FailureClass = FailureClass::Unknown;
}

// ---------------------------------------------------------------------------
// End to end: each upstream shape reaches its own class
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_upstream_refusal_shape_reaches_its_own_failure_class() {
    // The distinctions the settlement stage acts on, driven through real provider
    // errors rather than through the mapping alone. Each case names ONE call, and
    // the classes are asserted to be pairwise distinct below -- a flatten-to-one
    // mutation reds on the very first row that disagrees.
    //
    // A 400 with no recognized token is a MALFORMED request (routectl's own body
    // was refused), which is deliberately a different class from a capability
    // rejection: acting on the former as field evidence would mint a verdict from
    // a defect in what routectl sent.
    for (name, answer, expected) in [
        (
            "auth 401",
            CompleteAnswer::Failing(401),
            PaidProbeFailure::AuthRejected,
        ),
        (
            "rate limit 429",
            CompleteAnswer::Failing(429),
            PaidProbeFailure::Throttled,
        ),
        (
            "overloaded 529",
            CompleteAnswer::Failing(529),
            PaidProbeFailure::Throttled,
        ),
        (
            "server error 500",
            CompleteAnswer::Failing(500),
            PaidProbeFailure::Transient,
        ),
        (
            "transport status 0",
            CompleteAnswer::Failing(0),
            PaidProbeFailure::Transient,
        ),
        (
            "malformed 400",
            CompleteAnswer::Failing(400),
            PaidProbeFailure::RequestRefused,
        ),
    ] {
        let dial = Dial::legacy(answer);

        let outcome = dialed(&dial).await;

        assert_eq!(
            outcome,
            PaidProbeDialOutcome::ProviderFailed(expected),
            "{name} must classify as {}",
            expected.as_str(),
        );
        assert_eq!(
            dial.provider.calls(),
            1,
            "{name}: classification must cost no extra call",
        );
        assert_eq!(
            dial.fallback_provider.calls(),
            0,
            "{name}: nor reach another seat",
        );
    }
}

#[tokio::test]
async fn a_capability_rejection_is_its_own_class_distinct_from_a_malformed_request() {
    // THE distinction the paid class exists to draw, and the one a flattened
    // outcome would destroy: an upstream saying the FIELD is unsupported here is
    // evidence about the field, while a plain 400 says routectl's own body was
    // wrong. A settlement that could not tell them apart would have to replay the
    // paid call -- a second never-refundable unit -- to find out which it had.
    //
    // Driven through the CLASS rather than a provider double, for the same reason
    // the free path's sibling case is: the anthropic token table carries an EMPTY
    // `feature_unsupported` set (the class reaches this lane only via the
    // reasoning-replay path), so no upstream fixture on this kind can produce one.
    // A 400 fixture would classify as `BadRequest` and exercise a DIFFERENT arm
    // while appearing to pin this one -- which is exactly the vacuous shape the
    // end-to-end `malformed 400` row above pins separately.
    let capability = routectl_core::failure_class::FailureClass::FeatureUnsupported {
        capability: "thinking".to_string(),
    };

    let classified =
        crate::router::probe_failure_class::paid_failure_for_class_for_tests(capability);

    assert_eq!(
        classified,
        PaidProbeFailure::CapabilityRejected,
        "a capability rejection must reach its own class",
    );
    // THE CONTROL that makes the assertion about the distinction rather than about
    // one arm: the malformed shape must NOT land in the same class.
    assert_eq!(
        crate::router::probe_failure_class::paid_failure_for_class_for_tests(
            routectl_core::failure_class::FailureClass::BadRequest
        ),
        PaidProbeFailure::RequestRefused,
        "a malformed request must not read as evidence about the field",
    );
    assert_ne!(
        PaidProbeFailure::CapabilityRejected,
        PaidProbeFailure::RequestRefused,
        "the two must be distinguishable, or a settlement has to replay a paid \
         call to tell them apart",
    );
}

#[tokio::test]
async fn an_unmodelled_failure_class_fails_closed_as_unclassified() {
    // The `#[non_exhaustive]` catch-all. A class this build has not audited must
    // NOT read as a capability verdict -- that is the one reading a settlement
    // acts on the field for, so inheriting it by default would let a variant added
    // upstream mint a verdict by merely existing.
    //
    // `Unknown` stands in for that set: it is the one unaudited-shaped class this
    // build can reach, and it takes the same catch-all arm any future variant
    // takes.
    let err = routectl_core::Error::normalize_response("p1", "unparseable");
    let class = routectl_core::failure_class::classify(&err, Some("anthropic-api")).class;
    assert_eq!(
        class,
        routectl_core::failure_class::FailureClass::Unknown,
        "premise: this error must actually reach the unmodelled class",
    );

    assert_eq!(
        crate::router::probe_failure_class::classify_paid_probe_failure(&err),
        PaidProbeFailure::Unclassified,
        "an unaudited class must fail closed, never as a capability verdict",
    );
}

// ---------------------------------------------------------------------------
// The classes stay distinct, and the log stays closed
// ---------------------------------------------------------------------------

#[test]
fn every_paid_failure_class_has_a_distinct_closed_token() {
    // The closed set, enumerated exhaustively so a class added without a token --
    // or sharing one, which is a flatten by another route -- cannot land silently.
    let all = [
        PaidProbeFailure::CapabilityRejected,
        PaidProbeFailure::AuthRejected,
        PaidProbeFailure::Throttled,
        PaidProbeFailure::Transient,
        PaidProbeFailure::RequestRefused,
        PaidProbeFailure::Unclassified,
    ];
    for failure in all {
        match failure {
            PaidProbeFailure::CapabilityRejected
            | PaidProbeFailure::AuthRejected
            | PaidProbeFailure::Throttled
            | PaidProbeFailure::Transient
            | PaidProbeFailure::RequestRefused
            | PaidProbeFailure::Unclassified => {}
        }
    }

    let tokens: Vec<&str> = all.iter().copied().map(PaidProbeFailure::as_str).collect();
    let distinct: std::collections::BTreeSet<&str> = tokens.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        tokens.len(),
        "every class needs its own token, or a settlement cannot tell two \
         refusals apart in the log: {tokens:?}",
    );
    assert_eq!(distinct.len(), 6, "one token per class: {tokens:?}");
}

#[tokio::test]
async fn a_failure_log_line_carries_the_closed_token_and_no_upstream_text() {
    // The failure diagnostic is where an upstream's own message is most tempting,
    // and this path spends money -- so what is emitted is the closed token and
    // nothing else. The upstream body text is planted in the error and asserted
    // ABSENT, with the token's presence as the paired positive control.
    let dial = Dial::legacy(CompleteAnswer::Failing(500));

    let (outcome, lines) =
        routectl_testkit::capture_lines(async { dial.router.run_paid_probe().await }).await;

    assert!(
        matches!(
            outcome,
            PaidProbePass::Dialed(PaidProbeDialOutcome::ProviderFailed(
                PaidProbeFailure::Transient
            ))
        ),
        "premise: the pass must have failed transiently: {outcome:?}",
    );
    let rendered = lines.join("\n");
    // POSITIVE CONTROL: the closed token IS emitted, so the absence below is
    // about redaction rather than about a line that was never written.
    assert!(
        rendered.contains(PaidProbeFailure::Transient.as_str()),
        "the settlement line must carry the closed failure token: {rendered}",
    );
    assert!(
        !rendered.contains(UPSTREAM_ERROR_BODY),
        "a failure line must not carry the upstream's own message: {rendered}",
    );
}
