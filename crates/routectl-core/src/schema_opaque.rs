//! Transport-internal carrier for raw upstream SSE bytes that don't fit
//! the canonical `ChunkDelta` shape. Used by the Anthropic-API egress to
//! preserve unknown `content_block` types verbatim through the canonical
//! pipeline so the matching Anthropic ingress can re-emit them
//! byte-for-byte. Skip-serialized; never on the wire.
//!
//! Mirrors the `#[non_exhaustive]` precedent at
//! `crate::schema::RoutectlInternal` so future variants ship without
//! breaking downstream library consumers that match on this enum. The
//! `#[serde(skip)]` field pattern on `ChatChunk::opaque_events` mirrors
//! `ChatRequest::routectl_internal`.
//!
//! ## Naming choice
//!
//! Variant names mirror Anthropic-API SSE event names (`content_block_start`,
//! `content_block_delta`, `content_block_stop`) because Anthropic-API is the
//! first and only producer today. The abstraction is transport-neutral:
//! a future provider that emits opaque events with the same envelope shape
//! (start / delta / stop with a type tag and raw bytes) reuses the same
//! variants. A future provider with a fundamentally different envelope
//! (e.g. atomic event payloads) would add a new `#[non_exhaustive]` variant.

/// One opaque SSE event captured from an Anthropic upstream. Mirrors
/// the SSE envelope shape (start / delta / stop) but carries the raw
/// upstream-wire bytes verbatim so the matching ingress can re-emit
/// them without re-serialization.
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
        upstream_index: u32,
        type_tag: String,
        raw_data: Vec<u8>,
    },
    /// `content_block_delta` event where the delta type is unknown
    /// (e.g. `citations_delta`). Raw delta JSON preserved.
    ContentBlockDelta {
        upstream_index: u32,
        raw_delta: Vec<u8>,
    },
    /// `content_block_stop` event for an unknown block.
    ContentBlockStop { upstream_index: u32 },
}
