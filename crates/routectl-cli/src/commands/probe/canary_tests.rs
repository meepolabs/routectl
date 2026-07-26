//! Coverage for the canary builders and the baked probe profile: the
//! intent markers each builder derives, the ordered caching pair, the
//! exact-value profile pins, and classification of hand-built responses
//! through the shared `routectl_router::detect` path (including the
//! clean-stop gate). No network anywhere -- every response is a struct
//! literal.

use super::*;

use routectl_core::cache_control::compute_frozen_floor;
use routectl_core::capability::{
    SCHEMA_PARSE, SEARCH_ABSENT_FORCED, STRUCTURED_OUTPUT, SignalTier, WEB_SEARCH,
};
use routectl_core::{ChatResponse, Choice, Message, MessageContent, Role, Usage};
use routectl_router::{ObservationDirection, detect};

const MODEL: &str = "wire-model";

/// An assistant turn carrying flat text.
fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text(text.to_string()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// A response with one choice and the given finish reason.
fn response_with(message: Message, finish_reason: &str, usage: Option<Usage>) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some(finish_reason.to_string()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage,
        ..Default::default()
    }
}

// --- profile pins ------------------------------------------------------

#[test]
fn probe_profile_values_are_pinned() {
    assert_eq!(PROBE_PROFILE_V1.max_tokens, 1536);
    assert_eq!(PROBE_PROFILE_V1.structured_output_canaries, 1);
    assert_eq!(PROBE_PROFILE_V1.web_search_canaries, 1);
    assert_eq!(PROBE_PROFILE_V1.prompt_caching_canaries, 2);
    assert_eq!(PROBE_PROFILE_V1.thinking_canaries, 1);
}

#[test]
fn ceiling_admits_the_thinking_budget_plus_headroom() {
    // The shared ceiling must sit strictly above the requested thinking
    // budget so a completion still has room for a visible answer.
    const { assert!(PROBE_PROFILE_V1.max_tokens > THINKING_BUDGET_TOKENS) };
}

// --- builder intent markers --------------------------------------------

#[test]
fn structured_output_builder_marks_strict_and_one_required_key() {
    let canary = structured_output_canary(MODEL);
    assert!(canary.context.strict_output_requested);
    assert_eq!(canary.context.requested_schema_required_keys.len(), 1);
    assert_eq!(canary.request.max_tokens, Some(PROBE_PROFILE_V1.max_tokens));
    assert!(!canary.context.forced_web_search);
    assert!(!canary.context.reasoning_requested);
    assert!(!canary.context.cache_requested);
}

#[test]
fn web_search_builder_marks_forced_search_only() {
    let canary = web_search_canary(MODEL);
    assert!(canary.context.forced_web_search);
    assert!(!canary.context.strict_output_requested);
    assert!(canary.request.tool_choice.is_some());
    assert_eq!(canary.request.max_tokens, Some(PROBE_PROFILE_V1.max_tokens));
}

#[test]
fn thinking_builder_marks_reasoning_only() {
    let canary = thinking_canary(MODEL);
    assert!(canary.context.reasoning_requested);
    assert!(!canary.context.strict_output_requested);
    assert_eq!(
        canary.request.reasoning.and_then(|r| r.max_tokens),
        Some(THINKING_BUDGET_TOKENS)
    );
}

#[test]
fn caching_builder_marks_cache_and_yields_ordered_pair() {
    let canary = prompt_caching_canary(MODEL);
    assert!(canary.context.cache_requested);
    // Both calls carry a cache breakpoint; the prime writes it and the read
    // reuses the identical prefix.
    assert!(compute_frozen_floor(&canary.prime).has_caller_breakpoints());
    assert!(compute_frozen_floor(&canary.read).has_caller_breakpoints());
    // The two calls are byte-identical -- the ordered pair reads what the
    // prime wrote.
    let prime = serde_json::to_value(&canary.prime).expect("serialize prime");
    let read = serde_json::to_value(&canary.read).expect("serialize read");
    assert_eq!(prime, read);
    assert_eq!(canary.prime.max_tokens, Some(PROBE_PROFILE_V1.max_tokens));
}

// --- fixture classification through detect -----------------------------

#[test]
fn schema_conforming_response_verifies_structured_output() {
    let canary = structured_output_canary(MODEL);
    let resp = response_with(assistant_text(r#"{"answer":"ok"}"#), "stop", None);

    let observations = detect(&canary.context, &resp);

    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].capability_key, STRUCTURED_OUTPUT);
    assert_eq!(observations[0].evidence_class, SCHEMA_PARSE);
    assert_eq!(observations[0].direction, ObservationDirection::Verified);
    assert_eq!(observations[0].tier, SignalTier::SelfIdentifying);
}

#[test]
fn forced_search_empty_response_suspects_web_search_absence() {
    let canary = web_search_canary(MODEL);
    let resp = response_with(assistant_text("no search was performed"), "stop", None);

    let observations = detect(&canary.context, &resp);

    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].capability_key, WEB_SEARCH);
    assert_eq!(observations[0].evidence_class, SEARCH_ABSENT_FORCED);
    assert_eq!(
        observations[0].direction,
        ObservationDirection::SuspectAbsence
    );
    assert_eq!(observations[0].tier, SignalTier::Inferred);
}

#[test]
fn length_stop_yields_no_observations() {
    let canary = structured_output_canary(MODEL);
    // A schema-conforming body, but truncated by the token ceiling: the
    // clean-stop gate rejects, so no detector runs.
    let resp = response_with(assistant_text(r#"{"answer":"ok"}"#), "length", None);

    assert!(detect(&canary.context, &resp).is_empty());
}

#[test]
fn refusal_yields_no_observations() {
    let canary = structured_output_canary(MODEL);
    let mut message = assistant_text(r#"{"answer":"ok"}"#);
    message.refusal = Some("declined".to_string());
    let resp = response_with(message, "stop", None);

    assert!(detect(&canary.context, &resp).is_empty());
}

#[test]
fn unknown_finish_reason_yields_no_observations() {
    let canary = structured_output_canary(MODEL);
    let resp = response_with(
        assistant_text(r#"{"answer":"ok"}"#),
        "some_new_reason",
        None,
    );

    assert!(detect(&canary.context, &resp).is_empty());
}
