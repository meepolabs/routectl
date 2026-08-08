// extras.rs coverage: the reasoning mapping (effort / budget / overlay /
// disable forms), the provider_extras allowlist, and the store + include
// lane contract. `include!`d into `request_tests.rs`; all top-level imports
// live there, so do not add `use` lines here.
//
// Holds the `store` lane pair -- `store_false_hardcoded_for_chatgpt_oauth` /
// `store_provider_extras_override_ignored_for_chatgpt_oauth` (ChatgptOauth, via
// `cfg()`) against `store_true_does_not_force_encrypted_reasoning_include`
// (ApiKey, via `cfg_api_key()`). Those lanes must stay together and adjacent:
// split apart, the negative assertions go vacuous and stop pinning the lane
// gate they exist for.

// ---------------------------------------------------------------------------
// extras.rs -- reasoning + provider_extras
// ---------------------------------------------------------------------------

#[test]
fn reasoning_effort_maps_to_responses_reasoning() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["reasoning"], json!({"effort": "high", "summary": "auto"}));
}

#[test]
fn reasoning_max_tokens_warns_and_drops() {
    // Arrange: caller supplied a budget. Effort still flows through.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: Some(2048),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: no budget field on the wire; effort survives.
    let r = &v["reasoning"];
    assert_eq!(r["effort"], "medium");
    assert!(r.get("max_tokens").is_none());
    assert!(r.get("budget_tokens").is_none());
}

#[test]
fn reasoning_budget_only_maps_to_effort_band() {
    // Arrange: caller supplied only a budget (no explicit effort).
    // 8192 sits in the medium band (1025..=8192) per the reverse table.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(8192),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: budget is mapped to "medium" rather than dropped.
    assert_eq!(
        v["reasoning"],
        json!({"effort": "medium", "summary": "auto"})
    );
}

#[test]
fn reasoning_explicit_effort_wins_over_budget() {
    // Arrange: both set. Explicit effort must win; budget is ignored
    // (it would map to the medium band but "high" takes precedence).
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: Some(8192),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["reasoning"]["effort"], "high");
}

#[test]
fn reasoning_overlay_effort_never_overrides_computed_effort() {
    // Arrange: a stray effort in the reasoning remainder (e.g. from
    // operator payload_extras) must NOT win over the computed canonical
    // effort -- the typed field is authoritative.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });
    req.provider_extras = Some(json!({"reasoning": {"effort": "low", "summary": "concise"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: computed "high" wins; the overlay effort is dropped.
    assert_eq!(
        v["reasoning"],
        json!({"effort": "high", "summary": "concise"})
    );
}

#[test]
fn reasoning_summary_verbosity_survives_roundtrip() {
    // Arrange: the ingress stashed a caller-set summary in the reasoning
    // remainder; the egress must emit it verbatim (not force "auto").
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });
    req.provider_extras = Some(json!({"reasoning": {"summary": "concise"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["reasoning"],
        json!({"effort": "high", "summary": "concise"})
    );
}

#[test]
fn reasoning_context_and_mode_survive_roundtrip() {
    // Arrange: context (closed enum) + mode (open string) ride through the
    // reasoning remainder onto the wire alongside the computed effort.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });
    req.provider_extras = Some(json!({"reasoning": {"context": "all_turns", "mode": "pro"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: summary defaults to auto (caller set none); context/mode carry.
    assert_eq!(
        v["reasoning"],
        json!({"effort": "medium", "summary": "auto", "context": "all_turns", "mode": "pro"})
    );
}

#[test]
fn reasoning_absent_summary_defaults_to_auto() {
    // Arrange: no caller summary, effort present -> summary defaults auto.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("low".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["reasoning"], json!({"effort": "low", "summary": "auto"}));
}

#[test]
fn reasoning_summary_only_still_emits_reasoning_object() {
    // Arrange: a summary-only request (no effort/enabled/budget) must still
    // emit a reasoning object -- the emission guard used to early-return.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"reasoning": {"summary": "detailed"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: no effort field (none computed); summary carries verbatim.
    assert_eq!(v["reasoning"], json!({"summary": "detailed"}));
}

#[test]
fn reasoning_context_only_still_emits_reasoning_object() {
    // Arrange: context-only request still emits reasoning with defaulted
    // summary.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"reasoning": {"context": "current_turn"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["reasoning"],
        json!({"summary": "auto", "context": "current_turn"})
    );
}

#[test]
fn reasoning_explicit_disable_omits_even_with_remainder() {
    // Arrange: canonical enabled:false wins -- reasoning is omitted even
    // though a summary rode along in provider_extras.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: None,
        exclude: None,
        enabled: Some(false),
    });
    req.provider_extras = Some(json!({"reasoning": {"summary": "concise"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(v.get("reasoning").is_none());
}

#[test]
fn reasoning_explicit_disable_omits_even_with_explicit_effort() {
    // Arrange: enabled:false paired with an explicit effort. The disable
    // wins unconditionally -- a computed effort must NOT resurrect the
    // reasoning object.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: Some(false),
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("reasoning").is_none(),
        "enabled:false must omit reasoning even with an explicit effort; got: {v}"
    );
}

#[test]
fn reasoning_explicit_disable_omits_even_with_budget() {
    // Arrange: enabled:false paired with a budget (which would otherwise map
    // to an effort band). The disable still wins and omits reasoning.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(8192),
        exclude: None,
        enabled: Some(false),
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("reasoning").is_none(),
        "enabled:false must omit reasoning even with a budget; got: {v}"
    );
}

#[test]
fn reasoning_explicit_disable_omits_even_with_context_overlay() {
    // Arrange: enabled:false paired with a context overlay (a Responses
    // sub-key riding in provider_extras). The disable beats the overlay.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: None,
        exclude: None,
        enabled: Some(false),
    });
    req.provider_extras = Some(json!({"reasoning": {"context": "all_turns"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("reasoning").is_none(),
        "enabled:false must omit reasoning even with an effort + overlay; got: {v}"
    );
}

#[test]
fn reasoning_none_effort_omits_reasoning_object() {
    // Arrange: effort "none" is reasoning-OFF. It must NOT clamp to a
    // positive level -- the Responses API has no disable token, so omitting
    // the reasoning object entirely is the disable form.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("none".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("reasoning").is_none(),
        "effort:none must omit reasoning entirely, not emit a positive effort; got: {v}"
    );
}

#[test]
fn reasoning_none_effort_beats_budget_and_overlay() {
    // Arrange: effort "none" paired with a budget (which would otherwise map
    // to an effort band) and a summary overlay (which would otherwise force a
    // reasoning object into existence). The explicit OFF wins over both.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("none".into()),
        max_tokens: Some(8192),
        exclude: None,
        enabled: None,
    });
    req.provider_extras = Some(json!({"reasoning": {"summary": "concise"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("reasoning").is_none(),
        "effort:none must beat a budget and an overlay; got: {v}"
    );
}

#[test]
fn reasoning_unknown_effort_passes_through_verbatim() {
    // Arrange: an unknown, intent-bearing token. It must travel verbatim
    // rather than resolve to a positive level of routectl's choosing --
    // clamping an unknown DOWN would silently invert caller intent.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("turbo".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["reasoning"]["effort"], "turbo",
        "an unknown effort token must pass through verbatim; got: {v}"
    );
}

#[test]
fn no_reasoning_controls_omits_reasoning_object() {
    // Arrange: no canonical reasoning and no remainder -> no reasoning key.
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(v.get("reasoning").is_none());
}

#[test]
fn provider_extras_prompt_cache_key_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"prompt_cache_key": "user-42"}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["prompt_cache_key"], "user-42");
}

#[test]
fn provider_extras_service_tier_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"service_tier": "priority"}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["service_tier"], "priority");
}

#[test]
fn provider_extras_text_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"text": {"verbosity": "high"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: text passthrough preserves the operator-supplied shape.
    assert_eq!(v["text"], json!({"verbosity": "high"}));
}

#[test]
fn provider_extras_include_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"include": ["reasoning.encrypted_content"]}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn provider_extras_unknown_key_does_not_forward() {
    // Arrange: long-tail key the egress doesn't recognize.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"frequency_penalty_v2": 0.5}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(v.get("frequency_penalty_v2").is_none());
}

#[test]
fn store_false_hardcoded_for_chatgpt_oauth() {
    // Arrange: default cfg uses ChatgptOauth.
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["store"], json!(false));
}

#[test]
fn store_provider_extras_override_ignored_for_chatgpt_oauth() {
    // Arrange: operator tries to flip store on -- must be ignored.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"store": true}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["store"], json!(false));
}

#[test]
fn store_false_forces_encrypted_reasoning_include() {
    // Arrange: default chatgpt-oauth, store false, no operator include.
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: include carries the encrypted-reasoning carrier so the
    // upstream returns a non-empty encrypted_content for later replay.
    assert_eq!(v["store"], json!(false));
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn store_true_does_not_force_encrypted_reasoning_include() {
    // Arrange: api-key path with an explicit store=true override (server
    // retains reasoning, so no include is needed).
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"store": true}));

    // Act
    let v = translate_to_json(&cfg_api_key(), &req);

    // Assert: store honored, include NOT force-added.
    assert_eq!(v["store"], json!(true));
    assert!(
        v.get("include").is_none(),
        "include must not be forced when store is true; got: {v}"
    );
}

/// The store guard reads the FINAL merged `provider_extras`. The router
/// deep-merges provider-level and model-level `payload_extras` into
/// `req.provider_extras` at dispatch, so a `store = true` arriving via that
/// merged path (the origin the config-time provider-level reject cannot
/// see) must still be forced inert on the mantle lane. Pins BOTH that the
/// store flag stays false AND that the encrypted-reasoning include still
/// fires (store=false keeps `finalize_reasoning_include` active).
#[test]
fn store_true_via_merged_extras_is_inert_on_mantle_lane() {
    // Arrange: simulate the merged dispatch value carrying store=true.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"store": true}));

    // Act
    let v = translate_to_json(&cfg_bedrock_mantle(), &req);

    // Assert: store forced false and the encrypted-reasoning carrier is on
    // the wire for later replay.
    assert_eq!(
        v["store"],
        json!(false),
        "mantle lane must force store=false even from merged extras; got: {v}"
    );
    assert_eq!(
        v["include"],
        json!(["reasoning.encrypted_content"]),
        "store=false must still force the encrypted-reasoning include; got: {v}"
    );
}

#[test]
fn explicit_operator_include_is_respected_not_overwritten() {
    // Arrange: operator pins include to a custom value; store false.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"include": ["message.output_text.logprobs"]}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: the operator value is honored verbatim (NOT augmented with
    // the encrypted-reasoning carrier).
    assert_eq!(v["include"], json!(["message.output_text.logprobs"]));
}
