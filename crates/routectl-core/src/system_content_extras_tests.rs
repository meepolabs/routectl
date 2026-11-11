use serde_json::{Value, json};

use super::*;

/// The derived `SystemBlock` the hand-written deserializer replaced, kept
/// as the reference its outcomes are compared against. Same type name, so
/// the derived error text matches.
mod reference {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::{CacheControl, default_text_type};

    #[derive(Debug, Serialize, Deserialize)]
    pub(super) struct SystemBlock {
        #[serde(rename = "type", default = "default_text_type")]
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<Value>,
    }
}

use reference::SystemBlock as ReferenceSystemBlock;

fn outcome(parsed: Result<SystemBlock, serde_json::Error>) -> Result<Value, String> {
    parsed
        .map(|b| serde_json::to_value(&b).unwrap())
        .map_err(|e| e.to_string())
}

fn reference_outcome(
    parsed: Result<ReferenceSystemBlock, serde_json::Error>,
) -> Result<Value, String> {
    parsed
        .map(|b| serde_json::to_value(&b).unwrap())
        .map_err(|e| e.to_string())
}

fn corpus() -> Vec<Value> {
    let full = json!({"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}, "citations": []});
    let obj = full.as_object().unwrap().clone();
    let probes = [
        json!(null),
        json!(1),
        json!("s"),
        json!(true),
        json!([]),
        json!({}),
        json!({"type": "ephemeral", "ttl": "1h"}),
    ];
    let mut out = vec![full.clone(), json!("text"), json!(null), json!(3)];
    for key in obj.keys() {
        let mut without = obj.clone();
        without.remove(key);
        out.push(Value::Object(without));
        for v in &probes {
            let mut with = obj.clone();
            with.insert(key.clone(), v.clone());
            out.push(Value::Object(with));
        }
    }
    for v in &probes {
        let mut with = obj.clone();
        with.insert("caller".into(), v.clone());
        out.push(Value::Object(with));
    }
    for arr in [
        json!([]),
        json!(["text"]),
        json!(["text", "t"]),
        json!(["text", "t", null, null]),
        json!(["text", "t", null, null, 5]),
        json!([1, "t"]),
    ] {
        out.push(arr);
    }
    out
}

#[test]
fn every_input_parses_as_the_replaced_derive_parsed_it() {
    for input in corpus() {
        // Act
        let new = outcome(serde_json::from_value(input.clone()));
        let old = reference_outcome(serde_json::from_value(input.clone()));
        let text = serde_json::to_string(&input).unwrap();
        let new_text = outcome(serde_json::from_str(&text));
        let old_text = reference_outcome(serde_json::from_str(&text));

        // Assert
        assert_eq!(new, old, "from_value diverged on {input}");
        assert_eq!(new_text, old_text, "from_str diverged on {text}");
    }
    for text in [
        r#"{"text":"a","text":"b"}"#,
        r#"{"text":"a","caller":1,"caller":2}"#,
    ] {
        assert_eq!(
            outcome(serde_json::from_str(text)),
            reference_outcome(serde_json::from_str(text)),
            "{text}"
        );
    }
}

#[test]
fn system_content_shape_selection_is_unchanged() {
    for input in [
        json!([{"text": "x", "type": null}]),
        json!([{"text": "x", "cache_control": 1}]),
        json!([["text", "x"]]),
        json!("flat"),
        json!(5),
    ] {
        // Act
        let parsed = serde_json::from_value::<SystemContent>(input.clone());

        // Assert
        let reference = serde_json::from_value::<Vec<ReferenceSystemBlock>>(input.clone())
            .map(drop)
            .or_else(|_| serde_json::from_value::<String>(input.clone()).map(drop));
        assert_eq!(parsed.is_ok(), reference.is_ok(), "{input}");
    }
}

#[test]
fn system_block_typed_members_equal_its_serialized_keys() {
    // Arrange
    let block = SystemBlock {
        kind: "text".into(),
        text: "t".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: Some(json!([])),
        extras: Map::new(),
    };

    // Act
    let wire = serde_json::to_value(&block).unwrap();

    // Assert
    let mut serialized: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut typed = SYSTEM_BLOCK_MEMBERS.to_vec();
    serialized.sort_unstable();
    typed.sort_unstable();
    assert_eq!(typed, serialized);
}

#[test]
fn system_block_leftover_members_land_in_extras() {
    // (row, input, expected extras)
    let rows = [
        (
            "caller",
            json!({"type": "text", "text": "t", "caller": {"type": "direct"}}),
            json!({"caller": {"type": "direct"}}),
        ),
        (
            "non_ascii_value",
            json!({"text": "t", "note": "caf\u{e9}"}),
            json!({"note": "caf\u{e9}"}),
        ),
        (
            "empty_map",
            json!({"text": "t", "citations": null}),
            json!({}),
        ),
        (
            "reserved_name_collision",
            json!({"text": "typed", "extras": {"text": "nested"}}),
            json!({"extras": {"text": "nested"}}),
        ),
    ];
    for (row, input, expected) in rows {
        // Act
        let block: SystemBlock = serde_json::from_value(input).unwrap();

        // Assert
        assert_eq!(Value::Object(block.extras.clone()), expected, "row `{row}`");
        let wire = serde_json::to_value(&block).unwrap();
        for key in expected.as_object().unwrap().keys() {
            assert!(wire.get(key).is_none(), "row `{row}` emitted `{key}`");
        }
    }
}

#[test]
fn system_block_typed_text_stays_typed_beside_a_colliding_extra() {
    // Act
    let block: SystemBlock =
        serde_json::from_value(json!({"text": "typed", "extras": {"text": "nested"}})).unwrap();

    // Assert
    assert_eq!(block.text, "typed");
    assert!(!block.extras.contains_key("text"));
}
