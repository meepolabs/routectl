//! Pricing-verification load/merge.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use routectl_router::{CachePricingOverride, Config};

use super::verifications_path;

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
/// Called ONLY by the v1 -> v2 config migration path (the `config migrate`
/// ladder) so any historical sidecar stamp reaches the migrator's
/// `cache_pricing` input exactly once, before it folds into the catalog
/// overlay. The config loader no longer runs the migration -- it rejects a
/// too-old config instead -- so this is not on any load path.
pub fn load_and_merge_verifications(config: &mut Config) {
    let path = verifications_path();
    match load_verifications(&path) {
        Ok(v) => {
            let skipped = merge_verifications_into(config, &v);
            for sel in &skipped {
                let selector_safe = routectl_core::sanitize_for_log(sel);
                tracing::warn!(
                    selector = %selector_safe,
                    "pricing verification for `{selector_safe}` has a malformed date and was \
                     ignored"
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

#[cfg(test)]
#[path = "verifications_tests.rs"]
mod tests;
