//! The client side of the response-leg wire check: when the attempt that
//! answered carries a [`ResponseCheck`], fold what the client receives and
//! hand it to the check once the response is over.
//!
//! An attempt without a check builds nothing here: no assembler, and a
//! non-recording edit collector. The fold reads each frame just before it
//! is sent and never alters it.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use http_body::{Body, Frame, SizeHint};
use routectl_core::wire_check::{
    Assembled, IncompleteReason, SseFrame, StreamAssembler, StreamExit,
};
use routectl_router::ResponseCheck;

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

/// A complete response body that compares itself with the upstream's
/// answer once its one frame has been taken. Dropped unread, it records
/// `incomplete` instead: the client never received what would be compared.
/// The compare never runs ahead of the bytes.
pub(super) struct CheckedBody {
    body: Option<Bytes>,
    check: Option<(ResponseCheck, Bytes, ResponseEdits)>,
}

impl CheckedBody {
    /// `body`, compared through `check` (when present) with the edits the
    /// renderer declared while building it.
    pub(super) fn new(body: Bytes, check: Option<ResponseCheck>, edits: ResponseEdits) -> Self {
        Self {
            check: check.map(|check| (check, body.clone(), edits)),
            body: Some(body),
        }
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
        self.body.as_ref().is_none_or(Bytes::is_empty)
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.body.as_ref().map_or(0, Bytes::len);
        SizeHint::with_exact(u64::try_from(len).unwrap_or(u64::MAX))
    }
}

impl Drop for CheckedBody {
    fn drop(&mut self) {
        let Some((check, body, edits)) = self.check.take() else {
            return;
        };
        if self.body.is_some() {
            check.compare_stream(
                Assembled::Incomplete(IncompleteReason::Cancelled),
                StreamExit::Cancelled,
                &[],
            );
        } else {
            check.compare_body(&body, edits.as_slice());
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

/// Folds the frames a streamed response sends the client and, when it is
/// dropped, compares them with the upstream's stream through the check,
/// tracing both documents when both are whole. The exit defaults to a
/// cancelled task; each `drive_stream` exit names its own with
/// [`Self::end`].
pub(super) struct ClientStreamCheck {
    armed: Option<ArmedCheck>,
    edits: ResponseEdits,
    exit: StreamExit,
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
        Self {
            armed,
            edits,
            exit: StreamExit::Cancelled,
        }
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

    /// Name how the stream ended.
    pub(super) const fn end(&mut self, exit: StreamExit) {
        self.exit = exit;
    }
}

impl Drop for ClientStreamCheck {
    fn drop(&mut self) {
        if let Some(armed) = self.armed.take() {
            armed.check.compare_traced_stream(
                armed.assembler.finish(self.exit),
                self.exit,
                self.edits.as_slice(),
                &armed.ingress,
            );
        }
    }
}

#[cfg(test)]
#[path = "response_wire_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "response_wire_trace_tests.rs"]
mod trace_tests;
