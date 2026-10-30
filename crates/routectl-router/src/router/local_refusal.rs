//! Counting of requests routectl refuses on its own, before or instead of
//! an upstream answer, on `rc_request_refused_local_total{lane,reason}`. One
//! counting function serves every router exit that refuses locally, so a
//! refusal is counted once whichever exit it leaves through.

use routectl_core::wire_check::{EgressKind, IngressDialect, Lane};
use routectl_core::{ChatRequest, Error, RefusalReason, RequestObservation};

use super::{DispatchTarget, Router};

/// Where a request's local refusals are counted: the lane's ingress side
/// and the request's observation slot. Built once per walk, before any
/// attempt edits the request.
#[derive(Debug, Clone, Default)]
pub(super) struct RefusalOrigin {
    ingress: Option<IngressDialect>,
    observation: Option<RequestObservation>,
}

impl RefusalOrigin {
    /// The origin of `req`'s refusals.
    pub(super) fn of(req: &ChatRequest) -> Self {
        Self {
            ingress: ingress_dialect_of(req),
            observation: req.routectl_internal.observation.clone(),
        }
    }
}

/// The dialect the client's request arrived in: from its captured ingress
/// body, else from the ingress that parsed it. A request built without an
/// ingress has none.
fn ingress_dialect_of(req: &ChatRequest) -> Option<IngressDialect> {
    req.routectl_internal
        .ingress_wire
        .as_ref()
        .map(|wire| wire.dialect)
        .or_else(|| IngressDialect::of_provenance(req.routectl_internal.provenance))
}

impl Router {
    /// Count one request the `ingress` layer refused for `reason` while
    /// parsing it, on the lane `<ingress>->local`: it never reached a
    /// dispatch target.
    pub fn record_ingress_refusal(&self, ingress: IngressDialect, reason: RefusalReason) {
        self.wire_check
            .record_local_refusal(Lane::of(ingress, EgressKind::Local), reason);
    }

    /// Count one local refusal for `reason` on the lane of `origin`'s
    /// ingress feeding `target`'s egress. Without an ingress dialect or a
    /// known egress there is no lane, and nothing is counted.
    pub(super) fn count_local_refusal(
        &self,
        origin: &RefusalOrigin,
        target: &DispatchTarget,
        reason: RefusalReason,
    ) {
        self.count_egress_refusal(origin, target.egress_kind, reason);
    }

    /// Count `err` on the lane of `origin`'s ingress feeding `target`'s
    /// egress when it is a local refusal; any other error is not counted.
    pub(super) fn count_refusal_error(
        &self,
        origin: &RefusalOrigin,
        target: &DispatchTarget,
        err: &Error,
    ) {
        if let Some(reason) = err.refusal_reason() {
            self.count_local_refusal(origin, target, reason);
        }
    }

    /// Count one local refusal for `reason` on the lane of `origin`'s
    /// ingress feeding `egress`, and on `origin`'s observation slot; nothing
    /// without both an ingress and an egress.
    pub(super) fn count_egress_refusal(
        &self,
        origin: &RefusalOrigin,
        egress: Option<EgressKind>,
        reason: RefusalReason,
    ) {
        if let (Some(ingress), Some(egress)) = (origin.ingress, egress) {
            self.wire_check
                .record_local_refusal(Lane::of(ingress, egress), reason);
            if let Some(observation) = origin.observation.as_ref() {
                observation.add_local_refusal();
            }
        }
    }
}

#[cfg(test)]
#[path = "local_refusal_tests.rs"]
mod local_refusal_tests;
