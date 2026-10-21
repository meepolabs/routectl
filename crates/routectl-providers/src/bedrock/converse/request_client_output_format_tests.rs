//! The Anthropic client's own `output_config.format` on the Converse bag:
//! merged beside the composed effort, preferred over a `response_format`
//! conversion, and put through the same final-bag passes (key scrub,
//! `additionalProperties` repair, structured-outputs beta) as a converted
//! format. The client's other `output_config` members stay withheld.
//!
//! Requests are built the way the Anthropic ingress builds them, through the
//! anthropic-api egress's own test builder, and every assertion reads the
//! bag `normalize_request` ships.

use serde_json::{Value, json};

use routectl_core::ChatRequest;
use routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;

use super::super::normalize_request;
use crate::anthropic_api::request::thinking_carrier_tests::ingress_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const MANAGED_KEY_CONFLICT: &str = "provider_extra_managed_key_conflict";

fn cfg(adaptive: bool) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-6".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: Some(adaptive),
    }
}

/// An ingress-built request whose client `output_config` is `output_config`.
fn request_with_client_output_config(
    thinking: Option<Value>,
    effort: Option<&str>,
    output_config: Value,
) -> ChatRequest {
    let mut req = ingress_request(thinking, effort, 2048);
    req.provider_extras = Some(json!({ "output_config": output_config }));
    req
}

fn shipped_bag(cfg: &BedrockConfig, req: &ChatRequest) -> Value {
    let body = normalize_request(cfg, req).expect("translate");
    body["additionalModelRequestFields"].clone()
}

fn bag_betas(bag: &Value) -> Vec<&str> {
    bag["anthropic_beta"]
        .as_array()
        .map(|betas| betas.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn managed_key_conflict_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.policy_class == MANAGED_KEY_CONFLICT)
        .map_or(0, |e| e.action_count)
}

/// A client-native format with no `response_format` reaches the bag beside
/// the effort routectl composes, and only the format comes from the client's
/// object: the sent effort is re-clamped against `effort_levels` to a value
/// the client did not send, and a sibling member stays withheld.
#[test]
fn client_output_config_format_ships_beside_the_composed_effort() {
    // Arrange
    let format = json!({
        "type": "json_schema",
        "schema": {"type": "object", "additionalProperties": false}
    });
    let mut req = request_with_client_output_config(
        Some(json!({"type": "adaptive"})),
        Some("xhigh"),
        json!({
            "effort": "xhigh",
            "task_budget": {"tokens": 4096},
            "format": format.clone()
        }),
    );
    req.routectl_internal.effort_levels = ["low", "high"]
        .iter()
        .map(|level| (*level).to_string())
        .collect();

    // Act
    let bag = shipped_bag(&cfg(true), &req);

    // Assert
    assert_eq!(
        bag["output_config"],
        json!({"effort": "high", "format": format}),
        "{bag}"
    );
}

/// A format-only client `output_config` with no thinking carrier creates the
/// bag's `output_config` rather than needing an effort to merge into.
#[test]
fn client_output_config_format_ships_without_any_effort() {
    // Arrange
    let format = json!({"type": "json_object"});
    let req = request_with_client_output_config(None, None, json!({"format": format.clone()}));

    // Act
    let bag = shipped_bag(&cfg(false), &req);

    // Assert
    assert_eq!(bag["output_config"], json!({"format": format}), "{bag}");
}

/// Same precedence as the Anthropic egress: when the client sent its own
/// format AND a `response_format`, the client's format ships and the
/// conversion is discarded.
#[test]
fn client_output_config_format_wins_over_a_response_format_conversion() {
    // Arrange
    let client_schema = json!({
        "type": "object",
        "properties": {"client_marker": {"type": "string"}},
        "additionalProperties": false
    });
    let mut req = request_with_client_output_config(
        None,
        None,
        json!({"format": {"type": "json_schema", "schema": client_schema.clone()}}),
    );
    req.response_format = Some(json!({
        "type": "json_schema",
        "json_schema": {
            "name": "converted",
            "schema": {"type": "object", "properties": {"converted_marker": {"type": "string"}}}
        }
    }));

    // Act
    let bag = shipped_bag(&cfg(false), &req);

    // Assert
    assert_eq!(
        bag["output_config"]["format"]["schema"], client_schema,
        "{bag}"
    );
    assert!(
        !bag.to_string().contains("converted_marker"),
        "the converted format must not ship beside the client's: {bag}"
    );
}

/// The merged format goes through every final-bag pass a converted one does:
/// `name` and `strict` scrubbed, `additionalProperties: false` injected into
/// each object schema, and the structured-outputs beta unioned.
#[test]
fn client_output_config_format_passes_the_final_bag_repairs() {
    // Arrange
    let req = request_with_client_output_config(
        None,
        None,
        json!({"format": {
            "type": "json_schema",
            "name": "client-widget",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {"inner": {"type": "object", "properties": {}}}
            }
        }}),
    );

    // Act
    let bag = shipped_bag(&cfg(false), &req);

    // Assert
    let format = bag["output_config"]["format"]
        .as_object()
        .expect("the client format must ship");
    assert!(
        !format.contains_key("name") && !format.contains_key("strict"),
        "{bag}"
    );
    let schema = &format["schema"];
    assert_eq!(schema["additionalProperties"], false, "{bag}");
    assert_eq!(
        schema["properties"]["inner"]["additionalProperties"], false,
        "{bag}"
    );
    assert!(
        bag_betas(&bag).contains(&STRUCTURED_OUTPUTS_BETA),
        "the merged format must ship with its gating beta: {bag}"
    );
}

/// Only `format` is forwarded. The client's other `output_config` members are
/// withheld and counted exactly as before, and a client `output_config`
/// without a format yields the same bag as no client `output_config` at all.
#[test]
fn client_output_config_without_a_format_ships_the_bag_unchanged_and_counted() {
    // Arrange
    let thinking = Some(json!({"type": "adaptive"}));
    let with_members = request_with_client_output_config(
        thinking.clone(),
        Some("high"),
        json!({"effort": "high", "task_budget": {"tokens": 4096}}),
    );
    let mut without_config = with_members.clone();
    without_config.provider_extras = None;
    let before = managed_key_conflict_count();

    // Act
    let bag = shipped_bag(&cfg(true), &with_members);
    let after = managed_key_conflict_count();
    let control_bag = shipped_bag(&cfg(true), &without_config);

    // Assert
    assert_eq!(bag, control_bag);
    assert_eq!(bag["output_config"], json!({"effort": "high"}), "{bag}");
    assert!(
        !bag_betas(&bag).contains(&STRUCTURED_OUTPUTS_BETA),
        "no format, no structured-outputs beta: {bag}"
    );
    assert_eq!(
        after - before,
        1,
        "the withheld client output_config counts once for the request"
    );
}

const FORMAT_KEYS_LOST: &str = "response_format_keys_unrepresentable";

fn format_keys_drop_count(lane: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.drop_class == FORMAT_KEYS_LOST)
        .map_or(0, |e| e.drop_count)
}

fn json_schema_response_format(members: Value) -> Value {
    crate::anthropic_api::request_drop_counter_tests::json_schema_response_format(members)
}

/// Each row's converted format ships without the keys it carried, counted
/// once on this lane and credited to no other.
#[test]
fn response_format_name_and_strict_count_once_on_the_converse_lane() {
    let rows = [
        ("name only", json!({"name": "widget"})),
        ("strict only", json!({"strict": true})),
        ("name and strict", json!({"name": "widget", "strict": true})),
    ];
    for (row, members) in rows {
        // Arrange
        let mut req = ingress_request(None, None, 2048);
        req.response_format = Some(json_schema_response_format(members));

        // Act
        let before = (
            format_keys_drop_count("bedrock-converse"),
            format_keys_drop_count("anthropic"),
        );
        let bag = shipped_bag(&cfg(false), &req);
        let after = (
            format_keys_drop_count("bedrock-converse"),
            format_keys_drop_count("anthropic"),
        );

        // Assert
        let format = &bag["output_config"]["format"];
        assert_eq!(format["type"], "json_schema", "row {row}: {bag}");
        assert!(
            format.get("name").is_none() && format.get("strict").is_none(),
            "row {row}: {bag}"
        );
        assert_eq!(after.0 - before.0, 1, "row {row}");
        assert_eq!(after.1, before.1, "row {row}: not anthropic traffic");
    }
}

/// Nothing is counted when no converted key is lost: a client-native format
/// whose keys the final-bag scrub removes, a conversion that lost to that
/// format, and a conversion carrying neither key.
#[test]
fn converse_response_format_key_loss_counts_nothing_when_no_conversion_loses_a_key() {
    let client_native_extras =
        crate::anthropic_api::request_drop_counter_tests::client_native_format_with_name_and_strict(
        );
    let client_native = request_with_client_output_config(
        None,
        None,
        client_native_extras["output_config"].clone(),
    );
    let mut lost_to_client = client_native.clone();
    lost_to_client.response_format = Some(json_schema_response_format(
        json!({"name": "converted", "strict": true}),
    ));
    let mut keyless = ingress_request(None, None, 2048);
    keyless.response_format = Some(json_schema_response_format(json!({})));
    let rows = [
        ("client-native format scrubbed", client_native, true),
        (
            "conversion lost to a client-native format",
            lost_to_client,
            true,
        ),
        ("json_schema without name or strict", keyless, false),
    ];
    for (row, req, ships_client_format) in rows {
        // Act
        let before = format_keys_drop_count("bedrock-converse");
        let bag = shipped_bag(&cfg(false), &req);
        let after = format_keys_drop_count("bedrock-converse");

        // Assert
        let format = &bag["output_config"]["format"];
        assert_eq!(format["type"], "json_schema", "row {row}: {bag}");
        assert!(
            format.get("name").is_none() && format.get("strict").is_none(),
            "row {row}: {bag}"
        );
        assert_eq!(
            format["schema"]["properties"]
                .get("client_marker")
                .is_some(),
            ships_client_format,
            "row {row}: {bag}"
        );
        assert_eq!(after, before, "row {row}");
    }
}
