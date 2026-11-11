//! Whether the unmodeled members a canonical node carries reach the
//! Anthropic-shaped wire with it, and which of them.
//!
//! The decision is made once per request by the core same-dialect predicate
//! and the member set per node comes from that node's `forwardable_extras`,
//! the rule the request-identity projection merges by: typed and
//! routectl-managed names stay reserved even when the typed value is absent.

use serde_json::{Map, Value};

use routectl_core::wire_check::{EgressKind, forwards_unknowns};
use routectl_core::{ChatRequest, CustomTool, KnownContentPart, Message, SystemBlock};

/// Whether carried members are emitted on this request's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carriers {
    /// The request's dialect is the one this body speaks: emit them.
    Emit,
    /// A request from another dialect: its members mean nothing here.
    Withhold,
}

impl Carriers {
    /// The policy for `req`. anthropic-api, Bedrock Mantle and Bedrock
    /// Invoke share one assembly and send one wire dialect, so the direct
    /// kind decides for all three.
    pub fn for_request(req: &ChatRequest) -> Self {
        if forwards_unknowns(req.routectl_internal.provenance, EgressKind::AnthropicApi) {
            Self::Emit
        } else {
            Self::Withhold
        }
    }

    fn collect<'a>(
        self,
        members: impl Iterator<Item = (&'a String, &'a Value)>,
    ) -> Map<String, Value> {
        match self {
            Self::Emit => members.map(|(k, v)| (k.clone(), v.clone())).collect(),
            Self::Withhold => Map::new(),
        }
    }

    /// The members `message` emits on its wire message.
    pub fn message(self, message: &Message) -> Map<String, Value> {
        self.collect(message.forwardable_extras())
    }

    /// The members `part` emits on its wire block.
    pub fn part(self, part: &KnownContentPart) -> Map<String, Value> {
        self.collect(part.forwardable_extras())
    }

    /// The members `tool` emits on its wire tool.
    pub fn custom_tool(self, tool: &CustomTool) -> Map<String, Value> {
        self.collect(tool.forwardable_extras())
    }

    /// The members `block` emits on its wire system block.
    pub fn system_block(self, block: &SystemBlock) -> Map<String, Value> {
        self.collect(block.forwardable_extras())
    }

    /// The members of one wire message built from the consecutive canonical
    /// messages `run`, the first at canonical index `first`: their union,
    /// the earlier message winning a name both carry. Returns the union and
    /// the client-coordinate path of each later member it replaced.
    pub fn joined_messages(
        self,
        run: &[&Message],
        first: usize,
    ) -> (Map<String, Value>, Vec<(usize, String)>) {
        let mut union = Map::new();
        let mut replaced = Vec::new();
        if self == Self::Withhold {
            return (union, replaced);
        }
        for (offset, message) in run.iter().enumerate() {
            for (key, value) in message.forwardable_extras() {
                match union.get(key) {
                    None => {
                        union.insert(key.clone(), value.clone());
                    }
                    Some(kept) if kept != value => replaced.push((first + offset, key.clone())),
                    Some(_) => {}
                }
            }
        }
        (union, replaced)
    }
}

#[cfg(test)]
#[path = "carriers_tests.rs"]
mod tests;
