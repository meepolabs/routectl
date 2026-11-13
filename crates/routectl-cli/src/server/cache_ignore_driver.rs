//! The daemon's cache-ignore driver: re-evaluates the cache-ignore report
//! on a slow interval and logs only when a lane's verdict or the report's
//! availability changes.
//!
//! Report only. It reads the usage ledger read-only and logs; it never
//! strips a marker, reroutes, retries or changes a capability. Its state is
//! the previous status, the lanes reading `ignoring` with their counts, the
//! verdict of every other reported lane, and the providers the previous
//! report evaluated, all held in the task; nothing is persisted. So a boot
//! compares against an empty state: it WARNs once for each lane ignoring
//! now, which re-announces a lane still ignoring across a restart once and
//! never hides one that started ignoring while the daemon was down.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use routectl_router::{Config, Router};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::ledger_reader::epoch_ms_now;
use crate::cache_ignore::{
    CACHE_IGNORE_WINDOW, CacheIgnoreLaneReport, CacheIgnoreReport, CacheIgnoreStatus,
    CacheIgnoreWindow, LaneIdentifiers, LaneVerdict, UNAVAILABLE_DB, UNAVAILABLE_NO_DATA,
    read_cache_ignore_report,
};

/// How often the driver re-evaluates.
pub(super) const CACHE_IGNORE_DRIVER_INTERVAL: Duration = Duration::from_mins(15);

/// Upper bound on each evaluation's ledger open and query together.
const EVALUATION_DEADLINE: Duration = Duration::from_secs(2);

const WINDOW_HOURS: u64 = CACHE_IGNORE_WINDOW.as_secs() / 3600;

const EVENT_IGNORING: &str = "cache_markers_without_cache_activity";
const EVENT_IGNORING_CLEARED: &str = "cache_markers_without_cache_activity_cleared";
const EVENT_UNAVAILABLE: &str = "cache_ignore_unavailable";
const EVENT_AVAILABLE: &str = "cache_ignore_available";
const EVENT_BASELINE: &str = "cache_ignore_baseline";
const EVENT_EVALUATED: &str = "cache_ignore_evaluated";
const EVENT_PANICKED: &str = "cache_ignore_evaluation_panicked";

/// Identity of one evaluated lane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LaneKey {
    provider: String,
    seat: Option<String>,
    model: Option<String>,
    provider_kind: String,
}

impl LaneKey {
    fn of(lane: &CacheIgnoreLaneReport) -> Self {
        Self {
            provider: lane.provider.clone(),
            seat: lane.seat.clone(),
            model: lane.model.clone(),
            provider_kind: lane.provider_kind.clone(),
        }
    }

    fn identifiers(&self) -> LaneIdentifiers {
        LaneIdentifiers::render(&self.provider, self.seat.as_deref(), self.model.as_deref())
    }
}

/// What a lane reads once it no longer reads `ignoring`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClearedVerdict {
    /// The report still evaluates the lane's provider.
    Evaluated(LaneVerdict),
    /// The lane's provider left the evaluated set (removed by a reload, or
    /// no longer eligible), so the lane is no longer judged at all.
    NotEvaluated,
}

impl ClearedVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Evaluated(verdict) => verdict.as_str(),
            Self::NotEvaluated => "not_evaluated",
        }
    }
}

/// The counts a lane carried when it was last seen `ignoring`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaneCounts {
    evidence_turns: i64,
    active_turns: i64,
    sessions: i64,
}

impl LaneCounts {
    const fn of(lane: &CacheIgnoreLaneReport) -> Self {
        Self {
            evidence_turns: lane.evidence_turns,
            active_turns: lane.active_turns,
            sessions: lane.sessions,
        }
    }
}

/// What the driver believes after an evaluation: the report status, the
/// lanes reading `ignoring`, the verdict of every other reported lane, and
/// the providers the report evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriverState {
    status: CacheIgnoreStatus,
    ignoring: BTreeMap<LaneKey, LaneCounts>,
    others: BTreeMap<LaneKey, LaneVerdict>,
    evaluated: BTreeSet<String>,
}

impl DriverState {
    /// The state a boot compares against: an available report with no lane
    /// ignoring, so every lane ignoring at boot WARNs once and an
    /// unreadable ledger is reported once.
    const fn boot() -> Self {
        Self {
            status: CacheIgnoreStatus::Evaluated,
            ignoring: BTreeMap::new(),
            others: BTreeMap::new(),
            evaluated: BTreeSet::new(),
        }
    }

    /// The state after `report`. A lane the report cannot speak for keeps
    /// its previous verdict: every lane while the report is unavailable, and
    /// a lane missing from a truncated report (it may sit past the lane cap).
    fn observe(&self, report: &CacheIgnoreReport) -> Self {
        let mut ignoring = ignoring_lanes(report);
        let speaks_for_every_lane =
            report.status == CacheIgnoreStatus::Evaluated && !report.truncated;
        if !speaks_for_every_lane {
            for (key, counts) in &self.ignoring {
                let reported = report.lanes.iter().any(|lane| LaneKey::of(lane) == *key);
                if !reported {
                    ignoring.entry(key.clone()).or_insert(*counts);
                }
            }
        }
        Self {
            status: report.status,
            ignoring,
            others: other_lanes(report),
            evaluated: report.evaluated.iter().cloned().collect(),
        }
    }

    /// A lane's verdict now that it no longer reads `ignoring`. A lane a
    /// complete report omits had no qualifying turn in the window, unless
    /// its provider is no longer evaluated at all.
    fn verdict_after_ignoring(&self, lane: &LaneKey) -> ClearedVerdict {
        if !self.evaluated.contains(&lane.provider) {
            return ClearedVerdict::NotEvaluated;
        }
        ClearedVerdict::Evaluated(
            self.others
                .get(lane)
                .copied()
                .unwrap_or(LaneVerdict::Insufficient),
        )
    }
}

fn ignoring_lanes(report: &CacheIgnoreReport) -> BTreeMap<LaneKey, LaneCounts> {
    if report.status != CacheIgnoreStatus::Evaluated {
        return BTreeMap::new();
    }
    report
        .lanes
        .iter()
        .filter(|lane| lane.verdict == LaneVerdict::Ignoring)
        .map(|lane| (LaneKey::of(lane), LaneCounts::of(lane)))
        .collect()
}

fn other_lanes(report: &CacheIgnoreReport) -> BTreeMap<LaneKey, LaneVerdict> {
    report
        .lanes
        .iter()
        .filter(|lane| lane.verdict != LaneVerdict::Ignoring)
        .map(|lane| (LaneKey::of(lane), lane.verdict))
        .collect()
}

/// One logged change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// A lane entered `ignoring`.
    LaneIgnoring { lane: LaneKey, counts: LaneCounts },
    /// A lane left `ignoring`; `verdict` is what it reads now.
    LaneCleared {
        lane: LaneKey,
        verdict: ClearedVerdict,
    },
    /// The report became unavailable, or its unavailable class changed.
    Unavailable { class: &'static str },
    /// The report is available again.
    Available { previous_class: &'static str },
}

impl Event {
    /// WARN for a lane entering `ignoring` and for unavailability, except a
    /// ledger that does not exist yet (a fresh install, or usage recording
    /// off), which is INFO. Every clear is INFO.
    fn level(&self) -> tracing::Level {
        match self {
            Self::LaneIgnoring { .. } => tracing::Level::WARN,
            Self::Unavailable { class } if *class != UNAVAILABLE_NO_DATA => tracing::Level::WARN,
            Self::Unavailable { .. } | Self::LaneCleared { .. } | Self::Available { .. } => {
                tracing::Level::INFO
            }
        }
    }
}

/// The events between two states: the status change first, then each lane
/// entering `ignoring`, then each lane leaving it. A change of unavailable
/// class is a new `Unavailable` event; the same class again is silent.
fn transitions(prev: &DriverState, next: &DriverState) -> Vec<Event> {
    let mut events = Vec::new();
    match (prev.status, next.status) {
        (before, after) if before == after => {}
        (CacheIgnoreStatus::Unavailable(previous_class), CacheIgnoreStatus::Evaluated) => {
            events.push(Event::Available { previous_class });
        }
        (_, CacheIgnoreStatus::Unavailable(class)) => events.push(Event::Unavailable { class }),
        (CacheIgnoreStatus::Evaluated, CacheIgnoreStatus::Evaluated) => {}
    }
    for (lane, counts) in &next.ignoring {
        if !prev.ignoring.contains_key(lane) {
            events.push(Event::LaneIgnoring {
                lane: lane.clone(),
                counts: *counts,
            });
        }
    }
    for lane in prev.ignoring.keys() {
        if !next.ignoring.contains_key(lane) {
            events.push(Event::LaneCleared {
                lane: lane.clone(),
                verdict: next.verdict_after_ignoring(lane),
            });
        }
    }
    events
}

/// Drive the cache-ignore evaluation of the ledger at `db_path` until
/// `shutdown` fires. `db_path` is the boot config's: the path is
/// restart-required, so a reload that changes it must not move this reader
/// off the ledger the writer still fills. Eligibility follows the live
/// config.
pub(super) async fn run_cache_ignore_driver(
    router_swap: Arc<ArcSwap<Router>>,
    db_path: PathBuf,
    shutdown: watch::Receiver<()>,
) {
    drive(router_swap, shutdown, epoch_ms_now, ledger_read(db_path)).await;
}

/// The blocking read of the ledger at `db_path` under one config, bounded
/// by [`EVALUATION_DEADLINE`].
fn ledger_read(db_path: PathBuf) -> impl Fn(&Config, i64) -> CacheIgnoreReport + Send + Sync {
    move |config, now_ms| read_cache_ignore_report(&db_path, config, now_ms, EVALUATION_DEADLINE)
}

/// The driver loop over an injectable wall clock (epoch milliseconds, read
/// once per evaluation) and an injectable blocking ledger read.
async fn drive<C, R>(
    router_swap: Arc<ArcSwap<Router>>,
    mut shutdown: watch::Receiver<()>,
    clock: C,
    read: R,
) where
    C: Fn() -> i64,
    R: Fn(&Config, i64) -> CacheIgnoreReport + Send + Sync + 'static,
{
    let read = Arc::new(read);
    // Skip, not the default Burst: after a suspend the missed ticks would
    // each re-read the same ledger and log nothing new.
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + CACHE_IGNORE_DRIVER_INTERVAL,
        CACHE_IGNORE_DRIVER_INTERVAL,
    );
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let boot = evaluate(Arc::clone(&read), live_config(&router_swap), clock());
    let current = tokio::select! {
        biased;
        _ = shutdown.changed() => return,
        report = boot => report,
    };
    let mut state = log_boot(&current);

    loop {
        tokio::select! {
            // Shutdown first, so a tick ready in the same poll never starts
            // an evaluation after shutdown.
            biased;
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                let run = evaluate(Arc::clone(&read), live_config(&router_swap), clock());
                tokio::select! {
                    // Losing to shutdown drops the join handle; the
                    // detached read still ends within its deadline, which is
                    // anchored before its fast-fail open.
                    biased;
                    _ = shutdown.changed() => return,
                    report = run => state = log_tick(&state, &report),
                }
            }
        }
    }
}

/// The config of the router published at this tick, so a hot reload
/// changes which providers are evaluated.
fn live_config(router_swap: &ArcSwap<Router>) -> Arc<Config> {
    Arc::clone(&router_swap.load_full().config)
}

/// Log the boot line and one WARN per lane ignoring now, and return the
/// state to carry.
fn log_boot(current: &CacheIgnoreReport) -> DriverState {
    let prev = DriverState::boot();
    let next = prev.observe(current);
    tracing::info!(
        event = EVENT_BASELINE,
        lanes_ignoring = next.ignoring.len(),
        status = next.status.as_str(),
        window_hours = WINDOW_HOURS,
        "cache-ignore baseline",
    );
    log_events(&transitions(&prev, &next));
    next
}

/// Log what changed between `state` and `report` and return the new state.
fn log_tick(state: &DriverState, report: &CacheIgnoreReport) -> DriverState {
    let next = state.observe(report);
    tracing::debug!(
        event = EVENT_EVALUATED,
        lanes_ignoring = next.ignoring.len(),
        status = next.status.as_str(),
        "cache-ignore evaluated",
    );
    log_events(&transitions(state, &next));
    next
}

/// One report at `now_ms`, read off the async runtime. The read logs
/// nothing; a read that panicked is logged here, on the caller's task after
/// the await, and reads as an unavailable ledger.
async fn evaluate<R>(read: Arc<R>, config: Arc<Config>, now_ms: i64) -> CacheIgnoreReport
where
    R: Fn(&Config, i64) -> CacheIgnoreReport + Send + Sync + 'static,
{
    match tokio::task::spawn_blocking(move || read(&config, now_ms)).await {
        Ok(report) => report,
        Err(err) => {
            tracing::error!(
                event = EVENT_PANICKED,
                error = %err,
                "cache-ignore evaluation task failed",
            );
            failed_evaluation(now_ms)
        }
    }
}

/// The report for an evaluation whose blocking task did not return.
fn failed_evaluation(now_ms: i64) -> CacheIgnoreReport {
    CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(now_ms),
        status: CacheIgnoreStatus::Unavailable(UNAVAILABLE_DB),
        lanes: Vec::new(),
        evaluated: Vec::new(),
        not_evaluated: Vec::new(),
        truncated: false,
    }
}

fn log_events(events: &[Event]) {
    for event in events {
        log_event(event);
    }
}

fn log_event(event: &Event) {
    match event {
        Event::LaneIgnoring { lane, counts } => {
            let ids = lane.identifiers();
            tracing::warn!(
                event = EVENT_IGNORING,
                provider = %ids.provider,
                seat = ids.seat.as_deref(),
                model = ids.model.as_deref(),
                evidence_turns = counts.evidence_turns,
                active_turns = counts.active_turns,
                sessions = counts.sessions,
                window_hours = WINDOW_HOURS,
                "markers sent, no cache activity",
            );
        }
        Event::LaneCleared { lane, verdict } => {
            let ids = lane.identifiers();
            tracing::info!(
                event = EVENT_IGNORING_CLEARED,
                provider = %ids.provider,
                seat = ids.seat.as_deref(),
                model = ids.model.as_deref(),
                verdict = verdict.as_str(),
                window_hours = WINDOW_HOURS,
                "lane no longer reads markers sent, no cache activity",
            );
        }
        Event::Unavailable { class } if event.level() == tracing::Level::WARN => tracing::warn!(
            event = EVENT_UNAVAILABLE,
            class,
            "cache-ignore evaluation unavailable",
        ),
        Event::Unavailable { class } => tracing::info!(
            event = EVENT_UNAVAILABLE,
            class,
            "cache-ignore evaluation unavailable",
        ),
        Event::Available { previous_class } => tracing::info!(
            event = EVENT_AVAILABLE,
            previous_class,
            "cache-ignore evaluation available again",
        ),
    }
}

#[cfg(test)]
#[path = "cache_ignore_driver_tests.rs"]
mod tests;
