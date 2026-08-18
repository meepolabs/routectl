//! The migrating open every ledger-reading bootstrap warm performs before its
//! first query.
//!
//! # Why a warm migrates for itself
//!
//! A warm reads the ledger through a read-only open, and that open rejects a
//! file whose `PRAGMA user_version` is not exactly this binary's schema version
//! -- so against a not-yet-migrated file the read yields ZERO rows. The usage
//! WRITER owns the migrating open in normal operation, but it performs it on
//! its own spawned thread, so a warm sequenced after the writer starts would
//! only trade a certain silent miss for a race.
//!
//! Every warm that reads the ledger therefore runs the migrating open ITSELF,
//! synchronously, before its first query, and is wired at bootstrap BEFORE the
//! writer starts. The migration ladder is guarded and idempotent, so the
//! writer's own later open finds nothing left to do; and because nothing else
//! holds the DB at that point in bootstrap, the migration contends with no one.
//!
//! The failure mode this closes is silent by construction: a warm reading a
//! stale schema loads no rows, and zero rows is indistinguishable from a
//! legitimately empty ledger -- every lane comes back cold with nothing logged,
//! so the loss reads as health.

use std::path::Path;

use routectl_usage::open;

/// Bring the ledger at `db_path` to the current schema, reporting whether the
/// caller's warm may proceed to its read.
///
/// `warm` names the calling warm and `consequence` states what skipping it
/// costs; both ride the failure log as fields so one message serves every warm.
///
/// A failure here is never a bootstrap failure: it is logged at `debug` and the
/// warm is skipped. The usage writer hits the same failure on its own open and
/// reports it at `error` from the surface that owns the ledger's health, so a
/// second loud line here would only double-report it.
pub(super) fn migrate_before_warm(
    db_path: &Path,
    warm: &'static str,
    consequence: &'static str,
) -> bool {
    match open(db_path) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(
                db_path = %db_path.display(),
                error = %e,
                warm,
                consequence,
                "usage db could not be brought to the current schema; skipping startup warm"
            );
            false
        }
    }
}

#[cfg(test)]
#[path = "warm_open_tests.rs"]
mod tests;
