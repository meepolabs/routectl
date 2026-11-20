//! The curated per-provider window table: which upstream header window means
//! what, and how long it runs.
//!
//! # Why one table of one row type
//!
//! Curation is a CLOSED SET, and the properties that matter are properties of
//! a whole row rather than of any one column. A row that named a role without
//! a duration could not have its reset bounded; a role paired with the wrong
//! duration would bound it against the wrong window, and a join is a silent
//! way to get the pairing wrong. So all four facts a curation entry carries --
//! provider kind, upstream window id, role, expected duration -- live on ONE
//! row of ONE named struct, and the invariants below are checked over the
//! table as a whole.
//!
//! The discipline follows the shipped closed-set translation tables in this
//! crate: capture-grounded entries only, an unknown key yields nothing rather
//! than a guess. The shape does not: a two-column tuple cannot carry four
//! facts, and cannot stop a role and a duration from being mismatched.
//!
//! # Why the absence of a row is data
//!
//! Codex has no row, and that is the point of putting curation in a table an
//! auditor can read. Codex names its windows by slot (`primary`,
//! `secondary`), and a slot says nothing about length: one plan reports a
//! five-hour primary, another a seven-day one. Its windows reach the account
//! meter keyed by the length each reports (`codex_headers`), never through a
//! row. Reading a role off the NAME `primary` would invent a length from a
//! word.

use std::time::Duration;

use super::window::WindowRole;

/// Provider kind of the Anthropic subscription egress, as the rest of the
/// crate spells it.
pub const ANTHROPIC_PROVIDER_KIND: &str = routectl_core::provider_kind::config::ANTHROPIC_API;

/// Provider kind of the Codex subscription egress, which curates no row.
#[cfg(test)]
pub const CODEX_PROVIDER_KIND: &str = routectl_core::provider_kind::config::OPENAI_RESPONSES;

/// Upstream id of Anthropic's five-hour window -- the one CAPACITY-WINDOW
/// utilization the shipped header parser types (it types five other suffixes
/// too, none of them a capacity-window utilization).
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
/// a header it just read, `role` is which snapshot slot the window fills, and
/// `duration` is what bounds the reported reset (a window cannot reset further
/// out than its own length) and the length the meter keys it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedWindow {
    /// Provider kind emitting this window.
    pub provider_kind: &'static str,
    /// The upstream's own id for the window, as it appears in the header
    /// suffix (`5h`, `7d`).
    pub source_id: &'static str,
    /// Which header snapshot slot the window fills.
    pub role: WindowRole,
    /// How long the window runs upstream. The reset-plausibility bound needs
    /// it, which is why it is curated beside the role instead of written at a
    /// call site where the two could drift apart.
    pub duration: Duration,
}

/// The closed curated set. Every row is grounded in a captured envelope from
/// the provider it names; nothing here is inferred from a window's name.
///
/// Anthropic's response headers report a five-hour window (FAST) and a
/// seven-day window (SLOW). Codex curates nothing (see the module docs).
const CURATED_WINDOWS: &[CuratedWindow] = &[
    CuratedWindow {
        provider_kind: ANTHROPIC_PROVIDER_KIND,
        source_id: ANTHROPIC_FAST_SOURCE_ID,
        role: WindowRole::Fast,
        duration: Duration::from_hours(5),
    },
    CuratedWindow {
        provider_kind: ANTHROPIC_PROVIDER_KIND,
        source_id: "7d",
        role: WindowRole::Slow,
        duration: Duration::from_hours(24 * 7),
    },
];

/// Every curated window for one provider kind, in table order.
///
/// An uncurated provider kind yields an EMPTY iterator rather than a default
/// row, so no window is guessed for a provider routectl has not captured.
pub fn rows_for(provider_kind: &str) -> impl Iterator<Item = &'static CuratedWindow> {
    CURATED_WINDOWS
        .iter()
        .filter(move |row| row.provider_kind == provider_kind)
}

/// The curated row for one provider kind and role, or `None` when that
/// provider curates no window in that role.
///
/// `None` is an answer, not an error: it is how "Codex curates no FAST
/// window" reaches a caller.
pub fn row_for(provider_kind: &str, role: &WindowRole) -> Option<&'static CuratedWindow> {
    rows_for(provider_kind).find(|row| &row.role == role)
}

#[cfg(test)]
#[path = "curation_tests.rs"]
mod curation_tests;
