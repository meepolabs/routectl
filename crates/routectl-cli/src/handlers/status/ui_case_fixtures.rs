//! Test-only review cases for the Now and Accounts views: each case is a
//! small config, a seeded ledger and the daemon state a reviewer needs to
//! see, served as the `/status` aggregate and the QUERY answers. Every payload is built by the panels' own builders
//! (`build_from_sources`, the attention list through its recovery memory,
//! the usage and query reads over the seeded ledger), never written as JSON.
//!
//! A case is built at an explicit instant, so the browser tests read it at
//! the fixture instant and the review server at the wall clock. Nothing here
//! reads a config folder, a credentials store or the network: the config is
//! parsed from text and the ledger lives in a temp dir.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use routectl_core::EligibilityReason;
use routectl_core::failure_class::LastOutcome;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::{
    CircuitPhase, CredentialUnusableStatus, ProviderGateStatus, UnusableCause,
};
use routectl_router::{
    AccountIdentities, AccountReading, AccountReadings, AccountWindow, Billing, Config,
    LastReading, LengthClass, PlacementBirths, PlacementMoves, PlacementStatus, PoolEligibility,
    ReadingSource, ReadingWindow, RejectionTotals, ReportState, STALE_AFTER, Started, StateKey,
    UsageVendor,
};
use serde_json::Value;

use super::accounts::{AccountSources, build_from_sources};
use super::attention::{AttentionSources, DriverReads};
use super::router_view::AccountCredential;
use super::usage_health::{AccountReadingHistory, LedgerHealth};
use super::{AggregatePanels, Panel, StatusAggregate, utc_rfc3339};
use crate::commands::doctor::SeatFix;
use crate::commands::pricing::is_subscription;
use crate::commands::usage::WindowFlag;
use crate::server::seat_driver::{SeatDriverHealthSnapshot, managed_seats};
use crate::server::usage_poll::{PollStateSnapshot, SeatPollSnapshot};

const MIN_MS: i64 = 60_000;
const UPSTREAM: &str = "claude-sonnet-4-5";
const SEAT_MODEL: &str = "sonnet";
const FIVE_HOURS: u64 = 18_000;
const SEVEN_DAYS: u64 = 604_800;

/// One review case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCase {
    /// An empty ledger and no subscription login: API-key and cloud entries.
    Empty,
    /// One subscription seat, alone in its pool, with readings and traffic.
    OneSeat,
    /// A five-seat pool, two seats carrying traffic and three idle.
    FiveSeatPool,
    /// A two-seat pool with one seat logged out.
    LoggedOut,
    /// One seat whose usage poll has failed for 15 minutes.
    PollFailing,
}

impl UiCase {
    pub const ALL: [Self; 5] = [
        Self::Empty,
        Self::OneSeat,
        Self::FiveSeatPool,
        Self::LoggedOut,
        Self::PollFailing,
    ];

    /// The case's name on the review server's command line.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::OneSeat => "one-seat",
            Self::FiveSeatPool => "five-seat-pool",
            Self::LoggedOut => "logged-out",
            Self::PollFailing => "poll-failing",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|case| case.token() == token)
    }

    fn layout(self) -> Layout {
        match self {
            Self::Empty => Layout {
                pool: None,
                seats: Vec::new(),
                keyed_entries: true,
            },
            Self::OneSeat => Layout {
                pool: Some("main"),
                seats: vec![SeatSpec {
                    windows: vec![(FIVE_HOURS, 0.42, 1, false), (SEVEN_DAYS, 0.18, 1, false)],
                    requests_hours_ago: vec![1, 2],
                    ..SeatSpec::new("claude", "main")
                }],
                keyed_entries: false,
            },
            Self::FiveSeatPool => Layout {
                pool: Some("seats"),
                seats: vec![
                    SeatSpec {
                        windows: vec![(FIVE_HOURS, 0.42, 1, false)],
                        requests_hours_ago: vec![1, 2, 3],
                        ..SeatSpec::new("seat-1", "s1")
                    },
                    SeatSpec {
                        windows: vec![(FIVE_HOURS, 0.95, 1, false)],
                        requests_hours_ago: vec![1, 4],
                        ..SeatSpec::new("seat-2", "s2")
                    },
                    SeatSpec {
                        windows: vec![(FIVE_HOURS, 0.10, 20, true)],
                        ..SeatSpec::new("seat-3", "s3")
                    },
                    SeatSpec::new("seat-4", "s4"),
                    SeatSpec {
                        windows: vec![(FIVE_HOURS, 0.0, 1, false)],
                        ..SeatSpec::new("seat-5", "s5")
                    },
                ],
                keyed_entries: false,
            },
            Self::LoggedOut => Layout {
                pool: Some("seats"),
                seats: vec![
                    SeatSpec {
                        windows: vec![(FIVE_HOURS, 0.30, 1, false)],
                        requests_hours_ago: vec![1, 2],
                        ..SeatSpec::new("seat-a", "a")
                    },
                    SeatSpec {
                        logged_out_min: Some(40),
                        ..SeatSpec::new("seat-b", "b")
                    },
                ],
                keyed_entries: false,
            },
            Self::PollFailing => Layout {
                pool: Some("main"),
                seats: vec![SeatSpec {
                    windows: vec![(FIVE_HOURS, 0.55, 16, true)],
                    poll_failing_min: Some(15),
                    requests_hours_ago: vec![1],
                    ..SeatSpec::new("claude", "main")
                }],
                keyed_entries: false,
            },
        }
    }
}

/// What a case configures: its seats, all in one pool when it has a pool,
/// and whether it also has the API-key and cloud entries.
struct Layout {
    pool: Option<&'static str>,
    seats: Vec<SeatSpec>,
    keyed_entries: bool,
}

/// One `oauth://anthropic#<label>` seat and the state the daemon holds for
/// it.
struct SeatSpec {
    entry: &'static str,
    label: &'static str,
    /// Minutes since the seat was marked not logged in.
    logged_out_min: Option<i64>,
    /// `(length secs, used, reading age in minutes, stale)` per window.
    windows: Vec<(u64, f64, i64, bool)>,
    /// Minutes the seat's usage polls have been failing.
    poll_failing_min: Option<i64>,
    /// One served request per entry, this many hours ago.
    requests_hours_ago: Vec<i64>,
}

impl SeatSpec {
    const fn new(entry: &'static str, label: &'static str) -> Self {
        Self {
            entry,
            label,
            logged_out_min: None,
            windows: Vec::new(),
            poll_failing_min: None,
            requests_hours_ago: Vec::new(),
        }
    }

    fn account(&self) -> String {
        format!("anthropic#{}", self.label)
    }
}

/// The API-key and cloud entries of the empty case: `(entry, model, block)`.
const KEYED_ENTRIES: [(&str, &str, &str); 3] = [
    (
        "anthropic",
        "sonnet-api",
        "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
         credential = \"env://CASE_ANTHROPIC_KEY\"\nauth = \"x-api-key\"\n",
    ),
    (
        "compat",
        "glm",
        "format = \"openai-chat\"\nbase_url = \"https://compat.example/v1\"\n\
         credential = \"env://CASE_COMPAT_KEY\"\nauth = \"bearer\"\n",
    ),
    (
        "bed",
        "bedclaude",
        "format = \"bedrock-invoke\"\ncloud = { name = \"aws\", region = \"us-east-1\" }\n\
         credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
    ),
];

fn config_text(layout: &Layout) -> String {
    let mut text = format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION);
    for seat in &layout.seats {
        text.push_str(&format!(
            "[providers.{}]\nformat = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\n\
             credential = \"oauth://anthropic#{}\"\nauth = \"bearer\"\n",
            seat.entry, seat.label
        ));
    }
    if let Some(pool) = layout.pool {
        let members: Vec<String> = layout
            .seats
            .iter()
            .map(|seat| format!("\"{}\"", seat.entry))
            .collect();
        text.push_str(&format!(
            "[pools.{pool}]\nmembers = [{}]\n[models.{SEAT_MODEL}]\nprovider = \"{pool}\"\n\
             upstream = \"{UPSTREAM}\"\n",
            members.join(", ")
        ));
    }
    if layout.keyed_entries {
        for (entry, model, block) in KEYED_ENTRIES {
            text.push_str(&format!(
                "[providers.{entry}]\n{block}[models.{model}]\nprovider = \"{entry}\"\n\
                 upstream = \"{UPSTREAM}\"\n"
            ));
        }
    }
    text
}

/// A case with its ledger seeded, ready to answer the page's reads at any
/// instant. The ledger's rows sit their hours before the instant it was
/// opened at.
pub struct CaseSource {
    layout: Layout,
    config: Config,
    listen_addr: String,
    db_path: PathBuf,
    _ledger: tempfile::TempDir,
}

impl CaseSource {
    /// Seed `case`'s ledger relative to `now_ms`. `listen_addr` is the
    /// address the config panel names as the daemon's listener.
    pub fn open(case: UiCase, now_ms: i64, listen_addr: &str) -> Self {
        let layout = case.layout();
        let config: Config = toml::from_str(&config_text(&layout)).expect("the case config parses");
        let ledger = tempfile::tempdir().expect("a temp dir");
        let db_path = ledger.path().join("usage.db");
        let ids: Vec<String> = layout
            .seats
            .iter()
            .flat_map(|seat| {
                seat.requests_hours_ago
                    .iter()
                    .map(move |hours| format!("{}-{hours}h", seat.entry))
            })
            .collect();
        let accounts: Vec<String> = layout.seats.iter().map(SeatSpec::account).collect();
        let mut seeds = Vec::new();
        let mut next_id = ids.iter();
        for (seat, account) in layout.seats.iter().zip(&accounts) {
            for &hours in &seat.requests_hours_ago {
                let id = next_id.next().expect("one id per request");
                seeds.push((
                    id.as_str(),
                    hours,
                    SEAT_MODEL,
                    Some(SEAT_MODEL),
                    Some(seat.entry),
                    Some(account.as_str()),
                    "ok",
                    None,
                ));
            }
        }
        super::query::fixture::seed_ledger(&db_path, &seeds, now_ms);
        Self {
            layout,
            config,
            listen_addr: listen_addr.to_string(),
            db_path,
            _ledger: ledger,
        }
    }

    /// The `/status` aggregate at `now_ms`.
    pub fn aggregate(&self, now_ms: i64) -> Value {
        let state = self.state(now_ms);
        let as_of = utc_rfc3339(now_ms);
        let models = super::models::fixture::models_panel_of(
            &self.config,
            state.targets.clone(),
            state.credentials.clone(),
            now_ms,
        );
        let aggregate = StatusAggregate {
            panels: AggregatePanels {
                usage: super::usage::fixture::usage_panel_over(
                    &self.db_path,
                    WindowFlag::Today,
                    now_ms,
                ),
                health: super::health::fixture::health_panel_for(
                    &self.config,
                    state.targets.clone(),
                    state.pools.clone(),
                    (running_driver(now_ms), running_driver(now_ms)),
                    now_ms,
                ),
                config: super::config::fixture::config_panel_for(
                    &self.config,
                    &self.listen_addr,
                    as_of.clone(),
                ),
                doctor: super::doctor::fixture::clean_doctor_panel(
                    self.config.clone(),
                    state.targets.clone(),
                    as_of.clone(),
                ),
                accounts: Panel::available(
                    super::accounts::SCHEMA_VERSION,
                    as_of,
                    build_from_sources(self.account_sources(&state, now_ms)),
                ),
                attention: super::attention::fixture::attention_panel_of(
                    self.attention_sources(state, now_ms),
                ),
                models,
            },
        };
        serde_json::to_value(aggregate).expect("the aggregate serializes")
    }

    /// The route's answer to a QUERY `body` at `now_ms` (windows in UTC), or
    /// `None` for a body outside its vocabulary.
    pub fn query(&self, body: &[u8], now_ms: i64) -> Option<Value> {
        let panel =
            super::query::fixture::query_answer(&self.db_path, self.config.clone(), body, now_ms)?;
        Some(serde_json::to_value(panel).expect("the query panel serializes"))
    }

    fn state(&self, now_ms: i64) -> CaseState {
        let mut state = CaseState::default();
        for seat in &self.layout.seats {
            self.add_seat(&mut state, seat, now_ms);
        }
        if let Some(pool) = self.layout.pool {
            let out = self
                .layout
                .seats
                .iter()
                .filter(|seat| seat.logged_out_min.is_some())
                .count();
            let configured = self.layout.seats.len();
            state.pools.push(PoolEligibility {
                pool: pool.to_string(),
                configured,
                eligible: configured - out,
                by_reason: if out == 0 {
                    Vec::new()
                } else {
                    vec![(EligibilityReason::CredentialUnusable, out)]
                },
            });
        }
        if self.layout.keyed_entries {
            for (entry, model, _) in KEYED_ENTRIES {
                let account = format!("#{entry}");
                state
                    .targets
                    .push(target(model, entry, None, &account, closed(false)));
                state
                    .credentials
                    .insert(account, AccountCredential::usable());
            }
        }
        state
    }

    fn add_seat(&self, state: &mut CaseState, seat: &SeatSpec, now_ms: i64) {
        let account = seat.account();
        let served = !seat.requests_hours_ago.is_empty();
        let gate = seat.logged_out_min.map_or_else(
            || closed(served),
            |minutes| ProviderGateStatus {
                reason: Some(EligibilityReason::CredentialUnusable),
                credential_unusable: Some(CredentialUnusableStatus {
                    cause: UnusableCause::NotLoggedIn,
                    since_elapsed: Duration::from_mins(minutes.unsigned_abs()),
                }),
                ..closed(served)
            },
        );
        state.targets.push(target(
            &format!("{SEAT_MODEL}#{}", seat.entry),
            seat.entry,
            Some(seat.label),
            &account,
            gate,
        ));
        let credential = seat
            .logged_out_min
            .map_or_else(AccountCredential::usable, |minutes| {
                AccountCredential::unusable(
                    UnusableCause::NotLoggedIn,
                    now_ms - minutes * MIN_MS,
                    &SeatFix::of(&self.config, seat.entry),
                )
            });
        state.credentials.insert(account.clone(), credential);
        if let Some(pool) = self.layout.pool {
            state
                .account_pools
                .insert(account.clone(), vec![pool.to_string()]);
        }
        if let Some(reading) = reading(seat, now_ms) {
            state.readings.push(reading);
        }
        state.poll.push(SeatPollSnapshot {
            account,
            vendor: UsageVendor::Anthropic,
            awaiting_login: seat.logged_out_min.is_some(),
            last_attempt_ms: seat.logged_out_min.is_none().then_some(now_ms - MIN_MS),
            last_result: match (seat.logged_out_min, seat.poll_failing_min) {
                (Some(_), _) => None,
                (None, Some(_)) => Some("http"),
                (None, None) => Some("accepted"),
            },
            next_due_ms: Some(now_ms + 4 * MIN_MS),
            failure_since_ms: seat
                .poll_failing_min
                .map(|minutes| now_ms - minutes * MIN_MS),
        });
    }

    fn account_sources(&self, state: &CaseState, now_ms: i64) -> AccountSources {
        let identities = AccountIdentities::from_config(&self.config);
        let subscriptions = identities
            .iter()
            .map(|(account, identity)| {
                let flag = is_subscription(&self.config, &identity.provider_entry);
                (account.to_string(), flag)
            })
            .collect();
        AccountSources {
            identities,
            readings: readings(state.readings.clone()),
            poll: Some(poll_snapshot(state.poll.clone())),
            history: AccountReadingHistory::default(),
            poll_driver: running_driver(now_ms),
            refresh_driver: running_driver(now_ms),
            placement: no_placement(),
            pools: state.account_pools.clone(),
            subscriptions,
            credentials: state.credentials.clone(),
            now_ms,
        }
    }

    fn attention_sources(&self, state: CaseState, now_ms: i64) -> AttentionSources {
        let identities = AccountIdentities::from_config(&self.config);
        let fixes = identities
            .iter()
            .map(|(account, identity)| {
                let fix = SeatFix::of(&self.config, &identity.provider_entry);
                (account.to_string(), fix)
            })
            .collect();
        AttentionSources {
            now_ms,
            pools: state.pools,
            targets: state.targets,
            identities,
            credentials: state.credentials,
            fixes,
            readings: readings(state.readings),
            poll: Some(poll_snapshot(state.poll)),
            refresh_failures: BTreeMap::new(),
            drivers: DriverReads {
                refresh: running_driver(now_ms),
                poll: running_driver(now_ms),
                uptime: Duration::from_hours(2),
            },
            managed_accounts: managed_seats(&self.config).len(),
            record_loss: LedgerHealth::default(),
            doctor_findings: Some(Vec::new()),
        }
    }
}

/// The router-side state one read of a case copies out, before either
/// panel's builder runs.
#[derive(Default)]
struct CaseState {
    targets: Vec<RouteTargetStatus>,
    pools: Vec<PoolEligibility>,
    account_pools: BTreeMap<String, Vec<String>>,
    credentials: BTreeMap<String, AccountCredential>,
    readings: Vec<AccountReading>,
    poll: Vec<SeatPollSnapshot>,
}

fn target(
    state_key: &str,
    entry: &str,
    login_label: Option<&str>,
    account: &str,
    gate: ProviderGateStatus,
) -> RouteTargetStatus {
    RouteTargetStatus {
        state_key: state_key.to_string(),
        nickname: state_key.split('#').next().unwrap_or(state_key).to_string(),
        provider_name: entry.to_string(),
        upstream: UPSTREAM.to_string(),
        seat_label: login_label.map(|_| entry.to_string()),
        account: Some(account.to_string()),
        login_label: login_label.map(str::to_string),
        learned_lane: StateKey::parse(&format!("{entry}#{UPSTREAM}")),
        gate,
    }
}

/// A closed, dispatchable gate; `served` targets settled an ok request a
/// minute ago, the rest have settled nothing yet.
fn closed(served: bool) -> ProviderGateStatus {
    ProviderGateStatus {
        reason: None,
        rpm_available: None,
        circuit: CircuitPhase::Closed,
        half_open_probe_in_flight: false,
        circuit_open_elapsed: None,
        last_outcome: served.then_some(LastOutcome::Ok),
        last_outcome_elapsed: served.then_some(Duration::from_mins(1)),
        credential_park: None,
        credential_unusable: None,
    }
}

fn at_ms(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.unsigned_abs())
}

/// The seat's meter reading, or `None` for a seat nothing was read for. A
/// window at zero has not started, so it has no reset.
fn reading(seat: &SeatSpec, now_ms: i64) -> Option<AccountReading> {
    if seat.windows.is_empty() {
        return None;
    }
    let windows: Vec<AccountWindow> = seat
        .windows
        .iter()
        .map(|&(length_secs, used, age_min, stale)| {
            let observed_ms = now_ms - age_min * MIN_MS;
            let started = used > 0.0;
            AccountWindow {
                window: ReadingWindow {
                    length_secs,
                    class: if length_secs <= 86_400 {
                        LengthClass::Short
                    } else {
                        LengthClass::Long
                    },
                    scope: None,
                    used,
                    reset: started.then(|| at_ms(now_ms + (length_secs as i64) * 500)),
                    started: if started { Started::Yes } else { Started::No },
                },
                source: ReadingSource::Poll,
                observed_at: at_ms(observed_ms),
                age: Some(Duration::from_mins(age_min.unsigned_abs())),
                stale,
                kept: false,
            }
        })
        .collect();
    let newest_ms = now_ms - seat.windows.iter().map(|w| w.2).min().unwrap_or(0) * MIN_MS;
    Some(AccountReading {
        account: seat.account(),
        report: ReportState::Windows,
        windows,
        last_accepted: Some(LastReading {
            observed_at: at_ms(newest_ms),
            source: ReadingSource::Poll,
            fresh: Duration::from_millis((now_ms - newest_ms).unsigned_abs()) <= STALE_AFTER,
        }),
        billing: Billing::Unknown,
        billing_fresh: false,
    })
}

const fn readings(accounts: Vec<AccountReading>) -> AccountReadings {
    AccountReadings {
        accounts,
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

/// No pool placed on yet: the cases exercise no ranking.
fn no_placement() -> PlacementStatus {
    PlacementStatus {
        births: PlacementBirths::default(),
        moves: PlacementMoves::default(),
        cap_forced_moves: 0,
        pools: Vec::new(),
    }
}

const fn poll_snapshot(seats: Vec<SeatPollSnapshot>) -> PollStateSnapshot {
    PollStateSnapshot {
        seats,
        paused_until_ms: Vec::new(),
        switched_off: false,
    }
}

/// A seat driver that swept clean a minute ago.
const fn running_driver(now_ms: i64) -> SeatDriverHealthSnapshot {
    SeatDriverHealthSnapshot {
        running: true,
        last_sweep_ms: Some(now_ms - MIN_MS),
        last_good_sweep_ms: Some(now_ms - MIN_MS),
        sweeps: 24,
        transient: 0,
        definitive: 0,
    }
}

#[cfg(test)]
#[path = "ui_case_fixtures_tests.rs"]
mod tests;
