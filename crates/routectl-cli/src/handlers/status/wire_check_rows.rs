//! The live wire check's rows on the `/status/health` panel: what the router's
//! wire-check counters and findings ring say happened inside the configured
//! status window.
//!
//! Every field is a closed label, a sanitized path, a count or a unix second.
//! The source snapshot carries no body content, and this projection adds none.

use serde::Serialize;

use routectl_router::wire_check_state::{
    CounterReading, FieldRepairOutcome, WireCheckSnapshot, WireFinding,
};

/// The wire-check section of the health panel.
#[derive(Debug, Clone, Serialize)]
pub(super) struct WireCheckRows {
    /// The status window the rows below are filtered to, in seconds.
    window_secs: u64,
    /// One row per (lane, path class) with a loss seen inside the window.
    loss: Vec<LossRow>,
    /// The unexplained differences the findings ring holds from inside the
    /// window, by path, oldest first.
    loss_findings: Vec<LossFindingRow>,
    /// One row per (lane, outcome) of checks run inside the window.
    checks: Vec<CheckRow>,
    /// Upstream rejections field repair did not repair, inside the window,
    /// oldest first.
    unrepaired_rejections: Vec<UnrepairedRejectionRow>,
    /// One row per lane of requests refused locally inside the window.
    local_refusals: Vec<LocalRefusalRow>,
}

#[derive(Debug, Clone, Serialize)]
struct LossRow {
    lane: &'static str,
    path_class: &'static str,
    count: u64,
    last_seen_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct LossFindingRow {
    lane: &'static str,
    leg: &'static str,
    path: String,
    path_class: &'static str,
    ts_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CheckRow {
    lane: &'static str,
    outcome: &'static str,
    count: u64,
    last_seen_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct UnrepairedRejectionRow {
    lane: &'static str,
    leg: &'static str,
    path: String,
    field_repair_outcome: &'static str,
    provenance: &'static str,
    ts_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct LocalRefusalRow {
    lane: &'static str,
    count: u64,
    last_seen_unix_secs: u64,
}

const fn current(reading: &CounterReading, window_secs: u64, now_unix_secs: u64) -> bool {
    reading.count > 0 && reading.seen_within(window_secs, now_unix_secs)
}

fn loss_finding_row(finding: &WireFinding) -> Option<LossFindingRow> {
    let class = finding.class?;
    Some(LossFindingRow {
        lane: finding.lane.label(),
        leg: finding.leg.label(),
        path: finding.path.as_str().to_string(),
        path_class: class.label(),
        ts_unix_secs: finding.ts_unix_secs,
    })
}

fn unrepaired_row(finding: &WireFinding) -> Option<UnrepairedRejectionRow> {
    let field_repair_outcome = finding
        .field_repair_outcome
        .filter(|o| *o == FieldRepairOutcome::Unrepaired)?;
    let provenance = finding.provenance?;
    Some(UnrepairedRejectionRow {
        lane: finding.lane.label(),
        leg: finding.leg.label(),
        path: finding.path.as_str().to_string(),
        field_repair_outcome: field_repair_outcome.label(),
        provenance: provenance.label(),
        ts_unix_secs: finding.ts_unix_secs,
    })
}

/// Project `snapshot` onto the rows current at `now_unix_secs`.
pub(super) fn project(snapshot: &WireCheckSnapshot, now_unix_secs: u64) -> WireCheckRows {
    let window = snapshot.window_secs;
    let loss = snapshot
        .loss
        .iter()
        .filter(|c| current(&c.reading, window, now_unix_secs))
        .map(|c| LossRow {
            lane: c.lane.label(),
            path_class: c.path_class.label(),
            count: c.reading.count,
            last_seen_unix_secs: c.reading.last_seen_unix_secs,
        })
        .collect();
    let checks = snapshot
        .checks
        .iter()
        .filter(|c| current(&c.reading, window, now_unix_secs))
        .map(|c| CheckRow {
            lane: c.lane.label(),
            outcome: c.outcome.label(),
            count: c.reading.count,
            last_seen_unix_secs: c.reading.last_seen_unix_secs,
        })
        .collect();
    let in_window = || {
        snapshot
            .findings
            .iter()
            .filter(move |f| now_unix_secs.saturating_sub(f.ts_unix_secs) <= window)
    };
    let loss_findings = in_window().filter_map(loss_finding_row).collect();
    let unrepaired_rejections = in_window().filter_map(unrepaired_row).collect();
    let local_refusals = snapshot
        .local_refusals
        .iter()
        .filter(|c| current(&c.reading, window, now_unix_secs))
        .map(|c| LocalRefusalRow {
            lane: c.lane.label(),
            count: c.reading.count,
            last_seen_unix_secs: c.reading.last_seen_unix_secs,
        })
        .collect();
    WireCheckRows {
        window_secs: window,
        loss,
        loss_findings,
        checks,
        unrepaired_rejections,
        local_refusals,
    }
}

#[cfg(test)]
#[path = "wire_check_rows_tests.rs"]
mod tests;
