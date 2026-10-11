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

use std::fmt;
use std::sync::Arc;

use routectl_core::ChatRequest;
use routectl_core::wire_check::{
    Assembled, DeclaredEdit, EditKind, EditSite, IncompleteReason, Lane, LaneClass, Leg,
    StreamExit, WireTap, diff,
};
use serde_json::Value;

use crate::wire_check_state::WireCheckState;

use super::wire_check::attempt_lane;
use super::{DispatchMeta, DispatchTarget, Router};

const LOG_TARGET: &str = "routectl_router::wire_check";

/// The response-direction check of the attempt that answered the client.
/// Opaque: it holds the attempt's tap, its lane and the router's wire-check
/// state, and exposes only the comparison.
#[derive(Clone)]
pub struct ResponseCheck {
    lane: Lane,
    tap: WireTap,
    state: Arc<WireCheckState>,
}

impl fmt::Debug for ResponseCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseCheck")
            .field("lane", &self.lane.label())
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
    /// declared while building `client_body`.
    pub fn compare_body(self, client_body: &[u8], client_edits: &[DeclaredEdit]) {
        let client =
            serde_json::from_slice::<Value>(client_body).map_err(|_| IncompleteSide::Client(None));
        self.record(StreamExit::Ended, client, client_edits);
    }

    /// Compare the stream the client received, folded into `client`, with
    /// the upstream's stream; `exit` is how the stream ended. Either side
    /// unfinished records `incomplete` with its reason.
    pub fn compare_stream(
        self,
        client: Assembled,
        exit: StreamExit,
        client_edits: &[DeclaredEdit],
    ) {
        let client = match client {
            Assembled::Complete(document) => Ok(document),
            Assembled::Incomplete(reason) => Err(IncompleteSide::Client(Some(reason))),
        };
        self.record(exit, client, client_edits);
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

    fn record(
        self,
        exit: StreamExit,
        client: Result<Value, IncompleteSide>,
        client_edits: &[DeclaredEdit],
    ) {
        let declared = self.tap.take_response();
        let upstream = self.upstream_document(declared.response().map(|body| &body[..]), exit);
        let (upstream, client) = match (upstream, client) {
            (Ok(upstream), Ok(client)) => (upstream, client),
            (Err(side), _) | (_, Err(side)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    lane = self.lane.label(),
                    leg = Leg::Response.label(),
                    side = side.label(),
                    reason = side.reason_label(),
                    "wire check had no complete response to compare"
                );
                self.state.record_incomplete(self.lane);
                return;
            }
        };
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
    /// checks the response; clear it otherwise, so a carrier left by an
    /// earlier attempt never describes this one.
    pub(super) fn carry_response_check(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        meta: &mut DispatchMeta,
    ) {
        meta.response_check = tap
            .filter(|tap| tap.checks_response())
            .zip(attempt_lane(target, attempt_req))
            .map(|(tap, lane)| ResponseCheck {
                lane,
                tap: tap.clone(),
                state: Arc::clone(&self.wire_check),
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
