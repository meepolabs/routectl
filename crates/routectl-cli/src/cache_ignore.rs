//! The cache-ignore report: which configured Anthropic Messages and Bedrock
//! lanes kept receiving the caller's own cache markers yet reported no cache
//! activity, over one fixed window of the usage ledger.
//!
//! Report only. Nothing here strips a marker, reroutes, retries or changes a
//! capability, and nothing is logged: the daemon driver, `usage --detail` and
//! doctor each render the same [`CacheIgnoreReport`], so all three agree.
//!
//! Eligibility is a config fact: a provider is evaluated only when its kind
//! carries caller markers to the wire (anthropic-api, bedrock), it is not a
//! Bedrock provider on the Converse shape, AND its resolved cache capability
//! says cache hits are observable. A configured marker-carrying provider that
//! fails either test is listed with its reason; a ledger provider that is no
//! longer configured is never counted and never listed.

use std::path::Path;
use std::time::{Duration, Instant};

use routectl_core::provider_kind::config::{ANTHROPIC_API, BEDROCK};
use routectl_core::upstream_text_for_log;
use routectl_core::wire_check::EgressKind;
use routectl_router::{Config, ProviderEntry, cache_rules};
use routectl_usage::{
    CacheIgnoreLane, CacheIgnoreRule, DeadlineGuard, OpenError, QueryError, UsageDb,
    cache_ignore_lanes, open_readonly_fastfail,
};

use crate::commands::seat_report::safe;
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

impl CacheIgnoreLaneReport {
    /// This lane's identifiers as every surface prints them.
    pub fn identifiers(&self) -> LaneIdentifiers {
        LaneIdentifiers::render(&self.provider, self.seat.as_deref(), self.model.as_deref())
    }
}

/// A lane's provider, seat and model as doctor, `usage --detail` and the
/// daemon's log lines all print them, so the three agree on every lane.
/// Each field is neutralized with the doctor's field sanitizer. The model is
/// the served config nickname, scrubbed as upstream text first so a
/// credential- or account-shaped nickname never reaches a log or a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneIdentifiers {
    /// Provider name.
    pub provider: String,
    /// Seat, when the lane has one.
    pub seat: Option<String>,
    /// Served model, when recorded.
    pub model: Option<String>,
}

impl LaneIdentifiers {
    /// Render one lane's raw identifiers.
    pub fn render(provider: &str, seat: Option<&str>, model: Option<&str>) -> Self {
        Self {
            provider: safe(provider),
            seat: seat.map(safe),
            model: model.map(|model| safe(&upstream_text_for_log(model))),
        }
    }
}

/// Why a configured anthropic-api or bedrock provider is not evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotEvaluatedReason {
    /// Its resolved cache capability says cache hits are not observable.
    HitsNotObservable,
    /// A Bedrock provider on the Converse shape. A caller's top-level-only
    /// cache marker is forwarded inert there, so a turn the ledger records
    /// as caller-marked may have carried no working marker.
    ConverseShape,
}

impl NotEvaluatedReason {
    /// Every reason, in render order.
    pub const ALL: [Self; 2] = [Self::HitsNotObservable, Self::ConverseShape];

    /// The reason as one short phrase.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HitsNotObservable => "cache hits not observable",
            Self::ConverseShape => "Bedrock Converse forwards a top-level cache marker inert",
        }
    }
}

/// A configured provider whose lanes are not evaluated, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotEvaluatedProvider {
    /// Configured provider name.
    pub provider: String,
    /// Why its lanes are not evaluated.
    pub reason: NotEvaluatedReason,
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
    /// Configured providers whose lanes this report evaluates, sorted by
    /// name. Filled from config whatever the status.
    pub evaluated: Vec<String>,
    /// Configured anthropic-api / bedrock providers whose lanes are not
    /// evaluated, each with its reason, sorted by name. Filled from config
    /// whatever the status.
    pub not_evaluated: Vec<NotEvaluatedProvider>,
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
        let eligibility = Eligibility::of(config);
        Self {
            window: CacheIgnoreWindow::ending_at(now_ms),
            status: CacheIgnoreStatus::Unavailable(class),
            lanes: Vec::new(),
            evaluated: eligibility.evaluated(),
            not_evaluated: eligibility.not_evaluated,
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
    let eligibility = Eligibility::of(config);
    let evaluated = eligibility.evaluated();
    let Eligibility {
        rules,
        not_evaluated,
    } = eligibility;
    let queried = DeadlineGuard::install(db, deadline)
        .and_then(|_guard| cache_ignore_lanes(db, window.from_ms, window.to_ms, &rules));
    match queried {
        Ok(found) => CacheIgnoreReport {
            window,
            status: CacheIgnoreStatus::Evaluated,
            lanes: found.lanes.into_iter().map(Into::into).collect(),
            evaluated,
            not_evaluated,
            truncated: found.truncated,
        },
        Err(err) => CacheIgnoreReport {
            window,
            status: CacheIgnoreStatus::Unavailable(query_error_class(&err)),
            lanes: Vec::new(),
            evaluated,
            not_evaluated,
            truncated: false,
        },
    }
}

/// The report over the window ending at `now_ms`, read on a fresh
/// fast-fail read-only open of the ledger at `db_path`. The deadline is
/// anchored before the open, so `budget` bounds the open and the query
/// together. The connection is its own, so no caller-held guard can be
/// replaced (see [`cache_ignore_report`]).
pub fn read_cache_ignore_report(
    db_path: &Path,
    config: &Config,
    now_ms: i64,
    budget: Duration,
) -> CacheIgnoreReport {
    let deadline = Instant::now() + budget;
    match open_readonly_fastfail(db_path) {
        Ok(db) => cache_ignore_report(&db, config, now_ms, deadline),
        Err(err) => CacheIgnoreReport::open_failed(config, now_ms, &err),
    }
}

const fn query_error_class(err: &QueryError) -> &'static str {
    match err {
        QueryError::Interrupted => UNAVAILABLE_QUERY_TIMEOUT,
        QueryError::Sqlite(_) | QueryError::InvalidBucket => UNAVAILABLE_DB,
    }
}

/// The configured providers split into those the query evaluates and those
/// listed as not evaluated.
struct Eligibility {
    rules: Vec<CacheIgnoreRule>,
    not_evaluated: Vec<NotEvaluatedProvider>,
}

impl Eligibility {
    fn of(config: &Config) -> Self {
        let mut rules = Vec::new();
        let mut not_evaluated = Vec::new();
        for (name, entry) in config.providers.iter().filter(|(_, e)| carries_markers(e)) {
            if let Some(reason) = not_evaluated_reason(entry) {
                not_evaluated.push(NotEvaluatedProvider {
                    provider: name.clone(),
                    reason,
                });
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
            not_evaluated,
        }
    }

    /// The evaluated provider names, sorted (config order is by name).
    fn evaluated(&self) -> Vec<String> {
        self.rules
            .iter()
            .map(|rule| rule.provider.clone())
            .collect()
    }
}

/// Why a marker-carrying provider is not evaluated, if it is not.
fn not_evaluated_reason(entry: &ProviderEntry) -> Option<NotEvaluatedReason> {
    if matches!(entry.egress_kind(), EgressKind::BedrockConverse) {
        Some(NotEvaluatedReason::ConverseShape)
    } else if !entry.cache_capability().cache_hit_observable {
        Some(NotEvaluatedReason::HitsNotObservable)
    } else {
        None
    }
}

/// The egress kinds that put the caller's own cache markers on the wire.
fn carries_markers(entry: &ProviderEntry) -> bool {
    matches!(entry.kind_str(), ANTHROPIC_API | BEDROCK)
}

/// A served model nickname shaped like an account id after the word
/// `account`, which the upstream-text scrub redacts. Assembled at runtime.
#[cfg(test)]
pub fn key_shaped_model() -> String {
    format!("m account {}{}", "123456", "789012")
}

/// The digit run [`key_shaped_model`] carries.
#[cfg(test)]
pub const KEY_SHAPED_DIGITS: &str = concat!("123456", "789012");

#[cfg(test)]
#[path = "cache_ignore_tests.rs"]
mod tests;
