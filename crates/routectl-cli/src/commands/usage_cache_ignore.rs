//! The cache-ignore block `routectl usage --detail` appends once per run.
//!
//! The block renders the shared [`CacheIgnoreReport`], so it agrees with
//! the daemon's log lines and with doctor: same fixed window (not the
//! window-flag / `--since` range), same lanes, same verdicts. It prints lane
//! identifiers and counts only, rendered by the same
//! [`CacheIgnoreLaneReport::identifiers`] the daemon's log lines and doctor
//! use.

use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use routectl_router::Config;
use routectl_usage::{CACHE_IGNORE_LANE_CAP, UsageDb};

use crate::cache_ignore::{
    CACHE_IGNORE_WINDOW, CacheIgnoreLaneReport, CacheIgnoreReport, CacheIgnoreStatus, LaneVerdict,
    NotEvaluatedReason, cache_ignore_report,
};
use crate::commands::seat_report::safe;

/// How long the block's ledger read may run before it reports
/// `unavailable: query_timeout` instead.
pub const CACHE_IGNORE_READ_BUDGET: Duration = Duration::from_secs(2);

const WINDOW_TS_FORMAT: &str = "%Y-%m-%d %H:%M";

const IGNORING_NOTE: &str = "markers sent, no cache activity";

/// Evaluate the report on `db` over the window ending at `now_ms` and render
/// it. Never fails: a ledger that cannot answer renders one
/// `unavailable: <class>` line.
///
/// `db` must not carry a caller-held deadline (see [`cache_ignore_report`]).
pub fn cache_ignore_block(db: &UsageDb, config: &Config, now_ms: i64, deadline: Instant) -> String {
    render_cache_ignore(&cache_ignore_report(db, config, now_ms, deadline))
}

/// Render one report as the `--detail` cache-ignore block.
pub fn render_cache_ignore(report: &CacheIgnoreReport) -> String {
    let mut out = format!(
        "cache-ignore: last {}h, {} .. {} (fixed window; window flags and --since do not apply)\n",
        CACHE_IGNORE_WINDOW.as_secs() / 3600,
        format_window_ts(report.window.from_ms),
        format_window_ts(report.window.to_ms),
    );
    match report.status {
        CacheIgnoreStatus::Unavailable(class) => {
            out.push_str(&format!("  unavailable: {class}\n"));
        }
        CacheIgnoreStatus::Evaluated if report.lanes.is_empty() => {
            out.push_str(
                "  no lane had qualifying turns in the window \
                 (consecutive marked turns within the cache lifetime)\n",
            );
        }
        CacheIgnoreStatus::Evaluated => {
            for lane in &report.lanes {
                out.push_str(&render_lane(lane));
            }
            if report.truncated {
                out.push_str(&format!(
                    "  ... more lanes than the {CACHE_IGNORE_LANE_CAP}-lane cap; \
                     lanes with no cache activity are shown first\n",
                ));
            }
        }
    }
    for reason in NotEvaluatedReason::ALL {
        let names: Vec<String> = report
            .not_evaluated
            .iter()
            .filter(|p| p.reason == reason)
            .map(|p| safe(&p.provider))
            .collect();
        if !names.is_empty() {
            out.push_str(&format!(
                "  not evaluated ({}): {}\n",
                reason.as_str(),
                names.join(", "),
            ));
        }
    }
    out
}

fn render_lane(lane: &CacheIgnoreLaneReport) -> String {
    let ids = lane.identifiers();
    let mut line = format!(
        "  provider={} seat={} model={} kind={} verdict={} evidence={} active={} sessions={}",
        ids.provider,
        ids.seat.unwrap_or_else(|| "-".to_owned()),
        ids.model.unwrap_or_else(|| "-".to_owned()),
        safe(&lane.provider_kind),
        lane.verdict.as_str(),
        lane.evidence_turns,
        lane.active_turns,
        lane.sessions,
    );
    if lane.verdict == LaneVerdict::Ignoring {
        line.push_str(&format!("  ({IGNORING_NOTE})"));
    }
    line.push('\n');
    line
}

/// Local time to the minute; the raw milliseconds when the instant is not
/// representable locally.
fn format_window_ts(ts_ms: i64) -> String {
    Local.timestamp_millis_opt(ts_ms).single().map_or_else(
        || ts_ms.to_string(),
        |dt| dt.format(WINDOW_TS_FORMAT).to_string(),
    )
}

#[cfg(test)]
#[path = "usage_cache_ignore_tests.rs"]
mod tests;
