//! `routectl usage` -- the read surface over the usage-accounting DB.
//!
//! Reads the local SQLite usage DB (never writes it) through the same grouped
//! query `/status/query` serves, pricing each fine row at read time from the
//! effective per-field prices (`[models.X.pricing]`, then the refreshed and
//! shipped price tables), and prints an ASCII report. Subscription providers (managed-OAuth,
//! `credential` starting `oauth://`) carry no per-token dollar cost; their
//! dollar column reads `n/a (subscription)` and the quota line is the real
//! spend signal.
//!
//! The clap parsing, the window math, the pricer, and the stdout formatting
//! are deliberately separate so the correctness-critical pieces (window
//! bounds, cost bifurcation) are unit-testable against a temp DB with a fixed
//! reference instant.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use time::macros::format_description;
use time::{Date, Duration as TimeDuration, OffsetDateTime, PrimitiveDateTime, Time};

use crate::local_time::{self, CivilInstant, LocalZone, unix_ms};

use routectl_router::Config;
use routectl_router::prices::{PriceSubject, effective_pricing};
use routectl_usage::{
    AggRow, BucketSpec, CacheDecisionSummary, GroupDim, KCalibration,
    NearLosslessAttributionSummary, ObservationTotals, OpenError, QueryError, QueryMetrics,
    QuerySpec, QuotaSnapshot, Rates, ReductionSummary, RowCost, SUPPRESSED_SESSION_CAP,
    ShadowMisfireSummary, SuppressedSessions, UsageDb, WouldTrimSummary, cache_decision_summary,
    estimate_cost_tokens, k_calibration_summary, latest_quota_by_seat,
    near_lossless_attribution_summary, observation_totals, open_readonly, query, reduction_summary,
    shadow_misfire_summary, suppressed_sessions, ttfbs, would_trim_summary,
};

use crate::commands::pricing::{
    PriceSnapshot, default_price_book, is_subscription, rate_covers, rates_from_pricing,
};
use crate::commands::text_table::render_table;
use crate::commands::usage_cache_ignore::{CACHE_IGNORE_READ_BUDGET, cache_ignore_block};

/// Parsed `routectl usage` arguments, already validated by clap.
#[derive(Debug, Clone)]
pub struct UsageArgs {
    pub window: WindowFlag,
    pub since: Option<String>,
    pub until: Option<String>,
    /// The `--by` grouping; absent groups by model.
    pub by: Option<GroupDim>,
    pub detail: bool,
    pub db: Option<PathBuf>,
    /// When true, emit only the k-calibration diagnostic and return.
    pub k_calibration: bool,
}

/// The `routectl usage` window flags. clap's `window` group admits at most
/// one; [`UsageWindowArgs::selected`] is the one place a flag becomes a
/// [`WindowFlag`].
#[derive(Debug, Clone, Default, clap::Args)]
pub struct UsageWindowArgs {
    /// Usage since local midnight today.
    #[arg(long, group = "window")]
    pub today: bool,
    /// Usage since Monday 00:00 local of the current week.
    #[arg(long = "this-week", group = "window")]
    pub this_week: bool,
    /// Usage since the 1st of the current month, 00:00 local.
    #[arg(long = "this-month", group = "window")]
    pub this_month: bool,
    /// Usage over the 24 hours ending now.
    #[arg(long = "last-24h", group = "window")]
    pub last_24h: bool,
    /// Usage over the 7 days (168 hours) ending now.
    #[arg(long = "last-7d", group = "window")]
    pub last_7d: bool,
    /// All recorded usage.
    #[arg(long, group = "window")]
    pub all: bool,
}

impl UsageWindowArgs {
    /// The window the flags select; [`WindowFlag::None`] when none is set.
    pub const fn selected(&self) -> WindowFlag {
        if self.today {
            WindowFlag::Today
        } else if self.this_week {
            WindowFlag::ThisWeek
        } else if self.this_month {
            WindowFlag::ThisMonth
        } else if self.last_24h {
            WindowFlag::Last24h
        } else if self.last_7d {
            WindowFlag::Last7d
        } else if self.all {
            WindowFlag::All
        } else {
            WindowFlag::None
        }
    }
}

/// The mutually-exclusive window selector: a local calendar window, a
/// trailing fixed-length window ending now, or all history. `None` (no flag
/// and no `--since`) prints the multi-window summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFlag {
    Today,
    ThisWeek,
    ThisMonth,
    Last24h,
    Last7d,
    All,
    None,
}

/// Errors surfaced by the usage command's core. `NoData` is NOT an error
/// to the user -- `run` turns it into a friendly message and exit 0.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("bad date `{0}` (expected YYYY-MM-DD)")]
    BadDate(String),
    #[error("ambiguous local time for `{0}`")]
    AmbiguousTime(String),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error(
        "--opening-accuracy needs an explicit window: --today, --this-week, --this-month, \
         --last-24h, --last-7d, --all, or --since D [--until E]"
    )]
    OpeningAccuracyNeedsWindow,
    #[error("usage db unavailable at {path}: {source}")]
    Unavailable {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

// --- humanizing formatters ---------------------------------------------

const THOUSAND: i64 = 1_000;
const MILLION: i64 = 1_000_000;
const BILLION: i64 = 1_000_000_000;
const COMPACT_COUNT_FLOOR: i64 = 10_000;
const MS_PER_SECOND: i64 = 1_000;
const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// Compact a count: below 10_000 the plain integer; otherwise a one-decimal
/// figure with a K/M/B suffix, trimming a trailing `.0`.
pub(crate) fn human_count(n: i64) -> String {
    if n < COMPACT_COUNT_FLOOR {
        return n.to_string();
    }
    let (value, suffix) = if n >= BILLION {
        (n as f64 / BILLION as f64, "B")
    } else if n >= MILLION {
        (n as f64 / MILLION as f64, "M")
    } else {
        (n as f64 / THOUSAND as f64, "K")
    };
    let body = format!("{value:.1}");
    let trimmed = body.strip_suffix(".0").unwrap_or(&body);
    format!("{trimmed}{suffix}")
}

/// The ms threshold at which the one-decimal seconds format would round up to
/// `60.0s`; at or above this we use the minute path so no value renders `60.Xs`.
const SECONDS_ROUND_UP_FLOOR: i64 = 59_950;

/// Humanize a duration in ms: `<1s` as `Nms`, `<1m` as one-decimal seconds,
/// `<1h` as `MmSSs`, else `HhMMm`. A value that would round to `60.0s` is
/// promoted to the minute path (`1m00s`) so seconds never render as `>= 60.0s`.
fn human_ms(ms: i64) -> String {
    if ms < MS_PER_SECOND {
        return format!("{ms}ms");
    }
    if ms < SECONDS_ROUND_UP_FLOOR {
        return format!("{:.1}s", ms as f64 / MS_PER_SECOND as f64);
    }
    if ms < MS_PER_MINUTE {
        // The one-decimal seconds format would round this up to `60.0s`; show
        // it on the minute path instead so seconds never render as `>= 60.0s`.
        return "1m00s".to_string();
    }
    if ms < MS_PER_HOUR {
        let minutes = ms / MS_PER_MINUTE;
        let seconds = (ms % MS_PER_MINUTE) / MS_PER_SECOND;
        return format!("{minutes}m{seconds:02}s");
    }
    let hours = ms / MS_PER_HOUR;
    let minutes = (ms % MS_PER_HOUR) / MS_PER_MINUTE;
    format!("{hours}h{minutes:02}m")
}

// --- window math --------------------------------------------------------

/// Half-open `[from_ms, to_ms)` epoch-ms window. The upper bound is
/// computed as `now_ms + 1` so the latest row (stamped at `now_ms`) falls
/// inside the half-open aggregate query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBounds {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// Forward-probe step and cap used to recover the next valid local instant
/// when a calendar midnight lands in a DST spring-forward gap.
const DST_PROBE_STEP_MIN: i64 = 15;
const DST_PROBE_CAP_MIN: i64 = 180;

/// Resolve a local midnight to a concrete instant, handling every case
/// explicitly so the window can never silently collapse:
/// - one instant -> that instant.
/// - two instants (a fall-back at midnight) -> the later one, see
///   [`CivilInstant::latest`].
/// - none (spring-forward gap, midnight does not exist) -> debug, then probe
///   forward in small steps to the next valid local instant; only if that
///   fails within the cap fall back to `now`.
fn resolve_local_midnight(
    zone: &LocalZone,
    naive: PrimitiveDateTime,
    now: OffsetDateTime,
) -> OffsetDateTime {
    match zone.resolve(naive) {
        CivilInstant::Single(t) => t,
        CivilInstant::Ambiguous { latest, .. } => latest,
        CivilInstant::Skipped => {
            tracing::debug!(
                naive_midnight = %civil_text(naive),
                "local midnight falls in a DST spring-forward gap; probing forward for the next valid instant"
            );
            let mut offset = DST_PROBE_STEP_MIN;
            while offset <= DST_PROBE_CAP_MIN {
                let probe = naive + TimeDuration::minutes(offset);
                if let Some(t) = zone.resolve(probe).latest() {
                    return t;
                }
                offset += DST_PROBE_STEP_MIN;
            }
            now
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` of a wall-clock reading; `time`'s own Display is
/// `YYYY-MM-DD H:MM:SS.f`, which is not the text routectl logs.
fn civil_text(civil: PrimitiveDateTime) -> String {
    let unix_s = civil.assume_utc().unix_timestamp();
    local_time::calendar_text(
        unix_s,
        time::UtcOffset::UTC,
        local_time::TextLayout::Seconds,
    )
    .unwrap_or_else(|| unix_s.to_string())
}

/// Local midnight (00:00:00.000) on the calendar day the epoch-ms instant
/// `ts_ms` falls on, resolved through the same DST handling as every other
/// window edge.
///
/// `None` only when `ts_ms` is outside the representable date range, which no
/// ledger timestamp reaches; the caller treats that as nothing to bucket rather
/// than substituting a wrong anchor.
fn local_midnight_of(zone: &LocalZone, ts_ms: i64, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let instant = zone.at_unix_ms(ts_ms)?;
    Some(resolve_local_midnight(
        zone,
        local_time::midnight(instant.date()),
        now,
    ))
}

/// Local midnight (00:00:00.000) on the calendar day of `now`.
fn local_midnight(zone: &LocalZone, now: OffsetDateTime) -> OffsetDateTime {
    let local = zone.to_local(now);
    resolve_local_midnight(zone, local_time::midnight(local.date()), now)
}

/// Local midnight on the Monday of `now`'s ISO week (week starts Monday).
fn local_week_start(zone: &LocalZone, now: OffsetDateTime) -> OffsetDateTime {
    let today = zone.to_local(now).date();
    let since_monday = i64::from(today.weekday().number_days_from_monday());
    let monday = today
        .checked_sub(TimeDuration::days(since_monday))
        .unwrap_or(today);
    resolve_local_midnight(zone, local_time::midnight(monday), now)
}

/// Local midnight on the 1st of `now`'s month.
fn local_month_start(zone: &LocalZone, now: OffsetDateTime) -> OffsetDateTime {
    let today = zone.to_local(now).date();
    let first = today.replace_day(1).unwrap_or(today);
    resolve_local_midnight(zone, local_time::midnight(first), now)
}

/// Bounds for a window flag, anchored at an explicit `now` so tests are
/// deterministic. The binary passes the loaded host zone and the current
/// instant.
///
/// The trailing windows are exact durations ending at `now` and never consult
/// `zone`: across a daylight-saving change "last 24 hours" stays 24 hours,
/// while "today" spans the local day's real 23 or 25.
pub fn window_bounds(zone: &LocalZone, flag: WindowFlag, now: OffsetDateTime) -> WindowBounds {
    let to_ms = unix_ms(now) + 1;
    let from = match flag {
        WindowFlag::Today | WindowFlag::None => local_midnight(zone, now),
        WindowFlag::ThisWeek => local_week_start(zone, now),
        WindowFlag::ThisMonth => local_month_start(zone, now),
        WindowFlag::Last24h => return trailing_bounds(to_ms, MS_PER_DAY),
        WindowFlag::Last7d => return trailing_bounds(to_ms, 7 * MS_PER_DAY),
        WindowFlag::All => return WindowBounds { from_ms: 0, to_ms },
    };
    WindowBounds {
        from_ms: unix_ms(from),
        to_ms,
    }
}

/// `[to_ms - span_ms, to_ms)`.
const fn trailing_bounds(to_ms: i64, span_ms: i64) -> WindowBounds {
    WindowBounds {
        from_ms: to_ms - span_ms,
        to_ms,
    }
}

// --- time-series bucket resolution --------------------------------------

/// Ceiling on how many buckets one series may carry. A wider window does not
/// return more points: it returns proportionally WIDER ones, so a response is
/// bounded no matter how much history the ledger holds.
pub const MAX_BUCKETS: usize = 1000;

/// The granularity a series is requested at. The resolved width can be a
/// MULTIPLE of the requested unit once the cap widens the grid, so a reader
/// takes the width from the resolved [`BucketSpec`], never from this token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketUnit {
    Hour,
    Day,
}

impl BucketUnit {
    /// The unwidened width of one bucket, in milliseconds. Also the width an
    /// EMPTY series reports, since nothing was measured to widen against.
    pub const fn base_width_ms(self) -> i64 {
        match self {
            Self::Hour => MS_PER_HOUR,
            Self::Day => MS_PER_DAY,
        }
    }
}

/// Resolve a requested granularity into the grid a series is folded over: the
/// anchor its buckets are measured from, and the width/count that cover
/// `[anchor, to_ms)` without exceeding [`MAX_BUCKETS`].
///
/// `from_ms == 0` is the all-time window, whose literal lower bound is the 1970
/// epoch; bucketing from there would spend the whole grid on decades of empty
/// history, so it re-anchors to the local midnight of the ledger's earliest row.
/// The re-anchor is clamped to `from_ms`, so it can only ever move the lower
/// bound FORWARD past rows the window already excluded -- the row set stays
/// identical and the groups and totals folded from the same scan are unchanged.
/// The clamp is load-bearing: local midnight of a small positive timestamp is
/// itself negative in a positive-UTC-offset zone.
///
/// `None` means there is nothing to bucket: an empty ledger under all-time (no
/// earliest row to anchor on), or a window whose span is not positive.
///
/// Pure by construction -- `now` and `first_row_ms` are passed in, so the whole
/// widening ladder and every DST edge are exercised without a database.
///
/// The returned grid always satisfies the leaf's contract: `width_ms > 0`,
/// `count` in `1..=MAX_BUCKETS`, and a last bucket start that fits an `i64`.
/// Every intermediate is computed in `i128` so neither the span nor the widened
/// width can wrap on the way there.
pub fn resolve_bucket(
    zone: &LocalZone,
    unit: BucketUnit,
    from_ms: i64,
    to_ms: i64,
    first_row_ms: Option<i64>,
    now: OffsetDateTime,
) -> Option<(i64, BucketSpec)> {
    let anchor = if from_ms == 0 {
        unix_ms(local_midnight_of(zone, first_row_ms?, now)?).max(from_ms)
    } else {
        from_ms
    };
    let span = i128::from(to_ms) - i128::from(anchor);
    if span <= 0 {
        return None;
    }
    let base = i128::from(unit.base_width_ms());
    let mut width = base;
    let mut count = div_ceil_positive(span, width);
    if count > MAX_BUCKETS as i128 {
        // Widen to the smallest whole multiple of the requested unit that fits
        // the cap. `hour` past ~41 days lands on a whole day and beyond as
        // multiples of it, so no separate unit ladder is needed.
        width = base * div_ceil_positive(count, MAX_BUCKETS as i128);
        count = div_ceil_positive(span, width);
    }
    // `count * width >= span` by construction and `count <= MAX_BUCKETS`, so the
    // last start is at most `anchor + span`, i.e. at most `to_ms` -- an i64.
    let width_ms = i64::try_from(width).ok()?;
    let count = usize::try_from(count).ok()?;
    Some((anchor, BucketSpec { width_ms, count }))
}

/// Ceiling division for strictly positive operands. `i128::div_ceil` is not
/// stable on the pinned toolchain, and the signed-operand rounding rules do not
/// apply here: both arguments are positive by construction at every call site.
const fn div_ceil_positive(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1) / denominator
}

/// Bounds for an ad-hoc `--since D [--until E]` range in `zone`, anchored at
/// `now` for the open-ended upper bound. `from` is local midnight of `D`; `to`
/// is the last millisecond of `E` (23:59:59.999 local) made inclusive via
/// `+1`, or `now + 1` when `--until` is omitted.
pub fn since_bounds(
    zone: &LocalZone,
    since: &str,
    until: Option<&str>,
    now: OffsetDateTime,
) -> Result<WindowBounds, UsageError> {
    let from = parse_local_date_start(zone, since)?;
    let to_ms = match until {
        Some(e) => parse_local_date_end(zone, e)? + 1,
        None => unix_ms(now) + 1,
    };
    Ok(WindowBounds {
        from_ms: unix_ms(from),
        to_ms,
    })
}

/// A `YYYY-MM-DD` date. Leading whitespace and unpadded fields are accepted,
/// matching the grammar this flag has always taken.
fn parse_naive_date(s: &str) -> Result<Date, UsageError> {
    let format = format_description!("[year padding:none]-[month padding:none]-[day padding:none]");
    let compact: String = s
        .trim_start()
        .split('-')
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("-");
    Date::parse(&compact, format).map_err(|_| UsageError::BadDate(s.to_string()))
}

/// Local midnight at the start of `s` (YYYY-MM-DD).
fn parse_local_date_start(zone: &LocalZone, s: &str) -> Result<OffsetDateTime, UsageError> {
    let date = parse_naive_date(s)?;
    zone.resolve(local_time::midnight(date))
        .latest()
        .ok_or_else(|| UsageError::AmbiguousTime(s.to_string()))
}

/// Epoch-ms at 23:59:59.999 local on `s` (YYYY-MM-DD).
fn parse_local_date_end(zone: &LocalZone, s: &str) -> Result<i64, UsageError> {
    let date = parse_naive_date(s)?;
    let end = PrimitiveDateTime::new(
        date,
        Time::MAX
            .replace_nanosecond(999_000_000)
            .unwrap_or(Time::MAX),
    );
    zone.resolve(end)
        .latest()
        .map(unix_ms)
        .ok_or_else(|| UsageError::AmbiguousTime(s.to_string()))
}

// --- rollup + cost ------------------------------------------------------

/// One display row: a group's figures exactly as the shared grouped query
/// folded them, plus the two things that query does not carry -- whether any
/// of the group's rows were subscription usage (the `+sub` cost marker) and
/// the `--detail` time-to-first-token percentiles.
#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub label: String,
    /// The group's metrics as `/status/query` reports them.
    pub metrics: QueryMetrics,
    /// True when at least one of the group's rows was priced as subscription
    /// usage. The query's `cost_status` cannot say this for a `partial` group,
    /// whose mix may be priced plus unpriced rather than priced plus
    /// subscription.
    pub any_subscription: bool,
    /// Nearest-rank TTFT percentiles over the group's streaming successes;
    /// populated only under `--detail`.
    pub ttft_p50_ms: Option<i64>,
    pub ttft_p95_ms: Option<i64>,
}

/// The full report for one window: the display rows, the window total, the
/// latest quota snapshot per seat (subscription spend signal), and the
/// `--detail` diagnostics.
#[derive(Debug, Clone)]
pub struct WindowReport {
    pub title: String,
    pub by_header: &'static str,
    pub detail: bool,
    /// One row per group, in label order.
    pub groups: Vec<DisplayRow>,
    /// The window total, labelled `total`. Kept apart from `groups` so an
    /// alias or seat literally named "total" is never mistaken for it.
    pub total: DisplayRow,
    /// One latest quota snapshot per credential seat. Not windowed (a quota
    /// snapshot is a seat's latest known state); empty when nothing reported.
    pub quota: Vec<QuotaSnapshot>,
    /// Steady-state would-trim opportunity over the window (advisory; only
    /// populated and surfaced under `--detail`).
    pub would_trim: WouldTrimSummary,
    /// Shadow misfire monitor summary over the window (advisory; only
    /// populated and surfaced under `--detail`).
    pub shadow_misfire: ShadowMisfireSummary,
    /// Near-lossless per-heuristic attribution over the window (advisory;
    /// only populated and surfaced under `--detail`). Restricted at the
    /// query layer to `would_trim_recorder_version IS NOT NULL`.
    pub near_lossless_attribution: NearLosslessAttributionSummary,
    /// Lossless-minifier outcome summary over the window (only populated and
    /// surfaced under `--detail`). Its ratios hold only for windows the usage
    /// channel dropped nothing in -- the rendered block carries that caveat.
    pub reduction: ReductionSummary,
    /// Cache-breakpoint decision summary over the window (per-region decided /
    /// emitted counts plus the prefix-epoch breakdown; only populated and
    /// surfaced under `--detail`).
    pub cache_decision: CacheDecisionSummary,
    /// The window's K-suppressed `(session, provider_kind, model)` triples,
    /// bounded and newest-first (only populated and surfaced under `--detail`).
    /// The detection surface for a session wrongly latched off caching, whose
    /// cost accrues at HTTP 200 with every health signal green.
    pub suppressed_sessions: SuppressedSessions,
    /// The window's ledger observation totals: the dispatch walk counts that
    /// the walk line prints, and the gap fields that make them a lower bound.
    pub observation: ObservationTotals,
}

impl WindowReport {
    /// Every display row in render order: the groups, then the total.
    pub fn rows(&self) -> impl Iterator<Item = &DisplayRow> {
        self.groups.iter().chain(std::iter::once(&self.total))
    }
}

/// The provider kind whose reasoning tokens are DISJOINT from its output count
/// (Gemini reports `thoughtsTokenCount` separately from
/// `candidatesTokenCount`), so it alone prices reasoning as its own dimension.
/// Matches `ProviderEntry::kind_str`, which is also the token the ledger
/// persists per row.
const DISJOINT_REASONING_KIND: &str = routectl_core::provider_kind::config::GEMINI;

/// The provider kinds whose reasoning tokens are already INSIDE their output
/// count, so pricing reasoning again would double-charge.
///
/// Enumerated rather than treated as "everything that is not
/// [`DISJOINT_REASONING_KIND`]", and that distinction is load-bearing: a
/// catchall would classify a FUTURE provider kind as subsumed and silently drop
/// its reasoning tokens from the bill. Failing closed instead surfaces the new
/// kind as `Unpriced`, and adding it to one of these two lists is then the
/// deliberate act that starts pricing it. Together with
/// [`DISJOINT_REASONING_KIND`] this must cover every `ProviderEntry::kind_str`
/// token; the test below pins that it does.
const SUBSUMED_REASONING_KINDS: [&str; 4] = [
    routectl_core::provider_kind::config::OPENAI_COMPAT,
    routectl_core::provider_kind::config::ANTHROPIC_API,
    routectl_core::provider_kind::config::BEDROCK,
    routectl_core::provider_kind::config::OPENAI_RESPONSES,
];

/// How a row's reasoning tokens relate to its output count, resolved from the
/// kind PERSISTED with the row rather than from current config -- an operator
/// who re-kinds a provider under the same name must not retroactively change
/// what historical rows were.
enum ReasoningStructure {
    /// Reasoning is counted separately from output and bills at the output
    /// rate.
    Disjoint,
    /// Reasoning is already inside the output count; pricing it again would
    /// double-charge.
    Subsumed,
    /// The row carries no kind (pre-dispatch, or history written before the
    /// column was populated), or carries one this build does not recognize, so
    /// neither answer can be justified.
    Unknown,
}

/// Classify a row's reasoning structure from its persisted `provider_kind`.
///
/// An unrecognized non-NULL token is `Unknown`, NOT `Subsumed`: a kind this
/// build has never heard of may well report reasoning disjointly, and guessing
/// subsumed would drop those tokens from the bill with no signal.
fn reasoning_structure(provider_kind: Option<&str>) -> ReasoningStructure {
    match provider_kind {
        Some(DISJOINT_REASONING_KIND) => ReasoningStructure::Disjoint,
        Some(kind) if SUBSUMED_REASONING_KINDS.contains(&kind) => ReasoningStructure::Subsumed,
        None | Some(_) => ReasoningStructure::Unknown,
    }
}

/// Classify and cost one fine-grained row. Subscription detection runs
/// first (it overrides pricing); then a priced API-key row prices its
/// summed tokens; everything else is unpriced.
///
/// A subscription row carries no dollar cost, but it does carry the
/// API-EQUIVALENT value of its usage when every dimension it used resolved a
/// rate -- computed through the SAME rate resolution and token math a priced
/// row goes through, so the two can never disagree on reasoning structure,
/// cache-read basis, or persisted-kind keying. That equivalent is
/// complete-or-absent (see [`rates_cover_used_dimensions`]), and so is a
/// priced row (see `priced_usd`).
///
/// Rates resolve through `routectl_router::prices::effective_pricing` against
/// `prices`, field by field: `[models.X.pricing]` on the served model, then
/// the refreshed and shipped tables.
/// The subject is the served `[providers]` entry the row recorded (never a
/// pool), and the row's persisted kind selects a first-party table provider
/// when that entry no longer exists. No price for any field leaves the row
/// `Unpriced` (or, on the subscription arm, with no equivalent), never `$0`.
///
/// Cache writes price per lifetime. Written tokens with no reported lifetime,
/// or a lifetime split summing above its total, leave the row `Unpriced` (or
/// with no equivalent) whatever priced it: no rate can be matched to them.
///
/// Reasoning is priced from the row's PERSISTED `provider_kind`, so a provider
/// whose kind an operator later changed keeps each era priced by what it was.
/// A row with NO persisted kind and NONZERO reasoning tokens fails CLOSED as
/// `Unpriced`: both answers (charge or do not charge reasoning) are wrong for
/// one of the two possible structures, and guessing from current config is the
/// defect this avoids. A kindless row with ZERO reasoning tokens still prices,
/// since its structure cannot affect the result.
///
/// Returns the usage crate's `RowCost` verdict, which is also what the grouped
/// query layer's pricing closure must yield -- so the CLI report and the
/// `/status/query` endpoint price a row through exactly one function.
pub(crate) fn cost_for_row(config: &Config, prices: &PriceSnapshot, row: &AggRow) -> RowCost {
    let Some(provider) = row.key.provider.as_deref() else {
        return RowCost::Unpriced;
    };
    if is_subscription(config, provider) {
        return RowCost::Subscription(equivalent_usd(config, prices, row, provider));
    }
    match priced_usd(config, prices, row, provider) {
        Some(usd) => RowCost::Priced(usd),
        None => RowCost::Unpriced,
    }
}

/// The real dollar cost of one non-subscription row, or `None` when no usable
/// price exists.
///
/// A row is complete-or-absent: a dimension it used with no rate leaves the
/// row unpriced. A table gap or a partial `[models.X.pricing]` block is not a
/// statement that the dimension is free.
fn priced_usd(
    config: &Config,
    prices: &PriceSnapshot,
    row: &AggRow,
    provider: &str,
) -> Option<f64> {
    let resolved = resolve_row_rates(config, prices, row, provider)?;
    if !rates_cover_used_dimensions(&resolved, row) {
        return None;
    }
    price_row(&resolved, row)
}

/// The API-equivalent dollar value of one SUBSCRIPTION row: the same rate
/// resolution and token math a priced row uses, wrapped in the
/// complete-or-absent rule.
///
/// The completeness wrapper applies here whatever priced the row. A
/// subscription operator's workload runs at a high cache-hit rate, so a figure
/// that silently dropped an unpriced cache dimension would understate the value
/// by most of its magnitude -- worse than absent for exactly the workload this
/// number serves.
fn equivalent_usd(
    config: &Config,
    prices: &PriceSnapshot,
    row: &AggRow,
    provider: &str,
) -> Option<f64> {
    let resolved = resolve_row_rates(config, prices, row, provider)?;
    if !rates_cover_used_dimensions(&resolved, row) {
        return None;
    }
    // A non-finite product reads as absent rather than propagating into the
    // equivalent channel's sum, where it would poison every other row's value.
    price_row(&resolved, row).filter(|usd| usd.is_finite())
}

/// A row's resolved rate table plus whether its reasoning tokens bill as their
/// own dimension.
struct RowRates {
    rates: Rates,
    /// The row's cache-write tokens per lifetime in seconds.
    cache_writes: BTreeMap<u32, i64>,
    /// True when the row's reasoning tokens are DISJOINT from its output count
    /// and therefore a separately billed dimension. False when they are already
    /// inside output, where a reasoning rate would double-charge.
    reasoning_billed: bool,
}

/// Resolve the rate table for one row and settle its reasoning structure.
/// `None` means no price can be justified for the row at all: no upstream, no
/// effective pricing, an unknown persisted kind on a row that reported
/// reasoning tokens, or cache writes that cannot be priced by lifetime.
fn resolve_row_rates(
    config: &Config,
    prices: &PriceSnapshot,
    row: &AggRow,
    provider: &str,
) -> Option<RowRates> {
    let cache_writes = lifetime_priceable_writes(row)?;
    let subject = PriceSubject {
        model: row.key.model.as_deref(),
        provider,
        upstream: row.key.upstream.as_deref()?,
        persisted_kind: row.key.provider_kind.as_deref(),
    };
    let (pricing, _) = effective_pricing(config, &subject, prices.refreshed(), prices.shipped())?;
    let mut rates = rates_from_pricing(&pricing);
    let mut reasoning_billed = false;
    match reasoning_structure(row.key.provider_kind.as_deref()) {
        // A disjoint-reasoning row bills its thinking tokens at the output
        // rate; a subsumed one already counted them inside output, so pricing
        // them again would double-charge.
        ReasoningStructure::Disjoint => {
            rates.reasoning_per_mtok = rates.output_per_mtok;
            reasoning_billed = true;
        }
        ReasoningStructure::Subsumed => {}
        ReasoningStructure::Unknown if row.reasoning_tokens != 0 => return None,
        ReasoningStructure::Unknown => {}
    }
    Some(RowRates {
        rates,
        cache_writes,
        reasoning_billed,
    })
}

/// A row's cache-write tokens per lifetime, or `None` when some of them
/// cannot be priced by lifetime: tokens with no reported lifetime, or a split
/// summing above its total. A row read without its lifetimes has every
/// written token unknown.
fn lifetime_priceable_writes(row: &AggRow) -> Option<BTreeMap<u32, i64>> {
    let Some(lifetimes) = &row.cache_write_lifetimes else {
        return (row.cache_write_tokens == 0).then(BTreeMap::new);
    };
    if lifetimes.invalid_split || lifetimes.unknown_tokens > 0 {
        return None;
    }
    Some(lifetimes.tokens_by_lifetime.clone())
}

/// The token math shared by the real price and the API-equivalent value.
///
/// cache_read is billed PER TURN, so the cost basis is the summed cache-read
/// volume (`cache_read_billed`), not the peak. The peak / avg are display-only
/// context SIZE and must NOT drive cost; input / output / cache writes are real
/// summed flows.
fn price_row(resolved: &RowRates, row: &AggRow) -> Option<f64> {
    estimate_cost_tokens(
        row.input_tokens,
        row.output_tokens,
        row.reasoning_tokens,
        row.cache_read_billed,
        &resolved.cache_writes,
        &resolved.rates,
    )
    .map(|b| b.total_usd)
}

/// Whether every dimension this row actually USED resolved a rate -- the
/// complete-or-absent test for an API-equivalent value.
///
/// A dimension with a zero count is irrelevant (it contributes nothing either
/// way). A dimension with a nonzero count and no rate makes the whole figure a
/// partial one, and a partial equivalent misstates value rather than
/// approximating it. Presence decides, per [`rate_covers`]: a known `0` rate
/// prices its dimension at nothing and keeps the figure complete. Reasoning counts as a dimension only when it bills
/// separately; cache reads are weighed as the BILLED volume, never the peak;
/// each cache-write lifetime is its own dimension.
fn rates_cover_used_dimensions(resolved: &RowRates, row: &AggRow) -> bool {
    let rates = &resolved.rates;
    // A subsumed-reasoning row's thinking tokens are inside its output count, so
    // the output pair below already covers them; only a disjoint row weighs
    // reasoning as its own dimension.
    let reasoning_tokens = if resolved.reasoning_billed {
        row.reasoning_tokens
    } else {
        0
    };
    let writes = resolved
        .cache_writes
        .iter()
        .map(|(seconds, tokens)| (*tokens, rates.cache_write_per_mtok.get(seconds).copied()));
    [
        (row.input_tokens, rates.input_per_mtok),
        (row.output_tokens, rates.output_per_mtok),
        (reasoning_tokens, rates.reasoning_per_mtok),
        (row.cache_read_billed, rates.cache_read_per_mtok),
    ]
    .into_iter()
    .chain(writes)
    .all(|(tokens, rate)| tokens == 0 || rate_covers(rate))
}

/// A nearest-rank TTFT p50 and p95 in ms, each `None` without samples.
type TtftPair = (Option<i64>, Option<i64>);

/// The `--detail` TTFT percentiles: per group label, and over the whole
/// window. The window figure is kept apart from the labelled ones so a group
/// labelled "total" cannot share its samples.
#[derive(Debug, Default)]
struct TtftPercentiles {
    by_label: BTreeMap<String, TtftPair>,
    total: TtftPair,
}

impl TtftPercentiles {
    fn for_label(&self, label: &str) -> TtftPair {
        self.by_label.get(label).copied().unwrap_or((None, None))
    }
}

/// How far ahead the report's read deadline sits. The command is attended and
/// has no latency budget (the operator's interrupt is its limit), but the
/// shared query always runs under a deadline, so the CLI hands it one no real
/// read reaches.
const REPORT_READ_HORIZON: Duration = Duration::from_hours(24);

/// Build the full window report: the shared grouped query over the window,
/// grouped by `by` (model when absent) and priced row by row through
/// `cost_for_row` -- the same query and pricer `/status/query` runs -- plus
/// the quota lines and, under `--detail`, the diagnostics. No clap, no stdout.
pub fn build_window_report(
    db: &mut UsageDb,
    config: &Config,
    prices: &PriceSnapshot,
    title: String,
    bounds: WindowBounds,
    by: Option<GroupDim>,
    detail: bool,
) -> Result<WindowReport, UsageError> {
    let dim = by.unwrap_or(GroupDim::Model);
    let spec = QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by: dim,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let subscription_labels = RefCell::new(BTreeSet::new());
    let price = |row: &AggRow| {
        let cost = cost_for_row(config, prices, row);
        if matches!(cost, RowCost::Subscription(_)) {
            subscription_labels.borrow_mut().insert(dim.label(&row.key));
        }
        cost
    };
    let result = query(db, &spec, price, Instant::now() + REPORT_READ_HORIZON)?;
    let subscription_labels = subscription_labels.into_inner();

    let ttft = if detail {
        compute_ttft(db, bounds, dim)?
    } else {
        TtftPercentiles::default()
    };

    let groups = result
        .groups
        .into_iter()
        .map(|group| {
            let (ttft_p50_ms, ttft_p95_ms) = ttft.for_label(&group.label);
            DisplayRow {
                any_subscription: subscription_labels.contains(&group.label),
                label: group.label,
                metrics: group.metrics,
                ttft_p50_ms,
                ttft_p95_ms,
            }
        })
        .collect();
    let total = DisplayRow {
        label: "total".to_string(),
        metrics: result.totals,
        any_subscription: !subscription_labels.is_empty(),
        ttft_p50_ms: ttft.total.0,
        ttft_p95_ms: ttft.total.1,
    };

    let diagnostics = if detail {
        detail_diagnostics(db, bounds)?
    } else {
        DetailDiagnostics::default()
    };

    Ok(WindowReport {
        title,
        by_header: dim.as_str(),
        detail,
        groups,
        total,
        quota: latest_quota_by_seat(db)?,
        would_trim: diagnostics.would_trim,
        shadow_misfire: diagnostics.shadow_misfire,
        near_lossless_attribution: diagnostics.near_lossless_attribution,
        reduction: diagnostics.reduction,
        cache_decision: diagnostics.cache_decision,
        suppressed_sessions: diagnostics.suppressed_sessions,
        observation: observation_totals(db, bounds.from_ms, bounds.to_ms)?,
    })
}

/// The `--detail`-only window summaries; every one stays at its default
/// (and unrendered) without `--detail`.
#[derive(Default)]
struct DetailDiagnostics {
    would_trim: WouldTrimSummary,
    shadow_misfire: ShadowMisfireSummary,
    near_lossless_attribution: NearLosslessAttributionSummary,
    reduction: ReductionSummary,
    cache_decision: CacheDecisionSummary,
    suppressed_sessions: SuppressedSessions,
}

/// Read the `--detail` window summaries.
fn detail_diagnostics(db: &UsageDb, bounds: WindowBounds) -> Result<DetailDiagnostics, UsageError> {
    let (from_ms, to_ms) = (bounds.from_ms, bounds.to_ms);
    Ok(DetailDiagnostics {
        would_trim: would_trim_summary(db, from_ms, to_ms)?,
        shadow_misfire: shadow_misfire_summary(db, from_ms, to_ms)?,
        // Restricted at the query layer to recorder-version rows.
        near_lossless_attribution: near_lossless_attribution_summary(db, from_ms, to_ms)?,
        reduction: reduction_summary(db, from_ms, to_ms)?,
        cache_decision: cache_decision_summary(db, from_ms, to_ms)?,
        suppressed_sessions: suppressed_sessions(db, from_ms, to_ms)?,
    })
}

/// Nearest-rank percentile of a sorted, non-empty slice. `q` is in `[0,1]`;
/// rank = ceil(q * n), clamped to at least 1, 1-indexed.
fn nearest_rank(sorted: &[i64], q: f64) -> i64 {
    let n = sorted.len();
    let rank = ((q * n as f64).ceil() as usize).max(1);
    sorted[rank - 1]
}

/// The nearest-rank p50 and p95 of `values`, or `(None, None)` when empty.
fn ttft_pair(mut values: Vec<i64>) -> TtftPair {
    if values.is_empty() {
        return (None, None);
    }
    values.sort_unstable();
    (
        Some(nearest_rank(&values, 0.50)),
        Some(nearest_rank(&values, 0.95)),
    )
}

/// Per-group and window-wide TTFT p50/p95 via nearest-rank over the in-window
/// streaming time-to-first-byte samples, each sample labelled the way the
/// shared query labels its row. Only computed for `--detail`.
fn compute_ttft(
    db: &UsageDb,
    bounds: WindowBounds,
    by: GroupDim,
) -> Result<TtftPercentiles, UsageError> {
    let raw = ttfbs(db, bounds.from_ms, bounds.to_ms)?;
    let mut buckets: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut total = Vec::with_capacity(raw.len());
    for (key, ms) in raw {
        buckets.entry(by.label(&key)).or_default().push(ms);
        total.push(ms);
    }
    Ok(TtftPercentiles {
        by_label: buckets
            .into_iter()
            .map(|(label, values)| (label, ttft_pair(values)))
            .collect(),
        total: ttft_pair(total),
    })
}

// --- formatting ---------------------------------------------------------

/// Render a metric sum under the not-reported rule: `-` when no fine row in
/// the group reported the metric (`present == 0`), else the humanized sum.
fn metric_cell(present: i64, sum: i64) -> String {
    if present == 0 {
        return "-".to_string();
    }
    human_count(sum)
}

/// Render an optional figure: `-` when absent, else the humanized count.
fn optional_count_cell(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_string(), human_count)
}

/// The shared query's generation throughput as a whole tokens/second cell, or
/// `-` when no generation time was measured.
fn tok_per_s_cell(metrics: &QueryMetrics) -> String {
    metrics
        .throughput_tok_s
        .map_or_else(|| "-".to_string(), |v| (v.round() as i64).to_string())
}

/// Render the cost cell for a display row given its tri-state. A display
/// group can aggregate BOTH priced API-key rows and subscription rows; the
/// `+sub` marker flags that the dollar figure omits a subscription portion.
///
/// When the subscription portion resolved an API-equivalent value, that value
/// is shown as an explicit `~$` approximation ALONGSIDE the real figure, never
/// summed into it: one cell, two clearly-labelled channels. An unresolvable
/// subscription portion renders exactly as it did before the equivalent
/// existed.
fn cost_cell(row: &DisplayRow) -> String {
    let equivalent = row.metrics.equivalent_cost_usd;
    match (row.metrics.cost_usd, row.any_subscription) {
        (Some(usd), true) => match equivalent {
            Some(sub) => format!("${usd:.2} (+sub ~${sub:.2})"),
            None => format!("${usd:.2}+sub"),
        },
        (Some(usd), false) => format!("${usd:.2}"),
        (None, true) => match equivalent {
            Some(sub) => format!("n/a (sub ~${sub:.2})"),
            None => "n/a (subscription)".to_string(),
        },
        (None, false) => "n/a".to_string(),
    }
}

/// The normal (no-`--detail`) column headers, with the key dimension first.
fn normal_headers(key_header: &str) -> Vec<String> {
    [
        key_header,
        "reqs",
        "err",
        "input",
        "output",
        "cache_read",
        "hit%",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// The `--detail` column headers, appended after the normal columns.
const DETAIL_HEADERS: [&str; 8] = [
    "cost",
    "ctx_peak",
    "ctx_avg",
    "cache_wr",
    "ttft_p50",
    "ttft_p95",
    "tok/s",
    "srv_tools",
];

/// Render a token-weighted cache-hit percentage under the not-reported rule:
/// `-` when the provider reports no cache reads or the denominator is
/// degenerate, else the one-decimal percentage (e.g. `45.0%`), matching the
/// footer's formatting so column and footer read identically.
fn hit_pct_cell(cache_hit_pct: Option<f64>) -> String {
    cache_hit_pct.map_or_else(|| "-".to_string(), |pct| format!("{pct:.1}%"))
}

/// Displayed `input`: all prompt tokens NOT served from cache, i.e. fresh
/// input plus the cache-write total of both ledger eras. Applied uniformly to
/// every provider (a row reporting no write adds 0). With
/// `cache_read_billed`, this reconciles with the `cache_hit_pct` denominator.
/// NOT the cost basis -- cost prices the disjoint stored buckets separately.
const fn display_input(metrics: &QueryMetrics) -> i64 {
    metrics.input_tokens + metrics.cache_write_tokens
}

/// The normal data cells for one row, in header order.
fn normal_cells(row: &DisplayRow) -> Vec<String> {
    let m = &row.metrics;
    vec![
        row.label.clone(),
        m.requests.to_string(),
        m.errors.to_string(),
        human_count(display_input(m)),
        human_count(m.output_tokens),
        metric_cell(m.cache_read_present, m.cache_read_billed),
        hit_pct_cell(m.cache_hit_pct),
    ]
}

/// The extra `--detail` data cells for one row, in header order.
fn detail_cells(row: &DisplayRow) -> Vec<String> {
    let m = &row.metrics;
    vec![
        cost_cell(row),
        optional_count_cell(m.ctx_peak),
        optional_count_cell(m.ctx_avg),
        metric_cell(m.cache_write_present, m.cache_write_tokens),
        ttft_cell(row.ttft_p50_ms),
        ttft_cell(row.ttft_p95_ms),
        tok_per_s_cell(m),
        metric_cell(m.server_tool_present, m.server_tool_calls),
    ]
}

fn ttft_cell(ms: Option<i64>) -> String {
    ms.map_or_else(|| "-".to_string(), human_ms)
}

/// Render one window report as an aligned ASCII block to a string.
pub fn render_report(report: &WindowReport, zone: &LocalZone) -> String {
    let mut out = String::new();
    out.push_str(&report.title);
    out.push('\n');

    let mut headers = normal_headers(report.by_header);
    if report.detail {
        headers.extend(DETAIL_HEADERS.iter().map(|s| (*s).to_string()));
    }

    let mut table: Vec<Vec<String>> = vec![headers];
    for row in report.rows() {
        let mut cells = normal_cells(row);
        if report.detail {
            cells.extend(detail_cells(row));
        }
        table.push(cells);
    }

    out.push_str(&render_table(&table));

    if report.detail {
        out.push_str(&render_latency_summary(report));
        out.push_str(&render_would_trim(report));
        out.push_str(&render_shadow_misfire(report));
        out.push_str(&render_reduction(report));
        out.push_str(&render_cache_decisions(report));
        out.push_str(&render_suppressed_sessions(report, zone));
    }
    for q in &report.quota {
        out.push_str(&render_quota(q, zone));
    }
    out.push_str(&render_footer(report));
    out
}

/// One-line latency / throughput / streaming-share summary, derived from the
/// window total. Emitted only under `--detail`.
fn render_latency_summary(report: &WindowReport) -> String {
    let total = &report.total;
    let p50 = ttft_cell(total.ttft_p50_ms);
    let p95 = ttft_cell(total.ttft_p95_ms);
    let toks = tok_per_s_cell(&total.metrics);
    let m = &total.metrics;
    let pct = if m.requests > 0 {
        (100.0 * m.stream_count as f64 / m.requests as f64).round() as i64
    } else {
        0
    };
    format!(
        "latency: TTFT p50 {p50} / p95 {p95} (streaming)  |  throughput {toks} tok/s  |  {pct}% streaming\n"
    )
}

/// One-line steady-state would-trim opportunity summary: the count of
/// requests the trimmer flagged with a would-cut candidate and the summed
/// candidate tokens over the window. Advisory (non-mutating recording);
/// emitted only under `--detail`, and only when there is an opportunity, so a
/// window with no candidates stays uncluttered.
///
/// The verdict line is derived at render time from the persisted numeric
/// columns; it is never a stored token and never touches `reduction_strategy`.
///
/// Appends the near-lossless attribution block when the recorder ran in
/// the window, independent of whether the baseline would-cut candidate line
/// above it is present -- the two summaries are gated separately because a
/// window can carry near-lossless recordings with zero baseline candidates (or
/// vice versa) without either implying the other.
fn render_would_trim(report: &WindowReport) -> String {
    let wt = &report.would_trim;
    let near_lossless = &report.near_lossless_attribution;
    if wt.candidate_requests == 0 && near_lossless.recorder_requests == 0 {
        return String::new();
    }
    let mut out = String::new();
    if wt.candidate_requests > 0 {
        out.push_str(&format!(
            "would-trim: {} reqs with a would-cut candidate, {} tokens (advisory; not applied)\n  verdict: met={} unmet={} cold={} unpriced={}\n",
            wt.candidate_requests,
            human_count(wt.would_trim_tokens),
            wt.verdict_met,
            wt.verdict_unmet,
            wt.verdict_cold,
            wt.verdict_unpriced,
        ));
    }
    if near_lossless.recorder_requests > 0 {
        out.push_str(&render_near_lossless_attribution(near_lossless));
    }
    out
}

/// The near-lossless attribution block: the recorder candidate count, the
/// per-heuristic freed-token breakdown (dedup vs supersession), and the
/// path-extractability rate (summed counts divided AFTER summing, never a
/// per-row average). Advisory (measurement candidates; nothing is cut).
fn render_near_lossless_attribution(summary: &NearLosslessAttributionSummary) -> String {
    let path_rate = if summary.path_units > 0 {
        format!(
            "{}%",
            (100.0 * summary.path_extractable as f64 / summary.path_units as f64).round() as i64
        )
    } else {
        "-".to_string()
    };
    format!(
        "near-lossless-attribution: {} reqs recorded (advisory; not applied)\n  dedup={} supersession={} tokens  |  path-extractable {}\n",
        summary.recorder_requests,
        human_count(summary.dedup_tokens),
        human_count(summary.supersession_tokens),
        path_rate,
    )
}

/// One-line shadow misfire monitor advisory: the number of candidate turns
/// compared and the misfire count. Advisory (recording-only); emitted only
/// under `--detail`, and only when comparisons have been made.
fn render_shadow_misfire(report: &WindowReport) -> String {
    let s = &report.shadow_misfire;
    if s.compared_turns == 0 {
        return String::new();
    }
    let pct = if s.compared_turns > 0 {
        (100.0 * s.misfire_turns as f64 / s.compared_turns as f64).round() as i64
    } else {
        0
    };
    format!(
        "shadow: {} candidate turns compared, {} misfires ({}%)\n",
        s.compared_turns, s.misfire_turns, pct,
    )
}

/// The validity caveat carried by every rendered reduction block. The
/// counters are recorded through the lossy usage channel, and this read
/// surface cannot observe that channel's drop counters (they are
/// process-lifetime atomics in the running service, not ledger columns). So
/// the block reports COUNTS SEEN only -- no ratio, no share, no cost figure --
/// and names the counters an operator must check flat before drawing any
/// proportional conclusion from a window.
const REDUCTION_VALIDITY_NOTE: &str = "  validity: counts observed, not a rate -- \
a ratio or cost claim needs the service's usage-channel dropped_full / \
dropped_closed / write_errors counters flat across the window\n";

/// The lossless-minifier outcome block: the per-decision request histogram,
/// the summed raw effect counters, the read-time-derived token estimate, and
/// the drop-counter validity caveat. Emitted only under `--detail`, and only
/// when some request in the window carried an outcome.
///
/// Deliberately renders no ratio and no cost figure (see
/// [`REDUCTION_VALIDITY_NOTE`]). `est_tokens` is derived here from the summed
/// bytes and is never persisted.
///
/// A nonzero `strings_rejected` gets its own line: with the current minifier
/// that count is structurally unreachable, so a nonzero value means the
/// re-parse equality guard declined a rewrite -- a minifier defect to
/// investigate, NOT traffic headroom for a more capable transform.
fn render_reduction(report: &WindowReport) -> String {
    let r = &report.reduction;
    if r.decided_requests == 0 {
        return String::new();
    }
    let histogram = r
        .decisions
        .iter()
        .map(|(token, count)| format!("{token}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "reduction: {} reqs decided  |  {histogram}\n  saved {} bytes (~{} est tokens, bytes/4)  |  strings compressed={} skipped={}\n",
        r.decided_requests,
        human_count(r.bytes_saved),
        human_count(r.est_tokens_saved()),
        r.strings_compressed,
        r.strings_skipped,
    );
    if r.strings_rejected > 0 {
        out.push_str(&format!(
            "  rejected={} -- the re-parse equality guard declined a rewrite; \
             investigate the minifier, this is not traffic headroom\n",
            r.strings_rejected,
        ));
    }
    out.push_str(REDUCTION_VALIDITY_NOTE);
    out
}

/// Two-line cache-breakpoint decision summary: the per-region decided /
/// emitted counts and the prefix-epoch event breakdown. Emitted only under
/// `--detail`, and only when the window carries at least one decision or one
/// classified epoch, so a window with no cache activity stays uncluttered.
///
/// Each region reports emitted-of-decided rather than a bare rate: rows written
/// before the columns existed carry no decision, and folding them into a
/// denominator would understate the emit share on a mixed-history window.
fn render_cache_decisions(report: &WindowReport) -> String {
    let c = &report.cache_decision;
    if c.front_decided == 0 && c.terminal_decided == 0 && c.epoch_classified == 0 {
        return String::new();
    }
    let mut out = format!(
        "cache-breakpoints: front {}/{} emitted  |  terminal {}/{} emitted\n",
        c.front_emitted, c.front_decided, c.terminal_emitted, c.terminal_decided,
    );
    if c.epoch_classified > 0 {
        out.push_str(&format!(
            "  prefix-epoch: {} classified, stable={} rewritten={} reseeded={}\n",
            c.epoch_classified, c.epoch_stable, c.epoch_rewritten, c.epoch_reseeded,
        ));
    }
    out
}

/// Format a `ts_start` epoch-ms as a local timestamp, to the second: an
/// operator reading the finder is correlating with service logs and with a
/// kill-switch flip, so the seconds matter and the date must be unambiguous.
/// Falls back to the raw millisecond value when the instant is not
/// representable locally, so the row still identifies its window position.
fn format_suppression_ts(zone: &LocalZone, ts_ms: i64) -> String {
    local_time::format_seconds(zone, ts_ms)
}

/// The K-suppression finder's validity caveat. Same shape and the same three
/// counters as [`REDUCTION_VALIDITY_NOTE`]: the finder reports what the ledger
/// recorded, and a window the usage channel dropped records in under-reports
/// both the triple list and each count by an unknown amount.
const SUPPRESSED_SESSIONS_VALIDITY_NOTE: &str = "  validity: sessions seen, not a rate -- \
a ratio or cost claim needs the service's usage-channel dropped_full / \
dropped_closed / write_errors counters flat across the window\n";

/// The session-reference scope caveat, printed alongside the finder rows so
/// an operator does not try to match a reference against a serve log or a
/// second `usage` run. The digests are salted per process on purpose (see
/// `routectl_usage`'s session-reference module), so they group only within
/// the report that printed them.
const SUPPRESSED_SESSIONS_SCOPE_NOTE: &str = "  scope: the session reference is \
salted per process -- it groups rows within THIS report only, and never matches \
a serve-log session hash or another usage run\n";

/// The K-suppressed-session finder block: one line per
/// `(session, provider_kind, model)` triple that had a marker withheld on
/// economic grounds, newest suppression first, plus the drop-counter validity
/// caveat. Emitted only under `--detail`, and only when the window carries at
/// least one suppression, so an ordinary window stays uncluttered.
///
/// This block exists to make a latched session FINDABLE rather than merely
/// counted: a wrong suppression keeps paying uncached prices at HTTP 200 with
/// every availability, latency, and error signal green, so the loss is bounded
/// only by how fast the session is located and the kill switch thrown.
///
/// `session` renders the opaque per-process reference, never the raw
/// client-supplied session key. It is scoped to THIS report: the reference
/// is salted per process, so it does not match the router's session hash in
/// a serve log, nor the same session's reference in another `usage` run.
/// Grouping is within one report only, deliberately -- a cross-surface join
/// would need a persistent shared secret the digest construction forbids.
fn render_suppressed_sessions(report: &WindowReport, zone: &LocalZone) -> String {
    let found = &report.suppressed_sessions;
    if found.rows.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "k-suppressed sessions: {} (session, provider_kind, model) triples with a \
         marker withheld below break-even\n",
        found.rows.len(),
    );
    for row in &found.rows {
        out.push_str(&format!(
            "  session={:016x} kind={} model={} reqs={} first={} last={}\n",
            row.session_ref,
            row.provider_kind,
            row.model,
            row.suppressed_requests,
            format_suppression_ts(zone, row.first_suppressed_ms),
            format_suppression_ts(zone, row.last_suppressed_ms),
        ));
    }
    if found.truncated {
        out.push_str(&format!(
            "  ... more triples than the {SUPPRESSED_SESSION_CAP}-row cap; \
             showing the newest by last suppression\n",
        ));
    }
    out.push_str(SUPPRESSED_SESSIONS_SCOPE_NOTE);
    out.push_str(SUPPRESSED_SESSIONS_VALIDITY_NOTE);
    out
}

/// One quota line for one seat. An absent `status` renders as `unknown`
/// (codex sends no status token); the other unreported fields -- `seat`
/// (pre-seat history, forwarded client credentials), `utilization`,
/// `overage`, `reset` -- render as `-`.
fn render_quota(q: &QuotaSnapshot, zone: &LocalZone) -> String {
    let seat = q.seat.as_deref().unwrap_or("-");
    let status = q.status.as_deref().unwrap_or("unknown");
    let util = q
        .utilization
        .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0));
    let overage = q.overage_status.as_deref().unwrap_or("-");
    let reset = q
        .reset
        .map_or_else(|| "-".to_string(), |epoch_s| format_reset(zone, epoch_s));
    format!("quota[{seat}]: status={status} utilization={util} overage={overage} reset={reset}\n")
}

/// Format a quota-reset epoch (seconds) as a local timestamp. Quota
/// resets are stamped in epoch SECONDS by the upstream observer.
fn format_reset(zone: &LocalZone, epoch_s: i64) -> String {
    local_time::local_text(zone, epoch_s, local_time::TextLayout::Minutes)
        .unwrap_or_else(|| epoch_s.to_string())
}

fn render_footer(report: &WindowReport) -> String {
    let totals = &report.total.metrics;
    let hit = match totals.cache_hit_pct {
        Some(pct) => format!("cache hit {pct:.1}%"),
        None => "cache hit n/a".to_string(),
    };
    format!(
        "{hit}\nclient disconnects: {} ({} pre-dispatch)\ncancelled: {}\n{}",
        totals.client_disconnect_total,
        totals.client_disconnect_pre_dispatch,
        totals.cancelled_total,
        render_walk_line(&report.observation),
    )
}

/// One line of the window's dispatch walk counts, summed over the ledger's
/// `requests` rows (a row written before the columns existed adds 0). When
/// the ledger recorded a gap inside the window, rows were lost before they
/// reached it, so the line says its counts are a lower bound.
fn render_walk_line(o: &ObservationTotals) -> String {
    let mut line = format!(
        "walk: request-caused failures={}  |  excluded endpoint={} model={} target={} probe-run={}",
        o.request_caused_failures,
        o.excluded_endpoint,
        o.excluded_model,
        o.excluded_target,
        o.excluded_probe_run,
    );
    if o.has_gap() {
        line.push_str(&format!(
            "  |  lower bound: the ledger recorded {} gap rows ({} request rows dropped) in this window",
            o.gap_rows, o.gap_dropped,
        ));
    }
    line.push('\n');
    line
}

/// End-of-report legend explaining the derived columns and markers, one entry
/// per line with the descriptions aligned at a fixed column. Appended once
/// after the final window block (see `build_blocks`).
const LEGEND: &str = concat!(
    "legend:\n",
    "  input       = prompt tokens not served from cache (fresh + cache-write); excludes cache_read\n",
    "  cache_read  = prompt tokens served from cache (billed volume)\n",
    "  hit%        = token-weighted cache-hit rate: cache_read / cache-inclusive prompt tokens\n",
    "  \"-\"         = metric not reported by that provider\n",
    "  \"n/a (sub)\" = managed subscription (see quota)\n",
    "  \"~$\"        = notional API-equivalent value of subscription usage; never added to the dollar figure,\n",
    "                and absent when the subscription rows' rates are incomplete\n",
    "  --detail    = adds cost, ctx_peak/ctx_avg (largest and mean per-request prompt:\n",
    "                fresh input + cache read + cache write, not a flow),\n",
    "                cache_wr (cache writes of every lifetime, already in input),\n",
    "                ttft_p50/ttft_p95 (nearest-rank time to first token over streamed successes),\n",
    "                tok/s, server-tools,\n",
    "                and a would-trim opportunity line (advisory steady-state-trim candidates; never applied),\n",
    "                a reduction line (lossless-minifier outcomes actually applied; counts, not rates),\n",
    "                and a cache-breakpoint line (per-region injection decisions + prefix-epoch events),\n",
    "                and a k-suppressed-session finder (triples whose cache marker was withheld below break-even)",
);

// --- k-calibration report -----------------------------------------------

const K_CALIBRATION_COVERAGE_PASS: f64 = 0.90;
const K_CALIBRATION_ACCURACY_PASS: f64 = 0.40;
const K_CALIBRATION_SUFFICIENCY_PASS: usize = 200;

const fn gate_label(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

/// Render the k-calibration ASCII report. Consistent with the usage output
/// style. Returns a no-data message when `cal.n == 0`.
pub fn render_k_calibration(cal: &KCalibration) -> String {
    if cal.n == 0 {
        return "no calibrated predictions recorded yet\n".to_string();
    }
    let cov_pass = cal.coverage >= K_CALIBRATION_COVERAGE_PASS;
    let suf_pass = cal.n >= K_CALIBRATION_SUFFICIENCY_PASS;
    let overall = cov_pass && suf_pass;
    format!(
        "== k-calibration (all history) ==\ncoverage     {:.2}   (>= {:.2})  {}\naccuracy     {:.2}   (<= {:.2})  (diagnostic, not a gate)\nsufficiency  {}    (>= {})   {}\nhazard_decay {:+.3}         (diagnostic; age-conditioning trigger, not a gate)\noverall: {}\n",
        cal.coverage,
        K_CALIBRATION_COVERAGE_PASS,
        gate_label(cov_pass),
        cal.accuracy,
        K_CALIBRATION_ACCURACY_PASS,
        cal.n,
        K_CALIBRATION_SUFFICIENCY_PASS,
        gate_label(suf_pass),
        cal.hazard_decay,
        gate_label(overall),
    )
}

// --- entry point --------------------------------------------------------

/// Run `routectl usage`. Resolves the DB path, opens read-only, builds the
/// requested report(s), and prints them. `NoData` is a friendly message +
/// exit 0; `VersionTooNew` is a hard error.
///
/// When `--k-calibration` is set, emits only the calibration report over all
/// history and returns immediately (window/by/detail flags are ignored).
///
/// The price tables are read once, here, so every block of one run prices
/// against the same snapshot.
pub fn run(config: &Config, args: &UsageArgs) -> Result<(), Box<dyn std::error::Error>> {
    let db_path = crate::derived_path::usage_db_path(args.db.clone(), config)?;

    let mut db = match open_readonly(&db_path) {
        Ok(db) => db,
        Err(OpenError::NoData { .. }) => {
            println!(
                "no usage data yet (nothing recorded at {})",
                db_path.display()
            );
            return Ok(());
        }
        Err(OpenError::VersionTooOld { found, supported }) => {
            println!(
                "usage db at {} predates this binary (schema {found}, need {supported}); \
                 start the service once to migrate it",
                db_path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(Box::new(e)),
    };

    let prices = PriceSnapshot::pin(&default_price_book());
    let zone = LocalZone::load();
    let now = OffsetDateTime::now_utc();
    let output = build_output(&mut db, config, &prices, args, &zone, now)?;
    print!("{output}");
    Ok(())
}

/// Build the output string for the usage command: the k-calibration report
/// when `args.k_calibration` is set, else the joined window blocks. Separated
/// from `run` so it is testable without touching stdout or the clock.
pub fn build_output(
    db: &mut UsageDb,
    config: &Config,
    prices: &PriceSnapshot,
    args: &UsageArgs,
    zone: &LocalZone,
    now: OffsetDateTime,
) -> Result<String, UsageError> {
    if args.k_calibration {
        let cal = k_calibration_summary(db)?;
        return Ok(render_k_calibration(&cal));
    }
    let blocks = build_blocks(db, config, prices, args, zone, now)?;
    Ok(blocks.join("\n"))
}

/// Build the rendered report blocks for the request: the multi-window
/// summary (no flag, no `--since`), a single ad-hoc `--since` range, or a
/// single calendar window. Separated from `run` so it is testable without
/// touching stdout or the clock.
pub fn build_blocks(
    db: &mut UsageDb,
    config: &Config,
    prices: &PriceSnapshot,
    args: &UsageArgs,
    zone: &LocalZone,
    now: OffsetDateTime,
) -> Result<Vec<String>, UsageError> {
    let mut blocks = build_window_blocks(db, config, prices, args, zone, now)?;
    // Keep the legend and the cache-ignore block out of the block count:
    // append them to the last block so a multi-window summary still yields
    // exactly one block per window.
    if let Some(last) = blocks.last_mut() {
        if args.detail {
            let deadline = Instant::now() + CACHE_IGNORE_READ_BUDGET;
            last.push_str(&cache_ignore_block(
                db,
                config,
                zone,
                unix_ms(now),
                deadline,
            ));
        }
        last.push_str(LEGEND);
        last.push('\n');
    }
    Ok(blocks)
}

/// The per-window rendered blocks before the legend is appended.
fn build_window_blocks(
    db: &mut UsageDb,
    config: &Config,
    prices: &PriceSnapshot,
    args: &UsageArgs,
    zone: &LocalZone,
    now: OffsetDateTime,
) -> Result<Vec<String>, UsageError> {
    let reports = build_window_reports(db, config, prices, args, zone, now)?;
    Ok(reports
        .iter()
        .map(|report| render_report(report, zone))
        .collect())
}

/// One report per window the request names: the `--since` range, the
/// selected window, or (neither given) the multi-window summary's four.
pub(crate) fn build_window_reports(
    db: &mut UsageDb,
    config: &Config,
    prices: &PriceSnapshot,
    args: &UsageArgs,
    zone: &LocalZone,
    now: OffsetDateTime,
) -> Result<Vec<WindowReport>, UsageError> {
    let windows = if let Some(since) = &args.since {
        let title = match &args.until {
            Some(until) => format!("== {since} .. {until} =="),
            None => format!("== since {since} =="),
        };
        vec![(
            title,
            since_bounds(zone, since, args.until.as_deref(), now)?,
        )]
    } else if args.window == WindowFlag::None {
        SUMMARY_WINDOWS
            .iter()
            .map(|&flag| (window_title(flag), window_bounds(zone, flag, now)))
            .collect()
    } else {
        vec![(
            window_title(args.window),
            window_bounds(zone, args.window, now),
        )]
    };
    windows
        .into_iter()
        .map(|(title, bounds)| {
            build_window_report(db, config, prices, title, bounds, args.by, args.detail)
        })
        .collect()
}

/// The windows the multi-window summary prints, in order.
const SUMMARY_WINDOWS: [WindowFlag; 4] = [
    WindowFlag::Today,
    WindowFlag::ThisWeek,
    WindowFlag::ThisMonth,
    WindowFlag::All,
];

fn window_title(flag: WindowFlag) -> String {
    match flag {
        WindowFlag::Today => "== today ==",
        WindowFlag::ThisWeek => "== this week ==",
        WindowFlag::ThisMonth => "== this month ==",
        WindowFlag::Last24h => "== last 24 hours ==",
        WindowFlag::Last7d => "== last 7 days ==",
        WindowFlag::All => "== all time ==",
        WindowFlag::None => "== usage ==",
    }
    .to_string()
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
