//! Telemetry contract of the anthropic-api request-assembly path: the lane's
//! request-volume denominator, and the per-request policy action recorded when
//! the Claude Code client fingerprint is withheld.

use super::request::normalize;
use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
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
    normalize("test", req, false, None, false, true, false).expect("normalize must succeed")
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
    let _ = normalize("test", &clean, false, None, false, true, false)
        .expect("the clean request assembles");
    let _ = normalize("test", &rejected, false, None, false, true, false)
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
    let outcome = normalize("test", &req, false, None, false, true, false);
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
    let outcome = normalize("test", &req, false, None, false, true, false);
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
    let outcome = normalize("test", &req, false, None, false, true, false);
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

// ---------------------------------------------------------------------------
// Non-text parts of a system-role turn.
// ---------------------------------------------------------------------------

/// The `(anthropic, system_part_unrepresentable)` drop counter.
fn system_part_drop_count() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.drop_class == "system_part_unrepresentable")
        .map_or(0, |e| e.drop_count)
}

fn system_turn_with_image(text: &str) -> Message {
    Message {
        content: MessageContent::Parts(vec![
            routectl_core::ContentPart::Known(routectl_core::KnownContentPart::Text {
                text: text.into(),
                citations: None,
                cache_control: None,
            }),
            routectl_core::ContentPart::Known(routectl_core::KnownContentPart::ImageUrl {
                image_url: serde_json::json!({"url": "https://example.com/a.png"}),
                cache_control: None,
            }),
        ]),
        ..system_turn(text)
    }
}

/// With no canonical system the turn is lifted into the `system` field,
/// which carries text only: its text arrives, its image is counted lost.
#[test]
fn a_non_text_system_part_the_lift_cannot_carry_is_counted_on_the_anthropic_lane() {
    let req = ChatRequest {
        messages: vec![system_turn_with_image("match this"), user_turn("hi")].into(),
        ..base_req()
    };

    let before = system_part_drop_count();
    let body = assemble(&req);
    let after = system_part_drop_count();

    assert_eq!(after - before, 1);
    assert_eq!(body["system"], serde_json::json!("match this"));
    assert!(!body.to_string().contains("example.com"), "{body}");
}

/// With a canonical system the turn rides the wire in place, image and all,
/// so nothing is counted.
#[test]
fn a_forwarded_system_turn_keeps_its_image_and_counts_no_drop() {
    let req = ChatRequest {
        system: Some(SystemContent::Text("be brief".into())),
        messages: vec![
            user_turn("hi"),
            system_turn_with_image("match this"),
            user_turn("again"),
        ]
        .into(),
        ..base_req()
    };

    let before = system_part_drop_count();
    let body = assemble(&req);
    let after = system_part_drop_count();

    assert_eq!(after - before, 0);
    assert_eq!(body["messages"][1]["role"], serde_json::json!("system"));
    assert_eq!(
        body["messages"][1]["content"][1]["source"]["url"],
        serde_json::json!("https://example.com/a.png"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// The Mantle credential-bearing field withhold.
// ---------------------------------------------------------------------------

/// The `(anthropic, credential_bearing_field_withheld)` policy-action
/// counter. Zero before its first bump.
fn credential_withhold_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == super::LANE && e.policy_class == "credential_bearing_field_withheld")
        .map_or(0, |e| e.action_count)
}

fn with_mcp_servers() -> ChatRequest {
    ChatRequest {
        provider_extras: Some(serde_json::json!({"mcp_servers": [
            {"type": "url", "url": "https://mcp.example.com/a", "name": "a",
             "authorization_token": "zz-mcp-token-a"},
            {"type": "url", "url": "https://mcp.example.com/b", "name": "b",
             "authorization_token": "zz-mcp-token-b"},
        ]})),
        ..base_req()
    }
}

/// The assembled body, the policy-action delta, and the tap's
/// `ManagedExtrasStrip` declarations for one request on the given lane.
fn assemble_on_lane(
    req: &ChatRequest,
    mantle_lane: bool,
) -> (serde_json::Value, u64, Vec<(EditKind, String)>) {
    let mut req = req.clone();
    let tap = WireTap::admitted();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let before = credential_withhold_count();
    let body = normalize("test", &req, false, None, false, true, mantle_lane)
        .expect("normalize must succeed");
    let after = credential_withhold_count();
    let strips = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| edit.site() == EditSite::ManagedExtrasStrip)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect();
    (body, after - before, strips)
}

/// Two servers, each with a connector credential, are one withheld field:
/// the count is per request, and the declaration names the field once.
#[test]
fn mantle_mcp_servers_is_absent_from_the_body_and_counted_once() {
    // Arrange
    let req = with_mcp_servers();

    // Act
    let (body, delta, strips) = assemble_on_lane(&req, true);

    // Assert
    let rendered = body.to_string();
    assert!(body.get("mcp_servers").is_none(), "{rendered}");
    assert!(!rendered.contains("zz-mcp-token"), "{rendered}");
    assert_eq!(delta, 1);
    assert_eq!(strips, vec![(EditKind::Remove, "/mcp_servers".to_string())]);
}

/// Positive control for the absence pin: off Mantle the same fixture ships
/// `mcp_servers` verbatim and nothing is withheld or declared.
#[test]
fn the_non_mantle_lane_forwards_mcp_servers_and_counts_nothing() {
    // Arrange
    let req = with_mcp_servers();

    // Act
    let (body, delta, strips) = assemble_on_lane(&req, false);

    // Assert
    assert_eq!(
        body["mcp_servers"],
        req.provider_extras.as_ref().unwrap()["mcp_servers"]
    );
    assert_eq!(delta, 0);
    assert!(strips.is_empty(), "{strips:?}");
}

/// A Mantle request with no `mcp_servers` withholds nothing; a background
/// probe still has the field removed but leaves the client-traffic counter
/// untouched.
#[test]
fn mantle_requests_without_client_mcp_servers_count_nothing() {
    let mut probe = with_mcp_servers();
    probe.routectl_internal.background_probe = true;
    let rows = [
        ("no mcp_servers", base_req(), 0),
        ("background probe", probe, 1),
    ];
    for (row, req, expected_strips) in rows {
        // Act
        let (body, delta, strips) = assemble_on_lane(&req, true);

        // Assert
        assert!(body.get("mcp_servers").is_none(), "row {row}: {body}");
        assert_eq!(delta, 0, "row {row}");
        assert_eq!(strips.len(), expected_strips, "row {row}: {strips:?}");
    }
}

/// An operator `payload_extras` value is withheld on Mantle too: a Chat
/// client's request carries `mcp_servers` only because the operator set it,
/// and the off-Mantle row proves that value reaches the assembled body.
#[test]
fn mantle_withholds_an_operator_payload_mcp_servers() {
    // Arrange
    let servers = serde_json::json!([{"type": "url", "url": "https://mcp.example.com/op",
        "name": "op", "authorization_token": "zz-mcp-token-op"}]);
    let mut req = ChatRequest {
        provider_extras: Some(serde_json::json!({"mcp_servers": servers.clone()})),
        ..base_req()
    };
    req.routectl_internal.provenance = routectl_core::RequestProvenance::OpenaiIngress;
    req.routectl_internal.operator_payload_extras = Some(std::sync::Arc::new(
        serde_json::json!({"mcp_servers": servers.clone()}),
    ));

    // Act
    let (off_mantle, _, _) = assemble_on_lane(&req, false);
    let (on_mantle, delta, strips) = assemble_on_lane(&req, true);

    // Assert
    assert_eq!(off_mantle["mcp_servers"], servers, "{off_mantle}");
    assert!(on_mantle.get("mcp_servers").is_none(), "{on_mantle}");
    assert_eq!(delta, 1);
    assert_eq!(strips, vec![(EditKind::Remove, "/mcp_servers".to_string())]);
}

// ---------------------------------------------------------------------------
// `developer` turns: system authority, coerced to `user` on this wire.
// ---------------------------------------------------------------------------

const DEVELOPER_FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=devfp9";
const DEVELOPER_FINGERPRINT_TELL: &str = "devfp9";

fn other_role_turn(tag: &str, content: MessageContent) -> Message {
    Message {
        role: Role::Other(tag.into()),
        content,
        ..user_turn("")
    }
}

fn text_parts(texts: &[&str]) -> MessageContent {
    MessageContent::Parts(
        texts
            .iter()
            .map(|text| {
                routectl_core::ContentPart::Known(routectl_core::KnownContentPart::Text {
                    text: (*text).into(),
                    citations: None,
                    cache_control: None,
                })
            })
            .collect(),
    )
}

/// The request with `turn` between an assistant reply and a closing user turn,
/// so a dropped turn leaves no same-role pair to join.
fn req_with_turn(turn: Message) -> ChatRequest {
    let mut req = base_req();
    req.messages = vec![
        user_turn("hello"),
        Message {
            role: Role::Assistant,
            ..user_turn("ok")
        },
        turn,
        user_turn("again"),
    ]
    .into();
    req
}

/// SINGLE-SOURCE pin for the developer-turn strip: the fingerprint rides only
/// a `developer` turn, which ships as `user` with its legitimate part intact.
#[test]
fn a_fingerprint_only_in_a_developer_turn_is_withheld_from_anthropic_and_counted() {
    // Arrange
    let req = req_with_turn(other_role_turn(
        "developer",
        text_parts(&[DEVELOPER_FINGERPRINT, "dev-legit-sentinel"]),
    ));

    // Act
    let before = fingerprint_strip_count();
    let body = assemble(&req);
    let after = fingerprint_strip_count();

    // Assert
    let rendered = serde_json::to_string(&body).expect("the body must serialize");
    assert!(
        !rendered.contains(DEVELOPER_FINGERPRINT_TELL),
        "the fingerprint in a developer turn reached the wire: {rendered}"
    );
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4, "{rendered}");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"],
        serde_json::json!([{"type": "text", "text": "dev-legit-sentinel"}])
    );
    assert_eq!(after - before, 1, "the withhold is one policy action");
}

/// The rest of the `Other`-role contract on this lane: a developer turn that
/// is wholly the fingerprint leaves the wire and still counts; a developer
/// turn without it, and any non-authority tag even when it carries the text,
/// ship as `user` unchanged and count nothing.
#[test]
fn other_role_turns_are_screened_on_anthropic_only_when_they_carry_system_authority() {
    let rows: [(&str, Message, Option<serde_json::Value>, u64); 3] = [
        (
            "developer text that is wholly the fingerprint leaves the wire",
            other_role_turn(
                "developer",
                MessageContent::Text(DEVELOPER_FINGERPRINT.into()),
            ),
            None,
            1,
        ),
        (
            "developer without the fingerprint ships unchanged",
            other_role_turn("developer", MessageContent::Text("dev plain".into())),
            Some(serde_json::json!("dev plain")),
            0,
        ),
        (
            "a non-authority tag carrying the text ships as a user turn unchanged",
            other_role_turn(
                "moderator",
                MessageContent::Text(DEVELOPER_FINGERPRINT.into()),
            ),
            Some(serde_json::json!(DEVELOPER_FINGERPRINT)),
            0,
        ),
    ];
    for (name, turn, expected_content, expected_delta) in rows {
        // Arrange
        let req = req_with_turn(turn);

        // Act
        let before = fingerprint_strip_count();
        let body = assemble(&req);
        let after = fingerprint_strip_count();

        // Assert
        let messages = body["messages"].as_array().expect("messages array");
        let contents: Vec<&serde_json::Value> = messages.iter().map(|m| &m["content"]).collect();
        match expected_content {
            Some(content) => {
                assert_eq!(messages.len(), 4, "{name}: {body}");
                assert_eq!(messages[2]["role"], "user", "{name}");
                assert_eq!(messages[2]["content"], content, "{name}");
            }
            None => {
                assert_eq!(
                    contents,
                    vec![
                        &serde_json::json!("hello"),
                        &serde_json::json!("ok"),
                        &serde_json::json!("again")
                    ],
                    "{name}: {body}"
                );
            }
        }
        assert_eq!(after - before, expected_delta, "{name}");
    }
}
