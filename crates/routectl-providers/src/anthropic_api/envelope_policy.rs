//! Host-gated reasoning-envelope policy for the `redacted_thinking`
//! egress, plus its per-request DEBUG aggregate.
//!
//! The policy lives in its own module because THREE independent sites
//! construct a `RedactedThinking` block on this lane: the content-part
//! walk and the `reasoning_details` replay channel (both decided in
//! `messages.rs` `normalize_replay_invariants`) and the context-management
//! cache reinjection (`context_management.rs`).
//! A single owner threaded from `request::normalize` is what keeps the
//! three from drifting, and what keeps the line at one per request rather
//! than one per site.

/// Constant event name of the aggregated DEBUG line. A fixed token so
/// operators can filter on it without the log line ever carrying
/// request-derived text.
const ENVELOPE_OMIT_EVENT: &str = "reasoning_envelope_omitted";

/// Per-request policy + tally for routectl reasoning envelopes on the
/// `redacted_thinking` egress.
///
/// A client history can carry a routectl reasoning envelope
/// (`routectl_core::reasoning_envelope`) inside a `redacted_thinking`
/// block: the Anthropic wire has no slot for a foreign reasoning
/// artifact's id and scheme, so the Anthropic ingress wraps them into the
/// opaque `data` field to keep the round trip stateless. Two egress
/// targets need opposite handling of the same bytes:
///
/// - Another routectl hop, an Anthropic-compatible third-party host, or
///   the Bedrock Invoke lane: the envelope MUST ride through
///   byte-for-byte. It is how reasoning continuity survives the hop.
/// - The terminal Anthropic host: an envelope only ever wraps an artifact
///   another lane minted, and that host answers 400 to a
///   `redacted_thinking` blob it did not issue. Neither the envelope nor
///   its inner blob can ship, so the whole block is omitted and that
///   turn's hidden reasoning is lost on the lane switch. This is the same
///   rule `is_anthropic_emittable_detail` applies to a format-tagged
///   foreign detail.
///
/// A string that is not a recognized envelope (a native Anthropic blob, a
/// malformed or unknown-version string) ships unchanged everywhere: the
/// codec's `unwrap` is total and answers `None` for it.
///
/// The discriminator is resolved ONCE at the client boundary (where the
/// provider's `base_url` lives) and carried here as a plain bool, so no
/// per-block decision re-derives it.
///
/// Every site that constructs a TYPED `redacted_thinking` block on this
/// lane asks [`Self::omit`] first: the replay-invariant pass for content
/// parts and `reasoning_details` (so a turn the omission empties is
/// dropped before translation rather than shipped as `content: []`), and
/// the cache reinjection for a cached detail. A site that skips it ships
/// the envelope to a host that rejects it.
///
/// Deliberately outside that set: a caller-supplied `redacted_thinking`
/// whose `data` is not a JSON string (or is absent) does not deserialize
/// into the typed part at all; it lands in the untagged passthrough
/// variant and is re-emitted verbatim. No envelope can hide there: an
/// envelope is a string by construction.
///
/// Log hygiene, absolute: the blob, the claimed scheme, the claimed
/// artifact id, and any digest of those NEVER reach a log field at any
/// level. One aggregated DEBUG line per request carries a constant event
/// name, the provider id, and a count; a history can hold unbounded
/// reasoning blocks, so a per-block line would be a log amplifier driven
/// by request content.
pub struct ForeignEnvelopeTally<'a> {
    provider: &'a str,
    terminal_anthropic_host: bool,
    omitted: usize,
}

impl<'a> ForeignEnvelopeTally<'a> {
    pub const fn new(provider: &'a str, terminal_anthropic_host: bool) -> Self {
        Self {
            provider,
            terminal_anthropic_host,
            omitted: 0,
        }
    }

    /// Whether a `redacted_thinking` block carrying `data` is omitted on
    /// this target. Pure: callers that must decide a turn's fate before
    /// building it ask here without touching the tally.
    pub fn omits(&self, data: &str) -> bool {
        self.terminal_anthropic_host && routectl_core::reasoning_envelope::unwrap(data).is_some()
    }

    /// [`Self::omits`], counting an omitted block for the DEBUG line. A
    /// kept block ships its `data` byte for byte: byte-identity is
    /// load-bearing for prompt-cache affinity upstream.
    pub fn omit(&mut self, data: &str) -> bool {
        let omitted = self.omits(data);
        if omitted {
            self.omitted += 1;
        }
        omitted
    }

    /// Emit the aggregated DEBUG line, if anything was omitted. Called
    /// exactly once per request, by `request::normalize`, after BOTH
    /// message translation and context-management reinjection have run:
    /// the two feed the same tally, so flushing between them would emit
    /// two lines for one request.
    pub fn flush(&self) {
        if self.omitted > 0 {
            tracing::debug!(
                provider = self.provider,
                event = ENVELOPE_OMIT_EVENT,
                omitted_count = self.omitted,
                "omitting redacted_thinking blocks that carry a routectl reasoning \
                 envelope on the terminal Anthropic egress: the artifact inside was \
                 minted by another lane and that host rejects it. Non-terminal \
                 targets keep the envelope byte-for-byte."
            );
        }
    }
}

/// Test-only tally selecting VERBATIM passthrough, for the translation
/// tests that assert shapes unrelated to the terminal-host envelope
/// policy. The policy itself has its own end-to-end tests.
#[cfg(test)]
pub const fn passthrough_tally() -> ForeignEnvelopeTally<'static> {
    ForeignEnvelopeTally::new("anthropic", false)
}
