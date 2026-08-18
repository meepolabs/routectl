//! Cache-injection decision vocabulary shared across routectl-core
//! consumers.
//!
//! Single namespace source for the ten tokens the `cache_auto_decision`
//! log emits and the usage ledger's `cache_front_decision` /
//! `cache_terminal_decision` columns persist. The router's
//! `CacheInjection::strategy_str` maps its variants onto these consts, so
//! the emitted spelling and any reader's expectation meet on one literal.
//!
//! These tokens are a FROZEN CONTRACT: every one is written verbatim to a
//! ledger column and read back by the operator-facing finders, so a
//! renamed token silently re-classifies every historical row. Add new
//! tokens; never rename or repurpose an existing one.
//!
//! The vocabulary is CLOSED in the emitting direction (the router's enum
//! is exhaustive) but readers stay open-set-tolerant: a row carrying an
//! unrecognized token is skipped, never panicked on.

/// A breakpoint was injected in this marker's slot and validated. The one
/// token in the vocabulary that means caching actually happened.
pub const AUTO_EMITTED: &str = "auto_emitted";

/// The caller already supplied at least one breakpoint, so auto-emit
/// deferred entirely. A request-level fact evaluated FIRST: it takes
/// precedence over every [`AUTO_SKIPPED_GLOBAL_DISABLED`]-style reason.
pub const CALLER_SUPPLIED: &str = "caller_supplied";

/// The stable cacheable prefix carries high-confidence volatile tokens, so
/// caching it would write-without-read every request.
pub const VOLATILE_VETOED: &str = "volatile_vetoed";

/// Global `[cache] auto_emit_top_level_breakpoint = false` -- the master
/// kill for both markers.
pub const AUTO_SKIPPED_GLOBAL_DISABLED: &str = "auto_skipped:global_disabled";

/// The provider's own `auto_emit_top_level_breakpoint = false` (terminal
/// marker) or `auto_emit_per_block_breakpoints = false` (front marker).
pub const AUTO_SKIPPED_PROVIDER_DISABLED: &str = "auto_skipped:provider_disabled";

/// The target's egress cannot carry this marker to the wire, or its
/// capability is unknown and the gate failed closed.
pub const AUTO_SKIPPED_NO_CAPABILITY: &str = "auto_skipped:no_capability";

/// Injecting would push the breakpoint count past the per-request maximum.
pub const AUTO_SKIPPED_BREAKPOINT_CAP: &str = "auto_skipped:breakpoint_cap";

/// The request offers no slot this marker could occupy -- a front marker on
/// a flat-string system with no typed custom tool.
pub const AUTO_SKIPPED_NO_PLACEMENT_REGION: &str = "auto_skipped:no_placement_region";

/// The session's calibrated per-turn reuse floor sits below the target
/// row's emission break-even, so placing a marker would pay a write premium
/// the observed reuse rate cannot recover. Withholds BOTH markers, which is
/// why a request carrying it on both columns is ONE withheld request.
pub const AUTO_SKIPPED_K_BELOW_BREAK_EVEN: &str = "auto_skipped:k_below_break_even";

/// Injection was attempted but validating the COMBINED breakpoint sequence
/// failed, so the whole candidate was discarded.
pub const AUTO_SKIPPED_VALIDATION_ROLLED_BACK: &str = "auto_skipped:validation_rolled_back";

/// Every cache-decision token. Exhaustive: the emitting enum is closed, so
/// unlike the open-ended capability-key vocabulary this list IS the whole
/// contract, which is what lets a test assert the emitter covers it.
pub const CACHE_DECISION_TOKENS: &[&str] = &[
    AUTO_EMITTED,
    CALLER_SUPPLIED,
    VOLATILE_VETOED,
    AUTO_SKIPPED_GLOBAL_DISABLED,
    AUTO_SKIPPED_PROVIDER_DISABLED,
    AUTO_SKIPPED_NO_CAPABILITY,
    AUTO_SKIPPED_BREAKPOINT_CAP,
    AUTO_SKIPPED_NO_PLACEMENT_REGION,
    AUTO_SKIPPED_K_BELOW_BREAK_EVEN,
    AUTO_SKIPPED_VALIDATION_ROLLED_BACK,
];

/// Whether `token` is a recognized cache-decision token. Open-set-tolerant:
/// a reader uses this to skip a row whose token is absent or unrecognized
/// rather than panicking.
pub fn is_known_cache_decision(token: &str) -> bool {
    CACHE_DECISION_TOKENS.contains(&token)
}

#[cfg(test)]
#[path = "cache_decision_tests.rs"]
mod tests;
