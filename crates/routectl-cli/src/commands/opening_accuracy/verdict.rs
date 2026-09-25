//! The report's verdict: the exact numerical rate over the anchored cohort,
//! then window completeness, data quality and backend provenance, folded in
//! that order into one typed result and a process exit status.

use super::OpeningAccuracyReport;

/// Exit status when the report cannot run at all (no window, unreadable
/// ledger).
pub const ERROR_EXIT_CODE: i32 = 5;

/// The gate: at least this percent of the anchored cohort must pass.
pub const GATE_PCT: u64 = 95;

/// The numerical rate over EXACTLY the anchored cohort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numerical {
    Pass {
        pass: u64,
        eligible: u64,
    },
    Fail {
        pass: u64,
        eligible: u64,
    },
    /// No anchored row to measure: never a pass.
    Insufficient,
}

/// The report's single verdict. Only [`Verdict::Pass`] exits 0, and it
/// means a complete, clean numerical pass on direct evidence -- never an
/// authorization to deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// The numerical rate passed, but some anchored row's terminal is not
    /// established as the vendor's own; pending the operator's source check.
    BackendPending,
    Fail,
    /// No anchored rows, or legacy rows make the window incomplete.
    Insufficient,
    /// Unclassifiable rows or field defects: the rate cannot be trusted as
    /// complete.
    Indeterminate,
    /// No readable ledger at the path.
    NoLedger,
}

impl Verdict {
    /// Stable token printed on the verdict line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::BackendPending => "BACKEND_PENDING",
            Self::Fail => "FAIL",
            Self::Insufficient => "INSUFFICIENT",
            Self::Indeterminate => "INDETERMINATE",
            Self::NoLedger => "NO_LEDGER",
        }
    }

    /// Process exit status: 0 only for [`Verdict::Pass`].
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Insufficient | Self::NoLedger => 2,
            Self::Indeterminate => 3,
            Self::BackendPending => 4,
        }
    }
}

impl OpeningAccuracyReport {
    /// The integer-safe rate: `pass * 100 >= 95 * eligible`.
    pub fn numerical(&self) -> Numerical {
        let (pass, eligible) = (self.anchored.pass, self.anchored.eligible);
        if eligible == 0 {
            Numerical::Insufficient
        } else if u128::from(pass) * 100 >= u128::from(GATE_PCT) * u128::from(eligible) {
            Numerical::Pass { pass, eligible }
        } else {
            Numerical::Fail { pass, eligible }
        }
    }

    /// Whether unclassifiable rows or field defects veto a release result.
    pub fn data_quality_veto(&self) -> bool {
        self.unclassifiable_total() > 0 || self.defect_rows > 0
    }

    /// The folded verdict.
    pub fn verdict(&self) -> Verdict {
        match self.numerical() {
            Numerical::Insufficient => Verdict::Insufficient,
            Numerical::Fail { .. } => Verdict::Fail,
            Numerical::Pass { .. } if self.legacy > 0 => Verdict::Insufficient,
            Numerical::Pass { .. } if self.data_quality_veto() => Verdict::Indeterminate,
            Numerical::Pass { .. } if self.anchored.unverified_reports > 0 => {
                Verdict::BackendPending
            }
            Numerical::Pass { .. } => Verdict::Pass,
        }
    }
}
