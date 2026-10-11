//! The CLI-side bridge from the router's probe-clear acknowledgement to the
//! usage writer that makes a clear durable.
//!
//! The router declares the contract (`routectl_router::ProbeClearAcknowledgement`)
//! because it consumes it, and the usage crate owns the writer. Neither depends on
//! the other, so this crate -- which depends on both and owns the daemon's boot --
//! is where they meet, exactly as `capability_health` does for the health read.
//!
//! # Durable means committed, and nothing else
//!
//! A re-check probe that saw the upstream accept a field submits a `cleared` row
//! here, and the router removes the negative only if this answers `true`. So the
//! answer is `true` for one outcome alone, `CapabilityEventWrite::is_durable()`.
//! A refused admission (capture disabled, a full or closed channel), a write that
//! failed, and a row a boundary or purge superseded all answer `false`: the
//! negative keeps acting and the probe job stays retryable.
//!
//! # The row is a probe-source `cleared` row
//!
//! The same shape the dispatch drain writes for a clear, built by the same
//! function, but stamped `probe`: routectl issued the request that settled it,
//! not a client. The warm rebuild admits a probe-sourced field `cleared` row
//! (while still refusing a probe-sourced field negative), so the negative this
//! retracted stays retracted at the next boot.
//!
//! # Revision stamps come from the live router
//!
//! The installation is carried across every rebuild by
//! `Router::carry_over_learned_from`, so an installation that captured the boot
//! router's catalog and overlay revision would stamp a stale pair after a
//! revision-changing reload. It reads the PUBLISHED router instead, through a weak
//! reference to the daemon's swap: weak because the Router holds this installation,
//! and a strong reference would make every Router keep itself alive.
//!
//! # Fail safe by construction
//!
//! Nothing here is installed by `Router::new`. A Router built outside the daemon's
//! boot carries no installation, and the router reads that as NOT DURABLE.

use std::sync::{Arc, Weak};

use arc_swap::ArcSwap;
use routectl_router::router::CapabilityClearedEvent;
use routectl_router::{ProbeClearAcknowledgement, Router};
use routectl_usage::UsageHandle;

use crate::handlers::usage_capture::{epoch_ms_now, probe_cleared_capability_row};

/// Submits a probe-settled clear through the usage writer's acknowledged path.
pub(super) struct UsageProbeClearAck {
    usage: UsageHandle,
    /// The daemon's router swap, read for the revision pair the row is stamped
    /// with. Weak so the installation does not keep the Routers holding it alive.
    live: Weak<ArcSwap<Router>>,
}

#[async_trait::async_trait]
impl ProbeClearAcknowledgement for UsageProbeClearAck {
    async fn acknowledge_clear(&self, cleared: CapabilityClearedEvent) -> bool {
        // A swap that is gone means the daemon is shutting down: nothing can be
        // published on the strength of this row, so it is not submitted at all.
        let Some((catalog_version, overlay_revision)) = self.live_revision() else {
            tracing::debug!(
                "probe clear not submitted: the daemon's router is gone, so the \
                 negative stays as it is and the next boot's ledger replay restores it"
            );
            return false;
        };
        let row = probe_cleared_capability_row(
            &cleared,
            epoch_ms_now(),
            catalog_version,
            overlay_revision,
        );
        let receipt = match self.usage.admit_acknowledged_capability_event(
            row,
            cleared.persistence_generation,
            cleared.incarnation,
        ) {
            Ok(receipt) => receipt,
            Err(refusal) => {
                tracing::debug!(
                    outcome = refusal.as_str(),
                    "probe clear was not admitted; the negative stays acting"
                );
                return false;
            }
        };
        let outcome = receipt.await_outcome().await;
        if !outcome.is_durable() {
            tracing::debug!(
                outcome = outcome.as_str(),
                "probe clear did not land durably; the negative stays acting"
            );
        }
        outcome.is_durable()
    }
}

impl UsageProbeClearAck {
    /// The published router's `(catalog_version, overlay_revision)`, or `None`
    /// once the swap has been dropped.
    fn live_revision(&self) -> Option<(u32, u64)> {
        let live = self.live.upgrade()?;
        let router = live.load();
        Some((router.catalog_version(), router.overlay_revision()))
    }
}

/// Install the probe-clear acknowledgement on the owned Router, before
/// publication.
///
/// Consuming, like the builder it calls, so it can only run while the Router is
/// still owned -- at boot. A reload needs no call here:
/// `Router::carry_over_learned_from` attaches the outgoing installation to the
/// replacement, so every generation submits its clears to the one writer.
pub(super) fn install_probe_clear_ack(
    router: Router,
    usage: &UsageHandle,
    live: Weak<ArcSwap<Router>>,
) -> Router {
    router.with_probe_clear_acknowledgement(Arc::new(UsageProbeClearAck {
        usage: usage.clone(),
        live,
    }))
}

#[cfg(test)]
#[path = "probe_clear_ack_tests.rs"]
mod tests;
