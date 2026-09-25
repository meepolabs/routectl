//! ASCII rendering of the opening-accuracy report. Every value printed is a
//! count, a closed-set label, or a sanitized lane key, so no database value
//! can add a line; the verdict line is printed once, last before the notice.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::verdict::GATE_PCT;
use super::{Buckets, Numerical, OpeningAccuracyReport, Verdict};

/// Rendered with every verdict: what the report cannot establish.
pub const PROVENANCE_NOTICE: &str = "backend provenance: NOT established by this report. \
A terminal relayed by an Anthropic-compatible endpoint (a back hop included) is only what \
that endpoint reported; the release decision needs the operator's two-hop check. \
Exit status 0 is not an authorization to deploy.";

/// Prefix of the one verdict line.
pub const VERDICT_PREFIX: &str = "verdict: ";

/// The report for a window with no readable ledger.
pub(super) fn no_ledger(title: &str, why: &str) -> String {
    format!(
        "{}\n{}\n{VERDICT_PREFIX}{} (no ledger to measure)\n{PROVENANCE_NOTICE}\n",
        single_line(title),
        single_line(why),
        Verdict::NoLedger.as_str(),
    )
}

/// Render `report` under `title`.
pub fn render_opening_accuracy(title: &str, report: &OpeningAccuracyReport) -> String {
    let mut out = format!("{}\n", single_line(title));
    render_universe(&mut out, report);
    render_cohort(&mut out, report);
    out.push_str("\nerror |opening - terminal| / terminal, by opening source:\n");
    out.push_str(&bucket_table(&report.buckets_by_source));
    render_cold_start(&mut out, report);
    render_lanes(&mut out, report);
    out.push('\n');
    render_verdict(&mut out, report);
    out
}

/// `text` with every non-printable-ASCII character replaced by `?`.
fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn render_universe(out: &mut String, r: &OpeningAccuracyReport) {
    let _ = writeln!(
        out,
        "universe: {} anthropic streaming rows in the window",
        r.universe
    );
    let _ = writeln!(
        out,
        "reconciled: {} legacy + {} unclassifiable + {} no opening + {} known = {}",
        r.legacy,
        r.unclassifiable_total(),
        r.no_opening,
        r.known_total(),
        r.classified(),
    );
    if !r.unclassifiable.is_empty() {
        let _ = writeln!(out, "  unclassifiable: {}", pairs(&r.unclassifiable));
    }
    if !r.known_by_source.is_empty() {
        let _ = writeln!(out, "  known by source: {}", pairs(&r.known_by_source));
    }
    if !r.defects.is_empty() {
        let _ = writeln!(
            out,
            "  data defects on {} known rows: {}",
            r.defect_rows,
            pairs(&r.defects)
        );
    }
    let _ = writeln!(
        out,
        "terminal reports not vendor-verified (all outcomes): {}",
        r.unverified_reports
    );
}

fn render_cohort(out: &mut String, r: &OpeningAccuracyReport) {
    let c = &r.anchored;
    let _ = writeln!(
        out,
        "anchored cohort: {} eligible, {} pass ({} provisional, {} lane switched)",
        c.eligible, c.pass, c.provisional, c.lane_switched,
    );
    if !c.non_pass.is_empty() {
        let _ = writeln!(out, "  non-pass: {}", pairs(&c.non_pass));
    }
    let _ = writeln!(
        out,
        "  not vendor-verified: {} of the cohort, {} of its passes",
        c.unverified_reports, c.pass_unverified,
    );
    let rate = match r.numerical() {
        Numerical::Insufficient => "rate: none (no anchored rows)".to_string(),
        Numerical::Pass { pass, eligible } | Numerical::Fail { pass, eligible } => format!(
            "rate: {pass} / {eligible} = {} (need >= {GATE_PCT}%)  numerical {}",
            floor_pct(pass, eligible),
            if matches!(r.numerical(), Numerical::Pass { .. }) {
                "PASS"
            } else {
                "FAIL"
            },
        ),
    };
    let _ = writeln!(out, "  {rate}");
}

fn render_cold_start(out: &mut String, r: &OpeningAccuracyReport) {
    out.push_str("\ncold start (no anchor record for the session), by opening source:\n");
    if r.cold_start.is_empty() {
        out.push_str("  none\n");
    } else {
        out.push_str(&bucket_table(&r.cold_start));
    }
}

fn render_verdict(out: &mut String, r: &OpeningAccuracyReport) {
    let verdict = r.verdict();
    let why = match verdict {
        Verdict::Pass => "complete, clean numerical pass on direct vendor evidence",
        Verdict::BackendPending => {
            "numerical pass; anchored terminals not vendor-verified await the two-hop check"
        }
        Verdict::Fail => "numerical rate below the gate",
        Verdict::Insufficient if r.anchored.eligible == 0 => "no anchored rows to measure",
        Verdict::Insufficient => "legacy rows in the window; select a post-deploy window",
        Verdict::Indeterminate => "unclassifiable rows or data defects veto the result",
        Verdict::NoLedger => "no ledger to measure",
    };
    let _ = writeln!(out, "{VERDICT_PREFIX}{} ({why})", verdict.as_str());
    let _ = writeln!(out, "{PROVENANCE_NOTICE}");
}

fn render_lanes(out: &mut String, r: &OpeningAccuracyReport) {
    out.push_str("\nby served lane (kind:provider/model@upstream):\n");
    if r.lanes.is_empty() {
        out.push_str("  none\n");
        return;
    }
    let header = [
        "lane",
        "rows",
        "legacy",
        "unclass",
        "no-open",
        "anchored",
        "pass",
        "unverified",
        "defects",
    ];
    let mut rows = vec![header.map(str::to_string).to_vec()];
    for (lane, s) in &r.lanes {
        rows.push(vec![
            lane.clone(),
            s.rows.to_string(),
            s.legacy.to_string(),
            s.unclassifiable.to_string(),
            s.no_opening.to_string(),
            s.anchored.to_string(),
            s.pass.to_string(),
            s.unverified_reports.to_string(),
            s.defect_rows.to_string(),
        ]);
    }
    out.push_str(&table(&rows));
    for (lane, s) in r.lanes.iter().filter(|(_, s)| !s.sources.is_empty()) {
        let _ = writeln!(out, "  {lane}");
        let _ = writeln!(out, "    source: {}", pairs(&s.sources));
        let _ = writeln!(out, "    reason: {}", pairs(&s.reasons));
        let _ = writeln!(out, "    terminal_source: {}", pairs(&s.terminals));
        for (source, b) in &s.buckets {
            let _ = writeln!(out, "    {source}: {}", bucket_cells(b));
        }
    }
}

fn bucket_cells(b: &Buckets) -> String {
    format!(
        "0-5%={} 5-10%={} 10-20%={} >20%={} n/a={}",
        b.within_5, b.within_10, b.within_20, b.over_20, b.not_comparable
    )
}

fn bucket_table(buckets: &BTreeMap<&'static str, Buckets>) -> String {
    let mut rows = vec![
        ["source", "0-5%", "5-10%", "10-20%", ">20%", "n/a"]
            .map(str::to_string)
            .to_vec(),
    ];
    for (source, b) in buckets {
        rows.push(vec![
            (*source).to_string(),
            b.within_5.to_string(),
            b.within_10.to_string(),
            b.within_20.to_string(),
            b.over_20.to_string(),
            b.not_comparable.to_string(),
        ]);
    }
    table(&rows)
}

/// `key=count` pairs in key order.
fn pairs(map: &BTreeMap<&'static str, u64>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `num / den` as a percentage truncated to one decimal, so a displayed
/// rate never rounds up across the gate.
fn floor_pct(num: u64, den: u64) -> String {
    let permille = u128::from(num) * 1000 / u128::from(den.max(1));
    format!("{}.{}%", permille / 10, permille % 10)
}

/// Left-align the first column, right-align the rest, two-space gutters.
fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(String::len)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                if i == 0 {
                    format!("  {cell:<w$}", w = widths[i])
                } else {
                    format!("{cell:>w$}", w = widths[i])
                }
            })
            .collect();
        let _ = writeln!(out, "{}", cells.join("  ").trim_end());
    }
    out
}
