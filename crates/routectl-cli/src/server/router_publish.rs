//! The router publication step every reload path shares.
//!
//! One helper rather than a copy per path, because the ORDER it encodes is the
//! whole of its content: stamp the probe incarnation, then store. A path that
//! reversed those would leave a window in which the published router still
//! carries the outgoing incarnation.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_router::Router;

/// Stamp `router`'s probe incarnation, retire the outgoing incarnation's work,
/// and publish the router into `router_swap`.
///
/// THE publication step for every reload path, so the ordering cannot drift
/// between them. Stamping comes BEFORE the store and that is load-bearing:
/// between a store and a later stamp the published router still carries the
/// OUTGOING incarnation, so a request landing in that window activates onto
/// work the stamp is about to retire -- the job is queued and then immediately
/// cancelled, leaving the lane un-probed with nothing recording why. Stamping
/// first means every request that can reach the new router already sees the new
/// incarnation.
///
/// Safe to stamp before the swap because a caller only reaches here at its
/// COMMIT POINT: every failure and abandonment path has already returned, so
/// the previous router is not staying live.
pub(super) fn publish_router(router_swap: &Arc<ArcSwap<Router>>, router: Arc<Router>) {
    let retired_probes = router.publish_probe_incarnation();
    if retired_probes > 0 {
        tracing::debug!(
            retired_probe_jobs = retired_probes,
            "probe scheduler incarnation advanced before router publication",
        );
    }
    router_swap.store(router);
}
