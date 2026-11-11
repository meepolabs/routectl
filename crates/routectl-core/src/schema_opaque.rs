//! Transport-internal carrier for upstream stream events that don't fit
//! the canonical `ChunkDelta` shape. An egress captures them so the
//! ingress of the same dialect can re-emit them; every other ingress
//! ignores them. Skip-serialized; never on the wire.
//!
//! Three envelope families ride here:
//! - Anthropic-API `content_block` start / delta / stop events for an
//!   unknown block or delta type, as raw upstream bytes, so the Anthropic
//!   ingress re-emits them byte-for-byte.
//! - OpenAI Responses output items of an unmodeled type, as one parsed
//!   JSON value per finished item, so the Responses ingress re-emits each
//!   as its own output item.
//! - Events of either dialect that the canonical chunk cannot express: an
//!   unknown top-level event, an unknown delta on a block the egress does
//!   model, and the unmodeled members of a known event. Each is carried
//!   as parsed JSON on the chunk for that event, so the ingress of the
//!   same dialect re-emits it in order and may re-stamp the members it
//!   manages (indexes, sequence numbers) before doing so.
//!
//! Mirrors the `#[non_exhaustive]` precedent at
//! `crate::schema::RoutectlInternal` so future variants ship without
//! breaking downstream library consumers that match on this enum. The
//! `#[serde(skip)]` field pattern on `ChatChunk::opaque_events` mirrors
//! `ChatRequest::routectl_internal`.
//!
//! ## Naming choice
//!
//! Each variant is named after the upstream event it carries:
//! `ContentBlock*` after the Anthropic-API SSE events, `OutputItem` after
//! the Responses output item, `UnknownEvent` / `UnknownBlockDelta` /
//! `KnownEventExtras` after what the upstream sent rather than a vendor
//! event name, since both dialects produce them. A provider with the same
//! envelope shape
//! reuses a variant; one with a different envelope adds a new variant.

/// One opaque upstream stream event: an Anthropic-API content-block event
/// as raw bytes (start / delta / stop), a whole Responses output item as
/// JSON, or an event or event members the canonical chunk does not model,
/// as JSON.
///
/// `#[non_exhaustive]` so future variants ship without breaking
/// downstream library consumers that match on this enum.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OpaqueSseEvent {
    /// `content_block_start` event for an unknown block type. The raw
    /// JSON of the inner `content_block` (e.g. `{"type":"server_tool_use",
    /// "id":"...","input":{...}}`) is preserved in `raw_data`.
    ContentBlockStart {
        /// Block index as reported by the upstream stream.
        upstream_index: u32,
        /// The block's wire `type` discriminant.
        type_tag: String,
        /// Raw JSON of the inner `content_block`, preserved verbatim.
        raw_data: Vec<u8>,
    },
    /// `content_block_delta` event where the delta type is unknown
    /// (e.g. `citations_delta`). Raw delta JSON preserved.
    ContentBlockDelta {
        /// Block index as reported by the upstream stream.
        upstream_index: u32,
        /// Raw delta JSON, preserved verbatim.
        raw_delta: Vec<u8>,
    },
    /// `content_block_stop` event for an unknown block.
    ContentBlockStop {
        /// Block index as reported by the upstream stream.
        upstream_index: u32,
    },
    /// One complete OpenAI Responses output item of a type the canonical
    /// chunk does not model (e.g. `custom_tool_call`), carried whole so a
    /// Responses ingress can re-emit it as its own output item. Atomic:
    /// one event per item, never split into start / delta / stop.
    OutputItem {
        /// The item JSON as the upstream finalized it.
        item: serde_json::Value,
    },
    /// A top-level stream event of a type the egress does not model,
    /// carried whole.
    UnknownEvent {
        /// The event's wire `type` discriminant.
        event_type: String,
        /// The event JSON as the upstream sent it.
        event: serde_json::Value,
    },
    /// A delta of an unknown type on a block whose type the egress does
    /// model (an unknown block's deltas ride `ContentBlockDelta`).
    UnknownBlockDelta {
        /// Block index as reported by the upstream stream.
        upstream_index: u32,
        /// The delta JSON as the upstream sent it.
        delta: serde_json::Value,
    },
    /// The members of a known event that the canonical chunk has no slot
    /// for, to merge into the matching ingress event with typed members
    /// winning.
    KnownEventExtras {
        /// The wire `type` of the event the members arrived on.
        event_type: String,
        /// The upstream block or output index the event names, when it
        /// names one.
        upstream_index: Option<u32>,
        /// The unmodeled members, keyed by their wire names.
        members: serde_json::Map<String, serde_json::Value>,
    },
}
