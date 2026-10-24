//! Legacy pricing-verification sidecar read.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
#[path = "verifications_tests.rs"]
mod tests;
