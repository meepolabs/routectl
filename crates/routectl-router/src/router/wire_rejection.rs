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
//! A rejected token count is only read for its site: its body is a
//! projection of the client's, so it is never compared or recorded.
//!
//! Reads only. Nothing here changes the request, the error, or the learned
//! state the repair settles.

use routectl_core::wire_check::{
    AttemptRecord, EgressKind, Lane, LaneClass, Leg, RejectionProvenance, WirePath, WireTap,
};
use routectl_core::{ChatRequest, Error, sanitize_for_log};
use serde_json::Value;

use super::class_observe::upstream_facts;
use super::field_repair::{FIELD_ACTION_LOSSY_DROP_REPAIR, FIELD_ACTION_REWRITE_REPAIR};
use super::learn_engine::RejectionSite;
use super::learn_revert::{
    EditAttribution, MatchedEdit, SentSlot, WireRevert, attributable, revert_outcome,
};
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
        let pending = meta.wire_rejections.len();
        let site = lend_request_record(attempt_req, tap, |record| {
            let record = record?.with_rejection(err);
            compare_request(&self.wire_check, lane, &record);
            Some(read_site(lane, &record, rejected.as_ref(), pending))
        });
        let Some(site) = site else {
            self.wire_check.record_incomplete(lane);
            return None;
        };
        let (path, field) = rejected.map_or((WirePath::root(), None), |named| {
            (named.wire, Some(named.field))
        });
        if let Some(attribution) = EditAttribution::of(&site.matched_edit, site.provenance) {
            self.warn_edit_rejected(lane, &path, attribution);
        }
        if site.matched_edit != MatchedEdit::None {
            meta.wire_reverts.push(WireRevert::declined(pending));
        }
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

    /// Where on the wire a rejected token count points, read from the
    /// attempt's `tap`, warning when the path is laid to a routectl edit.
    /// `None` when there is no tap, no lane, no rejection status, or no
    /// record to read.
    ///
    /// Reads the site only. A count body is an allowlist projection of the
    /// client's, so a fidelity compare would report every field it omits as
    /// lost: nothing is compared, no counter or sampler slot is touched, and
    /// no rejection or revert is held on the dispatch meta.
    ///
    /// Must run before anything mutates `attempt_req` or drains `tap`.
    pub(super) fn read_count_rejection_site(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        err: &Error,
    ) -> Option<RejectionSite> {
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
            Some(read_site(lane, &record?, rejected.as_ref(), 0))
        })?;
        if let Some(attribution) = EditAttribution::of(&site.matched_edit, site.provenance) {
            let path = rejected.map_or_else(WirePath::root, |named| named.wire);
            self.warn_edit_rejected(lane, &path, attribution);
        }
        Some(site)
    }

    /// Record every rejection the request drew, each with what field repair
    /// made of it. Called once, when the dispatch has resolved and every
    /// repair record is final.
    /// A rejection a declared routectl edit explains takes the outcome of
    /// its revert; every other takes what field repair made of it.
    pub(super) fn emit_wire_rejections(&self, meta: &DispatchMeta) {
        for (position, pending) in meta.wire_rejections.iter().enumerate() {
            let outcome = revert_outcome(position, &meta.wire_reverts)
                .unwrap_or_else(|| field_repair_outcome(pending, &meta.field_repairs));
            self.wire_check.record_rejection(
                pending.lane,
                Leg::Request,
                &pending.path,
                pending.provenance,
                outcome,
            );
        }
    }
}

/// What the attempt's record says about the location `rejected` names:
/// where it came from, whether the body that went out carries it, and the
/// declared edits that put it there. `pending` is the rejection's position
/// among the request's pending rejections.
fn read_site(
    lane: Lane,
    record: &AttemptRecord<'_>,
    rejected: Option<&RejectedWirePath>,
    pending: usize,
) -> RejectionSite {
    let path = exact_locator(rejected);
    let sent = path
        .and(record.egress())
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    let provenance = provenance_of(lane, record, path, sent.as_ref());
    let in_egress = path
        .zip(sent.as_ref())
        .and_then(|(path, sent)| egress_location(lane.egress(), sent, path))
        .filter(|&carried| carried || absence_is_evidence(rejected));
    let matched_edit = match rejected {
        Some(named) if attributable(provenance, in_egress) => {
            MatchedEdit::among(record.edits(Leg::Request), &named.wire)
        }
        _ => MatchedEdit::None,
    };
    RejectionSite {
        provenance,
        in_egress,
        matched_edit,
        locator: rejected
            .filter(|named| !named.wire.is_coarsened())
            .map(|named| named.locator.clone()),
        sent: path
            .zip(sent.as_ref())
            .map(|(path, sent)| SentSlot::at(sent, path))
            .unwrap_or_default(),
        pending,
    }
}

/// The unmasked location a rejection names, unless the parser could not
/// localize it or its pointer was coarsened to an ancestor of it.
fn exact_locator(rejected: Option<&RejectedWirePath>) -> Option<&[String]> {
    rejected
        .filter(|named| !named.wire.is_coarsened())
        .map(|named| named.locator.as_slice())
}

/// Whether a walk that misses the named location says the body lacks the
/// field the upstream read. Only a reason refusing something sent there
/// does; any other reason may name the location because it is missing.
fn absence_is_evidence(rejected: Option<&RejectedWirePath>) -> bool {
    rejected.is_some_and(|named| named.presupposes_presence)
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
/// location `path` a rejection names: `Some(false)` only when every segment
/// but the last resolved and the object reached lacks the last key. `None`
/// whenever the body and the path disagree about shape or the walk could
/// read a segment two ways, since then the absence is not evidence. A miss
/// before the last segment is `None` too: the segment may be a tag the
/// validator implied (an untagged union member's default) or one key of a
/// client's that contains a dot.
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
    match walk(sent, path, TagOrKey::Unresolved) {
        Walk::Found(_) => Some(true),
        Walk::Mismatch | Walk::Ambiguous => None,
        Walk::Missing { .. } if egress == EgressKind::BedrockConverse && !bag_prefixed => {
            let bag = sent.get(CONVERSE_FIELD_BAG)?;
            matches!(walk(bag, path, TagOrKey::Unresolved), Walk::Found(_)).then_some(true)
        }
        Walk::Missing { last } => last.then_some(false),
    }
}

/// The value at `path` (raw keys and indices) in `body`.
pub(super) fn locate<'v>(body: &'v Value, path: &[String]) -> Option<&'v Value> {
    match walk(body, path, TagOrKey::Key) {
        Walk::Found(value) => Some(value),
        Walk::Missing { .. } | Walk::Mismatch | Walk::Ambiguous => None,
    }
}

/// How a walk of a raw path through a body ended.
enum Walk<'v> {
    Found(&'v Value),
    /// An object on the path lacks the next key; `last` when that key is
    /// the path's final segment.
    Missing {
        last: bool,
    },
    /// The path asks something the body's shape cannot answer: a key of a
    /// scalar, an index of an object, a non-index of an array, an index past
    /// an array's end.
    Mismatch,
    /// An object carries the next segment both as its own key and as its
    /// `type` tag, and the walk was told not to pick.
    Ambiguous,
}

/// How a walk reads a segment an object carries both as a key and as its
/// `type` tag.
#[derive(Clone, Copy)]
enum TagOrKey {
    /// Descend into the key.
    Key,
    /// End the walk as [`Walk::Ambiguous`].
    Unresolved,
}

/// Walk `path` (raw keys and indices) through `body`. A rejection names a
/// tagged-union member by its tag (`thinking.enabled.display` for
/// `{"thinking": {"type": "enabled", "display": ..}}`), so a segment an
/// object lacks but carries as its own `type` tag is consumed without
/// descending. A segment the object carries both ways is read per `both`.
fn walk<'v>(body: &'v Value, path: &[String], both: TagOrKey) -> Walk<'v> {
    let mut node = body;
    for (at, key) in path.iter().enumerate() {
        node = match node {
            Value::Object(map) => {
                let tagged = map.get(UNION_TAG).and_then(Value::as_str) == Some(key.as_str());
                match (map.get(key), tagged, both) {
                    (Some(_), true, TagOrKey::Unresolved) => return Walk::Ambiguous,
                    (Some(child), _, _) => child,
                    (None, true, _) => node,
                    (None, false, _) if key.parse::<usize>().is_ok() => return Walk::Mismatch,
                    (None, false, _) => {
                        return Walk::Missing {
                            last: at + 1 == path.len(),
                        };
                    }
                }
            }
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
/// retry did not succeed left the rejection unrepaired. A rewrite is lossy:
/// the request served carried a value the client did not send.
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
        Some(repair) if repair.repair_succeeded => match repair.action {
            FIELD_ACTION_LOSSY_DROP_REPAIR | FIELD_ACTION_REWRITE_REPAIR => {
                FieldRepairOutcome::RepairedLossy
            }
            _ => FieldRepairOutcome::Repaired,
        },
        _ => FieldRepairOutcome::Unrepaired,
    }
}

#[cfg(test)]
#[path = "wire_rejection_tests.rs"]
mod wire_rejection_tests;
