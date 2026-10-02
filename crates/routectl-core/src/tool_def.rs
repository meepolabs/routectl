//! Typed tool definitions on the request.
//!
//! The hub stores a tool def in one of two shapes:
//!
//! - `ToolDef::Custom(CustomTool)` -- canonical Anthropic-shape custom
//!   tool with first-class `cache_control`, `defer_loading`, and
//!   `strict`. The OpenAI `{type: "function", function: {...}}` shape
//!   is NOT lifted into this variant at ingress: the OpenAI ingress
//!   passes function tools through as `ToolDef::Other` verbatim (see
//!   `crates/routectl-cli/src/ingress/openai.rs`). The reverse
//!   translation lives on the egress leg -- the openai-compat egress
//!   detects a `{type: "function", ...}` `Other` value via
//!   `CustomTool::from_openai_function` and lifts it there. The flat
//!   OpenAI Responses function shape (`{type: "function", name, ...}`)
//!   IS lifted at the Responses ingress, through
//!   `CustomTool::from_responses_function`. The Responses egress decides
//!   which replayed inline declarations already declare a canonical
//!   function through `CustomTool::responses_function_name`, the
//!   allocation-free predicate that normalization is built on.
//! - `ToolDef::Other(Value)` -- forward-compat catchall. Anthropic
//!   built-in tools (`bash_*`, `code_execution_*`, `web_search_*`),
//!   server-side tools, and future shapes pass through verbatim. The
//!   Anthropic and Bedrock-Invoke egresses re-emit this Value as-is;
//!   OpenAI-compat egress drops with a `tracing::warn!` (or rejects
//!   under `strict_translation`).
//!
//! Discrimination on the wire: the `type` field decides. Absent or
//! `"custom"` -> `Custom`. Anything else -> `Other`. This avoids
//! `name`-based heuristics that would falsely absorb builtin tools
//! (which also carry `name`) into the typed variant. A legacy
//! bare-function element (`{function: {name, ...}}`, no `type`, no
//! top-level `name`) is lifted into `Custom` rather than failing on the
//! missing `name`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::cache_control::CacheControl;

/// Tool definition variants. See module docs.
#[derive(Debug, Clone)]
pub enum ToolDef {
    /// Typed Anthropic-shape custom tool. The OpenAI `{type: "function",
    /// function: {...}}` shape is NOT lifted here at ingress -- it passes
    /// through as `Other` and is translated on the openai-compat egress
    /// leg via `CustomTool::from_openai_function`.
    Custom(CustomTool),
    /// Forward-compat catchall for any other tool kind (Anthropic
    /// built-in tools, server-side tools, future wire shapes).
    /// Preserved verbatim through Anthropic / Bedrock-Invoke egresses
    /// (except an OpenAI Responses hosted-MCP tool, which they withhold);
    /// OpenAI-compat egresses drop with a warn (or reject under
    /// `strict_translation`).
    Other(Value),
}

/// Anthropic-shape custom tool. `input_schema` defaults to an empty
/// object schema so a minimal `{name}` tool round-trips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTool {
    /// Tool name the model calls.
    pub name: String,
    /// Human-readable tool description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input arguments. `inputSchema` (the
    /// MCP / Agent SDK spelling) is accepted on the wire as an exact
    /// alias; carrying both spellings on one tool is a duplicate-field
    /// error. Always serialized as `input_schema`.
    #[serde(default = "empty_object_schema", alias = "inputSchema")]
    pub input_schema: Value,
    /// Optional cache breakpoint marker on this tool definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Anthropic deferred-loading hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    /// Strict schema-adherence flag (OpenAI structured tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Optional `type` discriminant. Anthropic accepts `"custom"` or
    /// absence; we round-trip whichever the wire used.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_tag: Option<String>,
}

fn empty_object_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl CustomTool {
    /// If `v` is the OpenAI tool wire shape (`{type: "function", function:
    /// {name, description?, parameters?, strict?}}`), translate it into a
    /// canonical `CustomTool`. Returns `None` for any other shape so the
    /// caller can fall through to `ToolDef::Other` for builtin / unknown
    /// tool types.
    ///
    /// Used by the openai-compat EGRESS
    /// (`crates/routectl-providers/src/openai_compat/`) to recognize a
    /// `ToolDef::Other` value that is already in OpenAI function shape
    /// before forwarding it. The OpenAI ingress does NOT call this: it
    /// leaves function tools as `ToolDef::Other` verbatim (see the module
    /// docs).
    pub fn from_openai_function(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let is_function = obj.get("type").and_then(|t| t.as_str()) == Some("function");
        if !is_function {
            return None;
        }
        let func = obj.get("function")?.as_object()?;
        let name = func.get("name")?.as_str()?.to_string();
        let description = func
            .get("description")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);
        let input_schema = func
            .get("parameters")
            .cloned()
            .unwrap_or_else(empty_object_schema);
        let strict = func.get("strict").and_then(serde_json::Value::as_bool);
        Some(Self {
            name,
            description,
            input_schema,
            cache_control: None,
            defer_loading: None,
            strict,
            type_tag: None,
        })
    }

    /// If `v` is a flat OpenAI Responses function declaration (`{type:
    /// "function", name, description?, parameters?, strict?}`), normalize it
    /// into a canonical `CustomTool`: `parameters` becomes `input_schema`
    /// (defaulting to an empty object schema) and every other field is
    /// dropped. Returns `None` for any other `type`, a missing or non-string
    /// `name`, or a present field whose value does not fit its canonical
    /// type (e.g. a numeric `description`).
    ///
    /// Accepts exactly the values [`CustomTool::responses_function_name`]
    /// accepts, so the Responses ingress (admission into `req.tools`) and the
    /// Responses egress (duplicate suppression against a replayed inline
    /// declaration) agree on which raw declarations count as functions.
    pub fn from_responses_function(v: &Value) -> Option<Self> {
        let name = Self::responses_function_name(v)?;
        let field = |key: &str| v.get(key).filter(|f| !f.is_null());
        Some(Self {
            name: name.to_owned(),
            description: field("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            input_schema: v
                .get("parameters")
                .cloned()
                .unwrap_or_else(empty_object_schema),
            cache_control: None,
            defer_loading: None,
            strict: field("strict").and_then(Value::as_bool),
            type_tag: None,
        })
    }

    /// The name of `v` if [`CustomTool::from_responses_function`] would
    /// accept it, borrowed from `v` itself; `None` exactly when that
    /// normalization returns `None`. Accepted shape: `type` is the string
    /// `"function"`, `name` is a string, `parameters` is any JSON value (the
    /// canonical schema is an untyped `Value`, and JSON Schema admits boolean
    /// schemas), and each of `description` (string) and `strict` (bool) is
    /// absent, `null`, or of that type. Allocates nothing, so a caller that
    /// only needs the name never copies the schema.
    pub fn responses_function_name(v: &Value) -> Option<&str> {
        let obj = v.as_object()?;
        if obj.get("type").and_then(Value::as_str) != Some("function") {
            return None;
        }
        let fits = |key: &str, is_type: fn(&Value) -> bool| {
            obj.get(key).is_none_or(|f| f.is_null() || is_type(f))
        };
        let fields_fit = fits("description", Value::is_string) && fits("strict", Value::is_boolean);
        if !fields_fit {
            return None;
        }
        obj.get("name").and_then(Value::as_str)
    }
}

impl ToolDef {
    /// Cache_control if the tool def carries one. The validator uses this
    /// to count breakpoints. Owned because for the `Other` variant the
    /// marker lives inside an arbitrary `Value` and is parsed on demand.
    ///
    /// On `Other` variants with a present-but-malformed `cache_control`
    /// payload (e.g. wrong type, unknown TTL), the parse failure is
    /// logged via `tracing::warn!` and the function returns `None`.
    /// Without this, malformed builtin-tool cache_control would
    /// silently fail to count toward the breakpoint cap, AND would
    /// re-serialize to upstream verbatim where it produces a vague
    /// 400. The WARN gives operators a server-side breadcrumb;
    /// downstream validators still treat this as "no breakpoint",
    /// matching pre-fix behavior so callers don't trip on a new
    /// hard error.
    pub fn cache_control(&self) -> Option<CacheControl> {
        match self {
            Self::Custom(c) => c.cache_control.clone(),
            Self::Other(v) => {
                let raw = v.as_object().and_then(|o| o.get("cache_control"))?;
                match serde_json::from_value::<CacheControl>(raw.clone()) {
                    Ok(cc) => Some(cc),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "ToolDef::Other carried a malformed cache_control; \
                             ignored for breakpoint counting (upstream may reject the request)",
                        );
                        None
                    }
                }
            }
        }
    }
}

impl Serialize for ToolDef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Custom(c) => c.serialize(serializer),
            Self::Other(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolDef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let type_field = value
            .as_object()
            .and_then(|o| o.get("type"))
            .and_then(|t| t.as_str());
        match type_field {
            // Absent or "custom" -> typed Custom variant. We deserialize
            // from the same Value (rather than re-serializing) to keep
            // unknown fields silently ignored, matching today's behavior
            // for ChatRequest as a whole.
            None | Some("custom") => {
                let value = if type_field.is_none() {
                    lift_bare_function(value)
                } else {
                    value
                };
                serde_json::from_value::<CustomTool>(value)
                    .map(ToolDef::Custom)
                    .map_err(serde::de::Error::custom)
            }
            // Builtin or unknown discriminator -> opaque passthrough.
            Some(_) => Ok(Self::Other(value)),
        }
    }
}

/// Rewrite a legacy bare-function element -- `{function: {name,
/// description?, parameters?, strict?}}` with no `type` and no top-level
/// `name` -- into the canonical `CustomTool` wire shape. Any other value is
/// returned unchanged, so a conventional custom tool that also happens to
/// carry a `function` key keeps its own top-level fields.
fn lift_bare_function(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    if obj.contains_key("name") {
        return Value::Object(obj);
    }
    let func = match obj.remove("function") {
        Some(Value::Object(func)) => func,
        Some(other) => {
            obj.insert("function".into(), other);
            return Value::Object(obj);
        }
        None => return Value::Object(obj),
    };
    for (from, to) in [
        ("name", "name"),
        ("description", "description"),
        ("parameters", "input_schema"),
        ("strict", "strict"),
    ] {
        if let Some(v) = func.get(from) {
            obj.entry(to).or_insert_with(|| v.clone());
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_custom_tool_round_trips() {
        let v = json!({
            "name": "calculator",
            "description": "do math",
            "input_schema": {"type": "object", "properties": {"a": {"type": "number"}}},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Custom(_)));
        assert_eq!(td.cache_control().unwrap().effective_ttl(), "1h");
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn explicit_type_custom_round_trips() {
        let v = json!({
            "type": "custom",
            "name": "calc",
            "input_schema": {"type": "object"}
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Custom(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn openai_function_tool_falls_to_other() {
        let v = json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object"}
            }
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Other(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn anthropic_builtin_tool_falls_to_other() {
        let v = json!({
            "type": "bash_20250124",
            "name": "bash"
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Other(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn minimal_custom_tool_uses_default_input_schema() {
        let v = json!({"name": "noop"});
        let td: ToolDef = serde_json::from_value(v).unwrap();
        if let ToolDef::Custom(c) = td {
            assert_eq!(c.name, "noop");
            assert_eq!(c.input_schema["type"], "object");
        } else {
            panic!("expected Custom variant");
        }
    }

    #[test]
    fn camelcase_input_schema_yields_the_same_schema_as_snake_case() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let snake = json!({"name": "read_file", "input_schema": schema.clone()});
        let camel = json!({"name": "read_file", "inputSchema": schema.clone()});

        // Act
        let from_snake: ToolDef = serde_json::from_value(snake).unwrap();
        let from_camel: ToolDef = serde_json::from_value(camel).unwrap();

        // Assert
        let (ToolDef::Custom(s), ToolDef::Custom(c)) = (&from_snake, &from_camel) else {
            panic!("expected Custom for both spellings: {from_snake:?} / {from_camel:?}");
        };
        assert_eq!(c.input_schema, schema);
        assert_eq!(c.input_schema, s.input_schema);
    }

    #[test]
    fn camelcase_input_schema_serializes_under_the_canonical_key() {
        // Arrange
        let v = json!({"name": "t", "inputSchema": {"type": "object", "required": ["x"]}});

        // Act
        let td: ToolDef = serde_json::from_value(v).unwrap();
        let out = serde_json::to_value(&td).unwrap();

        // Assert
        assert_eq!(out["input_schema"]["required"], json!(["x"]));
        assert!(out.get("inputSchema").is_none(), "{out}");
    }

    #[test]
    fn both_schema_spellings_on_one_tool_is_rejected() {
        // Arrange
        let v = json!({
            "name": "t",
            "input_schema": {"type": "object"},
            "inputSchema": {"type": "object", "required": ["x"]}
        });

        // Act
        let err = serde_json::from_value::<ToolDef>(v).unwrap_err();

        // Assert
        assert!(err.to_string().contains("duplicate field"), "{err}");
    }

    #[test]
    fn bare_function_element_without_type_becomes_custom_tool() {
        // Arrange
        let v = json!({
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                "strict": true
            }
        });

        // Act
        let td: ToolDef = serde_json::from_value(v).unwrap();

        // Assert
        let ToolDef::Custom(c) = td else {
            panic!("expected Custom variant");
        };
        assert_eq!(c.name, "get_weather");
        assert_eq!(c.description.as_deref(), Some("Get weather"));
        assert_eq!(c.input_schema["properties"]["city"]["type"], "string");
        assert_eq!(c.strict, Some(true));
        assert_eq!(c.type_tag, None);
    }

    #[test]
    fn bare_function_element_without_parameters_gets_default_schema() {
        // Arrange
        let v = json!({"function": {"name": "noop"}});

        // Act
        let td: ToolDef = serde_json::from_value(v).unwrap();

        // Assert
        let ToolDef::Custom(c) = td else {
            panic!("expected Custom variant");
        };
        assert_eq!(c.name, "noop");
        assert_eq!(c.input_schema, empty_object_schema());
    }

    #[test]
    fn bare_function_element_without_a_name_is_still_rejected() {
        // Arrange
        let v = json!({"function": {"description": "nameless"}});

        // Act
        let err = serde_json::from_value::<ToolDef>(v).unwrap_err();

        // Assert
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn top_level_name_wins_over_a_nested_function_object() {
        // Arrange: a conventional custom tool that happens to carry a
        // `function` key keeps its own top-level fields.
        let v = json!({
            "name": "outer",
            "input_schema": {"type": "object", "required": ["a"]},
            "function": {"name": "inner", "parameters": {"type": "object"}}
        });

        // Act
        let td: ToolDef = serde_json::from_value(v).unwrap();

        // Assert
        let ToolDef::Custom(c) = td else {
            panic!("expected Custom variant");
        };
        assert_eq!(c.name, "outer");
        assert_eq!(c.input_schema["required"], json!(["a"]));
    }

    #[test]
    fn responses_function_normalizes_to_custom_tool_with_renamed_schema() {
        // Arrange
        let v = json!({
            "type": "function",
            "name": "shell",
            "description": "run a command",
            "parameters": {"type": "object", "required": ["cmd"]},
            "strict": true
        });

        // Act
        let c = CustomTool::from_responses_function(&v).expect("normalizes");

        // Assert
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "name": "shell",
                "description": "run a command",
                "input_schema": {"type": "object", "required": ["cmd"]},
                "strict": true
            })
        );
    }

    #[test]
    fn responses_function_without_parameters_gets_default_schema() {
        // Arrange
        let v = json!({"type": "function", "name": "noop"});

        // Act
        let c = CustomTool::from_responses_function(&v).expect("normalizes");

        // Assert
        assert_eq!(c.input_schema, empty_object_schema());
        assert_eq!(c.description, None);
        assert_eq!(c.strict, None);
    }

    #[test]
    fn responses_function_drops_fields_outside_the_flat_function_shape() {
        // Arrange: canonical-only and alias spellings are not part of the
        // flat Responses function shape and must not leak into the result.
        let v = json!({
            "type": "function",
            "name": "shell",
            "inputSchema": {"type": "object", "required": ["alias"]},
            "cache_control": {"type": "ephemeral"},
            "defer_loading": true,
            "extra": "ignored"
        });

        // Act
        let c = CustomTool::from_responses_function(&v).expect("normalizes");

        // Assert
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"name": "shell", "input_schema": empty_object_schema()})
        );
    }

    /// Values that are not a well-formed flat Responses function
    /// declaration, one mistyped or missing field per case.
    fn malformed_responses_functions() -> Vec<Value> {
        vec![
            json!(null),
            json!("function"),
            json!([{"type": "function", "name": "shell"}]),
            json!({"name": "shell"}),
            json!({"type": "custom", "name": "shell"}),
            json!({"type": "web_search", "name": "shell"}),
            json!({"type": ["function"], "name": "shell"}),
            json!({"type": "function"}),
            json!({"type": "function", "name": null}),
            json!({"type": "function", "name": 7}),
            json!({"type": "function", "name": "shell", "description": 42}),
            json!({"type": "function", "name": "shell", "description": ["a"]}),
            json!({"type": "function", "name": "shell", "strict": "yes"}),
            json!({"type": "function", "name": "shell", "strict": 1}),
            json!({"type": "function", "function": {"name": "shell"}}),
        ]
    }

    #[test]
    fn responses_function_rejects_values_that_are_not_well_formed_functions() {
        for v in malformed_responses_functions() {
            // Act
            let normalized = CustomTool::from_responses_function(&v);
            let name = CustomTool::responses_function_name(&v);

            // Assert
            assert!(
                normalized.is_none(),
                "{v} must not normalize: {normalized:?}"
            );
            assert!(name.is_none(), "{v} must not yield a name: {name:?}");
        }
    }

    #[test]
    fn responses_function_name_agrees_with_normalization_on_well_formed_functions() {
        for v in [
            json!({"type": "function", "name": "shell"}),
            json!({"type": "function", "name": "", "parameters": {}}),
            json!({
                "type": "function",
                "name": "shell",
                "description": "run",
                "parameters": {"type": "object"},
                "strict": false
            }),
            json!({
                "type": "function",
                "name": "shell",
                "description": null,
                "parameters": null,
                "strict": null
            }),
            json!({"type": "function", "name": "shell", "extra": 42, "inputSchema": 7}),
        ] {
            // Act
            let normalized = CustomTool::from_responses_function(&v);
            let name = CustomTool::responses_function_name(&v);

            // Assert
            let normalized = normalized.unwrap_or_else(|| panic!("{v} must normalize"));
            assert_eq!(name, Some(normalized.name.as_str()), "{v}");
        }
    }

    #[test]
    fn responses_function_carries_any_parameters_value_verbatim() {
        for params in [
            json!(42),
            json!("{}"),
            json!([{"type": "object"}]),
            json!(true),
            json!(false),
            json!(null),
            json!({}),
            json!({"type": "object", "required": ["cmd"]}),
        ] {
            // Arrange
            let v = json!({"type": "function", "name": "shell", "parameters": params});

            // Act
            let normalized = CustomTool::from_responses_function(&v);
            let name = CustomTool::responses_function_name(&v);

            // Assert
            let normalized = normalized.unwrap_or_else(|| panic!("{v} must normalize"));
            assert_eq!(normalized.input_schema, params, "{v}");
            assert_eq!(name, Some("shell"), "{v}");
        }
    }

    #[test]
    fn responses_function_null_fields_normalize_as_their_serde_defaults() {
        // A present `null` maps the way serde maps it onto the canonical
        // fields: `None` for the optional ones, a literal null schema for
        // `input_schema` (its default applies only when the key is absent).
        // Arrange
        let v = json!({
            "type": "function",
            "name": "shell",
            "description": null,
            "parameters": null,
            "strict": null
        });

        // Act
        let c = CustomTool::from_responses_function(&v).expect("normalizes");

        // Assert
        assert_eq!(c.description, None);
        assert_eq!(c.strict, None);
        assert_eq!(c.input_schema, Value::Null);
    }

    #[test]
    fn responses_function_name_borrows_the_declarations_own_name() {
        // Arrange
        let v = json!({"type": "function", "name": "shell", "parameters": {"type": "object"}});

        // Act
        let name = CustomTool::responses_function_name(&v).expect("a function");

        // Assert
        let original = v["name"].as_str().expect("string name");
        assert!(
            std::ptr::eq(name, original),
            "the name must point into the declaration, not a copy"
        );
    }

    #[test]
    fn cache_control_extracts_from_other_variant() {
        let v = json!({
            "type": "web_search_20250901",
            "name": "search",
            "cache_control": {"type": "ephemeral"}
        });
        let td: ToolDef = serde_json::from_value(v).unwrap();
        assert!(td.cache_control().is_some());
    }
}
