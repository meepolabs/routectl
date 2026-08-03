//! Auto-cache request plan (pure read; the injection CALL stays in dispatch).

use routectl_core::cache_control::compute_frozen_floor;
use routectl_core::{ChatRequest, scan_volatile};

/// Request-level inputs to the auto-cache decision, computed ONCE per
/// request off the original `req` (above the `'chain` loop) and reused
/// for every retry and fallback target. Holding these constant is what
/// makes auto-emit idempotent: retrying the same target sends
/// byte-identical bytes, and a fallback target re-derives nothing.
///
/// The gate reads `has_caller_breakpoints` / `caller_breakpoint_count`
/// (snapshotted from the frozen floor at build time) directly so the
/// predicate stays a cheap field compare.
pub(super) struct AutoCacheRequestPlan {
    pub(super) has_caller_breakpoints: bool,
    pub(super) caller_breakpoint_count: usize,
    pub(super) volatile_high_veto: bool,
    pub(super) global_auto_emit_enabled: bool,
}

impl AutoCacheRequestPlan {
    /// Build the plan from the ORIGINAL request. Pure read: never mutates
    /// `req`. Called once per dispatch fn, above the `'chain` loop.
    pub(super) fn build(req: &ChatRequest, global_auto_emit_enabled: bool) -> Self {
        let frozen_floor = compute_frozen_floor(req);
        let has_caller_breakpoints = frozen_floor.has_caller_breakpoints();
        let caller_breakpoint_count = frozen_floor.caller_breakpoint_count();
        let volatile_high_veto = scan_volatile(req).is_high_confidence_veto();
        Self {
            has_caller_breakpoints,
            caller_breakpoint_count,
            volatile_high_veto,
            global_auto_emit_enabled,
        }
    }
}

/// Outcome of an auto-cache injection decision for one dispatch target.
/// Drives control flow today (and is the stable per-target signal T6 will
/// log). Every non-`Emitted` variant means `attempt_req` was left
/// untouched -- the dispatched bytes equal the un-injected clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheInjection {
    /// A top-level ephemeral_5m breakpoint was injected and validated.
    Emitted,
    /// Global `[cache] auto_emit_top_level_breakpoint = false`.
    SkippedGlobalDisabled,
    /// Per-provider `auto_emit_top_level_breakpoint = false`.
    SkippedProviderDisabled,
    /// Target's provider does not honor a top-level breakpoint (or its
    /// capability is unknown -- fail closed).
    SkippedNoCapability,
    /// The caller already supplied at least one breakpoint; auto-emit
    /// would risk a second marker / byte rewrite, so we defer entirely.
    SkippedCallerSupplied,
    /// The stable cacheable prefix carries high-confidence volatile
    /// tokens; caching it would write-without-read every request.
    SkippedVolatileHigh,
    /// Injecting would push the breakpoint count past `MAX_BREAKPOINTS`.
    SkippedBreakpointCap,
    /// Injection was attempted but post-injection validation failed; the
    /// original `cache_control` was restored and the clone dispatched
    /// unchanged.
    ValidationRolledBack,
}

impl CacheInjection {
    /// Stable operator-facing token for this decision, emitted in the
    /// `cache_auto_decision` log. Not persisted: the usage DB's
    /// `requests.strategy` column is write-stopped as of 0.9.x, so the log
    /// line is the only place the token appears. These tokens are a
    /// CONTRACT: do not
    /// rename or repurpose them, only add new ones. The `auto_skipped:`
    /// prefix groups the variants where auto-emit ran but declined.
    /// `caller_supplied` is a request-level fact evaluated FIRST and takes
    /// precedence over every `auto_skipped:*` reason.
    ///
    /// | variant                  | token                              |
    /// |--------------------------|------------------------------------|
    /// | Emitted                  | `auto_emitted`                     |
    /// | SkippedCallerSupplied    | `caller_supplied`                  |
    /// | SkippedVolatileHigh      | `volatile_vetoed`                  |
    /// | SkippedGlobalDisabled    | `auto_skipped:global_disabled`     |
    /// | SkippedProviderDisabled  | `auto_skipped:provider_disabled`   |
    /// | SkippedNoCapability      | `auto_skipped:no_capability`       |
    /// | SkippedBreakpointCap     | `auto_skipped:breakpoint_cap`      |
    /// | ValidationRolledBack     | `auto_skipped:validation_rolled_back` |
    pub(super) const fn strategy_str(self) -> &'static str {
        match self {
            Self::Emitted => "auto_emitted",
            Self::SkippedCallerSupplied => "caller_supplied",
            Self::SkippedVolatileHigh => "volatile_vetoed",
            Self::SkippedGlobalDisabled => "auto_skipped:global_disabled",
            Self::SkippedProviderDisabled => "auto_skipped:provider_disabled",
            Self::SkippedNoCapability => "auto_skipped:no_capability",
            Self::SkippedBreakpointCap => "auto_skipped:breakpoint_cap",
            Self::ValidationRolledBack => "auto_skipped:validation_rolled_back",
        }
    }
}
