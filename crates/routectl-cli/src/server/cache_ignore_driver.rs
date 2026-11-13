//! The daemon's cache-ignore driver: re-evaluates the cache-ignore report
//! on a slow interval and logs only when a lane's verdict or the report's
//! availability changes.
//!
//! Report only. It reads the usage ledger read-only and logs; it never
//! strips a marker, reroutes, retries or changes a capability. Its whole
//! state is the previous evaluation's `ignoring` lane set plus the previous
//! status, held in the task; nothing is persisted. A restart instead derives
//! its baseline from the ledger itself (the report as of one interval ago),
//! so it neither repeats a WARN the previous process already logged nor
//! hides a change that happened while it was down.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_core::sanitize_for_log;
use routectl_router::{Config, Router};
use routectl_usage::open_readonly;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::ledger_reader::epoch_ms_now;
use crate::cache_ignore::{
    CACHE_IGNORE_WINDOW, CacheIgnoreLaneReport, CacheIgnoreReport, CacheIgnoreStatus,
    CacheIgnoreWindow, LaneVerdict, UNAVAILABLE_DB, UNAVAILABLE_NO_DATA, cache_ignore_report,
};

/// How often the driver re-evaluates. Also the look-back of the boot
/// baseline, so a restart compares against the evaluation the previous
/// process would have made one interval earlier.
pub(super) const CACHE_IGNORE_DRIVER_INTERVAL: Duration = Duration::from_mins(15);

/// Upper bound on each report's ledger query.
const EVALUATION_DEADLINE: Duration = Duration::from_secs(2);

const WINDOW_HOURS: u64 = CACHE_IGNORE_WINDOW.as_secs() / 3600;

const EVENT_IGNORING: &str = "cache_markers_without_cache_activity";
const EVENT_IGNORING_CLEARED: &str = "cache_markers_without_cache_activity_cleared";
const EVENT_UNAVAILABLE: &str = "cache_ignore_unavailable";
const EVENT_AVAILABLE: &str = "cache_ignore_available";
const EVENT_BASELINE: &str = "cache_ignore_baseline";
const EVENT_EVALUATED: &str = "cache_ignore_evaluated";

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
/// lanes reading `ignoring`, and the verdict of every other reported lane.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriverState {
    status: CacheIgnoreStatus,
    ignoring: BTreeMap<LaneKey, LaneCounts>,
    others: BTreeMap<LaneKey, LaneVerdict>,
}

impl DriverState {
    /// The boot baseline. Its status is always `Evaluated`: both boot
    /// evaluations read the ledger NOW, so an unreadable ledger is a current
    /// fault to report once, not history to suppress. An unavailable
    /// baseline contributes no lanes.
    fn baseline(report: &CacheIgnoreReport) -> Self {
        Self {
            status: CacheIgnoreStatus::Evaluated,
            ignoring: ignoring_lanes(report),
            others: other_lanes(report),
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
        }
    }

    /// A lane's verdict now that it no longer reads `ignoring`. A lane a
    /// complete report omits had no qualifying turn in the window.
    fn verdict_after_ignoring(&self, lane: &LaneKey) -> LaneVerdict {
        self.others
            .get(lane)
            .copied()
            .unwrap_or(LaneVerdict::Insufficient)
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
    LaneCleared { lane: LaneKey, verdict: LaneVerdict },
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

/// Drive the cache-ignore evaluation until `shutdown` fires.
pub(super) async fn run_cache_ignore_driver(
    router_swap: Arc<ArcSwap<Router>>,
    shutdown: watch::Receiver<()>,
) {
    drive(router_swap, shutdown, epoch_ms_now).await;
}

/// The driver loop over an injectable wall clock (epoch milliseconds),
/// read once per evaluation.
async fn drive<C: Fn() -> i64>(
    router_swap: Arc<ArcSwap<Router>>,
    mut shutdown: watch::Receiver<()>,
    clock: C,
) {
    // Skip, not the default Burst: after a suspend the missed ticks would
    // each re-read the same ledger and log nothing new.
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + CACHE_IGNORE_DRIVER_INTERVAL,
        CACHE_IGNORE_DRIVER_INTERVAL,
    );
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let now_ms = clock();
    let baseline_ms = now_ms.saturating_sub(interval_ms());
    let boot = evaluate(live_config(&router_swap), [baseline_ms, now_ms]);
    let [baseline, current] = tokio::select! {
        biased;
        _ = shutdown.changed() => return,
        reports = boot => reports,
    };
    let mut state = log_boot(&baseline, &current);

    loop {
        tokio::select! {
            // Shutdown first, so a tick ready in the same poll never starts
            // an evaluation after shutdown.
            biased;
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                let run = evaluate(live_config(&router_swap), [clock()]);
                tokio::select! {
                    // Losing to shutdown drops the join handle; the blocking
                    // read finishes on its own within its deadline.
                    biased;
                    _ = shutdown.changed() => return,
                    [report] = run => state = log_tick(&state, &report),
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

fn interval_ms() -> i64 {
    i64::try_from(CACHE_IGNORE_DRIVER_INTERVAL.as_millis()).unwrap_or(i64::MAX)
}

/// Compare the report as of one interval ago with the report as of now, log
/// the baseline line and every difference, and return the state to carry.
fn log_boot(baseline: &CacheIgnoreReport, current: &CacheIgnoreReport) -> DriverState {
    let prev = DriverState::baseline(baseline);
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

/// One report per instant, from one fresh read-only open of the config's
/// ledger, off the async runtime. Logs nothing: callers log after awaiting,
/// on their own task.
async fn evaluate<const N: usize>(config: Arc<Config>, at: [i64; N]) -> [CacheIgnoreReport; N] {
    let joined = tokio::task::spawn_blocking(move || match open_readonly(&config.usage.db_path) {
        Ok(db) => at.map(|now_ms| {
            cache_ignore_report(&db, &config, now_ms, Instant::now() + EVALUATION_DEADLINE)
        }),
        Err(err) => at.map(|now_ms| CacheIgnoreReport::open_failed(&config, now_ms, &err)),
    })
    .await;
    joined.unwrap_or_else(|_| at.map(failed_evaluation))
}

/// The report for an evaluation whose blocking task did not return.
fn failed_evaluation(now_ms: i64) -> CacheIgnoreReport {
    CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(now_ms),
        status: CacheIgnoreStatus::Unavailable(UNAVAILABLE_DB),
        lanes: Vec::new(),
        unobservable: Vec::new(),
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
        Event::LaneIgnoring { lane, counts } => tracing::warn!(
            event = EVENT_IGNORING,
            provider = %sanitize_for_log(&lane.provider),
            seat = lane.seat.as_deref().map(sanitize_for_log),
            model = lane.model.as_deref().map(sanitize_for_log),
            evidence_turns = counts.evidence_turns,
            active_turns = counts.active_turns,
            sessions = counts.sessions,
            window_hours = WINDOW_HOURS,
            "markers sent, no cache activity",
        ),
        Event::LaneCleared { lane, verdict } => tracing::info!(
            event = EVENT_IGNORING_CLEARED,
            provider = %sanitize_for_log(&lane.provider),
            seat = lane.seat.as_deref().map(sanitize_for_log),
            model = lane.model.as_deref().map(sanitize_for_log),
            verdict = verdict.as_str(),
            window_hours = WINDOW_HOURS,
            "lane no longer reads markers sent, no cache activity",
        ),
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
