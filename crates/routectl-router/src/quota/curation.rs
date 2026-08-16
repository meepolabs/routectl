//! The curated per-provider window table: which upstream window means what,
//! how long it runs, and where its threshold sits.
//!
//! # Why one table of one row type
//!
//! Curation is a CLOSED SET, and the properties that matter are properties of
//! a whole row rather than of any one column. A row that named a role without
//! a duration could not have its reset bounded; a role paired with the wrong
//! duration would bound it against the wrong window; a threshold split into a
//! second table keyed by role would admit a "role with no threshold" state
//! nobody wants, and a join is a silent way to get the pairing wrong. So all
//! five facts a curation entry carries -- provider kind, upstream window id,
//! role, expected duration, threshold -- live on ONE row of ONE named struct,
//! and the invariants below are checked over the table as a whole.
//!
//! The discipline follows the shipped closed-set translation tables in this
//! crate: capture-grounded entries only, an unknown key yields nothing rather
//! than a guess, and a provider with no row is DORMANT by construction. The
//! shape does not: a two-column tuple cannot carry five facts, and cannot stop
//! a role, a duration and a threshold from being mismatched.
//!
//! # Why the absence of a row is data
//!
//! Codex has no FAST row, and that is the point of putting curation in a table
//! an auditor can read. On the only Codex evidence routectl has captured, the
//! window the upstream calls `primary` runs 10080 minutes -- seven days, a
//! SLOW window by any honest reading -- and the secondary window is declared
//! unused. There is no short recovering window to curate, so Codex gets no
//! FAST row and its FAST cap is DORMANT. Reading a role off the NAME `primary`
//! would invent a placement signal from a word.

use std::time::Duration;

use super::window::WindowRole;

/// Provider kind of the Anthropic subscription egress, as the rest of the
/// crate spells it.
pub const ANTHROPIC_PROVIDER_KIND: &str = "anthropic-api";

/// Provider kind of the Codex subscription egress.
pub const CODEX_PROVIDER_KIND: &str = "openai-responses";

/// Upstream id of Anthropic's five-hour window, whose utilization is the one
/// suffix the shipped header parser types.
pub const ANTHROPIC_FAST_SOURCE_ID: &str = "5h";

/// How much slack a reported reset gets beyond its window's own duration.
///
/// Shared by every row rather than curated per row, and deliberately so: it
/// absorbs upstream clock skew and rounding, which are properties of the
/// comparison rather than of any one window. Curating it per row would add a
/// sixth fact whose only reachable use is widening a bound, and a tolerance
/// wider than the window it guards would readmit a reset from a LONGER window
/// -- so the table self-tests hold it below every row's duration.
pub const RESET_TOLERANCE: Duration = Duration::from_mins(5);

/// One curated upstream window.
///
/// Every field is load-bearing at a different site, which is why they travel
/// together: `provider_kind` + `source_id` are how a reducer finds the row for
/// a header it just read, `role` is what the placement partition keys on,
/// `duration` is what bounds the reported reset (a window cannot reset further
/// out than its own length), and `threshold` is where that role's cap or guard
/// sits.
#[derive(Debug, Clone, PartialEq)]
pub struct CuratedWindow {
    /// Provider kind emitting this window. A provider with no row here is
    /// dormant: nothing looks it up, so nothing caps it.
    pub provider_kind: &'static str,
    /// The upstream's own id for the window, as it appears in the header
    /// suffix (`5h`, `7d`, `primary`).
    pub source_id: &'static str,
    /// What this window means for a placement decision.
    pub role: WindowRole,
    /// How long the window runs upstream. The reset-plausibility bound needs
    /// it, which is why it is curated beside the role instead of written at a
    /// call site where the two could drift apart.
    pub duration: Duration,
    /// Utilization at or above which this role's cap (FAST) or guard (SLOW)
    /// engages, as a fraction of the window.
    pub threshold: f64,
}

/// The closed curated set. Every row is grounded in a captured envelope from
/// the provider it names; nothing here is inferred from a window's name.
///
/// Anthropic reports a five-hour window that recovers inside the lifetime of a
/// conversation (FAST, the placement signal, smoothed at half full) and a
/// seven-day window where being nearly full is a durable fact about the seat
/// (SLOW, a near-exhaustion guard). Codex reports one seven-day window and
/// declares its second unused, so it gets the SLOW row and no FAST one.
const CURATED_WINDOWS: &[CuratedWindow] = &[
    CuratedWindow {
        provider_kind: ANTHROPIC_PROVIDER_KIND,
        source_id: ANTHROPIC_FAST_SOURCE_ID,
        role: WindowRole::Fast,
        duration: Duration::from_hours(5),
        threshold: 0.5,
    },
    CuratedWindow {
        provider_kind: ANTHROPIC_PROVIDER_KIND,
        source_id: "7d",
        role: WindowRole::Slow,
        duration: Duration::from_hours(24 * 7),
        threshold: 0.9,
    },
    CuratedWindow {
        provider_kind: CODEX_PROVIDER_KIND,
        source_id: "primary",
        role: WindowRole::Slow,
        duration: Duration::from_hours(24 * 7),
        threshold: 0.9,
    },
];

/// Every curated window for one provider kind, in table order.
///
/// An uncurated provider kind yields an EMPTY iterator rather than a default
/// row, so a provider routectl has not captured and curated runs dormant
/// instead of being capped against a guessed window.
pub fn rows_for(provider_kind: &str) -> impl Iterator<Item = &'static CuratedWindow> {
    CURATED_WINDOWS
        .iter()
        .filter(move |row| row.provider_kind == provider_kind)
}

/// The curated row for one provider kind and role, or `None` when that
/// provider curates no window in that role.
///
/// `None` is the dormant answer, not an error: it is how "Codex has no FAST
/// window" reaches a caller.
pub fn row_for(provider_kind: &str, role: &WindowRole) -> Option<&'static CuratedWindow> {
    rows_for(provider_kind).find(|row| &row.role == role)
}

#[cfg(test)]
#[path = "curation_tests.rs"]
mod curation_tests;
