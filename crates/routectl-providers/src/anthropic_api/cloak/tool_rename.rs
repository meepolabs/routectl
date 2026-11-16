//! Normalizes tool names to the mcp__ prefix and records the per-request
//! reverse map that restores the client's names on the response.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// The single-underscore `mcp_` prefix: a non-Claude-Code tool name shape
/// that, alongside any other non-`mcp__` name, trips the Anthropic
/// subscription billing classifier. Used to detect the prefix-doubling
/// subcase so the result is `mcp__<rest>` rather than `mcp__mcp_<rest>`.
const MCP_SINGLE_PREFIX: &str = "mcp_";

/// The canonical double-underscore prefix the classifier accepts. Every
/// tool name that does not already start with this is normalized to it.
const MCP_DOUBLE_PREFIX: &str = "mcp__";

/// Normalize every tool NAME on the outgoing body to the canonical
/// double-underscore `mcp__` prefix. A non-Claude-Code request whose tool
/// SET contains enough non-`mcp__` names is diverted to the extra-usage
/// billing lane; prefixing every such name with `mcp__` keeps the request
/// in the subscription lane. The rename is prefix-only (the original name
/// is preserved as the suffix, internal separators untouched), idempotent
/// (an already-`mcp__` name is left alone and records no reverse entry),
/// and pure.
///
/// Per name:
/// - already starts with `mcp__` -> unchanged (no reverse entry).
/// - starts with `mcp_` (single underscore) -> the prefix is doubled:
///   `mcp_x` -> `mcp__x` (NOT `mcp__mcp_x`).
/// - any other (bare) name -> prefixed: `read_file` -> `mcp__read_file`.
///
/// Returns the per-request reverse map (renamed upstream name -> original
/// client name) containing ONLY names actually renamed.
///
/// Paths walked (serde_json `Value`, by path not typed enum, so unknown
/// forward-compat block types are still handled correctly):
/// - `tools[].name`
/// - `tool_choice.name` (when `tool_choice.type == "tool"`)
/// - `messages[].content[]` entries of type `tool_use` (`.name`)
/// - `messages[].content[]` entries of type `tool_reference` (`.tool_name`)
/// - nested `tool_result.content[]` entries of type `tool_reference`
///   (`.tool_name`)
pub(super) fn normalize_tool_names_to_mcp(body: &mut Value) -> HashMap<String, String> {
    let mut reverse: HashMap<String, String> = HashMap::new();
    let existing = collect_existing_tool_names(body);
    rename_tools_array(body, &existing, &mut reverse);
    rename_tool_choice(body, &existing, &mut reverse);
    rename_message_tool_refs(body, &existing, &mut reverse);
    reverse
}

/// Compute the canonical `mcp__` form of a tool name, or `None` if it
/// already starts with `mcp__` (idempotent: no rename, no reverse entry).
/// A single-underscore `mcp_` prefix is doubled (`mcp_x` -> `mcp__x`); any
/// other (bare) name is prefixed (`read_file` -> `mcp__read_file`).
fn renamed_to_mcp(name: &str) -> Option<String> {
    if name.starts_with(MCP_DOUBLE_PREFIX) {
        return None;
    }
    if let Some(suffix) = name.strip_prefix(MCP_SINGLE_PREFIX) {
        return Some(format!("{MCP_DOUBLE_PREFIX}{suffix}"));
    }
    Some(format!("{MCP_DOUBLE_PREFIX}{name}"))
}

/// Gather every tool name in the request's `tools[]` array. This is a
/// PRE-RENAME snapshot of `tools[]` names only: it is computed once before
/// the forward pass, does NOT track in-progress renames, and does NOT
/// detect collisions against names that appear only in message history
/// (`tool_use` / `tool_reference`). It backs the collision guard in
/// `rename_name_field`.
fn collect_existing_tool_names(body: &Value) -> HashSet<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Apply the rename at a single JSON object's string field, honoring the
/// collision guard and recording the reverse mapping. `existing` is the
/// pre-rename snapshot of `tools[]` names; a renamed form already present
/// there is skipped with a warning. The same name appearing across
/// multiple surfaces (e.g. in `tools[]` AND a prior `tool_use`) is renamed
/// on every surface by design -- safe because the rename is idempotent and
/// the reverse map records first-seen, so identical originals map to
/// identical renamed forms.
fn rename_name_field(
    obj: &mut Value,
    field: &str,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
) {
    let Some(original) = obj.get(field).and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let Some(renamed) = renamed_to_mcp(&original) else {
        return;
    };
    if existing.contains(&renamed) {
        tracing::warn!(
            "cloak tool-name rename skipped: renamed form collides with an \
             existing tool name in the request"
        );
        return;
    }
    if let Some(map) = obj.as_object_mut() {
        map.insert(field.to_string(), Value::String(renamed.clone()));
    }
    // First-seen is sufficient: the prefix-only mcp normalization maps a
    // given original to exactly one renamed form, and the collision guard
    // above skips a rename whose target already exists among tool names, so
    // distinct originals do not silently share one renamed form.
    reverse.entry(renamed).or_insert(original);
}

/// Rename `tools[].name` in place. Anthropic native builtins (objects
/// carrying a non-empty `"type"`) are skipped.
fn rename_tools_array(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut() {
        if tool_is_builtin(tool) {
            continue;
        }
        rename_name_field(tool, "name", existing, reverse);
    }
}

/// True for an Anthropic native builtin tool (web_search, code_execution,
/// ...): an object with a non-empty `"type"` field. Such tools are left
/// unchanged.
fn tool_is_builtin(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| !t.is_empty())
}

/// Rename `tool_choice.name` when `tool_choice.type == "tool"`.
fn rename_tool_choice(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
) {
    let is_tool_choice = body
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "tool");
    if !is_tool_choice {
        return;
    }
    if let Some(tc) = body.get_mut("tool_choice") {
        rename_name_field(tc, "name", existing, reverse);
    }
}

/// Rename tool references in message history: `tool_use.name`,
/// `tool_reference.tool_name`, and nested
/// `tool_result.content[].tool_reference.tool_name`.
fn rename_message_tool_refs(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content.iter_mut() {
            rename_content_part(part, existing, reverse);
        }
    }
}

/// Rename a single `messages[].content[]` entry by its `type`:
/// `tool_use` -> `.name`; `tool_reference` -> `.tool_name`;
/// `tool_result` -> recurse into nested `content[]` `tool_reference`s.
fn rename_content_part(
    part: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
) {
    match part.get("type").and_then(Value::as_str) {
        Some("tool_use") => rename_name_field(part, "name", existing, reverse),
        Some("tool_reference") => rename_name_field(part, "tool_name", existing, reverse),
        Some("tool_result") => {
            let Some(nested) = part.get_mut("content").and_then(Value::as_array_mut) else {
                return;
            };
            for nested_part in nested.iter_mut() {
                if nested_part.get("type").and_then(Value::as_str) == Some("tool_reference") {
                    rename_name_field(nested_part, "tool_name", existing, reverse);
                }
            }
        }
        _ => {}
    }
}
