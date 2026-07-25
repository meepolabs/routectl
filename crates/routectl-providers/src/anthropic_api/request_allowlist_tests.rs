use super::*;
use routectl_core::{ChatRequest, Message, Role};
use serde_json::json;

fn req_with_betas(betas: Vec<String>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
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
        max_tokens: Some(64),
        anthropic_beta: betas,
        ..Default::default()
    }
}

/// Pin: empty allowlist = pass-through. Default behavior, no
/// operator surprise on upgrade.
#[test]
fn empty_allowlist_passes_all_betas() {
    let req = req_with_betas(vec![
        "context-1m-2025-08-07".into(),
        "prompt-caching-2024-07-31".into(),
    ]);
    let body = normalize("p", &req, false, &[], false, None).unwrap();
    assert_eq!(
        body["anthropic_beta"],
        json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
    );
}

/// Pin: non-empty allowlist drops entries not in the list.
#[test]
fn non_empty_allowlist_drops_unknown() {
    let req = req_with_betas(vec![
        "context-1m-2025-08-07".into(),
        "secret-experimental-flag".into(),
        "prompt-caching-2024-07-31".into(),
    ]);
    let allowed = vec![
        "context-1m-2025-08-07".to_string(),
        "prompt-caching-2024-07-31".to_string(),
    ];
    let body = normalize("p", &req, false, &allowed, false, None).unwrap();
    // Order preserved, unknown flag dropped.
    assert_eq!(
        body["anthropic_beta"],
        json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
    );
}

/// Pin: every requested beta is rejected when none are on the
/// allowlist. The wire field is either absent or an empty array;
/// both mean "no betas reach upstream" and either serialization
/// is acceptable.
#[test]
fn allowlist_can_drop_all_requested() {
    let req = req_with_betas(vec!["totally-unknown".into()]);
    let allowed = vec!["context-1m-2025-08-07".to_string()];
    let body = normalize("p", &req, false, &allowed, false, None).unwrap();
    let got = &body["anthropic_beta"];
    assert!(
        got.is_null() || got.as_array().is_some_and(std::vec::Vec::is_empty),
        "expected absent or empty array, got: {got}"
    );
}
