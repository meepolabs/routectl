//! Process-lifetime state of the live wire check: which successful attempts
//! get checked, what every check found, and the content-free findings the
//! status surface reads.
//!
//! Three parts behind one [`WireCheckState`], shared by `Arc` and carried
//! across a hot reload so counts survive a config swap:
//!
//! - the sampler: the first `first_n` successful attempts of each
//!   (lane, client version) since boot are checked, then one in
//!   `sample_one_in`. At most [`MAX_TRACKED_VERSIONS_PER_LANE`] versions are
//!   tracked per lane; any further version shares one per-lane 1-in-M
//!   counter, so a client rotating version strings cannot buy unbounded
//!   full checks or unbounded memory;
//! - the counters `rc_wire_loss_total{lane,path_class}`,
//!   `rc_wire_check_total{lane,outcome}` and
//!   `rc_request_refused_local_total{lane}`, each key holding a count and the
//!   unix second it was last bumped. Every label is a closed enum, so the key
//!   space is bounded by the product of the enums;
//! - a ring of the newest [`FINDINGS_RING_CAPACITY`] findings, each naming a
//!   lane, leg, sanitized path and closed labels -- never a body value.
//!
//! A loss or an unrepaired rejection WARNs once per (lane, path, kind)
//! through a [`CappedWarnSet`]; the counters increment on every call
//! regardless. Locks are held only for the map or ring update and never
//! across a `tracing` call or an await.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use routectl_core::warn_dedup::{CappedWarnSet, WarnDecision};
use routectl_core::wire_check::{Lane, Leg, PathClass, RejectionProvenance, WirePath};

use crate::config::WireCheckConfig;

/// Most client versions the sampler tracks per lane for first-N checking.
pub const MAX_TRACKED_VERSIONS_PER_LANE: usize = 8;

/// Most findings the ring retains; the oldest is dropped first.
pub const FINDINGS_RING_CAPACITY: usize = 128;

/// Distinct (lane, path, kind) WARN keys remembered before the dedup set
/// stops recording. Paths are client-influenced, so the set needs a bound;
/// past it a new key stays silent while its counters still increment.
const WARN_DEDUP_CAP: usize = 1024;

const LOG_TARGET: &str = "routectl_router::wire_check";

/// How one check ended. Closed; it is the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckOutcome {
    /// The comparison finished and found no unexplained difference.
    Clean,
    /// The comparison finished and found at least one unexplained
    /// difference.
    Loss,
    /// The comparison stopped at its node budget, so it is not known to be
    /// clean.
    Budget,
    /// There was nothing complete to compare.
    Incomplete,
}

impl CheckOutcome {
    /// Every outcome, in declaration order.
    pub const ALL: [Self; 4] = [Self::Clean, Self::Loss, Self::Budget, Self::Incomplete];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Loss => "loss",
            Self::Budget => "budget",
            Self::Incomplete => "incomplete",
        }
    }
}

/// What field repair made of an upstream rejection. Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum F1Outcome {
    /// Repaired, and the repair kept the client's intent.
    Repaired,
    /// Repaired by dropping or replacing what the client asked for.
    RepairedLossy,
    /// Not repaired; the client received the rejection.
    Unrepaired,
}

impl F1Outcome {
    /// Every outcome, in declaration order.
    pub const ALL: [Self; 3] = [Self::Repaired, Self::RepairedLossy, Self::Unrepaired];

    /// Static label for logs and the status surface.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Repaired => "repaired",
            Self::RepairedLossy => "repaired_lossy",
            Self::Unrepaired => "unrepaired",
        }
    }
}

/// A counter's value and the unix second it was last bumped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterReading {
    /// Cumulative count since boot.
    pub count: u64,
    /// Unix seconds of the most recent bump.
    pub last_seen_unix_secs: u64,
}

impl CounterReading {
    /// Whether the last bump falls within `window_secs` before `now_unix_secs`.
    pub const fn seen_within(&self, window_secs: u64, now_unix_secs: u64) -> bool {
        now_unix_secs.saturating_sub(self.last_seen_unix_secs) <= window_secs
    }

    const fn bump(&mut self, now_unix_secs: u64) {
        self.count = self.count.saturating_add(1);
        self.last_seen_unix_secs = now_unix_secs;
    }
}

/// One content-free finding: a path a check flagged as lost, or a path an
/// upstream rejection named. A loss carries `class`; a rejection carries
/// `provenance` and `f1_outcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireFinding {
    /// Unix seconds when the finding was recorded.
    pub ts_unix_secs: u64,
    /// The attempt's lane.
    pub lane: Lane,
    /// Which body the path is in.
    pub leg: Leg,
    /// The sanitized pointer.
    pub path: WirePath,
    /// How the path differed, for a loss.
    pub class: Option<PathClass>,
    /// Where the rejected path came from, for a rejection.
    pub provenance: Option<RejectionProvenance>,
    /// What field repair made of the rejection, for a rejection.
    pub f1_outcome: Option<F1Outcome>,
}

/// One `rc_wire_loss_total{lane,path_class}` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireLossCounter {
    /// The lane the loss was found on.
    pub lane: Lane,
    /// How the path differed.
    pub path_class: PathClass,
    /// Count and last sighting.
    pub reading: CounterReading,
}

/// One `rc_wire_check_total{lane,outcome}` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireCheckCounter {
    /// The lane the check ran on.
    pub lane: Lane,
    /// How the check ended.
    pub outcome: CheckOutcome,
    /// Count and last sighting.
    pub reading: CounterReading,
}

/// One `rc_request_refused_local_total{lane}` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalRefusalCounter {
    /// The lane the refusal was counted on.
    pub lane: Lane,
    /// Count and last sighting.
    pub reading: CounterReading,
}

/// A point-in-time copy of every counter and the findings ring. Counter
/// lists are sorted by their labels; findings are oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireCheckSnapshot {
    /// The configured status window, in seconds.
    pub window_secs: u64,
    /// `rc_wire_loss_total` keys.
    pub loss: Vec<WireLossCounter>,
    /// `rc_wire_check_total` keys.
    pub checks: Vec<WireCheckCounter>,
    /// `rc_request_refused_local_total` keys.
    pub local_refusals: Vec<LocalRefusalCounter>,
    /// The retained findings, oldest first.
    pub findings: Vec<WireFinding>,
}

/// Sampler, counters, findings ring and WARN dedup for the live wire check.
/// Share one instance by `Arc`; every method takes `&self`.
#[derive(Debug)]
pub struct WireCheckState {
    sampler: Mutex<HashMap<Lane, LaneSampler>>,
    registry: Mutex<Registry>,
    warned: CappedWarnSet<(Lane, WirePath, WarnKind)>,
}

impl Default for WireCheckState {
    fn default() -> Self {
        Self {
            sampler: Mutex::default(),
            registry: Mutex::default(),
            warned: CappedWarnSet::new(WARN_DEDUP_CAP),
        }
    }
}

#[derive(Debug, Default)]
struct LaneSampler {
    versions: Vec<(Option<Box<str>>, u64)>,
    overflow_seen: u64,
}

#[derive(Debug, Default)]
struct Registry {
    loss: HashMap<(Lane, PathClass), CounterReading>,
    checks: HashMap<(Lane, CheckOutcome), CounterReading>,
    local_refusals: HashMap<Lane, CounterReading>,
    findings: VecDeque<WireFinding>,
}

impl Registry {
    fn push_finding(&mut self, finding: WireFinding) {
        while self.findings.len() >= FINDINGS_RING_CAPACITY {
            self.findings.pop_front();
        }
        self.findings.push_back(finding);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WarnKind {
    Loss(PathClass),
    UnrepairedRejection,
}

/// Recover a poisoned lock rather than propagate it: nothing here panics
/// while holding one, and a counter must never fail a request.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One admit decision against a running count of prior sightings: the
/// first `first_n` admit, then every `sample_one_in`-th.
const fn admit_nth(seen_before: u64, first_n: u64, sample_one_in: u64) -> bool {
    if seen_before < first_n {
        return true;
    }
    (seen_before - first_n)
        .saturating_add(1)
        .is_multiple_of(sample_one_in)
}

impl WireCheckState {
    /// Whether a successful attempt on `lane` from `client_version` gets a
    /// full check. Allocates only the first time a (lane, version) is
    /// tracked.
    pub fn admit(
        &self,
        lane: Lane,
        client_version: Option<&str>,
        config: &WireCheckConfig,
    ) -> bool {
        let first_n = u64::from(config.first_n);
        let sample_one_in = u64::from(config.sample_one_in.get());
        let mut sampler = lock(&self.sampler);
        let entry = sampler.entry(lane).or_default();
        if let Some((_, seen)) = entry
            .versions
            .iter_mut()
            .find(|(version, _)| version.as_deref() == client_version)
        {
            let admitted = admit_nth(*seen, first_n, sample_one_in);
            *seen = seen.saturating_add(1);
            return admitted;
        }
        if entry.versions.len() < MAX_TRACKED_VERSIONS_PER_LANE {
            entry.versions.push((client_version.map(Box::from), 1));
            return admit_nth(0, first_n, sample_one_in);
        }
        let seen = entry.overflow_seen;
        entry.overflow_seen = seen.saturating_add(1);
        admit_nth(seen, 0, sample_one_in)
    }

    /// Record a check that had nothing complete to compare.
    pub fn record_incomplete(&self, lane: Lane) {
        self.record_incomplete_at(lane, unix_now_secs());
    }

    fn record_incomplete_at(&self, lane: Lane, now: u64) {
        lock(&self.registry)
            .checks
            .entry((lane, CheckOutcome::Incomplete))
            .or_default()
            .bump(now);
    }

    /// Record a finished comparison on `leg`: every finding bumps its loss
    /// counter and enters the ring, and the check counts once under the
    /// outcome it earned. A `truncated` comparison is `budget` whatever it
    /// found, so it is never counted `clean`. Returns that outcome.
    pub fn record_compared<'p, I>(
        &self,
        lane: Lane,
        leg: Leg,
        findings: I,
        truncated: bool,
    ) -> CheckOutcome
    where
        I: IntoIterator<Item = (&'p WirePath, PathClass)>,
    {
        self.record_compared_at(lane, leg, findings, truncated, unix_now_secs())
    }

    fn record_compared_at<'p, I>(
        &self,
        lane: Lane,
        leg: Leg,
        findings: I,
        truncated: bool,
        now: u64,
    ) -> CheckOutcome
    where
        I: IntoIterator<Item = (&'p WirePath, PathClass)>,
    {
        let mut found_any = false;
        for (path, class) in findings {
            found_any = true;
            self.record_loss_at(lane, leg, path, class, now);
        }
        let outcome = match (truncated, found_any) {
            (true, _) => CheckOutcome::Budget,
            (false, true) => CheckOutcome::Loss,
            (false, false) => CheckOutcome::Clean,
        };
        lock(&self.registry)
            .checks
            .entry((lane, outcome))
            .or_default()
            .bump(now);
        outcome
    }

    fn record_loss_at(&self, lane: Lane, leg: Leg, path: &WirePath, class: PathClass, now: u64) {
        {
            let mut registry = lock(&self.registry);
            registry.loss.entry((lane, class)).or_default().bump(now);
            registry.push_finding(WireFinding {
                ts_unix_secs: now,
                lane,
                leg,
                path: path.clone(),
                class: Some(class),
                provenance: None,
                f1_outcome: None,
            });
        }
        if self.should_warn(lane, path, WarnKind::Loss(class)) {
            tracing::warn!(
                target: LOG_TARGET,
                lane = lane.label(),
                leg = leg.label(),
                path = path.as_str(),
                path_class = class.label(),
                "wire check found an unexplained difference"
            );
        }
    }

    /// Record a path an upstream rejection named, with its provenance and
    /// what field repair made of it. An unrepaired rejection WARNs once per
    /// (lane, path).
    pub fn record_rejection(
        &self,
        lane: Lane,
        leg: Leg,
        path: &WirePath,
        provenance: RejectionProvenance,
        f1_outcome: F1Outcome,
    ) {
        self.record_rejection_at(lane, leg, path, provenance, f1_outcome, unix_now_secs());
    }

    fn record_rejection_at(
        &self,
        lane: Lane,
        leg: Leg,
        path: &WirePath,
        provenance: RejectionProvenance,
        f1_outcome: F1Outcome,
        now: u64,
    ) {
        lock(&self.registry).push_finding(WireFinding {
            ts_unix_secs: now,
            lane,
            leg,
            path: path.clone(),
            class: None,
            provenance: Some(provenance),
            f1_outcome: Some(f1_outcome),
        });
        if f1_outcome == F1Outcome::Unrepaired
            && self.should_warn(lane, path, WarnKind::UnrepairedRejection)
        {
            tracing::warn!(
                target: LOG_TARGET,
                lane = lane.label(),
                leg = leg.label(),
                path = path.as_str(),
                provenance = provenance.label(),
                f1_outcome = f1_outcome.label(),
                "upstream rejected a request field and it was not repaired"
            );
        }
    }

    /// Count one request routectl refused locally on `lane`.
    pub fn record_local_refusal(&self, lane: Lane) {
        self.record_local_refusal_at(lane, unix_now_secs());
    }

    fn record_local_refusal_at(&self, lane: Lane, now: u64) {
        lock(&self.registry)
            .local_refusals
            .entry(lane)
            .or_default()
            .bump(now);
    }

    fn should_warn(&self, lane: Lane, path: &WirePath, kind: WarnKind) -> bool {
        match self.warned.admit((lane, path.clone(), kind)) {
            WarnDecision::Emit => true,
            WarnDecision::AlreadyWarned | WarnDecision::CapReached { notice: false } => false,
            WarnDecision::CapReached { notice: true } => {
                tracing::warn!(
                    target: LOG_TARGET,
                    cap = self.warned.cap(),
                    "wire check warning dedup is full; new paths are counted but no longer \
                     warned"
                );
                false
            }
        }
    }

    /// Copy every counter and the ring. Reads only: the sampler is not
    /// touched and nothing is reset.
    pub fn snapshot(&self, config: &WireCheckConfig) -> WireCheckSnapshot {
        let registry = lock(&self.registry);
        let mut loss: Vec<WireLossCounter> = registry
            .loss
            .iter()
            .map(|(&(lane, path_class), &reading)| WireLossCounter {
                lane,
                path_class,
                reading,
            })
            .collect();
        let mut checks: Vec<WireCheckCounter> = registry
            .checks
            .iter()
            .map(|(&(lane, outcome), &reading)| WireCheckCounter {
                lane,
                outcome,
                reading,
            })
            .collect();
        let mut local_refusals: Vec<LocalRefusalCounter> = registry
            .local_refusals
            .iter()
            .map(|(&lane, &reading)| LocalRefusalCounter { lane, reading })
            .collect();
        let findings = registry.findings.iter().cloned().collect();
        drop(registry);
        loss.sort_by_key(|c| (c.lane.label(), c.path_class.label()));
        checks.sort_by_key(|c| (c.lane.label(), c.outcome.label()));
        local_refusals.sort_by_key(|c| c.lane.label());
        WireCheckSnapshot {
            window_secs: config.window_secs.get(),
            loss,
            checks,
            local_refusals,
            findings,
        }
    }
}

/// Label-only renderings of the three counter families for the metrics log
/// line: `(lane, label, count)` triples, sorted.
#[derive(Debug)]
pub(crate) struct CounterLog {
    pub(crate) loss: Vec<(&'static str, &'static str, u64)>,
    pub(crate) checks: Vec<(&'static str, &'static str, u64)>,
    pub(crate) local_refusals: Vec<(&'static str, u64)>,
}

impl WireCheckSnapshot {
    pub(crate) fn counter_log(&self) -> CounterLog {
        CounterLog {
            loss: self
                .loss
                .iter()
                .map(|c| (c.lane.label(), c.path_class.label(), c.reading.count))
                .collect(),
            checks: self
                .checks
                .iter()
                .map(|c| (c.lane.label(), c.outcome.label(), c.reading.count))
                .collect(),
            local_refusals: self
                .local_refusals
                .iter()
                .map(|c| (c.lane.label(), c.reading.count))
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "wire_check_state_tests.rs"]
mod tests;
