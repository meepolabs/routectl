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

/// Serializes the typed request straight to a string. The egress body goes
/// through `serde_json::to_value` first, whose map silently collapses a
/// duplicate key, so only the typed serializer can expose one.
fn translate_to_wire_string(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> String {
    let r = translate(cfg, req).expect("translate");
    serde_json::to_string(&r).expect("serialize")
}

fn req_with_effort_and_summary(summary: Value) -> ChatRequest {
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });
    req.provider_extras = Some(json!({"reasoning": {"summary": summary}}));
    req
}

#[test]
fn reasoning_object_summary_serializes_one_summary_key() {
    // Arrange
    let req = req_with_effort_and_summary(json!({"mode": "x"}));

    // Act
    let wire = translate_to_wire_string(&cfg(), &req);

    // Assert: the caller's object rides alone; no "auto" default beside it.
    assert_eq!(wire.matches("\"summary\"").count(), 1, "wire: {wire}");
    assert!(wire.contains(r#""summary":{"mode":"x"}"#), "wire: {wire}");
    assert!(!wire.contains(r#""summary":"auto""#), "wire: {wire}");
}

#[test]
fn reasoning_null_summary_serializes_one_null_summary_key() {
    // Arrange: null is non-string, so it forwards verbatim via the flatten.
    let req = req_with_effort_and_summary(Value::Null);

    // Act
    let wire = translate_to_wire_string(&cfg(), &req);

    // Assert
    assert_eq!(wire.matches("\"summary\"").count(), 1, "wire: {wire}");
    assert!(wire.contains(r#""summary":null"#), "wire: {wire}");
}

#[test]
fn reasoning_string_summary_serializes_one_summary_key() {
    // Arrange
    let req = req_with_effort_and_summary(json!("concise"));

    // Act
    let wire = translate_to_wire_string(&cfg(), &req);

    // Assert
    assert_eq!(wire.matches("\"summary\"").count(), 1, "wire: {wire}");
    assert!(wire.contains(r#""summary":"concise""#), "wire: {wire}");
}

#[test]
fn reasoning_absent_summary_serializes_one_auto_summary_key() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let wire = translate_to_wire_string(&cfg(), &req);

    // Assert
    assert_eq!(wire.matches("\"summary\"").count(), 1, "wire: {wire}");
    assert!(wire.contains(r#""summary":"auto""#), "wire: {wire}");
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

// ---------------------------------------------------------------------------
// prompt_cache_options / prompt_cache_retention / prompt_cache_key
// ---------------------------------------------------------------------------

type DeclaredRequestEdit = (
    routectl_core::wire_check::EditSite,
    String,
    routectl_core::wire_check::EditKind,
);

/// Translate `req` under a wire tap, returning the emitted wire value and
/// every request-leg edit the translation declared.
fn translate_declaring(
    cfg: &OpenAiResponsesConfig,
    req: &ChatRequest,
) -> (Value, Vec<DeclaredRequestEdit>) {
    use routectl_core::wire_check::{Leg, WireTap};
    let mut req = req.clone();
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let wire = translate_to_json(cfg, &req);
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect();
    (wire, edits)
}

fn cache_options_value() -> Value {
    json!({"mode": "explicit", "ttl": "30m"})
}

/// A request carrying both prompt-cache fields as the Responses ingress
/// sweeps them, under `provenance`.
fn req_with_cache_fields(provenance: routectl_core::RequestProvenance) -> ChatRequest {
    let mut req = req_with(vec![user_text("hi")]);
    req.provider_extras = Some(json!({
        "prompt_cache_options": cache_options_value(),
        "prompt_cache_retention": "24h",
    }));
    req.routectl_internal.provenance = provenance;
    req
}

fn prompt_cache_surfaces() -> [(&'static str, OpenAiResponsesConfig); 3] {
    [
        ("api-key", cfg_api_key()),
        ("chatgpt-oauth", cfg()),
        ("bedrock-mantle", cfg_bedrock_mantle()),
    ]
}

#[test]
fn cache_fields_forward_per_surface() {
    use routectl_core::RequestProvenance;
    use routectl_core::wire_check::{EditKind, EditSite};
    // (surface, forwards options, forwards retention) for Responses ingress.
    let surface_rules = [
        ("api-key", true, true),
        ("chatgpt-oauth", true, false),
        ("bedrock-mantle", false, false),
    ];
    let provenances = [
        ("responses", RequestProvenance::ResponsesIngress),
        ("library", RequestProvenance::Library),
        ("chat", RequestProvenance::OpenaiIngress),
        ("anthropic", RequestProvenance::AnthropicIngress),
    ];
    let fields = [
        ("prompt_cache_options", cache_options_value()),
        ("prompt_cache_retention", json!("24h")),
    ];
    for ((surface, cfg), (rule_surface, options_ok, retention_ok)) in
        prompt_cache_surfaces().into_iter().zip(surface_rules)
    {
        assert_eq!(surface, rule_surface);
        for (provenance_name, provenance) in provenances {
            // Arrange
            let req = req_with_cache_fields(provenance);
            let is_responses = provenance == RequestProvenance::ResponsesIngress;

            // Act
            let (wire, edits) = translate_declaring(&cfg, &req);

            // Assert
            for ((field, value), surface_ok) in fields.iter().zip([options_ok, retention_ok]) {
                let row = format!("{surface} / {provenance_name} / {field}");
                let forwards = is_responses && surface_ok;
                if forwards {
                    assert_eq!(&wire[*field], value, "{row}: {wire}");
                } else {
                    assert!(wire.get(*field).is_none(), "{row}: {wire}");
                }
                let gate = (
                    EditSite::AuthSurfaceGate,
                    format!("/{field}"),
                    EditKind::Remove,
                );
                assert_eq!(
                    edits.contains(&gate),
                    is_responses && !surface_ok,
                    "{row}: the withhold is declared exactly when the surface gates it: {edits:?}"
                );
            }
        }
    }
}

#[test]
fn withheld_cache_fields_count_once_per_request() {
    use routectl_core::RequestProvenance;
    let withheld_rows = [
        (
            "chatgpt-oauth retention only",
            cfg(),
            json!({"prompt_cache_retention": "24h"}),
        ),
        (
            "chatgpt-oauth both",
            cfg(),
            json!({"prompt_cache_options": cache_options_value(), "prompt_cache_retention": "24h"}),
        ),
        (
            "bedrock-mantle both",
            cfg_bedrock_mantle(),
            json!({"prompt_cache_options": cache_options_value(), "prompt_cache_retention": "24h"}),
        ),
    ];
    for (row, cfg, extras) in withheld_rows {
        // Arrange
        let mut req = req_with(vec![user_text("hi")]);
        req.provider_extras = Some(extras);
        req.routectl_internal.provenance = RequestProvenance::ResponsesIngress;
        let before = responses_drop_count("prompt_cache_field_withheld");

        // Act
        let _ = translate_to_json(&cfg, &req);

        // Assert
        let after = responses_drop_count("prompt_cache_field_withheld");
        assert_eq!(after - before, 1, "{row}: one drop per request");
    }

    let forwarded_rows = [
        ("api-key both", cfg_api_key()),
        ("chatgpt-oauth options only", cfg()),
    ];
    for (row, cfg) in forwarded_rows {
        let mut req = req_with_cache_fields(RequestProvenance::ResponsesIngress);
        if row.ends_with("options only") {
            req.provider_extras = Some(json!({"prompt_cache_options": cache_options_value()}));
        }
        let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
            "openai-responses",
            "prompt_cache_field_withheld",
            || translate_to_json(&cfg, &req),
        );
        assert_eq!(wire["prompt_cache_options"], cache_options_value(), "{row}");
    }
}

/// The session-key fallback is withheld on Bedrock Mantle, whose acceptance
/// of `prompt_cache_*` is unrecorded; a client or operator key still
/// forwards there.
#[test]
fn prompt_cache_key_falls_back_to_hashed_session_key() {
    use routectl_core::wire_check::{EditKind, EditSite};
    const SESSION: &str = "codex-session-7f3e";
    // printf '%s' codex-session-7f3e | sha256sum
    const SESSION_SHA256: &str = "ad367646d8e3c2f215ee1396873c3992be45b0a036aa495163318406205e90a2";
    // printf 's%.0s' $(seq 256) | sha256sum
    const LONG_SESSION_SHA256: &str =
        "5dbb9529738ae602f76a348645d43c4ad1489c11b54f14ab8b983b735c101b23";
    let long_session = "s".repeat(256);

    struct Row<'a> {
        name: &'static str,
        client_key: Option<&'static str>,
        operator_key: Option<&'static str>,
        session: Option<&'a str>,
        expected: Option<&'a str>,
    }
    let rows = [
        Row {
            name: "client key wins",
            client_key: Some("client-key"),
            operator_key: None,
            session: Some(SESSION),
            expected: Some("client-key"),
        },
        Row {
            name: "operator payload_extras key wins",
            client_key: None,
            operator_key: Some("operator-key"),
            session: Some(SESSION),
            expected: Some("operator-key"),
        },
        Row {
            name: "session only",
            client_key: None,
            operator_key: None,
            session: Some(SESSION),
            expected: Some(SESSION_SHA256),
        },
        Row {
            name: "neither",
            client_key: None,
            operator_key: None,
            session: None,
            expected: None,
        },
        Row {
            name: "a 256-byte session key",
            client_key: None,
            operator_key: None,
            session: Some(&long_session),
            expected: Some(LONG_SESSION_SHA256),
        },
    ];
    for (surface, cfg) in prompt_cache_surfaces() {
        let derives_fallback = surface != "bedrock-mantle";
        for row in &rows {
            // Arrange
            let label = format!("{surface} / {}", row.name);
            let mut req = req_with(vec![user_text("hi")]);
            req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
            req.routectl_internal.inbound_session_key = row.session.map(str::to_string);
            let mut extras = serde_json::Map::new();
            if let Some(key) = row.client_key {
                extras.insert("prompt_cache_key".into(), json!(key));
            }
            if let Some(key) = row.operator_key {
                // The router merges the operator layer into provider_extras
                // and keeps a copy of that layer alone.
                extras.insert("prompt_cache_key".into(), json!(key));
                req.routectl_internal.operator_payload_extras =
                    Some(std::sync::Arc::new(json!({"prompt_cache_key": key})));
            }
            if !extras.is_empty() {
                req.provider_extras = Some(Value::Object(extras));
            }

            let before = responses_drop_count("prompt_cache_field_withheld");

            // Act
            let (wire, edits) = translate_declaring(&cfg, &req);

            // Assert
            let after = responses_drop_count("prompt_cache_field_withheld");
            assert_eq!(after, before, "{label}: no client field was withheld");
            let key = wire.get("prompt_cache_key").and_then(Value::as_str);
            let falls_back =
                row.client_key.is_none() && row.operator_key.is_none() && row.session.is_some();
            let hashed = falls_back && derives_fallback;
            if falls_back && !derives_fallback {
                assert_eq!(key, None, "{label}: no derived key on this surface: {wire}");
            } else if hashed {
                let key = key.unwrap_or_else(|| panic!("{label}: no key: {wire}"));
                assert_eq!(key.len(), 64, "{label}");
                assert!(
                    key.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "{label}: lowercase hex: {key}"
                );
                assert_eq!(Some(key), row.expected, "{label}");
                let session = row.session.expect("hashed rows carry a session");
                assert!(
                    !wire.to_string().contains(session),
                    "{label}: the raw session key reached the body"
                );
            } else {
                assert_eq!(key, row.expected, "{label}: {wire}");
            }
            let declared = (
                EditSite::CachePlacement,
                "/prompt_cache_key".to_string(),
                EditKind::Add,
            );
            assert_eq!(edits.contains(&declared), hashed, "{label}: {edits:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// request.rs -- per-block prompt_cache_breakpoint placement
// ---------------------------------------------------------------------------

fn breakpoint_value() -> Value {
    json!({"mode": "explicit"})
}

/// Ordinals 0 (`alpha`), 1 (`beta`) and 2 (`gamma`): the assistant turn
/// between them counts none, so `gamma` is `input[2].content[1]`.
fn breakpoint_conversation() -> Vec<Message> {
    vec![
        user_text("alpha"),
        assistant_text("reply"),
        user_parts(vec![text_part("beta"), text_part("gamma")]),
    ]
}

fn recorded_breakpoint(
    ordinal: Option<usize>,
    text: &str,
    client_pointer: &str,
) -> routectl_core::ResponsesCacheBreakpoint {
    routectl_core::ResponsesCacheBreakpoint {
        ordinal,
        text_digest: super::extras::breakpoint_text_digest(text),
        value: breakpoint_value(),
        client_pointer: client_pointer.into(),
    }
}

fn req_with_breakpoints(
    messages: Vec<Message>,
    breakpoints: Vec<routectl_core::ResponsesCacheBreakpoint>,
) -> ChatRequest {
    let mut req = req_with(messages);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
    req.routectl_internal.responses_cache_breakpoints = breakpoints;
    req
}

/// Every `(input index, content index)` whose block carries a breakpoint.
fn breakpoint_sites(wire: &Value) -> Vec<(usize, usize)> {
    let mut sites = Vec::new();
    for (item_index, item) in wire["input"].as_array().expect("input").iter().enumerate() {
        let Some(blocks) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("prompt_cache_breakpoint").is_some() {
                sites.push((item_index, block_index));
            }
        }
    }
    sites
}

#[test]
fn breakpoint_reattaches_on_ordinal_and_digest_match() {
    // Arrange
    let req = req_with_breakpoints(
        breakpoint_conversation(),
        vec![recorded_breakpoint(Some(2), "gamma", "/input/2/content/1")],
    );

    // Act
    let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        "prompt_cache_field_withheld",
        || translate_to_json(&cfg_api_key(), &req),
    );

    // Assert
    assert_eq!(wire["input"][2]["content"][1]["text"], "gamma", "{wire}");
    assert_eq!(
        wire["input"][2]["content"][1]["prompt_cache_breakpoint"],
        breakpoint_value(),
        "{wire}"
    );
    assert_eq!(
        breakpoint_sites(&wire),
        vec![(2, 1)],
        "only block 2 is marked"
    );
}

#[test]
fn breakpoints_on_text_only_content_and_after_a_tool_turn_reattach() {
    // Arrange: a flat-text user turn counts one ordinal and a tool turn
    // counts none, mirroring the ingress ordinal rule.
    let messages = vec![
        user_text("alpha"),
        tool_message("call_1", "tool says"),
        user_text("beta"),
    ];
    let req = req_with_breakpoints(
        messages,
        vec![
            recorded_breakpoint(Some(0), "alpha", "/input/0/content/0"),
            recorded_breakpoint(Some(1), "beta", "/input/2/content/0"),
        ],
    );

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    assert_eq!(breakpoint_sites(&wire), vec![(0, 0), (2, 0)], "{wire}");
}

#[test]
fn unplaced_breakpoints_count_once_and_declare() {
    use routectl_core::wire_check::{EditKind, EditSite};
    struct Row {
        name: &'static str,
        cfg: OpenAiResponsesConfig,
        breakpoints: Vec<routectl_core::ResponsesCacheBreakpoint>,
        site: EditSite,
    }
    let rows = [
        Row {
            name: "text rewritten (digest mismatch)",
            cfg: cfg_api_key(),
            breakpoints: vec![recorded_breakpoint(Some(2), "GAMMA", "/input/2/content/1")],
            site: EditSite::DialectFieldStrip,
        },
        Row {
            name: "ordinal shifted",
            cfg: cfg_api_key(),
            breakpoints: vec![recorded_breakpoint(Some(1), "gamma", "/input/2/content/1")],
            site: EditSite::DialectFieldStrip,
        },
        Row {
            name: "lifted (None)",
            cfg: cfg_api_key(),
            breakpoints: vec![
                recorded_breakpoint(None, "rules", "/input/0/content/0"),
                recorded_breakpoint(None, "more rules", "/input/1/content/0"),
            ],
            site: EditSite::DialectFieldStrip,
        },
        Row {
            name: "chatgpt-oauth surface",
            cfg: cfg(),
            breakpoints: vec![
                recorded_breakpoint(Some(0), "alpha", "/input/0/content/0"),
                recorded_breakpoint(Some(2), "gamma", "/input/2/content/1"),
            ],
            site: EditSite::AuthSurfaceGate,
        },
        Row {
            name: "bedrock-mantle surface",
            cfg: cfg_bedrock_mantle(),
            breakpoints: vec![recorded_breakpoint(Some(2), "gamma", "/input/2/content/1")],
            site: EditSite::AuthSurfaceGate,
        },
    ];
    for row in rows {
        // Arrange
        let req = req_with_breakpoints(breakpoint_conversation(), row.breakpoints.clone());
        let before = responses_drop_count("prompt_cache_field_withheld");

        // Act
        let (wire, edits) = translate_declaring(&row.cfg, &req);

        // Assert
        let after = responses_drop_count("prompt_cache_field_withheld");
        assert_eq!(after - before, 1, "{}: one drop per request", row.name);
        assert!(
            breakpoint_sites(&wire).is_empty(),
            "{}: no block is marked: {wire}",
            row.name
        );
        let removes: Vec<_> = edits
            .iter()
            .filter(|(_, path, _)| path.ends_with("/prompt_cache_breakpoint"))
            .cloned()
            .collect();
        let expected: Vec<_> = row
            .breakpoints
            .iter()
            .map(|b| {
                (
                    row.site,
                    format!("{}/prompt_cache_breakpoint", b.client_pointer),
                    EditKind::Remove,
                )
            })
            .collect();
        assert_eq!(removes, expected, "{}: one Remove per entry", row.name);
    }
}

#[test]
fn breakpoints_from_another_ingress_are_ignored() {
    // Arrange: only the Responses ingress records breakpoints; a sidecar on
    // any other provenance is not acted on.
    let mut req = req_with_breakpoints(
        breakpoint_conversation(),
        vec![recorded_breakpoint(Some(2), "gamma", "/input/2/content/1")],
    );
    req.routectl_internal.provenance = routectl_core::RequestProvenance::Library;

    // Act
    let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        "prompt_cache_field_withheld",
        || translate_to_json(&cfg_api_key(), &req),
    );

    // Assert
    assert!(breakpoint_sites(&wire).is_empty(), "{wire}");
}

#[test]
fn top_level_and_per_block_withholds_share_one_count() {
    // Arrange: chatgpt-oauth withholds both the retention field and the
    // breakpoint.
    let mut req = req_with_breakpoints(
        breakpoint_conversation(),
        vec![recorded_breakpoint(Some(2), "gamma", "/input/2/content/1")],
    );
    req.provider_extras = Some(json!({"prompt_cache_retention": "24h"}));
    let before = responses_drop_count("prompt_cache_field_withheld");

    // Act
    let (wire, edits) = translate_declaring(&cfg(), &req);

    // Assert
    let after = responses_drop_count("prompt_cache_field_withheld");
    assert_eq!(after - before, 1, "one drop for the whole request");
    assert!(wire.get("prompt_cache_retention").is_none(), "{wire}");
    assert!(breakpoint_sites(&wire).is_empty(), "{wire}");
    assert_eq!(
        edits
            .iter()
            .filter(|(_, _, kind)| *kind == routectl_core::wire_check::EditKind::Remove)
            .filter(|(_, path, _)| path.contains("prompt_cache"))
            .count(),
        2,
        "both withholds are declared: {edits:?}"
    );
}

#[test]
fn a_breakpoint_withhold_counts_when_translation_then_fails() {
    // Arrange: an image_url that is not an object fails the input build.
    let mut messages = breakpoint_conversation();
    messages.push(user_parts(vec![ContentPart::Known(
        KnownContentPart::ImageUrl {
            image_url: json!("not-an-object"),
            cache_control: None,
            extras: serde_json::Map::new(),
        },
    )]));
    let req = req_with_breakpoints(
        messages,
        vec![recorded_breakpoint(Some(2), "gamma", "/input/2/content/1")],
    );
    let before = responses_drop_count("prompt_cache_field_withheld");

    // Act
    let err = translate_err(&cfg_api_key(), &req);

    // Assert
    let after = responses_drop_count("prompt_cache_field_withheld");
    assert!(err.contains("image_url"), "unexpected error: {err}");
    assert_eq!(after - before, 1, "the withhold counts despite the failure");
}

/// A Responses-dialect unmodeled part carrying a breakpoint of its own.
fn unmodeled_part_with_breakpoint(type_tag: &str, fields: Value) -> ContentPart {
    let mut extras = fields.as_object().cloned().unwrap_or_default();
    extras.insert("prompt_cache_breakpoint".into(), breakpoint_value());
    ContentPart::Other {
        type_tag: type_tag.into(),
        cache_control: None,
        extras,
    }
}

/// A breakpoint on a block the egress forwards as written rides inside that
/// block, so the ApiKey-only rule is applied there: kept on api-key, taken
/// off, counted and declared on chatgpt-oauth, and never shipped by Mantle,
/// which drops an unmodeled part or rebuilds an `input_file` from its
/// carrier fields.
#[test]
fn breakpoint_on_a_forwarded_part_follows_the_surface_rule() {
    use routectl_core::wire_check::{EditKind, EditSite};
    struct Row {
        name: &'static str,
        cfg: OpenAiResponsesConfig,
        part: ContentPart,
        /// The wire block at `input[0].content[1]`, or `None` when the part
        /// does not ship.
        shipped: Option<Value>,
        withheld: bool,
    }
    let file_data = "data:application/pdf;base64,JVBER";
    let document = || unmodeled_part_with_breakpoint("input_document", json!({"doc": "d-1"}));
    let rows = [
        Row {
            name: "input_document on api-key",
            cfg: cfg_api_key(),
            part: document(),
            shipped: Some(json!({
                "type": "input_document",
                "doc": "d-1",
                "prompt_cache_breakpoint": breakpoint_value()
            })),
            withheld: false,
        },
        Row {
            name: "input_document on chatgpt-oauth",
            cfg: cfg(),
            part: document(),
            shipped: Some(json!({"type": "input_document", "doc": "d-1"})),
            withheld: true,
        },
        Row {
            name: "input_document on bedrock-mantle",
            cfg: cfg_bedrock_mantle(),
            part: document(),
            shipped: None,
            withheld: false,
        },
        Row {
            name: "input_file on bedrock-mantle",
            cfg: cfg_bedrock_mantle(),
            part: unmodeled_part_with_breakpoint("input_file", json!({"file_data": file_data})),
            shipped: Some(json!({"type": "input_file", "file_data": file_data})),
            withheld: true,
        },
    ];
    for row in rows {
        // Arrange
        let req = req_with_breakpoints(vec![user_parts(vec![text_part("look"), row.part])], vec![]);
        let before = responses_drop_count("prompt_cache_field_withheld");

        // Act
        let (wire, edits) = translate_declaring(&row.cfg, &req);

        // Assert
        let after = responses_drop_count("prompt_cache_field_withheld");
        assert_eq!(
            wire.pointer("/input/0/content/1"),
            row.shipped.as_ref(),
            "{}: {wire}",
            row.name
        );
        let ships_key = row
            .shipped
            .as_ref()
            .is_some_and(|block| block.get("prompt_cache_breakpoint").is_some());
        assert_eq!(
            wire.to_string().contains("prompt_cache_breakpoint"),
            ships_key,
            "{}: the key reaches the wire only inside the forwarded part: {wire}",
            row.name
        );
        assert_eq!(
            after - before,
            u64::from(row.withheld),
            "{}: withheld breakpoints count once per request",
            row.name
        );
        let gate = (
            EditSite::AuthSurfaceGate,
            "/input/*/content/*/prompt_cache_breakpoint".to_string(),
            EditKind::Remove,
        );
        assert_eq!(
            edits.contains(&gate),
            row.withheld,
            "{}: the withhold is declared exactly when made: {edits:?}",
            row.name
        );
    }
}
