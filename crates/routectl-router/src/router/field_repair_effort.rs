//! The effort field's closed-table surface and its per-lane scope.
//!
//! One row per effort level, `output_config.effort.<level>`, each PRESENT when
//! the effort the request emits is exactly that level. A ranked rewrite writes
//! the replacement level only into carriers that already hold the refused one,
//! so it never turns thinking on for a request that sent an effort alone. While
//! the target sets a non-empty `effort_levels` the egress clamps the effort
//! after this read, so the wire value is not the one a verdict would key on and
//! every effort row reads absent.
//!
//! A refused level is replaced by the nearest lower level the refusal listed,
//! else the nearest higher one, never by a level the request already sent. A
//! repaired level is learned only when the refusal cannot have depended on the
//! request's thinking shape: the verdict keys on the effort field alone.

use routectl_core::ChatRequest;
use routectl_core::provider_kind::config as kind;
use routectl_core::wire_check::EgressKind;
use routectl_providers::effort::VALID_EFFORT_TOKENS;

use crate::field_verdict::FieldVerdictKey;

use super::super::learn_engine::{EffortRank, ThinkingShape};
use super::super::{CapabilityClearedEvent, DispatchTarget, Router};
use super::{FieldRepairRow, FieldSurface, FieldTransform, extras_effort};

/// The effort the body `req` composes: the client's `output_config.effort`
/// when sent (the egress merges it over the one it derives), else the
/// canonical `reasoning.effort`. A client effort that is not a string emits
/// no level.
pub(super) fn emitted_effort(req: &ChatRequest) -> Option<&str> {
    match extras_effort(req) {
        Some(sent) => sent.as_str(),
        None => req.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
    }
}

/// Whether `req` emits exactly `level` as its effort, read only while the
/// target clamps nothing.
pub(super) fn effort_present(req: &ChatRequest, level: &str) -> bool {
    req.routectl_internal.effort_levels.is_empty() && emitted_effort(req) == Some(level)
}

/// Write `value` into each effort carrier of `req` that holds `refused`: the
/// client's `output_config.effort` and the canonical `reasoning.effort`. A
/// carrier holding anything else, or absent, is left as it is; no `reasoning`
/// or `output_config` is created.
pub(super) fn write_effort(req: &mut ChatRequest, refused: &str, value: &str) {
    if let Some(sent) = req
        .provider_extras
        .as_mut()
        .and_then(|extras| extras.get_mut("output_config"))
        .and_then(|config| config.get_mut("effort"))
        .filter(|sent| sent.as_str() == Some(refused))
    {
        *sent = value.into();
    }
    if let Some(effort) = req
        .reasoning
        .as_mut()
        .and_then(|reasoning| reasoning.effort.as_mut())
        .filter(|effort| effort.as_str() == refused)
    {
        *effort = value.to_string();
    }
}

/// The level that replaces `refused` given the rank bitmask `listed` of the
/// levels a refusal named as accepted: the nearest member below `refused`,
/// else the nearest above it, skipping `refused` itself and every level in
/// the bitmask `excluded`. `None` when no listed level is left.
pub(super) fn ranked_replacement(
    refused: EffortRank,
    listed: u8,
    excluded: u8,
) -> Option<EffortRank> {
    let open = listed & !excluded & !refused.bit();
    let rank = || {
        VALID_EFFORT_TOKENS
            .iter()
            .filter_map(|token| EffortRank::of_token(token))
    };
    let is_open = |level: &EffortRank| open & level.bit() != 0;
    rank()
        .take_while(|level| *level != refused)
        .filter(is_open)
        .last()
        .or_else(|| rank().skip_while(|level| *level != refused).find(is_open))
}

/// The level that replaces the one the ranked row `row` names, for a refusal
/// listing the levels `listed`, never one in the bitmask `sent` of levels the
/// request already sent, nor the refused one.
pub(super) fn ranked_edit_value(
    row: &FieldRepairRow,
    listed: u8,
    sent: u8,
) -> Option<&'static str> {
    let refused = ranked_level(row)?;
    ranked_replacement(refused, listed, sent | refused.bit()).map(EffortRank::token)
}

/// Which way a ranked rewrite moved the effort level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PickDirection {
    Lower,
    /// Above the refused level: the served request may cost more than the
    /// one the client sent.
    Higher,
}

impl PickDirection {
    pub(super) const fn token(self) -> &'static str {
        match self {
            Self::Lower => "lower",
            Self::Higher => "higher",
        }
    }
}

/// A ranked rewrite's refused level, the level it wrote, and which way it
/// moved. Both levels are rank tokens, never text read from a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RankedPick {
    pub(super) refused: &'static str,
    pub(super) chosen: &'static str,
    pub(super) direction: PickDirection,
}

/// The pick a rewrite of the ranked row `row` to `chosen` made, or `None`
/// for any other row or a value outside the rank.
pub(super) fn ranked_pick(row: &FieldRepairRow, chosen: &str) -> Option<RankedPick> {
    let refused = ranked_level(row)?;
    let chosen = EffortRank::of_token(chosen)?;
    let direction = if chosen > refused {
        PickDirection::Higher
    } else {
        PickDirection::Lower
    };
    Some(RankedPick {
        refused: refused.token(),
        chosen: chosen.token(),
        direction,
    })
}

/// The level a ranked row names, or `None` for any other row.
fn ranked_level(row: &FieldRepairRow) -> Option<EffortRank> {
    match row.surface {
        FieldSurface::Effort(token) if is_ranked(row) => EffortRank::of_token(token),
        _ => None,
    }
}

/// The rank bitmask of the levels the ranked rows `rows` name: every level a
/// request whose refusals named those rows has already sent.
pub(super) fn sent_levels<'r>(rows: impl IntoIterator<Item = &'r FieldRepairRow>) -> u8 {
    rows.into_iter()
        .filter_map(ranked_level)
        .fold(0, |mask, level| mask | level.bit())
}

/// Whether a ranked repair of a refusal drawn by an egress body whose thinking
/// shape was `thinking` may be learned. Anthropic-family models refuse some
/// levels only beside a disabled or between_tools thinking, and no capture
/// says whether a body of theirs that omits `thinking` (`Absent`) refuses
/// alike, so there only adaptive and enabled thinking learn. An egress whose
/// body has no thinking object at all (`NoThinkingObject`) learns. A shape
/// that could not be read learns nothing.
pub(super) const fn ranked_step_mints(thinking: ThinkingShape) -> bool {
    match thinking {
        ThinkingShape::Adaptive | ThinkingShape::Enabled | ThinkingShape::NoThinkingObject => true,
        ThinkingShape::Disabled
        | ThinkingShape::BetweenTools
        | ThinkingShape::Absent
        | ThinkingShape::Unknown => false,
    }
}

impl Router {
    /// Clear the resident verdict of the effort level the body `sent` emits,
    /// on `target`'s lane: that body was accepted, so its level is, and a
    /// verdict saying the level is refused would send a later request away
    /// from it, possibly back to the level that was refused in its place.
    /// Keyed on the level that went out, whatever any repair step refused.
    ///
    /// Only where field repair may learn on the lane at all, and only while
    /// the target clamps no effort: a clamped wire value is not the level
    /// the body names. A key in `removed` already ran its removal in this
    /// settlement and is not removed again.
    pub(in crate::router) fn clear_accepted_effort(
        &self,
        target: &DispatchTarget,
        sent: &ChatRequest,
        removed: &[FieldVerdictKey],
    ) -> Option<CapabilityClearedEvent> {
        if !self.config.capability.enabled {
            return None;
        }
        let provider_kind = target.provider_kind?;
        let lane = super::attributable_field_lane(
            &self.config,
            &target.provider_name,
            target.use_forwarded_credential,
        )?;
        if !lane.matches_target(provider_kind, target.egress_kind) {
            return None;
        }
        let table = self
            .field_repair_table()
            .for_lane(provider_kind, Some(lane.egress));
        let row = super::present_rows(table, sent).find(is_ranked)?;
        let key = FieldVerdictKey::new(target.learned_lane.as_ref()?, row.path, provider_kind)?;
        if removed.contains(&key) {
            return None;
        }
        self.field_verdicts()
            .clear_accepted(&key, self.registry_generation())
    }
}

/// Whether `row` rewrites through the effort rank.
pub(in crate::router) const fn is_ranked(row: &FieldRepairRow) -> bool {
    matches!(row.transform, FieldTransform::RankedRewrite { .. })
}

/// Which closed-table rows a lane may carry or have a rejection resolve to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RowScope {
    /// Every row.
    #[default]
    All,
    /// Every row but the ranked effort rows: the kind has no captured effort
    /// refusal to ground them.
    WithoutRanked,
    /// The ranked effort rows alone: the kind's only grounded refusals are of
    /// an effort level, so no other row may be carried or named there.
    RankedOnly,
    /// Exactly the rows at these paths: a lane whose admitted rows are each
    /// grounded by a refusal captured on that lane itself, never borrowed
    /// from a sibling lane that shares its wire dialect.
    Admitted(&'static [&'static str]),
}

/// The row paths a Bedrock Mantle Anthropic Messages lane carries.
pub(super) const ANTHROPIC_API_MANTLE_ROWS: &[&str] = &[];

/// The row paths a Bedrock Mantle Chat Completions lane carries.
pub(super) const OPENAI_COMPAT_MANTLE_ROWS: &[&str] = &[];

/// The row paths a Bedrock Mantle Responses lane carries.
pub(super) const OPENAI_RESPONSES_MANTLE_ROWS: &[&str] = &[];

impl RowScope {
    /// The scope of a lane of `provider_kind` sending with `egress`.
    ///
    /// A Mantle egress answers its own allowlist; every other egress keeps
    /// its provider kind's scope, so a Mantle lane can only narrow what its
    /// direct sibling carries. An unknown egress carries nothing.
    pub(super) fn of_lane(provider_kind: &str, egress: Option<EgressKind>) -> Self {
        match egress {
            None => Self::Admitted(&[]),
            Some(EgressKind::AnthropicApiMantle) => Self::Admitted(ANTHROPIC_API_MANTLE_ROWS),
            Some(EgressKind::OpenaiCompatMantle) => Self::Admitted(OPENAI_COMPAT_MANTLE_ROWS),
            Some(EgressKind::OpenaiResponsesMantle) => Self::Admitted(OPENAI_RESPONSES_MANTLE_ROWS),
            Some(_) => match provider_kind {
                kind::OPENAI_COMPAT => Self::WithoutRanked,
                kind::OPENAI_RESPONSES => Self::RankedOnly,
                _ => Self::All,
            },
        }
    }

    pub(super) fn admits(self, row: &FieldRepairRow) -> bool {
        match self {
            Self::All => true,
            Self::WithoutRanked => !is_ranked(row),
            Self::RankedOnly => is_ranked(row),
            Self::Admitted(paths) => paths.contains(&row.path),
        }
    }
}
