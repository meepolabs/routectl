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

/// The shared normalizer does NOT carry the structured-outputs body beta.
/// The flag belongs to the egress that actually reads betas from the body
/// (Bedrock-Invoke), which unions it after its own beta withholding so a
/// withheld flag cannot drop it again downstream. On the api.anthropic.com
/// lane the body field is stripped before send and the `anthropic-beta`
/// header carries the flag instead.
#[test]
fn shared_normalizer_leaves_the_body_beta_carrier_to_the_egress() {
    let mut req = req_with_betas(Vec::new());
    req.response_format = Some(json!({
        "type": "json_schema",
        "json_schema": {"name": "out", "schema": {"type": "object"}},
    }));

    let body = normalize("p", &req, false, false, None, false, true).unwrap();
    assert!(
        body["output_config"].get("format").is_some(),
        "precondition: the response_format lift must land output_config.format; got: {body}"
    );
    let got = &body["anthropic_beta"];
    assert!(
        got.is_null() || got.as_array().is_some_and(std::vec::Vec::is_empty),
        "the shared normalizer must not mint the body beta; got: {got}"
    );
}

/// A caller's own requested structured-outputs flag still rides the body
/// verbatim -- the normalizer neither drops nor reorders it.
#[test]
fn caller_requested_structured_outputs_beta_survives_the_body_carrier() {
    let mut req = req_with_betas(vec![
        "structured-outputs-2025-12-15".into(),
        "context-1m-2025-08-07".into(),
    ]);
    req.response_format = Some(json!({"type": "json_object"}));

    let body = normalize("p", &req, false, false, None, false, true).unwrap();
    assert_eq!(
        body["anthropic_beta"],
        json!(["structured-outputs-2025-12-15", "context-1m-2025-08-07"]),
        "the normalizer must add nothing and reorder nothing"
    );
}

/// No structured-output directive -> the body carrier is untouched.
#[test]
fn body_without_output_config_format_gains_no_structured_outputs_beta() {
    let req = req_with_betas(vec!["context-1m-2025-08-07".into()]);
    let body = normalize("p", &req, false, false, None, false, true).unwrap();
    assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07"]));
}
