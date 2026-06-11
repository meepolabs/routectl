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
    aggregate, estimate_cost_tokens, latencies, latest_quota, open_readonly, AggRow, GroupKey,
    OpenError, QueryError, QuotaSnapshot, Rates, UsageDb,
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

    fn header(self) -> &'static str {
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
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub sum_latency_ms: i64,
    pub max_latency_ms: i64,
    pub server_tool_calls: i64,
    pub priced_total_usd: Option<f64>,
    pub any_subscription: bool,
    pub any_unpriced: bool,
    pub p95_latency_ms: Option<i64>,
}

/// The full report for one window: the display rows, the latest quota
/// snapshot (subscription spend signal), and the window-wide footer.
#[derive(Debug, Clone)]
pub struct WindowReport {
    pub title: String,
    pub by_header: Option<&'static str>,
    pub detail: bool,
    pub rows: Vec<DisplayRow>,
    pub quota: Option<QuotaSnapshot>,
    pub cache_hit_rate: Option<f64>,
    pub total_errors: i64,
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
        .map(|r| r.starts_with("oauth://"))
        .unwrap_or(false)
}

/// Convert the router's per-million-token pricing into the usage crate's
/// leaf-safe `Rates`.
fn rates_from_pricing(p: &routectl_router::PricingConfig) -> Rates {
    Rates {
        input_per_mtok: p.input_per_mtok,
        output_per_mtok: p.output_per_mtok,
        cache_read_per_mtok: p.cache_read_per_mtok,
        cache_write_5m_per_mtok: p.cache_write_5m_per_mtok,
        cache_write_1h_per_mtok: p.cache_write_1h_per_mtok,
    }
}

/// The cost contribution of one fine-grained `AggRow`.
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
    match estimate_cost_tokens(
        row.input_tokens,
        row.output_tokens,
        row.reasoning_tokens,
        row.cache_read,
        row.cache_write_5m,
        row.cache_write_1h,
        &rates,
    ) {
        Some(b) => RowCost::Priced(b.total_usd),
        None => RowCost::Unpriced,
    }
}

/// The display label for a fine row under a given breakdown dimension. The
/// total view collapses every row into one `"total"` group.
fn group_label(key: &GroupKey, by: Option<GroupDim>) -> String {
    match by {
        None => "total".to_string(),
        Some(GroupDim::Model) => key.model.clone().unwrap_or_else(|| "(none)".to_string()),
        Some(GroupDim::Provider) => key.provider.clone().unwrap_or_else(|| "(none)".to_string()),
        Some(GroupDim::Alias) => key.alias.clone(),
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
    cache_read: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    sum_latency_ms: i64,
    max_latency_ms: i64,
    server_tool_calls: i64,
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
        self.cache_read += row.cache_read;
        self.cache_write_5m += row.cache_write_5m;
        self.cache_write_1h += row.cache_write_1h;
        self.sum_latency_ms += row.sum_latency_ms;
        self.max_latency_ms = self.max_latency_ms.max(row.max_latency_ms);
        self.server_tool_calls += row.server_tool_calls;
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

/// Build the full window report: aggregate -> rollup to `by` -> cost per
/// fine row folded into the display group -> quota + footer. Pure over the
/// DB read; no clap, no stdout.
pub fn build_window_report(
    db: &UsageDb,
    config: &Config,
    title: String,
    bounds: WindowBounds,
    by: Option<GroupDim>,
    detail: bool,
) -> Result<WindowReport, UsageError> {
    let rows = aggregate(db, bounds.from_ms, bounds.to_ms)?;

    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();
    for row in &rows {
        let label = group_label(&row.key, by);
        let cost = cost_for_row(config, row);
        groups.entry(label).or_default().add(row, cost);
    }

    let p95 = if detail {
        compute_p95(db, bounds, by)?
    } else {
        BTreeMap::new()
    };

    let display_rows: Vec<DisplayRow> = groups
        .into_iter()
        .map(|(label, acc)| finalize_row(label, acc, &p95))
        .collect();

    let (cache_hit_rate, total_errors) = footer(&rows);

    Ok(WindowReport {
        title,
        by_header: by.map(|d| d.header()),
        detail,
        rows: display_rows,
        quota: latest_quota(db)?,
        cache_hit_rate,
        total_errors,
    })
}

/// Turn one accumulator into a display row, resolving the cost tri-state.
fn finalize_row(label: String, acc: Acc, p95: &BTreeMap<String, i64>) -> DisplayRow {
    DisplayRow {
        p95_latency_ms: p95.get(&label).copied(),
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
        cache_read: acc.cache_read,
        cache_write_5m: acc.cache_write_5m,
        cache_write_1h: acc.cache_write_1h,
        sum_latency_ms: acc.sum_latency_ms,
        max_latency_ms: acc.max_latency_ms,
        server_tool_calls: acc.server_tool_calls,
    }
}

/// Window-wide footer: cache-hit-rate = cache_read / (cache_read + input)
/// over every fine row (None when the denominator is zero), and the total
/// error count.
fn footer(rows: &[AggRow]) -> (Option<f64>, i64) {
    let mut cache_read = 0i64;
    let mut input = 0i64;
    let mut errors = 0i64;
    for r in rows {
        cache_read += r.cache_read;
        input += r.input_tokens;
        errors += r.errors;
    }
    let denom = cache_read + input;
    let rate = if denom > 0 {
        Some(cache_read as f64 / denom as f64)
    } else {
        None
    };
    (rate, errors)
}

/// Per-display-group p95 latency via the nearest-rank method: sort the
/// in-window latencies for the group ascending and pick the value at index
/// `ceil(0.95 * n) - 1`. Only computed for `--detail`.
fn compute_p95(
    db: &UsageDb,
    bounds: WindowBounds,
    by: Option<GroupDim>,
) -> Result<BTreeMap<String, i64>, UsageError> {
    let raw = latencies(db, bounds.from_ms, bounds.to_ms)?;
    let mut buckets: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (key, ms) in raw {
        buckets.entry(group_label(&key, by)).or_default().push(ms);
    }
    let mut out = BTreeMap::new();
    for (label, mut values) in buckets {
        if values.is_empty() {
            continue;
        }
        values.sort_unstable();
        let n = values.len();
        // nearest-rank: rank = ceil(0.95 * n), 1-indexed.
        let rank = ((0.95 * n as f64).ceil() as usize).max(1);
        out.insert(label, values[rank - 1]);
    }
    Ok(out)
}

// --- formatting ---------------------------------------------------------

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

/// Render one window report as an aligned ASCII block to a string.
pub fn render_report(report: &WindowReport) -> String {
    let mut out = String::new();
    out.push_str(&report.title);
    out.push('\n');

    let key_header = report.by_header.unwrap_or("scope");
    let mut headers: Vec<String> = vec![
        key_header.to_string(),
        "reqs".into(),
        "input".into(),
        "output".into(),
        "cache_rd".into(),
        "cost".into(),
    ];
    if report.detail {
        headers.extend(
            ["cw_5m", "cw_1h", "p95_ms", "max_ms", "wall_ms", "srv_tools"]
                .iter()
                .map(|s| s.to_string()),
        );
    }

    let mut table: Vec<Vec<String>> = vec![headers];
    for row in &report.rows {
        let mut cells = vec![
            row.label.clone(),
            row.requests.to_string(),
            row.input_tokens.to_string(),
            row.output_tokens.to_string(),
            row.cache_read.to_string(),
            cost_cell(row),
        ];
        if report.detail {
            cells.push(row.cache_write_5m.to_string());
            cells.push(row.cache_write_1h.to_string());
            cells.push(
                row.p95_latency_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            );
            cells.push(row.max_latency_ms.to_string());
            cells.push(row.sum_latency_ms.to_string());
            cells.push(row.server_tool_calls.to_string());
        }
        table.push(cells);
    }

    out.push_str(&render_table(&table));

    if let Some(q) = &report.quota {
        out.push_str(&render_quota(q));
    }
    out.push_str(&render_footer(report));
    out
}

/// Left-align column 0, right-align the rest, padded to the widest cell in
/// each column. ASCII spaces only.
fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
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
        .map(|u| format!("{:.0}%", u * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let overage = q.overage_status.as_deref().unwrap_or("-");
    let reset = q.reset.map(format_reset).unwrap_or_else(|| "-".to_string());
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
    let hit = report
        .cache_hit_rate
        .map(|r| format!("{:.1}%", r * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    format!("cache-hit-rate: {hit}   errors: {}\n", report.total_errors)
}

// --- entry point --------------------------------------------------------

/// Run `routectl usage`. Resolves the DB path, opens read-only, builds the
/// requested report(s), and prints them. `NoData` is a friendly message +
/// exit 0; `VersionTooNew` is a hard error.
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
        Err(e) => return Err(Box::new(e)),
    };

    let now = Local::now();
    let blocks = build_blocks(&db, config, args, now)?;
    print!("{}", blocks.join("\n"));
    Ok(())
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
