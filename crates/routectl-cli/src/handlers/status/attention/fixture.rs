//! Test-only fully populated `/status/attention` panel, built by this panel's
//! own per-daemon state (the recovery memory over the pure builder) from two
//! reads of the shared fixture config's accounts, so the second read carries
//! a condition the first one saw and that has since cleared.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use routectl_core::UpstreamReason;
use routectl_core::failure_class::LastOutcome;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::{ProviderGateStatus, UnusableCause};
use routectl_router::{
    AccountIdentities, AccountReading, AccountReadings, AccountWindow, Billing, Finding,
    LastReading, LengthClass, PoolEligibility, ReadingSource, ReadingWindow, RejectionTotals,
    ReportState, Started, Status, UsageVendor,
};
use serde_json::Value;

use super::super::panel_fixtures::{
    FIXTURE_NOW_MS, fixture_as_of, fixture_config, fixture_targets,
};
use super::super::router_view::AccountCredential;
use super::super::usage_health::LedgerHealth;
use super::{AttentionPanel, AttentionSources, AttentionState, DriverReads, Panel, SCHEMA_VERSION};
use crate::commands::doctor::SeatFix;
use crate::server::seat_driver::{RefreshFailure, SeatDriverHealthSnapshot};
use crate::server::usage_poll::{PollStateSnapshot, SeatPollSnapshot};

/// How long before the fixture instant the first of the two reads ran.
const FIRST_READ_BEFORE_MS: i64 = 120_000;

/// The serialized populated attention envelope.
pub fn populated_attention() -> Value {
    serde_json::to_value(populated_attention_panel()).expect("the attention panel serializes")
}

/// The list with its doctor report current: an empty pool (broken), an open
/// route, two open routes on one provider entry failing alike, a logged-out
/// seat, a parked login, a failing token refresh, a
/// window near its limit, a login billing at overage, a failing usage poll,
/// a stopped poll driver, a recent record loss and a doctor failure (all
/// degraded), and a second pool that was empty on the earlier read and has
/// recovered.
pub fn populated_attention_panel() -> Panel<AttentionPanel> {
    two_reads(true)
}

/// The serialized envelope of [`doctor_unavailable_attention_panel`].
pub fn populated_attention_doctor_unavailable() -> Value {
    serde_json::to_value(doctor_unavailable_attention_panel())
        .expect("the attention panel serializes")
}

/// The same daemon whose second read could build no doctor report: coverage
/// reads `unavailable` and the doctor failure seen on the first read is held,
/// neither cleared nor recovered.
pub fn doctor_unavailable_attention_panel() -> Panel<AttentionPanel> {
    two_reads(false)
}

/// The panel one read of `sources` builds through a fresh daemon's state
/// (the recovery memory over the pure builder), stamped at its `now_ms`.
pub fn attention_panel_of(sources: AttentionSources) -> Panel<AttentionPanel> {
    let as_of = super::super::utc_rfc3339(sources.now_ms);
    let data = AttentionState::new().build(sources);
    Panel::available(SCHEMA_VERSION, as_of, data)
}

fn two_reads(doctor_current: bool) -> Panel<AttentionPanel> {
    let state = AttentionState::new();
    let first_ms = FIXTURE_NOW_MS - FIRST_READ_BEFORE_MS;
    let _ = state.build(sources(first_ms, 0, Some(doctor_findings())));
    let findings = doctor_current.then(doctor_findings);
    let data = state.build(sources(FIXTURE_NOW_MS, 1, findings));
    Panel::available(SCHEMA_VERSION, fixture_as_of(), data)
}

/// One read at `now_ms` with the `backup` pool's eligible seat count.
fn sources(
    now_ms: i64,
    backup_eligible: usize,
    doctor_findings: Option<Vec<Finding>>,
) -> AttentionSources {
    let identities = AccountIdentities::from_config(&fixture_config());
    let fixes = identities
        .iter()
        .map(|(account, identity)| {
            let fix = SeatFix::of(&fixture_config(), &identity.provider_entry);
            (account.to_string(), fix)
        })
        .collect();
    AttentionSources {
        now_ms,
        pools: vec![
            pool("seats", 2, 0),
            pool("backup", 1, backup_eligible),
            pool("api", 1, 1),
        ],
        targets: targets(),
        identities,
        credentials: credentials(),
        fixes,
        readings: readings(),
        poll: Some(poll()),
        refresh_failures: BTreeMap::from([(
            "#anthropic".to_string(),
            RefreshFailure {
                first_failure_ms: FIXTURE_NOW_MS - 1_800_000,
                token: "transient",
            },
        )]),
        drivers: DriverReads {
            refresh: SeatDriverHealthSnapshot {
                running: true,
                last_sweep_ms: Some(FIXTURE_NOW_MS - 20_000),
                last_good_sweep_ms: Some(FIXTURE_NOW_MS - 80_000),
                sweeps: 12,
                transient: 3,
                definitive: 1,
            },
            poll: SeatDriverHealthSnapshot {
                running: false,
                last_sweep_ms: Some(FIXTURE_NOW_MS - 900_000),
                last_good_sweep_ms: Some(FIXTURE_NOW_MS - 900_000),
                sweeps: 4,
                transient: 0,
                definitive: 0,
            },
            uptime: Duration::from_hours(2),
        },
        managed_accounts: 2,
        record_loss: LedgerHealth {
            dropped_full: 3,
            dropped_closed: 0,
            write_errors: 1,
        },
        doctor_findings,
    }
}

/// The shared targets plus two more on the `glm` target's provider entry,
/// both open after a server error: they fold into one provider item while
/// `glm`, open after a transport error, keeps its own route item.
fn targets() -> Vec<RouteTargetStatus> {
    let mut targets = fixture_targets();
    let glm = targets
        .iter()
        .find(|target| target.nickname == "glm")
        .expect("the shared targets carry glm")
        .clone();
    let failing = |nickname: &str| RouteTargetStatus {
        state_key: nickname.to_string(),
        nickname: nickname.to_string(),
        gate: ProviderGateStatus {
            last_outcome: Some(LastOutcome::Http5xx),
            ..glm.gate
        },
        ..glm.clone()
    };
    targets.extend([failing("glm-air"), failing("glm-flash")]);
    targets
}

fn pool(name: &str, configured: usize, eligible: usize) -> PoolEligibility {
    PoolEligibility {
        pool: name.to_string(),
        configured,
        eligible,
        by_reason: Vec::new(),
    }
}

fn doctor_findings() -> Vec<Finding> {
    vec![Finding {
        section: "config",
        name: "schema".to_string(),
        status: Status::Fail,
        detail: "config schema: fixture detail".to_string(),
        remediation: Some("routectl config check".to_string()),
    }]
}

/// The seat logged out for 2 hours, Codex parked for 10 more minutes, the
/// API-key entries usable.
fn credentials() -> BTreeMap<String, AccountCredential> {
    let logged_out = AccountCredential::unusable(
        UnusableCause::NotLoggedIn,
        FIXTURE_NOW_MS - 7_200_000,
        &SeatFix::of(&fixture_config(), "claude-seat"),
    );
    let parked =
        AccountCredential::parked(UpstreamReason::CredentialInvalid, FIXTURE_NOW_MS + 600_000);
    let usable = AccountCredential::usable();
    [
        ("anthropic#a", logged_out),
        ("codex", parked),
        ("#anthropic", usable.clone()),
        ("#compat", usable),
    ]
    .into_iter()
    .map(|(account, credential)| (account.to_string(), credential))
    .collect()
}

fn at_ms(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.unsigned_abs())
}

/// Codex has a fresh 5-hour window 95% used; the default Anthropic login
/// reports billing beyond its included plan.
fn readings() -> AccountReadings {
    let near_limit = AccountWindow {
        window: ReadingWindow {
            length_secs: 18_000,
            class: LengthClass::Short,
            scope: None,
            used: 0.95,
            reset: Some(at_ms(FIXTURE_NOW_MS + 3_600_000)),
            started: Started::Yes,
        },
        source: ReadingSource::Response,
        observed_at: at_ms(FIXTURE_NOW_MS - 60_000),
        age: Some(Duration::from_mins(1)),
        stale: false,
        kept: false,
    };
    AccountReadings {
        accounts: vec![
            AccountReading {
                account: "codex".to_string(),
                report: ReportState::Windows,
                windows: vec![near_limit],
                last_accepted: Some(LastReading {
                    observed_at: at_ms(FIXTURE_NOW_MS - 60_000),
                    source: ReadingSource::Response,
                    fresh: true,
                }),
                billing: Billing::Unknown,
                billing_fresh: false,
            },
            AccountReading {
                account: "#anthropic".to_string(),
                report: ReportState::NoReading,
                windows: Vec::new(),
                last_accepted: None,
                billing: Billing::Overage,
                billing_fresh: true,
            },
        ],
        rejections: RejectionTotals {
            invalid_utilization: 0,
            expired_reset: 0,
            implausible_reset: 0,
            overflow: 0,
            contradictory: 0,
            unknown_length: 0,
            invalid_started: 0,
            malformed_body: 0,
        },
        refused_by_admission: 0,
        corrections_confirmed: 0,
    }
}

/// The Anthropic seat's polls failing for 20 minutes.
fn poll() -> PollStateSnapshot {
    PollStateSnapshot {
        seats: vec![SeatPollSnapshot {
            account: "anthropic#a".to_string(),
            vendor: UsageVendor::Anthropic,
            awaiting_login: false,
            last_attempt_ms: Some(FIXTURE_NOW_MS - 60_000),
            last_result: Some("http"),
            next_due_ms: Some(FIXTURE_NOW_MS + 240_000),
            failure_since_ms: Some(FIXTURE_NOW_MS - 1_200_000),
        }],
        paused_until_ms: Vec::new(),
        switched_off: false,
    }
}
