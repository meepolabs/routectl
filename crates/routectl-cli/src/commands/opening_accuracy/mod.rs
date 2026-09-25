//! `routectl usage --opening-accuracy` -- the read-only release-gate report
//! for the Anthropic context meter's anchored opening count.
//!
//! The universe is every Anthropic-ingress streaming row in an explicit
//! window, and each row lands in exactly one category: legacy (no opening
//! diagnostics key at all), unclassifiable (by a fixed kind), no opening, or
//! known (by closed-set `opening_source`). The anchored cohort is EXACTLY the
//! known rows whose source is `anchor` -- a choice fixed before the terminal
//! usage arrived -- and no outcome removes a row from it: an inconsistent
//! opening marker, a failed turn, or a missing, zero, malformed or
//! non-evidence terminal is a non-pass with one named reason.
//!
//! The report states independent facts and folds them into one verdict
//! ([`verdict::Verdict`]): the exact numerical rate over the anchored cohort;
//! window completeness (legacy rows); data quality (unclassifiable rows and
//! field defects on known rows, which never change the rate); and backend
//! provenance (terminal reports not established as the vendor's own). Only a
//! complete, clean numerical PASS on direct evidence exits 0, and even that
//! authorizes nothing by itself.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Local};
use routectl_usage::{OpenError, StreamExtraRow, UsageDb, for_each_stream_extra, open_readonly};

use crate::commands::usage::{UsageError, WindowBounds, WindowFlag, since_bounds, window_bounds};
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::anthropic::context_anchor::{MissReason, OpeningSource};

use classify::{Class, ErrorPct, KnownRow, PASS_PCT, classify};

pub use render::{PROVENANCE_NOTICE, render_opening_accuracy};
pub use verdict::{ERROR_EXIT_CODE, Numerical, Verdict};

mod classify;
mod lane;
mod render;
mod verdict;

const BUCKET_10_PCT: u128 = 10;
const BUCKET_20_PCT: u128 = 20;

/// Opening-to-terminal error distribution for one group of known rows.
/// Each upper edge is inclusive: an error of exactly 5% is `within_5`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Buckets {
    pub within_5: u64,
    pub within_10: u64,
    pub within_20: u64,
    pub over_20: u64,
    /// No stated opening, or no positive terminal that is input evidence.
    pub not_comparable: u64,
}

impl Buckets {
    const fn add(&mut self, error: Option<ErrorPct>) {
        let slot = match error {
            None => &mut self.not_comparable,
            Some(e) if e.within(PASS_PCT) => &mut self.within_5,
            Some(e) if e.within(BUCKET_10_PCT) => &mut self.within_10,
            Some(e) if e.within(BUCKET_20_PCT) => &mut self.within_20,
            Some(_) => &mut self.over_20,
        };
        *slot += 1;
    }
}

/// The anchored cohort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchoredCohort {
    pub eligible: u64,
    pub pass: u64,
    /// Passes whose terminal was not established as the vendor's own.
    pub pass_unverified: u64,
    /// Cohort rows, pass or not, carrying a terminal report not established
    /// as the vendor's own.
    pub unverified_reports: u64,
    /// Non-passes by their single reason.
    pub non_pass: BTreeMap<&'static str, u64>,
    /// Anchored rows chosen before the serving lane was known.
    pub provisional: u64,
    /// Anchored rows selected against a lane other than the one that served.
    pub lane_switched: u64,
}

impl AnchoredCohort {
    /// Count one anchored row; returns whether it passed.
    fn record(&mut self, row: &KnownRow, outcome: &str) -> bool {
        self.eligible += 1;
        self.provisional += u64::from(row.provisional);
        self.lane_switched += u64::from(row.lane_switched);
        self.unverified_reports += u64::from(row.is_unverified_report());
        match row.non_pass(outcome) {
            Some(reason) => {
                *self.non_pass.entry(reason).or_default() += 1;
                false
            }
            None => {
                self.pass += 1;
                self.pass_unverified += u64::from(!row.vendor_verified);
                true
            }
        }
    }
}

/// One served lane's share of the universe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneStats {
    pub rows: u64,
    pub legacy: u64,
    pub unclassifiable: u64,
    pub no_opening: u64,
    pub anchored: u64,
    pub pass: u64,
    /// Known rows carrying a terminal report not established as the
    /// vendor's own, all outcomes.
    pub unverified_reports: u64,
    /// Known rows with at least one field-level data defect.
    pub defect_rows: u64,
    pub sources: BTreeMap<&'static str, u64>,
    pub reasons: BTreeMap<&'static str, u64>,
    pub terminals: BTreeMap<&'static str, u64>,
    pub buckets: BTreeMap<&'static str, Buckets>,
}

impl LaneStats {
    fn record_known(&mut self, row: &KnownRow, error: Option<ErrorPct>) {
        *self.sources.entry(row.source).or_default() += 1;
        *self.reasons.entry(row.reason).or_default() += 1;
        *self.terminals.entry(row.terminal_display()).or_default() += 1;
        self.buckets.entry(row.source).or_default().add(error);
        self.unverified_reports += u64::from(row.is_unverified_report());
        self.defect_rows += u64::from(!row.defects.is_empty());
    }
}

/// The whole report over one window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpeningAccuracyReport {
    pub universe: u64,
    pub legacy: u64,
    pub unclassifiable: BTreeMap<&'static str, u64>,
    pub no_opening: u64,
    pub known_by_source: BTreeMap<&'static str, u64>,
    /// Field-level defects on known rows, by kind; one row may carry several.
    pub defects: BTreeMap<&'static str, u64>,
    /// Known rows with at least one defect.
    pub defect_rows: u64,
    /// Known rows, all outcomes and sources, carrying a terminal report not
    /// established as the vendor's own.
    pub unverified_reports: u64,
    pub anchored: AnchoredCohort,
    pub buckets_by_source: BTreeMap<&'static str, Buckets>,
    /// Known rows whose anchor tier found no record for the session.
    pub cold_start: BTreeMap<&'static str, Buckets>,
    pub lanes: BTreeMap<String, LaneStats>,
}

impl OpeningAccuracyReport {
    fn unclassifiable_total(&self) -> u64 {
        self.unclassifiable.values().sum()
    }

    fn known_total(&self) -> u64 {
        self.known_by_source.values().sum()
    }

    /// Rows accounted for by some category; equals `universe` by
    /// construction.
    pub fn classified(&self) -> u64 {
        self.legacy + self.unclassifiable_total() + self.no_opening + self.known_total()
    }

    fn add(&mut self, row: &StreamExtraRow) {
        self.universe += 1;
        let lane = self.lanes.entry(lane::lane_label(row)).or_default();
        lane.rows += 1;
        match classify(row.extra.as_deref()) {
            Class::Legacy => {
                self.legacy += 1;
                lane.legacy += 1;
            }
            Class::Unclassifiable(kind) => {
                *self.unclassifiable.entry(kind).or_default() += 1;
                lane.unclassifiable += 1;
            }
            Class::NoOpening => {
                self.no_opening += 1;
                lane.no_opening += 1;
            }
            Class::Known(known) => {
                let error = known.error();
                *self.known_by_source.entry(known.source).or_default() += 1;
                self.buckets_by_source
                    .entry(known.source)
                    .or_default()
                    .add(error);
                if known.reason == MissReason::Cold.as_str() {
                    self.cold_start.entry(known.source).or_default().add(error);
                }
                for defect in &known.defects {
                    *self.defects.entry(defect).or_default() += 1;
                }
                self.defect_rows += u64::from(!known.defects.is_empty());
                self.unverified_reports += u64::from(known.is_unverified_report());
                lane.record_known(&known, error);
                if known.source == OpeningSource::Anchor.as_str() {
                    let passed = self.anchored.record(&known, &row.outcome);
                    lane.anchored += 1;
                    lane.pass += u64::from(passed);
                }
            }
        }
    }
}

// --- entry points -------------------------------------------------------

/// Build the report over `bounds` from an open ledger.
pub fn build_report(
    db: &UsageDb,
    bounds: WindowBounds,
) -> Result<OpeningAccuracyReport, UsageError> {
    let mut report = OpeningAccuracyReport::default();
    for_each_stream_extra(
        db,
        AnthropicIngress.id(),
        bounds.from_ms,
        bounds.to_ms,
        |row| report.add(&row),
    )?;
    Ok(report)
}

/// The window and title for the report. An explicit window is required: the
/// multi-window default of `routectl usage` has no single gate to state.
pub fn opening_accuracy_bounds(
    flag: WindowFlag,
    since: Option<&str>,
    until: Option<&str>,
    now: DateTime<Local>,
) -> Result<(WindowBounds, String), UsageError> {
    if let Some(since) = since {
        let bounds = since_bounds(since, until, now)?;
        let span = until.map_or_else(
            || format!("since {since}"),
            |until| format!("{since} .. {until}"),
        );
        return Ok((bounds, format!("== opening accuracy: {span} ==")));
    }
    let span = match flag {
        WindowFlag::Today => "today",
        WindowFlag::ThisWeek => "this week",
        WindowFlag::ThisMonth => "this month",
        WindowFlag::All => "all time",
        WindowFlag::None => return Err(UsageError::OpeningAccuracyNeedsWindow),
    };
    Ok((
        window_bounds(flag, now),
        format!("== opening accuracy: {span} =="),
    ))
}

/// Open the ledger at `path` read-only and render the report with its
/// verdict. A missing or not-yet-migrated ledger renders as
/// [`Verdict::NoLedger`]; any other open or read failure is
/// [`UsageError::Unavailable`] naming the path.
pub fn opening_accuracy_for_path(
    path: &Path,
    title: &str,
    bounds: WindowBounds,
) -> Result<(String, Verdict), UsageError> {
    let unavailable = |source: Box<dyn std::error::Error + Send + Sync>| UsageError::Unavailable {
        path: path.display().to_string(),
        source,
    };
    let db = match open_readonly(path) {
        Ok(db) => db,
        Err(OpenError::NoData { .. }) => {
            let why = format!("no usage data yet (nothing recorded at {})", path.display());
            return Ok((render::no_ledger(title, &why), Verdict::NoLedger));
        }
        Err(OpenError::VersionTooOld { found, supported }) => {
            let why = format!(
                "usage db at {} predates this binary (schema {found}, need {supported}); \
                 start the service once to migrate it",
                path.display()
            );
            return Ok((render::no_ledger(title, &why), Verdict::NoLedger));
        }
        Err(e) => return Err(unavailable(Box::new(e))),
    };
    let report = build_report(&db, bounds).map_err(|e| unavailable(Box::new(e)))?;
    let verdict = report.verdict();
    Ok((render_opening_accuracy(title, &report), verdict))
}

/// Run `routectl usage --opening-accuracy` against the ledger at `db_path`:
/// print the report and return its verdict for the process exit status.
pub fn run(
    db_path: &Path,
    flag: WindowFlag,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Verdict, UsageError> {
    let (bounds, title) = opening_accuracy_bounds(flag, since, until, Local::now())?;
    let (out, verdict) = opening_accuracy_for_path(db_path, &title, bounds)?;
    print!("{out}");
    Ok(verdict)
}

#[cfg(test)]
#[path = "test_fixture.rs"]
mod test_fixture;

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "verdict_tests.rs"]
mod verdict_tests;

#[cfg(test)]
#[path = "render_tests.rs"]
mod render_tests;
