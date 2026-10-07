//! Tests for the bounded Responses store.

use super::*;
use routectl_core::Message;
use serde_json::json;

#[test]
fn insert_and_get_round_trips() {
    let s = ResponsesStore::new(4);
    assert!(s.is_empty());
    s.insert(
        "resp_a".into(),
        json!({"status": "completed"}),
        vec![Message {
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }],
    );
    assert_eq!(s.len(), 1);
    assert_eq!(s.get("resp_a").unwrap()["status"], json!("completed"));
    let (response, context) = s.get_full("resp_a").expect("full entry");
    assert_eq!(response["status"], json!("completed"));
    assert_eq!(context.messages.len(), 1);
}

#[test]
fn unknown_id_is_none() {
    let s = ResponsesStore::new(4);
    assert!(s.get("nope").is_none());
    assert!(s.get_full("nope").is_none());
}

#[test]
fn fifo_eviction_drops_oldest() {
    let s = ResponsesStore::new(2);
    s.insert("a".into(), json!({"i": 1}), Vec::new());
    s.insert("b".into(), json!({"i": 2}), Vec::new());
    s.insert("c".into(), json!({"i": 3}), Vec::new());
    assert!(s.get("a").is_none());
    assert!(s.get("b").is_some());
    assert!(s.get("c").is_some());
    assert_eq!(s.len(), 2);
}

#[test]
fn overwrite_keeps_slot_and_updates_entry() {
    let s = ResponsesStore::new(2);
    s.insert("a".into(), json!({"v": 1}), Vec::new());
    s.insert("b".into(), json!({"v": 2}), Vec::new());
    // Overwrite a: still one slot for a, updated payload.
    s.insert("a".into(), json!({"v": 11}), Vec::new());
    assert_eq!(s.len(), 2);
    assert_eq!(s.get("a").unwrap()["v"], json!(11));
    assert!(s.get("b").is_some());
    // Next insert evicts by queue position (a's ORIGINAL slot), so b
    // survives only if a's slot is the front... a was queued first, so
    // the next eviction removes a.
    s.insert("c".into(), json!({"v": 3}), Vec::new());
    assert!(s.get("a").is_none(), "a's original queue slot evicted");
    assert!(s.get("b").is_some());
    assert!(s.get("c").is_some());
}
