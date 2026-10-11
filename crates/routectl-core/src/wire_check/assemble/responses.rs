//! OpenAI Responses: the `response` snapshot on `response.created`,
//! `response.in_progress` and `response.completed` is the envelope;
//! `output` is rebuilt from the item events by `output_index`, so it holds
//! every item the stream carried even when the terminal snapshot lists
//! fewer or none. With no item events, the snapshot's own `output` stands.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Budget, Input, Step, index_field, merge_field};

#[derive(Default)]
pub(super) struct ResponsesFold {
    snapshot: Map<String, Value>,
    items: BTreeMap<u64, Map<String, Value>>,
}

/// Which array inside an item a part event addresses.
#[derive(Clone, Copy)]
enum PartList {
    Content,
    Summary,
}

impl PartList {
    const fn key(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Summary => "summary",
        }
    }

    const fn index_key(self) -> &'static str {
        match self {
            Self::Content => "content_index",
            Self::Summary => "summary_index",
        }
    }
}

/// What a part-level event does to the addressed part.
enum PartEdit {
    Replace(Value),
    Append(&'static str, Value),
    Set(&'static str, Value),
}

impl ResponsesFold {
    pub(super) fn apply(&mut self, input: Input, budget: &mut Budget) -> Step {
        let Input::Event { kind, mut body } = input else {
            return Step::Fold;
        };
        let Some(kind) = kind else {
            return Step::Fold;
        };
        match kind.as_str() {
            "response.created" | "response.in_progress" => {
                self.take_snapshot(&mut body);
                Step::Fold
            }
            "response.completed" => {
                self.take_snapshot(&mut body);
                Step::Close
            }
            "response.failed" | "response.incomplete" | "response.cancelled" | "error" => {
                Step::UpstreamError
            }
            other => self.apply_item_event(other, body, budget),
        }
    }

    pub(super) fn finish(mut self) -> Value {
        if !self.items.is_empty() {
            let output = self.items.into_values().map(Value::Object).collect();
            self.snapshot
                .insert("output".to_owned(), Value::Array(output));
        }
        Value::Object(self.snapshot)
    }

    fn take_snapshot(&mut self, body: &mut Map<String, Value>) {
        if let Some(Value::Object(response)) = body.remove("response") {
            self.snapshot = response;
        }
    }

    fn apply_item_event(
        &mut self,
        kind: &str,
        mut body: Map<String, Value>,
        budget: &mut Budget,
    ) -> Step {
        let Some(output_index) = index_field(&body, "output_index") else {
            return Step::Fold;
        };
        let take = |body: &mut Map<String, Value>, key: &str| body.remove(key).unwrap_or_default();
        let (list, edit) = match kind {
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(Value::Object(item)) = body.remove("item") {
                    self.items.insert(output_index, item);
                }
                return Step::Fold;
            }
            "response.function_call_arguments.delta" => {
                let delta = take(&mut body, "delta");
                let item = self.items.entry(output_index).or_default();
                merge_field(item, "arguments".to_owned(), delta);
                return Step::Fold;
            }
            "response.function_call_arguments.done" => {
                let arguments = take(&mut body, "arguments");
                let item = self.items.entry(output_index).or_default();
                item.insert("arguments".to_owned(), arguments);
                return Step::Fold;
            }
            "response.content_part.added" | "response.content_part.done" => (
                PartList::Content,
                PartEdit::Replace(take(&mut body, "part")),
            ),
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => (
                PartList::Summary,
                PartEdit::Replace(take(&mut body, "part")),
            ),
            "response.output_text.delta" | "response.reasoning_text.delta" => (
                PartList::Content,
                PartEdit::Append("text", take(&mut body, "delta")),
            ),
            "response.refusal.delta" => (
                PartList::Content,
                PartEdit::Append("refusal", take(&mut body, "delta")),
            ),
            "response.reasoning_summary_text.delta" => (
                PartList::Summary,
                PartEdit::Append("text", take(&mut body, "delta")),
            ),
            "response.output_text.done" | "response.reasoning_text.done" => (
                PartList::Content,
                PartEdit::Set("text", take(&mut body, "text")),
            ),
            "response.refusal.done" => (
                PartList::Content,
                PartEdit::Set("refusal", take(&mut body, "refusal")),
            ),
            "response.reasoning_summary_text.done" => (
                PartList::Summary,
                PartEdit::Set("text", take(&mut body, "text")),
            ),
            _ => return Step::Fold,
        };
        let part_index = index_field(&body, list.index_key()).unwrap_or(0);
        let item = self.items.entry(output_index).or_default();
        if edit_part(item, list, part_index, edit, budget) {
            Step::Fold
        } else {
            Step::OverBudget
        }
    }
}

/// Apply `edit` to `item[list][part_index]`, padding the list with nulls
/// up to the index. Returns false when the padding would exceed the
/// budget.
fn edit_part(
    item: &mut Map<String, Value>,
    list: PartList,
    part_index: u64,
    edit: PartEdit,
    budget: &mut Budget,
) -> bool {
    let slot = item
        .entry(list.key())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !slot.is_array() {
        *slot = Value::Array(Vec::new());
    }
    let Value::Array(parts) = slot else {
        return true;
    };
    let Ok(part_index) = usize::try_from(part_index) else {
        return false;
    };
    let missing = part_index.saturating_add(1).saturating_sub(parts.len());
    if missing > 0 {
        if !budget.pad(missing) {
            return false;
        }
        parts.resize(part_index.saturating_add(1), Value::Null);
    }
    let Some(part) = parts.get_mut(part_index) else {
        return true;
    };
    match edit {
        PartEdit::Replace(value) => *part = value,
        PartEdit::Append(key, value) => with_object(part, |fields| {
            merge_field(fields, key.to_owned(), value);
        }),
        PartEdit::Set(key, value) => with_object(part, |fields| {
            fields.insert(key.to_owned(), value);
        }),
    }
    true
}

fn with_object(part: &mut Value, edit: impl FnOnce(&mut Map<String, Value>)) {
    if !part.is_object() {
        *part = Value::Object(Map::new());
    }
    if let Value::Object(fields) = part {
        edit(fields);
    }
}
