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

use crate::calibration::{Factor, LaneKey};

use super::{DispatchTarget, Router};

/// Numerator of the safety margin the estimate must exceed before a target
/// is skipped.
const WINDOW_MARGIN_NUMERATOR: u64 = 3;

/// Denominator of the safety margin. Together with the numerator: skip only
/// past 3/4 of the window.
///
/// An INTEGER ratio, deliberately: a routing decision must not hinge on a
/// float comparison.
///
/// Lowering the ratio is what guards the estimator's DEFLATE direction: a
/// byte-length estimate deflates by roughly 1.3-1.5x on high-entropy prose,
/// and 3/4 covers a deflate factor up to 4/3. The 1.5x tail is deliberately
/// left to the reactive backstop. Both error directions are survivable, and
/// they cost differently: an underestimate MISSES a skip, costing one doomed
/// round trip before the request falls onward; an overestimate makes a FALSE
/// skip, which among confirmed windows is a re-route rather than a denial,
/// because one estimate is compared against every window, so every surviving
/// confirmed-window target is strictly larger and therefore also fits.
///
/// 3/4 is not slack. The catalog window is TOTAL context, while the output
/// reserve on Anthropic-shape lanes (up to 64k by default) is subtracted from
/// it before any input fits, so the effective INPUT ceiling is lower than the
/// window -- on a 200k-class lane it lands near 0.68 of it, under the
/// fraction at which this margin skips.
///
/// Not an operator knob: a margin tuned per deployment turns a routing
/// decision into a support surface.
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
///
/// Deliberately process-wide rather than per-`Router`, unlike the skip
/// counter it accompanies: `RouterMetrics` carries across a hot-reload via
/// `carry_over_metrics_from`, so the two stay in agreement about whether a
/// skip happened -- neither resets independently of the other on a reload.
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
    corrected_tokens: u64,
}

impl Router {
    /// The learned per-lane correction to apply to this target's estimate,
    /// or `None` to compare the raw estimate exactly as the static gate does.
    ///
    /// Every refusal collapses here: the kill switch, a target that forms no
    /// lane (either half of the key absent), an unseen lane, one with too
    /// little or too old evidence, and one whose reduced ratio fell outside
    /// the sane band. One `None`, so there is one fallback path.
    ///
    /// The lane's model dimension is the served NICKNAME, the same label the
    /// recording path writes under. Keying it on the upstream wire id would
    /// silently never match, holding every lane uncorrected forever while
    /// reading as health.
    fn calibration_factor(&self, target: &DispatchTarget, now: SystemTime) -> Option<Factor> {
        if !self.config.calibration.enabled {
            return None;
        }
        let key = LaneKey {
            provider_kind: (*target.provider_kind.as_ref()?).to_string(),
            nickname: target.nickname.clone()?,
        };
        self.calibration_store.factor_for(&key, now)
    }

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
        // One clock read for the whole pass, so every target's evidence is
        // aged against the same instant.
        let now = SystemTime::now();
        for (idx, target) in chain.iter().enumerate() {
            // An unconfirmed window (unset, `Disabled`, or `Missing`) KEEPS
            // the target: skipping is the aggressive behavior here, and an
            // unknown fact never enables it. The accessor is shared with the
            // `/v1/models` discovery read, so the two surfaces cannot report
            // different windows for one target.
            let Some(window) = target.model.context_window_tokens() else {
                continue;
            };
            // Serializing the request is the expensive part, so it happens
            // once and only for a chain that has a window to compare
            // against.
            let estimated_tokens =
                *estimate.get_or_insert_with(|| crate::context_trim::estimate_total_tokens(req));
            // The corrected figure is a LOCAL, deliberately. Each target is
            // its own lane, so one raw serialization feeds a per-target
            // corrected value here -- and no other estimate consumer can
            // adopt this number, which is what keeps the persisted
            // estimate columns meaning exactly what they meant before.
            let corrected_tokens = self
                .calibration_factor(target, now)
                .map_or(estimated_tokens, |factor| factor.apply(estimated_tokens));
            let window = u64::from(window);
            if !exceeds_window_margin(corrected_tokens, window) {
                continue;
            }
            overflowing[idx] = true;
            if first_skipped.is_none() {
                first_skipped = Some(SkipReport {
                    state_key: target.state_key.clone(),
                    nickname: target.nickname.clone().unwrap_or_default(),
                    window,
                    corrected_tokens,
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
        //
        // Both figures are reported: the raw estimate, and the corrected one
        // the decision actually used. They are equal on an uncorrected lane,
        // and their divergence is the only way an operator can see a learned
        // correction move a skip.
        if throttle.claim(now_epoch_secs()) {
            tracing::warn!(
                event = "window_gate_skip",
                state_key = %report.state_key,
                model = %routectl_core::sanitize_for_log(&report.nickname),
                estimated_tokens,
                corrected_tokens = report.corrected_tokens,
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
