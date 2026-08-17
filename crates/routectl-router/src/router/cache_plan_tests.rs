//! Tests for the pure auto-cache decision contract: the `front_slot` plan
//! fact and the operator-facing decision token vocabulary.
//!
//! The plan is a request-derived immutable fact, so every assertion here
//! builds it from a `ChatRequest` and never mutates the request.

use super::*;

use routectl_core::{
    CustomTool, Message, MessageContent, Role, SystemBlock, SystemContent, ToolDef,
};

fn user_req(system: Option<SystemContent>, tools: Option<Vec<ToolDef>>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4".into(),
        system,
        tools,
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
        ..Default::default()
    }
}

fn system_blocks(texts: &[&str]) -> SystemContent {
    SystemContent::Blocks(
        texts
            .iter()
            .map(|t| SystemBlock {
                kind: "text".into(),
                text: (*t).into(),
                cache_control: None,
                citations: None,
            })
            .collect(),
    )
}

fn custom_tool(name: &str) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

#[test]
fn front_slot_resolves_to_the_last_system_block_when_the_system_has_blocks() {
    // Arrange
    let req = user_req(Some(system_blocks(&["a", "b"])), None);

    // Act
    let plan = AutoCacheRequestPlan::build(&req, true);

    // Assert
    assert_eq!(
        plan.front_slot,
        Some(FrontSlot::LastSystemBlock { block_index: 1 }),
    );
}

#[test]
fn front_slot_resolves_to_a_custom_tool_when_the_system_offers_no_anchor() {
    // Arrange: a flat-string system has no per-block marker field, so the
    // tools slot is the only anchor left.
    let req = user_req(
        Some(SystemContent::Text("flat".into())),
        Some(vec![custom_tool("calc"), custom_tool("search")]),
    );

    // Act
    let plan = AutoCacheRequestPlan::build(&req, true);

    // Assert
    assert_eq!(
        plan.front_slot,
        Some(FrontSlot::LastCustomTool { tool_index: 1 }),
    );
}

#[test]
fn front_slot_is_none_when_the_request_offers_no_placement_region() {
    // Arrange: flat-string system, no tools -- the accepted coverage gap.
    let req = user_req(Some(SystemContent::Text("flat".into())), None);

    // Act
    let plan = AutoCacheRequestPlan::build(&req, true);

    // Assert
    assert_eq!(plan.front_slot, None);
}

#[test]
fn front_slot_is_none_when_no_system_and_no_tools_are_present() {
    // Arrange
    let req = user_req(None, None);

    // Act
    let plan = AutoCacheRequestPlan::build(&req, true);

    // Assert
    assert_eq!(plan.front_slot, None);
}

#[test]
fn front_slot_is_identical_across_repeated_builds_of_the_same_request() {
    // Arrange: the idempotence invariant -- retries and fallback targets
    // rebuild nothing, and a rebuild must agree byte-for-byte.
    let req = user_req(
        Some(system_blocks(&["a", "b"])),
        Some(vec![custom_tool("t")]),
    );

    // Act
    let first = AutoCacheRequestPlan::build(&req, true);
    let second = AutoCacheRequestPlan::build(&req, false);

    // Assert
    assert_eq!(first.front_slot, second.front_slot);
    assert_eq!(
        first.front_slot,
        Some(FrontSlot::LastSystemBlock { block_index: 1 }),
    );
}

#[test]
fn no_placement_region_maps_to_its_stable_decision_token() {
    assert_eq!(
        CacheInjection::SkippedNoPlacementRegion.strategy_str(),
        "auto_skipped:no_placement_region",
    );
}

#[test]
fn cache_decision_carries_the_two_marker_outcomes_independently() {
    // Arrange + Act: a front marker with no region skips alone; the
    // terminal marker still emits.
    let decision = CacheDecision {
        front: CacheInjection::SkippedNoPlacementRegion,
        terminal: CacheInjection::Emitted,
    };

    // Assert
    assert_eq!(
        decision.front.strategy_str(),
        "auto_skipped:no_placement_region",
    );
    assert_eq!(decision.terminal.strategy_str(), "auto_emitted");
}
