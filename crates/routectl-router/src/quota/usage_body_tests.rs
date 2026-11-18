//! Tests for the usage-body reducers.
//!
//! Every fixture is hand-written, not captured. The Anthropic shape follows
//! the `/api/oauth/usage` schema embedded in Claude Code (`five_hour`,
//! `seven_day`, `seven_day_<scope>` each `{utilization, resets_at}` with a
//! 0-100 percent and an ISO 8601 reset, plus `extra_usage` and `limits[]`),
//! with an idle window reported as `{utilization: 0, resets_at: null}`. The
//! Codex shape follows openai/codex `codex-backend-openapi-models`
//! (`RateLimitStatusPayload`, `RateLimitWindowSnapshot`: `used_percent`,
//! `limit_window_seconds`, `reset_after_seconds`, `reset_at` epoch seconds),
//! with an unstarted window reporting `reset_after_seconds` equal to its length
//! and `reset_at` one length after the read.

use super::*;

use std::time::Instant;

use crate::quota::reading::LengthClass;

/// 2026-11-17T12:00:00Z.
const OBSERVED_EPOCH_SECS: u64 = 1_794_916_800;
const OBSERVED_RFC3339: &str = "2026-11-17T12:00:00Z";

fn observed() -> ObservationStamp {
    ObservationStamp::from_parts(
        SystemTime::UNIX_EPOCH + Duration::from_secs(OBSERVED_EPOCH_SECS),
        Instant::now(),
    )
}

fn at_offset(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(OBSERVED_EPOCH_SECS + secs)
}

/// An Anthropic body running a 5-hour window at 42% (resets 2 h after the
/// read) and a weekly at 17% (resets 3 days after), with an Opus-scoped weekly,
/// a null Sonnet weekly, and the non-window members a real body carries.
const ANTHROPIC_RUNNING: &str = r#"{
  "five_hour": {"utilization": 42, "resets_at": "2026-11-17T14:00:00.000000+00:00"},
  "seven_day": {"utilization": 17.0, "resets_at": "2026-11-20T12:00:00Z"},
  "seven_day_opus": {"utilization": 5, "resets_at": "2026-11-20T12:00:00Z"},
  "seven_day_sonnet": null,
  "seven_day_oauth_apps": null,
  "extra_usage": {"is_enabled": true, "monthly_limit": 5000, "used_credits": 120, "utilization": 2.4, "disabled_reason": null},
  "limits": [{"kind": "session", "group": "all", "percent": 42, "resets_at": "2026-11-17T14:00:00Z", "is_active": true}],
  "plan": "max"
}"#;

/// A Codex body with a 5-hour primary at 30% (resets in 1 h) and a weekly
/// secondary at 64% (resets in 2 days), plus credits, plan and account ids.
const CODEX_RUNNING: &str = r#"{
  "plan_type": "plus",
  "rate_limit": {
    "allowed": true,
    "limit_reached": false,
    "primary_window": {"used_percent": 30, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": 1794920400},
    "secondary_window": {"used_percent": 64, "limit_window_seconds": 604800, "reset_after_seconds": 172800, "reset_at": 1795089600}
  },
  "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
  "account_id": "acct-synthetic",
  "user_id": "user-synthetic"
}"#;

fn windows(observation: &AccountObservation) -> &[WindowReading] {
    match observation.report() {
        ObservationReport::Complete(set) => set.as_slice(),
        other => panic!("expected a complete report, got {other:?}"),
    }
}

fn anthropic_window_json(utilization: &str, resets_at: &str) -> String {
    format!(r#"{{"utilization": {utilization}, "resets_at": {resets_at}}}"#)
}

fn codex_window_json(used: &str, length: &str, reset_after: &str, reset_at: &str) -> String {
    format!(
        r#"{{"used_percent": {used}, "limit_window_seconds": {length}, "reset_after_seconds": {reset_after}, "reset_at": {reset_at}}}"#
    )
}

fn codex_body(primary: &str, secondary: &str) -> String {
    format!(
        r#"{{"plan_type": "plus", "rate_limit": {{"allowed": true, "limit_reached": false, "primary_window": {primary}, "secondary_window": {secondary}}}}}"#
    )
}

/// A valid running 5-hour Codex window at `used`.
fn codex_five_hour(used: &str) -> String {
    codex_window_json(
        used,
        "18000",
        "3600",
        &(OBSERVED_EPOCH_SECS + 3_600).to_string(),
    )
}

fn anthropic_with_five_hour(window: &str) -> String {
    format!(r#"{{"five_hour": {window}, "seven_day": null}}"#)
}

fn anthropic_with_many_scoped_weeklies(count: usize) -> String {
    let scoped: Vec<String> = (0..count)
        .map(|i| {
            format!(
                r#""seven_day_s{i}": {}"#,
                anthropic_window_json("10", r#""2026-11-20T12:00:00Z""#)
            )
        })
        .collect();
    format!(
        r#"{{"five_hour": null, "seven_day": null, {}}}"#,
        scoped.join(", ")
    )
}

#[derive(Clone, Copy, Debug)]
enum Vendor {
    Anthropic,
    Codex,
}

fn reduce(vendor: Vendor, body: &str) -> Result<AccountObservation, BodyError> {
    match vendor {
        Vendor::Anthropic => reduce_anthropic_body(body.as_bytes(), observed()),
        Vendor::Codex => reduce_codex_body(body.as_bytes(), observed()),
    }
}

// ---- Malformed bodies ----

#[test]
fn malformed_bodies_reduce_to_their_named_error() {
    let rfc = format!(r#""{OBSERVED_RFC3339}""#);
    let unstarted_used = codex_window_json(
        "5",
        "18000",
        "18000",
        &(OBSERVED_EPOCH_SECS + 18_000).to_string(),
    );
    let reset_in_ms = codex_window_json(
        "30",
        "18000",
        "3600",
        &((OBSERVED_EPOCH_SECS + 3_600) * 1_000).to_string(),
    );
    let longer_than_31_days = codex_window_json(
        "30",
        &(31 * 86_400 + 1).to_string(),
        "3600",
        &(OBSERVED_EPOCH_SECS + 3_600).to_string(),
    );
    let rows: Vec<(&str, Vendor, String, BodyError)> = vec![
        ("empty", Vendor::Anthropic, String::new(), BodyError::Empty),
        (
            "whitespace only",
            Vendor::Codex,
            " \n\t ".into(),
            BodyError::Empty,
        ),
        (
            "not JSON",
            Vendor::Anthropic,
            "<html>sign in</html>".into(),
            BodyError::NotJson,
        ),
        (
            "truncated JSON",
            Vendor::Codex,
            CODEX_RUNNING[..CODEX_RUNNING.len() / 2].into(),
            BodyError::NotJson,
        ),
        (
            "trailing garbage after the object",
            Vendor::Anthropic,
            format!("{ANTHROPIC_RUNNING} x"),
            BodyError::NotJson,
        ),
        (
            "NaN utilization (not representable in JSON)",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("NaN", "null")),
            BodyError::NotJson,
        ),
        (
            "number beyond f64 range",
            Vendor::Codex,
            codex_body(&codex_five_hour("1e400"), "null"),
            BodyError::NotJson,
        ),
        (
            "top level is an array",
            Vendor::Anthropic,
            "[]".into(),
            BodyError::WrongType,
        ),
        (
            "utilization is a string",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json(r#""42""#, "null")),
            BodyError::WrongType,
        ),
        (
            "utilization is null",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("null", "null")),
            BodyError::WrongType,
        ),
        (
            "resets_at is a number",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("42", "1794920400")),
            BodyError::WrongType,
        ),
        (
            "window is a string",
            Vendor::Anthropic,
            anthropic_with_five_hour(r#""42%""#),
            BodyError::WrongType,
        ),
        (
            "scoped weekly is a number",
            Vendor::Anthropic,
            r#"{"five_hour": null, "seven_day": null, "seven_day_opus": 5}"#.into(),
            BodyError::WrongType,
        ),
        (
            "rate_limit is a string",
            Vendor::Codex,
            r#"{"rate_limit": "ok"}"#.into(),
            BodyError::WrongType,
        ),
        (
            "rate_limit is null",
            Vendor::Codex,
            r#"{"rate_limit": null}"#.into(),
            BodyError::WrongType,
        ),
        (
            "limit_window_seconds is fractional",
            Vendor::Codex,
            codex_body(
                &codex_window_json("30", "18000.5", "3600", "1794920400"),
                "null",
            ),
            BodyError::WrongType,
        ),
        (
            "reset_after_seconds is negative",
            Vendor::Codex,
            codex_body(
                &codex_window_json("30", "18000", "-1", "1794920400"),
                "null",
            ),
            BodyError::WrongType,
        ),
        (
            "reset_at is an ISO string",
            Vendor::Codex,
            codex_body(&codex_window_json("30", "18000", "3600", &rfc), "null"),
            BodyError::WrongType,
        ),
        (
            "negative utilization",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("-1", "null")),
            BodyError::InvalidPercent,
        ),
        (
            "negative used_percent",
            Vendor::Codex,
            codex_body(&codex_five_hour("-0.5"), "null"),
            BodyError::InvalidPercent,
        ),
        (
            "utilization above 100",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("100.5", r#""2026-11-17T14:00:00Z""#)),
            BodyError::InvalidPercent,
        ),
        (
            "used_percent above 100",
            Vendor::Codex,
            codex_body(&codex_five_hour("101"), "null"),
            BodyError::InvalidPercent,
        ),
        (
            "reset not RFC 3339",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("42", r#""next tuesday""#)),
            BodyError::InvalidTimestamp,
        ),
        (
            "reset in epoch milliseconds",
            Vendor::Codex,
            codex_body(&reset_in_ms, "null"),
            BodyError::Window(RejectionReason::ImplausibleReset),
        ),
        (
            "reset already passed",
            Vendor::Anthropic,
            anthropic_with_five_hour(&anthropic_window_json("42", &rfc)),
            BodyError::Window(RejectionReason::ExpiredReset),
        ),
        (
            "window length over 31 days",
            Vendor::Codex,
            codex_body(&longer_than_31_days, "null"),
            BodyError::Window(RejectionReason::UnknownLength),
        ),
        (
            "window length zero",
            Vendor::Codex,
            codex_body(&codex_window_json("0", "0", "0", "1794920400"), "null"),
            BodyError::Window(RejectionReason::UnknownLength),
        ),
        (
            "unstarted window with usage",
            Vendor::Codex,
            codex_body(&unstarted_used, "null"),
            BodyError::Window(RejectionReason::InvalidStarted),
        ),
        (
            "duplicate top-level key",
            Vendor::Anthropic,
            r#"{"five_hour": null, "seven_day": null, "five_hour": null}"#.into(),
            BodyError::DuplicateKey,
        ),
        (
            "duplicate key inside a window",
            Vendor::Codex,
            codex_body(
                r#"{"used_percent": 1, "used_percent": 90, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": 1794920400}"#,
                "null",
            ),
            BodyError::DuplicateKey,
        ),
        (
            "duplicate key inside an ignored member",
            Vendor::Anthropic,
            r#"{"five_hour": null, "seven_day": null, "limits": [{"kind": "a", "kind": "b"}]}"#
                .into(),
            BodyError::DuplicateKey,
        ),
        (
            "two windows of one length",
            Vendor::Codex,
            codex_body(&codex_five_hour("30"), &codex_five_hour("40")),
            BodyError::DuplicateWindow,
        ),
        (
            "too many windows",
            Vendor::Anthropic,
            anthropic_with_many_scoped_weeklies(MAX_WINDOWS_PER_OBSERVATION + 1),
            BodyError::TooManyWindows,
        ),
        (
            "missing five_hour",
            Vendor::Anthropic,
            r#"{"seven_day": null}"#.into(),
            BodyError::MissingMember,
        ),
        (
            "missing seven_day",
            Vendor::Anthropic,
            r#"{"five_hour": null}"#.into(),
            BodyError::MissingMember,
        ),
        (
            "missing resets_at",
            Vendor::Anthropic,
            anthropic_with_five_hour(r#"{"utilization": 0}"#),
            BodyError::MissingMember,
        ),
        (
            "missing rate_limit",
            Vendor::Codex,
            r#"{"plan_type": "plus"}"#.into(),
            BodyError::MissingMember,
        ),
        (
            "missing secondary_window",
            Vendor::Codex,
            r#"{"rate_limit": {"primary_window": null}}"#.into(),
            BodyError::MissingMember,
        ),
        (
            "missing reset_after_seconds",
            Vendor::Codex,
            codex_body(
                r#"{"used_percent": 1, "limit_window_seconds": 18000, "reset_at": 1794920400}"#,
                "null",
            ),
            BodyError::MissingMember,
        ),
        (
            "non-canonical scope (uppercase)",
            Vendor::Anthropic,
            r#"{"five_hour": null, "seven_day": null, "seven_day_Opus": null}"#.into(),
            BodyError::NonCanonicalScope,
        ),
        (
            "non-canonical scope (empty)",
            Vendor::Anthropic,
            r#"{"five_hour": null, "seven_day": null, "seven_day_": null}"#.into(),
            BodyError::NonCanonicalScope,
        ),
        (
            "non-canonical scope (too long)",
            Vendor::Anthropic,
            format!(
                r#"{{"five_hour": null, "seven_day": null, "seven_day_{}": null}}"#,
                "a".repeat(33)
            ),
            BodyError::NonCanonicalScope,
        ),
    ];

    for (name, vendor, body, expected) in rows {
        let outcome = reduce(vendor, &body);
        assert_eq!(
            outcome.as_ref().map(|_| ()).unwrap_err(),
            &expected,
            "row: {name} ({vendor:?})"
        );
    }
}

/// The bound is exactly the per-observation maximum, so the too-many row above
/// fails on the count and not on some other property of the fixture.
#[test]
fn the_window_count_bound_accepts_exactly_the_maximum() {
    let body = anthropic_with_many_scoped_weeklies(MAX_WINDOWS_PER_OBSERVATION);

    let observation = reduce(Vendor::Anthropic, &body).expect("the maximum is accepted");

    assert_eq!(windows(&observation).len(), MAX_WINDOWS_PER_OBSERVATION);
}

#[test]
fn exactly_100_percent_is_accepted_as_exhausted() {
    let body = codex_body(&codex_five_hour("100"), "null");

    let observation = reduce(Vendor::Codex, &body).expect("100% is in scale");

    assert_eq!(windows(&observation)[0].used().fraction(), 1.0);
}

// ---- Anthropic ----

/// Length, scope, used fraction, started state and reset of one window.
type AnthropicRow<'a> = (u64, Option<&'a str>, f64, Started, Option<SystemTime>);

#[test]
fn anthropic_running_body_reduces_known_windows_and_ignores_the_rest() {
    let stamp = observed();

    let observation =
        reduce_anthropic_body(ANTHROPIC_RUNNING.as_bytes(), stamp).expect("a valid body");

    assert_eq!(observation.source(), ReadingSource::Poll);
    assert_eq!(observation.observed(), &stamp);
    let got: Vec<AnthropicRow> = windows(&observation)
        .iter()
        .map(|w| {
            (
                w.key().length_secs(),
                w.key().scope().map(Scope::as_str),
                w.used().fraction(),
                w.started(),
                w.reset().map(|r| r.at()),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                FIVE_HOURS_SECS,
                None,
                0.42,
                Started::Yes,
                Some(at_offset(2 * 3_600))
            ),
            (
                SEVEN_DAYS_SECS,
                None,
                0.17,
                Started::Yes,
                Some(at_offset(3 * 86_400))
            ),
            (
                SEVEN_DAYS_SECS,
                Some("opus"),
                0.05,
                Started::Yes,
                Some(at_offset(3 * 86_400))
            ),
        ]
    );
}

#[test]
fn anthropic_body_with_every_window_null_reduces_to_no_windows() {
    let body =
        r#"{"five_hour": null, "seven_day": null, "seven_day_opus": null, "extra_usage": null}"#;

    let observation = reduce(Vendor::Anthropic, body).expect("a schema-valid body");

    assert_eq!(observation.report(), &ObservationReport::NoWindows);
    assert_eq!(observation.source(), ReadingSource::Poll);
}

#[test]
fn anthropic_body_without_the_required_members_is_an_error_not_no_windows() {
    let rows = [
        ("empty object", "{}"),
        (
            "only non-window members",
            r#"{"extra_usage": null, "limits": []}"#,
        ),
        ("only a scoped weekly", r#"{"seven_day_opus": null}"#),
    ];

    for (name, body) in rows {
        assert_eq!(
            reduce(Vendor::Anthropic, body).map(|_| ()),
            Err(BodyError::MissingMember),
            "row: {name}"
        );
    }
}

#[test]
fn claude_unstarted_window_reduces_to_not_started_at_zero() {
    let body = anthropic_with_five_hour(&anthropic_window_json("0", "null"));

    let observation = reduce(Vendor::Anthropic, &body).expect("the idle shape is valid");

    let [window] = windows(&observation) else {
        panic!("one window expected");
    };
    assert_eq!(window.started(), Started::No);
    assert_eq!(window.used().fraction(), 0.0);
    assert_eq!(window.reset(), None);
    assert_eq!(window.class(), LengthClass::Short);
}

/// A running window that has not used anything yet still carries its reset and
/// is not the idle shape.
#[test]
fn claude_window_at_zero_with_a_reset_is_running() {
    let body = anthropic_with_five_hour(&anthropic_window_json("0", r#""2026-11-17T16:00:00Z""#));

    let observation = reduce(Vendor::Anthropic, &body).expect("a valid body");

    assert_eq!(windows(&observation)[0].started(), Started::Yes);
}

#[test]
fn claude_used_window_without_a_reset_is_refused() {
    let body = anthropic_with_five_hour(&anthropic_window_json("12", "null"));

    assert_eq!(
        reduce(Vendor::Anthropic, &body).map(|_| ()),
        Err(BodyError::Window(RejectionReason::InvalidStarted))
    );
}

// ---- Codex ----

#[test]
fn codex_five_hour_primary_and_weekly_secondary_classify_short_and_long() {
    let observation = reduce(Vendor::Codex, CODEX_RUNNING).expect("a valid body");

    assert_eq!(observation.source(), ReadingSource::Poll);
    let got: Vec<(LengthClass, u64, f64, Started, Option<SystemTime>)> = windows(&observation)
        .iter()
        .map(|w| {
            (
                w.class(),
                w.key().length_secs(),
                w.used().fraction(),
                w.started(),
                w.reset().map(|r| r.at()),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                LengthClass::Short,
                FIVE_HOURS_SECS,
                0.30,
                Started::Yes,
                Some(at_offset(3_600))
            ),
            (
                LengthClass::Long,
                SEVEN_DAYS_SECS,
                0.64,
                Started::Yes,
                Some(at_offset(2 * 86_400))
            ),
        ]
    );
    assert!(
        windows(&observation)
            .iter()
            .all(|w| w.key().scope().is_none()),
        "no vendor value becomes a scope"
    );
}

/// The class follows the reported length, not the slot: a weekly-only plan
/// reports its weekly window as the primary.
#[test]
fn codex_weekly_primary_with_no_secondary_is_one_long_window() {
    let weekly = codex_window_json(
        "12",
        "604800",
        "86400",
        &(OBSERVED_EPOCH_SECS + 86_400).to_string(),
    );
    let body = codex_body(&weekly, "null");

    let observation = reduce(Vendor::Codex, &body).expect("a valid body");

    let [window] = windows(&observation) else {
        panic!("one window expected");
    };
    assert_eq!(window.class(), LengthClass::Long);
}

#[test]
fn codex_unstarted_windows_reduce_to_not_started_at_zero() {
    let rows = [
        ("reset_after equal to the length", "18000", 18_000_u64),
        ("reset_after above the length (rounding)", "18001", 18_000),
    ];

    for (name, reset_after, reset_offset) in rows {
        let primary = codex_window_json(
            "0",
            "18000",
            reset_after,
            &(OBSERVED_EPOCH_SECS + reset_offset).to_string(),
        );
        let body = codex_body(&primary, "null");

        let observation = reduce(Vendor::Codex, &body).expect(name);

        let [window] = windows(&observation) else {
            panic!("row {name}: one window expected");
        };
        assert_eq!(window.started(), Started::No, "row: {name}");
        assert_eq!(window.used().fraction(), 0.0, "row: {name}");
        assert_eq!(
            window.reset().map(|r| r.at()),
            Some(at_offset(reset_offset)),
            "row: {name}"
        );
    }
}

#[test]
fn codex_window_one_second_into_its_length_is_running() {
    let primary = codex_window_json(
        "0",
        "18000",
        "17999",
        &(OBSERVED_EPOCH_SECS + 17_999).to_string(),
    );

    let observation = reduce(Vendor::Codex, &codex_body(&primary, "null")).expect("valid");

    assert_eq!(windows(&observation)[0].started(), Started::Yes);
}

#[test]
fn codex_body_with_both_windows_null_reduces_to_no_windows() {
    let observation =
        reduce(Vendor::Codex, &codex_body("null", "null")).expect("a schema-valid body");

    assert_eq!(observation.report(), &ObservationReport::NoWindows);
}
