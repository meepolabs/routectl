//! Tests for the Codex header reducer.
//!
//! Fixtures place every suffix where the shipped header parser puts it: only
//! `primary-used-percent` and `primary-reset-at` are typed, every other suffix
//! (window lengths, `reset-after-seconds`, the whole `secondary-*` family and
//! each extra limit's family) sits in `extras`. The captured family is the
//! one `quota_headers.rs` pins: a weekly primary at 16%, an unused secondary,
//! and a `bengalfox` extra limit.

use super::*;

use std::time::Instant;

use crate::quota::reading::LengthClass;

const OBSERVED_EPOCH_SECS: u64 = 1_786_000_000;
const FIVE_HOURS_SECS: u64 = 5 * 3_600;
const SEVEN_DAYS_SECS: u64 = 7 * 86_400;

fn observed() -> ObservationStamp {
    ObservationStamp::from_parts(
        SystemTime::UNIX_EPOCH + Duration::from_secs(OBSERVED_EPOCH_SECS),
        Instant::now(),
    )
}

fn at_offset(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(OBSERVED_EPOCH_SECS + secs)
}

fn quota(
    primary_percent: &str,
    primary_reset_at: Option<u64>,
    extras: &[(&str, String)],
) -> CodexQuota {
    let mut quota = CodexQuota::default();
    quota.active_limit = Some("premium".into());
    quota.primary_used_percent = Some(primary_percent.into());
    quota.primary_reset_at = primary_reset_at.map(|secs| secs.to_string());
    quota.extras = extras
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect();
    quota
}

/// The captured family: a weekly primary at 16% running for a day, the
/// secondary declared unused, and a `bengalfox` weekly extra limit unstarted.
fn captured() -> CodexQuota {
    quota(
        "16",
        Some(OBSERVED_EPOCH_SECS + 6 * 86_400),
        &[
            ("plan-type", "prolite".into()),
            ("primary-window-minutes", "10080".into()),
            ("primary-reset-after-seconds", (6 * 86_400).to_string()),
            ("primary-over-secondary-limit-percent", "0".into()),
            ("secondary-used-percent", "0".into()),
            ("secondary-window-minutes", "0".into()),
            ("secondary-reset-after-seconds", "0".into()),
            ("secondary-reset-at", String::new()),
            ("credits-has-credits", "False".into()),
            ("bengalfox-primary-used-percent", "0".into()),
            ("bengalfox-primary-window-minutes", "10080".into()),
            ("bengalfox-primary-reset-after-seconds", "604800".into()),
            (
                "bengalfox-primary-reset-at",
                (OBSERVED_EPOCH_SECS + SEVEN_DAYS_SECS).to_string(),
            ),
            ("bengalfox-limit-name", "GPT-5.3-Codex-Spark".into()),
        ],
    )
}

/// A plan with a five-hour primary at 25% resetting in two hours and a
/// weekly secondary at 40% resetting in three days.
fn five_hour_plan() -> CodexQuota {
    quota(
        "25",
        Some(OBSERVED_EPOCH_SECS + 7_200),
        &[
            ("primary-window-minutes", "300".into()),
            ("primary-reset-after-seconds", "7200".into()),
            ("secondary-used-percent", "40".into()),
            ("secondary-window-minutes", "10080".into()),
            ("secondary-reset-after-seconds", "259200".into()),
            (
                "secondary-reset-at",
                (OBSERVED_EPOCH_SECS + 259_200).to_string(),
            ),
        ],
    )
}

fn windows(reduction: &CodexHeaderReduction) -> &[WindowReading] {
    match reduction
        .observation
        .as_ref()
        .map(AccountObservation::report)
    {
        Some(ObservationReport::Partial(set)) => set.as_slice(),
        other => panic!("expected a partial report, got {other:?}"),
    }
}

/// `(length_secs, scope, used, reset, started)` of one window.
type Seen = (u64, Option<String>, f64, Option<SystemTime>, Started);

fn seen(window: &WindowReading) -> Seen {
    (
        window.key().length_secs(),
        window.key().scope().map(|scope| scope.as_str().to_owned()),
        window.used().fraction(),
        window.reset().map(|reset| reset.at()),
        window.started(),
    )
}

#[test]
fn every_reported_window_is_read_at_its_reported_length() {
    let rows: [(&str, CodexQuota, Vec<Seen>); 2] = [
        (
            "five-hour primary and weekly secondary",
            five_hour_plan(),
            vec![
                (
                    FIVE_HOURS_SECS,
                    None,
                    0.25,
                    Some(at_offset(7_200)),
                    Started::Yes,
                ),
                (
                    SEVEN_DAYS_SECS,
                    None,
                    0.40,
                    Some(at_offset(259_200)),
                    Started::Yes,
                ),
            ],
        ),
        (
            "captured weekly primary, unused secondary, scoped extra limit",
            captured(),
            vec![
                (
                    SEVEN_DAYS_SECS,
                    None,
                    0.16,
                    Some(at_offset(6 * 86_400)),
                    Started::Yes,
                ),
                (
                    SEVEN_DAYS_SECS,
                    Some("codex_bengalfox".into()),
                    0.0,
                    Some(at_offset(SEVEN_DAYS_SECS)),
                    Started::No,
                ),
            ],
        ),
    ];
    for (name, quota, expected) in rows {
        let reduction = reduce_codex_headers(&quota, observed());

        let got: Vec<Seen> = windows(&reduction).iter().map(seen).collect();
        assert_eq!(got, expected, "row: {name}");
        assert!(reduction.rejections.is_empty(), "row: {name}");
        let observation = reduction.observation.expect("an observation");
        assert_eq!(observation.source(), ReadingSource::Response, "row: {name}");
    }
}

#[test]
fn the_class_follows_the_reported_length_not_the_slot() {
    let reduction = reduce_codex_headers(&five_hour_plan(), observed());

    let classes: Vec<LengthClass> = windows(&reduction)
        .iter()
        .map(WindowReading::class)
        .collect();
    assert_eq!(classes, vec![LengthClass::Short, LengthClass::Long]);
}

/// Each row's window is refused alone; the other window in the family
/// survives, so the observation still carries it.
#[test]
fn a_bad_window_is_refused_alone_and_counted() {
    let mut no_length = five_hour_plan();
    no_length
        .extras
        .retain(|(key, _)| key != "primary-window-minutes");
    let mut bad_length = five_hour_plan();
    set(&mut bad_length, "primary-window-minutes", "5h");
    let mut too_long = five_hour_plan();
    set(&mut too_long, "primary-window-minutes", "50000");
    let mut bad_percent = five_hour_plan();
    bad_percent.primary_used_percent = Some("abc".into());
    let mut negative = five_hour_plan();
    negative.primary_used_percent = Some("-5".into());
    let mut millis = five_hour_plan();
    millis.primary_reset_at = Some(((OBSERVED_EPOCH_SECS + 7_200) * 1_000).to_string());
    let mut unstarted_used = five_hour_plan();
    set(&mut unstarted_used, "primary-reset-after-seconds", "18000");
    let rows: [(&str, CodexQuota, RejectionReason); 7] = [
        (
            "no window length",
            no_length,
            RejectionReason::UnknownLength,
        ),
        (
            "unparseable length",
            bad_length,
            RejectionReason::UnknownLength,
        ),
        (
            "length over 31 days",
            too_long,
            RejectionReason::UnknownLength,
        ),
        (
            "unparseable percent",
            bad_percent,
            RejectionReason::InvalidUtilization,
        ),
        (
            "negative percent",
            negative,
            RejectionReason::InvalidUtilization,
        ),
        (
            "millisecond reset",
            millis,
            RejectionReason::ImplausibleReset,
        ),
        (
            "unstarted with usage",
            unstarted_used,
            RejectionReason::InvalidStarted,
        ),
    ];
    for (name, quota, reason) in rows {
        let reduction = reduce_codex_headers(&quota, observed());

        assert_eq!(reduction.rejections, vec![reason], "row: {name}");
        let lengths: Vec<u64> = windows(&reduction)
            .iter()
            .map(|w| w.key().length_secs())
            .collect();
        assert_eq!(lengths, vec![SEVEN_DAYS_SECS], "row: {name}");
    }
}

fn set(quota: &mut CodexQuota, suffix: &str, value: &str) {
    let slot = quota
        .extras
        .iter_mut()
        .find(|(key, _)| key == suffix)
        .expect("the fixture carries the suffix");
    slot.1 = value.to_string();
}

#[test]
fn a_percent_over_one_hundred_saturates_to_exhausted() {
    let mut quota = five_hour_plan();
    quota.primary_used_percent = Some("140".into());

    let reduction = reduce_codex_headers(&quota, observed());

    assert_eq!(windows(&reduction)[0].used().fraction(), 1.0);
}

#[test]
fn reset_after_seconds_stands_in_for_a_missing_reset_at() {
    let mut quota = five_hour_plan();
    quota.primary_reset_at = None;

    let reduction = reduce_codex_headers(&quota, observed());

    assert_eq!(
        windows(&reduction)[0].reset().map(|reset| reset.at()),
        Some(at_offset(7_200))
    );
}

#[test]
fn without_reset_after_seconds_the_started_state_is_unknown() {
    let mut quota = five_hour_plan();
    quota
        .extras
        .retain(|(key, _)| key != "primary-reset-after-seconds");

    let reduction = reduce_codex_headers(&quota, observed());

    assert_eq!(windows(&reduction)[0].started(), Started::Unknown);
}

#[test]
fn a_family_reporting_no_usable_window_yields_no_observation() {
    let mut no_reset = quota("16", None, &[("primary-window-minutes", "10080".into())]);
    no_reset.active_limit = None;
    let rows: [(&str, CodexQuota); 3] = [
        ("no used percent at all", CodexQuota::default()),
        ("a window with no reset", no_reset),
        (
            "only an unused secondary",
            quota(
                "",
                None,
                &[
                    ("secondary-used-percent", "0".into()),
                    ("secondary-window-minutes", "0".into()),
                    ("secondary-reset-at", String::new()),
                ],
            ),
        ),
    ];
    for (name, quota) in rows {
        let reduction = reduce_codex_headers(&quota, observed());

        assert!(reduction.observation.is_none(), "row: {name}");
        assert!(reduction.rejections.is_empty(), "row: {name}");
    }
}

#[test]
fn an_extra_limit_with_a_non_canonical_name_is_refused() {
    let mut quota = captured();
    for (key, _) in &mut quota.extras {
        if let Some(rest) = key.strip_prefix("bengalfox-") {
            *key = format!("{}-{rest}", "x".repeat(40));
        }
    }

    let reduction = reduce_codex_headers(&quota, observed());

    assert_eq!(reduction.rejections, vec![RejectionReason::MalformedBody]);
    assert_eq!(windows(&reduction).len(), 1);
}
