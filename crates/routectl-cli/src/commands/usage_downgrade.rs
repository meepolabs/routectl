//! `routectl usage downgrade --to 16`: hand a v17 usage DB back to a v16
//! binary.
//!
//! Offline only. The usage crate's `downgrade_to_v16` refuses while any other
//! connection -- the daemon's writer included -- has the file open, verifies the
//! file is exactly the additive v17 shape, and re-stamps both version markers
//! in one transaction. This module only resolves the path and renders the
//! outcome.

use std::path::Path;

use routectl_usage::{DowngradeError, downgrade_to_v16};

/// The only target version this build can downgrade to.
pub const SUPPORTED_TARGET: i64 = 16;

/// Run the downgrade against `db_path`, returning the line to print on
/// success.
///
/// # Errors
///
/// Returns the usage crate's refusal or failure unchanged; every error leaves
/// the file as it was.
pub fn run(db_path: &Path) -> Result<String, DowngradeError> {
    downgrade_to_v16(db_path)?;
    Ok(format!(
        "usage db {} is now schema v{SUPPORTED_TARGET}; a v{SUPPORTED_TARGET} binary can open it. \
         Opening it with this binary migrates it forward again.",
        db_path.display()
    ))
}

#[cfg(test)]
#[path = "usage_downgrade_tests.rs"]
mod tests;
