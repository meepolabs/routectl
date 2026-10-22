//! Settling a streamed reply's response evidence at the stream's natural end.
//!
//! The router hands a successful stream's evidence carrier out on the
//! dispatch meta at first content. First content is not the end of the reply,
//! so the carrier rides into the render task and is settled only where the
//! whole stream, including its end-of-stream frames, has reached the client.
//! Every other exit drops it unsettled, which writes nothing and only counts.
//!
//! Settlement reads through the router published AT SETTLE TIME, not the one
//! the request started on: a reload that turns the capability subsystem off,
//! or drops the served provider entry, does not advance the registry
//! generation, so only the current router's config can refuse it.
//!
//! The counter (`rc_stream_evidence_total{result,exit}`) has no exporter; it
//! rides a DEBUG snapshot line beside the router metrics snapshot.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use routectl_core::wire_check::StreamExit;
use routectl_router::router::EvidencePolicy;
use routectl_router::{Router, StreamEvidence};

use crate::handlers::usage_capture::UsageCapture;

/// A stream read may miss an evidence shape the complete path sees, so it
/// admits only verified positives: a gap in the fold must never mint a
/// suspected-absence negative.
const STREAM_POLICY: EvidencePolicy = EvidencePolicy {
    admit_suspect: false,
};

/// Every way a stream can leave `drive_stream`, in counter-slot order.
const EXITS: [StreamExit; 5] = [
    StreamExit::Ended,
    StreamExit::Disconnect,
    StreamExit::Cancelled,
    StreamExit::RenderFailure,
    StreamExit::UpstreamError,
];

const fn exit_slot(exit: StreamExit) -> usize {
    match exit {
        StreamExit::Ended => 0,
        StreamExit::Disconnect => 1,
        StreamExit::Cancelled => 2,
        StreamExit::RenderFailure => 3,
        StreamExit::UpstreamError => 4,
    }
}

/// The `exit` label of the counter.
pub(crate) const fn exit_label(exit: StreamExit) -> &'static str {
    match exit {
        StreamExit::Ended => "ended",
        StreamExit::Disconnect => "disconnect",
        StreamExit::Cancelled => "cancelled",
        StreamExit::RenderFailure => "render_failure",
        StreamExit::UpstreamError => "upstream_error",
    }
}

/// `rc_stream_evidence_total{result,exit}`: carriers settled at the natural
/// end versus abandoned, by how the stream left. `settled` means the detectors
/// read a folded response, not that an observation acted; a carrier settled at
/// the natural end whose fold was spoiled, or whose scope the published
/// router's config refuses, is `abandoned` under `ended`.
#[derive(Debug, Default)]
pub(crate) struct StreamEvidenceCounters {
    settled: [AtomicU64; EXITS.len()],
    abandoned: [AtomicU64; EXITS.len()],
}

impl StreamEvidenceCounters {
    fn record_settled(&self, exit: StreamExit) {
        self.settled[exit_slot(exit)].fetch_add(1, Ordering::Relaxed);
    }

    fn record_abandoned(&self, exit: StreamExit) {
        self.abandoned[exit_slot(exit)].fetch_add(1, Ordering::Relaxed);
    }

    /// Every non-zero `(result, exit, count)` cell.
    pub(crate) fn snapshot(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut cells = Vec::new();
        for (result, slots) in [("settled", &self.settled), ("abandoned", &self.abandoned)] {
            for exit in EXITS {
                let count = slots[exit_slot(exit)].load(Ordering::Relaxed);
                if count > 0 {
                    cells.push((result, exit_label(exit), count));
                }
            }
        }
        cells
    }

    pub(crate) fn log_snapshot(&self) {
        tracing::debug!(
            target: "routectl_cli::handlers::stream_evidence_settle",
            rc_stream_evidence_total = ?self.snapshot(),
            "stream evidence snapshot"
        );
    }
}

static STREAM_EVIDENCE_COUNTERS: LazyLock<StreamEvidenceCounters> =
    LazyLock::new(StreamEvidenceCounters::default);

/// Flush the process-wide stream-evidence counter to `tracing` (DEBUG).
pub(crate) fn log_stream_evidence_snapshot() {
    STREAM_EVIDENCE_COUNTERS.log_snapshot();
}

/// Where a streamed turn settles its evidence: the daemon's published-router
/// slot and the counter its carriers are tallied in.
#[derive(Clone)]
pub struct StreamEvidenceSettler {
    live: Arc<ArcSwap<Router>>,
    counters: &'static StreamEvidenceCounters,
}

impl StreamEvidenceSettler {
    /// A settler reading through `live`, tallied in the process-wide counter.
    pub fn new(live: Arc<ArcSwap<Router>>) -> Self {
        Self {
            live,
            counters: &STREAM_EVIDENCE_COUNTERS,
        }
    }

    /// A settler tallied in `counters` instead of the process-wide counter.
    #[cfg(test)]
    pub(crate) const fn with_counters(
        live: Arc<ArcSwap<Router>>,
        counters: &'static StreamEvidenceCounters,
    ) -> Self {
        Self { live, counters }
    }
}

/// A streamed turn's evidence slot: where it settles, and the carrier once
/// the dispatch handed one out.
#[derive(Default)]
pub struct StreamEvidenceSlot {
    settler: Option<StreamEvidenceSettler>,
    held: Option<(StreamEvidence, StreamEvidenceSettler)>,
}

impl StreamEvidenceSlot {
    /// A slot that settles through `settler`.
    pub const fn new(settler: StreamEvidenceSettler) -> Self {
        Self {
            settler: Some(settler),
            held: None,
        }
    }

    /// Hold the dispatch's carrier until the stream ends. A carrier with no
    /// settler is dropped here, unread.
    pub fn hold(&mut self, carrier: Option<StreamEvidence>) {
        if let (Some(settler), Some(carrier)) = (self.settler.as_ref(), carrier) {
            self.held = Some((carrier, settler.clone()));
        }
    }

    /// The carrier as it rides through the stream loop.
    pub(crate) fn into_pending(self) -> PendingStreamEvidence {
        PendingStreamEvidence {
            held: self.held,
            exit: StreamExit::Cancelled,
        }
    }
}

/// A carrier (or none) riding through `drive_stream`. Read only by
/// [`Self::settle`]; dropped otherwise, which counts it as abandoned under the
/// last exit named with [`Self::end`] (a cancelled task when none was) and
/// writes nothing.
pub(crate) struct PendingStreamEvidence {
    held: Option<(StreamEvidence, StreamEvidenceSettler)>,
    exit: StreamExit,
}

impl PendingStreamEvidence {
    /// Name how the stream left.
    pub(crate) const fn end(&mut self, exit: StreamExit) {
        self.exit = exit;
    }

    /// Read the carrier through the currently published router, verified
    /// observations only, and drain what acted into `capture`'s ledger
    /// stamped with that router's revision pair.
    pub(crate) fn settle(mut self, capture: &UsageCapture) {
        let Some((carrier, settler)) = self.held.take() else {
            return;
        };
        let router = settler.live.load();
        let mut events = Vec::new();
        let read = carrier.settle(&router, STREAM_POLICY, &mut events);
        capture.drain_observation_events(
            &events,
            router.catalog_version(),
            router.overlay_revision(),
        );
        if read {
            settler.counters.record_settled(StreamExit::Ended);
        } else {
            settler.counters.record_abandoned(StreamExit::Ended);
        }
    }
}

impl Drop for PendingStreamEvidence {
    fn drop(&mut self) {
        if let Some((_, settler)) = self.held.take() {
            settler.counters.record_abandoned(self.exit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_lists_each_counted_cell_by_result_and_exit() {
        // Arrange
        let counters = StreamEvidenceCounters::default();
        counters.record_settled(StreamExit::Ended);
        counters.record_abandoned(StreamExit::Disconnect);
        counters.record_abandoned(StreamExit::Disconnect);
        counters.record_abandoned(StreamExit::UpstreamError);

        // Act
        let cells = counters.snapshot();

        // Assert
        assert_eq!(
            cells,
            vec![
                ("settled", "ended", 1),
                ("abandoned", "disconnect", 2),
                ("abandoned", "upstream_error", 1),
            ]
        );
    }

    #[test]
    fn every_exit_has_its_own_slot_and_label() {
        let slots: std::collections::BTreeSet<_> = EXITS.iter().map(|e| exit_slot(*e)).collect();
        let labels: std::collections::BTreeSet<_> = EXITS.iter().map(|e| exit_label(*e)).collect();
        assert_eq!(slots.len(), EXITS.len());
        assert_eq!(labels.len(), EXITS.len());
    }

    #[tokio::test]
    async fn the_snapshot_line_carries_the_counter() {
        let counters = StreamEvidenceCounters::default();
        counters.record_abandoned(StreamExit::RenderFailure);

        let ((), events) = routectl_testkit::with_capture(async { counters.log_snapshot() }).await;

        let line = events
            .iter()
            .find(|e| e.message == "stream evidence snapshot")
            .expect("snapshot line");
        assert_eq!(
            line.field("rc_stream_evidence_total"),
            Some(r#"[("abandoned", "render_failure", 1)]"#)
        );
    }
}
