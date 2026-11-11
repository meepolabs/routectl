//! Emission of carried members onto the openai-compat wire body.
//!
//! Canonical serialization skips every node's `extras`, so the serialized
//! `messages` array this egress starts from holds typed members only. When
//! the request speaks this wire's dialect, [`emit`] merges each message's and
//! each typed content part's carried members back onto the slot that node
//! serialized to. Typed and routectl-managed names stay reserved even when
//! the typed value is absent, and a member the slot already has is never
//! replaced. A request with no carried member is left byte-for-byte as
//! serialized.

use serde_json::{Map, Value};

use routectl_core::wire_check::{EgressKind, forwards_unknowns};
use routectl_core::{ChatRequest, ContentPart, Message, MessageContent};

/// Message members a reasoning dialect writes itself, reserved beside the
/// members `Message` types.
const DIALECT_MESSAGE_KEYS: [&str; 1] = ["reasoning_content"];

/// Whether `req`'s carried members may reach this wire: a library caller's,
/// or a Chat Completions client's. The Mantle lane sends the same dialect,
/// so the direct kind decides for both.
pub(super) fn emits_carriers(req: &ChatRequest) -> bool {
    forwards_unknowns(req.routectl_internal.provenance, EgressKind::OpenaiCompat)
}

/// Merge the carried members of each of `messages` onto its serialized slot
/// in `slots`, the array `messages` serialized to, element for element.
pub(super) fn emit(slots: &mut Value, messages: &[Message]) {
    let Some(slots) = slots.as_array_mut() else {
        return;
    };
    for (slot, message) in slots.iter_mut().zip(messages) {
        if let Some(members) = slot.as_object_mut() {
            emit_message(members, message);
        }
    }
}

fn emit_message(members: &mut Map<String, Value>, message: &Message) {
    merge_members(
        members,
        message
            .forwardable_extras()
            .filter(|(key, _)| !DIALECT_MESSAGE_KEYS.contains(&key.as_str())),
    );
    let (MessageContent::Parts(parts), Some(Value::Array(part_slots))) =
        (&message.content, members.get_mut("content"))
    else {
        return;
    };
    for (part_slot, part) in part_slots.iter_mut().zip(parts) {
        if let (ContentPart::Known(known), Some(part_members)) = (part, part_slot.as_object_mut()) {
            merge_members(part_members, known.forwardable_extras());
        }
    }
}

fn merge_members<'a>(
    target: &mut Map<String, Value>,
    members: impl Iterator<Item = (&'a String, &'a Value)>,
) {
    for (key, value) in members {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

#[cfg(test)]
#[path = "carriers_tests.rs"]
mod tests;
