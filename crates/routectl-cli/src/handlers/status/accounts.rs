//! `/status/accounts` panel: every configured credential account with its
//! display identity, its account-meter readings under the stale rule and its
//! background usage poll state, its pools, its billing kind and its
//! credential state, plus the meter's refusal and correction totals, the
//! reading history counters, the refresh and usage poll drivers' health and
//! the pace placement rule's counters and seat ranking.
//!
//! Built from memory only: one router snapshot (account identities, meter
//! readings, placement ranking, the last two at one instant), one poll-state
//! snapshot and two counter reads. No usage
//! ledger, no vendor I/O. Every wire type is owned here and mapped from the
//! internal read shapes, so a router-side rename never changes the wire.
//!
//! The account list is the served config's credential accounts, the same set
//! the usage poll and the CLI read: an OAuth seat, or `#<entry>` for an
//! API-key or cloud credential, listed whether or not its provider built. A
//! meter reading for an account the config does not name is never listed. A
//! forwarded entry carries the client's credential, so it names no account
//! and is not listed either.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::{
    AccountIdentities, AccountIdentity, AccountReading, AccountReadings, AccountWindow,
    LastReading, LengthClass, ObservationStamp, PlacementStatus, ReadingSource, RejectionTotals,
    ReportState, Started, UsageVendor,
};

use super::router_view::{AccountCredential, StatusRouterView};
use super::usage_health::AccountReadingHistory;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
use crate::commands::doctor::attention::credential_cause;
use crate::server::seat_driver::SeatDriverHealthSnapshot;
use crate::server::usage_poll::{PollApplicability, PollStateSnapshot, UsagePollState};

/// Wire-shape version of the accounts panel payload. A removed or renamed
/// field, or a field whose meaning changes, bumps it, and the dashboard's
/// expected version moves with it in the same change. An added field does not
/// bump: readers ignore keys they do not know.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub(super) struct AccountsPanel {
    /// One row per configured account, in account order.
    accounts: Vec<AccountRow>,
    /// Each usage-poll vendor paused right now, and when its pause ends.
    vendor_pauses: Vec<VendorPause>,
    /// The meter's running refusal totals, by reason.
    rejections: Rejections,
    /// Meter writes refused because their account is not configured.
    refused_by_admission: u64,
    /// Lower readings the meter accepted after enough agreeing repeats.
    corrections_confirmed: u64,
    /// The reading history writer's running counters.
    history: AccountReadingHistory,
    /// The background usage poll driver's health.
    poll_driver: DriverRow,
    /// The background credential refresh driver's health.
    refresh_driver: DriverRow,
    /// The pace rule's counters and its per-pool, per-model seat ranking.
    placement: placement::PlacementRow,
}

/// One configured account. `account` is the key other panels join on;
/// `provider_entry` and `login_label` are display fields carried separately
/// so no reader parses the key.
#[derive(Debug, Clone, Serialize)]
struct AccountRow {
    account: String,
    /// The `[providers]` entry the account is reported under: for a login
    /// several entries name, the lexicographically smallest of them.
    provider_entry: String,
    /// The `<label>` of an `oauth://<provider>#<label>` credential; null for
    /// an unlabeled login and any other credential.
    login_label: Option<String>,
    /// `no_reading`, `windows` or `no_windows`.
    report: &'static str,
    /// Every held window, ordered by length then scope.
    windows: Vec<WindowRow>,
    /// The latest accepted reading, of any source.
    last_accepted: Option<LastAcceptedRow>,
    poll: PollRow,
    /// The `[pools]` keys whose members dispatch on this account, sorted.
    pools: Vec<String>,
    /// Whether the account bills as a managed-OAuth subscription.
    subscription: bool,
    credential: CredentialRow,
}

/// Whether the account's credential can be dispatched on now. Every key is
/// always present.
#[derive(Debug, Clone, Serialize)]
struct CredentialRow {
    /// `usable`, `unusable`, `parked`, or `not_built` for an account no
    /// built dispatch target names (its provider failed to build), about
    /// which the router knows nothing.
    state: &'static str,
    /// The plain-words cause with its age, worded as the needs-attention
    /// list words it; null for `usable` and `not_built`.
    cause: Option<String>,
    /// Epoch ms the account was first marked unusable.
    since_ms: Option<i64>,
    /// Epoch ms a park lapses into a re-test.
    until_ms: Option<i64>,
    /// The bare command that readmits an unusable account, copyable as is.
    fix: Option<String>,
    /// What readmits an unusable account in plain words when no bare
    /// command does.
    remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WindowRow {
    /// `short` (at most 24 hours) or `long`.
    class: &'static str,
    length_secs: u64,
    /// The vendor scope tag; null for an account-wide window.
    scope: Option<String>,
    /// Used fraction, in `[0, 1]`.
    used: f64,
    /// Unused fraction; null when the reading is stale and so gives no room.
    remaining: Option<f64>,
    /// Epoch ms the window resets; for an unstarted window, where it would
    /// end if it began at the observation.
    reset_at_ms: Option<i64>,
    /// `yes`, `no` or `unknown`.
    started: &'static str,
    /// `response` or `poll`.
    source: &'static str,
    observed_at_ms: i64,
    /// The reading's age on the older of its two clocks; null when its
    /// stamp cannot be ordered against the read instant.
    age_ms: Option<i64>,
    stale: bool,
    /// True for a running window a complete poll omitted that the meter
    /// still keeps; it binds placement for as long as the meter keeps it.
    kept: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct LastAcceptedRow {
    observed_at_ms: i64,
    source: &'static str,
}

/// The account's background usage poll state. Every key is always present.
#[derive(Debug, Clone, Copy, Serialize)]
struct PollRow {
    /// `polled` for a managed login the running poll reads,
    /// `awaiting_login` for one it skips until a fresh login, `off` for one
    /// it would read with `[usage] poll = false`, else `not_polled` (an
    /// API-key or cloud credential, a login of a vendor with no usage
    /// endpoint, or a daemon running no poll).
    applicability: &'static str,
    /// `anthropic` or `codex` when polled, awaiting a login or off.
    vendor: Option<&'static str>,
    last_attempt_ms: Option<i64>,
    /// The last poll's outcome token.
    last_result: Option<&'static str>,
    /// When the account is next due, as of the poll's last sweep.
    next_due_ms: Option<i64>,
    /// When the account's current run of failed polls began; null before a
    /// failure and once a poll, or a newer reading from any source, is
    /// accepted.
    failure_since_ms: Option<i64>,
}

/// A background seat driver's health. Owned here so the health panel's
/// driver rows can change under their own version without moving this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct DriverRow {
    running: bool,
    last_sweep_ms: Option<i64>,
    last_good_sweep_ms: Option<i64>,
    sweeps: u64,
    transient: u64,
    definitive: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct VendorPause {
    vendor: &'static str,
    paused_until_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Rejections {
    invalid_utilization: u64,
    expired_reset: u64,
    implausible_reset: u64,
    overflow: u64,
    contradictory: u64,
    unknown_length: u64,
    invalid_started: u64,
    malformed_body: u64,
}

/// Everything the panel is built from, read once.
pub(super) struct AccountSources {
    /// Every configured account with its display identity.
    pub(super) identities: AccountIdentities,
    pub(super) readings: AccountReadings,
    /// `None` on a daemon running no usage poll.
    pub(super) poll: Option<PollStateSnapshot>,
    pub(super) history: AccountReadingHistory,
    pub(super) poll_driver: SeatDriverHealthSnapshot,
    pub(super) refresh_driver: SeatDriverHealthSnapshot,
    /// Read at the same instant as `readings`.
    pub(super) placement: PlacementStatus,
    /// Each configured account's pools; an absent account is in none.
    pub(super) pools: BTreeMap<String, Vec<String>>,
    /// Each configured account's subscription flag; absent reads false.
    pub(super) subscriptions: BTreeMap<String, bool>,
    /// The credential state of each account a built target names.
    pub(super) credentials: BTreeMap<String, AccountCredential>,
    /// The epoch-ms instant the credential states were read at.
    pub(super) now_ms: i64,
}

pub(super) fn build_from_sources(sources: AccountSources) -> AccountsPanel {
    let AccountSources {
        identities,
        readings,
        poll,
        history,
        poll_driver,
        refresh_driver,
        placement,
        mut pools,
        subscriptions,
        credentials,
        now_ms,
    } = sources;
    let mut by_account: BTreeMap<String, AccountReading> = readings
        .accounts
        .into_iter()
        .map(|reading| (reading.account.clone(), reading))
        .collect();
    let accounts = identities
        .iter()
        .map(|(account, identity)| {
            let reading = by_account.remove(account);
            let standing = Standing {
                poll: map_poll(poll.as_ref(), account),
                pools: pools.remove(account).unwrap_or_default(),
                subscription: subscriptions.get(account).copied().unwrap_or(false),
                credential: map_credential(credentials.get(account), now_ms),
            };
            map_account(account, identity, reading, standing)
        })
        .collect();
    AccountsPanel {
        accounts,
        vendor_pauses: poll
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .paused_until_ms
                    .iter()
                    .map(|&(vendor, until)| VendorPause {
                        vendor: vendor_token(vendor),
                        paused_until_ms: until,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        rejections: map_rejections(readings.rejections),
        refused_by_admission: readings.refused_by_admission,
        corrections_confirmed: readings.corrections_confirmed,
        history,
        poll_driver: map_driver(poll_driver),
        refresh_driver: map_driver(refresh_driver),
        placement: placement::map_placement(placement),
    }
}

/// An account's row fields that do not come from its meter reading.
struct Standing {
    poll: PollRow,
    pools: Vec<String>,
    subscription: bool,
    credential: CredentialRow,
}

fn map_account(
    account: &str,
    identity: &AccountIdentity,
    reading: Option<AccountReading>,
    standing: Standing,
) -> AccountRow {
    let (report, windows, last_accepted) = reading
        .map_or((ReportState::NoReading, Vec::new(), None), |reading| {
            (reading.report, reading.windows, reading.last_accepted)
        });
    AccountRow {
        account: account.to_owned(),
        provider_entry: identity.provider_entry.clone(),
        login_label: identity.login_label.clone(),
        report: report_token(report),
        windows: windows.iter().map(map_window).collect(),
        last_accepted: last_accepted.map(map_last_accepted),
        poll: standing.poll,
        pools: standing.pools,
        subscription: standing.subscription,
        credential: standing.credential,
    }
}

/// The wire form of an account's credential state; `None` is an account no
/// built target names.
fn map_credential(credential: Option<&AccountCredential>, now_ms: i64) -> CredentialRow {
    credential.map_or(
        CredentialRow {
            state: "not_built",
            cause: None,
            since_ms: None,
            until_ms: None,
            fix: None,
            remediation: None,
        },
        |credential| CredentialRow {
            state: credential.state().as_str(),
            cause: credential_cause(&credential.condition, now_ms),
            since_ms: credential.since_ms(),
            until_ms: credential.until_ms(),
            fix: credential.fix.clone(),
            remediation: credential.remediation.clone(),
        },
    )
}

fn map_window(window: &AccountWindow) -> WindowRow {
    WindowRow {
        class: class_token(window.window.class),
        length_secs: window.window.length_secs,
        scope: window.window.scope.clone(),
        used: window.window.used,
        remaining: window.remaining(),
        reset_at_ms: window.window.reset.map(epoch_ms),
        started: started_token(window.window.started),
        source: source_token(window.source),
        observed_at_ms: epoch_ms(window.observed_at),
        age_ms: window.age.map(duration_ms),
        stale: window.stale,
        kept: window.kept,
    }
}

fn map_last_accepted(last: LastReading) -> LastAcceptedRow {
    LastAcceptedRow {
        observed_at_ms: epoch_ms(last.observed_at),
        source: source_token(last.source),
    }
}

fn map_poll(poll: Option<&PollStateSnapshot>, account: &str) -> PollRow {
    let applicability = poll.map_or(PollApplicability::NotPolled, |snapshot| {
        snapshot.applicability(account)
    });
    let vendor = match applicability {
        PollApplicability::Polled(vendor)
        | PollApplicability::AwaitingLogin(vendor)
        | PollApplicability::SwitchedOff(vendor) => Some(vendor),
        PollApplicability::NotPolled => None,
    };
    let seat = poll.and_then(|snapshot| snapshot.seats.iter().find(|seat| seat.account == account));
    PollRow {
        applicability: applicability_token(applicability),
        vendor: vendor.map(vendor_token),
        last_attempt_ms: seat.and_then(|seat| seat.last_attempt_ms),
        last_result: seat.and_then(|seat| seat.last_result),
        next_due_ms: seat.and_then(|seat| seat.next_due_ms),
        failure_since_ms: seat.and_then(|seat| seat.failure_since_ms),
    }
}

const fn applicability_token(applicability: PollApplicability) -> &'static str {
    match applicability {
        PollApplicability::Polled(_) => "polled",
        PollApplicability::AwaitingLogin(_) => "awaiting_login",
        PollApplicability::SwitchedOff(_) => "off",
        PollApplicability::NotPolled => "not_polled",
    }
}

const fn map_driver(health: SeatDriverHealthSnapshot) -> DriverRow {
    DriverRow {
        running: health.running,
        last_sweep_ms: health.last_sweep_ms,
        last_good_sweep_ms: health.last_good_sweep_ms,
        sweeps: health.sweeps,
        transient: health.transient,
        definitive: health.definitive,
    }
}

const fn map_rejections(totals: RejectionTotals) -> Rejections {
    Rejections {
        invalid_utilization: totals.invalid_utilization,
        expired_reset: totals.expired_reset,
        implausible_reset: totals.implausible_reset,
        overflow: totals.overflow,
        contradictory: totals.contradictory,
        unknown_length: totals.unknown_length,
        invalid_started: totals.invalid_started,
        malformed_body: totals.malformed_body,
    }
}

const fn report_token(report: ReportState) -> &'static str {
    match report {
        ReportState::NoReading => "no_reading",
        ReportState::Windows => "windows",
        ReportState::NoWindows => "no_windows",
    }
}

const fn class_token(class: LengthClass) -> &'static str {
    match class {
        LengthClass::Short => "short",
        LengthClass::Long => "long",
    }
}

const fn started_token(started: Started) -> &'static str {
    match started {
        Started::Yes => "yes",
        Started::No => "no",
        Started::Unknown => "unknown",
    }
}

const fn source_token(source: ReadingSource) -> &'static str {
    match source {
        ReadingSource::Response => "response",
        ReadingSource::Poll => "poll",
    }
}

const fn vendor_token(vendor: UsageVendor) -> &'static str {
    match vendor {
        UsageVendor::Anthropic => "anthropic",
        UsageVendor::Codex => "codex",
    }
}

/// `at` in epoch milliseconds, negative before the epoch, saturating.
fn epoch_ms(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => duration_ms(after),
        Err(before) => duration_ms(before.duration()).saturating_neg(),
    }
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Both seat drivers' health, read once each.
struct DriverHealths {
    poll: SeatDriverHealthSnapshot,
    refresh: SeatDriverHealthSnapshot,
}

/// Read every source from one router snapshot and the poll state, the
/// readings and the placement ranking at one instant.
fn read_sources(
    view: &StatusRouterView,
    poll_state: Option<&UsagePollState>,
    history: AccountReadingHistory,
    drivers: DriverHealths,
) -> AccountSources {
    let now = ObservationStamp::now();
    let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    AccountSources {
        identities: view.account_identities(),
        readings: view.account_readings(&now),
        poll: poll_state.map(|state| view.usage_poll_snapshot(state)),
        history,
        poll_driver: drivers.poll,
        refresh_driver: drivers.refresh,
        placement: view.placement_status(&now),
        pools: view.account_pools(),
        subscriptions: view.account_subscriptions(),
        credentials: view.account_credentials((Instant::now(), now_ms)),
        now_ms,
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<AccountsPanel> {
    let view = state.router.view();
    let poll_state = state.daemon_meta.usage_poll_state().cloned();
    let history = state.usage_health.account_reading_history();
    let drivers = DriverHealths {
        poll: state.daemon_meta.usage_poll_driver(),
        refresh: state.daemon_meta.seat_driver(),
    };
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || {
            let sources = read_sources(&view, poll_state.as_ref(), history, drivers);
            Panel::available(SCHEMA_VERSION, as_of, build_from_sources(sources))
        },
    )
    .await;
    state.observability.accounts.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<AccountsPanel>> {
    Json(build(&state).await)
}

mod placement;

#[cfg(test)]
pub(super) mod fixture;

#[cfg(test)]
#[path = "accounts_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "accounts_unbuilt_tests.rs"]
mod unbuilt_tests;

#[cfg(test)]
#[path = "accounts_poll_off_tests.rs"]
mod poll_off_tests;

#[cfg(test)]
#[path = "accounts_placement_tests.rs"]
mod placement_tests;

#[cfg(test)]
#[path = "accounts_fields_tests.rs"]
mod fields_tests;
