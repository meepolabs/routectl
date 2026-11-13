//! The cache-ignore report: which configured Anthropic Messages and Bedrock
//! lanes kept receiving the caller's own cache markers yet reported no cache
//! activity, over one fixed window of the usage ledger.
//!
//! Report only. Nothing here strips a marker, reroutes, retries or changes a
//! capability, and nothing is logged: the daemon driver, `usage --detail` and
//! doctor each render the same [`CacheIgnoreReport`], so all three agree.
//!
//! Eligibility is a config fact: a provider is evaluated only when its kind
//! carries caller markers to the wire (anthropic-api, bedrock) AND its
//! resolved cache capability says cache hits are observable. A ledger
//! provider that is no longer configured is never counted and never listed.

use std::time::{Duration, Instant};

use routectl_core::provider_kind::config::{ANTHROPIC_API, BEDROCK};
use routectl_router::{Config, ProviderEntry, cache_rules};
use routectl_usage::{
    CacheIgnoreLane, CacheIgnoreRule, DeadlineGuard, OpenError, QueryError, UsageDb,
    cache_ignore_lanes,
};

use crate::server::ledger_reader::open_error_class;

/// The one window every reader evaluates, ending at the caller's `now`.
pub const CACHE_IGNORE_WINDOW: Duration = Duration::from_hours(6);

/// Fewest evidence turns (with no active turn) for a lane to read `ignoring`.
pub const IGNORING_MIN_EVIDENCE: i64 = 3;

/// Unavailable class: the ledger file does not exist yet.
pub const UNAVAILABLE_NO_DATA: &str = "no_data";
/// Unavailable class: the query ran past the caller's deadline.
pub const UNAVAILABLE_QUERY_TIMEOUT: &str = "query_timeout";
/// Unavailable class: any other query failure.
pub const UNAVAILABLE_DB: &str = "db_unavailable";

/// The evaluated span, `[from_ms, to_ms)` in Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheIgnoreWindow {
    /// Inclusive start.
    pub from_ms: i64,
    /// Exclusive end: the caller's `now`.
    pub to_ms: i64,
}

impl CacheIgnoreWindow {
    /// The [`CACHE_IGNORE_WINDOW`] ending at `now_ms`.
    pub fn ending_at(now_ms: i64) -> Self {
        let span_ms = i64::try_from(CACHE_IGNORE_WINDOW.as_millis()).unwrap_or(i64::MAX);
        Self {
            from_ms: now_ms.saturating_sub(span_ms),
            to_ms: now_ms,
        }
    }
}

/// Whether the ledger answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheIgnoreStatus {
    /// The lanes were computed.
    Evaluated,
    /// The ledger could not answer; the class is a fixed, path-free token.
    Unavailable(&'static str),
}

impl CacheIgnoreStatus {
    /// `evaluated` or `unavailable`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Evaluated => "evaluated",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// A lane's verdict. Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneVerdict {
    /// At least [`IGNORING_MIN_EVIDENCE`] evidence turns and no active turn:
    /// markers were sent and no cache activity came back.
    Ignoring,
    /// At least one turn reported cache activity.
    Clear,
    /// Too little evidence either way.
    Insufficient,
}

impl LaneVerdict {
    /// Fold one lane's counts.
    pub const fn from_counts(evidence_turns: i64, active_turns: i64) -> Self {
        if active_turns > 0 {
            Self::Clear
        } else if evidence_turns >= IGNORING_MIN_EVIDENCE {
            Self::Ignoring
        } else {
            Self::Insufficient
        }
    }

    /// `ignoring`, `clear` or `insufficient`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignoring => "ignoring",
            Self::Clear => "clear",
            Self::Insufficient => "insufficient",
        }
    }
}

/// One `(provider, seat, model, provider_kind)` lane with its verdict.
/// Carries no session identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIgnoreLaneReport {
    /// Configured provider name.
    pub provider: String,
    /// Seat, when the provider has seats.
    pub seat: Option<String>,
    /// Served model.
    pub model: Option<String>,
    /// Config provider kind persisted with the rows.
    pub provider_kind: String,
    /// The lane's verdict.
    pub verdict: LaneVerdict,
    /// Qualifying turns with a zero cache read and a zero cache write.
    pub evidence_turns: i64,
    /// Qualifying turns with any cache read or write.
    pub active_turns: i64,
    /// Distinct sessions among the qualifying turns.
    pub sessions: i64,
}

impl From<CacheIgnoreLane> for CacheIgnoreLaneReport {
    fn from(lane: CacheIgnoreLane) -> Self {
        Self {
            verdict: LaneVerdict::from_counts(lane.evidence_turns, lane.active_turns),
            provider: lane.provider,
            seat: lane.seat,
            model: lane.model,
            provider_kind: lane.provider_kind,
            evidence_turns: lane.evidence_turns,
            active_turns: lane.active_turns,
            sessions: lane.sessions,
        }
    }
}

/// The whole report. Plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIgnoreReport {
    /// The evaluated span.
    pub window: CacheIgnoreWindow,
    /// Whether the ledger answered; `lanes` is empty unless `Evaluated`.
    pub status: CacheIgnoreStatus,
    /// Evaluated lanes: those with no active turn first, then most evidence
    /// first.
    pub lanes: Vec<CacheIgnoreLaneReport>,
    /// Configured anthropic-api / bedrock providers whose cache hits are not
    /// observable, sorted by name. Filled from config whatever the status.
    pub unobservable: Vec<String>,
    /// The window held more lanes than the query returns. When set, a lane
    /// missing from this report may have been cut by the cap: a consumer
    /// diffing two reports must not treat its absence as a transition.
    pub truncated: bool,
}

impl CacheIgnoreReport {
    /// The report for a ledger that could not be opened. A missing ledger
    /// is [`UNAVAILABLE_NO_DATA`]; every other failure carries its
    /// path-free open class.
    pub fn open_failed(config: &Config, now_ms: i64, err: &OpenError) -> Self {
        let class = match err {
            OpenError::NoData { .. } => UNAVAILABLE_NO_DATA,
            other => open_error_class(other),
        };
        Self::unavailable(config, now_ms, class)
    }

    fn unavailable(config: &Config, now_ms: i64, class: &'static str) -> Self {
        Self {
            window: CacheIgnoreWindow::ending_at(now_ms),
            status: CacheIgnoreStatus::Unavailable(class),
            lanes: Vec::new(),
            unobservable: Eligibility::of(config).unobservable,
            truncated: false,
        }
    }
}

/// Evaluate every eligible configured provider over the window ending at
/// `now_ms`, with every statement interrupted once `deadline` passes.
///
/// Installs its own deadline on `db`, replacing any handler already there,
/// and clears it on return. Never call it on a connection under a
/// caller-held `DeadlineGuard`: that caller's later reads would run with no
/// deadline.
pub fn cache_ignore_report(
    db: &UsageDb,
    config: &Config,
    now_ms: i64,
    deadline: Instant,
) -> CacheIgnoreReport {
    let window = CacheIgnoreWindow::ending_at(now_ms);
    let Eligibility {
        rules,
        unobservable,
    } = Eligibility::of(config);
    let queried = DeadlineGuard::install(db, deadline)
        .and_then(|_guard| cache_ignore_lanes(db, window.from_ms, window.to_ms, &rules));
    match queried {
        Ok(found) => CacheIgnoreReport {
            window,
            status: CacheIgnoreStatus::Evaluated,
            lanes: found.lanes.into_iter().map(Into::into).collect(),
            unobservable,
            truncated: found.truncated,
        },
        Err(err) => CacheIgnoreReport {
            window,
            status: CacheIgnoreStatus::Unavailable(query_error_class(&err)),
            lanes: Vec::new(),
            unobservable,
            truncated: false,
        },
    }
}

const fn query_error_class(err: &QueryError) -> &'static str {
    match err {
        QueryError::Interrupted => UNAVAILABLE_QUERY_TIMEOUT,
        QueryError::Sqlite(_) | QueryError::InvalidBucket => UNAVAILABLE_DB,
    }
}

/// The configured providers split into those the query evaluates and those
/// listed as unobservable.
struct Eligibility {
    rules: Vec<CacheIgnoreRule>,
    unobservable: Vec<String>,
}

impl Eligibility {
    fn of(config: &Config) -> Self {
        let mut rules = Vec::new();
        let mut unobservable = Vec::new();
        for (name, entry) in config.providers.iter().filter(|(_, e)| carries_markers(e)) {
            if !entry.cache_capability().cache_hit_observable {
                unobservable.push(name.clone());
            } else if let Some(cache) = cache_rules(entry.kind_str()) {
                rules.push(CacheIgnoreRule {
                    provider: name.clone(),
                    min_prompt_tokens: i64::from(cache.min_prefix_tokens),
                    lifetime_ms: i64::from(cache.ttl_seconds) * 1000,
                });
            }
        }
        Self {
            rules,
            unobservable,
        }
    }
}

/// The egress kinds that put the caller's own cache markers on the wire.
fn carries_markers(entry: &ProviderEntry) -> bool {
    matches!(entry.kind_str(), ANTHROPIC_API | BEDROCK)
}

#[cfg(test)]
#[path = "cache_ignore_tests.rs"]
mod tests;
