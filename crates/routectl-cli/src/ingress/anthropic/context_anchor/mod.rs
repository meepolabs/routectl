//! Session anchor for the Anthropic ingress's client-facing context meter.
//!
//! A translated lane cannot report its real opening input count before the
//! first content arrives, so the meter seeds the opening from the previous
//! turn of the same conversation: the provider's own cache-inclusive input
//! total for that turn, plus the growth since. The anchor is only trusted
//! when the new request provably extends the old one:
//!
//! - same session key and requested model (the store key);
//! - the same resolved target (provider kind, nickname, upstream model) and
//!   router publication generation as the turn that produced the anchor;
//! - no fewer messages than before (a compacted history misses);
//! - a SHA-256 digest over the prompt-affecting fields plus EVERY prior
//!   message equal to the digest recorded for the prior request, so a
//!   same-length rewrite anywhere in the history misses.
//!
//! The growth is a bytes/4 floor over the SAME normalized stream the digest
//! hashes, measured on the inbound request. It is anchor-only: the router's
//! persisted `estimate_total_tokens` is not used here and is not changed.
//!
//! The normalized stream leaves out only what has no effect on the prompt
//! the model reads and legitimately changes turn to turn: `cache_control`
//! annotations at their wire positions (clients move the breakpoint to the
//! newest message every turn), the client billing/attribution system block
//! (a per-request checksum), and sampling parameters. Every passthrough
//! extra, request `metadata` included (the Anthropic egress forwards it),
//! is hashed, so a changed or unknown field fails closed as a miss.
//!
//! Handler flow, one turn:
//!
//! 1. `AnchorKey::new(session, requested_model)` at admission; `None`
//!    (an overlong session key or model name) means the turn is not
//!    anchored at all.
//! 2. `ContextAnchorStore::reserve_turn(key)` BEFORE measuring, so a large
//!    older request cannot take a newer place in the order.
//! 3. `TurnTicket::current_record()`, then `TurnTicket::measure(req,
//!    prior_message_count)` to get a `PendingAnchor`.
//! 4. `evaluate(record, pending.identity(), lane)` for the opening.
//! 5. `PendingAnchor::settle(outcome)` once the turn ends. It consumes the
//!    pending turn and publishes only into the store and under the key the
//!    ticket was reserved for; dropping it unsettled publishes nothing.
//!
//! `AnchorLane::generation` must be the Router's own publication
//! generation, read through a Router accessor the handler wiring adds. The
//! Router's existing `registry_generation` is NOT a substitute: the
//! registry is shared across rebuilds and only moves when the registry
//! does, so a reload that changes prompt-shaping config can leave it
//! unchanged and keep a stale anchor alive.
//!
//! The store is in memory only: a restart starts cold, and a new router
//! publication (once wired as above) moves the generation, so records from
//! before a reload no longer apply. No prompt body is retained -- a record
//! is counts, a digest, and a lane.

mod digest;
mod store;

pub use store::{
    ANCHOR_CAPACITY, ContextAnchorStore, PendingAnchor, SettleOutcome, TurnOutcome, TurnTicket,
};

use routectl_core::ChatRequest;

/// The bytes/4 floor rule of the router's persisted estimate, applied here
/// to the normalized stream instead of the raw serialized request.
const NORMALIZED_BYTES_PER_TOKEN: u64 = 4;

/// SHA-256 output identifying a request prefix.
pub type PrefixDigest = [u8; 32];

/// Longest session identifier a key may hold, in bytes. Matches the bound
/// the ingresses already apply to an inbound session key.
pub const MAX_ANCHOR_SESSION_KEY_BYTES: usize =
    crate::ingress::session_key::MAX_INBOUND_SESSION_KEY_BYTES;

/// Longest requested-model name a key may hold, in bytes. The model field
/// is client-supplied; without this bound the entry cap alone would not
/// bound the store's memory.
pub const MAX_ANCHOR_MODEL_BYTES: usize = 256;

/// Store key: the inbound session identifier plus the requested model.
/// Only constructible through [`AnchorKey::new`], which enforces both
/// byte bounds.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AnchorKey {
    session_key: String,
    requested_model: String,
}

/// Redacted: a key holds a raw session identifier, which never goes to logs.
impl std::fmt::Debug for AnchorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AnchorKey(..)")
    }
}

impl AnchorKey {
    /// Build a key, or `None` when either part exceeds its byte bound --
    /// such a request is simply not anchored.
    pub fn new(session_key: &str, requested_model: &str) -> Option<Self> {
        if session_key.len() > MAX_ANCHOR_SESSION_KEY_BYTES
            || requested_model.len() > MAX_ANCHOR_MODEL_BYTES
        {
            return None;
        }
        Some(Self {
            session_key: session_key.to_owned(),
            requested_model: requested_model.to_owned(),
        })
    }
}

/// The resolved target that served (or is expected to serve) a turn.
///
/// Deliberately carries no seat or credential: rotating between seats of
/// the same target does not change what the upstream counts, so it must
/// not invalidate an anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorLane {
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model nickname.
    pub model: String,
    /// Upstream wire model id the nickname resolved to. A nickname
    /// repointed at another model is a different lane.
    pub upstream_model: String,
    /// Router publication generation that resolved the target. A reload
    /// can change prompt-shaping policy after the inbound request was
    /// measured, so a record from another generation never applies. Must
    /// come from the Router's publication accessor, not its shared
    /// `registry_generation` (see the module docs).
    pub generation: u64,
}

impl AnchorLane {
    fn same_target(&self, other: &Self) -> bool {
        self.provider_kind == other.provider_kind
            && self.model == other.model
            && self.upstream_model == other.upstream_model
    }
}

/// What one request contributes to the anchor: its size, its digest, and
/// the digest of the prefix a prior record claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIdentity {
    message_count: usize,
    digest: Option<PrefixDigest>,
    prior_prefix: Option<(usize, PrefixDigest)>,
    normalized_bytes: u64,
    normalized_estimate: u64,
}

impl RequestIdentity {
    /// Measure `req`: its bytes/4 floor estimate, its full digest, and --
    /// when `prior_message_count` is given and not larger than the request
    /// -- the digest of its first `prior_message_count` messages, computed
    /// in the same pass. Pure; takes no lock.
    pub fn measure(req: &ChatRequest, prior_message_count: Option<usize>) -> Self {
        let digests = digest::digest_request(req, prior_message_count);
        Self {
            message_count: req.messages.len(),
            digest: digests.full,
            prior_prefix: digests.prefix,
            normalized_bytes: digests.normalized_bytes,
            normalized_estimate: digests.normalized_bytes / NORMALIZED_BYTES_PER_TOKEN,
        }
    }

    /// Number of messages in the measured request.
    pub const fn message_count(&self) -> usize {
        self.message_count
    }

    /// Bytes/4 floor over the normalized stream of the measured request.
    pub const fn normalized_estimate(&self) -> u64 {
        self.normalized_estimate
    }

    /// Digest of the whole measured request; `None` if hashing failed.
    pub const fn digest(&self) -> Option<PrefixDigest> {
        self.digest
    }

    /// Bytes of the normalized stream the digest hashed.
    pub const fn normalized_bytes(&self) -> u64 {
        self.normalized_bytes
    }
}

/// One successful turn, as the next turn of the conversation needs it.
/// Immutable: a newer turn replaces the record, never edits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRecord {
    message_count: usize,
    prefix_digest: PrefixDigest,
    normalized_estimate: u64,
    actual_input: u64,
    lane: AnchorLane,
    turn: u64,
}

impl AnchorRecord {
    /// Message count of the anchored request.
    pub const fn message_count(&self) -> usize {
        self.message_count
    }

    /// Bytes/4 floor over the normalized stream of the anchored request.
    pub const fn normalized_estimate(&self) -> u64 {
        self.normalized_estimate
    }

    /// Provider-reported cache-inclusive input total of the anchored turn.
    pub const fn actual_input(&self) -> u64 {
        self.actual_input
    }

    /// Lane that served the anchored turn.
    pub const fn lane(&self) -> &AnchorLane {
        &self.lane
    }
}

/// Why an anchor could not be used for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MissReason {
    /// No record for this session and model.
    Cold,
    /// The record came from a different provider kind, nickname or
    /// upstream model.
    LaneChanged,
    /// The record came from a different router publication.
    GenerationChanged,
    /// The request carries fewer messages than the anchored one.
    HistoryShrank,
    /// The prior messages or prompt-affecting fields differ.
    PrefixChanged,
}

impl MissReason {
    /// Stable, log-safe label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::LaneChanged => "lane_changed",
            Self::GenerationChanged => "generation_changed",
            Self::HistoryShrank => "history_shrank",
            Self::PrefixChanged => "prefix_changed",
        }
    }
}

/// Result of checking a request against its session's anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorVerdict {
    /// The anchor applies; `opening_input` is the anchored estimate.
    Hit {
        /// Prior actual input plus the normalized bytes/4 growth since.
        opening_input: u64,
    },
    /// The anchor does not apply.
    Miss(MissReason),
}

/// Decide whether `prior` anchors `current` on `lane`.
pub fn evaluate(
    prior: Option<&AnchorRecord>,
    current: &RequestIdentity,
    lane: &AnchorLane,
) -> AnchorVerdict {
    let Some(prior) = prior else {
        return AnchorVerdict::Miss(MissReason::Cold);
    };
    if !prior.lane.same_target(lane) {
        return AnchorVerdict::Miss(MissReason::LaneChanged);
    }
    if prior.lane.generation != lane.generation {
        return AnchorVerdict::Miss(MissReason::GenerationChanged);
    }
    if current.message_count < prior.message_count {
        return AnchorVerdict::Miss(MissReason::HistoryShrank);
    }
    if current.prior_prefix != Some((prior.message_count, prior.prefix_digest)) {
        return AnchorVerdict::Miss(MissReason::PrefixChanged);
    }
    AnchorVerdict::Hit {
        opening_input: anchored_input(
            prior.actual_input,
            prior.normalized_estimate,
            current.normalized_estimate,
        ),
    }
}

/// `prior_actual` moved by the signed change in raw estimate, saturating
/// at both ends so neither shrinkage nor huge inputs can wrap.
pub const fn anchored_input(prior_actual: u64, prior_raw: u64, current_raw: u64) -> u64 {
    if current_raw >= prior_raw {
        prior_actual.saturating_add(current_raw - prior_raw)
    } else {
        prior_actual.saturating_sub(prior_raw - current_raw)
    }
}

#[cfg(test)]
#[path = "anchor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "annotation_tests.rs"]
mod annotation_tests;

#[cfg(test)]
#[path = "estimate_tests.rs"]
mod estimate_tests;
