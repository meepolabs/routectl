//! Client extras forward to the three Anthropic-shape egresses (anthropic-api,
//! Bedrock Invoke, the Converse bag) only from the Anthropic ingress or a
//! library caller. Another ingress dialect's keys are absent from the wire and
//! counted once per request as `foreign_dialect_extra` on the egress's lane;
//! translation reads and the operator's own `payload_extras` are unaffected.

use std::sync::Arc;

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{ChatRequest, Message, MessageContent, RequestProvenance, Role};
use serde_json::{Value, json};

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CLASS: &str = "foreign_dialect_extra";

/// One Anthropic-shape egress: its telemetry lane and how it builds the wire
/// value the extras land in (the whole body, or the Converse bag).
struct Egress {
    lane: &'static str,
    wire: fn(&ChatRequest) -> Value,
}

fn egresses() -> [Egress; 3] {
    [
        Egress {
            lane: super::super::LANE,
            wire: anthropic_body,
        },
        Egress {
            lane: crate::bedrock::invoke::LANE,
            wire: invoke_body,
        },
        Egress {
            lane: crate::bedrock::converse::LANE,
            wire: converse_bag,
        },
    ]
}

fn anthropic_body(req: &ChatRequest) -> Value {
    super::normalize("test", req, false, false, None, false, true).expect("anthropic body")
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-4-7".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn invoke_body(req: &ChatRequest) -> Value {
    crate::bedrock::invoke::normalize_request(&bedrock_cfg(BedrockApiShape::Invoke), req)
        .expect("invoke body")
}

fn converse_bag(req: &ChatRequest) -> Value {
    let body =
        crate::bedrock::converse::normalize_request(&bedrock_cfg(BedrockApiShape::Converse), req)
            .expect("converse body");
    body.get("additionalModelRequestFields")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn drop_count(lane: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.drop_class == CLASS)
        .map_or(0, |e| e.drop_count)
}

fn request(provenance: RequestProvenance, extras: Value) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        provider_extras: Some(extras),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req
}

/// The wire value plus the lane's `foreign_dialect_extra` delta across it.
fn wire_and_delta(egress: &Egress, req: &ChatRequest) -> (Value, u64) {
    let before = drop_count(egress.lane);
    let wire = (egress.wire)(req);
    (wire, drop_count(egress.lane) - before)
}

#[test]
fn foreign_dialect_extras_are_absent_from_every_anthropic_shape_egress_and_counted_once() {
    // Arrange -- the OpenAI-dialect fields a wire watch saw reach Claude, each
    // from a Chat and a Responses client. Each row sends the field beside a
    // second foreign key, so the count is once per request, not per key.
    let fields: [(&str, Value); 5] = [
        ("store", json!(true)),
        ("stream_options", json!({"include_usage": true})),
        ("include", json!(["reasoning.encrypted_content"])),
        ("prompt_cache_key", json!("zz-cache-key")),
        ("client_metadata", json!({"installation_id": "zz-install"})),
    ];
    let provenances = [
        RequestProvenance::OpenaiIngress,
        RequestProvenance::ResponsesIngress,
    ];

    for egress in egresses() {
        for provenance in provenances {
            for (field, value) in &fields {
                let row = format!("{field} on {} from {provenance:?}", egress.lane);
                let req = request(provenance, json!({*field: value, "zz_second_key": 1}));

                // Act
                let (wire, delta) = wire_and_delta(&egress, &req);

                // Assert
                assert!(wire.get(*field).is_none(), "{row}: {wire}");
                assert!(wire.get("zz_second_key").is_none(), "{row}: {wire}");
                assert_eq!(delta, 1, "{row}");
            }
        }
    }
}

#[test]
fn an_anthropic_ingress_context_management_still_forwards_to_every_anthropic_shape_egress() {
    // Arrange
    let edits = json!({"edits": [{"type": "clear_tool_uses_20250919"}]});
    let req = request(
        RequestProvenance::AnthropicIngress,
        json!({"context_management": edits.clone()}),
    );

    for egress in egresses() {
        // Act
        let (wire, delta) = wire_and_delta(&egress, &req);

        // Assert
        assert_eq!(wire["context_management"], edits, "{}: {wire}", egress.lane);
        assert_eq!(delta, 0, "{}", egress.lane);
    }
}

#[test]
fn a_library_caller_forwards_its_extras_to_every_anthropic_shape_egress_unchanged() {
    // Arrange -- a key no ingress would send; a library caller chose it.
    let req = request(RequestProvenance::Library, json!({"store": "zz-lib-store"}));

    for egress in egresses() {
        // Act
        let (wire, delta) = wire_and_delta(&egress, &req);

        // Assert
        assert_eq!(wire["store"], "zz-lib-store", "{}: {wire}", egress.lane);
        assert_eq!(delta, 0, "{}", egress.lane);
    }
}

#[test]
fn a_chat_client_top_k_no_longer_reaches_any_anthropic_shape_egress() {
    // Arrange -- `top_k` is a valid Anthropic field, but a Chat client spells
    // it for its own upstream: the accepted cost of dropping by provenance.
    let req = request(RequestProvenance::OpenaiIngress, json!({"top_k": 40}));

    for egress in egresses() {
        // Act
        let (wire, delta) = wire_and_delta(&egress, &req);

        // Assert
        assert!(wire.get("top_k").is_none(), "{}: {wire}", egress.lane);
        assert_eq!(delta, 1, "{}", egress.lane);
    }
}

#[test]
fn a_chat_client_parallel_tool_calls_still_folds_into_the_anthropic_tool_choice() {
    // Arrange -- a translation read, not a forwarded extra.
    let mut req = request(
        RequestProvenance::OpenaiIngress,
        json!({"parallel_tool_calls": false}),
    );
    req.tools = Some(vec![routectl_core::ToolDef::Custom(
        routectl_core::CustomTool {
            name: "calc".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        },
    )]);
    let egress = &egresses()[0];

    // Act
    let (wire, delta) = wire_and_delta(egress, &req);

    // Assert
    assert_eq!(
        wire["tool_choice"]["disable_parallel_tool_use"], true,
        "{wire}"
    );
    assert!(wire.get("parallel_tool_calls").is_none(), "{wire}");
    assert_eq!(delta, 0);
}

#[test]
fn a_foreign_client_key_is_dropped_while_an_operator_payload_extra_forwards_on_converse() {
    // Arrange -- the router merged the operator's `top_k` and a Chat client's
    // `top_k` and `store` into one bag; the client won `top_k`.
    let mut req = request(
        RequestProvenance::OpenaiIngress,
        json!({"top_k": 99, "store": true}),
    );
    req.routectl_internal.operator_payload_extras = Some(Arc::new(json!({"top_k": 40})));
    let egress = &egresses()[2];

    // Act
    let (bag, delta) = wire_and_delta(egress, &req);

    // Assert -- the operator's value ships, the client's never does.
    assert_eq!(bag["top_k"], 40, "{bag}");
    assert!(bag.get("store").is_none(), "{bag}");
    assert_eq!(delta, 1);
}

#[test]
fn an_operator_payload_extra_alone_is_never_counted_foreign() {
    // Arrange -- every key in the bag is the operator's, with its value.
    let mut req = request(RequestProvenance::ResponsesIngress, json!({"top_k": 40}));
    req.routectl_internal.operator_payload_extras = Some(Arc::new(json!({"top_k": 40})));

    for egress in egresses() {
        // Act
        let (wire, delta) = wire_and_delta(&egress, &req);

        // Assert
        assert_eq!(wire["top_k"], 40, "{}: {wire}", egress.lane);
        assert_eq!(delta, 0, "{}", egress.lane);
    }
}

fn policy_count(lane: &str, class: &str) -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.policy_class == class)
        .map_or(0, |e| e.action_count)
}

#[test]
fn a_foreign_client_output_config_format_is_dropped_and_counted_on_every_anthropic_shape_egress() {
    // Arrange -- a Chat client's `output_config` is spelled for its own
    // upstream; the Converse bag must not read its `format` either. The
    // Anthropic-ingress copy of the same bag is the positive control.
    let format = json!({"type": "json_schema", "schema": {"type": "object"}});
    let extras = json!({"output_config": {"format": format}});
    let foreign = request(RequestProvenance::OpenaiIngress, extras.clone());
    let same_dialect = request(RequestProvenance::AnthropicIngress, extras);

    for egress in egresses() {
        let conflicts_before = policy_count(egress.lane, "provider_extra_managed_key_conflict");

        // Act
        let (wire, delta) = wire_and_delta(&egress, &foreign);
        let conflicts = policy_count(egress.lane, "provider_extra_managed_key_conflict");
        let (control, control_delta) = wire_and_delta(&egress, &same_dialect);

        // Assert -- counted once, foreign only, never also a managed-key conflict.
        assert!(
            wire.pointer("/output_config/format").is_none(),
            "{}: {wire}",
            egress.lane
        );
        assert_eq!(delta, 1, "{}", egress.lane);
        assert_eq!(conflicts, conflicts_before, "{}", egress.lane);
        assert_eq!(
            control["output_config"]["format"]["type"], "json_schema",
            "{}: {control}",
            egress.lane
        );
        assert_eq!(control_delta, 0, "{}", egress.lane);
    }
}

#[test]
fn an_operator_output_config_format_still_reaches_the_converse_bag_for_a_foreign_client() {
    // Arrange -- the router merged the operator's `output_config` over a Chat
    // client's; only the operator's format may ship.
    let operator = json!({"output_config": {"format": {"type": "json_schema", "schema": {"type": "object", "title": "zz-operator"}}}});
    let mut req = request(
        RequestProvenance::OpenaiIngress,
        json!({"output_config": {"format": {"type": "json_schema", "schema": {"type": "object", "title": "zz-client"}}}}),
    );
    req.routectl_internal.operator_payload_extras = Some(Arc::new(operator));
    let egress = &egresses()[2];

    // Act
    let (bag, delta) = wire_and_delta(egress, &req);

    // Assert
    assert_eq!(
        bag["output_config"]["format"]["schema"]["title"], "zz-operator",
        "{bag}"
    );
    assert_eq!(delta, 1);
}

/// The request-leg edits a normalize declared on its tap.
fn declared_edits(egress: &Egress, mut req: ChatRequest) -> Vec<(EditSite, String, EditKind)> {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    (egress.wire)(&req);
    tap.take_request()
        .edits(Leg::Request)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect()
}

#[test]
fn every_foreign_dialect_key_is_declared_removed_on_every_anthropic_shape_egress() {
    // Arrange -- `output_config` takes the Converse format-read path; the
    // others take the extras merge. A non-token key is left undeclared.
    let foreign = request(
        RequestProvenance::ResponsesIngress,
        json!({
            "store": true,
            "prompt_cache_key": "zz",
            "output_config": {"format": {"type": "json_object"}},
            "has space": 1,
        }),
    );
    let same_dialect = request(RequestProvenance::AnthropicIngress, json!({"store": true}));
    let removed = |key: &str| {
        (
            EditSite::ManagedExtrasStrip,
            format!("/{key}"),
            EditKind::Remove,
        )
    };

    for egress in egresses() {
        // Act
        let edits = declared_edits(&egress, foreign.clone());
        let control = declared_edits(&egress, same_dialect.clone());

        // Assert
        for key in ["store", "prompt_cache_key", "output_config"] {
            assert!(
                edits.contains(&removed(key)),
                "{} {key}: {edits:?}",
                egress.lane
            );
        }
        let strips = edits
            .iter()
            .filter(|(site, _, _)| *site == EditSite::ManagedExtrasStrip)
            .count();
        assert_eq!(strips, 3, "{}: {edits:?}", egress.lane);
        assert!(
            !control.contains(&removed("store")),
            "{}: {control:?}",
            egress.lane
        );
    }
}
