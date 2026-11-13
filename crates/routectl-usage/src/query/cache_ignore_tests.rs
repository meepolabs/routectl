//! Tests for the per-lane cache-ignore evidence query.

use super::*;
use crate::db::{open, open_readonly};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

const FROM: i64 = 1_000_000;
const TO: i64 = 2_000_000;
const LIFETIME_MS: i64 = 300_000;
const MIN_PROMPT: i64 = 4096;
/// Spacing of the positive fixture's turns: well inside the lifetime.
const STEP_MS: i64 = 1_000;

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

fn rule(provider: &str) -> CacheIgnoreRule {
    CacheIgnoreRule {
        provider: provider.to_owned(),
        min_prompt_tokens: MIN_PROMPT,
        lifetime_ms: LIFETIME_MS,
    }
}

fn rules() -> Vec<CacheIgnoreRule> {
    vec![rule("p"), rule("p2")]
}

/// Every column the query reads, so each test case can flip exactly one.
#[derive(Debug, Clone)]
struct Turn {
    ts_start: i64,
    session_id: Option<String>,
    provider: &'static str,
    seat: Option<&'static str>,
    model: String,
    provider_kind: &'static str,
    outcome: &'static str,
    decision: Option<&'static str>,
    epoch: Option<i64>,
    input: i64,
    cache_read: Option<i64>,
    write_tokens: Option<i64>,
    write_5m: Option<i64>,
    write_1h: Option<i64>,
    operation: Option<&'static str>,
    probe: Option<i64>,
}

/// One qualifying zero/zero turn on the lane `(p, seat-a, m, anthropic-api)`.
fn turn(ts_start: i64) -> Turn {
    Turn {
        ts_start,
        session_id: Some("sess-1".to_owned()),
        provider: "p",
        seat: Some("seat-a"),
        model: "m".to_owned(),
        provider_kind: "anthropic-api",
        outcome: "ok",
        decision: Some("caller_supplied"),
        epoch: Some(0),
        input: 5000,
        cache_read: Some(0),
        write_tokens: Some(0),
        write_5m: None,
        write_1h: None,
        operation: Some("inference"),
        probe: Some(0),
    }
}

fn insert(db: &UsageDb, request_id: &str, t: &Turn) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, seat, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, cache_read, cache_write_5m, cache_write_1h, \
             cache_write_tokens, cache_front_decision, prefix_epoch_event, operation, probe) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, ?4, ?5, ?6, ?7, 0, ?8, 0, 0, \
             0, 1, 0, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                t.ts_start,
                request_id,
                t.model.as_str(),
                t.provider,
                t.provider_kind,
                t.seat,
                t.session_id.as_deref(),
                t.outcome,
                t.input,
                t.cache_read,
                t.write_5m,
                t.write_1h,
                t.write_tokens,
                t.decision,
                t.epoch,
                t.operation,
                t.probe,
            ],
        )
        .expect("insert turn");
}

/// Four consecutive turns on one lane: the first has no previous turn, the
/// other three are evidence.
fn positive_fixture() -> Vec<Turn> {
    (0..4).map(|i| turn(FROM + 10_000 + i * STEP_MS)).collect()
}

fn seed(db: &UsageDb, turns: &[Turn]) {
    for (i, t) in turns.iter().enumerate() {
        insert(db, &format!("r{i}"), t);
    }
}

/// Total `(evidence, active)` over every returned lane.
fn totals(db: &UsageDb, rules: &[CacheIgnoreRule]) -> (i64, i64) {
    let got = cache_ignore_lanes(db, FROM, TO, rules).expect("query");
    got.lanes.iter().fold((0, 0), |(e, a), lane| {
        (e + lane.evidence_turns, a + lane.active_turns)
    })
}

/// Seed the positive fixture with `mutate` applied, return the totals under
/// `rules`.
fn totals_under(rules: &[CacheIgnoreRule], mutate: impl FnOnce(&mut [Turn])) -> (i64, i64) {
    let (_dir, db) = open_db();
    let mut turns = positive_fixture();
    mutate(&mut turns);
    seed(&db, &turns);
    totals(&db, rules)
}

/// [`totals_under`] the default rules.
fn totals_after(mutate: impl FnOnce(&mut [Turn])) -> (i64, i64) {
    totals_under(&rules(), mutate)
}

type Mutation = fn(&mut [Turn]);

/// One table row: a name, the fixture mutation, the expected totals.
type Case = (&'static str, Mutation, (i64, i64));

/// A red row: `(name, got, expected)`.
type Mismatch = (&'static str, (i64, i64), (i64, i64));

/// Every row whose totals under `rules` differ from its expectation, so one
/// run reports every red row.
fn mismatches(rules: &[CacheIgnoreRule], cases: &[Case]) -> Vec<Mismatch> {
    cases
        .iter()
        .map(|&(name, mutate, expected)| (name, totals_under(rules, mutate), expected))
        .filter(|(_, got, expected)| got != expected)
        .collect()
}

/// Place the four fixture turns `STEP_MS` apart with the last at `last_ts`.
fn end_at(t: &mut [Turn], last_ts: i64) {
    for (i, turn) in t.iter_mut().enumerate() {
        turn.ts_start = last_ts - (3 - i as i64) * STEP_MS;
    }
}

#[test]
fn three_zero_turns_after_a_first_turn_are_three_evidence_turns_in_one_session() {
    // Arrange
    let (_dir, db) = open_db();
    seed(&db, &positive_fixture());

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &rules()).expect("query");

    // Assert
    assert_eq!(
        got,
        CacheIgnoreLanes {
            lanes: vec![CacheIgnoreLane {
                provider: "p".into(),
                seat: Some("seat-a".into()),
                model: Some("m".into()),
                provider_kind: "anthropic-api".into(),
                evidence_turns: 3,
                active_turns: 0,
                sessions: 1,
            }],
            truncated: false,
        }
    );
}

#[test]
fn bedrock_lanes_qualify_like_anthropic_api_lanes() {
    let got = totals_after(|t| t.iter_mut().for_each(|t| t.provider_kind = "bedrock"));
    assert_eq!(got, (3, 0));
}

/// Each row flips one column of the positive fixture; `expected` is the
/// fixture's `(evidence, active)` afterwards. Mutations land on the LAST turn
/// unless the row says otherwise, so a row that removes one turn reads 2.
/// Each row's turn drops for exactly one reason: no other predicate would
/// also remove it.
#[test]
fn a_single_flipped_column_removes_the_turn_from_evidence() {
    let last = 3;
    let cases: Vec<Case> = vec![
        // Two NULL-session turns share one partition, so the second one has
        // a previous turn and only the NULL-session filter removes it.
        (
            "session NULL",
            |t| {
                t[2].session_id = None;
                t[3].session_id = None;
            },
            (1, 0),
        ),
        (
            "first turn of another session",
            |t| t[3].session_id = Some("sess-2".to_owned()),
            (2, 0),
        ),
        ("prev on another provider", |t| t[3].provider = "p2", (2, 0)),
        (
            "prev on another seat",
            |t| t[3].seat = Some("seat-b"),
            (2, 0),
        ),
        ("prev on no seat", |t| t[3].seat = None, (2, 0)),
        (
            "prev on another model",
            |t| t[3].model = "m2".to_owned(),
            (2, 0),
        ),
        (
            "gap lifetime + 1 ms",
            |t| t[3].ts_start = t[2].ts_start + LIFETIME_MS + 1,
            (2, 0),
        ),
        (
            "gap exactly lifetime qualifies",
            |t| t[3].ts_start = t[2].ts_start + LIFETIME_MS,
            (3, 0),
        ),
        ("prompt 4095", |t| t[3].input = 4095, (2, 0)),
        ("prompt 4096 qualifies", |t| t[3].input = 4096, (3, 0)),
        ("epoch rewritten", |t| t[3].epoch = Some(1), (2, 0)),
        ("epoch reseeded", |t| t[3].epoch = Some(2), (2, 0)),
        ("epoch NULL", |t| t[3].epoch = None, (2, 0)),
        (
            "decision auto_emitted",
            |t| t[3].decision = Some("auto_emitted"),
            (2, 0),
        ),
        ("decision NULL", |t| t[3].decision = None, (2, 0)),
        (
            "kind openai-responses",
            |t| t[3].provider_kind = "openai-responses",
            (2, 0),
        ),
        (
            "this outcome not ok",
            |t| t[3].outcome = "upstream_error",
            (2, 0),
        ),
        // The flipped turn is itself not ok AND is the next turn's prev, so
        // two turns leave: an intervening failure breaks the pair rather
        // than being skipped over.
        ("prev outcome not ok", |t| t[2].outcome = "timeout", (1, 0)),
        ("probe", |t| t[3].probe = Some(1), (2, 0)),
        (
            "operation count_tokens",
            |t| t[3].operation = Some("count_tokens"),
            (2, 0),
        ),
        // The whole fixture moves to the window end so every gap stays
        // inside the lifetime: only the upper bound removes the last turn.
        ("row at the window end", |t| end_at(t, TO), (2, 0)),
        (
            "row 1 ms before the window end qualifies",
            |t| end_at(t, TO - 1),
            (3, 0),
        ),
    ];
    assert_eq!(totals_after(|_| {}), (last, 0), "positive control");

    let red = mismatches(&rules(), &cases);
    assert!(red.is_empty(), "(case, got, expected): {red:#?}");
}

/// Place `t[3]` more than the lifetime after `t[1]` but within it of `t[2]`,
/// so `t[3]` qualifies only while `t[2]` stays its previous turn.
fn bridge_over_middle(t: &mut [Turn]) {
    t[3].ts_start = t[1].ts_start + LIFETIME_MS + 1;
}

/// Each row flips one non-outcome column on the MIDDLE turn `t[2]`. Those
/// predicates apply after the pairing, so `t[2]` is still `t[3]`'s previous
/// turn: `t[2]` alone drops. Applying the predicate before the pairing would
/// pair `t[3]` with `t[1]`, past the lifetime, and drop it too.
#[test]
fn a_middle_turn_failing_a_predicate_still_pairs_with_the_next_turn() {
    let cases: Vec<Case> = vec![
        ("control: no flip", bridge_over_middle, (3, 0)),
        (
            "control: a turn filtered before the pairing is skipped over",
            |t| {
                bridge_over_middle(t);
                t[2].probe = Some(1);
            },
            (1, 0),
        ),
        (
            "decision auto_emitted",
            |t| {
                bridge_over_middle(t);
                t[2].decision = Some("auto_emitted");
            },
            (2, 0),
        ),
        (
            "kind openai-responses",
            |t| {
                bridge_over_middle(t);
                t[2].provider_kind = "openai-responses";
            },
            (2, 0),
        ),
        (
            "epoch rewritten",
            |t| {
                bridge_over_middle(t);
                t[2].epoch = Some(1);
            },
            (2, 0),
        ),
        (
            "epoch reseeded",
            |t| {
                bridge_over_middle(t);
                t[2].epoch = Some(2);
            },
            (2, 0),
        ),
        (
            "prompt 4095",
            |t| {
                bridge_over_middle(t);
                t[2].input = 4095;
            },
            (2, 0),
        ),
    ];

    let red = mismatches(&rules(), &cases);
    assert!(red.is_empty(), "(case, got, expected): {red:#?}");
}

#[test]
fn a_row_before_the_window_start_is_no_previous_turn() {
    // Arrange: the first turn sits 1 ms before the window, so the second
    // turn has nothing in the window to pair with.
    assert_eq!(totals_after(|_| {}), (3, 0), "positive control");

    // Act
    let got = totals_after(|t| t[0].ts_start = FROM - 1);

    // Assert
    assert_eq!(got, (2, 0));
}

#[test]
fn a_provider_without_a_rule_is_not_counted() {
    // Arrange
    let (_dir, db) = open_db();
    seed(&db, &positive_fixture());

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &[rule("other")]).expect("query");

    // Assert
    assert!(got.lanes.is_empty(), "{got:?}");
}

#[test]
fn the_rule_minimum_and_lifetime_are_read_per_provider() {
    // Arrange: a stricter rule for "p" than the fixture satisfies.
    let (_dir, db) = open_db();
    seed(&db, &positive_fixture());
    let strict_min = CacheIgnoreRule {
        min_prompt_tokens: 5001,
        ..rule("p")
    };
    let strict_lifetime = CacheIgnoreRule {
        lifetime_ms: STEP_MS - 1,
        ..rule("p")
    };

    // Act
    let by_min = cache_ignore_lanes(&db, FROM, TO, &[strict_min]).expect("query");
    let by_lifetime = cache_ignore_lanes(&db, FROM, TO, &[strict_lifetime]).expect("query");

    // Assert
    assert!(by_min.lanes.is_empty(), "{by_min:?}");
    assert!(by_lifetime.lanes.is_empty(), "{by_lifetime:?}");
}

#[test]
fn a_provider_named_twice_counts_each_turn_once_under_its_first_rule() {
    let (_dir, db) = open_db();
    seed(&db, &positive_fixture());
    let strict = CacheIgnoreRule {
        min_prompt_tokens: 5001,
        ..rule("p")
    };

    let got = cache_ignore_lanes(&db, FROM, TO, &[rule("p"), strict.clone()]).expect("query");
    let strict_first = cache_ignore_lanes(&db, FROM, TO, &[strict, rule("p")]).expect("query");

    assert_eq!(got.lanes.len(), 1);
    assert_eq!(got.lanes[0].evidence_turns, 3);
    assert!(strict_first.lanes.is_empty(), "{strict_first:?}");
}

#[test]
fn empty_rules_return_empty_without_reading_the_ledger() {
    // Arrange: a connection on which any statement fails.
    let (_dir, db) = open_db();
    db.conn()
        .execute_batch("DROP TABLE requests")
        .expect("drop table");

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &[]).expect("empty rules read nothing");

    // Assert
    assert_eq!(got, CacheIgnoreLanes::default());
    assert!(
        cache_ignore_lanes(&db, FROM, TO, &rules()).is_err(),
        "control: a non-empty rule set does read the ledger"
    );
}

#[test]
fn absent_writes_and_reads_are_excluded_and_observed_zeros_are_evidence() {
    let cases: Vec<Case> = vec![
        (
            "write NULL, legacy NULL",
            |t| t[3].write_tokens = None,
            (2, 0),
        ),
        ("write 0", |t| t[3].write_tokens = Some(0), (3, 0)),
        (
            "legacy era: write NULL, 5m 0, 1h NULL, probe NULL",
            |t| {
                t[3].write_tokens = None;
                t[3].write_5m = Some(0);
                t[3].write_1h = None;
                t[3].probe = None;
            },
            (3, 0),
        ),
        (
            "legacy write only: 5m 0, 1h NULL",
            |t| {
                t[3].write_tokens = None;
                t[3].write_5m = Some(0);
            },
            (3, 0),
        ),
        ("probe NULL is not a probe", |t| t[3].probe = None, (3, 0)),
        (
            "operation NULL is inference",
            |t| t[3].operation = None,
            (3, 0),
        ),
        ("cache_read NULL", |t| t[3].cache_read = None, (2, 0)),
        ("read > 0 is active", |t| t[3].cache_read = Some(10), (2, 1)),
        (
            "write > 0 is active",
            |t| t[3].write_tokens = Some(10),
            (2, 1),
        ),
        (
            "legacy 1h write > 0 is active",
            |t| {
                t[3].write_tokens = None;
                t[3].write_1h = Some(10);
            },
            (2, 1),
        ),
        (
            "read counts toward the minimum",
            |t| {
                t[3].input = 96;
                t[3].cache_read = Some(4000);
            },
            (2, 1),
        ),
        (
            "write counts toward the minimum",
            |t| {
                t[3].input = 96;
                t[3].write_tokens = Some(4000);
            },
            (2, 1),
        ),
        (
            "read + input one short of the minimum",
            |t| {
                t[3].input = 95;
                t[3].cache_read = Some(4000);
            },
            (2, 0),
        ),
    ];

    let red = mismatches(&rules(), &cases);
    assert!(red.is_empty(), "(case, got, expected): {red:#?}");
}

/// With no minimum, the prompt-size predicate cannot exclude the turn, so an
/// absent read or write is kept out by its own non-NULL check alone.
#[test]
fn absent_reads_and_writes_are_excluded_even_with_no_minimum() {
    let no_minimum: Vec<CacheIgnoreRule> = rules()
        .into_iter()
        .map(|r| CacheIgnoreRule {
            min_prompt_tokens: 0,
            ..r
        })
        .collect();
    let cases: Vec<Case> = vec![
        ("control: observed zeros", |_| {}, (3, 0)),
        ("cache_read NULL", |t| t[3].cache_read = None, (2, 0)),
        (
            "write NULL, legacy NULL",
            |t| t[3].write_tokens = None,
            (2, 0),
        ),
    ];

    let red = mismatches(&no_minimum, &cases);
    assert!(red.is_empty(), "(case, got, expected): {red:#?}");
}

#[test]
fn sessions_count_distinct_sessions_on_the_lane() {
    // Arrange: the positive fixture twice, in two sessions.
    let (_dir, db) = open_db();
    let mut turns = positive_fixture();
    turns.extend(positive_fixture().into_iter().map(|mut t| {
        t.session_id = Some("sess-2".to_owned());
        t
    }));
    seed(&db, &turns);

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &rules()).expect("query");

    // Assert
    assert_eq!(got.lanes.len(), 1);
    assert_eq!((got.lanes[0].evidence_turns, got.lanes[0].sessions), (6, 2));
}

#[test]
fn lanes_order_by_evidence_then_active_descending_then_keys() {
    // Arrange: lane m-b has 3 evidence; m-a and m-c have 1 evidence each,
    // m-c with one active turn as well.
    let (_dir, db) = open_db();
    let lane = |model: &'static str, n: i64, active_last: bool| {
        let mut turns: Vec<Turn> = (0..=n)
            .map(|i| Turn {
                model: model.to_owned(),
                ..turn(FROM + 10_000 + i * STEP_MS)
            })
            .collect();
        if active_last {
            turns.push(Turn {
                model: model.to_owned(),
                cache_read: Some(10),
                ..turn(FROM + 10_000 + (n + 1) * STEP_MS)
            });
        }
        turns
    };
    let mut turns = lane("m-a", 1, false);
    turns.extend(lane("m-b", 3, false));
    turns.extend(lane("m-c", 1, true));
    seed(&db, &turns);

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &rules()).expect("query");

    // Assert
    let order: Vec<(Option<&str>, i64, i64)> = got
        .lanes
        .iter()
        .map(|l| (l.model.as_deref(), l.evidence_turns, l.active_turns))
        .collect();
    assert_eq!(
        order,
        vec![
            (Some("m-b"), 3, 0),
            (Some("m-c"), 1, 1),
            (Some("m-a"), 1, 0)
        ]
    );
}

/// `n` lanes, each one evidence turn, distinct by model.
fn seed_lanes(db: &UsageDb, n: usize) {
    db.conn().execute_batch("BEGIN").expect("begin");
    for i in 0..n {
        for (k, ts) in [FROM + 10, FROM + 20].into_iter().enumerate() {
            let t = Turn {
                model: format!("m{i:04}"),
                ..turn(ts)
            };
            insert(db, &format!("lane{i}-{k}"), &t);
        }
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

#[test]
fn more_lanes_than_the_cap_return_the_cap_and_say_truncated() {
    let red: Vec<_> = [
        (CACHE_IGNORE_LANE_CAP + 1, CACHE_IGNORE_LANE_CAP, true),
        (CACHE_IGNORE_LANE_CAP, CACHE_IGNORE_LANE_CAP, false),
    ]
    .into_iter()
    .filter_map(|(lanes, expected_len, expected_truncated)| {
        let (_dir, db) = open_db();
        seed_lanes(&db, lanes);
        let got = cache_ignore_lanes(&db, FROM, TO, &rules()).expect("query");
        let got = (got.lanes.len(), got.truncated);
        (got != (expected_len, expected_truncated)).then_some((
            lanes,
            got,
            (expected_len, expected_truncated),
        ))
    })
    .collect();

    assert!(red.is_empty(), "(lanes, got, expected): {red:#?}");
}

#[test]
fn the_result_carries_no_session_id() {
    // Arrange
    let (_dir, db) = open_db();
    let turns: Vec<Turn> = positive_fixture()
        .into_iter()
        .map(|t| Turn {
            session_id: Some("session-key-marker".to_owned()),
            ..t
        })
        .collect();
    seed(&db, &turns);

    // Act
    let got = cache_ignore_lanes(&db, FROM, TO, &rules()).expect("query");

    // Assert: the control proves the fixture produced a lane to inspect.
    assert_eq!(got.lanes.len(), 1, "control: one evidence lane");
    let rendered = format!("{got:?}");
    assert!(
        !rendered.contains("session-key-marker"),
        "no session id may leave the query: {rendered}"
    );
}

#[test]
fn the_window_scan_rides_the_ts_start_index() {
    let (_dir, db) = open_db();
    seed(&db, &positive_fixture());

    let plan: Vec<String> = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {CACHE_IGNORE_SQL}"))
        .expect("prepare explain")
        .query_map(
            rusqlite::params![
                FROM,
                TO,
                rules_json(&rules()),
                "ok",
                CALLER_SUPPLIED_TOKEN,
                ANTHROPIC_API_KIND,
                BEDROCK_KIND,
                PREFIX_EPOCH_STABLE,
                257,
            ],
            |row| row.get::<_, String>(3),
        )
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    assert!(
        plan.iter().any(|d| d.contains("idx_requests_ts_start")),
        "the cache-ignore query must use idx_requests_ts_start; plan was {plan:?}"
    );
}

/// Rows the bench seeds per transaction, so the build never holds more than
/// one batch of pending pages.
const BENCH_BATCH: usize = 10_000;
const BENCH_ROWS: usize = 1_000_000;
const BENCH_WINDOW_ROWS: usize = 60_000;
const BENCH_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;
const BENCH_SPAN_MS: i64 = 80 * 24 * 60 * 60 * 1000;
const BENCH_BUDGET: Duration = Duration::from_millis(250);

/// Seed `BENCH_ROWS` rows ending at `now_ms`: `BENCH_WINDOW_ROWS` of them in
/// the last 6 h, the rest spread over the preceding ~80 days. Sessions run
/// eight turns on one of 24 lanes; about 31% of turns write a positive cache
/// total, the live share, so the bench does not read a flattering all-zero
/// ledger.
fn seed_bench(path: &std::path::Path, now_ms: i64) {
    let db = open(path).expect("open bench ledger");
    let old_rows = (BENCH_ROWS - BENCH_WINDOW_ROWS) as i64;
    let old_step = (BENCH_SPAN_MS - BENCH_WINDOW_MS) / old_rows;
    let new_step = BENCH_WINDOW_MS / BENCH_WINDOW_ROWS as i64;
    let models = ["m-a", "m-b", "m-c"];
    let kinds = [
        "anthropic-api",
        "bedrock",
        "openai-responses",
        "anthropic-api",
    ];
    let providers = ["p", "p2"];
    for start in (0..BENCH_ROWS).step_by(BENCH_BATCH) {
        db.conn().execute_batch("BEGIN").expect("begin");
        for i in start..(start + BENCH_BATCH).min(BENCH_ROWS) {
            let ts = if (i as i64) < old_rows {
                now_ms - BENCH_SPAN_MS + i as i64 * old_step
            } else {
                now_ms - BENCH_WINDOW_MS + (i as i64 - old_rows) * new_step
            };
            let session = i / 8;
            let lane = session % 24;
            let writes = i % 100 < 31;
            let t = Turn {
                ts_start: ts,
                session_id: Some(format!("sess-{session}")),
                provider: providers[lane % 2],
                model: models[lane % 3].to_owned(),
                provider_kind: kinds[lane % 4],
                cache_read: Some(if i % 3 == 0 { 0 } else { 4000 }),
                write_tokens: Some(if writes { 1200 } else { 0 }),
                ..turn(ts)
            };
            insert(&db, &format!("bench-{i}"), &t);
        }
        db.conn().execute_batch("COMMIT").expect("commit");
    }
}

/// The query alone over a 6 h window on a 1M-row ledger, on a fresh
/// read-only connection (OS cache warm from the seeding). `#[ignore]`d:
/// seeding costs minutes of disk I/O.
///
/// ```text
/// cargo test -p routectl-usage --release --lib -- --ignored --exact \
///   query::cache_ignore::tests::a_million_row_ledger_answers_inside_the_served_budget \
///   --nocapture
/// ```
#[test]
#[ignore = "seeds a 1M-row ledger; see docs/DEVELOPMENT.md \"Explicit runs\""]
fn a_million_row_ledger_answers_inside_the_served_budget() {
    // Arrange
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("ms fit i64");
    seed_bench(&path, now_ms);
    let from_ms = now_ms - BENCH_WINDOW_MS;
    let db = open_readonly(&path).expect("open read-only");
    let in_window: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM requests WHERE ts_start >= ?1 AND ts_start < ?2",
            rusqlite::params![from_ms, now_ms + 1],
            |row| row.get(0),
        )
        .expect("count window");
    assert!(in_window >= 50_000, "window holds {in_window} rows");
    drop(db);
    let db = open_readonly(&path).expect("fresh read-only connection");

    // Act
    let started = Instant::now();
    let got = cache_ignore_lanes(&db, from_ms, now_ms + 1, &rules()).expect("query");
    let elapsed = started.elapsed();

    // Assert
    println!(
        "{BENCH_ROWS} rows, {in_window} in window: {} lanes in {elapsed:?} (budget {BENCH_BUDGET:?})",
        got.lanes.len()
    );
    assert!(!got.lanes.is_empty(), "the bench must count some lanes");
    assert!(elapsed < BENCH_BUDGET, "took {elapsed:?}");
}
