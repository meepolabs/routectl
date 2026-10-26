//! Translation telemetry of the Invoke lane: the request-volume denominator,
//! and the unrecognized `response_format` drop counted under this lane rather
//! than the anthropic lane whose assembly builds the body.

use serde_json::json;

use routectl_core::{ChatRequest, Message, MessageContent, Role};

use super::{LANE, normalize_request};
use crate::anthropic_api::request_drop_counter_tests::{
    client_native_format_with_name_and_strict, json_schema_response_format,
    schema_past_the_repair_depth_bound,
};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const DROP_CLASS: &str = "response_format_unrepresentable";

fn cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn user_req() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn drop_count(lane: &str) -> u64 {
    class_count(lane, DROP_CLASS)
}

fn class_count(lane: &str, class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

const FORMAT_KEYS_CLASS: &str = "response_format_keys_unrepresentable";

/// Every Invoke request counts toward this lane's denominator exactly once,
/// including one that fails assembly.
#[test]
fn every_invoke_request_counts_toward_the_lane_denominator() {
    // Arrange: one clean request and one the replay invariant rejects.
    let clean = user_req();
    let mut rejected = user_req();
    rejected.messages = vec![Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text("result".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();

    // Act
    let before = crate::translation_drop_metrics::translation_lane_seen(LANE);
    let _ = normalize_request(&cfg(), &clean).expect("the clean request assembles");
    let _ = normalize_request(&cfg(), &rejected).expect_err("the malformed request is rejected");
    let after = crate::translation_drop_metrics::translation_lane_seen(LANE);

    // Assert
    assert_eq!(after - before, 2, "one count per request, failed or not");
}

/// SINGLE-SOURCE pin for this lane: the directive is absent from the body
/// that ships, counted once here, and credited to no other lane.
#[test]
fn an_unrecognized_response_format_is_absent_from_the_invoke_body_and_counted() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "zz_grammar_mode"}));

    // Act
    let (invoke_before, anthropic_before) = (drop_count(LANE), drop_count("anthropic"));
    let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
    let (invoke_after, anthropic_after) = (drop_count(LANE), drop_count("anthropic"));

    // Assert
    let rendered = serde_json::to_string(&body).expect("body serializes");
    assert!(!rendered.contains("zz_grammar_mode"), "{rendered}");
    assert!(body.get("output_config").is_none(), "{rendered}");
    assert_eq!(invoke_after - invoke_before, 1);
    assert_eq!(
        anthropic_after, anthropic_before,
        "an Invoke request is not anthropic traffic"
    );
}

/// The flush runs on the Err arm of the shared assembly too: the directive is
/// dropped by the converter, then the schema repair rejects the request.
#[test]
fn an_unrecognized_response_format_still_counts_when_invoke_assembly_fails() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "zz_grammar_mode"}));
    req.provider_extras = Some(schema_past_the_repair_depth_bound());

    // Act
    let before = drop_count(LANE);
    let outcome = normalize_request(&cfg(), &req);
    let after = drop_count(LANE);

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(after - before, 1);
}

/// POSITIVE CONTROL: a mapped directive leaves the counter untouched.
#[test]
fn a_mapped_response_format_counts_no_invoke_drop() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "json_object"}));

    // Act
    let before = drop_count(LANE);
    let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
    let after = drop_count(LANE);

    // Assert
    assert_eq!(body["output_config"]["format"]["type"], "json_object");
    assert_eq!(after, before);
}

/// SINGLE-SOURCE pin for this lane's `name` / `strict` conversion loss: the
/// converted format ships without them, counted once here and credited to no
/// other lane.
#[test]
fn response_format_name_and_strict_count_once_on_the_invoke_lane() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json_schema_response_format(
        json!({"name": "widget", "strict": true}),
    ));

    // Act
    let before = (
        class_count(LANE, FORMAT_KEYS_CLASS),
        class_count("anthropic", FORMAT_KEYS_CLASS),
    );
    let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
    let after = (
        class_count(LANE, FORMAT_KEYS_CLASS),
        class_count("anthropic", FORMAT_KEYS_CLASS),
    );

    // Assert
    let format = &body["output_config"]["format"];
    assert_eq!(format["type"], "json_schema", "{body}");
    assert!(
        format.get("name").is_none() && format.get("strict").is_none(),
        "{body}"
    );
    assert_eq!(after.0 - before.0, 1);
    assert_eq!(
        after.1, before.1,
        "an Invoke request is not anthropic traffic"
    );
}

/// The flush runs on the Err arm of the shared assembly: the converted format
/// ships, then the schema repair rejects it.
#[test]
fn response_format_name_still_counts_when_invoke_assembly_fails() {
    // Arrange
    let deep = schema_past_the_repair_depth_bound()["output_config"]["format"]["schema"].clone();
    let mut req = user_req();
    req.response_format = Some(json!({
        "type": "json_schema",
        "json_schema": {"name": "widget", "schema": deep}
    }));

    // Act
    let before = class_count(LANE, FORMAT_KEYS_CLASS);
    let outcome = normalize_request(&cfg(), &req);
    let after = class_count(LANE, FORMAT_KEYS_CLASS);

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(after - before, 1);
}

/// A client-native format's scrubbed keys, and a conversion that lost to it,
/// count nothing; the client format is what ships in both rows.
#[test]
fn invoke_response_format_key_loss_counts_nothing_for_a_client_native_format() {
    let mut client_native = user_req();
    client_native.provider_extras = Some(client_native_format_with_name_and_strict());
    let mut lost_to_client = client_native.clone();
    lost_to_client.response_format = Some(json_schema_response_format(
        json!({"name": "converted", "strict": true}),
    ));
    let rows = [
        ("client-native format scrubbed", client_native),
        ("conversion lost to a client-native format", lost_to_client),
    ];
    for (row, req) in rows {
        // Act
        let before = class_count(LANE, FORMAT_KEYS_CLASS);
        let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
        let after = class_count(LANE, FORMAT_KEYS_CLASS);

        // Assert
        let format = &body["output_config"]["format"];
        assert!(
            format["schema"]["properties"]
                .get("client_marker")
                .is_some(),
            "row {row}: the client format must ship: {body}"
        );
        assert!(
            format.get("name").is_none() && format.get("strict").is_none(),
            "row {row}: {body}"
        );
        assert_eq!(after, before, "row {row}");
    }
}

const OPERATOR_CONFLICT_CLASS: &str = "operator_extra_managed_key_conflict";

fn operator_conflict_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == LANE && e.policy_class == OPERATOR_CONFLICT_CLASS)
        .map_or(0, |e| e.action_count)
}

/// The Invoke body after merging `extras` as the operator's
/// `additional_model_request_fields`, the managed-key WARNs it emitted (by
/// key), and how much it moved the operator conflict count.
fn merge_operator_extras_observed(
    extras: serde_json::Value,
) -> (serde_json::Value, Vec<String>, u64) {
    let cfg = BedrockConfig {
        additional_model_request_fields: Some(extras),
        ..cfg()
    };
    let req = user_req();
    let before = operator_conflict_count();
    let mut body = serde_json::Value::Null;
    let events = routectl_testkit::capture_events(|| {
        body = normalize_request(&cfg, &req).expect("invoke body assembles");
    });
    let after = operator_conflict_count();
    let warned = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message
                    .contains("additional_model_request_fields attempted to override")
        })
        .filter_map(|e| e.field("key").map(str::to_owned))
        .collect();
    (body, warned, after - before)
}

/// SINGLE-SOURCE pin for this lane's operator refusal, one row per managed
/// key: `anthropic_version` keeps the value Bedrock requires, and
/// `response_format` (absent from the old closed Invoke set) never reaches the
/// top level, where the Messages validator refuses it as an extra input.
#[test]
fn an_operator_extra_naming_a_managed_key_is_withheld_and_counted_once() {
    let rows = [
        (
            "anthropic_version",
            json!({"anthropic_version": "zz-operator-version"}),
        ),
        (
            "response_format",
            json!({"response_format": {"type": "json_object"}}),
        ),
    ];
    for (key, extras) in rows {
        // Act
        let (body, warned, delta) = merge_operator_extras_observed(extras);

        // Assert
        assert!(
            !body.to_string().contains("zz-operator-version"),
            "row {key}: {body}"
        );
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31", "row {key}");
        assert!(body.get("response_format").is_none(), "row {key}: {body}");
        assert_eq!(warned, vec![key.to_string()], "row {key}");
        assert_eq!(delta, 1, "row {key}");
    }
}

/// Two refused keys on one request are one policy action, each still named
/// by its own WARN.
#[test]
fn two_refused_operator_keys_count_one_policy_action() {
    // Act
    let (body, mut warned, delta) = merge_operator_extras_observed(json!({
        "anthropic_version": "zz-operator-version",
        "response_format": {"type": "json_object"},
    }));

    // Assert
    warned.sort();
    assert_eq!(warned, ["anthropic_version", "response_format"]);
    assert!(body.get("response_format").is_none(), "{body}");
    assert_eq!(delta, 1);
}

/// POSITIVE CONTROL: an unmanaged operator key lands on the body, with no WARN
/// and no count.
#[test]
fn an_unmanaged_operator_extra_lands_on_the_invoke_body_uncounted() {
    // Act
    let (body, warned, delta) = merge_operator_extras_observed(json!({"top_k": 40}));

    // Assert
    assert_eq!(body["top_k"], 40, "{body}");
    assert!(warned.is_empty(), "{warned:?}");
    assert_eq!(delta, 0);
}
