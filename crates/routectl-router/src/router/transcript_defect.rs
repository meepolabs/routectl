//! A tool call / tool result pairing defect as a per-request provenance fact.
//!
//! The router validates a request's pairing once, before the chain walk, and
//! forwards the transcript unchanged. The defect it finds is recorded on the
//! request and read at three points of the walk:
//!
//! - a Gemini target refuses the request locally, because its translation
//!   looks up each result's call by id and cannot emit a valid body without
//!   the pair;
//! - an upstream `BadRequest` on a defective transcript neither walks the
//!   chain nor teaches any learner: routectl's own validator already explains
//!   it, and the next target would draw the same rejection;
//! - such a rejection is still counted as one the field parser did not
//!   localize, so the parser's blindness ratio keeps its denominator.

use std::collections::HashSet;

use routectl_core::failure_class::FailureClass;
use routectl_core::wire_check::EgressKind;
use routectl_core::{
    ChatRequest, Error, RefusalReason, ToolPairingDefect, ToolPairingError, validate_tool_pairing,
};

use super::capability_learn::LearnDedupeKey;
use super::{DispatchTarget, Router};

/// Validate `req`'s tool pairing and record the defect on its internal
/// state, leaving the transcript untouched. Returns the full error so a
/// local refusal can render the same message the pairing check produces.
pub(super) fn record_transcript_defect(req: &mut ChatRequest) -> Option<ToolPairingError> {
    let err = validate_tool_pairing(&req.messages).err()?;
    tracing::debug!(
        defect = %err.defect(),
        message_index = err.message_index(),
        item_index = ?err.item_index(),
        "tool call/result pairing defect recorded; transcript forwarded unchanged"
    );
    req.routectl_internal.transcript_defect = Some(err.defect());
    Some(err)
}

/// The local refusal a pairing defect becomes where it is refused.
pub(super) fn tool_pairing_refusal(err: &ToolPairingError) -> Error {
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

/// Whether `err` is an upstream `BadRequest` answering a request whose
/// transcript carries `defect`. Such a rejection is laid to the transcript:
/// it does not walk the chain and no learner reads it. A local refusal is
/// never one, even when it classifies `BadRequest`: it says nothing about
/// what the upstream rejected.
pub(super) fn is_transcript_rejection(
    err: &Error,
    class: &FailureClass,
    defect: Option<ToolPairingDefect>,
) -> bool {
    defect.is_some() && matches!(err, Error::Upstream { .. }) && *class == FailureClass::BadRequest
}

impl Router {
    /// Count a rejection laid to the transcript once per request per target
    /// as one the field parser did not localize, under the same eligibility
    /// the parser-blindness observation applies.
    pub(super) fn observe_transcript_rejection(
        &self,
        err: &Error,
        remapped: bool,
        target: &DispatchTarget,
        dedupe: &mut HashSet<LearnDedupeKey>,
    ) {
        if self
            .field_rejection_provider_kind(err, remapped, target)
            .is_some()
        {
            self.count_parser_unlocalized(target, dedupe);
        }
    }
}
