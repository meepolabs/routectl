//! The router side of the probe-clear acknowledgement: one trait the
//! accounting owner implements, and the optional installation the Router holds.
//!
//! # Why a clear waits for the ledger
//!
//! A re-check probe that sees the upstream accept a field the lane holds a
//! negative on has disproved that negative. Removing it only in memory would
//! not survive a restart: the next boot's replay finds the negative in the
//! ledger and restores it, so the lane goes back to stripping a field it
//! accepts. The removal therefore happens only once the `cleared` row
//! describing it is durable, and until then the negative stays acting and the
//! probe job stays retryable.
//!
//! # Why a trait rather than a dependency
//!
//! The ledger writer lives in a crate this one must not depend on, so the
//! contract is declared here, in the crate that consumes it, and the
//! writer-owning crate implements it -- the same shape as
//! [`super::CapabilityPersistenceHealth`].
//!
//! # Fail safe by construction
//!
//! The installation is an `Option`, absent in everything `Router::new` builds.
//! An absent installation reads as NOT DURABLE, so a build path that never
//! installs one leaves every disproved negative acting rather than clearing it
//! in memory only.

use std::sync::Arc;

use super::{CapabilityClearedEvent, Router};

/// Persists a probe-settled clear and reports whether it is durable.
///
/// `Send + Sync` so the Router can hold one behind an `Arc` and share it with
/// the probe worker.
#[async_trait::async_trait]
pub trait ProbeClearAcknowledgement: Send + Sync {
    /// Submit `cleared` for a durable write and report whether it landed.
    ///
    /// `false` is the SAFE answer: an implementation that cannot establish the
    /// row is on disk -- refused, superseded, failed, or never submitted --
    /// returns it, and the negative keeps acting. The caller bounds the wait
    /// with its own timeout, which also reads as `false`.
    async fn acknowledge_clear(&self, cleared: CapabilityClearedEvent) -> bool;
}

impl Router {
    /// Install the probe-clear acknowledgement, consuming and returning the
    /// Router.
    ///
    /// A consuming builder for the same reason as
    /// [`Self::with_capability_persistence_health`]: the installation belongs
    /// to boot, before publication. A second call replaces the installation.
    #[must_use]
    pub fn with_probe_clear_acknowledgement(
        mut self,
        acknowledgement: Arc<dyn ProbeClearAcknowledgement>,
    ) -> Self {
        self.probe_clear_ack = Some(acknowledgement);
        self
    }

    /// The installed acknowledgement, or `None` on a Router nobody installed
    /// one on. Crate-internal; exists for the carry-over to attach.
    pub(crate) const fn probe_clear_acknowledgement(
        &self,
    ) -> Option<&Arc<dyn ProbeClearAcknowledgement>> {
        self.probe_clear_ack.as_ref()
    }

    /// Submit `cleared` through the installed acknowledgement, bounded by
    /// [`crate::probe_scheduler::PROBE_CLEAR_ACK_TIMEOUT`]. `false` with no
    /// installation, on refusal, and on expiry.
    pub(super) async fn acknowledge_probe_clear(&self, cleared: CapabilityClearedEvent) -> bool {
        let Some(acknowledgement) = self.probe_clear_ack.as_ref() else {
            return false;
        };
        tokio::time::timeout(
            crate::probe_scheduler::PROBE_CLEAR_ACK_TIMEOUT,
            acknowledgement.acknowledge_clear(cleared),
        )
        .await
        .unwrap_or(false)
    }
}
