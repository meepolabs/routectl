//! Tests for the Anthropic header reducer, driven from captured wire values.
//!
//! Two of these pin refusals rather than results, and those are the ones
//! that matter. The bare-reset test pins a refusal that CANNOT be observed
//! from the corpus, because on every captured envelope the bare reset happens
//! to equal the 5h one -- so a reducer that pairs them passes every other test
//! here. The millis test pins the same class one level down.

use super::*;

use std::time::Instant;

/// Captured `anthropic-ratelimit-unified-*` values from a live envelope: a
/// full per-window triad plus the bare `reset`, in the epoch SECONDS the
/// family reports. The bare reset EQUALS the 5h one here, exactly as it does
/// on every captured envelope -- which is why the refusal below is pinned by a
/// test instead of trusted to observation.
const CAPTURED_5H_RESET_SECS: u64 = 1_781_001_000;
const CAPTURED_7D_RESET_SECS: u64 = 1_781_049_600;
const CAPTURED_5H_UTILIZATION: &str = "0.02";
const CAPTURED_7D_UTILIZATION: &str = "0.17";

fn epoch_secs(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// Observed an hour into the captured 5h window, which also sits well inside
/// the captured 7d window, so both resets are plausible from one stamp.
fn observed() -> ObservationStamp {
    ObservationStamp::from_parts(epoch_secs(CAPTURED_5H_RESET_SECS - 3_600), Instant::now())
}

/// The captured Anthropic family exactly as the shipped header parser produces
/// it: `5h-utilization`, `status`, `overage-status`, `overage-utilization`,
/// `representative-claim` and the bare `reset` in TYPED fields, and only what
/// the parser leaves over -- both per-window resets and the 7d utilization --
/// in `extras`. Placing a typed suffix in `extras` here would make every
/// assertion below prove the wrong thing.
fn captured_anthropic() -> AnthropicUnifiedQuota {
    let mut quota = AnthropicUnifiedQuota::default();
    quota.status = Some("allowed".into());
    quota.overage_status = Some("allowed".into());
    quota.utilization = Some(CAPTURED_5H_UTILIZATION.into());
    quota.overage_utilization = Some("0.0".into());
    quota.representative_claim = Some("five_hour".into());
    quota.reset = Some(CAPTURED_5H_RESET_SECS.to_string());
    quota.extras = vec![
        ("5h-status".into(), "allowed".into()),
        ("5h-reset".into(), CAPTURED_5H_RESET_SECS.to_string()),
        ("7d-status".into(), "allowed".into()),
        ("7d-reset".into(), CAPTURED_7D_RESET_SECS.to_string()),
        ("7d-utilization".into(), CAPTURED_7D_UTILIZATION.into()),
        ("overage-reset".into(), "1780994400".into()),
        ("fallback-percentage".into(), "0.5".into()),
    ];
    quota
}

/// The fraction and reset of a known window, or a panic naming the role that
/// unexpectedly came back unknown.
fn known(window: &QuotaWindow, role: &str) -> (f64, SystemTime) {
    match window {
        QuotaWindow::Known {
            utilization,
            reset_at,
        } => (utilization.fraction(), reset_at.at()),
        QuotaWindow::Unknown => panic!("the {role} window was expected to be known"),
    }
}

#[test]
fn a_captured_anthropic_envelope_yields_both_windows_with_their_own_resets() {
    let observed = observed();

    let snapshot = reduce_anthropic(&captured_anthropic(), &observed).snapshot;

    let (fast_fraction, fast_reset) = known(&snapshot.fast, "fast");
    let (slow_fraction, slow_reset) = known(&snapshot.slow, "slow");
    assert_eq!(fast_fraction, 0.02);
    assert_eq!(fast_reset, epoch_secs(CAPTURED_5H_RESET_SECS));
    assert_eq!(
        slow_fraction, 0.17,
        "the 7d utilization is reachable only through extras, and must not be \
         confused with the typed 5h value"
    );
    assert_eq!(slow_reset, epoch_secs(CAPTURED_7D_RESET_SECS));
    assert_eq!(snapshot.billing, Billing::Included);
}

#[test]
fn the_bare_anthropic_reset_is_never_paired_to_the_five_hour_window() {
    // Arrange: the per-window `5h-reset` is absent while the bare `reset`
    // carries exactly the value it would have had. On every captured envelope
    // those two are IDENTICAL, so a reducer that reads the bare value as the
    // 5h window's reset produces a Known window here and passes every other
    // test in this file. The bare header does not say which window it
    // describes, and a routing signal is not built on that guess.
    let observed = observed();
    let mut quota = captured_anthropic();
    quota.extras.retain(|(key, _)| key != "5h-reset");
    assert_eq!(
        quota.reset.as_deref(),
        Some(CAPTURED_5H_RESET_SECS.to_string().as_str()),
        "the bare reset must still carry the 5h value, or this test proves nothing"
    );

    // Act
    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    // Assert
    assert_eq!(
        snapshot.fast,
        QuotaWindow::Unknown,
        "with no 5h-reset the fast window has no reset of its own and must stay unknown, \
         however conveniently the bare reset happens to match it today"
    );
    let (slow_fraction, _) = known(&snapshot.slow, "slow");
    assert_eq!(
        slow_fraction, 0.17,
        "refusing the bare reset costs the fast window only"
    );
}

#[test]
fn a_milliseconds_scale_reset_produces_no_known_window() {
    // Arrange: a REAL captured reset multiplied by a thousand, i.e. the exact
    // shape of a milliseconds value misread as seconds. It lands tens of
    // thousands of years out, which every expiry check reads as permanently
    // valid.
    let observed = observed();
    let mut quota = captured_anthropic();
    let millis = (CAPTURED_5H_RESET_SECS * 1000).to_string();
    for (key, value) in &mut quota.extras {
        if key == "5h-reset" {
            *value = millis.clone();
        }
    }

    // Act
    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    // Assert
    assert_eq!(
        snapshot.fast,
        QuotaWindow::Unknown,
        "an implausible reset must yield no known window at all, rather than a \
         low-utilization seat that stays fresh forever and attracts every new session"
    );
}

#[test]
fn a_malformed_utilization_drops_only_its_own_window() {
    let observed = observed();
    let mut quota = captured_anthropic();
    quota.utilization = Some("not-a-number".into());

    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    assert_eq!(snapshot.fast, QuotaWindow::Unknown);
    let (slow_fraction, _) = known(&snapshot.slow, "slow");
    assert_eq!(slow_fraction, 0.17);
}

#[test]
fn every_window_malformed_yields_a_cap_dormant_snapshot() {
    let observed = observed();
    let mut quota = captured_anthropic();
    quota.utilization = Some("nonsense".into());
    for (key, value) in &mut quota.extras {
        if key == "7d-utilization" {
            *value = "nonsense".into();
        }
    }

    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    assert_eq!(snapshot.fast, QuotaWindow::Unknown);
    assert_eq!(snapshot.slow, QuotaWindow::Unknown);
}

#[test]
fn an_anthropic_family_with_no_resets_at_all_yields_no_known_window() {
    let observed = observed();
    let mut quota = captured_anthropic();
    quota
        .extras
        .retain(|(key, _)| key != "5h-reset" && key != "7d-reset");

    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    assert_eq!(snapshot.fast, QuotaWindow::Unknown);
    assert_eq!(snapshot.slow, QuotaWindow::Unknown);
}

#[test]
fn an_expired_reset_yields_no_known_window() {
    // Arrange: observed AFTER the 5h window has already reset.
    let observed =
        ObservationStamp::from_parts(epoch_secs(CAPTURED_5H_RESET_SECS + 60), Instant::now());

    let snapshot = reduce_anthropic(&captured_anthropic(), &observed).snapshot;

    assert_eq!(snapshot.fast, QuotaWindow::Unknown);
}

#[test]
fn a_missing_representative_claim_reads_as_unknown_billing_not_included() {
    let observed = observed();
    let mut quota = captured_anthropic();
    quota.representative_claim = None;

    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    assert_eq!(
        snapshot.billing,
        Billing::Unknown,
        "an absent claim is no evidence about which budget a seat bills against"
    );
}

#[test]
fn an_overage_claim_reads_as_overage_billing() {
    let observed = observed();
    let mut quota = captured_anthropic();
    quota.representative_claim = Some("overage".into());

    let snapshot = reduce_anthropic(&quota, &observed).snapshot;

    assert_eq!(snapshot.billing, Billing::Overage);
}
