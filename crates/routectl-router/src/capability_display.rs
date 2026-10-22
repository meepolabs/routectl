//! The panel-only display tokens a read-only surface (the doctor capability
//! matrix, the status surface) renders for a capability cell, and the source
//! tags naming which tier a shown signal came from.
//!
//! The cell itself is resolved by the precedence seam's display mode,
//! [`resolve_display`](crate::capability_precedence::resolve_display), which
//! picks among these tokens.

/// Display verdict token for an operator route-away override cell. A
/// PANEL-ONLY token, distinct from the core
/// [`Verdict`](routectl_core::capability::Verdict) vocabulary: an
/// override is an operator assertion, not a learned or catalog signal,
/// and the core verdict enum is a versioned, internal ledger contract that
/// must not grow display-only states.
pub const FORCED_UNSUPPORTED: &str = "forced_unsupported";

/// Display verdict token for an operator force-supported override cell.
/// PANEL-ONLY -- see [`FORCED_UNSUPPORTED`].
pub const FORCED_SUPPORTED: &str = "forced_supported";

/// Display verdict token for a matrix cell whose nicknames resolve to
/// different verdicts (one carries a nickname-scoped override the other does
/// not), so no lane-wide verdict, source or layer describes it. PANEL-ONLY,
/// like the override tokens: never part of the core verdict vocabulary.
pub const VERDICT_MIXED: &str = "mixed";

/// Source tag: an operator override decided the cell.
pub const SOURCE_OVERRIDE: &str = "override";
/// Source tag: a learned observation from live traffic.
pub const SOURCE_LIVE: &str = "live";
/// Source tag: a learned observation from an out-of-band probe.
pub const SOURCE_PROBE: &str = "probe";
/// Source tag: a catalog capability prior.
pub const SOURCE_PRIOR: &str = "prior";
/// Source tag: the shipped beta seed.
pub const SOURCE_SEED: &str = "seed";

/// Display verdict token for a beta flag the lane cannot carry at all.
/// PANEL-ONLY, like the override tokens.
pub const VERDICT_UNREPRESENTABLE: &str = "unrepresentable";

/// Source tag: an operator wire floor pins the capability.
pub const SOURCE_PIN: &str = "pin";
/// Source tag: the lane cannot represent the capability on its wire.
pub const SOURCE_REPRESENTABILITY: &str = "representability";

#[cfg(test)]
#[path = "capability_display_tests.rs"]
mod capability_display_tests;
