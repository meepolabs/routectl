//! The Invoke body's `anthropic_beta` ships each flag once at its first
//! position. The router has already unioned the operator's `header_extras`
//! pins into the client list; the fixture config carries no
//! `additional_model_request_fields`, so nothing but the dedup (and the
//! withhold) shapes the array.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, Message, MessageContent, Role};

use super::{dedup_betas, normalize_request};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CONTEXT_1M: &str = "context-1m-2025-08-07";
const CLAUDE_CODE: &str = "claude-code-20250219";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";
const EFFORT: &str = "effort-2025-11-24";

fn cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn request_with_client_betas(client: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        anthropic_beta: client.iter().map(|f| (*f).to_string()).collect(),
        ..Default::default()
    }
}

fn shipped_betas(req: &ChatRequest) -> Value {
    let body = normalize_request(&cfg(), req).expect("invoke body assembles");
    body.get("anthropic_beta").cloned().unwrap_or(Value::Null)
}

#[test]
fn a_client_repeat_ships_once() {
    // Arrange
    let req = request_with_client_betas(&[CONTEXT_1M, CONTEXT_1M]);

    // Act
    let betas = shipped_betas(&req);

    // Assert
    assert_eq!(betas, json!([CONTEXT_1M]));
}

#[test]
fn distinct_flags_keep_client_order() {
    // Arrange
    let req = request_with_client_betas(&[CLAUDE_CODE, CONTEXT_1M, EFFORT, INTERLEAVED]);

    // Act
    let betas = shipped_betas(&req);

    // Assert
    assert_eq!(betas, json!([CLAUDE_CODE, CONTEXT_1M, EFFORT, INTERLEAVED]));
}

#[test]
fn withholding_a_pinned_flag_the_client_also_sent_keeps_one_copy() {
    // Arrange: the router unions the pin into the client list, so the flag
    // arrives twice and also names the floor.
    let mut req = request_with_client_betas(&[CONTEXT_1M, INTERLEAVED, CONTEXT_1M]);
    req.routectl_internal.operator_betas = vec![CONTEXT_1M.to_string()];
    req.routectl_internal.withheld_betas = [CONTEXT_1M, INTERLEAVED].map(String::from).into();

    // Act
    let betas = shipped_betas(&req);

    // Assert
    assert_eq!(betas, json!([CONTEXT_1M]));
}

/// The dedup on the body array itself, where an entry can be any JSON value:
/// a non-string entry is kept verbatim (a repeat included), string entries
/// are compared as sent (no trim), and an empty string is a flag like any
/// other.
#[test]
fn dedup_keeps_non_string_entries_verbatim_and_compares_strings_as_sent() {
    struct Row {
        name: &'static str,
        betas: Option<Value>,
        want: Value,
    }
    let rows = [
        Row {
            name: "no array dedups to nothing",
            betas: None,
            want: json!([]),
        },
        Row {
            name: "a value that is not an array is ignored",
            betas: Some(json!(INTERLEAVED)),
            want: json!([]),
        },
        Row {
            name: "non-string entries keep their place, repeats included",
            betas: Some(json!([CONTEXT_1M, 7, CONTEXT_1M, null, {"x": 1}, 7, INTERLEAVED])),
            want: json!([CONTEXT_1M, 7, null, {"x": 1}, 7, INTERLEAVED]),
        },
        Row {
            name: "a whitespace-padded flag is a distinct flag",
            betas: Some(json!([CONTEXT_1M, " context-1m-2025-08-07 ", CONTEXT_1M])),
            want: json!([CONTEXT_1M, " context-1m-2025-08-07 "]),
        },
        Row {
            name: "an empty string is kept once",
            betas: Some(json!(["", INTERLEAVED, ""])),
            want: json!(["", INTERLEAVED]),
        },
        Row {
            name: "a repeat with a flag between keeps the first position",
            betas: Some(json!([EFFORT, INTERLEAVED, EFFORT])),
            want: json!([EFFORT, INTERLEAVED]),
        },
    ];

    for row in rows {
        let deduped = dedup_betas(row.betas.as_ref());

        assert_eq!(Value::Array(deduped), row.want, "{}", row.name);
    }
}
