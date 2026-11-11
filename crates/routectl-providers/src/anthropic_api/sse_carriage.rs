//! Event-by-event carriage of what the canonical chunk cannot hold, for a
//! client that speaks the Anthropic dialect: the unmodeled members of a
//! `message_start.message`, a `message_delta.delta` and a known
//! `content_block_start`, a top-level event of a type the parser does not
//! model, and a delta of an unknown type on a block it does model.
//!
//! A known block's start and stop are always carried, members or not, so
//! the ingress can tie a later unknown delta to the block it opened for that
//! start, and close that block where the upstream closed its own.
//!
//! Each rides on the chunk for the event that carried it, never on a later
//! one, so the ingress re-emits it in upstream order. Every carried event is
//! held under the opaque-capture caps; one over them ends the stream with an
//! error and counts, rather than reaching the client truncated.

use serde_json::{Map, Value};

use routectl_core::{ChatChunk, Error, OpaqueSseEvent, Result, sanitize_for_log};

use super::sse::SseState;
use super::sse_opaque::{
    MAX_OPAQUE_BYTES_PER_BLOCK, MAX_OPAQUE_EVENTS_PER_STREAM, MAX_TOTAL_OPAQUE_BYTES_PER_STREAM,
};

/// The members of `message_start.message` the Anthropic stream render
/// writes itself.
pub(super) const MESSAGE_START_KEYS: &[&str] = &[
    "id",
    "type",
    "role",
    "content",
    "model",
    "stop_reason",
    "stop_sequence",
    "usage",
];

impl SseState {
    /// Carry the members of the `event_type` event beyond `reserved`.
    pub(super) fn carry_event_members(
        &mut self,
        provider_id: &str,
        event_type: &str,
        upstream_index: Option<u32>,
        mut members: Map<String, Value>,
        reserved: &[&str],
    ) -> Result<()> {
        if !self.carries_unmodeled {
            return Ok(());
        }
        members.retain(|key, _| !reserved.contains(&key.as_str()));
        if members.is_empty() {
            return Ok(());
        }
        let size = serialized_len(&Value::Object(members.clone()));
        self.carry(
            provider_id,
            event_type,
            size,
            OpaqueSseEvent::KnownEventExtras {
                event_type: event_type.to_owned(),
                upstream_index,
                members,
            },
        )
    }

    /// Carry a known `content_block_start` with its unmodeled members and
    /// its `type`, even when it has no other member: the ingress opens its
    /// own blocks lazily, and this is how it ties the block it opens, and a
    /// later unknown delta, to this upstream block.
    pub(super) fn carry_block_start_members(
        &mut self,
        provider_id: &str,
        index: u32,
        block_type: &str,
        mut members: Map<String, Value>,
    ) -> Result<()> {
        if !self.carries_unmodeled {
            return Ok(());
        }
        members.insert("type".to_owned(), Value::String(block_type.to_owned()));
        let size = serialized_len(&Value::Object(members.clone()));
        self.carry(
            provider_id,
            "content_block_start",
            size,
            OpaqueSseEvent::KnownEventExtras {
                event_type: "content_block_start".to_owned(),
                upstream_index: Some(index),
                members,
            },
        )
    }

    /// Carry the stop of a known block, so the ingress closes the block it
    /// opened for it at this point in the stream rather than at the next
    /// block or the end.
    pub(super) fn carry_block_stop(&mut self, provider_id: &str, index: u32) -> Result<()> {
        if !self.carries_unmodeled {
            return Ok(());
        }
        self.carry(
            provider_id,
            "content_block_stop",
            0,
            OpaqueSseEvent::KnownEventExtras {
                event_type: "content_block_stop".to_owned(),
                upstream_index: Some(index),
                members: Map::new(),
            },
        )
    }

    /// Carry a top-level event of a type the parser does not model.
    pub(super) fn carry_unknown_event(
        &mut self,
        provider_id: &str,
        event_type: String,
        event: Value,
    ) -> Result<()> {
        let size = serialized_len(&event);
        self.carry(
            provider_id,
            "unknown_event",
            size,
            OpaqueSseEvent::UnknownEvent { event_type, event },
        )
    }

    /// A delta of an unknown type on a modeled block: carried on its own
    /// chunk for a client of this dialect, dropped for any other.
    pub(super) fn carry_unknown_delta(
        &mut self,
        provider_id: &str,
        index: u32,
        delta: Value,
    ) -> Result<Option<ChatChunk>> {
        if !self.carries_unmodeled {
            return Ok(None);
        }
        let size = serialized_len(&delta);
        self.carry(
            provider_id,
            "content_block_delta",
            size,
            OpaqueSseEvent::UnknownBlockDelta {
                upstream_index: index,
                delta,
            },
        )?;
        Ok(None)
    }

    fn carry(
        &mut self,
        provider_id: &str,
        event_kind: &str,
        size: usize,
        carried: OpaqueSseEvent,
    ) -> Result<()> {
        self.carried_bytes_total = self.carried_bytes_total.saturating_add(size);
        self.carried_events_total = self.carried_events_total.saturating_add(1);
        if size > MAX_OPAQUE_BYTES_PER_BLOCK
            || self.carried_bytes_total > MAX_TOTAL_OPAQUE_BYTES_PER_STREAM
            || self.carried_events_total > MAX_OPAQUE_EVENTS_PER_STREAM
        {
            record_oversize_event();
            tracing::warn!(
                provider = %provider_id,
                event_kind = %sanitize_for_log(event_kind),
                event_bytes = size,
                carried_bytes_total = self.carried_bytes_total,
                carried_events_total = self.carried_events_total,
                "anthropic SSE: carried event over the opaque-capture caps; ending the stream",
            );
            return Err(Error::Streaming(format!(
                "anthropic upstream sent a {} event over the carriage cap",
                sanitize_for_log(event_kind)
            )));
        }
        self.event_carriage.push(carried);
        Ok(())
    }
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// Carried events that ended a stream for exceeding the opaque-capture
/// caps, since process start.
pub fn oversize_event_total() -> u64 {
    read_oversize_total()
}

#[cfg(not(test))]
static OVERSIZE_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(not(test))]
fn read_oversize_total() -> u64 {
    OVERSIZE_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(test))]
fn record_oversize_event() {
    OVERSIZE_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

// The unit-test build keeps the total per thread, so a test reads back only
// the trips its own parser caused.
#[cfg(test)]
thread_local! {
    static OVERSIZE_TOTAL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn read_oversize_total() -> u64 {
    OVERSIZE_TOTAL.get()
}

#[cfg(test)]
fn record_oversize_event() {
    OVERSIZE_TOTAL.set(OVERSIZE_TOTAL.get().saturating_add(1));
}
