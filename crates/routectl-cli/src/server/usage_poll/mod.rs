//! Background usage poll: reads each managed seat's usage endpoint once it
//! has gone 5 minutes without an accepted reading (from a response or a
//! poll), so the account meter holds a reading for idle seats.
//! `client` is the only code that talks to a vendor usage endpoint.
//!
//! [`UsagePollJob`] is a [`SeatJob`]: the seat driver acquires each seat's
//! credential generation before `run` reads its token, and `apply` lands the
//! outcome under that generation. `apply` writes three things and nothing
//! else: the poll schedule, the account meter (through
//! `Router::observe_polled_usage`, accepted bodies only) and the reading
//! history (accepted readings only). Eligibility, the breaker, credential
//! marks and the token are never touched, whatever the endpoint answered.
//! A credentials reload between the token read and `apply` supersedes the
//! poll, since the router learns of a re-login only after the store does.
//!
//! Nothing here logs or carries in an error the token, the response body or
//! headers, or a vendor account id.
//!
//! In a test build the client resolves no host name (see
//! `client::network_fence`), so a daemon a test boots never polls a vendor,
//! whatever logins and models its fixture holds.

mod client;
#[cfg(test)]
pub(super) use self::client::build_failure as client_build_failure;
#[cfg(any(test, feature = "test-utils"))]
pub use self::client::network_fence::{FenceProbe, probe_production_client};
mod history;
#[cfg(test)]
pub use self::history::rows as history_rows_for_tests;
mod schedule;

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::header::HeaderValue;
use routectl_auth::{OAuthStore, PeekedAccessToken, PeekedCredential};
use routectl_core::sanitize_for_log;
use routectl_router::{
    AcceptedReading, Config, ObservationStamp, PolledUsageRejection, Router, UsageVendor,
};
use routectl_usage::UsageHandle;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use self::client::{FetchResult, SeatCredential, UsagePollClient, anthropic_user_agent};
use self::schedule::{PollOutcome, PollSchedule, RandomJitter, SeatCandidate};
use crate::handlers::status::DaemonMeta;
use crate::server::seat_driver::{
    ConfiguredUserAgent, ManagedSeat, SeatJob, SeatVerdict, accounts_awaiting_login, managed_seats,
    run_seat_driver,
};

/// The wall clock the schedule runs on, in epoch ms. Injected so tests can
/// move it without sleeping.
pub(super) trait WallClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The host's wall clock.
struct SystemClock;

impl WallClock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::local_time::unix_ms(time::OffsetDateTime::now_utc())
    }
}

/// The poll schedule shared between the job and its read-only snapshot.
#[derive(Clone)]
pub struct UsagePollState {
    schedule: Arc<Mutex<PollSchedule<RandomJitter>>>,
    clock: Arc<dyn WallClock>,
}

impl UsagePollState {
    fn new(jitter: RandomJitter, clock: Arc<dyn WallClock>) -> Self {
        Self {
            schedule: Arc::new(Mutex::new(PollSchedule::new(jitter))),
            clock,
        }
    }

    /// Every managed seat of `config` with its poll state, and each vendor's
    /// pause still in effect. Reads memory only: one short lock, no I/O.
    /// A seat the schedule has not swept yet reads as never attempted with
    /// no due time. A seat in `awaiting_login` is one the poll skips until a
    /// fresh login, so its sweep state was dropped and reads the same way.
    /// With `[usage] poll` off in `config` every seat reads as switched off
    /// with no sweep state, as the next sweep leaves it.
    pub fn snapshot(
        &self,
        config: &Config,
        awaiting_login: &BTreeSet<String>,
    ) -> PollStateSnapshot {
        let now_ms = self.clock.now_ms();
        let switched_off = !config.usage.poll;
        let schedule = self.schedule.lock();
        let seats = managed_seats(config)
            .into_iter()
            .filter_map(|seat| {
                let vendor = usage_vendor(&seat.provider)?;
                let state = schedule.seat(&seat.account).filter(|_| !switched_off);
                Some(SeatPollSnapshot {
                    vendor,
                    awaiting_login: awaiting_login.contains(&seat.account),
                    last_attempt_ms: state.and_then(schedule::SeatState::last_attempt_ms),
                    last_result: state
                        .and_then(schedule::SeatState::last_outcome)
                        .map(PollOutcome::as_str),
                    next_due_ms: state.map(schedule::SeatState::next_due_ms),
                    failure_since_ms: state.and_then(schedule::SeatState::failure_since_ms),
                    account: seat.account,
                })
            })
            .collect();
        let paused_until_ms = [UsageVendor::Anthropic, UsageVendor::Codex]
            .into_iter()
            .filter_map(|vendor| {
                let until = schedule.pause_until(vendor)?;
                (until > now_ms).then_some((vendor, until))
            })
            .collect();
        PollStateSnapshot {
            switched_off,
            seats,
            paused_until_ms,
        }
    }
}

/// One read of the poll state, for the status surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollStateSnapshot {
    /// Whether `[usage] poll` is off in the config the snapshot was read
    /// against.
    pub switched_off: bool,
    /// One entry per polled seat, in account order.
    pub seats: Vec<SeatPollSnapshot>,
    /// Each paused vendor and when its pause ends (epoch ms).
    pub paused_until_ms: Vec<(UsageVendor, i64)>,
}

impl PollStateSnapshot {
    /// Whether the poll reads `account`'s usage, and from which vendor.
    pub fn applicability(&self, account: &str) -> PollApplicability {
        self.seats
            .iter()
            .find(|seat| seat.account == account)
            .map_or(PollApplicability::NotPolled, |seat| {
                if self.switched_off {
                    PollApplicability::SwitchedOff(seat.vendor)
                } else if seat.awaiting_login {
                    PollApplicability::AwaitingLogin(seat.vendor)
                } else {
                    PollApplicability::Polled(seat.vendor)
                }
            })
    }
}

/// One polled seat's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPollSnapshot {
    /// The seat's account key.
    pub account: String,
    /// The vendor whose usage endpoint the seat is polled at.
    pub vendor: UsageVendor,
    /// Whether the router holds the seat as needing a fresh login, so the
    /// poll skips it until one lands.
    pub awaiting_login: bool,
    /// When the seat was last polled (epoch ms), if it has been.
    pub last_attempt_ms: Option<i64>,
    /// The last poll's result token, if it has been polled.
    pub last_result: Option<&'static str>,
    /// When the seat is next due (epoch ms) as of the last sweep.
    pub next_due_ms: Option<i64>,
    /// When the seat's current run of failed polls began (epoch ms); `None`
    /// before any failure and once a poll, or a newer reading from any source,
    /// is accepted.
    pub failure_since_ms: Option<i64>,
}

/// Whether the background poll reads an account's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollApplicability {
    /// A managed login polled at this vendor's usage endpoint.
    Polled(UsageVendor),
    /// A managed login of this vendor the poll skips until a fresh login,
    /// since its token is known dead.
    AwaitingLogin(UsageVendor),
    /// A managed login of this vendor the poll would read, with the poll
    /// switched off by `[usage] poll = false`. Not a fault.
    SwitchedOff(UsageVendor),
    /// Any other credential: an API key, a cloud credential, a forwarded
    /// one, or a login of a vendor with no usage endpoint.
    NotPolled,
}

/// What `run` produced for one seat.
pub(super) enum PollAttempt {
    /// The seat's provider has no usage endpoint; nothing was read or sent.
    NotPolled,
    Attempted {
        vendor: UsageVendor,
        attempted_ms: i64,
        /// The store's reload generation when the credential was read.
        reload_generation: u64,
        result: AttemptResult,
    },
}

pub(super) enum AttemptResult {
    /// No token outside the refresh lead window; nothing was sent.
    TokenNotReady,
    /// A Codex login with no recorded ChatGPT account id; nothing was sent.
    NoAccountId,
    Fetched(FetchResult),
}

/// Polls each idle managed seat's usage endpoint with its own token.
pub struct UsagePollJob {
    store: Arc<OAuthStore>,
    client: UsagePollClient,
    usage: UsageHandle,
    state: UsagePollState,
    /// The published router, read before a token is read and again when a
    /// result lands, to see the current `[usage] poll` switch.
    live_router: Arc<ArcSwap<Router>>,
}

impl UsagePollJob {
    /// The production job. Building it resolves no vendor client identity:
    /// the Codex identity headers are read per request, so they reflect
    /// whatever the provider factory installed before the first poll.
    pub fn new(
        store: Arc<OAuthStore>,
        usage: UsageHandle,
        live_router: Arc<ArcSwap<Router>>,
    ) -> reqwest::Result<Self> {
        Ok(Self::with_parts(
            store,
            UsagePollClient::new()?,
            usage,
            UsagePollState::new(RandomJitter::from_os(), Arc::new(SystemClock)),
            live_router,
        ))
    }

    const fn with_parts(
        store: Arc<OAuthStore>,
        client: UsagePollClient,
        usage: UsageHandle,
        state: UsagePollState,
        live_router: Arc<ArcSwap<Router>>,
    ) -> Self {
        Self {
            store,
            client,
            usage,
            state,
            live_router,
        }
    }

    /// The read-only handle to the poll state.
    pub fn state(&self) -> UsagePollState {
        self.state.clone()
    }

    fn land_body(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        vendor: UsageVendor,
        body: &[u8],
        generation: u64,
    ) -> PollOutcome {
        match router.observe_polled_usage(&seat.account, vendor, body, generation) {
            Ok(accepted) => {
                let outcome = accepted_outcome(&accepted);
                self.usage
                    .try_send_account_readings(history::rows(&seat.account, &accepted));
                outcome
            }
            // A seat dropped from the config, or a re-login, while the poll
            // was in flight: the body describes nothing current.
            Err(PolledUsageRejection::NotAdmitted | PolledUsageRejection::Superseded) => {
                PollOutcome::Superseded
            }
            Err(PolledUsageRejection::Body(_) | PolledUsageRejection::Rejected(_)) => {
                PollOutcome::Malformed
            }
        }
    }
}

impl SeatJob for UsagePollJob {
    const KIND: &'static str = "usage_poll";

    type Outcome = PollAttempt;

    /// The managed seats the schedule finds due, at most one per vendor.
    /// A seat counts from the meter's last accepted reading of any source:
    /// an accepted response or poll reading suppresses the poll, traffic
    /// alone does not (a response without an accepted quota reading leaves
    /// the seat due). A seat awaiting a fresh login
    /// is left out, as the refresh job leaves it out: its token is known
    /// dead, and the login that revives it starts its schedule afresh.
    /// With `[usage] poll` off nothing is due and every seat's schedule
    /// state is dropped (vendor pauses are kept), so a failure run or
    /// backoff from before the switch does not carry over once it is back
    /// on.
    fn due(&self, router: &Router) -> Vec<ManagedSeat> {
        if !router.config.usage.poll {
            let now_ms = self.state.clock.now_ms();
            let _ = self.state.schedule.lock().sweep(now_ms, &[]);
            return Vec::new();
        }
        let readings = router.account_readings(&ObservationStamp::now());
        let dead = accounts_awaiting_login(router);
        let seats: Vec<(ManagedSeat, UsageVendor)> = managed_seats(&router.config)
            .into_iter()
            .filter(|seat| !dead.contains(&seat.account))
            .filter_map(|seat| usage_vendor(&seat.provider).map(|vendor| (seat, vendor)))
            .collect();
        let candidates: Vec<SeatCandidate<'_>> = seats
            .iter()
            .map(|(seat, vendor)| SeatCandidate {
                account: &seat.account,
                vendor: *vendor,
                last_accepted_ms: readings
                    .accounts
                    .iter()
                    .find(|reading| reading.account == seat.account)
                    .and_then(|reading| reading.last_accepted)
                    .map(|last| history::epoch_ms(last.observed_at)),
            })
            .collect();
        let now_ms = self.state.clock.now_ms();
        let picks = self.state.schedule.lock().sweep(now_ms, &candidates);
        picks
            .into_iter()
            .map(|index| seats[index].0.clone())
            .collect()
    }

    /// The sweep's router is the one loaded before the first seat's
    /// request; a reload during an earlier seat's request shows only on the
    /// published one.
    fn is_switched_off(&self) -> bool {
        !self.live_router.load().config.usage.poll
    }

    fn run(&self, seat: &ManagedSeat) -> impl Future<Output = PollAttempt> + Send {
        let attempted_ms = self.state.clock.now_ms();
        let vendor = usage_vendor(&seat.provider);
        let store = Arc::clone(&self.store);
        let client = self.client.clone();
        let seat = seat.clone();
        async move {
            let Some(vendor) = vendor else {
                return PollAttempt::NotPolled;
            };
            let user_agent = (vendor == UsageVendor::Anthropic).then(|| seat_user_agent(&seat));
            let credential = store.peek_credential(&seat.account).await;
            let reload_generation = credential.reload_generation;
            let result = fetch_usage(&client, credential, vendor, user_agent).await;
            PollAttempt::Attempted {
                vendor,
                attempted_ms,
                reload_generation,
                result,
            }
        }
    }

    fn apply(
        &self,
        router: &Router,
        seat: &ManagedSeat,
        generation: u64,
        attempt: PollAttempt,
    ) -> SeatVerdict {
        let PollAttempt::Attempted {
            vendor,
            attempted_ms,
            reload_generation,
            result,
        } = attempt
        else {
            return SeatVerdict::Quiet;
        };
        // `router` is the one the sweep loaded before the request went out;
        // a reload that switched the poll off since then shows only on the
        // published one.
        if !self.live_router.load().config.usage.poll {
            return SeatVerdict::Quiet;
        }
        let outcome = if self.store.reload_generation() == reload_generation {
            match result {
                AttemptResult::TokenNotReady => PollOutcome::TokenNotReady,
                AttemptResult::NoAccountId => PollOutcome::Auth,
                AttemptResult::Fetched(FetchResult::Ok(body)) => {
                    self.land_body(router, seat, vendor, &body, generation)
                }
                AttemptResult::Fetched(failure) => failure_outcome(&failure),
            }
        } else {
            PollOutcome::Superseded
        };
        self.state
            .schedule
            .lock()
            .record(&seat.account, vendor, attempted_ms, outcome);
        log_outcome(seat, outcome);
        outcome.verdict()
    }
}

/// Spawn the background usage poll over `store`'s managed logins, its
/// health and poll state published on `daemon_meta`. Returns `None`, after
/// one ERROR, when the poll's HTTP client cannot be built: the daemon then
/// serves without the poll, and its health reads as not running. Returns
/// `None`, after one ERROR, when `daemon_meta` already carries a poll: a
/// second driver would poll every seat again on its own schedule while
/// `/status` showed only the first.
///
/// The caller spawns this only after the provider factory has run: the
/// first Codex identity read installs the pinned default for the whole
/// process if no configured one was installed before it.
pub(super) fn spawn_usage_poll_driver(
    store: &Arc<OAuthStore>,
    usage: &UsageHandle,
    router_swap: &Arc<ArcSwap<Router>>,
    shutdown: &watch::Receiver<()>,
    daemon_meta: &DaemonMeta,
) -> Option<JoinHandle<()>> {
    let job = match UsagePollJob::new(Arc::clone(store), usage.clone(), Arc::clone(router_swap)) {
        Ok(job) => job,
        Err(error) => {
            tracing::error!(
                job = UsagePollJob::KIND,
                error = %error,
                "usage poll client could not be built; the background usage poll does not run",
            );
            return None;
        }
    };
    if daemon_meta.install_usage_poll_state(job.state()).is_err() {
        tracing::error!(
            job = UsagePollJob::KIND,
            "a usage poll already runs for this daemon; the second one is not started",
        );
        return None;
    }
    Some(tokio::spawn(run_seat_driver(
        job,
        Arc::clone(router_swap),
        shutdown.clone(),
        daemon_meta.usage_poll_driver_health(),
    )))
}

/// The `User-Agent` `seat`'s Anthropic usage call presents. A configured
/// value that is not a valid header value is replaced by the compiled
/// default, with one WARN that names the seat and never the value.
fn seat_user_agent(seat: &ManagedSeat) -> HeaderValue {
    let resolved = anthropic_user_agent(seat.user_agent.as_ref().map(ConfiguredUserAgent::as_str));
    if resolved.configured_invalid {
        tracing::warn!(
            event = "usage_poll_user_agent_refused",
            account = %sanitize_for_log(&seat.account),
            reason = "invalid_header_value",
            "configured user_agent is not a valid header value; the usage poll sends the default",
        );
    }
    resolved.value
}

async fn fetch_usage(
    client: &UsagePollClient,
    credential: PeekedCredential,
    vendor: UsageVendor,
    user_agent: Option<HeaderValue>,
) -> AttemptResult {
    let PeekedAccessToken::Ready(token) = credential.token else {
        return AttemptResult::TokenNotReady;
    };
    let seat_credential = match vendor {
        UsageVendor::Anthropic => SeatCredential::Anthropic {
            token: &token,
            user_agent: user_agent.unwrap_or_else(|| anthropic_user_agent(None).value),
        },
        UsageVendor::Codex => match credential.account_id.as_deref() {
            Some(account_id) => SeatCredential::Codex {
                token: &token,
                account_id: Some(account_id),
            },
            None => return AttemptResult::NoAccountId,
        },
    };
    AttemptResult::Fetched(client.fetch(seat_credential).await)
}

/// The usage endpoint vendor of a managed login's provider, or `None` for a
/// provider with none.
fn usage_vendor(provider: &str) -> Option<UsageVendor> {
    match provider {
        "anthropic" => Some(UsageVendor::Anthropic),
        "codex" => Some(UsageVendor::Codex),
        _ => None,
    }
}

const fn accepted_outcome(accepted: &AcceptedReading) -> PollOutcome {
    if accepted.windows.is_empty() {
        PollOutcome::NoWindows
    } else {
        PollOutcome::Accepted
    }
}

const fn failure_outcome(result: &FetchResult) -> PollOutcome {
    match *result {
        FetchResult::Redirect => PollOutcome::Redirect,
        FetchResult::Auth => PollOutcome::Auth,
        FetchResult::Http(status) => PollOutcome::Http(status),
        FetchResult::Timeout => PollOutcome::Timeout,
        FetchResult::Transport => PollOutcome::Transport,
        FetchResult::TooLarge | FetchResult::Ok(_) => PollOutcome::TooLarge,
        FetchResult::RateLimited(wait) => PollOutcome::Paused(wait),
    }
}

fn log_outcome(seat: &ManagedSeat, outcome: PollOutcome) {
    let status = match outcome {
        PollOutcome::Http(status) => Some(status),
        _ => None,
    };
    tracing::debug!(
        event = "usage_poll",
        account = %sanitize_for_log(&seat.account),
        outcome = outcome.as_str(),
        status,
        "usage poll finished",
    );
}

#[cfg(test)]
#[path = "rig_tests.rs"]
mod rig;

#[cfg(test)]
#[path = "job_tests.rs"]
mod job_tests;

#[cfg(test)]
#[path = "containment_tests.rs"]
mod containment_tests;

#[cfg(test)]
#[path = "config_reload_tests.rs"]
mod config_reload_tests;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod daemon_tests;

#[cfg(test)]
#[path = "user_agent_tests.rs"]
mod user_agent_tests;

#[cfg(test)]
#[path = "switch_tests.rs"]
mod switch_tests;
