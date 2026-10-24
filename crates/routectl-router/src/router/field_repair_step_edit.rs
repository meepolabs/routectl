//! The edit one repair step makes and how that step settles: [`FieldEdit`]
//! resolves a row's transform against the rejection that named it, and a
//! [`RepairStep`] reports its WARN action and commits its verdict once the
//! plan proves it.

use std::time::Instant;

use routectl_core::ChatRequest;
use routectl_core::wire_check::EditKind;

use crate::field_verdict_value::VerdictValue;

use super::super::super::rejection_parser::{NamedRejection, RejectionKind, RejectionValue};
use super::super::{
    FIELD_ACTION_CEILING_FILL_REPAIR, FIELD_ACTION_CEILING_REPAIR, FIELD_ACTION_DROP_REPAIR,
    FIELD_ACTION_LOSSY_DROP_REPAIR, FIELD_ACTION_REWRITE_HIGHER_REPAIR,
    FIELD_ACTION_REWRITE_REPAIR, FieldSurface, FieldTransform, effort,
};
use super::{RepairStep, SettledStep};

/// The concrete edit one repair makes: a row's transform resolved against the
/// rejection that named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::router::field_repair) enum FieldEdit {
    Drop,
    Rewrite(&'static str),
    Ceiling(u32),
}

impl FieldEdit {
    /// The edit `transform` makes for `named`, or `None` when it makes none:
    /// a rewrite whose rejection named no shape, a ceiling whose rejection
    /// named no ceiling, and a ranked rewrite whose rejection listed no level
    /// to replace the refused one with (a refused field included). `sent` is
    /// the rank bitmask of the effort levels the request already sent, none
    /// of which a ranked rewrite picks again.
    pub(in crate::router::field_repair) fn for_rejection(
        transform: FieldTransform,
        named: &NamedRejection,
        sent: u8,
    ) -> Option<Self> {
        match (transform, named.value) {
            (FieldTransform::Drop, _) => Some(Self::Drop),
            (FieldTransform::Rewrite { .. }, Some(RejectionValue::Shape(value))) => {
                Some(Self::Rewrite(value))
            }
            (FieldTransform::Ceiling, Some(RejectionValue::Ceiling(ceiling))) => {
                Some(Self::Ceiling(ceiling))
            }
            (FieldTransform::RankedRewrite { .. }, Some(RejectionValue::Levels(listed))) => {
                effort::ranked_edit_value(named.row, listed, sent).map(Self::Rewrite)
            }
            (
                FieldTransform::Rewrite { .. },
                Some(RejectionValue::Ceiling(_) | RejectionValue::Levels(_)) | None,
            )
            | (
                FieldTransform::Ceiling,
                Some(RejectionValue::Shape(_) | RejectionValue::Levels(_)) | None,
            )
            | (
                FieldTransform::RankedRewrite { .. },
                Some(RejectionValue::Shape(_) | RejectionValue::Ceiling(_)) | None,
            ) => None,
        }
    }

    pub(super) fn apply_to(self, surface: FieldSurface, req: &mut ChatRequest) -> bool {
        match self {
            Self::Drop => surface.drop_from(req),
            Self::Rewrite(value) => surface.rewrite_in(req, value),
            Self::Ceiling(ceiling) => surface.clamp_to(req, ceiling),
        }
    }

    pub(in crate::router::field_repair) const fn declared_kind(self) -> EditKind {
        match self {
            Self::Drop => EditKind::Remove,
            Self::Rewrite(_) | Self::Ceiling(_) => EditKind::Rewrite,
        }
    }

    /// The value a verdict committed for this edit applies: none for a drop.
    const fn verdict_value(self) -> Option<VerdictValue> {
        match self {
            Self::Drop => None,
            Self::Rewrite(value) => Some(VerdictValue::Shape(value)),
            Self::Ceiling(ceiling) => Some(VerdictValue::Ceiling(ceiling)),
        }
    }
}

impl RepairStep<'_> {
    /// The value this repair wrote, or `None` for a drop.
    pub(super) const fn written_value(&self) -> Option<VerdictValue> {
        self.edit.verdict_value()
    }

    /// The ranked pick this repair made, or `None` for any other repair.
    pub(super) fn ranked_pick(&self) -> Option<effort::RankedPick> {
        match self.edit {
            FieldEdit::Rewrite(value) => effort::ranked_pick(&self.row, value),
            FieldEdit::Drop | FieldEdit::Ceiling(_) => None,
        }
    }

    /// The closed-set WARN action token for this repair.
    pub(super) fn repair_action(&self) -> &'static str {
        let higher = self
            .ranked_pick()
            .is_some_and(|pick| pick.direction == effort::PickDirection::Higher);
        match self.edit {
            FieldEdit::Rewrite(_) if higher => FIELD_ACTION_REWRITE_HIGHER_REPAIR,
            FieldEdit::Rewrite(_) => FIELD_ACTION_REWRITE_REPAIR,
            FieldEdit::Ceiling(_) if self.fill_clamped => FIELD_ACTION_CEILING_FILL_REPAIR,
            FieldEdit::Ceiling(_) => FIELD_ACTION_CEILING_REPAIR,
            FieldEdit::Drop if self.kind == RejectionKind::InvalidValue => {
                FIELD_ACTION_LOSSY_DROP_REPAIR
            }
            FieldEdit::Drop => FIELD_ACTION_DROP_REPAIR,
        }
    }

    /// Whether this repair may commit a verdict once its retry succeeds or
    /// the upstream moves on to another field.
    fn repair_teaches(&self) -> bool {
        self.mintable
            && (self.written_value().is_some() || self.kind == RejectionKind::UnsupportedField)
    }

    /// Persist this step's verdict with `value`, or release its guard when
    /// the repair teaches nothing or the plan cannot settle.
    ///
    /// Only when the rejection refused the FIELD, the repair rewrote it to
    /// the one accepted value the rejection named or to a level its list
    /// named, or it lowered a token limit to the ceiling the rejection named.
    /// A refused value repaired by a DROP
    /// says the lane accepts the field, so a verdict dropping it would strip
    /// every valid value from every later request on the lane.
    pub(super) fn commit(
        self,
        value: Option<VerdictValue>,
        request_features: &[String],
        now: Instant,
    ) -> SettledStep {
        let learned = if self.repair_teaches() {
            self.guard.and_then(|guard| {
                guard.commit(
                    self.tier,
                    value,
                    self.reject_status,
                    request_features.to_vec(),
                    now,
                )
            })
        } else {
            None
        };
        SettledStep {
            handle: self.handle,
            learned,
        }
    }
}
