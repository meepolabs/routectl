//! Test-only fully populated `/status/usage` panel, folded by this panel's own
//! `assemble` from ledger-shaped rows.

use routectl_usage::{AggRow, GroupKey};
use serde_json::Value;

use std::path::Path;

use super::super::panel_fixtures::{FIXTURE_NOW_MS, at_epoch_ms, fixture_as_of};
use super::super::usage_health::{LedgerHealth, WindowObservation};
use super::super::utc_rfc3339;
use super::{Panel, SCHEMA_VERSION, UsagePanel, assemble, build_panel};
use crate::commands::usage::{WindowBounds, WindowFlag, window_bounds};
use crate::local_time::LocalZone;

const DAY_MS: i64 = 86_400_000;

/// The serialized populated usage envelope.
pub fn populated_usage() -> Value {
    serde_json::to_value(populated_usage_panel()).expect("the usage panel serializes")
}

/// Today's window over a dispatched group served in two eras (so the
/// coalesce runs), a pre-dispatch group with no provider, model or upstream,
/// errors in two classes, and an incomplete observation block.
pub fn populated_usage_panel() -> Panel<UsagePanel> {
    let bounds = WindowBounds {
        from_ms: FIXTURE_NOW_MS - DAY_MS / 2,
        to_ms: FIXTURE_NOW_MS + 1,
    };
    let sonnet = key(
        Some("sonnet"),
        Some("anthropic"),
        Some("claude-sonnet-4-5"),
        "fast",
    );
    let sonnet_old_era = GroupKey {
        provider_kind: None,
        ..sonnet.clone()
    };
    let aborted = key(None, None, None, "smart");
    let rows = vec![
        agg_row(sonnet.clone(), 40, 2, 0),
        agg_row(sonnet_old_era, 10, 0, 1),
        agg_row(aborted.clone(), 3, 2, 0),
    ];
    let breakdown = vec![
        (sonnet, "rate-limited".to_string(), 2),
        (aborted.clone(), "server-error".to_string(), 1),
        (aborted, "unclassified".to_string(), 1),
    ];
    let dto = assemble("today", bounds, rows, breakdown, observation(bounds));
    Panel::available(SCHEMA_VERSION, fixture_as_of(), dto)
}

/// The `window` envelope (`today` or `all`, UTC) at `now_ms` over the
/// ledger at `db_path`, read by the panel's own `build_panel` with no
/// record loss.
pub fn usage_panel_over(db_path: &Path, window: WindowFlag, now_ms: i64) -> Panel<UsagePanel> {
    let token = match window {
        WindowFlag::All => "all",
        _ => "today",
    };
    let bounds = window_bounds(&LocalZone::utc(), window, at_epoch_ms(now_ms));
    let panel = build_panel(
        db_path,
        token,
        bounds,
        LedgerHealth::default(),
        utc_rfc3339(now_ms),
    );
    assert!(panel.unavailable.is_none(), "the fixture ledger reads");
    panel
}

fn key(
    model: Option<&str>,
    provider: Option<&str>,
    upstream: Option<&str>,
    alias: &str,
) -> GroupKey {
    GroupKey {
        model: model.map(str::to_string),
        provider: provider.map(str::to_string),
        upstream: upstream.map(str::to_string),
        alias: alias.to_string(),
        provider_kind: provider.map(|_| "anthropic-api".to_string()),
        seat: None,
    }
}

/// One aggregate row of `requests`, `errors` of them failed and
/// `cancelled` of them abandoned, the rest ok. Every token dimension and
/// every reporting denominator is populated.
fn agg_row(key: GroupKey, requests: i64, errors: i64, cancelled: i64) -> AggRow {
    AggRow {
        key,
        requests,
        ok: requests - errors - cancelled - 1,
        errors,
        input_tokens: requests * 1_200,
        output_tokens: requests * 300,
        reasoning_tokens: requests * 40,
        cache_read_peak: 90_000,
        cache_read_avg: 45_000,
        cache_read_billed: requests * 45_000,
        cache_write_5m: requests * 100,
        cache_write_1h: requests * 20,
        server_tool_calls: 2,
        sum_ttfb_ms: requests * 400,
        ttfb_count: requests,
        gen_window_ms: requests * 2_000,
        gen_output_tokens: requests * 300,
        reasoning_present: requests,
        cache_read_present: requests - 1,
        cache_write_5m_present: requests - 1,
        cache_write_1h_present: requests - 1,
        server_tool_present: 1,
        stream_count: requests - 1,
        client_disconnect_total: 1,
        client_disconnect_pre_dispatch: 1,
        cancelled_total: cancelled,
        cache_write_tokens: requests * 120,
        cache_write_present: requests - 1,
        cache_write_lifetimes: None,
    }
}

fn observation(bounds: WindowBounds) -> WindowObservation {
    WindowObservation {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        wire_losses: 2,
        translation_drops: 1,
        local_refusals: 1,
        field_repairs: 3,
        missing_terminals: 1,
        request_caused_failures: 1,
        excluded_endpoint: 1,
        excluded_model: 2,
        excluded_target: 1,
        excluded_probe_run: 1,
        gap_rows: 1,
        gap_dropped: 4,
        complete: false,
    }
}
