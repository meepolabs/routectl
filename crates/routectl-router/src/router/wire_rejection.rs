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
//! The same settlement says whether the body that went out carries the path
//! at all, which the learning engine reads to leave alone a rejection naming
//! a path routectl never sent.
//!
//! Reads only. Nothing here changes the request, the error, or the learned
//! state the repair settles.

use routectl_core::wire_check::{
    AttemptRecord, EgressKind, Lane, LaneClass, Leg, RejectionProvenance, WirePath, WireTap,
};
use routectl_core::{ChatRequest, Error, sanitize_for_log};
use serde_json::Value;

use super::class_observe::upstream_facts;
use super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR;
use super::learn_engine::RejectionSite;
use super::rejection_parser::{
    CONVERSE_FIELD_BAG, REJECTION_STATUSES, RawPath, RejectedWirePath, rejected_wire_path,
};
use super::wire_check::{SampleCharge, attempt_lane, compare_request, lend_request_record};
use super::{DispatchMeta, DispatchTarget, FieldRepair, Router};
use crate::wire_check_state::FieldRepairOutcome;

/// One rejected attempt, classified before any repair touched its request
/// and waiting for the field-repair outcome. Holds no body content: a
/// sanitized pointer, the unmasked field path the join needs (never
/// rendered, opaque to `Debug`), closed labels, the target's sanitized state
/// key and a position in the request's repair records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWireRejection {
    lane: Lane,
    /// Where the rejection says the field sits in the request body; the
    /// root pointer when it named no path.
    path: WirePath,
    /// The canonical field path a repair record names, unmasked, when
    /// localized. Joined against repair records only; `path` is what renders.
    field: Option<RawPath>,
    provenance: RejectionProvenance,
    state_key: String,
    /// How many repair records the request held when the rejection
    /// arrived; a repair answering it can only be recorded after.
    repairs_before: usize,
}

impl Router {
    /// Settle the wire check for an attempt that did not succeed. Gives back
    /// the attempt's sampler charge whether or not it was admitted, then,
    /// for a status in the rejection set, compares the request leg, records
    /// the rejection's provenance on `meta`, and returns where on the wire
    /// the rejection points. `None` when no rejection was recorded.
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
    ) -> Option<RejectionSite> {
        drop(charge);
        let (Some(tap), Some(lane)) = (tap, attempt_lane(target, attempt_req)) else {
            return None;
        };
        let status = upstream_facts(err)
            .status
            .filter(|status| REJECTION_STATUSES.contains(status))?;
        let rejected = rejected_wire_path(
            target.provider_kind.unwrap_or_default(),
            status,
            err.parsed_body(),
        );
        let site = lend_request_record(attempt_req, tap, |record| {
            let record = record?.with_rejection(err);
            compare_request(&self.wire_check, lane, &record);
            let path = exact_locator(rejected.as_ref());
            let sent = path
                .and(record.egress())
                .and_then(|body| serde_json::from_slice::<Value>(body).ok());
            Some(RejectionSite {
                provenance: provenance_of(lane, &record, path, sent.as_ref()),
                in_egress: path
                    .zip(sent.as_ref())
                    .and_then(|(path, sent)| egress_location(lane.egress(), sent, path)),
            })
        });
        let Some(site) = site else {
            self.wire_check.record_incomplete(lane);
            return None;
        };
        let (path, field) = rejected.map_or((WirePath::root(), None), |named| {
            (named.wire, Some(named.field))
        });
        meta.wire_rejections.push(PendingWireRejection {
            lane,
            path,
            field,
            provenance: site.provenance,
            state_key: sanitize_for_log(&target.state_key),
            repairs_before: meta.field_repairs.len(),
        });
        Some(site)
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
                field_repair_outcome(pending, &meta.field_repairs),
            );
        }
    }
}

/// The unmasked location a rejection names, unless the parser could not
/// localize it or its pointer was coarsened to an ancestor of it.
fn exact_locator(rejected: Option<&RejectedWirePath>) -> Option<&[String]> {
    rejected
        .filter(|named| !named.wire.is_coarsened())
        .map(|named| named.locator.as_slice())
}

/// Where the path a rejection names came from: looked up in the client's
/// body and in `sent`, the parsed body routectl sent. On a TRANSLATION lane
/// the two bodies do not share paths, so a located path is not applicable.
fn provenance_of(
    lane: Lane,
    record: &AttemptRecord<'_>,
    path: Option<&[String]>,
    sent: Option<&Value>,
) -> RejectionProvenance {
    let Some(path) = path else {
        return RejectionProvenance::Unlocalized;
    };
    if lane.class() == LaneClass::Translation {
        return RejectionProvenance::NotApplicable;
    }
    let received = serde_json::from_slice::<Value>(&record.ingress().bytes).ok();
    let (Some(sent), Some(received)) = (sent, received) else {
        return RejectionProvenance::Unlocalized;
    };
    match (locate(&received, path), locate(sent, path)) {
        (Some(client), Some(wire)) if client == wire => RejectionProvenance::ClientUnchanged,
        (Some(_), _) => RejectionProvenance::ClientChanged,
        (None, Some(_)) => RejectionProvenance::RoutectlAdded,
        (None, None) => RejectionProvenance::Unlocalized,
    }
}

/// Whether `sent`, the body that went out on an `egress` lane, carries the
/// location `path` a rejection names: `Some(false)` only when the walk ends
/// on an object that lacks the key. `None` whenever the body and the path
/// disagree about shape, since then the absence is not evidence.
///
/// On Converse the model names its own fields relative to the field bag and
/// the request validator names them from the root, bag included. A path the
/// root lacks is looked for in the bag; one neither carries is `None`, not
/// absent, because the model also reads fields the Converse body spells
/// elsewhere. A bag-prefixed path on any other egress names a shape that
/// body does not have.
fn egress_location(egress: EgressKind, sent: &Value, path: &[String]) -> Option<bool> {
    let bag_prefixed = path.first().map(String::as_str) == Some(CONVERSE_FIELD_BAG);
    if bag_prefixed && egress != EgressKind::BedrockConverse {
        return None;
    }
    match walk(sent, path) {
        Walk::Found(_) => Some(true),
        Walk::Mismatch => None,
        Walk::Missing if egress == EgressKind::BedrockConverse && !bag_prefixed => {
            matches!(walk(sent.get(CONVERSE_FIELD_BAG)?, path), Walk::Found(_)).then_some(true)
        }
        Walk::Missing => Some(false),
    }
}

/// The value at `path` (raw keys and indices) in `body`.
fn locate<'v>(body: &'v Value, path: &[String]) -> Option<&'v Value> {
    match walk(body, path) {
        Walk::Found(value) => Some(value),
        Walk::Missing | Walk::Mismatch => None,
    }
}

/// How a walk of a raw path through a body ended.
enum Walk<'v> {
    Found(&'v Value),
    /// An object on the path lacks the next key.
    Missing,
    /// The path asks something the body's shape cannot answer: a key of a
    /// scalar, an index of an object, a non-index of an array, an index past
    /// an array's end.
    Mismatch,
}

/// Walk `path` (raw keys and indices) through `body`. A rejection names a
/// tagged-union member by its tag (`thinking.enabled.display` for
/// `{"thinking": {"type": "enabled", "display": ..}}`), so a segment an
/// object lacks but carries as its own `type` tag is consumed without
/// descending.
fn walk<'v>(body: &'v Value, path: &[String]) -> Walk<'v> {
    let mut node = body;
    for key in path {
        node = match node {
            Value::Object(map) => match map.get(key) {
                Some(child) => child,
                None if map.get(UNION_TAG).and_then(Value::as_str) == Some(key.as_str()) => node,
                None if key.parse::<usize>().is_ok() => return Walk::Mismatch,
                None => return Walk::Missing,
            },
            Value::Array(items) => match key.parse::<usize>().ok().and_then(|i| items.get(i)) {
                Some(item) => item,
                None => return Walk::Mismatch,
            },
            _ => return Walk::Mismatch,
        };
    }
    Walk::Found(node)
}

/// The member a tagged union names its variant by.
const UNION_TAG: &str = "type";

/// What field repair made of `pending`: the first repair on the same target
/// for the same field recorded at or after the rejection. A repair whose
/// retry did not succeed left the rejection unrepaired.
fn field_repair_outcome(
    pending: &PendingWireRejection,
    repairs: &[FieldRepair],
) -> FieldRepairOutcome {
    let Some(field) = pending.field.as_ref() else {
        return FieldRepairOutcome::Unrepaired;
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
                FieldRepairOutcome::RepairedLossy
            } else {
                FieldRepairOutcome::Repaired
            }
        }
        _ => FieldRepairOutcome::Unrepaired,
    }
}

#[cfg(test)]
#[path = "wire_rejection_tests.rs"]
mod wire_rejection_tests;
