//! The client side of the response-leg wire check: when the attempt that
//! answered carries a [`ResponseCheck`], fold what the client receives and
//! hand it to the check once the response is over.
//!
//! An attempt without a check builds nothing here: no assembler, and a
//! non-recording edit collector. The fold reads each frame just before it
//! is sent and never alters it.

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

/// Where a streamed response goes: the channel to the client and the check
/// that folds what is sent on it.
pub(super) struct ClientSink {
    pub(super) tx: tokio::sync::mpsc::Sender<SseEvent>,
    pub(super) check: ClientStreamCheck,
}

impl ClientSink {
    /// A sink over `tx`, folding for `check` when present.
    pub(super) fn new(
        tx: tokio::sync::mpsc::Sender<SseEvent>,
        check: Option<ResponseCheck>,
    ) -> Self {
        Self {
            tx,
            check: ClientStreamCheck::new(check),
        }
    }
}

/// Folds the frames a streamed response sends the client and, when it is
/// dropped, compares them with the upstream's stream through the check. The
/// exit defaults to a cancelled task; each `drive_stream` exit names its
/// own with [`Self::end`].
pub(super) struct ClientStreamCheck {
    armed: Option<(ResponseCheck, StreamAssembler)>,
    edits: ResponseEdits,
    exit: StreamExit,
}

impl ClientStreamCheck {
    /// A fold for `check`'s stream; inert when `check` is `None`.
    pub(super) fn new(check: Option<ResponseCheck>) -> Self {
        let edits = response_edits_for(check.as_ref());
        let armed = check.map(|check| {
            let assembler = StreamAssembler::new(check.lane().ingress());
            (check, assembler)
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
        if let Some((_, assembler)) = self.armed.as_mut() {
            assembler.push(SseFrame::new(event.event.as_deref(), &event.data));
        }
    }

    /// Name how the stream ended.
    pub(super) const fn end(&mut self, exit: StreamExit) {
        self.exit = exit;
    }
}

impl Drop for ClientStreamCheck {
    fn drop(&mut self) {
        if let Some((check, assembler)) = self.armed.take() {
            check.compare_stream(
                assembler.finish(self.exit),
                self.exit,
                self.edits.as_slice(),
            );
        }
    }
}

#[cfg(test)]
#[path = "response_wire_tests.rs"]
mod tests;
