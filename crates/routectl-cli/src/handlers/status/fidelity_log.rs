//! Rendering for the INFO fidelity snapshot: the per-verdict and per-budget rows,
//! flattened to tokens, counts, and booleans.
//!
//! # Why rendering is its own module, separate from the emitter
//!
//! The emitter ([`super::field_verdict_log`]) is one `tracing::info!` call and the
//! field-name-to-source mapping it pins. This is the projection under it, and the
//! projection is where the JUDGEMENT is: several DTO fields are `Option`s whose
//! absent case is a real operator-facing state rather than missing data, and each
//! needs its own explicit rendering. Keeping the two apart lets the mapping be
//! guarded by source text (a swap between two field names is invisible in the
//! emitted values) while the renderings are asserted behaviorally.
//!
//! # Why every absent case gets a literal
//!
//! On a status surface an absent field reads as an unavailable panel. Three of the
//! absences here mean something quite specific and quite different from "no data":
//! an unblocked verdict (pre-flight IS rewriting traffic), a canary that has never
//! settled (as opposed to one that settled inconclusively), and an unreadable
//! budget (as opposed to a budget of zero). Each is spelled.
//!
//! # Content discipline
//!
//! Every rendered field is a count, a boolean, a closed-set token, a
//! code-authored path literal, a `sanitize_for_log`-sanitized state key, or an
//! operator-authored config key (a provider name). Deliberately NO request value,
//! response body, credential, session key, or upstream text at any verbosity.

use routectl_router::FieldVerdictStatus;

use super::paid_probe_budget::PaidProbeBudget;

/// Rendered class for a verdict whose path this build's closed table does not
/// carry -- what a verdict persisted by a build with a wider table looks like.
///
/// A literal rather than an absent value: "this build has no row for this key" is
/// a real answer, and it is the answer that explains why the verdict is inert.
pub(super) const TRANSFORM_CLASS_UNKNOWN: &str = "unknown";

/// Rendered required-quorum for the same case. The CLASS is what carries a
/// quorum, so a verdict with no class has none to report -- and a zero here would
/// read as "no confirmations needed", which is the opposite of the truth.
pub(super) const REQUIRED_QUORUM_UNKNOWN: &str = "unknown";

/// Rendered used-count for a budget whose accounting could not be read.
///
/// A literal rather than a zero, and the distinction is the whole point: zero
/// claims nothing was spent, which a failed or corrupt read cannot substantiate.
pub(super) const COMMITTED_UNKNOWN: &str = "unknown";

/// Ceiling on rendered rows per row-carrying field on the INFO line.
///
/// A log line is a BOUNDED surface. The verdict count grows with the number of
/// (target, field) identities a deployment has learned, which is operator- and
/// traffic-driven rather than fixed, so an uncapped render lets one line grow
/// without limit -- and a status poll emits it every few seconds. What an operator
/// loses to the cap is bounded and stated: the TOTAL count is emitted as its own
/// field and so is the OMITTED count, so a truncated line says exactly how much it
/// is not showing rather than silently presenting a partial set as complete.
///
/// ONE constant across both row kinds deliberately. Two would be two numbers to
/// reason about for one property (how long can this line get), and a per-kind
/// ceiling buys nothing: the kinds are read together.
///
/// A CODE constant, never a config knob: a bound an operator -- or an agent --
/// can raise mid-session with no diff is an unlogged exemption, not an option.
pub(super) const MAX_RENDERED_ROWS: usize = 32;

/// A bounded render: the rows that fit, how many there were, and how many were
/// dropped.
///
/// The three travel TOGETHER as one value rather than as three emitted fields the
/// caller assembles, because that is what makes a truncated render
/// self-describing. A caller that emitted the rows and forgot the counts would
/// present a partial set as a complete one, and nothing in the line would say so.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Bounded<T> {
    pub(super) rows: Vec<T>,
    /// Rows the surface HAD, before the cap.
    pub(super) total: usize,
    /// Rows the cap dropped. Zero whenever `total` is within the cap, which is
    /// the ordinary case, so a nonzero value is itself the signal that a reader
    /// is looking at a subset.
    pub(super) omitted: usize,
}

/// Truncate `rows` to [`MAX_RENDERED_ROWS`], recording what was dropped.
///
/// The FIRST N rather than a sample: an operator reading a truncated line needs a
/// stable prefix they can correlate across polls, and a sample would present a
/// different subset each time while the omitted count stayed the same.
fn bounded<T>(mut rows: Vec<T>) -> Bounded<T> {
    let total = rows.len();
    rows.truncate(MAX_RENDERED_ROWS);
    Bounded {
        omitted: total - rows.len(),
        rows,
        total,
    }
}

/// One rendered verdict row: every field resolved to a token, count, or boolean.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RenderedVerdict {
    pub(super) state_key: String,
    pub(super) capability_key: String,
    pub(super) transform_class: &'static str,
    pub(super) prefix_impacting: bool,
    pub(super) source: &'static str,
    pub(super) phase: &'static str,
    pub(super) confirmations: u32,
    pub(super) required_quorum: String,
    pub(super) blocked_reason: &'static str,
    pub(super) canary: &'static str,
    pub(super) canary_remaining_requests: u32,
    pub(super) canary_last_outcome: &'static str,
    pub(super) requests_in_flight: u64,
    pub(super) unconfirmed_requests: u64,
}

/// Render the verdict rows, in the order the router reported them, BOUNDED.
///
/// See [`bounded`] for why the render is capped and why the counts ride along.
pub(super) fn rendered_verdicts(rows: &[FieldVerdictStatus]) -> Bounded<RenderedVerdict> {
    bounded(
        rows.iter()
            .map(|row| RenderedVerdict {
                state_key: row.state_key.clone(),
                capability_key: row.capability_key.clone(),
                transform_class: row.transform_class.unwrap_or(TRANSFORM_CLASS_UNKNOWN),
                prefix_impacting: row.prefix_impacting,
                source: row.source.as_str(),
                phase: row.phase.as_str(),
                confirmations: row.confirmations,
                // Rendered through the DTO's own accessors wherever one exists, so the
                // absent case is spelled by the type that owns its meaning rather than
                // re-decided here.
                required_quorum: row
                    .required_quorum
                    .map_or_else(|| REQUIRED_QUORUM_UNKNOWN.to_string(), |q| q.to_string()),
                blocked_reason: row.blocked_reason_token(),
                canary: row.canary.as_str(),
                canary_remaining_requests: row.canary_remaining_requests,
                canary_last_outcome: row.canary_outcome_token(),
                requests_in_flight: row.requests_in_flight,
                unconfirmed_requests: row.unconfirmed_requests,
            })
            .collect(),
    )
}

/// One rendered paid-probe budget row.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RenderedBudget {
    pub(super) provider: String,
    pub(super) daily_cap: u32,
    pub(super) committed_today: String,
    pub(super) accounting: &'static str,
}

/// Render the budget rows, in the order the accounting read reported them,
/// BOUNDED under the same ceiling as the verdict rows.
pub(super) fn rendered_budgets(budgets: &[PaidProbeBudget]) -> Bounded<RenderedBudget> {
    bounded(
        budgets
            .iter()
            .map(|budget| RenderedBudget {
                provider: budget.provider.clone(),
                daily_cap: budget.daily_cap,
                committed_today: budget
                    .committed_today
                    .map_or_else(|| COMMITTED_UNKNOWN.to_string(), |used| used.to_string()),
                accounting: budget.accounting.as_str(),
            })
            .collect(),
    )
}

#[cfg(test)]
#[path = "fidelity_log_tests.rs"]
mod tests;
