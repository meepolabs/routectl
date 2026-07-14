//! v1 -> v2 config migration: retires the legacy `[cache_pricing]`
//! TOML table into the catalog overlay ([`crate::catalog_overlay`]).
//!
//! TWO-PHASE + IDEMPOTENT, driven by [`migrate_v1_to_v2`]:
//!   1. Build an overlay candidate cell for every entry in the (already
//!      sidecar-merged, by the caller) `[cache_pricing]` table, then
//!      atomically write it into `catalog_overlay.json` via the revision-
//!      checked `crate::catalog_overlay::save`. A pre-existing overlay key
//!      whose value DIFFERS from the candidate is a conflict: nothing is
//!      written, for ANY key -- fail closed rather than guess which side
//!      is right.
//!   2. Format-preserving rewrite of `config.toml` via `toml_edit`: set
//!      `version = 2`, drop `[cache_pricing]`. A plain `toml::to_string`
//!      round trip would destroy operator comments; `toml_edit` edits the
//!      original document in place instead.
//!
//! Crash between phase 1 and phase 2 leaves `config.toml` still reporting
//! `version < 2`, so the caller reruns this whole function unchanged on
//! the next load. Phase 1 is naturally idempotent (a candidate whose value
//! already matches what phase 1 wrote last time is a silent no-op, not a
//! conflict -- see [`cell_values_equal`]); phase 2 is a single atomic
//! rewrite, so it either has not run yet or has already fully completed.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use routectl_core::failure_class::{FailureClass, class_guidance_for_status};
use toml_edit::{DocumentMut, TableLike};

use crate::catalog::{CachePricingOverride, CachePricingSelector};
use crate::catalog_overlay::{self, OverlayCell, OverlaySource};
use crate::class_policy::ConfigFailureClass;
use crate::config::CURRENT_CONFIG_VERSION;

/// Seconds in a day, for epoch-day arithmetic off the system clock.
const SECONDS_PER_DAY: i64 = 86_400;

/// What [`migrate_v1_to_v2`] did, for the caller's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Number of `[cache_pricing]` entries folded into the overlay
    /// candidate (including ones that turned out to already match an
    /// existing overlay cell and triggered no write).
    pub cells_migrated: usize,
}

/// Errors from [`migrate_v1_to_v2`]. Every variant fails closed: on any
/// error, NEITHER the overlay NOR `config.toml` has been modified for this
/// call (a partial phase-1 write across multiple keys never happens --
/// conflicts are collected up front and the whole write is skipped when
/// any exist).
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// A `[cache_pricing]` selector key does not parse as
    /// `provider_kind:model_glob`.
    #[error("cache-pricing migration: selector `{selector}` is invalid: {reason}")]
    InvalidSelector { selector: String, reason: String },

    /// A `[cache_pricing]` override is degenerate (same checks as
    /// `crate::catalog::validate_overrides`, applied here so a bad override
    /// never gets carried forward into the overlay).
    #[error("cache-pricing migration: override for `{selector}` is invalid: {reason}")]
    InvalidOverride { selector: String, reason: String },

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

    /// A filesystem failure specific to the `config.toml` rewrite phase.
    #[error("cache-pricing migration: config `{path}`: {reason}")]
    ConfigIo { path: String, reason: String },
}

/// Run the v1 -> v2 migration against the config at `config_path`, folding
/// `cache_pricing` (the operator's `[cache_pricing]` table, ALREADY merged
/// with any legacy `pricing_verifications.json` stamps by the caller --
/// see `crate::config::CachePricingOverride`'s doc and
/// `routectl-cli`'s `commands::catalog::load_and_merge_verifications`) into
/// the catalog overlay at `overlay_path`, then rewriting `config.toml` to
/// `version = 2` with `[cache_pricing]` dropped.
///
/// Safe to call unconditionally when the caller already knows
/// `config.version < CURRENT_CONFIG_VERSION`: an empty `cache_pricing` is a
/// no-op for phase 1 (no overlay write) but phase 2 still runs, bumping the
/// version stamp so this function is not invoked again on the next load.
pub fn migrate_v1_to_v2(
    cache_pricing: &BTreeMap<String, CachePricingOverride>,
    config_path: &Path,
    overlay_path: &Path,
) -> Result<MigrationOutcome, MigrationError> {
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

    if changed {
        catalog_overlay::save(overlay_path, overlay.revision, merged_cells)?;
    }

    rewrite_config_to_v2(config_path)?;

    Ok(MigrationOutcome {
        cells_migrated: candidates.len(),
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
                selector = %selector,
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
        && a.capabilities == b.capabilities
}

/// Format-preserving rewrite: set `version = 2` and drop `[cache_pricing]`
/// via `toml_edit`, then write the result back atomically (temp file in
/// the same directory, fsync, rename). The original file's permission
/// bits are restored after the rename (best-effort) -- unlike the catalog
/// overlay's writer, `config.toml` is an operator-facing file, so this
/// must not silently narrow (or widen) its mode.
fn rewrite_config_to_v2(config_path: &Path) -> Result<(), MigrationError> {
    let display = config_path.display().to_string();
    let text = std::fs::read_to_string(config_path).map_err(|e| MigrationError::ConfigIo {
        path: display.clone(),
        reason: format!("read: {e}"),
    })?;

    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| MigrationError::ConfigIo {
            path: display.clone(),
            reason: format!("parse for rewrite: {e}"),
        })?;

    // Each step stamps its own LITERAL target: v1 -> v2 stamps `2`. Only
    // the ladder in `migrate_to_current` knows what "current" is, so a
    // later bump of `CURRENT_CONFIG_VERSION` cannot make this v1->v2 rung
    // silently over-stamp a version it did not actually migrate to.
    doc["version"] = toml_edit::value(2i64);
    doc.remove("cache_pricing");

    crate::config_write::write_config_atomic(config_path, doc.to_string().as_bytes()).map_err(
        |reason| MigrationError::ConfigIo {
            path: display,
            reason,
        },
    )
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

/// The highest config version the step ladder in [`migrate_to_current`]
/// knows how to produce. Deliberately a LITERAL, not `CURRENT_CONFIG_VERSION`:
/// the two are kept equal, but the ladder's rungs and its "too new" ceiling
/// must stay pinned to the versions whose transforms actually exist, so a
/// bare bump of the const (task ordering) can never make the ladder claim a
/// version it has no step for -- and an already-latest doc stays a no-op
/// regardless of what the const currently reads.
const LATEST_MIGRATION_VERSION: u32 = 3;

/// The ladder must always be able to reach the current version: every step
/// up to `CURRENT_CONFIG_VERSION` has to exist. Enforced at compile time so
/// a bump of the const without a matching rung is caught by the build, not
/// at runtime.
const _: () = assert!(
    LATEST_MIGRATION_VERSION >= CURRENT_CONFIG_VERSION,
    "config migration ladder is missing a step for the current config version",
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

/// A refusal from [`migrate_v2_to_v3`]: the transform declines and leaves
/// the document byte-untouched. Two disjoint causes, both fail-closed:
///
/// - [`Refusal::BehaviorBearing`]: the v2 doc carries behavior-bearing
///   `retry_allowlist` / `retry_denylist` codes that have no lossless fold
///   into the `[retry.classes.*]` class overlay.
/// - [`Refusal::Malformed`]: a `retry_allowlist` / `retry_denylist` entry is
///   not a valid `u16` HTTP status. Silently dropping it would strip the key
///   and change behavior, so the migrator refuses rather than fold.
///
/// Carries the offending content and (for the behavior-bearing case)
/// rendered per-code guidance so the caller can both log a structured audit
/// event and print hand-edit instructions.
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
        }
    }
}

impl std::error::Error for Refusal {}

/// Errors from the [`migrate_to_current`] ladder.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The v1 -> v2 rung failed (overlay fold or its atomic config rewrite).
    #[error(transparent)]
    V1ToV2(#[from] MigrationError),

    /// The v2 -> v3 rung refused a behavior-bearing config; nothing written.
    #[error("config migration to version 3 refused:\n{0}")]
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

/// The inputs the v1 -> v2 rung needs to run its catalog-overlay fold and
/// atomic `config.toml` rewrite. Grouped so [`migrate_to_current`]'s
/// signature stays legible; a v2 (or later) file ignores them entirely.
pub struct V1Migration<'a> {
    /// The operator's `[cache_pricing]` table, ALREADY merged with any
    /// legacy sidecar stamps by the caller (see [`migrate_v1_to_v2`]).
    pub cache_pricing: &'a BTreeMap<String, CachePricingOverride>,
    /// Path to the `config.toml` the v1 -> v2 rung reads and atomically
    /// rewrites in place.
    pub config_path: &'a Path,
    /// Path to the catalog overlay the fold writes into.
    pub overlay_path: &'a Path,
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

/// Render a [`ConfigFailureClass`] as the kebab-case token it spells in
/// `[retry.classes.<token>]`, via its own `Serialize` impl so the spelling
/// cannot drift from the config surface.
fn class_token(class: ConfigFailureClass) -> String {
    serde_json::to_string(&class)
        .expect("ConfigFailureClass serialization is infallible")
        .trim_matches('"')
        .to_string()
}

/// The `[retry.classes.<token>]` spelling for a canonical [`FailureClass`],
/// or `None` for a class with no operator-nameable config token (`Unknown`
/// or a future `#[non_exhaustive]` variant).
fn nearest_class_token(class: &FailureClass) -> Option<String> {
    ConfigFailureClass::from_failure_class(class).map(class_token)
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

/// Migrate a config from its RAW on-disk `version` up to the latest the
/// build knows, applying each step in order. Dispatches on `raw_version`
/// (never a typed parse -- a too-old file may not deserialize under the
/// current schema) and mutates `doc` in place so that, on success, `doc`
/// is the fully-migrated document the caller commits.
///
/// The ladder is deliberately a flat sequence of literal rungs, not a
/// trait or registry: the next break is one file-local step function plus
/// one rung here.
///
/// - `raw_version <= 1`: runs [`migrate_v1_to_v2`], which folds
///   `[cache_pricing]` into the overlay and ATOMICALLY rewrites
///   `config.toml` to `version = 2` (overlay fold ordered BEFORE the stamp,
///   so a crash between them is recoverable by rerun). Because that rung
///   commits its own result to disk, `doc` is then re-read from
///   `config_path` so the in-memory document reflects the v2 result before
///   the pure v2 -> v3 rung runs on it.
/// - `raw_version == 2`: runs the pure [`migrate_v2_to_v3`] transform on
///   `doc`; NO IO -- the caller performs the single final commit.
/// - `raw_version == LATEST` ([`LATEST_MIGRATION_VERSION`]): no-op.
/// - `raw_version > LATEST`: [`MigrateError::VersionTooNew`].
///
/// # Errors
///
/// [`MigrateError::VersionTooNew`] for a future version, [`MigrateError::V1ToV2`]
/// if the v1 rung's IO fails, [`MigrateError::Refused`] if the v2 rung declines.
pub fn migrate_to_current(
    doc: &mut DocumentMut,
    raw_version: u32,
    v1: &V1Migration<'_>,
) -> Result<Vec<StepOutcome>, MigrateError> {
    if raw_version > LATEST_MIGRATION_VERSION {
        return Err(MigrateError::VersionTooNew {
            found: raw_version,
            supported: LATEST_MIGRATION_VERSION,
        });
    }

    let mut steps = Vec::new();
    let mut version = raw_version;

    if version <= 1 {
        migrate_v1_to_v2(v1.cache_pricing, v1.config_path, v1.overlay_path)?;
        let text =
            std::fs::read_to_string(v1.config_path).map_err(|e| MigrationError::ConfigIo {
                path: v1.config_path.display().to_string(),
                reason: format!("reread after v1->v2: {e}"),
            })?;
        *doc = text
            .parse::<DocumentMut>()
            .map_err(|e| MigrationError::ConfigIo {
                path: v1.config_path.display().to_string(),
                reason: format!("reparse after v1->v2: {e}"),
            })?;
        steps.push(StepOutcome {
            from_version: 1,
            to_version: 2,
        });
        version = 2;
    }

    if version == 2 {
        let outcome = migrate_v2_to_v3(doc).map_err(MigrateError::Refused)?;
        steps.push(outcome);
    }

    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn override_with_wm(wm: f32) -> CachePricingOverride {
        CachePricingOverride {
            wm: Some(wm),
            override_acknowledges_cost_risk: wm < 2.0,
            ..Default::default()
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
    // Happy path: cache_pricing entries land in the overlay, config.toml
    // is rewritten to version = 2 with [cache_pricing] dropped, and
    // operator comments/ordering survive.
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_happy_path_moves_cache_pricing_into_overlay_and_bumps_version() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
            "# operator note: grok override below\n\
             [server]\n\
             host = \"127.0.0.1\" # loopback only\n\
             port = 8787\n\
             \n\
             [cache_pricing]\n\
             \"openai-compat:grok-*\" = { wm = 1.5, verified_at = \"2026-06-01\", \
             override_acknowledges_cost_risk = true }\n",
        );
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
        let outcome = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path).expect("migrate");

        // Assert: overlay carries the migrated cell.
        assert_eq!(outcome.cells_migrated, 1);
        let overlay = catalog_overlay::load(&overlay_path).expect("load overlay");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.verified_at, "2026-06-01");
        assert_eq!(cell.wm, Some(1.5));

        // Assert: config.toml rewritten -- version = 2, [cache_pricing]
        // gone, comments and unrelated content preserved.
        let rewritten = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(rewritten.contains("version = 2"), "rewritten: {rewritten}");
        assert!(
            !rewritten.contains("cache_pricing"),
            "rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains("# operator note: grok override below"),
            "rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains("host = \"127.0.0.1\" # loopback only"),
            "rewritten: {rewritten}"
        );

        // Assert: the rewritten file re-parses cleanly as a v2 Config.
        let reparsed: crate::config::Config = toml::from_str(&rewritten).expect("reparse");
        assert_eq!(reparsed.version, 2);
        assert!(reparsed.cache_pricing.is_empty());
    }

    #[test]
    fn migrate_no_op_cache_pricing_still_bumps_version() {
        // Arrange: a v1 config with no [cache_pricing] at all.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(dir.path(), "[server]\nhost = \"127.0.0.1\"\n");

        // Act
        let outcome =
            migrate_v1_to_v2(&BTreeMap::new(), &cfg_path, &overlay_path).expect("migrate");

        // Assert: nothing to migrate, but the version stamp still bumps so
        // this file is never re-migrated.
        assert_eq!(outcome.cells_migrated, 0);
        assert!(!overlay_path.exists(), "no cells -> no overlay write");
        let reparsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(reparsed.version, 2);
    }

    /// `[cache_pricing]` written in the TABLE form (`[cache_pricing."key"]`
    /// dotted sub-tables, one per selector) rather than the inline-map form
    /// used by the other tests here -- this is what `toml::to_string_pretty`
    /// actually emits for a `BTreeMap<String, T>` field, so it is the
    /// realistic on-disk shape for an operator-edited or `config show`-saved
    /// file. Also covers MULTIPLE selectors and other unrelated tables
    /// interleaved around `[cache_pricing]`, so `doc.remove("cache_pricing")`
    /// must drop the whole subtree without disturbing `[providers.foo]`.
    #[test]
    fn migrate_rewrite_handles_table_form_cache_pricing_with_multiple_entries() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
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
        let outcome = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path).expect("migrate");

        // Assert: both selectors migrated, [providers.foo] survives intact,
        // and the rewritten file re-parses as a clean v2 Config.
        assert_eq!(outcome.cells_migrated, 2);
        let rewritten = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(
            !rewritten.contains("cache_pricing"),
            "rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains("[providers.foo]"),
            "rewritten: {rewritten}"
        );

        let reparsed: crate::config::Config = toml::from_str(&rewritten).expect("reparse");
        assert_eq!(reparsed.version, 2);
        assert!(reparsed.cache_pricing.is_empty());
        assert!(reparsed.providers.contains_key("foo"));

        let overlay = catalog_overlay::load(&overlay_path).unwrap();
        assert!(overlay.cells.contains_key("openai-compat:grok-*"));
        assert!(overlay.cells.contains_key("openai-compat:mistral-*"));
    }

    // -----------------------------------------------------------------------
    // Idempotence: simulate a crash between phase 1 (overlay written) and
    // phase 2 (config.toml not yet rewritten) by running the SAME
    // migration twice against the SAME still-v1 config.toml.
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_rerun_after_crash_between_phases_completes_cleanly() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
            "[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { \
             wm = 1.5 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // Act: first run completes both phases.
        migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path).expect("first run");
        let overlay_after_first = catalog_overlay::load(&overlay_path).expect("load");

        // Simulate "crash between phases": config.toml still names the
        // ALREADY-migrated data (phase 2 already ran for real above, but
        // the caller re-invokes with the same cache_pricing map it read
        // before phase 2 rewrote the file -- exactly what happens on a
        // process restart between phase 1 committing and phase 2's
        // rename landing).
        let outcome = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect("idempotent rerun must not conflict");

        // Assert: no dupes, no conflict, same overlay content (revision
        // unchanged -- the second run recognized the existing cell as an
        // exact value match and wrote nothing new).
        assert_eq!(outcome.cells_migrated, 1);
        let overlay_after_second = catalog_overlay::load(&overlay_path).expect("load");
        assert_eq!(overlay_after_second.revision, overlay_after_first.revision);
        assert_eq!(overlay_after_second.cells, overlay_after_first.cells);
    }

    #[test]
    fn migrate_idempotent_rerun_survives_a_verified_at_fallback_date_change() {
        // Arrange: an override with NO explicit verified_at -- the
        // migrator stamps "today". Prove a rerun (which recomputes "today"
        // again, possibly a different day in a real crash-restart) is
        // still recognized as the same candidate, not a conflict.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
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
        migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path).expect("first run");

        // Act: manually age the stored verified_at, mimicking a rerun on
        // a later calendar day whose freshly-computed "today" would
        // differ from what got stored the first time.
        let mut overlay = catalog_overlay::load(&overlay_path).expect("load");
        let expected_revision = overlay.revision;
        if let Some(Some(cell)) = overlay.cells.get_mut("openai-compat:grok-*") {
            cell.verified_at = "2020-01-01".to_string();
        }
        catalog_overlay::save(&overlay_path, expected_revision, overlay.cells.clone())
            .expect("re-save with aged date");

        // Assert: rerunning does not conflict despite the stored
        // verified_at no longer matching what "today" would produce now.
        migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect("rerun must not conflict on a verified_at-only difference");
    }

    // -----------------------------------------------------------------------
    // Conflict: an existing overlay cell with a DIFFERENT value fails
    // closed and writes nothing (overlay untouched, config.toml untouched).
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_conflict_with_different_existing_overlay_value_writes_nothing() {
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
                capabilities: None,
            }),
        );
        catalog_overlay::save(&overlay_path, 0, existing).expect("seed overlay");
        let overlay_before = std::fs::read(&overlay_path).unwrap();

        let cfg_path = write_config(
            dir.path(),
            "[server]\nhost = \"127.0.0.1\"\n\n[cache_pricing]\n\"openai-compat:grok-*\" = { \
             wm = 1.5 }\n",
        );
        let cfg_before = std::fs::read(&cfg_path).unwrap();
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // Act
        let err = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect_err("conflicting value must fail closed");

        // Assert: nothing written on either side.
        assert!(matches!(err, MigrationError::Conflict(_)), "err: {err}");
        assert_eq!(std::fs::read(&overlay_path).unwrap(), overlay_before);
        assert_eq!(std::fs::read(&cfg_path).unwrap(), cfg_before);
    }

    #[test]
    fn migrate_conflict_with_disabled_existing_cell_writes_nothing() {
        // Arrange: the overlay explicitly disables this selector (JSON
        // null) -- an operator's deliberate choice the migrator must not
        // silently overwrite with a value.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let mut existing = BTreeMap::new();
        existing.insert("openai-compat:grok-*".to_string(), None);
        catalog_overlay::save(&overlay_path, 0, existing).expect("seed overlay");

        let cfg_path = write_config(
            dir.path(),
            "[cache_pricing]\n\"openai-compat:grok-*\" = { wm = 1.5 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("openai-compat:grok-*".to_string(), override_with_wm(1.5));

        // Act / Assert
        let err = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect_err("a disabled existing cell must conflict, not be overwritten");
        assert!(matches!(err, MigrationError::Conflict(_)), "err: {err}");

        let overlay = catalog_overlay::load(&overlay_path).unwrap();
        assert_eq!(
            overlay.cells.get("openai-compat:grok-*"),
            Some(&None),
            "the disabled cell must remain untouched"
        );
    }

    // -----------------------------------------------------------------------
    // Invalid input fails closed before anything is written.
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_invalid_override_fails_closed_and_writes_nothing() {
        // Arrange: rm <= 0.0 is unconditionally rejected by
        // CachePricingOverride::validate.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
            "[cache_pricing]\n\"openai-compat:grok-*\" = { rm = 0.0 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                rm: Some(0.0),
                ..Default::default()
            },
        );

        // Act
        let err = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect_err("degenerate rm must fail closed");

        // Assert
        assert!(
            matches!(err, MigrationError::InvalidOverride { .. }),
            "err: {err}"
        );
        assert!(!overlay_path.exists(), "nothing should have been written");
        assert!(
            std::fs::read_to_string(&cfg_path)
                .unwrap()
                .contains("cache_pricing"),
            "config.toml must be untouched on failure"
        );
    }

    #[test]
    fn migrate_malformed_selector_key_fails_closed() {
        // Arrange: a selector missing the required `:` separator.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
            "[cache_pricing]\n\"no-colon-here\" = { wm = 1.5 }\n",
        );
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert("no-colon-here".to_string(), override_with_wm(1.5));

        // Act / Assert
        let err = migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path)
            .expect_err("malformed selector must fail closed");
        assert!(
            matches!(err, MigrationError::InvalidSelector { .. }),
            "err: {err}"
        );
        assert!(!overlay_path.exists());
    }

    // -----------------------------------------------------------------------
    // A verify-only entry (no value fields, only verified_at) still lands
    // in the overlay -- the provenance/staleness stamp the old sidecar
    // used to carry moves forward, not just economics overrides.
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_verify_only_entry_lands_as_a_provenance_only_overlay_cell() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(dir.path(), "[server]\nhost = \"127.0.0.1\"\n");
        let mut cache_pricing = BTreeMap::new();
        cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                verified_at: Some("2026-06-30".to_string()),
                ..Default::default()
            },
        );

        // Act
        migrate_v1_to_v2(&cache_pricing, &cfg_path, &overlay_path).expect("migrate");

        // Assert
        let overlay = catalog_overlay::load(&overlay_path).unwrap();
        let cell = overlay
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
    // rewrite_config_to_v2 stamps the LITERAL 2, not the current-version
    // const -- pinned so a later bump of CURRENT_CONFIG_VERSION cannot make
    // the v1->v2 rung over-stamp a version it did not migrate to.
    // -----------------------------------------------------------------------

    #[test]
    fn rewrite_config_to_v2_stamps_the_literal_2_not_the_const() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_config(dir.path(), "version = 1\n[server]\nhost = \"127.0.0.1\"\n");

        rewrite_config_to_v2(&cfg_path).expect("rewrite");

        let text = std::fs::read_to_string(&cfg_path).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["version"].as_integer(), Some(2));
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
    // migrate_to_current ladder. These deliberately assert LITERAL target
    // versions, never CURRENT_CONFIG_VERSION, so they hold under both the
    // pre- and post- const-bump tree state.
    // -----------------------------------------------------------------------

    fn v1_inputs<'a>(
        cache_pricing: &'a BTreeMap<String, CachePricingOverride>,
        config_path: &'a Path,
        overlay_path: &'a Path,
    ) -> V1Migration<'a> {
        V1Migration {
            cache_pricing,
            config_path,
            overlay_path,
        }
    }

    #[test]
    fn ladder_v1_doc_chains_to_3_folding_cache_pricing_and_dropping_lists() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
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

        let mut doc = std::fs::read_to_string(&cfg_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let v1 = v1_inputs(&cache_pricing, &cfg_path, &overlay_path);

        let steps = migrate_to_current(&mut doc, 1, &v1).expect("v1 chains to 3");
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[0],
            StepOutcome {
                from_version: 1,
                to_version: 2
            }
        );
        assert_eq!(
            steps[1],
            StepOutcome {
                from_version: 2,
                to_version: 3
            }
        );

        // In-memory doc is fully migrated to v3, cache_pricing folded away,
        // retry list dropped.
        let out = doc.to_string();
        assert_eq!(doc["version"].as_integer(), Some(3), "{out}");
        assert!(!out.contains("cache_pricing"), "{out}");
        assert!(!out.contains("retry_allowlist"), "{out}");

        // The v1->v2 rung folded the override into the overlay.
        let overlay = catalog_overlay::load(&overlay_path).unwrap();
        assert!(overlay.cells.contains_key("openai-compat:grok-*"));

        // On-disk file is at v2 (the v1->v2 rung's own commit); the caller
        // owns the final v3 commit.
        let on_disk = std::fs::read_to_string(&cfg_path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(on_disk["version"].as_integer(), Some(2));
    }

    #[test]
    fn ladder_v2_doc_migrates_to_3_without_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(dir.path(), "version = 2\n[retry]\nretry_allowlist = []\n");
        let before = std::fs::read(&cfg_path).unwrap();

        let mut doc = "version = 2\n[retry]\nretry_allowlist = []\n"
            .parse::<DocumentMut>()
            .unwrap();
        let cache_pricing = BTreeMap::new();
        let v1 = v1_inputs(&cache_pricing, &cfg_path, &overlay_path);

        let steps = migrate_to_current(&mut doc, 2, &v1).expect("v2 -> 3");
        assert_eq!(
            steps,
            vec![StepOutcome {
                from_version: 2,
                to_version: 3
            }]
        );
        assert_eq!(doc["version"].as_integer(), Some(3));
        // Pure transform: config.toml on disk untouched.
        assert_eq!(std::fs::read(&cfg_path).unwrap(), before);
    }

    #[test]
    fn ladder_already_latest_doc_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(dir.path(), "version = 3\n");
        let mut doc = "version = 3\n".parse::<DocumentMut>().unwrap();
        let cache_pricing = BTreeMap::new();
        let v1 = v1_inputs(&cache_pricing, &cfg_path, &overlay_path);

        let steps = migrate_to_current(&mut doc, LATEST_MIGRATION_VERSION, &v1).expect("no-op");
        assert!(steps.is_empty());
        assert_eq!(doc["version"].as_integer(), Some(3));
    }

    #[test]
    fn ladder_future_version_is_too_new() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(dir.path(), "version = 9\n");
        let mut doc = "version = 9\n".parse::<DocumentMut>().unwrap();
        let cache_pricing = BTreeMap::new();
        let v1 = v1_inputs(&cache_pricing, &cfg_path, &overlay_path);

        let err = migrate_to_current(&mut doc, LATEST_MIGRATION_VERSION + 1, &v1)
            .expect_err("future version is too new");
        assert!(
            matches!(err, MigrateError::VersionTooNew { found, supported }
                if found == LATEST_MIGRATION_VERSION + 1 && supported == LATEST_MIGRATION_VERSION),
            "err: {err}"
        );
    }

    #[test]
    fn ladder_v2_doc_with_behavior_bearing_list_refuses_without_disk_write() {
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let cfg_path = write_config(
            dir.path(),
            "version = 2\n[retry]\nretry_allowlist = [503]\n",
        );
        let before = std::fs::read(&cfg_path).unwrap();
        let mut doc = "version = 2\n[retry]\nretry_allowlist = [503]\n"
            .parse::<DocumentMut>()
            .unwrap();
        let cache_pricing = BTreeMap::new();
        let v1 = v1_inputs(&cache_pricing, &cfg_path, &overlay_path);

        let err = migrate_to_current(&mut doc, 2, &v1).expect_err("behavior-bearing list refuses");
        assert!(matches!(err, MigrateError::Refused(_)), "err: {err}");
        assert_eq!(std::fs::read(&cfg_path).unwrap(), before);
    }
}
