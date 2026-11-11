//! Capture of unmodeled message members before a canonical parse.
//!
//! [`Message`](crate::Message) keeps its derived `Deserialize`, which
//! ignores members it does not model. An ingress that wants those members
//! for a same-dialect egress calls [`split_message_extras`] on the raw
//! `messages` array first, then parses, then stores each returned map in
//! the matching message's `extras`.

use serde_json::{Map, Value};

use crate::member_slots::take_members_outside;

/// Wire member names [`Message`](crate::Message) models. Every other member
/// of a message object is an extra.
pub const MESSAGE_MEMBERS: [&str; 8] = [
    "role",
    "content",
    "reasoning",
    "reasoning_details",
    "name",
    "tool_call_id",
    "tool_calls",
    "refusal",
];

/// Remove every member a canonical message does not model from each object
/// in `messages`, returning them in message order (one map per element,
/// so index `i` belongs to `messages[i]`). A non-object element yields an
/// empty map and is left for the parse to reject. A message with no such
/// member allocates nothing.
///
/// Run it after any ingress rewrite that folds a dialect spelling into a
/// modeled member, so the folded member is not taken as an extra.
pub fn split_message_extras(messages: &mut [Value]) -> Vec<Map<String, Value>> {
    messages
        .iter_mut()
        .map(|message| match message {
            Value::Object(obj) => take_members_outside(obj, &MESSAGE_MEMBERS),
            _ => Map::new(),
        })
        .collect()
}

#[cfg(test)]
#[path = "message_extras_tests.rs"]
mod tests;
