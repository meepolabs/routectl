// The operator ceiling, the serializer-specific lane gate, and what the derived
// allowance is worth when the REAL Anthropic serializer renders it.
//
// An `include!`d FRAGMENT of `paid_probe_profile_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// The operator ceiling folds into the same viability constraint
// ---------------------------------------------------------------------------

#[test]
fn a_configured_ceiling_below_the_catalog_ceiling_becomes_the_effective_one() {
    // Precedence: the EFFECTIVE ceiling is the lower of the two confirmed
    // ceilings, because both bind the request the probe would send.
    let profile = profile_with_configured_ceiling(fully_priced(), false, 8_000)
        .expect("8000 is far above the legacy floor");

    assert_eq!(profile.output_ceiling_tokens(), 8_000);
    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

#[test]
fn a_catalog_ceiling_below_the_configured_ceiling_stays_the_effective_one() {
    // The other direction of the same min: a configured ceiling never RAISES a
    // catalog one, so an operator cannot opt out of the catalog's confirmation.
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), Some(4_000)));

    let profile = profile_with_configured_ceiling(cell, false, 60_000).expect("4000 is viable");

    assert_eq!(profile.output_ceiling_tokens(), 4_000);
}

#[test]
fn an_absent_configured_ceiling_leaves_the_catalog_ceiling_effective() {
    // Zero is the production sentinel for "no operator override", never a real
    // ceiling of zero -- so it must not refuse and must not clamp.
    let profile = profile_with_configured_ceiling(fully_priced(), false, 0)
        .expect("an unset operator ceiling must not refuse");

    assert_eq!(profile.output_ceiling_tokens(), 64_000);
}

#[test]
fn a_configured_ceiling_exactly_at_the_legacy_floor_still_profiles() {
    let profile = profile_with_configured_ceiling(fully_priced(), false, 1025)
        .expect("a configured ceiling at the floor is viable");

    assert_eq!(profile.output_ceiling_tokens(), 1025);
    assert_eq!(profile.max_tokens(), 1025);
}

#[test]
fn a_configured_ceiling_one_below_the_legacy_floor_yields_no_profile() {
    // The catalog ceiling is 64000 here, so only the CONFIGURED one can refuse:
    // a derivation ignoring it would size a 1025-token body against a model the
    // operator capped at 1024.
    assert_eq!(
        profile_with_configured_ceiling(fully_priced(), false, 1024),
        None,
    );
}

#[test]
fn a_configured_ceiling_exactly_at_the_adaptive_floor_still_profiles() {
    let profile = profile_with_configured_ceiling(fully_priced(), true, 1)
        .expect("a configured ceiling at the adaptive floor is viable");

    assert_eq!(profile.output_ceiling_tokens(), 1);
    assert_eq!(profile.max_tokens(), 1);
}

// ---------------------------------------------------------------------------
// The floor is serializer-specific, so the lane must be too
// ---------------------------------------------------------------------------

#[test]
fn a_non_anthropic_provider_kind_yields_no_profile() {
    let router = router_built(fully_priced(), false, 0, ProviderKind::OpenaiCompat);

    assert_eq!(
        router.paid_probe_profile(&key()),
        None,
        "the derived floor describes one serializer, so another lane gets no profile",
    );
}

#[test]
fn a_real_anthropic_provider_kind_profiles_the_same_cell() {
    // The positive control for the guard above: identical cell, identical
    // ceiling, identical shape -- only the provider entry's kind differs.
    let router = router_built(fully_priced(), false, 0, ProviderKind::AnthropicApi);

    let profile = router
        .paid_probe_profile(&key())
        .expect("the acting lane must still profile");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

#[test]
fn an_absent_provider_entry_yields_no_profile() {
    // A reload can remove the entry under a resolved model, and a kind that
    // cannot be read is not the acting lane's kind.
    let mut router = router_built(fully_priced(), false, 0, ProviderKind::AnthropicApi);
    let mut config = (*router.config).clone();
    config.providers.remove("p1");
    router.config = Arc::new(config);

    assert_eq!(router.paid_probe_profile(&key()), None);
}

// ---------------------------------------------------------------------------
// The real serializer: what the derived allowance is worth on the wire
// ---------------------------------------------------------------------------

/// A body carrying the grounded closed-table field and `max_tokens`, normalized
/// by the REAL Anthropic egress at `adaptive`.
///
/// The serializer is the production `Provider::normalize_request`, not a
/// replica: the floor is a claim about what that code does at 1024 versus 1025,
/// and a hand-written body could only restate the claim.
fn wire_body(max_tokens: u32, adaptive: bool) -> serde_json::Value {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".to_string()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(max_tokens),
        reasoning: Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    req.routectl_internal.supports_adaptive_thinking = adaptive;
    let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
        routectl_providers::anthropic_api::AnthropicApiConfig::new("p1", "literal-key"),
    );
    egress
        .normalize_request(&req)
        .expect("the probe-shaped body must normalize")
}

#[test]
fn the_legacy_floor_is_the_smallest_max_tokens_the_real_serializer_keeps_thinking_at() {
    let profile = profile_for(fully_priced(), false).expect("fully priced");

    let at_floor = wire_body(profile.max_tokens(), false);

    assert_eq!(at_floor["max_tokens"], 1025);
    assert_eq!(at_floor["thinking"]["type"], "enabled");
    assert_eq!(at_floor["thinking"]["display"], "summarized");
}

#[test]
fn one_below_the_legacy_floor_loses_the_field_under_test_entirely() {
    // The control that makes the floor's value load-bearing rather than
    // decorative: at 1024 the real serializer drops the whole thinking object,
    // so a body sized there asks the upstream nothing about the field.
    let below = wire_body(LEGACY_MIN_VIABLE_MAX_TOKENS - 1, false);

    assert_eq!(below["max_tokens"], 1024);
    assert!(
        below.get("thinking").is_none(),
        "the serializer must drop thinking below the floor; got {below}"
    );
}

#[test]
fn the_adaptive_floor_still_carries_the_field_under_test_on_the_real_wire() {
    let profile = profile_for(fully_priced(), true).expect("fully priced");

    let at_floor = wire_body(profile.max_tokens(), true);

    assert_eq!(at_floor["max_tokens"], 1);
    assert_eq!(at_floor["thinking"]["type"], "adaptive");
    assert_eq!(at_floor["thinking"]["display"], "summarized");
}

#[test]
fn no_admitted_cell_ever_sizes_a_zero_output_allowance() {
    // Stated as a property of the DERIVATION, deliberately: this build makes no
    // claim about what the serializer emits at a zero allowance, so the guard is
    // that no admitted cell can produce one. Read off the PROFILE rather than
    // off the two constants, since an assertion on a constant is a tautology the
    // compiler evaluates rather than a statement about the derivation.
    //
    // Both shapes: the ceiling is the only input that could reach zero, and each
    // shape reads its own floor.
    for adaptive in [false, true] {
        let zero_ceiling = present(row(Some(3.0e-6), Some(1.5e-5), Some(0)));
        assert_eq!(
            profile_for(zero_ceiling, adaptive),
            None,
            "a zero ceiling must refuse rather than size a zero-output body",
        );

        let sized = profile_for(fully_priced(), adaptive).expect("fully priced");
        assert!(
            sized.max_tokens() > 0,
            "every sized allowance must be positive; got {}",
            sized.max_tokens(),
        );
    }
}
