//! Tests for the `additionalProperties: false` repair.
//!
//! Two layers, deliberately:
//!
//! - ASSEMBLED-BODY tests drive the whole `request::normalize` path, because
//!   the defect being closed is only reachable there: a caller-supplied
//!   `output_config.format` rides through `merge_provider_extras` verbatim and
//!   wins over the converter, so a repair anywhere earlier is a no-op for it.
//!   The root and nested cases are the negative controls -- both fail without
//!   the assembly-side wiring.
//! - WALK tests drive `repair_schema` directly, where a traversal rule can be
//!   stated in one schema without an enclosing request.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, Error, Message, MessageContent, Role};

use super::{MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES, probe, repair_schema};
use crate::anthropic_api::request::normalize;
use crate::bounded_diagnostics::MAX_LOGGED_DIAGNOSTIC_ITEMS;

const PROVIDER: &str = "anthropic:test";

fn user_req() -> ChatRequest {
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
        max_tokens: Some(1024),
        ..Default::default()
    }
}

/// A request whose `output_config.format.schema` arrives the way real callers
/// send it: as a caller-supplied `output_config` on `provider_extras`, the
/// path that bypasses the canonical `response_format` converter entirely.
fn req_with_caller_output_schema(schema: Value) -> ChatRequest {
    let mut req = user_req();
    req.provider_extras = Some(json!({
        "output_config": {"format": {"type": "json_schema", "schema": schema}}
    }));
    req
}

fn assembled_schema(req: &ChatRequest) -> Value {
    let body = normalize(PROVIDER, req, false, &[], false, None, false, true).unwrap();
    body["output_config"]["format"]["schema"].clone()
}

fn repair(schema: Value) -> Value {
    let mut schema = schema;
    repair_schema(PROVIDER, &mut schema).unwrap();
    schema
}

// ---------------------------------------------------------------------------
// Assembled body: the negative controls
// ---------------------------------------------------------------------------

/// NEGATIVE CONTROL. Anthropic rejects a root object schema omitting the key
/// with 400 `For 'object' type, 'additionalProperties' must be explicitly set
/// to false`. Without the assembly-side repair the caller's schema ships
/// verbatim and this assertion fails.
#[test]
fn injects_additional_properties_false_on_root_object() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "properties": {"city": {"type": "string"}}
    }));

    // Act
    let schema = assembled_schema(&req);

    // Assert
    assert_eq!(schema["additionalProperties"], json!(false));
}

/// NEGATIVE CONTROL. The nested case is a separate live 400: a root that
/// already carries the key does not exempt its children.
#[test]
fn injects_additional_properties_false_on_nested_object() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "address": {
                "type": "object",
                "properties": {"street": {"type": "string"}}
            }
        }
    }));

    // Act
    let schema = assembled_schema(&req);

    // Assert
    assert_eq!(
        schema["properties"]["address"]["additionalProperties"],
        json!(false),
        "a nested object omitting the key 400s even when the root carries it"
    );
}

/// The canonical `response_format` converter path reaches the same repair, so
/// an OpenAI-shape structured-output request is fixed too.
#[test]
fn injects_additional_properties_false_on_a_response_format_schema() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({
        "type": "json_schema",
        "json_schema": {
            "name": "weather",
            "schema": {"type": "object", "properties": {"city": {"type": "string"}}}
        }
    }));

    // Act
    let schema = assembled_schema(&req);

    // Assert
    assert_eq!(schema["additionalProperties"], json!(false));
}

/// A body carrying no structured-output directive at all must not grow one.
#[test]
fn leaves_a_body_without_an_output_schema_untouched() {
    // Arrange
    let req = user_req();

    // Act
    let body = normalize(PROVIDER, &req, false, &[], false, None, false, true).unwrap();

    // Assert
    assert!(body.get("output_config").is_none());
}

/// A present non-`false` value reaches the wire unchanged. Overwriting or
/// dropping it would silently discard semantics the caller asked for; the
/// upstream rejects it naming the field, which is the truthful outcome.
#[test]
fn forwards_a_present_additional_properties_true_verbatim() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {"city": {"type": "string"}}
    }));

    // Act
    let schema = assembled_schema(&req);

    // Assert
    assert_eq!(
        schema["additionalProperties"],
        json!(true),
        "a present value is never overwritten and never dropped"
    );
}

#[test]
#[tracing_test::traced_test]
fn warns_once_naming_the_path_when_a_present_value_is_not_false() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "bag": {"type": "object", "additionalProperties": true}
        }
    }));

    // Act
    let _ = assembled_schema(&req);

    // Assert
    assert!(logs_contain(
        "output_schema_additional_properties_not_false"
    ));
    assert!(logs_contain("schema.properties.bag"));
}

/// The WARN must name paths only. A schema's property names and values are
/// caller data, so the record carries the location and nothing else.
#[test]
#[tracing_test::traced_test]
fn never_logs_schema_values() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "additionalProperties": {"type": "string", "description": "caller-secret-value"},
        "properties": {"city": {"type": "string"}}
    }));

    // Act
    let _ = assembled_schema(&req);

    // Assert
    assert!(logs_contain(
        "output_schema_additional_properties_not_false"
    ));
    assert!(!logs_contain("caller-secret-value"));
}

/// A routine repair is not an operator decision, so injecting emits nothing.
#[test]
#[tracing_test::traced_test]
fn injecting_alone_emits_no_warning() {
    // Arrange
    let req = req_with_caller_output_schema(json!({
        "type": "object",
        "properties": {"city": {"type": "string"}}
    }));

    // Act
    let _ = assembled_schema(&req);

    // Assert
    assert!(!logs_contain(
        "output_schema_additional_properties_not_false"
    ));
}

// ---------------------------------------------------------------------------
// Walk: object-shape detection
// ---------------------------------------------------------------------------

#[test]
fn preserves_a_present_additional_properties_false() {
    // Arrange
    let schema = json!({"type": "object", "additionalProperties": false, "properties": {}});

    // Act
    let out = repair(schema.clone());

    // Assert
    assert_eq!(out, schema);
}

/// Object shape is detected by `properties` even with no `type` keyword,
/// because Anthropic's requirement follows the instance type, not the
/// annotation.
#[test]
fn injects_into_a_typeless_schema_carrying_properties() {
    // Arrange
    let schema = json!({"properties": {"a": {"type": "string"}}});

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["additionalProperties"], json!(false));
}

/// The dangerous direction is a false positive. A string schema is not an
/// object, and injecting the key there would corrupt a valid schema.
#[test]
fn leaves_a_non_object_schema_alone() {
    // Arrange
    let schema = json!({"type": "string", "minLength": 3});

    // Act
    let out = repair(schema.clone());

    // Assert
    assert_eq!(out, schema);
}

/// A multi-member `type` union is not read as object-shaped on `type` alone:
/// its upstream treatment is unmeasured, and a miss degrades to the same 400
/// the caller sees today while a false positive corrupts the schema.
#[test]
fn leaves_a_multi_member_type_union_without_properties_alone() {
    // Arrange
    let schema = json!({"type": ["object", "null"]});

    // Act
    let out = repair(schema.clone());

    // Assert
    assert_eq!(out, schema);
}

// ---------------------------------------------------------------------------
// Walk: descent set
// ---------------------------------------------------------------------------

#[test]
fn injects_into_defs_and_definitions_entries() {
    // Arrange
    let schema = json!({
        "$defs": {"Inner": {"type": "object", "properties": {}}},
        "definitions": {"Legacy": {"type": "object", "properties": {}}},
        "$ref": "#/$defs/Inner"
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["$defs"]["Inner"]["additionalProperties"], json!(false));
    assert_eq!(
        out["definitions"]["Legacy"]["additionalProperties"],
        json!(false)
    );
}

#[test]
fn injects_into_every_composition_branch() {
    // Arrange
    let schema = json!({
        "anyOf": [{"type": "object", "properties": {}}, {"type": "string"}],
        "oneOf": [{"type": "object", "properties": {}}],
        "allOf": [{"type": "object", "properties": {}}],
        "not": {"type": "object", "properties": {}},
        "if": {"type": "object", "properties": {}},
        "then": {"type": "object", "properties": {}},
        "else": {"type": "object", "properties": {}}
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["anyOf"][0]["additionalProperties"], json!(false));
    assert!(out["anyOf"][1].get("additionalProperties").is_none());
    for key in ["oneOf", "allOf"] {
        assert_eq!(out[key][0]["additionalProperties"], json!(false));
    }
    for key in ["not", "if", "then", "else"] {
        assert_eq!(out[key]["additionalProperties"], json!(false));
    }
}

#[test]
fn injects_into_items_as_a_single_schema_and_as_an_array() {
    // Arrange
    let single = json!({"type": "array", "items": {"type": "object", "properties": {}}});
    let tuple = json!({
        "type": "array",
        "items": [{"type": "object", "properties": {}}, {"type": "string"}]
    });

    // Act
    let single = repair(single);
    let tuple = repair(tuple);

    // Assert
    assert_eq!(single["items"]["additionalProperties"], json!(false));
    assert_eq!(tuple["items"][0]["additionalProperties"], json!(false));
    assert!(tuple["items"][1].get("additionalProperties").is_none());
}

#[test]
fn injects_into_pattern_properties_entries() {
    // Arrange
    let schema = json!({
        "type": "object",
        "patternProperties": {"^x-": {"type": "object", "properties": {}}}
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(
        out["patternProperties"]["^x-"]["additionalProperties"],
        json!(false)
    );
}

/// An object under an unenumerated keyword degrades to the SAME upstream 400
/// the caller gets today -- never a fabricated success, and never a rewrite of
/// something whose meaning the walk does not know.
#[test]
fn leaves_an_object_under_an_unenumerated_keyword_opaque() {
    // Arrange
    let schema = json!({
        "type": "object",
        "properties": {},
        "unevaluatedProperties": {"type": "object", "properties": {"a": {"type": "string"}}}
    });

    // Act
    let out = repair(schema);

    // Assert
    assert!(
        out["unevaluatedProperties"]
            .get("additionalProperties")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Walk: instance-valued keywords are caller DATA
// ---------------------------------------------------------------------------

/// An object sitting under `const` or `enum` is a VALUE the caller wants
/// matched, not a schema. Inserting a keyword into it would change what the
/// caller asked for, so these subtrees must come out byte-identical.
#[test]
fn leaves_object_values_under_const_and_enum_byte_unchanged() {
    // Arrange
    let const_value = json!({"type": "object", "properties": {"nested": true}});
    let enum_values = json!([{"type": "object", "properties": {}}, {"kind": "object"}]);
    let schema = json!({
        "type": "object",
        "properties": {
            "pinned": {"const": const_value},
            "choice": {"enum": enum_values}
        }
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["properties"]["pinned"]["const"], const_value);
    assert_eq!(out["properties"]["choice"]["enum"], enum_values);
}

#[test]
fn leaves_object_values_under_default_and_examples_byte_unchanged() {
    // Arrange
    let default_value = json!({"type": "object", "properties": {"a": 1}});
    let examples = json!([{"type": "object"}]);
    let schema = json!({
        "type": "object",
        "properties": {},
        "default": default_value,
        "examples": examples
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["default"], default_value);
    assert_eq!(out["examples"], examples);
}

/// A schema-valued `additionalProperties` is a present value: forwarded
/// verbatim, and never entered (its own inner objects are not repaired,
/// because the key is not in the descent set).
#[test]
fn forwards_a_schema_valued_additional_properties_verbatim() {
    // Arrange
    let inner = json!({"type": "object", "properties": {"a": {"type": "string"}}});
    let schema = json!({"type": "object", "properties": {}, "additionalProperties": inner});

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["additionalProperties"], inner);
}

// ---------------------------------------------------------------------------
// Walk: `$ref` is never dereferenced
// ---------------------------------------------------------------------------

/// A `$ref` is a string, not a container. Declining to follow it makes a
/// self-referential schema terminate by construction -- there is no cycle to
/// guard because there is no dereference.
#[test]
fn a_self_referential_ref_terminates() {
    // Arrange
    let schema = json!({
        "$ref": "#/$defs/Node",
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {"next": {"$ref": "#/$defs/Node"}}
            }
        }
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["$defs"]["Node"]["additionalProperties"], json!(false));
    assert_eq!(
        out["$defs"]["Node"]["properties"]["next"],
        json!({"$ref": "#/$defs/Node"}),
        "a ref site is left as the string it is"
    );
}

#[test]
fn a_root_self_reference_terminates() {
    // Arrange
    let schema = json!({
        "type": "object",
        "properties": {"child": {"$ref": "#"}}
    });

    // Act
    let out = repair(schema);

    // Assert
    assert_eq!(out["additionalProperties"], json!(false));
    assert_eq!(out["properties"]["child"], json!({"$ref": "#"}));
}

// ---------------------------------------------------------------------------
// Walk: bounds produce a clean error, never a panic
// ---------------------------------------------------------------------------

#[test]
fn a_schema_deeper_than_the_depth_limit_errors_without_panic() {
    // Arrange
    let mut schema = json!({"type": "object", "properties": {}});
    for _ in 0..=MAX_SCHEMA_DEPTH {
        schema = json!({"type": "object", "properties": {"a": schema}});
    }

    // Act
    let Err(err) = repair_schema(PROVIDER, &mut schema) else {
        panic!("a schema deeper than the limit must error");
    };

    // Assert
    assert!(matches!(err, Error::NormalizeRequest(..)));
    assert!(err.to_string().contains("nests deeper"));
}

#[test]
fn a_schema_wider_than_the_node_limit_errors_without_panic() {
    // Arrange
    let properties: serde_json::Map<String, Value> = (0..=MAX_SCHEMA_NODES)
        .map(|i| (format!("p{i}"), json!({"type": "string"})))
        .collect();
    let mut schema = json!({"type": "object", "properties": properties});

    // Act
    let Err(err) = repair_schema(PROVIDER, &mut schema) else {
        panic!("a schema wider than the node limit must error");
    };

    // Assert
    assert!(matches!(err, Error::NormalizeRequest(..)));
    assert!(err.to_string().contains("schema nodes"));
}

/// A schema right at the limits is served, not rejected: the bound must not
/// fire one node early.
#[test]
fn a_schema_at_the_depth_limit_is_repaired() {
    // Arrange
    let mut schema = json!({"type": "object", "properties": {}});
    for _ in 0..MAX_SCHEMA_DEPTH {
        schema = json!({"type": "object", "properties": {"a": schema}});
    }

    // Act
    let result = repair_schema(PROVIDER, &mut schema);

    // Assert
    assert!(result.is_ok());
    assert_eq!(schema["additionalProperties"], json!(false));
}

/// The node bound counts as nodes are PUSHED rather than popped, so the
/// exact-boundary case is pinned from both sides: the root plus
/// `MAX_SCHEMA_NODES - 1` children is exactly the limit and must be served.
#[test]
fn a_schema_at_exactly_the_node_limit_is_repaired() {
    // Arrange
    let properties: serde_json::Map<String, Value> = (0..MAX_SCHEMA_NODES - 1)
        .map(|i| (format!("p{i}"), json!({"type": "string"})))
        .collect();
    let mut schema = json!({"type": "object", "properties": properties});

    // Act
    let result = repair_schema(PROVIDER, &mut schema);

    // Assert
    assert!(
        result.is_ok(),
        "the root plus MAX_SCHEMA_NODES - 1 children is exactly the limit"
    );
    assert_eq!(schema["additionalProperties"], json!(false));
}

// ---------------------------------------------------------------------------
// Walk: the cost of the walk is bounded, not just its outcome
// ---------------------------------------------------------------------------

/// A path costs O(depth x segment length) to render and at most
/// `MAX_LOGGED_DIAGNOSTIC_ITEMS` of them can reach the WARN, so a schema
/// carrying a non-`false` value on every node must not render one per node.
/// Rendering unconditionally turns a single conventional-sized request into
/// minutes of CPU and hundreds of GiB of transient allocation, and `/v1/*`
/// carries no concurrency limit.
#[test]
fn a_schema_with_many_forwarded_values_renders_only_the_retained_paths() {
    // Arrange: far more non-`false` values than the sample can keep, each at
    // a distinct path so none of them dedupes away.
    let properties: serde_json::Map<String, Value> = (0..500)
        .map(|i| {
            (
                format!("p{i}"),
                json!({"type": "object", "additionalProperties": true}),
            )
        })
        .collect();
    let mut schema = json!({"type": "object", "properties": properties});
    probe::reset();

    // Act
    let repair = repair_schema(PROVIDER, &mut schema).unwrap();

    // Assert
    assert_eq!(
        probe::renders(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "rendering must stop once the sample can keep nothing more"
    );
    assert_eq!(repair.forwarded.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
    assert!(
        repair.forwarded.truncated(),
        "an operator must still see the list was cut"
    );
}

/// The node bound must contain the walk's MEMORY, which means firing before
/// the pending set exists. One node can hold millions of `anyOf` branches, so
/// a pop-time check admits the whole fan-out into the work stack and the path
/// arena first -- exactly the sink the bound exists to prevent.
#[test]
fn a_wide_fanout_is_rejected_without_interning_the_whole_pending_set() {
    // Arrange: a single root whose one keyword holds far more branches than
    // the node limit permits.
    let branches: Vec<Value> = (0..MAX_SCHEMA_NODES * 2).map(|_| json!({})).collect();
    let mut schema = json!({"anyOf": branches});
    probe::reset();

    // Act
    let Err(err) = repair_schema(PROVIDER, &mut schema) else {
        panic!("a fan-out wider than the node limit must error");
    };

    // Assert
    assert!(matches!(err, Error::NormalizeRequest(..)));
    assert!(err.to_string().contains("schema nodes"));
    assert!(
        probe::segments() <= MAX_SCHEMA_NODES + MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "the arena must stop growing at the node limit, not at the body size \
         limit; interned {} segments",
        probe::segments()
    );
}

/// Path segments are caller-chosen property names, `patternProperties`
/// regexes and `$defs` keys. Log injection is already blocked by the field's
/// Debug rendering; the live risk is SIZE, so each rendered path is capped
/// the same way the rest of the egress caps caller strings.
#[test]
fn a_forwarded_path_is_length_capped_before_it_reaches_the_sample() {
    // Arrange
    let name = "n".repeat(64 * 1024);
    let mut schema = json!({
        "type": "object",
        "properties": {name: {"type": "object", "additionalProperties": true}}
    });

    // Act
    let repair = repair_schema(PROVIDER, &mut schema).unwrap();

    // Assert
    let path = &repair.forwarded.items()[0];
    assert!(
        path.chars().count() <= 256,
        "a caller-sized path must not reach the log field; got {} chars",
        path.chars().count()
    );
}

/// The two cost bounds are load-bearing TOGETHER. Long names sharing a
/// capped prefix all render to the SAME capped string, so the deduplicating
/// sample stores one item and never fills -- which leaves the render live for
/// every node. Capping inside the render is what keeps that case cheap:
/// a full render of each path would be quadratic in the body.
#[test]
fn many_forwarded_values_under_equal_prefix_names_stay_cheap_to_render() {
    // Arrange: a prefix of long property names longer than the render cap,
    // then a fan of forwarding leaves beneath it, so every leaf's path
    // renders to the SAME capped string.
    let leaves: serde_json::Map<String, Value> = (0..200)
        .map(|i| {
            (
                format!("z{i}"),
                json!({"type": "object", "additionalProperties": true}),
            )
        })
        .collect();
    let long = "a".repeat(4096);
    let mut schema = json!({"type": "object", "properties": leaves});
    for _ in 0..8 {
        schema = json!({"type": "object", "properties": {long.clone(): schema}});
    }
    probe::reset();

    // Act
    let repair = repair_schema(PROVIDER, &mut schema).unwrap();

    // Assert: the sample deduplicated to one entry, so it never filled and
    // the render ran per forwarding node -- each render must still be capped,
    // which keeps the total materialized well under the O(nodes x prefix)
    // cost an uncapped render would pay.
    assert_eq!(repair.forwarded.len(), 1, "equal capped prefixes dedupe");
    assert!(probe::renders() > 1, "the dedupe leaves the render live");
    assert!(
        probe::rendered_chars() <= probe::renders() * 256,
        "each render must stop at the cap; materialized {} chars over {} \
         renders",
        probe::rendered_chars(),
        probe::renders()
    );
    for path in repair.forwarded.items() {
        assert!(path.chars().count() <= 256);
    }
}
