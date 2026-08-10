//! Host-gated reasoning-envelope policy for the `redacted_thinking`
//! egress, plus its per-request WARN aggregate.
//!
//! The policy lives in its own module because THREE independent sites
//! construct a `RedactedThinking` block on this lane -- the content-part
//! walk and the `reasoning_details` replay channel (both in `messages.rs`)
//! and the context-management cache reinjection (`context_management.rs`).
//! A single owner threaded from `request::normalize` is what keeps the
//! three from drifting, and what keeps the WARN at one per request rather
//! than one per site.

/// Constant event name of the aggregated envelope-unwrap WARN. A fixed
/// token so operators can filter on it without the log line ever carrying
/// request-derived text.
const ENVELOPE_UNWRAP_EVENT: &str = "reasoning_envelope_unwrapped";

/// Per-request policy + tally for the reasoning-envelope unwrap on the
/// `redacted_thinking` egress.
///
/// A client history can carry a routectl reasoning envelope
/// (`routectl_core::reasoning_envelope`) inside a `redacted_thinking`
/// block: the codec exists because the Anthropic wire has no slot for a
/// foreign reasoning artifact's id and scheme, so a router hop wraps them
/// into the opaque `data` field to keep the round trip stateless. Two
/// egress targets need opposite handling of the same bytes:
///
/// - Another routectl hop, an Anthropic-compatible third-party host, or
///   the Bedrock Invoke lane: the envelope MUST ride through
///   byte-for-byte. It is how reasoning continuity survives the hop, and
///   a strip here breaks a working path on every request.
/// - The terminal Anthropic host: the envelope is a routectl-internal
///   framing, so what ships is the unwrapped INNER BLOB -- exactly the
///   bytes the caller would have sent with no routectl in the path.
///   Upstream behavior for a `redacted_thinking` blob that upstream did
///   not itself mint is UNPROBED; nothing here claims anything about it,
///   and unwrapping is the only replacement that invents no upstream
///   claim and discards no replay state.
///
/// The discriminator is resolved ONCE at the client boundary (where the
/// provider's `base_url` lives) and carried here as a plain bool, so no
/// per-block decision re-derives it.
///
/// Every site that constructs a TYPED `redacted_thinking` block on this
/// lane routes its `data` through [`Self::wire_data`]; a site that assigns
/// the field directly bypasses both the policy and the tally, so the wire
/// would diverge per channel while the log claimed a lower count.
///
/// Deliberately outside that set: a caller-supplied `redacted_thinking`
/// whose `data` is not a JSON string (or is absent) does not deserialize
/// into the typed part at all -- it lands in the untagged passthrough
/// variant and is re-emitted verbatim. No envelope can hide there: the
/// canonical field is a `String`, and an envelope is a string by
/// construction, so such a value could never have carried one.
///
/// Log hygiene, absolute: the blob, the claimed scheme, the claimed
/// artifact id, and any digest of those NEVER reach a log field at any
/// level. Scheme and id are client-minted claims that carry no authority
/// (see the envelope module), so logging them would be both a hygiene
/// breach and misleading. One aggregated WARN per request carries a
/// constant event name, the provider id, and a count -- a history can hold
/// unbounded reasoning blocks, so a per-block WARN would be a log
/// amplifier driven by request content.
pub struct EnvelopeUnwrapTally<'a> {
    provider: &'a str,
    terminal_anthropic_host: bool,
    unwrapped: usize,
}

impl<'a> EnvelopeUnwrapTally<'a> {
    pub const fn new(provider: &'a str, terminal_anthropic_host: bool) -> Self {
        Self {
            provider,
            terminal_anthropic_host,
            unwrapped: 0,
        }
    }

    /// The `redacted_thinking` `data` bytes to put on the wire.
    ///
    /// Returns `data` unchanged for every non-terminal target and for
    /// every input that is not a well-formed envelope of a recognized
    /// version -- `unwrap` is total and answers `None` for a malformed or
    /// unknown-version string, which is the verbatim-passthrough answer.
    /// The inner blob is never decoded, trimmed, or otherwise inspected:
    /// byte-identity is load-bearing for prompt-cache affinity upstream.
    pub fn wire_data(&mut self, data: &str) -> String {
        if !self.terminal_anthropic_host {
            return data.to_string();
        }
        match routectl_core::reasoning_envelope::unwrap(data) {
            // `unwrap` rejects an empty blob, so a match always yields a
            // non-empty replacement.
            Some((_scheme, _id, blob)) => {
                self.unwrapped += 1;
                blob.to_string()
            }
            None => data.to_string(),
        }
    }

    /// Emit the aggregated WARN, if anything was unwrapped. Called exactly
    /// once per request, by `request::normalize`, after BOTH message
    /// translation and context-management reinjection have run -- the two
    /// feed the same tally, so flushing between them would emit two lines
    /// for one request.
    pub fn flush(&self) {
        if self.unwrapped > 0 {
            tracing::warn!(
                provider = self.provider,
                event = ENVELOPE_UNWRAP_EVENT,
                unwrapped_count = self.unwrapped,
                "replacing routectl reasoning envelopes with their inner blob on \
                 the terminal Anthropic egress: the envelope is router-internal \
                 framing with no meaning upstream, so the artifact ships as the \
                 caller originally sent it. Non-terminal targets keep the \
                 envelope byte-for-byte."
            );
        }
    }
}

/// Test-only tally selecting VERBATIM passthrough, for the translation
/// tests that assert shapes unrelated to the terminal-host envelope
/// policy. The policy itself has its own end-to-end tests.
#[cfg(test)]
pub const fn passthrough_tally() -> EnvelopeUnwrapTally<'static> {
    EnvelopeUnwrapTally::new("anthropic", false)
}
