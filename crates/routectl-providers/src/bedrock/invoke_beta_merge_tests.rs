//! The Invoke body's `anthropic_beta` is the configured floor followed by
//! the client's flags, each flag sent once. The fixture config carries no
//! `additional_model_request_fields`, so nothing but the merge (and the
//! withhold) shapes the array.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, Message, MessageContent, Role};

use super::{merge_floor_and_client_betas, normalize_request};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CONTEXT_1M: &str = "context-1m-2025-08-07";
const CLAUDE_CODE: &str = "claude-code-20250219";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";
const EFFORT: &str = "effort-2025-11-24";

fn cfg_with_floor(floor: &[&str]) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: floor.iter().map(|f| (*f).to_string()).collect(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
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
        }]
        .into(),
        anthropic_beta: client.iter().map(|f| (*f).to_string()).collect(),
        ..Default::default()
    }
}

fn shipped_betas(cfg: &BedrockConfig, req: &ChatRequest) -> Value {
    let body = normalize_request(cfg, req).expect("invoke body assembles");
    body.get("anthropic_beta").cloned().unwrap_or(Value::Null)
}

#[test]
fn a_flag_in_both_floor_and_client_ships_once_in_floor_position() {
    // Arrange
    let cfg = cfg_with_floor(&[CONTEXT_1M, CLAUDE_CODE]);
    let req = request_with_client_betas(&[INTERLEAVED, CONTEXT_1M]);

    // Act
    let betas = shipped_betas(&cfg, &req);

    // Assert
    assert_eq!(betas, json!([CONTEXT_1M, CLAUDE_CODE, INTERLEAVED]));
}

#[test]
fn a_client_repeat_with_no_floor_ships_once() {
    // Arrange
    let cfg = cfg_with_floor(&[]);
    let req = request_with_client_betas(&[CONTEXT_1M, CONTEXT_1M]);

    // Act
    let betas = shipped_betas(&cfg, &req);

    // Assert
    assert_eq!(betas, json!([CONTEXT_1M]));
}

#[test]
fn distinct_flags_keep_floor_order_then_client_order() {
    // Arrange
    let cfg = cfg_with_floor(&[CLAUDE_CODE, CONTEXT_1M]);
    let req = request_with_client_betas(&[EFFORT, INTERLEAVED]);

    // Act
    let betas = shipped_betas(&cfg, &req);

    // Assert
    assert_eq!(betas, json!([CLAUDE_CODE, CONTEXT_1M, EFFORT, INTERLEAVED]));
}

#[test]
fn withholding_a_floor_flag_the_client_also_sent_keeps_one_copy() {
    // Arrange
    let cfg = cfg_with_floor(&[CONTEXT_1M]);
    let mut req = request_with_client_betas(&[CONTEXT_1M, INTERLEAVED]);
    req.routectl_internal.withheld_betas = [CONTEXT_1M, INTERLEAVED].map(String::from).into();

    // Act
    let betas = shipped_betas(&cfg, &req);

    // Assert
    assert_eq!(betas, json!([CONTEXT_1M]));
}

/// The merge on the body array itself, where a client entry can be any JSON
/// value: a non-string entry is kept verbatim (a repeat included), string
/// entries are compared as sent (no trim), and an empty string is a flag
/// like any other.
#[test]
fn merge_keeps_non_string_entries_verbatim_and_compares_strings_as_sent() {
    struct Row {
        name: &'static str,
        floor: &'static [&'static str],
        client: Option<Value>,
        want: Value,
    }
    let rows = [
        Row {
            name: "no floor and no client array merges to nothing",
            floor: &[],
            client: None,
            want: json!([]),
        },
        Row {
            name: "a client value that is not an array is ignored",
            floor: &[CONTEXT_1M],
            client: Some(json!(INTERLEAVED)),
            want: json!([CONTEXT_1M]),
        },
        Row {
            name: "a floor repeat ships once",
            floor: &[CONTEXT_1M, CLAUDE_CODE, CONTEXT_1M],
            client: Some(json!([])),
            want: json!([CONTEXT_1M, CLAUDE_CODE]),
        },
        Row {
            name: "non-string entries keep their place, repeats included",
            floor: &[CONTEXT_1M],
            client: Some(json!([7, CONTEXT_1M, null, {"x": 1}, 7, INTERLEAVED])),
            want: json!([CONTEXT_1M, 7, null, {"x": 1}, 7, INTERLEAVED]),
        },
        Row {
            name: "a whitespace-padded flag is a distinct flag",
            floor: &[CONTEXT_1M],
            client: Some(json!([" context-1m-2025-08-07 ", CONTEXT_1M])),
            want: json!([CONTEXT_1M, " context-1m-2025-08-07 "]),
        },
        Row {
            name: "an empty string is kept once",
            floor: &[],
            client: Some(json!(["", INTERLEAVED, ""])),
            want: json!(["", INTERLEAVED]),
        },
        Row {
            name: "a client repeat with a flag between keeps the first position",
            floor: &[],
            client: Some(json!([EFFORT, INTERLEAVED, EFFORT])),
            want: json!([EFFORT, INTERLEAVED]),
        },
    ];

    for row in rows {
        let floor: Vec<String> = row.floor.iter().map(|f| (*f).to_string()).collect();

        let merged = merge_floor_and_client_betas(&floor, row.client.as_ref());

        assert_eq!(Value::Array(merged), row.want, "{}", row.name);
    }
}
