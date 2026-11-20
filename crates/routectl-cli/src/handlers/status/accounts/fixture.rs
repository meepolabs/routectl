//! Test-only fully populated `/status/accounts` panel, built by this panel's
//! own `build_from_sources` from the shared fixture config's accounts.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;

use routectl_router::{
    AccountIdentities, AccountReading, AccountReadings, AccountWindow, Billing, LastReading,
    LengthClass, PlacementBirths, PlacementMoves, PlacementStatus, PoolPlacement, ReadingSource,
    ReadingWindow, RejectionTotals, ReportState, SeatPlacement, Started, UsageVendor,
};
use serde_json::Value;

use super::super::panel_fixtures::{FIXTURE_NOW_MS, fixture_as_of, fixture_config};
use super::super::router_view::AccountCredential;
use super::super::usage_health::AccountReadingHistory;
use super::{AccountSources, AccountsPanel, Panel, SCHEMA_VERSION, build_from_sources};
use crate::commands::doctor::SeatFix;
use crate::commands::pricing::is_subscription;
use crate::server::seat_driver::SeatDriverHealthSnapshot;
use crate::server::usage_poll::{PollStateSnapshot, SeatPollSnapshot};

/// The serialized populated accounts envelope.
pub fn populated_accounts() -> Value {
    serde_json::to_value(populated_accounts_panel()).expect("the accounts panel serializes")
}

/// Every account the fixture config names: the labeled Anthropic seat with a
/// fresh polled window and a stale response window, the unlabeled Codex login
/// with no windows, and the API-key entries with no reading; a paused vendor,
/// nonzero refusal, correction and history counters, both drivers running,
/// and a placement ranking with a ranked and an unavailable seat. The seat
/// is logged out and in two pools, Codex is parked, the API-key entries are
/// in no pool with the Anthropic one usable and the compat one unreadable,
/// and the cloud entry did not build.
pub fn populated_accounts_panel() -> Panel<AccountsPanel> {
    let sources = AccountSources {
        identities: AccountIdentities::from_config(&fixture_config()),
        readings: readings(),
        poll: Some(poll()),
        history: AccountReadingHistory {
            enqueued: 9,
            dropped_full: 1,
            dropped_closed: 1,
            persisted: 7,
            write_errors: 1,
        },
        poll_driver: SeatDriverHealthSnapshot {
            running: true,
            last_sweep_ms: Some(FIXTURE_NOW_MS - 45_000),
            last_good_sweep_ms: Some(FIXTURE_NOW_MS - 300_000),
            sweeps: 7,
            transient: 2,
            definitive: 1,
        },
        refresh_driver: SeatDriverHealthSnapshot {
            running: true,
            last_sweep_ms: Some(FIXTURE_NOW_MS - 20_000),
            last_good_sweep_ms: Some(FIXTURE_NOW_MS - 80_000),
            sweeps: 12,
            transient: 3,
            definitive: 4,
        },
        placement: placement(),
        pools: [
            ("anthropic#a", vec!["backup", "seats"]),
            ("codex", vec!["seats"]),
        ]
        .into_iter()
        .map(|(account, pools)| {
            let pools = pools.into_iter().map(str::to_string).collect();
            (account.to_string(), pools)
        })
        .collect(),
        subscriptions: AccountIdentities::from_config(&fixture_config())
            .iter()
            .map(|(account, identity)| {
                let flag = is_subscription(&fixture_config(), &identity.provider_entry);
                (account.to_string(), flag)
            })
            .collect(),
        credentials: credentials(),
        now_ms: FIXTURE_NOW_MS,
    };
    Panel::available(SCHEMA_VERSION, fixture_as_of(), build_from_sources(sources))
}

/// The seat logged out for 2 hours (a login: a bare command), the compat
/// key unreadable for 5 minutes (a correction, no command), Codex parked for
/// 10 more minutes, the Anthropic API-key entry usable; the cloud entry is
/// absent, as an unbuilt one is.
fn credentials() -> BTreeMap<String, AccountCredential> {
    let config = fixture_config();
    let logged_out = AccountCredential::unusable(
        UnusableCause::NotLoggedIn,
        FIXTURE_NOW_MS - 7_200_000,
        &SeatFix::of(&config, "claude-seat"),
    );
    let unreadable = AccountCredential::unusable(
        UnusableCause::Unreadable,
        FIXTURE_NOW_MS - 300_000,
        &SeatFix::of(&config, "compat"),
    );
    let parked =
        AccountCredential::parked(UpstreamReason::CredentialInvalid, FIXTURE_NOW_MS + 600_000);
    [
        ("anthropic#a", logged_out),
        ("codex", parked),
        ("#anthropic", AccountCredential::usable()),
        ("#compat", unreadable),
    ]
    .into_iter()
    .map(|(account, credential)| (account.to_string(), credential))
    .collect()
}

fn at_ms(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.unsigned_abs())
}

fn readings() -> AccountReadings {
    let polled = AccountWindow {
        window: ReadingWindow {
            length_secs: 18_000,
            class: LengthClass::Short,
            scope: None,
            used: 0.42,
            reset: Some(at_ms(FIXTURE_NOW_MS + 3_600_000)),
            started: Started::Yes,
        },
        source: ReadingSource::Poll,
        observed_at: at_ms(FIXTURE_NOW_MS - 60_000),
        age: Some(Duration::from_mins(1)),
        stale: false,
        kept: false,
    };
    let stale = AccountWindow {
        window: ReadingWindow {
            length_secs: 604_800,
            class: LengthClass::Long,
            scope: Some("opus".to_string()),
            used: 0.9,
            reset: None,
            started: Started::Unknown,
        },
        source: ReadingSource::Response,
        observed_at: at_ms(FIXTURE_NOW_MS - 3_600_000),
        age: None,
        stale: true,
        kept: false,
    };
    AccountReadings {
        accounts: vec![
            AccountReading {
                account: "anthropic#a".to_string(),
                report: ReportState::Windows,
                windows: vec![polled, stale],
                last_accepted: Some(LastReading {
                    observed_at: at_ms(FIXTURE_NOW_MS - 60_000),
                    source: ReadingSource::Poll,
                    fresh: true,
                }),
                billing: Billing::Unknown,
                billing_fresh: false,
            },
            AccountReading {
                account: "codex".to_string(),
                report: ReportState::NoWindows,
                windows: Vec::new(),
                last_accepted: None,
                billing: Billing::Unknown,
                billing_fresh: false,
            },
        ],
        rejections: RejectionTotals {
            invalid_utilization: 1,
            expired_reset: 2,
            implausible_reset: 1,
            overflow: 1,
            contradictory: 1,
            unknown_length: 1,
            invalid_started: 1,
            malformed_body: 3,
        },
        refused_by_admission: 2,
        corrections_confirmed: 3,
    }
}

/// One pool ranked for two upstream models: the labeled Anthropic seat is
/// ranked first for one and breaker-open for the other, whose stale scoped
/// weekly leaves it unread.
fn placement() -> PlacementStatus {
    PlacementStatus {
        births: PlacementBirths {
            roomy: 12,
            low_room: 2,
            unread: 4,
        },
        moves: PlacementMoves {
            credential_unusable: 1,
            account_parked: 2,
            breaker_open: 3,
        },
        cap_forced_moves: 1,
        pools: vec![
            PoolPlacement {
                pool: "claude-pool".to_string(),
                model: "claude-opus-4-7".to_string(),
                seats: vec![SeatPlacement {
                    account: Some("anthropic#a".to_string()),
                    rank: None,
                    tier: "unread",
                    score: None,
                    reason: "breaker_open",
                }],
            },
            PoolPlacement {
                pool: "claude-pool".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                seats: vec![SeatPlacement {
                    account: Some("anthropic#a".to_string()),
                    rank: Some(1),
                    tier: "roomy",
                    score: Some(1.38),
                    reason: "pace",
                }],
            },
        ],
    }
}

/// The Anthropic seat polled after a failed attempt, the Codex login
/// awaiting a fresh login, and Codex paused.
fn poll() -> PollStateSnapshot {
    PollStateSnapshot {
        switched_off: false,
        seats: vec![
            SeatPollSnapshot {
                account: "anthropic#a".to_string(),
                vendor: UsageVendor::Anthropic,
                awaiting_login: false,
                last_attempt_ms: Some(FIXTURE_NOW_MS - 60_000),
                last_result: Some("timeout"),
                next_due_ms: Some(FIXTURE_NOW_MS + 240_000),
                failure_since_ms: Some(FIXTURE_NOW_MS - 120_000),
            },
            SeatPollSnapshot {
                account: "codex".to_string(),
                vendor: UsageVendor::Codex,
                awaiting_login: true,
                last_attempt_ms: None,
                last_result: None,
                next_due_ms: None,
                failure_since_ms: None,
            },
        ],
        paused_until_ms: vec![(UsageVendor::Codex, FIXTURE_NOW_MS + 600_000)],
    }
}
