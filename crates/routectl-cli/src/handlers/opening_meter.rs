//! The client context meter's opening count for one streamed turn, on a
//! dialect whose first stream event reports input usage (Anthropic
//! `message_start`).
//!
//! Admission reserves the turn's place in the session anchor order FIRST,
//! then measures the inbound request (digest and normalized size), so a
//! large older request cannot take a newer place by finishing its hash
//! later. Everything the opening depends on is read off the one Router the
//! request holds: the route head, the calibrated estimate and the
//! publication generation both lanes carry.
//!
//! Which count opens the stream:
//!
//! - fast path (the dispatch resolved inside the flush grace): the winning
//!   attempt's own first-event usage when its opening chunk carries one
//!   (the renderer takes it field for field), else the selection against
//!   the SERVED lane, which is final;
//! - warm path (the grace expired first): the selection against the route
//!   HEAD, flushed once as the provisional opening; the later upstream
//!   opener is not rendered and the terminal `message_delta` corrects it.
//!
//! Only a natural, fully delivered completion publishes an anchor, and only
//! from terminal input evidence: usage the renderer accepted on the finish
//! chunk or a usage-only chunk after it, before it emitted `message_stop`,
//! whose parser marked the input as the upstream's own closing report or
//! the vendor's own opening measurement (`UsageInputSource`). A usage-only
//! chunk before the finish is interim; a proxy-reported opening, an interim
//! carry and an unmarked count are not evidence; a straggler after the stop
//! is never seen. Every other end drops the pending turn, which publishes
//! nothing.
//!
//! [`OpeningMeter::diagnostics`] snapshots what the turn's usage-ledger row
//! records about the opening (see `opening_diagnostics`).

use std::sync::Arc;
use std::time::Instant;

use routectl_core::{ChatChunk, ChatRequest, OpeningUsage, OpeningUsageOrigin, UsageInputSource};
use routectl_router::{DispatchMeta, Router, estimate_request};

use crate::handlers::opening_diagnostics::{
    OpeningDiagnostics, OpeningFacts, TerminalReport, UPSTREAM_OPENER_REASON, source,
};

use crate::ingress::anthropic::context_anchor::{
    AnchorKey, AnchorLane, AnchorProbe, AnchorRecord, ContextAnchorStore, OpeningLaneBasis,
    OpeningSelection, PendingAnchor, SettleOutcome, TurnOutcome, select_opening,
};
use crate::ingress::{SseEvent, StreamRequestContext};

/// Where a turn's rendered opening count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpeningOrigin {
    /// The winning attempt's own first-event usage, rendered verbatim.
    UpstreamWire(WireOpening),
    /// A selected estimate: anchor, calibrated or raw.
    Selected(OpeningSelection),
}

/// Who reported an upstream-wire opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireOpening {
    /// The wire the opening was parsed from.
    pub origin: OpeningUsageOrigin,
    /// The vendor's own endpoint reported it. Otherwise it is only what an
    /// Anthropic-compatible immediate upstream reported, which says nothing
    /// verified about that upstream's own backend.
    pub from_vendor_endpoint: bool,
}

impl OpeningOrigin {
    /// Stable, log-safe label of the opening source.
    pub const fn source_label(self) -> &'static str {
        match self {
            Self::UpstreamWire(WireOpening {
                from_vendor_endpoint: true,
                ..
            }) => source::UPSTREAM_WIRE,
            Self::UpstreamWire(WireOpening {
                from_vendor_endpoint: false,
                ..
            }) => source::UPSTREAM_WIRE_UNVERIFIED,
            Self::Selected(selection) => selection.source.as_str(),
        }
    }

    /// Stable, log-safe label of why the anchor tier did or did not apply.
    pub const fn reason_label(self) -> &'static str {
        match self {
            Self::UpstreamWire(_) => UPSTREAM_OPENER_REASON,
            Self::Selected(selection) => selection.reason.as_str(),
        }
    }

    /// Chosen before the winning lane was known.
    pub const fn is_provisional(self) -> bool {
        match self {
            Self::UpstreamWire(_) => false,
            Self::Selected(selection) => selection.provisional,
        }
    }
}

/// Progress of the opening frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpeningState {
    /// Nothing chosen yet.
    Pending,
    /// The fast path seeded the served-lane selection; the first chunk
    /// decides whether the upstream opener replaces it.
    Seeded(OpeningSelection),
    /// The opening frame is decided.
    Rendered(OpeningOrigin),
}

/// What the serving attempt's own first event reported, whether or not it
/// became the client's opening.
#[derive(Debug, Clone, Copy)]
struct UpstreamOpener {
    observed_at: Instant,
    cache_inclusive_input: u64,
}

impl UpstreamOpener {
    fn of(opening: &OpeningUsage) -> Self {
        Self {
            observed_at: opening.observed_at,
            cache_inclusive_input: cache_inclusive_opening_input(opening),
        }
    }
}

/// The input total the rendered `message_start` shows: `input_tokens` plus
/// the disjoint write and read fields. The per-TTL breakdown splits the
/// write field, so it is not added again.
fn cache_inclusive_opening_input(opening: &OpeningUsage) -> u64 {
    u64::from(opening.input_tokens)
        + u64::from(opening.cache_creation_input_tokens.unwrap_or(0))
        + u64::from(opening.cache_read_input_tokens.unwrap_or(0))
}

/// One keyed turn: the record it is checked against and its pending
/// publication.
struct AnchorTurn {
    record: Option<Arc<AnchorRecord>>,
    pending: PendingAnchor,
}

/// Opening-count state for one streamed turn.
pub struct OpeningMeter {
    raw_tokens: u64,
    calibration_raw_tokens: u64,
    generation: u64,
    head_lane: Option<AnchorLane>,
    served_lane: Option<AnchorLane>,
    anchor: Option<AnchorTurn>,
    opening: OpeningState,
    upstream_opener: Option<UpstreamOpener>,
    finish_seen: bool,
    stopped: bool,
    terminal_input: Option<u64>,
    terminal_report: TerminalReport,
}

/// One chunk's terminal-input evidence, read before rendering.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCandidate {
    finishes: bool,
    input: Option<(u64, Option<UsageInputSource>, Option<bool>)>,
}

/// The Anthropic SSE event that ends a message.
const MESSAGE_STOP_EVENT: &str = "message_stop";

impl OpeningMeter {
    /// Admit one inbound request. A request with no session key, or a key
    /// or requested model past its byte bound, is not anchored; it still
    /// gets a calibrated or raw opening.
    pub fn admit(store: &ContextAnchorStore, router: &Router, req: &ChatRequest) -> Self {
        let ticket = req
            .routectl_internal
            .inbound_session_key
            .as_deref()
            .and_then(|session| AnchorKey::new(session, &req.model))
            .map(|key| store.reserve_turn(key));
        let anchor = ticket.map(|ticket| {
            let record = ticket.current_record();
            let prior_message_count = record.as_ref().map(|record| record.message_count());
            AnchorTurn {
                pending: ticket.measure(req, prior_message_count),
                record,
            }
        });
        let estimate = estimate_request(req);
        Self {
            raw_tokens: estimate.meter,
            calibration_raw_tokens: estimate.total,
            generation: router.publication_generation(),
            head_lane: router.opening_lane(&req.model).map(AnchorLane::from),
            served_lane: None,
            anchor,
            opening: OpeningState::Pending,
            upstream_opener: None,
            finish_seen: false,
            stopped: false,
            terminal_input: None,
            terminal_report: TerminalReport::Missing,
        }
    }

    /// The uncorrected display estimate of the request.
    pub const fn raw_tokens(&self) -> u64 {
        self.raw_tokens
    }

    /// The opening's provenance: what the opening frame carries, or will
    /// carry if no chunk supersedes a fast-path seed. `None` before any
    /// opening is chosen.
    pub const fn opening(&self) -> Option<OpeningOrigin> {
        match self.opening {
            OpeningState::Rendered(origin) => Some(origin),
            OpeningState::Seeded(selection) => Some(OpeningOrigin::Selected(selection)),
            OpeningState::Pending => None,
        }
    }

    /// Pick the opening against the route head for the provisional frame
    /// the warm path flushes at the grace deadline.
    pub fn provisional_opening(&mut self, router: &Router) -> u64 {
        let selection = self.select(router, OpeningLaneBasis::head(self.head_lane.as_ref()));
        self.opening = OpeningState::Rendered(OpeningOrigin::Selected(selection));
        selection.tokens
    }

    /// Record the lane that won the dispatch.
    pub fn bind_served(&mut self, meta: &DispatchMeta) {
        self.served_lane = AnchorLane::served(meta, self.generation);
    }

    /// Record the winning lane and pick the final opening against it, for
    /// a dispatch that resolved inside the grace.
    pub fn served_opening(&mut self, meta: &DispatchMeta, router: &Router) -> u64 {
        self.bind_served(meta);
        let selection = self.select(router, OpeningLaneBasis::served(self.served_lane.as_ref()));
        self.opening = OpeningState::Seeded(selection);
        selection.tokens
    }

    /// Observe one chunk before it is rendered: note the serving attempt's
    /// own first event wherever it rides, and, for the chunk that opens the
    /// client stream on the fast path, whether that event is the opening.
    /// The opening is decided by the first chunk only.
    pub fn observe_opening_chunk(&mut self, chunk: &ChatChunk) {
        let opening_usage = chunk
            .upstream_meta
            .as_ref()
            .and_then(|meta| meta.opening_usage.as_ref());
        if self.upstream_opener.is_none() {
            self.upstream_opener = opening_usage.map(UpstreamOpener::of);
        }
        let OpeningState::Seeded(selection) = self.opening else {
            return;
        };
        let wire = opening_usage.map(|opening| WireOpening {
            origin: opening.origin,
            from_vendor_endpoint: opening.from_vendor_endpoint,
        });
        self.opening = OpeningState::Rendered(wire.map_or(
            OpeningOrigin::Selected(selection),
            OpeningOrigin::UpstreamWire,
        ));
    }

    /// Read one chunk's terminal-input evidence BEFORE the renderer
    /// consumes it; apply the result with [`Self::accept_rendered`] once the
    /// chunk has been rendered.
    pub fn terminal_candidate(chunk: &ChatChunk) -> TerminalCandidate {
        let input = chunk
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens)
            .map(|prompt| {
                let meta = chunk.upstream_meta.as_ref();
                (
                    u64::from(prompt),
                    meta.and_then(|meta| meta.usage_input_source),
                    meta.and_then(|meta| meta.usage_from_vendor_endpoint),
                )
            });
        TerminalCandidate {
            // The Anthropic renderer reads the first choice only.
            finishes: chunk
                .choices
                .first()
                .is_some_and(|choice| choice.finish_reason.is_some()),
            input,
        }
    }

    /// Apply a rendered chunk's candidate, then freeze if the render
    /// emitted `message_stop`.
    ///
    /// Nothing after the stop counts: the renderer drops such chunks, so
    /// their usage never reached the client. Before it, a usage-bearing
    /// chunk counts only from the finish chunk on, and only when its parser
    /// marked the input as terminal evidence.
    ///
    /// Returns whether the accepted terminal report changed, so its caller
    /// can refresh what the usage row records.
    pub fn accept_rendered(&mut self, candidate: TerminalCandidate, events: &[SseEvent]) -> bool {
        if self.stopped {
            return false;
        }
        let before = self.terminal_report;
        self.finish_seen |= candidate.finishes;
        if self.finish_seen
            && let Some((prompt, source, from_vendor)) = candidate.input
        {
            self.terminal_report = TerminalReport::Reported {
                input: prompt,
                source,
                from_vendor,
            };
            self.terminal_input = source
                .filter(|source| source.is_terminal_evidence())
                .map(|_| prompt)
                .filter(|prompt| *prompt > 0);
        }
        if events
            .iter()
            .any(|event| event.event.as_deref() == Some(MESSAGE_STOP_EVENT))
        {
            self.stopped = true;
        }
        self.terminal_report != before
    }

    /// Settle a naturally completed turn. Publishes only when accepted
    /// terminal evidence reported a nonzero cache-inclusive input total.
    pub fn settle_completed(self) -> SettleOutcome {
        let origin = self.opening();
        let terminal = self.terminal_report;
        let terminal_vendor_verified = terminal.vendor_verified();
        let outcome = match self.served_lane {
            Some(served_lane) => TurnOutcome::Completed {
                served_lane,
                cache_inclusive_input: self.terminal_input,
            },
            None => TurnOutcome::Failed,
        };
        let anchored = self.anchor.is_some();
        let settled = self.anchor.map_or(SettleOutcome::NotPublished, |anchor| {
            anchor.pending.settle(outcome)
        });
        log_settled_turn(
            origin,
            SettledTerminal {
                label: terminal.label(),
                vendor_verified: terminal_vendor_verified,
            },
            anchored,
            settled,
        );
        settled
    }

    /// What the turn's usage row records about its opening, as of now.
    pub fn diagnostics(&self) -> OpeningDiagnostics {
        OpeningDiagnostics {
            opening: self.opening().map(|origin| self.opening_facts(origin)),
            upstream_first_event_at: self.upstream_opener.map(|opener| opener.observed_at),
            terminal: self.terminal_report,
        }
    }

    fn opening_facts(&self, origin: OpeningOrigin) -> OpeningFacts {
        let (input, selected_on_head) = match origin {
            OpeningOrigin::UpstreamWire(_) => (
                self.upstream_opener
                    .map(|opener| opener.cache_inclusive_input),
                false,
            ),
            OpeningOrigin::Selected(selection) => (Some(selection.tokens), selection.provisional),
        };
        let lane_switched = self
            .served_lane
            .as_ref()
            .map(|served| selected_on_head && self.head_lane.as_ref() != Some(served));
        OpeningFacts {
            source: origin.source_label(),
            reason: origin.reason_label(),
            input,
            provisional: origin.is_provisional(),
            lane_switched,
        }
    }

    fn select(&self, router: &Router, basis: OpeningLaneBasis<'_>) -> OpeningSelection {
        let probe = self.anchor.as_ref().map(|anchor| AnchorProbe {
            record: anchor.record.as_deref(),
            identity: anchor.pending.identity(),
        });
        let calibration_raw = self.calibration_raw_tokens;
        select_opening(probe, basis, self.raw_tokens, |lane, _| {
            router.calibrated_estimate(&lane.provider_kind, &lane.nickname, calibration_raw)
        })
    }
}

/// The terminal-input labels a settled turn logs.
struct SettledTerminal {
    label: &'static str,
    vendor_verified: bool,
}

fn log_settled_turn(
    origin: Option<OpeningOrigin>,
    terminal: SettledTerminal,
    anchored: bool,
    settled: SettleOutcome,
) {
    let Some(origin) = origin else {
        return;
    };
    tracing::debug!(
        opening_source = origin.source_label(),
        opening_reason = origin.reason_label(),
        opening_provisional = origin.is_provisional(),
        terminal_source = terminal.label,
        terminal_vendor_verified = terminal.vendor_verified,
        anchored,
        anchor_published = matches!(settled, SettleOutcome::Published),
        "context meter opening settled",
    );
}

/// Request-scoped seeds a streamed turn carries into its render task.
pub struct StreamTurn {
    pub session_key: Option<String>,
    pub ctx: StreamRequestContext,
    pub meter: Option<OpeningMeter>,
}

impl StreamTurn {
    /// A turn whose dialect reports no opening count.
    #[cfg(test)]
    pub const fn unmetered(session_key: Option<String>, ctx: StreamRequestContext) -> Self {
        Self {
            session_key,
            ctx,
            meter: None,
        }
    }

    /// Fast path: seed the opening from the lane that won.
    pub fn seed_served(&mut self, meta: &DispatchMeta, router: &Router) {
        if let Some(meter) = self.meter.as_mut() {
            self.ctx.input_tokens_estimate = meter.served_opening(meta, router);
        }
    }

    /// Warm path: seed the provisional opening from the route head.
    pub fn seed_provisional(&mut self, router: &Router) {
        if let Some(meter) = self.meter.as_mut() {
            self.ctx.input_tokens_estimate = meter.provisional_opening(router);
        }
    }
}

#[cfg(test)]
#[path = "opening_meter_tests.rs"]
mod tests;
