//! `/status/accounts` panel: every configured credential account with its
//! display identity, its account-meter readings under the stale rule and its
//! background usage poll state, plus the meter's refusal totals, the reading
//! history counters and the usage poll driver's health.
//!
//! Built from memory only: one router snapshot (route targets, managed seats,
//! meter readings), one poll-state snapshot and two counter reads. No usage
//! ledger, no vendor I/O. Every wire type is owned here and mapped from the
//! internal read shapes, so a router-side rename never changes the wire.
//!
//! The account list is the route targets' credential accounts: an OAuth seat,
//! or `#<entry>` for an API-key or cloud credential. A meter reading for an
//! account no target names is never listed. A forwarded target carries the
//! client's credential, so it names no account and is not listed either.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::router::RouteTargetStatus;
use routectl_router::{
    AccountReading, AccountReadings, AccountWindow, LastReading, LengthClass, ObservationStamp,
    ReadingSource, RejectionTotals, ReportState, Started, UsageVendor,
};

use super::health::{SeatDriverRow, map_seat_driver};
use super::router_view::StatusRouterView;
use super::usage_health::AccountReadingHistory;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
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
    /// The reading history writer's running counters.
    history: AccountReadingHistory,
    /// The background usage poll driver's health.
    poll_driver: SeatDriverRow,
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
}

#[derive(Debug, Clone, Copy, Serialize)]
struct LastAcceptedRow {
    observed_at_ms: i64,
    source: &'static str,
}

/// The account's background usage poll state. Every key is always present.
#[derive(Debug, Clone, Copy, Serialize)]
struct PollRow {
    /// `polled` for a managed login the running poll reads, else
    /// `not_polled` (an API-key or cloud credential, a login of a vendor
    /// with no usage endpoint, or a daemon running no poll).
    applicability: &'static str,
    /// `anthropic` or `codex` when polled.
    vendor: Option<&'static str>,
    last_attempt_ms: Option<i64>,
    /// The last poll's outcome token.
    last_result: Option<&'static str>,
    /// When the account is next due, as of the poll's last sweep.
    next_due_ms: Option<i64>,
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
    pub(super) targets: Vec<RouteTargetStatus>,
    /// Each managed login's account and the provider entry it is reported
    /// under.
    pub(super) managed_entries: BTreeMap<String, String>,
    pub(super) readings: AccountReadings,
    /// `None` on a daemon running no usage poll.
    pub(super) poll: Option<PollStateSnapshot>,
    pub(super) history: AccountReadingHistory,
    pub(super) poll_driver: SeatDriverHealthSnapshot,
}

/// One configured account's display identity.
struct Identity {
    provider_entry: String,
    login_label: Option<String>,
}

/// The credential accounts the route targets name. The provider entry of a
/// managed login is the one the seat driver reports it under; any other
/// account takes the smallest entry among its targets, the same rule.
fn configured_accounts(
    targets: &[RouteTargetStatus],
    managed_entries: &BTreeMap<String, String>,
) -> BTreeMap<String, Identity> {
    let mut accounts: BTreeMap<String, Identity> = BTreeMap::new();
    for target in targets {
        let Some(account) = target.account.as_ref() else {
            continue;
        };
        match accounts.get_mut(account) {
            Some(kept) => {
                if target.provider_name < kept.provider_entry {
                    kept.provider_entry.clone_from(&target.provider_name);
                }
                if kept.login_label.is_none() {
                    kept.login_label.clone_from(&target.login_label);
                }
            }
            None => {
                accounts.insert(
                    account.clone(),
                    Identity {
                        provider_entry: target.provider_name.clone(),
                        login_label: target.login_label.clone(),
                    },
                );
            }
        }
    }
    for (account, identity) in &mut accounts {
        if let Some(entry) = managed_entries.get(account) {
            identity.provider_entry.clone_from(entry);
        }
    }
    accounts
}

pub(super) fn build_from_sources(sources: AccountSources) -> AccountsPanel {
    let AccountSources {
        targets,
        managed_entries,
        readings,
        poll,
        history,
        poll_driver,
    } = sources;
    let mut by_account: BTreeMap<String, AccountReading> = readings
        .accounts
        .into_iter()
        .map(|reading| (reading.account.clone(), reading))
        .collect();
    let accounts = configured_accounts(&targets, &managed_entries)
        .into_iter()
        .map(|(account, identity)| {
            let reading = by_account.remove(&account);
            let poll_row = map_poll(poll.as_ref(), &account);
            map_account(account, identity, reading, poll_row)
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
        history,
        poll_driver: map_seat_driver(poll_driver),
    }
}

fn map_account(
    account: String,
    identity: Identity,
    reading: Option<AccountReading>,
    poll: PollRow,
) -> AccountRow {
    let (report, windows, last_accepted) = reading
        .map_or((ReportState::NoReading, Vec::new(), None), |reading| {
            (reading.report, reading.windows, reading.last_accepted)
        });
    AccountRow {
        account,
        provider_entry: identity.provider_entry,
        login_label: identity.login_label,
        report: report_token(report),
        windows: windows.iter().map(map_window).collect(),
        last_accepted: last_accepted.map(map_last_accepted),
        poll,
    }
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
    }
}

fn map_last_accepted(last: LastReading) -> LastAcceptedRow {
    LastAcceptedRow {
        observed_at_ms: epoch_ms(last.observed_at),
        source: source_token(last.source),
    }
}

fn map_poll(poll: Option<&PollStateSnapshot>, account: &str) -> PollRow {
    let vendor = match poll.map(|snapshot| snapshot.applicability(account)) {
        Some(PollApplicability::Polled(vendor)) => Some(vendor),
        Some(PollApplicability::NotPolled) | None => None,
    };
    let seat = poll.and_then(|snapshot| snapshot.seats.iter().find(|seat| seat.account == account));
    PollRow {
        applicability: if vendor.is_some() {
            "polled"
        } else {
            "not_polled"
        },
        vendor: vendor.map(vendor_token),
        last_attempt_ms: seat.and_then(|seat| seat.last_attempt_ms),
        last_result: seat.and_then(|seat| seat.last_result),
        next_due_ms: seat.and_then(|seat| seat.next_due_ms),
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

/// Read every source from one router snapshot and the poll state.
fn read_sources(
    view: &StatusRouterView,
    poll_state: Option<&UsagePollState>,
    history: AccountReadingHistory,
    poll_driver: SeatDriverHealthSnapshot,
) -> AccountSources {
    AccountSources {
        targets: view.route_targets(Instant::now()),
        managed_entries: view.managed_seat_entries(),
        readings: view.account_readings(&ObservationStamp::now()),
        poll: poll_state.map(|state| view.usage_poll_snapshot(state)),
        history,
        poll_driver,
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<AccountsPanel> {
    let view = state.router.view();
    let poll_state = state.daemon_meta.usage_poll_state().cloned();
    let history = state.usage_health.account_reading_history();
    let poll_driver = state.daemon_meta.usage_poll_driver();
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || {
            let sources = read_sources(&view, poll_state.as_ref(), history, poll_driver);
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

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod tests;
