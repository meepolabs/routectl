//! Extras-inclusive identity of canonical nodes, for hashing and comparison.
//!
//! Canonical serde skips every node's `extras` carrier, so a fingerprint or
//! an equality check over canonical serialization cannot see a change to
//! carried members alone. This projection serializes a node with its
//! carriers merged the way a same-dialect egress emits them: typed keys win,
//! and an extras key that names a typed or managed field is skipped even
//! when the typed value is absent.
//!
//! The output is an identity, never a wire form: a translation lane must not
//! see carried members, so nothing here hands out a serializable value. The
//! only outputs are bytes ([`projection_bytes`]) or a write into a sink
//! ([`write_projection`]).
//!
//! A node with no mergeable carrier projects to exactly its canonical
//! serialization, so the identity of carrier-free traffic does not change.
//! A node with one projects through a `Value`, whose object keys are sorted.

use std::io;

use serde::ser::Error as _;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::content_part_de::Shape;
use crate::message_extras::MESSAGE_MEMBERS;
use crate::system_content::SYSTEM_BLOCK_MEMBERS;
use crate::tool_def::CUSTOM_TOOL_MEMBERS;
use crate::{
    ContentPart, CustomTool, KnownContentPart, Message, MessageContent, SystemBlock, SystemContent,
    ToolDef,
};

/// Every member name `Message` serializes.
const MESSAGE_KEYS: &[&str] = &MESSAGE_MEMBERS;

/// Every member name `SystemBlock` serializes.
const SYSTEM_BLOCK_KEYS: &[&str] = &SYSTEM_BLOCK_MEMBERS;

/// Every member name `CustomTool` serializes, plus the `inputSchema` spelling
/// it accepts for `input_schema`.
const CUSTOM_TOOL_KEYS: &[&str] = &CUSTOM_TOOL_MEMBERS;

/// Names reserved on every typed content part: the tag, and the cache marker
/// routectl manages even on the variants that carry none.
const PART_MANAGED_KEYS: &[&str] = &["type", "cache_control"];

/// The member names `part`'s variant serializes, the `type` tag excluded.
fn part_typed_keys(part: &KnownContentPart) -> impl Iterator<Item = &'static str> {
    Shape::of_part(part).typed_keys()
}

const fn part_extras(part: &KnownContentPart) -> &Map<String, Value> {
    match part {
        KnownContentPart::Text { extras, .. }
        | KnownContentPart::Image { extras, .. }
        | KnownContentPart::ImageUrl { extras, .. }
        | KnownContentPart::File { extras, .. }
        | KnownContentPart::Document { extras, .. }
        | KnownContentPart::ToolUse { extras, .. }
        | KnownContentPart::ToolResult { extras, .. }
        | KnownContentPart::Thinking { extras, .. }
        | KnownContentPart::RedactedThinking { extras, .. } => extras,
    }
}

fn is_part_reserved(part: &KnownContentPart, key: &str) -> bool {
    PART_MANAGED_KEYS.contains(&key) || part_typed_keys(part).any(|typed| typed == key)
}

fn has_mergeable(extras: &Map<String, Value>, reserved: &[&str]) -> bool {
    extras.keys().any(|key| !reserved.contains(&key.as_str()))
}

fn part_has_mergeable(part: &KnownContentPart) -> bool {
    part_extras(part)
        .keys()
        .any(|key| !is_part_reserved(part, key))
}

fn message_has_mergeable(message: &Message) -> bool {
    has_mergeable(&message.extras, MESSAGE_KEYS)
        || matches!(&message.content, MessageContent::Parts(parts) if parts.iter().any(
            |part| matches!(part, ContentPart::Known(known) if part_has_mergeable(known))
        ))
}

/// Add each carried member of `extras` that `is_reserved` does not claim to
/// `target`, never replacing a member `target` already has.
fn merge_carried(
    target: &mut Value,
    extras: &Map<String, Value>,
    is_reserved: impl Fn(&str) -> bool,
) {
    let Value::Object(members) = target else {
        return;
    };
    for (key, value) in extras {
        if !is_reserved(key) {
            members.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

fn merge_part(slot: &mut Value, part: &ContentPart) {
    if let ContentPart::Known(known) = part {
        merge_carried(slot, part_extras(known), |key| is_part_reserved(known, key));
    }
}

/// Serialize `node` canonically, then merge its carriers via `merge`.
fn project_through_value<T: Serialize, S: Serializer>(
    node: &T,
    serializer: S,
    merge: impl FnOnce(&mut Value),
) -> Result<S::Ok, S::Error> {
    let mut value = serde_json::to_value(node).map_err(S::Error::custom)?;
    merge(&mut value);
    value.serialize(serializer)
}

mod sealed {
    pub trait Sealed {
        fn project<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error>;
    }
}

/// A canonical node, node sequence or request prefix that has a carrier
/// projection. Sealed: the set of projectable shapes is fixed here.
pub trait Projectable: sealed::Sealed {}

/// A canonical request prefix: system, tools, then messages. Projects as the
/// three-element array `[system, tools, messages]`, an absent system or tool
/// list as `null`.
#[derive(Debug, Clone, Copy)]
pub struct RequestPrefix<'a> {
    /// The request's system content.
    pub system: Option<&'a SystemContent>,
    /// The request's tool list.
    pub tools: Option<&'a [ToolDef]>,
    /// The messages the prefix covers.
    pub messages: &'a [Message],
}

struct Projected<'a, T: ?Sized>(&'a T);

impl<T: Projectable + ?Sized> Serialize for Projected<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.project(serializer)
    }
}

/// Write the projection of `node` into `writer` as compact JSON.
pub fn write_projection<W: io::Write, T: Projectable + ?Sized>(
    writer: W,
    node: &T,
) -> serde_json::Result<()> {
    serde_json::to_writer(writer, &Projected(node))
}

/// The projection of `node` as compact JSON bytes.
pub fn projection_bytes<T: Projectable + ?Sized>(node: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&Projected(node))
}

impl sealed::Sealed for Message {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !message_has_mergeable(self) {
            return self.serialize(serializer);
        }
        project_through_value(self, serializer, |value| {
            merge_carried(value, &self.extras, |key| MESSAGE_KEYS.contains(&key));
            if let (MessageContent::Parts(parts), Some(Value::Array(slots))) =
                (&self.content, value.get_mut("content"))
            {
                for (slot, part) in slots.iter_mut().zip(parts) {
                    merge_part(slot, part);
                }
            }
        })
    }
}
impl Projectable for Message {}

impl sealed::Sealed for [Message] {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter().map(Projected))
    }
}
impl Projectable for [Message] {}

impl sealed::Sealed for KnownContentPart {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !part_has_mergeable(self) {
            return self.serialize(serializer);
        }
        project_through_value(self, serializer, |value| {
            merge_carried(value, part_extras(self), |key| is_part_reserved(self, key));
        })
    }
}
impl Projectable for KnownContentPart {}

impl sealed::Sealed for ContentPart {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(known) => known.project(serializer),
            Self::Other { .. } => self.serialize(serializer),
        }
    }
}
impl Projectable for ContentPart {}

impl sealed::Sealed for SystemBlock {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !has_mergeable(&self.extras, SYSTEM_BLOCK_KEYS) {
            return self.serialize(serializer);
        }
        project_through_value(self, serializer, |value| {
            merge_carried(value, &self.extras, |key| SYSTEM_BLOCK_KEYS.contains(&key));
        })
    }
}
impl Projectable for SystemBlock {}

impl sealed::Sealed for SystemContent {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(_) => self.serialize(serializer),
            Self::Blocks(blocks) => serializer.collect_seq(blocks.iter().map(Projected)),
        }
    }
}
impl Projectable for SystemContent {}

impl sealed::Sealed for CustomTool {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !has_mergeable(&self.extras, CUSTOM_TOOL_KEYS) {
            return self.serialize(serializer);
        }
        project_through_value(self, serializer, |value| {
            merge_carried(value, &self.extras, |key| CUSTOM_TOOL_KEYS.contains(&key));
        })
    }
}
impl Projectable for CustomTool {}

impl sealed::Sealed for ToolDef {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Custom(tool) => tool.project(serializer),
            Self::Other(_) => self.serialize(serializer),
        }
    }
}
impl Projectable for ToolDef {}

impl sealed::Sealed for [ToolDef] {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter().map(Projected))
    }
}
impl Projectable for [ToolDef] {}

impl sealed::Sealed for RequestPrefix<'_> {
    fn project<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (
            self.system.map(Projected),
            self.tools.map(Projected),
            Projected(self.messages),
        )
            .serialize(serializer)
    }
}
impl Projectable for RequestPrefix<'_> {}

#[cfg(test)]
#[path = "carrier_projection_tests.rs"]
mod tests;
