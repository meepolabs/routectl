use super::*;
use routectl_core::ChatResponse;
use routectl_core::capability::{
    CACHE_HIT, PROMPT_CACHING, SCHEMA_MISMATCH, SCHEMA_PARSE, SEARCH_ABSENT_FORCED, SEARCH_BLOCKS,
    STRUCTURED_OUTPUT, SignalTier, THINKING, THINKING_BLOCKS, WEB_SEARCH,
};
use serde_json::{Value, json};

const DEGRADED_FIXTURE: &str =
    include_str!("../tests/fixtures/capability_degraded_provisional.json");

fn chat_response(value: Value) -> ChatResponse {
    serde_json::from_value(value).expect("valid ChatResponse json")
}

fn degraded_case(name: &str) -> ChatResponse {
    let fixture: Value =
        serde_json::from_str(DEGRADED_FIXTURE).expect("valid degraded fixture json");
    serde_json::from_value(fixture["cases"][name].clone())
        .unwrap_or_else(|e| panic!("case {name} deserializes to ChatResponse: {e}"))
}

fn gate_response(finish: Option<&str>) -> ChatResponse {
    let mut choice = json!({"index": 0, "message": {"role": "assistant", "content": "ok"}});
    if let Some(reason) = finish {
        choice["finish_reason"] = json!(reason);
    }
    chat_response(json!({ "choices": [choice] }))
}

fn strict_ctx(required_keys: &[&str]) -> DetectorContext {
    DetectorContext {
        strict_output_requested: true,
        requested_schema_required_keys: required_keys.iter().map(|k| (*k).to_string()).collect(),
        ..Default::default()
    }
}

const fn verified_obs(
    capability_key: &'static str,
    evidence_class: &'static str,
) -> CapabilityObservation {
    CapabilityObservation {
        capability_key,
        evidence_class,
        direction: ObservationDirection::Verified,
        tier: SignalTier::SelfIdentifying,
    }
}

const fn suspect_obs(
    capability_key: &'static str,
    evidence_class: &'static str,
) -> CapabilityObservation {
    CapabilityObservation {
        capability_key,
        evidence_class,
        direction: ObservationDirection::SuspectAbsence,
        tier: SignalTier::Inferred,
    }
}

// --- clean-stop gate: per-token accept / reject -------------------------

#[test]
fn gate_accepts_each_clean_stop_token() {
    for token in ["stop", "tool_calls", "pause_turn"] {
        assert!(
            clean_stop_choice(&gate_response(Some(token))).is_some(),
            "gate should accept {token}"
        );
    }
}

#[test]
fn gate_rejects_length_and_content_filter() {
    assert!(clean_stop_choice(&gate_response(Some("length"))).is_none());
    assert!(clean_stop_choice(&gate_response(Some("content_filter"))).is_none());
}

#[test]
fn gate_rejects_absent_finish_reason() {
    assert!(clean_stop_choice(&gate_response(None)).is_none());
}

#[test]
fn gate_fails_closed_on_unknown_tokens() {
    for token in ["provider_specific_halt", "error", "incomplete", "STOP", ""] {
        assert!(
            clean_stop_choice(&gate_response(Some(token))).is_none(),
            "gate should reject unknown token {token:?}"
        );
    }
}

#[test]
fn gate_rejects_refusal_on_a_clean_token() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "refusal": "declined"},
            "finish_reason": "stop"
        }]
    }));
    assert!(clean_stop_choice(&resp).is_none());
}

#[test]
fn gate_requires_exactly_one_choice() {
    let two = chat_response(json!({
        "choices": [
            {"index": 0, "message": {"role": "assistant", "content": "a"}, "finish_reason": "stop"},
            {"index": 1, "message": {"role": "assistant", "content": "b"}, "finish_reason": "stop"}
        ]
    }));
    assert!(clean_stop_choice(&two).is_none());
    let zero = chat_response(json!({ "choices": [] }));
    assert!(clean_stop_choice(&zero).is_none());
}

// --- structured_output --------------------------------------------------

#[test]
fn structured_output_verified_when_required_keys_present() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"name\": \"x\", \"age\": 3}"},
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&strict_ctx(&["name", "age"]), &resp),
        vec![verified_obs(STRUCTURED_OUTPUT, SCHEMA_PARSE)]
    );
}

#[test]
fn structured_output_verified_with_no_declared_required_keys() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"anything\": 1}"},
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&strict_ctx(&[]), &resp),
        vec![verified_obs(STRUCTURED_OUTPUT, SCHEMA_PARSE)]
    );
}

#[test]
fn structured_output_suspect_on_missing_required_key() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"name\": \"x\"}"},
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&strict_ctx(&["name", "age"]), &resp),
        vec![suspect_obs(STRUCTURED_OUTPUT, SCHEMA_MISMATCH)]
    );
}

#[test]
fn structured_output_suspect_on_prose_body() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "The answer is x."},
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&strict_ctx(&["name"]), &resp),
        vec![suspect_obs(STRUCTURED_OUTPUT, SCHEMA_MISMATCH)]
    );
}

#[test]
fn structured_output_no_observation_when_not_strict() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"name\": \"x\"}"},
            "finish_reason": "stop"
        }]
    }));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

#[test]
fn strict_tool_response_is_gated_but_not_verified() {
    // Anthropic-shape tool_use payload: gated (finish=tool_calls) but the
    // structured payload lives in the tool input, not verified in v1.
    let anthropic = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "emit", "input": {"name": "x"}}]
            },
            "finish_reason": "tool_calls"
        }]
    }));
    assert!(detect(&strict_ctx(&["name"]), &anthropic).is_empty());

    // OpenAI-shape tool_calls: same treatment.
    let openai = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "emit"}}]
            },
            "finish_reason": "tool_calls"
        }]
    }));
    assert!(detect(&strict_ctx(&["name"]), &openai).is_empty());
}

// --- web_search ---------------------------------------------------------

#[test]
fn web_search_verified_from_usage_server_tool_use() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "server_tool_use": {"web_search_requests": 2}
        }
    }));
    assert_eq!(
        detect(&DetectorContext::default(), &resp),
        vec![verified_obs(WEB_SEARCH, SEARCH_BLOCKS)]
    );
}

#[test]
fn web_search_verified_from_search_result_block() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "here"},
                    {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []}
                ]
            },
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&DetectorContext::default(), &resp),
        vec![verified_obs(WEB_SEARCH, SEARCH_BLOCKS)]
    );
}

#[test]
fn web_search_suspect_when_forced_and_no_evidence() {
    let ctx = DetectorContext {
        forced_web_search: true,
        ..Default::default()
    };
    let resp = gate_response(Some("stop"));
    assert_eq!(
        detect(&ctx, &resp),
        vec![suspect_obs(WEB_SEARCH, SEARCH_ABSENT_FORCED)]
    );
}

#[test]
fn web_search_no_observation_on_auto_without_evidence() {
    // tool_choice=auto is not `forced_web_search`, so an absent search
    // never produces an observation.
    let resp = gate_response(Some("stop"));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

#[test]
fn web_search_zero_request_count_is_not_evidence() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "server_tool_use": {"web_search_requests": 0}
        }
    }));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

#[test]
fn web_search_ignores_unrelated_server_tool_counter() {
    // A positive count for a non-search server tool must not credit web
    // search -- the detector keys on the search-specific counter only.
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "server_tool_use": {"code_execution_requests": 3}
        }
    }));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

// --- prompt_caching -----------------------------------------------------

#[test]
fn prompt_caching_verified_on_cache_read() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "cache_read_input_tokens": 128
        }
    }));
    assert_eq!(
        detect(&DetectorContext::default(), &resp),
        vec![verified_obs(PROMPT_CACHING, CACHE_HIT)]
    );
}

#[test]
fn prompt_caching_verified_on_cache_creation_tokens() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "cache_creation_input_tokens": 64
        }
    }));
    assert_eq!(
        detect(&DetectorContext::default(), &resp),
        vec![verified_obs(PROMPT_CACHING, CACHE_HIT)]
    );
}

#[test]
fn prompt_caching_verified_on_per_ttl_breakdown() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "cache_creation": {"ephemeral_5m_input_tokens": 32}
        }
    }));
    assert_eq!(
        detect(&DetectorContext::default(), &resp),
        vec![verified_obs(PROMPT_CACHING, CACHE_HIT)]
    );
}

#[test]
fn prompt_caching_no_observation_on_zero_counters() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
        }
    }));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

// --- thinking -----------------------------------------------------------

#[test]
fn thinking_verified_from_reasoning_details() {
    let ctx = DetectorContext {
        reasoning_requested: true,
        ..Default::default()
    };
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "step", "format": "anthropic-claude-v1"}
                ]
            },
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        detect(&ctx, &resp),
        vec![verified_obs(THINKING, THINKING_BLOCKS)]
    );
}

#[test]
fn thinking_verified_from_reasoning_tokens() {
    let ctx = DetectorContext {
        reasoning_requested: true,
        ..Default::default()
    };
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "answer"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "reasoning_tokens": 40
        }
    }));
    assert_eq!(
        detect(&ctx, &resp),
        vec![verified_obs(THINKING, THINKING_BLOCKS)]
    );
}

#[test]
fn thinking_no_observation_when_requested_but_absent() {
    let ctx = DetectorContext {
        reasoning_requested: true,
        ..Default::default()
    };
    let resp = gate_response(Some("stop"));
    assert!(detect(&ctx, &resp).is_empty());
}

#[test]
fn thinking_no_observation_when_not_requested() {
    let resp = chat_response(json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "step", "format": "anthropic-claude-v1"}
                ]
            },
            "finish_reason": "stop"
        }]
    }));
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}

// --- R4 degraded suite: every case yields zero observations -------------

#[test]
fn degraded_gate_failing_cases_yield_zero_under_maximal_intent() {
    // A context that would fire every detector -- to prove the gate, not a
    // sparse intent, is what zeroes these out.
    let maximal = DetectorContext {
        strict_output_requested: true,
        requested_schema_required_keys: vec!["answer".to_string()],
        forced_web_search: true,
        reasoning_requested: true,
        cache_requested: true,
    };
    for name in [
        "finish_length_plausible_body",
        "content_filter",
        "refusal",
        "missing_finish_reason",
        "truncated_but_parseable_json",
        "unknown_finish_reason",
    ] {
        let resp = degraded_case(name);
        assert!(
            detect(&maximal, &resp).is_empty(),
            "degraded case {name} must yield zero observations"
        );
    }
}

#[test]
fn degraded_non_strict_prose_yields_zero_on_a_clean_stop() {
    // This case passes the gate; zero observations come from the detectors
    // finding no requested-capability evidence under a neutral intent.
    let resp = degraded_case("non_strict_prose");
    assert!(clean_stop_choice(&resp).is_some());
    assert!(detect(&DetectorContext::default(), &resp).is_empty());
}
