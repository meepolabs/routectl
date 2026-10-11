//! Anthropic Messages: `message_start` opens the message, content blocks
//! fold by `index`, `message_delta` sets the stop fields and usage, and
//! `message_stop` closes it.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Input, Step, index_field, merge_field};

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
    /// difference stays visible.
    fn finish(mut self) -> Value {
        if !self.partial_json.is_empty() {
            let input = serde_json::from_str::<Value>(&self.partial_json)
                .unwrap_or(Value::String(self.partial_json));
            self.body.insert("input".to_owned(), input);
        }
        Value::Object(self.body)
    }
}

impl AnthropicFold {
    pub(super) fn apply(&mut self, input: Input) -> Step {
        let Input::Event { kind, mut body } = input else {
            return Step::Fold;
        };
        match kind.as_deref() {
            Some("message_start") => {
                if let Some(Value::Object(message)) = body.remove("message") {
                    self.message = message;
                }
                Step::Fold
            }
            Some("content_block_start") => {
                self.start_block(&mut body);
                Step::Fold
            }
            Some("content_block_delta") => {
                self.apply_block_delta(&mut body);
                Step::Fold
            }
            Some("message_delta") => {
                self.apply_message_delta(&mut body);
                Step::Fold
            }
            Some("message_stop") => Step::Close,
            Some("error") => Step::UpstreamError,
            _ => Step::Fold,
        }
    }

    pub(super) fn finish(mut self) -> Value {
        let content = self.blocks.into_values().map(Block::finish).collect();
        self.message
            .insert("content".to_owned(), Value::Array(content));
        Value::Object(self.message)
    }

    fn start_block(&mut self, body: &mut Map<String, Value>) {
        let index = index_field(body, "index").unwrap_or(0);
        let block_body = match body.remove("content_block") {
            Some(Value::Object(block)) => block,
            _ => Map::new(),
        };
        self.blocks.insert(
            index,
            Block {
                body: block_body,
                partial_json: String::new(),
            },
        );
    }

    fn apply_block_delta(&mut self, body: &mut Map<String, Value>) {
        let index = index_field(body, "index").unwrap_or(0);
        let Some(Value::Object(mut delta)) = body.remove("delta") else {
            return;
        };
        let block = self.blocks.entry(index).or_default();
        let delta_kind = delta.remove("type");
        match delta_kind.as_ref().and_then(Value::as_str) {
            Some("input_json_delta") => {
                if let Some(Value::String(part)) = delta.remove("partial_json") {
                    block.partial_json.push_str(&part);
                }
            }
            Some("citations_delta") => {
                if let Some(citation) = delta.remove("citation") {
                    merge_field(
                        &mut block.body,
                        "citations".to_owned(),
                        Value::Array(vec![citation]),
                    );
                }
            }
            _ => {
                for (key, value) in delta {
                    merge_field(&mut block.body, key, value);
                }
            }
        }
    }

    fn apply_message_delta(&mut self, body: &mut Map<String, Value>) {
        if let Some(Value::Object(delta)) = body.remove("delta") {
            self.message.extend(delta);
        }
        if let Some(Value::Object(usage)) = body.remove("usage") {
            let slot = self
                .message
                .entry("usage")
                .or_insert_with(|| Value::Object(Map::new()));
            match slot {
                Value::Object(existing) => existing.extend(usage),
                other => *other = Value::Object(usage),
            }
        }
    }
}
