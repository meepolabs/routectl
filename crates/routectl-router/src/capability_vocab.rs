//! Map-on-read for the persisted capability-event vocabulary.
//!
//! Every capability-event row records the vocabulary version its tokens were
//! written under (`vocab_version`; NULL is the legacy v1 vocabulary). Stored
//! rows are never rewritten: replay maps each row's tokens forward to the
//! current vocabulary, one version step at a time, through a pure per-step
//! rename table, and only then decodes them.
//!
//! A row whose version this build does not know (older than v1, newer than
//! current, or with no step to bridge a gap), whose token a step retires with
//! no current equivalent, or whose version a step retires outright, is skipped
//! WHOLE -- it never reaches an admission arm with some tokens mapped and
//! others not. The caller counts the skip.

use crate::capability_rebuild::CapabilityEventRow;

/// The vocabulary a row with a NULL `vocab_version` was written under.
pub const LEGACY_VOCAB_VERSION: i64 = 1;

/// The vocabulary this build decodes. Rows at an older version are mapped
/// forward to it through `VOCAB_STEPS`.
pub const CURRENT_VOCAB_VERSION: i64 = 2;

/// The token columns a rename may apply to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenField {
    Verdict,
    Phase,
    Source,
    Tier,
    EvidenceClass,
    Capability,
}

/// One token rename in a vocabulary step. `to: None` retires the token: it
/// has no equivalent in the next vocabulary, so a row carrying it is skipped.
#[derive(Debug, Clone, Copy)]
pub struct Rename {
    pub field: TokenField,
    pub from: &'static str,
    pub to: Option<&'static str>,
}

/// The renames that take a row from vocabulary `from` to `from + 1`.
///
/// `retire_all` marks a step whose change is not expressible as a token
/// rename -- the lane-key grammar itself changed -- so no row of version
/// `from` maps forward and every one is skipped; `renames` is then unread.
#[derive(Debug, Clone, Copy)]
pub struct VocabStep {
    pub from: i64,
    pub renames: &'static [Rename],
    pub retire_all: bool,
}

/// The production ladder.
///
/// v1 -> v2 retires every v1 row: v1 keyed a lane by the model nickname, v2
/// by `provider_entry#upstream`. Mapping a nickname to a lane needs the
/// config in force when the row was written, which the ledger does not
/// record, so a v1 row is skipped, counted, and relearned rather than
/// replayed under the wrong lane.
pub const VOCAB_STEPS: &[VocabStep] = &[VocabStep {
    from: 1,
    renames: &[],
    retire_all: true,
}];

/// Why a row was not decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VocabSkip {
    /// The row's version is outside `LEGACY_VOCAB_VERSION..=current`, or the
    /// ladder has no step out of one of the versions it would pass through.
    UnknownVersion(i64),
    /// A step retires a token the row carries.
    RetiredToken { field: TokenField, version: i64 },
    /// A step retires every row of the version the row passes through.
    RetiredVersion(i64),
}

impl VocabSkip {
    /// Stable log token for the skip reason.
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::UnknownVersion(_) => "unknown_vocab_version",
            Self::RetiredToken { .. } => "retired_vocab_token",
            Self::RetiredVersion(_) => "retired_vocab_version",
        }
    }
}

/// Map `row` to the current vocabulary through the production ladder.
pub fn map_to_current(row: CapabilityEventRow) -> Result<CapabilityEventRow, VocabSkip> {
    map_through(row, VOCAB_STEPS, CURRENT_VOCAB_VERSION)
}

/// Map `row` from its stamped version up to `current` through `steps`.
///
/// Pure: the input row is consumed and a fresh row returned, and an error
/// discards the partially-mapped copy, so no caller can observe a row with
/// only some steps applied. The returned row carries `vocab_version =
/// Some(current)`.
pub fn map_through(
    row: CapabilityEventRow,
    steps: &[VocabStep],
    current: i64,
) -> Result<CapabilityEventRow, VocabSkip> {
    let stamped = row.vocab_version.unwrap_or(LEGACY_VOCAB_VERSION);
    if !(LEGACY_VOCAB_VERSION..=current).contains(&stamped) {
        return Err(VocabSkip::UnknownVersion(stamped));
    }
    let mut mapped = row;
    for version in stamped..current {
        let step = steps
            .iter()
            .find(|step| step.from == version)
            .ok_or(VocabSkip::UnknownVersion(stamped))?;
        mapped = apply_step(mapped, step)?;
    }
    mapped.vocab_version = Some(current);
    Ok(mapped)
}

/// Apply one step's renames to every token column of `row`.
fn apply_step(row: CapabilityEventRow, step: &VocabStep) -> Result<CapabilityEventRow, VocabSkip> {
    if step.retire_all {
        return Err(VocabSkip::RetiredVersion(step.from));
    }
    let rename = |field: TokenField, token: String| -> Result<String, VocabSkip> {
        match step
            .renames
            .iter()
            .find(|r| r.field == field && r.from == token)
        {
            None => Ok(token),
            Some(Rename { to: Some(to), .. }) => Ok((*to).to_string()),
            Some(Rename { to: None, .. }) => Err(VocabSkip::RetiredToken {
                field,
                version: step.from,
            }),
        }
    };
    let rename_opt = |field: TokenField, token: Option<String>| -> Result<_, VocabSkip> {
        token.map(|t| rename(field, t)).transpose()
    };
    Ok(CapabilityEventRow {
        verdict: rename(TokenField::Verdict, row.verdict)?,
        phase: rename_opt(TokenField::Phase, row.phase)?,
        source: rename(TokenField::Source, row.source)?,
        tier: rename_opt(TokenField::Tier, row.tier)?,
        evidence_class: rename_opt(TokenField::EvidenceClass, row.evidence_class)?,
        capability: rename(TokenField::Capability, row.capability)?,
        ..row
    })
}

#[cfg(test)]
#[path = "capability_vocab_tests.rs"]
mod tests;
