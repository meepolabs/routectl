//! Re-emission of what a Responses upstream sent beyond the canonical
//! chunk: stream events of a type no canonical field models, and the
//! unmodeled members of an event that is modeled. The openai-responses
//! egress carries each on the chunk of the event that carried it, only for
//! a client of this dialect.
//!
//! This render synthesizes its own output items under its own dense
//! `output_index`, so an upstream event cannot be forwarded as sent. The
//! egress carries every `output_item.added` with its item's identity, which
//! ties the upstream index to the item this render opens for it. A carried
//! event is re-stamped (`sequence_number`, `output_index`, `item_id`) and
//! emitted in order: at once when its item is open, otherwise right after
//! this render opens that item. Members of a modeled event merge into the
//! render's own event of that type for the same item, the render's members
//! winning; several upstream events this render emits as one (a buffered
//! call's argument deltas) merge first-wins.
//!
//! An event naming an item this render cannot place (no carried identity,
//! an `item_id` the render's item does not carry, an item never opened by
//! the end of the stream), or one past the held caps, is withheld: its
//! members are declared removed and the stream counts once.

use serde_json::{Map, Value};

use routectl_core::wire_check::{EditKind, EditSite, WirePath};
use routectl_core::{OpaqueSseEvent, is_safe_token};
use routectl_providers::translation_drop_metrics::record_translation_drop;

use crate::ingress::{ResponseEdits, SseEvent};

use super::ResponsesStreamState;

/// Census lane the withheld events are counted on.
const LANE: &str = routectl_core::provider_kind::trace::OPENAI_RESPONSES;

/// Drop class for upstream events and members the render cannot place.
const UNKNOWN_MEMBER_WITHHELD: &str = "unknown_member_withheld";

const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
const OUTPUT_INDEX: &str = "output_index";
const ITEM_ID: &str = "item_id";
const ITEM: &str = "item";
const FUNCTION_CALL: &str = "function_call";

/// Members a carried event's declaration skips: those this render
/// re-stamps on every event it emits.
const RESTAMPED_KEYS: &[&str] = routectl_providers::openai_responses::EVENT_IDENTITY_MEMBERS;

/// Most carried events held for an item this render has not opened yet.
const MAX_HELD_EVENTS: usize = 1024;

/// Most bytes of carried events held for unopened items.
const MAX_HELD_BYTES: usize = 4 * 1024 * 1024;

/// Most upstream items whose identity is tracked.
const MAX_TRACKED_ITEMS: usize = 4096;

/// The identity an output item is emitted under: its `type`, with the
/// `call_id` of a function call or the `id` of any other item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ItemKey {
    kind: String,
    id: String,
}

impl ItemKey {
    /// The key of `item`, an output item as either side emits it.
    pub(super) fn of(item: &Value) -> Self {
        let field = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let kind = field("type");
        let id = if kind == FUNCTION_CALL {
            field("call_id")
        } else {
            field("id")
        };
        Self { kind, id }
    }
}

/// An upstream item, by its upstream `output_index`.
#[derive(Debug)]
struct UpstreamItem {
    key: ItemKey,
    item_id: Option<String>,
}

/// An item this render opened, with where and under what id.
#[derive(Debug)]
struct OpenedItem {
    key: ItemKey,
    output_index: u64,
    item_id: Option<String>,
}

/// A carried event or member set waiting for this render.
#[derive(Debug)]
enum Held {
    /// An event for an item not opened yet.
    Event {
        key: ItemKey,
        event_type: String,
        event: Map<String, Value>,
    },
    /// Members of a modeled event, for the render's own event of that type
    /// on the same item (`None`: an event naming no item).
    Members {
        event_type: String,
        key: Option<ItemKey>,
        members: Map<String, Value>,
    },
}

/// Per-stream carriage state.
#[derive(Debug, Default)]
pub(super) struct CarriedState {
    upstream: std::collections::BTreeMap<u32, UpstreamItem>,
    opened: Vec<OpenedItem>,
    held: Vec<Held>,
    held_bytes: usize,
    withheld_counted: bool,
}

/// Where a carried event belongs.
enum Placement {
    /// The stream as a whole: it names no item.
    Stream,
    /// The item with this key.
    Item(ItemKey),
    /// An item this render cannot place.
    Unmapped,
}

/// Take in one carried event at its position in the chunk. A modeled
/// event's members are held for the render's own event; an unknown event
/// is emitted, held for its item, or withheld.
pub(super) fn replay_carried(
    event: &OpaqueSseEvent,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    edits: &mut ResponseEdits,
) {
    match event {
        OpaqueSseEvent::KnownEventExtras {
            event_type,
            upstream_index,
            members,
        } => hold_event_members(event_type, *upstream_index, members, state, edits),
        OpaqueSseEvent::UnknownEvent { event_type, event } => {
            replay_unknown_event(event_type, event, state, events, edits);
        }
        _ => {}
    }
}

fn hold_event_members(
    event_type: &str,
    upstream_index: Option<u32>,
    members: &Map<String, Value>,
    state: &mut ResponsesStreamState,
    edits: &mut ResponseEdits,
) {
    let mut members = members.clone();
    if event_type == OUTPUT_ITEM_ADDED
        && let (Some(index), Some(item)) = (upstream_index, members.remove(ITEM))
    {
        track_upstream_item(&mut state.carried, index, &item);
    }
    if members.is_empty() {
        return;
    }
    let key = match upstream_index {
        None => None,
        Some(index) => match state.carried.upstream.get(&index) {
            Some(item) => Some(item.key.clone()),
            None => {
                withhold(&mut state.carried, upstream_index, &members, edits);
                return;
            }
        },
    };
    hold(
        &mut state.carried,
        Held::Members {
            event_type: event_type.to_owned(),
            key,
            members,
        },
        upstream_index,
        edits,
    );
}

fn track_upstream_item(carried: &mut CarriedState, index: u32, item: &Value) {
    if carried.upstream.len() >= MAX_TRACKED_ITEMS && !carried.upstream.contains_key(&index) {
        return;
    }
    let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
    carried.upstream.insert(
        index,
        UpstreamItem {
            key: ItemKey::of(item),
            item_id,
        },
    );
}

fn replay_unknown_event(
    event_type: &str,
    event: &Value,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    edits: &mut ResponseEdits,
) {
    let Value::Object(body) = event else {
        return;
    };
    let upstream_index = upstream_index_of(body);
    if !is_safe_token(event_type) {
        tracing::debug!("openai-responses ingress: unknown upstream event with an unusable name");
        withhold(&mut state.carried, upstream_index, body, edits);
        return;
    }
    match placement(&state.carried, body) {
        Placement::Stream => super::stream::push_event(state, events, event_type, event.clone()),
        Placement::Item(key) => {
            if state.carried.opened.iter().any(|item| item.key == key) {
                emit_for_item(event_type, body.clone(), &key, state, events, edits);
            } else {
                let held = Held::Event {
                    key,
                    event_type: event_type.to_owned(),
                    event: body.clone(),
                };
                hold(&mut state.carried, held, upstream_index, edits);
            }
        }
        Placement::Unmapped => withhold(&mut state.carried, upstream_index, body, edits),
    }
}

fn upstream_index_of(body: &Map<String, Value>) -> Option<u32> {
    body.get(OUTPUT_INDEX)
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
}

fn placement(carried: &CarriedState, body: &Map<String, Value>) -> Placement {
    if body.contains_key(OUTPUT_INDEX) {
        return match upstream_index_of(body).and_then(|index| carried.upstream.get(&index)) {
            Some(item) => Placement::Item(item.key.clone()),
            None => Placement::Unmapped,
        };
    }
    match body.get(ITEM_ID) {
        None => Placement::Stream,
        Some(Value::String(id)) => carried
            .upstream
            .values()
            .find(|item| item.item_id.as_deref() == Some(id.as_str()))
            .map_or(Placement::Unmapped, |item| {
                Placement::Item(item.key.clone())
            }),
        Some(_) => Placement::Unmapped,
    }
}

/// Emit `body` for the opened item `key`, its index and id re-stamped to
/// this render's own.
fn emit_for_item(
    event_type: &str,
    mut body: Map<String, Value>,
    key: &ItemKey,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    edits: &mut ResponseEdits,
) {
    let Some(opened) = state.carried.opened.iter().find(|item| &item.key == key) else {
        return;
    };
    if body.contains_key(ITEM_ID) {
        let Some(item_id) = opened.item_id.clone() else {
            withhold(&mut state.carried, upstream_index_of(&body), &body, edits);
            return;
        };
        body.insert(ITEM_ID.to_owned(), Value::String(item_id));
    }
    if body.contains_key(OUTPUT_INDEX) {
        body.insert(OUTPUT_INDEX.to_owned(), Value::from(opened.output_index));
    }
    super::stream::push_event(state, events, event_type, Value::Object(body));
}

/// Record that this render opens `item` at `output_index`. Called before
/// the item's own `output_item.added` goes out, so members carried for that
/// event find it.
pub(super) fn note_opened(item: &Value, output_index: u64, carried: &mut CarriedState) {
    let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
    carried.opened.push(OpenedItem {
        key: ItemKey::of(item),
        output_index,
        item_id,
    });
}

/// Emit every event held for `item`, which this render just opened. Called
/// right after the item's opening events.
pub(super) fn replay_held(
    item: &Value,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    edits: &mut ResponseEdits,
) {
    let key = ItemKey::of(item);
    let held = std::mem::take(&mut state.carried.held);
    for entry in held {
        match entry {
            Held::Event {
                key: held_key,
                event_type,
                event,
            } if held_key == key => {
                state.carried.held_bytes = state
                    .carried
                    .held_bytes
                    .saturating_sub(serialized_len(&event));
                emit_for_item(&event_type, event, &key, state, events, edits);
            }
            other => state.carried.held.push(other),
        }
    }
}

/// Merge into `payload`, the render's own `event_name` event, every held
/// member set carried for that event type on the same item. The render's
/// members win; among the carried, the first held wins.
pub(super) fn merge_held_members(
    carried: &mut CarriedState,
    event_name: &str,
    payload: &mut Map<String, Value>,
) {
    if carried.held.is_empty() {
        return;
    }
    let output_index = payload.get(OUTPUT_INDEX).and_then(Value::as_u64);
    let held = std::mem::take(&mut carried.held);
    for entry in held {
        match entry {
            Held::Members {
                event_type,
                key,
                members,
            } if event_type == event_name && same_target(carried, key.as_ref(), output_index) => {
                carried.held_bytes = carried.held_bytes.saturating_sub(serialized_len(&members));
                for (name, value) in members {
                    payload.entry(name).or_insert(value);
                }
            }
            other => carried.held.push(other),
        }
    }
}

/// Whether members held for the item `key` (`None`: the stream as a
/// whole) belong on a render event at `output_index`.
fn same_target(carried: &CarriedState, key: Option<&ItemKey>, output_index: Option<u64>) -> bool {
    match (key, output_index) {
        (None, None) => true,
        (Some(key), Some(index)) => opened_index(carried, key) == Some(index),
        _ => false,
    }
}

fn opened_index(carried: &CarriedState, key: &ItemKey) -> Option<u64> {
    carried
        .opened
        .iter()
        .find(|item| &item.key == key)
        .map(|item| item.output_index)
}

/// Withhold everything still held when the stream ends: its item was never
/// opened, or the render never emitted the event its members were for.
pub(super) fn withhold_unplaced(carried: &mut CarriedState, edits: &mut ResponseEdits) {
    for entry in std::mem::take(&mut carried.held) {
        let (key, members) = match entry {
            Held::Event { key, event, .. } => (Some(key), event),
            Held::Members { key, members, .. } => (key, members),
        };
        let upstream_index = key.and_then(|key| {
            carried
                .upstream
                .iter()
                .find(|(_, item)| item.key == key)
                .map(|(index, _)| *index)
        });
        withhold(carried, upstream_index, &members, edits);
    }
    carried.held_bytes = 0;
}

fn hold(
    carried: &mut CarriedState,
    held: Held,
    upstream_index: Option<u32>,
    edits: &mut ResponseEdits,
) {
    let held = match fold_into_held(carried, held) {
        Some(held) => held,
        None => return,
    };
    let (Held::Event { event: body, .. } | Held::Members { members: body, .. }) = &held;
    let size = serialized_len(body);
    let held_bytes = carried.held_bytes.saturating_add(size);
    if carried.held.len() >= MAX_HELD_EVENTS || held_bytes > MAX_HELD_BYTES {
        let body = body.clone();
        withhold(carried, upstream_index, &body, edits);
        return;
    }
    carried.held_bytes = held_bytes;
    carried.held.push(held);
}

/// Fold a member set into one already held for the same event type and
/// item, the first held value winning: the render emits one event of a type
/// per item where the upstream may send many (a buffered call's argument
/// deltas). Returns the set when nothing matched.
fn fold_into_held(carried: &mut CarriedState, held: Held) -> Option<Held> {
    let Held::Members {
        event_type,
        key,
        members,
    } = held
    else {
        return Some(held);
    };
    let existing = carried.held.iter_mut().find_map(|entry| match entry {
        Held::Members {
            event_type: held_type,
            key: held_key,
            members: held_members,
        } if *held_type == event_type && *held_key == key => Some(held_members),
        _ => None,
    });
    let Some(existing) = existing else {
        return Some(Held::Members {
            event_type,
            key,
            members,
        });
    };
    let added: Map<String, Value> = members
        .into_iter()
        .filter(|(name, _)| !existing.contains_key(name))
        .collect();
    let held_bytes = carried.held_bytes.saturating_add(serialized_len(&added));
    if held_bytes > MAX_HELD_BYTES {
        return Some(Held::Members {
            event_type,
            key,
            members: added,
        });
    }
    existing.extend(added);
    carried.held_bytes = held_bytes;
    None
}

/// Withhold one carried event or member set: declare each of its members
/// removed at the item it named, and count the stream once.
fn withhold(
    carried: &mut CarriedState,
    upstream_index: Option<u32>,
    members: &Map<String, Value>,
    edits: &mut ResponseEdits,
) {
    let base = upstream_index
        .and_then(|index| usize::try_from(index).ok())
        .map_or_else(WirePath::root, |index| {
            WirePath::root().child("output").index(index)
        });
    for key in members
        .keys()
        .filter(|key| !RESTAMPED_KEYS.contains(&key.as_str()))
    {
        edits.declare_at(
            EditSite::StreamEnvelopeStamp,
            || base.child(key),
            EditKind::Remove,
        );
    }
    tracing::debug!(
        withheld = members.len(),
        upstream_index = ?upstream_index,
        "openai-responses ingress: carried upstream event with no item to place it on; withheld"
    );
    if !std::mem::replace(&mut carried.withheld_counted, true) {
        record_translation_drop(LANE, UNKNOWN_MEMBER_WITHHELD);
    }
}

fn serialized_len(members: &Map<String, Value>) -> usize {
    serde_json::to_vec(members).map_or(usize::MAX, |bytes| bytes.len())
}

#[cfg(test)]
#[path = "stream_carriage_tests.rs"]
mod tests;
