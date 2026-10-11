//! OpenAI Chat Completions: every chunk carries per-choice deltas; the
//! fold rebuilds each choice's `message`, joins tool-call argument
//! fragments by tool index, and keeps the last non-null usage. The response
//! is whole once every choice seen has a `finish_reason`, or at `[DONE]`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{
    Budget, Input, Step, index_field, insert, merge_field, object_at, set_nonempty, slot_at,
};

const CHUNK_OBJECT: &str = "chat.completion.chunk";
const COMPLETION_OBJECT: &str = "chat.completion";

#[derive(Default)]
pub(super) struct ChatFold {
    envelope: Map<String, Value>,
    choices: BTreeMap<u64, Choice>,
}

#[derive(Default)]
struct Choice {
    fields: Map<String, Value>,
    message: Map<String, Value>,
    tool_calls: BTreeMap<u64, Map<String, Value>>,
}

impl Choice {
    fn finish(mut self, index: u64) -> Value {
        if !self.tool_calls.is_empty() {
            let calls = self.tool_calls.into_values().map(Value::Object).collect();
            self.message
                .insert("tool_calls".to_owned(), Value::Array(calls));
        }
        self.fields.insert("index".to_owned(), Value::from(index));
        self.fields
            .insert("message".to_owned(), Value::Object(self.message));
        Value::Object(self.fields)
    }

    fn apply_delta(&mut self, delta: Map<String, Value>, budget: &mut Budget) -> bool {
        delta
            .into_iter()
            .all(|(key, value)| match (key.as_str(), value) {
                ("tool_calls", Value::Array(calls)) => calls
                    .into_iter()
                    .all(|call| self.apply_tool_call(call, budget)),
                ("role", value) => set_nonempty(budget, &mut self.message, key, value),
                (_, value) => merge_field(budget, &mut self.message, key, value),
            })
    }

    /// Ids, types and names are repeated on later fragments by some
    /// upstreams, so they replace; only `function.arguments` joins.
    fn apply_tool_call(&mut self, call: Value, budget: &mut Budget) -> bool {
        let Value::Object(mut call) = call else {
            return true;
        };
        let index = index_field(&call, "index").unwrap_or(0);
        call.remove("index");
        let Some(entry) = slot_at(budget, &mut self.tool_calls, index) else {
            return false;
        };
        call.into_iter()
            .all(|(key, value)| match (key.as_str(), value) {
                ("function", Value::Object(function)) => apply_function(entry, function, budget),
                ("id" | "type", value) => set_nonempty(budget, entry, key, value),
                (_, value) => merge_field(budget, entry, key, value),
            })
    }
}

fn apply_function(
    entry: &mut Map<String, Value>,
    function: Map<String, Value>,
    budget: &mut Budget,
) -> bool {
    let Some(target) = object_at(budget, entry, "function") else {
        return false;
    };
    function.into_iter().all(|(key, value)| {
        if key == "name" {
            set_nonempty(budget, target, key, value)
        } else {
            merge_field(budget, target, key, value)
        }
    })
}

/// Whether a chunk reports a stream failure: a top-level `error` holding
/// anything but null or an empty object.
pub(super) fn is_failure(body: &Map<String, Value>) -> bool {
    match body.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(_) => true,
    }
}

impl ChatFold {
    pub(super) fn apply(&mut self, input: Input, budget: &mut Budget) -> Step {
        let mut body = match input {
            Input::Done => return Step::Close,
            Input::Event { body, .. } => body,
        };
        if is_failure(&body) {
            return Step::UpstreamError;
        }
        let choices_kept = match body.remove("choices") {
            Some(Value::Array(choices)) => choices
                .into_iter()
                .all(|choice| self.apply_choice(choice, budget)),
            _ => true,
        };
        if !choices_kept || !self.apply_envelope(body, budget) {
            return Step::OverBudget;
        }
        Step::Fold
    }

    pub(super) fn finish(mut self) -> Value {
        let choices = self
            .choices
            .into_iter()
            .map(|(index, choice)| choice.finish(index))
            .collect();
        self.envelope
            .insert("choices".to_owned(), Value::Array(choices));
        Value::Object(self.envelope)
    }

    fn apply_envelope(&mut self, body: Map<String, Value>, budget: &mut Budget) -> bool {
        body.into_iter()
            .all(|(key, value)| match (key.as_str(), value) {
                ("object", Value::String(object)) if object == CHUNK_OBJECT => insert(
                    budget,
                    &mut self.envelope,
                    key,
                    Value::String(COMPLETION_OBJECT.to_owned()),
                ),
                ("usage", value) if !value.is_null() => {
                    insert(budget, &mut self.envelope, key, value)
                }
                (_, value) => set_nonempty(budget, &mut self.envelope, key, value),
            })
    }

    pub(super) fn every_choice_finished(&self) -> bool {
        !self.choices.is_empty()
            && self
                .choices
                .values()
                .all(|choice| choice.fields.contains_key("finish_reason"))
    }

    fn apply_choice(&mut self, choice: Value, budget: &mut Budget) -> bool {
        let Value::Object(mut choice) = choice else {
            return true;
        };
        let index = index_field(&choice, "index").unwrap_or(0);
        choice.remove("index");
        let Some(state) = slot_at(budget, &mut self.choices, index) else {
            return false;
        };
        choice
            .into_iter()
            .all(|(key, value)| match (key.as_str(), value) {
                ("delta", Value::Object(delta)) => state.apply_delta(delta, budget),
                ("finish_reason", value) => set_nonempty(budget, &mut state.fields, key, value),
                (_, value) => merge_field(budget, &mut state.fields, key, value),
            })
    }
}
