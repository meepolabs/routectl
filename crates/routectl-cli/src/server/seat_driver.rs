//! The daemon's background seat driver: keeps the managed logins the config
//! names refreshed, whether or not traffic uses them.
//!
//! Sibling of `metrics_driver.rs` and `probe_driver.rs`, riding the same
//! reload pipeline and shutdown signal. The driver owns cadence, shutdown and
//! its own health; a `SeatJob` owns which seats are due, what running one
//! does, and how its outcome lands on the router. Refresh is the one job.
//!
//! Every seat runs under a credential generation acquired BEFORE the job
//! runs, and its outcome applies only under that generation, so a re-login or
//! another refresh that lands while the job is in flight wins over it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;
use routectl_auth::{DefinitiveCause, OAuthStore, RefreshOutcome, SecretRef};
use routectl_core::sanitize_for_log;
use routectl_router::runtime_state::{MarkOutcome, UnusableCause};
use routectl_router::{Config, Router};
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
pub(super) struct ManagedSeat {
    pub(super) provider: String,
    pub(super) label: Option<String>,
    pub(super) account: String,
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
    /// What running the job for one seat produced.
    type Outcome: Send;

    /// The seats due for this job on `router`, in sweep order.
    fn due(&self, router: &Router) -> Vec<ManagedSeat>;

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
}

impl RefreshJob {
    pub(super) const fn new(store: Arc<OAuthStore>) -> Self {
        Self { store }
    }
}

impl SeatJob for RefreshJob {
    type Outcome = RefreshOutcome;

    /// Every configured managed seat except those already marked with a
    /// cause only a fresh login clears: a refused refresh token is refused
    /// again, so re-posting it each sweep is a retest with a known answer.
    /// The reload that sees the login resets the mark, which makes the seat
    /// due again.
    fn due(&self, router: &Router) -> Vec<ManagedSeat> {
        let dead = accounts_awaiting_login(router);
        managed_seats(&router.config)
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
/// through, in account order. A seat is reached through a model's own
/// provider entry or through a member of the pool it names; a stored
/// credential no entry names is never listed.
pub(super) fn managed_seats(config: &Config) -> Vec<ManagedSeat> {
    let mut seats: BTreeMap<String, ManagedSeat> = BTreeMap::new();
    for model in config.models.values() {
        for entry_name in egress_entries(config, &model.provider) {
            if let Some(seat) = managed_seat_of(config, entry_name) {
                seats.entry(seat.account.clone()).or_insert(seat);
            }
        }
    }
    seats.into_values().collect()
}

fn egress_entries<'a>(config: &'a Config, provider: &'a str) -> Vec<&'a str> {
    config.pools.get(provider).map_or_else(
        || vec![provider],
        |pool| pool.members.iter().map(String::as_str).collect(),
    )
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
        provider,
        label,
        account,
    })
}

/// Accounts the router holds as unusable for a cause only a login clears.
fn accounts_awaiting_login(router: &Router) -> BTreeSet<String> {
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

    fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    fn record_sweep(&self, tally: SweepTally) {
        let now_ms = chrono::Utc::now().timestamp_millis();
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
struct SweepTally {
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
/// ERROR when the loop stops for any other reason: the daemon keeps serving,
/// but its managed logins are no longer refreshed in the background.
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
            exit = ?exit,
            "seat driver stopped before shutdown; managed logins are no longer refreshed in the background",
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
async fn sweep<J: SeatJob>(job: &J, router: &Router) -> SweepTally {
    let mut tally = SweepTally::default();
    for seat in job.due(router) {
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
