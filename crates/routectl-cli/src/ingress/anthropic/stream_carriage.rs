//! Re-emission of what an Anthropic upstream sent beyond the canonical
//! chunk: the unmodeled members of `message_start.message`, of a
//! `message_delta` event and its `delta`, and of a known
//! `content_block_start`; top-level events
//! of a type no canonical field models, and deltas of an unknown type on a
//! known block. The Anthropic-API egress carries each on the chunk of the
//! event that carried it, only for a client of this dialect.
//!
//! Members merge into the matching event with the render's own members
//! winning. Block indexes stay this render's own: a carried block start
//! is held until the render opens the block it stands for, which ties the
//! upstream index to the render's index for the unknown deltas that follow.
//! A `message_start` whose members arrive after the render already sent its
//! own (the warm-hold early frame) cannot take them. They are held for the
//! top level of the closing `message_delta`, the event that states
//! top-level changes to the final message, under the render's own members
//! and the upstream `message_delta`'s. A stream that closes with no
//! `message_delta` withholds them, declared and counted.

use serde_json::{Map, Value};

use routectl_core::wire_check::{EditKind, EditSite, WirePath};
use routectl_core::{OpaqueSseEvent, is_safe_token, sanitize_for_log};
use routectl_providers::translation_drop_metrics::record_translation_drop;

use crate::ingress::{ResponseEdits, SseEvent};

use super::AnthropicStreamState;
use super::stream::{close_open_block, ensure_text_block, flush_tool_blocks, push_block_delta};

/// Census lane the withheld `message_start` members are counted on.
const LANE: &str = routectl_core::provider_kind::trace::ANTHROPIC;

/// Drop class for upstream members the render cannot place on the event
/// that carried them.
const UNKNOWN_MEMBER_WITHHELD: &str = "unknown_member_withheld";

const MESSAGE_START: &str = "message_start";
const MESSAGE_DELTA: &str = "message_delta";
const CONTENT_BLOCK_START: &str = "content_block_start";
const CONTENT_BLOCK_STOP: &str = "content_block_stop";
const TEXT_BLOCK: &str = "text";

/// An upstream block whose start was carried, with what the render owes it
/// once it opens the matching block of its own.
#[derive(Debug, Default, Clone)]
pub(super) struct CarriedBlock {
    upstream_index: u32,
    block_type: String,
    members: Map<String, Value>,
    deltas: Vec<Value>,
}

impl CarriedBlock {
    fn is_empty(&self) -> bool {
        self.members.is_empty() && self.deltas.is_empty()
    }
}

/// Per-stream carriage state.
#[derive(Debug, Default)]
pub(super) struct CarriedState {
    /// Carried block starts the render has not opened a block for yet, in
    /// upstream order.
    pending: Vec<CarriedBlock>,
    /// The upstream index and render index of the open block a carried
    /// start was bound to.
    bound: Option<(u32, usize)>,
    /// `message_delta.delta` members waiting for the render's
    /// `message_delta`.
    delta_members: Map<String, Value>,
    /// Top-level `message_delta` members waiting for the render's
    /// `message_delta`.
    top_level_members: Map<String, Value>,
    /// `message_start.message` members that arrived after the render sent
    /// its `message_start`.
    late_start_members: Map<String, Value>,
}

/// What the render's closing `message_delta` owes the upstream.
#[derive(Debug, Default)]
pub(super) struct MessageDeltaCarriage {
    /// Members for the `delta` object.
    pub(super) delta: Map<String, Value>,
    /// Members for the event's top level.
    pub(super) top_level: Map<String, Value>,
}

impl CarriedState {
    /// Everything carried for the closing `message_delta`: the upstream
    /// `delta` members, and at the top level the upstream's own top-level
    /// members over the late `message_start` members. A late member the
    /// `delta` object restates, whether the render's own key or a carried
    /// one, is superseded by it. None when nothing was carried.
    pub(super) fn take_message_delta(&mut self) -> Option<MessageDeltaCarriage> {
        let delta = std::mem::take(&mut self.delta_members);
        let mut top_level = std::mem::take(&mut self.top_level_members);
        for (key, value) in std::mem::take(&mut self.late_start_members) {
            if !MESSAGE_DELTA_DELTA_KEYS.contains(&key.as_str()) && !delta.contains_key(&key) {
                top_level.entry(key).or_insert(value);
            }
        }
        (!delta.is_empty() || !top_level.is_empty())
            .then_some(MessageDeltaCarriage { delta, top_level })
    }

    /// Hold `message_start` members that arrived after the render's own
    /// `message_start` for the closing `message_delta`.
    pub(super) fn hold_late_start_members(&mut self, members: &Map<String, Value>) {
        merge_carried(&mut self.late_start_members, members);
    }

    /// The late `message_start` members no `message_delta` took.
    pub(super) fn take_late_start_members(&mut self) -> Map<String, Value> {
        std::mem::take(&mut self.late_start_members)
    }

    /// The first carried start of `block_type`, now owned by the block the
    /// render opened at `ingress_index`.
    pub(super) fn bind(&mut self, block_type: &str, ingress_index: usize) -> Option<CarriedBlock> {
        let position = self
            .pending
            .iter()
            .position(|p| p.block_type == block_type)?;
        let carried = self.pending.remove(position);
        self.bound = Some((carried.upstream_index, ingress_index));
        Some(carried)
    }

    /// The first carried `tool_use` start, owned by the tool block the
    /// render buffers until it flushes.
    pub(super) fn take_tool_start(&mut self) -> Option<CarriedBlock> {
        let position = self
            .pending
            .iter()
            .position(|p| p.block_type == "tool_use")?;
        Some(self.pending.remove(position))
    }
}

/// Take the carried `message_start` members off `opaque_events`; they merge
/// into the render's own `message_start`, not into the replay.
pub(super) fn take_start_members(
    opaque_events: &mut Vec<OpaqueSseEvent>,
) -> Option<Map<String, Value>> {
    let position = opaque_events.iter().position(|event| {
        matches!(event, OpaqueSseEvent::KnownEventExtras { event_type, .. } if event_type == MESSAGE_START)
    })?;
    match opaque_events.remove(position) {
        OpaqueSseEvent::KnownEventExtras { members, .. } => Some(members),
        _ => None,
    }
}

/// Add each carried member to `target` that the render did not write.
pub(super) fn merge_carried(target: &mut Map<String, Value>, carried: &Map<String, Value>) {
    for (key, value) in carried {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

/// The top-level members of the render's `message_delta` event.
pub(super) const MESSAGE_DELTA_KEYS: &[&str] = &["type", "delta", "usage"];

/// The members of the render's `message_delta.delta`.
pub(super) const MESSAGE_DELTA_DELTA_KEYS: &[&str] = &["stop_reason", "stop_sequence"];

/// Add each carried top-level member to the render's `message_delta`
/// `payload` that is not one of the render's own keys, whether or not this
/// event wrote that key.
pub(super) fn merge_top_level(payload: &mut Map<String, Value>, carried: &Map<String, Value>) {
    for (key, value) in carried {
        if !MESSAGE_DELTA_KEYS.contains(&key.as_str()) {
            payload.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

/// Late `message_start` members the stream closed without a
/// `message_delta` for: declare each one removed and count the stream once.
pub(super) fn withhold_start_members(members: &Map<String, Value>, edits: &mut ResponseEdits) {
    if members.is_empty() {
        return;
    }
    for key in members.keys() {
        edits.declare_at(
            EditSite::StreamEnvelopeStamp,
            || WirePath::root().child(key),
            EditKind::Remove,
        );
    }
    tracing::debug!(
        withheld = members.len(),
        "anthropic ingress: late message_start members with no message_delta to carry them; withheld"
    );
    record_translation_drop(LANE, UNKNOWN_MEMBER_WITHHELD);
}

/// Replay one carried event at its position in the chunk.
pub(super) fn replay_carried(
    event: &OpaqueSseEvent,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    match event {
        OpaqueSseEvent::KnownEventExtras {
            event_type,
            upstream_index,
            members,
        } => match (event_type.as_str(), upstream_index) {
            (CONTENT_BLOCK_START, Some(index)) => {
                open_carried_start(*index, members, state, events)
            }
            (CONTENT_BLOCK_STOP, Some(index)) => close_carried_block(*index, state, events),
            (MESSAGE_DELTA, _) => hold_message_delta_members(members, &mut state.carried),
            _ => {
                tracing::debug!("anthropic ingress: carried members of an event it does not render")
            }
        },
        OpaqueSseEvent::UnknownEvent { event_type, event } => {
            emit_unknown_event(event_type, event, events);
        }
        OpaqueSseEvent::UnknownBlockDelta {
            upstream_index,
            delta,
        } => route_unknown_delta(*upstream_index, delta, state, events),
        _ => {}
    }
}

/// Hold a carried `message_delta`'s members, shaped as the event: its
/// top-level members, and its `delta` object's under `delta`.
fn hold_message_delta_members(members: &Map<String, Value>, carried: &mut CarriedState) {
    for (key, value) in members {
        match (key.as_str(), value) {
            ("delta", Value::Object(delta)) => merge_carried(&mut carried.delta_members, delta),
            _ => {
                carried
                    .top_level_members
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }
}

/// An upstream block start ends whatever block the render has open, since
/// upstream blocks never overlap, and waits for the render's matching block.
fn open_carried_start(
    upstream_index: u32,
    members: &Map<String, Value>,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    close_open_block(state, events);
    flush_unbound(state, events);
    let mut members = members.clone();
    let block_type = match members.remove("type") {
        Some(Value::String(block_type)) => block_type,
        _ => String::new(),
    };
    state.carried.pending.push(CarriedBlock {
        upstream_index,
        block_type,
        members,
        deltas: Vec::new(),
    });
}

/// The upstream closed `upstream_index`: close the render block bound to
/// it, if that block is still the open one. A buffered tool block is
/// flushed and closed here, so what the upstream sent after it follows it.
fn close_carried_block(
    upstream_index: u32,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    let closes_tool_block = state
        .tool_blocks
        .iter()
        .filter_map(|block| block.carried.as_ref())
        .any(|carried| carried.upstream_index == upstream_index);
    if closes_tool_block {
        flush_tool_blocks(state, events);
        close_open_block(state, events);
        return;
    }
    if let Some((bound, ingress_index)) = state.carried.bound
        && bound == upstream_index
        && state.open.is_some_and(|(open, _)| open == ingress_index)
    {
        close_open_block(state, events);
        state.carried.bound = None;
    }
}

/// Emit a carried text block the render never opened because no text
/// followed its start, so its members and deltas still reach the client;
/// drop every other unbound start.
pub(super) fn flush_unbound(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    let pending = std::mem::take(&mut state.carried.pending);
    for carried in pending {
        if carried.block_type == TEXT_BLOCK && !carried.is_empty() {
            state.carried.pending.push(carried);
            ensure_text_block(state, events);
            close_open_block(state, events);
        } else if !carried.is_empty() {
            tracing::debug!(
                block_type = %sanitize_for_log(&carried.block_type),
                "anthropic ingress: carried members of a block the render never opened"
            );
        }
    }
}

/// Emit an unknown top-level event under its own name. A name that could
/// not be an SSE event name is not emitted.
fn emit_unknown_event(event_type: &str, event: &Value, events: &mut Vec<SseEvent>) {
    if !is_safe_token(event_type) {
        tracing::debug!("anthropic ingress: unknown upstream event with an unusable name; skipped");
        return;
    }
    match serde_json::to_string(event) {
        Ok(data) => events.push(SseEvent::named(event_type, data)),
        Err(e) => {
            tracing::debug!(error = %e, "anthropic ingress: unknown upstream event not serializable")
        }
    }
}

/// An unknown delta goes to the render block its upstream block is bound
/// to, now; to a block not opened yet, with that block; otherwise nowhere.
fn route_unknown_delta(
    upstream_index: u32,
    delta: &Value,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    if let Some((bound, ingress_index)) = state.carried.bound
        && bound == upstream_index
        && state.open.is_some_and(|(open, _)| open == ingress_index)
    {
        push_block_delta(events, ingress_index, delta.clone());
        return;
    }
    if let Some(carried) = state
        .carried
        .pending
        .iter_mut()
        .find(|p| p.upstream_index == upstream_index)
    {
        let opens_now = carried.block_type == TEXT_BLOCK;
        carried.deltas.push(delta.clone());
        if opens_now {
            ensure_text_block(state, events);
        }
        return;
    }
    if let Some(carried) = state
        .tool_blocks
        .iter_mut()
        .filter_map(|block| block.carried.as_mut())
        .find(|c| c.upstream_index == upstream_index)
    {
        carried.deltas.push(delta.clone());
        return;
    }
    tracing::debug!(
        upstream_index,
        "anthropic ingress: unknown delta for a block the render has no record of; skipped"
    );
}

/// Write `carried` into the block start the render just built at
/// `ingress_index`, then emit the deltas it held.
pub(super) fn complete_block_start(
    block: &mut Value,
    carried: Option<&CarriedBlock>,
) -> Vec<Value> {
    let Some(carried) = carried else {
        return Vec::new();
    };
    if let Value::Object(fields) = block {
        merge_carried(fields, &carried.members);
    }
    carried.deltas.clone()
}

#[cfg(test)]
#[path = "stream_carriage_tests.rs"]
mod tests;
