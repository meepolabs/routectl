//! Anthropic Messages: `message_start` opens the message, content blocks
//! fold by `index`, `message_delta` sets the stop fields and usage, and
//! `message_stop` closes it.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{
    Budget, Input, Step, index_field, insert, merge_field, object_at, replace_object, slot_at,
};

const ERROR_EVENT: &str = "error";

#[derive(Default)]
pub(super) struct AnthropicFold {
    message: Map<String, Value>,
    blocks: BTreeMap<u64, Block>,
}

#[derive(Default)]
struct Block {
    body: Map<String, Value>,
    partial_json: String,
}

impl Block {
    /// The non-streamed body carries a tool block's input as a parsed
    /// object; a buffer that does not parse is kept as its raw text so the
    /// difference stays visible. None when the parsed input would exceed
    /// the node cap.
    fn finish(mut self, budget: &mut Budget) -> Option<Value> {
        if !self.partial_json.is_empty() {
            let input = serde_json::from_str::<Value>(&self.partial_json)
                .unwrap_or(Value::String(self.partial_json));
            if !insert(budget, &mut self.body, "input".to_owned(), input) {
                return None;
            }
        }
        Some(Value::Object(self.body))
    }
}

/// Whether a frame of this kind reports a stream failure.
pub(super) fn is_failure(kind: Option<&str>) -> bool {
    kind == Some(ERROR_EVENT)
}

impl AnthropicFold {
    pub(super) fn apply(&mut self, input: Input, budget: &mut Budget) -> Step {
        let Input::Event { kind, mut body } = input else {
            return Step::Fold;
        };
        if is_failure(kind.as_deref()) {
            return Step::UpstreamError;
        }
        let kept = match kind.as_deref() {
            Some("message_start") => match body.remove("message") {
                Some(Value::Object(message)) => replace_object(budget, &mut self.message, message),
                _ => true,
            },
            Some("content_block_start") => self.start_block(&mut body, budget),
            Some("content_block_delta") => self.apply_block_delta(&mut body, budget),
            Some("message_delta") => self.apply_message_delta(&mut body, budget),
            Some("message_stop") => return Step::Close,
            _ => true,
        };
        if kept { Step::Fold } else { Step::OverBudget }
    }

    pub(super) fn finish(mut self, budget: &mut Budget) -> Option<Value> {
        let content = self
            .blocks
            .into_values()
            .map(|block| block.finish(budget))
            .collect::<Option<Vec<_>>>()?;
        self.message
            .insert("content".to_owned(), Value::Array(content));
        Some(Value::Object(self.message))
    }

    fn start_block(&mut self, body: &mut Map<String, Value>, budget: &mut Budget) -> bool {
        let index = index_field(body, "index").unwrap_or(0);
        let Some(block) = slot_at(budget, &mut self.blocks, index) else {
            return false;
        };
        block.partial_json.clear();
        let incoming = match body.remove("content_block") {
            Some(Value::Object(fields)) => fields,
            _ => Map::new(),
        };
        replace_object(budget, &mut block.body, incoming)
    }

    fn apply_block_delta(&mut self, body: &mut Map<String, Value>, budget: &mut Budget) -> bool {
        let index = index_field(body, "index").unwrap_or(0);
        let Some(Value::Object(mut delta)) = body.remove("delta") else {
            return true;
        };
        let Some(block) = slot_at(budget, &mut self.blocks, index) else {
            return false;
        };
        let delta_kind = delta.remove("type");
        match delta_kind.as_ref().and_then(Value::as_str) {
            Some("input_json_delta") => {
                if let Some(Value::String(part)) = delta.remove("partial_json") {
                    block.partial_json.push_str(&part);
                }
                true
            }
            Some("citations_delta") => delta.remove("citation").is_none_or(|citation| {
                merge_field(
                    budget,
                    &mut block.body,
                    "citations".to_owned(),
                    Value::Array(vec![citation]),
                )
            }),
            _ => delta
                .into_iter()
                .all(|(key, value)| merge_field(budget, &mut block.body, key, value)),
        }
    }

    fn apply_message_delta(&mut self, body: &mut Map<String, Value>, budget: &mut Budget) -> bool {
        if let Some(Value::Object(delta)) = body.remove("delta")
            && !delta
                .into_iter()
                .all(|(key, value)| insert(budget, &mut self.message, key, value))
        {
            return false;
        }
        let Some(Value::Object(usage)) = body.remove("usage") else {
            return true;
        };
        let Some(slot) = object_at(budget, &mut self.message, "usage") else {
            return false;
        };
        usage
            .into_iter()
            .all(|(key, value)| insert(budget, slot, key, value))
    }
}
