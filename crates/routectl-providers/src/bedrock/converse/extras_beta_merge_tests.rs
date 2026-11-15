//! The Converse bag's `anthropic_beta` is the request's flags (the router
//! has already unioned the operator's `header_extras` pins into them), each
//! sent once at its first position. Entries are compared as sent: nothing is
//! trimmed and an empty string is a flag like any other. No entry at all
//! leaves the key off the bag.

use serde_json::{Map, Value, json};

use routectl_core::ChatRequest;

use super::insert_anthropic_beta;

const CONTEXT_1M: &str = "context-1m-2025-08-07";
const CLAUDE_CODE: &str = "claude-code-20250219";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";

fn bag_for(betas: &[&str]) -> Map<String, Value> {
    let req = ChatRequest {
        anthropic_beta: betas.iter().map(|f| (*f).to_string()).collect(),
        ..Default::default()
    };
    let mut bag = Map::new();
    insert_anthropic_beta(&req, &mut bag);
    bag
}

#[test]
fn bag_keeps_request_order_with_each_flag_once() {
    struct Row {
        name: &'static str,
        betas: &'static [&'static str],
        want: Value,
    }
    let rows = [
        Row {
            name: "distinct flags keep their order",
            betas: &[CONTEXT_1M, CLAUDE_CODE, INTERLEAVED],
            want: json!([CONTEXT_1M, CLAUDE_CODE, INTERLEAVED]),
        },
        Row {
            name: "a repeat with a flag between keeps the first position",
            betas: &[CONTEXT_1M, INTERLEAVED, CONTEXT_1M],
            want: json!([CONTEXT_1M, INTERLEAVED]),
        },
        Row {
            name: "a whitespace-padded flag is a distinct flag",
            betas: &[CONTEXT_1M, " context-1m-2025-08-07 ", CONTEXT_1M],
            want: json!([CONTEXT_1M, " context-1m-2025-08-07 "]),
        },
        Row {
            name: "an empty string is kept once",
            betas: &["", INTERLEAVED, ""],
            want: json!(["", INTERLEAVED]),
        },
    ];

    for row in rows {
        let bag = bag_for(row.betas);

        assert_eq!(bag.get("anthropic_beta"), Some(&row.want), "{}", row.name);
    }
}

#[test]
fn bag_with_no_flags_leaves_the_key_off() {
    let bag = bag_for(&[]);

    assert!(bag.is_empty(), "no flags must add no key: {bag:?}");
}
