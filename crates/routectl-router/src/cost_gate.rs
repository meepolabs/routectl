//! Pure break-even COST GATE for prompt-cache prefix reduction.
//!
//! This module answers a single question with arithmetic only: given a
//! tier-correct [`CachePricingRow`] and a proposed prefix edit, is breaking
//! the warm cache net-positive in dollars, or should the original prefix be
//! kept? It is advisory-only: nothing here mutates a request,
//! estimates the reuse count `K`, or touches the dispatch path. The caller
//! supplies a hypothetical `k` and gets back a [`GateDecision`].
//!
//! The economics (see the cache-economics design): a token behind a warm
//! breakpoint bills at the cheap READ multiplier and has negative
//! cost-to-keep; removing it shifts every downstream byte so the disturbed
//! suffix must re-write, and every future discounted read is forfeited.
//! Breaking is therefore worth it only when the read savings over the
//! assumed future reuses exceed the one-time re-write / re-send tax.
//!
//! Two branches keyed on [`CachePricingRow::auto_cacher`]:
//! - WRITE-PREMIUM providers (`auto_cacher == false`, e.g. Anthropic /
//!   Bedrock / Qwen-explicit) pay a real write tax on the re-warm, so
//!   BREAK iff `k * d * rm > c_after * wm`.
//! - AUTO-CACHER providers (`auto_cacher == true`, e.g. OpenAI / DeepSeek /
//!   Gemini-implicit) have free writes (`wm ~= 1.0`); the only one-time cost
//!   is re-sending the disturbed suffix at miss price instead of a cheap
//!   read, so BREAK iff `k * d * rm > c_after * (1 - rm)`.
//!
//! Every doubt resolves to KEEP: an unverified / sentinel row, no candidate,
//! or a post-cut prefix below the cacheable floor all force KEEP before the
//! branch inequality is even consulted.
//!
//! Purity: depends only on [`crate::cache_pricing::CachePricingRow`] and
//! std. The `f32` row multipliers are widened to `f64` for the arithmetic.

use crate::cache_pricing::CachePricingRow;

/// A proposed prefix reduction handed to the gate. Immutable; the gate never
/// mutates it. Token counts are in prefix tokens.
///
/// `#[non_exhaustive]`: later phases add edit-position / breakpoint detail,
/// so external crates construct this only through its public constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrefixReductionCandidate {
    /// Tokens removed by the proposed edit.
    pub d: u64,
    /// Cached tokens at or after the edit point that must re-write (the
    /// disturbed suffix of the prefix).
    pub c_after: u64,
    /// Total cached prefix tokens before the edit.
    pub c: u64,
}

impl PrefixReductionCandidate {
    /// Construct a candidate. `d` = tokens removed, `c_after` = cached tokens
    /// at/after the edit point that must re-write, `c` = total cached prefix.
    pub const fn new(d: u64, c_after: u64, c: u64) -> Self {
        Self { d, c_after, c }
    }
}

/// Why the gate chose KEEP.
///
/// `#[non_exhaustive]`: later phases add reasons (anti-pattern suppression,
/// quality floor), so callers must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeepReason {
    /// The branch inequality did not hold: breaking would cost more than it
    /// saves at the assumed reuse count.
    NetNegative,
    /// After the cut the remaining prefix `(c - d)` drops below the
    /// provider's cacheable floor, so the remainder re-bills at full input
    /// price every request. A hard KEEP.
    BelowMinPrefix,
    /// The row is unverified / sentinel: its multipliers are not trusted for
    /// a live break. Fail closed to KEEP.
    InsufficientData,
    /// There is nothing to remove (`d == 0` or `c_after == 0`).
    NoCandidate,
}

/// The gate's verdict over one candidate.
///
/// `#[non_exhaustive]`: more verdicts may be added, so callers must include a
/// wildcard arm. [`GateDecision::FreeBreak`] is reserved for a later phase
/// (eviction-guard / incidental re-warm); [`evaluate`] never returns this today.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GateDecision {
    /// Keep the original prefix.
    Keep { reason: KeepReason },
    /// Break the cache and apply the edit; net-positive at the assumed reuse.
    Break { delta_tokens: u64 },
    /// RESERVED for a later phase: a break that is free by construction
    /// (prefix already evicted, or an incidental re-warm that was happening
    /// anyway). Never returned today; reserved for a later eviction-guard increment.
    FreeBreak {
        delta_tokens: u64,
        reason: &'static str,
    },
}

impl GateDecision {
    /// Stable, append-only reduction-strategy token for the usage ledger.
    /// These strings are a public contract: never rename, only add.
    pub const fn strategy_str(&self) -> &'static str {
        match self {
            Self::Keep { .. } => "cost_gate:keep",
            Self::Break { .. } => "cost_gate:break",
            Self::FreeBreak { .. } => "cost_gate:free_break",
        }
    }
}

/// Break-even reuse count `K*`: the minimum number of future prefix reuses
/// (within the TTL window) at which breaking the cache turns net-positive.
///
/// Returns `None` when there is nothing to remove (`d == 0`), which would
/// otherwise divide by zero. Branch on [`CachePricingRow::auto_cacher`]:
/// - write-premium: `K* = (c_after * wm) / (d * rm)`.
/// - auto-cacher:   `K* = (c_after * (1 - rm)) / (d * rm)`.
///
/// Arithmetic is in `f64` (the `f32` row fields are widened) to avoid
/// precision loss on the deep DeepSeek read multipliers.
#[must_use]
pub fn break_even_k(row: &CachePricingRow, candidate: &PrefixReductionCandidate) -> Option<f64> {
    if candidate.d == 0 {
        return None;
    }
    // A non-positive read multiplier means reads are free, so no finite
    // break-even reuse count exists; return None rather than dividing by
    // zero (which would yield inf).
    if row.rm as f64 <= 0.0 {
        return None;
    }
    let c_after = candidate.c_after as f64;
    let d = candidate.d as f64;
    let wm = row.wm as f64;
    let rm = row.rm as f64;
    let read_savings_per_reuse = d * rm;
    let one_time_cost = if row.auto_cacher {
        c_after * (1.0 - rm)
    } else {
        c_after * wm
    };
    Some(one_time_cost / read_savings_per_reuse)
}

/// Decide KEEP vs BREAK for a candidate at an assumed reuse count `k`.
///
/// Guards run in order, each short-circuiting to KEEP:
/// 1. `!row.verified` -> `Keep { InsufficientData }`.
/// 2. `d == 0` or `c_after == 0` -> `Keep { NoCandidate }`.
/// 3. `(c - d) < row.min_prefix_tokens` -> `Keep { BelowMinPrefix }`.
/// 4. otherwise the branch inequality: BREAK iff `k * d * rm` beats the
///    one-time tax (write-premium: `c_after * wm`; auto-cacher:
///    `c_after * (1 - rm)`).
#[must_use]
pub fn evaluate(
    row: &CachePricingRow,
    candidate: &PrefixReductionCandidate,
    k: f64,
) -> GateDecision {
    if !row.verified {
        return GateDecision::Keep {
            reason: KeepReason::InsufficientData,
        };
    }
    if candidate.d == 0 || candidate.c_after == 0 {
        return GateDecision::Keep {
            reason: KeepReason::NoCandidate,
        };
    }
    if candidate.c.saturating_sub(candidate.d) < u64::from(row.min_prefix_tokens) {
        return GateDecision::Keep {
            reason: KeepReason::BelowMinPrefix,
        };
    }

    let c_after = candidate.c_after as f64;
    let d = candidate.d as f64;
    let wm = row.wm as f64;
    let rm = row.rm as f64;
    let read_savings = k * d * rm;
    let one_time_cost = if row.auto_cacher {
        c_after * (1.0 - rm)
    } else {
        c_after * wm
    };
    if read_savings > one_time_cost {
        GateDecision::Break {
            delta_tokens: candidate.d,
        }
    } else {
        GateDecision::Keep {
            reason: KeepReason::NetNegative,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_pricing::{CachePricingRow, lookup};

    // The doc's worked-result scenario, reused as the K* oracle:
    // 200k cached prefix, drop 50k oldest-first, so c_after == c == 200k.
    const D: u64 = 50_000;
    const C_AFTER: u64 = 200_000;
    const C: u64 = 200_000;

    fn scenario() -> PrefixReductionCandidate {
        PrefixReductionCandidate::new(D, C_AFTER, C)
    }

    // -- break_even_k oracle, lookup-driven where the row is reachable -----

    #[test]
    fn break_even_k_anthropic_5m_is_fifty() {
        // Arrange: write-premium row, wm=1.25, rm=0.10, auto_cacher=false.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        assert!(!row.auto_cacher, "anthropic must be write-premium");

        // Act
        let k_star = break_even_k(&row, &scenario()).expect("d > 0");

        // Assert: (200000 * 1.25) / (50000 * 0.10) == 50. The f32 row
        // multipliers widen to f64, so allow a small float tolerance.
        assert!((k_star - 50.0).abs() < 1e-4, "K* was {k_star}");
    }

    #[test]
    fn break_even_k_anthropic_1h_is_eighty() {
        // Arrange: the 1h tier flows a distinct row (wm=2.0) -- pins that the
        // tier-correct row reaches the math.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("1h"));
        assert_eq!(row.wm, 2.0);

        // Act
        let k_star = break_even_k(&row, &scenario()).expect("d > 0");

        // Assert: (200000 * 2.0) / (50000 * 0.10) == 80.
        assert!((k_star - 80.0).abs() < 1e-4, "K* was {k_star}");
    }

    #[test]
    fn break_even_k_openai_auto_cacher_is_thirty_six() {
        // Arrange: auto-cacher row (free writes), rm=0.10.
        let row = lookup("openai-responses", "gpt-5.5", None);
        assert!(row.auto_cacher, "openai-responses must be an auto-cacher");

        // Act
        let k_star = break_even_k(&row, &scenario()).expect("d > 0");

        // Assert: (200000 * (1 - 0.10)) / (50000 * 0.10) == 36.
        assert!((k_star - 36.0).abs() < 1e-4, "K* was {k_star}");
    }

    #[test]
    fn break_even_k_deepseek_pro_is_about_four_seventy_eight() {
        // Arrange: deepest read multiplier (rm=0.0083), auto-cacher.
        let row = lookup("openai-compat", "deepseek-v4-pro", None);
        assert!(row.auto_cacher);

        // Act
        let k_star = break_even_k(&row, &scenario()).expect("d > 0");

        // Assert: computed from the row's ACTUAL rm, not the doc's rounded
        // 0.008 (which gives ~496). (200000 * (1 - rm)) / (50000 * rm).
        let rm = row.rm as f64;
        let expected = (C_AFTER as f64 * (1.0 - rm)) / (D as f64 * rm);
        assert!((k_star - expected).abs() < 1e-6, "K* was {k_star}");
        assert!(
            (477.0..479.0).contains(&k_star),
            "K* {k_star} should land near 478 for rm=0.0083",
        );
    }

    #[test]
    fn break_even_k_returns_none_when_nothing_removed() {
        // Arrange: a verified row but d == 0.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let candidate = PrefixReductionCandidate::new(0, C_AFTER, C);

        // Act / Assert: no break-even, no divide-by-zero.
        assert_eq!(break_even_k(&row, &candidate), None);
    }

    #[test]
    fn break_even_k_pure_row_write_premium_matches_formula() {
        // A direct sentinel construction exercises the pure math without a
        // table lookup (the sentinel is write-premium, wm=2.0, rm=0.10).
        let row = CachePricingRow::sentinel();
        assert!(!row.auto_cacher);

        let k_star = break_even_k(&row, &scenario()).expect("d > 0");

        // (200000 * 2.0) / (50000 * 0.10) == 80.
        assert!((k_star - 80.0).abs() < 1e-4, "K* was {k_star}");
    }

    // -- evaluate verdicts: write-premium branch ---------------------------

    #[test]
    fn evaluate_write_premium_breaks_above_break_even_k() {
        // Arrange: K* == 50 for this anthropic 5m row.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));

        // Act: k just above the threshold.
        let decision = evaluate(&row, &scenario(), 50.0001);

        // Assert
        assert_eq!(decision, GateDecision::Break { delta_tokens: D });
    }

    #[test]
    fn evaluate_write_premium_keeps_below_break_even_k() {
        // Arrange: same row, K* == 50.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));

        // Act: k just below the threshold (also the doc's K=30 -> KEEP).
        let decision = evaluate(&row, &scenario(), 49.9999);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
        );
    }

    // -- evaluate verdicts: auto-cacher branch + branch-split proof --------

    #[test]
    fn evaluate_auto_cacher_breaks_above_its_lower_threshold() {
        // Arrange: openai auto-cacher, K* == 36.
        let row = lookup("openai-responses", "gpt-5.5", None);

        // Act
        let decision = evaluate(&row, &scenario(), 36.0001);

        // Assert
        assert_eq!(decision, GateDecision::Break { delta_tokens: D });
    }

    #[test]
    fn evaluate_auto_cacher_keeps_below_its_threshold() {
        // Arrange: openai auto-cacher, K* == 36.
        let row = lookup("openai-responses", "gpt-5.5", None);

        // Act: k just below the threshold.
        let decision = evaluate(&row, &scenario(), 35.9999);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
        );
    }

    #[test]
    fn auto_cacher_threshold_is_lower_than_write_premium_at_same_rm() {
        // Both rows share rm=0.10; the only difference is the write premium.
        // The auto-cacher (tau_eff = (1-rm)/rm = 9) must break at a strictly
        // lower K than the write-premium row (tau = wm/rm = 12.5). This is
        // the branch split made observable.
        let write_premium = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let auto_cacher = lookup("openai-responses", "gpt-5.5", None);
        assert_eq!(write_premium.rm, auto_cacher.rm, "same rm by construction");

        let k_wp = break_even_k(&write_premium, &scenario()).expect("d > 0");
        let k_ac = break_even_k(&auto_cacher, &scenario()).expect("d > 0");

        assert!(
            k_ac < k_wp,
            "auto-cacher K* {k_ac} must be below write-premium K* {k_wp}",
        );
    }

    #[test]
    fn break_even_k_mid_prefix_cut_uses_c_after_not_c() {
        // Arrange: a mid-prefix cut where the disturbed suffix that must
        // re-write (c_after) is far smaller than the total cached prefix (c).
        // Write-premium anthropic 5m row (wm=1.25, rm=0.10).
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let candidate = PrefixReductionCandidate::new(10_000, 50_000, 200_000);

        // Act
        let k_star = break_even_k(&row, &candidate).expect("d > 0");

        // Assert: K* = (c_after * wm) / (d * rm) = (50000 * 1.25) /
        // (10000 * 0.10) = 62.5. Proves the formula uses c_after (the
        // re-write cost), not the full prefix c.
        assert!((k_star - 62.5).abs() < 1e-4, "K* was {k_star}");
    }

    #[test]
    fn evaluate_at_exact_break_even_k_keeps_net_negative() {
        // Arrange: a row with f32-exact multipliers (wm=1.0, rm=0.125) and a
        // candidate (c_after=50_000, d=8_000) chosen so K* is EXACTLY 50.0
        // with no float drift: (50_000 * 1.0) / (8_000 * 0.125) == 50.0.
        // Built off the sentinel via overrides so it is verified.
        use crate::cache_pricing::CachePricingOverride;
        let ov = CachePricingOverride {
            wm: Some(1.0),
            rm: Some(0.125),
            min_prefix_tokens: Some(1),
            override_acknowledges_cost_risk: true,
            ..Default::default()
        };
        let row = CachePricingRow::sentinel()
            .with_overrides(&ov)
            .expect("accepted with ack");
        assert!(!row.auto_cacher, "must be write-premium");
        let candidate = PrefixReductionCandidate::new(8_000, 50_000, 200_000);
        let k_star = break_even_k(&row, &candidate).expect("d > 0");
        assert_eq!(k_star, 50.0, "K* must be exactly 50.0, was {k_star}");

        // Act: k EXACTLY at the threshold. The contract is strict `>`, so a
        // tie is not a Break.
        let decision = evaluate(&row, &candidate, 50.0);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
        );
    }

    // -- guard tests -------------------------------------------------------

    #[test]
    fn evaluate_unverified_row_keeps_insufficient_data_even_with_huge_k() {
        // Arrange: an unknown provider resolves to the unverified sentinel.
        let row = lookup("some-future-kind", "whatever-model", None);
        assert!(!row.verified);

        // Act: a wildly favorable k must NOT override the verified guard.
        let decision = evaluate(&row, &scenario(), 1_000_000.0);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::InsufficientData
            }
        );
    }

    #[test]
    fn evaluate_zero_delta_keeps_no_candidate() {
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let candidate = PrefixReductionCandidate::new(0, C_AFTER, C);
        let decision = evaluate(&row, &candidate, 1_000.0);
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::NoCandidate
            }
        );
    }

    #[test]
    fn evaluate_zero_c_after_keeps_no_candidate() {
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let candidate = PrefixReductionCandidate::new(D, 0, C);
        let decision = evaluate(&row, &candidate, 1_000.0);
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::NoCandidate
            }
        );
    }

    #[test]
    fn evaluate_below_min_prefix_keeps_even_with_favorable_k() {
        // Arrange: the Opus 4.8 row has min_prefix_tokens = 1024. Cut so the
        // remaining prefix (c - d) falls below that floor.
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        assert_eq!(row.min_prefix_tokens, 1024);
        // c = 1500, d = 1000 -> remaining 500 < 1024. c_after non-zero.
        let candidate = PrefixReductionCandidate::new(1_000, 1_500, 1_500);

        // Act: a hugely favorable k must still lose to the hard KEEP.
        let decision = evaluate(&row, &candidate, 1_000_000.0);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Keep {
                reason: KeepReason::BelowMinPrefix
            }
        );
    }

    // -- strategy_str token stability --------------------------------------

    #[test]
    fn strategy_str_tokens_are_stable() {
        assert_eq!(
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
            .strategy_str(),
            "cost_gate:keep"
        );
        assert_eq!(
            GateDecision::Break { delta_tokens: 1 }.strategy_str(),
            "cost_gate:break"
        );
        assert_eq!(
            GateDecision::FreeBreak {
                delta_tokens: 1,
                reason: "evicted"
            }
            .strategy_str(),
            "cost_gate:free_break"
        );
    }
}
