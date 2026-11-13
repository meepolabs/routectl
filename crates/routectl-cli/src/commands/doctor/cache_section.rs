//! The `cache` section: lanes whose caller cache markers drew no cache
//! activity over the cache-ignore window, read from the usage ledger.
//!
//! Report only. A lane reading `ignoring` is a Warn, never a Fail, so the
//! section never moves the exit code. The wording stays "markers sent, no cache
//! activity": the ledger cannot tell an upstream that dropped the markers from
//! a marker lost before the wire.

use std::time::{Duration, Instant};

use routectl_core::upstream_text_for_log;
use routectl_router::{Config, Finding, Status};
use routectl_usage::open_readonly_fastfail;

use crate::cache_ignore::{
    CACHE_IGNORE_WINDOW, CacheIgnoreLaneReport, CacheIgnoreReport, CacheIgnoreStatus, LaneVerdict,
    cache_ignore_report,
};
use crate::commands::seat_report::safe;

use super::DoctorContext;
use super::served::GatherSources;

const SECTION: &str = "cache";

/// The read budget of the CLI `routectl doctor`: a one-shot diagnostic the
/// operator waits on, so it may take longer than a dashboard poll.
pub(super) const DISK_CACHE_READ_BUDGET: Duration = Duration::from_secs(2);

/// The read budget of the served `/status/doctor` panel. That panel shares the
/// `/status` aggregate's `AGGREGATE_RESERVE_MS` (1000 ms, see
/// `server::status_gate`) with the health and config panels and the response
/// write, so this read takes at most a quarter of it. The cache-ignore query
/// benches a 1M-row ledger with 60k rows in the window inside 250 ms.
pub(super) const SERVED_CACHE_READ_BUDGET: Duration = Duration::from_millis(250);

const _: () = assert!(
    SERVED_CACHE_READ_BUDGET.as_millis() < crate::server::status_gate::AGGREGATE_RESERVE_MS as u128
);

/// The read budget for a gather from `sources`.
pub(super) fn read_budget(sources: &GatherSources) -> Duration {
    match sources {
        GatherSources::Disk => DISK_CACHE_READ_BUDGET,
        GatherSources::Served(inputs) => inputs.cache_read_budget(),
    }
}

/// The cache-ignore report over the window ending at `now_ms`, read on a
/// connection of its own: the assembler installs its own deadline and clears
/// it, so it must never share a connection with another guarded read. The
/// deadline is anchored before the open, so `budget` bounds the whole read.
pub(super) fn gather_cache_ignore(
    config: &Config,
    budget: Duration,
    now_ms: i64,
) -> CacheIgnoreReport {
    let deadline = Instant::now() + budget;
    match open_readonly_fastfail(&config.usage.db_path) {
        Ok(db) => cache_ignore_report(&db, config, now_ms, deadline),
        Err(err) => CacheIgnoreReport::open_failed(config, now_ms, &err),
    }
}

/// One Warn per `ignoring` lane, one Pass summary when the ledger answered,
/// one Pass `unavailable: <class>` when it did not, and one Pass naming the
/// providers whose cache hits cannot be observed, when any exist.
pub(super) fn section_cache(ctx: &DoctorContext) -> Vec<Finding> {
    let report = &ctx.cache_ignore;
    let mut findings: Vec<Finding> = report
        .lanes
        .iter()
        .filter(|lane| lane.verdict == LaneVerdict::Ignoring)
        .map(ignoring_lane_finding)
        .collect();
    findings.push(status_finding(report));
    if !report.unobservable.is_empty() {
        findings.push(unobservable_finding(&report.unobservable));
    }
    findings
}

const fn window_hours() -> u64 {
    CACHE_IGNORE_WINDOW.as_secs() / 3600
}

fn lane_name(lane: &CacheIgnoreLaneReport) -> String {
    let seat = lane
        .seat
        .as_deref()
        .map_or_else(String::new, |seat| format!("#{}", safe(seat)));
    // The ledger's model can be the name a client asked for.
    let model = lane.model.as_deref().map_or_else(
        || "-".to_string(),
        |model| safe(&upstream_text_for_log(model)),
    );
    format!("lane {}{seat} {model}", safe(&lane.provider))
}

fn ignoring_lane_finding(lane: &CacheIgnoreLaneReport) -> Finding {
    Finding {
        section: SECTION,
        name: lane_name(lane),
        status: Status::Warn,
        detail: format!(
            "markers sent, no cache activity: {} turns in {} sessions over the last {} h \
             carried the caller's cache markers and read and wrote no cache (kind {})",
            lane.evidence_turns,
            lane.sessions,
            window_hours(),
            safe(&lane.provider_kind),
        ),
        remediation: Some(format!(
            "check that the upstream behind provider `{}` caches prompts and that the caller's \
             markers reach it; routing is unchanged",
            safe(&lane.provider),
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
    format!(
        "{} lanes evaluated over the last {} h: {} markers sent with no cache activity, {} with \
         cache activity, {} with too few turns to judge{truncated}",
        report.lanes.len(),
        window_hours(),
        count(LaneVerdict::Ignoring),
        count(LaneVerdict::Clear),
        count(LaneVerdict::Insufficient),
    )
}

fn unobservable_finding(providers: &[String]) -> Finding {
    let names: Vec<String> = providers.iter().map(|name| safe(name)).collect();
    Finding {
        section: SECTION,
        name: "unobservable providers".to_string(),
        status: Status::Pass,
        detail: format!(
            "cache hits are not observable on {}, so their lanes are not evaluated",
            names.join(" ")
        ),
        remediation: Some(
            "set `cache_capability` with `cache_hit_observable = true` on a provider whose \
             upstream reports cache usage"
                .to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "cache_section_tests.rs"]
mod tests;
