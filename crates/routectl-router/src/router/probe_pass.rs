//! `Router::run_probe_pass`: the one driver-facing entry point that runs the
//! free due batch to completion and reports what it did.

use super::Router;

/// What one whole probe pass did, for the driver to log.
///
/// `non_exhaustive` so a later field stays additive for `routectl-cli`, the
/// only outside consumer -- adding one here must never break that build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbePassSummary {
    /// How many free validators this pass ran to settlement.
    pub free_validators_run: usize,
}

impl Router {
    /// Run the due free batch to completion. Called once per driver tick.
    pub async fn run_probe_pass(&self) -> ProbePassSummary {
        ProbePassSummary {
            free_validators_run: self.run_due_probes().await,
        }
    }
}
