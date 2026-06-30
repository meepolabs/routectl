//! `routectl pricing` -- inspect and stamp the cache-economics pricing manifest.
//!
//! Two subcommands:
//!   list    -- print the effective manifest (baked rows merged with operator overrides
//!              and sidecar verifications) as an aligned ASCII table.
//!   verify  -- stamp a baked cell verified as of today, persisting the stamp to a
//!              machine-managed sidecar file (`pricing_verifications.json` in the
//!              routectl config dir).
//!
//! The sidecar is purely additive: it only inserts a `verified_at`-only override for
//! selectors that have no existing entry in `config.cache_pricing`. If the operator
//! has a `[cache_pricing]` entry for the same key, that entry wins (config.toml beats
//! the sidecar at merge time).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use routectl_router::{
    baked_table_rows, is_stale_today, BakedPricingRow, CachePricingOverride, CachePricingSelector,
    Config,
};

// ---------------------------------------------------------------------------
// Sidecar store
// ---------------------------------------------------------------------------

/// On-disk shape for `pricing_verifications.json`.
///
/// Uses a wrapper struct (not a bare map) so future fields can be added
/// without a format break.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingVerifications {
    /// Maps a selector string (`"provider_kind:model_glob"`) to a
    /// verification date (`"YYYY-MM-DD"`).
    #[serde(default)]
    pub verified: BTreeMap<String, String>,
}

/// Path to the sidecar file. Mirrors the `resolve_config_path` dir logic
/// in `main.rs`.
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

/// Save a single verification entry atomically (load-modify-save).
/// Validates the selector and date before persisting so the sidecar
/// cannot be poisoned by malformed input. Ensures the parent directory
/// exists. Uses a PID-suffixed temp-then-rename pattern so the file is
/// never partially written and concurrent writers cannot clobber each
/// other.
pub fn save_verification(
    path: &Path,
    selector: &str,
    date: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    CachePricingSelector::parse(selector)
        .map_err(|e| format!("invalid selector `{selector}`: {e}"))?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|e| format!("invalid date `{date}`: {e}"))?;

    let mut v = load_verifications(path)?;
    v.verified.insert(selector.to_string(), date.to_string());
    save_verifications_atomic(path, &v)?;
    Ok(())
}

fn save_verifications_atomic(
    path: &Path,
    v: &PricingVerifications,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;

    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;

    let json = serde_json::to_string_pretty(v)
        .map_err(|e| format!("serialize pricing verifications: {e}"))?;

    // PID-suffixed temp name in the same directory: rename is atomic
    // within one filesystem and concurrent writers cannot clobber each
    // other.
    let tmp_path = parent.join(format!(
        ".pricing_verifications.{}.tmp.json",
        std::process::id()
    ));
    std::fs::write(&tmp_path, json.as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp_path.display(), path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Config merge
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
// Effective-row computation (pure, testable)
// ---------------------------------------------------------------------------

/// Compute the effective `CachePricingRow` for a baked cell against the
/// operator's config. Falls back to the baked row if the override fails
/// validation.
fn effective_row(cell: &BakedPricingRow, config: &Config) -> routectl_router::CachePricingRow {
    let key = format!("{}:{}", cell.provider_kind, cell.model_glob);
    match config.cache_pricing.get(&key) {
        Some(ov) => cell.row.with_overrides(ov).unwrap_or(cell.row),
        None => cell.row,
    }
}

/// True when `selector` exactly names a baked cell
/// (provider_kind AND model_glob both match).
fn names_baked_cell(sel: &CachePricingSelector) -> bool {
    baked_table_rows()
        .iter()
        .any(|cell| cell.provider_kind == sel.provider_kind && cell.model_glob == sel.model_glob)
}

/// Build the table rows and the count of config keys that do not
/// exactly match any baked cell. Returns `(table_rows, unmatched_count)`.
///
/// Each element of `table_rows` is a `Vec<String>` of column values in the
/// order: provider_kind, model_glob, tier, wm, rm, ttl(s), min_prefix, auto,
/// verified, source, verified_at, stale.
pub fn build_list_data(config: &Config) -> (Vec<Vec<String>>, usize) {
    let header = vec![
        "provider_kind".to_string(),
        "model_glob".to_string(),
        "tier".to_string(),
        "wm".to_string(),
        "rm".to_string(),
        "ttl(s)".to_string(),
        "min_prefix".to_string(),
        "auto".to_string(),
        "verified".to_string(),
        "source".to_string(),
        "verified_at".to_string(),
        "stale".to_string(),
    ];

    let baked = baked_table_rows();
    let mut rows = vec![header];
    for cell in &baked {
        let row = effective_row(cell, config);
        let stale = if row.verified && is_stale_today(row.verified_at) {
            "STALE"
        } else {
            ""
        };
        rows.push(vec![
            cell.provider_kind.to_string(),
            cell.model_glob.to_string(),
            row.tier.unwrap_or("-").to_string(),
            format!("{:.4}", row.wm),
            format!("{:.4}", row.rm),
            row.ttl_seconds.to_string(),
            row.min_prefix_tokens.to_string(),
            if row.auto_cacher { "yes" } else { "no" }.to_string(),
            row.verified.to_string(),
            row.source.to_string(),
            row.verified_at.to_string(),
            stale.to_string(),
        ]);
    }

    let baked_keys: std::collections::BTreeSet<String> = baked
        .iter()
        .map(|cell| format!("{}:{}", cell.provider_kind, cell.model_glob))
        .collect();
    let unmatched = config
        .cache_pricing
        .keys()
        .filter(|k| !baked_keys.contains(*k))
        .count();

    (rows, unmatched)
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// `routectl pricing list` -- print the effective pricing manifest.
pub fn list(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let (table, unmatched) = build_list_data(config);
    print!("{}", render_table(&table));
    if unmatched > 0 {
        println!(
            "\nnote: {unmatched} override/verification selector(s) do not exactly name a baked \
             cell; they still apply by glob at request-lookup time but are not reflected in the \
             rows above."
        );
    }
    Ok(())
}

/// `routectl pricing verify <selector>` -- stamp a baked cell verified today.
pub fn verify(selector_raw: &str) -> Result<(), Box<dyn std::error::Error>> {
    let sel =
        CachePricingSelector::parse(selector_raw).map_err(|e| format!("invalid selector: {e}"))?;

    if !names_baked_cell(&sel) {
        println!(
            "note: selector `{selector_raw}` matches no baked cell; \
             the verification will apply by glob at lookup time"
        );
    }

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = verifications_path();
    save_verification(&path, selector_raw, &today)?;

    println!(
        "verified: selector={selector_raw}  date={today}  written to {}",
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
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
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
    // Sidecar round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn sidecar_save_then_load_round_trips_stamped_entry() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing_verifications.json");

        // Act
        save_verification(&path, "openai-compat:grok-*", "2026-06-30").unwrap();
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

    // -----------------------------------------------------------------------
    // load_verifications on missing path returns Default
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // load_verifications on malformed JSON returns error
    // -----------------------------------------------------------------------

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
            .insert("openai-compat:grok-*".to_string(), existing.clone());

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
    // Effective-row helper: baked unverified cell + pure verification override
    // -----------------------------------------------------------------------

    #[test]
    fn effective_row_with_verification_override_sets_verified_and_source() {
        // Arrange: find an unverified baked cell to use as the base
        // (or use grok-* which is known unverified in the baked table)
        let baked = baked_table_rows();
        let cell = baked
            .iter()
            .find(|c| c.provider_kind == "openai-compat" && c.model_glob == "grok-*")
            .expect("expected openai-compat:grok-* in baked table");

        let mut config = minimal_config();
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                verified_at: Some("2026-06-30".to_string()),
                ..Default::default()
            },
        );

        // Act
        let row = effective_row(cell, &config);

        // Assert
        assert!(
            row.verified,
            "should be verified after pure-verification override"
        );
        assert_eq!(
            row.source, "operator-verified",
            "source should be operator-verified"
        );
    }

    #[test]
    fn effective_row_without_override_returns_baked_row_unchanged() {
        // Arrange
        let baked = baked_table_rows();
        let cell = baked
            .iter()
            .find(|c| c.provider_kind == "openai-compat" && c.model_glob == "grok-*")
            .expect("expected openai-compat:grok-* in baked table");

        let config = minimal_config();

        // Act
        let row = effective_row(cell, &config);

        // Assert: the row is identical to the baked row
        assert_eq!(row.wm, cell.row.wm);
        assert_eq!(row.rm, cell.row.rm);
        assert_eq!(row.verified, cell.row.verified);
        assert_eq!(row.source, cell.row.source);
    }

    // -----------------------------------------------------------------------
    // names_baked_cell predicate
    // -----------------------------------------------------------------------

    #[test]
    fn names_baked_cell_true_for_known_entry() {
        // Arrange
        let sel = CachePricingSelector::parse("openai-compat:grok-*").unwrap();

        // Act / Assert
        assert!(
            names_baked_cell(&sel),
            "openai-compat:grok-* should be in the baked table"
        );
    }

    #[test]
    fn names_baked_cell_false_for_unknown_entry() {
        // Arrange
        let sel = CachePricingSelector::parse("openai-compat:totally-made-up-*").unwrap();

        // Act / Assert
        assert!(
            !names_baked_cell(&sel),
            "totally-made-up-* should not be in the baked table"
        );
    }

    // -----------------------------------------------------------------------
    // build_list_data round-trip: merged verification shows verified/source/date
    // -----------------------------------------------------------------------

    #[test]
    fn build_list_data_merged_verification_shows_verified_operator_source_and_date() {
        // Arrange: merge a verification for grok-* into an otherwise empty config
        let mut config = minimal_config();
        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());
        merge_verifications_into(&mut config, &v);

        // Act
        let (rows, _) = build_list_data(&config);

        // Find the grok-* row (skip header at index 0)
        let grok_row = rows
            .iter()
            .skip(1)
            .find(|r| r[0] == "openai-compat" && r[1] == "grok-*")
            .expect("grok-* row should be present");

        // verified col = index 8, source = 9, verified_at = 10
        assert_eq!(grok_row[8], "true", "verified should be true");
        assert_eq!(
            grok_row[9], "operator-verified",
            "source should be operator-verified"
        );
        assert_eq!(grok_row[10], "2026-06-30", "verified_at should match stamp");
    }

    // -----------------------------------------------------------------------
    // stale rendering: far-past verified_at -> STALE; recent -> empty
    // -----------------------------------------------------------------------

    #[test]
    fn build_list_data_stale_column_reflects_staleness() {
        // Arrange: inject a verified override with a date from 1971 (always stale)
        let mut config = minimal_config();
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                verified_at: Some("1971-01-01".to_string()),
                ..Default::default()
            },
        );

        // Act
        let (rows, _) = build_list_data(&config);
        let grok_row = rows
            .iter()
            .skip(1)
            .find(|r| r[0] == "openai-compat" && r[1] == "grok-*")
            .expect("grok-* row should be present");

        // stale col = index 11
        assert_eq!(grok_row[11], "STALE", "1971-01-01 should be STALE");

        // Arrange: fresh date should not be STALE
        let mut config2 = minimal_config();
        config2.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            CachePricingOverride {
                verified_at: Some("2099-01-01".to_string()),
                ..Default::default()
            },
        );
        let (rows2, _) = build_list_data(&config2);
        let grok_row2 = rows2
            .iter()
            .skip(1)
            .find(|r| r[0] == "openai-compat" && r[1] == "grok-*")
            .expect("grok-* row should be present");
        assert_eq!(grok_row2[11], "", "future date should not show STALE");
    }

    // -----------------------------------------------------------------------
    // unmatched-note count: glob key naming no baked cell -> count == 1
    // -----------------------------------------------------------------------

    #[test]
    fn build_list_data_unmatched_count_for_glob_naming_no_baked_cell() {
        // Arrange: a config key that names no baked cell
        let mut config = minimal_config();
        config.cache_pricing.insert(
            "openai-compat:totally-made-up-*".to_string(),
            CachePricingOverride::default(),
        );

        // Act
        let (_, unmatched) = build_list_data(&config);

        // Assert
        assert_eq!(
            unmatched, 1,
            "one override naming no baked cell -> unmatched == 1"
        );
    }

    // -----------------------------------------------------------------------
    // save_verification rejects bad selector and bad date
    // -----------------------------------------------------------------------

    #[test]
    fn save_verification_rejects_malformed_date() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing_verifications.json");

        // Act
        let result = save_verification(&path, "openai-compat:grok-*", "not-a-date");

        // Assert
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid date"),
            "expected 'invalid date' in: {msg}"
        );
        assert!(!path.exists(), "sidecar should not have been written");
    }

    #[test]
    fn save_verification_rejects_bad_selector() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing_verifications.json");

        // Act: selector missing the colon
        let result = save_verification(&path, "no-colon-here", "2026-06-30");

        // Assert
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid selector"),
            "expected 'invalid selector' in: {msg}"
        );
        assert!(!path.exists(), "sidecar should not have been written");
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn minimal_config() -> Config {
        let toml = r#"
[server]
host = "127.0.0.1"
port = 4000
"#;
        toml::from_str(toml).expect("minimal config should parse")
    }
}
