//! Proactive context-window gate: the second chain filter pass.
//!
//! Runs after the capability pre-filter and skips a chain target whose
//! context window clearly cannot hold the estimated request, so an
//! oversized request never spends a doomed knock on a small-window
//! fallback. The reactive `context-window` failure class remains the
//! backstop for whatever the estimate lets through.
//!
//! Contract difference from `feature_filter`: this pass has NO empty-chain
//! error path. The estimate is approximate and a filter pass cannot prove a
//! survivor is dynamically dispatchable (the breaker / rpm gate consumes
//! budget when consulted), so the gate must never be the reason a request
//! has nowhere to go.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use routectl_core::ChatRequest;

use super::{DispatchTarget, Router};

/// Numerator of the safety margin the estimate must exceed before a target
/// is skipped.
const WINDOW_MARGIN_NUMERATOR: u64 = 3;

/// Denominator of the safety margin. Together with the numerator: skip only
/// past 3/4 of the window.
///
/// An INTEGER ratio, deliberately: a routing decision must not hinge on a
/// float comparison. The value is conservative against the estimator's
/// ASYMMETRIC error -- a byte-length estimate deflates by roughly 1.3-1.5x
/// on high-entropy prose, the only direction that can make a skip wrong, and
/// 3/4 leaves headroom for it. Not an operator knob: a margin tuned per
/// deployment turns a routing decision into a support surface.
const WINDOW_MARGIN_DENOMINATOR: u64 = 4;

/// Minimum seconds between two skip WARNs in one process. A broken or
/// hostile client sending repeated oversized input must not turn correct
/// routing into an unbounded warning stream; the counter stays exact
/// regardless.
const SKIP_WARN_INTERVAL_SECS: u64 = 60;

/// Whether `estimate` clears the safety margin of `window` -- that is,
/// whether this target is CLEARLY too small for the request.
///
/// The one decision site for the margin, with one caller. A later per-lane
/// correction changes this signature and nothing else.
const fn exceeds_window_margin(estimate: u64, window: u64) -> bool {
    // `window` is promoted from a `u32` catalog field, so the product with
    // the numerator cannot overflow `u64`.
    estimate > window * WINDOW_MARGIN_NUMERATOR / WINDOW_MARGIN_DENOMINATOR
}

/// Epoch-second stamp of the last emitted skip WARN.
struct SkipWarnThrottle {
    last_warn_epoch_secs: AtomicU64,
}

impl SkipWarnThrottle {
    const fn new() -> Self {
        Self {
            last_warn_epoch_secs: AtomicU64::new(0),
        }
    }

    /// Claim the right to emit one WARN at `now_secs`, or refuse because the
    /// interval has not elapsed. One compare-and-swap, so concurrent
    /// claimants yield exactly one winner. `saturating_sub` makes a
    /// backwards clock jump suppress rather than re-open the window.
    fn claim(&self, now_secs: u64) -> bool {
        let last = self.last_warn_epoch_secs.load(Ordering::Relaxed);
        if now_secs.saturating_sub(last) < SKIP_WARN_INTERVAL_SECS {
            return false;
        }
        self.last_warn_epoch_secs
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

/// The process-wide throttle the production call site shares. A per-process
/// stamp, not a per-record item cap: the stream this bounds is repeats across
/// REQUESTS, which no within-record sampler can see.
static SKIP_WARN_THROTTLE: SkipWarnThrottle = SkipWarnThrottle::new();

/// Seconds since the Unix epoch, `0` on a pre-epoch clock (which then
/// suppresses rather than emits, the safe direction for a bounded WARN).
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// What one skipped target contributes to the WARN line: routectl-internal
/// identifiers and the catalog window only.
struct SkipReport {
    state_key: String,
    nickname: String,
    window: u64,
}

impl Router {
    /// Skip chain targets whose context window clearly cannot hold the
    /// estimated request, keyed on the shipped safety margin.
    ///
    /// Never returns an empty chain and never returns an error: a chain of
    /// one is returned untouched before any estimate is computed, a target
    /// whose window is unconfirmed is kept, and a chain whose every target
    /// overflows is returned unchanged so the caller still sees exactly
    /// today's upstream error rather than a routectl-invented one.
    pub(super) fn filter_chain_by_window(
        &self,
        chain: Vec<DispatchTarget>,
        req: &ChatRequest,
    ) -> Vec<DispatchTarget> {
        self.filter_chain_by_window_with(chain, req, &SKIP_WARN_THROTTLE)
    }

    /// The gate's body with the WARN throttle passed in. Split from
    /// `filter_chain_by_window` only so a test can bound the WARN stream
    /// against a throttle no sibling test has already claimed.
    fn filter_chain_by_window_with(
        &self,
        chain: Vec<DispatchTarget>,
        req: &ChatRequest,
        throttle: &SkipWarnThrottle,
    ) -> Vec<DispatchTarget> {
        // Kill switch plus the FIRST never-skip-the-last layer. Both return
        // before an estimate exists, so a disabled gate (or a chain with
        // nothing to fall to) computes nothing, reorders nothing, counts
        // nothing, and logs nothing.
        if !self.config.window_gate.enabled || chain.len() <= 1 {
            return chain;
        }
        // Decide first, act second: neither the counter nor the WARN may
        // move for a skip the layers below go on to refuse.
        let mut estimate: Option<u64> = None;
        let mut overflowing: Vec<bool> = vec![false; chain.len()];
        let mut first_skipped: Option<SkipReport> = None;
        for (idx, target) in chain.iter().enumerate() {
            // An unconfirmed window (unset, `Disabled`, or `Missing`) KEEPS
            // the target: skipping is the aggressive behavior here, and an
            // unknown fact never enables it.
            let Some(window) = target
                .model
                .effective_row
                .priced()
                .and_then(|row| row.max_context_tokens)
            else {
                continue;
            };
            // Serializing the request is the expensive part, so it happens
            // once and only for a chain that has a window to compare
            // against.
            let estimated_tokens =
                *estimate.get_or_insert_with(|| crate::context_trim::estimate_total_tokens(req));
            let window = u64::from(window);
            if !exceeds_window_margin(estimated_tokens, window) {
                continue;
            }
            overflowing[idx] = true;
            if first_skipped.is_none() {
                first_skipped = Some(SkipReport {
                    state_key: target.state_key.clone(),
                    nickname: target.nickname.clone().unwrap_or_default(),
                    window,
                });
            }
        }
        let skips = overflowing.iter().filter(|over| **over).count();
        // SECOND never-skip-the-last layer: a skip is realized only while
        // another candidate remains. Every target overflowing means there is
        // nothing to fall to, so the resolved chain stands unchanged and no
        // skip is counted, no WARN emitted.
        if skips == 0 || skips == chain.len() {
            return chain;
        }
        let (Some(estimated_tokens), Some(report)) = (estimate, first_skipped) else {
            return chain;
        };
        // One `incr_` per skip so the counter is the authoritative skip
        // count, not a per-request event count; the last increment's return
        // is the running total the WARN reports.
        let mut skips_total = 0;
        for _ in 0..skips {
            skips_total = self.metrics.incr_window_gate_skip();
        }
        // One line per interval per process, naming the first skipped
        // target. routectl-internal identifiers (state key, model nickname)
        // and catalog / estimate figures ONLY -- never anything from the
        // request body, its tools, its attachments, or the session key.
        if throttle.claim(now_epoch_secs()) {
            tracing::warn!(
                event = "window_gate_skip",
                state_key = %report.state_key,
                model = %report.nickname,
                estimated_tokens,
                window_tokens = report.window,
                skips_total,
                "target context window cannot hold the estimated request; \
                 skipped before dispatch",
            );
        }
        let mut kept: Vec<DispatchTarget> = Vec::with_capacity(chain.len() - skips);
        let mut skipped: Vec<DispatchTarget> = Vec::with_capacity(skips);
        for (target, over) in chain.into_iter().zip(overflowing) {
            if over {
                skipped.push(target);
            } else {
                kept.push(target);
            }
        }
        // THIRD never-skip-the-last layer: an empty result is refused, and
        // `skipped` is the original chain in its original order in exactly
        // that case. There is no empty-chain error path in this module.
        //
        // Deliberately redundant: the all-overflow refusal above already
        // guarantees a survivor, so this cannot fire today. It stays because
        // the never-empty guarantee must not rest on a single condition --
        // a future edit to either layer above leaves this one standing. Do
        // not strip it as dead code.
        if kept.is_empty() {
            return skipped;
        }
        kept
    }
}

#[cfg(test)]
#[path = "window_gate_tests.rs"]
mod window_gate_tests;
