//! JSON Schema -> Gemini OpenAPI-subset cleaning for the shared `Schema` proto.
//!
//! Gemini's `functionDeclarations[].parameters` and
//! `generationConfig.responseSchema` are the SAME proto and accept an
//! OpenAPI 3.0 Schema subset, not raw JSON Schema. A caller tool or
//! structured-output schema authored for OpenAI/Anthropic can carry
//! constructs Gemini rejects with a 400 or silently mis-parses. This module
//! normalizes a caller schema into the subset before emit:
//!
//!   - `oneOf` -> `anyOf` (Gemini has no `oneOf`).
//!   - Resolve an intra-document `$ref` (`#/$defs/X`, `#/definitions/X`) by
//!     inlining its cleaned target so nested-model shape survives; peer
//!     proxies do the same. Cyclic/self-referential refs terminate with a
//!     bounded empty schema, and an unresolvable ref degrades to a drop.
//!   - Strip keywords Gemini's Schema proto rejects: `$schema`,
//!     `additionalProperties`, `$defs`/`definitions`, `allOf`, `not`,
//!     `const`, `patternProperties`. The `$defs`/`definitions` containers
//!     never reach the wire; only their inlined ref targets do.
//!   - Nullable: `type: [T, "null"]` -> `type: T` + `nullable: true`;
//!     an explicit `nullable` is preserved.
//!   - A multi-concrete-member `type` union (`["string","integer"]`) is
//!     lowered to an `anyOf` of single-`type` branches (Gemini has no
//!     multi-type `type`); a `"null"` member still lifts to `nullable`.
//!   - `format` is allowlisted to Gemini-supported values, otherwise dropped.
//!   - Numeric/boolean `enum` entries coerced to strings (Gemini's enum
//!     is a repeated string).
//!   - `type` uppercased to Gemini's TYPE enum (STRING, INTEGER, ...).
//!
//! Recurses only through genuinely schema-valued keywords (`properties`,
//! `items`, `prefixItems`, `anyOf`/`oneOf` branches); literal-valued
//! keywords (`default`, `example`, `title`, ...) are cloned verbatim so a
//! `type` field inside a VALUE is never misread as a schema type keyword.
//! Pure function of its input -- no logging, no mutation.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// Normalize a caller JSON Schema into the Gemini OpenAPI subset.
///
/// The top-level schema is treated as the ref document root: its
/// `$defs`/`definitions` back any intra-document `$ref` encountered at any
/// depth. Those defs are collected once here and threaded through the
/// recursion so nested refs resolve against the document root.
pub(super) fn clean_schema(schema: &Value) -> Value {
    let defs = collect_defs(schema);
    let mut visited = HashSet::new();
    clean_value(schema, &defs, &mut visited)
}

/// Collect the root `$defs`/`definitions` object schemas into a lookup keyed
/// by their JSON-pointer ref (`#/$defs/Name`, `#/definitions/Name`).
fn collect_defs(schema: &Value) -> HashMap<String, Value> {
    let mut defs = HashMap::new();
    if let Value::Object(map) = schema {
        for container in ["$defs", "definitions"] {
            if let Some(Value::Object(entries)) = map.get(container) {
                for (name, target) in entries {
                    defs.insert(format!("#/{container}/{name}"), target.clone());
                }
            }
        }
    }
    defs
}

fn clean_value(
    schema: &Value,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
) -> Value {
    match schema {
        Value::Object(map) => clean_object(map, defs, visited),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| clean_value(i, defs, visited))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Resolve an intra-document `$ref` to its cleaned target, with path-based
/// cycle protection. A revisited pointer (cycle) or a missing target yields
/// `None`, degrading the ref site to a bounded terminal at the call site.
fn resolve_ref(
    pointer: &str,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
) -> Option<Value> {
    let decoded = decode_ref_pointer(pointer);
    if visited.contains(&decoded) {
        return None;
    }
    let target = defs.get(&decoded)?.clone();
    visited.insert(decoded.clone());
    let cleaned = clean_value(&target, defs, visited);
    visited.remove(&decoded);
    Some(cleaned)
}

/// Decode JSON Pointer escapes (RFC 6901) in a `$ref` so an escaped def name
/// matches the raw key `collect_defs` stored: `~1` -> `/` and `~0` -> `~`,
/// applied in that order so a literal `~1` written as `~01` is not mangled.
/// Container segments (`$defs`/`definitions`) carry no escapable characters,
/// so decoding the whole pointer reconstructs the raw-joined key.
fn decode_ref_pointer(pointer: &str) -> String {
    pointer.replace("~1", "/").replace("~0", "~")
}

fn clean_object(
    map: &Map<String, Value>,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
) -> Value {
    let mut out = Map::new();
    // A resolvable `$ref` inlines its cleaned target as the base schema;
    // sibling keywords (rare, per JSON Schema 2020-12) override it.
    let mut inlined: Option<Map<String, Value>> = None;
    for (key, value) in map {
        match key.as_str() {
            // Resolve intra-document refs by inlining; drop the `$ref`
            // itself. An unresolvable or cyclic ref leaves `inlined` unset,
            // degrading to the prior drop/empty behavior with no panic.
            "$ref" => {
                if let Value::String(pointer) = value
                    && let Some(Value::Object(target)) = resolve_ref(pointer, defs, visited)
                {
                    inlined = Some(target);
                }
            }
            // Keywords Gemini's Schema proto rejects: drop them rather than
            // pass through and 400. `$defs`/`definitions` only back `$ref`
            // and are inlined at ref sites, so the containers are dead weight
            // on the wire. `allOf`/`not`/`const`/`patternProperties` have no
            // Gemini equivalent; dropping loses the constraint (const loses
            // the pin) but avoids a hard 400 on common pydantic/zod schemas.
            "$schema"
            | "additionalProperties"
            | "$defs"
            | "definitions"
            | "allOf"
            | "not"
            | "const"
            | "patternProperties" => {}
            "oneOf" | "anyOf" => {
                out.insert("anyOf".to_string(), clean_value(value, defs, visited));
            }
            "type" => insert_type(&mut out, value),
            "enum" => {
                out.insert("enum".to_string(), coerce_enum(value));
            }
            // Gemini accepts `format` only for a small closed set; any other
            // value (uri/email/uuid/date/...) is dropped, not passed through.
            "format" => {
                if value.as_str().is_some_and(is_supported_format) {
                    out.insert("format".to_string(), value.clone());
                }
            }
            // Name-keyed schema map: child keys are user-chosen property
            // names, not keywords, so recurse per-value without keyword
            // interpretation.
            "properties" => {
                out.insert(key.clone(), clean_schema_map(value, defs, visited));
            }
            // Genuinely schema-valued keywords: recurse as schemas.
            "items" | "prefixItems" => {
                out.insert(key.clone(), clean_value(value, defs, visited));
            }
            // Literal-valued keywords (`default`, `example`, `examples`,
            // `title`, `description`) and any unrecognized keyword carry data
            // VALUES, not nested schemas -- recursing would misread a value's
            // `type` field as a schema type keyword and corrupt it. Clone
            // verbatim.
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    match inlined {
        Some(mut merged) => {
            for (key, value) in out {
                merged.insert(key, value);
            }
            Value::Object(merged)
        }
        None => Value::Object(out),
    }
}

/// Recurse into a map whose keys are user-chosen names (property names)
/// rather than JSON-Schema keywords: clean each value as a schema, leave the
/// keys untouched.
fn clean_schema_map(
    value: &Value,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(name, schema)| (name.clone(), clean_value(schema, defs, visited)))
                .collect(),
        ),
        other => clean_value(other, defs, visited),
    }
}

/// Emit Gemini's `type` (uppercased) and lift a `"null"` union member to a
/// `nullable: true` flag. A union with multiple concrete members is lowered
/// to an `anyOf` of single-`type` branches, since Gemini's `type` is scalar.
/// A non-array, non-string `type` passes through.
fn insert_type(out: &mut Map<String, Value>, value: &Value) {
    match value {
        Value::String(t) => {
            out.insert("type".to_string(), Value::String(t.to_uppercase()));
        }
        Value::Array(members) => {
            let mut has_null = false;
            let mut concrete: Vec<String> = Vec::new();
            for member in members {
                match member.as_str() {
                    Some("null") => has_null = true,
                    Some(t) => concrete.push(t.to_uppercase()),
                    None => {}
                }
            }
            match concrete.len() {
                0 => {}
                1 => {
                    out.insert("type".to_string(), Value::String(concrete.remove(0)));
                }
                _ => {
                    let branches = concrete
                        .into_iter()
                        .map(|t| {
                            let mut branch = Map::new();
                            branch.insert("type".to_string(), Value::String(t));
                            Value::Object(branch)
                        })
                        .collect();
                    out.insert("anyOf".to_string(), Value::Array(branches));
                }
            }
            if has_null {
                out.insert("nullable".to_string(), Value::Bool(true));
            }
        }
        other => {
            out.insert("type".to_string(), other.clone());
        }
    }
}

/// Gemini's Schema proto accepts `format` only for a small closed set per
/// type (STRING: `enum`, `date-time`; NUMBER: `float`, `double`; INTEGER:
/// `int32`, `int64`). Every other JSON-Schema format (uri/email/uuid/date/
/// password/...) is dropped rather than passed through, which would 400.
fn is_supported_format(f: &str) -> bool {
    matches!(
        f,
        "enum" | "date-time" | "float" | "double" | "int32" | "int64"
    )
}

/// Coerce numeric and boolean enum entries to their string form; Gemini's
/// enum is a repeated string and rejects non-string members.
fn coerce_enum(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| match item {
                    Value::Number(n) => Value::String(n.to_string()),
                    Value::Bool(b) => Value::String(b.to_string()),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_of_is_renamed_to_any_of() {
        // Arrange
        let schema = json!({
            "oneOf": [{"type": "string"}, {"type": "integer"}]
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("oneOf").is_none(), "oneOf must be removed");
        let any_of = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(any_of[0]["type"], "STRING");
        assert_eq!(any_of[1]["type"], "INTEGER");
    }

    #[test]
    fn schema_and_ref_and_additional_properties_are_stripped() {
        // A `$ref` with no matching `$defs`/`definitions` target is
        // unresolvable, so it degrades to a drop (no inline, no panic).
        // Arrange
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/definitions/Foo",
            "additionalProperties": false,
            "type": "object"
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("$ref").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn nullable_type_array_lifts_to_nullable_flag() {
        // Arrange
        let schema = json!({"type": ["string", "null"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "STRING");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn explicit_nullable_flag_is_preserved() {
        // Arrange
        let schema = json!({"type": "string", "nullable": true});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "STRING");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn numeric_and_boolean_enum_entries_coerced_to_strings() {
        // Arrange
        let schema = json!({"type": "integer", "enum": [1, 2, 3]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["enum"], json!(["1", "2", "3"]));
    }

    #[test]
    fn string_enum_entries_pass_through() {
        // Arrange
        let schema = json!({"type": "string", "enum": ["a", "b"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["enum"], json!(["a", "b"]));
    }

    #[test]
    fn type_is_uppercased() {
        // Arrange
        let schema = json!({"type": "boolean"});

        // Act + Assert
        assert_eq!(clean_schema(&schema)["type"], "BOOLEAN");
    }

    #[test]
    fn nested_object_properties_are_cleaned_recursively() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "leaf": {"type": ["number", "null"]}
                    }
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let inner = &cleaned["properties"]["inner"];
        assert!(inner.get("additionalProperties").is_none());
        assert_eq!(inner["type"], "OBJECT");
        let leaf = &inner["properties"]["leaf"];
        assert_eq!(leaf["type"], "NUMBER");
        assert_eq!(leaf["nullable"], true);
    }

    #[test]
    fn array_items_schema_is_cleaned_recursively() {
        // Arrange
        let schema = json!({
            "type": "array",
            "items": {"type": "string", "$schema": "x"}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "ARRAY");
        assert_eq!(cleaned["items"]["type"], "STRING");
        assert!(cleaned["items"].get("$schema").is_none());
    }

    #[test]
    fn property_named_like_a_keyword_is_not_treated_as_a_keyword() {
        // A property literally named "type" must recurse as a schema, not
        // be uppercased as a type token.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "type": {"type": "string"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["type"]["type"], "STRING");
    }

    #[test]
    fn defs_and_definitions_are_stripped() {
        // Arrange: with no `$ref` pointing at them, the `$defs`/`definitions`
        // containers are dead weight and never reach the wire; their child
        // keys are definition names, not keywords.
        let schema = json!({
            "type": "object",
            "$defs": {"Foo": {"type": "string"}},
            "definitions": {"Bar": {"type": "integer"}}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$defs").is_none());
        assert!(cleaned.get("definitions").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn ref_to_defs_inlines_cleaned_target() {
        // A property whose schema is `{"$ref": "#/$defs/X"}` emits X's
        // inlined, cleaned object shape -- not an empty `{}`.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Address": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "city": {"type": "string"},
                        "zip": {"type": ["string", "null"]}
                    }
                }
            },
            "properties": {
                "home": {"$ref": "#/$defs/Address"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let home = &cleaned["properties"]["home"];
        assert!(home.get("$ref").is_none(), "$ref must be resolved away");
        assert_eq!(home["type"], "OBJECT");
        assert!(
            home.get("additionalProperties").is_none(),
            "inlined target must be cleaned too"
        );
        assert_eq!(home["properties"]["city"]["type"], "STRING");
        assert_eq!(home["properties"]["zip"]["type"], "STRING");
        assert_eq!(home["properties"]["zip"]["nullable"], true);
        // The container itself never reaches the wire.
        assert!(cleaned.get("$defs").is_none());
    }

    #[test]
    fn ref_to_definitions_inlines_cleaned_target() {
        // Arrange
        let schema = json!({
            "type": "object",
            "definitions": {
                "Tag": {"type": "string", "enum": [1, 2]}
            },
            "properties": {
                "tag": {"$ref": "#/definitions/Tag"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let tag = &cleaned["properties"]["tag"];
        assert!(tag.get("$ref").is_none());
        assert_eq!(tag["type"], "STRING");
        assert_eq!(tag["enum"], json!(["1", "2"]));
        assert!(cleaned.get("definitions").is_none());
    }

    #[test]
    fn self_referential_ref_terminates_safely() {
        // A def that references itself must not recurse forever; the cyclic
        // ref site degrades to a bounded empty schema.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"},
                        "next": {"$ref": "#/$defs/Node"}
                    }
                }
            },
            "properties": {
                "root": {"$ref": "#/$defs/Node"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let root = &cleaned["properties"]["root"];
        assert_eq!(root["type"], "OBJECT");
        assert_eq!(root["properties"]["value"]["type"], "STRING");
        // The self-reference terminates to a bounded empty schema.
        assert_eq!(root["properties"]["next"], json!({}));
    }

    #[test]
    fn mutually_recursive_refs_terminate_safely() {
        // A -> B -> A cycle must terminate rather than blow the stack.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": {"b": {"$ref": "#/$defs/B"}}
                },
                "B": {
                    "type": "object",
                    "properties": {"a": {"$ref": "#/$defs/A"}}
                }
            },
            "properties": {
                "start": {"$ref": "#/$defs/A"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let start = &cleaned["properties"]["start"];
        assert_eq!(start["type"], "OBJECT");
        let b = &start["properties"]["b"];
        assert_eq!(b["type"], "OBJECT");
        // The back-edge A (already on the resolution path) terminates empty.
        assert_eq!(b["properties"]["a"], json!({}));
    }

    #[test]
    fn diamond_ref_inlines_at_each_site() {
        // The same def referenced from two independent sites (not a cycle)
        // inlines at both -- path-based cycle detection must not confuse a
        // diamond for a loop.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Leaf": {"type": "string"}
            },
            "properties": {
                "left": {"$ref": "#/$defs/Leaf"},
                "right": {"$ref": "#/$defs/Leaf"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["left"]["type"], "STRING");
        assert_eq!(cleaned["properties"]["right"]["type"], "STRING");
    }

    #[test]
    fn unresolvable_ref_degrades_to_empty() {
        // A `$ref` with no matching target drops without panicking.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "missing": {"$ref": "#/$defs/DoesNotExist"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["missing"], json!({}));
    }

    #[test]
    fn ref_with_escaped_pointer_tokens_resolves() {
        // A def name containing `/` (escaped as `~1`) or `~` (escaped as `~0`)
        // must resolve per RFC 6901, not fail lookup and degrade to `{}`.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "A/B": {"type": "string"},
                "C~D": {"type": "integer"}
            },
            "properties": {
                "slash": {"$ref": "#/$defs/A~1B"},
                "tilde": {"$ref": "#/$defs/C~0D"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert: both escaped refs inline their cleaned targets.
        assert_eq!(cleaned["properties"]["slash"]["type"], "STRING");
        assert_eq!(cleaned["properties"]["tilde"]["type"], "INTEGER");
    }

    #[test]
    fn pattern_properties_is_stripped() {
        // Gemini's Schema proto has no `patternProperties`; it is dropped
        // rather than passed through (a 400 otherwise).
        // Arrange
        let schema = json!({
            "type": "object",
            "patternProperties": {
                "^x": {"type": "string"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("patternProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn object_valued_default_passes_through_byte_identical() {
        // A `default` VALUE that happens to contain a `type` field must NOT
        // be treated as a nested schema: its `type` stays lowercase.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "size": {
                    "type": "string",
                    "default": {"type": "small", "count": 3}
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(
            cleaned["properties"]["size"]["default"],
            json!({"type": "small", "count": 3}),
            "a default value must pass through verbatim, uncorrupted"
        );
    }

    #[test]
    fn literal_valued_keywords_pass_through_verbatim() {
        // `example`/`examples`/`title`/`description` carry data, not schemas,
        // so a `type` field inside them is never uppercased.
        // Arrange
        let schema = json!({
            "type": "string",
            "example": {"type": "x"},
            "examples": [{"type": "y"}],
            "title": "Type",
            "description": "the type field"
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["example"], json!({"type": "x"}));
        assert_eq!(cleaned["examples"], json!([{"type": "y"}]));
        assert_eq!(cleaned["title"], "Type");
        assert_eq!(cleaned["description"], "the type field");
    }

    #[test]
    fn multi_concrete_type_union_lowers_to_any_of() {
        // Arrange
        let schema = json!({"type": ["string", "integer"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(
            cleaned.get("type").is_none(),
            "a multi-type union must not retain a scalar type"
        );
        let branches = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[1]["type"], "INTEGER");
        assert!(cleaned.get("nullable").is_none());
    }

    #[test]
    fn multi_concrete_type_union_with_null_sets_nullable() {
        // Arrange
        let schema = json!({"type": ["string", "integer", "null"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let branches = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[1]["type"], "INTEGER");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn unsupported_keywords_are_stripped() {
        // Arrange
        let schema = json!({
            "type": "object",
            "allOf": [{"type": "string"}],
            "not": {"type": "integer"},
            "const": {"type": "x"}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("allOf").is_none());
        assert!(cleaned.get("not").is_none());
        assert!(cleaned.get("const").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn supported_format_survives_and_unsupported_is_dropped() {
        // Arrange + Act + Assert
        let supported = json!({"type": "string", "format": "date-time"});
        assert_eq!(clean_schema(&supported)["format"], "date-time");

        let unsupported = json!({"type": "string", "format": "uri"});
        assert!(clean_schema(&unsupported).get("format").is_none());
    }

    #[test]
    fn combined_schema_applies_every_transform() {
        // Arrange: one schema exercising all constructs at once.
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "oneOf": [
                        {"type": "string", "enum": ["ok", "err"]},
                        {"type": ["integer", "null"], "enum": [0, 1]}
                    ]
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");

        let status = &cleaned["properties"]["status"];
        assert!(status.get("oneOf").is_none());
        let branches = status.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[0]["enum"], json!(["ok", "err"]));
        assert_eq!(branches[1]["type"], "INTEGER");
        assert_eq!(branches[1]["nullable"], true);
        assert_eq!(branches[1]["enum"], json!(["0", "1"]));

        let tags = &cleaned["properties"]["tags"];
        assert_eq!(tags["type"], "ARRAY");
        assert_eq!(tags["items"]["type"], "STRING");
    }
}
