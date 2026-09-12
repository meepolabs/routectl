//! Telemetry contract of the anthropic-api request-assembly path: the lane's
//! request-volume denominator, and the per-request policy action recorded when
//! the Claude Code client fingerprint is withheld.
//!
//! SERIAL GUARDS: the registry is process-global and the runner is threaded,
//! so every test here that drives the strip carries
//! `anthropic_api_client_fingerprint_stripped` -- the ones asserting a delta
//! AND the ones that only bump the key incidentally while asserting something
//! else. A guard name no sibling shares excludes nothing.

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
    normalize("test", req, false, &[], false, None, false, true).expect("normalize must succeed")
}

// ---------------------------------------------------------------------------
// The denominator.
// ---------------------------------------------------------------------------

/// Every request the lane assembles counts toward its denominator, including
/// one that FAILS assembly: a rate whose denominator omitted the failures
/// would read low for exactly the requests that went worst.
///
/// The assertion is a LOWER BOUND, and deliberately so: this is one shared key
/// that every `normalize` call in the crate's suite bumps, so no serial guard
/// short of one shared by every such test could make an exact delta stable. A
/// lower bound is race-immune in the only direction the registry moves (up)
/// while still failing on the defect it exists to catch -- an unwired or
/// mis-placed site leaves the delta at 0. That `normalize` is the SOLE site is
/// a grep property, welded by the census rather than assertable from here.
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
    let _ = normalize("test", &clean, false, &[], false, None, false, true)
        .expect("the clean request assembles");
    let _ = normalize("test", &rejected, false, &[], false, None, false, true)
        .expect_err("the malformed request is rejected");
    let after = lane_seen_count();

    // Assert
    assert!(
        after - before >= 2,
        "both requests must count toward the denominator (a rejected request is still a \
         request the lane processed); delta was {}",
        after - before
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
    let outcome = normalize("test", &req, false, &[], false, None, false, true);
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
#[serial_test::serial(anthropic_api_client_fingerprint_stripped)]
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
