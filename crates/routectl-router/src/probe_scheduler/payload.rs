//! The bounded, capability-grounded payload a probe sends, and its ceilings.
//!
//! Captured at ACTIVATION from the admitted post-overlay request, so the probe
//! asks about the exact field and value the upstream was about to see. Every
//! ceiling is enforced at capture, so nothing out of bounds is ever stored.

use std::collections::BTreeSet;

/// Ceiling on the number of beta tokens retained PER SOURCE.
///
/// Applied to the client set and the operator set independently, because the
/// two are stored separately and reapplied to different carriers -- a single
/// shared count would let one source consume the other's headroom. The pinned
/// Claude Code floor is nine flags and an operator adds a small handful, so
/// this leaves room over every real composition per source.
pub const PROBE_BETA_MAX_COUNT_PER_SOURCE: usize = 24;

/// Ceiling on ONE retained beta token, in bytes. Real flags are dated
/// slugs (`interleaved-thinking-2025-05-14`), so this bounds a single
/// adversarial token.
pub const PROBE_BETA_MAX_TOKEN_BYTES: usize = 64;

/// Ceiling on the COMBINED retained beta bytes, summed across BOTH sources.
///
/// Explicitly combined rather than per-source: the two sets are reapplied to
/// one request and the egress joins them into one header, so what needs
/// bounding is their total. A separate bound from the count and the per-token
/// ceiling because those multiply -- 2 sources x 24 tokens x 64 bytes is 3072,
/// and the point of a retention bound is that the product is not the real
/// ceiling.
pub const PROBE_BETA_MAX_TOTAL_BYTES: usize = 512;

/// The display values a probe will ask about.
///
/// The closed vocabulary the Anthropic egress actually models: `summarized` and
/// `omitted` are what the canonical `reasoning.exclude` boolean derives, and
/// `updates` is the third value a real Anthropic client sends.
///
/// A probe is RESTRICTED to these, and the reason is a denial-of-probing
/// hazard rather than a memory one. The display value is client-supplied text,
/// and a probe that carried an unmodeled value would ask the upstream about a
/// shape the closed table cannot repair -- the answer settles nothing, but the
/// settlement is terminal for the incarnation. So one client sending junk in
/// that field would tombstone the lane's identity and stop every other client's
/// traffic from ever being probed. Refusing at capture keeps the junk from
/// reaching a probe at all.
pub const PROBE_MODELED_DISPLAY_VALUES: [&str; 3] = ["summarized", "omitted", "updates"];

/// Whether `token` is a beta flag this build will retain and re-emit.
///
/// The egress joins betas into ONE comma-separated `anthropic-beta` header, so
/// a token carrying a comma would forge additional flags and one carrying CR or
/// LF would forge a header. Both are refused rather than escaped: a flag needing
/// either is not a flag this build can reproduce on the wire, and the probe's
/// whole purpose is to reproduce the admitted request's header exactly.
///
/// Refused, never sanitized-and-kept: a rewritten token is a DIFFERENT flag, so
/// keeping it would put the probe in a beta context the upstream never saw while
/// its answer was still attributed to the field.
fn is_retainable_beta(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= PROBE_BETA_MAX_TOKEN_BYTES
        && !token.contains(',')
        && !token.contains('\r')
        && !token.contains('\n')
        && token.chars().all(|c| !c.is_control())
}

/// Trim and validate one source's tokens into a bounded, deduplicated set.
///
/// Order of operations is load-bearing. The COUNT bound is checked against the
/// raw input BEFORE any per-token work, so an adversarial 10-million-element
/// vector is refused after one length read rather than after ten million trims;
/// then each token is trimmed and validated; then dedupe runs through a
/// `BTreeSet` (O(n log n)), not a linear scan per element (O(n^2)).
///
/// Returns `None` on any refusal -- a trimmed or partial set is never produced,
/// because it would ask a different question than the admitted request posed.
fn bounded_source(raw: &[String]) -> Option<Vec<String>> {
    if raw.len() > PROBE_BETA_MAX_COUNT_PER_SOURCE {
        return None;
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for token in raw {
        let trimmed = token.trim();
        if !is_retainable_beta(trimmed) {
            return None;
        }
        if seen.insert(trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Some(out)
}

/// The bounded, capability-grounded payload a probe sends.
///
/// Carried FROM the admitted post-overlay request at activation time rather
/// than rebuilt at dial time: the point of the probe is to ask about the
/// exact field/value the upstream was about to see, and a payload
/// reconstructed later from config would ask a different question.
///
/// Memory is bounded by construction. `field_path` is a `&'static str` from the
/// closed table and costs nothing per payload; `field_value` is one of the three
/// fixed literals in [`PROBE_MODELED_DISPLAY_VALUES`], so its size is a constant
/// of this build rather than a ceiling on caller text; and the two beta sets
/// together are refused above [`PROBE_BETA_MAX_TOTAL_BYTES`] -- a COMBINED
/// ceiling, so one source cannot spend the other's budget.
///
/// One payload therefore holds at most `longest modeled display value +
/// PROBE_BETA_MAX_TOTAL_BYTES` bytes, and the beta half is the only part a
/// caller influences at all. With the queue depth bounding the tracked-job
/// count, total retained payload bytes are bounded by depth times that sum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePayload {
    /// The closed-table dotted path under test. A `&'static str` from the
    /// table itself, never upstream text.
    field_path: &'static str,
    /// The value the admitted request carried for that field. Always one of
    /// [`PROBE_MODELED_DISPLAY_VALUES`] -- anything else is refused at capture.
    field_value: String,
    /// The CLIENT/ingress beta set the admitted request carried on
    /// `anthropic_beta`.
    ///
    /// Stored SEPARATELY from the operator set, never unioned. The egress
    /// treats the two differently: the client set is subject to the
    /// per-provider `allowed_betas` allowlist, while the operator set bypasses
    /// it unconditionally. Collapsing them into one list and reapplying it to
    /// `operator_betas` would smuggle a client flag past the very allowlist
    /// that filtered it out of the admitted request -- the probe's header
    /// would then be WIDER than the header under test.
    client_betas: Vec<String>,
    /// The OPERATOR beta floor the admitted request carried on
    /// `routectl_internal.operator_betas`, composed by the dispatch overlay
    /// from provider and model `header_extras`.
    operator_betas: Vec<String>,
    /// Whether the admitted request presented a genuine Claude Code session
    /// capture.
    ///
    /// Retained as a BIT, not as the headers: the egress's `is_non_cc`
    /// decision gates the Claude-Code beta floor, so a probe classified
    /// differently from the request it probes for would be sent a different
    /// header. Presence is all that decision reads, and the session id itself
    /// is a correlation identifier a background scheduler has no business
    /// holding across a queueing delay.
    originating_claude_code_session: bool,
}

impl ProbePayload {
    /// Build a payload carrying the admitted request's field and its beta
    /// context, or `None` when any retention bound or validity rule is
    /// breached.
    ///
    /// `None` SKIPS activation. Nothing is truncated, trimmed-and-kept, or
    /// partially stored: an unmodeled display token is a different,
    /// meaningless token, and a reduced beta set asks a different question
    /// than the admitted request posed while its answer would still be
    /// attributed to the field.
    ///
    /// THE constructor for every caller: the fields are private, so a struct
    /// literal anywhere outside this module -- including a sibling module in
    /// this same crate -- is `E0451`. That makes the bounds below unskippable
    /// rather than merely conventional.
    #[must_use]
    pub fn new(
        field_path: &'static str,
        field_value: String,
        client_betas: &[String],
        operator_betas: &[String],
        originating_claude_code_session: bool,
    ) -> Option<Self> {
        // The vocabulary IS the bound. A separate byte ceiling existed here and
        // was unreachable: every accepted value is one of three fixed literals,
        // so no value inside the vocabulary can exceed a length the vocabulary
        // does not contain. See `PROBE_MODELED_DISPLAY_VALUES` for why the
        // restriction is a denial-of-probing guard rather than a memory one.
        if !PROBE_MODELED_DISPLAY_VALUES.contains(&field_value.as_str()) {
            return None;
        }
        let client_betas = bounded_source(client_betas)?;
        let operator_betas = bounded_source(operator_betas)?;
        let total: usize = client_betas
            .iter()
            .chain(operator_betas.iter())
            .map(String::len)
            .sum();
        if total > PROBE_BETA_MAX_TOTAL_BYTES {
            return None;
        }
        Some(Self {
            field_path,
            field_value,
            client_betas,
            operator_betas,
            originating_claude_code_session,
        })
    }

    /// The closed-table dotted path under test.
    #[must_use]
    pub const fn field_path(&self) -> &'static str {
        self.field_path
    }

    /// The captured field value, always one of [`PROBE_MODELED_DISPLAY_VALUES`].
    #[must_use]
    pub fn field_value(&self) -> &str {
        &self.field_value
    }

    /// The captured CLIENT/ingress beta set, for the `anthropic_beta` carrier.
    #[must_use]
    pub fn client_betas(&self) -> &[String] {
        &self.client_betas
    }

    /// The captured OPERATOR beta floor, for the `operator_betas` carrier.
    #[must_use]
    pub fn operator_betas(&self) -> &[String] {
        &self.operator_betas
    }

    /// Whether the admitted request presented a genuine Claude Code session.
    #[must_use]
    pub const fn originating_claude_code_session(&self) -> bool {
        self.originating_claude_code_session
    }
}
