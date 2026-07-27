//! All-or-nothing stable sort of `tools[]` by name at the OAuth-egress cloak
//! seam.
//!
//! `tools[]` is an ordered wire array. Sorting it stabilizes the cache prefix
//! for a client that shuffles tool order request-to-request, but only when
//! every tool is a NAMED CUSTOM tool with a UNIQUE name. A builtin / opaque
//! passthrough (`ToolDef::Other` -- a non-empty `type` other than `"custom"`)
//! carries no top-level name and rides a verbatim-passthrough contract, so
//! reordering the array around it risks silent semantic corruption. A missing
//! or duplicate name is equally unsafe. Any one of these stands the ENTIRE
//! sort down -- a partial reorder is worse than none.

use std::collections::HashSet;

use serde_json::Value;

/// Stably sort the outgoing body's `tools[]` by name, IFF every entry is a
/// named custom tool with a unique name. Otherwise a no-op (verbatim order).
///
/// Deterministic and idempotent: the names are unique, so a stable sort by
/// name yields one fixed order, and re-running it over the already-sorted
/// array is a no-op. Pure aside from the in-place reorder of `tools[]`.
pub(super) fn sort_custom_tools_by_name(body: &mut Value) {
    let should_sort = {
        let Some(tools) = body.get("tools").and_then(Value::as_array) else {
            return;
        };
        // Nothing to reorder for fewer than two tools; skip the scan.
        if tools.len() < 2 {
            return;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(tools.len());
        let mut ok = true;
        for tool in tools {
            match custom_tool_name(tool) {
                // `insert` returns false on a duplicate, falling through to
                // the stand-down arm.
                Some(name) if seen.insert(name) => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        ok
    };
    if !should_sort {
        return;
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.sort_by(|a, b| {
            let an = a.get("name").and_then(Value::as_str).unwrap_or_default();
            let bn = b.get("name").and_then(Value::as_str).unwrap_or_default();
            an.cmp(bn)
        });
    }
}

/// The tool's name IFF it is a named custom tool: an object whose `type` is
/// absent or exactly `"custom"` and whose `name` is a non-empty string.
/// `None` for an opaque / builtin tool (a `type` present and not `"custom"`)
/// or a missing / empty / non-string name.
fn custom_tool_name(tool: &Value) -> Option<&str> {
    let obj = tool.as_object()?;
    let is_custom = match obj.get("type") {
        None => true,
        Some(Value::String(t)) => t == "custom",
        Some(_) => false,
    };
    if !is_custom {
        return None;
    }
    let name = obj.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn sorts_unique_named_custom_tools() {
        let mut body = json!({
            "tools": [
                {"name": "zebra"},
                {"name": "alpha"},
                {"type": "custom", "name": "mango"}
            ]
        });
        sort_custom_tools_by_name(&mut body);
        assert_eq!(names(&body), vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn idempotent_double_sort_equals_single() {
        let template = json!({
            "tools": [{"name": "delta"}, {"name": "beta"}, {"name": "charlie"}]
        });
        let mut once = template.clone();
        sort_custom_tools_by_name(&mut once);
        let mut twice = once.clone();
        sort_custom_tools_by_name(&mut twice);
        assert_eq!(once, twice);
    }

    #[test]
    fn stands_down_on_opaque_tool_present() {
        // A single builtin (Other-shaped: non-custom `type`) stands the
        // whole sort down -- verbatim order preserved.
        let mut body = json!({
            "tools": [
                {"name": "zebra"},
                {"type": "web_search_20250901", "name": "search"},
                {"name": "alpha"}
            ]
        });
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn stands_down_on_duplicate_names() {
        let mut body = json!({
            "tools": [{"name": "dup"}, {"name": "alpha"}, {"name": "dup"}]
        });
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn stands_down_on_missing_name() {
        let mut body = json!({
            "tools": [{"name": "zebra"}, {"description": "no name here"}]
        });
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn stands_down_on_empty_name() {
        let mut body = json!({
            "tools": [{"name": "zebra"}, {"name": ""}]
        });
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn no_op_on_single_tool() {
        let mut body = json!({"tools": [{"name": "solo"}]});
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn no_op_when_no_tools_field() {
        let mut body = json!({"messages": []});
        let before = body.clone();
        sort_custom_tools_by_name(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn explicit_type_custom_is_sorted() {
        let mut body = json!({
            "tools": [
                {"type": "custom", "name": "yak"},
                {"type": "custom", "name": "ant"}
            ]
        });
        sort_custom_tools_by_name(&mut body);
        assert_eq!(names(&body), vec!["ant", "yak"]);
    }
}
