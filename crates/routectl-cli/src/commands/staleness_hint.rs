//! `routectl catalog import` staleness hint: a one-line note nudging the
//! operator to refresh a catalog overlay whose freshest `verified_at` stamp
//! has aged past the configured `[capability] staleness_hint_days` horizon.
//!
//! Three testable pieces:
//! - [`staleness_hint_line`] is the pure age check plus message.
//! - [`freshest_verified_at`] picks the most recent stamp across the overlay's
//!   present cells (disabled cells carry no stamp).
//! - [`emit_staleness_hint`] is the guarded seam that takes every gate as an
//!   injected boolean so each gate is pinned in isolation. The seam writes
//!   ONLY to the passed stderr handle and NEVER to stdout, and stays silent
//!   on JSON output, a non-terminal stderr, CI, or the kill switch.
//!
//! Kill switch: set `ROUTECTL_NO_STALENESS_HINT` (to any value) to suppress
//! the hint unconditionally. The hint is also suppressed when `CI` is set,
//! when stderr is not a terminal, and on any `--json` output path.

use std::io::Write;

use routectl_router::{CatalogOverlay, is_stale_days};

/// Build the staleness note when `verified_at` (a `YYYY-MM-DD` overlay stamp)
/// is more than `threshold_days` before `today_epoch_days`. Returns `None`
/// when the stamp is within the horizon. Delegates the age check to
/// [`routectl_router::is_stale_days`] so the boundary matches the catalog's
/// own staleness rule exactly (strict greater-than: fresh AT the threshold,
/// stale one day past it; a malformed stamp reads as stale).
#[must_use]
pub fn staleness_hint_line(
    verified_at: &str,
    threshold_days: i64,
    today_epoch_days: i64,
) -> Option<String> {
    if is_stale_days(verified_at, today_epoch_days, threshold_days) {
        Some(format!(
            "note: catalog overlay last verified {verified_at} is more than \
             {threshold_days} days old; run `routectl catalog import` to refresh it"
        ))
    } else {
        None
    }
}

/// The most recent `verified_at` stamp across the overlay's PRESENT cells, or
/// `None` when the overlay holds no present cell (empty, or every cell
/// disabled). `YYYY-MM-DD` stamps order chronologically under byte
/// comparison, so the lexicographic max is the freshest date.
#[must_use]
pub fn freshest_verified_at(overlay: &CatalogOverlay) -> Option<String> {
    overlay
        .cells
        .values()
        .filter_map(|cell| cell.as_ref())
        .map(|cell| cell.verified_at.as_str())
        .max()
        .map(str::to_owned)
}

/// Guarded stderr emission of the staleness hint. Every gate is an injected
/// boolean so each can be pinned in isolation: the hint is suppressed on JSON
/// output, a non-terminal stderr, CI, or the kill switch, and otherwise
/// written to `stderr` only (never stdout) when [`staleness_hint_line`]
/// yields a note. A write failure is ignored -- a best-effort advisory never
/// fails the command it rides on.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn emit_staleness_hint(
    stderr: &mut impl Write,
    verified_at: &str,
    threshold_days: i64,
    today_epoch_days: i64,
    is_tty: bool,
    is_ci: bool,
    kill_switch: bool,
    is_json: bool,
) {
    if is_json || !is_tty || is_ci || kill_switch {
        return;
    }
    if let Some(line) = staleness_hint_line(verified_at, threshold_days, today_epoch_days) {
        let _ = writeln!(stderr, "{line}");
    }
}

#[cfg(test)]
#[path = "staleness_hint_tests.rs"]
mod tests;
