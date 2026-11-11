//! Event-by-event carriage of what the canonical chunk cannot hold, for a
//! client that speaks the Responses dialect: a stream event of a type the
//! parser does not model, and the unmodeled members of an event it does.
//!
//! Each rides on the first chunk the event produced, or on a carrier chunk
//! of its own when it produced none, never on a later event's chunk, so the
//! ingress re-emits it in upstream order. The ingress re-stamps the members
//! it manages (`sequence_number`, `output_index`, `item_id`).
//!
//! Every `output_item.added` is carried, members or not, with the identity
//! of its item under `item`: the ingress opens its own items lazily under
//! its own indexes, and this is how it ties an upstream `output_index` to
//! the item it synthesized for it.

use serde_json::{Map, Value};

use routectl_core::wire_check::IngressDialect;
use routectl_core::{ChatChunk, OpaqueSseEvent, UpstreamMeta};

use super::ResponsesStreamState;
use crate::openai_responses::response_types::ResponsesStreamEvent;

/// The stream event types whose content the canonical chunk carries, or
/// the Responses render rebuilds from it: the parser's own events, and the
/// part and done events the render synthesizes for every item. Only their
/// unmodeled members are carried; an event of any other type is carried
/// whole.
pub(super) const KNOWN_EVENT_TYPES: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.output_item.done",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.custom_tool_call_input.delta",
    "response.custom_tool_call_input.done",
    "response.completed",
    "response.incomplete",
    "response.failed",
    "response.cancelled",
];

/// Event types never carried: the stream's error and terminal handling
/// stays the parser's.
const NEVER_CARRIED: &[&str] = &["error"];

/// Members of a modeled event the Responses ingress writes itself, or
/// whose content reaches it through the canonical chunk.
pub(super) const MANAGED_EVENT_KEYS: &[&str] = &[
    "type",
    "sequence_number",
    "output_index",
    "item_id",
    "content_index",
    "summary_index",
    "call_id",
    "delta",
    "text",
    "arguments",
    "item",
    "part",
    "response",
];

const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";

/// The key under which a carried `output_item.added` names its item.
pub(super) const ITEM_IDENTITY_KEY: &str = "item";

/// Whether the parser or the Responses render models events of
/// `event_type`.
pub(super) fn is_known_event_type(event_type: &str) -> bool {
    KNOWN_EVENT_TYPES.contains(&event_type)
}

/// Whether an event of `event_type` rides whole to a Responses client.
pub(super) fn is_carried_whole(event_type: &str) -> bool {
    !is_known_event_type(event_type) && !NEVER_CARRIED.contains(&event_type)
}

/// The members of `item` that identify it to the ingress: its `type`, and
/// its `id` and `call_id` when it has them.
fn item_identity(item: &Value) -> Value {
    let identity: Map<String, Value> = ["type", "id", "call_id"]
        .into_iter()
        .filter_map(|key| {
            item.get(key)
                .filter(|value| value.is_string())
                .map(|value| (key.to_owned(), value.clone()))
        })
        .collect();
    Value::Object(identity)
}

impl ResponsesStreamState {
    /// What `event` carries beyond the canonical chunk, for a client of
    /// this dialect; nothing for any other client.
    pub(super) fn event_carriage(
        &self,
        kind: &str,
        event: &ResponsesStreamEvent,
    ) -> Option<OpaqueSseEvent> {
        if !self.carries_unmodeled() || NEVER_CARRIED.contains(&kind) {
            return None;
        }
        let Ok(Value::Object(mut members)) = serde_json::from_str::<Value>(event.raw.as_deref()?)
        else {
            return None;
        };
        if is_carried_whole(kind) {
            return Some(OpaqueSseEvent::UnknownEvent {
                event_type: kind.to_owned(),
                event: Value::Object(members),
            });
        }
        members.retain(|key, _| !MANAGED_EVENT_KEYS.contains(&key.as_str()));
        if kind == OUTPUT_ITEM_ADDED
            && let Some(item) = event.item.as_ref()
        {
            members.insert(ITEM_IDENTITY_KEY.to_owned(), item_identity(item));
        }
        (!members.is_empty()).then(|| OpaqueSseEvent::KnownEventExtras {
            event_type: kind.to_owned(),
            upstream_index: event.output_index,
            members,
        })
    }

    /// `chunks` with `carried` on the first of them, or on a carrier chunk
    /// of its own when the event produced none. The chunk is stamped as
    /// parsed from a Responses body, so only a Responses render emits it.
    pub(super) fn attach_carriage(
        &self,
        mut chunks: Vec<ChatChunk>,
        carried: Option<OpaqueSseEvent>,
    ) -> Vec<ChatChunk> {
        let Some(carried) = carried else {
            return chunks;
        };
        let stamp = UpstreamMeta::default().with_wire_dialect(IngressDialect::Responses);
        if chunks.is_empty() {
            chunks.push(self.carrier_chunk(stamp));
        } else if let Some(first) = chunks.first_mut() {
            first.upstream_meta = Some(match first.upstream_meta.take() {
                Some(meta) => meta.merge(stamp),
                None => stamp,
            });
        }
        if let Some(first) = chunks.first_mut() {
            first.opaque_events.insert(0, carried);
        }
        chunks
    }

    /// Whether the client speaks this dialect, so the events and members
    /// the canonical chunk has no slot for reach it.
    pub(super) fn carries_unmodeled(&self) -> bool {
        crate::openai_responses::client_carries_unmodeled_items(self.provenance)
    }
}
