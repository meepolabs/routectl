//! `routectl pricing` -- inspect and stamp the cache-economics catalog.
//!
//! Two subcommands:
//!   list    -- print the EFFECTIVE catalog (the two-layer merge of the
//!              baked table with the on-disk `catalog_overlay.json`,
//!              `routectl_router::merge`) as an aligned ASCII table. Every
//!              row renders PRESENT (with derived provenance + a staleness
//!              marker) or DISABLED (overlay `null`); MISSING never
//!              appears in this catalog-only listing (see
//!              [`build_list_data`]'s doc) even though the render path
//!              still renders it correctly (see the `missing_state_renders`
//!              test) for a future consumer keyed on configured aliases.
//!   verify  -- stamp an EXISTING overlay cell's `verified_at` to today,
//!              flipping its `source` to `user` (verifying is a user act).
//!              Writes through the revision-checked overlay writer
//!              (`routectl_router::save_catalog_overlay`). A selector with
//!              no overlay cell (baked-only, or entirely unknown) has
//!              nothing to stamp and is an error -- creating a new overlay
//!              cell is a later import/set verb.
//!
//! LEGACY SIDECAR (`pricing_verifications.json`): this module still carries
//! the READ side of the old sidecar format ([`PricingVerifications`],
//! [`load_verifications`], [`merge_verifications_into`],
//! [`load_and_merge_verifications`]) -- but ONLY as a read path consumed by
//! the v1 -> v2 config migration (`server::load_effective_config`, which
//! calls [`load_and_merge_verifications`] to fold any historical sidecar
//! stamps into `config.cache_pricing` before the migrator moves them into
//! the catalog overlay). Nothing in the CLI writes the sidecar anymore --
//! `verify` now stamps the overlay directly -- so the write side
//! (`save_verification` / the atomic sidecar writer) is gone. The read side
//! stays until v1 config support itself is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use routectl_router::{
    CachePricingOverride, CachePricingSelector, CatalogOverlay, CatalogRow, Config, EffectiveRow,
    OverlayCell, OverlaySource, Source, baked_table_rows, is_stale_today, load_catalog_overlay,
    merge, overlay_default_path, save_catalog_overlay,
};

// ---------------------------------------------------------------------------
// Legacy sidecar (read-only, migration-only)
// ---------------------------------------------------------------------------

/// On-disk shape for the legacy `pricing_verifications.json` sidecar.
///
/// Uses a wrapper struct (not a bare map) so future fields can be added
/// without a format break. Read-only: nothing writes this shape anymore.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingVerifications {
    /// Maps a selector string (`"provider_kind:model_glob"`) to a
    /// verification date (`"YYYY-MM-DD"`).
    #[serde(default)]
    pub verified: BTreeMap<String, String>,
}

/// Path to the legacy sidecar file. Mirrors the `resolve_config_path` dir
/// logic in `main.rs`.
pub fn verifications_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("routectl").join("pricing_verifications.json")
}

/// Load the sidecar. Missing file -> `Default` (first run, not an error).
/// Malformed file -> returns an error (do not silently wipe).
pub fn load_verifications(path: &Path) -> Result<PricingVerifications, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PricingVerifications::default());
        }
        Err(e) => {
            return Err(format!(
                "cannot read pricing verifications `{}`: {e}",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text)
        .map_err(|e| format!("malformed pricing verifications `{}`: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Legacy config merge (migration input only)
// ---------------------------------------------------------------------------

/// For each `(selector, date)` in `v` whose selector is NOT already a key in
/// `config.cache_pricing`, validate the date and insert a pure verification
/// override (`verified_at = Some(date)`, all value fields `None`). Entries
/// with a malformed date are skipped and their selectors are returned so the
/// caller can warn. Config.toml entries always win (selectors already present
/// in `config.cache_pricing` are skipped silently -- not reported).
pub fn merge_verifications_into(config: &mut Config, v: &PricingVerifications) -> Vec<String> {
    let mut skipped: Vec<String> = Vec::new();
    for (selector, date) in &v.verified {
        if config.cache_pricing.contains_key(selector) {
            continue;
        }
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
            skipped.push(selector.clone());
            continue;
        }
        config.cache_pricing.insert(
            selector.clone(),
            CachePricingOverride {
                verified_at: Some(date.clone()),
                ..Default::default()
            },
        );
    }
    skipped
}

/// Resolve the sidecar path, load, and merge into `config`. A missing file
/// is silently ignored (first run). A malformed sidecar JSON logs a warning
/// and skips the merge. Individual entries with a malformed date are dropped
/// with a per-entry warning.
///
/// Called ONLY by the v1 -> v2 config migration path (`server::
/// load_effective_config`, gated on `config.version < CURRENT_CONFIG_VERSION`)
/// so any historical sidecar stamp reaches the migrator's `cache_pricing`
/// input exactly once, before it folds into the catalog overlay.
pub fn load_and_merge_verifications(config: &mut Config) {
    let path = verifications_path();
    match load_verifications(&path) {
        Ok(v) => {
            let skipped = merge_verifications_into(config, &v);
            for sel in &skipped {
                tracing::warn!(
                    selector = %sel,
                    "pricing verification for `{sel}` has a malformed date and was ignored"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "pricing verifications sidecar could not be loaded; skipping merge"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Effective-catalog display (two-layer merge)
// ---------------------------------------------------------------------------

/// Table column header, in order.
const HEADER: &[&str] = &[
    "provider_kind",
    "model_glob",
    "status",
    "tier",
    "wm",
    "rm",
    "ttl(s)",
    "min_prefix",
    "auto",
    "max_ctx",
    "source",
    "verified_at",
    "stale",
];

/// Split a `"provider_kind:model_glob"` selector key for display. Every key
/// in this table is drawn from the baked table or a loaded overlay, both of
/// which are already selector-shaped, so a parse failure should not occur in
/// practice; falls back to `("?", key)` rather than panic on a hand-edited
/// overlay with a malformed key.
fn split_selector(key: &str) -> (String, String) {
    match CachePricingSelector::parse(key) {
        Ok(sel) => (sel.provider_kind, sel.model_glob),
        Err(_) => ("?".to_string(), key.to_string()),
    }
}

/// Provenance label for a `Present` row's winning layer.
const fn source_str(source: Source) -> &'static str {
    match source {
        Source::Baked => "baked",
        Source::Import => "import",
        Source::User => "user",
    }
}

/// Render one selector's [`EffectiveRow`] to the table's column order.
/// `DISABLED` and `MISSING` share the same conservative "nothing else to
/// show" shape (dashes for every economics / provenance column) -- the two
/// states share the same downstream sentinel treatment, so the display
/// treats them identically apart from the status label itself.
fn render_row(key: &str, effective: &EffectiveRow) -> Vec<String> {
    let (provider_kind, model_glob) = split_selector(key);
    match effective {
        EffectiveRow::Present {
            row,
            source,
            verified_at,
        } => {
            let stale = if is_stale_today(verified_at) {
                "WARN"
            } else {
                "-"
            };
            vec![
                provider_kind,
                model_glob,
                "PRESENT".to_string(),
                row.tier.unwrap_or("-").to_string(),
                format!("{:.4}", row.wm),
                format!("{:.4}", row.rm),
                row.ttl_seconds.to_string(),
                row.min_prefix_tokens.to_string(),
                if row.auto_cacher { "yes" } else { "no" }.to_string(),
                row.max_context_tokens
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
                source_str(*source).to_string(),
                verified_at.clone(),
                stale.to_string(),
            ]
        }
        EffectiveRow::Disabled => dashed_row(provider_kind, model_glob, "DISABLED"),
        EffectiveRow::Missing => dashed_row(provider_kind, model_glob, "MISSING"),
    }
}

/// A row whose only meaningful columns are the selector and `status`; every
/// economics / provenance column is a dash placeholder.
fn dashed_row(provider_kind: String, model_glob: String, status: &str) -> Vec<String> {
    let mut row = vec![provider_kind, model_glob, status.to_string()];
    row.extend(std::iter::repeat_n("-".to_string(), HEADER.len() - 3));
    row
}

/// Build the table rows and the punch-list of selectors whose EFFECTIVE
/// `max_context_tokens` is unknown (`Present` with a `None` window; a
/// `Disabled` / `Missing` selector carries no window to be unknown about,
/// so it is never punch-listed).
///
/// Rows are the two-layer merge ([`merge`]) of every selector key appearing
/// in EITHER the baked table or the loaded overlay -- the union, keyed
/// exactly as `"provider_kind:model_glob"`. A selector appearing in
/// neither layer is not a row here (there is nothing to enumerate it from);
/// [`EffectiveRow::Missing`] is reachable from this table only when an
/// overlay entry is later removed leaving a dangling reference elsewhere --
/// day-to-day, every displayed row backs onto at least one layer by
/// construction. See the `missing state renders` test below for the
/// direct classification/render coverage `MISSING` still needs.
pub fn build_list_data(overlay: &CatalogOverlay) -> (Vec<Vec<String>>, Vec<String>) {
    let baked = baked_table_rows();
    let mut baked_map: BTreeMap<String, &CatalogRow> = BTreeMap::new();
    for cell in &baked {
        baked_map.insert(
            format!("{}:{}", cell.provider_kind, cell.model_glob),
            &cell.row,
        );
    }

    let mut keys: BTreeSet<String> = baked_map.keys().cloned().collect();
    keys.extend(overlay.cells.keys().cloned());

    let mut rows = vec![HEADER.iter().map(|s| s.to_string()).collect()];
    let mut punch_set: BTreeSet<String> = BTreeSet::new();
    for key in &keys {
        let baked_row = baked_map.get(key).copied();
        let overlay_cell = overlay.cells.get(key);
        let effective = merge(baked_row, overlay_cell);
        if let Some(row) = effective.priced()
            && row.max_context_tokens.is_none()
        {
            punch_set.insert(key.clone());
        }
        rows.push(render_row(key, &effective));
    }

    (rows, punch_set.into_iter().collect())
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// `routectl pricing list` -- print the effective catalog (baked + overlay).
pub fn list(overlay: &CatalogOverlay) -> Result<(), Box<dyn std::error::Error>> {
    let (table, punch_list) = build_list_data(overlay);
    print!("{}", render_table(&table));
    if !punch_list.is_empty() {
        println!(
            "\npunch-list: {} selector(s) with an unknown max_context_tokens \
             (context-fraction advisory falls back to absolute tokens only):",
            punch_list.len()
        );
        for name in &punch_list {
            println!("  {name}");
        }
    }
    Ok(())
}

/// `routectl pricing verify <selector>` -- stamp an existing overlay cell
/// verified today. Resolves the default overlay path; see [`verify_at`] for
/// the testable core.
pub fn verify(selector_raw: &str) -> Result<(), Box<dyn std::error::Error>> {
    verify_at(selector_raw, &overlay_default_path())
}

/// Core of [`verify`], taking the overlay path explicitly so tests can point
/// it at a temp directory instead of the real `catalog_overlay.json`.
///
/// Verifying is a USER act: an existing cell -- whichever layer wrote it --
/// is rewritten with `source: user` and `verified_at` bumped to today; every
/// other field on the cell is carried through unchanged. A selector with no
/// overlay cell (baked-only, or entirely unknown to both layers) has nothing
/// to stamp: creating a NEW overlay cell is a later import/set verb, so this
/// returns a clear error instead of silently pinning the current effective
/// values. A selector whose overlay cell is explicitly `null` (disabled) is
/// likewise nothing to stamp -- verify never resurrects a disabled row.
fn verify_at(selector_raw: &str, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    CachePricingSelector::parse(selector_raw).map_err(|e| format!("invalid selector: {e}"))?;

    let overlay = load_catalog_overlay(path)?;
    let cell = match overlay.cells.get(selector_raw) {
        None => {
            return Err(format!(
                "no overlay cell for selector `{selector_raw}`; nothing to stamp (baked-only or \
                 unknown to the catalog) -- creating a new overlay cell is a later import/set verb"
            )
            .into());
        }
        Some(None) => {
            return Err(format!(
                "selector `{selector_raw}` is disabled in the overlay (null); nothing to stamp"
            )
            .into());
        }
        Some(Some(cell)) => cell,
    };

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let stamped = OverlayCell {
        source: OverlaySource::User,
        verified_at: today.clone(),
        wm: cell.wm,
        rm: cell.rm,
        ttl_seconds: cell.ttl_seconds,
        min_prefix_tokens: cell.min_prefix_tokens,
        max_context_tokens: cell.max_context_tokens,
        capabilities: cell.capabilities.clone(),
    };
    let mut cells = overlay.cells.clone();
    cells.insert(selector_raw.to_string(), Some(stamped));
    save_catalog_overlay(path, overlay.revision, cells)?;

    println!(
        "verified: selector={selector_raw}  date={today}  source=user  written to {}",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Table renderer (local copy; do not share the private fn from usage.rs)
// ---------------------------------------------------------------------------

/// Left-align column 0, right-align the rest, padded to the widest cell in
/// each column. ASCII spaces only. Callers must pass rows of uniform column
/// count; a ragged row renders misaligned (never panics).
fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i == 0 {
                line.push_str(&format!("{cell:<width$}", width = widths[i]));
            } else {
                line.push_str(&format!("{cell:>width$}", width = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Legacy sidecar: read side only (round-trips a manually-written file --
    // the write side that used to produce it is gone).
    // -----------------------------------------------------------------------

    #[test]
    fn load_verifications_reads_a_manually_written_sidecar_file() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing_verifications.json");
        std::fs::write(
            &path,
            r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
        )
        .unwrap();

        // Act
        let loaded = load_verifications(&path).unwrap();

        // Assert
        assert_eq!(
            loaded
                .verified
                .get("openai-compat:grok-*")
                .map(String::as_str),
            Some("2026-06-30")
        );
    }

    #[test]
    fn load_verifications_missing_path_returns_empty_default() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");

        // Act
        let result = load_verifications(&path);

        // Assert -- not an error; the map is empty
        assert!(result.is_ok());
        assert!(result.unwrap().verified.is_empty());
    }

    #[test]
    fn load_verifications_malformed_json_returns_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not valid json {{{").unwrap();

        // Act
        let result = load_verifications(&path);

        // Assert
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("malformed"), "expected 'malformed' in: {msg}");
    }

    // -----------------------------------------------------------------------
    // merge_verifications_into: additive and config wins
    // -----------------------------------------------------------------------

    #[test]
    fn merge_adds_new_selector_with_verified_at_only() {
        // Arrange
        let mut config = minimal_config();
        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: the key was inserted as a pure verification override
        assert!(skipped.is_empty(), "no entries should be skipped");
        let ov = config
            .cache_pricing
            .get("openai-compat:grok-*")
            .expect("key should be inserted");
        assert_eq!(
            ov.verified_at.as_deref(),
            Some("2026-06-30"),
            "verified_at should be set"
        );
        assert!(ov.wm.is_none(), "wm should be None (pure verification)");
        assert!(ov.rm.is_none(), "rm should be None");
        assert!(ov.ttl_seconds.is_none(), "ttl_seconds should be None");
        assert!(
            ov.min_prefix_tokens.is_none(),
            "min_prefix_tokens should be None"
        );
    }

    #[test]
    fn merge_does_not_overwrite_existing_config_key() {
        // Arrange: the config already has an entry for this selector
        let mut config = minimal_config();
        let existing = CachePricingOverride {
            wm: Some(1.5),
            verified_at: Some("2025-01-01".to_string()),
            override_acknowledges_cost_risk: true,
            ..Default::default()
        };
        config
            .cache_pricing
            .insert("openai-compat:grok-*".to_string(), existing);

        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: the config entry is unchanged; existing key not in skipped
        assert!(
            skipped.is_empty(),
            "config-key wins should not appear in skipped"
        );
        let ov = config
            .cache_pricing
            .get("openai-compat:grok-*")
            .expect("key should still be present");
        assert_eq!(
            ov.verified_at.as_deref(),
            Some("2025-01-01"),
            "config entry should not be overwritten by sidecar"
        );
        assert_eq!(ov.wm, Some(1.5), "wm should be unchanged");
    }

    #[test]
    fn merge_skips_malformed_date_and_inserts_valid_sibling() {
        // Arrange: one bad date, one good date
        let mut config = minimal_config();
        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-13-99".to_string());
        v.verified.insert(
            "openai-compat:mistral-*".to_string(),
            "2026-06-30".to_string(),
        );

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: malformed-date entry is skipped and reported
        assert_eq!(skipped, vec!["openai-compat:grok-*".to_string()]);
        assert!(
            !config.cache_pricing.contains_key("openai-compat:grok-*"),
            "malformed entry should not be inserted"
        );
        assert!(
            config.cache_pricing.contains_key("openai-compat:mistral-*"),
            "valid sibling should be inserted"
        );
    }

    // -----------------------------------------------------------------------
    // build_list_data: PRESENT (baked / import / user), DISABLED, punch-list
    // -----------------------------------------------------------------------

    #[test]
    fn present_baked_row_shows_baked_source_and_no_stale_warn() {
        // Arrange: no overlay entry -- the baked cell wins as-is.
        let overlay = CatalogOverlay::default();

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("baked row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[10], "baked");
        assert_eq!(row[12], "-", "the fresh baked snapshot date is not stale");
    }

    #[test]
    fn present_import_cell_overrides_baked_and_shows_import_source() {
        // Arrange: an import cell for a real baked selector, overriding wm.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[4], "0.5000", "wm overridden by the import cell");
        assert_eq!(row[10], "import");
        assert_eq!(row[11], "2026-07-01");
    }

    #[test]
    fn present_user_cell_with_no_baked_match_renders_from_sentinel_base() {
        // Arrange: a user cell naming a selector no baked cell backs.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:totally-new-model-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(999),
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert: sentinel base (wm 2.0) with the user's min_prefix override.
        let row = find_row(&rows, "openai-compat", "totally-new-model-*").expect("row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[4], "2.0000", "sentinel wm as the base");
        assert_eq!(row[7], "999", "user min_prefix override applied");
        assert_eq!(row[10], "user");
    }

    #[test]
    fn disabled_cell_renders_disabled_status_regardless_of_baked() {
        // Arrange: a null overlay entry for a real baked selector.
        let mut overlay = CatalogOverlay::default();
        overlay
            .cells
            .insert("openai-compat:grok-*".to_string(), None);

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert: every economics / provenance column dashes out.
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[2], "DISABLED");
        for col in &row[3..] {
            assert_eq!(col, "-");
        }
    }

    #[test]
    fn stale_verified_at_renders_warn_marker() {
        // Arrange: an import cell stamped far in the past.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2020-01-01".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[12], "WARN");
    }

    #[test]
    fn round_trip_display_renders_import_user_and_disabled_states() {
        // Arrange: one import cell, one user cell, one null-disabled cell,
        // all round-tripped through the real overlay writer/loader.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        cells.insert(
            "anthropic-api:claude-opus-4-8*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(1024),
                max_context_tokens: Some(200_000),
                capabilities: None,
            }),
        );
        cells.insert("openai-compat:disabled-model".to_string(), None);
        routectl_router::save_catalog_overlay(&path, 0, cells).expect("save");

        // Act
        let overlay = load_catalog_overlay(&path).expect("load");
        let (rows, _) = build_list_data(&overlay);

        // Assert: all three states render distinctly.
        let import_row = find_row(&rows, "openai-compat", "grok-*").expect("import row");
        assert_eq!(import_row[2], "PRESENT");
        assert_eq!(import_row[10], "import");

        let user_row = find_row(&rows, "anthropic-api", "claude-opus-4-8*").expect("user row");
        assert_eq!(user_row[2], "PRESENT");
        assert_eq!(user_row[10], "user");

        let disabled_row =
            find_row(&rows, "openai-compat", "disabled-model").expect("disabled row");
        assert_eq!(disabled_row[2], "DISABLED");
    }

    #[test]
    fn missing_state_renders_missing_status() {
        // MISSING requires calling `merge` with neither layer present, which
        // cannot arise from `build_list_data`'s own baked-union-overlay key
        // enumeration (every enumerated key backs onto at least one layer by
        // construction). Exercised directly here so the render path's
        // MISSING arm is covered.
        let effective = merge(None, None);
        assert_eq!(effective, EffectiveRow::Missing);

        let row = render_row("openai-compat:nowhere-*", &effective);
        assert_eq!(row[2], "MISSING");
        for col in &row[3..] {
            assert_eq!(col, "-");
        }
    }

    #[test]
    fn punch_list_names_present_row_with_unknown_max_context_tokens() {
        // Arrange: the anthropic-api "*" catch-all is baked with an unknown
        // window; no overlay entry supplies one.
        let overlay = CatalogOverlay::default();

        // Act
        let (_, punch_list) = build_list_data(&overlay);

        // Assert
        assert!(
            punch_list.contains(&"anthropic-api:*".to_string()),
            "punch_list: {punch_list:?}"
        );
        assert!(
            !punch_list.contains(&"anthropic-api:claude-opus-4-8*".to_string()),
            "a known-window model must not appear: {punch_list:?}"
        );
    }

    #[test]
    fn punch_list_cleared_when_overlay_supplies_a_window() {
        // Arrange: an overlay cell supplies the missing window for the
        // otherwise-unknown anthropic-api "*" catch-all.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "anthropic-api:*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: Some(200_000),
                capabilities: None,
            }),
        );

        // Act
        let (_, punch_list) = build_list_data(&overlay);

        // Assert
        assert!(
            !punch_list.contains(&"anthropic-api:*".to_string()),
            "punch_list: {punch_list:?}"
        );
    }

    /// Locate a rendered row by `(provider_kind, model_glob)`.
    fn find_row<'a>(
        rows: &'a [Vec<String>],
        provider_kind: &str,
        model_glob: &str,
    ) -> Option<&'a Vec<String>> {
        rows.iter()
            .skip(1)
            .find(|r| r[0] == provider_kind && r[1] == model_glob)
    }

    // -----------------------------------------------------------------------
    // verify_at: stamps an EXISTING overlay cell -- verifying is a user
    // act; creating cells is a separate import/set concern.
    // -----------------------------------------------------------------------

    #[test]
    fn verify_at_stamps_existing_user_cell_updates_verified_at_only() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2020-01-01".to_string(),
                wm: Some(1.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(512),
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        verify_at("openai-compat:grok-*", &path).expect("verify");

        // Assert: source stays user, verified_at bumped to today, values kept.
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.verified_at, today, "verified_at bumped to today");
        assert_eq!(cell.wm, Some(1.5));
        assert_eq!(cell.min_prefix_tokens, Some(512));
    }

    #[test]
    fn verify_at_flips_import_cell_source_to_user() {
        // Arrange: an import cell -- verifying is a user act, so the source
        // flips even though the cell originated from an import.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-01-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        verify_at("openai-compat:grok-*", &path).expect("verify");

        // Assert
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.wm, Some(0.5), "value fields carry through unchanged");
    }

    #[test]
    fn verify_at_errors_when_no_overlay_cell_exists_for_selector() {
        // Arrange: an empty overlay (the selector is baked-only or unknown).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        save_catalog_overlay(&path, 0, BTreeMap::new()).expect("seed empty");

        // Act
        let err =
            verify_at("openai-compat:grok-*", &path).expect_err("no overlay cell must be an error");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("nothing to stamp"), "msg: {msg}");
        assert!(msg.contains("baked-only"), "msg: {msg}");
    }

    #[test]
    fn verify_at_errors_when_overlay_cell_is_disabled() {
        // Arrange: the selector is explicitly disabled (JSON null).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert("openai-compat:grok-*".to_string(), None);
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        let err =
            verify_at("openai-compat:grok-*", &path).expect_err("a disabled cell must be an error");

        // Assert: never resurrects a disabled row.
        let msg = err.to_string();
        assert!(msg.contains("disabled"), "msg: {msg}");
        let overlay = load_catalog_overlay(&path).expect("load");
        assert_eq!(
            overlay.cells.get("openai-compat:grok-*"),
            Some(&None),
            "the disabled cell must remain untouched"
        );
    }

    #[test]
    fn verify_at_rejects_malformed_selector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err =
            verify_at("no-colon-here", &path).expect_err("malformed selector must be rejected");
        assert!(err.to_string().contains("invalid selector"), "msg: {err}");
        assert!(!path.exists(), "nothing should have been written");
    }

    fn minimal_config() -> Config {
        let toml = r#"
[server]
host = "127.0.0.1"
port = 4000
"#;
        toml::from_str(toml).expect("minimal config should parse")
    }
}
