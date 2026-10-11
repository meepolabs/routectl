//! The wire side of a failed attempt: give back the sampler charge the
//! attempt took when its tap was installed, and, when the upstream rejected
//! the request, run the full request-leg check on it and classify where the
//! path the rejection names came from.
//!
//! A rejection is always checked, whatever the sampler decided: the
//! sampler counts successful attempts only. The classification is held on
//! [`DispatchMeta`] as a content-free pending record and joined at dispatch
//! end with what field repair made of the same path, because the repair
//! that answers a rejection runs after it and may never run at all.
//!
//! Reads only. Nothing here changes the request, the error, or the learned
//! state the repair settles.

use routectl_core::wire_check::{
    AttemptRecord, Lane, LaneClass, Leg, RejectionProvenance, WirePath, WireTap,
};
use routectl_core::{ChatRequest, Error, sanitize_for_log};
use serde_json::Value;

use super::class_observe::upstream_facts;
use super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR;
use super::rejection_parser::{REJECTION_STATUSES, RejectedWirePath, rejected_wire_path};
use super::wire_check::{SampleCharge, attempt_lane, compare_request, lend_request_record};
use super::{DispatchMeta, DispatchTarget, FieldRepair, Router};
use crate::wire_check_state::F1Outcome;

/// One rejected attempt, classified before any repair touched its request
/// and waiting for the field-repair outcome. Holds no body content: a
/// sanitized pointer, closed labels, the target's sanitized state key and
/// a position in the request's repair records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWireRejection {
    lane: Lane,
    /// Where the rejection says the field sits in the request body; the
    /// root pointer when it named no path.
    path: WirePath,
    /// The canonical field path a repair record names, when localized.
    field: Option<WirePath>,
    provenance: RejectionProvenance,
    state_key: String,
    /// How many repair records the request held when the rejection
    /// arrived; a repair answering it can only be recorded after.
    repairs_before: usize,
}

impl Router {
    /// Settle the wire check for an attempt that did not succeed. Gives back
    /// the attempt's sampler charge whether or not it was admitted, then,
    /// for a status in the rejection set, compares the request leg and
    /// records the rejection's provenance on `meta`.
    ///
    /// Must run before anything mutates `attempt_req` or drains `tap`, so
    /// both still describe the body the upstream refused.
    pub(super) fn settle_failed_attempt(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        charge: SampleCharge,
        err: &Error,
        meta: &mut DispatchMeta,
    ) {
        drop(charge);
        let (Some(tap), Some(lane)) = (tap, attempt_lane(target, attempt_req)) else {
            return;
        };
        let Some(status) = upstream_facts(err)
            .status
            .filter(|status| REJECTION_STATUSES.contains(status))
        else {
            return;
        };
        let rejected = rejected_wire_path(
            target.provider_kind.unwrap_or_default(),
            status,
            err.parsed_body(),
        );
        let provenance = lend_request_record(attempt_req, tap, |record| {
            let record = record?.with_rejection(err);
            compare_request(&self.wire_check, lane, &record);
            Some(provenance_of(lane, &record, rejected.as_ref()))
        });
        let Some(provenance) = provenance else {
            self.wire_check.record_incomplete(lane);
            return;
        };
        let (path, field) = rejected.map_or((WirePath::root(), None), |named| {
            (named.wire, Some(named.field))
        });
        meta.wire_rejections.push(PendingWireRejection {
            lane,
            path,
            field,
            provenance,
            state_key: sanitize_for_log(&target.state_key),
            repairs_before: meta.field_repairs.len(),
        });
    }

    /// Record every rejection the request drew, each with what field repair
    /// made of it. Called once, when the dispatch has resolved and every
    /// repair record is final.
    pub(super) fn emit_wire_rejections(&self, meta: &DispatchMeta) {
        for pending in &meta.wire_rejections {
            self.wire_check.record_rejection(
                pending.lane,
                Leg::Request,
                &pending.path,
                pending.provenance,
                f1_outcome(pending, &meta.field_repairs),
            );
        }
    }
}

/// Where the path a rejection names came from: looked up in the client's
/// body and in the body routectl sent. On a TRANSLATION lane the two bodies
/// do not share paths, so a located path is not applicable.
fn provenance_of(
    lane: Lane,
    record: &AttemptRecord<'_>,
    rejected: Option<&RejectedWirePath>,
) -> RejectionProvenance {
    let Some(path) = rejected
        .filter(|named| !named.wire.is_coarsened())
        .map(|named| named.locator.as_slice())
    else {
        return RejectionProvenance::Unlocalized;
    };
    if lane.class() == LaneClass::Translation {
        return RejectionProvenance::NotApplicable;
    }
    let sent = record
        .egress()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    let received = serde_json::from_slice::<Value>(&record.ingress().bytes).ok();
    let (Some(sent), Some(received)) = (sent, received) else {
        return RejectionProvenance::Unlocalized;
    };
    match (locate(&received, path), locate(&sent, path)) {
        (Some(client), Some(wire)) if client == wire => RejectionProvenance::ClientUnchanged,
        (Some(_), _) => RejectionProvenance::ClientChanged,
        (None, Some(_)) => RejectionProvenance::RoutectlAdded,
        (None, None) => RejectionProvenance::Unlocalized,
    }
}

/// The value at `path` (raw keys and indices) in `body`. A rejection names a
/// tagged-union member by its tag (`thinking.enabled.display` for
/// `{"thinking": {"type": "enabled", "display": ..}}`), so a segment an
/// object lacks but carries as its own `type` tag is consumed without
/// descending.
fn locate<'v>(body: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter().try_fold(body, |node, key| match node {
        Value::Object(map) => map.get(key).or_else(|| {
            (map.get(UNION_TAG).and_then(Value::as_str) == Some(key.as_str())).then_some(node)
        }),
        Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    })
}

/// The member a tagged union names its variant by.
const UNION_TAG: &str = "type";

/// What field repair made of `pending`: the first repair on the same target
/// for the same field recorded at or after the rejection. A repair whose
/// retry did not succeed left the rejection unrepaired.
fn f1_outcome(pending: &PendingWireRejection, repairs: &[FieldRepair]) -> F1Outcome {
    let Some(field) = pending.field.as_ref().filter(|field| !field.is_coarsened()) else {
        return F1Outcome::Unrepaired;
    };
    let repair = repairs
        .get(pending.repairs_before..)
        .unwrap_or_default()
        .iter()
        .find(|repair| {
            repair.state_key == pending.state_key
                && repair.field_path.split('.').eq(field.segments())
        });
    match repair {
        Some(repair) if repair.repair_succeeded => {
            if repair.action == FIELD_ACTION_LOSSY_DROP_REPAIR {
                F1Outcome::RepairedLossy
            } else {
                F1Outcome::Repaired
            }
        }
        _ => F1Outcome::Unrepaired,
    }
}

#[cfg(test)]
#[path = "wire_rejection_tests.rs"]
mod wire_rejection_tests;
