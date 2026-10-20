//! The Converse bag's `thinking` and `output_config.effort`, and the
//! `inferenceConfig` sampling clamp, for requests an Anthropic client sent:
//! the client's thinking object and effort ride the bag as sent, a forced
//! tool choice strips only active thinking, and the clamp keys off the
//! thinking type the final bag ships.
//!
//! Requests are built the way the Anthropic ingress builds them, through the
//! anthropic-api egress's own test builder.

use routectl_core::wire_check::{EditSite, Leg, WireTap};
use routectl_core::{CustomTool, ToolDef};
use serde_json::{Value, json};

use super::super::normalize_request;
use crate::anthropic_api::request::thinking_carrier_tests::ingress_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CALLER_TEMPERATURE: f64 = 0.3;

fn cfg(adaptive: bool) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-5-5".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: Some(adaptive),
    }
}

fn converse_body(req: &routectl_core::ChatRequest, adaptive: bool) -> Value {
    normalize_request(&cfg(adaptive), req).expect("translate")
}

fn bag(body: &Value) -> &Value {
    &body["additionalModelRequestFields"]
}

fn bag_effort(body: &Value) -> Option<&str> {
    bag(body)
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
}

fn driver_fixture(case: &str, file: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../routectl-cli/tests/fixtures/driver/anthropic-api")
        .join(case)
        .join(file);
    let text = std::fs::read_to_string(&path).expect("driver fixture must read");
    serde_json::from_str(&text).expect("driver fixture must parse")
}

/// Every type is sent with a `display` so the row also proves the display
/// comes back from its own carrier. Each object differs from what the
/// canonical translation would build (a budget it would clamp or drop, a
/// type it cannot express), so only the forwarding path produces it.
#[test]
fn the_client_thinking_object_reaches_the_bag_as_sent() {
    for thinking in [
        json!({"type": "enabled", "budget_tokens": 1500, "display": "summarized"}),
        json!({"type": "adaptive", "display": "omitted"}),
        json!({"type": "disabled", "display": "summarized"}),
        json!({"type": "between_tools", "display": "omitted"}),
        json!({"type": "future_mode", "depth": 3, "display": "future_display"}),
    ] {
        // Arrange
        let req = ingress_request(Some(thinking.clone()), None, 1200);

        // Act
        let body = converse_body(&req, false);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
    }
}

/// The committed plain-turn capture: disabled thinking beside a positive
/// effort, the shape a real client sends for a turn that should not think.
#[test]
fn the_plain_turn_capture_keeps_its_effort_beside_disabled_thinking() {
    // Arrange
    let ingress = driver_fixture("plain-turn-01", "ingress_request.json");
    let effort = ingress
        .pointer("/output_config/effort")
        .and_then(Value::as_str);
    assert!(
        effort.is_some(),
        "precondition: the capture carries an effort"
    );
    let max_tokens = ingress["max_tokens"].as_u64().expect("max_tokens") as u32;
    let req = ingress_request(Some(ingress["thinking"].clone()), effort, max_tokens);

    // Act
    let body = converse_body(&req, false);

    // Assert
    assert_eq!(bag(&body)["thinking"], ingress["thinking"], "{body}");
    assert_eq!(bag_effort(&body), effort, "{body}");
}

/// `(row, thinking sent, effort sent, effort_levels, effort on the bag)`.
type EffortRow<'a> = (
    &'a str,
    Option<Value>,
    Option<&'a str>,
    &'a [&'a str],
    Option<&'a str>,
);

#[test]
fn the_client_effort_reaches_the_bag_as_sent() {
    let levels =
        |l: &[&str]| -> std::sync::Arc<[String]> { l.iter().map(|s| (*s).to_string()).collect() };
    let rows: &[EffortRow] = &[
        (
            "adaptive, no list",
            Some(json!({"type": "adaptive"})),
            Some("xhigh"),
            &[],
            Some("xhigh"),
        ),
        (
            "enabled, no list",
            Some(json!({"type": "enabled", "budget_tokens": 4096})),
            Some("max"),
            &[],
            Some("max"),
        ),
        (
            "disabled, no list",
            Some(json!({"type": "disabled"})),
            Some("high"),
            &[],
            Some("high"),
        ),
        (
            "adaptive, none",
            Some(json!({"type": "adaptive"})),
            Some("none"),
            &[],
            Some("none"),
        ),
        (
            "adaptive, none, explicit list",
            Some(json!({"type": "adaptive"})),
            Some("none"),
            &["low", "medium", "high"],
            Some("none"),
        ),
        (
            "adaptive, explicit list re-clamps",
            Some(json!({"type": "adaptive"})),
            Some("max"),
            &["low", "medium", "high"],
            Some("high"),
        ),
        (
            "disabled, explicit list re-clamps",
            Some(json!({"type": "disabled"})),
            Some("xhigh"),
            &["low", "high"],
            Some("high"),
        ),
        (
            "adaptive, no effort",
            Some(json!({"type": "adaptive"})),
            None,
            &[],
            None,
        ),
        ("no thinking, effort", None, Some("low"), &[], Some("low")),
    ];
    for (row, thinking, effort, list, expected) in rows {
        // Arrange
        let mut req = ingress_request(thinking.clone(), *effort, 64_000);
        req.routectl_internal.effort_levels = levels(list);

        // Act
        let body = converse_body(&req, false);

        // Assert
        assert_eq!(bag_effort(&body), *expected, "{row}: {body}");
        assert_eq!(
            bag(&body).get("thinking"),
            thinking.as_ref(),
            "{row}: thinking rides as sent: {body}"
        );
    }
}

/// The adaptive knob is the one source of an effort the client did not
/// send: its adaptive shape needs one.
#[test]
fn the_adaptive_knob_supplies_an_effort_only_when_the_client_sent_none() {
    // Arrange
    let silent = ingress_request(
        Some(json!({"type": "enabled", "budget_tokens": 4096})),
        None,
        64_000,
    );
    let explicit = ingress_request(
        Some(json!({"type": "enabled", "budget_tokens": 4096})),
        Some("low"),
        64_000,
    );

    // Act
    let silent_body = converse_body(&silent, true);
    let explicit_body = converse_body(&explicit, true);

    // Assert
    assert_eq!(bag(&silent_body)["thinking"]["type"], "adaptive");
    assert!(bag_effort(&silent_body).is_some(), "{silent_body}");
    assert_eq!(bag_effort(&explicit_body), Some("low"), "{explicit_body}");
}

/// A probe-sized request on a lane without the adaptive knob: the canonical
/// translation would emit no thinking here, but the client's adaptive object
/// ships, so the clamp must read the bag.
#[test]
fn an_adaptive_carrier_on_a_legacy_lane_pins_temperature() {
    // Arrange
    let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 512);
    req.temperature = Some(CALLER_TEMPERATURE);

    // Act
    let body = converse_body(&req, false);

    // Assert
    assert_eq!(bag(&body)["thinking"]["type"], "adaptive", "{body}");
    assert_eq!(body["inferenceConfig"]["temperature"], 1.0, "{body}");
}

#[test]
fn adaptive_thinking_without_a_client_temperature_ships_no_temperature() {
    // Arrange
    let req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 64_000);

    // Act
    let body = converse_body(&req, true);

    // Assert
    assert_eq!(bag(&body)["thinking"]["type"], "adaptive", "{body}");
    let inference = body["inferenceConfig"]
        .as_object()
        .expect("max_tokens keeps inferenceConfig on the body");
    assert!(!inference.contains_key("temperature"), "{body}");
    assert!(!inference.contains_key("topP"), "{body}");
}

#[test]
fn inactive_thinking_types_keep_the_caller_temperature() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode"}),
    ] {
        // Arrange
        let mut req = ingress_request(Some(thinking.clone()), Some("high"), 64_000);
        req.temperature = Some(CALLER_TEMPERATURE);

        // Act
        let body = converse_body(&req, false);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
        assert_eq!(
            body["inferenceConfig"]["temperature"], CALLER_TEMPERATURE,
            "{thinking}: {body}"
        );
    }
}

fn forcing_tool_use(mut req: routectl_core::ChatRequest) -> routectl_core::ChatRequest {
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "calc".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);
    req.tool_choice = Some(json!("required"));
    req
}

/// Forced tool use strips active thinking from the bag, declares the strip
/// once, and gives the caller's sampling back.
#[test]
fn forced_tool_use_strips_active_thinking_once_and_restores_the_caller_sampling() {
    for thinking in [
        json!({"type": "adaptive"}),
        json!({"type": "enabled", "budget_tokens": 4096}),
    ] {
        // Arrange
        let mut req = forcing_tool_use(ingress_request(Some(thinking.clone()), None, 64_000));
        req.top_p = Some(0.9);

        // Act
        let (body, edits) = declared(req, false, &[EditSite::ThinkingStripForcedToolUse]);

        // Assert
        assert!(bag(&body).get("thinking").is_none(), "{thinking}: {body}");
        assert_eq!(
            edits,
            vec![(EditSite::ThinkingStripForcedToolUse, "/thinking".into())],
            "{thinking}"
        );
        let inference = &body["inferenceConfig"];
        assert!(inference.get("temperature").is_none(), "{thinking}: {body}");
        assert_eq!(inference["topP"], 0.9, "{thinking}: {body}");
    }
}

/// Inactive and unknown thinking types spend no reasoning budget, so they
/// ride beside a forced tool choice with the client's effort and sampling,
/// and nothing is declared.
#[test]
fn forced_tool_use_keeps_inactive_thinking_with_the_client_effort() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode"}),
    ] {
        // Arrange
        let mut req = forcing_tool_use(ingress_request(
            Some(thinking.clone()),
            Some("high"),
            64_000,
        ));
        req.temperature = Some(CALLER_TEMPERATURE);

        // Act
        let (body, edits) = declared(req, false, &[EditSite::ThinkingStripForcedToolUse]);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
        assert_eq!(bag_effort(&body), Some("high"), "{thinking}: {body}");
        assert_eq!(
            body["inferenceConfig"]["temperature"], CALLER_TEMPERATURE,
            "{thinking}: {body}"
        );
        assert!(edits.is_empty(), "{thinking}: {edits:?}");
    }
}

/// `(row, thinking sent, effort sent, adaptive knob, effort on the bag)`.
type StrippedEffortRow<'a> = (&'a str, Value, Option<&'a str>, bool, Option<&'a str>);

/// The strip takes `output_config.effort` with it only when routectl
/// composed the effort; the client's own effort stays.
#[test]
fn forced_tool_use_drops_only_a_routectl_composed_effort() {
    let rows: &[StrippedEffortRow] = &[
        (
            "client effort beside adaptive",
            json!({"type": "adaptive"}),
            Some("xhigh"),
            false,
            Some("xhigh"),
        ),
        (
            "client effort beside enabled",
            json!({"type": "enabled", "budget_tokens": 4096}),
            Some("low"),
            false,
            Some("low"),
        ),
        (
            "effort the adaptive knob inserted",
            json!({"type": "enabled", "budget_tokens": 4096}),
            None,
            true,
            None,
        ),
    ];
    for (row, thinking, effort, knob, expected) in rows {
        // Arrange
        let req = forcing_tool_use(ingress_request(Some(thinking.clone()), *effort, 64_000));
        if *knob {
            let unforced = ingress_request(Some(thinking.clone()), *effort, 64_000);
            assert!(
                bag_effort(&converse_body(&unforced, true)).is_some(),
                "{row}: precondition: the knob composes an effort"
            );
        }

        // Act
        let body = converse_body(&req, *knob);

        // Assert
        assert!(bag(&body).get("thinking").is_none(), "{row}: {body}");
        assert_eq!(bag_effort(&body), *expected, "{row}: {body}");
    }
}

fn declared(
    req: routectl_core::ChatRequest,
    adaptive: bool,
    sites: &[EditSite],
) -> (Value, Vec<(EditSite, String)>) {
    let mut req = req;
    let tap = WireTap::admitted();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let body = converse_body(&req, adaptive);
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| sites.contains(&edit.site()))
        .map(|edit| (edit.site(), edit.path().to_string()))
        .collect();
    (body, edits)
}

/// The operator overrides Converse applies to a forwarded object are
/// declared on the attempt's tap, as on the anthropic-api body.
#[test]
fn the_forwarded_thinking_overrides_are_declared() {
    // Arrange
    let converted = ingress_request(
        Some(json!({"type": "enabled", "budget_tokens": 8192})),
        Some("high"),
        64_000,
    );
    let mut capped = converted.clone();
    capped.routectl_internal.max_thinking_budget = 4096;
    let untouched = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 64_000);

    // Act
    let sites = [
        EditSite::AdaptiveThinkingOverride,
        EditSite::ThinkingBudgetCap,
    ];
    let (converted_body, converted_edits) = declared(converted, true, &sites);
    let (capped_body, capped_edits) = declared(capped, false, &sites);
    let (_, untouched_edits) = declared(untouched, true, &sites);

    // Assert
    assert_eq!(
        bag(&converted_body)["thinking"],
        json!({"type": "adaptive"})
    );
    assert_eq!(
        converted_edits,
        vec![
            (EditSite::AdaptiveThinkingOverride, "/thinking/type".into()),
            (
                EditSite::AdaptiveThinkingOverride,
                "/thinking/budget_tokens".into()
            ),
        ]
    );
    assert_eq!(bag(&capped_body)["thinking"]["budget_tokens"], 4096);
    assert_eq!(
        capped_edits,
        vec![(
            EditSite::ThinkingBudgetCap,
            "/thinking/budget_tokens".into()
        )]
    );
    assert!(untouched_edits.is_empty(), "{untouched_edits:?}");
}
