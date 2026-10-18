//! Counting of requests routectl refuses on its own, before or instead of
//! an upstream answer, on `rc_request_refused_local_total{lane,reason}`. One
//! counting function serves every router exit that refuses locally, so a
//! refusal is counted once whichever exit it leaves through.

use routectl_core::wire_check::{EgressKind, IngressDialect, Lane};
use routectl_core::{ChatRequest, RefusalReason};

use super::{DispatchTarget, Router};

/// The dialect the client's request arrived in, from its captured ingress
/// body. A request built without an ingress has none.
pub(super) fn ingress_dialect_of(req: &ChatRequest) -> Option<IngressDialect> {
    req.routectl_internal
        .ingress_wire
        .as_ref()
        .map(|wire| wire.dialect)
}

impl Router {
    /// Count one request the `ingress` layer refused for `reason` while
    /// parsing it, on the lane `<ingress>->local`: it never reached a
    /// dispatch target.
    pub fn record_ingress_refusal(&self, ingress: IngressDialect, reason: RefusalReason) {
        self.wire_check
            .record_local_refusal(Lane::of(ingress, EgressKind::Local), reason);
    }

    /// Count one local refusal for `reason` on the lane of `ingress` feeding
    /// `target`'s egress. Without an ingress dialect or a known egress there
    /// is no lane, and nothing is counted.
    pub(super) fn count_local_refusal(
        &self,
        ingress: Option<IngressDialect>,
        target: &DispatchTarget,
        reason: RefusalReason,
    ) {
        if let (Some(ingress), Some(egress)) = (ingress, target.egress_kind) {
            self.wire_check
                .record_local_refusal(Lane::of(ingress, egress), reason);
        }
    }
}

#[cfg(test)]
#[path = "local_refusal_tests.rs"]
mod local_refusal_tests;
