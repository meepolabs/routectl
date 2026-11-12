//! Capture of the members of `function_call` and `reasoning` input items
//! that canonical does not model, into the request's
//! [`ResponsesInputItemMembers`] record. Canonical folds both kinds into
//! fields other egresses serialize, so their members ride the record, keyed
//! by the identity the Responses egress emits the item under.

use serde_json::{Map, Value};

use routectl_core::{ResponsesInputItemMembers, ResponsesItemMemberEntry};
use routectl_providers::openai_responses::{FUNCTION_CALL_ITEM_KEYS, REASONING_ITEM_KEYS};

/// Record the unmodeled members of the `function_call` item at `index` in
/// the client's `input[]`, keyed by the call id canonical takes for it.
pub(super) fn capture_function_call(
    record: &mut ResponsesInputItemMembers,
    index: usize,
    item: &Value,
) {
    let key = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(entry) = entry(index, item, FUNCTION_CALL_ITEM_KEYS) else {
        return;
    };
    insert(&mut record.function_call, &mut record.unkeyed, key, entry);
}

/// Record the unmodeled members of the `reasoning` item at `index` in the
/// client's `input[]`, keyed by its `id`.
pub(super) fn capture_reasoning(
    record: &mut ResponsesInputItemMembers,
    index: usize,
    item: &Value,
) {
    let key = item.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(entry) = entry(index, item, REASONING_ITEM_KEYS) else {
        return;
    };
    insert(&mut record.reasoning, &mut record.unkeyed, key, entry);
}

fn entry(index: usize, item: &Value, modeled: &[&str]) -> Option<ResponsesItemMemberEntry> {
    let members: Map<String, Value> = item
        .as_object()?
        .iter()
        .filter(|(key, _)| !modeled.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!members.is_empty()).then(|| ResponsesItemMemberEntry {
        client_pointer: format!("/input/{index}"),
        members,
    })
}

/// File `entry` under `key`; an empty key or one already taken has no
/// identity of its own to be emitted under.
fn insert(
    keyed: &mut std::collections::BTreeMap<String, ResponsesItemMemberEntry>,
    unkeyed: &mut Vec<ResponsesItemMemberEntry>,
    key: &str,
    entry: ResponsesItemMemberEntry,
) {
    if key.is_empty() || keyed.contains_key(key) {
        unkeyed.push(entry);
    } else {
        keyed.insert(key.to_owned(), entry);
    }
}
