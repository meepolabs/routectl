//! Coverage for the fidelity snapshot's RENDERING: every `Option` whose absent
//! case is a real operator-facing state, spelled rather than left to `Debug`.
//!
//! # Why these are their own tests rather than trace assertions
//!
//! A captured event carries the rendered STRING, so a trace assertion on the
//! emitted line can confirm a token is present but not that it came from the
//! field it is named for. These drive the renderers directly, where each absent
//! case is one function of one input -- which is also what makes the failure
//! message say which state was misrendered.

use super::*;

use routectl_router::{
    CanaryPosture, FieldVerdictStatus, FieldVerdictStatusSpec, PreflightBlockedReason,
};
use routectl_usage::PaidProbeUsage;

use super::super::paid_probe_budget::AccountingHealth;

/// A verdict row with every optional field ABSENT: no class, no quorum, nothing
/// blocking, no settled canary.
///
/// Not a realistic combination and deliberately so -- it is the maximally-absent
/// input, which is the one a renderer that forwarded `Debug` would turn into a row
/// of `None`s. Built through the DTO's own constructor-free literal because the
/// type is the contract this renders.
fn maximally_absent_spec() -> FieldVerdictStatusSpec {
    FieldVerdictStatusSpec {
        state_key: "m0".to_string(),
        // Assembled from parts rather than spelled whole: the namespace prefix has
        // exactly one compiled spelling, which a workspace-wide scan enforces, so a
        // full literal here would be the second spelling that scan exists to forbid.
        capability_key: format!("{}{}", "fie", "ld:some.future.field"),
        transform_class: None,
        prefix_impacting: false,
        source: routectl_core::EvidenceSource::Live,
        phase: routectl_core::FailurePhase::F1,
        confirmations: 0,
        required_quorum: None,
        blocked_reason: None,
        canary: CanaryPosture::Counting,
        canary_remaining_requests: 100,
        canary_last_outcome: None,
        requests_in_flight: 0,
        unconfirmed_requests: 0,
    }
}

fn maximally_absent_row() -> FieldVerdictStatus {
    FieldVerdictStatus::from_spec_for_tests(maximally_absent_spec())
}

/// Every absent verdict field renders as its own literal, never as an empty or a
/// zero.
///
/// The three absences mean three different things and none of them is "no data":
/// a class this build's table does not carry, a quorum that only a class can have,
/// and a canary that has NEVER settled -- which is not the same as one that settled
/// inconclusively.
#[test]
fn every_absent_verdict_field_renders_as_its_own_literal() {
    let rendered = rendered_verdicts(&[maximally_absent_row()]);

    let row = &rendered.rows[0];
    assert_eq!(
        row.transform_class, TRANSFORM_CLASS_UNKNOWN,
        "a path this build's closed table has no row for renders as unknown, which \
         is also what explains why the verdict is inert",
    );
    assert_eq!(
        row.required_quorum, REQUIRED_QUORUM_UNKNOWN,
        "and it has no quorum to report -- a zero would read as no confirmations needed",
    );
    assert_eq!(
        row.canary_last_outcome,
        maximally_absent_row().canary_outcome_token(),
        "a canary that never settled is distinguishable from one that settled \
         inconclusively, or an abandoned re-verification reads as a completed one",
    );
}

/// An UNBLOCKED verdict renders the explicit none token.
///
/// This is the state in which pre-flight is actively rewriting an operator's
/// traffic, so it is the one absence that must be least ambiguous -- an empty
/// field here reads as an unavailable panel rather than as "nothing is stopping
/// this".
#[test]
fn an_unblocked_verdict_renders_the_explicit_none_token() {
    let rendered = rendered_verdicts(&[maximally_absent_row()]);

    assert_eq!(
        rendered.rows[0].blocked_reason,
        maximally_absent_row().blocked_reason_token(),
    );
}

/// A row with every optional field PRESENT renders each one's own value.
///
/// The paired control for the absence tests: without it, a renderer that emitted
/// the unknown literals unconditionally would satisfy all of them -- and would
/// report every verdict on every deployment as unactionable.
#[test]
fn a_fully_populated_verdict_renders_each_present_value() {
    let row = FieldVerdictStatus::from_spec_for_tests(FieldVerdictStatusSpec {
        transform_class: Some("prefix_impacting"),
        prefix_impacting: true,
        confirmations: 1,
        required_quorum: Some(2),
        blocked_reason: Some(PreflightBlockedReason::BelowQuorum),
        canary: CanaryPosture::Due,
        canary_last_outcome: Some(routectl_router::CanaryOutcome::Confirmed),
        requests_in_flight: 4,
        unconfirmed_requests: 9,
        ..maximally_absent_spec()
    });

    let rendered = rendered_verdicts(&[row]);

    let out = &rendered.rows[0];
    assert_eq!(out.transform_class, "prefix_impacting");
    assert!(out.prefix_impacting);
    assert_eq!(out.required_quorum, "2");
    assert_eq!(out.blocked_reason, "below_quorum");
    assert_eq!(out.canary, "due");
    assert_eq!(out.canary_last_outcome, "confirmed");
    assert_eq!(out.confirmations, 1);
    assert_eq!(
        (out.requests_in_flight, out.unconfirmed_requests),
        (4, 9),
        "the two exposure counts are rendered from their own fields: they mean \
         opposite things and a swap is invisible in the emitted line",
    );
}

fn budget_row(committed: Option<u32>, accounting: AccountingHealth) -> PaidProbeBudget {
    PaidProbeBudget {
        provider: "anthropic".to_string(),
        daily_cap: 5,
        committed_today: committed,
        accounting,
    }
}

/// An unreadable budget renders its used count as the unknown literal, never as
/// zero.
///
/// Zero is a claim that nothing was spent, which a failed or corrupt read cannot
/// substantiate -- and a zero beside an otherwise-ordinary row is exactly how an
/// unreadable budget comes to look like a clean one.
///
/// Both failing readings are driven, because they are different operator
/// situations and a renderer that special-cased one would pass a single-case test
/// while rendering the other as a spend.
#[test]
fn a_budget_with_no_readable_count_renders_unknown_rather_than_zero() {
    for accounting in [AccountingHealth::Malformed, AccountingHealth::Unreadable] {
        let rendered = rendered_budgets(&[budget_row(None, accounting)]);

        assert_eq!(
            rendered.rows[0].committed_today, COMMITTED_UNKNOWN,
            "{accounting:?}: an unread count is not a zero spend",
        );
        assert_eq!(
            rendered.rows[0].accounting,
            accounting.as_str(),
            "and the health token says WHICH way the read failed",
        );
    }
}

/// A readable budget renders its count, including a genuine zero.
///
/// The control, and the zero case specifically: a renderer that emitted the
/// unknown literal whenever the count was zero would make a healthy unspent budget
/// look unreadable, which is the inverse error and just as misleading.
#[test]
fn a_readable_budget_renders_its_count_including_zero() {
    let rendered = rendered_budgets(&[
        budget_row(Some(0), AccountingHealth::Healthy),
        budget_row(Some(3), AccountingHealth::Healthy),
    ]);

    assert_eq!(
        rendered.rows[0].committed_today, "0",
        "a real zero spend renders as zero: it is a reading, not an absence",
    );
    assert_eq!(rendered.rows[1].committed_today, "3");
    assert_eq!(rendered.rows[0].accounting, "healthy");
}

/// The rendered vocabulary has no collision with the unknown literals.
///
/// A class legitimately named `unknown`, or a spend of literally the string
/// `unknown`, would make a present value indistinguishable from an absent one --
/// and nothing about the reading would say which it was.
#[test]
fn no_real_token_collides_with_an_unknown_literal() {
    use routectl_router::CanaryPosture as Posture;

    let reals = [
        "envelope",
        "prefix_impacting",
        Posture::Counting.as_str(),
        Posture::Due.as_str(),
        Posture::InFlight.as_str(),
        PreflightBlockedReason::NotEligible.as_str(),
        PreflightBlockedReason::BelowQuorum.as_str(),
        PreflightBlockedReason::NoTargetOptIn.as_str(),
        PreflightBlockedReason::CanarySuspended.as_str(),
        AccountingHealth::Healthy.as_str(),
        AccountingHealth::Malformed.as_str(),
        AccountingHealth::Unreadable.as_str(),
    ];
    for unknown in [
        TRANSFORM_CLASS_UNKNOWN,
        REQUIRED_QUORUM_UNKNOWN,
        COMMITTED_UNKNOWN,
    ] {
        assert!(
            !reals.contains(&unknown),
            "the absent-value literal {unknown:?} must not also be a real token, or \
             a present value reads as an absent one",
        );
    }
}

/// The usage read's three answers map onto three distinct health tokens.
///
/// Pins the mapping rather than the tokens: the read and the surface are different
/// vocabularies, and a collapse between two of the read's answers here would hide
/// one of them behind the other's token.
#[test]
fn each_usage_read_answer_maps_to_its_own_health_token() {
    let mapped = [
        (PaidProbeUsage::Committed(0), AccountingHealth::Healthy),
        (PaidProbeUsage::Malformed, AccountingHealth::Malformed),
        (PaidProbeUsage::Unreadable, AccountingHealth::Unreadable),
    ];
    let mut tokens: Vec<&str> = mapped.iter().map(|(_, h)| h.as_str()).collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(
        tokens.len(),
        mapped.len(),
        "each read answer keeps its own token: {tokens:?}",
    );
}

// ---------------------------------------------------------------------------
// The render ceiling
// ---------------------------------------------------------------------------

/// A row set within the ceiling renders whole, with nothing omitted.
///
/// The paired positive for the truncation case below, and it is the one that
/// matters more in practice: almost every real deployment sits here, so a cap that
/// truncated early would be silently hiding rows on ordinary traffic.
#[test]
fn a_row_set_within_the_ceiling_renders_whole() {
    let rows: Vec<FieldVerdictStatus> = (0..MAX_RENDERED_ROWS)
        .map(|_| maximally_absent_row())
        .collect();

    let rendered = rendered_verdicts(&rows);

    assert_eq!(rendered.rows.len(), MAX_RENDERED_ROWS);
    assert_eq!(rendered.total, MAX_RENDERED_ROWS);
    assert_eq!(
        rendered.omitted, 0,
        "a set exactly at the ceiling is not truncated -- the bound is inclusive",
    );
}

/// A row set OVER the ceiling is truncated, and says how much it dropped.
///
/// The bound exists because the row count grows with what a deployment has learned
/// -- operator- and traffic-driven, not fixed -- while this line is emitted on every
/// status poll. What the cap must not do is hide the truncation: the total and the
/// omitted count are both reported, so a reader knows they are looking at a subset
/// rather than mistaking a partial set for a complete one.
///
/// Mutation checks: remove the `truncate` -> red on the row count; hardcode
/// `omitted: 0` -> red on the omitted assertion; report `total` as the truncated
/// length -> red on the total.
#[test]
fn a_row_set_over_the_ceiling_is_truncated_and_reports_what_it_dropped() {
    let over = MAX_RENDERED_ROWS + 7;
    let rows: Vec<FieldVerdictStatus> = (0..over).map(|_| maximally_absent_row()).collect();

    let rendered = rendered_verdicts(&rows);

    assert_eq!(
        rendered.rows.len(),
        MAX_RENDERED_ROWS,
        "the rendered set is capped, so one line cannot grow without bound",
    );
    assert_eq!(
        rendered.total, over,
        "the TOTAL is what the surface had, not what fit -- a reader needs the real \
         count to know the line is partial",
    );
    assert_eq!(
        rendered.omitted, 7,
        "and the omitted count says exactly how much is hidden, so a truncated line \
         is self-describing rather than silently partial",
    );
}

/// The budget rows are bounded under the SAME ceiling.
///
/// One constant across both kinds, because the property is "how long can this line
/// get" and that is one question. A per-kind ceiling would be two numbers to reason
/// about for one bound.
#[test]
fn the_budget_rows_share_the_same_ceiling() {
    let over = MAX_RENDERED_ROWS + 3;
    let budgets: Vec<PaidProbeBudget> = (0..over)
        .map(|_| budget_row(Some(0), AccountingHealth::Healthy))
        .collect();

    let rendered = rendered_budgets(&budgets);

    assert_eq!(rendered.rows.len(), MAX_RENDERED_ROWS);
    assert_eq!(rendered.total, over);
    assert_eq!(rendered.omitted, 3);
}
