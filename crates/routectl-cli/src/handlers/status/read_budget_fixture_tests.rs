// The live-mix ledger fixture for the read-budget benchmarks: a lazily seeded
// multi-million-row ledger whose row widths, fine-group cardinality and column
// mixes follow a copy of a real ledger (950,153 rows over 93 days, measured
// with sqlite3 against a file copy). `include!`d into
// `read_budget_bench_tests.rs`; all imports come from the host, so do not add
// `use` lines here.
//
// Every share below is per 10,000 rows unless named otherwise. The generator
// is deterministic (a counter-based hash per row and field), so two runs seed
// byte-identical ledgers.

/// Rows in the large fixture.
const LIVE_MIX_ROWS: usize = 5_000_000;

/// Calendar span the rows are spread evenly over, ending at the seeding
/// instant.
const LIVE_MIX_SPAN_DAYS: i64 = 90;

/// Rows per seeding transaction. The rows are generated one at a time and never
/// held in memory, so the seeder's footprint is one row plus SQLite's cache.
const SEED_TX_ROWS: usize = 10_000;

/// How often the seeder reports progress, in rows.
const SEED_PROGRESS_EVERY: usize = 500_000;

/// The trailing share of rows written after the ledger began recording a
/// cache-write total (`cache_write_tokens`). Rows before it carry the legacy
/// per-lifetime columns instead, so the two eras stay disjoint as on disk.
const NEW_ERA_SHARE: usize = 210;

/// The trailing share of rows written after the ledger began recording the
/// request operation (`operation`, `harness`); older rows leave both NULL.
const OPERATION_ERA_SHARE: usize = 1_350;

/// Error rows among routed lanes. Unrouted lanes are all errors, which brings
/// the ledger-wide error share to the live 1.6%.
const ROUTED_ERROR_SHARE: u64 = 145;

/// `client_disconnect` and `cancelled` rows among routed lanes, after errors.
const DISCONNECT_SHARE: u64 = 60;
const CANCELLED_SHARE: u64 = 1;

/// Per 1,000 new-era anthropic rows: a positive cache-write total, then a
/// reported zero; the rest report no write usage (NULL).
const NEW_ERA_POSITIVE_PER_MILLE: u64 = 723;
const NEW_ERA_ZERO_PER_MILLE: u64 = 199;

/// Per 1,000 legacy-era anthropic rows carrying a 5m / 1h write split.
const LEGACY_WRITE_PER_MILLE: u64 = 310;

/// Per 40 seated rows, how many carry no quota snapshot.
const UNQUOTED_SEATED_PER_40: u64 = 1;

/// Longest padding any column takes, bytes.
const PAD_MAX: usize = 6_000;

/// One family of fine-grain lanes: every combination of its providers, seats,
/// models and alias variants is one fine group.
struct Family {
    /// Rows per 10,000 that fall in this family.
    weight: u64,
    kind: Option<&'static str>,
    /// Empty means the lane recorded no provider (an unrouted request).
    providers: &'static [&'static str],
    /// Empty means a NULL seat.
    seats: &'static [&'static str],
    models: &'static [&'static str],
    /// Alias variants per model; generated models when `models` is empty.
    aliases: usize,
    generated_models: usize,
}

const ANTHROPIC_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-haiku-5-5",
    "claude-sonnet-5-5",
];

/// The six lane families. Weights sum to 10,000 and lane counts to 194, the
/// live ledger's fine-group count. NULL-seat families (API keys, the compat
/// provider and unrouted rows) hold 51% of rows, as live.
const FAMILIES: [Family; 6] = [
    Family {
        weight: 4_981,
        kind: Some("anthropic-api"),
        providers: &["anthropic-one", "anthropic-other", "passthrough"],
        seats: &[],
        models: ANTHROPIC_MODELS,
        aliases: 4,
        generated_models: 0,
    },
    Family {
        weight: 3_455,
        kind: Some("anthropic-api"),
        providers: &["anthropic-oauth"],
        seats: &[
            "seat-01", "seat-02", "seat-03", "seat-04", "seat-05", "seat-06",
        ],
        models: ANTHROPIC_MODELS.split_at(5).0,
        aliases: 2,
        generated_models: 0,
    },
    Family {
        weight: 575,
        kind: Some("anthropic-api"),
        providers: &["anthropic-oauth-2"],
        seats: &["seat-07"],
        models: ANTHROPIC_MODELS.split_at(5).0,
        aliases: 2,
        generated_models: 0,
    },
    Family {
        weight: 870,
        kind: Some("openai-responses"),
        providers: &["codex"],
        seats: &["codex-seat"],
        models: &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.5"],
        aliases: 6,
        generated_models: 0,
    },
    Family {
        weight: 102,
        kind: Some("openai-compat"),
        providers: &["compat-a", "compat-b"],
        seats: &[],
        models: &[],
        aliases: 1,
        generated_models: 7,
    },
    Family {
        weight: 17,
        kind: None,
        providers: &[],
        seats: &[],
        models: &[],
        aliases: 1,
        generated_models: 20,
    },
];

/// One fine group's identity columns.
struct Lane {
    requested_model: String,
    /// NULL on an unrouted lane, as is `upstream`.
    model: Option<String>,
    alias: String,
    provider: Option<&'static str>,
    kind: Option<&'static str>,
    seat: Option<&'static str>,
}

impl Lane {
    const fn is_routed(&self) -> bool {
        self.provider.is_some()
    }
}

/// Every lane of `family`.
fn family_lanes(family: &Family) -> Vec<Lane> {
    let models: Vec<String> = if family.models.is_empty() {
        let stem = if family.providers.is_empty() {
            "unrouted-model"
        } else {
            "compat-model"
        };
        (1..=family.generated_models)
            .map(|n| format!("{stem}-{n:02}"))
            .collect()
    } else {
        family.models.iter().map(ToString::to_string).collect()
    };
    let providers: Vec<Option<&'static str>> = if family.providers.is_empty() {
        vec![None]
    } else {
        family.providers.iter().copied().map(Some).collect()
    };
    let seats: Vec<Option<&'static str>> = if family.seats.is_empty() {
        vec![None]
    } else {
        family.seats.iter().copied().map(Some).collect()
    };
    let mut lanes = Vec::new();
    for provider in &providers {
        for seat in &seats {
            for model in &models {
                for variant in 0..family.aliases {
                    lanes.push(Lane {
                        requested_model: model.clone(),
                        model: provider.map(|_| model.clone()),
                        alias: format!("route-{variant}-{model}"),
                        provider: *provider,
                        kind: family.kind,
                        seat: *seat,
                    });
                }
            }
        }
    }
    lanes
}

/// A counter-based 64-bit mix (splitmix64's finalizer): a deterministic,
/// well-spread draw from a row index and a per-field salt.
const fn draw(row: usize, salt: u64) -> u64 {
    let mut z = (row as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(salt.wrapping_mul(0xD1B5_4A32_D192_ED03));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The lane row `row` falls in.
fn pick_lane(lanes: &[Vec<Lane>], row: usize) -> &Lane {
    let mut point = draw(row, 1) % 10_000;
    for (family, members) in FAMILIES.iter().zip(lanes) {
        if point < family.weight {
            let index = (draw(row, 2) % members.len() as u64) as usize;
            return &members[index];
        }
        point -= family.weight;
    }
    unreachable!("family weights sum to 10,000")
}

/// The outcome columns: `(outcome, error_class, resolved_class, http_status)`.
fn outcome_for(
    lane: &Lane,
    row: usize,
) -> (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    i64,
) {
    if !lane.is_routed() {
        return ("gate_blocked", Some("locally_unavailable"), None, 503);
    }
    let point = draw(row, 3) % 10_000;
    if point < ROUTED_ERROR_SHARE {
        let outcome = if point.is_multiple_of(4) {
            "gate_blocked"
        } else {
            "upstream_error"
        };
        let class = match draw(row, 4) % 1_000 {
            0..=409 => Some("network-error"),
            410..=744 => Some("bad-request"),
            745..=888 => Some("rate-limited"),
            889..=958 => Some("server-error"),
            959..=977 => Some("auth"),
            978 => Some("overloaded"),
            _ => None,
        };
        return (outcome, Some("upstream"), class, 502);
    }
    if point < ROUTED_ERROR_SHARE + DISCONNECT_SHARE {
        return ("client_disconnect", None, None, 200);
    }
    if point < ROUTED_ERROR_SHARE + DISCONNECT_SHARE + CANCELLED_SHARE {
        return ("cancelled", None, None, 200);
    }
    ("ok", None, None, 200)
}

/// A row's cache-write columns across both ledger eras.
#[derive(Default)]
struct CacheWrites {
    legacy_5m: Option<i64>,
    legacy_1h: Option<i64>,
    tokens: Option<i64>,
    lifetimes: Option<String>,
}

fn cache_writes_for(lane: &Lane, row: usize, new_era: bool) -> CacheWrites {
    let point = draw(row, 5) % 1_000;
    let size = 300 + (draw(row, 6) % 7_400) as i64;
    let one_hour = draw(row, 7) % 100 < 14;
    match (lane.kind, new_era) {
        (Some("anthropic-api"), true) if point < NEW_ERA_POSITIVE_PER_MILLE => {
            let (five, hour) = if one_hour {
                (size / 3, size - size / 3)
            } else {
                (size, 0)
            };
            CacheWrites {
                tokens: Some(size),
                lifetimes: Some(format!("{{\"300\":{five},\"3600\":{hour}}}")),
                ..CacheWrites::default()
            }
        }
        (Some("anthropic-api"), true)
            if point < NEW_ERA_POSITIVE_PER_MILLE + NEW_ERA_ZERO_PER_MILLE =>
        {
            CacheWrites {
                tokens: Some(0),
                lifetimes: Some("{\"300\":0,\"3600\":0}".to_string()),
                ..CacheWrites::default()
            }
        }
        (Some("openai-responses"), true) => CacheWrites {
            tokens: Some(0),
            ..CacheWrites::default()
        },
        (Some("anthropic-api"), false) if point < LEGACY_WRITE_PER_MILLE => {
            let positive = !point.is_multiple_of(40);
            CacheWrites {
                legacy_5m: Some(if positive { size } else { 0 }),
                legacy_1h: Some(if positive && one_hour { size * 2 } else { 0 }),
                ..CacheWrites::default()
            }
        }
        _ => CacheWrites::default(),
    }
}

/// The quota snapshot columns of a seated row:
/// `(claim, status, utilization, reset, extras)`.
type Quota = (&'static str, Option<&'static str>, f64, i64, String);

fn quota_for(lane: &Lane, row: usize, ts: i64, pad: &str) -> Option<Quota> {
    lane.seat?;
    let point = draw(row, 8);
    if point % 40 < UNQUOTED_SEATED_PER_40 {
        return None;
    }
    let status = (point % 100 >= 18).then_some("allowed");
    let utilization = (point % 1_000) as f64 / 1_000.0;
    let extras = padded_json("7d-status", pad, 250 + (draw(row, 9) % 200) as usize);
    Some(("five_hour", status, utilization, ts + 18_000_000, extras))
}

/// A JSON object of about `len` bytes whose one value is padding.
fn padded_json(key: &str, pad: &str, len: usize) -> String {
    let body = len.saturating_sub(key.len() + 7).min(PAD_MAX);
    format!("{{\"{key}\":\"{}\"}}", &pad[..body])
}

/// `extra` on 58% of rows, about 312 B.
fn extra_for(row: usize, pad: &str) -> Option<String> {
    (draw(row, 10) % 100 < 58)
        .then(|| padded_json("stream_stage", pad, 280 + (draw(row, 11) % 66) as usize))
}

/// The write-stopped `would_trim_raw_marks` on 44% of rows: heavy-tailed like
/// the live column (half under 200 B, an eighth past 3 KB, so some rows
/// spill to overflow pages), averaging about 1.2 KB where present.
fn raw_marks_for(row: usize, pad: &str) -> Option<String> {
    let point = draw(row, 12);
    if point % 100 >= 44 {
        return None;
    }
    let spread = (draw(row, 13) % 1_000) as usize;
    let len = match point % 100 {
        0..=21 => 60 + spread / 7,
        22..=38 => 600 + spread * 9 / 5,
        _ => 3_000 + spread * 3,
    };
    Some(padded_json("marks", pad, len))
}

/// A time-ordered 36-character id, so the unique `request_id` index grows at
/// its right edge the way a UUIDv7 key does.
fn ordered_id(ts: i64, row: usize) -> String {
    let tail = draw(row, 14);
    format!(
        "{:08x}-{:04x}-7{:03x}-8{:03x}-{:012x}",
        (ts >> 16) as u32,
        ts as u16,
        (row & 0xfff) as u16,
        (tail & 0xfff) as u16,
        (tail >> 16) & 0xffff_ffff_ffff
    )
}

const SEED_INSERT_SQL: &str = "INSERT INTO requests (\
    ts_start, ts_end, request_id, ingress_dialect, requested_model, alias, model, \
    upstream, provider, provider_kind, seat, session_id, stream, tool_count, msg_count, \
    outcome, http_status, error_class, finish_reason, attempt_count, fallback_count, \
    latency_ms, ttfb_ms, input_tokens, output_tokens, reasoning_tokens, cache_read, \
    cache_write_5m, cache_write_1h, server_tool_use, quota_claim, quota_status, \
    quota_utilization, quota_reset, quota_extras, extra, would_trim_raw_marks, \
    resolved_class, operation, harness, cache_write_tokens, cache_write_lifetimes, \
    reduction_decision, cache_front_decision) \
    VALUES (?1, ?2, ?3, 'anthropic', ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
    ?14, ?15, ?16, ?17, 1, 0, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, \
    ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, 'skipped:no-tail', 'auto_emitted')";

/// Insert row `row` of a fixture whose rows start at `from_ms`, `step_ms`
/// apart (in thousandths of a millisecond).
fn insert_live_mix_row(
    stmt: &mut rusqlite::Statement<'_>,
    lanes: &[Vec<Lane>],
    pad: &str,
    row: usize,
    from_ms: i64,
) {
    let span_ms = LIVE_MIX_SPAN_DAYS * 86_400_000;
    let ts = from_ms + (row as i64 * span_ms) / LIVE_MIX_ROWS as i64;
    let lane = pick_lane(lanes, row);
    let (outcome, error_class, resolved_class, http_status) = outcome_for(lane, row);
    let new_era = row >= LIVE_MIX_ROWS - LIVE_MIX_ROWS * NEW_ERA_SHARE / 10_000;
    let operation_era = row >= LIVE_MIX_ROWS - LIVE_MIX_ROWS * OPERATION_ERA_SHARE / 10_000;
    let writes = cache_writes_for(lane, row, new_era);
    let quota = quota_for(lane, row, ts, pad);
    let ok = outcome == "ok";
    let stream = i64::from(draw(row, 15) % 1_000 >= 13);
    let latency = 400 + (draw(row, 16) % 30_000) as i64;
    let operation = operation_era.then(|| {
        if draw(row, 17).is_multiple_of(90) {
            "count_tokens"
        } else {
            "inference"
        }
    });
    stmt.execute(rusqlite::params![
        ts,
        ts + latency,
        ordered_id(ts, row),
        lane.requested_model,
        lane.alias,
        lane.model,
        lane.provider,
        lane.kind,
        lane.seat,
        (draw(row, 18) % 100 < 91).then(|| ordered_id(ts / 3_600_000, row / 40)),
        stream,
        (draw(row, 19) % 30) as i64,
        (draw(row, 20) % 400) as i64,
        outcome,
        http_status,
        error_class,
        ok.then_some("end_turn"),
        latency,
        (draw(row, 21) % 100 < 98).then(|| 150 + (draw(row, 22) % 3_000) as i64),
        (draw(row, 23) % 14_500) as i64,
        (draw(row, 24) % 2_056) as i64,
        (draw(row, 25) % 100 < 43).then(|| (draw(row, 26) % 800) as i64),
        (draw(row, 27) % 100 < 97).then(|| (draw(row, 28) % 277_000) as i64),
        writes.legacy_5m,
        writes.legacy_1h,
        draw(row, 29)
            .is_multiple_of(400)
            .then_some("{\"web_fetch_requests\":0,\"web_search_requests\":1}"),
        quota.as_ref().map(|q| q.0),
        quota.as_ref().and_then(|q| q.1),
        quota.as_ref().map(|q| q.2),
        quota.as_ref().map(|q| q.3),
        quota.as_ref().map(|q| q.4.as_str()),
        extra_for(row, pad),
        raw_marks_for(row, pad),
        resolved_class,
        operation,
        operation_era.then_some("claude_code"),
        writes.tokens,
        writes.lifetimes,
    ])
    .expect("seed row");
}

/// Seed the live-mix fixture at `path`, spread over the `LIVE_MIX_SPAN_DAYS`
/// ending at `now_ms`, in `SEED_TX_ROWS`-row transactions through the
/// migrating open, so the ledger is a fresh current-schema file with every
/// index maintained on insert. Leaves the WAL checkpointed and truncated.
fn seed_live_mix(path: &Path, now_ms: i64) {
    let db = open(path).expect("open fixture ledger");
    let conn = db.conn();
    conn.execute_batch("PRAGMA synchronous = OFF; PRAGMA cache_size = -262144;")
        .expect("seeding pragmas");
    let lanes: Vec<Vec<Lane>> = FAMILIES.iter().map(family_lanes).collect();
    let pad = "x".repeat(PAD_MAX);
    let from_ms = now_ms - LIVE_MIX_SPAN_DAYS * 86_400_000;
    let mut stmt = conn.prepare(SEED_INSERT_SQL).expect("prepare seed");
    let started = Instant::now();
    for chunk_start in (0..LIVE_MIX_ROWS).step_by(SEED_TX_ROWS) {
        conn.execute_batch("BEGIN").expect("begin");
        for row in chunk_start..(chunk_start + SEED_TX_ROWS).min(LIVE_MIX_ROWS) {
            insert_live_mix_row(&mut stmt, &lanes, &pad, row, from_ms);
        }
        conn.execute_batch("COMMIT").expect("commit");
        let done = chunk_start + SEED_TX_ROWS;
        if done.is_multiple_of(SEED_PROGRESS_EVERY) {
            println!(
                "  seeded {done} / {LIVE_MIX_ROWS} rows ({:?})",
                started.elapsed()
            );
        }
    }
    drop(stmt);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint the seeded ledger");
}

/// What a seeded ledger actually holds, read back from the file.
struct FixtureStats {
    rows: i64,
    inference_rows: i64,
    min_ts: i64,
    max_ts: i64,
    positive_writes: i64,
    zero_writes: i64,
    null_writes: i64,
    legacy_writes: i64,
    error_rows: i64,
    null_seat: i64,
    quota_bearing: i64,
    table_bytes: i64,
    fine_groups: i64,
}

const FIXTURE_STATS_SQL: &str = "SELECT COUNT(*), \
    SUM(operation IS NOT 'count_tokens'), MIN(ts_start), MAX(ts_start), \
    SUM(cache_write_tokens > 0), SUM(cache_write_tokens = 0), \
    SUM(cache_write_tokens IS NULL), \
    SUM(cache_write_5m IS NOT NULL OR cache_write_1h IS NOT NULL), \
    SUM(outcome NOT IN ('ok', 'client_disconnect', 'cancelled')), \
    SUM(seat IS NULL), \
    SUM(quota_status IS NOT NULL OR quota_utilization IS NOT NULL) \
    FROM requests";

/// The fine grain the aggregate statements group by.
const FINE_GROUPS_SQL: &str = "SELECT COUNT(*) FROM (SELECT 1 FROM requests \
    GROUP BY COALESCE(model, requested_model), provider, upstream, alias, \
    provider_kind, seat)";

impl FixtureStats {
    fn read(path: &Path) -> Self {
        let db = open_readonly(path).expect("open fixture for stats");
        let conn = db.conn();
        let table_bytes: i64 = conn
            .query_row(
                "SELECT SUM(pgsize) FROM dbstat WHERE name = 'requests'",
                [],
                |r| r.get(0),
            )
            .expect("requests table size");
        let fine_groups: i64 = conn
            .query_row(FINE_GROUPS_SQL, [], |r| r.get(0))
            .expect("fine groups");
        conn.query_row(FIXTURE_STATS_SQL, [], |r| {
            Ok(Self {
                rows: r.get(0)?,
                inference_rows: r.get(1)?,
                min_ts: r.get(2)?,
                max_ts: r.get(3)?,
                positive_writes: r.get(4)?,
                zero_writes: r.get(5)?,
                null_writes: r.get(6)?,
                legacy_writes: r.get(7)?,
                error_rows: r.get(8)?,
                null_seat: r.get(9)?,
                quota_bearing: r.get(10)?,
                table_bytes,
                fine_groups,
            })
        })
        .expect("fixture stats")
    }

    /// `part` as a percentage of the rows.
    fn pct(&self, part: i64) -> f64 {
        part as f64 * 100.0 / self.rows as f64
    }

    fn bytes_per_row(&self) -> i64 {
        self.table_bytes / self.rows.max(1)
    }

    fn print(&self) {
        println!(
            "ledger: rows={} inference_rows={} table_bytes_per_row={} fine_groups={} \
             span_days={:.1}",
            self.rows,
            self.inference_rows,
            self.bytes_per_row(),
            self.fine_groups,
            (self.max_ts - self.min_ts) as f64 / 86_400_000.0
        );
        println!(
            "ledger mix: cache_write positive={:.2}% zero={:.2}% null={:.2}% legacy_split={:.2}% \
             errors={:.2}% null_seat={:.2}% quota_bearing={:.2}%",
            self.pct(self.positive_writes),
            self.pct(self.zero_writes),
            self.pct(self.null_writes),
            self.pct(self.legacy_writes),
            self.pct(self.error_rows),
            self.pct(self.null_seat),
            self.pct(self.quota_bearing)
        );
    }

    /// Assert the seeded ledger matches the live mix it stands in for. The
    /// bands bracket the live measurement: 1,441 table bytes per row, 194 fine
    /// groups, cache writes 1.38% positive / 0.69% zero / 97.9% NULL, 1.62%
    /// error rows, 51.0% NULL seat, 47.7% quota-bearing.
    fn assert_live_mix(&self, now_ms: i64) {
        let in_band = |what: &str, value: f64, low: f64, high: f64| {
            assert!(
                (low..=high).contains(&value),
                "fixture {what} = {value:.3}, outside the live band {low}..={high}"
            );
        };
        assert_eq!(self.rows, LIVE_MIX_ROWS as i64, "fixture row count");
        in_band(
            "table bytes per row",
            self.bytes_per_row() as f64,
            1_300.0,
            1_700.0,
        );
        in_band("fine groups", self.fine_groups as f64, 180.0, 220.0);
        in_band(
            "positive cache-write %",
            self.pct(self.positive_writes),
            1.2,
            1.6,
        );
        in_band("zero cache-write %", self.pct(self.zero_writes), 0.4, 0.9);
        in_band("NULL cache-write %", self.pct(self.null_writes), 97.5, 98.4);
        in_band("error-row %", self.pct(self.error_rows), 1.4, 1.8);
        in_band("NULL-seat %", self.pct(self.null_seat), 48.0, 54.0);
        in_band("quota-bearing %", self.pct(self.quota_bearing), 45.0, 51.0);
        let earliest = now_ms - LIVE_MIX_SPAN_DAYS * 86_400_000;
        assert!(
            self.min_ts >= earliest && self.max_ts < now_ms,
            "every row within {LIVE_MIX_SPAN_DAYS} days: [{}, {}] vs [{earliest}, {now_ms})",
            self.min_ts,
            self.max_ts
        );
    }
}
