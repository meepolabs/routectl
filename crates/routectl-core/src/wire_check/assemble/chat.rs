//! OpenAI Chat Completions: every chunk carries per-choice deltas; the
//! fold rebuilds each choice's `message`, joins tool-call argument
//! fragments by tool index, and keeps the last non-null usage. The response
//! is whole once every choice seen has a `finish_reason`, or at `[DONE]`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Input, Step, index_field, merge_field, set_nonempty};

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

    fn apply_delta(&mut self, delta: Map<String, Value>) {
        for (key, value) in delta {
            match (key.as_str(), value) {
                ("tool_calls", Value::Array(calls)) => {
                    for call in calls {
                        self.apply_tool_call(call);
                    }
                }
                ("role", value) => set_nonempty(&mut self.message, key, value),
                (_, value) => merge_field(&mut self.message, key, value),
            }
        }
    }

    /// Ids, types and names are repeated on later fragments by some
    /// upstreams, so they replace; only `function.arguments` joins.
    fn apply_tool_call(&mut self, call: Value) {
        let Value::Object(mut call) = call else {
            return;
        };
        let index = index_field(&call, "index").unwrap_or(0);
        call.remove("index");
        let entry = self.tool_calls.entry(index).or_default();
        for (key, value) in call {
            match (key.as_str(), value) {
                ("function", Value::Object(function)) => apply_function(entry, function),
                ("id" | "type", value) => set_nonempty(entry, key, value),
                (_, value) => merge_field(entry, key, value),
            }
        }
    }
}

fn apply_function(entry: &mut Map<String, Value>, function: Map<String, Value>) {
    let slot = entry
        .entry("function")
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    let Value::Object(target) = slot else {
        return;
    };
    for (key, value) in function {
        if key == "name" {
            set_nonempty(target, key, value);
        } else {
            merge_field(target, key, value);
        }
    }
}

impl ChatFold {
    pub(super) fn apply(&mut self, input: Input) -> Step {
        let mut body = match input {
            Input::Done => return Step::Close,
            Input::Event { body, .. } => body,
        };
        if carries_error(&body) {
            return Step::UpstreamError;
        }
        if let Some(Value::Array(choices)) = body.remove("choices") {
            for choice in choices {
                self.apply_choice(choice);
            }
        }
        for (key, value) in body {
            match (key.as_str(), value) {
                ("object", Value::String(object)) if object == CHUNK_OBJECT => {
                    self.envelope
                        .insert(key, Value::String(COMPLETION_OBJECT.to_owned()));
                }
                ("usage", value) if !value.is_null() => {
                    self.envelope.insert(key, value);
                }
                (_, value) => set_nonempty(&mut self.envelope, key, value),
            }
        }
        if self.every_choice_finished() {
            Step::Terminal
        } else {
            Step::Fold
        }
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

    fn every_choice_finished(&self) -> bool {
        !self.choices.is_empty()
            && self
                .choices
                .values()
                .all(|choice| choice.fields.contains_key("finish_reason"))
    }

    fn apply_choice(&mut self, choice: Value) {
        let Value::Object(mut choice) = choice else {
            return;
        };
        let index = index_field(&choice, "index").unwrap_or(0);
        choice.remove("index");
        let state = self.choices.entry(index).or_default();
        for (key, value) in choice {
            match (key.as_str(), value) {
                ("delta", Value::Object(delta)) => state.apply_delta(delta),
                ("finish_reason", value) => set_nonempty(&mut state.fields, key, value),
                (_, value) => merge_field(&mut state.fields, key, value),
            }
        }
    }
}

/// A chunk whose top-level `error` holds anything but null or an empty
/// object reports a mid-stream failure.
fn carries_error(body: &Map<String, Value>) -> bool {
    match body.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(_) => true,
    }
}
