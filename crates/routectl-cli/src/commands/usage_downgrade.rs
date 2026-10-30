//! `routectl usage downgrade --to 17|16`: hand the usage DB back to an earlier
//! binary.
//!
//! Offline only. The usage crate's `downgrade_to` refuses while any other
//! connection -- the daemon's writer included -- has the file open, runs every
//! step from the file's version down to the target in one transaction (each
//! verifies its tables are exactly the additive shape, then re-stamps both
//! version markers one lower), and drops nothing. This module only renders
//! the outcome.

use std::path::Path;

use routectl_usage::{DowngradeError, downgrade_to};

/// The target versions this build can downgrade to.
pub use routectl_usage::SUPPORTED_TARGETS;

/// Run the downgrade of `db_path` to `target`, returning the line to print on
/// success.
///
/// # Errors
///
/// Returns the usage crate's refusal or failure unchanged; every error leaves
/// the file as it was.
pub fn run(db_path: &Path, target: i64) -> Result<String, DowngradeError> {
    downgrade_to(db_path, target)?;
    Ok(format!(
        "usage db {} is now schema v{target}; a v{target} binary can open it. \
         Opening it with this binary migrates it forward again.",
        db_path.display()
    ))
}

#[cfg(test)]
#[path = "usage_downgrade_tests.rs"]
mod tests;
