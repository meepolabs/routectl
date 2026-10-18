//! A tool call / tool result pairing defect as a per-request provenance fact.
//!
//! The router validates a request's pairing once, before the chain walk, and
//! forwards the transcript unchanged. The defect it finds is recorded on the
//! request and read at three points of the walk:
//!
//! - a Gemini target refuses the request locally, because its translation
//!   looks up each result's call by id and cannot emit a valid body without
//!   the pair;
//! - an upstream request-fault rejection (400 or 422) on a defective
//!   transcript, however it classifies or remaps, is not retried, does not
//!   walk the chain, and teaches no learner: routectl's own validator already
//!   explains it, and the same seat or the next target would draw the same
//!   rejection;
//! - such a rejection is still counted as one the field parser did not
//!   localize, so the parser's blindness ratio keeps its denominator.

use std::collections::HashSet;

use routectl_core::wire_check::EgressKind;
use routectl_core::{
    ChatRequest, Error, RefusalReason, ToolPairingDefect, ToolPairingError, validate_tool_pairing,
};

use super::capability_learn::LearnDedupeKey;
use super::rejection_parser::REJECTION_STATUSES;
use super::{DispatchTarget, Router};

/// Validate `req`'s tool pairing and record the result on its internal
/// state, leaving the transcript untouched: the defect it breaks, or `None`
/// when it pairs, so a value left over from an earlier dispatch never
/// stands. Returns the full error so a local refusal can render the same
/// message the pairing check produces.
pub(super) fn record_transcript_defect(req: &mut ChatRequest) -> Option<ToolPairingError> {
    let err = validate_tool_pairing(&req.messages).err();
    req.routectl_internal.transcript_defect = err.as_ref().map(ToolPairingError::defect);
    if let Some(err) = &err {
        tracing::debug!(
            defect = %err.defect(),
            message_index = err.message_index(),
            item_index = ?err.item_index(),
            "tool call/result pairing defect recorded; transcript forwarded unchanged"
        );
    }
    err
}

/// The local refusal a pairing defect becomes where it is refused.
fn tool_pairing_refusal(err: &ToolPairingError) -> Error {
    Error::Validation(err.to_string(), RefusalReason::ToolPairing)
}

/// The refusal `target` raises for a defective transcript: only a Gemini
/// egress refuses, every other target forwards.
pub(super) fn egress_pairing_refusal(
    target: &DispatchTarget,
    pairing: Option<&ToolPairingError>,
) -> Option<Error> {
    if target.egress_kind != Some(EgressKind::Gemini) {
        return None;
    }
    pairing.map(tool_pairing_refusal)
}

/// Whether `err` is an upstream request-fault rejection (a status in
/// [`REJECTION_STATUSES`], the list the field learner reads) answering a
/// request whose transcript carries `defect`. Such a rejection is laid to the transcript: it is not
/// retried, does not walk the chain, and no learner reads it. Decided on the
/// status alone, before any classifier lift or operator remap, because
/// neither changes what the upstream was sent. A local refusal is never one:
/// it says nothing about what the upstream rejected.
pub(super) fn is_transcript_rejection(err: &Error, defect: Option<ToolPairingDefect>) -> bool {
    defect.is_some()
        && matches!(err, Error::Upstream { status, .. } if REJECTION_STATUSES.contains(status))
}

impl Router {
    /// Count a rejection laid to the transcript once per request per target
    /// as one the field parser did not localize. An operator remap does not
    /// exempt it: the remap says how the status routes, not what it was.
    pub(super) fn observe_transcript_rejection(
        &self,
        err: &Error,
        target: &DispatchTarget,
        dedupe: &mut HashSet<LearnDedupeKey>,
    ) {
        if self
            .field_rejection_provider_kind(err, false, target)
            .is_some()
        {
            self.count_parser_unlocalized(target, dedupe);
        }
    }
}
