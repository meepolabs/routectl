//! Tests for [`ThinkingObject`]: the object-only, display-free invariant
//! and copy-on-write mutation.

use serde_json::{Value, json};

use super::*;

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("fixture must be an object, got {other}"),
    }
}

#[test]
fn from_client_refuses_every_non_object() {
    let rows = [
        ("null", Value::Null),
        ("bool", json!(true)),
        ("number", json!(1024)),
        ("string", json!("enabled")),
        ("array", json!([{"type": "enabled"}])),
        ("empty array", json!([])),
    ];

    for (name, value) in rows {
        assert_eq!(ThinkingObject::from_client(value), None, "row {name}");
    }
}

#[test]
fn from_client_removes_top_level_display_and_keeps_every_other_key() {
    let sent = json!({
        "type": "adaptive",
        "display": "summarized",
        "budget_tokens": 2048,
        "between_tools": true,
        "future_key": {"display": "nested", "x": [1, 2]}
    });
    assert!(sent.get("display").is_some(), "fixture must carry display");

    let thinking = ThinkingObject::from_client(sent).expect("object accepted");

    assert_eq!(
        thinking.as_object(),
        &object(json!({
            "type": "adaptive",
            "budget_tokens": 2048,
            "between_tools": true,
            "future_key": {"display": "nested", "x": [1, 2]}
        }))
    );
}

#[test]
fn from_client_accepts_empty_and_display_only_objects() {
    let rows = [
        ("empty", json!({}), json!({})),
        ("display only", json!({"display": "omitted"}), json!({})),
        (
            "no display",
            json!({"type": "enabled", "budget_tokens": 1024}),
            json!({"type": "enabled", "budget_tokens": 1024}),
        ),
    ];

    for (name, sent, expected) in rows {
        let thinking = ThinkingObject::from_client(sent).expect(name);
        assert_eq!(thinking.as_object(), &object(expected), "row {name}");
    }
}

#[test]
fn thinking_type_reads_only_a_string_type() {
    let rows = [
        ("string", json!({"type": "enabled"}), Some("enabled")),
        (
            "unknown string",
            json!({"type": "brand_new"}),
            Some("brand_new"),
        ),
        ("absent", json!({"budget_tokens": 1}), None),
        ("number", json!({"type": 1}), None),
        ("null", json!({"type": null}), None),
        ("object", json!({"type": {"name": "enabled"}}), None),
    ];

    for (name, sent, expected) in rows {
        let thinking = ThinkingObject::from_client(sent).expect(name);
        assert_eq!(thinking.thinking_type(), expected, "row {name}");
    }
}

#[test]
fn clone_shares_the_allocation_until_mutated() {
    let original =
        ThinkingObject::from_client(json!({"type": "enabled", "budget_tokens": 1024})).unwrap();

    let mut staged = original.clone();
    assert!(staged.shares_allocation_with(&original));

    staged.set_thinking_type("adaptive");

    assert!(!staged.shares_allocation_with(&original));
    assert_eq!(original.thinking_type(), Some("enabled"));
    assert_eq!(staged.thinking_type(), Some("adaptive"));
    assert_eq!(
        staged.as_object(),
        &object(json!({"type": "adaptive", "budget_tokens": 1024}))
    );
}

#[test]
fn set_thinking_type_inserts_type_when_absent() {
    let original = ThinkingObject::from_client(json!({"budget_tokens": 1024})).unwrap();
    let mut staged = original.clone();

    staged.set_thinking_type("disabled");

    assert_eq!(original.thinking_type(), None);
    assert_eq!(
        staged.as_object(),
        &object(json!({"type": "disabled", "budget_tokens": 1024}))
    );
}

#[test]
fn set_thinking_type_overwrites_a_non_string_type() {
    let mut thinking = ThinkingObject::from_client(json!({"type": 7})).unwrap();

    thinking.set_thinking_type("enabled");

    assert_eq!(thinking.thinking_type(), Some("enabled"));
}
