//! Pure break-even COST GATE for prompt-cache prefix reduction.
//!
//! This module answers a single question with arithmetic only: given a
//! provider kind's [`CacheRules`] and a proposed prefix edit, is breaking
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
//! Two branches keyed on [`CacheRules::auto_cacher`]:
//! - WRITE-PREMIUM providers (`auto_cacher == false`, e.g. Anthropic /
//!   Bedrock) pay a real write tax on the re-warm, so
//!   BREAK iff `k * d * rm > c_after * wm`.
//! - AUTO-CACHER providers (`auto_cacher == true`, e.g. OpenAI-compatible
//!   and Responses upstreams) have free writes; the only one-time cost is
//!   re-sending the disturbed suffix at miss price instead of a cheap read,
//!   so BREAK iff `k * d * rm > c_after * (1 - rm)`.
//!
//! Every doubt resolves to KEEP: no candidate, or a post-cut prefix below
//! the cacheable floor, force KEEP before the branch inequality is even
//! consulted. A provider kind with no rules ([`cache_rules`] returns `None`)
//! never reaches [`evaluate`] / [`break_even_k`]: callers treat it as
//! unpriced.
//!
//! A separate, simpler question lives here too:
//! [`emission_break_even_k`] prices PLACING a breakpoint (1 write + `K`
//! reads) rather than editing a warm prefix, so it needs no candidate and no
//! token counts -- see its own docs.
//!
//! Purity: depends only on `routectl-core`'s provider-kind vocabulary and
//! std. Rules are per provider kind, not per model: nothing here reads a
//! price.

use routectl_core::provider_kind::config as kind;

/// Prompt-cache economics of one provider kind, as multipliers of the base
/// input price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheRules {
    /// Cache-write multiplier.
    pub wm: f64,
    /// Cache-read multiplier.
    pub rm: f64,
    /// How long a written prefix stays warm, in seconds.
    pub ttl_seconds: u32,
    /// Smallest prefix the provider caches, in tokens.
    pub min_prefix_tokens: u32,
    /// The provider caches on its own with no write premium, so a marker
    /// is never withheld on economic grounds.
    pub auto_cacher: bool,
}

const WRITE_PREMIUM_5M: CacheRules = CacheRules {
    wm: 1.25,
    rm: 0.1,
    ttl_seconds: 300,
    min_prefix_tokens: 4096,
    auto_cacher: false,
};

const OPENAI_COMPAT_RULES: CacheRules = CacheRules {
    wm: 1.0,
    rm: 0.1,
    ttl_seconds: 300,
    min_prefix_tokens: 4096,
    auto_cacher: true,
};

const OPENAI_RESPONSES_RULES: CacheRules = CacheRules {
    wm: 1.25,
    rm: 0.1,
    ttl_seconds: 86_400,
    min_prefix_tokens: 1024,
    auto_cacher: true,
};

/// The TTL window to measure reuse over when a provider kind has no
/// [`CacheRules`]: the shortest window any kind uses.
pub const UNPRICED_TTL_SECONDS: u32 = 300;

/// The cache rules for a `[providers.X] kind`, or `None` when the kind has
/// none (Gemini, or a kind this build does not know), which every caller
/// treats as unpriced.
#[must_use]
pub fn cache_rules(provider_kind: &str) -> Option<CacheRules> {
    match provider_kind {
        kind::ANTHROPIC_API | kind::BEDROCK => Some(WRITE_PREMIUM_5M),
        kind::OPENAI_COMPAT => Some(OPENAI_COMPAT_RULES),
        kind::OPENAI_RESPONSES => Some(OPENAI_RESPONSES_RULES),
        _ => None,
    }
}

/// A proposed prefix reduction handed to the gate. Immutable; the gate never
/// mutates it. Token counts are in prefix tokens.
///
/// `#[non_exhaustive]`: edit-position / breakpoint detail may be added,
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
/// `#[non_exhaustive]`: more reasons may be added (anti-pattern suppression,
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
    /// No cache rules to price against: [`cache_rules`] returned `None` for
    /// the target's provider kind. This module never derives it itself;
    /// CALLERS construct this variant directly when they choose not to
    /// invoke [`evaluate`] at all.
    InsufficientData,
    /// There is nothing to remove (`d == 0` or `c_after == 0`).
    NoCandidate,
}

/// The gate's verdict over one candidate.
///
/// `#[non_exhaustive]`: more verdicts may be added, so callers must include a
/// wildcard arm. [`GateDecision::FreeBreak`] covers the eviction-guard /
/// incidental-re-warm cases; [`evaluate`] never returns it, but consumers
/// match and render it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GateDecision {
    /// Keep the original prefix.
    Keep {
        /// Why the prefix is kept.
        reason: KeepReason,
    },
    /// Break the cache and apply the edit; net-positive at the assumed reuse.
    Break {
        /// Tokens saved by breaking.
        delta_tokens: u64,
    },
    /// A break that is free by construction (prefix already evicted, or an
    /// incidental re-warm that was happening anyway). Never returned by
    /// [`evaluate`]; it exists for the eviction-guard path and is matched
    /// and rendered by consumers.
    FreeBreak {
        /// Tokens saved by breaking.
        delta_tokens: u64,
        /// Why the break is free.
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
/// otherwise divide by zero. Branch on [`CacheRules::auto_cacher`]:
/// - write-premium: `K* = (c_after * wm) / (d * rm)`.
/// - auto-cacher:   `K* = (c_after * (1 - rm)) / (d * rm)`.
#[must_use]
pub fn break_even_k(rules: &CacheRules, candidate: &PrefixReductionCandidate) -> Option<f64> {
    if candidate.d == 0 {
        return None;
    }
    // A non-positive read multiplier means reads are free, so no finite
    // break-even reuse count exists; return None rather than dividing by
    // zero (which would yield inf).
    if rules.rm <= 0.0 {
        return None;
    }
    let read_savings_per_reuse = candidate.d as f64 * rules.rm;
    Some(one_time_cost(rules, candidate.c_after) / read_savings_per_reuse)
}

/// Emission-side break-even reuse count `K*`: the minimum per-turn prefix
/// reuse count at which PLACING a cache breakpoint pays for its write
/// premium, over the honest 1-write + `K`-reads model:
///
/// ```text
/// no marker:   (1 + K) * C * 1.0
/// with marker: C * wm + K * C * rm
/// marker wins iff  wm + K*rm < 1 + K   ->   K* = (wm - 1) / (1 - rm)
/// ```
///
/// The base input price `C` cancels, so this needs no token counts and no
/// [`PrefixReductionCandidate`] -- it is a per-kind constant. It is a
/// deliberately separate function from [`break_even_k`], which prices a
/// prefix EDIT and requires a candidate that has no emission-side analogue.
///
/// Returns `None` to mean NEVER SUPPRESS -- no finite positive break-even
/// exists, or withholding a marker could not pay:
/// - `wm <= 1.0`: writes carry no premium, so `K*` would be zero or
///   negative.
/// - `rm >= 1.0`: a read costs at least a fresh miss, which would make the
///   denominator zero or negative.
/// - [`CacheRules::auto_cacher`]: a provider that caches for free must never
///   have a marker withheld on economic grounds. This is checked
///   explicitly rather than inferred from `wm`: the Responses rules carry a
///   `wm` above `1.0`.
#[must_use]
pub fn emission_break_even_k(rules: &CacheRules) -> Option<f64> {
    if rules.auto_cacher || rules.wm <= 1.0 || rules.rm >= 1.0 {
        return None;
    }
    Some((rules.wm - 1.0) / (1.0 - rules.rm))
}

/// Decide KEEP vs BREAK for a candidate at an assumed reuse count `k`.
///
/// Guards run in order, each short-circuiting to KEEP:
/// 1. `d == 0` or `c_after == 0` -> `Keep { NoCandidate }`.
/// 2. `(c - d) < rules.min_prefix_tokens` -> `Keep { BelowMinPrefix }`.
/// 3. otherwise the branch inequality: BREAK iff `k * d * rm` beats the
///    one-time tax (write-premium: `c_after * wm`; auto-cacher:
///    `c_after * (1 - rm)`).
#[must_use]
pub fn evaluate(rules: &CacheRules, candidate: &PrefixReductionCandidate, k: f64) -> GateDecision {
    if candidate.d == 0 || candidate.c_after == 0 {
        return GateDecision::Keep {
            reason: KeepReason::NoCandidate,
        };
    }
    if candidate.c.saturating_sub(candidate.d) < u64::from(rules.min_prefix_tokens) {
        return GateDecision::Keep {
            reason: KeepReason::BelowMinPrefix,
        };
    }

    let read_savings = k * candidate.d as f64 * rules.rm;
    if read_savings > one_time_cost(rules, candidate.c_after) {
        GateDecision::Break {
            delta_tokens: candidate.d,
        }
    } else {
        GateDecision::Keep {
            reason: KeepReason::NetNegative,
        }
    }
}

/// The one-time tax of breaking a warm prefix whose disturbed suffix is
/// `c_after` tokens: a re-write at `wm`, or for an auto-cacher a re-send at
/// miss price instead of a read.
fn one_time_cost(rules: &CacheRules, c_after: u64) -> f64 {
    let c_after = c_after as f64;
    if rules.auto_cacher {
        c_after * (1.0 - rules.rm)
    } else {
        c_after * rules.wm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The doc's worked-result scenario, reused as the K* oracle:
    // 200k cached prefix, drop 50k oldest-first, so c_after == c == 200k.
    const D: u64 = 50_000;
    const C_AFTER: u64 = 200_000;
    const C: u64 = 200_000;

    fn scenario() -> PrefixReductionCandidate {
        PrefixReductionCandidate::new(D, C_AFTER, C)
    }

    fn rules_for(provider_kind: &str) -> CacheRules {
        cache_rules(provider_kind).expect("kind carries cache rules")
    }

    /// Write-premium rules with arbitrary multipliers, for pinning the
    /// formulas independently of any shipped constant.
    fn write_premium(wm: f64, rm: f64) -> CacheRules {
        CacheRules {
            wm,
            rm,
            ttl_seconds: 300,
            min_prefix_tokens: 1,
            auto_cacher: false,
        }
    }

    // -- cache_rules: the per-kind constants ---------------------------------

    #[test]
    fn cache_rules_per_provider_kind_match_the_shipped_constants() {
        // Each row restates the economics the provider's catch-all cache row
        // carried before rules became per-kind constants.
        let cases: [(&str, Option<CacheRules>); 5] = [
            (
                "anthropic-api",
                Some(CacheRules {
                    wm: 1.25,
                    rm: 0.1,
                    ttl_seconds: 300,
                    min_prefix_tokens: 4096,
                    auto_cacher: false,
                }),
            ),
            (
                "bedrock",
                Some(CacheRules {
                    wm: 1.25,
                    rm: 0.1,
                    ttl_seconds: 300,
                    min_prefix_tokens: 4096,
                    auto_cacher: false,
                }),
            ),
            (
                "openai-compat",
                Some(CacheRules {
                    wm: 1.0,
                    rm: 0.1,
                    ttl_seconds: 300,
                    min_prefix_tokens: 4096,
                    auto_cacher: true,
                }),
            ),
            (
                "openai-responses",
                Some(CacheRules {
                    wm: 1.25,
                    rm: 0.1,
                    ttl_seconds: 86_400,
                    min_prefix_tokens: 1024,
                    auto_cacher: true,
                }),
            ),
            ("gemini", None),
        ];
        for (provider_kind, expected) in cases {
            assert_eq!(
                cache_rules(provider_kind),
                expected,
                "cache rules for {provider_kind}",
            );
        }
    }

    #[test]
    fn cache_rules_none_for_an_unknown_kind() {
        assert_eq!(cache_rules("some-future-kind"), None);
        assert_eq!(cache_rules(""), None);
    }

    // -- break_even_k --------------------------------------------------------

    #[test]
    fn break_even_k_per_provider_kind() {
        // write-premium: (200000 * 1.25) / (50000 * 0.10) == 50.
        // auto-cacher:   (200000 * (1 - 0.10)) / (50000 * 0.10) == 36.
        let cases = [
            ("anthropic-api", 50.0),
            ("bedrock", 50.0),
            ("openai-compat", 36.0),
            ("openai-responses", 36.0),
        ];
        for (provider_kind, expected) in cases {
            let k_star = break_even_k(&rules_for(provider_kind), &scenario()).expect("d > 0");
            assert!(
                (k_star - expected).abs() < 1e-9,
                "{provider_kind}: K* was {k_star}, expected {expected}",
            );
        }
    }

    #[test]
    fn break_even_k_returns_none_when_nothing_removed() {
        let candidate = PrefixReductionCandidate::new(0, C_AFTER, C);
        assert_eq!(break_even_k(&rules_for("anthropic-api"), &candidate), None);
    }

    #[test]
    fn break_even_k_returns_none_when_reads_are_free() {
        assert_eq!(break_even_k(&write_premium(1.25, 0.0), &scenario()), None);
    }

    #[test]
    fn break_even_k_write_premium_scales_with_wm() {
        // (200000 * 2.0) / (50000 * 0.10) == 80.
        let k_star = break_even_k(&write_premium(2.0, 0.1), &scenario()).expect("d > 0");
        assert!((k_star - 80.0).abs() < 1e-9, "K* was {k_star}");
    }

    #[test]
    fn break_even_k_mid_prefix_cut_uses_c_after_not_c() {
        // K* = (c_after * wm) / (d * rm) = (50000 * 1.25) / (10000 * 0.10)
        // = 62.5. Proves the formula uses c_after (the re-write cost), not
        // the full prefix c.
        let candidate = PrefixReductionCandidate::new(10_000, 50_000, 200_000);
        let k_star = break_even_k(&rules_for("anthropic-api"), &candidate).expect("d > 0");
        assert!((k_star - 62.5).abs() < 1e-9, "K* was {k_star}");
    }

    #[test]
    fn auto_cacher_threshold_is_lower_than_write_premium_at_same_rm() {
        // Same rm and wm; the only difference is the auto_cacher flag. The
        // auto-cacher (tau_eff = (1-rm)/rm = 9) must break at a strictly
        // lower K than the write-premium kind (tau = wm/rm = 12.5).
        let wp = rules_for("anthropic-api");
        let ac = rules_for("openai-responses");
        assert_eq!((wp.wm, wp.rm), (ac.wm, ac.rm), "same multipliers");

        let k_wp = break_even_k(&wp, &scenario()).expect("d > 0");
        let k_ac = break_even_k(&ac, &scenario()).expect("d > 0");
        assert!(k_ac < k_wp, "auto-cacher K* {k_ac} must be below {k_wp}");
    }

    // -- emission_break_even_k -----------------------------------------------

    #[test]
    fn emission_break_even_k_per_provider_kind() {
        // Write-premium kinds: (1.25 - 1) / (1 - 0.10) == 0.2777...
        // Auto-cachers never suppress, including openai-responses whose wm
        // is above 1.0, so the auto_cacher flag (not wm) decides it.
        let cases = [
            ("anthropic-api", Some(0.25 / 0.9)),
            ("bedrock", Some(0.25 / 0.9)),
            ("openai-compat", None),
            ("openai-responses", None),
        ];
        for (provider_kind, expected) in cases {
            let got = emission_break_even_k(&rules_for(provider_kind));
            match (got, expected) {
                (Some(g), Some(e)) => {
                    assert!((g - e).abs() < 1e-12, "{provider_kind}: K* was {g}");
                }
                _ => assert_eq!(got, expected, "{provider_kind}"),
            }
        }
    }

    #[test]
    fn emission_break_even_k_never_suppresses_without_a_finite_break_even() {
        let cases = [
            ("no write premium", write_premium(1.0, 0.1)),
            ("write cheaper than a miss", write_premium(0.5, 0.1)),
            ("read costs a full miss", write_premium(1.25, 1.0)),
        ];
        for (label, rules) in cases {
            assert_eq!(emission_break_even_k(&rules), None, "{label}");
        }
    }

    #[test]
    fn emission_break_even_k_is_far_below_the_edit_side_k() {
        let rules = rules_for("anthropic-api");
        let emission = emission_break_even_k(&rules).expect("wm > 1.0");
        let edit = break_even_k(&rules, &scenario()).expect("d > 0");
        assert!(emission < edit, "emission K* {emission} vs edit K* {edit}");
    }

    // -- evaluate ------------------------------------------------------------

    #[test]
    fn evaluate_breaks_above_and_keeps_below_the_break_even_k() {
        let net_negative = GateDecision::Keep {
            reason: KeepReason::NetNegative,
        };
        let brk = GateDecision::Break { delta_tokens: D };
        let cases = [
            ("write-premium above", "anthropic-api", 50.0001, brk.clone()),
            (
                "write-premium below",
                "anthropic-api",
                49.9999,
                net_negative.clone(),
            ),
            ("auto-cacher above", "openai-responses", 36.0001, brk),
            (
                "auto-cacher below",
                "openai-responses",
                35.9999,
                net_negative,
            ),
        ];
        for (label, provider_kind, k, expected) in cases {
            assert_eq!(
                evaluate(&rules_for(provider_kind), &scenario(), k),
                expected,
                "{label}",
            );
        }
    }

    #[test]
    fn evaluate_at_exact_break_even_k_keeps_net_negative() {
        // f64-exact multipliers (wm=1.0, rm=0.125) and a candidate chosen so
        // K* is EXACTLY 50.0: (50_000 * 1.0) / (8_000 * 0.125) == 50.0. The
        // contract is strict `>`, so a tie is not a Break.
        let rules = write_premium(1.0, 0.125);
        let candidate = PrefixReductionCandidate::new(8_000, 50_000, 200_000);
        assert_eq!(break_even_k(&rules, &candidate), Some(50.0));

        assert_eq!(
            evaluate(&rules, &candidate, 50.0),
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
        );
    }

    #[test]
    fn evaluate_keeps_no_candidate_when_nothing_to_remove() {
        let rules = rules_for("anthropic-api");
        let cases = [
            ("d == 0", PrefixReductionCandidate::new(0, C_AFTER, C)),
            ("c_after == 0", PrefixReductionCandidate::new(D, 0, C)),
        ];
        for (label, candidate) in cases {
            assert_eq!(
                evaluate(&rules, &candidate, 1_000_000.0),
                GateDecision::Keep {
                    reason: KeepReason::NoCandidate
                },
                "{label}",
            );
        }
    }

    #[test]
    fn evaluate_below_min_prefix_keeps_even_with_favorable_k() {
        // The anthropic-api floor is 4096 tokens: c = 5000, d = 1000 leaves
        // 4000 below it, while c = 6000 leaves 5000 above it (control).
        let rules = rules_for("anthropic-api");
        let below = PrefixReductionCandidate::new(1_000, 5_000, 5_000);
        let above = PrefixReductionCandidate::new(1_000, 5_000, 6_000);

        assert_eq!(
            evaluate(&rules, &below, 1_000_000.0),
            GateDecision::Keep {
                reason: KeepReason::BelowMinPrefix
            }
        );
        assert_eq!(
            evaluate(&rules, &above, 1_000_000.0),
            GateDecision::Break {
                delta_tokens: 1_000
            }
        );
    }

    #[test]
    fn insufficient_data_is_caller_constructed_not_evaluate_derived() {
        // Callers construct `InsufficientData` when a kind has no rules; pin
        // that it round-trips through `strategy_str` like any other reason.
        let decision = GateDecision::Keep {
            reason: KeepReason::InsufficientData,
        };
        assert_eq!(decision.strategy_str(), "cost_gate:keep");
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
