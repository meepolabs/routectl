//! The client side of the response-leg wire check: when the attempt that
//! answered carries a [`ResponseCheck`], fold what the client receives and
//! hand it to the check once the response is over.
//!
//! An attempt without a check builds nothing here: no assembler, and a
//! non-recording edit collector. The fold reads each frame just before it
//! is sent and never alters it.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Bytes;
use http_body::{Body, Frame, SizeHint};
use routectl_core::wire_check::{
    Assembled, IncompleteReason, SseFrame, StreamAssembler, StreamExit,
};
use routectl_router::{ResponseCheck, Router};
use routectl_usage::Outcome;

use crate::handlers::usage_capture::UsageCapture;
use crate::ingress::{ResponseEdits, SseEvent};

/// The response edits a renderer should declare into: recording only when
/// `check` is present.
pub(super) fn response_edits_for(check: Option<&ResponseCheck>) -> ResponseEdits {
    if check.is_some() {
        ResponseEdits::recording()
    } else {
        ResponseEdits::default()
    }
}

/// Record that a complete response could not be rendered for the client.
pub(super) fn abandon_unrendered(check: Option<ResponseCheck>) {
    if let Some(check) = check {
        check.compare_stream(
            Assembled::Incomplete(IncompleteReason::RenderFailure),
            StreamExit::RenderFailure,
            &[],
        );
    }
}

/// The usage row of a complete response the handler has rendered, held by
/// its [`CheckedBody`] until the body is either sent or dropped.
pub(super) struct PendingRow {
    capture: UsageCapture,
    router: Arc<Router>,
    session_key: Option<String>,
}

impl PendingRow {
    /// `capture`'s row, whose live K and calibration samples go to `router`
    /// under `session_key` once the body is delivered.
    pub(super) const fn new(
        capture: UsageCapture,
        router: Arc<Router>,
        session_key: Option<String>,
    ) -> Self {
        Self {
            capture,
            router,
            session_key,
        }
    }

    /// Record the live samples, then write the row as `ok`.
    ///
    /// The samples share the row's `ok` boundary: the startup replay admits
    /// `ok` rows only, so a sample recorded for any other outcome would make
    /// the in-memory stores route one way now and another after a restart.
    fn deliver(mut self) {
        let session_key = self.session_key.as_deref();
        self.capture.record_k_sample(&self.router, session_key);
        self.capture
            .record_calibration_sample(&self.router, session_key);
        self.capture.finalize(Outcome::Ok);
    }
}

/// A complete response body that compares itself with the upstream's
/// answer once its one frame has been taken, and only then writes the
/// request's usage row, so the row counts what the compare found. Dropped
/// unread, it records `incomplete` instead and leaves the row to the
/// capture's own drop: the client never received what would be compared.
/// The compare never runs ahead of the bytes.
pub(super) struct CheckedBody {
    body: Option<Bytes>,
    check: Option<(ResponseCheck, Bytes, ResponseEdits)>,
    row: Option<PendingRow>,
}

impl CheckedBody {
    /// `body`, compared through `check` (when present) with the edits the
    /// renderer declared while building it; `row` is written when the body
    /// is over.
    pub(super) fn new(
        body: Bytes,
        check: Option<ResponseCheck>,
        edits: ResponseEdits,
        row: PendingRow,
    ) -> Self {
        Self {
            check: check.map(|check| (check, body.clone(), edits)),
            body: Some(body),
            row: Some(row),
        }
    }

    /// Whether the client has everything: the frame was taken, or there was
    /// nothing to send (`is_end_stream` already told hyper so, and it does
    /// not poll such a body).
    fn is_delivered(&self) -> bool {
        self.body.as_ref().is_none_or(Bytes::is_empty)
    }
}

impl Body for CheckedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(
            self.get_mut()
                .body
                .take()
                .filter(|body| !body.is_empty())
                .map(|body| Ok(Frame::data(body))),
        )
    }

    fn is_end_stream(&self) -> bool {
        self.is_delivered()
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.body.as_ref().map_or(0, Bytes::len);
        SizeHint::with_exact(u64::try_from(len).unwrap_or(u64::MAX))
    }
}

impl Drop for CheckedBody {
    fn drop(&mut self) {
        let delivered = self.is_delivered();
        if let Some((check, body, edits)) = self.check.take() {
            if delivered {
                check.compare_body(&body, edits.as_slice());
            } else {
                check.compare_stream(
                    Assembled::Incomplete(IncompleteReason::Cancelled),
                    StreamExit::Cancelled,
                    &[],
                );
            }
        }
        if let Some(row) = self.row.take()
            && delivered
        {
            row.deliver();
        }
    }
}

/// Where a streamed response goes: the channel to the client and the check
/// that folds what is sent on it.
pub(super) struct ClientSink {
    pub(super) tx: tokio::sync::mpsc::Sender<SseEvent>,
    pub(super) check: ClientStreamCheck,
}

impl ClientSink {
    /// A sink over `tx`, folding for `check` when present; `ingress` is the
    /// id of the adapter rendering the stream.
    pub(super) fn new(
        tx: tokio::sync::mpsc::Sender<SseEvent>,
        check: Option<ResponseCheck>,
        ingress: &str,
    ) -> Self {
        Self {
            tx,
            check: ClientStreamCheck::new(check, ingress),
        }
    }
}

/// Folds the frames a streamed response sends the client and compares them
/// with the upstream's stream through the check, tracing both documents
/// when both are whole. Each `drive_stream` exit compares with
/// [`Self::finish`] before it writes the usage row, so the row counts what
/// the compare found; a check dropped unfinished (a cancelled task)
/// compares as cancelled.
pub(super) struct ClientStreamCheck {
    armed: Option<ArmedCheck>,
    edits: ResponseEdits,
}

struct ArmedCheck {
    check: ResponseCheck,
    assembler: StreamAssembler,
    ingress: Box<str>,
}

impl ClientStreamCheck {
    /// A fold for `check`'s stream, rendered by the `ingress` adapter; inert
    /// when `check` is `None`.
    pub(super) fn new(check: Option<ResponseCheck>, ingress: &str) -> Self {
        let edits = response_edits_for(check.as_ref());
        let armed = check.map(|check| ArmedCheck {
            assembler: StreamAssembler::new(check.lane().ingress()),
            check,
            ingress: ingress.into(),
        });
        Self { armed, edits }
    }

    /// The collector the renderer declares its response edits into.
    pub(super) const fn edits(&mut self) -> &mut ResponseEdits {
        &mut self.edits
    }

    /// Fold one frame the client is about to receive.
    pub(super) fn observe(&mut self, event: &SseEvent) {
        if let Some(armed) = self.armed.as_mut() {
            armed
                .assembler
                .push(SseFrame::new(event.event.as_deref(), &event.data));
        }
    }

    /// Compare what the client received, the stream having ended as
    /// `exit`. Only the first call compares.
    pub(super) fn finish(&mut self, exit: StreamExit) {
        if let Some(armed) = self.armed.take() {
            armed.check.compare_traced_stream(
                armed.assembler.finish(exit),
                exit,
                self.edits.as_slice(),
                &armed.ingress,
            );
        }
    }
}

impl Drop for ClientStreamCheck {
    fn drop(&mut self) {
        self.finish(StreamExit::Cancelled);
    }
}

#[cfg(test)]
#[path = "response_wire_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "response_wire_trace_tests.rs"]
mod trace_tests;

#[cfg(test)]
#[path = "response_wire_translation_trace_tests.rs"]
mod translation_trace_tests;

#[cfg(test)]
#[path = "response_wire_row_tests.rs"]
mod row_tests;
