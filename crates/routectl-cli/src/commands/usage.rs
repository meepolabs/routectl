//! `routectl usage` -- the read surface over the usage-accounting DB.
//!
//! Reads the local SQLite usage DB (never writes it), rolls the
//! finest-grained aggregate rows up to a display dimension, derives cost
//! at query time from the `[registry]` pricing table, and prints an ASCII
//! report. Subscription providers (managed-OAuth, `api_key_ref` starting
//! `oauth://`) carry no per-token dollar cost; their dollar column reads
//! `n/a (subscription)` and the quota line is the real spend signal.
//!
//! The clap parsing, the window math, the rollup + cost core, and the
//! stdout formatting are deliberately separate so the correctness-critical
//! pieces (window bounds, rollup, cost bifurcation) are unit-testable
//! against a temp DB with a fixed reference instant.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

use routectl_router::Config;
use routectl_usage::{
    AggRow, GroupKey, KCalibration, M1AttributionSummary, OpenError, QueryError, QuotaSnapshot,
    Rates, ShadowMisfireSummary, UsageDb, WouldTrimSummary, aggregate, estimate_cost_tokens,
    k_calibration_summary, latest_quota, m1_attribution_summary, open_readonly,
    shadow_misfire_summary, ttfbs, would_trim_summary,
};

/// Parsed `routectl usage` arguments, already validated by clap.
#[derive(Debug, Clone)]
pub struct UsageArgs {
    pub window: WindowFlag,
    pub since: Option<String>,
    pub until: Option<String>,
    pub by: Option<GroupDim>,
    pub detail: bool,
    pub db: Option<PathBuf>,
    /// When true, emit only the k-calibration diagnostic and return.
    pub k_calibration: bool,
}

/// The mutually-exclusive calendar-window selector. `None` (no flag and no
/// `--since`) prints the multi-window summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFlag {
    Today,
    ThisWeek,
    ThisMonth,
    All,
    None,
}

/// The breakdown dimension for `--by`. Absent means a single total row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDim {
    Model,
    Provider,
    Alias,
}

impl GroupDim {
    /// Parse the `--by` value. Returns `None` on an unknown token (clap's
    /// value-parser rejects those before this is reached; this keeps the
    /// mapping in one place for tests).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            "alias" => Some(Self::Alias),
            _ => None,
        }
    }

    const fn header(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Alias => "alias",
        }
    }
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
}

// --- humanizing formatters ---------------------------------------------

const THOUSAND: i64 = 1_000;
const MILLION: i64 = 1_000_000;
const BILLION: i64 = 1_000_000_000;
const COMPACT_COUNT_FLOOR: i64 = 10_000;
const MS_PER_SECOND: i64 = 1_000;
const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;

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

/// Resolve a naive local midnight to a concrete instant, handling every
/// `LocalResult` explicitly so the window can never silently collapse:
/// - `Single` -> that instant.
/// - `Ambiguous` -> the earliest (a fall-back day has two midnights; the
///   earliest is the true day start).
/// - `None` (spring-forward gap, midnight does not exist) -> warn, then probe
///   forward in small steps to the next valid local instant; only if that
///   fails within the cap fall back to `now`.
fn resolve_local_midnight(naive: NaiveDateTime, now: DateTime<Local>) -> DateTime<Local> {
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => {
            tracing::warn!(
                naive_midnight = %naive,
                "local midnight falls in a DST spring-forward gap; probing forward for the next valid instant"
            );
            let mut offset = DST_PROBE_STEP_MIN;
            while offset <= DST_PROBE_CAP_MIN {
                let probe = naive + chrono::Duration::minutes(offset);
                if let LocalResult::Single(t) | LocalResult::Ambiguous(t, _) =
                    Local.from_local_datetime(&probe)
                {
                    return t;
                }
                offset += DST_PROBE_STEP_MIN;
            }
            now
        }
    }
}

/// Local midnight (00:00:00.000) on the calendar day of `now`.
fn local_midnight(now: DateTime<Local>) -> DateTime<Local> {
    let naive = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    resolve_local_midnight(naive, now)
}

/// Local midnight on the Monday of `now`'s ISO week (week starts Monday).
fn local_week_start(now: DateTime<Local>) -> DateTime<Local> {
    let weekday_from_monday = now.date_naive().weekday().num_days_from_monday() as i64;
    let monday = now.date_naive() - chrono::Duration::days(weekday_from_monday);
    let naive = monday.and_hms_opt(0, 0, 0).expect("valid midnight");
    resolve_local_midnight(naive, now)
}

/// Local midnight on the 1st of `now`'s month.
fn local_month_start(now: DateTime<Local>) -> DateTime<Local> {
    let first = now.date_naive().with_day(1).expect("day 1 always valid");
    let naive = first.and_hms_opt(0, 0, 0).expect("valid midnight");
    resolve_local_midnight(naive, now)
}

/// Bounds for a calendar window flag, anchored at an explicit `now` so
/// tests are deterministic. The binary passes `Local::now()`.
pub fn window_bounds(flag: WindowFlag, now: DateTime<Local>) -> WindowBounds {
    let to_ms = now.timestamp_millis() + 1;
    let from = match flag {
        WindowFlag::Today | WindowFlag::None => local_midnight(now),
        WindowFlag::ThisWeek => local_week_start(now),
        WindowFlag::ThisMonth => local_month_start(now),
        WindowFlag::All => return WindowBounds { from_ms: 0, to_ms },
    };
    WindowBounds {
        from_ms: from.timestamp_millis(),
        to_ms,
    }
}

/// Bounds for an ad-hoc `--since D [--until E]` range, anchored at `now`
/// for the open-ended upper bound. `from` is local midnight of `D`; `to`
/// is the last millisecond of `E` (23:59:59.999 local) made inclusive via
/// `+1`, or `now + 1` when `--until` is omitted.
pub fn since_bounds(
    since: &str,
    until: Option<&str>,
    now: DateTime<Local>,
) -> Result<WindowBounds, UsageError> {
    let from = parse_local_date_start(since)?;
    let to_ms = match until {
        Some(e) => parse_local_date_end(e)? + 1,
        None => now.timestamp_millis() + 1,
    };
    Ok(WindowBounds {
        from_ms: from.timestamp_millis(),
        to_ms,
    })
}

fn parse_naive_date(s: &str) -> Result<NaiveDate, UsageError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| UsageError::BadDate(s.to_string()))
}

/// Local midnight at the start of `s` (YYYY-MM-DD).
fn parse_local_date_start(s: &str) -> Result<DateTime<Local>, UsageError> {
    let date = parse_naive_date(s)?;
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .earliest()
        .ok_or_else(|| UsageError::AmbiguousTime(s.to_string()))
}

/// Epoch-ms at 23:59:59.999 local on `s` (YYYY-MM-DD).
fn parse_local_date_end(s: &str) -> Result<i64, UsageError> {
    let date = parse_naive_date(s)?;
    let end = date
        .and_hms_milli_opt(23, 59, 59, 999)
        .expect("valid end-of-day");
    Local
        .from_local_datetime(&end)
        .earliest()
        .map(|dt| dt.timestamp_millis())
        .ok_or_else(|| UsageError::AmbiguousTime(s.to_string()))
}

// --- rollup + cost ------------------------------------------------------

/// One display row: the rolled-up counters plus the cost decision the
/// formatter renders. Cost is intentionally tri-state: `priced_total_usd`
/// is the summed dollar cost of API-key rows that had a price;
/// `any_subscription` flags that managed-OAuth rows contributed (no $);
/// `any_unpriced` flags API-key rows with no `[registry]` price.
#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub label: String,
    pub requests: i64,
    pub ok: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    /// Peak cached-context SIZE in the group (MAX of the fine rows' per-turn
    /// snapshots). NOT a flow -- never folded into a token total.
    pub cache_read_peak: i64,
    /// Mean cached-context size in the group: presence-weighted mean of the
    /// fine rows' per-turn snapshots (`sum(avg_i * present_i) / total_present`).
    /// Display-only.
    pub cache_read_avg: i64,
    /// Summed billed cache-read VOLUME for the group (a flow). The rendered
    /// `cache_read` column shows this directly; it is also the numerator of the
    /// token-weighted `hit%`. Distinct from `cache_read_peak`/`cache_read_avg`,
    /// which are per-turn context SIZE snapshots (never summed).
    pub cache_read_billed: i64,
    /// Token-weighted cache-hit rate for this row, precomputed at finalize so
    /// the rendered cell never recomputes from summed fields. `None` when the
    /// provider does not report cache reads or the denominator is degenerate.
    /// The `total` row's value is overridden after the footer is computed so it
    /// mirrors the authoritative footer rate (see `build_window_report`).
    pub cache_hit_rate: Option<f64>,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub server_tool_calls: i64,
    pub priced_total_usd: Option<f64>,
    pub any_subscription: bool,
    pub any_unpriced: bool,
    pub ttft_p50_ms: Option<i64>,
    pub ttft_p95_ms: Option<i64>,
    pub gen_window_ms: i64,
    pub gen_output_tokens: i64,
    pub stream_count: i64,
    pub reasoning_present: i64,
    pub cache_read_present: i64,
    pub cache_write_5m_present: i64,
    pub cache_write_1h_present: i64,
    pub server_tool_present: i64,
}

/// The full report for one window: the display rows, the latest quota
/// snapshot (subscription spend signal), and the window-wide footer.
#[derive(Debug, Clone)]
pub struct WindowReport {
    pub title: String,
    pub by_header: &'static str,
    pub detail: bool,
    pub rows: Vec<DisplayRow>,
    pub quota: Option<QuotaSnapshot>,
    pub cache_hit_rate: Option<f64>,
    pub total_errors: i64,
    /// Window-wide `client_disconnect` outcome count (excluded from
    /// `total_errors`; see `AggRow::client_disconnect_total`).
    pub client_disconnects: i64,
    /// Subset of `client_disconnects` that never reached dispatch (raw
    /// `model IS NULL`, i.e. disconnected before the first content chunk).
    pub client_disconnects_pre_dispatch: i64,
    /// Steady-state would-trim opportunity over the window (advisory; only
    /// populated and surfaced under `--detail`).
    pub would_trim: WouldTrimSummary,
    /// Shadow misfire monitor summary over the window (advisory; only
    /// populated and surfaced under `--detail`).
    pub shadow_misfire: ShadowMisfireSummary,
    /// M1 near-lossless per-heuristic attribution over the window (advisory;
    /// only populated and surfaced under `--detail`). Restricted at the
    /// query layer to `would_trim_recorder_version IS NOT NULL`.
    pub m1_attribution: M1AttributionSummary,
}

/// True iff `provider` is a managed-OAuth subscription provider: its
/// configured `api_key_ref` begins with `oauth://`. A row whose provider
/// is unknown to the config or absent is NOT subscription (it simply
/// carries no cost). This is the ONLY subscription signal -- auth_kind is
/// deliberately not consulted.
fn is_subscription(config: &Config, provider: &str) -> bool {
    config
        .providers
        .get(provider)
        .and_then(|p| p.api_key_ref())
        .is_some_and(|r| r.starts_with("oauth://"))
}

/// Convert the router's per-million-token pricing into the usage crate's
/// leaf-safe `Rates`.
const fn rates_from_pricing(p: &routectl_router::PricingConfig) -> Rates {
    Rates {
        input_per_mtok: p.input_per_mtok,
        output_per_mtok: p.output_per_mtok,
        cache_read_per_mtok: p.cache_read_per_mtok,
        cache_write_5m_per_mtok: p.cache_write_5m_per_mtok,
        cache_write_1h_per_mtok: p.cache_write_1h_per_mtok,
    }
}

/// The cost contribution of one fine-grained `AggRow`.
#[derive(Clone)]
enum RowCost {
    /// Managed-OAuth subscription row: no per-token dollar cost.
    Subscription,
    /// API-key row with a `[registry]` price: this dollar amount.
    Priced(f64),
    /// API-key row with no price, or no served provider: counts toward
    /// totals but contributes no dollar cost.
    Unpriced,
}

/// Classify and cost one fine-grained row. Subscription detection runs
/// first (it overrides pricing); then a priced API-key row prices its
/// summed tokens; everything else is unpriced.
fn cost_for_row(config: &Config, row: &AggRow) -> RowCost {
    let Some(provider) = row.key.provider.as_deref() else {
        return RowCost::Unpriced;
    };
    if is_subscription(config, provider) {
        return RowCost::Subscription;
    }
    let Some(upstream) = row.key.upstream.as_deref() else {
        return RowCost::Unpriced;
    };
    let Some(pricing) = config.pricing_for(upstream, provider) else {
        return RowCost::Unpriced;
    };
    let rates = rates_from_pricing(pricing);
    // cache_read is billed PER TURN, so the cost basis is the summed cache-read
    // volume (`cache_read_billed`), not the peak. The peak / avg are
    // display-only context SIZE and must NOT drive cost; input / output /
    // cache_write_* are real summed flows.
    match estimate_cost_tokens(
        row.input_tokens,
        row.output_tokens,
        row.reasoning_tokens,
        row.cache_read_billed,
        row.cache_write_5m,
        row.cache_write_1h,
        &rates,
    ) {
        Some(b) => RowCost::Priced(b.total_usd),
        None => RowCost::Unpriced,
    }
}

/// Per-group time-to-first-token percentiles: label -> (p50, p95) in ms,
/// each `None` when the group has no streaming samples.
type TtftMap = BTreeMap<String, (Option<i64>, Option<i64>)>;

/// The display label for a fine row under a given breakdown dimension.
/// A null model/provider column falls back to `(unattributed)`.
fn group_label(key: &GroupKey, by: GroupDim) -> String {
    match by {
        GroupDim::Model => key
            .model
            .clone()
            .unwrap_or_else(|| "(unattributed)".to_string()),
        GroupDim::Provider => key
            .provider
            .clone()
            .unwrap_or_else(|| "(unattributed)".to_string()),
        GroupDim::Alias => key.alias.clone(),
    }
}

/// Accumulator mirroring `DisplayRow`'s summable fields while the cost
/// tri-state is folded in.
#[derive(Default)]
struct Acc {
    requests: i64,
    ok: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    /// Running MAX of the fine rows' per-turn cache-read peaks.
    cache_read_peak: i64,
    /// Presence-weighted accumulator for the group's mean cache-read size:
    /// `sum(avg_i * present_i)`; divided by `cache_read_present` at finalize.
    cache_read_avg_weighted: i64,
    /// Running SUM of the fine rows' billed cache-read volume (a flow).
    cache_read_billed: i64,
    /// Cache-inclusive prompt-token denominator for the hit%, summed over the
    /// fine rows that report cache reads (`cache_read_present > 0`) via the one
    /// shared `cache_prompt_den` rule. Numerator is `cache_read_billed`
    /// (non-reporting rows contribute 0 there), so this pairs with it to give a
    /// per-group rate that matches the footer by construction.
    cache_hit_den: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    server_tool_calls: i64,
    gen_window_ms: i64,
    gen_output_tokens: i64,
    stream_count: i64,
    reasoning_present: i64,
    cache_read_present: i64,
    cache_write_5m_present: i64,
    cache_write_1h_present: i64,
    server_tool_present: i64,
    priced_usd: f64,
    any_priced: bool,
    any_subscription: bool,
    any_unpriced: bool,
}

impl Acc {
    fn add(&mut self, row: &AggRow, cost: RowCost) {
        self.requests += row.requests;
        self.ok += row.ok;
        self.errors += row.errors;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        self.reasoning_tokens += row.reasoning_tokens;
        self.cache_read_peak = self.cache_read_peak.max(row.cache_read_peak);
        // safe in i64: ~200k-token ceiling * realistic per-window turn counts << i64::MAX
        self.cache_read_avg_weighted += row.cache_read_avg * row.cache_read_present;
        self.cache_read_billed += row.cache_read_billed;
        if row.cache_read_present > 0 {
            self.cache_hit_den += cache_prompt_den(row);
        }
        self.cache_write_5m += row.cache_write_5m;
        self.cache_write_1h += row.cache_write_1h;
        self.server_tool_calls += row.server_tool_calls;
        self.gen_window_ms += row.gen_window_ms;
        self.gen_output_tokens += row.gen_output_tokens;
        self.stream_count += row.stream_count;
        self.reasoning_present += row.reasoning_present;
        self.cache_read_present += row.cache_read_present;
        self.cache_write_5m_present += row.cache_write_5m_present;
        self.cache_write_1h_present += row.cache_write_1h_present;
        self.server_tool_present += row.server_tool_present;
        match cost {
            RowCost::Subscription => self.any_subscription = true,
            RowCost::Priced(usd) => {
                self.priced_usd += usd;
                self.any_priced = true;
            }
            RowCost::Unpriced => self.any_unpriced = true,
        }
    }
}

/// Build the full window report: aggregate -> rollup to `by` (defaulting to
/// model) -> cost per fine row folded into the display group -> a window-wide
/// `total` row -> quota + footer. Pure over the DB read; no clap, no stdout.
pub fn build_window_report(
    db: &UsageDb,
    config: &Config,
    title: String,
    bounds: WindowBounds,
    by: Option<GroupDim>,
    detail: bool,
) -> Result<WindowReport, UsageError> {
    let dim = by.unwrap_or(GroupDim::Model);
    let rows = aggregate(db, bounds.from_ms, bounds.to_ms)?;

    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();
    let mut total = Acc::default();
    for row in &rows {
        let label = group_label(&row.key, dim);
        let cost = cost_for_row(config, row);
        groups.entry(label).or_default().add(row, cost.clone());
        total.add(row, cost);
    }

    let ttft = if detail {
        compute_ttft(db, bounds, dim)?
    } else {
        BTreeMap::new()
    };

    // Steady-state would-trim opportunity: only queried (and surfaced) under
    // --detail. The default table stays unchanged.
    let would_trim = if detail {
        would_trim_summary(db, bounds.from_ms, bounds.to_ms)?
    } else {
        WouldTrimSummary::default()
    };

    // Shadow misfire monitor: only queried (and surfaced) under --detail.
    let shadow_misfire = if detail {
        shadow_misfire_summary(db, bounds.from_ms, bounds.to_ms)?
    } else {
        ShadowMisfireSummary::default()
    };

    // M1 near-lossless attribution: only queried (and surfaced) under
    // --detail. Restricted at the query layer to recorder-version rows.
    let m1_attribution = if detail {
        m1_attribution_summary(db, bounds.from_ms, bounds.to_ms)?
    } else {
        M1AttributionSummary::default()
    };

    let mut display_rows: Vec<DisplayRow> = groups
        .into_iter()
        .map(|(label, acc)| finalize_row(label, acc, &ttft))
        .collect();
    display_rows.push(finalize_row("total".to_string(), total, &ttft));

    let (cache_hit_rate, total_errors, client_disconnects, client_disconnects_pre_dispatch) =
        footer(&rows);
    // The total row mirrors the authoritative footer (token-weighted over
    // cache-reporting rows) so the two never disagree in mixed-provider windows.
    if let Some(t) = display_rows.last_mut() {
        t.cache_hit_rate = cache_hit_rate;
    }

    Ok(WindowReport {
        title,
        by_header: dim.header(),
        detail,
        rows: display_rows,
        quota: latest_quota(db)?,
        cache_hit_rate,
        total_errors,
        client_disconnects,
        client_disconnects_pre_dispatch,
        would_trim,
        shadow_misfire,
        m1_attribution,
    })
}

/// Turn one accumulator into a display row, resolving the cost tri-state and
/// attaching the group's pre-computed TTFT percentiles (if any).
fn finalize_row(label: String, acc: Acc, ttft: &TtftMap) -> DisplayRow {
    let (ttft_p50_ms, ttft_p95_ms) = ttft.get(&label).copied().unwrap_or((None, None));
    let cache_read_avg = if acc.cache_read_present > 0 {
        acc.cache_read_avg_weighted / acc.cache_read_present
    } else {
        0
    };
    let cache_hit_rate = cache_hit_ratio(acc.cache_read_billed, acc.cache_hit_den);
    DisplayRow {
        ttft_p50_ms,
        ttft_p95_ms,
        priced_total_usd: if acc.any_priced {
            Some(acc.priced_usd)
        } else {
            None
        },
        any_subscription: acc.any_subscription,
        any_unpriced: acc.any_unpriced,
        label,
        requests: acc.requests,
        ok: acc.ok,
        errors: acc.errors,
        input_tokens: acc.input_tokens,
        output_tokens: acc.output_tokens,
        reasoning_tokens: acc.reasoning_tokens,
        cache_read_peak: acc.cache_read_peak,
        cache_read_avg,
        cache_read_billed: acc.cache_read_billed,
        cache_hit_rate,
        cache_write_5m: acc.cache_write_5m,
        cache_write_1h: acc.cache_write_1h,
        server_tool_calls: acc.server_tool_calls,
        gen_window_ms: acc.gen_window_ms,
        gen_output_tokens: acc.gen_output_tokens,
        stream_count: acc.stream_count,
        reasoning_present: acc.reasoning_present,
        cache_read_present: acc.cache_read_present,
        cache_write_5m_present: acc.cache_write_5m_present,
        cache_write_1h_present: acc.cache_write_1h_present,
        server_tool_present: acc.server_tool_present,
    }
}

/// Cache-inclusive prompt-token denominator for one aggregate row:
/// `input + cache_read_billed + cache_write_5m + cache_write_1h`. The DB stores
/// input / cache_read / cache_write as DISJOINT prompt dimensions, so their sum
/// is the cache-INCLUSIVE prompt total against which billed cache reads are
/// weighed. This is the ONE denominator rule -- consumed by both the per-group
/// accumulator (`Acc::add`) and the footer, so a mixed-provider window can never
/// show two contradictory hit%. A row with `cache_read_present > 0` but
/// `input_tokens == 0` still contributes its reads/writes here.
const fn cache_prompt_den(row: &AggRow) -> i64 {
    row.input_tokens + row.cache_read_billed + row.cache_write_5m + row.cache_write_1h
}

/// The token-weighted cache-hit ratio `num / den`, or `None` when the
/// denominator is degenerate (`<= 0`) -- the caller renders `-`, never `0%`.
/// `cache_read_billed` is a summed FLOW, so both the per-group and footer paths
/// legitimately accumulate their own `(num, den)` pair (weighted aggregation is
/// not mean-of-means) and feed it through this one ratio rule; they differ only
/// in HOW they accumulate, never in the ratio.
fn cache_hit_ratio(num: i64, den: i64) -> Option<f64> {
    if den <= 0 {
        return None;
    }
    Some(num as f64 / den as f64)
}

/// Window-wide footer: cache-hit-rate, the total error count, and the
/// client-disconnect breakdown.
///
/// The rate is the token-weighted fraction of prompt tokens served from cache,
/// computed over the rows that report cache reads (`cache_read_present > 0`):
/// `sum(cache_read_billed) / sum(input + cache_read_billed + cache_write_5m +
/// cache_write_1h)`. cache_read here is a billed FLOW, so summing it across rows
/// is correct (the per-turn snapshot used for the display SIZE column is NOT --
/// that one must never be summed). Rows whose provider does not report cache are
/// excluded from both numerator and denominator. `None` when no qualifying row
/// has a positive denominator. Errors ARE a flow, so the error count stays a
/// cross-row sum. `client_disconnect_total` / `client_disconnect_pre_dispatch`
/// are likewise cross-row sums straight off `AggRow` -- see that struct's docs
/// for why they are excluded from `errors`.
fn footer(rows: &[AggRow]) -> (Option<f64>, i64, i64, i64) {
    let mut num = 0i64;
    let mut den = 0i64;
    let mut errors = 0i64;
    let mut client_disconnects = 0i64;
    let mut client_disconnects_pre_dispatch = 0i64;
    for r in rows {
        errors += r.errors;
        client_disconnects += r.client_disconnect_total;
        client_disconnects_pre_dispatch += r.client_disconnect_pre_dispatch;
        if r.cache_read_present > 0 {
            num += r.cache_read_billed;
            den += cache_prompt_den(r);
        }
    }
    let rate = cache_hit_ratio(num, den);
    (
        rate,
        errors,
        client_disconnects,
        client_disconnects_pre_dispatch,
    )
}

/// Nearest-rank percentile of a sorted, non-empty slice. `q` is in `[0,1]`;
/// rank = ceil(q * n), clamped to at least 1, 1-indexed.
fn nearest_rank(sorted: &[i64], q: f64) -> i64 {
    let n = sorted.len();
    let rank = ((q * n as f64).ceil() as usize).max(1);
    sorted[rank - 1]
}

/// Per-display-group TTFT p50/p95 via nearest-rank over the in-window
/// streaming time-to-first-byte samples. A window-wide `total` bucket is
/// accumulated alongside the per-group buckets. Only computed for `--detail`.
fn compute_ttft(db: &UsageDb, bounds: WindowBounds, by: GroupDim) -> Result<TtftMap, UsageError> {
    let raw = ttfbs(db, bounds.from_ms, bounds.to_ms)?;
    let mut buckets: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (key, ms) in raw {
        buckets.entry(group_label(&key, by)).or_default().push(ms);
        buckets.entry("total".to_string()).or_default().push(ms);
    }
    let mut out = BTreeMap::new();
    for (label, mut values) in buckets {
        if values.is_empty() {
            continue;
        }
        values.sort_unstable();
        let p50 = nearest_rank(&values, 0.50);
        let p95 = nearest_rank(&values, 0.95);
        out.insert(label, (Some(p50), Some(p95)));
    }
    Ok(out)
}

// --- formatting ---------------------------------------------------------

const MS_PER_SECOND_F: f64 = 1_000.0;

/// Render a metric sum under the not-reported rule: `-` when no fine row in
/// the group reported the metric (`present == 0`), else the humanized sum.
fn metric_cell(present: i64, sum: i64) -> String {
    if present == 0 {
        return "-".to_string();
    }
    human_count(sum)
}

/// Robust generation throughput as an integer tokens/second, or `None` when
/// there is no generation window to divide by. Aggregates totals rather than
/// averaging per-request ratios.
fn tok_per_s(gen_output_tokens: i64, gen_window_ms: i64) -> Option<i64> {
    if gen_window_ms == 0 {
        return None;
    }
    Some((gen_output_tokens as f64 * MS_PER_SECOND_F / gen_window_ms as f64).round() as i64)
}

/// Render the cost cell for a display row given its tri-state. A display
/// group can aggregate BOTH priced API-key rows and subscription rows; the
/// `+sub` suffix flags that the dollar figure omits a subscription portion.
fn cost_cell(row: &DisplayRow) -> String {
    match (row.priced_total_usd, row.any_subscription) {
        (Some(usd), true) => format!("${usd:.2}+sub"),
        (Some(usd), false) => format!("${usd:.2}"),
        (None, true) => "n/a (subscription)".to_string(),
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
const DETAIL_HEADERS: [&str; 9] = [
    "cost",
    "ctx_peak",
    "ctx_avg",
    "cache_wr_5m",
    "cache_wr_1h",
    "ttft_p50",
    "ttft_p95",
    "tok/s",
    "srv_tools",
];

/// Render the token-weighted cache-hit-rate cell for a display row under the
/// not-reported rule: `-` when the provider reports no cache reads or the
/// denominator is degenerate, else the one-decimal percentage (e.g. `45.0%`),
/// matching the footer's formatting so column and footer read identically.
fn hit_pct_cell(row: &DisplayRow) -> String {
    row.cache_hit_rate
        .map_or_else(|| "-".to_string(), |r| format!("{:.1}%", r * 100.0))
}

/// Displayed `input`: all prompt tokens NOT served from cache, i.e. fresh
/// input plus both cache-write buckets. Applied uniformly to every provider
/// (write buckets are 0 for OpenAI-style rows, so they render unchanged). With
/// `cache_read_billed`, this reconciles with the `cache_hit_pct` denominator.
/// NOT the cost basis -- cost prices the disjoint stored buckets separately.
const fn display_input(row: &DisplayRow) -> i64 {
    row.input_tokens + row.cache_write_5m + row.cache_write_1h
}

/// The normal data cells for one row, in header order.
fn normal_cells(row: &DisplayRow) -> Vec<String> {
    vec![
        row.label.clone(),
        row.requests.to_string(),
        row.errors.to_string(),
        human_count(display_input(row)),
        human_count(row.output_tokens),
        metric_cell(row.cache_read_present, row.cache_read_billed),
        hit_pct_cell(row),
    ]
}

/// The extra `--detail` data cells for one row, in header order.
fn detail_cells(row: &DisplayRow) -> Vec<String> {
    vec![
        cost_cell(row),
        metric_cell(row.cache_read_present, row.cache_read_peak),
        metric_cell(row.cache_read_present, row.cache_read_avg),
        metric_cell(row.cache_write_5m_present, row.cache_write_5m),
        metric_cell(row.cache_write_1h_present, row.cache_write_1h),
        ttft_cell(row.ttft_p50_ms),
        ttft_cell(row.ttft_p95_ms),
        tok_per_s(row.gen_output_tokens, row.gen_window_ms)
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        metric_cell(row.server_tool_present, row.server_tool_calls),
    ]
}

fn ttft_cell(ms: Option<i64>) -> String {
    ms.map_or_else(|| "-".to_string(), human_ms)
}

/// Render one window report as an aligned ASCII block to a string.
pub fn render_report(report: &WindowReport) -> String {
    let mut out = String::new();
    out.push_str(&report.title);
    out.push('\n');

    let mut headers = normal_headers(report.by_header);
    if report.detail {
        headers.extend(DETAIL_HEADERS.iter().map(|s| (*s).to_string()));
    }

    let mut table: Vec<Vec<String>> = vec![headers];
    for row in &report.rows {
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
    }
    if let Some(q) = &report.quota {
        out.push_str(&render_quota(q));
    }
    out.push_str(&render_footer(report));
    out
}

/// The window total row, used for the detail latency-summary line. It is
/// always the LAST row appended by `build_window_report` (after the sorted
/// per-group rows), so it is selected by position rather than by label -- an
/// alias named "total" under `--by alias` must not be mistaken for it.
fn total_row(report: &WindowReport) -> Option<&DisplayRow> {
    report.rows.last()
}

/// One-line latency / throughput / streaming-share summary, derived from the
/// window total row. Emitted only under `--detail`.
fn render_latency_summary(report: &WindowReport) -> String {
    let Some(total) = total_row(report) else {
        return String::new();
    };
    let p50 = ttft_cell(total.ttft_p50_ms);
    let p95 = ttft_cell(total.ttft_p95_ms);
    let toks = tok_per_s(total.gen_output_tokens, total.gen_window_ms)
        .map_or_else(|| "-".to_string(), |v| v.to_string());
    let pct = if total.requests > 0 {
        (100.0 * total.stream_count as f64 / total.requests as f64).round() as i64
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
/// Appends the M1 near-lossless attribution block when the recorder ran in
/// the window, independent of whether the baseline would-cut candidate line
/// above it is present -- the two summaries are gated separately because a
/// window can carry M1 recordings with zero baseline candidates (or vice
/// versa) without either implying the other.
fn render_would_trim(report: &WindowReport) -> String {
    let wt = &report.would_trim;
    let m1 = &report.m1_attribution;
    if wt.candidate_requests == 0 && m1.recorder_requests == 0 {
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
    if m1.recorder_requests > 0 {
        out.push_str(&render_m1_attribution(m1));
    }
    out
}

/// The M1 near-lossless attribution block: the recorder candidate count, the
/// per-heuristic freed-token breakdown (dedup vs supersession), and the
/// path-extractability rate (summed counts divided AFTER summing, never a
/// per-row average). Advisory (measurement candidates; nothing is cut).
fn render_m1_attribution(m1: &M1AttributionSummary) -> String {
    let path_rate = if m1.path_units > 0 {
        format!(
            "{}%",
            (100.0 * m1.path_extractable as f64 / m1.path_units as f64).round() as i64
        )
    } else {
        "-".to_string()
    };
    format!(
        "m1-attribution: {} reqs recorded (advisory; not applied)\n  dedup={} supersession={} tokens  |  path-extractable {}\n",
        m1.recorder_requests,
        human_count(m1.dedup_tokens),
        human_count(m1.supersession_tokens),
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

/// Left-align column 0, right-align the rest, padded to the widest cell in
/// each column. ASCII spaces only.
fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i == 0 {
                line.push_str(&format!("{cell:<width$}", width = widths[i]));
            } else {
                line.push_str(&format!("{cell:>width$}", width = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn render_quota(q: &QuotaSnapshot) -> String {
    let status = q.status.as_deref().unwrap_or("unknown");
    let util = q
        .utilization
        .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0));
    let overage = q.overage_status.as_deref().unwrap_or("-");
    let reset = q.reset.map_or_else(|| "-".to_string(), format_reset);
    format!("quota: status={status} utilization={util} overage={overage} reset={reset}\n")
}

/// Format a quota-reset epoch (seconds) as a local timestamp. Quota
/// resets are stamped in epoch SECONDS by the upstream observer.
fn format_reset(epoch_s: i64) -> String {
    match Local.timestamp_opt(epoch_s, 0).earliest() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => epoch_s.to_string(),
    }
}

fn render_footer(report: &WindowReport) -> String {
    let hit = report.cache_hit_rate.map_or_else(
        || "cache hit n/a".to_string(),
        |r| format!("cache hit {:.1}%", r * 100.0),
    );
    format!(
        "{hit}\nclient disconnects: {} ({} pre-dispatch)\n",
        report.client_disconnects, report.client_disconnects_pre_dispatch
    )
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
    "  --detail    = adds cost, ctx_peak/ctx_avg (cached-context size, not a flow),\n",
    "                cache-write 5m/1h (breakdown of the share already in input), ttft, tok/s, server-tools,\n",
    "                and a would-trim opportunity line (advisory steady-state-trim candidates; never applied)",
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
pub fn run(config: &Config, args: &UsageArgs) -> Result<(), Box<dyn std::error::Error>> {
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(|| config.usage.db_path.clone());

    let db = match open_readonly(&db_path) {
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

    let now = Local::now();
    let output = build_output(&db, config, args, now)?;
    print!("{output}");
    Ok(())
}

/// Build the output string for the usage command: the k-calibration report
/// when `args.k_calibration` is set, else the joined window blocks. Separated
/// from `run` so it is testable without touching stdout or the clock.
pub fn build_output(
    db: &UsageDb,
    config: &Config,
    args: &UsageArgs,
    now: DateTime<Local>,
) -> Result<String, UsageError> {
    if args.k_calibration {
        let cal = k_calibration_summary(db)?;
        return Ok(render_k_calibration(&cal));
    }
    let blocks = build_blocks(db, config, args, now)?;
    Ok(blocks.join("\n"))
}

/// Build the rendered report blocks for the request: the multi-window
/// summary (no flag, no `--since`), a single ad-hoc `--since` range, or a
/// single calendar window. Separated from `run` so it is testable without
/// touching stdout or the clock.
pub fn build_blocks(
    db: &UsageDb,
    config: &Config,
    args: &UsageArgs,
    now: DateTime<Local>,
) -> Result<Vec<String>, UsageError> {
    let mut blocks = build_window_blocks(db, config, args, now)?;
    // Keep the legend out of the block count: append it to the last block so a
    // multi-window summary still yields exactly one block per window.
    if let Some(last) = blocks.last_mut() {
        last.push_str(LEGEND);
        last.push('\n');
    }
    Ok(blocks)
}

/// The per-window rendered blocks before the legend is appended.
fn build_window_blocks(
    db: &UsageDb,
    config: &Config,
    args: &UsageArgs,
    now: DateTime<Local>,
) -> Result<Vec<String>, UsageError> {
    if let Some(since) = &args.since {
        let bounds = since_bounds(since, args.until.as_deref(), now)?;
        let title = match &args.until {
            Some(until) => format!("== {since} .. {until} =="),
            None => format!("== since {since} =="),
        };
        let report = build_window_report(db, config, title, bounds, args.by, args.detail)?;
        return Ok(vec![render_report(&report)]);
    }

    if args.window == WindowFlag::None {
        let windows = [
            (WindowFlag::Today, "== today =="),
            (WindowFlag::ThisWeek, "== this week =="),
            (WindowFlag::ThisMonth, "== this month =="),
            (WindowFlag::All, "== all time =="),
        ];
        let mut blocks = Vec::with_capacity(windows.len());
        for (flag, title) in windows {
            let bounds = window_bounds(flag, now);
            let report =
                build_window_report(db, config, title.to_string(), bounds, args.by, args.detail)?;
            blocks.push(render_report(&report));
        }
        return Ok(blocks);
    }

    let title = window_title(args.window);
    let bounds = window_bounds(args.window, now);
    let report = build_window_report(db, config, title, bounds, args.by, args.detail)?;
    Ok(vec![render_report(&report)])
}

fn window_title(flag: WindowFlag) -> String {
    match flag {
        WindowFlag::Today => "== today ==",
        WindowFlag::ThisWeek => "== this week ==",
        WindowFlag::ThisMonth => "== this month ==",
        WindowFlag::All => "== all time ==",
        WindowFlag::None => "== usage ==",
    }
    .to_string()
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
