use serde_json::{Map, Value, json};

use super::*;
use crate::content_part::{ContentPart, KnownContentPart};

/// The derived pair the single-pass deserializer replaced, kept as the
/// reference its outcomes are compared against.
mod reference {
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value};

    use crate::cache_control::CacheControl;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(untagged)]
    pub(super) enum ContentPart {
        Known(KnownContentPart),
        Other {
            #[serde(rename = "type")]
            type_tag: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
            #[serde(flatten)]
            extras: Map<String, Value>,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(super) enum KnownContentPart {
        Text {
            text: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            citations: Option<Value>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        Image {
            source: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        ImageUrl {
            image_url: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        File {
            file: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        Document {
            source: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            title: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            citations: Option<Value>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        ToolUse {
            id: String,
            name: String,
            input: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        ToolResult {
            tool_use_id: String,
            content: Value,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            is_error: Option<bool>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        },
        Thinking {
            thinking: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            signature: Option<String>,
        },
        RedactedThinking {
            data: String,
        },
    }
}

/// What a parse produced, comparable across the two implementations:
/// the variant family and the canonical serialization, or the error text.
#[derive(Debug, PartialEq)]
enum Outcome {
    Known(Value),
    Other(Value),
    Rejected(String),
}

fn outcome_of(part: Result<ContentPart, serde_json::Error>) -> Outcome {
    match part {
        Ok(p @ ContentPart::Known(_)) => Outcome::Known(serde_json::to_value(&p).unwrap()),
        Ok(p @ ContentPart::Other { .. }) => Outcome::Other(serde_json::to_value(&p).unwrap()),
        Err(e) => Outcome::Rejected(e.to_string()),
    }
}

fn reference_outcome_of(part: Result<reference::ContentPart, serde_json::Error>) -> Outcome {
    match part {
        Ok(p @ reference::ContentPart::Known(_)) => {
            Outcome::Known(serde_json::to_value(&p).unwrap())
        }
        Ok(p @ reference::ContentPart::Other { .. }) => {
            Outcome::Other(serde_json::to_value(&p).unwrap())
        }
        Err(e) => Outcome::Rejected(e.to_string()),
    }
}

/// Values every typed member is tried with: each JSON kind, plus the two
/// well-formed `cache_control` shapes and a malformed one.
fn member_values() -> Vec<Value> {
    vec![
        json!(null),
        json!(true),
        json!(7),
        json!(-1),
        json!(1.5),
        json!("s"),
        json!("caf\u{e9}"),
        json!([]),
        json!(["a"]),
        json!({}),
        json!({"type": "ephemeral"}),
        json!({"type": "ephemeral", "ttl": "1h"}),
        json!({"ttl": "1h"}),
    ]
}

/// A well-formed block of each shape, every optional member present.
fn full_blocks() -> Vec<Value> {
    let cc = json!({"type": "ephemeral", "ttl": "5m"});
    vec![
        json!({"type": "text", "text": "hi", "citations": [], "cache_control": cc}),
        json!({"type": "image", "source": {"type": "url", "url": "u"}, "cache_control": cc}),
        json!({"type": "image_url", "image_url": {"url": "u"}, "cache_control": cc}),
        json!({"type": "file", "file": {"file_id": "f"}, "cache_control": cc}),
        json!({"type": "document", "source": {}, "title": "t", "citations": {}, "cache_control": cc}),
        json!({"type": "tool_use", "id": "a", "name": "b", "input": {}, "cache_control": cc}),
        json!({"type": "tool_result", "tool_use_id": "a", "content": "c", "is_error": false, "cache_control": cc}),
        json!({"type": "thinking", "thinking": "t", "signature": "s"}),
        json!({"type": "redacted_thinking", "data": "d"}),
    ]
}

/// Dense corpus: each full block; each with one member removed; each with
/// one member replaced by every probe value; each with an unknown member;
/// tags of every kind; positional arrays; non-object scalars.
fn corpus() -> Vec<Value> {
    let mut out = Vec::new();
    for block in full_blocks() {
        let obj = block.as_object().unwrap().clone();
        out.push(block.clone());
        for key in obj.keys() {
            let mut without = obj.clone();
            without.remove(key);
            out.push(Value::Object(without));
            for v in member_values() {
                let mut with = obj.clone();
                with.insert(key.clone(), v);
                out.push(Value::Object(with));
            }
        }
        for v in member_values() {
            let mut with = obj.clone();
            with.insert("caller".into(), v.clone());
            out.push(Value::Object(with.clone()));
            with.insert("cache_control".into(), v);
            out.push(Value::Object(with));
        }
        let positional: Vec<Value> = obj.values().cloned().collect();
        out.push(Value::Array(positional));
    }
    for tag in [
        json!(0),
        json!(5),
        json!(8),
        json!(9),
        json!(-1),
        json!(1.0),
        json!("zz"),
        json!("Text"),
    ] {
        out.push(json!({"type": tag, "text": "x", "id": "a", "name": "b", "input": 1}));
        out.push(json!({"type": tag}));
    }
    for arr in [
        json!([]),
        json!(["text"]),
        json!(["text", "hi"]),
        json!(["text", "hi", null, {"type": "ephemeral"}]),
        json!(["text", "hi", null, null, "extra"]),
        json!(["text", 5]),
        json!(["zz", "hi"]),
        json!([0, "hi"]),
        json!(["tool_use", "a", "b", {}]),
        json!(["redacted_thinking", "d", "extra"]),
    ] {
        out.push(arr);
    }
    for scalar in [json!(null), json!(true), json!(3), json!("text"), json!({})] {
        out.push(scalar);
    }
    out.push(json!({"type": "zz", "cache_control": null, "caller": "x"}));
    out.push(json!({"type": "zz", "cache_control": 5}));
    out.push(json!({"cache_control": {"type": "ephemeral"}, "text": "x"}));
    out
}

#[test]
fn every_input_parses_as_the_replaced_untagged_derive_parsed_it() {
    let corpus = corpus();
    assert!(corpus.len() > 600, "corpus shrank to {}", corpus.len());
    let mut families = [0usize; 3];
    for input in &corpus {
        // Act
        let new = outcome_of(serde_json::from_value(input.clone()));
        let old = reference_outcome_of(serde_json::from_value(input.clone()));
        let text = serde_json::to_string(input).unwrap();
        let new_text = outcome_of(serde_json::from_str(&text));
        let old_text = reference_outcome_of(serde_json::from_str(&text));

        // Assert
        assert_eq!(new, old, "from_value diverged on {input}");
        assert_eq!(new_text, old_text, "from_str diverged on {text}");
        families[match new {
            Outcome::Known(_) => 0,
            Outcome::Other(_) => 1,
            Outcome::Rejected(_) => 2,
        }] += 1;
    }
    assert!(
        families.iter().all(|n| *n > 20),
        "corpus must exercise known, other and rejected outcomes: {families:?}"
    );
}

#[test]
fn a_repeated_member_in_a_text_body_parses_as_the_replaced_derive_parsed_it() {
    for text in [
        r#"{"type":"text","text":"a","text":"b"}"#,
        r#"{"type":"text","text":"a","type":"image"}"#,
        r#"{"type":"text","text":"a","cache_control":null,"cache_control":null}"#,
        r#"{"type":"zz","cache_control":null,"cache_control":null}"#,
        r#"{"type":"zz","a":1,"a":2}"#,
        r#"{"type":"tool_use","id":"a","name":"b","input":{},"caller":1,"caller":2}"#,
    ] {
        // Act
        let new = outcome_of(serde_json::from_str(text));
        let old = reference_outcome_of(serde_json::from_str(text));

        // Assert
        assert_eq!(new, old, "diverged on {text}");
    }
}

/// `MessageContent` as it was derived over the replaced pair: the path
/// every ingress reaches a content block through. Same type name, so the
/// derived error text matches.
mod reference_content {
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(untagged)]
    pub(super) enum MessageContent {
        Text(String),
        Parts(Vec<super::reference::ContentPart>),
        Null,
    }
}

use reference_content::MessageContent as ReferenceMessageContent;

#[test]
fn every_input_inside_message_content_parses_as_before() {
    for block in corpus() {
        // Arrange
        let input = json!([block, {"type": "text", "text": "tail", "caller": 1}]);
        let text = serde_json::to_string(&input).unwrap();

        // Act
        let new = serde_json::from_str::<crate::MessageContent>(&text)
            .map(|c| serde_json::to_value(&c).unwrap())
            .map_err(|e| e.to_string());
        let old = serde_json::from_str::<ReferenceMessageContent>(&text)
            .map(|c| serde_json::to_value(&c).unwrap())
            .map_err(|e| e.to_string());

        // Assert
        assert_eq!(new, old, "diverged on {text}");
    }
}

/// A fully populated value of each variant, built from the canonical type
/// so the weld reads the derived `Serialize` key set.
fn populated(shape: Shape) -> KnownContentPart {
    let cc = Some(crate::CacheControl::ephemeral_5m());
    let extras = Map::new();
    match shape {
        Shape::Text => KnownContentPart::Text {
            text: "t".into(),
            citations: Some(json!([])),
            cache_control: cc,
            extras,
        },
        Shape::Image => KnownContentPart::Image {
            source: json!({}),
            cache_control: cc,
            extras,
        },
        Shape::ImageUrl => KnownContentPart::ImageUrl {
            image_url: json!({}),
            cache_control: cc,
            extras,
        },
        Shape::File => KnownContentPart::File {
            file: json!({}),
            cache_control: cc,
            extras,
        },
        Shape::Document => KnownContentPart::Document {
            source: json!({}),
            title: Some("t".into()),
            citations: Some(json!({})),
            cache_control: cc,
            extras,
        },
        Shape::ToolUse => KnownContentPart::ToolUse {
            id: "i".into(),
            name: "n".into(),
            input: json!({}),
            cache_control: cc,
            extras,
        },
        Shape::ToolResult => KnownContentPart::ToolResult {
            tool_use_id: "i".into(),
            content: json!("c"),
            is_error: Some(true),
            cache_control: cc,
            extras,
        },
        Shape::Thinking => KnownContentPart::Thinking {
            thinking: "t".into(),
            signature: Some("s".into()),
            extras,
        },
        Shape::RedactedThinking => KnownContentPart::RedactedThinking {
            data: "d".into(),
            extras,
        },
    }
}

#[test]
fn each_shapes_typed_members_equal_its_serialized_keys() {
    for (tag, shape) in SHAPES {
        // Arrange
        let part = populated(shape);
        assert_eq!(part.type_tag(), tag, "SHAPES order drifted at `{tag}`");

        // Act
        let wire = serde_json::to_value(&part).unwrap();

        // Assert
        let mut serialized: Vec<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut typed: Vec<&str> = shape.typed_member_names().collect();
        serialized.sort_unstable();
        typed.sort_unstable();
        assert_eq!(typed, serialized, "shape `{tag}`");
    }
}

#[test]
fn shapes_are_listed_in_declaration_order() {
    // A numeric tag selects a variant by declaration index, as the derived
    // internally tagged enum did, so the list order is part of the contract.
    for (index, (tag, _)) in SHAPES.iter().enumerate() {
        // Act
        let reference: reference::KnownContentPart =
            serde_json::from_value(serde_json::to_value(populated(SHAPES[index].1)).unwrap())
                .unwrap();
        let mut numeric = serde_json::to_value(&reference).unwrap();
        numeric["type"] = json!(index);
        let parsed: ContentPart = serde_json::from_value(numeric).unwrap();

        // Assert
        assert_eq!(parsed.type_tag(), *tag);
    }
}

fn extras_of(part: &ContentPart) -> &Map<String, Value> {
    let ContentPart::Known(known) = part else {
        panic!("expected a known part, got {part:?}");
    };
    match known {
        KnownContentPart::Text { extras, .. }
        | KnownContentPart::Image { extras, .. }
        | KnownContentPart::ImageUrl { extras, .. }
        | KnownContentPart::File { extras, .. }
        | KnownContentPart::Document { extras, .. }
        | KnownContentPart::ToolUse { extras, .. }
        | KnownContentPart::ToolResult { extras, .. }
        | KnownContentPart::Thinking { extras, .. }
        | KnownContentPart::RedactedThinking { extras, .. } => extras,
    }
}

#[test]
fn leftover_members_land_in_the_variants_extras() {
    // (row, input, expected extras)
    let rows = [
        (
            "caller",
            json!({"type": "tool_use", "id": "a", "name": "b", "input": {}, "caller": {"type": "direct"}}),
            json!({"caller": {"type": "direct"}}),
        ),
        (
            "non_ascii_value",
            json!({"type": "text", "text": "hi", "note": "caf\u{e9} \u{65e5}\u{672c}"}),
            json!({"note": "caf\u{e9} \u{65e5}\u{672c}"}),
        ),
        (
            "empty_map",
            json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}),
            json!({}),
        ),
        (
            "reserved_name_collision",
            json!({"type": "text", "text": "typed", "extras": {"text": "nested"}}),
            json!({"extras": {"text": "nested"}}),
        ),
        (
            "member_typed_only_on_another_shape",
            json!({"type": "tool_use", "id": "a", "name": "b", "input": {}, "text": "loose"}),
            json!({"text": "loose"}),
        ),
        (
            "cache_control_on_a_shape_without_it",
            json!({"type": "thinking", "thinking": "t", "cache_control": {"type": "ephemeral"}}),
            json!({"cache_control": {"type": "ephemeral"}}),
        ),
        (
            "every_shape_member_extra",
            json!({"type": "redacted_thinking", "data": "d", "estimated_tokens": 12, "z": null}),
            json!({"estimated_tokens": 12, "z": null}),
        ),
    ];
    for (row, input, expected) in rows {
        // Act
        let part: ContentPart = serde_json::from_value(input.clone()).unwrap();

        // Assert
        assert_eq!(
            Value::Object(extras_of(&part).clone()),
            expected,
            "row `{row}`"
        );
        let wire = serde_json::to_value(&part).unwrap();
        for key in expected.as_object().unwrap().keys() {
            let typed_here = matches!(
                (&part, key.as_str()),
                (ContentPart::Known(KnownContentPart::Text { .. }), "text")
            );
            assert!(
                typed_here || wire.get(key).is_none(),
                "row `{row}` emitted `{key}`"
            );
        }
    }
}

#[test]
fn a_typed_member_stays_typed_when_an_extra_shares_its_name() {
    // Arrange
    let input = json!({"type": "text", "text": "typed", "extras": {"text": "nested"}});

    // Act
    let part: ContentPart = serde_json::from_value(input).unwrap();

    // Assert
    let ContentPart::Known(KnownContentPart::Text { text, extras, .. }) = &part else {
        panic!("expected text, got {part:?}");
    };
    assert_eq!(text, "typed");
    assert!(!extras.contains_key("text"));
    assert_eq!(
        serde_json::to_value(&part).unwrap(),
        json!({"type": "text", "text": "typed"})
    );
}

#[test]
fn an_unknown_block_keeps_every_member_verbatim_in_other() {
    // Arrange
    let input = json!({"type": "server_tool_use", "id": "s", "caller": {"type": "direct"}});

    // Act
    let part: ContentPart = serde_json::from_value(input.clone()).unwrap();

    // Assert
    assert!(matches!(part, ContentPart::Other { .. }), "{part:?}");
    assert_eq!(serde_json::to_value(&part).unwrap(), input);
}

#[test]
fn known_content_part_rejects_a_block_that_is_not_a_known_shape() {
    for (input, needle) in [
        (json!({"type": "server_tool_use"}), "unknown variant"),
        (json!({"type": "text"}), "malformed `text` content block"),
    ] {
        // Act
        let err = serde_json::from_value::<KnownContentPart>(input.clone()).unwrap_err();

        // Assert
        assert!(err.to_string().contains(needle), "{input}: {err}");
    }
    let ok: KnownContentPart =
        serde_json::from_value(json!({"type": "text", "text": "x", "caller": 1})).unwrap();
    assert!(matches!(ok, KnownContentPart::Text { ref extras, .. } if extras.len() == 1));
}

#[test]
fn derived_reference_round_trips_unchanged() {
    // The reference must itself parse a well-formed block, or the weld
    // above compares two failures.
    for block in full_blocks() {
        let parsed: reference::ContentPart = serde_json::from_value(block.clone()).unwrap();
        assert!(
            matches!(parsed, reference::ContentPart::Known(_)),
            "{block}"
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), block);
    }
}

#[test]
fn a_repeated_member_inside_cache_control_collapses_where_the_derive_rejected() {
    // Arrange: reachable only from a text body; the module docs name it.
    let text =
        r#"{"type":"text","text":"a","cache_control":{"type":"ephemeral","type":"ephemeral"}}"#;

    // Act
    let new = outcome_of(serde_json::from_str(text));
    let old = reference_outcome_of(serde_json::from_str(text));

    // Assert
    assert!(matches!(old, Outcome::Rejected(_)), "{old:?}");
    assert_eq!(
        new,
        Outcome::Known(
            json!({"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}})
        )
    );
}
