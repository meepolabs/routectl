//! Lift Anthropic-shape `thinking` and `redacted_thinking` content
//! blocks on assistant messages into a `reasoning_details` envelope
//! field on the same message.
//!
//! When cc (or any Anthropic client) echoes the prior assistant turn
//! on multi-turn, it carries reasoning as content blocks:
//!
//!   "content": [
//!     {"type":"thinking", "thinking":"...", "signature":"..."},
//!     {"type":"redacted_thinking", "data":"..."},
//!     {"type":"text", "text":"..."}
//!   ]
//!
//! The OpenAI-compat wire shape does not understand these block types.
//! Without this lift, the blocks pass through verbatim (PartKind::Other
//! in `content.rs`) and any strict OpenAI-compat upstream 400s. For
//! upstreams that REQUIRE reasoning echo-back (DeepSeek v4+, recent
//! vLLM), the missing field causes the explicit error
//! `"The reasoning_content in the thinking mode must be passed back
//! to the API"`.
//!
//! This lift extracts every `thinking` and `redacted_thinking` part
//! from the assistant's content array, builds a typed
//! `reasoning_details` array on the message envelope, and leaves
//! the surviving content (text + tool_use) for the downstream lifts
//! to process. The dialect's `preserve_history_reasoning` runtime
//! then reads `reasoning_details` and (for the deepseek and vllm
//! dialects) flattens to `reasoning_content`; for openrouter the
//! typed array is sent as-is.
//!
//! No-op when the provider's `history_reasoning` is `Strip`: the
//! caller (`request::normalize`) handles the strip pass separately
//! AFTER `lift_all`, so leaving the typed array here is harmless --
//! the strip pass removes it cleanly.
//!
//! Runs after `content.rs` (which transforms images, drops documents)
//! and BEFORE `tool_use.rs` (which lifts `tool_use` content -> top-level
//! `tool_calls`). Order is pinned by `LIFT_STEPS` in `mod.rs`.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, Result};

const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

pub fn lift(
    _id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    _strict: bool,
) -> Result<()> {
    let Some(messages) = obj.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return Ok(());
    };
    for msg in messages.iter_mut() {
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };
        if msg_obj.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        rewrite_assistant_thinking(msg_obj);
    }
    Ok(())
}

fn rewrite_assistant_thinking(msg: &mut Map<String, Value>) {
    let Some(content_val) = msg.get("content") else {
        return;
    };
    let Some(parts) = content_val.as_array() else {
        return;
    };

    // Offset the lifted index counter past any pre-existing
    // reasoning_details entries' indexes. Without this, a message
    // with one pre-existing entry at `index: 0` and one lifted
    // thinking part would emit two entries both at `index: 0`,
    // breaking downstream consumers that key on detail_index for
    // block ordering / identity (notably the Anthropic ingress
    // renderer's per-detail_index thinking-block emission).
    let starting_index: u32 = msg
        .get("reasoning_details")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("index").and_then(|v| v.as_u64()))
                .max()
                .map(|m| (m as u32).saturating_add(1))
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let mut surviving: Vec<Value> = Vec::with_capacity(parts.len());
    let mut details: Vec<Value> = Vec::new();
    let mut detail_index: u32 = starting_index;
    for part in parts.iter().cloned() {
        let kind = part
            .as_object()
            .and_then(|o| o.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        match kind {
            "thinking" => {
                let obj = part.as_object().cloned().unwrap_or_default();
                let text = obj
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let mut entry = Map::new();
                entry.insert("type".into(), Value::String("reasoning.text".into()));
                entry.insert("format".into(), Value::String(ANTHROPIC_FORMAT.into()));
                entry.insert("index".into(), Value::from(detail_index));
                entry.insert("text".into(), Value::String(text));
                if let Some(sig) = obj.get("signature").cloned() {
                    if !sig.is_null() {
                        entry.insert("signature".into(), sig);
                    }
                }
                details.push(Value::Object(entry));
                detail_index += 1;
            }
            "redacted_thinking" => {
                let obj = part.as_object().cloned().unwrap_or_default();
                let data = obj
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let mut entry = Map::new();
                entry.insert("type".into(), Value::String("reasoning.encrypted".into()));
                entry.insert("format".into(), Value::String(ANTHROPIC_FORMAT.into()));
                entry.insert("index".into(), Value::from(detail_index));
                entry.insert("data".into(), Value::String(data));
                details.push(Value::Object(entry));
                detail_index += 1;
            }
            _ => {
                surviving.push(part);
            }
        }
    }

    if details.is_empty() {
        return;
    }

    // Replace content with surviving parts (or remove if empty -- the
    // tool_use lift downstream handles content collapse to string/null).
    if surviving.is_empty() {
        msg.insert("content".into(), Value::Array(Vec::new()));
    } else {
        msg.insert("content".into(), Value::Array(surviving));
    }

    // Merge with any pre-existing reasoning_details (e.g. from a
    // dialect that already populated some). New entries go AFTER
    // existing ones to preserve relative order with the model output.
    if let Some(existing) = msg
        .get_mut("reasoning_details")
        .and_then(|v| v.as_array_mut())
    {
        existing.extend(details);
    } else {
        msg.insert("reasoning_details".into(), Value::Array(details));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn empty_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    fn run(messages: Value) -> Map<String, Value> {
        let mut obj: Map<String, Value> = json!({"model":"m","messages":messages})
            .as_object()
            .unwrap()
            .clone();
        lift("test", &mut obj, &empty_req(), false).unwrap();
        obj
    }

    /// Pure-thinking assistant message: thinking block lifted to
    /// reasoning_details, content becomes empty array.
    #[test]
    fn pure_thinking_lifted_to_reasoning_details() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"thinking","thinking":"step 1","signature":"sig-a"}
            ]
        }]));
        let msg = &obj["messages"][0];
        let details = msg["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["type"], "reasoning.text");
        assert_eq!(details[0]["text"], "step 1");
        assert_eq!(details[0]["signature"], "sig-a");
        assert_eq!(details[0]["format"], ANTHROPIC_FORMAT);
        assert_eq!(msg["content"], json!([]));
    }

    /// Mixed assistant content: thinking + text + tool_use. Thinking
    /// extracted to reasoning_details; text + tool_use preserved in
    /// content for downstream lifts.
    #[test]
    fn mixed_content_preserves_text_and_tool_use() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"thinking","thinking":"reasoning text","signature":"sig"},
                {"type":"text","text":"hello"},
                {"type":"tool_use","id":"toolu_X","name":"f","input":{}}
            ]
        }]));
        let msg = &obj["messages"][0];
        assert_eq!(msg["reasoning_details"].as_array().unwrap().len(), 1);
        let surviving = msg["content"].as_array().unwrap();
        assert_eq!(surviving.len(), 2);
        assert_eq!(surviving[0]["type"], "text");
        assert_eq!(surviving[1]["type"], "tool_use");
    }

    /// redacted_thinking block lifts to reasoning.encrypted entry with
    /// the `data` field carried through.
    #[test]
    fn redacted_thinking_lifted_to_encrypted_entry() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"redacted_thinking","data":"opaque-base64"}
            ]
        }]));
        let msg = &obj["messages"][0];
        let details = msg["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["type"], "reasoning.encrypted");
        assert_eq!(details[0]["data"], "opaque-base64");
        assert_eq!(msg["content"], json!([]));
    }

    /// Both kinds in the same message preserve order via `index`.
    #[test]
    fn mixed_thinking_and_redacted_keep_order_via_index() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"thinking","thinking":"a","signature":""},
                {"type":"redacted_thinking","data":"b"},
                {"type":"thinking","thinking":"c","signature":""}
            ]
        }]));
        let msg = &obj["messages"][0];
        let details = msg["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 3);
        assert_eq!(details[0]["index"], 0);
        assert_eq!(details[0]["text"], "a");
        assert_eq!(details[1]["index"], 1);
        assert_eq!(details[1]["data"], "b");
        assert_eq!(details[2]["index"], 2);
        assert_eq!(details[2]["text"], "c");
    }

    /// User messages with thinking parts are untouched (shouldn't
    /// happen in practice but defensive).
    #[test]
    fn user_message_thinking_blocks_untouched() {
        let obj = run(json!([{
            "role": "user",
            "content": [
                {"type":"thinking","thinking":"x","signature":""}
            ]
        }]));
        let msg = &obj["messages"][0];
        assert!(msg.get("reasoning_details").is_none());
        // Content stays exactly as-is.
        assert_eq!(msg["content"][0]["type"], "thinking");
    }

    /// Pre-existing reasoning_details on the message envelope get
    /// extended (not overwritten) by lifted thinking parts. The
    /// lifted entry's `index` must be offset past the max existing
    /// index so downstream consumers keying on detail_index for
    /// block ordering / identity see unique values.
    #[test]
    fn lifted_details_appended_to_existing_array() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"thinking","thinking":"new","signature":""}
            ],
            "reasoning_details": [
                {"type":"reasoning.text","format":"deepseek-v1","index":0,"text":"prior"}
            ]
        }]));
        let msg = &obj["messages"][0];
        let details = msg["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0]["text"], "prior");
        assert_eq!(details[0]["index"], 0);
        assert_eq!(details[1]["text"], "new");
        // Lifted entry must NOT collide with the pre-existing index=0.
        assert_eq!(
            details[1]["index"], 1,
            "lifted index must offset past existing max"
        );
    }

    /// Review follow-up to Bug H: when a message already carries
    /// multiple reasoning_details with non-contiguous indexes (e.g.
    /// 0, 2, 5), the lifted entries' indexes must all start past
    /// max+1 (here: 6) AND each lifted entry's index must be unique
    /// among the lifted set. Pre-fix, both lifted entries would
    /// have indexes 0 and 1, colliding with the existing 0.
    #[test]
    fn lifted_details_offset_past_max_existing_index_and_stay_unique() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [
                {"type":"thinking","thinking":"new-a","signature":""},
                {"type":"redacted_thinking","data":"opaque"},
                {"type":"thinking","thinking":"new-c","signature":""}
            ],
            "reasoning_details": [
                {"type":"reasoning.text","format":"deepseek-v1","index":0,"text":"p0"},
                {"type":"reasoning.text","format":"deepseek-v1","index":2,"text":"p2"},
                {"type":"reasoning.text","format":"deepseek-v1","index":5,"text":"p5"}
            ]
        }]));
        let msg = &obj["messages"][0];
        let details = msg["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 6, "3 existing + 3 lifted");
        // Existing entries preserve their indexes.
        assert_eq!(details[0]["index"], 0);
        assert_eq!(details[1]["index"], 2);
        assert_eq!(details[2]["index"], 5);
        // Lifted entries start at max+1 = 6 and increment.
        assert_eq!(details[3]["index"], 6);
        assert_eq!(details[3]["text"], "new-a");
        assert_eq!(details[4]["index"], 7);
        assert_eq!(details[4]["data"], "opaque");
        assert_eq!(details[5]["index"], 8);
        assert_eq!(details[5]["text"], "new-c");
        // All indexes are unique across the merged array.
        let mut seen = std::collections::HashSet::new();
        for d in details {
            let idx = d["index"].as_u64().expect("index is u64");
            assert!(seen.insert(idx), "duplicate index {idx} in merged details");
        }
    }

    /// String content (legacy shape) is a no-op.
    #[test]
    fn string_content_no_op() {
        let obj = run(json!([{"role":"assistant","content":"hi"}]));
        let msg = &obj["messages"][0];
        assert!(msg.get("reasoning_details").is_none());
        assert_eq!(msg["content"], "hi");
    }

    /// Assistant with no thinking parts is a no-op.
    #[test]
    fn no_thinking_no_op() {
        let obj = run(json!([{
            "role": "assistant",
            "content": [{"type":"text","text":"plain"}]
        }]));
        let msg = &obj["messages"][0];
        assert!(msg.get("reasoning_details").is_none());
        assert_eq!(msg["content"][0]["text"], "plain");
    }
}
