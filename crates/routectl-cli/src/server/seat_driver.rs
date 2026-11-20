//! The daemon's background seat driver: keeps the managed logins the config
//! names refreshed, whether or not traffic uses them.
//!
//! Sibling of `metrics_driver.rs` and `probe_driver.rs`, riding the same
//! reload pipeline and shutdown signal. The driver owns cadence, shutdown and
//! its own health; a `SeatJob` owns which seats are due, what running one
//! does, and how its outcome lands on the router.
//!
//! Every seat runs under a credential generation acquired BEFORE the job
//! runs, and its outcome applies only under that generation, so a re-login or
//! another refresh that lands while the job is in flight wins over it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use routectl_auth::{DefinitiveCause, OAuthStore, RefreshOutcome, SecretRef};
use routectl_core::sanitize_for_log;
use routectl_router::runtime_state::{MarkOutcome, UnusableCause};
use routectl_router::{AccountIdentities, Config, Router};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

/// How often the driver sweeps the configured seats.
///
/// A code constant, not an operator knob: a seat is due only inside the
/// store's refresh lead window, so the interval bounds how late inside that
/// window a refresh starts, and it must stay well under it.
pub(super) const SEAT_DRIVER_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// The OAuth providers whose configured seats the driver keeps refreshed.
const MANAGED_PROVIDERS: [&str; 2] = ["anthropic", "codex"];

/// Epoch-ms sentinel for "no sweep has finished yet".
const NEVER: i64 = 0;

/// One configured managed-login seat and the credential account the router
/// keys its marks on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedSeat {
    /// The `[providers]` entry the seat is reported under. A login named by
    /// several entries is one seat, reported under the lexicographically
    /// smallest of them so every surface agrees whatever the model order.
    pub(crate) entry: String,
    pub(crate) provider: String,
    pub(crate) label: Option<String>,
    pub(crate) account: String,
    /// The `user_agent` that entry sets, if any.
    pub(crate) user_agent: Option<ConfiguredUserAgent>,
}

/// A provider entry's configured `user_agent`. Operator text: its `Debug`
/// never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredUserAgent(String);

impl ConfiguredUserAgent {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ConfiguredUserAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfiguredUserAgent(..)")
    }
}

/// How one seat's outcome counts toward the driver's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeatVerdict {
    /// Nothing went wrong: not due, or refreshed.
    Quiet,
    /// The job failed for a reason that may clear on its own.
    Transient,
    /// The seat cannot be used again without a fresh login.
    Definitive,
}

/// One kind of per-seat background work.
pub(super) trait SeatJob: Send + Sync {
    /// The job's name in the driver's log lines.
    const KIND: &'static str;

    /// What running the job for one seat produced.
    type Outcome: Send;

    /// The seats due for this job on `router`, in sweep order.
    fn due(&self, router: &Router) -> Vec<ManagedSeat>;

    /// Whether the job was switched off since the sweep picked its seats.
    /// Checked before each seat's credential generation is acquired, so a
    /// seat still waiting when the job goes off is left untouched.
    fn is_switched_off(&self) -> bool {
        false
    }

    /// Run the job for `seat`.
    fn run(&self, seat: &ManagedSeat) -> impl Future<Output = Self::Outcome> + Send;

    /// Land `outcome` on `router` under the credential `generation` acquired
    /// before the job ran.
    fn apply(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        generation: u64,
        outcome: Self::Outcome,
    ) -> SeatVerdict;
}

/// Refresh each configured Anthropic and Codex seat whose access token is
/// inside the store's refresh lead window.
pub(super) struct RefreshJob {
    store: Arc<OAuthStore>,
    failures: Arc<RefreshFailures>,
}

impl RefreshJob {
    pub(super) const fn new(store: Arc<OAuthStore>, failures: Arc<RefreshFailures>) -> Self {
        Self { store, failures }
    }
}

impl SeatJob for RefreshJob {
    const KIND: &'static str = "refresh";

    type Outcome = RefreshOutcome;

    /// Every configured managed seat except those already marked with a
    /// cause only a fresh login clears: a refused refresh token is refused
    /// again, so re-posting it each sweep is a retest with a known answer.
    /// The reload that sees the login resets the mark, which makes the seat
    /// due again.
    ///
    /// Each sweep starts here, so this is also where the failure record
    /// drops accounts the config no longer manages and accounts whose
    /// credential was replaced since their failure was recorded.
    fn due(&self, router: &Router) -> Vec<ManagedSeat> {
        let seats = managed_seats(&router.config);
        self.failures.prune(router, &seats);
        let dead = accounts_awaiting_login(router);
        seats
            .into_iter()
            .filter(|seat| !dead.contains(&seat.account))
            .collect()
    }

    fn run(&self, seat: &ManagedSeat) -> impl Future<Output = RefreshOutcome> + Send {
        let store = Arc::clone(&self.store);
        let seat = seat.clone();
        async move {
            store
                .refresh_if_due(&seat.provider, seat.label.as_deref())
                .await
        }
    }

    fn apply(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        generation: u64,
        outcome: RefreshOutcome,
    ) -> SeatVerdict {
        self.failures.record(router, seat, generation, outcome);
        match outcome {
            RefreshOutcome::NotDue => SeatVerdict::Quiet,
            RefreshOutcome::Refreshed => {
                settle_refreshed(router, seat, generation);
                SeatVerdict::Quiet
            }
            RefreshOutcome::Transient { access_expired } => {
                if access_expired {
                    mark_unusable(router, seat, UnusableCause::Expired, generation);
                }
                SeatVerdict::Transient
            }
            RefreshOutcome::Definitive(cause) => {
                mark_unusable(router, seat, unusable_cause(cause), generation);
                SeatVerdict::Definitive
            }
        }
    }
}

/// One account's current refresh failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshFailure {
    /// When the first failing result of the current run of failures was
    /// recorded (epoch ms).
    pub first_failure_ms: i64,
    /// The latest failing result: `transient`, `transient_access_expired`,
    /// `revoked` or `not_logged_in`.
    pub token: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct FailureEntry {
    failure: RefreshFailure,
    /// The credential generation the failing results were observed under.
    generation: u64,
}

/// The refresh driver's current failure per configured account.
/// Memory-only; a restart starts it empty. Written only by the refresh job:
/// an entry starts at the first failing result, keeps that first time across
/// later failing results, and goes on a quiet result, on a credential
/// replacement, or when the config stops managing the account. Every key is
/// a seat of the config in effect at the last sweep, so the record never
/// holds more entries than that config has managed seats.
#[derive(Debug, Default)]
pub struct RefreshFailures {
    entries: Mutex<BTreeMap<String, FailureEntry>>,
}

impl RefreshFailures {
    /// Every current failure, keyed by credential account.
    pub fn snapshot(&self) -> BTreeMap<String, RefreshFailure> {
        self.entries
            .lock()
            .iter()
            .map(|(account, entry)| (account.clone(), entry.failure))
            .collect()
    }

    /// Record one failing `token` for `account` under `generation`. Test
    /// seam for readers of the record that cannot run the job.
    #[cfg(test)]
    pub(crate) fn record_failure_for_tests(
        &self,
        account: &str,
        generation: u64,
        token: &'static str,
    ) {
        self.record_failure(account, generation, token);
    }

    fn record(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        generation: u64,
        outcome: RefreshOutcome,
    ) {
        let replaced = router.acquire_credential_generation(&seat.account) != generation;
        match failure_token(outcome) {
            Some(token) if !replaced => self.record_failure(&seat.account, generation, token),
            // A credential replaced while the refresh was in flight makes the
            // result evidence about the old credential only.
            Some(_) | None => {
                self.entries.lock().remove(&seat.account);
            }
        }
    }

    fn record_failure(&self, account: &str, generation: u64, token: &'static str) {
        let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
        let mut entries = self.entries.lock();
        let first_failure_ms = entries
            .get(account)
            .filter(|entry| entry.generation == generation)
            .map_or(now_ms, |entry| entry.failure.first_failure_ms);
        entries.insert(
            account.to_string(),
            FailureEntry {
                failure: RefreshFailure {
                    first_failure_ms,
                    token,
                },
                generation,
            },
        );
    }

    fn prune(&self, router: &Router, seats: &[ManagedSeat]) {
        let managed: BTreeSet<&str> = seats.iter().map(|seat| seat.account.as_str()).collect();
        self.entries.lock().retain(|account, entry| {
            managed.contains(account.as_str())
                && router.acquire_credential_generation(account) == entry.generation
        });
    }
}

/// The record's token for a failing refresh result, `None` for a quiet one.
const fn failure_token(outcome: RefreshOutcome) -> Option<&'static str> {
    match outcome {
        RefreshOutcome::NotDue | RefreshOutcome::Refreshed => None,
        RefreshOutcome::Transient {
            access_expired: false,
        } => Some("transient"),
        RefreshOutcome::Transient {
            access_expired: true,
        } => Some("transient_access_expired"),
        RefreshOutcome::Definitive(DefinitiveCause::Revoked) => Some("revoked"),
        RefreshOutcome::Definitive(DefinitiveCause::NotLoggedIn) => Some("not_logged_in"),
    }
}

const fn unusable_cause(cause: DefinitiveCause) -> UnusableCause {
    match cause {
        DefinitiveCause::NotLoggedIn => UnusableCause::NotLoggedIn,
        DefinitiveCause::Revoked => UnusableCause::Revoked,
    }
}

fn mark_unusable(router: &Router, seat: &ManagedSeat, cause: UnusableCause, generation: u64) {
    let account = sanitize_for_log(&seat.account);
    if router.mark_credential_unusable(&seat.account, cause, Some(generation)) == MarkOutcome::Stale
    {
        tracing::debug!(
            account = %account,
            cause = cause.as_str(),
            "background refresh outcome overtaken by a newer credential; not marking",
        );
    }
}

fn settle_refreshed(router: &Router, seat: &ManagedSeat, generation: u64) {
    let _ = router.credential_refreshed_in_generation(&seat.account, generation);
}

/// The deduplicated managed-login seats the config's models dispatch
/// through, in account order, each under the provider entry every surface
/// reports its account under. A seat is reached through a model's own
/// provider entry or through a member of the pool it names; a stored
/// credential no entry names is never listed.
pub(crate) fn managed_seats(config: &Config) -> Vec<ManagedSeat> {
    AccountIdentities::from_config(config)
        .iter()
        .filter_map(|(_, identity)| managed_seat_of(config, &identity.provider_entry))
        .collect()
}

fn managed_seat_of(config: &Config, entry_name: &str) -> Option<ManagedSeat> {
    let entry = config.providers.get(entry_name)?;
    if entry.forwarded_base_url().is_some() {
        return None;
    }
    let SecretRef::OAuth { provider, label } = SecretRef::parse(entry.api_key_ref()?).ok()? else {
        return None;
    };
    if !MANAGED_PROVIDERS.contains(&provider.as_str()) {
        return None;
    }
    let account = routectl_auth::oauth::seat_key(&provider, label.as_deref());
    Some(ManagedSeat {
        entry: entry_name.to_string(),
        provider,
        label,
        account,
        user_agent: entry.user_agent.clone().map(ConfiguredUserAgent),
    })
}

/// Accounts the router holds as unusable for a cause only a login clears.
pub(crate) fn accounts_awaiting_login(router: &Router) -> BTreeSet<String> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .filter(|target| {
            target.gate.credential_unusable.is_some_and(|mark| {
                matches!(
                    mark.cause,
                    UnusableCause::Revoked | UnusableCause::NotLoggedIn
                )
            })
        })
        .filter_map(|target| managed_seat_of(&router.config, &target.provider_name))
        .map(|seat| seat.account)
        .collect()
}

/// The driver's own health. Memory-only; a restart starts it from zero.
#[derive(Debug, Default)]
pub struct SeatDriverHealth {
    running: AtomicBool,
    last_sweep_ms: AtomicI64,
    last_good_sweep_ms: AtomicI64,
    sweeps: AtomicU64,
    transient: AtomicU64,
    definitive: AtomicU64,
}

/// One read of [`SeatDriverHealth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatDriverHealthSnapshot {
    /// Whether the driver task is running.
    pub running: bool,
    /// When the last sweep finished (epoch ms), or `None` before the first.
    pub last_sweep_ms: Option<i64>,
    /// When the last sweep with no transient or definitive seat outcome
    /// finished (epoch ms), or `None` before one.
    pub last_good_sweep_ms: Option<i64>,
    /// Sweeps finished since the driver started.
    pub sweeps: u64,
    /// Seat outcomes that failed for a reason that may clear on its own.
    pub transient: u64,
    /// Seat outcomes that found a seat unusable until a fresh login.
    pub definitive: u64,
}

impl SeatDriverHealth {
    /// Read every field once.
    pub fn snapshot(&self) -> SeatDriverHealthSnapshot {
        let stamp =
            |value: &AtomicI64| Some(value.load(Ordering::Relaxed)).filter(|ms| *ms != NEVER);
        SeatDriverHealthSnapshot {
            running: self.running.load(Ordering::Relaxed),
            last_sweep_ms: stamp(&self.last_sweep_ms),
            last_good_sweep_ms: stamp(&self.last_good_sweep_ms),
            sweeps: self.sweeps.load(Ordering::Relaxed),
            transient: self.transient.load(Ordering::Relaxed),
            definitive: self.definitive.load(Ordering::Relaxed),
        }
    }

    /// Record a running driver that finished one sweep with no failing seat,
    /// then one with `transient` and `definitive` failing seats. Test seam
    /// for readers of the health that cannot run the driver.
    #[cfg(test)]
    pub(crate) fn record_two_sweeps_for_tests(&self, transient: u64, definitive: u64) {
        self.set_running(true);
        self.record_sweep(SweepTally::default());
        self.record_sweep(SweepTally {
            transient,
            definitive,
        });
    }

    fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    fn record_sweep(&self, tally: SweepTally) {
        let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
        self.transient.fetch_add(tally.transient, Ordering::Relaxed);
        self.definitive
            .fetch_add(tally.definitive, Ordering::Relaxed);
        if tally.transient == 0 && tally.definitive == 0 {
            self.last_good_sweep_ms.store(now_ms, Ordering::Relaxed);
        }
        self.last_sweep_ms.store(now_ms, Ordering::Relaxed);
        self.sweeps.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct SweepTally {
    transient: u64,
    definitive: u64,
}

/// Why the driver loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverExit {
    Shutdown,
    /// The shutdown channel closed without a signal: nothing will ever tell
    /// the loop to stop, and nothing is left that could.
    ShutdownChannelClosed,
}

/// Drive `job` until `shutdown` fires, keeping `health` current. Logs one
/// ERROR naming the job when the loop stops for any other reason: the daemon
/// keeps serving, but that job no longer runs in the background.
pub(super) async fn run_seat_driver<J: SeatJob>(
    job: J,
    router_swap: Arc<ArcSwap<Router>>,
    shutdown: watch::Receiver<()>,
    health: Arc<SeatDriverHealth>,
) {
    health.set_running(true);
    let exit = drive(&job, &router_swap, shutdown, &health).await;
    health.set_running(false);
    if exit != DriverExit::Shutdown {
        tracing::error!(
            job = J::KIND,
            exit = ?exit,
            "{} seat driver stopped before shutdown; the job no longer runs in the background",
            J::KIND,
        );
    }
}

async fn drive<J: SeatJob>(
    job: &J,
    router_swap: &ArcSwap<Router>,
    mut shutdown: watch::Receiver<()>,
    health: &SeatDriverHealth,
) -> DriverExit {
    // Skip, not the default Burst: after a suspend the missed ticks coalesce
    // into one sweep, and due-ness is judged on the wall clock, so that one
    // sweep already sees every token that expired meanwhile.
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + SEAT_DRIVER_INTERVAL,
        SEAT_DRIVER_INTERVAL,
    );
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => return exit_for(changed),
            _ = tick.tick() => {
                let router = router_swap.load_full();
                tokio::select! {
                    // A refresh can sit on the token endpoint for its whole
                    // timeout; shutdown drops the sweep rather than waiting.
                    biased;
                    changed = shutdown.changed() => return exit_for(changed),
                    tally = sweep(job, &router) => health.record_sweep(tally),
                }
            }
        }
    }
}

const fn exit_for(changed: Result<(), watch::error::RecvError>) -> DriverExit {
    match changed {
        Ok(()) => DriverExit::Shutdown,
        Err(_) => DriverExit::ShutdownChannelClosed,
    }
}

/// One sequential pass over the due seats.
pub(super) async fn sweep<J: SeatJob>(job: &J, router: &Router) -> SweepTally {
    let mut tally = SweepTally::default();
    for seat in job.due(router) {
        if job.is_switched_off() {
            break;
        }
        let generation = router.acquire_credential_generation(&seat.account);
        let outcome = job.run(&seat).await;
        match job.apply(router, &seat, generation, outcome) {
            SeatVerdict::Quiet => {}
            SeatVerdict::Transient => tally.transient += 1,
            SeatVerdict::Definitive => tally.definitive += 1,
        }
    }
    tally
}

#[cfg(test)]
#[path = "seat_driver_tests.rs"]
mod tests;
