//! Pure cost estimation over a `UsageRecord` and a per-million-token
//! rate table.
//!
//! This module is leaf-safe: it depends on nothing outside this crate.
//! The router owns the TOML pricing config and glob resolution; the
//! rates land here as a plain `Rates` value so the cost math stays a
//! pure function with no config or router coupling.
//!
//! Money note: monetary amounts are `f64`. This is an ESTIMATE surfaced
//! for operator visibility, not ledger-grade accounting -- rounding
//! drift at the sub-cent level is acceptable here, so binary
//! floating-point is fine.

use crate::record::UsageRecord;

const TOKENS_PER_MTOK: f64 = 1_000_000.0;

/// Per-million-token rates (USD), one optional field per billable
/// dimension. A `None` field means "no price known for this dimension";
/// the dimension then contributes nothing to the estimate. This is a
/// usage-owned mirror of the router's pricing config -- deliberately NOT
/// the same type, to keep this crate a leaf.
///
/// There is no reasoning rate: Anthropic bills reasoning tokens as
/// output, so reasoning never appears as a separate cost dimension.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rates {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_5m_per_mtok: Option<f64>,
    pub cache_write_1h_per_mtok: Option<f64>,
}

/// Per-dimension cost breakdown plus the total, all in USD. The five
/// component fields always sum to `total_usd`.
#[derive(Debug, Clone, PartialEq)]
pub struct CostBreakdown {
    pub total_usd: f64,
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_5m_usd: f64,
    pub cache_write_1h_usd: f64,
}

/// Estimate the cost of one usage row against a rate table.
///
/// Returns `None` when the rate table is entirely unpriced (all five
/// fields `None`) -- the caller decides how to display an unpriced row.
/// Otherwise returns a breakdown where each dimension contributes
/// `tokens * rate / 1_000_000` only when BOTH the row's token count and
/// the matching rate are present; any missing half makes that dimension
/// contribute `0.0`.
///
/// Return contract (the display layer depends on this): `None` means no
/// price is configured (all rates `None`); `Some(total_usd == 0.0)`
/// means the record IS priced but carries no billable tokens. Callers
/// distinguish "n/a (subscription)" from "$0.00" using the provider /
/// auth context, NOT this return value alone.
///
/// Reasoning tokens are intentionally excluded: they are billed as
/// output upstream, so counting them here would double-charge.
pub fn estimate_cost(record: &UsageRecord, rates: &Rates) -> Option<CostBreakdown> {
    estimate_cost_tokens(
        opt_u64_to_i64(record.input_tokens),
        opt_u64_to_i64(record.output_tokens),
        opt_u64_to_i64(record.reasoning_tokens),
        opt_u64_to_i64(record.cache_read),
        opt_u64_to_i64(record.cache_write_5m),
        opt_u64_to_i64(record.cache_write_1h),
        rates,
    )
}

/// Estimate the cost of an AGGREGATE of usage rows from already-summed
/// token counts (SQLite `SUM(...)` results are `i64`). This is the entry
/// point the `routectl usage` CLI uses after rolling fine-grained
/// `AggRow`s into a display group.
///
/// Same return contract as [`estimate_cost`]: `None` when the rate table
/// is entirely unpriced (all five fields `None`); otherwise a breakdown
/// where each dimension contributes `tokens * rate / 1_000_000` only when
/// both the token count and the matching rate are present.
///
/// `reasoning` is accepted for symmetry with the aggregate shape but is
/// NOT priced: reasoning tokens are billed as output upstream, so charging
/// them here would double-count.
pub fn estimate_cost_tokens(
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    rates: &Rates,
) -> Option<CostBreakdown> {
    // `reasoning` is part of the aggregate shape but never priced.
    let _ = reasoning;

    let all_unpriced = rates.input_per_mtok.is_none()
        && rates.output_per_mtok.is_none()
        && rates.cache_read_per_mtok.is_none()
        && rates.cache_write_5m_per_mtok.is_none()
        && rates.cache_write_1h_per_mtok.is_none();
    if all_unpriced {
        return None;
    }

    let input_usd = component(input, rates.input_per_mtok);
    let output_usd = component(output, rates.output_per_mtok);
    let cache_read_usd = component(cache_read, rates.cache_read_per_mtok);
    let cache_write_5m_usd = component(cache_write_5m, rates.cache_write_5m_per_mtok);
    let cache_write_1h_usd = component(cache_write_1h, rates.cache_write_1h_per_mtok);

    let total_usd =
        input_usd + output_usd + cache_read_usd + cache_write_5m_usd + cache_write_1h_usd;

    Some(CostBreakdown {
        total_usd,
        input_usd,
        output_usd,
        cache_read_usd,
        cache_write_5m_usd,
        cache_write_1h_usd,
    })
}

/// Convert a record's `Option<u64>` token field to the `i64` the
/// aggregate-token entry point speaks. `None` -> 0; a count past
/// `i64::MAX` (never happens for real token totals) saturates.
fn opt_u64_to_i64(tokens: Option<u64>) -> i64 {
    tokens
        .map(|t| i64::try_from(t).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// One dimension's contribution: zero unless a rate is present and the
/// token count is positive. Token counts are SQLite SUMs (`i64`); rates
/// are USD per million tokens.
fn component(tokens: i64, rate: Option<f64>) -> f64 {
    match rate {
        Some(r) if tokens > 0 => tokens as f64 * r / TOKENS_PER_MTOK,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Outcome, UsageRecord};

    /// Build a usage row with all-None token fields, then let the caller
    /// override the dimensions a given test cares about. Keeps the cost
    /// tests terse and focused on the token/rate interplay.
    fn record_with_tokens(
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write_5m: Option<u64>,
        cache_write_1h: Option<u64>,
    ) -> UsageRecord {
        UsageRecord {
            ts_start: 0,
            ts_end: 0,
            request_id: "req".to_string(),
            ingress_dialect: "anthropic".to_string(),
            requested_model: "m".to_string(),
            alias: "a".to_string(),
            model: None,
            upstream: None,
            provider: None,
            provider_kind: None,
            seat: None,
            session_id: None,
            stream: false,
            max_tokens_req: None,
            tool_count: 0,
            thinking_req: None,
            thinking_req_kind: None,
            msg_count: 1,
            service_tier: None,
            outcome: Outcome::Ok,
            http_status: None,
            error_class: None,
            finish_reason: None,
            attempt_count: 1,
            fallback_count: 0,
            latency_ms: 0,
            ttfb_ms: None,
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
            cache_read,
            cache_write_5m,
            cache_write_1h,
            server_tool_use: None,
            quota_claim: None,
            quota_status: None,
            quota_overage_status: None,
            quota_utilization: None,
            quota_overage_utilization: None,
            quota_reset: None,
            quota_extras: None,
            extra: None,
            strategy: None,
            reduction_strategy: None,
            selection_decision: None,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
        }
    }

    fn all_rates() -> Rates {
        Rates {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(15.0),
            cache_read_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: Some(6.0),
        }
    }

    #[test]
    fn all_rates_and_all_dimensions_sum_to_total() {
        // Arrange
        let record = record_with_tokens(
            Some(1_000_000),
            Some(1_000_000),
            Some(1_000_000),
            Some(1_000_000),
            Some(1_000_000),
        );
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert
        assert_eq!(breakdown.input_usd, 3.0);
        assert_eq!(breakdown.output_usd, 15.0);
        assert_eq!(breakdown.cache_read_usd, 0.3);
        assert_eq!(breakdown.cache_write_5m_usd, 3.75);
        assert_eq!(breakdown.cache_write_1h_usd, 6.0);
        let expected = 3.0 + 15.0 + 0.3 + 3.75 + 6.0;
        assert_eq!(breakdown.total_usd, expected);
        let component_sum = breakdown.input_usd
            + breakdown.output_usd
            + breakdown.cache_read_usd
            + breakdown.cache_write_5m_usd
            + breakdown.cache_write_1h_usd;
        assert_eq!(breakdown.total_usd, component_sum);
    }

    #[test]
    fn dimension_with_tokens_but_no_rate_contributes_zero() {
        // Arrange: output has tokens but no output rate.
        let record = record_with_tokens(Some(1_000_000), Some(500_000), None, None, None);
        let rates = Rates {
            input_per_mtok: Some(3.0),
            output_per_mtok: None,
            ..Rates::default()
        };

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert
        assert_eq!(breakdown.input_usd, 3.0);
        assert_eq!(breakdown.output_usd, 0.0);
        assert_eq!(breakdown.total_usd, 3.0);
    }

    #[test]
    fn all_none_rates_returns_none() {
        // Arrange
        let record = record_with_tokens(Some(1_000_000), Some(1_000_000), None, None, None);
        let rates = Rates::default();

        // Act
        let result = estimate_cost(&record, &rates);

        // Assert
        assert!(result.is_none(), "fully-unpriced table => None");
    }

    /// Priced table but no billable tokens => `Some(0.0)`, NOT `None`.
    /// Pins the None-vs-Some(0.0) contract the display layer relies on:
    /// `None` = unpriced; `Some(0.0)` = priced with zero billable tokens.
    #[test]
    fn priced_but_no_tokens_returns_some_zero() {
        // Arrange: every token dimension None, every rate Some.
        let record = record_with_tokens(None, None, None, None, None);
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced => Some, not None");

        // Assert
        assert_eq!(breakdown.total_usd, 0.0, "no billable tokens => $0.00");
        assert_eq!(breakdown.input_usd, 0.0);
        assert_eq!(breakdown.output_usd, 0.0);
        assert_eq!(breakdown.cache_read_usd, 0.0);
        assert_eq!(breakdown.cache_write_5m_usd, 0.0);
        assert_eq!(breakdown.cache_write_1h_usd, 0.0);
    }

    #[test]
    fn null_token_dimension_is_skipped_while_others_compute() {
        // Arrange: input present, output token count is None.
        let record = record_with_tokens(Some(2_000_000), None, None, None, None);
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert
        assert_eq!(breakdown.input_usd, 6.0);
        assert_eq!(breakdown.output_usd, 0.0);
        assert_eq!(breakdown.total_usd, 6.0);
    }

    #[test]
    fn per_dimension_components_each_correct() {
        // Arrange: distinct token counts so each component is unique.
        let record = record_with_tokens(
            Some(100_000),
            Some(200_000),
            Some(400_000),
            Some(800_000),
            Some(1_600_000),
        );
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert
        assert_eq!(breakdown.input_usd, 100_000.0 * 3.0 / 1_000_000.0);
        assert_eq!(breakdown.output_usd, 200_000.0 * 15.0 / 1_000_000.0);
        assert_eq!(breakdown.cache_read_usd, 400_000.0 * 0.3 / 1_000_000.0);
        assert_eq!(breakdown.cache_write_5m_usd, 800_000.0 * 3.75 / 1_000_000.0);
        assert_eq!(
            breakdown.cache_write_1h_usd,
            1_600_000.0 * 6.0 / 1_000_000.0
        );
    }

    #[test]
    fn reasoning_tokens_do_not_affect_cost() {
        // Arrange: a row with reasoning tokens set but no separate
        // reasoning rate exists; reasoning must not be charged here.
        let mut record = record_with_tokens(Some(1_000_000), None, None, None, None);
        record.reasoning_tokens = Some(5_000_000);
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert: only input charged; reasoning ignored entirely.
        assert_eq!(breakdown.total_usd, 3.0);
    }

    #[test]
    fn token_entry_point_matches_hand_computed_sum() {
        // Arrange: distinct counts per dimension so each term is visible.
        let rates = all_rates();

        // Act
        let breakdown =
            estimate_cost_tokens(100_000, 200_000, 9_999, 400_000, 800_000, 1_600_000, &rates)
                .expect("priced");

        // Assert: hand-computed per-dimension terms; reasoning (9_999) ignored.
        let input = 100_000.0 * 3.0 / 1_000_000.0;
        let output = 200_000.0 * 15.0 / 1_000_000.0;
        let cache_read = 400_000.0 * 0.3 / 1_000_000.0;
        let cw5 = 800_000.0 * 3.75 / 1_000_000.0;
        let cw1 = 1_600_000.0 * 6.0 / 1_000_000.0;
        assert_eq!(breakdown.input_usd, input);
        assert_eq!(breakdown.output_usd, output);
        assert_eq!(breakdown.cache_read_usd, cache_read);
        assert_eq!(breakdown.cache_write_5m_usd, cw5);
        assert_eq!(breakdown.cache_write_1h_usd, cw1);
        assert_eq!(breakdown.total_usd, input + output + cache_read + cw5 + cw1);
    }

    #[test]
    fn token_entry_point_all_none_rates_returns_none() {
        // Arrange + Act
        let result = estimate_cost_tokens(1_000, 1_000, 0, 0, 0, 0, &Rates::default());

        // Assert
        assert!(result.is_none(), "fully-unpriced table => None");
    }

    #[test]
    fn token_entry_point_priced_but_zero_tokens_returns_some_zero() {
        // Arrange: priced table, no billable tokens.
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost_tokens(0, 0, 0, 0, 0, 0, &rates).expect("priced => Some");

        // Assert
        assert_eq!(breakdown.total_usd, 0.0);
    }

    #[test]
    fn token_entry_point_zero_dimension_skips_while_others_compute() {
        // Arrange: input present, output count zero.
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost_tokens(2_000_000, 0, 0, 0, 0, 0, &rates).expect("priced");

        // Assert
        assert_eq!(breakdown.input_usd, 6.0);
        assert_eq!(breakdown.output_usd, 0.0);
        assert_eq!(breakdown.total_usd, 6.0);
    }

    /// End-to-end regression pin for the cache-double-count fix. After the
    /// capture-site fix, the `input_tokens` column holds cache-EXCLUSIVE
    /// new input (here 100), with cache_read (600) and cache_write_5m (300)
    /// as separate disjoint dimensions. The total must price each dimension
    /// ONCE -- NOT the pre-fix figure where a cache-inclusive `input_tokens`
    /// of 1000 re-charged the 600 read + 300 write tokens at the input rate.
    #[test]
    fn cache_exclusive_input_is_not_double_counted() {
        // Arrange: a post-fix row. input_tokens is the new input only.
        let record = record_with_tokens(
            Some(100), // cache-exclusive new input
            Some(200), // output
            Some(600), // cache_read
            Some(300), // cache_write_5m
            None,      // cache_write_1h: no 1h write
        );
        let rates = all_rates();

        // Act
        let breakdown = estimate_cost(&record, &rates).expect("priced");

        // Assert: each dimension priced exactly once at its own rate.
        let expected = 100.0 * 3.0 / 1_000_000.0      // input
            + 200.0 * 15.0 / 1_000_000.0              // output
            + 600.0 * 0.3 / 1_000_000.0               // cache_read
            + 300.0 * 3.75 / 1_000_000.0; // cache_write_5m
        assert_eq!(breakdown.total_usd, expected);

        // And explicitly NOT the pre-fix double-count, where a
        // cache-inclusive input_tokens of 1000 would have re-priced the
        // 600 read + 300 write tokens at the input rate.
        let pre_fix_double_count = 1000.0 * 3.0 / 1_000_000.0
            + 200.0 * 15.0 / 1_000_000.0
            + 600.0 * 0.3 / 1_000_000.0
            + 300.0 * 3.75 / 1_000_000.0;
        assert_ne!(breakdown.total_usd, pre_fix_double_count);
    }
}
