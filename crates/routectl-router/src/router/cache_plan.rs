//! Auto-cache request plan (pure read; the injection CALL stays in dispatch).

use routectl_core::cache_control::{FrontSlot, compute_frozen_floor, front_breakpoint_slot};
use routectl_core::{ChatRequest, scan_volatile};

/// Request-level inputs to the auto-cache decision, computed ONCE per
/// request off the original `req` (above the `'chain` loop) and reused
/// for every retry and fallback target. Holding these constant is what
/// makes auto-emit idempotent PER TARGET: retrying the same target sends
/// byte-identical bytes, and a fallback target re-derives nothing from the
/// previous hop.
///
/// Idempotence is per-TARGET, not per-REQUEST. Every fact here is
/// target-invariant, but the placement step also consults a per-target
/// verdict (the K-gated emission gate, consulted once per chain entry
/// above the retry loop), so two targets in one fallback chain may
/// legitimately differ on emit-vs-skip. That is why a session-derived
/// suppression verdict must never be stored on this struct: it would apply
/// one target's economics to a different model's target.
///
/// The gate reads `has_caller_breakpoints` / `caller_breakpoint_count`
/// (snapshotted from the frozen floor at build time) directly so the
/// predicate stays a cheap field compare.
pub(super) struct AutoCacheRequestPlan {
    pub(super) has_caller_breakpoints: bool,
    pub(super) caller_breakpoint_count: usize,
    pub(super) volatile_high_veto: bool,
    pub(super) global_auto_emit_enabled: bool,
    /// Resolved anchor for the FRONT marker, or `None` when the request
    /// offers no placement region (a flat-string system with no custom
    /// tool). `None` is what makes the front decision
    /// [`CacheInjection::SkippedNoPlacementRegion`].
    ///
    /// The slot carries a resolved INDEX, so every target and every retry
    /// marks the same element without re-deriving it.
    pub(super) front_slot: Option<FrontSlot>,
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
            front_slot: front_breakpoint_slot(req),
        }
    }
}

/// Outcome of an auto-cache injection decision for ONE marker on one
/// dispatch target. Drives control flow today (and is the stable
/// per-target signal the cache-decision log carries). Every non-`Emitted`
/// variant means that marker was not placed -- the dispatched bytes equal
/// the un-injected clone for that marker's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheInjection {
    /// An ephemeral_5m breakpoint was injected in this marker's slot and
    /// validated.
    Emitted,
    /// Global `[cache] auto_emit_top_level_breakpoint = false`.
    SkippedGlobalDisabled,
    /// Per-provider `auto_emit_top_level_breakpoint = false` (terminal
    /// marker) or `auto_emit_per_block_breakpoints = false` (front
    /// marker). The two knobs are independent.
    SkippedProviderDisabled,
    /// The target's egress cannot carry this marker to the wire (or its
    /// capability is unknown -- fail closed). For the TERMINAL marker:
    /// `CacheCapability::supports_top_level_cache_control` is false. For
    /// the FRONT marker: the provider kind has no per-block breakpoint
    /// surface (`ProviderEntry::supports_per_block_breakpoints`), so an
    /// explicit operator opt-in there is inert rather than honored.
    SkippedNoCapability,
    /// The caller already supplied at least one breakpoint; auto-emit
    /// would risk a second marker / byte rewrite, so we defer entirely.
    SkippedCallerSupplied,
    /// The session's CALIBRATED per-turn reuse floor sits below the target
    /// row's emission break-even `K*`, so placing a marker would pay a write
    /// premium the observed reuse rate cannot recover. Withholds BOTH
    /// markers, and only ever for a calibrated estimate on a priced,
    /// non-auto-cacher row while `[cache] k_gated_emission` is on.
    SkippedKBelowBreakEven,
    /// The stable cacheable prefix carries high-confidence volatile
    /// tokens; caching it would write-without-read every request.
    SkippedVolatileHigh,
    /// Injecting would push the breakpoint count past `MAX_BREAKPOINTS`.
    SkippedBreakpointCap,
    /// The request offers no slot this marker could occupy -- the front
    /// marker on a request whose system is a flat string and which carries
    /// no typed custom tool. The alternative (lifting the system to
    /// blocks) is a wire-shape change on a re-encoding-banned path, so the
    /// marker is skipped and the coverage gap recorded instead.
    SkippedNoPlacementRegion,
    /// Injection was attempted but validating the COMBINED breakpoint
    /// sequence failed, so the whole candidate was discarded and the
    /// pre-cache clone dispatched unchanged. Both markers record this
    /// unless one was already skipped for its own reason.
    ValidationRolledBack,
}

impl CacheInjection {
    /// Stable operator-facing token for this decision, emitted in the
    /// `cache_auto_decision` log and persisted in the usage ledger's
    /// per-marker decision columns. The legacy `requests.strategy` column
    /// stays write-stopped as of 0.9.x. These tokens are a
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
    /// | SkippedNoPlacementRegion | `auto_skipped:no_placement_region` |
    /// | SkippedKBelowBreakEven   | `auto_skipped:k_below_break_even`  |
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
            Self::SkippedNoPlacementRegion => "auto_skipped:no_placement_region",
            Self::SkippedKBelowBreakEven => "auto_skipped:k_below_break_even",
            Self::ValidationRolledBack => "auto_skipped:validation_rolled_back",
        }
    }
}

/// Per-marker outcome of the auto-cache placement step for one dispatch
/// target: one decision for the FRONT marker (a system block or a custom
/// tool definition) and one for the TERMINAL marker (the top-level
/// `cache_control` field).
///
/// The two decisions are independent -- a marker with no placement region
/// skips alone and does not suppress the other -- but they are produced
/// together so the placement step commits or discards a single candidate
/// request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CacheDecision {
    pub(super) front: CacheInjection,
    pub(super) terminal: CacheInjection,
}

#[cfg(test)]
#[path = "cache_plan_tests.rs"]
mod cache_plan_tests;
