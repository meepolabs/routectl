//! `Router::observe_polled_usage` lands a polled usage body on the account
//! meter and nowhere else, and `Router::account_readings` reads it back.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use routectl_core::Provider;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step};
use super::super::{Config, Router};
use super::PolledUsageRejection;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::quota::freshness::ObservationStamp;
use crate::quota::meter::ReportState;
use crate::quota::reading::{ReadingSource, Started};
use crate::quota::reduce::RejectionReason;
use crate::quota::usage_body::{BodyError, UsageVendor};
use crate::quota::view::AccountReading;
use crate::resolved::ResolvedModel;
use crate::runtime_state::{AccountCredential, ProviderGateStatus};

const ACCOUNT: &str = "anthropic#a";
const STATE_KEY: &str = "m-a";

/// One model on entry `p-a` authenticating as the OAuth seat `anthropic#a`,
/// so the seat is both a configured meter account and a credential account.
fn router() -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p-a".into(),
        ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
            circuit_failures: Some(1),
            ..Default::default()
        }),
    );
    let scripted = ScriptedProvider::new("p-a", &[Step::Ok]);
    let model = ResolvedModel::new(
        STATE_KEY,
        "p-a",
        scripted as Arc<dyn Provider>,
        "claude-m-a",
    )
    .with_auth_secret_ref(routectl_auth::SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some("a".into()),
    });
    let models: BTreeMap<String, Arc<ResolvedModel>> =
        std::iter::once((STATE_KEY.to_string(), Arc::new(model))).collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn rfc3339_in(offset: Duration) -> String {
    OffsetDateTime::from(SystemTime::now() + offset)
        .format(&Rfc3339)
        .expect("a formattable instant")
}

/// An Anthropic usage body running the 5-hour window at `percent`, resetting
/// at `resets_at`.
fn anthropic_running(percent: u32, resets_at: &str) -> Vec<u8> {
    format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{resets_at}"}}, "seven_day": null}}"#
    )
    .into_bytes()
}

/// An Anthropic usage body for an idle account: the 5-hour window unstarted.
fn anthropic_idle() -> Vec<u8> {
    br#"{"five_hour": {"utilization": 0, "resets_at": null}, "seven_day": null}"#.to_vec()
}

fn record(router: &Router) -> Option<AccountCredential> {
    router.credential_accounts.lock().get(ACCOUNT).copied()
}

fn reading(router: &Router) -> AccountReading {
    let readings = router.account_readings(&ObservationStamp::now());
    readings
        .accounts
        .into_iter()
        .find(|a| a.account == ACCOUNT)
        .expect("the configured account is listed")
}

/// What the poll entry must never change: the target's gate (eligibility,
/// breaker phase, park and unusable mark) and the account's credential record.
#[derive(Debug, PartialEq)]
struct Containment {
    gate: ProviderGateStatus,
    record: Option<AccountCredential>,
}

fn containment(router: &Router) -> Containment {
    Containment {
        gate: router.gate_status_for_tests(STATE_KEY),
        record: record(router),
    }
}

#[test]
fn an_accepted_body_lands_on_the_meter_and_reads_back_through_the_view() {
    // Arrange
    let router = router();
    let generation = router.acquire_credential_generation(ACCOUNT);
    let body = anthropic_running(42, &rfc3339_in(Duration::from_hours(2)));

    // Act
    let accepted = router
        .observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, &body, generation)
        .expect("accepted");

    // Assert
    assert_eq!(accepted.windows.len(), 1);
    assert_eq!(accepted.windows[0].length_secs, 5 * 3_600);
    assert_eq!(accepted.windows[0].started, Started::Yes);
    let held = reading(&router);
    assert_eq!(held.report, ReportState::Windows);
    assert_eq!(
        held.last_accepted.map(|last| last.source),
        Some(ReadingSource::Poll)
    );
    let [window] = held.windows.as_slice() else {
        panic!("one window: {:?}", held.windows);
    };
    assert!((window.window.used - 0.42).abs() < 1e-9);
    assert!(!window.stale);
    assert_eq!(window.source, ReadingSource::Poll);
}

/// A superseded generation changes nothing and counts nothing, whether its
/// body would have been taken or refused.
#[test]
fn a_generation_overtaken_by_a_reset_is_superseded_and_changes_nothing() {
    let rows: [(&str, Vec<u8>); 2] = [
        ("idle body", anthropic_idle()),
        ("malformed body", b"<html>".to_vec()),
    ];
    for (name, body) in rows {
        // Arrange
        let router = router();
        let generation = router.acquire_credential_generation(ACCOUNT);
        assert_eq!(
            router.lift_credential_parks([ACCOUNT]).len(),
            1,
            "row: {name}"
        );
        let before = containment(&router);

        // Act
        let result =
            router.observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, &body, generation);

        // Assert
        assert_eq!(result, Err(PolledUsageRejection::Superseded), "row: {name}");
        assert_eq!(containment(&router), before, "row: {name}");
        assert_eq!(
            reading(&router).report,
            ReportState::NoReading,
            "row: {name}"
        );
        assert_eq!(
            router
                .account_readings(&ObservationStamp::now())
                .rejections
                .malformed_body,
            0,
            "row: {name}"
        );
    }
}

/// An account with no credential record never handed out a generation, so no
/// generation is current for it, and the check must not mint one.
#[test]
fn the_generation_check_creates_no_credential_record() {
    // Arrange
    let router = router();
    assert_eq!(record(&router), None);

    // Act
    let result = router.observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, &anthropic_idle(), 0);

    // Assert
    assert_eq!(result, Err(PolledUsageRejection::Superseded));
    assert_eq!(record(&router), None);
    assert_eq!(reading(&router).report, ReportState::NoReading);
}

#[test]
fn an_account_the_config_does_not_serve_is_refused_with_nothing_stored() {
    // Arrange
    let router = router();
    let retired = "anthropic#retired";
    let generation = router.acquire_credential_generation(retired);

    // Act
    let result = router.observe_polled_usage(
        retired,
        UsageVendor::Anthropic,
        &anthropic_idle(),
        generation,
    );

    // Assert
    assert_eq!(result, Err(PolledUsageRejection::NotAdmitted));
    let readings = router.account_readings(&ObservationStamp::now());
    assert_eq!(readings.refused_by_admission, 1);
    assert!(readings.accounts.iter().all(|a| a.account != retired));
    assert_eq!(reading(&router).report, ReportState::NoReading);
}

#[test]
fn a_body_that_does_not_reduce_is_counted_and_stores_nothing() {
    let rows: [(&str, &[u8], BodyError); 2] = [
        ("empty", b"", BodyError::Empty),
        (
            "unstarted with usage",
            br#"{"five_hour": {"utilization": 3, "resets_at": null}, "seven_day": null}"#,
            BodyError::Window(RejectionReason::InvalidStarted),
        ),
    ];
    for (name, body, expected) in rows {
        // Arrange
        let router = router();
        let generation = router.acquire_credential_generation(ACCOUNT);

        // Act
        let result = router.observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, body, generation);

        // Assert
        assert_eq!(
            result,
            Err(PolledUsageRejection::Body(expected)),
            "row: {name}"
        );
        let readings = router.account_readings(&ObservationStamp::now());
        let counted = readings.rejections.malformed_body + readings.rejections.invalid_started;
        assert_eq!(counted, 1, "row: {name}");
        assert_eq!(
            reading(&router).report,
            ReportState::NoReading,
            "row: {name}"
        );
    }
}

#[test]
fn a_contradicting_body_is_refused_and_keeps_the_stored_reading() {
    // Arrange
    let router = router();
    let generation = router.acquire_credential_generation(ACCOUNT);
    let resets_at = rfc3339_in(Duration::from_hours(2));
    router
        .observe_polled_usage(
            ACCOUNT,
            UsageVendor::Anthropic,
            &anthropic_running(50, &resets_at),
            generation,
        )
        .expect("accepted");

    // Act
    let result = router.observe_polled_usage(
        ACCOUNT,
        UsageVendor::Anthropic,
        &anthropic_running(10, &resets_at),
        generation,
    );

    // Assert
    assert_eq!(
        result,
        Err(PolledUsageRejection::Rejected(
            RejectionReason::Contradictory
        ))
    );
    let held = reading(&router);
    assert!((held.windows[0].window.used - 0.50).abs() < 1e-9);
}

/// Every outcome of the poll entry, accepted or refused, leaves the target's
/// eligibility, breaker and credential record exactly as they were.
#[test]
fn no_poll_outcome_touches_eligibility_the_breaker_or_the_credential_record() {
    let resets_at = rfc3339_in(Duration::from_hours(2));
    let rows: [(&str, Vec<u8>, bool); 4] = [
        ("accepted running", anthropic_running(30, &resets_at), true),
        ("accepted idle", anthropic_idle(), true),
        ("not json", b"<html>".to_vec(), false),
        ("missing member", br#"{"five_hour": null}"#.to_vec(), false),
    ];
    for (name, body, accepted) in rows {
        // Arrange
        let router = router();
        let generation = router.acquire_credential_generation(ACCOUNT);
        let before = containment(&router);
        assert_eq!(before.gate.reason, None, "row: {name}: eligible before");

        // Act
        let result =
            router.observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, &body, generation);

        // Assert
        assert_eq!(result.is_ok(), accepted, "row: {name}: {result:?}");
        assert_eq!(containment(&router), before, "row: {name}");
    }
}

/// An idle seat polled as unstarted joins placement's below-cap tier while the
/// reading is fresh.
#[test]
fn a_polled_idle_seat_projects_a_known_empty_fast_window() {
    // Arrange
    let router = router();
    let generation = router.acquire_credential_generation(ACCOUNT);

    // Act
    router
        .observe_polled_usage(
            ACCOUNT,
            UsageVendor::Anthropic,
            &anthropic_idle(),
            generation,
        )
        .expect("accepted");

    // Assert
    let key = crate::quota::key::seat_key_for_served_identity(ACCOUNT);
    let fast = crate::quota::view_test_support::fresh_five_hour(
        &router.quota_store,
        &key,
        &ObservationStamp::now(),
    );
    assert_eq!(fast, Some(0.0));
}

/// A seat polled as running near its cap, then polled as idle while that
/// window is still in effect, keeps its running reading: the idle body is
/// refused, counted once, and never ranks the seat as the emptiest.
#[test]
fn an_idle_body_over_a_window_still_running_is_refused_and_keeps_the_reading() {
    // Arrange
    let router = router();
    let generation = router.acquire_credential_generation(ACCOUNT);
    router
        .observe_polled_usage(
            ACCOUNT,
            UsageVendor::Anthropic,
            &anthropic_running(95, &rfc3339_in(Duration::from_hours(3))),
            generation,
        )
        .expect("accepted");

    // Act
    let result = router.observe_polled_usage(
        ACCOUNT,
        UsageVendor::Anthropic,
        &anthropic_idle(),
        generation,
    );

    // Assert
    assert_eq!(
        result,
        Err(PolledUsageRejection::Rejected(
            RejectionReason::Contradictory
        ))
    );
    let held = reading(&router);
    assert_eq!(held.windows[0].window.started, Started::Yes);
    assert!((held.windows[0].window.used - 0.95).abs() < 1e-9);
    assert_eq!(
        router
            .account_readings(&ObservationStamp::now())
            .rejections
            .contradictory,
        1
    );
}

/// Records, for every WARN, whether the credential account map was free.
struct LockProbe {
    accounts: Arc<parking_lot::Mutex<super::super::credential_park::CredentialAccounts>>,
    free_at_warn: Arc<std::sync::Mutex<Vec<bool>>>,
}

impl tracing::Subscriber for LockProbe {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            let is_free = self.accounts.try_lock().is_some();
            self.free_at_warn.lock().expect("probe lock").push(is_free);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Every dispatch admission takes the credential account map, so a refused
/// poll's WARN is written only after the map is released.
#[test]
fn a_refused_poll_warns_with_the_credential_account_map_released() {
    let rows: [(&str, Vec<u8>); 2] = [
        ("meter refusal", anthropic_idle()),
        ("body refusal", b"<html>".to_vec()),
    ];
    for (name, refused) in rows {
        // Arrange
        let router = router();
        let generation = router.acquire_credential_generation(ACCOUNT);
        router
            .observe_polled_usage(
                ACCOUNT,
                UsageVendor::Anthropic,
                &anthropic_running(95, &rfc3339_in(Duration::from_hours(3))),
                generation,
            )
            .expect("accepted");
        let free_at_warn = Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = LockProbe {
            accounts: Arc::clone(&router.credential_accounts),
            free_at_warn: Arc::clone(&free_at_warn),
        };
        // A second live dispatch keeps callsite registration off the
        // single-dispatch fast path, which another thread could otherwise
        // resolve as never-interested before this probe is installed.
        let _keepalive = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());

        // Act
        let result = tracing::subscriber::with_default(probe, || {
            router.observe_polled_usage(ACCOUNT, UsageVendor::Anthropic, &refused, generation)
        });

        // Assert
        assert!(result.is_err(), "row: {name}: {result:?}");
        assert_eq!(
            *free_at_warn.lock().expect("probe lock"),
            vec![true],
            "row: {name}: one WARN, written with the map released"
        );
    }
}
