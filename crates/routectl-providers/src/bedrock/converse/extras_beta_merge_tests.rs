//! The Converse bag's `anthropic_beta` is the configured floor followed by
//! the client's flags, each flag sent once at its first position. Entries
//! are compared as sent: nothing is trimmed and an empty string is a flag
//! like any other. No entry at all leaves the key off the bag.

use serde_json::{Map, Value, json};

use routectl_core::ChatRequest;

use super::insert_anthropic_beta;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CONTEXT_1M: &str = "context-1m-2025-08-07";
const CLAUDE_CODE: &str = "claude-code-20250219";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

fn merged_bag(floor: &[&str], client: &[&str]) -> Map<String, Value> {
    let cfg = BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: owned(floor),
        additional_model_request_fields: None,
    };
    let req = ChatRequest {
        anthropic_beta: owned(client),
        ..Default::default()
    };
    let mut bag = Map::new();
    insert_anthropic_beta(&cfg, &req, &mut bag);
    bag
}

#[test]
fn bag_merge_keeps_floor_then_client_order_with_each_flag_once() {
    struct Row {
        name: &'static str,
        floor: &'static [&'static str],
        client: &'static [&'static str],
        want: Value,
    }
    let rows = [
        Row {
            name: "a flag in both floor and client keeps its floor position",
            floor: &[CONTEXT_1M, CLAUDE_CODE],
            client: &[INTERLEAVED, CONTEXT_1M],
            want: json!([CONTEXT_1M, CLAUDE_CODE, INTERLEAVED]),
        },
        Row {
            name: "a client repeat with a flag between keeps the first position",
            floor: &[],
            client: &[CONTEXT_1M, INTERLEAVED, CONTEXT_1M],
            want: json!([CONTEXT_1M, INTERLEAVED]),
        },
        Row {
            name: "a floor repeat ships once",
            floor: &[CLAUDE_CODE, CLAUDE_CODE],
            client: &[],
            want: json!([CLAUDE_CODE]),
        },
        Row {
            name: "a whitespace-padded flag is a distinct flag",
            floor: &[CONTEXT_1M],
            client: &[" context-1m-2025-08-07 ", CONTEXT_1M],
            want: json!([CONTEXT_1M, " context-1m-2025-08-07 "]),
        },
        Row {
            name: "an empty string is kept once",
            floor: &[""],
            client: &[INTERLEAVED, ""],
            want: json!(["", INTERLEAVED]),
        },
    ];

    for row in rows {
        let bag = merged_bag(row.floor, row.client);

        assert_eq!(bag.get("anthropic_beta"), Some(&row.want), "{}", row.name);
    }
}

#[test]
fn bag_merge_with_no_flags_leaves_the_key_off() {
    let bag = merged_bag(&[], &[]);

    assert!(bag.is_empty(), "no flags must add no key: {bag:?}");
}
