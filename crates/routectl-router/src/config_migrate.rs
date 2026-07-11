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
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::{CachePricingOverride, CachePricingSelector};
use crate::catalog_overlay::{self, OverlayCell, OverlaySource};
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
/// `routectl-cli`'s `commands::pricing::load_and_merge_verifications`) into
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
/// stamped via `routectl pricing verify`), not a bulk vendor import --
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

    doc["version"] = toml_edit::value(i64::from(CURRENT_CONFIG_VERSION));
    doc.remove("cache_pricing");

    write_config_atomic(config_path, doc.to_string().as_bytes()).map_err(|reason| {
        MigrationError::ConfigIo {
            path: display,
            reason,
        }
    })
}

/// Temp-file-then-rename write, mirroring `catalog_overlay`'s writer
/// discipline (fsync before rename), but preserving the ORIGINAL file's
/// permission bits instead of forcing `0o600` -- `config.toml` is
/// operator-owned and its existing mode is deliberate, not a default this
/// migration should override.
///
/// Also fsyncs the PARENT DIRECTORY after the rename: `rename` is atomic
/// for concurrent readers, but the rename itself is not durable across a
/// crash/power loss until the directory entry pointing at the new inode is
/// flushed too -- without this, a crash right after this call returns can
/// roll the directory entry back to the pre-rename file even though the
/// temp file's own contents were already fsynced.
fn write_config_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;

    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).ok().map(|m| m.permissions().mode())
    };

    let mut tmp = tempfile::Builder::new()
        .prefix(".config.tmp.")
        .suffix(".toml")
        .tempfile_in(parent)
        .map_err(|e| format!("tempfile: {e}"))?;
    tmp.write_all(bytes)
        .map_err(|e| format!("write tempfile: {e}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("fsync tempfile: {e}"))?;
    tmp.persist(path).map_err(|e| format!("rename: {e}"))?;

    #[cfg(unix)]
    if let Some(mode) = original_mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
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
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
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
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
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
        assert_eq!(reparsed.version, CURRENT_CONFIG_VERSION);
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
}
