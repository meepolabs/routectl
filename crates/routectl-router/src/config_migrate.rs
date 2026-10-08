//! Config schema migration: a PURE planning phase plus a caller-owned
//! two-phase commit.
//!
//! [`plan_migration`] computes a [`MigrationPlan`] WITHOUT touching disk:
//! it runs every ladder transform in memory (the v1 `[cache_pricing]` ->
//! catalog-overlay fold, the v2 -> v3 retry-list retirement, the v3 -> v4
//! `seat_selection`-onto-pool relocation, the v4 -> v5 capability-list
//! retirement) and returns the config-text
//! candidate, the overlay candidate, and the removed keys. Every refusal
//! and conflict check is part of planning, so no on-disk mutation can
//! occur before the caller has a validated plan in hand -- a [`Refusal`]
//! or an overlay conflict surfaces from the pure planner, leaving both
//! files byte-untouched.
//!
//! The caller commits the plan in two phases (recoverable, not literally
//! atomic across two files without a journal):
//!   1. Overlay first: fold `[cache_pricing]` into `catalog_overlay.json`
//!      via the revision-checked `crate::catalog_overlay::save`. A
//!      pre-existing overlay key whose value DIFFERS from the candidate is
//!      a conflict caught at plan time -- nothing is written, for ANY key.
//!   2. `config.toml` LAST, as the visible completion marker: a
//!      format-preserving rewrite (via `toml_edit`, so operator comments
//!      survive) that stamps the new `version` and drops the retired keys.
//!
//! A crash between the two phases leaves `config.toml` still reporting the
//! OLD version, so a rerun re-plans from scratch: the overlay fold is
//! idempotent (a candidate whose value already matches what a prior run
//! wrote is a silent no-op, not a conflict -- see [`cell_values_equal`],
//! so the rerun's plan carries no overlay write) and the single config
//! rewrite then stamps the new version. The migration therefore always
//! reruns safely to a consistent result.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use routectl_auth::SecretRef;
use routectl_core::failure_class::{FailureClass, class_guidance_for_status};
use toml_edit::{Array, DocumentMut, Item, Key, Table, TableLike, Value};

use crate::catalog::{CachePricingOverride, CachePricingSelector};
use crate::catalog_overlay::{self, OverlayCell, OverlaySource};
use crate::config::{CURRENT_CONFIG_VERSION, message_safe_key};

/// Seconds in a day, for epoch-day arithmetic off the system clock.
const SECONDS_PER_DAY: i64 = 86_400;

/// A pending catalog-overlay write computed by `plan_v1_overlay`: the
/// merged cell map plus the `base_revision` it was merged against. The
/// caller commits it through the revision-checked `catalog_overlay::save`,
/// which refuses (no write) if the on-disk revision has moved since.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayWrite {
    /// The overlay revision the merge was computed against; `save` writes
    /// `base_revision + 1` only if the on-disk revision still matches.
    pub base_revision: u64,
    /// The full merged cell map to persist.
    pub cells: BTreeMap<String, Option<OverlayCell>>,
}

/// Which on-disk files a [`MigrationPlan`] will touch when committed, and
/// the payloads each write needs. Folding the config text and the overlay
/// write INTO the variants makes the illegal states unrepresentable -- a
/// `ConfigOnly`/`ConfigAndOverlay` plan always carries its config text, and
/// only a `ConfigAndOverlay` plan carries an overlay write -- so the caller
/// never has to `.expect()` a cross-field invariant the type did not
/// enforce.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteKind {
    /// The file is already current with nothing to fold; committing is a
    /// no-op and writes nothing.
    NoChange,
    /// Only `config.toml` changes (a v2 -> v3 rung, or a v3 -> v3
    /// normalization, or a v1 file whose overlay fold is an idempotent
    /// no-op because the cells already match). Carries the fully-migrated
    /// config text to commit.
    ConfigOnly(String),
    /// Both `catalog_overlay.json` and `config.toml` change (a v1 file
    /// whose `[cache_pricing]` fold adds or edits an overlay cell). Carries
    /// the fully-migrated config text and the pending overlay write to
    /// commit FIRST.
    ConfigAndOverlay(String, OverlayWrite),
}

/// The fully-computed result of the PURE planning phase: everything the
/// caller needs to commit the migration, with no on-disk mutation yet
/// performed. Produced by [`plan_migration`].
///
/// A refusal or conflict is reported as an `Err` from [`plan_migration`]
/// instead of a plan, so holding a `MigrationPlan` means every refusal and
/// validation check the migrator owns has already passed.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationPlan {
    /// The raw on-disk version the plan migrates from.
    pub from: u32,
    /// The version the committed `config.toml` will stamp (equal to `from`
    /// when no rung runs).
    pub to: u32,
    /// Which files the commit will touch, and the payloads (config text,
    /// pending overlay write) it will write.
    pub write_kind: WriteKind,
    /// Human-readable descriptions of the keys this migration removes, for
    /// a dry-run change summary. Derived purely from the original document.
    pub removed_keys: Vec<String>,
    /// The provider entries the PURE v3 -> v4 rung renames, as
    /// `(from, to)` pairs in document order: an entry holding its own
    /// provider-family name vacates it for the `[pools.<family>]` block the
    /// knob relocation creates. Derived purely from the original document,
    /// for the same change summary -- a rename the operator is not shown is a
    /// rename they never agreed to.
    pub renamed_entries: Vec<(String, String)>,
    /// The per-rung outcomes, in ladder order.
    pub steps: Vec<StepOutcome>,
}

impl MigrationPlan {
    /// The fully-migrated `config.toml` text to commit, or `None` for a
    /// [`WriteKind::NoChange`] plan. A read-model over [`Self::write_kind`]
    /// -- it cannot represent the illegal "candidate present but nothing to
    /// write" state the old separate field could.
    #[must_use]
    pub fn config_candidate(&self) -> Option<&str> {
        match &self.write_kind {
            WriteKind::NoChange => None,
            WriteKind::ConfigOnly(text) | WriteKind::ConfigAndOverlay(text, _) => Some(text),
        }
    }

    /// The pending overlay write, present only for a
    /// [`WriteKind::ConfigAndOverlay`] plan.
    #[must_use]
    pub const fn overlay_candidate(&self) -> Option<&OverlayWrite> {
        match &self.write_kind {
            WriteKind::ConfigAndOverlay(_, ow) => Some(ow),
            _ => None,
        }
    }
}

/// Errors from the v1 overlay fold in `plan_v1_overlay`. Every variant
/// fails closed: on any error the overlay is left byte-untouched (a partial
/// write across multiple keys never happens -- conflicts are collected up
/// front and the whole write is skipped when any exist).
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// A `[cache_pricing]` selector key does not parse as
    /// `provider_kind:model_glob`.
    #[error("cache-pricing migration: selector `{selector}` is invalid: {reason}")]
    InvalidSelector {
        /// The offending selector key.
        selector: String,
        /// Why the selector failed to parse.
        reason: String,
    },

    /// A `[cache_pricing]` override is degenerate (same checks as
    /// `crate::catalog::validate_overrides`, applied here so a bad override
    /// never gets carried forward into the overlay).
    #[error("cache-pricing migration: override for `{selector}` is invalid: {reason}")]
    InvalidOverride {
        /// The selector whose override is invalid.
        selector: String,
        /// Why the override is rejected.
        reason: String,
    },

    /// One or more candidate selectors already carry a DIFFERENT overlay
    /// value, or the overlay explicitly disables them (JSON `null`).
    /// Migration wrote nothing -- for any key, not just the conflicting
    /// ones -- so a partial migration never lands.
    #[error(
        "cache-pricing migration conflict: {0:?} already carry a different overlay value (or \
         are explicitly disabled there); migration wrote nothing -- resolve by hand (edit \
         catalog_overlay.json or config.toml) before retrying"
    )]
    Conflict(Vec<String>),

    /// Loading the current overlay failed (corrupt, too-new schema, I/O).
    #[error("cache-pricing migration: {0}")]
    Overlay(#[from] catalog_overlay::OverlayError),
}

/// Compute the v1 `[cache_pricing]` -> catalog-overlay fold as a PENDING
/// write, WITHOUT touching disk. Loads the current overlay (a read),
/// validates and merges every `[cache_pricing]` candidate cell, and detects
/// conflicts up front:
///
/// - `Ok(Some(write))`: at least one cell is new or edited; the caller
///   commits `write` through the revision-checked `catalog_overlay::save`.
/// - `Ok(None)`: every candidate already matches an existing overlay cell
///   (an idempotent rerun) -- no overlay write is needed.
/// - `Err(Conflict)`: one or more candidate selectors already carry a
///   DIFFERENT overlay value (or are explicitly disabled). Nothing is
///   written, for ANY key -- fail closed rather than guess which side is
///   right.
///
/// `cache_pricing` is the operator's `[cache_pricing]` table, ALREADY
/// merged with any legacy `pricing_verifications.json` stamps by the caller
/// (see `routectl-cli`'s `commands::catalog::load_and_merge_verifications`).
fn plan_v1_overlay(
    cache_pricing: &BTreeMap<String, CachePricingOverride>,
    overlay_path: &Path,
) -> Result<Option<OverlayWrite>, MigrationError> {
    let candidates = build_candidate_cells(cache_pricing)?;

    let overlay = catalog_overlay::load(overlay_path)?;
    let mut merged_cells = overlay.cells.clone();
    let mut conflicts = Vec::new();
    let mut changed = false;
    for (selector, cell) in &candidates {
        match merged_cells.get(selector) {
            None => {
                merged_cells.insert(selector.clone(), Some(cell.clone()));
                changed = true;
            }
            Some(Some(existing)) if cell_values_equal(existing, cell) => {
                // Already migrated with this exact value -- idempotent
                // no-op (verified_at may differ; ignored by design so a
                // rerun on a later day never manufactures a conflict).
            }
            Some(_) => conflicts.push(selector.clone()),
        }
    }

    if !conflicts.is_empty() {
        return Err(MigrationError::Conflict(conflicts));
    }

    Ok(if changed {
        Some(OverlayWrite {
            base_revision: overlay.revision,
            cells: merged_cells,
        })
    } else {
        None
    })
}

/// Validate and convert every `[cache_pricing]` entry into an overlay
/// candidate cell, all-or-nothing (an invalid selector or override aborts
/// the whole migration before anything is written).
///
/// Provenance is `OverlaySource::User`: this data was operator-authored
/// (a `[cache_pricing]` entry the operator wrote, or a date the operator
/// stamped via `routectl catalog verify`), not a bulk vendor import --
/// `OverlaySource::Import` is reserved for a later bulk-refresh pipeline.
///
/// `has_storage_rent` / `storage_rent` / `auto_cacher` have no field on
/// [`OverlayCell`] (reserved/unused on every baked row today -- see
/// `crate::catalog::CatalogRow`) and are dropped with a warning when an
/// override actually sets one; every other field carries through.
fn build_candidate_cells(
    cache_pricing: &BTreeMap<String, CachePricingOverride>,
) -> Result<BTreeMap<String, OverlayCell>, MigrationError> {
    let today = today_ymd();
    let mut out = BTreeMap::new();
    for (selector, ov) in cache_pricing {
        CachePricingSelector::parse(selector).map_err(|reason| {
            MigrationError::InvalidSelector {
                selector: selector.clone(),
                reason,
            }
        })?;
        ov.validate()
            .map_err(|reason| MigrationError::InvalidOverride {
                selector: selector.clone(),
                reason,
            })?;

        if ov.has_storage_rent.is_some() || ov.storage_rent.is_some() || ov.auto_cacher.is_some() {
            tracing::warn!(
                selector = %routectl_core::sanitize_for_log(selector),
                "cache-pricing migration: has_storage_rent/storage_rent/auto_cacher have no \
                 field on the catalog overlay (reserved/unused on every baked row today) and \
                 were dropped for this selector",
            );
        }

        out.insert(
            selector.clone(),
            OverlayCell {
                source: OverlaySource::User,
                verified_at: ov.verified_at.clone().unwrap_or_else(|| today.clone()),
                wm: ov.wm,
                rm: ov.rm,
                ttl_seconds: ov.ttl_seconds,
                min_prefix_tokens: ov.min_prefix_tokens,
                max_context_tokens: ov.max_context_tokens,
                max_output_tokens: None,
                input_cost_per_token: ov.input_cost_per_token,
                output_cost_per_token: ov.output_cost_per_token,
                capabilities: None,
            },
        );
    }
    Ok(out)
}

/// Compare the VALUE fields of two overlay cells, ignoring `source` and
/// `verified_at`. Used to recognize an idempotent migration rerun: the
/// `verified_at` fallback in [`build_candidate_cells`] stamps "today" for
/// an override with no explicit date, which would otherwise manufacture a
/// spurious conflict on a rerun that lands on a later calendar day.
fn cell_values_equal(a: &OverlayCell, b: &OverlayCell) -> bool {
    a.wm == b.wm
        && a.rm == b.rm
        && a.ttl_seconds == b.ttl_seconds
        && a.min_prefix_tokens == b.min_prefix_tokens
        && a.max_context_tokens == b.max_context_tokens
        && a.input_cost_per_token == b.input_cost_per_token
        && a.output_cost_per_token == b.output_cost_per_token
        && a.capabilities == b.capabilities
}

/// Pure v1 -> v2 transform on the RAW toml_edit document: set `version = 2`
/// and drop `[cache_pricing]`. NO IO -- the caller owns the single commit.
/// The `[cache_pricing]` fold into the overlay is [`plan_v1_overlay`]'s
/// separate concern; this touches only the document.
///
/// Stamps the LITERAL target `2`: only the ladder in [`apply_config_transforms`]
/// knows what "current" is, so a later bump of `CURRENT_CONFIG_VERSION`
/// cannot make this v1 -> v2 rung silently over-stamp a version it did not
/// actually migrate to.
fn apply_v1_to_v2_doc(doc: &mut DocumentMut) {
    doc["version"] = toml_edit::value(2i64);
    doc.remove("cache_pricing");
}

/// Today's date as `"YYYY-MM-DD"`, derived from the system clock via pure
/// arithmetic (no date library, mirroring `crate::catalog`'s
/// `today_epoch_day` / `parse_epoch_day` pair -- this is their forward
/// counterpart, days-since-epoch to civil date, so the two modules never
/// need to share a dependency to agree on a calendar).
fn today_ymd() -> String {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        i64::try_from(d.as_secs()).unwrap_or(0) / SECONDS_PER_DAY
    });
    let (y, m, d) = civil_from_epoch_day(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`: proleptic-Gregorian epoch-day count
/// (days since 1970-01-01) to a `(year, month, day)` civil date. Pure
/// integer arithmetic, no allocation, no panic on any `i64` input.
fn civil_from_epoch_day(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// The highest config version the step ladder in [`apply_config_transforms`]
/// knows how to produce. Deliberately a LITERAL, not `CURRENT_CONFIG_VERSION`:
/// the two are kept equal, but the ladder's rungs and its "too new" ceiling
/// must stay pinned to the versions whose transforms actually exist, so a
/// bare bump of the const (task ordering) can never make the ladder claim a
/// version it has no step for -- and an already-latest doc stays a no-op
/// regardless of what the const currently reads.
const LATEST_MIGRATION_VERSION: u32 = 5;

/// The ladder must always be able to reach the current version: every step
/// up to `CURRENT_CONFIG_VERSION` has to exist. Enforced at compile time so
/// a bump of the const without a matching rung is caught by the build, not
/// at runtime.
const _: () = assert!(
    LATEST_MIGRATION_VERSION == CURRENT_CONFIG_VERSION,
    "config migration ladder must end exactly at the current config version",
);

/// What a single migration step accomplished, for the ladder's audit line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    /// The version the step migrated FROM.
    pub from_version: u32,
    /// The version the step stamped (a literal step target, never the
    /// current-version const).
    pub to_version: u32,
}

/// Which retired key(s) carried the behavior that forced a [`Refusal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalSource {
    /// A non-empty `retry_allowlist`.
    Allowlist,
    /// A `retry_denylist` present with content.
    Denylist,
    /// Both keys carried content (already a config-load error, but the
    /// migrator still refuses cleanly rather than half-transforming).
    Both,
}

/// A refusal from a `config migrate` transform: the migrator declines and
/// leaves the document byte-untouched. All causes fail closed:
///
/// - [`Refusal::BehaviorBearing`]: the v2 doc carries behavior-bearing
///   `retry_allowlist` / `retry_denylist` codes that have no lossless fold
///   into the `[retry.classes.*]` class overlay.
/// - [`Refusal::Malformed`]: a `retry_allowlist` / `retry_denylist` entry is
///   not a valid `u16` HTTP status. Silently dropping it would strip the key
///   and change behavior, so the migrator refuses rather than fold.
/// - [`Refusal::EgressAllowlist`]: the v4 -> v5 rung ([`migrate_v4_to_v5`])
///   found a non-empty egress allowlist (`allowed_betas` /
///   `allowed_body_fields`). Version 5 retires the lists, and an allowlist
///   (what may be sent) has no lossless conversion into a denylist (what
///   must not be), so the operator decides per list.
/// - [`Refusal::CapabilityConflict`]: the v4 -> v5 fold would put a
///   capability into `unsupported` on a cell whose `force_supported`
///   already names it.
/// - [`Refusal::OverrideShape`]: the v4 -> v5 fold's destination under
///   `[capability.overrides]` already exists in a shape it cannot extend
///   (a non-table `capability` / `overrides` / cell, or a non-array
///   `unsupported`). Removing the retired key without landing its values
///   would lose them, so the migrator refuses instead.
///
/// Carries the offending content and (where meaningful) rendered guidance so
/// the caller can both log a structured audit event and print hand-edit
/// instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Behavior-bearing retry lists with no lossless class fold.
    BehaviorBearing {
        /// The offending status codes, in the order encountered.
        codes: Vec<u16>,
        /// Which retired key(s) bore the behavior.
        source: RefusalSource,
        /// One rendered guidance line per offending code, naming the nearest
        /// `[retry.classes.<token>]` class(es) and the intended `fallback`.
        guidance: Vec<String>,
    },
    /// A retry-list entry that is not a valid `u16` HTTP status (non-integer,
    /// negative, out of range, or a float).
    Malformed {
        /// Which retired key(s) carried the malformed entry.
        source: RefusalSource,
        /// The malformed entries, rendered as they appear in the file.
        entries: Vec<String>,
    },
    /// Non-empty egress allowlists (`allowed_betas` /
    /// `allowed_body_fields`) the v4 -> v5 rung cannot retire without an
    /// operator decision.
    EgressAllowlist {
        /// Each non-empty list, in deterministic order. Carries the entry
        /// count only, never the listed values.
        allowlists: Vec<PresentAllowlist>,
    },
    /// Folded `unsupported_features` values that a `force_supported` entry
    /// on the same `[capability.overrides]` cell already marks supported.
    CapabilityConflict {
        /// One line per conflicting value, naming the cell, the capability
        /// and the retired key it was folded from.
        cells: Vec<String>,
    },
    /// A `[capability.overrides]` destination the v4 -> v5 fold must extend
    /// already holds a value of the wrong shape.
    OverrideShape {
        /// One line per offending destination, naming its key path and the
        /// shape it must have. Carries no values.
        paths: Vec<String>,
    },
    /// A provider-level `seat_selection` the v3 -> v4 rung cannot relocate
    /// onto a pool block without guessing -- including two entries whose
    /// derived pool names collide.
    SeatSelectionRelocation {
        /// One line per problem, naming the provider entr(ies) and why the
        /// knob has no derivable pool.
        entries: Vec<String>,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BehaviorBearing {
                source, guidance, ..
            } => {
                writeln!(
                    f,
                    "the retired `retry_allowlist` / `retry_denylist` keys carry per-status retry \
                     behavior that cannot be folded losslessly into `[retry.classes.*]`: a bare \
                     HTTP status has no stable failure class (503 is server-error or overloaded; \
                     a 4xx may lift to content-policy / context-window / feature-unsupported on \
                     response-body tokens the migrator never sees), so an automatic fold would \
                     silently change behavior. Nothing was written. Re-express these by hand as \
                     `[retry.classes.<class>]` leaves, then remove the two keys:"
                )?;
                for line in guidance {
                    writeln!(f, "  - {line}")?;
                }
                if matches!(source, RefusalSource::Allowlist | RefusalSource::Both) {
                    writeln!(
                        f,
                        "  note: an allowlist also means every OTHER 4xx/5xx class must NOT fall \
                         back -- set `fallback = false` on the remaining classes by hand."
                    )?;
                }
                Ok(())
            }
            Self::Malformed { source, entries } => {
                let keys = match source {
                    RefusalSource::Allowlist => "`retry_allowlist` key",
                    RefusalSource::Denylist => "`retry_denylist` key",
                    RefusalSource::Both => "`retry_allowlist` / `retry_denylist` keys",
                };
                writeln!(
                    f,
                    "the retired {keys} carry an entry that is not a valid HTTP status code (must \
                     be an integer in 0..=65535): migrating would silently drop it and change \
                     retry behavior, so nothing was written. Fix or remove the malformed \
                     entr{} by hand, then rerun:",
                    if entries.len() == 1 { "y" } else { "ies" },
                )?;
                for entry in entries {
                    writeln!(f, "  - {entry}")?;
                }
                Ok(())
            }
            Self::EgressAllowlist { allowlists } => {
                writeln!(
                    f,
                    "config version 5 retires the egress allowlists (`allowed_betas` / \
                     `allowed_body_fields`), and the lists below are not empty. An allowlist \
                     names what may be sent; it does not convert losslessly into the list of \
                     what must not be, so nothing was written. Decide each list by hand, delete \
                     it, then rerun:"
                )?;
                for allowlist in allowlists {
                    let PresentAllowlist { path, entry_count } = allowlist;
                    let noun = if *entry_count == 1 {
                        "entry"
                    } else {
                        "entries"
                    };
                    writeln!(f, "  - {path} ({entry_count} {noun})")?;
                }
                writeln!(
                    f,
                    "  deleting a beta list accepts pass-through: client betas are forwarded \
                     except the ones the router withholds from the lane (its shipped seed plus \
                     the flags it has learned the lane rejects)."
                )?;
                writeln!(
                    f,
                    "  for a beta that must never be sent, add `unsupported = [\"beta:<flag>\"]` \
                     under `[capability.overrides.<provider>]`."
                )?;
                writeln!(
                    f,
                    "  a body-field list (`allowed_body_fields`) has no successor; delete it."
                )
            }
            Self::CapabilityConflict { cells } => {
                writeln!(
                    f,
                    "folding the retired `unsupported_features` lists into \
                     `[capability.overrides]` would mark a capability unsupported on a cell \
                     whose `force_supported` already marks it supported. Which one is meant \
                     cannot be derived, so nothing was written. Remove the capability from one \
                     of the two lists, then rerun:"
                )?;
                for cell in cells {
                    writeln!(f, "  - {cell}")?;
                }
                Ok(())
            }
            Self::OverrideShape { paths } => {
                writeln!(
                    f,
                    "folding the retired `unsupported_features` lists into \
                     `[capability.overrides]` needs to extend the destinations below, but each \
                     already holds a value of another shape. Dropping the retired lists without \
                     landing their values would lose them, so nothing was written. Give each \
                     destination the shape shown, then rerun:"
                )?;
                for path in paths {
                    writeln!(f, "  - {path}")?;
                }
                Ok(())
            }
            Self::SeatSelectionRelocation { entries } => {
                writeln!(
                    f,
                    "the `seat_selection` knob moves off `[providers.X]` onto the \
                     `[pools.<name>]` block that groups the accounts, but the entries below \
                     carry one that cannot be relocated automatically: the pool name is \
                     derived from the provider family named by the entry's `oauth://` ref, so \
                     an entry with no oauth ref, with refs to more than one family, or whose \
                     derived pool name is already taken has no single answer. Nothing was \
                     written. Add the `[pools.<name>]` block by hand (moving the knob onto it) \
                     and remove the provider-level key, then rerun:"
                )?;
                for entry in entries {
                    writeln!(f, "  - {entry}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Refusal {}

/// One non-empty egress allowlist found by the v4 -> v5 rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentAllowlist {
    /// The fully qualified key, e.g. `bedrock.allowed_betas` or
    /// `providers.<name>.allowed_betas`.
    pub path: String,
    /// How many entries the list carries.
    pub entry_count: usize,
}

/// Errors from the [`plan_migration`] planner.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The v1 overlay fold planning failed (validation or a conflict).
    #[error(transparent)]
    V1ToV2(#[from] MigrationError),

    /// A ladder rung refused a behavior-bearing config; nothing written.
    #[error("config migration refused:\n{0}")]
    Refused(Refusal),

    /// The file's version is newer than any step the ladder can produce.
    #[error(
        "config version {found} is newer than this build can migrate (latest known is {supported}); \
         upgrade routectl"
    )]
    VersionTooNew {
        /// The version stamped in the file.
        found: u32,
        /// The latest version the ladder knows how to produce.
        supported: u32,
    },
}

/// Read a `[retry]` key as a list of `u16` status codes off the RAW doc.
///
/// - `Ok(None)`: the key is absent, or present but not an array (the typed
///   deserialize reports a non-array type error) -- distinguished from
///   present-empty so the caller can tell an absent `retry_denylist` from
///   `[]`.
/// - `Ok(Some(codes))`: every array entry is a valid `u16` HTTP status.
/// - `Err(entries)`: one or more entries are NOT a valid `u16` status
///   (non-integer, negative, out of range, or a float), rendered as they
///   appear in the file. The migrator must NOT silently drop these: doing so
///   would strip the key and change retry behavior -- fail-open. It refuses
///   instead.
fn read_status_list(table: &dyn TableLike, key: &str) -> Result<Option<Vec<u16>>, Vec<String>> {
    let Some(arr) = table.get(key).and_then(toml_edit::Item::as_array) else {
        return Ok(None);
    };
    let mut codes = Vec::new();
    let mut malformed = Vec::new();
    for entry in arr {
        match entry.as_integer().and_then(|v| u16::try_from(v).ok()) {
            Some(code) => codes.push(code),
            None => malformed.push(render_entry(entry)),
        }
    }
    if malformed.is_empty() {
        Ok(Some(codes))
    } else {
        Err(malformed)
    }
}

/// Render a malformed retry-list entry as it appears in the file, for the
/// [`Refusal::Malformed`] message: a bare number for an out-of-range or
/// negative integer or a float, a quoted string for a non-numeric entry.
fn render_entry(value: &toml_edit::Value) -> String {
    if let Some(i) = value.as_integer() {
        i.to_string()
    } else if let Some(f) = value.as_float() {
        f.to_string()
    } else if let Some(s) = value.as_str() {
        format!("\"{s}\"")
    } else if let Some(b) = value.as_bool() {
        b.to_string()
    } else {
        value.to_string().trim().to_string()
    }
}

/// The `[retry.classes.<token>]` spelling for a canonical [`FailureClass`],
/// or `None` for a class with no operator-nameable config token (`Unknown`
/// or a future `#[non_exhaustive]` variant). Delegates to the canonical
/// [`FailureClass::class_token`] so the migrator's guidance shares the one
/// vocabulary source.
fn nearest_class_token(class: &FailureClass) -> Option<String> {
    class.class_token().map(str::to_string)
}

/// Build one guidance line for `code`, naming the nearest class token(s)
/// from the real taxonomy and the `fallback` value the retired list
/// intended for it.
fn guidance_for_code(code: u16, intended_fallback: bool) -> String {
    let g = class_guidance_for_status(code);
    let Some(primary) = nearest_class_token(&g.primary) else {
        return format!("status {code} has no failure class; remove it or classify by hand");
    };
    let alternatives: Vec<String> = g
        .alternatives
        .iter()
        .filter_map(nearest_class_token)
        .collect();
    let alt_note = if alternatives.is_empty() {
        String::new()
    } else {
        format!(" (may also classify as {})", alternatives.join(" / "))
    };
    format!(
        "status {code} -> nearest class `{primary}`{alt_note}; set \
         `[retry.classes.{primary}].fallback = {intended_fallback}`",
    )
}

/// Pure v2 -> v3 transform on the RAW toml_edit document (never a typed
/// pre-migration `Config`). Performs NO `config.toml` IO -- the caller owns
/// the single commit.
///
/// The v2 -> v3 break retires the per-status `retry_allowlist` /
/// `retry_denylist` keys in favour of the `[retry.classes.*]` class
/// overlay. The transform is BINARY:
///
/// - `retry_allowlist` empty AND `retry_denylist` absent-or-empty: the
///   keys carry no behavior, so this stamps LITERAL `version = 3` and
///   removes both keys, preserving comments and key order. Lossless.
/// - Any behavior-bearing list (a non-empty allowlist, or a denylist
///   present with content): [`Refusal`] with NO mutation. A bare status
///   has no provider- and body-independent failure class, so an automatic
///   fold would silently change behavior for a fraction of codes -- the
///   fail-closed line the break forbids.
///
/// Key removal goes through [`TableLike`] so it lands on BOTH the
/// inline-table (`retry = { ... }`) and standard-table (`[retry]`) shapes;
/// a plain `Table` walk would silently no-op on the inline shape.
///
/// # Errors
///
/// Returns [`Refusal::BehaviorBearing`] when the doc carries behavior-bearing
/// retry lists, or [`Refusal::Malformed`] when a retry-list entry is not a
/// valid `u16` HTTP status. Both leave the document byte-untouched.
pub fn migrate_v2_to_v3(doc: &mut DocumentMut) -> Result<StepOutcome, Refusal> {
    let retry = doc.get("retry").and_then(|item| item.as_table_like());
    let allow_read = retry.map_or(Ok(None), |r| read_status_list(r, "retry_allowlist"));
    let deny_read = retry.map_or(Ok(None), |r| read_status_list(r, "retry_denylist"));

    // Fail closed on any malformed entry BEFORE the behavior-bearing check:
    // silently dropping it would strip the key and change retry behavior --
    // exactly the fail-open the v2->v3 break forbids.
    let allow_bad = allow_read.as_ref().err().cloned().unwrap_or_default();
    let deny_bad = deny_read.as_ref().err().cloned().unwrap_or_default();
    if !allow_bad.is_empty() || !deny_bad.is_empty() {
        let source = match (!allow_bad.is_empty(), !deny_bad.is_empty()) {
            (true, true) => RefusalSource::Both,
            (true, false) => RefusalSource::Allowlist,
            (false, true) => RefusalSource::Denylist,
            (false, false) => unreachable!("guarded by the outer condition"),
        };
        let entries = allow_bad.into_iter().chain(deny_bad).collect();
        return Err(Refusal::Malformed { source, entries });
    }

    let allowlist = allow_read.ok().flatten().unwrap_or_default();
    let deny_codes = deny_read.ok().flatten().unwrap_or_default();

    let allow_bearing = !allowlist.is_empty();
    let deny_bearing = !deny_codes.is_empty();

    if allow_bearing || deny_bearing {
        let source = match (allow_bearing, deny_bearing) {
            (true, true) => RefusalSource::Both,
            (true, false) => RefusalSource::Allowlist,
            (false, true) => RefusalSource::Denylist,
            (false, false) => unreachable!("guarded by the outer condition"),
        };
        let mut codes = Vec::new();
        let mut guidance = Vec::new();
        // An allowlist names the ONLY codes that fall back (fallback = true);
        // a denylist names codes that must NOT fall back (fallback = false).
        for &code in &allowlist {
            codes.push(code);
            guidance.push(guidance_for_code(code, true));
        }
        for &code in &deny_codes {
            codes.push(code);
            guidance.push(guidance_for_code(code, false));
        }
        return Err(Refusal::BehaviorBearing {
            codes,
            source,
            guidance,
        });
    }

    if let Some(retry) = doc
        .get_mut("retry")
        .and_then(|item| item.as_table_like_mut())
    {
        retry.remove("retry_allowlist");
        retry.remove("retry_denylist");
    }
    doc["version"] = toml_edit::value(3i64);

    Ok(StepOutcome {
        from_version: 2,
        to_version: 3,
    })
}

/// The retired egress-allowlist keys under the global `[bedrock]` table.
const BEDROCK_ALLOWLIST_KEYS: [&str; 2] = ["allowed_betas", "allowed_body_fields"];

/// The retired egress-allowlist key on a `[providers.<name>]` entry.
const PROVIDER_ALLOWLIST_KEY: &str = "allowed_betas";

/// The retired per-target capability list folded into `[capability.overrides]`.
const UNSUPPORTED_FEATURES_KEY: &str = "unsupported_features";

/// Pure v4 -> v5 transform on the RAW toml_edit document. NO IO -- the caller
/// owns the single commit. Format-preserving: operator comments and unrelated
/// content survive.
///
/// Version 5 retires the per-target capability lists and the egress
/// allowlists:
///
/// - `[providers.<name>] unsupported_features = [...]` folds into
///   `[capability.overrides.<name>] unsupported = [...]`;
/// - `[models.<nick>] unsupported_features = [...]` folds into
///   `[capability.overrides."<provider>:<nick>"] unsupported = [...]`, where
///   `<provider>` is the model's own `provider` field (a model with no
///   `provider` is left for the shared gate to reject);
/// - an EMPTY `allowed_betas` / `allowed_body_fields` list (pass-through, so
///   it carries no behavior) is removed, and `[bedrock]` is removed once
///   nothing is left in it.
///
/// Raw capability tokens carry through verbatim (the override namespace is
/// open and the registry normalizes at build time). A value already on the
/// target's `unsupported` array is not duplicated. Stamps LITERAL
/// `version = 5`.
///
/// # Errors
///
/// Every refusal happens before any mutation, leaving `doc` byte-untouched:
///
/// - [`Refusal::EgressAllowlist`] when any allowlist is non-empty;
/// - [`Refusal::OverrideShape`] when a fold destination under
///   `[capability.overrides]` exists in a shape the fold cannot extend;
/// - [`Refusal::CapabilityConflict`] when a folded value is already in the
///   target cell's `force_supported`.
pub fn migrate_v4_to_v5(doc: &mut DocumentMut) -> Result<StepOutcome, Refusal> {
    let allowlists = present_egress_allowlists(doc);
    if !allowlists.is_empty() {
        return Err(Refusal::EgressAllowlist { allowlists });
    }

    let plan = collect_unsupported_features(doc);
    let paths = override_shape_problems(doc, &plan);
    if !paths.is_empty() {
        return Err(Refusal::OverrideShape { paths });
    }
    let cells = forced_fold_conflicts(doc, &plan);
    if !cells.is_empty() {
        return Err(Refusal::CapabilityConflict { cells });
    }

    fold_unsupported_features(doc, &plan)?;
    drop_empty_egress_allowlists(doc);
    doc["version"] = toml_edit::value(5i64);

    Ok(StepOutcome {
        from_version: 4,
        to_version: 5,
    })
}

/// Apply the folds in `plan` and remove every retired `unsupported_features`
/// key it names.
///
/// # Errors
///
/// [`Refusal::OverrideShape`] when a destination cannot be extended; the
/// retired key is then left in place rather than dropped unfolded.
fn fold_unsupported_features(
    doc: &mut DocumentMut,
    plan: &UnsupportedFeaturesPlan,
) -> Result<(), Refusal> {
    for fold in &plan.folds {
        append_unsupported_override(doc, &fold.spec, &fold.values)?;
    }
    if let Some(providers) = doc.get_mut("providers").and_then(Item::as_table_like_mut) {
        for name in &plan.provider_removals {
            if let Some(entry) = providers.get_mut(name).and_then(Item::as_table_like_mut) {
                entry.remove(UNSUPPORTED_FEATURES_KEY);
            }
        }
    }
    if let Some(models) = doc.get_mut("models").and_then(Item::as_table_like_mut) {
        for nick in &plan.model_removals {
            if let Some(entry) = models.get_mut(nick).and_then(Item::as_table_like_mut) {
                entry.remove(UNSUPPORTED_FEATURES_KEY);
            }
        }
    }
    Ok(())
}

/// The `[capability]` table key.
const CAPABILITY_KEY: &str = "capability";

/// The `[capability.overrides]` table key.
const OVERRIDES_KEY: &str = "overrides";

/// The per-cell array the fold extends.
const OVERRIDE_UNSUPPORTED_KEY: &str = "unsupported";

/// The key path of the `[capability.overrides]` cell for `spec`.
fn override_cell_path(spec: &str) -> String {
    format!("{CAPABILITY_KEY}.{OVERRIDES_KEY}.{}", render_spec_key(spec))
}

/// One [`Refusal::OverrideShape`] line: the path and the shape it needs.
fn shape_line(path: &str, shape: &str) -> String {
    format!("{path} (must be {shape})")
}

/// One line per fold destination that already exists in a shape the fold
/// cannot extend, in deterministic order. Destinations that do not exist yet
/// are fine (the fold creates them), and nothing is checked when there is
/// nothing to fold.
fn override_shape_problems(doc: &DocumentMut, plan: &UnsupportedFeaturesPlan) -> Vec<String> {
    if plan.folds.is_empty() {
        return Vec::new();
    }
    let Some(capability) = doc.get(CAPABILITY_KEY) else {
        return Vec::new();
    };
    let Some(capability) = capability.as_table_like() else {
        return vec![shape_line(CAPABILITY_KEY, "a table")];
    };
    let Some(overrides) = capability.get(OVERRIDES_KEY) else {
        return Vec::new();
    };
    let Some(overrides) = overrides.as_table_like() else {
        return vec![shape_line(
            &format!("{CAPABILITY_KEY}.{OVERRIDES_KEY}"),
            "a table",
        )];
    };
    let mut problems = Vec::new();
    for fold in &plan.folds {
        let Some(cell) = overrides.get(&fold.spec) else {
            continue;
        };
        let path = override_cell_path(&fold.spec);
        match cell.as_table_like() {
            None => problems.push(shape_line(&path, "a table")),
            Some(cell)
                if cell
                    .get(OVERRIDE_UNSUPPORTED_KEY)
                    .is_some_and(|item| item.as_array().is_none()) =>
            {
                problems.push(shape_line(
                    &format!("{path}.{OVERRIDE_UNSUPPORTED_KEY}"),
                    "an array",
                ));
            }
            Some(_) => {}
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// One line per folded value whose target cell already lists the same string
/// in `force_supported`, in deterministic (spec, value) order.
fn forced_fold_conflicts(doc: &DocumentMut, plan: &UnsupportedFeaturesPlan) -> Vec<String> {
    let overrides = doc
        .get("capability")
        .and_then(Item::as_table_like)
        .and_then(|capability| capability.get("overrides"))
        .and_then(Item::as_table_like);
    let Some(overrides) = overrides else {
        return Vec::new();
    };
    let mut cells = Vec::new();
    for fold in &plan.folds {
        let forced = overrides
            .get(&fold.spec)
            .and_then(Item::as_table_like)
            .and_then(|entry| entry.get("force_supported"))
            .and_then(Item::as_array);
        let Some(forced) = forced else {
            continue;
        };
        for value in fold.values.iter().filter_map(Value::as_str) {
            if forced.iter().any(|f| f.as_str() == Some(value)) {
                cells.push(format!(
                    "[capability.overrides.{}] `{}`: in force_supported, and in the \
                     retired {}",
                    render_spec_key(&fold.spec),
                    message_safe_key(value),
                    message_safe_key(&fold.source),
                ));
            }
        }
    }
    cells.sort();
    cells.dedup();
    cells
}

/// A `[capability.overrides.<spec>]` key as it renders in a TOML header in
/// refusal text: a model-scoped `provider:nick` spec needs quoting, and the
/// operator-written names are control-char filtered.
fn render_spec_key(spec: &str) -> String {
    let spec = message_safe_key(spec);
    if spec.contains(':') {
        format!("\"{spec}\"")
    } else {
        spec
    }
}

/// Remove every present-but-empty egress allowlist, then the `[bedrock]`
/// table if nothing is left in it. Runs only after
/// [`present_egress_allowlists`] found no non-empty list; a non-array value
/// is left in place for the shared gate to reject.
fn drop_empty_egress_allowlists(doc: &mut DocumentMut) {
    if let Some(bedrock) = doc.get_mut("bedrock").and_then(Item::as_table_like_mut) {
        for key in BEDROCK_ALLOWLIST_KEYS {
            if array_is_empty(bedrock, key) {
                bedrock.remove(key);
            }
        }
    }
    if let Some(providers) = doc.get_mut("providers").and_then(Item::as_table_like_mut) {
        for (_, item) in providers.iter_mut() {
            if let Some(entry) = item.as_table_like_mut()
                && array_is_empty(entry, PROVIDER_ALLOWLIST_KEY)
            {
                entry.remove(PROVIDER_ALLOWLIST_KEY);
            }
        }
    }
    if bedrock_table_is_empty(doc) {
        doc.as_table_mut().remove("bedrock");
    }
}

/// Whether the document carries a `[bedrock]` table with no keys in it.
fn bedrock_table_is_empty(doc: &DocumentMut) -> bool {
    doc.get("bedrock")
        .and_then(Item::as_table_like)
        .is_some_and(TableLike::is_empty)
}

/// Every non-empty egress allowlist in the document, in deterministic order:
/// the global `[bedrock]` `allowed_betas` / `allowed_body_fields`, then each
/// `[providers.<name>]` `allowed_betas`. An empty list is pass-through
/// (carries no behavior) and is not reported.
fn present_egress_allowlists(doc: &DocumentMut) -> Vec<PresentAllowlist> {
    let mut found = Vec::new();
    if let Some(bedrock) = doc.get("bedrock").and_then(Item::as_table_like) {
        for key in BEDROCK_ALLOWLIST_KEYS {
            if let Some(entry_count) = non_empty_array_len(bedrock, key) {
                found.push(PresentAllowlist {
                    path: format!("bedrock.{key}"),
                    entry_count,
                });
            }
        }
    }
    if let Some(providers) = doc.get("providers").and_then(Item::as_table_like) {
        for (name, item) in providers.iter() {
            if let Some(entry_count) = item
                .as_table_like()
                .and_then(|entry| non_empty_array_len(entry, PROVIDER_ALLOWLIST_KEY))
            {
                found.push(PresentAllowlist {
                    path: format!(
                        "providers.{}.{PROVIDER_ALLOWLIST_KEY}",
                        message_safe_key(name)
                    ),
                    entry_count,
                });
            }
        }
    }
    found
}

/// The length of `table`'s `key` when it is a non-empty array.
fn non_empty_array_len(table: &dyn TableLike, key: &str) -> Option<usize> {
    table
        .get(key)
        .and_then(Item::as_array)
        .map(Array::len)
        .filter(|len| *len > 0)
}

/// Whether `table` carries `key` as an empty array.
fn array_is_empty(table: &dyn TableLike, key: &str) -> bool {
    table
        .get(key)
        .and_then(Item::as_array)
        .is_some_and(Array::is_empty)
}

/// One `unsupported_features` list to fold: the `[capability.overrides]`
/// spec it lands under, its values, and the retired key it came from (for
/// refusal text).
struct UnsupportedFold {
    spec: String,
    values: Vec<Value>,
    source: String,
}

/// The mutation plan [`collect_unsupported_features`] hands to the folding
/// pass: the provider / model names whose `unsupported_features` key retires,
/// and the folds for the non-empty lists.
struct UnsupportedFeaturesPlan {
    provider_removals: Vec<String>,
    model_removals: Vec<String>,
    folds: Vec<UnsupportedFold>,
}

/// Immutable collection pass for [`migrate_v4_to_v5`]: the provider and
/// model names whose `unsupported_features` key must be removed, plus the
/// folds for the non-empty lists. A present-but-empty list is recorded for
/// removal with no fold; a model with no `provider` field is skipped
/// entirely (left for the gate).
fn collect_unsupported_features(doc: &DocumentMut) -> UnsupportedFeaturesPlan {
    let mut provider_removals = Vec::new();
    let mut model_removals = Vec::new();
    let mut folds = Vec::new();

    if let Some(providers) = doc.get("providers").and_then(Item::as_table_like) {
        for (name, item) in providers.iter() {
            let Some(entry) = item.as_table_like() else {
                continue;
            };
            if let Some(values) = read_unsupported_features(entry) {
                provider_removals.push(name.to_string());
                if !values.is_empty() {
                    folds.push(UnsupportedFold {
                        spec: name.to_string(),
                        values,
                        source: format!("[providers.{name}].{UNSUPPORTED_FEATURES_KEY}"),
                    });
                }
            }
        }
    }

    if let Some(models) = doc.get("models").and_then(Item::as_table_like) {
        for (nick, item) in models.iter() {
            let Some(entry) = item.as_table_like() else {
                continue;
            };
            let Some(values) = read_unsupported_features(entry) else {
                continue;
            };
            let Some(provider) = entry.get("provider").and_then(Item::as_str) else {
                continue;
            };
            model_removals.push(nick.to_string());
            if !values.is_empty() {
                folds.push(UnsupportedFold {
                    spec: format!("{provider}:{nick}"),
                    values,
                    source: format!("[models.{nick}].{UNSUPPORTED_FEATURES_KEY}"),
                });
            }
        }
    }

    UnsupportedFeaturesPlan {
        provider_removals,
        model_removals,
        folds,
    }
}

/// Read a target's `unsupported_features` key: `Some(values)` when present as
/// an array (possibly empty), `None` when absent or not an array (a
/// non-array value is left in place for the shared gate to reject).
fn read_unsupported_features(table: &dyn TableLike) -> Option<Vec<Value>> {
    let arr = table.get(UNSUPPORTED_FEATURES_KEY)?.as_array()?;
    Some(arr.iter().cloned().collect())
}

/// Append `values` to `[capability.overrides.<spec>] unsupported`, creating
/// the `[capability]` / `[capability.overrides]` parents (implicit, so no
/// empty headers are emitted) and the per-spec table as needed. Values whose
/// string form already appears on the target array are skipped so a
/// duplicate legacy+override declaration folds without doubling up.
///
/// # Errors
///
/// [`Refusal::OverrideShape`] naming the first destination that exists in a
/// shape the fold cannot extend. Nothing is written before that check.
fn append_unsupported_override(
    doc: &mut DocumentMut,
    spec: &str,
    values: &[Value],
) -> Result<(), Refusal> {
    let shape_refusal = |path: &str, shape: &str| Refusal::OverrideShape {
        paths: vec![shape_line(path, shape)],
    };
    let cell_path = override_cell_path(spec);
    let overrides = ensure_overrides_table(doc)?;
    if !overrides.contains_key(spec) {
        overrides.insert(spec, Item::Table(Table::new()));
    }
    let entry = overrides
        .get_mut(spec)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| shape_refusal(&cell_path, "a table"))?;
    if !entry.contains_key(OVERRIDE_UNSUPPORTED_KEY) {
        entry.insert(OVERRIDE_UNSUPPORTED_KEY, toml_edit::value(Array::new()));
    }
    let arr = entry
        .get_mut(OVERRIDE_UNSUPPORTED_KEY)
        .and_then(Item::as_array_mut)
        .ok_or_else(|| {
            shape_refusal(
                &format!("{cell_path}.{OVERRIDE_UNSUPPORTED_KEY}"),
                "an array",
            )
        })?;
    let mut seen: std::collections::BTreeSet<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    for value in values {
        if let Some(s) = value.as_str()
            && !seen.insert(s.to_string())
        {
            continue;
        }
        arr.push(value.clone());
    }
    Ok(())
}

/// Get (or create) the `[capability.overrides]` table, creating the
/// `[capability]` and `[capability.overrides]` parents as implicit tables so
/// they never render as empty headers.
///
/// # Errors
///
/// [`Refusal::OverrideShape`] when `capability` or `overrides` already
/// exists in a non-table shape.
fn ensure_overrides_table(doc: &mut DocumentMut) -> Result<&mut dyn TableLike, Refusal> {
    let not_a_table = |path: String| Refusal::OverrideShape {
        paths: vec![shape_line(&path, "a table")],
    };
    let root = doc.as_table_mut();
    if !root.contains_key(CAPABILITY_KEY) {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert(CAPABILITY_KEY, Item::Table(table));
    }
    let capability = root
        .get_mut(CAPABILITY_KEY)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| not_a_table(CAPABILITY_KEY.to_string()))?;
    if !capability.contains_key(OVERRIDES_KEY) {
        let mut table = Table::new();
        table.set_implicit(true);
        capability.insert(OVERRIDES_KEY, Item::Table(table));
    }
    capability
        .get_mut(OVERRIDES_KEY)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| not_a_table(format!("{CAPABILITY_KEY}.{OVERRIDES_KEY}")))
}

/// The config key a provider entry's OAuth credential reference lives on.
/// The Bedrock lanes authenticate through a `bedrock_mantle.creds` /
/// `creds` descriptor instead, and no such descriptor names an
/// `oauth://` seat -- so an entry whose seats need pooling always carries
/// its ref here.
const PROVIDER_SECRET_REF_KEY: &str = "api_key_ref";

/// The provider family named by a provider entry's `oauth://` credential
/// ref, or `None` when the entry has no such ref (an API-key or Bedrock
/// entry, or a malformed value the shared gate rejects).
fn oauth_family_of_entry(entry: &dyn TableLike) -> Option<String> {
    let uri = entry.get(PROVIDER_SECRET_REF_KEY)?.as_str()?;
    match SecretRef::parse(uri) {
        Ok(SecretRef::OAuth { provider, .. }) => Some(provider),
        _ => None,
    }
}

/// Whether a provider entry's `oauth://` ref is BARE -- no `#label`, so at
/// v3 it expanded to every stored seat of its family.
fn has_bare_oauth_ref(entry: &dyn TableLike) -> bool {
    let Some(uri) = entry.get(PROVIDER_SECRET_REF_KEY).and_then(Item::as_str) else {
        return false;
    };
    matches!(
        SecretRef::parse(uri),
        Ok(SecretRef::OAuth { label: None, .. })
    )
}

/// One provider entry whose bare `oauth://` ref expanded to every stored
/// seat of its family under v3 semantics, and so may need materializing into
/// explicit accounts under v4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareOauthRef {
    /// The `[providers.<name>]` key the ref sits on.
    pub entry: String,
    /// The provider family the ref names.
    pub family: String,
}

/// Every provider entry carrying a BARE `oauth://` ref, in document order.
///
/// Pure: it reports what the document says, never what the credential store
/// holds. The store-aware half of the migration reads this to decide which
/// families have more than one stored seat and so need explicit accounts --
/// a v4 file must not leave a bare ref standing in for several seats,
/// because at v4 a bare ref means the DEFAULT SEAT alone.
#[must_use]
pub fn bare_oauth_pool_candidates(doc: &DocumentMut) -> Vec<BareOauthRef> {
    let Some(providers) = doc.get("providers").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    providers
        .iter()
        .filter_map(|(name, item)| {
            let entry = item.as_table_like()?;
            if !has_bare_oauth_ref(entry) {
                return None;
            }
            Some(BareOauthRef {
                entry: name.to_string(),
                family: oauth_family_of_entry(entry)?,
            })
        })
        .collect()
}

/// One provider entry's move onto the explicit-pool shape: which pool the
/// entry's accounts group under, the member entry names in write order, and
/// the `seat_selection` value (if any) the pool block inherits.
///
/// Produced by the pure v3 -> v4 rung or by the store-aware caller that
/// knows the family's stored seats, and applied by
/// [`apply_seat_pool_move`]. Splitting the plan from the application is what
/// lets the command compose one combined diff and reproduce it byte-for-byte
/// under the write lock.
///
/// The ORIGINAL entry is always the pool's first member. It keeps its
/// operator-chosen name in the ordinary case: at v4 its bare
/// `oauth://<family>` ref means the default seat, and every `[models.X]
/// provider` value naming it keeps resolving. Only the family's LABELLED
/// seats materialize as new entries, under the names the naming convention
/// derives.
///
/// The ONE exception is `rename_to`: an entry NAMED after its own provider
/// family holds the very name the pool must take (providers, pools and
/// model nicknames share one namespace), so it moves to the default-seat
/// account name the convention derives for it. That shape is what a config
/// authored before pools existed looks like, so the migration renames rather
/// than refusing and leaving the operator to hand-edit the entry and every
/// model naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPoolMove {
    /// The provider entry the move starts from: the pool's default-seat
    /// member, named as it appears in the document BEFORE the move.
    pub entry: String,
    /// The name `entry` is renamed to, or `None` when it keeps its own.
    /// Set only for an entry holding its family's plain name, which the
    /// pool needs -- see [`crate::seat_naming::family_default_rename`].
    pub rename_to: Option<String>,
    /// The `[pools.<name>]` key the accounts group under.
    pub pool: String,
    /// One account entry per LABELLED seat, in write order. Each is a copy
    /// of `entry` carrying that seat's `#label` ref.
    pub accounts: Vec<SeatPoolAccount>,
}

impl SeatPoolMove {
    /// The name the pool's default-seat member carries AFTER the move: the
    /// rename target when one is planned, else the entry's own name.
    #[must_use]
    pub fn member_name(&self) -> &str {
        self.rename_to.as_deref().unwrap_or(&self.entry)
    }
}

/// One account entry a [`SeatPoolMove`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPoolAccount {
    /// The `[providers.<name>]` key this account takes.
    pub entry_name: String,
    /// The `oauth://` ref this account authenticates with.
    pub secret_ref: String,
    /// Whether the entry already exists carrying this ref, making its write
    /// a no-op (an idempotent rerun over a config the move already produced).
    pub already_present: bool,
}

/// Plan the PURE part of one provider entry's v3 -> v4 move: relocating a
/// present provider-level `seat_selection` onto a `[pools.<name>]` block
/// whose sole member is that entry.
///
/// The pool takes the plain provider-family name from
/// [`crate::seat_naming::pool_name`] -- never a rule restated here, because
/// the migration and the login writer must generate identical names. An
/// entry that itself HOLDS that name vacates it, moving to the default-seat
/// account name [`crate::seat_naming::family_default_rename`] derives; the
/// models naming it follow the rename.
///
/// # Errors
///
/// Returns a description of why the move has no single answer: no
/// `oauth://` ref to derive a family from, an unusable family token, a
/// derived pool name already held by an unrelated entry, a model nickname,
/// or a hand-authored pool block, or a rename target already held by a
/// different entry. The caller turns these into one
/// [`Refusal::SeatSelectionRelocation`], leaving the document untouched.
fn plan_seat_pool_move(doc: &DocumentMut, entry_name: &str) -> Result<SeatPoolMove, String> {
    let entry = doc
        .get("providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(entry_name))
        .and_then(Item::as_table_like)
        .ok_or_else(|| format!("[providers.{entry_name}] is not a table"))?;

    let family = oauth_family_of_entry(entry).ok_or_else(|| {
        format!(
            "[providers.{entry_name}] carries `seat_selection` but no `oauth://` \
             `{PROVIDER_SECRET_REF_KEY}`, so no provider family names its pool"
        )
    })?;
    let rename_to = plan_family_rename(doc, entry_name, &family)?;
    let pool = derive_pool_name(doc, entry_name, &family, rename_to.is_some())?;
    Ok(SeatPoolMove {
        entry: entry_name.to_string(),
        rename_to,
        pool,
        accounts: Vec::new(),
    })
}

/// The name a family-named provider entry vacates its family name for, or
/// `None` when the entry is not family-named.
///
/// The derivation is the naming module's; the occupancy check is this
/// caller's, against the raw document (the pure rung has no typed `Config`).
/// A target already held by a different entry refuses: the migration never
/// displaces one credential's entry with another's.
fn plan_family_rename(
    doc: &DocumentMut,
    entry_name: &str,
    family: &str,
) -> Result<Option<String>, String> {
    let renamed = crate::seat_naming::family_default_rename(entry_name, family).map_err(|e| {
        format!("[providers.{entry_name}]: account entry name for provider family `{family}`: {e}")
    })?;
    let Some(renamed) = renamed else {
        return Ok(None);
    };
    if provider_entry_exists(doc, &renamed) {
        return Err(format!(
            "[providers.{entry_name}] holds the plain family name `{family}`, which the \
             `[pools.{family}]` block must take, so the entry moves to `{renamed}` -- but a \
             `[providers.{renamed}]` entry already exists and authenticates with its own \
             credential. Rename one of them by hand, then rerun"
        ));
    }
    Ok(Some(renamed))
}

/// Whether `[providers.<name>]` is present in the document.
fn provider_entry_exists(doc: &DocumentMut, name: &str) -> bool {
    doc.get("providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.contains_key(name))
}

/// The `[pools.<name>]` key one provider entry's accounts group under, or a
/// description of the namespace collision that leaves the move with no
/// single answer.
///
/// Providers, pools and model nicknames share ONE namespace on a
/// `[models.X] provider` value, so a derived pool name held by any of them
/// is a refusal rather than a guess. `entry_vacates_pool_name` says the
/// entry being moved is the one holding that name and is about to give it
/// up, so it does not count as a collision -- that shape is a rename, not an
/// unrelated claimant.
fn derive_pool_name(
    doc: &DocumentMut,
    entry_name: &str,
    family: &str,
    entry_vacates_pool_name: bool,
) -> Result<String, String> {
    let pool = crate::seat_naming::pool_name(family).map_err(|e| {
        format!("[providers.{entry_name}]: pool name for provider family `{family}`: {e}")
    })?;
    let held_by_provider = !entry_vacates_pool_name && provider_entry_exists(doc, &pool);
    let held_by_model = doc
        .get("models")
        .and_then(Item::as_table_like)
        .is_some_and(|models| models.contains_key(&pool));
    let held_by_pool = doc
        .get("pools")
        .and_then(Item::as_table_like)
        .is_some_and(|pools| pools.contains_key(&pool));
    if held_by_pool {
        return Err(format!(
            "[providers.{entry_name}]: a `[pools.{pool}]` block already exists; move the \
             entry's `seat_selection` onto it by hand"
        ));
    }
    if held_by_provider || held_by_model {
        return Err(format!(
            "[providers.{entry_name}]: the pool name `{pool}` is already used by a provider \
             entry or a model nickname; providers, pools and model nicknames share one \
             namespace -- rename the colliding entry, then rerun"
        ));
    }
    Ok(pool)
}

/// Apply one [`SeatPoolMove`] to `doc`, format-preserving: rename the entry
/// when the pool needs its name, move the entry's `seat_selection` (when
/// present) onto the pool block, clone the entry once per labelled seat,
/// write `[pools.<name>]` listing the entry plus every account, and repoint
/// the models that routed at the entry onto the pool.
///
/// ORDER MATTERS. The rename runs FIRST and takes the models with it, so a
/// model naming the family-named entry follows it to `<family>-default`
/// before the pool repoint decides whether it should name the pool instead.
/// Composed the other way round, a renamed entry would leave its models
/// pointing at a `[providers.X]` key that no longer exists.
///
/// The repoint is what PRESERVES DISPATCH BREADTH, and it runs exactly when
/// the move materializes labelled-seat accounts. Under v3 a bare
/// `oauth://<family>` ref on the entry expanded to every stored seat, so a
/// model naming that entry dispatched across all of them; under v4 the same
/// ref means the default seat alone. Leaving the model on the entry would
/// silently cut an N-seat model down to one seat -- through the very
/// migration whose purpose is behavior preservation. The pool carries the
/// full seat set, so the model has to name the pool.
///
/// A move with NO accounts (the pure rung's `seat_selection` relocation on a
/// single-seat family) leaves model references alone: the pool has one
/// member, so breadth is identical either way, and a member inherits its
/// pool's strategy through `Config::seat_selection_for` regardless of which
/// name the model uses. Fewer bytes changed, same behavior.
///
/// Every map walk goes through [`TableLike`] so an inline `providers = { ...
/// }` / `models = { ... }` shape is handled as well as the standard-table
/// one. Idempotent: an already-applied rename is a no-op (the source key is
/// gone and the target already carries the entry), an account already present
/// with the right ref is left alone, a member already listed is not
/// duplicated, and a model already naming the pool is not rewritten.
pub fn apply_seat_pool_move(doc: &mut DocumentMut, mv: &SeatPoolMove) {
    if let Some(renamed) = &mv.rename_to {
        rename_provider_entry(doc, &mv.entry, renamed);
    }
    let member = mv.member_name().to_string();
    let selection = take_provider_seat_selection(doc, &member);

    for account in &mv.accounts {
        clone_provider_entry(doc, &member, account);
    }
    let mut members = vec![member.as_str()];
    members.extend(mv.accounts.iter().map(|a| a.entry_name.as_str()));
    write_pool_block(doc, &mv.pool, &members, selection);
    if !mv.accounts.is_empty() {
        repoint_models_at(doc, &member, &mv.pool);
    }
}

/// Move a `[providers.<from>]` block to `[providers.<to>]`, taking the
/// models that named it along, format-preserving: the block's own body,
/// comments and position are the item's, and the key's decor (the comment
/// lines attached to the header) rides on the key.
///
/// A no-op when `from` is absent (an already-applied rename) or when `to` is
/// occupied -- the caller derives `to` through the naming module, which
/// refuses an occupied target rather than displacing a credential, so
/// reaching either arm means the rename has already landed.
fn rename_provider_entry(doc: &mut DocumentMut, from: &str, to: &str) {
    let Some(providers) = doc.get_mut("providers").and_then(Item::as_table_like_mut) else {
        return;
    };
    if providers.contains_key(to) {
        return;
    }
    let Some(key) = providers.key(from).cloned() else {
        return;
    };
    let Some(item) = providers.remove(from) else {
        return;
    };
    let renamed = Key::new(to)
        .with_leaf_decor(key.leaf_decor().clone())
        .with_dotted_decor(key.dotted_decor().clone());
    providers.entry_format(&renamed).or_insert(item);
    repoint_models_at(doc, from, to);
}

/// Repoint every `[models.X] provider` value naming `from` at `to`.
///
/// `[models.X] provider` resolves against providers and pools in ONE
/// namespace, so this is a rename of the target, not a new field -- which is
/// why the same rewrite serves both moving models onto a pool and following
/// a renamed provider entry.
fn repoint_models_at(doc: &mut DocumentMut, from: &str, to: &str) {
    let Some(models) = doc.get_mut("models").and_then(Item::as_table_like_mut) else {
        return;
    };
    for (_, item) in models.iter_mut() {
        let Some(model) = item.as_table_like_mut() else {
            continue;
        };
        if model.get("provider").and_then(Item::as_str) == Some(from) {
            model.insert("provider", toml_edit::value(to));
        }
    }
}

/// The `[models.X]` nicknames whose `provider` value names `entry`, in
/// document order. Read-only counterpart to the repoint
/// [`apply_seat_pool_move`] performs, for the change summary the operator
/// confirms.
#[must_use]
pub fn models_routed_at(doc: &DocumentMut, entry: &str) -> Vec<String> {
    let Some(models) = doc.get("models").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|(_, item)| {
            item.as_table_like()
                .and_then(|model| model.get("provider"))
                .and_then(Item::as_str)
                == Some(entry)
        })
        .map(|(nickname, _)| nickname.to_string())
        .collect()
}

/// Remove and return a provider entry's `seat_selection` value, preserving
/// its formatting so the relocated key renders as the operator wrote it.
fn take_provider_seat_selection(doc: &mut DocumentMut, entry_name: &str) -> Option<Value> {
    doc.get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(entry_name))
        .and_then(Item::as_table_like_mut)
        .and_then(|entry| entry.remove(SEAT_SELECTION_KEY))
        .and_then(|item| item.as_value().cloned())
}

/// Add one seat's account entry as a copy of `source` carrying that seat's
/// `oauth://` ref. An entry already present with the right ref is left
/// byte-untouched (the idempotent-rerun path).
fn clone_provider_entry(doc: &mut DocumentMut, source: &str, account: &SeatPoolAccount) {
    let Some(providers) = doc.get_mut("providers").and_then(Item::as_table_like_mut) else {
        return;
    };
    if providers.contains_key(&account.entry_name) {
        return;
    }
    let Some(item) = providers.get(source).cloned() else {
        return;
    };
    providers.insert(&account.entry_name, item);
    if let Some(entry) = providers
        .get_mut(&account.entry_name)
        .and_then(Item::as_table_like_mut)
    {
        entry.insert(
            PROVIDER_SECRET_REF_KEY,
            toml_edit::value(account.secret_ref.as_str()),
        );
    }
}

/// Write `[pools.<pool>]`: union `members` into whatever member list the
/// block already carries, creating the `pools` table and the block itself
/// when absent.
///
/// Format-preserving and IDEMPOTENT: a member already listed is not
/// duplicated, so re-running over this function's own output changes no
/// bytes. That property is what lets two writers -- the migration ladder
/// and the login auto-surface -- grow one pool without either having to
/// know what the other wrote.
///
/// Nothing else on the block is touched: an existing `seat_selection` or
/// `accepts_new_logins` marker is an operator statement and survives
/// verbatim.
pub fn upsert_pool_members(doc: &mut DocumentMut, pool: &str, members: &[&str]) {
    let root = doc.as_table_mut();
    if !root.contains_key("pools") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("pools", Item::Table(table));
    }
    let Some(pools) = root.get_mut("pools").and_then(Item::as_table_like_mut) else {
        return;
    };
    if !pools.contains_key(pool) {
        pools.insert(pool, Item::Table(Table::new()));
    }
    let Some(block) = pools.get_mut(pool).and_then(Item::as_table_like_mut) else {
        return;
    };
    if !block.contains_key("members") {
        block.insert("members", toml_edit::value(Array::new()));
    }
    if let Some(arr) = block.get_mut("members").and_then(Item::as_array_mut) {
        let mut seen: std::collections::BTreeSet<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for member in members {
            // Insert BEFORE the push, so a duplicate within `members`
            // itself is dropped too -- a caller assembling a list from two
            // sources must not be able to write `["a", "a"]`.
            if seen.insert((*member).to_string()) {
                arr.push(*member);
            }
        }
    }
}

/// [`upsert_pool_members`] plus the migration's own extra: set
/// `seat_selection` when a value was relocated off a provider entry onto
/// this block.
fn write_pool_block(doc: &mut DocumentMut, pool: &str, members: &[&str], selection: Option<Value>) {
    upsert_pool_members(doc, pool, members);
    let Some(block) = doc
        .get_mut("pools")
        .and_then(Item::as_table_like_mut)
        .and_then(|pools| pools.get_mut(pool))
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    if let Some(selection) = selection {
        block.insert(SEAT_SELECTION_KEY, Item::Value(selection));
    }
}

/// The retired provider-level knob the v3 -> v4 rung relocates.
const SEAT_SELECTION_KEY: &str = "seat_selection";

/// Pure v3 -> v4 transform on the RAW toml_edit document. NO IO and no
/// store access -- the ladder runs twice (plan and locked re-read) and the
/// committed bytes must reproduce what planning gated, so a credential-store
/// read here would break both that property and the ladder's offline
/// testability. The store-aware half (materializing one account entry per
/// stored seat) is composed by the `config migrate` command on top of this
/// rung's output.
///
/// The v3 -> v4 break makes explicit `[pools.<name>]` blocks the only
/// multi-seat shape: `seat_selection` is a property of a SET of accounts,
/// so it moves off `[providers.X]` onto the block that names the set, and a
/// bare `oauth://<provider>` ref means the DEFAULT SEAT alone. This rung
/// stamps LITERAL `version = 4` and relocates every present provider-level
/// `seat_selection` onto a pool block whose sole member is that entry.
///
/// A file carrying no provider-level `seat_selection` needs no structural
/// change: the rung stamps the version and nothing else.
///
/// # Errors
///
/// Returns [`Refusal::SeatSelectionRelocation`] -- with NO mutation -- when
/// a present `seat_selection` has no derivable pool (no `oauth://` ref to
/// name a family, an unusable family token, or a derived pool name held by
/// an unrelated entry, a model nickname, or a hand-authored pool block), or
/// when two entries derive the SAME pool name.
pub fn migrate_v3_to_v4(doc: &mut DocumentMut) -> Result<StepOutcome, Refusal> {
    let mut moves = Vec::new();
    let mut problems = Vec::new();
    for entry_name in provider_entries_with_seat_selection(doc) {
        match plan_seat_pool_move(doc, &entry_name) {
            Ok(mv) => moves.push(mv),
            Err(problem) => problems.push(problem),
        }
    }
    problems.extend(colliding_pool_derivations(&moves));
    if !problems.is_empty() {
        return Err(Refusal::SeatSelectionRelocation { entries: problems });
    }

    for mv in &moves {
        apply_seat_pool_move(doc, mv);
    }
    doc["version"] = toml_edit::value(4i64);

    Ok(StepOutcome {
        from_version: 3,
        to_version: 4,
    })
}

/// One problem line per pool name that MORE THAN ONE planned move derives,
/// naming every entry that claims it.
///
/// Each move writes the pool block with its own entry as a member and its own
/// relocated `seat_selection`, so two moves onto one pool name would merge
/// entries the operator kept separate -- possibly separate egresses -- and let
/// the later knob silently overwrite the earlier one. Refused instead: which
/// accounts share a pool is an operator statement, not something derivable
/// from a shared provider family.
fn colliding_pool_derivations(moves: &[SeatPoolMove]) -> Vec<String> {
    let mut claimants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for mv in moves {
        claimants
            .entry(mv.pool.as_str())
            .or_default()
            .push(mv.entry.as_str());
    }
    claimants
        .into_iter()
        .filter(|(_, entries)| entries.len() > 1)
        .map(|(pool, entries)| {
            format!(
                "{}: each carry `seat_selection` and each derive the pool name `{pool}` from \
                 the same provider family; grouping them into one pool would merge accounts \
                 you kept separate and drop all but one `seat_selection` value -- write the \
                 `[pools.<name>]` blocks for this provider by hand, then rerun",
                entries
                    .iter()
                    .map(|e| format!("[providers.{e}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect()
}

/// Provider entry names carrying a provider-level `seat_selection`, in
/// document order. Walked through [`TableLike`] so an inline `providers =
/// { ... }` map is seen too.
fn provider_entries_with_seat_selection(doc: &DocumentMut) -> Vec<String> {
    let Some(providers) = doc.get("providers").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    providers
        .iter()
        .filter(|(_, item)| {
            item.as_table_like()
                .is_some_and(|entry| entry.contains_key(SEAT_SELECTION_KEY))
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Apply the PURE `config.toml` document transforms for the ladder, from
/// `raw_version` up to the latest, mutating `doc` in place. Performs NO IO
/// and touches NO overlay -- the v1 `[cache_pricing]` fold is
/// `plan_v1_overlay`'s separate concern. Both [`plan_migration`] (on a
/// clone, to build the candidate) and the caller's commit closure (on the
/// re-read document under the write lock) call this, so the committed bytes
/// reproduce exactly what planning gated.
///
/// The ladder is deliberately a flat sequence of literal rungs, not a
/// trait or registry: the next break is one file-local step function plus
/// one rung here.
///
/// - `raw_version <= 1`: `apply_v1_to_v2_doc` stamps `version = 2` and
///   drops `[cache_pricing]`, then the v2 -> v3 rung runs on the result.
/// - `raw_version == 2`: [`migrate_v2_to_v3`] retires the per-status retry
///   lists and stamps `version = 3`.
/// - `raw_version == 3`: [`migrate_v3_to_v4`] relocates a provider-level
///   `seat_selection` onto a `[pools.<name>]` block and stamps
///   `version = 4`.
/// - `raw_version == 4`: [`migrate_v4_to_v5`] folds `unsupported_features`
///   into `[capability.overrides]`, drops the empty egress allowlists (and
///   an emptied `[bedrock]`), and stamps `version = 5`.
///
/// A file already at `LATEST_MIGRATION_VERSION` runs no rung (no step).
///
/// # Errors
///
/// A [`Refusal`] from any rung, leaving `doc` byte-untouched.
pub fn apply_config_transforms(
    doc: &mut DocumentMut,
    raw_version: u32,
) -> Result<Vec<StepOutcome>, Refusal> {
    // Every rung runs on a scratch copy: a later rung's refusal must not
    // leave an earlier rung's edits behind in the caller's document.
    let mut next = doc.clone();
    let mut steps = Vec::new();
    let mut version = raw_version;

    if version <= 1 {
        apply_v1_to_v2_doc(&mut next);
        steps.push(StepOutcome {
            from_version: 1,
            to_version: 2,
        });
        version = 2;
    }

    if version == 2 {
        steps.push(migrate_v2_to_v3(&mut next)?);
        version = 3;
    }

    if version == 3 {
        steps.push(migrate_v3_to_v4(&mut next)?);
        version = 4;
    }

    if version == 4 {
        steps.push(migrate_v4_to_v5(&mut next)?);
    }

    *doc = next;
    Ok(steps)
}

/// Compute a [`MigrationPlan`] for the config `base_doc` at its RAW on-disk
/// `raw_version`, WITHOUT touching disk. Runs every ladder transform in
/// memory and every refusal / conflict check up front, so a returned plan
/// means all of the migrator's validation has passed and the caller can
/// commit it (overlay first, `config.toml` last).
///
/// Dispatches on `raw_version` (never a typed parse -- a too-old file may
/// not deserialize under the current schema). `cache_pricing` is the v1
/// fold input (already sidecar-merged by the caller); `overlay_path` is
/// READ to detect a conflicting or already-folded overlay cell.
///
/// # Errors
///
/// [`MigrateError::VersionTooNew`] for a future version;
/// [`MigrateError::V1ToV2`] for an overlay conflict or an invalid
/// `[cache_pricing]` entry; [`MigrateError::Refused`] when a rung declines a
/// behavior-bearing config. Every error leaves both files byte-untouched.
pub fn plan_migration(
    base_doc: &DocumentMut,
    raw_version: u32,
    cache_pricing: &BTreeMap<String, CachePricingOverride>,
    overlay_path: &Path,
) -> Result<MigrationPlan, MigrateError> {
    if raw_version > LATEST_MIGRATION_VERSION {
        return Err(MigrateError::VersionTooNew {
            found: raw_version,
            supported: LATEST_MIGRATION_VERSION,
        });
    }

    // Overlay fold candidate (v1 only). Ordered before the config transform
    // so an overlay conflict is reported the same way the old ladder did --
    // and, like the config transform, it writes nothing.
    let overlay_candidate = if raw_version <= 1 {
        plan_v1_overlay(cache_pricing, overlay_path).map_err(MigrateError::V1ToV2)?
    } else {
        None
    };

    // Config document transform on a CLONE: a Refusal surfaces here, leaving
    // the caller's `base_doc` (and the real file) untouched.
    let mut doc = base_doc.clone();
    let steps = apply_config_transforms(&mut doc, raw_version).map_err(MigrateError::Refused)?;

    let removed_keys = collect_removed_keys(base_doc, raw_version);
    let renamed_entries = collect_renamed_entries(base_doc, raw_version);
    let to = steps.last().map_or(raw_version, |s| s.to_version);

    let write_kind = if steps.is_empty() {
        WriteKind::NoChange
    } else if let Some(ow) = overlay_candidate {
        WriteKind::ConfigAndOverlay(doc.to_string(), ow)
    } else {
        WriteKind::ConfigOnly(doc.to_string())
    };

    Ok(MigrationPlan {
        from: raw_version,
        to,
        write_kind,
        removed_keys,
        renamed_entries,
        steps,
    })
}

/// The `(from, to)` renames the PURE v3 -> v4 rung performs, derived from the
/// ORIGINAL document, for the change summary.
///
/// Only the knob-relocation path renames here: an entry carrying
/// `seat_selection` and holding the plain family name its new pool block
/// needs. A family-named entry with no knob is phase 2's to rename, if its
/// family turns out to be multi-seat -- which this pure planner cannot know.
/// Refusals are not re-derived: this runs only after the rung itself planned
/// successfully.
fn collect_renamed_entries(doc: &DocumentMut, raw_version: u32) -> Vec<(String, String)> {
    if raw_version > 3 {
        return Vec::new();
    }
    provider_entries_with_seat_selection(doc)
        .into_iter()
        .filter_map(|entry| {
            let mv = plan_seat_pool_move(doc, &entry).ok()?;
            mv.rename_to.map(|to| (mv.entry, to))
        })
        .collect()
}

/// Human-readable descriptions of the keys the migration removes, derived
/// PURELY from the original document, for a dry-run change summary. Sorted,
/// so the report does not depend on document order.
fn collect_removed_keys(doc: &DocumentMut, raw_version: u32) -> Vec<String> {
    let mut removed = Vec::new();
    if let Some(retry) = doc.get("retry").and_then(Item::as_table_like) {
        if retry.contains_key("retry_allowlist") {
            removed.push("retry.retry_allowlist".to_string());
        }
        if retry.contains_key("retry_denylist") {
            removed.push("retry.retry_denylist".to_string());
        }
    }
    if raw_version <= 1 && doc.contains_key("cache_pricing") {
        removed.push("[cache_pricing] (folded into the catalog overlay)".to_string());
    }
    if raw_version <= 3 {
        for entry in provider_entries_with_seat_selection(doc) {
            let entry = message_safe_key(&entry);
            removed.push(format!(
                "[providers.{entry}].seat_selection (relocated onto the pool block that groups \
                 the accounts)"
            ));
        }
    }
    if raw_version <= 4 {
        collect_unsupported_features_removals(doc, &mut removed);
        collect_egress_allowlist_removals(doc, &mut removed);
    }
    removed.sort();
    removed
}

/// Append the provider / model `unsupported_features` keys the v4 -> v5 rung
/// folds into `[capability.overrides]`, for the summary.
fn collect_unsupported_features_removals(doc: &DocumentMut, removed: &mut Vec<String>) {
    if let Some(providers) = doc.get("providers").and_then(Item::as_table_like) {
        for (name, item) in providers.iter() {
            if item
                .as_table_like()
                .is_some_and(|t| t.contains_key(UNSUPPORTED_FEATURES_KEY))
            {
                let name = message_safe_key(name);
                removed.push(format!(
                    "[providers.{name}].unsupported_features (folded into \
                     [capability.overrides.{name}].unsupported)"
                ));
            }
        }
    }
    if let Some(models) = doc.get("models").and_then(Item::as_table_like) {
        for (nick, item) in models.iter() {
            let Some(entry) = item.as_table_like() else {
                continue;
            };
            if entry.contains_key(UNSUPPORTED_FEATURES_KEY) {
                let nick = message_safe_key(nick);
                match entry.get("provider").and_then(Item::as_str) {
                    Some(provider) => removed.push(format!(
                        "[models.{nick}].unsupported_features (folded into \
                         [capability.overrides.\"{}:{nick}\"].unsupported)",
                        message_safe_key(provider)
                    )),
                    None => removed.push(format!("[models.{nick}].unsupported_features")),
                }
            }
        }
    }
}

/// Append the empty egress allowlists the v4 -> v5 rung removes, and the
/// `[bedrock]` table when the removal empties it, for the summary.
fn collect_egress_allowlist_removals(doc: &DocumentMut, removed: &mut Vec<String>) {
    if let Some(bedrock) = doc.get("bedrock").and_then(Item::as_table_like) {
        for key in BEDROCK_ALLOWLIST_KEYS {
            if array_is_empty(bedrock, key) {
                removed.push(format!("bedrock.{key} (empty; retired)"));
            }
        }
        let survivors = bedrock
            .iter()
            .filter(|(key, _)| {
                !(BEDROCK_ALLOWLIST_KEYS.contains(key) && array_is_empty(bedrock, key))
            })
            .count();
        if survivors == 0 {
            removed.push("[bedrock] (empty once its retired lists are removed)".to_string());
        }
    }
    if let Some(providers) = doc.get("providers").and_then(Item::as_table_like) {
        for (name, item) in providers.iter() {
            if item
                .as_table_like()
                .is_some_and(|entry| array_is_empty(entry, PROVIDER_ALLOWLIST_KEY))
            {
                let name = message_safe_key(name);
                removed.push(format!(
                    "providers.{name}.{PROVIDER_ALLOWLIST_KEY} (empty; retired)"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn override_with_wm(wm: f32) -> CachePricingOverride {
        CachePricingOverride {
            wm: Some(wm),
            override_acknowledges_cost_risk: wm < 2.0,
            ..Default::default()
        }
    }

    fn doc_of(text: &str) -> DocumentMut {
        text.parse::<DocumentMut>().unwrap()
    }

    /// Commit a plan's overlay candidate the way the caller's commit does,
    /// so a test can assert the on-disk overlay after a would-be migration.
    fn commit_overlay(plan: &MigrationPlan, overlay_path: &Path) {
        if let Some(ow) = plan.overlay_candidate() {
            catalog_overlay::save(overlay_path, ow.base_revision, ow.cells.clone())
                .expect("overlay save");
        }
    }

    // -----------------------------------------------------------------------
    // civil_from_epoch_day: round-trips known dates (mirrors catalog.rs's
    // own parse_epoch_day round-trip test, its inverse).
    // -----------------------------------------------------------------------

    #[test]
    fn civil_from_epoch_day_matches_known_dates() {
        assert_eq!(civil_from_epoch_day(0), (1970, 1, 1));
        assert_eq!(civil_from_epoch_day(1), (1970, 1, 2));
        assert_eq!(civil_from_epoch_day(365), (1971, 1, 1));
        // 2026-01-01 is epoch day 20454.
        assert_eq!(civil_from_epoch_day(20_454), (2026, 1, 1));
        // Pre-epoch (negative epoch-day) input.
        assert_eq!(civil_from_epoch_day(-1), (1969, 12, 31));
        // Leap day, and the day immediately after it.
        assert_eq!(civil_from_epoch_day(18_321), (2020, 2, 29));
        assert_eq!(civil_from_epoch_day(18_322), (2020, 3, 1));
        // Century-boundary leap year (2000 IS divisible by 400 -> leap).
        assert_eq!(civil_from_epoch_day(11_016), (2000, 2, 29));
        // Century-boundary non-leap year (2100 is divisible by 100 but NOT
        // 400 -> not a leap year, so Feb 2100 has only 28 days).
        assert_eq!(civil_from_epoch_day(47_481), (2099, 12, 31));
        assert_eq!(civil_from_epoch_day(47_541), (2100, 3, 1));
    }

    // -----------------------------------------------------------------------
    // Happy path (pure plan): cache_pricing folds into the overlay CANDIDATE,
    // the config CANDIDATE bumps to v3 with [cache_pricing] dropped and
    // operator comments preserved -- and NOTHING is written to disk.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_folds_cache_pricing_into_overlay_candidate_and_bumps_config_to_latest() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let src = "# operator note: grok override below\n\
             [server]\n\
             host = \"127.0.0.1\" # loopback only\n\
             port = 8787\n\
             \n\
             [cache_pricing]\n\
             \"openai-compat:grok-*\" = { wm = 1.5, verified_at = \"2026-06-01\", \
             override_acknowledges_cost_risk = true }\n";
        let doc = doc_of(src);
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                wm: Some(1.5),
                verified_at: Some("2026-06-01".to_string()),
                override_acknowledges_cost_risk: true,
                ..Default::default()
            },
        );

        // Act
        let plan = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("plan");

        // Assert: planning is PURE -- the overlay file was not created.
        assert!(
            !overlay_path.exists(),
            "planning must not write the overlay"
        );
        assert_eq!(plan.from, 1);
        assert_eq!(plan.to, LATEST_MIGRATION_VERSION);
        assert!(matches!(plan.write_kind, WriteKind::ConfigAndOverlay(..)));

        // Assert: the overlay candidate carries the migrated cell.
        let ow = plan.overlay_candidate().expect("overlay candidate");
        assert_eq!(ow.base_revision, 0);
        let cell = ow
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.verified_at, "2026-06-01");
        assert_eq!(cell.wm, Some(1.5));

        // Assert: the config candidate is at the latest version,
        // [cache_pricing] gone, comments and unrelated content preserved, and
        // it re-parses as a current Config.
        let candidate = plan.config_candidate().expect("config candidate");
        assert!(
            candidate.contains(&format!("version = {LATEST_MIGRATION_VERSION}")),
            "candidate: {candidate}"
        );
        assert!(
            !candidate.contains("cache_pricing"),
            "candidate: {candidate}"
        );
        assert!(
            candidate.contains("# operator note: grok override below"),
            "candidate: {candidate}"
        );
        assert!(
            candidate.contains("host = \"127.0.0.1\" # loopback only"),
            "candidate: {candidate}"
        );
        let reparsed: crate::config::Config = toml::from_str(candidate).expect("reparse");
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
        assert!(reparsed.cache_pricing.is_empty());
    }

    #[test]
    fn plan_v1_no_cache_pricing_is_config_only_and_still_bumps_version() {
        // Arrange: a v1 config with no [cache_pricing] at all.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("[server]\nhost = \"127.0.0.1\"\n");

        // Act
        let plan = plan_migration(&doc, 1, &BTreeMap::new(), &overlay_path).expect("plan");

        // Assert: nothing to fold -> config-only, no overlay candidate, but
        // the config candidate still stamps the version forward.
        assert!(matches!(plan.write_kind, WriteKind::ConfigOnly(_)));
        assert!(plan.overlay_candidate().is_none());
        assert!(!overlay_path.exists());
        let reparsed: crate::config::Config =
            toml::from_str(plan.config_candidate().unwrap()).unwrap();
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
    }

    /// `[cache_pricing]` written in the TABLE form (`[cache_pricing."key"]`
    /// dotted sub-tables, one per selector) rather than the inline-map form
    /// used by the other tests here -- this is what `toml::to_string_pretty`
    /// actually emits for a `BTreeMap<String, T>` field, so it is the
    /// realistic on-disk shape for an operator-edited or `config show`-saved
    /// file. Also covers MULTIPLE selectors and other unrelated tables
    /// interleaved around `[cache_pricing]`, so the fold must drop the whole
    /// subtree without disturbing `[providers.foo]`.
    #[test]
    fn plan_v1_table_form_cache_pricing_with_multiple_entries() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(
            "[server]\n\
             host = \"127.0.0.1\"\n\
             \n\
             [cache_pricing.\"openai-compat:grok-*\"]\n\
             wm = 1.5\n\
             override_acknowledges_cost_risk = true\n\
             \n\
             [cache_pricing.\"openai-compat:mistral-*\"]\n\
             min_prefix_tokens = 2048\n\
             \n\
             [providers.foo]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"env://X\"\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));
        cache_pricing.insert(
            "openai-compat:mistral-*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(2048),
                ..Default::default()
            },
        );

        // Act
        let plan = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("plan");

        // Assert: both selectors in the overlay candidate, [providers.foo]
        // survives in the config candidate, and the candidate re-parses.
        let ow = plan.overlay_candidate().expect("overlay candidate");
        assert!(ow.cells.contains_key("openai-compat:grok-*"));
        assert!(ow.cells.contains_key("openai-compat:mistral-*"));

        let candidate = plan.config_candidate().expect("config candidate");
        assert!(
            !candidate.contains("cache_pricing"),
            "candidate: {candidate}"
        );
        assert!(
            candidate.contains("[providers.foo]"),
            "candidate: {candidate}"
        );
        let reparsed: crate::config::Config = toml::from_str(candidate).expect("reparse");
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
        assert!(reparsed.cache_pricing.is_empty());
        assert!(reparsed.providers.contains_key("foo"));
    }

    // -----------------------------------------------------------------------
    // Idempotence: after the overlay half of a v1 migration has committed but
    // config.toml is still v1 (a crash between the two commit phases), a
    // re-plan against the SAME still-v1 doc plans NO overlay write (the cell
    // already matches) and just the config stamp -- so the rerun completes
    // cleanly without duplicating or conflicting.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_rerun_after_overlay_committed_plans_no_overlay_write() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(
            "[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { \
             wm = 1.5 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // First plan + commit the overlay half only (config.toml still v1).
        let first = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("first plan");
        commit_overlay(&first, &overlay_path);
        let overlay_after_first = catalog_overlay::load(&overlay_path).expect("load");

        // Act: re-plan against the SAME still-v1 doc.
        let second = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("rerun plan");

        // Assert: the rerun plans NO overlay write (the cell already matches),
        // only the config stamp -- and the on-disk overlay is unchanged.
        assert!(
            second.overlay_candidate().is_none(),
            "an already-folded cell must not re-plan an overlay write"
        );
        assert!(matches!(second.write_kind, WriteKind::ConfigOnly(_)));
        commit_overlay(&second, &overlay_path);
        let overlay_after_second = catalog_overlay::load(&overlay_path).expect("load");
        assert_eq!(overlay_after_second.revision, overlay_after_first.revision);
        assert_eq!(overlay_after_second.cells, overlay_after_first.cells);
    }

    #[test]
    fn plan_v1_idempotent_rerun_survives_a_verified_at_fallback_date_change() {
        // Arrange: an override with NO explicit verified_at -- the planner
        // stamps "today". Prove a rerun (which recomputes "today" again,
        // possibly a different day in a real crash-restart) is still
        // recognized as the same candidate, not a conflict.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(
            "[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { \
             min_prefix_tokens = 1024 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(1024),
                ..Default::default()
            },
        );
        let first = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("first plan");
        commit_overlay(&first, &overlay_path);

        // Manually age the stored verified_at, mimicking a rerun on a later
        // calendar day whose freshly-computed "today" would differ.
        let mut overlay = catalog_overlay::load(&overlay_path).expect("load");
        let expected_revision = overlay.revision;
        if let Some(Some(cell)) = overlay.cells.get_mut("openai-compat:grok-*") {
            cell.verified_at = "2020-01-01".to_string();
        }
        catalog_overlay::save(&overlay_path, expected_revision, overlay.cells.clone())
            .expect("re-save with aged date");

        // Act / Assert: re-planning does not conflict despite the stored
        // verified_at no longer matching what "today" would produce now.
        let rerun = plan_migration(&doc, 1, &cache_pricing, &overlay_path)
            .expect("rerun must not conflict on a verified_at-only difference");
        assert!(
            rerun.overlay_candidate().is_none(),
            "a verified_at-only difference is not a change"
        );
    }

    // -----------------------------------------------------------------------
    // Conflict: an existing overlay cell with a DIFFERENT value fails the
    // PLAN (before any write), leaving the overlay byte-untouched.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_conflict_with_different_existing_overlay_value_fails_closed() {
        // Arrange: the overlay already carries a DIFFERENT wm for this
        // selector (e.g. hand-edited, or from an unrelated prior write).
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let mut existing = BTreeMap::new();
        existing.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: Some(9.9),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        catalog_overlay::save(&overlay_path, 0, existing).expect("seed overlay");
        let overlay_before = std::fs::read(&overlay_path).unwrap();

        let doc = doc_of(
            "[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { \
             wm = 1.5 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // Act
        let err = plan_migration(&doc, 1, &cache_pricing, &overlay_path)
            .expect_err("conflicting value must fail the plan");

        // Assert: a plan-time conflict, and the overlay is byte-untouched.
        assert!(
            matches!(err, MigrateError::V1ToV2(MigrationError::Conflict(_))),
            "err: {err}"
        );
        assert_eq!(std::fs::read(&overlay_path).unwrap(), overlay_before);
    }

    #[test]
    fn plan_v1_conflict_with_disabled_existing_cell_fails_closed() {
        // Arrange: the overlay explicitly disables this selector (JSON
        // null) -- an operator's deliberate choice the migrator must not
        // silently overwrite with a value.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let mut existing = BTreeMap::new();
        existing.insert("openai-compat:grok-*".to_string(), None);
        catalog_overlay::save(&overlay_path, 0, existing).expect("seed overlay");

        let doc = doc_of("[cache_pricing]\n\"openai-compat:grok-*\" = { wm = 1.5 }\n");
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // Act / Assert
        let err = plan_migration(&doc, 1, &cache_pricing, &overlay_path)
            .expect_err("a disabled existing cell must conflict, not be overwritten");
        assert!(
            matches!(err, MigrateError::V1ToV2(MigrationError::Conflict(_))),
            "err: {err}"
        );

        let overlay = catalog_overlay::load(&overlay_path).unwrap();
        assert_eq!(
            overlay.cells.get("openai-compat:grok-*"),
            Some(&None),
            "the disabled cell must remain untouched"
        );
    }

    // -----------------------------------------------------------------------
    // Invalid input fails the plan before anything is written.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_invalid_override_fails_closed() {
        // Arrange: rm <= 0.0 is unconditionally rejected by
        // CachePricingOverride::validate.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("[cache_pricing]\n\"openai-compat:grok-*\" = { rm = 0.0 }\n");
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                rm: Some(0.0),
                ..Default::default()
            },
        );

        // Act
        let err = plan_migration(&doc, 1, &cache_pricing, &overlay_path)
            .expect_err("degenerate rm must fail closed");

        // Assert
        assert!(
            matches!(
                err,
                MigrateError::V1ToV2(MigrationError::InvalidOverride { .. })
            ),
            "err: {err}"
        );
        assert!(!overlay_path.exists(), "nothing should have been written");
    }

    #[test]
    fn plan_v1_malformed_selector_key_fails_closed() {
        // Arrange: a selector missing the required `:` separator.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("[cache_pricing]\n\"no-colon-here\" = { wm = 1.5 }\n");
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("no-colon-here".to_string(), override_with_wm(1.5));

        // Act / Assert
        let err = plan_migration(&doc, 1, &cache_pricing, &overlay_path)
            .expect_err("malformed selector must fail closed");
        assert!(
            matches!(
                err,
                MigrateError::V1ToV2(MigrationError::InvalidSelector { .. })
            ),
            "err: {err}"
        );
        assert!(!overlay_path.exists());
    }

    // -----------------------------------------------------------------------
    // A verify-only entry (no value fields, only verified_at) still lands
    // in the overlay candidate -- the provenance/staleness stamp the old
    // sidecar used to carry moves forward, not just economics overrides.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_verify_only_entry_lands_as_a_provenance_only_overlay_cell() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("[server]\nhost = \"127.0.0.1\"\n");
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                verified_at: Some("2026-06-30".to_string()),
                ..Default::default()
            },
        );

        // Act
        let plan = plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("plan");

        // Assert
        let ow = plan.overlay_candidate().expect("overlay candidate");
        let cell = ow
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.verified_at, "2026-06-30");
        assert_eq!(cell.wm, None);
        assert_eq!(cell.rm, None);
    }

    // -----------------------------------------------------------------------
    // cell_values_equal: ignores source/verified_at, compares value fields.
    // -----------------------------------------------------------------------

    #[test]
    fn cell_values_equal_ignores_source_and_verified_at() {
        let a = OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-01-01".to_string(),
            wm: Some(1.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        };
        let mut b = a.clone();
        b.source = OverlaySource::Import;
        b.verified_at = "2099-12-31".to_string();

        assert!(cell_values_equal(&a, &b));

        let mut c = a.clone();
        c.wm = Some(9.9);
        assert!(!cell_values_equal(&a, &c));
    }

    // -----------------------------------------------------------------------
    // apply_v1_to_v2_doc stamps the LITERAL 2, not the current-version
    // const -- pinned so a later bump of CURRENT_CONFIG_VERSION cannot make
    // the v1->v2 rung over-stamp a version it did not migrate to.
    // -----------------------------------------------------------------------

    #[test]
    fn apply_v1_to_v2_doc_stamps_the_literal_2_and_drops_cache_pricing() {
        let mut doc = doc_of(
            "version = 1\n[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\
             \"openai-compat:grok-*\" = { wm = 1.5 }\n",
        );

        apply_v1_to_v2_doc(&mut doc);

        assert_eq!(doc["version"].as_integer(), Some(2));
        assert!(!doc.to_string().contains("cache_pricing"));
    }

    // -----------------------------------------------------------------------
    // migrate_v2_to_v3: clean docs stamp version 3 and drop both keys,
    // comment/order preserving, across both [retry] shapes.
    // -----------------------------------------------------------------------

    #[test]
    fn v2_to_v3_clean_doc_stamps_3_and_preserves_comments_and_order() {
        let src = "# operator note: keep me\n\
                   version = 2\n\
                   \n\
                   [retry]\n\
                   max_attempts = 4 # tuned by hand\n\
                   retry_allowlist = []\n\
                   initial_backoff_ms = 250\n";
        let mut doc = src.parse::<DocumentMut>().unwrap();

        let outcome = migrate_v2_to_v3(&mut doc).expect("clean doc migrates");
        assert_eq!(outcome.from_version, 2);
        assert_eq!(outcome.to_version, 3);

        let out = doc.to_string();
        assert!(out.contains("version = 3"), "{out}");
        assert!(!out.contains("retry_allowlist"), "{out}");
        assert!(!out.contains("retry_denylist"), "{out}");
        assert!(out.contains("# operator note: keep me"), "{out}");
        assert!(out.contains("max_attempts = 4 # tuned by hand"), "{out}");
        // Surviving keys keep their relative order: max_attempts before
        // initial_backoff_ms, with the removed key gone from between them.
        let ma = out.find("max_attempts").unwrap();
        let ib = out.find("initial_backoff_ms").unwrap();
        assert!(ma < ib, "{out}");
        // Re-parses cleanly.
        out.parse::<DocumentMut>().expect("reparse");
    }

    #[test]
    fn v2_to_v3_clean_doc_with_no_retry_table_just_stamps_3() {
        let mut doc = "version = 2\n[server]\nhost = \"127.0.0.1\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        migrate_v2_to_v3(&mut doc).expect("no retry table is clean");
        assert_eq!(doc["version"].as_integer(), Some(3));
    }

    #[test]
    fn v2_to_v3_removes_empty_lists_from_standard_table_shape() {
        let mut doc = "version = 2\n\n[retry]\nretry_allowlist = []\nretry_denylist = []\n"
            .parse::<DocumentMut>()
            .unwrap();
        migrate_v2_to_v3(&mut doc).expect("empty lists are clean");
        let out = doc.to_string();
        assert!(!out.contains("retry_allowlist"), "{out}");
        assert!(!out.contains("retry_denylist"), "{out}");
        assert!(out.contains("version = 3"), "{out}");
    }

    #[test]
    fn v2_to_v3_removes_empty_list_from_inline_table_shape() {
        // A plain Table-typed walk silently no-ops on the inline shape; the
        // TableLike path must actually drop the key here.
        let mut doc = "version = 2\nretry = { max_attempts = 3, retry_allowlist = [] }\n"
            .parse::<DocumentMut>()
            .unwrap();
        migrate_v2_to_v3(&mut doc).expect("inline empty list is clean");
        let out = doc.to_string();
        assert!(!out.contains("retry_allowlist"), "{out}");
        assert!(out.contains("max_attempts = 3"), "{out}");
        assert!(out.contains("version = 3"), "{out}");
        out.parse::<DocumentMut>().expect("reparse");
    }

    // -----------------------------------------------------------------------
    // migrate_v2_to_v3: behavior-bearing lists REFUSE with no mutation, name
    // the codes and the nearest [retry.classes.<token>] mapping.
    // -----------------------------------------------------------------------

    #[test]
    fn v2_to_v3_non_empty_allowlist_refuses_and_leaves_doc_untouched() {
        let src = "version = 2\n\n[retry]\nretry_allowlist = [503, 500]\n";
        let mut doc = src.parse::<DocumentMut>().unwrap();
        let before = doc.to_string();

        let refusal = migrate_v2_to_v3(&mut doc).expect_err("behavior-bearing allowlist refuses");
        let Refusal::BehaviorBearing { source, codes, .. } = &refusal else {
            panic!("expected BehaviorBearing, got {refusal:?}");
        };
        assert_eq!(*source, RefusalSource::Allowlist);
        assert_eq!(*codes, vec![503, 500]);
        // Doc byte-untouched.
        assert_eq!(doc.to_string(), before);

        let msg = refusal.to_string();
        assert!(msg.contains("503"), "{msg}");
        // 503 -> server-error primary, overloaded alternative; 500 ->
        // server-error, unambiguous.
        assert!(msg.contains("[retry.classes.server-error]"), "{msg}");
        assert!(msg.contains("overloaded"), "{msg}");
        assert!(msg.contains("fallback = true"), "{msg}");
    }

    #[test]
    fn v2_to_v3_denylist_with_content_refuses_with_fallback_false() {
        let src = "version = 2\nretry = { retry_denylist = [400] }\n";
        let mut doc = src.parse::<DocumentMut>().unwrap();
        let before = doc.to_string();

        let refusal = migrate_v2_to_v3(&mut doc).expect_err("denylist with content refuses");
        let Refusal::BehaviorBearing { source, codes, .. } = &refusal else {
            panic!("expected BehaviorBearing, got {refusal:?}");
        };
        assert_eq!(*source, RefusalSource::Denylist);
        assert_eq!(*codes, vec![400]);
        assert_eq!(doc.to_string(), before);

        let msg = refusal.to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("fallback = false"), "{msg}");
    }

    #[test]
    fn v2_to_v3_both_lists_present_refuses_as_both() {
        let src = "version = 2\n\n[retry]\nretry_allowlist = [503]\nretry_denylist = [400]\n";
        let mut doc = src.parse::<DocumentMut>().unwrap();
        let before = doc.to_string();

        let refusal = migrate_v2_to_v3(&mut doc).expect_err("both lists refuse");
        let Refusal::BehaviorBearing { source, codes, .. } = &refusal else {
            panic!("expected BehaviorBearing, got {refusal:?}");
        };
        assert_eq!(*source, RefusalSource::Both);
        assert_eq!(*codes, vec![503, 400]);
        assert_eq!(doc.to_string(), before);
    }

    #[test]
    fn guidance_for_out_of_taxonomy_status_renders_prose_not_placeholder_key() {
        // Arrange: 999 is a valid u16 (so it is not Malformed) but has no
        // stable failure class.
        let out_of_taxonomy = 999u16;

        // Act
        let line = guidance_for_code(out_of_taxonomy, true);

        // Assert: prose guidance, never a bracketed pseudo-key.
        assert!(line.contains("has no failure class"), "{line}");
        assert!(line.contains("classify by hand"), "{line}");
        assert!(!line.contains("[retry.classes."), "{line}");
        assert!(!line.contains("(no stable class"), "{line}");
    }

    // -----------------------------------------------------------------------
    // migrate_v2_to_v3: a malformed retry-list entry (not a valid u16 status)
    // REFUSES fail-closed with no mutation rather than silently dropping the
    // entry (which would strip the key and change behavior). Covered for a
    // non-integer, an out-of-range integer, and a valid-but-weird float,
    // each against both the allowlist and the denylist.
    // -----------------------------------------------------------------------

    /// Assert that `[retry]` with `key = [<entry>]` refuses as Malformed,
    /// names `<needle>` in the message, and leaves the doc byte-untouched.
    fn assert_malformed_refusal(key: &str, entry: &str, needle: &str, want_source: RefusalSource) {
        let src = format!("version = 2\n\n[retry]\n{key} = [{entry}]\n");
        let mut doc = src.parse::<DocumentMut>().unwrap();
        let before = doc.to_string();

        let refusal =
            migrate_v2_to_v3(&mut doc).expect_err("malformed retry-list entry must refuse");
        let Refusal::Malformed { source, entries } = &refusal else {
            panic!("expected Malformed, got {refusal:?}");
        };
        assert_eq!(*source, want_source, "source for {key}={entry}");
        assert!(
            entries.iter().any(|e| e.contains(needle)),
            "entries {entries:?} must name `{needle}`"
        );
        // Doc byte-untouched: no version bump, keys intact.
        assert_eq!(doc.to_string(), before, "doc must be byte-untouched");

        let msg = refusal.to_string();
        assert!(msg.contains(needle), "message must name `{needle}`: {msg}");
        assert!(msg.contains("not a valid HTTP status"), "{msg}");
    }

    #[test]
    fn v2_to_v3_non_integer_allowlist_entry_refuses_byte_untouched() {
        assert_malformed_refusal(
            "retry_allowlist",
            "\"not-a-number\"",
            "not-a-number",
            RefusalSource::Allowlist,
        );
    }

    #[test]
    fn v2_to_v3_non_integer_denylist_entry_refuses_byte_untouched() {
        assert_malformed_refusal(
            "retry_denylist",
            "\"not-a-number\"",
            "not-a-number",
            RefusalSource::Denylist,
        );
    }

    #[test]
    fn v2_to_v3_out_of_range_allowlist_entry_refuses_byte_untouched() {
        assert_malformed_refusal(
            "retry_allowlist",
            "99999",
            "99999",
            RefusalSource::Allowlist,
        );
    }

    #[test]
    fn v2_to_v3_out_of_range_denylist_entry_refuses_byte_untouched() {
        assert_malformed_refusal("retry_denylist", "99999", "99999", RefusalSource::Denylist);
    }

    #[test]
    fn v2_to_v3_float_allowlist_entry_refuses_byte_untouched() {
        assert_malformed_refusal(
            "retry_allowlist",
            "200.5",
            "200.5",
            RefusalSource::Allowlist,
        );
    }

    #[test]
    fn v2_to_v3_float_denylist_entry_refuses_byte_untouched() {
        assert_malformed_refusal("retry_denylist", "200.5", "200.5", RefusalSource::Denylist);
    }

    // -----------------------------------------------------------------------
    // plan_migration ladder. These deliberately assert LITERAL target
    // versions, never CURRENT_CONFIG_VERSION, so they hold under both the
    // pre- and post- const-bump tree state. Planning is PURE: no test here
    // observes a disk write from plan_migration.
    // -----------------------------------------------------------------------

    #[test]
    fn plan_v1_doc_chains_to_latest_folding_cache_pricing_and_dropping_lists() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(
            "version = 1\n\n\
             [cache_pricing]\n\
             \"openai-compat:grok-*\" = { wm = 1.5, override_acknowledges_cost_risk = true }\n\
             \n\
             [retry]\n\
             max_attempts = 3\n\
             retry_allowlist = []\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        let plan =
            plan_migration(&doc, 1, &cache_pricing, &overlay_path).expect("v1 chains to latest");
        assert_eq!(plan.from, 1);
        assert_eq!(plan.to, LATEST_MIGRATION_VERSION);
        assert!(matches!(plan.write_kind, WriteKind::ConfigAndOverlay(..)));
        // One step per rung between v1 and the latest, each stamping its own
        // literal target -- so this holds as the ladder grows.
        assert_eq!(
            plan.steps,
            (1..LATEST_MIGRATION_VERSION)
                .map(|from_version| StepOutcome {
                    from_version,
                    to_version: from_version + 1,
                })
                .collect::<Vec<_>>()
        );

        // Config candidate is fully migrated, cache_pricing folded away,
        // retry list dropped -- and the overlay candidate carries the cell.
        let out = plan.config_candidate().expect("candidate");
        assert!(
            out.contains(&format!("version = {LATEST_MIGRATION_VERSION}")),
            "{out}"
        );
        assert!(!out.contains("cache_pricing"), "{out}");
        assert!(!out.contains("retry_allowlist"), "{out}");
        assert!(
            plan.overlay_candidate()
                .expect("overlay candidate")
                .cells
                .contains_key("openai-compat:grok-*")
        );

        // Planning is pure: the overlay file was not created.
        assert!(
            !overlay_path.exists(),
            "planning must not write the overlay"
        );
    }

    #[test]
    fn plan_v2_doc_migrates_to_latest_config_only() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("version = 2\n[retry]\nretry_allowlist = []\n");

        let plan = plan_migration(&doc, 2, &BTreeMap::new(), &overlay_path).expect("v2 -> latest");
        assert_eq!(
            plan.steps,
            (2..LATEST_MIGRATION_VERSION)
                .map(|from_version| StepOutcome {
                    from_version,
                    to_version: from_version + 1,
                })
                .collect::<Vec<_>>()
        );
        assert!(matches!(plan.write_kind, WriteKind::ConfigOnly(_)));
        assert!(plan.overlay_candidate().is_none());
        let out = plan.config_candidate().expect("candidate");
        assert!(
            out.contains(&format!("version = {LATEST_MIGRATION_VERSION}")),
            "{out}"
        );
    }

    #[test]
    fn plan_already_latest_doc_is_a_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(&format!("version = {LATEST_MIGRATION_VERSION}\n"));

        let plan = plan_migration(
            &doc,
            LATEST_MIGRATION_VERSION,
            &BTreeMap::new(),
            &overlay_path,
        )
        .expect("no-op");
        assert!(plan.steps.is_empty());
        assert_eq!(plan.write_kind, WriteKind::NoChange);
        assert!(plan.config_candidate().is_none());
    }

    #[test]
    fn plan_future_version_is_too_new() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("version = 9\n");

        let err = plan_migration(
            &doc,
            LATEST_MIGRATION_VERSION + 1,
            &BTreeMap::new(),
            &overlay_path,
        )
        .expect_err("future version is too new");
        assert!(
            matches!(err, MigrateError::VersionTooNew { found, supported }
                if found == LATEST_MIGRATION_VERSION + 1 && supported == LATEST_MIGRATION_VERSION),
            "err: {err}"
        );
    }

    #[test]
    fn plan_v2_doc_with_behavior_bearing_list_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("version = 2\n[retry]\nretry_allowlist = [503]\n");

        let err = plan_migration(&doc, 2, &BTreeMap::new(), &overlay_path)
            .expect_err("behavior-bearing list refuses");
        assert!(matches!(err, MigrateError::Refused(_)), "err: {err}");
    }

    #[test]
    fn apply_config_transforms_chains_v1_to_latest_on_the_document() {
        let mut doc = doc_of(
            "version = 1\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { wm = 1.5 }\n\n\
             [retry]\nretry_allowlist = []\n",
        );

        let steps = apply_config_transforms(&mut doc, 1).expect("chains");

        assert_eq!(
            steps.len(),
            usize::try_from(LATEST_MIGRATION_VERSION - 1).unwrap()
        );
        let out = doc.to_string();
        assert!(
            out.contains(&format!("version = {LATEST_MIGRATION_VERSION}")),
            "{out}"
        );
        assert!(!out.contains("cache_pricing"), "{out}");
        assert!(!out.contains("retry_allowlist"), "{out}");
    }

    // -----------------------------------------------------------------------
    // v4 -> v5: provider / model `unsupported_features` fold into
    // `[capability.overrides]`, empty egress allowlists (and an emptied
    // `[bedrock]`) retire, and every refusal leaves the doc untouched.
    // -----------------------------------------------------------------------

    const V4_PROVIDER_MODEL: &str = "\
version = 4\n\
\n\
[providers.fast]\n\
kind = \"openai-compat\"\n\
base_url = \"https://x\"\n\
api_key_ref = \"literal:k\"\n\
unsupported_features = [\"web_search\"]\n\
\n\
[models.gpt]\n\
provider = \"fast\"\n\
upstream = \"gpt-4o\"\n\
unsupported_features = [\"computer_use\"]\n";

    /// A fixture carrying the provider entry the allowlist rows hang off.
    const V4_ANTHROPIC_PROVIDER: &str = "\
version = 4\n\
\n\
[providers.a]\n\
kind = \"anthropic-api\"\n\
base_url = \"https://x\"\n\
api_key_ref = \"literal:k\"\n";

    #[test]
    fn v4_to_v5_folds_provider_and_model_lists_and_stamps_five() {
        // Arrange
        let mut doc = doc_of(V4_PROVIDER_MODEL);

        // Act
        let step = migrate_v4_to_v5(&mut doc).expect("no allowlist, no conflict -> folds");

        // Assert
        assert_eq!(
            step,
            StepOutcome {
                from_version: 4,
                to_version: 5
            }
        );
        let out = doc.to_string();
        assert!(out.contains("version = 5"), "{out}");
        assert!(!out.contains("unsupported_features"), "{out}");
        assert!(out.contains("[capability.overrides.fast]"), "{out}");
        assert!(out.contains("[capability.overrides.\"fast:gpt\"]"), "{out}");
        let reparsed = doc_of(&out);
        let overrides = &reparsed["capability"]["overrides"];
        assert_eq!(
            overrides["fast"]["unsupported"].as_array().map(Array::len),
            Some(1)
        );
        assert_eq!(
            overrides["fast"]["unsupported"][0].as_str(),
            Some("web_search")
        );
        assert_eq!(
            overrides["fast:gpt"]["unsupported"][0].as_str(),
            Some("computer_use")
        );
    }

    #[test]
    fn v4_to_v5_preserves_route_away_verdicts_for_every_folded_cell() {
        use crate::override_registry::{OverrideRegistry, OverrideVerdict};

        // Arrange: the v4 input routes `web_search` away on provider `fast`
        // and `computer_use` away on its model `gpt`.
        let mut doc = doc_of(V4_PROVIDER_MODEL);

        // Act
        migrate_v4_to_v5(&mut doc).expect("folds");
        let after: crate::config::Config =
            toml::from_str(&doc.to_string()).expect("migrated config parses");
        let registry = OverrideRegistry::build(&after);

        // Assert: each legacy list's verdict survives at its own scope.
        let route_away = Some(OverrideVerdict::RouteAway);
        for (nickname, capability, expected) in [
            ("gpt", "web_search", route_away),
            ("gpt", "computer_use", route_away),
            ("other", "web_search", route_away),
            ("other", "computer_use", None),
        ] {
            assert_eq!(
                registry.resolve("fast", nickname, capability, "openai-compat"),
                expected,
                "fast:{nickname} {capability}"
            );
        }
    }

    #[test]
    fn v4_to_v5_plain_file_only_stamps_the_version() {
        let src = "# keep me\nversion = 4\n\n[server]\nhost = \"127.0.0.1\" # loopback\n";
        let mut doc = doc_of(src);

        migrate_v4_to_v5(&mut doc).expect("stamps");

        assert_eq!(
            doc.to_string(),
            src.replacen("version = 4", "version = 5", 1)
        );
    }

    #[test]
    fn v4_to_v5_drops_empty_allowlists_and_the_emptied_bedrock_table() {
        // Arrange
        let src = format!(
            "{V4_ANTHROPIC_PROVIDER}allowed_betas = []\n\n\
             [bedrock]\nallowed_betas = []\nallowed_body_fields = []\n"
        );
        let mut doc = doc_of(&src);

        // Act
        migrate_v4_to_v5(&mut doc).expect("empty allowlists retire");

        // Assert
        let out = doc.to_string();
        assert!(!out.contains("allowed_betas"), "{out}");
        assert!(!out.contains("allowed_body_fields"), "{out}");
        assert!(!out.contains("bedrock"), "{out}");
        assert!(out.contains("[providers.a]"), "{out}");
        let config: crate::config::Config =
            toml::from_str(&out).expect("the v5 output parses as config");
        assert_eq!(config.version, 5);
    }

    #[test]
    fn v4_to_v5_keeps_a_bedrock_table_that_still_holds_another_key() {
        let mut doc = doc_of("version = 4\n[bedrock]\nallowed_betas = []\nother = 1\n");

        migrate_v4_to_v5(&mut doc).expect("empty list retires");

        let out = doc.to_string();
        assert!(!out.contains("allowed_betas"), "{out}");
        assert!(out.contains("[bedrock]\nother = 1"), "{out}");
    }

    #[test]
    fn v4_to_v5_refuses_every_non_empty_allowlist_untouched_with_count_not_values() {
        const CANARY: &str = "canary-flag-2099-01-01";
        let cases = [
            (
                "bedrock.allowed_betas",
                format!("version = 4\n[bedrock]\nallowed_betas = [\"{CANARY}\", \"b\"]\n"),
                "bedrock.allowed_betas (2 entries)",
            ),
            (
                "bedrock.allowed_body_fields",
                format!("version = 4\n[bedrock]\nallowed_body_fields = [\"{CANARY}\"]\n"),
                "bedrock.allowed_body_fields (1 entry)",
            ),
            (
                "providers.a.allowed_betas",
                format!("{V4_ANTHROPIC_PROVIDER}allowed_betas = [\"{CANARY}\"]\n"),
                "providers.a.allowed_betas (1 entry)",
            ),
        ];
        for (row, src, expected_line) in cases {
            // Arrange
            let mut doc = doc_of(&src);

            // Act
            let refusal = migrate_v4_to_v5(&mut doc).expect_err(row);

            // Assert
            assert_eq!(
                doc.to_string(),
                src,
                "{row}: the doc must stay byte-identical"
            );
            let Refusal::EgressAllowlist { allowlists } = &refusal else {
                panic!("{row}: expected EgressAllowlist, got {refusal:?}");
            };
            assert_eq!(allowlists.len(), 1, "{row}");
            assert_eq!(allowlists[0].path, row);
            let text = refusal.to_string();
            assert!(text.contains(expected_line), "{row}: {text}");
            assert!(
                !text.contains(CANARY),
                "{row}: values must never render: {text}"
            );
            assert!(
                text.contains("unsupported = [\"beta:<flag>\"]"),
                "{row}: the successor must be named: {text}"
            );
            assert!(
                text.contains("[capability.overrides.<provider>]"),
                "{row}: {text}"
            );
            assert!(text.contains("has no successor"), "{row}: {text}");
        }
    }

    #[test]
    fn v4_to_v5_reports_every_non_empty_allowlist_in_order() {
        let src = format!(
            "{V4_ANTHROPIC_PROVIDER}allowed_betas = [\"x\"]\n\n\
             [bedrock]\nallowed_betas = [\"y\"]\nallowed_body_fields = [\"z\"]\n"
        );
        let mut doc = doc_of(&src);

        let refusal = migrate_v4_to_v5(&mut doc).expect_err("three lists refuse");

        let Refusal::EgressAllowlist { allowlists } = &refusal else {
            panic!("expected EgressAllowlist, got {refusal:?}");
        };
        let paths: Vec<&str> = allowlists.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "bedrock.allowed_betas",
                "bedrock.allowed_body_fields",
                "providers.a.allowed_betas"
            ]
        );
    }

    /// The lines a refusal names an operator-written key or value in.
    fn refusal_item_lines(refusal: &Refusal) -> Vec<String> {
        match refusal {
            Refusal::EgressAllowlist { allowlists } => {
                allowlists.iter().map(|a| a.path.clone()).collect()
            }
            Refusal::CapabilityConflict { cells } => cells.clone(),
            Refusal::OverrideShape { paths } => paths.clone(),
            other => panic!("unexpected refusal {other:?}"),
        }
    }

    #[test]
    fn v4_to_v5_refusal_text_filters_control_characters_from_key_names() {
        // A quoted TOML key carrying ESC, CR and LF, spelled as TOML escapes.
        const KEY: &str = "f\\u001b[31m\\r\\n";
        const RENDERED: &str = "f?[31m??";
        let provider = format!(
            "version = 4\n[providers.\"{KEY}\"]\nkind = \"openai-compat\"\n\
             base_url = \"https://x\"\napi_key_ref = \"literal:k\"\n"
        );
        let cases = [
            (
                "allowlist path",
                format!("{provider}allowed_betas = [\"x\"]\n"),
                format!("providers.{RENDERED}.allowed_betas"),
            ),
            (
                "conflict cell and value",
                format!(
                    "{provider}unsupported_features = [\"web\\u001b\\r\\n\"]\n\
                     [capability.overrides.\"{KEY}\"]\n\
                     force_supported = [\"web\\u001b\\r\\n\"]\n"
                ),
                format!(
                    "[capability.overrides.{RENDERED}] `web???`: in force_supported, and in \
                     the retired [providers.{RENDERED}].unsupported_features"
                ),
            ),
            (
                "override cell path",
                format!(
                    "{provider}unsupported_features = [\"web_search\"]\n\
                     [capability.overrides]\n\"{KEY}\" = 1\n"
                ),
                format!("capability.overrides.{RENDERED} (must be a table)"),
            ),
        ];

        for (row, src, expected) in cases {
            // Arrange
            let mut doc = doc_of(&src);

            // Act
            let refusal = migrate_v4_to_v5(&mut doc).expect_err(row);
            let message = refusal.to_string();

            // Assert
            assert_eq!(refusal_item_lines(&refusal), [expected.as_str()], "{row}");
            for byte in ['\u{1b}', '\r'] {
                assert!(!message.contains(byte), "{row}: {byte:?} in {message:?}");
            }
            let item = format!("  - {expected}");
            assert!(
                message.lines().any(|line| line.starts_with(&item)),
                "{row}: the item line is not whole in {message:?}"
            );
        }
    }

    #[test]
    fn removed_keys_summary_filters_control_characters_from_key_names() {
        // A quoted TOML key carrying ESC, CR and LF, spelled as TOML escapes.
        const KEY: &str = "f\\u001b[31m\\r\\n";
        const RENDERED: &str = "f?[31m??";
        let provider = format!("[providers.\"{KEY}\"]\nkind = \"openai-compat\"\n");
        let cases = [
            (
                "provider unsupported_features",
                4,
                format!("version = 4\n{provider}unsupported_features = [\"x\"]\n"),
                format!(
                    "[providers.{RENDERED}].unsupported_features (folded into \
                     [capability.overrides.{RENDERED}].unsupported)"
                ),
            ),
            (
                "model nick and provider",
                4,
                format!(
                    "version = 4\n[models.\"{KEY}\"]\nprovider = \"{KEY}\"\n\
                     unsupported_features = [\"x\"]\n"
                ),
                format!(
                    "[models.{RENDERED}].unsupported_features (folded into \
                     [capability.overrides.\"{RENDERED}:{RENDERED}\"].unsupported)"
                ),
            ),
            (
                "model nick without provider",
                4,
                format!("version = 4\n[models.\"{KEY}\"]\nunsupported_features = [\"x\"]\n"),
                format!("[models.{RENDERED}].unsupported_features"),
            ),
            (
                "provider empty allowlist",
                4,
                format!("version = 4\n{provider}allowed_betas = []\n"),
                format!("providers.{RENDERED}.allowed_betas (empty; retired)"),
            ),
            (
                "provider seat_selection",
                3,
                format!("version = 3\n{provider}seat_selection = \"round-robin\"\n"),
                format!(
                    "[providers.{RENDERED}].seat_selection (relocated onto the pool block \
                     that groups the accounts)"
                ),
            ),
        ];

        for (row, raw_version, src, expected) in cases {
            // Arrange
            let doc = doc_of(&src);

            // Act
            let removed = collect_removed_keys(&doc, raw_version);

            // Assert
            assert_eq!(removed, [expected.as_str()], "{row}");
        }
    }

    /// A v3 file whose v3 -> v4 rung succeeds (it relocates
    /// `seat_selection`) and whose v4 -> v5 fold then conflicts with a
    /// `force_supported` entry on the same cell.
    const V3_LADDER_CONFLICT: &str = "\
version = 3

[providers.anthropic-managed]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"
seat_selection = \"round-robin\"
unsupported_features = [\"computer_use\"]

[capability.overrides.anthropic-managed]
force_supported = [\"computer_use\"]
";

    #[test]
    fn a_fold_onto_a_force_supported_cell_refuses_with_the_doc_untouched() {
        let cases = [
            (
                "provider-scoped",
                4,
                format!(
                    "{V4_PROVIDER_MODEL}\n[capability.overrides.fast]\n\
                     force_supported = [\"web_search\"]\n"
                ),
                "[capability.overrides.fast] `web_search`",
            ),
            (
                "model-scoped",
                4,
                format!(
                    "{V4_PROVIDER_MODEL}\n[capability.overrides.\"fast:gpt\"]\n\
                     force_supported = [\"computer_use\"]\n"
                ),
                "[capability.overrides.\"fast:gpt\"] `computer_use`",
            ),
            (
                "v3 ladder, refused at the v4 rung",
                3,
                V3_LADDER_CONFLICT.to_string(),
                "[capability.overrides.anthropic-managed] `computer_use`",
            ),
        ];
        for (row, raw_version, src, expected_cell) in cases {
            // Arrange
            let mut doc = doc_of(&src);

            // Act
            let refusal = apply_config_transforms(&mut doc, raw_version).expect_err(row);

            // Assert
            assert_eq!(
                doc.to_string(),
                src,
                "{row}: the doc must stay byte-identical"
            );
            let Refusal::CapabilityConflict { cells } = &refusal else {
                panic!("{row}: expected CapabilityConflict, got {refusal:?}");
            };
            assert_eq!(cells.len(), 1, "{row}: {cells:?}");
            assert!(cells[0].starts_with(expected_cell), "{row}: {cells:?}");
        }
    }

    #[test]
    fn v4_to_v5_refuses_a_fold_onto_a_mis_shaped_override_untouched() {
        const CANARY: &str = "canary-capability-2099";
        let cases = [
            (
                "capability is not a table",
                V4_PROVIDER_MODEL.replacen("version = 4\n", "version = 4\ncapability = 1\n", 1),
                "capability (must be a table)",
            ),
            (
                "overrides is not a table",
                format!("{V4_PROVIDER_MODEL}\n[capability]\noverrides = \"{CANARY}\"\n"),
                "capability.overrides (must be a table)",
            ),
            (
                "the cell is not a table",
                format!("{V4_PROVIDER_MODEL}\n[capability.overrides]\nfast = \"{CANARY}\"\n"),
                "capability.overrides.fast (must be a table)",
            ),
            (
                "the cell's unsupported is not an array",
                format!(
                    "{V4_PROVIDER_MODEL}\n[capability.overrides.fast]\n\
                     unsupported = \"{CANARY}\"\n"
                ),
                "capability.overrides.fast.unsupported (must be an array)",
            ),
            (
                "a model cell's unsupported is not an array",
                format!(
                    "{V4_PROVIDER_MODEL}\n[capability.overrides.\"fast:gpt\"]\n\
                     unsupported = \"{CANARY}\"\n"
                ),
                "capability.overrides.\"fast:gpt\".unsupported (must be an array)",
            ),
        ];
        for (row, src, expected_path) in cases {
            // Arrange
            let mut doc = doc_of(&src);

            // Act
            let refusal = migrate_v4_to_v5(&mut doc).expect_err(row);

            // Assert
            assert_eq!(
                doc.to_string(),
                src,
                "{row}: the doc must stay byte-identical"
            );
            let Refusal::OverrideShape { paths } = &refusal else {
                panic!("{row}: expected OverrideShape, got {refusal:?}");
            };
            assert_eq!(paths, &[expected_path.to_string()], "{row}");
            let text = refusal.to_string();
            assert!(text.contains(expected_path), "{row}: {text}");
            assert!(
                !text.contains(CANARY),
                "{row}: values must never render: {text}"
            );
        }
    }

    #[test]
    fn mis_shaped_override_destinations_refuse_at_the_fold_itself() {
        let cases = [
            (
                "capability",
                "capability = 1\n",
                "capability (must be a table)",
            ),
            (
                "overrides",
                "[capability]\noverrides = \"x\"\n",
                "capability.overrides (must be a table)",
            ),
            (
                "cell",
                "[capability.overrides]\nfast = 1\n",
                "capability.overrides.fast (must be a table)",
            ),
            (
                "unsupported",
                "[capability.overrides.fast]\nunsupported = \"x\"\n",
                "capability.overrides.fast.unsupported (must be an array)",
            ),
        ];
        for (row, src, expected_path) in cases {
            let mut doc = doc_of(src);
            let values = [Value::from("web_search")];

            let refusal = append_unsupported_override(&mut doc, "fast", &values).expect_err(row);

            assert_eq!(
                refusal,
                Refusal::OverrideShape {
                    paths: vec![expected_path.to_string()]
                },
                "{row}"
            );
        }
    }

    #[test]
    fn v4_to_v5_merges_into_an_existing_override_without_duplicating() {
        let src = "version = 4\n\n[providers.fast]\nkind = \"openai-compat\"\n\
                   base_url = \"https://x\"\napi_key_ref = \"literal:k\"\n\
                   unsupported_features = [\"web_search\", \"computer_use\"]\n\n\
                   [capability.overrides.fast]\nunsupported = [\"web_search\"]\n";
        let mut doc = doc_of(src);

        migrate_v4_to_v5(&mut doc).expect("folds");

        let out = doc.to_string();
        assert!(!out.contains("unsupported_features"), "{out}");
        assert_eq!(out.matches("web_search").count(), 1, "{out}");
        assert!(out.contains("computer_use"), "{out}");
    }

    #[test]
    fn v4_to_v5_preserves_comments_and_unrelated_content() {
        let src = "# operator note: keep me\nversion = 4\n\n\
                   [server]\nhost = \"127.0.0.1\" # loopback\n\n\
                   [providers.fast]\nkind = \"openai-compat\"\nbase_url = \"https://x\"\n\
                   api_key_ref = \"literal:k\"\nunsupported_features = [\"web_search\"]\n";
        let mut doc = doc_of(src);

        migrate_v4_to_v5(&mut doc).expect("folds");

        let out = doc.to_string();
        assert!(out.contains("# operator note: keep me"), "{out}");
        assert!(out.contains("host = \"127.0.0.1\" # loopback"), "{out}");
    }

    #[test]
    fn v4_to_v5_removes_a_present_but_empty_list_with_no_override() {
        let src = "version = 4\n\n[providers.fast]\nkind = \"openai-compat\"\n\
                   base_url = \"https://x\"\napi_key_ref = \"literal:k\"\n\
                   unsupported_features = []\n";
        let mut doc = doc_of(src);

        migrate_v4_to_v5(&mut doc).expect("empty list retires");

        let out = doc.to_string();
        assert!(!out.contains("unsupported_features"), "{out}");
        assert!(!out.contains("[capability.overrides"), "{out}");
    }

    // -----------------------------------------------------------------------
    // Ladder + report: a v3 file reaches the fold through v4, the report
    // names every folded / removed key in sorted order for any input <= 4,
    // and a v5 file is already current.
    // -----------------------------------------------------------------------

    /// A v3 file carrying both the v3 -> v4 input (`seat_selection`) and the
    /// v4 -> v5 input (`unsupported_features`, an empty allowlist).
    const V3_SEAT_SELECTION_AND_UNSUPPORTED: &str = "\
version = 3

[bedrock]
allowed_betas = []

[providers.anthropic-managed]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"
seat_selection = \"round-robin\"
unsupported_features = [\"web_search\"]

[models.opus]
provider = \"anthropic-managed\"
upstream = \"claude-opus-4-8\"
unsupported_features = [\"computer_use\"]

[aliases]
default = \"opus\"
";

    #[test]
    fn plan_v3_ladders_through_v4_to_v5_with_the_fold_applied() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of(V3_SEAT_SELECTION_AND_UNSUPPORTED);

        // Act
        let plan = plan_migration(&doc, 3, &BTreeMap::new(), &overlay_path).expect("v3 -> v5");

        // Assert
        assert_eq!(
            plan.steps,
            vec![
                StepOutcome {
                    from_version: 3,
                    to_version: 4
                },
                StepOutcome {
                    from_version: 4,
                    to_version: 5
                },
            ]
        );
        assert_eq!(plan.to, 5);
        let out = plan.config_candidate().expect("candidate");
        assert!(out.contains("version = 5"), "{out}");
        assert!(out.contains("[pools.anthropic]"), "{out}");
        assert!(!out.contains("unsupported_features"), "{out}");
        assert!(!out.contains("bedrock"), "{out}");
        assert!(
            out.contains("[capability.overrides.anthropic-managed]"),
            "{out}"
        );
        assert!(
            out.contains("[capability.overrides.\"anthropic-managed:opus\"]"),
            "{out}"
        );
        let config: crate::config::Config =
            toml::from_str(out).expect("the v5 candidate parses as config");
        assert_eq!(config.version, 5);
    }

    #[test]
    fn removed_keys_list_every_fold_and_drop_sorted_for_v3_and_v4_inputs() {
        let v4 = V3_SEAT_SELECTION_AND_UNSUPPORTED
            .replacen("version = 3", "version = 4", 1)
            .replace("seat_selection = \"round-robin\"\n", "");
        for (raw_version, src) in [(3, V3_SEAT_SELECTION_AND_UNSUPPORTED.to_string()), (4, v4)] {
            let removed = collect_removed_keys(&doc_of(&src), raw_version);

            for expected in [
                "[bedrock] (empty once its retired lists are removed)",
                "bedrock.allowed_betas (empty; retired)",
                "[models.opus].unsupported_features (folded into \
                 [capability.overrides.\"anthropic-managed:opus\"].unsupported)",
                "[providers.anthropic-managed].unsupported_features (folded into \
                 [capability.overrides.anthropic-managed].unsupported)",
            ] {
                assert!(
                    removed.iter().any(|k| k == expected),
                    "v{raw_version}: missing `{expected}` in {removed:?}"
                );
            }
            let mut sorted = removed.clone();
            sorted.sort();
            assert_eq!(removed, sorted, "v{raw_version}: the report must be sorted");
        }
    }

    #[test]
    fn plan_v5_doc_is_already_current() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("version = 5\n[server]\nhost = \"127.0.0.1\"\n");

        let plan = plan_migration(&doc, 5, &BTreeMap::new(), &overlay_path).expect("no-op");

        assert!(plan.steps.is_empty());
        assert_eq!(plan.write_kind, WriteKind::NoChange);
        assert!(plan.removed_keys.is_empty(), "{:?}", plan.removed_keys);
    }

    #[test]
    fn plan_v4_egress_allowlist_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let doc = doc_of("version = 4\n[bedrock]\nallowed_body_fields = [\"messages\"]\n");

        let err = plan_migration(&doc, 4, &BTreeMap::new(), &overlay_path)
            .expect_err("egress allowlist refuses");

        assert!(
            matches!(err, MigrateError::Refused(Refusal::EgressAllowlist { .. })),
            "err: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // v3 -> v4: the provider-level `seat_selection` knob relocates onto a
    // `[pools.<name>]` block. Offline (no store read), format-preserving, and
    // fail-closed on any namespace collision.
    // -----------------------------------------------------------------------

    /// A v3 provider entry with a bare `oauth://` ref and the retired
    /// provider-level knob, plus the model/alias that make it a valid config.
    const V3_WITH_PROVIDER_SEAT_SELECTION: &str = "\
# operator note: keep me
version = 3

[providers.anthropic-managed]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"
seat_selection = \"round-robin\" # spread the load

[models.opus]
provider = \"anthropic-managed\"
upstream = \"claude-opus-4-8\"

[aliases]
default = \"opus\"
";

    #[test]
    fn v3_to_v4_relocates_provider_seat_selection_onto_a_pool_block() {
        // Arrange
        let mut doc = doc_of(V3_WITH_PROVIDER_SEAT_SELECTION);

        // Act
        let step = migrate_v3_to_v4(&mut doc).expect("relocation plans and applies");

        // Assert: the literal rung target, the knob moved, the pool names the
        // provider family, and the entry is its sole member.
        assert_eq!(
            step,
            StepOutcome {
                from_version: 3,
                to_version: 4
            }
        );
        let out = doc.to_string();
        assert!(out.contains("version = 4"), "{out}");
        assert!(
            !out.contains("[providers.anthropic-managed]\nkind")
                || !providers_carry_seat_selection(&doc),
            "the provider-level knob must be gone: {out}"
        );
        assert!(out.contains("[pools.anthropic]"), "{out}");
        assert!(
            out.contains("members = [\"anthropic-managed\"]"),
            "the entry must be the pool's sole member: {out}"
        );
        assert!(
            out.contains("seat_selection = \"round-robin\""),
            "the knob's value must survive on the pool: {out}"
        );
        // Format-preserving: the operator's comments survive.
        assert!(out.contains("# operator note: keep me"), "{out}");
        assert!(out.contains("# spread the load"), "{out}");
    }

    /// Whether any provider entry still carries the retired knob.
    fn providers_carry_seat_selection(doc: &DocumentMut) -> bool {
        !provider_entries_with_seat_selection(doc).is_empty()
    }

    #[test]
    fn v3_to_v4_with_no_provider_seat_selection_is_a_version_stamp_only() {
        // Arrange: a v3 file whose provider carries no retired knob.
        let src = "version = 3\n\
             [providers.fast]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"env://K\"\n";
        let mut doc = doc_of(src);

        // Act
        migrate_v3_to_v4(&mut doc).expect("stamps");

        // Assert: nothing but the version changed.
        let out = doc.to_string();
        assert_eq!(out, src.replacen("version = 3", "version = 4", 1));
        assert!(!out.contains("pools"), "{out}");
    }

    /// The knob relocation walks provider entries through `TableLike`, so an
    /// INLINE `providers = { ... }` map is seen too -- a plain `as_table_mut`
    /// walk silently no-ops on that shape.
    #[test]
    fn v3_to_v4_relocates_through_an_inline_providers_table() {
        // Arrange
        let mut doc = doc_of(
            "version = 3\n\
             providers = { managed = { kind = \"anthropic-api\", api_key_ref = \
             \"oauth://anthropic\", seat_selection = \"round-robin\" } }\n",
        );

        // Act
        migrate_v3_to_v4(&mut doc).expect("relocation reaches into the inline table");

        // Assert
        let out = doc.to_string();
        assert!(!providers_carry_seat_selection(&doc), "{out}");
        assert!(out.contains("[pools.anthropic]"), "{out}");
        assert!(out.contains("members = [\"managed\"]"), "{out}");
        assert!(out.contains("seat_selection = \"round-robin\""), "{out}");
    }

    #[test]
    fn v3_to_v4_is_idempotent_over_its_own_output() {
        // Arrange
        let mut doc = doc_of(V3_WITH_PROVIDER_SEAT_SELECTION);
        migrate_v3_to_v4(&mut doc).expect("first pass");
        let once = doc.to_string();

        // Act: re-running the rung over the migrated document finds no
        // provider-level knob left to move.
        let mut again = doc_of(&once.replacen("version = 4", "version = 3", 1));
        migrate_v3_to_v4(&mut again).expect("second pass");

        // Assert
        assert_eq!(again.to_string(), once);
    }

    #[test]
    fn v3_to_v4_refuses_two_same_family_entries_deriving_one_pool() {
        // Arrange: two OAuth entries on the SAME family, each carrying the
        // retired knob and each pointing at a DIFFERENT egress. Both derive
        // the pool name `anthropic`.
        let src = "version = 3\n\
             [providers.primary]\n\
             kind = \"anthropic-api\"\n\
             base_url = \"https://one.example\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"round-robin\"\n\
             [providers.secondary]\n\
             kind = \"anthropic-api\"\n\
             base_url = \"https://two.example\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"sticky\"\n";
        let mut doc = doc_of(src);

        // Act
        let err = migrate_v3_to_v4(&mut doc).expect_err("one derived pool, two claimants");

        // Assert: one line naming BOTH entries, document byte-untouched.
        let Refusal::SeatSelectionRelocation { ref entries } = err else {
            panic!("err: {err:?}");
        };
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert!(entries[0].contains("[providers.primary]"), "{entries:?}");
        assert!(entries[0].contains("[providers.secondary]"), "{entries:?}");
        assert_eq!(
            doc.to_string(),
            src,
            "a refusal must not mutate the document"
        );
    }

    #[test]
    fn v3_to_v4_refuses_a_seat_selection_with_no_oauth_ref() {
        // Arrange: an API-key provider carrying the retired knob -- no
        // provider family to name a pool after.
        let src = "version = 3\n\
             [providers.keyed]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"env://K\"\n\
             seat_selection = \"round-robin\"\n";
        let mut doc = doc_of(src);

        // Act
        let err = migrate_v3_to_v4(&mut doc).expect_err("no family -> refuse");

        // Assert: named refusal, document byte-untouched.
        assert!(
            matches!(err, Refusal::SeatSelectionRelocation { ref entries }
                if entries.len() == 1 && entries[0].contains("keyed")),
            "err: {err:?}"
        );
        assert_eq!(
            doc.to_string(),
            src,
            "a refusal must not mutate the document"
        );
    }

    #[test]
    fn v3_to_v4_refuses_when_the_derived_pool_name_is_taken() {
        // Arrange: a second provider entry already holds the family name the
        // pool would take -- providers, pools and model nicknames share one
        // namespace, so there is no defensible answer.
        let src = "version = 3\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#other\"\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"round-robin\"\n";
        let mut doc = doc_of(src);

        // Act
        let err = migrate_v3_to_v4(&mut doc).expect_err("collision -> refuse");

        // Assert
        assert!(
            matches!(err, Refusal::SeatSelectionRelocation { ref entries }
                if entries.len() == 1 && entries[0].contains("one namespace")),
            "err: {err:?}"
        );
        assert_eq!(doc.to_string(), src);
    }

    #[test]
    fn v3_to_v4_refuses_when_a_pool_block_already_exists() {
        // Arrange: a hand-authored pool under the derived name. Relocating
        // onto it would silently change that pool's strategy.
        let src = "version = 3\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"round-robin\"\n\
             [pools.anthropic]\n\
             members = [\"managed\"]\n";
        let mut doc = doc_of(src);

        // Act
        let err = migrate_v3_to_v4(&mut doc).expect_err("existing pool -> refuse");

        // Assert
        assert!(
            matches!(err, Refusal::SeatSelectionRelocation { ref entries }
                if entries[0].contains("[pools.anthropic]")),
            "err: {err:?}"
        );
        assert_eq!(doc.to_string(), src);
    }

    /// The relocated key shows up in the dry-run change summary, so the
    /// operator sees WHY the file grew a pool block.
    #[test]
    fn removed_keys_names_the_relocated_seat_selection() {
        let doc = doc_of(V3_WITH_PROVIDER_SEAT_SELECTION);

        let removed = collect_removed_keys(&doc, 3);

        assert!(
            removed
                .iter()
                .any(|k| k.contains("anthropic-managed") && k.contains("seat_selection")),
            "removed: {removed:?}"
        );
    }

    // -----------------------------------------------------------------------
    // bare_oauth_pool_candidates: the PURE input phase 2 reads. Reports what
    // the document says, never what the credential store holds.
    // -----------------------------------------------------------------------

    #[test]
    fn bare_oauth_candidates_report_bare_refs_only() {
        let doc = doc_of(
            "version = 4\n\
             [providers.bare]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [providers.pinned]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#work\"\n\
             [providers.keyed]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"env://K\"\n",
        );

        let candidates = bare_oauth_pool_candidates(&doc);

        assert_eq!(
            candidates,
            vec![BareOauthRef {
                entry: "bare".to_string(),
                family: "anthropic".to_string(),
            }],
            "only the bare oauth ref is a pool candidate"
        );
    }

    // -----------------------------------------------------------------------
    // apply_seat_pool_move: the store-aware caller's write primitive. Clones
    // the entry per labelled seat and lists every member on the pool.
    // -----------------------------------------------------------------------

    #[test]
    fn apply_seat_pool_move_clones_the_entry_per_labelled_seat() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             auth_kind = \"oauth-bearer\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert: the clone carries the source's other knobs and the seat's
        // own ref, and the pool lists the original entry first.
        let out = doc.to_string();
        assert!(out.contains("[providers.anthropic-work]"), "{out}");
        assert!(
            out.contains("api_key_ref = \"oauth://anthropic#work\""),
            "{out}"
        );
        assert!(
            out.matches("auth_kind = \"oauth-bearer\"").count() == 2,
            "the clone must carry the source's knobs: {out}"
        );
        assert!(
            out.contains("members = [\"managed\", \"anthropic-work\"]"),
            "{out}"
        );
    }

    /// The rename half of the primitive, and the ORDER it composes in: the
    /// entry vacates the family name and its models follow it, THEN the pool
    /// repoint moves those models onto the pool. Composed the other way the
    /// models would name a provider key that no longer exists.
    #[test]
    fn apply_seat_pool_move_renames_the_entry_then_repoints_onto_the_pool() {
        // Arrange: the family-named shape, with a model on it.
        let mut doc = doc_of(
            "version = 4\n\
             # keep this comment on the entry\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             auth_kind = \"oauth-bearer\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude\"\n",
        );
        let mv = SeatPoolMove {
            entry: "anthropic".to_string(),
            rename_to: Some("anthropic-default".to_string()),
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert: the entry moved (comment intact), the pool took the vacated
        // name with the renamed entry as its first member, the labelled seat
        // cloned off the RENAMED entry, and the model lands on the POOL.
        let out = doc.to_string();
        assert!(out.contains("[providers.anthropic-default]"), "{out}");
        assert!(!out.contains("[providers.anthropic]\n"), "{out}");
        assert!(out.contains("# keep this comment on the entry"), "{out}");
        assert!(
            out.contains("members = [\"anthropic-default\", \"anthropic-work\"]"),
            "{out}"
        );
        assert!(
            out.contains("api_key_ref = \"oauth://anthropic#work\""),
            "{out}"
        );
        assert_eq!(
            models_routed_at(&doc, "anthropic"),
            vec!["opus".to_string()]
        );
        assert!(
            models_routed_at(&doc, "anthropic-default").is_empty(),
            "the repoint must not leave the model on the renamed entry: {out}"
        );
    }

    /// A rename with NO accounts (the pure rung's knob relocation on a
    /// family-named single-seat entry) still moves the entry, and the model
    /// follows the RENAME rather than jumping to the one-member pool -- fewer
    /// bytes changed, identical breadth.
    #[test]
    fn a_rename_without_accounts_takes_the_models_to_the_renamed_entry() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude\"\n",
        );
        let mv = SeatPoolMove {
            entry: "anthropic".to_string(),
            rename_to: Some("anthropic-default".to_string()),
            pool: "anthropic".to_string(),
            accounts: Vec::new(),
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert
        let out = doc.to_string();
        assert!(out.contains("members = [\"anthropic-default\"]"), "{out}");
        assert_eq!(
            models_routed_at(&doc, "anthropic-default"),
            vec!["opus".to_string()],
            "{out}"
        );
    }

    /// Idempotence over the rename: the source key is gone and the target
    /// holds the entry, so a replayed move changes no bytes. Without this the
    /// locked-write replay (which reapplies the same plan) would double-write.
    #[test]
    fn a_rename_is_a_no_op_over_its_own_output() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude\"\n",
        );
        let mv = SeatPoolMove {
            entry: "anthropic".to_string(),
            rename_to: Some("anthropic-default".to_string()),
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };
        apply_seat_pool_move(&mut doc, &mv);
        let once = doc.to_string();

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert
        assert_eq!(doc.to_string(), once);
    }

    /// The pure rung renames a family-named entry that carries the retired
    /// knob: the pool block it creates needs the name the entry holds.
    #[test]
    fn v3_to_v4_renames_a_family_named_entry_carrying_the_knob() {
        // Arrange
        let mut doc = doc_of(
            "version = 3\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"round-robin\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude\"\n",
        );

        // Act
        migrate_v3_to_v4(&mut doc).expect("the family-named shape migrates");

        // Assert
        let out = doc.to_string();
        assert!(out.contains("[providers.anthropic-default]"), "{out}");
        assert!(out.contains("[pools.anthropic]"), "{out}");
        assert!(out.contains("members = [\"anthropic-default\"]"), "{out}");
        assert!(out.contains("seat_selection = \"round-robin\""), "{out}");
        assert_eq!(
            models_routed_at(&doc, "anthropic-default"),
            vec!["opus".to_string()],
            "the model must follow the renamed entry: {out}"
        );
    }

    /// The rename never displaces a credential: a `<family>-default` entry
    /// already present makes the move unresolvable, so the rung refuses with
    /// the document byte-untouched.
    #[test]
    fn v3_to_v4_refuses_when_the_rename_target_is_taken() {
        // Arrange
        let src = "version = 3\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             seat_selection = \"round-robin\"\n\
             [providers.anthropic-default]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"env://SOMETHING_ELSE\"\n";
        let mut doc = doc_of(src);

        // Act
        let err = migrate_v3_to_v4(&mut doc).expect_err("taken target -> refuse");

        // Assert
        assert!(
            matches!(err, Refusal::SeatSelectionRelocation { ref entries }
                if entries.len() == 1 && entries[0].contains("anthropic-default")),
            "err: {err:?}"
        );
        assert_eq!(doc.to_string(), src);
    }

    #[test]
    fn apply_seat_pool_move_is_a_no_op_over_its_own_output() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };
        apply_seat_pool_move(&mut doc, &mv);
        let once = doc.to_string();

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert: no duplicated entry, no duplicated member.
        assert_eq!(doc.to_string(), once);
    }

    // -----------------------------------------------------------------------
    // The model repoint: a materialized pool takes over the models that
    // routed at the entry, or v3's dispatch breadth is silently lost (at v4
    // the entry's bare ref is the default seat alone).
    // -----------------------------------------------------------------------

    #[test]
    fn a_materialized_pool_takes_over_the_models_that_routed_at_the_entry() {
        // Arrange: two models on the entry, one on an unrelated provider.
        let mut doc = doc_of(
            "version = 4\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [providers.other]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"env://K\"\n\
             [models.opus]\n\
             provider = \"managed\"\n\
             upstream = \"claude-opus-4-8\"\n\
             [models.sonnet]\n\
             provider = \"managed\"\n\
             upstream = \"claude-sonnet-4-8\"\n\
             [models.gpt]\n\
             provider = \"other\"\n\
             upstream = \"gpt-4o\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert: both models on the entry now name the POOL (which carries
        // every seat); the unrelated model is untouched.
        let models = doc.get("models").and_then(Item::as_table_like).unwrap();
        for nickname in ["opus", "sonnet"] {
            let provider = models
                .get(nickname)
                .and_then(Item::as_table_like)
                .and_then(|m| m.get("provider"))
                .and_then(Item::as_str);
            assert_eq!(
                provider,
                Some("anthropic"),
                "model `{nickname}` must route at the pool, not the single-seat entry"
            );
        }
        assert_eq!(
            models
                .get("gpt")
                .and_then(Item::as_table_like)
                .and_then(|m| m.get("provider"))
                .and_then(Item::as_str),
            Some("other"),
            "a model on an unrelated provider must not be repointed"
        );
        // The entry itself stays a member, so its default seat is still served.
        assert!(
            doc.to_string()
                .contains("members = [\"managed\", \"anthropic-work\"]"),
            "{doc}"
        );
    }

    /// A move with NO materialized accounts is the pure rung's `seat_selection`
    /// relocation on a single-seat family. The pool has one member, so breadth
    /// is identical either way and a member inherits its pool's strategy
    /// regardless -- so model references stay put rather than churn bytes.
    #[test]
    fn a_single_member_pool_leaves_model_references_alone() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [models.opus]\n\
             provider = \"managed\"\n\
             upstream = \"claude-opus-4-8\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: Vec::new(),
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert
        assert!(doc.to_string().contains("provider = \"managed\""), "{doc}");
    }

    #[test]
    fn the_model_repoint_is_a_no_op_over_its_own_output() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n\
             [models.opus]\n\
             provider = \"managed\"\n\
             upstream = \"claude-opus-4-8\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };
        apply_seat_pool_move(&mut doc, &mv);
        let once = doc.to_string();

        // Act: a model already naming the pool is not rewritten again.
        apply_seat_pool_move(&mut doc, &mv);

        // Assert
        assert_eq!(doc.to_string(), once);
    }

    /// The repoint walks models through `TableLike`, so an INLINE `models = {
    /// ... }` map is seen too -- a plain table walk would silently leave those
    /// models on the single-seat entry.
    #[test]
    fn the_model_repoint_reaches_into_an_inline_models_table() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             models = { opus = { provider = \"managed\", upstream = \"claude-opus-4-8\" } }\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic\"\n",
        );
        let mv = SeatPoolMove {
            entry: "managed".to_string(),
            rename_to: None,
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        };

        // Act
        apply_seat_pool_move(&mut doc, &mv);

        // Assert
        let out = doc.to_string();
        assert!(out.contains("provider = \"anthropic\""), "{out}");
        assert!(!out.contains("provider = \"managed\""), "{out}");
    }

    #[test]
    fn models_routed_at_names_only_the_entrys_own_models() {
        let doc = doc_of(
            "version = 4\n\
             [models.opus]\n\
             provider = \"managed\"\n\
             upstream = \"claude-opus-4-8\"\n\
             [models.gpt]\n\
             provider = \"other\"\n\
             upstream = \"gpt-4o\"\n",
        );

        assert_eq!(models_routed_at(&doc, "managed"), vec!["opus".to_string()]);
        assert!(models_routed_at(&doc, "nobody").is_empty());
    }

    /// The union is what lets two writers (the ladder and the login
    /// auto-surface) grow one pool without either knowing what the other
    /// wrote: a rerun must add no member and change no byte.
    #[test]
    fn upsert_pool_members_unions_without_duplicating_and_is_byte_idempotent() {
        // Arrange
        let mut doc = doc_of("version = 4\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["a", "b"]);
        let after_first = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["b", "c"]);
        let after_second = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["a", "b", "c"]);

        // Assert
        assert!(
            after_first.contains(r#"members = ["a", "b"]"#),
            "{after_first}"
        );
        assert!(
            after_second.contains(r#"members = ["a", "b", "c"]"#),
            "{after_second}"
        );
        assert_eq!(doc.to_string(), after_second, "a rerun must change no byte");
    }

    /// The union covers duplicates WITHIN one call's member list too, not
    /// just against what the block already carries: a caller assembling
    /// the list from two sources (an entry name plus a pool's members)
    /// must not be able to write a duplicate.
    #[test]
    fn upsert_pool_members_drops_duplicates_within_one_calls_member_list() {
        // Arrange
        let mut doc = doc_of("version = 4\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["a", "a", "b", "a"]);

        // Assert
        assert!(doc.to_string().contains(r#"members = ["a", "b"]"#), "{doc}");
    }

    /// Everything else on the block is the operator's statement and
    /// survives a member union verbatim -- including the growth marker,
    /// which login never flips.
    #[test]
    fn upsert_pool_members_leaves_seat_selection_and_the_growth_marker_untouched() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [pools.anthropic]\n\
             members = [\"a\"]\n\
             seat_selection = \"round-robin\" # spread the load\n\
             accepts_new_logins = false\n",
        );

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["b"]);

        // Assert
        let out = doc.to_string();
        assert!(out.contains(r#"members = ["a", "b"]"#), "{out}");
        assert!(
            out.contains("seat_selection = \"round-robin\" # spread the load"),
            "{out}"
        );
        assert!(out.contains("accepts_new_logins = false"), "{out}");
    }

    /// An inline `pools = { ... }` shape must be walked as well as the
    /// standard-table one, or a member union silently no-ops.
    #[test]
    fn upsert_pool_members_descends_into_an_inline_pools_table() {
        // Arrange
        let mut doc = doc_of("version = 4\npools = { anthropic = { members = [\"a\"] } }\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["b"]);

        // Assert
        assert!(doc.to_string().contains(r#""a", "b""#), "{doc}");
    }
}
