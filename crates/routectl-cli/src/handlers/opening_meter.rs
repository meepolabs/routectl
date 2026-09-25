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

use std::sync::Arc;

use routectl_core::{ChatChunk, ChatRequest, OpeningUsageOrigin, UsageInputSource};
use routectl_router::{DispatchMeta, Router, estimate_request};

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
            }) => "upstream_wire",
            Self::UpstreamWire(WireOpening {
                from_vendor_endpoint: false,
                ..
            }) => "upstream_wire_unverified",
            Self::Selected(selection) => selection.source.as_str(),
        }
    }

    /// Stable, log-safe label of why the anchor tier did or did not apply.
    pub const fn reason_label(self) -> &'static str {
        match self {
            Self::UpstreamWire(_) => "upstream_opener",
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
    finish_seen: bool,
    stopped: bool,
    terminal_input: Option<u64>,
}

/// One chunk's terminal-input evidence, read before rendering.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCandidate {
    finishes: bool,
    input: Option<(u64, Option<UsageInputSource>)>,
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
            finish_seen: false,
            stopped: false,
            terminal_input: None,
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

    /// Observe the chunk that opens the client stream on the fast path.
    /// A no-op once the opening is decided.
    pub fn observe_opening_chunk(&mut self, chunk: &ChatChunk) {
        let OpeningState::Seeded(selection) = self.opening else {
            return;
        };
        let wire = chunk
            .upstream_meta
            .as_ref()
            .and_then(|meta| meta.opening_usage.as_ref())
            .map(|opening| WireOpening {
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
                let source = chunk
                    .upstream_meta
                    .as_ref()
                    .and_then(|meta| meta.usage_input_source);
                (u64::from(prompt), source)
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
    pub fn accept_rendered(&mut self, candidate: TerminalCandidate, events: &[SseEvent]) {
        if self.stopped {
            return;
        }
        self.finish_seen |= candidate.finishes;
        if self.finish_seen
            && let Some((prompt, source)) = candidate.input
        {
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
    }

    /// Settle a naturally completed turn. Publishes only when accepted
    /// terminal evidence reported a nonzero cache-inclusive input total.
    pub fn settle_completed(self) -> SettleOutcome {
        let origin = self.opening();
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
        log_settled_turn(origin, anchored, settled);
        settled
    }

    fn select(&self, router: &Router, basis: OpeningLaneBasis<'_>) -> OpeningSelection {
        let probe = self.anchor.as_ref().map(|anchor| AnchorProbe {
            record: anchor.record.as_deref(),
            identity: anchor.pending.identity(),
        });
        let calibration_raw = self.calibration_raw_tokens;
        select_opening(probe, basis, self.raw_tokens, |lane, _| {
            router.calibrated_estimate(&lane.provider_kind, &lane.model, calibration_raw)
        })
    }
}

fn log_settled_turn(origin: Option<OpeningOrigin>, anchored: bool, settled: SettleOutcome) {
    let Some(origin) = origin else {
        return;
    };
    tracing::debug!(
        opening_source = origin.source_label(),
        opening_reason = origin.reason_label(),
        opening_provisional = origin.is_provisional(),
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
