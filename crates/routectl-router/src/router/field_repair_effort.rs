//! The effort field's closed-table surface and its per-kind scope.
//!
//! One row per effort level, `output_config.effort.<level>`, each PRESENT when
//! the effort the request emits is exactly that level. A ranked rewrite writes
//! the replacement level only into carriers that already hold the refused one,
//! so it never turns thinking on for a request that sent an effort alone. While
//! the target sets a non-empty `effort_levels` the egress clamps the effort
//! after this read, so the wire value is not the one a verdict would key on and
//! every effort row reads absent.

use routectl_core::ChatRequest;
use routectl_core::provider_kind::config as kind;

use super::{FieldRepairRow, FieldTransform, extras_effort};

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

/// Whether `row` rewrites through the effort rank.
pub(super) const fn is_ranked(row: &FieldRepairRow) -> bool {
    matches!(row.transform, FieldTransform::RankedRewrite { .. })
}

/// Which closed-table rows a lane of one provider kind may carry or have a
/// rejection resolve to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RowScope {
    /// Every row.
    #[default]
    All,
    /// Every row but the ranked effort rows: the kind has no captured effort
    /// refusal to ground them.
    WithoutRanked,
}

impl RowScope {
    /// The scope of a lane of `provider_kind`.
    pub(super) fn of_kind(provider_kind: &str) -> Self {
        match provider_kind {
            kind::OPENAI_COMPAT => Self::WithoutRanked,
            _ => Self::All,
        }
    }

    pub(super) const fn admits(self, row: &FieldRepairRow) -> bool {
        match self {
            Self::All => true,
            Self::WithoutRanked => !is_ranked(row),
        }
    }
}
