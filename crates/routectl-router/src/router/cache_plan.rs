//! Auto-cache request plan and the per-attempt marker placement it feeds (the
//! placement CALL stays in the dispatch walk).

use routectl_core::cache_control::{
    FrontSlot, MAX_BREAKPOINTS, compute_frozen_floor, front_breakpoint_slot,
    system_block_is_wire_eligible, validate_source,
};
use routectl_core::cache_decision::{
    AUTO_EMITTED, AUTO_SKIPPED_BREAKPOINT_CAP, AUTO_SKIPPED_GLOBAL_DISABLED,
    AUTO_SKIPPED_K_BELOW_BREAK_EVEN, AUTO_SKIPPED_NO_CAPABILITY, AUTO_SKIPPED_NO_PLACEMENT_REGION,
    AUTO_SKIPPED_PROVIDER_DISABLED, AUTO_SKIPPED_VALIDATION_ROLLED_BACK, CALLER_SUPPLIED,
    VOLATILE_VETOED,
};
use routectl_core::{CacheControl, ChatRequest, scan_volatile};

use crate::config::CacheCapability;

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
    /// stays write-stopped as of 0.9.x.
    ///
    /// The spellings live in `routectl_core::cache_decision`, which is the
    /// vocabulary's single source across the emitting router and every
    /// reader. They are a CONTRACT: do not rename or repurpose them, only
    /// add new ones. The `auto_skipped:` prefix groups the variants where
    /// auto-emit ran but declined. `caller_supplied` is a request-level
    /// fact evaluated FIRST and takes precedence over every
    /// `auto_skipped:*` reason.
    pub(super) const fn strategy_str(self) -> &'static str {
        match self {
            Self::Emitted => AUTO_EMITTED,
            Self::SkippedCallerSupplied => CALLER_SUPPLIED,
            Self::SkippedVolatileHigh => VOLATILE_VETOED,
            Self::SkippedGlobalDisabled => AUTO_SKIPPED_GLOBAL_DISABLED,
            Self::SkippedProviderDisabled => AUTO_SKIPPED_PROVIDER_DISABLED,
            Self::SkippedNoCapability => AUTO_SKIPPED_NO_CAPABILITY,
            Self::SkippedBreakpointCap => AUTO_SKIPPED_BREAKPOINT_CAP,
            Self::SkippedNoPlacementRegion => AUTO_SKIPPED_NO_PLACEMENT_REGION,
            Self::SkippedKBelowBreakEven => AUTO_SKIPPED_K_BELOW_BREAK_EVEN,
            Self::ValidationRolledBack => AUTO_SKIPPED_VALIDATION_ROLLED_BACK,
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

/// Config inputs the placement step reads per dispatch target. Grouped so
/// the two call sites (completion + streaming) pass one value and cannot
/// drift in argument order.
#[derive(Debug, Clone, Copy)]
pub(super) struct CacheTargetGates {
    /// The target provider's prompt-cache capability, `None` when the
    /// provider is absent from the table (fail closed).
    pub(super) capability: Option<CacheCapability>,
    /// Per-provider `auto_emit_top_level_breakpoint`, resolved (absent
    /// inherits the global switch).
    pub(super) terminal_enabled: bool,
    /// Whether the target's EGRESS can carry a per-block marker to the
    /// wire (`ProviderEntry::supports_per_block_breakpoints`) -- the front
    /// marker's capability gate, distinct from the operator switch below.
    pub(super) front_supported: bool,
    /// Per-provider `auto_emit_per_block_breakpoints`, resolved through
    /// `ProviderEntry::per_block_breakpoints_enabled` (absent takes the
    /// kind-level default). Operator INTENT: placement also requires
    /// `front_supported`.
    pub(super) front_enabled: bool,
}

/// Resolve the per-target cache gates from the target's `[providers.X]`
/// entry. An absent entry fails closed on every gate (no capability, no
/// per-block support, no default to read). One resolver so the completion
/// and streaming call sites cannot resolve the knobs differently.
pub(super) fn cache_target_gates(
    provider_cfg: Option<&crate::config::ProviderEntry>,
) -> CacheTargetGates {
    CacheTargetGates {
        capability: provider_cfg.map(crate::config::ProviderEntry::cache_capability),
        terminal_enabled: provider_cfg
            .and_then(crate::config::ProviderEntry::auto_emit_top_level_breakpoint)
            .unwrap_or(true),
        front_supported: provider_cfg
            .is_some_and(crate::config::ProviderEntry::supports_per_block_breakpoints),
        front_enabled: provider_cfg
            .is_some_and(crate::config::ProviderEntry::per_block_breakpoints_enabled),
    }
}

/// Decide-and-maybe-inject the auto-cache markers on the PER-ATTEMPT
/// clone, TRANSACTIONALLY: both candidate markers are applied to one
/// throwaway clone, the complete breakpoint sequence is validated ONCE,
/// and the candidate is then committed whole or discarded whole. Mutates
/// ONLY `attempt_req`, never the original request. Never returns `Err`
/// and never panics: any doubt degrades to "dispatch the un-injected
/// clone".
///
/// Two markers, independently gated:
///
/// - FRONT -- a per-block `cache_control` on the resolved
///   [`FrontSlot`] (the last wire-eligible system block, else the last
///   custom tool). Requires BOTH the provider's
///   `auto_emit_per_block_breakpoints` (operator intent) AND
///   `supports_per_block_breakpoints` (wire capability: anthropic-api and
///   Bedrock Converse, where the egress translates the marker into a
///   `cachePoint` block). `CacheCapability` does NOT gate it -- that
///   struct describes the TOP-LEVEL field only.
/// - TERMINAL -- the top-level `cache_control` field. Gated by the
///   provider's `auto_emit_top_level_breakpoint` and by
///   `supports_top_level_cache_control`, exactly as before.
///
/// The global `[cache] auto_emit_top_level_breakpoint` switch is the
/// master kill for dispatch-path auto-emission and withholds BOTH
/// markers, so an operator who has auto-emit off today never starts
/// receiving a front marker.
///
/// Rollback is by construction: the pre-cache request is never mutated
/// until the candidate validates, so a discarded candidate preserves
/// every overlay / strip / minify change AND every cache-placement slot
/// (tools, system, messages, top-level) with no restore bookkeeping. On
/// validation failure both provisionally-emitted markers record
/// [`CacheInjection::ValidationRolledBack`]; a marker already skipped
/// keeps its own reason.
///
/// Called once per chain entry, AFTER `apply_layered_overlays` (so
/// injection is the last dispatch-time touch of any cache marker) and
/// before the inner retry loop, so all retries on a target reuse
/// byte-identical bytes. A fallback target starts from a fresh clone of
/// the canonical request, so a discarded candidate on one target cannot
/// contaminate the next. Byte identity is therefore per-TARGET: identical
/// across retries of one target, and never promised across two targets of a
/// fallback chain, which may legitimately differ on emit-vs-skip.
///
/// `caller_supplied` is evaluated FIRST as a request-level fact: a request
/// that already carries caller breakpoints is independent of which target /
/// provider is selected, so it takes precedence over every per-target /
/// config `auto_skipped:*` reason (global / provider kill-switch, capability).
///
/// `k_suppressed` is the per-target verdict of `Router::k_emission_suppressed`,
/// consulted once at the call site. It withholds BOTH markers as a shared
/// reason, checked immediately AFTER `shared_skip_reason` so
/// `caller_supplied` and `global_disabled` keep their precedence -- both are
/// already short-circuits inside the gate itself, so the ordering here only
/// pins that the recorded TOKEN stays theirs.
pub(super) fn apply_auto_cache_placement(
    attempt_req: &mut ChatRequest,
    plan: &AutoCacheRequestPlan,
    gates: CacheTargetGates,
    k_suppressed: bool,
) -> CacheDecision {
    if let Some(reason) = shared_skip_reason(plan) {
        return CacheDecision {
            front: reason,
            terminal: reason,
        };
    }
    if k_suppressed {
        return CacheDecision {
            front: CacheInjection::SkippedKBelowBreakEven,
            terminal: CacheInjection::SkippedKBelowBreakEven,
        };
    }

    let mut front = if !gates.front_enabled {
        CacheInjection::SkippedProviderDisabled
    } else if !gates.front_supported {
        // An explicit opt-in on a kind whose egress cannot carry a
        // per-block marker is INERT, not honored: emitting here would ship
        // a marker the wire drops (openai-compat WARNs and 400s under
        // strict_translation) while recording a false `auto_emitted`.
        CacheInjection::SkippedNoCapability
    } else if plan.volatile_high_veto {
        CacheInjection::SkippedVolatileHigh
    } else if plan.front_slot.is_none() {
        CacheInjection::SkippedNoPlacementRegion
    } else {
        CacheInjection::Emitted
    };
    let terminal = if !gates.terminal_enabled {
        CacheInjection::SkippedProviderDisabled
    } else if !gates
        .capability
        .is_some_and(|c| c.supports_top_level_cache_control)
    {
        CacheInjection::SkippedNoCapability
    } else if plan.volatile_high_veto {
        CacheInjection::SkippedVolatileHigh
    } else {
        CacheInjection::Emitted
    };

    let intended = usize::from(front == CacheInjection::Emitted)
        + usize::from(terminal == CacheInjection::Emitted);
    if intended == 0 {
        return CacheDecision { front, terminal };
    }
    // Defensive drift guard: no-caller implies 0 today, so +2 is always
    // within MAX. Kept so a future change that injects alongside caller
    // markers cannot silently exceed the cap.
    if plan.caller_breakpoint_count.saturating_add(intended) > MAX_BREAKPOINTS {
        return CacheDecision {
            front: cap_skip(front),
            terminal: cap_skip(terminal),
        };
    }

    // The candidate is a throwaway clone: nothing observable is mutated
    // until it validates, which IS the rollback.
    let mut candidate = attempt_req.clone();
    if let (CacheInjection::Emitted, Some(slot)) = (front, plan.front_slot) {
        // The slot index was resolved off the ORIGINAL request; a
        // dispatch-time step (strip, a payload overlay) may have reshaped
        // system / tools since. Placement fails closed rather than
        // marking a different element than the one that was selected.
        if !place_front_marker(&mut candidate, slot) {
            front = CacheInjection::SkippedNoPlacementRegion;
        }
    }
    if terminal == CacheInjection::Emitted {
        candidate.cache_control = Some(CacheControl::ephemeral_5m());
    }
    if front != CacheInjection::Emitted && terminal != CacheInjection::Emitted {
        return CacheDecision { front, terminal };
    }

    if validate_source(&candidate).is_ok() {
        *attempt_req = candidate;
        CacheDecision { front, terminal }
    } else {
        CacheDecision {
            front: rollback_skip(front),
            terminal: rollback_skip(terminal),
        }
    }
}

/// Request-level reasons that withhold BOTH markers, in precedence order.
/// `None` means the request itself is eligible and the per-marker gates
/// decide.
///
/// The high-confidence volatile veto is NOT here: it is applied per marker
/// so it keeps its existing precedence BELOW the per-target config and
/// capability gates, leaving the terminal marker's recorded token
/// unchanged from before per-marker placement existed.
const fn shared_skip_reason(plan: &AutoCacheRequestPlan) -> Option<CacheInjection> {
    if plan.has_caller_breakpoints {
        return Some(CacheInjection::SkippedCallerSupplied);
    }
    if !plan.global_auto_emit_enabled {
        return Some(CacheInjection::SkippedGlobalDisabled);
    }
    None
}

/// Fold a per-marker decision through the breakpoint-cap refusal: a
/// marker that was going to be emitted records the cap, a marker already
/// skipped keeps its own reason.
const fn cap_skip(decision: CacheInjection) -> CacheInjection {
    match decision {
        CacheInjection::Emitted => CacheInjection::SkippedBreakpointCap,
        other => other,
    }
}

/// Fold a per-marker decision through a whole-candidate validation
/// failure: a marker that was applied to the discarded candidate records
/// the rollback, a marker already skipped keeps its own reason.
const fn rollback_skip(decision: CacheInjection) -> CacheInjection {
    match decision {
        CacheInjection::Emitted => CacheInjection::ValidationRolledBack,
        other => other,
    }
}

/// Write an ephemeral_5m marker into `slot` on `req`. Returns false when
/// the slot no longer resolves to the element that was selected (a
/// reshaped system / tools array), leaving `req` untouched.
///
/// CONSTRAINT (accepted, measured rather than fixed): a marker on a system
/// block that a downstream egress DROPS -- the billing-attribution strip in
/// `routectl-providers` `system_filter` removes a Claude Code attribution
/// block -- silently never reaches the wire. Coupling this placement to
/// that providers-crate predicate would invert the crate dependency; the
/// divergence is diagnosable instead by comparing the recorded decision
/// against the observed `detect_prompt_caching` outcome.
pub(super) fn place_front_marker(req: &mut ChatRequest, slot: FrontSlot) -> bool {
    match slot {
        FrontSlot::LastSystemBlock { block_index } => {
            let Some(routectl_core::SystemContent::Blocks(blocks)) = req.system.as_mut() else {
                return false;
            };
            let Some(block) = blocks.get_mut(block_index) else {
                return false;
            };
            if !system_block_is_wire_eligible(block) {
                return false;
            }
            block.cache_control = Some(CacheControl::ephemeral_5m());
            true
        }
        FrontSlot::LastCustomTool { tool_index } => {
            let Some(tools) = req.tools.as_mut() else {
                return false;
            };
            let Some(routectl_core::ToolDef::Custom(tool)) = tools.get_mut(tool_index) else {
                return false;
            };
            tool.cache_control = Some(CacheControl::ephemeral_5m());
            true
        }
    }
}

#[cfg(test)]
#[path = "cache_plan_tests.rs"]
mod cache_plan_tests;
