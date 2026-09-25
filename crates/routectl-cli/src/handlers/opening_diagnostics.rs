//! Bounded diagnostics of one Anthropic stream's context meter opening, and
//! the vocabulary they are persisted under in the usage ledger's existing
//! `extra` JSON column (no schema change).
//!
//! Every value is a closed-set label, a token count, a boolean or a duration
//! in milliseconds. Counts, booleans and durations are JSON numbers and
//! booleans, never strings. Nothing here carries a session id, prompt text,
//! account or seat name, URL or error text.
//!
//! Only a metered stream (a dialect whose first event reports input usage)
//! writes these keys; every other row leaves `extra` as it was.
//!
//! A row either has an opening (`opening_present = true`: the
//! `message_start` event was enqueued for the client) or explicitly does not
//! (`opening_present = false`: a pre-opening HTTP error, a stream that errors
//! or fails to render before its first event, or a client that left before
//! one). With an opening, the row also carries:
//!
//! - `opening_source`: `upstream_wire` (the vendor's own first event),
//!   `upstream_wire_unverified` (an Anthropic-compatible endpoint's first
//!   event, whatever that endpoint's own backend is), `anchor`, `calibrated`
//!   or `raw`;
//! - `opening_reason`: why the anchor tier did or did not apply, decided
//!   before dispatch (`upstream_opener` for an upstream-wire opening);
//! - `opening_input`: the cache-inclusive input the client frame reported,
//!   `input_tokens` plus the disjoint cache write and read fields, each once
//!   (the per-TTL write breakdown is a split of the write field, not added);
//! - `opening_provisional`: chosen before the serving lane was known;
//! - `opening_lane_switched`: the opening was selected against a lane other
//!   than the one that served. Absent when no lane served;
//! - `opening_first_event_ms`: request start to the upstream's own first
//!   event, when the serving attempt sent one. First CONTENT is the row's
//!   `ttfb_ms` column;
//! - `opening_first_enqueue_ms`: request start to the first body event
//!   handed to the response's SSE channel. A server-side enqueue time, not
//!   the moment the client received it;
//! - `terminal_source`: provenance of the terminal input the renderer
//!   accepted, one of the [`terminal`] labels;
//! - `terminal_input`: that terminal's cache-inclusive input, when reported;
//! - `terminal_vendor_verified`: the terminal input is established as the
//!   vendor's own measurement, from the endpoint the parser knows it read
//!   (the first-party API host or an AWS Bedrock stream), never from two
//!   numbers agreeing. `false` is "not established", which is every report
//!   an intermediate endpoint relayed, an explicit final report included: it
//!   says what the immediate upstream reported, not how that upstream's
//!   backend measured it.
//!
//! Everything but the terminal fields and the timings is fixed before the
//! terminal usage arrives.

use std::time::Instant;

use routectl_core::UsageInputSource;
use serde_json::Value;

use crate::ingress::anthropic::context_anchor::{MissReason, OpeningReason, OpeningSource};

/// Ledger `extra` keys.
pub mod key {
    /// Boolean: the stream's opening frame was enqueued for the client.
    pub const OPENING_PRESENT: &str = "opening_present";
    /// Label: which tier produced the opening count.
    pub const OPENING_SOURCE: &str = "opening_source";
    /// Label: why the anchor tier did or did not apply.
    pub const OPENING_REASON: &str = "opening_reason";
    /// Number: cache-inclusive opening input the client frame reported.
    pub const OPENING_INPUT: &str = "opening_input";
    /// Boolean: the opening was chosen before the serving lane was known.
    pub const OPENING_PROVISIONAL: &str = "opening_provisional";
    /// Boolean: the opening was selected against a non-serving lane.
    pub const OPENING_LANE_SWITCHED: &str = "opening_lane_switched";
    /// Number: milliseconds from request start to the upstream first event.
    pub const OPENING_FIRST_EVENT_MS: &str = "opening_first_event_ms";
    /// Number: milliseconds from request start to the first body event
    /// enqueued for the client (server side, not client receipt).
    pub const OPENING_FIRST_ENQUEUE_MS: &str = "opening_first_enqueue_ms";
    /// Label: provenance of the accepted terminal input.
    pub const TERMINAL_SOURCE: &str = "terminal_source";
    /// Number: cache-inclusive input of the accepted terminal.
    pub const TERMINAL_INPUT: &str = "terminal_input";
    /// Boolean: the terminal input is established as the vendor's own.
    pub const TERMINAL_VENDOR_VERIFIED: &str = "terminal_vendor_verified";

    /// Every key above.
    pub const ALL: [&str; 11] = [
        OPENING_PRESENT,
        OPENING_SOURCE,
        OPENING_REASON,
        OPENING_INPUT,
        OPENING_PROVISIONAL,
        OPENING_LANE_SWITCHED,
        OPENING_FIRST_EVENT_MS,
        OPENING_FIRST_ENQUEUE_MS,
        TERMINAL_SOURCE,
        TERMINAL_INPUT,
        TERMINAL_VENDOR_VERIFIED,
    ];
}

/// `opening_source` labels of an upstream-wire opening. A selected opening
/// is labelled by `OpeningSource::as_str` (`anchor`, `calibrated`, `raw`).
pub mod source {
    /// The vendor's own first-event usage, rendered verbatim.
    pub const UPSTREAM_WIRE: &str = "upstream_wire";
    /// An Anthropic-compatible endpoint's first-event usage, rendered
    /// verbatim; says nothing verified about that endpoint's backend.
    pub const UPSTREAM_WIRE_UNVERIFIED: &str = "upstream_wire_unverified";
}

/// `opening_reason` label of an upstream-wire opening. A selected opening
/// is labelled by `OpeningReason::as_str`.
pub const UPSTREAM_OPENER_REASON: &str = "upstream_opener";

/// `terminal_source` labels.
pub mod terminal {
    /// The closing event reported the input itself.
    pub const EXPLICIT_FINAL: &str = "explicit_final";
    /// Carried from the vendor's own first event.
    pub const VENDOR_OPENING: &str = "vendor_opening";
    /// Carried from an Anthropic-compatible endpoint's first event.
    pub const PROXY_OPENING: &str = "proxy_opening";
    /// Reported before the terminal event and carried onto it.
    pub const INTERIM_CARRY: &str = "interim_carry";
    /// The closing event reported some input components, not the uncached
    /// input itself.
    pub const PARTIAL_FINAL: &str = "partial_final";
    /// A prompt count with no provenance from its parser.
    pub const UNMARKED: &str = "unmarked";
    /// A provenance this build does not name.
    pub const UNRECOGNIZED: &str = "unrecognized";
    /// No accepted terminal reported input.
    pub const MISSING: &str = "missing";
}

/// Every `opening_source` label a metered row can carry.
pub const OPENING_SOURCE_LABELS: &[&str] = &[
    OpeningSource::Anchor.as_str(),
    OpeningSource::Calibrated.as_str(),
    OpeningSource::Raw.as_str(),
    source::UPSTREAM_WIRE,
    source::UPSTREAM_WIRE_UNVERIFIED,
];

/// Every `opening_reason` label a metered row can carry.
pub const OPENING_REASON_LABELS: &[&str] = &[
    OpeningReason::AnchorHit.as_str(),
    OpeningReason::Unanchored.as_str(),
    OpeningReason::LaneUnresolved.as_str(),
    MissReason::Cold.as_str(),
    MissReason::LaneChanged.as_str(),
    MissReason::GenerationChanged.as_str(),
    MissReason::HistoryShrank.as_str(),
    MissReason::PrefixChanged.as_str(),
    UPSTREAM_OPENER_REASON,
];

/// Every `terminal_source` label a metered row can carry.
pub const TERMINAL_SOURCE_LABELS: &[&str] = &[
    terminal::EXPLICIT_FINAL,
    terminal::VENDOR_OPENING,
    terminal::PROXY_OPENING,
    terminal::INTERIM_CARRY,
    terminal::PARTIAL_FINAL,
    terminal::UNMARKED,
    terminal::UNRECOGNIZED,
    terminal::MISSING,
];

/// Whether a persisted `terminal_source` label names terminal input evidence
/// (`UsageInputSource::is_terminal_evidence`), for a reader of the ledger
/// that has only the label. Unknown labels are not evidence.
pub fn is_terminal_evidence_label(label: &str) -> bool {
    NAMED_INPUT_SOURCES.into_iter().any(|source| {
        source.is_terminal_evidence()
            && TerminalReport::Reported {
                input: 0,
                source: Some(source),
                from_vendor: None,
            }
            .label()
                == label
    })
}

/// Every provenance [`TerminalReport::label`] names.
const NAMED_INPUT_SOURCES: [UsageInputSource; 5] = [
    UsageInputSource::ExplicitFinal,
    UsageInputSource::VendorOpening,
    UsageInputSource::ProxyOpening,
    UsageInputSource::InterimCarry,
    UsageInputSource::PartialFinal,
];

/// The facts of a chosen opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpeningFacts {
    /// `opening_source` label.
    pub source: &'static str,
    /// `opening_reason` label.
    pub reason: &'static str,
    /// Cache-inclusive opening input; `None` only if it cannot be stated.
    pub input: Option<u64>,
    /// Chosen before the serving lane was known.
    pub provisional: bool,
    /// Selected against a non-serving lane; `None` when no lane served.
    pub lane_switched: Option<bool>,
}

/// What the accepted terminal said about input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReport {
    /// No accepted terminal reported input.
    Missing,
    /// An accepted terminal reported `input`, with its parser's provenance.
    Reported {
        /// Cache-inclusive input.
        input: u64,
        /// Parser provenance; `None` when the parser stated none.
        source: Option<UsageInputSource>,
        /// Whether the parser read it from the vendor's own endpoint;
        /// `None` when the parser did not say.
        from_vendor: Option<bool>,
    },
}

impl TerminalReport {
    /// `terminal_source` label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => terminal::MISSING,
            Self::Reported { source: None, .. } => terminal::UNMARKED,
            Self::Reported {
                source: Some(source),
                ..
            } => match source {
                UsageInputSource::ExplicitFinal => terminal::EXPLICIT_FINAL,
                UsageInputSource::VendorOpening => terminal::VENDOR_OPENING,
                UsageInputSource::ProxyOpening => terminal::PROXY_OPENING,
                UsageInputSource::InterimCarry => terminal::INTERIM_CARRY,
                UsageInputSource::PartialFinal => terminal::PARTIAL_FINAL,
                _ => terminal::UNRECOGNIZED,
            },
        }
    }

    const fn input(self) -> Option<u64> {
        match self {
            Self::Missing => None,
            Self::Reported { input, .. } => Some(input),
        }
    }

    /// Whether the input is established as the vendor's own measurement.
    /// A vendor-opening carry is by construction. An explicit final report
    /// is only when its parser read it from the vendor's own endpoint;
    /// otherwise it is what an intermediate endpoint relayed.
    pub const fn vendor_verified(self) -> bool {
        match self {
            Self::Reported {
                source: Some(UsageInputSource::VendorOpening),
                ..
            } => true,
            Self::Reported {
                source: Some(UsageInputSource::ExplicitFinal),
                from_vendor,
                ..
            } => matches!(from_vendor, Some(true)),
            _ => false,
        }
    }
}

/// One metered stream's diagnostics at a point in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpeningDiagnostics {
    /// The chosen opening; `None` before any is chosen.
    pub opening: Option<OpeningFacts>,
    /// When the serving attempt's own first event was parsed.
    pub upstream_first_event_at: Option<Instant>,
    /// What the accepted terminal said about input.
    pub terminal: TerminalReport,
}

impl OpeningDiagnostics {
    /// The `extra` entries for a row whose request started at `start` and
    /// whose first body event was enqueued for the client at
    /// `first_enqueue_at` (`None` if none was). Without both a chosen
    /// opening and an enqueued body event the row explicitly has no opening.
    pub fn extra_entries(
        &self,
        start: Instant,
        first_enqueue_at: Option<Instant>,
    ) -> Vec<(&'static str, Value)> {
        let (Some(opening), Some(first_enqueue_at)) = (self.opening, first_enqueue_at) else {
            return vec![(key::OPENING_PRESENT, Value::Bool(false))];
        };
        let mut entries = vec![
            (key::OPENING_PRESENT, Value::Bool(true)),
            (key::OPENING_SOURCE, Value::from(opening.source)),
            (key::OPENING_REASON, Value::from(opening.reason)),
            (key::OPENING_PROVISIONAL, Value::Bool(opening.provisional)),
            (
                key::OPENING_FIRST_ENQUEUE_MS,
                Value::from(millis_since(start, first_enqueue_at)),
            ),
            (key::TERMINAL_SOURCE, Value::from(self.terminal.label())),
            (
                key::TERMINAL_VENDOR_VERIFIED,
                Value::Bool(self.terminal.vendor_verified()),
            ),
        ];
        if let Some(input) = opening.input {
            entries.push((key::OPENING_INPUT, Value::from(input)));
        }
        if let Some(switched) = opening.lane_switched {
            entries.push((key::OPENING_LANE_SWITCHED, Value::Bool(switched)));
        }
        if let Some(at) = self.upstream_first_event_at {
            entries.push((
                key::OPENING_FIRST_EVENT_MS,
                Value::from(millis_since(start, at)),
            ));
        }
        if let Some(input) = self.terminal.input() {
            entries.push((key::TERMINAL_INPUT, Value::from(input)));
        }
        entries
    }
}

fn millis_since(start: Instant, at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "opening_diagnostics_tests.rs"]
mod tests;
