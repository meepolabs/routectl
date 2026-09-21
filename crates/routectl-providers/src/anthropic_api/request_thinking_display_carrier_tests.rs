//! `routectl_internal.anthropic_thinking_display` reaches the assembled
//! wire body as `thinking.display`, on the plain egress and after the
//! OAuth cloak.
//!
//! `extras_thinking_display_tests.rs` pins the `build_thinking` shape;
//! these assert at the body level, where a later reconciliation pass
//! could still drop or rewrite the key.

use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

use super::normalize;
use crate::anthropic_api::cloak::{ClaudeCodeIdentity, CloakConfig, cloak_oauth_egress};

/// A third `display` value this hub's canonical `exclude` boolean cannot
/// express, so only a verbatim carrier forward can put it on the wire.
const UNMODELED_DISPLAY: &str = "updates";

fn req_with_carrier(adaptive: bool) -> ChatRequest {
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
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.supports_adaptive_thinking = adaptive;
    req.routectl_internal.anthropic_thinking_display = Some(UNMODELED_DISPLAY.to_string());
    req
}

#[test]
fn carrier_reaches_the_wire_body_on_the_legacy_shape() {
    let req = req_with_carrier(false);

    let body = normalize("p", &req, false, &[], false, None, false, true).expect("normalize");

    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["display"], UNMODELED_DISPLAY);
}

#[test]
fn carrier_reaches_the_wire_body_on_the_adaptive_shape() {
    let req = req_with_carrier(true);

    let body = normalize("p", &req, true, &[], false, None, false, true).expect("normalize");

    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["thinking"]["display"], UNMODELED_DISPLAY);
}

#[test]
fn absent_carrier_leaves_no_display_key_on_the_wire_body() {
    // Load-bearing negative; its positive control is the legacy test
    // above (same builder, carrier set, key present).
    let mut req = req_with_carrier(false);
    req.routectl_internal.anthropic_thinking_display = None;

    let body = normalize("p", &req, false, &[], false, None, false, true).expect("normalize");

    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(
        body["thinking"].get("display").is_none(),
        "no carrier and no exclude must leave no display key; got {}",
        body["thinking"]
    );
}

#[test]
fn cloak_leaves_the_thinking_object_untouched() {
    let req = req_with_carrier(false);
    let plain = normalize("p", &req, false, &[], false, None, false, true).expect("normalize");

    let mut cloaked = plain.clone();
    cloak_oauth_egress(
        &mut cloaked,
        &req,
        &ClaudeCodeIdentity::mint(Some("sess-carrier")),
        true,
        &CloakConfig::default(),
    );

    assert_eq!(cloaked["thinking"]["display"], UNMODELED_DISPLAY);
    assert_eq!(
        cloaked["thinking"], plain["thinking"],
        "the cloak rewrites identity and tool names, never thinking"
    );
}

/// The router's lazy probe builds its count-token body by setting the
/// carrier plus the minimal canonical reasoning state the normalizer needs.
/// This pins the SHAPE that body must have, through the real serializer:
/// carrier + `reasoning.enabled` + `max_tokens`.
///
/// Without the reasoning state `build_thinking` returns early and the body
/// serializes with NO `thinking` object, so the probe would send a plain
/// token count and read its success as evidence about a field it never put
/// on the wire. Lives here, at the serializer, because that is the only
/// place the absence is observable -- the typed request looks correct either
/// way.
#[test]
fn a_probe_shaped_body_serializes_both_thinking_type_and_display() {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        // The minimal canonical state: thinking explicitly active plus a
        // budget window for the legacy shape to clamp into.
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());

    let body = normalize("p", &req, false, &[], false, None, false, true).expect("normalize");

    assert_eq!(
        body["thinking"]["type"], "enabled",
        "a probe body must carry an active thinking object"
    );
    assert_eq!(
        body["thinking"]["display"], "summarized",
        "a probe body must carry the display field under test"
    );
}

/// The converse, and the reason the fix exists: the carrier ALONE puts
/// nothing on the wire. Pins the normalizer's early return so a future
/// change that made the carrier self-sufficient would surface here rather
/// than silently making the probe's extra state redundant.
#[test]
fn the_carrier_without_a_reasoning_state_serializes_no_thinking_object() {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(2048),
        reasoning: None,
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());

    let body = normalize("p", &req, false, &[], false, None, false, true).expect("normalize");

    assert!(
        body.get("thinking").is_none(),
        "the carrier alone must not synthesize a thinking object: {body}"
    );
}
