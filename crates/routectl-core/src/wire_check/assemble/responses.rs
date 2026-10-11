//! OpenAI Responses: the `response` snapshot on `response.created`,
//! `response.in_progress` and the terminal `response.completed` or
//! `response.incomplete` (a length or content-filter cutoff, which still
//! delivers the response) is the envelope;
//! `output` is rebuilt from the item events by `output_index`, so it holds
//! every item the stream carried even when the terminal snapshot lists
//! fewer or none. With no item events, the snapshot's own `output` stands.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Budget, Input, Step, array_at, index_field, merge_field, replace_object, slot_at};

const FAILURE_EVENTS: [&str; 3] = ["response.failed", "response.cancelled", "error"];

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
        if is_failure(Some(&kind)) {
            return Step::UpstreamError;
        }
        let step = match kind.as_str() {
            "response.created" | "response.in_progress" => Step::Fold,
            "response.completed" | "response.incomplete" => Step::Close,
            other => return self.apply_item_event(other, body, budget),
        };
        if self.take_snapshot(&mut body, budget) {
            step
        } else {
            Step::OverBudget
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

    fn take_snapshot(&mut self, body: &mut Map<String, Value>, budget: &mut Budget) -> bool {
        match body.remove("response") {
            Some(Value::Object(response)) => replace_object(budget, &mut self.snapshot, response),
            _ => true,
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
                let Some(Value::Object(incoming)) = body.remove("item") else {
                    return Step::Fold;
                };
                let kept = slot_at(budget, &mut self.items, output_index)
                    .is_some_and(|item| replace_object(budget, item, incoming));
                return kept_step(kept);
            }
            "response.function_call_arguments.delta" => {
                let delta = take(&mut body, "delta");
                let kept = slot_at(budget, &mut self.items, output_index)
                    .is_some_and(|item| merge_field(budget, item, "arguments".to_owned(), delta));
                return kept_step(kept);
            }
            "response.function_call_arguments.done" => {
                let arguments = take(&mut body, "arguments");
                let kept = slot_at(budget, &mut self.items, output_index).is_some_and(|item| {
                    super::insert(budget, item, "arguments".to_owned(), arguments)
                });
                return kept_step(kept);
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
        let kept = slot_at(budget, &mut self.items, output_index)
            .is_some_and(|item| edit_part(item, list, part_index, edit, budget));
        kept_step(kept)
    }
}

/// Whether a frame of this kind reports a stream failure.
pub(super) fn is_failure(kind: Option<&str>) -> bool {
    kind.is_some_and(|kind| FAILURE_EVENTS.contains(&kind))
}

const fn kept_step(kept: bool) -> Step {
    if kept { Step::Fold } else { Step::OverBudget }
}

/// Apply `edit` to `item[list][part_index]`, padding the list with nulls
/// up to the index. Returns false when the node cap would be exceeded.
fn edit_part(
    item: &mut Map<String, Value>,
    list: PartList,
    part_index: u64,
    edit: PartEdit,
    budget: &mut Budget,
) -> bool {
    let Ok(part_index) = usize::try_from(part_index) else {
        return false;
    };
    let Some(parts) = array_at(budget, item, list.key()) else {
        return false;
    };
    let missing = part_index.saturating_add(1).saturating_sub(parts.len());
    if missing > 0 {
        if !budget.charge_nodes(missing) {
            return false;
        }
        parts.resize(part_index.saturating_add(1), Value::Null);
    }
    let Some(part) = parts.get_mut(part_index) else {
        return true;
    };
    match edit {
        PartEdit::Replace(value) => {
            if !budget.charge(&value) {
                return false;
            }
            let old = std::mem::replace(part, value);
            budget.credit(&old);
            true
        }
        PartEdit::Append(key, value) => with_object(part, |fields| {
            merge_field(budget, fields, key.to_owned(), value)
        }),
        PartEdit::Set(key, value) => with_object(part, |fields| {
            super::insert(budget, fields, key.to_owned(), value)
        }),
    }
}

/// Run `edit` on the part as an object; a non-object part (one node)
/// becomes an empty object (one node), so the swap costs nothing.
fn with_object(part: &mut Value, edit: impl FnOnce(&mut Map<String, Value>) -> bool) -> bool {
    if !part.is_object() {
        *part = Value::Object(Map::new());
    }
    match part {
        Value::Object(fields) => edit(fields),
        _ => true,
    }
}
