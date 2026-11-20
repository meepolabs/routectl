//! `/status/attention` panel: the needs-attention list, with its verdict,
//! counts and how complete its doctor input was.
//!
//! One read gathers plain snapshots from one pinned router snapshot (pools,
//! dispatch targets, credential marks, account readings), the daemon facts
//! (both seat drivers' health, the refresh driver's current failures, the
//! usage poll state), the request-row loss counters and the Fail findings of
//! a no-network doctor report. The pure builder turns them into items, and
//! the per-daemon [`AttentionMemory`] adds first-seen ages and recovered
//! items. Every condition, severity and wording comes from the builder; this
//! module only copies state across. Nothing here writes to the router, the
//! ledger or a counter.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::CircuitPhase;
use routectl_router::{
    AccountIdentities, AccountIdentity, AccountReading, AccountReadings, AccountWindow, Billing,
    Finding, ObservationStamp, PoolEligibility,
};
use serde::Serialize;

use super::attention_memory::{AttentionMemory, DoctorCoverage};
use super::router_view::{AccountCredential, StatusRouterView};
use super::usage_health::LedgerHealth;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, utc_rfc3339};
use crate::commands::doctor::attention::{
    AccountSnapshot, AttentionInputs, AttentionItem, Counts, CredentialState, DriverSnapshot,
    PollFailure, PoolSnapshot, RecordLoss, RouteSnapshot, Verdict, WindowSnapshot,
};
use crate::server::seat_driver::{RefreshFailure, SeatDriverHealthSnapshot};
use crate::server::usage_poll::{PollStateSnapshot, UsagePollState};

/// Wire-shape version of the attention panel payload. A removed field, or a
/// field or token whose meaning changes, bumps it; an added field or kind
/// does not.
pub const SCHEMA_VERSION: u32 = 1;

/// How long after daemon start a seat driver that has never swept still
/// counts as starting rather than stopped. Its task marks itself running as
/// its first step, so this only absorbs task start-up on a loaded boot.
const DRIVER_START_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub(super) struct AttentionPanel {
    verdict: Verdict,
    counts: Counts,
    coverage: Coverage,
    /// Active items first, worst first, then recovered ones.
    items: Vec<AttentionItem>,
}

/// Which inputs this read had.
#[derive(Debug, Clone, Copy, Serialize)]
struct Coverage {
    /// `current` when the doctor report was built for this read,
    /// `unavailable` when none could be; earlier doctor items are then held.
    doctor: &'static str,
}

/// The per-daemon state behind the panel: the recovery memory and the
/// instant the status surface started, which the driver start-up grace is
/// measured from.
#[derive(Debug)]
pub(super) struct AttentionState {
    memory: AttentionMemory,
    started: Instant,
}

impl AttentionState {
    /// The state of a daemon starting now.
    pub(super) fn new() -> Self {
        Self::started_at(Instant::now())
    }

    /// The state of a daemon that started at `started`.
    pub(super) fn started_at(started: Instant) -> Self {
        Self {
            memory: AttentionMemory::new(),
            started,
        }
    }

    fn build(&self, sources: AttentionSources) -> AttentionPanel {
        let coverage = match sources.doctor_findings {
            Some(_) => DoctorCoverage::Current,
            None => DoctorCoverage::Unavailable,
        };
        let attention = self.memory.build(inputs_from(sources), coverage);
        AttentionPanel {
            verdict: attention.verdict,
            counts: attention.counts,
            coverage: Coverage {
                doctor: coverage_token(coverage),
            },
            items: attention.items,
        }
    }
}

const fn coverage_token(coverage: DoctorCoverage) -> &'static str {
    match coverage {
        DoctorCoverage::Current => "current",
        DoctorCoverage::Unavailable => "unavailable",
    }
}

/// Everything one read is built from, copied out of the daemon at one
/// pinned instant.
pub(super) struct AttentionSources {
    pub(super) now_ms: i64,
    pub(super) pools: Vec<PoolEligibility>,
    pub(super) targets: Vec<RouteTargetStatus>,
    /// Every configured account with its display identity.
    pub(super) identities: AccountIdentities,
    /// The credential state of each account a built target names.
    pub(super) credentials: BTreeMap<String, AccountCredential>,
    /// The line that readmits each configured account's credential.
    pub(super) fixes: BTreeMap<String, String>,
    pub(super) readings: AccountReadings,
    /// `None` on a daemon running no usage poll.
    pub(super) poll: Option<PollStateSnapshot>,
    pub(super) refresh_failures: BTreeMap<String, RefreshFailure>,
    pub(super) drivers: DriverReads,
    pub(super) managed_accounts: usize,
    pub(super) record_loss: LedgerHealth,
    /// The doctor report's Fail findings; `None` when no report was built.
    pub(super) doctor_findings: Option<Vec<Finding>>,
}

/// Both seat drivers' health and how long the status surface has been up.
#[derive(Debug, Clone, Copy)]
pub(super) struct DriverReads {
    pub(super) refresh: SeatDriverHealthSnapshot,
    pub(super) poll: SeatDriverHealthSnapshot,
    pub(super) uptime: Duration,
}

/// The builder's inputs for `sources`. One account snapshot per configured
/// account, however many routes or pools name it.
fn inputs_from(sources: AttentionSources) -> AttentionInputs {
    let AttentionSources {
        now_ms,
        pools,
        targets,
        identities,
        credentials,
        fixes,
        readings,
        poll,
        refresh_failures,
        drivers,
        managed_accounts,
        record_loss,
        doctor_findings,
    } = sources;
    let mut by_account: BTreeMap<String, AccountReading> = readings
        .accounts
        .into_iter()
        .map(|reading| (reading.account.clone(), reading))
        .collect();
    let accounts = identities
        .iter()
        .map(|(account, identity)| {
            let (windows, billing) = reading_parts(by_account.remove(account));
            AccountSnapshot {
                account: account.to_string(),
                display: display_of(identity),
                credential: credential_state(credentials.get(account)),
                fix: fixes.get(account).cloned(),
                refresh_failure: refresh_failures.get(account).copied(),
                poll_failure: poll_failure(poll.as_ref(), account),
                windows,
                billing,
            }
        })
        .collect();
    AttentionInputs {
        now_ms,
        pools: pools.into_iter().map(pool_snapshot).collect(),
        routes: targets
            .into_iter()
            .map(|target| route_snapshot(target, now_ms))
            .collect(),
        accounts,
        drivers: driver_snapshot(drivers),
        managed_accounts,
        record_loss: RecordLoss {
            dropped_full: record_loss.dropped_full,
            dropped_closed: record_loss.dropped_closed,
            write_errors: record_loss.write_errors,
            last_rise_ms: None,
        },
        doctor_findings: doctor_findings.unwrap_or_default(),
    }
}

fn pool_snapshot(pool: PoolEligibility) -> PoolSnapshot {
    PoolSnapshot {
        pool: pool.pool,
        configured: pool.configured,
        eligible: pool.eligible,
    }
}

/// A target's circuit as the list reads it. The router's `Open` phase covers
/// both the cooldown and a half-open probe in flight: either way every request
/// but the probe is refused, so both are open. `HalfOpenReady` admits the
/// next request as the probe, so it is not.
fn route_snapshot(target: RouteTargetStatus, now_ms: i64) -> RouteSnapshot {
    let circuit_open = target.gate.circuit == CircuitPhase::Open;
    RouteSnapshot {
        open_since_ms: target
            .gate
            .circuit_open_elapsed
            .filter(|_| circuit_open)
            .map(|elapsed| now_ms.saturating_sub(duration_ms(elapsed))),
        state_key: target.state_key,
        account: target.account,
        circuit_open,
    }
}

/// An account's credential as the builder reads it. An account no built
/// target names carries no mark the router could hold, so nothing is known
/// against it: it raises no credential item, and its refresh, poll and
/// window conditions are still listed.
fn credential_state(credential: Option<&AccountCredential>) -> CredentialState {
    credential.map_or(CredentialState::Usable, |credential| credential.condition)
}

/// `provider_entry (login_label)`, or the bare entry with no label.
fn display_of(identity: &AccountIdentity) -> String {
    match &identity.login_label {
        Some(label) => format!("{} ({label})", identity.provider_entry),
        None => identity.provider_entry.clone(),
    }
}

/// The account's current run of failed usage polls. Only a recorded run
/// counts: an account the poll skips, does not read or has switched off has
/// no run, so it has no poll failure.
fn poll_failure(poll: Option<&PollStateSnapshot>, account: &str) -> Option<PollFailure> {
    let seat = poll?.seats.iter().find(|seat| seat.account == account)?;
    Some(PollFailure {
        since_ms: seat.failure_since_ms?,
        last_result: seat.last_result.map(str::to_string),
    })
}

/// The reading-derived half of an account snapshot: its windows and billing
/// state. An account with no reading has no windows and unknown billing.
fn reading_parts(reading: Option<AccountReading>) -> (Vec<WindowSnapshot>, Billing) {
    reading.map_or((Vec::new(), Billing::Unknown), |reading| {
        (
            reading.windows.iter().map(window_snapshot).collect(),
            reading.billing,
        )
    })
}

fn window_snapshot(window: &AccountWindow) -> WindowSnapshot {
    WindowSnapshot {
        length_secs: window.window.length_secs,
        scope: window.window.scope.clone(),
        used: window.window.used,
        stale: window.stale,
        reset_ms: window.window.reset.map(epoch_ms),
    }
}

const fn driver_snapshot(drivers: DriverReads) -> DriverSnapshot {
    DriverSnapshot {
        refresh_running: counts_as_running(drivers.refresh, drivers.uptime),
        poll_running: counts_as_running(drivers.poll, drivers.uptime),
    }
}

/// Whether a seat driver counts as running. Its health reads not running
/// both before its task starts and after it stops, so one that has never
/// swept counts as starting until the start-up grace has passed; one that
/// swept and then stopped is stopped at once.
const fn counts_as_running(health: SeatDriverHealthSnapshot, uptime: Duration) -> bool {
    health.running
        || (health.last_sweep_ms.is_none() && uptime.as_millis() < DRIVER_START_GRACE.as_millis())
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

/// The daemon facts a read copies, taken on the async side.
struct DaemonReads {
    poll_state: Option<UsagePollState>,
    refresh_failures: BTreeMap<String, RefreshFailure>,
    refresh_driver: SeatDriverHealthSnapshot,
    poll_driver: SeatDriverHealthSnapshot,
    record_loss: LedgerHealth,
}

impl DaemonReads {
    fn of(state: &StatusState) -> Self {
        Self {
            poll_state: state.daemon_meta.usage_poll_state().cloned(),
            refresh_failures: state.daemon_meta.refresh_failures(),
            refresh_driver: state.daemon_meta.seat_driver(),
            poll_driver: state.daemon_meta.usage_poll_driver(),
            record_loss: state.usage_health.ledger_health(),
        }
    }
}

/// Read every router source from one snapshot at one pinned instant: the
/// monotonic half of the account readings' stamp anchors every elapsed age.
fn read_sources(
    view: &StatusRouterView,
    daemon: DaemonReads,
    started: Instant,
    doctor_findings: Option<Vec<Finding>>,
) -> AttentionSources {
    let stamp = ObservationStamp::now();
    let now = stamp.monotonic();
    let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    let identities = view.account_identities();
    let fixes = identities
        .iter()
        .map(|(account, identity)| {
            (
                account.to_string(),
                view.seat_fix(&identity.provider_entry).command(),
            )
        })
        .collect();
    AttentionSources {
        now_ms,
        pools: view.pool_eligibility(now),
        targets: view.route_targets(now),
        credentials: view.account_credentials((now, now_ms)),
        identities,
        fixes,
        readings: view.account_readings(&stamp),
        poll: daemon
            .poll_state
            .as_ref()
            .map(|state| view.usage_poll_snapshot(state)),
        refresh_failures: daemon.refresh_failures,
        drivers: DriverReads {
            refresh: daemon.refresh_driver,
            poll: daemon.poll_driver,
            uptime: now.saturating_duration_since(started),
        },
        managed_accounts: view.managed_accounts().len(),
        record_loss: daemon.record_loss,
        doctor_findings,
    }
}

/// The panel for the standalone route: gathers its own no-network doctor
/// report first, then builds the list from it.
pub(super) async fn build(state: &StatusState) -> Panel<AttentionPanel> {
    let doctor_findings = super::doctor::report_fail_findings(state).await;
    build_with_doctor(state, doctor_findings).await
}

/// The panel over doctor Fail findings the caller already holds (`None` when
/// it could build no report), so a caller that built the doctor panel in the
/// same request does not gather a second report.
pub(super) async fn build_with_doctor(
    state: &StatusState,
    doctor_findings: Option<Vec<Finding>>,
) -> Panel<AttentionPanel> {
    let view = state.router.view();
    let daemon = DaemonReads::of(state);
    let attention = Arc::clone(&state.attention);
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || {
            let sources = read_sources(&view, daemon, attention.started, doctor_findings);
            let as_of = utc_rfc3339(sources.now_ms);
            Panel::available(SCHEMA_VERSION, as_of, attention.build(sources))
        },
    )
    .await;
    state.observability.attention.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<AttentionPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
pub(super) mod fixture;
#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "attention_wire_tests.rs"]
mod wire_tests;
