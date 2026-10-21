//! Telemetry contract of the anthropic-api request-assembly path: the lane's
//! request-volume denominator, and the per-request policy action recorded when
//! the Claude Code client fingerprint is withheld.

use super::request::normalize;
use routectl_core::{ChatRequest, Message, MessageContent, Role, SystemBlock, SystemContent};

/// This lane's request-volume denominator, read through the registry's own
/// accessor so it reads correctly even before any class on this lane has
/// fired.
fn lane_seen_count() -> u64 {
    crate::translation_drop_metrics::translation_lane_seen(super::LANE)
}

/// The `(anthropic, client_fingerprint_stripped)` policy-action counter, read
/// through the public snapshot. Zero before its first bump.
fn fingerprint_strip_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

fn user_turn(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn system_turn(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![user_turn("hello")].into(),
        ..Default::default()
    }
}

fn assemble(req: &ChatRequest) -> serde_json::Value {
    normalize("test", req, false, false, None, false, true).expect("normalize must succeed")
}

// ---------------------------------------------------------------------------
// The denominator.
// ---------------------------------------------------------------------------

/// Every request the lane assembles counts toward its denominator, including
/// one that FAILS assembly: a rate whose denominator omitted the failures
/// would read low for exactly the requests that went worst.
///
/// The delta is exact: a site counted twice per request would make every
/// rate on the lane read low. That `normalize` is the SOLE site is a grep
/// property, welded by the census rather than assertable from here.
#[test]
fn every_assembled_request_counts_toward_the_lane_denominator() {
    // Arrange: one clean request and one that fails the replay invariant (a
    // Role::Tool turn with no tool_call_id is rejected outright).
    let clean = base_req();
    let mut rejected = base_req();
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
    let before = lane_seen_count();
    let _ = normalize("test", &clean, false, false, None, false, true)
        .expect("the clean request assembles");
    let _ = normalize("test", &rejected, false, false, None, false, true)
        .expect_err("the malformed request is rejected");
    let after = lane_seen_count();

    // Assert
    assert_eq!(
        after - before,
        2,
        "both requests must count toward the denominator once each (a rejected request is \
         still a request the lane processed)"
    );
}

// ---------------------------------------------------------------------------
// The fingerprint strip: one test per SOURCE, each driving its site alone.
// ---------------------------------------------------------------------------

/// SINGLE-SOURCE pin for the canonical-`system` strip. The messages array
/// carries a clean `Role::System` turn, so the legacy-lift site is present and
/// deliberately has nothing to strip: deleting the record at the canonical
/// site must red THIS test, which a both-sources fixture could not do -- the
/// sibling site would set the shared tally and leave it green.
#[test]
fn the_canonical_system_billing_strip_counts_one_policy_action() {
    // Arrange -- TWO billing blocks in one canonical system. The count is per
    // REQUEST, so two stripped blocks are still one action; a per-occurrence
    // bump would read 2 here.
    let mut req = base_req();
    req.system = Some(SystemContent::Blocks(vec![
        block("x-anthropic-billing-header: v=1; fp=secret"),
        block("x-anthropic-billing-header: v=2; fp=other"),
        block("you are helpful"),
    ]));
    req.messages = vec![system_turn("clean legacy system"), user_turn("hello")].into();

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert
    assert_eq!(
        after - before,
        1,
        "two stripped blocks in one request are one policy action"
    );
    assert!(
        !body.to_string().contains("fp="),
        "the withheld fingerprint must not reach the wire body: {body}"
    );
}

/// SINGLE-SOURCE pin for the legacy `Role::System` lift. No canonical system
/// at all, so the canonical site cannot fire and only the lift strips --
/// deleting the record inside the lift branch must red THIS test.
#[test]
fn the_legacy_lift_billing_strip_counts_one_policy_action() {
    // Arrange -- req.system stays None, forcing the lift fallback.
    let mut req = base_req();
    req.system = None;
    req.messages = vec![
        system_turn("x-anthropic-billing-header: v=1; fp=secret"),
        system_turn("you are helpful"),
        user_turn("hello"),
    ]
    .into();

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert
    assert_eq!(after - before, 1, "the lift withheld one fingerprint");
    let system = body["system"]
        .as_str()
        .expect("the lift produces flat text");
    assert!(
        !system.contains("x-anthropic-billing-header:"),
        "the withheld fingerprint must not reach the wire: {system:?}"
    );
    assert!(
        system.contains("you are helpful"),
        "the non-billing prompt must survive: {system:?}"
    );
}

/// PER-ARM cover for the FORWARDED-TURN withhold site, the third surface on
/// this lane. It is reachable only under `SystemTurnPolicy::Forward`, which
/// needs a canonical system present -- so unlike the other two sites this one
/// requires BOTH a clean canonical system AND a billing block in a forwarded
/// `Role::System` turn. The Anthropic ingress forwards such turns unlifted,
/// so the shape is live in production.
///
/// The pin names THIS test because the canonical-system test cannot stand in
/// for it: that one sets the shared tally from its own site, leaving this
/// record deletable with the suite green.
#[test]
fn the_forwarded_system_turn_billing_strip_counts_one_policy_action() {
    // Arrange -- a CLEAN canonical system (so Forward policy is selected and
    // the canonical site records nothing), with the fingerprint only in a
    // forwarded turn.
    let mut req = base_req();
    req.system = Some(routectl_core::SystemContent::Text("you are helpful".into()));
    req.messages = vec![
        system_turn("x-anthropic-billing-header: v=1; fp=secret"),
        user_turn("hello"),
    ]
    .into();

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert
    assert_eq!(
        after - before,
        1,
        "the forwarded-turn withhold must count exactly one policy action"
    );
    let rendered = serde_json::to_string(&body).expect("the body must serialize");
    assert!(
        !rendered.contains("x-anthropic-billing-header:"),
        "the withheld fingerprint must not reach the wire: {rendered}"
    );
}

/// The record sits ahead of every fallible step, so the request whose whole
/// canonical system IS the block -- which collapses to no `system` at all --
/// still reaches the counter. A record placed past that collapse would miss
/// exactly the requests that withhold the most while the denominator counted
/// them.
#[test]
fn an_all_billing_system_still_counts_the_policy_action() {
    // Arrange
    let mut req = base_req();
    req.system = Some(SystemContent::Text(
        "x-anthropic-billing-header: v=1; fp=secret".into(),
    ));

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert -- nothing was assembled into `system`, and the strip counted.
    assert!(
        body.get("system").is_none() || body["system"].is_null(),
        "a pure-billing system must collapse to absent, got: {body}"
    );
    assert_eq!(
        after - before,
        1,
        "a system that collapsed to nothing still withheld a fingerprint"
    );
}

/// A request that withholds the fingerprint and only THEN fails assembly is
/// still counted: the flush sits outside every fallible step, on both arms.
/// Without that, the numerator would miss precisely the requests that went
/// worst while the denominator counted them.
#[test]
fn a_request_that_strips_then_fails_assembly_still_counts_the_policy_action() {
    // Arrange -- billing block in the canonical system, and a Role::Tool turn
    // with no tool_call_id, which the replay-invariant walk rejects AFTER the
    // strip has run.
    let mut req = base_req();
    req.system = Some(SystemContent::Blocks(vec![block(
        "x-anthropic-billing-header: v=1; fp=secret",
    )]));
    req.messages = vec![Message {
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
    let before = fingerprint_strip_count();
    let outcome = normalize("test", &req, false, false, None, false, true);
    let after = fingerprint_strip_count();

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(
        after - before,
        1,
        "a failed request that withheld the fingerprint still counts"
    );
}

/// POSITIVE CONTROL on the counter. A request carrying no billing block at
/// all -- on either source -- leaves the key untouched, so the delta
/// assertions above cannot be passing against a counter that bumps for every
/// request.
#[test]
fn a_request_with_no_billing_block_records_no_policy_action() {
    // Arrange
    let mut req = base_req();
    req.system = Some(SystemContent::Blocks(vec![block("you are helpful")]));
    req.messages = vec![system_turn("also clean"), user_turn("hello")].into();

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert
    assert_eq!(after, before, "nothing was withheld, so nothing is counted");
    assert_eq!(body["system"][0]["text"], "you are helpful");
}

// ---------------------------------------------------------------------------
// The unrecognized response_format drop.
// ---------------------------------------------------------------------------

/// The `(anthropic, response_format_unrepresentable)` drop counter. Every test
/// reaching it carries `anthropic_response_format_unrepresentable`.
fn response_format_drop_count() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.drop_class == "response_format_unrepresentable")
        .map_or(0, |e| e.drop_count)
}

fn with_response_format(format: serde_json::Value) -> ChatRequest {
    ChatRequest {
        response_format: Some(format),
        ..base_req()
    }
}

/// SINGLE-SOURCE pin: the request's only loss is the unrecognized tag.
#[test]
fn an_unrecognized_response_format_is_absent_from_the_anthropic_body_and_counted() {
    // Arrange
    let req = with_response_format(serde_json::json!({"type": "zz_grammar_mode"}));

    // Act
    let before = response_format_drop_count();
    let body = assemble(&req);
    let after = response_format_drop_count();

    // Assert
    let rendered = serde_json::to_string(&body).expect("the body must serialize");
    assert!(!rendered.contains("zz_grammar_mode"), "{rendered}");
    assert!(body.get("output_config").is_none(), "{rendered}");
    assert_eq!(after - before, 1);
}

/// A caller-supplied `output_config` whose schema nests past the repair
/// walk's depth bound. It rides `provider_extras` past the converter, so the
/// repair that rejects it runs AFTER the directive has been dropped.
pub fn schema_past_the_repair_depth_bound() -> serde_json::Value {
    let mut schema = serde_json::json!({"type": "object", "properties": {}});
    for _ in 0..300 {
        schema = serde_json::json!({"type": "object", "properties": {"a": schema}});
    }
    serde_json::json!({"output_config": {"format": {"type": "json_schema", "schema": schema}}})
}

/// The flush runs on the Err arm too: the directive is dropped by the
/// converter, then the schema repair rejects the request.
#[test]
fn an_unrecognized_response_format_still_counts_when_assembly_fails() {
    // Arrange
    let mut req = with_response_format(serde_json::json!({"type": "zz_grammar_mode"}));
    req.provider_extras = Some(schema_past_the_repair_depth_bound());

    // Act
    let before = response_format_drop_count();
    let outcome = normalize("test", &req, false, false, None, false, true);
    let after = response_format_drop_count();

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(after - before, 1);
}

/// Each row leaves the counter untouched: a mapped format, the default
/// plain-text mode, and a background probe carrying the unrecognized tag.
#[test]
fn response_formats_that_lose_nothing_or_are_not_client_traffic_count_nothing() {
    let mut probe = with_response_format(serde_json::json!({"type": "zz_grammar_mode"}));
    probe.routectl_internal.background_probe = true;
    let rows = [
        (
            "json_object",
            with_response_format(serde_json::json!({"type": "json_object"})),
        ),
        (
            "text",
            with_response_format(serde_json::json!({"type": "text"})),
        ),
        ("background probe", probe),
    ];
    for (row, req) in rows {
        // Act
        let before = response_format_drop_count();
        let _ = assemble(&req);
        let after = response_format_drop_count();

        // Assert
        assert_eq!(after, before, "row {row}");
    }
}

// ---------------------------------------------------------------------------
// The response_format `name` / `strict` conversion loss.
// ---------------------------------------------------------------------------

/// The `(anthropic, response_format_keys_unrepresentable)` drop counter.
fn format_keys_drop_count() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.drop_class == "response_format_keys_unrepresentable")
        .map_or(0, |e| e.drop_count)
}

/// A `json_schema` directive carrying `json_schema` members beside its schema.
pub fn json_schema_response_format(members: serde_json::Value) -> serde_json::Value {
    let mut json_schema = serde_json::json!({
        "schema": {"type": "object", "properties": {}, "additionalProperties": false}
    });
    if let (Some(target), Some(extra)) = (json_schema.as_object_mut(), members.as_object()) {
        target.extend(extra.clone());
    }
    serde_json::json!({"type": "json_schema", "json_schema": json_schema})
}

/// A client-native `output_config.format` carrying both keys the format
/// object cannot represent.
pub fn client_native_format_with_name_and_strict() -> serde_json::Value {
    serde_json::json!({"output_config": {"format": {
        "type": "json_schema",
        "name": "client_native",
        "strict": true,
        "schema": {"type": "object", "properties": {"client_marker": {"type": "string"}},
                   "additionalProperties": false}
    }}})
}

/// Each row's converted format ships without the keys it carried, and the
/// request counts once however many of the two keys were lost.
#[test]
fn response_format_name_and_strict_count_once_on_the_anthropic_lane() {
    let rows = [
        ("name only", serde_json::json!({"name": "widget"})),
        ("strict only", serde_json::json!({"strict": true})),
        (
            "name and strict",
            serde_json::json!({"name": "widget", "strict": true}),
        ),
    ];
    for (row, members) in rows {
        // Arrange
        let req = with_response_format(json_schema_response_format(members));

        // Act
        let before = format_keys_drop_count();
        let body = assemble(&req);
        let after = format_keys_drop_count();

        // Assert
        let format = &body["output_config"]["format"];
        assert_eq!(format["type"], "json_schema", "row {row}: {body}");
        assert!(
            format.get("name").is_none() && format.get("strict").is_none(),
            "row {row}: {body}"
        );
        assert_eq!(after - before, 1, "row {row}");
    }
}

/// The flush runs on the Err arm too: the converted format ships, then the
/// schema repair rejects it.
#[test]
fn response_format_name_still_counts_when_assembly_fails() {
    // Arrange
    let deep = schema_past_the_repair_depth_bound()["output_config"]["format"]["schema"].clone();
    let req = with_response_format(serde_json::json!({
        "type": "json_schema",
        "json_schema": {"name": "widget", "schema": deep}
    }));

    // Act
    let before = format_keys_drop_count();
    let outcome = normalize("test", &req, false, false, None, false, true);
    let after = format_keys_drop_count();

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(after - before, 1);
}

/// Nothing is counted when no converted key is lost. The client-native row
/// is the scrub-only case: its `name` / `strict` are removed from a format
/// that never passed through the converter.
#[test]
fn response_format_key_loss_counts_nothing_when_no_conversion_loses_a_key() {
    let mut client_native = base_req();
    client_native.provider_extras = Some(client_native_format_with_name_and_strict());
    let mut lost_to_client = with_response_format(json_schema_response_format(
        serde_json::json!({"name": "converted", "strict": true}),
    ));
    lost_to_client.provider_extras = Some(client_native_format_with_name_and_strict());
    let mut probe = with_response_format(json_schema_response_format(
        serde_json::json!({"name": "widget"}),
    ));
    probe.routectl_internal.background_probe = true;
    let rows = [
        ("client-native format scrubbed", client_native, true),
        (
            "conversion lost to a client-native format",
            lost_to_client,
            true,
        ),
        (
            "json_schema without name or strict",
            with_response_format(json_schema_response_format(serde_json::json!({}))),
            false,
        ),
        ("background probe", probe, false),
    ];
    for (row, req, ships_client_format) in rows {
        // Act
        let before = format_keys_drop_count();
        let body = assemble(&req);
        let after = format_keys_drop_count();

        // Assert
        let format = &body["output_config"]["format"];
        assert_eq!(format["type"], "json_schema", "row {row}: {body}");
        assert!(
            format.get("name").is_none() && format.get("strict").is_none(),
            "row {row}: {body}"
        );
        assert_eq!(
            format["schema"]["properties"]
                .get("client_marker")
                .is_some(),
            ships_client_format,
            "row {row}: {body}"
        );
        assert_eq!(after, before, "row {row}");
    }
}
