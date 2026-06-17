//! Shared parse step for OpenAI-shape `Message.tool_calls` entries.
//!
//! The OpenAI Chat Completions ingress populates the canonical
//! `Message.tool_calls` field on an assistant turn rather than emitting
//! `KnownContentPart::ToolUse` content parts. Three egresses must re-emit
//! those calls so the following `tool_result` turn is not orphaned
//! upstream. The egresses emit different native shapes (Anthropic
//! `ContentBlock::ToolUse`, Bedrock Converse `toolUse`, Responses
//! `function_call`), so a single shared emit function returning one type
//! is impossible -- but the PARSE is identical across all of them and
//! lives here.
//!
//! OpenAI shape: `{id, type: "function", function: {name, arguments}}`
//! where `arguments` is a JSON-ENCODED STRING. This helper extracts the
//! id / name and parses `arguments` into a `serde_json::Value`. Callers
//! that need the Responses wire form re-serialize `arguments` back to a
//! string; callers that need an object (Anthropic input, Converse input)
//! use the `Value` directly.

use serde_json::{json, Value};

/// One OpenAI-shape `tool_calls` entry, normalized for re-emission.
///
/// `arguments` is always a parsed JSON value (an object on the happy
/// path). When the source `arguments` string is not valid JSON it is
/// wrapped under `{"_arguments": "<raw>"}` so the upstream returns a
/// useful error instead of routectl silently shipping a malformed body --
/// mirrors the Anthropic-API egress fallback.
pub(crate) struct NormalizedToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

/// Parse a slice of OpenAI-shape `tool_calls` entries into
/// `NormalizedToolCall`s. `index` is used only to synthesize a
/// deterministic, non-empty id when an entry omits one: both the Bedrock
/// Converse `toolUseId` and the Responses `call_id` must be non-empty or
/// the upstream rejects the body, so an empty id is replaced with
/// `call_<index>`. `provider` tags the WARN emitted when an entry's
/// `arguments` string is not valid JSON.
pub(crate) fn normalize_tool_calls(
    provider: &str,
    tool_calls: &[Value],
) -> Vec<NormalizedToolCall> {
    tool_calls
        .iter()
        .enumerate()
        .map(|(index, call)| normalize_one(provider, index, call))
        .collect()
}

fn normalize_one(provider: &str, index: usize, call: &Value) -> NormalizedToolCall {
    let raw_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
    // Empty-id fallback first (deterministic `call_<index>`, itself
    // charset-valid), then sanitize to Anthropic's `^[a-zA-Z0-9_-]+$`
    // so the emitted toolUseId / call_id matches the tool_result that
    // correlates to it. Sanitization is deterministic, so a sanitized
    // id and its result land on the same value.
    let id = if raw_id.is_empty() {
        format!("call_{index}")
    } else {
        crate::tool_id::sanitize_tool_id(raw_id).into_owned()
    };
    let function = call.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Branch on the wire shape of `function.arguments`. The OpenAI spec
    // calls for a JSON-encoded string, but some upstreams (and some
    // egresses on the multi-turn echo path) ship an already-parsed object.
    // The previous `.as_str()` chain dropped the object case to `"{}"`
    // and lost the args, breaking the tool loop. Both object-consuming
    // egresses (Anthropic, Bedrock Converse) want the Value object
    // directly; the Responses egress re-serializes to a string at its
    // emit site.
    let args_ref = function.and_then(|f| f.get("arguments"));
    let arguments = match args_ref {
        Some(Value::String(s)) => parse_arguments_string(provider, &id, s),
        Some(v @ Value::Object(_)) => v.clone(),
        _ => json!({}),
    };
    NormalizedToolCall {
        id,
        name,
        arguments,
    }
}

/// Parse the OpenAI-spec stringified `arguments` payload. On parse
/// failure, wrap the raw text under `{"_arguments": "<raw>"}` and emit a
/// WARN so the upstream returns a useful error instead of routectl
/// silently shipping a malformed body. An empty string collapses to an
/// empty object.
fn parse_arguments_string(provider: &str, id: &str, raw: &str) -> Value {
    if raw.is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|e| {
        tracing::warn!(
            provider = provider,
            tool_id = %id,
            error = %e,
            "tool_call.arguments not valid JSON; wrapping under _arguments for upstream",
        );
        json!({ "_arguments": raw })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A standard OpenAI-shape entry yields the id, name, and a parsed
    /// arguments object.
    #[test]
    fn parses_id_name_and_object_arguments() {
        // Arrange
        let calls = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
        })];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "call_1");
        assert_eq!(out[0].name, "get_weather");
        assert_eq!(out[0].arguments, json!({"city": "SF"}));
    }

    /// A missing id is synthesized to a non-empty `call_<index>` value so
    /// the downstream toolUseId / call_id is never empty.
    #[test]
    fn missing_id_is_synthesized_non_empty() {
        // Arrange
        let calls = vec![
            json!({"function": {"name": "a", "arguments": "{}"}}),
            json!({"id": "", "function": {"name": "b", "arguments": "{}"}}),
        ];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out[0].id, "call_0");
        assert_eq!(out[1].id, "call_1");
        assert!(out.iter().all(|c| !c.id.is_empty()));
    }

    /// Non-JSON arguments fall back to a `{"_arguments": "<raw>"}` wrap
    /// rather than producing a malformed body.
    #[test]
    fn non_json_arguments_wrap_under_underscore_arguments() {
        // Arrange
        let calls = vec![json!({
            "id": "call_x",
            "function": {"name": "f", "arguments": "not json"},
        })];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out[0].arguments, json!({"_arguments": "not json"}));
    }

    /// Empty arguments string collapses to an empty object.
    #[test]
    fn empty_arguments_string_yields_empty_object() {
        // Arrange
        let calls = vec![json!({
            "id": "call_e",
            "function": {"name": "f", "arguments": ""},
        })];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out[0].arguments, json!({}));
    }

    /// An already-parsed object `arguments` value must pass through
    /// unchanged. The previous `.as_str()` chain dropped this case to
    /// `{}` and lost the args, breaking the tool loop for any upstream
    /// that ships objects instead of the spec string.
    #[test]
    fn object_arguments_pass_through_unchanged() {
        // Arrange
        let calls = vec![json!({
            "id": "call_obj",
            "function": {"name": "get_weather", "arguments": {"city": "SF"}},
        })];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out[0].arguments, json!({"city": "SF"}));
    }

    /// Missing `arguments` (neither string nor object) defaults to an
    /// empty object so downstream emit sites don't have to special-case
    /// a missing key.
    #[test]
    fn missing_arguments_yields_empty_object() {
        // Arrange
        let calls = vec![json!({
            "id": "call_m",
            "function": {"name": "f"},
        })];

        // Act
        let out = normalize_tool_calls("test", &calls);

        // Assert
        assert_eq!(out[0].arguments, json!({}));
    }
}
