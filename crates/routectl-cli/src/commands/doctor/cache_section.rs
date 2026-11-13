//! The `cache` section: lanes whose caller cache markers drew no cache
//! activity over the cache-ignore window, read from the usage ledger.
//!
//! Report only. A lane reading `ignoring` is a Warn, never a Fail, so the
//! section never moves the exit code. The wording stays "markers sent, no cache
//! activity": the ledger cannot tell an upstream that dropped the markers from
//! a marker lost before the wire.

use std::time::Duration;

use routectl_router::{Config, Finding, Status};

use crate::cache_ignore::{
    CACHE_IGNORE_WINDOW, CacheIgnoreLaneReport, CacheIgnoreReport, CacheIgnoreStatus, LaneVerdict,
    NotEvaluatedProvider, NotEvaluatedReason, read_cache_ignore_report,
};
use crate::commands::seat_report::safe;

use super::DoctorContext;
use super::served::GatherSources;

const SECTION: &str = "cache";

/// The read budget of the CLI `routectl doctor`: a one-shot diagnostic the
/// operator waits on, so it may take longer than a dashboard poll.
pub(super) const DISK_CACHE_READ_BUDGET: Duration = Duration::from_secs(2);

/// The read budget of the served `/status/doctor` panel: this read's own
/// share of the `/status` aggregate's `AGGREGATE_RESERVE_MS` (1000 ms, see
/// `server::status_gate`), which the health and config panels and the
/// response write also draw on, so it takes at most a quarter of it. It
/// bounds the cache-ignore open and query only. The bench
/// `query::cache_ignore::tests::a_million_row_ledger_answers_inside_the_served_budget`
/// in routectl-usage times the query alone (1M rows, 60k in the window,
/// warm OS cache) against this number.
pub(super) const SERVED_CACHE_READ_BUDGET: Duration = Duration::from_millis(250);

const _: () = assert!(
    SERVED_CACHE_READ_BUDGET.as_millis() * 4
        <= crate::server::status_gate::AGGREGATE_RESERVE_MS as u128
);

/// The read budget for a gather from `sources`.
pub(super) fn read_budget(sources: &GatherSources) -> Duration {
    match sources {
        GatherSources::Disk => DISK_CACHE_READ_BUDGET,
        GatherSources::Served(inputs) => inputs.cache_read_budget(),
    }
}

/// The cache-ignore report over the window ending at `now_ms`, read on a
/// connection of its own with `budget` bounding the open and the query.
pub(super) fn gather_cache_ignore(
    config: &Config,
    budget: Duration,
    now_ms: i64,
) -> CacheIgnoreReport {
    read_cache_ignore_report(&config.usage.db_path, config, now_ms, budget)
}

/// One Warn per `ignoring` lane, one Pass summary when the ledger answered,
/// one Pass `unavailable: <class>` when it did not, and one Pass per
/// not-evaluated reason naming the providers it applies to, when any exist.
pub(super) fn section_cache(ctx: &DoctorContext) -> Vec<Finding> {
    let report = &ctx.cache_ignore;
    let mut findings: Vec<Finding> = report
        .lanes
        .iter()
        .filter(|lane| lane.verdict == LaneVerdict::Ignoring)
        .map(ignoring_lane_finding)
        .collect();
    findings.push(status_finding(report));
    findings.extend(not_evaluated_findings(&report.not_evaluated));
    findings
}

const fn window_hours() -> u64 {
    CACHE_IGNORE_WINDOW.as_secs() / 3600
}

fn lane_name(lane: &CacheIgnoreLaneReport) -> String {
    let ids = lane.identifiers();
    let seat = ids.seat.map_or_else(String::new, |seat| format!("#{seat}"));
    let model = ids.model.unwrap_or_else(|| "-".to_string());
    format!("lane {}{seat} {model}", ids.provider)
}

/// `n` followed by `noun`, with an `s` unless `n` is one.
fn counted(n: impl Into<i64>, noun: &str) -> String {
    let n = n.into();
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{plural}")
}

fn ignoring_lane_finding(lane: &CacheIgnoreLaneReport) -> Finding {
    Finding {
        section: SECTION,
        name: lane_name(lane),
        status: Status::Warn,
        detail: format!(
            "markers sent, no cache activity: {} in {} over the last {} h \
             carried the caller's cache markers and read and wrote no cache (kind {})",
            counted(lane.evidence_turns, "turn"),
            counted(lane.sessions, "session"),
            window_hours(),
            safe(&lane.provider_kind),
        ),
        remediation: Some(format!(
            "check that the upstream behind provider `{}` caches prompts and that the caller's \
             markers reach it; if it reports no cache usage, stop evaluating it by setting both \
             keys, `cache_capability = {{ supports_top_level_cache_control = <the kind's \
             default>, cache_hit_observable = false }}`, keeping supports_top_level_cache_control \
             at the kind's default; routing is unchanged",
            lane.identifiers().provider,
        )),
    }
}

fn status_finding(report: &CacheIgnoreReport) -> Finding {
    let detail = match report.status {
        CacheIgnoreStatus::Unavailable(class) => format!("unavailable: {class}"),
        CacheIgnoreStatus::Evaluated => evaluated_detail(report),
    };
    Finding {
        section: SECTION,
        name: "cache markers".to_string(),
        status: Status::Pass,
        detail,
        remediation: None,
    }
}

fn evaluated_detail(report: &CacheIgnoreReport) -> String {
    let count = |verdict: LaneVerdict| report.lanes.iter().filter(|l| l.verdict == verdict).count();
    let truncated = if report.truncated {
        "; more lanes than listed, the rest not evaluated"
    } else {
        ""
    };
    let evaluated = i64::try_from(report.lanes.len()).unwrap_or(i64::MAX);
    format!(
        "{} evaluated over the last {} h: {} with markers sent and no cache activity, {} with \
         cache activity, {} with too few turns to judge{truncated}",
        counted(evaluated, "lane"),
        window_hours(),
        count(LaneVerdict::Ignoring),
        count(LaneVerdict::Clear),
        count(LaneVerdict::Insufficient),
    )
}

/// One Pass per reason any configured provider is not evaluated, naming
/// those providers.
fn not_evaluated_findings(providers: &[NotEvaluatedProvider]) -> Vec<Finding> {
    NotEvaluatedReason::ALL
        .into_iter()
        .filter_map(|reason| {
            let names: Vec<String> = providers
                .iter()
                .filter(|p| p.reason == reason)
                .map(|p| safe(&p.provider))
                .collect();
            (!names.is_empty()).then(|| not_evaluated_finding(reason, &names))
        })
        .collect()
}

fn not_evaluated_finding(reason: NotEvaluatedReason, names: &[String]) -> Finding {
    let (name, remediation) = match reason {
        NotEvaluatedReason::HitsNotObservable => (
            "unobservable providers",
            Some(
                "on a provider whose upstream reports cache usage, set both keys: \
                 `cache_capability = { supports_top_level_cache_control = <the kind's default>, \
                 cache_hit_observable = true }`, keeping supports_top_level_cache_control at the \
                 kind's default",
            ),
        ),
        NotEvaluatedReason::ConverseShape => ("converse providers", None),
    };
    Finding {
        section: SECTION,
        name: name.to_string(),
        status: Status::Pass,
        detail: format!("not evaluated ({}): {}", reason.as_str(), names.join(" ")),
        remediation: remediation.map(str::to_string),
    }
}

#[cfg(test)]
#[path = "cache_section_tests.rs"]
mod tests;
