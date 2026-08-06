//! Pure candidate builder for a catalog overlay import: derives
//! [`GeneratedCell`]s from two already-fetched vendor `Value`s via
//! [`derive_cells`] and maps them onto [`OverlayCell`] candidates under
//! the GROUP-AND-AGREE rule.
//!
//! GROUP-AND-AGREE: the overlay key (`provider_kind:model_glob`) is
//! TIER-AGNOSTIC, but `wm` / `ttl_seconds` are TIER-SPECIFIC for the
//! Anthropic-shaped selectors that carry a 5-minute and a 1-hour row
//! under the same key. Writing either tier's `wm` onto the shared overlay
//! key would silently apply the wrong multiplier to the other tier's
//! requests once `crate::catalog`'s merge applies it. So
//! [`build_import_candidate`] groups the derived cells by selector and
//! includes a field on the candidate [`OverlayCell`] ONLY when every cell
//! in the group agrees on it; a disagreeing field is OMITTED and stays
//! baked-authoritative. `rm` / `max_context_tokens` / the base per-token
//! rates / `capabilities` are computed once per selector and shared across
//! tiers, so they always agree and always import; a single-cell
//! (auto-cacher) family agrees with itself trivially and imports every
//! field.
//!
//! EMPTY ALLOWLIST: this module always calls `derive_cells` with
//! [`Allowlist::empty`] -- the checked-in
//! `catalog_data/cross_check_allowlist.json` resolves noise specific to
//! the vendored codegen snapshots, which does not apply to freshly
//! fetched sources. A selector whose two sources disagree therefore
//! always fails the cross-check here, even for a mismatch codegen would
//! resolve via that allowlist. Because that skip is EXPECTED rather than
//! exceptional, it also has to undo any earlier run's cell for the same
//! selector: see [`stale_import_cells`].
//!
//! PER-SELECTOR PARTITION: `derive_cells` returns one `Result` per static
//! selector and never short-circuits, so one selector's cross-check
//! failure never aborts the rest of the candidate -- it is partitioned
//! into [`ImportCandidate::skipped`] instead. Source-level failures
//! (timeout, non-200, invalid JSON) are a CLI fetch-boundary concern, not
//! this module's: it only ever sees already-parsed `Value`s.
//!
//! ADMISSION: a selector is added to the candidate only when it is
//! present in the baked catalog table ([`crate::catalog::baked_table_rows`]).
//! `derive_cells` only ever enumerates selectors from the same static
//! tables the baked table is itself generated from, so this should never
//! reject a real selector in practice -- it is a guard against the
//! selector tables drifting ahead of a stale, not-yet-regenerated baked
//! table, not a live rejection path.
//!
//! `OverlayCell` has no `auto_cacher` / `storage_rent` fields (settled):
//! the import cannot structurally produce a differing `auto_cacher` (the
//! static selector table is read identically by codegen and by this
//! module), and storage-rent fields are reserved-unused on every baked
//! row.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::catalog::{CatalogRow, baked_table_rows};
use crate::catalog_codegen::{
    Allowlist, GeneratedCell, derive_cells, reason_is_cross_check_mismatch,
};
use crate::catalog_codegen_selectors::{
    ANTHROPIC_SELECTORS, BEDROCK_SELECTORS, CATCH_ALL_ROWS, OPENAI_COMPAT_SELECTORS,
    OPENAI_RESPONSES_SELECTORS,
};
use crate::catalog_overlay::{CatalogOverlay, OverlayCell, OverlaySource};
use crate::catalog_state::{ImpactClass, ImpactField, classify_field, escalate, selector_key};

/// Marks where an import candidate's source data came from.
/// `#[non_exhaustive]` with a single variant: a future producer (e.g. a
/// usage-derived refresh) is meant to flow through the same
/// [`build_import_candidate`] -> diff -> confirm pipeline as just another
/// candidate origin, not a parallel code path -- no trait, since a second
/// variant is the only extension this seam needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CandidateOrigin {
    /// Derived from the vendored-doc refresh sources (litellm + models.dev).
    DocRefresh,
}

/// One selector the group-and-agree mapper could not admit into a
/// candidate: an unknown selector (see the module doc's admission note)
/// or a per-selector cross-check disagreement between the two sources
/// (see the module doc's PER-SELECTOR PARTITION note). [`diff_overlay`]
/// carries an [`ImportCandidate`]'s skip list into its own
/// [`ImportDiff::skipped`] verbatim -- it never adds a new skip reason of
/// its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSelector {
    /// The selector key that was skipped.
    pub selector: String,
    /// Human-readable reason for the skip.
    pub reason: String,
    /// Machine-readable skip discriminator.
    pub kind: SkipKind,
}

/// Why the group-and-agree mapper could not admit a selector -- the
/// machine-readable discriminator beside [`SkippedSelector::reason`]'s
/// human string. Only [`SkipKind::CrossCheckDisagreement`] counts the
/// selector as PRESENT toward the shrink-guard family/source totals (see
/// [`candidate_shrink_counts`]); every other kind, and the fail-safe
/// [`SkipKind::Other`] default, leaves the selector uncounted so a
/// genuinely-vanished model still trips the guard. A skip site that
/// forgets to set a kind therefore fails SAFE (strict), never looser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipKind {
    /// The two freshly-fetched sources disagree on a field for this
    /// selector (see the module doc's PER-SELECTOR PARTITION note): an
    /// EXPECTED skip under the empty allowlist, so the selector still
    /// counts as present -- the model has not vanished, its two sources
    /// merely refreshed at different times.
    CrossCheckDisagreement,
    /// The selector is not present in the baked catalog table (see the
    /// module doc's admission note). Not counted.
    UnknownSelector,
    /// A derived cell carried a degenerate value (see
    /// `validate_candidate_cell`). Not counted.
    DegenerateValue,
    /// Any other skip -- the fail-safe default so an un-tagged skip is
    /// never counted and the shrink guard stays strict.
    #[default]
    Other,
}

/// One built import candidate: per-selector [`OverlayCell`]s ready to
/// merge into the overlay, plus every selector the group-and-agree
/// mapper had to skip (unknown selector, or a per-selector cross-check
/// disagreement) with a human-readable reason. This module performs no
/// I/O; the caller decides what to do with `cells` and `skipped`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportCandidate {
    /// Where this candidate's rows were sourced from.
    pub origin: CandidateOrigin,
    /// The run date stamped on every candidate cell -- ONE value for the
    /// whole import run (candidate materialization time), passed in by
    /// the caller rather than read from the wall clock (this module is
    /// pure).
    pub verified_at: String,
    /// Per-selector overlay cells ready to merge.
    pub cells: BTreeMap<String, OverlayCell>,
    /// Selectors the group-and-agree mapper had to skip, with reasons.
    pub skipped: Vec<SkippedSelector>,
}

/// Build an [`ImportCandidate`] from two already-fetched, already-parsed
/// source `Value`s. Pure: no filesystem, no network, no clock --
/// `verified_at` is the caller's already-decided run date for the whole
/// import.
///
/// See the module doc for the group-and-agree mapping rule, the empty
/// allowlist, the per-selector partition, and admission.
#[must_use]
pub fn build_import_candidate(
    origin: CandidateOrigin,
    litellm: &Value,
    models_dev: &Value,
    verified_at: &str,
) -> ImportCandidate {
    let allowlist = Allowlist::empty();
    let known_selectors = known_selector_keys();

    let mut cells = BTreeMap::new();
    let mut skipped = Vec::new();
    for (key, result) in derive_cells(litellm, models_dev, &allowlist) {
        match result {
            Ok(group) => match admit_group(&key, &group, &known_selectors, verified_at) {
                Ok((selector, cell)) => {
                    cells.insert(selector, cell);
                }
                Err(skip) => skipped.push(skip),
            },
            Err(reason) => {
                // `derive_cells` returns a source cross-check
                // disagreement (counts as present) OR a missing-key /
                // absent-data error (a real absence -- not counted).
                let kind = if reason_is_cross_check_mismatch(&reason) {
                    SkipKind::CrossCheckDisagreement
                } else {
                    SkipKind::Other
                };
                skipped.push(SkippedSelector {
                    selector: key,
                    reason,
                    kind,
                });
            }
        }
    }

    ImportCandidate {
        origin,
        verified_at: verified_at.to_string(),
        cells,
        skipped,
    }
}

/// Admit one selector's derived cell group into the candidate, or reject
/// it as an unknown selector (see the module doc's admission note) or as
/// carrying a degenerate derived value (see [`validate_candidate_cell`]).
/// `key` is the selector key [`derive_cells`] tagged this group with --
/// the caller's own attribution, never re-derived from `group` here.
fn admit_group(
    key: &str,
    group: &[GeneratedCell],
    known_selectors: &BTreeSet<String>,
    verified_at: &str,
) -> Result<(String, OverlayCell), SkippedSelector> {
    if !known_selectors.contains(key) {
        return Err(SkippedSelector {
            selector: key.to_string(),
            reason: "selector is not present in the baked catalog table".to_string(),
            kind: SkipKind::UnknownSelector,
        });
    }
    let cell = group_and_agree(group, verified_at);
    validate_candidate_cell(key, &cell)?;
    Ok((key.to_string(), cell))
}

/// Reject a candidate cell carrying a non-finite or non-positive `wm` /
/// `rm`, a zero `max_context_tokens` / `ttl_seconds`, or a negative /
/// non-finite base per-token rate -- a vendor
/// snapshot publishing a degenerate derived number (e.g. a negative or
/// zero cache-read price, or an overflowing division) should skip that
/// one selector rather than poison the break-even math downstream. Only
/// fields the candidate actually SET are checked -- a field omitted by
/// [`group_and_agree`]'s agreement rule stays baked-authoritative and is
/// validated there instead, not here.
fn validate_candidate_cell(key: &str, cell: &OverlayCell) -> Result<(), SkippedSelector> {
    let invalid = |field: &str, value: String| SkippedSelector {
        selector: key.to_string(),
        reason: format!("invalid derived value: {field}={value}"),
        kind: SkipKind::DegenerateValue,
    };
    if let Some(wm) = cell.wm
        && (!wm.is_finite() || wm <= 0.0)
    {
        return Err(invalid("wm", wm.to_string()));
    }
    if let Some(rm) = cell.rm
        && (!rm.is_finite() || rm <= 0.0)
    {
        return Err(invalid("rm", rm.to_string()));
    }
    if let Some(max_context_tokens) = cell.max_context_tokens
        && max_context_tokens == 0
    {
        return Err(invalid(
            "max_context_tokens",
            max_context_tokens.to_string(),
        ));
    }
    if let Some(ttl_seconds) = cell.ttl_seconds
        && ttl_seconds == 0
    {
        return Err(invalid("ttl_seconds", ttl_seconds.to_string()));
    }
    // A negative or non-finite base rate is source corruption, not a price.
    // Zero is allowed: a genuinely free tier is a real vendor offering.
    for (field, rate) in [
        ("input_cost_per_token", cell.input_cost_per_token),
        ("output_cost_per_token", cell.output_cost_per_token),
    ] {
        if let Some(rate) = rate
            && (!rate.is_finite() || rate < 0.0)
        {
            return Err(invalid(field, rate.to_string()));
        }
    }
    Ok(())
}

/// Build one candidate [`OverlayCell`] from a group of [`GeneratedCell`]s
/// that share the same selector key: a value-bearing field lands on the
/// candidate ONLY when every cell in the group agrees on it (see the
/// module doc's GROUP-AND-AGREE rule); a disagreeing field is omitted so
/// it stays baked-authoritative at merge time.
fn group_and_agree(group: &[GeneratedCell], verified_at: &str) -> OverlayCell {
    OverlayCell {
        source: OverlaySource::Import,
        verified_at: verified_at.to_string(),
        wm: agree(group, |cell| cell.wm),
        rm: agree(group, |cell| cell.rm),
        ttl_seconds: agree(group, |cell| cell.ttl_seconds),
        min_prefix_tokens: agree(group, |cell| cell.min_prefix_tokens),
        max_context_tokens: agree(group, |cell| cell.max_context_tokens).flatten(),
        input_cost_per_token: agree(group, |cell| cell.input_cost_per_token).flatten(),
        output_cost_per_token: agree(group, |cell| cell.output_cost_per_token).flatten(),
        capabilities: agree(group, |cell| cell.capabilities.clone())
            .map(|caps| caps.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
    }
}

/// `Some(value)` when every cell in `group` agrees on `get`'s projection,
/// `None` on any disagreement (or an empty group, which never occurs in
/// practice -- every static selector table entry derives at least one
/// cell).
fn agree<T: PartialEq>(group: &[GeneratedCell], get: impl Fn(&GeneratedCell) -> T) -> Option<T> {
    let mut values = group.iter().map(get);
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

/// The selector keys the baked catalog table actually carries -- the
/// admission set (see the module doc).
fn known_selector_keys() -> BTreeSet<String> {
    baked_table_rows()
        .into_iter()
        .map(|row| selector_key(row.provider_kind, row.model_glob))
        .collect()
}

// ---------------------------------------------------------------------------
// diff_overlay: candidate + current overlay -> applied / skipped / conflicted
// ---------------------------------------------------------------------------

/// What the overlay currently carries for one selector, mirroring
/// [`CatalogOverlay::cells`]'s own three-state value shape
/// (`Option<Option<OverlayCell>>`) without the nested-`Option` type --
/// see [`DiffRow::existing`].
#[derive(Debug, Clone, PartialEq)]
pub enum ExistingCell {
    /// No overlay entry for this selector.
    Absent,
    /// Explicitly disabled (overlay key present, JSON `null`).
    Disabled,
    /// An existing cell, either `source: import` or `source: user`.
    Present(OverlayCell),
}

impl ExistingCell {
    fn from_overlay(overlay: &CatalogOverlay, selector: &str) -> Self {
        match overlay.cells.get(selector) {
            None => Self::Absent,
            Some(None) => Self::Disabled,
            Some(Some(cell)) => Self::Present(cell.clone()),
        }
    }

    const fn cell(&self) -> Option<&OverlayCell> {
        match self {
            Self::Present(cell) => Some(cell),
            Self::Absent | Self::Disabled => None,
        }
    }
}

/// One row of an [`ImportDiff`]: the candidate cell proposed for
/// `selector`, what the overlay currently carries there
/// ([`ExistingCell`]), the escalated [`ImpactClass`] of every field the
/// candidate actually changes relative to the CURRENT effective value
/// (baked, or the existing cell's own fields where it sets them), and
/// whether that change trends toward a lower break-even reuse count
/// (`wm` down or `rm` up -- see `crate::cost_gate::break_even_k`).
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    /// The selector key this row targets.
    pub selector: String,
    /// The overlay cell the candidate would write.
    pub candidate: OverlayCell,
    /// The current effective value this row is diffed against.
    pub existing: ExistingCell,
    /// How impactful the change is (see [`ImpactClass`]).
    pub impact: ImpactClass,
    /// Whether the change trends toward a lower break-even reuse count.
    pub cheaper_direction: bool,
}

/// The result of diffing an [`ImportCandidate`] against the current
/// overlay: every selector the candidate could write, sorted into
/// exactly one bucket. `skipped` carries [`ImportCandidate::skipped`]
/// verbatim (the per-selector cross-check disagreements
/// [`build_import_candidate`] already partitioned out) -- nothing here
/// adds a NEW skip reason; `diff_overlay` only classifies what the
/// candidate actually built.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportDiff {
    /// Lands on a fresh selector, or freely overwrites the selector's
    /// own prior `source: import` cell.
    pub applied: Vec<DiffRow>,
    /// Selectors the candidate could not admit, carried verbatim from
    /// [`ImportCandidate::skipped`].
    pub skipped: Vec<SkippedSelector>,
    /// A `source: user` cell already owns this selector (or the
    /// selector is explicitly disabled) -- the existing value is
    /// preserved untouched; this row is never applied.
    pub conflicted: Vec<DiffRow>,
    /// Selectors whose STALE `source: import` cell must be REMOVED from
    /// the overlay (see `stale_import_cells`).
    pub cleared: Vec<String>,
}

/// Diff `candidate` against `current_overlay`, classifying every
/// candidate cell as applied, conflicted, or (via
/// `candidate.skipped`) already-skipped. `baked` is the caller's
/// selector-keyed baked-table snapshot (see [`baked_row_map`]) --
/// [`build_import_candidate`]'s admission rule guarantees every
/// candidate selector has a matching baked row in practice, but a
/// lookup miss degrades gracefully (the CURRENT-effective-value
/// comparison simply has no baked fallback for that field).
///
/// `source: user` CELLS ARE NEVER TOUCHED: an existing user cell (or an
/// explicit disable) always sorts the row into `conflicted`, regardless
/// of whether the candidate's values happen to coincide with what is
/// already there -- the overlay stores one `OverlayCell` per selector
/// with ONE `source` for the whole row, so there is no partial per-field
/// ownership to merge around. An absent key, or an existing
/// `source: import` cell, always sorts into `applied`.
///
/// A selector the candidate SKIPPED for a cross-check disagreement gets
/// no row at all, but its stale `source: import` cell (if any) is listed
/// in [`ImportDiff::cleared`] -- see `stale_import_cells`.
#[must_use]
pub fn diff_overlay(
    current_overlay: &CatalogOverlay,
    candidate: &ImportCandidate,
    baked: &BTreeMap<String, CatalogRow>,
) -> ImportDiff {
    let mut diff = ImportDiff {
        skipped: candidate.skipped.clone(),
        cleared: stale_import_cells(current_overlay, &candidate.skipped),
        ..ImportDiff::default()
    };

    for (selector, cell) in &candidate.cells {
        let baked_row = baked.get(selector);
        let existing = ExistingCell::from_overlay(current_overlay, selector);
        let is_conflict = match &existing {
            ExistingCell::Absent => false,
            ExistingCell::Disabled => true,
            ExistingCell::Present(cell) => cell.source == OverlaySource::User,
        };
        let diff_row = row(selector, cell, existing, baked_row);
        if is_conflict {
            diff.conflicted.push(diff_row);
        } else {
            diff.applied.push(diff_row);
        }
    }

    diff
}

/// The selectors whose existing `source: import` cell must be REMOVED
/// from the overlay because this run skipped them for a cross-check
/// disagreement.
///
/// A skipped selector produces no candidate cell, so without this the
/// PREVIOUS run's `source: import` cell would survive and keep
/// overriding the vetted baked row indefinitely -- an operator who
/// imported an older snapshot pair would stay pinned to that snapshot's
/// value even after upgrading to a baked catalog that corrects it. The
/// baked row is the cross-checked, allowlist-resolved, reviewed value;
/// an unverifiable import cell is not, so removing the cell (and letting
/// the merge fall back to baked) is the fail-safe direction.
///
/// `source: user` cells are NEVER cleared -- an operator override
/// outranks both the import and the baked row. An explicitly disabled
/// selector (overlay key present, JSON `null`) is likewise left alone:
/// the disable is an operator decision, not a stale import.
///
/// Only [`SkipKind::CrossCheckDisagreement`] clears. The other kinds
/// carry no such guarantee: [`SkipKind::UnknownSelector`] has no baked
/// row to fall back to by definition, and
/// [`SkipKind::DegenerateValue`] / [`SkipKind::Other`] would clear on
/// transient source corruption. Leaving those cells in place keeps this
/// narrow.
fn stale_import_cells(overlay: &CatalogOverlay, skipped: &[SkippedSelector]) -> Vec<String> {
    skipped
        .iter()
        .filter(|skip| skip.kind == SkipKind::CrossCheckDisagreement)
        .filter(|skip| is_import_cell(overlay, &skip.selector))
        .map(|skip| skip.selector.clone())
        .collect()
}

/// `true` when `overlay` currently carries a `source: import` cell for
/// `selector` -- the only state `stale_import_cells` is allowed to
/// clear.
#[must_use]
pub fn is_import_cell(overlay: &CatalogOverlay, selector: &str) -> bool {
    matches!(
        ExistingCell::from_overlay(overlay, selector),
        ExistingCell::Present(cell) if cell.source == OverlaySource::Import
    )
}

/// `true` when applying `diff` would write nothing new to the overlay:
/// it clears no stale import cell, AND every applied row's candidate cell
/// is byte-identical (including `verified_at`) to the overlay cell
/// already sitting there under that selector. Vacuously `true` when both
/// `diff.cleared` and `diff.applied` are empty -- the pre-existing no-op
/// case this extends.
///
/// A non-empty [`ImportDiff::cleared`] is ALWAYS an effective change:
/// every entry names a cell that exists on disk right now and must be
/// removed (see `stale_import_cells`), so the write cannot be skipped.
///
/// A row only reaches `applied` with `ExistingCell::Absent` or
/// `ExistingCell::Present` (never `Disabled`, which always sorts into
/// `conflicted` -- see [`diff_overlay`]'s doc). `Absent` is never a
/// no-op: there is nothing yet on disk to match, so a fresh selector
/// always counts as a real change. `Present` is a no-op exactly when the
/// existing cell equals the candidate field-for-field -- which, for a
/// same-day re-import, includes `verified_at`: both runs stamp the same
/// calendar date, so a byte-identical source pair produces a
/// byte-identical candidate cell. A re-import on a LATER day moves
/// `verified_at` even with unchanged prices, and that counts as a real
/// change, same as any other field's drift.
#[must_use]
pub fn diff_has_no_effective_change(diff: &ImportDiff) -> bool {
    diff.cleared.is_empty()
        && diff.applied.iter().all(|row| {
            matches!(&row.existing, ExistingCell::Present(existing) if *existing == row.candidate)
        })
}

/// Build one [`DiffRow`], escalating the impact class of every
/// candidate field that actually differs from the CURRENT effective
/// value: `existing`'s own field when it sets one, else `baked`'s.
/// Falls back to [`ImpactField::VerifiedAt`]'s (display-only) class
/// when no value field differs -- the row still stamps a fresh
/// `verified_at`, even if every priced field coincides with what is
/// already in effect. A [`ExistingCell::Disabled`] selector always
/// starts from [`ImpactField::Enablement`]'s routing-affecting class:
/// re-enabling an operator-disabled row is itself a routing change,
/// regardless of what its value fields say.
fn row(
    selector: &str,
    candidate: &OverlayCell,
    existing: ExistingCell,
    baked: Option<&CatalogRow>,
) -> DiffRow {
    let existing_cell = existing.cell();
    let mut impact = if matches!(existing, ExistingCell::Disabled) {
        classify_field(ImpactField::Enablement)
    } else {
        ImpactClass::DisplayOnly
    };
    let mut cheaper_direction = false;

    let effective_wm = existing_cell
        .and_then(|c| c.wm)
        .or_else(|| baked.map(|b| b.wm));
    if let Some(new_wm) = candidate.wm
        && effective_wm != Some(new_wm)
    {
        impact = escalate(impact, classify_field(ImpactField::Wm));
        if effective_wm.is_some_and(|old_wm| new_wm < old_wm) {
            cheaper_direction = true;
        }
    }

    let effective_rm = existing_cell
        .and_then(|c| c.rm)
        .or_else(|| baked.map(|b| b.rm));
    if let Some(new_rm) = candidate.rm
        && effective_rm != Some(new_rm)
    {
        impact = escalate(impact, classify_field(ImpactField::Rm));
        if effective_rm.is_some_and(|old_rm| new_rm > old_rm) {
            cheaper_direction = true;
        }
    }

    let effective_ttl = existing_cell
        .and_then(|c| c.ttl_seconds)
        .or_else(|| baked.map(|b| b.ttl_seconds));
    if candidate.ttl_seconds.is_some() && candidate.ttl_seconds != effective_ttl {
        impact = escalate(impact, classify_field(ImpactField::TtlSeconds));
    }

    let effective_min_prefix = existing_cell
        .and_then(|c| c.min_prefix_tokens)
        .or_else(|| baked.map(|b| b.min_prefix_tokens));
    if candidate.min_prefix_tokens.is_some() && candidate.min_prefix_tokens != effective_min_prefix
    {
        impact = escalate(impact, classify_field(ImpactField::MinPrefixTokens));
    }

    let effective_max_context = existing_cell
        .and_then(|c| c.max_context_tokens)
        .or_else(|| baked.and_then(|b| b.max_context_tokens));
    if candidate.max_context_tokens.is_some()
        && candidate.max_context_tokens != effective_max_context
    {
        impact = escalate(impact, classify_field(ImpactField::MaxContextTokens));
    }

    let effective_input_cost = existing_cell
        .and_then(|c| c.input_cost_per_token)
        .or_else(|| baked.and_then(|b| b.input_cost_per_token));
    if candidate.input_cost_per_token.is_some()
        && candidate.input_cost_per_token != effective_input_cost
    {
        impact = escalate(impact, classify_field(ImpactField::InputCostPerToken));
    }

    let effective_output_cost = existing_cell
        .and_then(|c| c.output_cost_per_token)
        .or_else(|| baked.and_then(|b| b.output_cost_per_token));
    if candidate.output_cost_per_token.is_some()
        && candidate.output_cost_per_token != effective_output_cost
    {
        impact = escalate(impact, classify_field(ImpactField::OutputCostPerToken));
    }

    let effective_capabilities = existing_cell
        .and_then(|c| c.capabilities.clone())
        .or_else(|| baked.map(|b| b.capabilities.clone()));
    if candidate.capabilities.is_some() && candidate.capabilities != effective_capabilities {
        impact = escalate(impact, classify_field(ImpactField::Capabilities));
    }

    if impact == ImpactClass::DisplayOnly {
        impact = classify_field(ImpactField::VerifiedAt);
    }

    DiffRow {
        selector: selector.to_string(),
        candidate: candidate.clone(),
        existing,
        impact,
        cheaper_direction,
    }
}

/// The caller-supplied `baked` argument [`diff_overlay`] compares
/// candidate values against: every baked-table row, keyed the same way
/// [`ImportCandidate::cells`] is.
#[must_use]
pub fn baked_row_map() -> BTreeMap<String, CatalogRow> {
    baked_table_rows()
        .into_iter()
        .map(|entry| {
            (
                selector_key(entry.provider_kind, entry.model_glob),
                entry.row,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shrink guard: per-source and per-family row-count floors, pure decision
// over caller-supplied counts (baseline I/O lives in
// `crate::catalog_import_state`).
// ---------------------------------------------------------------------------

/// Selector row counts partitioned two ways: `per_source` by provider
/// kind (`anthropic-api` / `bedrock` / `openai-responses` /
/// `openai-compat`), `per_family` by the finer vendor grouping within
/// each provider kind (see `family_table`) -- an `openai-compat`-wide
/// aggregate would hide one vendor's snapshot truncating behind the
/// other vendors' healthy rows, so the guard checks both granularities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShrinkCounts {
    /// Present-row count per source (provider kind).
    pub per_source: BTreeMap<String, usize>,
    /// Present-row count per family (vendor grouping).
    pub per_family: BTreeMap<String, usize>,
}

/// One source (provider kind) that fell below the shrink guard's floor,
/// or dropped to zero after previously contributing rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrunkSource {
    /// The source (provider kind) name.
    pub source: String,
    /// Row count in the current baseline.
    pub baseline: usize,
    /// Row count in the candidate.
    pub candidate: usize,
}

/// One family (vendor grouping) that fell below its size-scaled floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrunkFamily {
    /// The family (vendor grouping) name.
    pub family: String,
    /// Row count in the current baseline.
    pub baseline: usize,
    /// Row count in the candidate.
    pub candidate: usize,
    /// The size-scaled floor the candidate had to meet.
    pub required: usize,
}

/// The shrink guard's report: which sources/families shrank, by name,
/// and whether that makes the guard's verdict a rejection. Bypassing a
/// rejection (`--allow-shrink`) is the CALLER's decision (the `catalog
/// import` CLI command) -- this module only ever reports what shrank,
/// never a bypass flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShrinkVerdict {
    /// Sources that fell below their floor.
    pub shrunk_sources: Vec<ShrunkSource>,
    /// Sources that dropped to zero after previously contributing rows.
    pub zero_sources: Vec<ShrunkSource>,
    /// Families that fell below their size-scaled floor.
    pub shrunk_families: Vec<ShrunkFamily>,
}

impl ShrinkVerdict {
    /// `true` when at least one source or family fell below its floor.
    #[must_use]
    pub const fn is_shrunk(&self) -> bool {
        !self.shrunk_sources.is_empty()
            || !self.zero_sources.is_empty()
            || !self.shrunk_families.is_empty()
    }
}

/// Per-source floor: a source must retain at least this percentage of
/// its baseline row count.
const PER_SOURCE_FLOOR_PCT: usize = 90;
/// A family whose baseline row count is at or below this many rows must
/// retain EVERY row (exact preservation); above it, the percentage
/// floor applies instead.
const SMALL_FAMILY_MAX_BASELINE: usize = 5;
/// Per-family floor for a family above [`SMALL_FAMILY_MAX_BASELINE`].
const LARGE_FAMILY_FLOOR_PCT: usize = 80;

/// Evaluate the shrink guard: `candidate` counts (derived from the
/// candidate's OWN selectors, never the overlay -- user edits would
/// otherwise pollute the baseline) against `baseline` counts (the last
/// successful import's persisted counts, or the baked-table fallback on
/// a first run -- see `crate::catalog_import_state::load_baseline`).
/// Pure: takes both count sets as plain data, no I/O.
///
/// RULES (verbatim): a source retains its rows only if
/// `candidate >= 90% * baseline`, AND a source that contributed at
/// least one baseline row never drops to exactly zero (reported
/// separately from an ordinary shrink, though the same floor already
/// catches it mathematically -- the caller gets a clearer "this source
/// vanished" message). A family with `baseline <= 5` must retain every
/// row (`candidate >= baseline`); above that, `candidate >= 80% *
/// baseline`. A source/family absent from `baseline` (baseline count 0)
/// is never checked -- it never contributed rows to shrink.
#[must_use]
pub fn shrink_guard(candidate: &ShrinkCounts, baseline: &ShrinkCounts) -> ShrinkVerdict {
    let mut verdict = ShrinkVerdict::default();

    for (source, &baseline_count) in &baseline.per_source {
        if baseline_count == 0 {
            continue;
        }
        let candidate_count = candidate.per_source.get(source).copied().unwrap_or(0);
        if candidate_count == 0 {
            verdict.zero_sources.push(ShrunkSource {
                source: source.clone(),
                baseline: baseline_count,
                candidate: candidate_count,
            });
        } else if candidate_count < required_count(baseline_count, PER_SOURCE_FLOOR_PCT) {
            verdict.shrunk_sources.push(ShrunkSource {
                source: source.clone(),
                baseline: baseline_count,
                candidate: candidate_count,
            });
        }
    }

    for (family, &baseline_count) in &baseline.per_family {
        if baseline_count == 0 {
            continue;
        }
        let candidate_count = candidate.per_family.get(family).copied().unwrap_or(0);
        let required = if baseline_count <= SMALL_FAMILY_MAX_BASELINE {
            baseline_count
        } else {
            required_count(baseline_count, LARGE_FAMILY_FLOOR_PCT)
        };
        if candidate_count < required {
            verdict.shrunk_families.push(ShrunkFamily {
                family: family.clone(),
                baseline: baseline_count,
                candidate: candidate_count,
                required,
            });
        }
    }

    verdict
}

/// The smallest row count that is at least `pct` percent of `baseline`
/// (a ceiling division, so e.g. 80% of 6 is 5, never 4 -- 4 is only
/// 66.7%).
const fn required_count(baseline: usize, pct: usize) -> usize {
    baseline.saturating_mul(pct).div_ceil(100)
}

/// [`ShrinkCounts`] derived from a candidate's admitted selectors PLUS
/// the selectors it skipped for an EXPECTED cross-check disagreement
/// (`SkipKind::CrossCheckDisagreement`) -- those models have not
/// vanished, their two sources merely disagreed under the empty
/// allowlist, so counting them as present stops one legitimate
/// disagreement in a small family from tripping the shrink guard. Every
/// other skip kind stays uncounted (fail-safe: a genuinely-vanished
/// selector still trips the guard). Never counts the overlay -- see
/// [`shrink_guard`]'s doc.
#[must_use]
pub fn candidate_shrink_counts(candidate: &ImportCandidate) -> ShrinkCounts {
    let admitted = candidate.cells.keys().map(String::as_str);
    let disagreement_skips = candidate
        .skipped
        .iter()
        .filter(|skip| skip.kind == SkipKind::CrossCheckDisagreement)
        .map(|skip| skip.selector.as_str());
    shrink_counts_from_selectors(admitted.chain(disagreement_skips))
}

/// [`ShrinkCounts`] derived from the compiled-in baked table -- the
/// first-run baseline fallback when no `catalog_import_state.json`
/// exists yet (see `crate::catalog_import_state::load_baseline`).
#[must_use]
pub fn baked_shrink_counts() -> ShrinkCounts {
    // Dedupe by selector key: `baked_table_rows()` carries TWO rows per
    // tiered Anthropic/Bedrock selector (5m and 1h), but a candidate
    // (and this baseline it is compared against) is keyed one-per-
    // selector regardless of tier -- counting both tier rows would
    // double-count tiered families and falsely trip the shrink guard
    // on the very first import.
    let keys: BTreeSet<String> = baked_table_rows()
        .into_iter()
        .map(|entry| selector_key(entry.provider_kind, entry.model_glob))
        .collect();
    shrink_counts_from_selectors(keys.iter().map(String::as_str))
}

/// Partition `selectors` into per-source and per-family counts.
fn shrink_counts_from_selectors<'a>(selectors: impl Iterator<Item = &'a str>) -> ShrinkCounts {
    let family_lookup = family_table();
    let mut per_source = BTreeMap::new();
    let mut per_family = BTreeMap::new();
    for selector in selectors {
        let provider_kind = selector.split_once(':').map_or(selector, |(kind, _)| kind);
        *per_source.entry(provider_kind.to_string()).or_insert(0) += 1;
        let family = family_lookup
            .get(selector)
            .cloned()
            .unwrap_or_else(|| provider_kind.to_string());
        *per_family.entry(family).or_insert(0) += 1;
    }
    ShrinkCounts {
        per_source,
        per_family,
    }
}

/// The FAMILY name for every selector the baked table carries: the
/// finest per-vendor grouping the shrink guard partitions rows by.
/// Single-vendor provider kinds (`anthropic-api`, `bedrock`,
/// `openai-responses`) use the provider kind itself; `openai-compat` --
/// an umbrella spanning many independent vendors -- uses the vendor
/// name (`AutoCacherSelector::models_dev_provider`) instead, so one
/// vendor's snapshot truncating cannot hide behind the other vendors'
/// healthy rows in an `openai-compat`-wide aggregate. A selector absent
/// from this table (should never happen -- it mirrors the same static
/// tables [`known_selector_keys`] and [`derive_cells`] already
/// enumerate) falls back to its own provider kind as its family.
fn family_table() -> BTreeMap<String, String> {
    let mut table = BTreeMap::new();
    for sel in ANTHROPIC_SELECTORS {
        table.insert(
            selector_key("anthropic-api", sel.model_glob),
            "anthropic-api".to_string(),
        );
    }
    for sel in BEDROCK_SELECTORS {
        table.insert(
            selector_key("bedrock", sel.model_glob),
            "bedrock".to_string(),
        );
    }
    for sel in OPENAI_RESPONSES_SELECTORS {
        table.insert(
            selector_key("openai-responses", sel.model_glob),
            "openai-responses".to_string(),
        );
    }
    for sel in OPENAI_COMPAT_SELECTORS {
        table.insert(
            selector_key("openai-compat", sel.model_glob),
            sel.models_dev_provider.to_string(),
        );
    }
    for catch_all in CATCH_ALL_ROWS {
        table.insert(
            selector_key(catch_all.provider_kind, "*"),
            catch_all.provider_kind.to_string(),
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cell(provider_kind: &'static str, model_glob: &'static str) -> GeneratedCell {
        GeneratedCell {
            provider_kind,
            model_glob,
            wm: 1.0,
            rm: 0.1,
            ttl_seconds: 300,
            min_prefix_tokens: 1024,
            auto_cacher: false,
            tier: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: Vec::new(),
        }
    }

    /// One tiered Anthropic selector plus one auto-cacher (OpenAI
    /// Responses) selector, with every source field both selectors need
    /// agreeing exactly between the two `Value`s -- mirrors
    /// `catalog_codegen`'s own drive-`derive_cells` fixture, since this
    /// module's group-and-agree rule pairs with that same 5m/1h split.
    fn tiered_and_auto_cacher_fixture() -> (Value, Value) {
        let tiered = &ANTHROPIC_SELECTORS[0];
        let auto_cacher = &OPENAI_RESPONSES_SELECTORS[0];

        let litellm = serde_json::json!({
            tiered.litellm_key: {
                "input_cost_per_token": 1.0e-5,
                "output_cost_per_token": 5.0e-5,
                "cache_read_input_token_cost": 1.0e-6,
                "cache_creation_input_token_cost": 1.25e-5,
                "cache_creation_input_token_cost_above_1hr": 2.0e-5,
                "max_input_tokens": 200_000.0,
            },
            auto_cacher.litellm_key: {
                "input_cost_per_token": 2.0e-6,
                "output_cost_per_token": 8.0e-6,
                "cache_read_input_token_cost": 2.0e-7,
                "max_input_tokens": 400_000.0,
            },
        });
        let models_dev = serde_json::json!({
            "anthropic": {
                "models": {
                    tiered.models_dev_model: {
                        // models.dev prices are per MILLION tokens: the same
                        // rates the litellm half of this fixture states
                        // per-token.
                        "cost": {
                            "input": 10.0,
                            "output": 50.0,
                            "cache_read": 1.0,
                            "cache_write": 12.5,
                        },
                        "limit": {"context": 200_000},
                    },
                },
            },
            auto_cacher.models_dev_provider: {
                "models": {
                    auto_cacher.models_dev_model: {
                        "cost": {"input": 2.0, "output": 8.0, "cache_read": 0.2},
                        "limit": {"context": 400_000},
                    },
                },
            },
        });
        (litellm, models_dev)
    }

    // -----------------------------------------------------------------------
    // GROUP-AND-AGREE: the load-bearing correctness rule.
    // -----------------------------------------------------------------------

    #[test]
    fn tiered_family_imports_rm_and_context_but_omits_wm_and_ttl() {
        // Arrange
        let (litellm, models_dev) = tiered_and_auto_cacher_fixture();
        let tiered = &ANTHROPIC_SELECTORS[0];

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        );

        // Assert
        let key = selector_key("anthropic-api", tiered.model_glob);
        let cell = candidate
            .cells
            .get(&key)
            .expect("tiered selector must import");
        assert!(
            cell.wm.is_none(),
            "wm disagrees between the 5m and 1h rows, must be omitted"
        );
        assert!(
            cell.ttl_seconds.is_none(),
            "ttl_seconds disagrees between the 5m and 1h rows, must be omitted"
        );
        assert!(cell.rm.is_some(), "rm agrees across tiers, must import");
        assert!(
            cell.max_context_tokens.is_some(),
            "max_context_tokens agrees across tiers, must import"
        );
        assert!(
            cell.min_prefix_tokens.is_some(),
            "min_prefix_tokens agrees across tiers, must import"
        );
        assert_eq!(
            cell.input_cost_per_token,
            Some(1.0e-5),
            "base rates are derived once per selector, so they agree across tiers and import"
        );
        assert_eq!(cell.output_cost_per_token, Some(5.0e-5));
    }

    #[test]
    fn auto_cacher_family_imports_every_supported_field() {
        // Arrange
        let (litellm, models_dev) = tiered_and_auto_cacher_fixture();
        let auto_cacher = &OPENAI_RESPONSES_SELECTORS[0];

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        );

        // Assert
        let key = selector_key("openai-responses", auto_cacher.model_glob);
        let cell = candidate
            .cells
            .get(&key)
            .expect("auto-cacher selector must import");
        assert!(cell.wm.is_some());
        assert!(cell.rm.is_some());
        assert!(cell.ttl_seconds.is_some());
        assert!(cell.min_prefix_tokens.is_some());
        assert!(cell.max_context_tokens.is_some());
    }

    // -----------------------------------------------------------------------
    // Provenance: every candidate cell carries source:import + the passed
    // verified_at.
    // -----------------------------------------------------------------------

    #[test]
    fn every_candidate_cell_is_stamped_with_import_source_and_the_passed_verified_at() {
        // Arrange
        let (litellm, models_dev) = tiered_and_auto_cacher_fixture();

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-03-03",
        );

        // Assert
        assert_eq!(candidate.verified_at, "2026-03-03");
        assert!(!candidate.cells.is_empty());
        for cell in candidate.cells.values() {
            assert_eq!(cell.source, OverlaySource::Import);
            assert_eq!(cell.verified_at, "2026-03-03");
        }
    }

    // -----------------------------------------------------------------------
    // Per-selector cross-check disagreement: skipped, not a whole-run abort.
    // -----------------------------------------------------------------------

    #[test]
    fn per_selector_cross_check_disagreement_is_skipped_without_aborting_the_rest() {
        // Arrange: perturb the tiered selector's max_input_tokens so it
        // disagrees with models.dev's context limit; the auto-cacher
        // fixture data stays healthy.
        let (mut litellm, models_dev) = tiered_and_auto_cacher_fixture();
        let tiered = &ANTHROPIC_SELECTORS[0];
        litellm[tiered.litellm_key]["max_input_tokens"] = serde_json::json!(999_999.0);

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        );

        // Assert: the disagreeing selector is skipped with a reason,
        // never written.
        let tiered_key = selector_key("anthropic-api", tiered.model_glob);
        assert!(!candidate.cells.contains_key(&tiered_key));
        let (_, reason) = candidate
            .skipped
            .iter()
            .find(|skip| skip.selector == tiered_key)
            .map(|skip| (&skip.selector, &skip.reason))
            .expect("disagreeing selector must be partitioned as a skip");
        assert!(reason.contains("cross-check mismatch"), "reason: {reason}");

        // The rest of the candidate still builds.
        let auto_cacher = &OPENAI_RESPONSES_SELECTORS[0];
        let auto_key = selector_key("openai-responses", auto_cacher.model_glob);
        assert!(
            candidate.cells.contains_key(&auto_key),
            "a healthy selector must still build despite another selector's skip"
        );
    }

    // -----------------------------------------------------------------------
    // Empty allowlist: the import path cross-checks strictly, with no
    // allowlist bypass available (unlike codegen's vendored allowlist).
    // -----------------------------------------------------------------------

    #[test]
    fn import_cross_checks_strictly_with_no_allowlist_even_for_a_real_price_mismatch() {
        // Arrange: rm disagrees between the two sources for the tiered
        // selector. Codegen could resolve this via
        // catalog_data/cross_check_allowlist.json; the import path never
        // consults an allowlist.
        let (mut litellm, models_dev) = tiered_and_auto_cacher_fixture();
        let tiered = &ANTHROPIC_SELECTORS[0];
        litellm[tiered.litellm_key]["cache_read_input_token_cost"] = serde_json::json!(5.0e-6);

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        );

        // Assert
        let tiered_key = selector_key("anthropic-api", tiered.model_glob);
        assert!(!candidate.cells.contains_key(&tiered_key));
        assert!(
            candidate
                .skipped
                .iter()
                .any(|skip| skip.selector == tiered_key)
        );
    }

    // -----------------------------------------------------------------------
    // Admission: only baked-known selectors are accepted.
    // -----------------------------------------------------------------------

    #[test]
    fn admit_group_rejects_a_selector_not_present_in_the_baked_table() {
        // Arrange
        let group = vec![fake_cell("made-up-provider", "made-up-glob*")];
        let known = BTreeSet::new();

        // Act
        let result = admit_group(
            "made-up-provider:made-up-glob*",
            &group,
            &known,
            "2026-07-11",
        );

        // Assert
        let skip = result.expect_err("unknown selector must be rejected");
        assert_eq!(skip.selector, "made-up-provider:made-up-glob*");
        assert!(
            skip.reason.contains("not present"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_accepts_a_selector_present_in_the_baked_table() {
        // Arrange
        let group = vec![fake_cell("anthropic-api", "claude-opus-4-8*")];
        let known = BTreeSet::from(["anthropic-api:claude-opus-4-8*".to_string()]);

        // Act
        let (selector, cell) = admit_group(
            "anthropic-api:claude-opus-4-8*",
            &group,
            &known,
            "2026-07-11",
        )
        .expect("known selector must admit");

        // Assert
        assert_eq!(selector, "anthropic-api:claude-opus-4-8*");
        assert_eq!(cell.wm, Some(1.0));
    }

    // -----------------------------------------------------------------------
    // No cells, no candidate: never carries auto_cacher / storage_rent
    // (OverlayCell has no such fields -- compile-time guaranteed).
    // -----------------------------------------------------------------------

    #[test]
    fn group_and_agree_builds_only_overlay_cell_fields() {
        // Arrange
        let group = vec![fake_cell("anthropic-api", "claude-opus-4-8*")];

        // Act
        let cell = group_and_agree(&group, "2026-07-11");

        // Assert: constructing `OverlayCell` here without `..Default`
        // already forces this to name every field the type has -- no
        // `auto_cacher` / `storage_rent` field exists to name.
        assert_eq!(cell.source, OverlaySource::Import);
        assert_eq!(cell.wm, Some(1.0));
        assert_eq!(cell.rm, Some(0.1));
    }

    // -----------------------------------------------------------------------
    // Derived-value validation: a degenerate wm/rm/max_context_tokens/
    // ttl_seconds is skipped, never admitted into the candidate.
    // -----------------------------------------------------------------------

    fn cell_with_wm(wm: f32) -> GeneratedCell {
        GeneratedCell {
            wm,
            ..fake_cell("anthropic-api", "claude-opus-4-8*")
        }
    }

    const KNOWN_OPUS: &str = "anthropic-api:claude-opus-4-8*";

    fn known_opus() -> BTreeSet<String> {
        BTreeSet::from([KNOWN_OPUS.to_string()])
    }

    #[test]
    fn admit_group_rejects_a_nan_wm() {
        let group = vec![cell_with_wm(f32::NAN)];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a non-finite wm must be rejected");

        assert!(
            skip.reason.contains("invalid derived value: wm="),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_rejects_a_negative_wm() {
        let group = vec![cell_with_wm(-1.0)];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a negative wm must be rejected");

        assert!(
            skip.reason.contains("invalid derived value: wm=-1"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_rejects_a_zero_wm() {
        let group = vec![cell_with_wm(0.0)];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a zero wm must be rejected");

        assert!(
            skip.reason.contains("invalid derived value: wm=0"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_rejects_a_non_positive_rm() {
        let group = vec![GeneratedCell {
            rm: 0.0,
            ..fake_cell("anthropic-api", "claude-opus-4-8*")
        }];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a zero rm must be rejected");

        assert!(
            skip.reason.contains("invalid derived value: rm=0"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_rejects_a_zero_max_context_tokens() {
        let group = vec![GeneratedCell {
            max_context_tokens: Some(0),
            ..fake_cell("anthropic-api", "claude-opus-4-8*")
        }];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a zero max_context_tokens must be rejected");

        assert!(
            skip.reason
                .contains("invalid derived value: max_context_tokens=0"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn admit_group_rejects_a_zero_ttl_seconds() {
        let group = vec![GeneratedCell {
            ttl_seconds: 0,
            ..fake_cell("anthropic-api", "claude-opus-4-8*")
        }];

        let skip = admit_group(KNOWN_OPUS, &group, &known_opus(), "2026-07-11")
            .expect_err("a zero ttl_seconds must be rejected");

        assert!(
            skip.reason.contains("invalid derived value: ttl_seconds=0"),
            "reason: {}",
            skip.reason
        );
    }

    #[test]
    fn build_import_candidate_skips_a_selector_whose_derived_rm_is_negative_and_never_admits_it() {
        // Arrange: both sources agree on a negative cache_read price for
        // the auto-cacher selector, so the cross-check passes and the new
        // value-validation pass is the only thing that can catch it.
        let (mut litellm, mut models_dev) = tiered_and_auto_cacher_fixture();
        let auto_cacher = &OPENAI_RESPONSES_SELECTORS[0];
        litellm[auto_cacher.litellm_key]["cache_read_input_token_cost"] =
            serde_json::json!(-2.0e-7);
        // Same rate in models.dev's per-million unit, so the two sources
        // genuinely agree and the cross-check has nothing to flag.
        models_dev[auto_cacher.models_dev_provider]["models"][auto_cacher.models_dev_model]["cost"]
            ["cache_read"] = serde_json::json!(-0.2);

        // Act
        let candidate = build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        );

        // Assert
        let key = selector_key("openai-responses", auto_cacher.model_glob);
        assert!(!candidate.cells.contains_key(&key));
        let skip = candidate
            .skipped
            .iter()
            .find(|skip| skip.selector == key)
            .expect("the negative-rm selector must be partitioned as a skip");
        assert!(
            skip.reason.contains("invalid derived value: rm="),
            "reason: {}",
            skip.reason
        );
    }

    // -----------------------------------------------------------------------
    // diff_overlay: user cells (and explicit disables) are never
    // overwritten; import cells are freely overwritten.
    // -----------------------------------------------------------------------

    fn opus_selector() -> String {
        selector_key("anthropic-api", "claude-opus-4-8*")
    }

    fn candidate_cell(wm: f32, rm: f32) -> OverlayCell {
        OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-07-11".to_string(),
            wm: Some(wm),
            rm: Some(rm),
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }
    }

    fn one_cell_candidate(selector: &str, cell: OverlayCell) -> ImportCandidate {
        let mut cells = BTreeMap::new();
        cells.insert(selector.to_string(), cell);
        ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-07-11".to_string(),
            cells,
            skipped: Vec::new(),
        }
    }

    fn overlay_with_cell(selector: &str, cell: Option<OverlayCell>) -> CatalogOverlay {
        let mut cells = BTreeMap::new();
        cells.insert(selector.to_string(), cell);
        CatalogOverlay {
            schema_version: 1,
            revision: 1,
            cells,
        }
    }

    #[test]
    fn diff_overlay_user_cell_is_a_conflict_never_applied() {
        // Arrange: the candidate wants wm = 1.0; the existing user cell
        // says wm = 1.5.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let overlay = overlay_with_cell(
            &selector,
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: Some(1.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert: conflicted, never applied; the user's value is
        // preserved untouched in the row's `existing` field.
        assert!(diff.applied.is_empty());
        assert_eq!(diff.conflicted.len(), 1);
        let row = &diff.conflicted[0];
        assert_eq!(row.selector, selector);
        match &row.existing {
            ExistingCell::Present(cell) => assert_eq!(cell.wm, Some(1.5)),
            other => panic!("expected the existing user cell preserved, got {other:?}"),
        }
    }

    #[test]
    fn diff_overlay_import_cell_is_freely_overwritten() {
        // Arrange: the existing cell is itself `source: import`.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let overlay = overlay_with_cell(
            &selector,
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-01-01".to_string(),
                wm: Some(1.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert: applied, never conflicted; the candidate's own value wins.
        assert!(diff.conflicted.is_empty());
        assert_eq!(diff.applied.len(), 1);
        assert_eq!(diff.applied[0].candidate.wm, Some(1.0));
    }

    #[test]
    fn diff_overlay_disabled_selector_is_a_conflict_with_routing_affecting_impact() {
        // Arrange: the operator explicitly disabled this selector
        // (JSON null); import must not silently re-enable it.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let overlay = overlay_with_cell(&selector, None);
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert!(diff.applied.is_empty());
        assert_eq!(diff.conflicted.len(), 1);
        assert_eq!(diff.conflicted[0].existing, ExistingCell::Disabled);
        assert_eq!(diff.conflicted[0].impact, ImpactClass::RoutingAffecting);
    }

    #[test]
    fn diff_overlay_fresh_apply_against_baked_flags_cost_affecting_and_wm_down_as_cheaper() {
        // Arrange: a fresh apply into an absent overlay key, candidate
        // wm lower than the baked wm.
        let selector = opus_selector();
        let baked = baked_row_map();
        let baked_wm = baked.get(&selector).expect("known selector").wm;
        let candidate = one_cell_candidate(&selector, candidate_cell(baked_wm - 0.25, 0.10));
        let overlay = CatalogOverlay::default();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert_eq!(diff.applied.len(), 1);
        let row = &diff.applied[0];
        assert_eq!(row.impact, ImpactClass::CostAffecting);
        assert!(row.cheaper_direction, "a lower wm trends cheaper to break");
        assert_eq!(row.existing, ExistingCell::Absent);
    }

    #[test]
    fn diff_overlay_fresh_apply_flags_rm_up_as_cheaper_direction() {
        // Arrange: candidate rm higher than the baked rm -- a smaller
        // future discount forfeited by keeping the cache, so breaking
        // becomes relatively cheaper (see `crate::cost_gate::break_even_k`).
        let selector = opus_selector();
        let baked = baked_row_map();
        let baked_row = baked.get(&selector).expect("known selector");
        let candidate =
            one_cell_candidate(&selector, candidate_cell(baked_row.wm, baked_row.rm + 0.05));
        let overlay = CatalogOverlay::default();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert_eq!(diff.applied.len(), 1);
        assert!(
            diff.applied[0].cheaper_direction,
            "a higher rm trends cheaper to break"
        );
    }

    #[test]
    fn diff_overlay_falls_back_to_display_only_when_no_value_differs_from_baked() {
        // Arrange: the candidate's derived numbers happen to coincide
        // exactly with the baked row -- only `verified_at` moved.
        let selector = opus_selector();
        let baked = baked_row_map();
        let baked_row = baked.get(&selector).expect("known selector");
        let candidate = one_cell_candidate(&selector, candidate_cell(baked_row.wm, baked_row.rm));
        let overlay = CatalogOverlay::default();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert_eq!(diff.applied.len(), 1);
        assert_eq!(diff.applied[0].impact, ImpactClass::DisplayOnly);
        assert!(!diff.applied[0].cheaper_direction);
    }

    #[test]
    fn diff_overlay_carries_the_candidates_own_skipped_list_verbatim() {
        // Arrange
        let mut candidate = one_cell_candidate(&opus_selector(), candidate_cell(1.0, 0.10));
        candidate.skipped.push(SkippedSelector {
            selector: "bedrock:*".to_string(),
            reason: "cross-check mismatch".to_string(),
            kind: SkipKind::CrossCheckDisagreement,
        });
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&CatalogOverlay::default(), &candidate, &baked);

        // Assert
        assert_eq!(diff.skipped, candidate.skipped);
    }

    // -----------------------------------------------------------------------
    // Stale-import clearing: a cross-check-skipped selector's prior
    // `source: import` cell is removed so the vetted baked row wins again,
    // while a `source: user` cell at the same position survives.
    // -----------------------------------------------------------------------

    fn grok_selector() -> String {
        selector_key("openai-compat", "grok-*")
    }

    /// A candidate that admits nothing and skips `selector` for a
    /// cross-check disagreement -- exactly what a refresh produces when
    /// the two freshly-fetched sources disagree under the empty allowlist.
    fn cross_check_skip_candidate(selector: &str) -> ImportCandidate {
        ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-08-04".to_string(),
            cells: BTreeMap::new(),
            skipped: vec![SkippedSelector {
                selector: selector.to_string(),
                reason: "cross-check mismatch".to_string(),
                kind: SkipKind::CrossCheckDisagreement,
            }],
        }
    }

    /// An overlay cell an OLD snapshot pair would have imported for Grok:
    /// `rm = 0.25`, which the baked catalog now corrects to `0.15`.
    fn stale_grok_import_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-01-01".to_string(),
            wm: Some(1.0),
            rm: Some(0.25),
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }
    }

    #[test]
    fn diff_overlay_clears_a_stale_import_cell_when_the_selector_is_cross_check_skipped() {
        // Arrange: an operator carried an OLD snapshot's Grok import cell
        // (rm = 0.25) forward; the refresh skips Grok on a cross-check
        // disagreement, so no candidate cell overwrites it. The baked row
        // now says rm = 0.15.
        let selector = grok_selector();
        let baked = baked_row_map();
        assert_eq!(
            baked.get(&selector).expect("grok is baked-known").rm,
            0.15_f32,
            "fixture guard: this test asserts the baked Grok rm is 0.15",
        );
        let overlay = overlay_with_cell(&selector, Some(stale_grok_import_cell()));
        let candidate = cross_check_skip_candidate(&selector);

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert: the stale import cell is scheduled for removal, so the
        // baked 0.15 becomes authoritative again; the skip is a real
        // effective change, not a no-op.
        assert_eq!(diff.cleared, vec![selector.clone()]);
        assert!(diff.applied.is_empty());
        assert!(diff.conflicted.is_empty());
        assert!(!diff_has_no_effective_change(&diff));
    }

    #[test]
    fn diff_overlay_never_clears_a_user_cell_at_a_cross_check_skipped_selector() {
        // Arrange: same skip, but the Grok cell is a `source: user`
        // override -- an operator override always wins over both import
        // and baked, so it must survive the clear.
        let selector = grok_selector();
        let baked = baked_row_map();
        let user_cell = OverlayCell {
            source: OverlaySource::User,
            ..stale_grok_import_cell()
        };
        let overlay = overlay_with_cell(&selector, Some(user_cell.clone()));
        let candidate = cross_check_skip_candidate(&selector);

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert: nothing is cleared; the user override is untouched.
        assert!(diff.cleared.is_empty());
        assert!(diff.applied.is_empty());
        assert!(diff.conflicted.is_empty());
        assert!(!is_import_cell(&overlay, &selector));
        assert_eq!(overlay.cells.get(&selector), Some(&Some(user_cell)));
    }

    #[test]
    fn diff_overlay_only_clears_cross_check_skips_never_the_other_skip_kinds() {
        // Arrange: the same stale Grok import cell, but each skip kind
        // OTHER than a cross-check disagreement -- none of them proves a
        // vetted baked row is available to fall back to.
        let selector = grok_selector();
        let baked = baked_row_map();
        let overlay = overlay_with_cell(&selector, Some(stale_grok_import_cell()));

        for kind in [
            SkipKind::UnknownSelector,
            SkipKind::DegenerateValue,
            SkipKind::Other,
        ] {
            // Act
            let mut candidate = cross_check_skip_candidate(&selector);
            candidate.skipped[0].kind = kind;
            let diff = diff_overlay(&overlay, &candidate, &baked);

            // Assert
            assert!(diff.cleared.is_empty(), "must not clear on {kind:?}");
        }
    }

    #[test]
    fn diff_overlay_does_not_clear_an_absent_or_disabled_cross_check_skipped_selector() {
        // Arrange: no cell (absent) and an explicit disable both have no
        // stale import cell to clear.
        let selector = grok_selector();
        let baked = baked_row_map();
        let candidate = cross_check_skip_candidate(&selector);

        // Act + Assert: absent overlay.
        let absent = diff_overlay(&CatalogOverlay::default(), &candidate, &baked);
        assert!(absent.cleared.is_empty());

        // Act + Assert: explicitly disabled (JSON null) overlay.
        let disabled = overlay_with_cell(&selector, None);
        let disabled_diff = diff_overlay(&disabled, &candidate, &baked);
        assert!(disabled_diff.cleared.is_empty());
    }

    // -----------------------------------------------------------------------
    // diff_has_no_effective_change: the byte-identical re-import guard.
    // -----------------------------------------------------------------------

    #[test]
    fn diff_has_no_effective_change_is_vacuously_true_for_an_empty_applied_set() {
        let diff = ImportDiff::default();
        assert!(diff_has_no_effective_change(&diff));
    }

    #[test]
    fn diff_has_no_effective_change_true_when_the_existing_import_cell_matches_exactly() {
        // Arrange: the candidate re-derives the exact same cell (including
        // verified_at) already sitting in the overlay -- a same-day,
        // byte-identical re-import.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let overlay = overlay_with_cell(&selector, Some(candidate_cell(1.0, 0.10)));
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert_eq!(diff.applied.len(), 1, "still classified as applied");
        assert!(diff_has_no_effective_change(&diff));
    }

    #[test]
    fn diff_has_no_effective_change_false_when_a_value_field_actually_differs() {
        // Arrange: same selector, but the candidate's rm differs from the
        // existing import cell's rm.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let overlay = overlay_with_cell(&selector, Some(candidate_cell(1.0, 0.20)));
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert!(!diff_has_no_effective_change(&diff));
    }

    #[test]
    fn diff_has_no_effective_change_false_when_only_verified_at_moved() {
        // Arrange: every value field agrees, but the existing cell's
        // verified_at is an earlier date than the candidate's -- a
        // re-import on a later day must still count as a real change.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let mut stale = candidate_cell(1.0, 0.10);
        stale.verified_at = "2020-01-01".to_string();
        let overlay = overlay_with_cell(&selector, Some(stale));
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert
        assert!(!diff_has_no_effective_change(&diff));
    }

    #[test]
    fn diff_has_no_effective_change_false_for_a_fresh_absent_selector() {
        // Arrange: a fresh apply into an absent overlay key is never a
        // no-op, even though it is the only row in `applied`.
        let selector = opus_selector();
        let candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        let baked = baked_row_map();

        // Act
        let diff = diff_overlay(&CatalogOverlay::default(), &candidate, &baked);

        // Assert
        assert_eq!(diff.applied[0].existing, ExistingCell::Absent);
        assert!(!diff_has_no_effective_change(&diff));
    }

    // -----------------------------------------------------------------------
    // Shared classifier parity: `diff_overlay` and the drift log
    // (`crate::catalog_state`, see
    // `classify_field_wm_is_cost_affecting_matching_the_import_diffs_own_label`
    // there) call the exact same `classify_field` for a `wm` change.
    // -----------------------------------------------------------------------

    #[test]
    fn diff_overlay_wm_change_matches_the_drift_logs_shared_impact_class() {
        let selector = opus_selector();
        let baked = baked_row_map();
        let baked_wm = baked.get(&selector).expect("known selector").wm;
        let candidate = one_cell_candidate(&selector, candidate_cell(baked_wm - 0.25, 0.10));

        let diff = diff_overlay(&CatalogOverlay::default(), &candidate, &baked);

        assert_eq!(diff.applied[0].impact, ImpactClass::CostAffecting);
        assert_eq!(diff.applied[0].impact.label(), "cost-affecting");
        assert_eq!(classify_field(ImpactField::Wm), ImpactClass::CostAffecting);
    }

    // -----------------------------------------------------------------------
    // shrink_guard: per-source floor, zero-drop, small-family exact
    // preservation, large-family 80% (with correct ceiling rounding).
    // -----------------------------------------------------------------------

    fn counts(source: (&str, usize), family: (&str, usize)) -> ShrinkCounts {
        let mut per_source = BTreeMap::new();
        per_source.insert(source.0.to_string(), source.1);
        let mut per_family = BTreeMap::new();
        per_family.insert(family.0.to_string(), family.1);
        ShrinkCounts {
            per_source,
            per_family,
        }
    }

    #[test]
    fn shrink_guard_flags_a_source_below_the_90_percent_floor() {
        let baseline = counts(("openai-compat", 10), ("deepseek", 10));
        let candidate = counts(("openai-compat", 8), ("deepseek", 10)); // 80% < 90%

        let verdict = shrink_guard(&candidate, &baseline);

        assert_eq!(verdict.shrunk_sources.len(), 1);
        assert_eq!(verdict.shrunk_sources[0].source, "openai-compat");
        assert!(verdict.zero_sources.is_empty());
        assert!(verdict.is_shrunk());
    }

    #[test]
    fn shrink_guard_reports_a_previously_contributing_source_dropping_to_zero_separately() {
        let baseline = counts(("bedrock", 4), ("some-family", 4));
        // `bedrock` is entirely absent from the candidate's per-source
        // counts; the healthy family dimension stays untouched.
        let candidate = counts(("anthropic-api", 3), ("some-family", 4));

        let verdict = shrink_guard(&candidate, &baseline);

        assert_eq!(verdict.zero_sources.len(), 1);
        assert_eq!(verdict.zero_sources[0].source, "bedrock");
        assert!(
            verdict.shrunk_sources.is_empty(),
            "a zero-drop is reported once, not doubled into shrunk_sources too"
        );
        assert!(verdict.shrunk_families.is_empty());
    }

    #[test]
    fn shrink_guard_small_family_requires_exact_preservation() {
        let baseline = counts(("anthropic-api", 5), ("anthropic-api", 5));
        let candidate_ok = counts(("anthropic-api", 5), ("anthropic-api", 5));
        let candidate_short = counts(("anthropic-api", 5), ("anthropic-api", 4));

        assert!(!shrink_guard(&candidate_ok, &baseline).is_shrunk());
        let verdict = shrink_guard(&candidate_short, &baseline);
        assert_eq!(verdict.shrunk_families.len(), 1);
        assert_eq!(verdict.shrunk_families[0].required, 5);
    }

    #[test]
    fn shrink_guard_large_family_requires_80_percent_not_less() {
        let baseline = counts(("openai-compat", 10), ("deepseek", 10));
        let candidate_at_floor = counts(("openai-compat", 10), ("deepseek", 8));
        let candidate_below_floor = counts(("openai-compat", 10), ("deepseek", 7));

        assert!(!shrink_guard(&candidate_at_floor, &baseline).is_shrunk());
        let verdict = shrink_guard(&candidate_below_floor, &baseline);
        assert_eq!(verdict.shrunk_families.len(), 1);
        assert_eq!(verdict.shrunk_families[0].required, 8);
    }

    #[test]
    fn shrink_guard_rounds_a_non_exact_80_percent_floor_up_not_down() {
        let baseline = counts(("openai-compat", 6), ("deepseek", 6));
        // 80% of 6 is 4.8; the required floor must round UP to 5, never
        // truncate down to 4 (4/6 is only 66.7%).
        let candidate = counts(("openai-compat", 6), ("deepseek", 4));

        let verdict = shrink_guard(&candidate, &baseline);

        assert_eq!(verdict.shrunk_families[0].required, 5);
    }

    #[test]
    fn shrink_guard_reports_nothing_for_healthy_counts() {
        let baseline = counts(("openai-compat", 10), ("deepseek", 10));
        let candidate = counts(("openai-compat", 10), ("deepseek", 10));

        assert!(!shrink_guard(&candidate, &baseline).is_shrunk());
    }

    #[test]
    fn shrink_guard_ignores_a_source_or_family_absent_from_the_baseline() {
        let baseline = ShrinkCounts::default();
        let candidate = counts(("anthropic-api", 0), ("anthropic-api", 0));

        assert!(!shrink_guard(&candidate, &baseline).is_shrunk());
    }

    // -----------------------------------------------------------------------
    // allow-shrink scope: `shrink_guard` has no bypass parameter -- it
    // only ever reports what shrank. A user-cell conflict or a
    // cross-check skip is computed by the wholly separate `diff_overlay`
    // and is never touched by a shrink verdict.
    // -----------------------------------------------------------------------

    #[test]
    fn shrink_guard_verdict_never_suppresses_diff_overlays_conflicts_or_skips() {
        // Arrange: a diff with both a user-cell conflict and a
        // cross-check skip.
        let selector = opus_selector();
        let mut candidate = one_cell_candidate(&selector, candidate_cell(1.0, 0.10));
        candidate.skipped.push(SkippedSelector {
            selector: "bedrock:*".to_string(),
            reason: "cross-check mismatch".to_string(),
            kind: SkipKind::CrossCheckDisagreement,
        });
        let overlay = overlay_with_cell(
            &selector,
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: Some(9.99),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        let baked = baked_row_map();

        // A shrink verdict that would reject on its own -- an unrelated
        // decision computed by an unrelated function.
        let shrunk_baseline = counts(("anthropic-api", 100), ("anthropic-api", 100));
        let shrunk_candidate = counts(("anthropic-api", 1), ("anthropic-api", 1));
        assert!(shrink_guard(&shrunk_candidate, &shrunk_baseline).is_shrunk());

        // Act
        let diff = diff_overlay(&overlay, &candidate, &baked);

        // Assert: the conflict and the skip are present regardless --
        // `shrink_guard` has no parameter through which it could have
        // suppressed either.
        assert_eq!(diff.conflicted.len(), 1);
        assert_eq!(diff.skipped.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Derivation smoke tests: candidate/baked counts partition sensibly.
    // -----------------------------------------------------------------------

    #[test]
    fn baked_shrink_counts_partitions_every_baked_row_into_a_source_and_a_family() {
        let counts = baked_shrink_counts();
        let total_by_source: usize = counts.per_source.values().sum();
        let total_by_family: usize = counts.per_family.values().sum();
        let deduped_selectors = baked_row_map().len();
        assert_eq!(total_by_source, deduped_selectors);
        assert_eq!(total_by_family, deduped_selectors);
        assert!(counts.per_source.contains_key("anthropic-api"));
        assert!(counts.per_family.contains_key("deepseek"));
    }

    #[test]
    fn build_import_candidate_tags_missing_source_data_skips_as_not_counted() {
        // Arrange: two empty source objects. Every selector that reads
        // source data fails with a missing-key / absent-data `Err` (never
        // a cross-check disagreement).
        let empty = serde_json::json!({});

        // Act
        let candidate =
            build_import_candidate(CandidateOrigin::DocRefresh, &empty, &empty, "2026-07-11");

        // Assert: those skips are NOT tagged as a disagreement, so they
        // stay uncounted (fail-safe); the counted total never picks them up.
        assert!(
            candidate
                .skipped
                .iter()
                .any(|skip| skip.kind == SkipKind::Other),
            "expected at least one missing-data skip tagged Other"
        );
        assert!(
            !candidate
                .skipped
                .iter()
                .any(|skip| skip.kind == SkipKind::CrossCheckDisagreement),
            "no empty-source skip is a genuine cross-check disagreement"
        );
        let counts = candidate_shrink_counts(&candidate);
        let admitted_only =
            shrink_counts_from_selectors(candidate.cells.keys().map(String::as_str));
        assert_eq!(
            counts, admitted_only,
            "not-counted skips must not inflate the shrink totals"
        );
    }

    #[test]
    fn candidate_shrink_counts_matches_the_candidates_own_admitted_selectors() {
        let candidate = one_cell_candidate(&opus_selector(), candidate_cell(1.0, 0.10));
        let counts = candidate_shrink_counts(&candidate);
        assert_eq!(counts.per_source.get("anthropic-api"), Some(&1));
        assert_eq!(counts.per_family.get("anthropic-api"), Some(&1));
    }

    #[test]
    fn small_family_with_a_cross_check_disagreement_skip_still_passes() {
        // Arrange: a small family with 2 admitted selectors + 1 skipped
        // for an EXPECTED cross-check disagreement, against baseline 3.
        let mut candidate = one_cell_candidate(
            &selector_key("anthropic-api", "claude-a*"),
            candidate_cell(1.0, 0.10),
        );
        candidate.cells.insert(
            selector_key("anthropic-api", "claude-b*"),
            candidate_cell(1.0, 0.10),
        );
        candidate.skipped.push(SkippedSelector {
            selector: selector_key("anthropic-api", "claude-c*"),
            reason: "cross-check mismatch at anthropic-api:claude-c*:wm".to_string(),
            kind: SkipKind::CrossCheckDisagreement,
        });
        let candidate_counts = candidate_shrink_counts(&candidate);
        let baseline = counts(("anthropic-api", 3), ("anthropic-api", 3));

        // Act
        let verdict = shrink_guard(&candidate_counts, &baseline);

        // Assert: the disagreement-skipped selector counts as present, so
        // the family total is 3 and the small-family exact rule passes.
        assert_eq!(candidate_counts.per_family.get("anthropic-api"), Some(&3));
        assert!(!verdict.is_shrunk());
    }

    #[test]
    fn small_family_real_shrink_still_trips_the_guard() {
        // Arrange: 1 admitted, NO disagreement skip, baseline 2.
        let candidate = one_cell_candidate(
            &selector_key("anthropic-api", "claude-a*"),
            candidate_cell(1.0, 0.10),
        );
        let candidate_counts = candidate_shrink_counts(&candidate);
        let baseline = counts(("anthropic-api", 2), ("anthropic-api", 2));

        // Act
        let verdict = shrink_guard(&candidate_counts, &baseline);

        // Assert: a genuine shrink (1 < 2) still trips the exact rule.
        assert!(verdict.is_shrunk());
        assert_eq!(verdict.shrunk_families.len(), 1);
        assert_eq!(verdict.shrunk_families[0].required, 2);
    }

    #[test]
    fn a_non_disagreement_skip_never_counts_and_a_vanished_family_trips() {
        // Arrange: every selector in the family gone; the one skip is an
        // UnknownSelector skip, NOT a disagreement, so nothing counts.
        let candidate = ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-07-11".to_string(),
            cells: BTreeMap::new(),
            skipped: vec![SkippedSelector {
                selector: selector_key("anthropic-api", "claude-a*"),
                reason: "selector is not present in the baked catalog table".to_string(),
                kind: SkipKind::UnknownSelector,
            }],
        };
        let candidate_counts = candidate_shrink_counts(&candidate);
        let baseline = counts(("anthropic-api", 3), ("anthropic-api", 3));

        // Act
        let verdict = shrink_guard(&candidate_counts, &baseline);

        // Assert: the non-disagreement skip is uncounted; the vanished
        // family trips the guard (a previously-contributing source hitting
        // zero).
        assert_eq!(candidate_counts.per_family.get("anthropic-api"), None);
        assert!(verdict.is_shrunk());
        assert_eq!(verdict.zero_sources.len(), 1);
    }

    #[test]
    fn baked_row_map_covers_every_baked_selector() {
        let map = baked_row_map();
        // Deduped by selector key -- see `baked_shrink_counts`'s doc for
        // why `baked_table_rows()` itself carries more rows than there
        // are distinct selectors (tiered families report 5m and 1h
        // rows separately).
        assert_eq!(map.len(), known_selector_keys().len());
        assert!(map.contains_key(&opus_selector()));
    }
}
