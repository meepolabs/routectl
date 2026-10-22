//! The daemon's periodic router-metrics snapshot driver.
//!
//! Its own module rather than a block in the reload coordinator: the
//! coordinator decides WHETHER to publish a replacement router, while this is
//! one of the bounded interval drivers that ride the same pipeline. Sibling of
//! `probe_driver.rs`, which owns the other one.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_router::Router;
use tokio::sync::watch;

/// How often the router-metrics driver flushes a snapshot to `tracing`.
/// Mirrors the front-proxy's own snapshot-interval discipline
/// (`crate::proxy::listener`): a sibling constant rather than a shared one,
/// since the two live in different crates with no public seam between them.
pub(super) const ROUTER_METRICS_SNAPSHOT_INTERVAL: std::time::Duration =
    std::time::Duration::from_mins(1);

/// Periodically flush the live `Router`'s metrics snapshot to `tracing`
/// until `shutdown` fires, then flush once more before returning -- so a
/// session shorter than one interval still surfaces its totals. Skips the
/// immediate t=0 tick `interval` would otherwise fire (an all-zero startup
/// snapshot carries no signal), matching the front-proxy's own driver.
pub(super) async fn run_router_metrics_snapshot_driver(
    router_swap: Arc<ArcSwap<Router>>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut snapshot_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + ROUTER_METRICS_SNAPSHOT_INTERVAL,
        ROUTER_METRICS_SNAPSHOT_INTERVAL,
    );
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                log_snapshots(&router_swap);
                return;
            }
            _ = snapshot_tick.tick() => {
                log_snapshots(&router_swap);
            }
        }
    }
}

/// One snapshot of the router's metrics and of the stream-evidence counter
/// the ingress keeps beside them.
fn log_snapshots(router_swap: &ArcSwap<Router>) {
    router_swap.load().log_metrics_snapshot();
    crate::handlers::stream_evidence_settle::log_stream_evidence_snapshot();
}
