//! The post-response quota feed: the one place an upstream reading enters the
//! store, on BOTH completion paths.
//!
//! # Why both paths, and why that is not symmetric work
//!
//! Non-streaming quota metadata rides on the assembled response
//! (`ChatResponse::upstream_meta`), read once at the terminal success arm.
//! STREAMING quota rides on the FIRST canonical chunk only -- `ChatChunk`'s own
//! field documents that, and it is where the response head's headers are
//! available -- so the stream path has to lift it from a chunk as it passes,
//! before rendering consumes it, and must not re-read it per chunk. Waiting for
//! end-of-stream instead loses it entirely.
//!
//! Wiring only the non-streaming path would leave a deployment whose client
//! streams -- the ordinary case -- with every seat permanently reading as
//! no-evidence, and NOTHING would look wrong: no error, no warning, just a
//! store that stays empty while placement quietly falls back.
//!
//! # Exactly once per response
//!
//! [`feed_response`] runs at the terminal success arm, which returns
//! immediately after, so it fires once per non-streaming response.
//! [`feed_first_chunk`] is armed with the seat key and DISARMS itself on the
//! first chunk carrying metadata, so a stream feeds at most once however many
//! chunks follow -- and a stream that never carries metadata feeds nothing
//! rather than feeding an empty reading.
//!
//! # Which seat is fed
//!
//! The seat that ACTUALLY served, never the intended home: on a fallback the
//! reading describes the credential the upstream answered for. Its absent cases
//! -- a pre-dispatch failure, a non-OAuth credential, a forwarded client
//! credential -- are all correct skips, since none of them is an account whose
//! subscription budget routectl owns.

use std::sync::Arc;

use routectl_core::upstream_meta::UpstreamMeta;
use routectl_core::{ChatChunk, ChatResponse};

use super::freshness::ObservationStamp;
use super::key::SeatKey;
use super::reduce::{Reduction, reduce_anthropic, reduce_codex};
use super::store::QuotaStore;

/// Feed the reading carried by a completed NON-STREAMING response.
///
/// A response with no quota metadata, or a seat with no account identity, feeds
/// nothing.
pub fn feed_response(store: &QuotaStore, seat: Option<&SeatKey>, resp: &ChatResponse) {
    let (Some(seat), Some(meta)) = (seat, resp.upstream_meta.as_ref()) else {
        return;
    };
    feed_meta(store, seat, meta);
}

/// A stream's one-shot quota feed.
///
/// Constructed with the served seat's key at the point the stream is handed
/// back, then offered each chunk as it flows through. It feeds on the first
/// chunk carrying metadata and disarms, so the reading is taken from the
/// response head -- where it exists -- and never re-taken.
pub struct FirstChunkFeed {
    store: Arc<QuotaStore>,
    /// The served seat, taken on the first fed chunk. `None` both after
    /// feeding and when the served target had no account identity, so a
    /// stream on a non-OAuth credential is inert from the start.
    seat: Option<SeatKey>,
}

impl FirstChunkFeed {
    /// Arm a feed for the seat that served this stream. A `None` seat yields an
    /// inert feed -- the documented skip cases cost no per-chunk work.
    pub const fn armed(store: Arc<QuotaStore>, seat: Option<SeatKey>) -> Self {
        Self { store, seat }
    }

    /// Offer one chunk. Feeds and disarms on the first chunk carrying quota
    /// metadata; every other chunk, and every chunk after the first fed one, is
    /// a no-op.
    pub fn offer(&mut self, chunk: &ChatChunk) {
        let Some(meta) = chunk.upstream_meta.as_ref() else {
            return;
        };
        let Some(seat) = self.seat.take() else {
            return;
        };
        feed_meta(&self.store, &seat, meta);
    }
}

/// Reduce one carrier and merge it, counting whatever the trust rules refused.
///
/// A carrier holding neither vendor family is not an observation at all and
/// merges nothing: merging an all-`Unknown` reading would be harmless under the
/// merge rule but would still advance the stored observation instant, which is
/// what orders later readings.
fn feed_meta(store: &QuotaStore, seat: &SeatKey, meta: &UpstreamMeta) {
    let observed = ObservationStamp::now();
    let reduction = if let Some(quota) = meta.anthropic_unified.as_ref() {
        reduce_anthropic(quota, &observed)
    } else if let Some(quota) = meta.codex.as_ref() {
        reduce_codex(quota, &observed)
    } else {
        return;
    };
    let Reduction {
        snapshot,
        rejections,
    } = reduction;
    for reason in rejections {
        store.record_rejection(seat, reason);
    }
    store.observe(seat, snapshot);
}

#[cfg(test)]
#[path = "feed_tests.rs"]
mod feed_tests;
