//! The response leg of the live wire check: the serving attempt's tap rides
//! out of the dispatch on [`DispatchMeta`] as a [`ResponseCheck`], and the
//! ingress renderer hands it the document the client received once the
//! response is over. The check compares that document with what the
//! upstream sent and records the outcome like the request leg, with leg
//! `response`.
//!
//! Only an attempt the sampler admitted on a FIDELITY lane carries one; every
//! other attempt leaves the carrier empty and pays nothing on the response.
//! Reads only: nothing here changes what the client receives, and every
//! failure to build a document is `incomplete`, never `clean`.
//!
//! A stream has no complete body for the providers and the renderer to
//! trace, so a stream whose two documents both assemble emits them here,
//! through the same TRACE body lines a complete response emits. A
//! TRANSLATION-lane stream folded while the body trace was on carries a
//! trace-only check: it traces its two documents the same way and compares
//! nothing, since the two sides speak different dialects.

use std::fmt;
use std::sync::Arc;

use routectl_core::wire_check::{
    Assembled, DeclaredEdit, EditKind, EditSite, IncompleteReason, Lane, LaneClass, Leg, PathClass,
    StreamExit, TapContents, WirePath, WireTap, diff,
};
use routectl_core::{ChatRequest, RequestObservation};
use serde_json::Value;

use crate::wire_check_state::WireCheckState;

use super::wire_check::{attempt_lane, count_wire_losses};
use super::{DispatchMeta, DispatchTarget, Router};

const LOG_TARGET: &str = "routectl_router::wire_check";

/// Largest complete body, client or upstream, compared inline; a larger one
/// records `budget` unparsed. The compare runs on the thread dropping the
/// response, and two parses plus the diff of a body near the stream
/// assembler's 16 MiB cap took 18-144 ms (`test-release`, one JSON array of
/// short text blocks vs one of short strings); at 2 MiB, 2-16 ms.
const MAX_INLINE_COMPARE_BYTES: usize = 2 * 1024 * 1024;

/// The response-direction check of the attempt that answered the client.
/// Opaque: it holds the attempt's tap, its lane, the router's wire-check
/// state and the request's observation slot, and exposes only the
/// comparison.
#[derive(Clone)]
pub struct ResponseCheck {
    lane: Lane,
    tap: WireTap,
    state: Arc<WireCheckState>,
    provider_id: Option<Arc<str>>,
    compares: bool,
    observation: Option<RequestObservation>,
}

impl fmt::Debug for ResponseCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseCheck")
            .field("lane", &self.lane.label())
            .field("compares", &self.compares)
            .finish_non_exhaustive()
    }
}

impl ResponseCheck {
    /// The lane the checked attempt travelled.
    pub const fn lane(&self) -> Lane {
        self.lane
    }

    /// Compare a complete response body the client received with the
    /// upstream's answer. `client_edits` are the edits the renderer
    /// declared while building `client_body`. Either body larger than 2 MiB
    /// records `budget` without being parsed.
    pub fn compare_body(self, client_body: &[u8], client_edits: &[DeclaredEdit]) {
        if !self.compares {
            return;
        }
        let declared = self.tap.take_response();
        let upstream_len = declared.response().map_or(0, |body| body.len());
        if client_body.len().max(upstream_len) > MAX_INLINE_COMPARE_BYTES {
            self.record_over_budget();
            return;
        }
        let client =
            serde_json::from_slice::<Value>(client_body).map_err(|_| IncompleteSide::Client(None));
        self.record(&declared, StreamExit::Ended, client, client_edits, None);
    }

    /// Compare the stream the client received, folded into `client`, with
    /// the upstream's stream; `exit` is how the stream ended. Either side
    /// unfinished records `incomplete` with its reason. Traces nothing; see
    /// [`Self::compare_traced_stream`].
    pub fn compare_stream(
        self,
        client: Assembled,
        exit: StreamExit,
        client_edits: &[DeclaredEdit],
    ) {
        self.compare_stream_as(client, exit, client_edits, None);
    }

    /// [`Self::compare_stream`] that also traces both documents when both
    /// assemble: the upstream's as the upstream success body, the client's
    /// as the egress response body under `ingress`, the id of the ingress
    /// adapter that rendered it.
    pub fn compare_traced_stream(
        self,
        client: Assembled,
        exit: StreamExit,
        client_edits: &[DeclaredEdit],
        ingress: &str,
    ) {
        self.compare_stream_as(client, exit, client_edits, Some(ingress));
    }

    fn compare_stream_as(
        self,
        client: Assembled,
        exit: StreamExit,
        client_edits: &[DeclaredEdit],
        traced_ingress: Option<&str>,
    ) {
        if !self.compares {
            self.trace_uncompared_stream(client, exit, traced_ingress);
            return;
        }
        let client = match client {
            Assembled::Complete(document) => Ok(document),
            Assembled::Incomplete(reason) => Err(IncompleteSide::Client(Some(reason))),
        };
        let declared = self.tap.take_response();
        self.record(&declared, exit, client, client_edits, traced_ingress);
    }

    /// Finish a trace-only check: trace both documents when both
    /// assembled, record nothing either way.
    fn trace_uncompared_stream(
        &self,
        client: Assembled,
        exit: StreamExit,
        traced_ingress: Option<&str>,
    ) {
        let upstream = self.tap.finish_response_stream(exit);
        if let (Some(ingress), Assembled::Complete(client), Some(Assembled::Complete(upstream))) =
            (traced_ingress, client, upstream)
        {
            self.trace_stream_documents(&upstream, &client, ingress);
        }
    }

    fn record_over_budget(self) {
        tracing::debug!(
            target: LOG_TARGET,
            lane = self.lane.label(),
            leg = Leg::Response.label(),
            "wire check skipped a response too large to compare inline"
        );
        self.state.record_compared(
            self.lane,
            Leg::Response,
            std::iter::empty::<(&WirePath, PathClass)>(),
            true,
        );
    }

    /// The upstream's answer: the complete body the provider deposited, or
    /// else the stream it folded, finished as `exit`.
    fn upstream_document(
        &self,
        deposited: Option<&[u8]>,
        exit: StreamExit,
    ) -> Result<Value, IncompleteSide> {
        if let Some(body) = deposited {
            return serde_json::from_slice(body).map_err(|_| IncompleteSide::Upstream(None));
        }
        match self.tap.finish_response_stream(exit) {
            Some(Assembled::Complete(document)) => Ok(document),
            Some(Assembled::Incomplete(reason)) => Err(IncompleteSide::Upstream(Some(reason))),
            None => Err(IncompleteSide::Upstream(None)),
        }
    }

    fn record_unfinished(self, side: IncompleteSide) {
        tracing::debug!(
            target: LOG_TARGET,
            lane = self.lane.label(),
            leg = Leg::Response.label(),
            side = side.label(),
            reason = side.reason_label(),
            "wire check had no complete response to compare"
        );
        self.state.record_incomplete(self.lane);
    }

    /// Compare `client` with the upstream's answer. A cut client side next
    /// to a deposited upstream body records `incomplete` without parsing
    /// that body: an unread drop lands here, and the deposit can reach the
    /// assembler's cap, past the inline limit. Otherwise an unfinished
    /// upstream side is named first. `traced_ingress`, when set, traces the
    /// two documents; a complete body never sets it, because its provider
    /// and its renderer already traced it.
    fn record(
        self,
        declared: &TapContents,
        exit: StreamExit,
        client: Result<Value, IncompleteSide>,
        client_edits: &[DeclaredEdit],
        traced_ingress: Option<&str>,
    ) {
        let deposited = declared.response().map(|body| &body[..]);
        let documents = match (deposited, client) {
            (Some(_), Err(side)) => Err(side),
            (deposited, client) => self
                .upstream_document(deposited, exit)
                .and_then(|upstream| client.map(|client| (upstream, client))),
        };
        let (upstream, client) = match documents {
            Ok(documents) => documents,
            Err(side) => return self.record_unfinished(side),
        };
        if let Some(ingress) = traced_ingress {
            self.trace_stream_documents(&upstream, &client, ingress);
        }
        let edits: Vec<DeclaredEdit> = declared
            .edits(Leg::Response)
            .iter()
            .chain(client_edits)
            .cloned()
            .collect();
        let outcome = diff(&upstream, &client, LaneClass::Fidelity, &edits);
        self.state.record_compared(
            self.lane,
            Leg::Response,
            outcome.findings.iter().map(|f| (&f.path, f.class)),
            outcome.truncated,
        );
        count_wire_losses(self.observation.as_ref(), &outcome);
    }

    /// Emit a finished stream's documents as the upstream success body and
    /// the egress response body. The upstream line carries the provider's
    /// own `provider_kind` token, the one its outgoing request line carries,
    /// because a fixture capture keys the request's lane on the last such
    /// token it reads.
    fn trace_stream_documents(&self, upstream: &Value, client: &Value, ingress: &str) {
        let (Some(kind), Some(provider_id)) = (
            routectl_core::provider_kind::trace_kind(self.lane.egress()),
            self.provider_id.as_deref(),
        ) else {
            return;
        };
        routectl_core::trace_upstream_success_body(kind, provider_id, upstream);
        routectl_core::trace_egress_body(ingress, client);
    }
}

/// Which side of a response comparison had no complete document, and why
/// when an assembler said so.
#[derive(Clone, Copy)]
enum IncompleteSide {
    Upstream(Option<IncompleteReason>),
    Client(Option<IncompleteReason>),
}

impl IncompleteSide {
    const fn label(self) -> &'static str {
        match self {
            Self::Upstream(_) => "upstream",
            Self::Client(_) => "client",
        }
    }

    const fn reason_label(self) -> &'static str {
        match self {
            Self::Upstream(Some(reason)) | Self::Client(Some(reason)) => reason.label(),
            Self::Upstream(None) | Self::Client(None) => "unreadable",
        }
    }
}

impl Router {
    /// Hand the serving attempt's response check to `meta` when its tap
    /// checks the response, or a trace-only one when its tap only folds the
    /// upstream's stream; clear it otherwise, so a carrier left by an
    /// earlier attempt never describes this one.
    pub(super) fn carry_response_check(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        meta: &mut DispatchMeta,
    ) {
        meta.response_check = tap
            .filter(|tap| tap.folds_response())
            .zip(attempt_lane(target, attempt_req))
            .map(|(tap, lane)| ResponseCheck {
                lane,
                tap: tap.clone(),
                state: Arc::clone(&self.wire_check),
                provider_id: target.provider.as_ref().map(|p| Arc::from(p.id())),
                compares: tap.checks_response(),
                observation: attempt_req.routectl_internal.observation.clone(),
            });
    }
}

/// Declare, on a response-checking tap, that the client sees routectl's
/// model label in place of whatever model the upstream named (or added
/// where the upstream named none).
pub(super) fn declare_model_relabel(tap: Option<&WireTap>) {
    declare_response(
        tap,
        EditSite::ResponseModelRelabel,
        "/model",
        EditKind::Rewrite,
    );
    declare_response(tap, EditSite::ResponseModelRelabel, "/model", EditKind::Add);
}

/// Declare, on a response-checking tap, the serving provider's name added
/// to a complete body.
pub(super) fn declare_provider_attribution(tap: Option<&WireTap>) {
    declare_response(
        tap,
        EditSite::ProviderAttributionAdd,
        "/routectl_provider",
        EditKind::Add,
    );
}

fn declare_response(tap: Option<&WireTap>, site: EditSite, path: &'static str, kind: EditKind) {
    let Some(tap) = tap.filter(|tap| tap.checks_response()) else {
        return;
    };
    match DeclaredEdit::new(site, path, kind) {
        Ok(edit) => tap.declare_response(edit),
        Err(e) => tracing::debug!(
            target: LOG_TARGET,
            site = site.label(),
            error = %e,
            "wire-check response edit declaration refused"
        ),
    }
}

#[cfg(test)]
#[path = "wire_response_tests.rs"]
mod wire_response_tests;
