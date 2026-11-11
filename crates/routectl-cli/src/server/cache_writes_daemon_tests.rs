//! Prompt-cache writes on the persisted ledger row, end to end through the
//! assembled daemon: real HTTP into its ingress, the real router and
//! egresses dialing mock origins, the real usage writer, and the rows read
//! back from the file after a restart.
//!
//! The ledger starts as a seeded schema-20 file holding one row with a
//! legacy 5-minute write. The first daemon migrates it and serves one
//! request per case below, complete and streamed, on an anthropic-api and an
//! openai-responses provider; a second daemon then boots on the migrated
//! file, and every row is read again.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
use routectl_usage::{UsageRecord, UsageWriter};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::observation_reconcile_daemon_tests::{Daemon, usage_dir};

const ANTHROPIC_ALIAS: &str = "anthropic";
const RESPONSES_ALIAS: &str = "responses";
const UPSTREAM: &str = "upstream";
const SEEDED_ID: &str = "seeded-legacy-write";

/// The two columns the v21 step appends, in physical order.
const V21_COLUMNS: [&str; 2] = ["cache_write_tokens", "cache_write_lifetimes"];
/// The last `requests` column a schema-20 file carries.
const LAST_V20_COLUMN: &str = "excluded_probe_run";

/// The token and cache-write columns of one ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheRow {
    input_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
    cache_write_tokens: Option<i64>,
    cache_write_lifetimes: Option<String>,
}

/// A row written in the cache-write era: legacy per-TTL columns NULL.
fn written(
    input_tokens: i64,
    cache_read: i64,
    total: Option<i64>,
    lifetimes: Option<&str>,
) -> CacheRow {
    CacheRow {
        input_tokens: Some(input_tokens),
        cache_read: Some(cache_read),
        cache_write_5m: None,
        cache_write_1h: None,
        cache_write_tokens: total,
        cache_write_lifetimes: lifetimes.map(str::to_string),
    }
}

fn cache_row(db_path: &std::path::Path, request_id: &str) -> CacheRow {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    db.conn()
        .query_row(
            "SELECT input_tokens, cache_read, cache_write_5m, cache_write_1h, \
                    cache_write_tokens, cache_write_lifetimes \
             FROM requests WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok(CacheRow {
                    input_tokens: r.get(0)?,
                    cache_read: r.get(1)?,
                    cache_write_5m: r.get(2)?,
                    cache_write_1h: r.get(3)?,
                    cache_write_tokens: r.get(4)?,
                    cache_write_lifetimes: r.get(5)?,
                })
            },
        )
        .unwrap_or_else(|why| panic!("exactly one row for {request_id}: {why}"))
}

fn user_version(db_path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .expect("raw open")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

fn requests_columns(db_path: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db_path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('requests')")
        .expect("prepare table_info");
    stmt.query_map([], |r| r.get(0))
        .expect("query table_info")
        .collect::<Result<_, _>>()
        .expect("column names")
}

/// Write one row with a legacy 5-minute write through the current writer,
/// then take the file back to the schema-20 shape: any index over the two
/// v21 columns dropped, the columns dropped, and both version stamps
/// lowered. The row keeps a recent timestamp so the boot prune keeps it.
async fn seed_v20_ledger(db_path: &std::path::Path) {
    let path = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (handle, writer) = UsageWriter::start(path.clone(), 4, 0, true);
        let now = chrono::Local::now().timestamp_millis();
        handle.try_send(UsageRecord {
            request_id: SEEDED_ID.to_string(),
            alias: ANTHROPIC_ALIAS.to_string(),
            ts_start: now,
            ts_end: now,
            outcome: routectl_usage::Outcome::Ok,
            cache_write_5m: Some(11),
            ..UsageRecord::default()
        });
        drop(handle);
        writer.shutdown();
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index' \
                     AND tbl_name = 'requests' AND sql IS NOT NULL \
                     AND (sql LIKE '%cache_write_tokens%' OR sql LIKE '%cache_write_lifetimes%')",
                )
                .expect("prepare index scan");
            stmt.query_map([], |r| r.get(0))
                .expect("index scan")
                .collect::<Result<_, _>>()
                .expect("index names")
        };
        for index in indexes {
            conn.execute_batch(&format!("DROP INDEX {index}"))
                .expect("drop a v21 index");
        }
        for column in V21_COLUMNS.iter().rev() {
            conn.execute_batch(&format!("ALTER TABLE requests DROP COLUMN {column}"))
                .expect("drop a v21 column");
        }
        conn.execute_batch(
            "PRAGMA user_version = 20;
             UPDATE meta SET value = '20' WHERE key = 'schema_version';",
        )
        .expect("stamp 20");
    })
    .await
    .expect("seed task");
}

/// One Anthropic SSE frame per JSON value, each named by its `type`.
fn sse(frames: &[Value]) -> String {
    frames
        .iter()
        .map(|data| {
            format!(
                "event: {}\ndata: {data}\n\n",
                data["type"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

fn anthropic_complete(usage: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": usage
    }))
}

/// A whole Anthropic stream whose `message_start` carries `opener` and
/// whose closing `message_delta` carries `delta`.
fn anthropic_stream(opener: &Value, delta: &Value) -> ResponseTemplate {
    let body = sse(&[
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": opener}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": delta}),
        json!({"type": "message_stop"}),
    ]);
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A Responses stream ending in `response.completed` with `usage`. The
/// Responses egress reads every answer as a stream, complete or not.
fn responses_stream(usage: &Value) -> ResponseTemplate {
    let done = json!({
        "id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
        "status": "completed", "model": UPSTREAM,
        "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
        "usage": usage
    });
    let mut created = done.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    let frames = [
        json!({"type": "response.created", "response": created}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "message", "id": "m1", "role": "assistant",
                        "status": "completed", "content": []}}),
        json!({"type": "response.content_part.added", "output_index": 0, "content_index": 0,
               "part": {"type": "output_text", "text": "", "annotations": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0,
               "content_index": 0, "delta": "ok"}),
        json!({"type": "response.completed", "response": done}),
    ];
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(&frames))
}

/// The usage shape of a public OpenAI Responses report for a fresh prompt
/// (input 1941, of which 1938 written to the cache and none read), with
/// `input_tokens_details` replaced by `details`.
fn responses_usage(details: Option<Value>) -> Value {
    let mut usage = json!({"input_tokens": 1941, "output_tokens": 5, "total_tokens": 1946});
    if let Some(details) = details {
        usage["input_tokens_details"] = details;
    }
    usage
}

/// Which ingress and provider a case runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    AnthropicApi,
    OpenaiResponses,
}

/// One request the first daemon serves: its id (also the marker its
/// upstream matches on), its lane, whether the client streams, what the
/// upstream answers and the row the ledger must hold.
struct Case {
    id: &'static str,
    lane: Lane,
    stream: bool,
    answer: ResponseTemplate,
    expected: CacheRow,
}

/// Anthropic's documented closing `message_delta.usage` shape: input, both
/// cache totals and output, no `cache_creation` object.
fn total_only_delta(total: u32) -> Value {
    json!({"input_tokens": 2, "cache_creation_input_tokens": total,
           "cache_read_input_tokens": 0, "output_tokens": 7})
}

fn cases() -> Vec<Case> {
    let documented_write = Some(json!({"cache_write_tokens": 1938, "cached_tokens": 0}));
    vec![
        Case {
            id: "anthropic-complete-split",
            lane: Lane::AnthropicApi,
            stream: false,
            answer: anthropic_complete(&json!({
                "input_tokens": 2, "output_tokens": 7,
                "cache_creation_input_tokens": 2523, "cache_read_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 2000,
                                   "ephemeral_1h_input_tokens": 523}})),
            expected: written(2, 0, Some(2523), Some(r#"{"300":2000,"3600":523}"#)),
        },
        Case {
            id: "anthropic-stream-opener-split-total-only-delta",
            lane: Lane::AnthropicApi,
            stream: true,
            answer: anthropic_stream(
                &json!({"input_tokens": 2, "output_tokens": 1,
                        "cache_creation_input_tokens": 2523, "cache_read_input_tokens": 0,
                        "cache_creation": {"ephemeral_5m_input_tokens": 2523}}),
                &total_only_delta(2523),
            ),
            expected: written(2, 0, Some(2523), Some(r#"{"300":2523}"#)),
        },
        Case {
            id: "anthropic-stream-thirty-minute-lifetime",
            lane: Lane::AnthropicApi,
            stream: true,
            answer: anthropic_stream(
                &json!({"input_tokens": 2, "output_tokens": 1,
                        "cache_creation_input_tokens": 640, "cache_read_input_tokens": 0,
                        "cache_creation": {"ephemeral_30m_input_tokens": 640}}),
                &total_only_delta(640),
            ),
            expected: written(2, 0, Some(640), Some(r#"{"1800":640}"#)),
        },
        Case {
            id: "responses-complete-write-total",
            lane: Lane::OpenaiResponses,
            stream: false,
            answer: responses_stream(&responses_usage(documented_write.clone())),
            expected: written(3, 0, Some(1938), None),
        },
        Case {
            id: "responses-stream-write-total",
            lane: Lane::OpenaiResponses,
            stream: true,
            answer: responses_stream(&responses_usage(documented_write)),
            expected: written(3, 0, Some(1938), None),
        },
        Case {
            id: "responses-complete-no-write-field",
            lane: Lane::OpenaiResponses,
            stream: false,
            answer: responses_stream(&responses_usage(Some(json!({"cached_tokens": 0})))),
            expected: written(1941, 0, None, None),
        },
    ]
}

/// One origin per lane, each answering a case's request by the case id
/// its body carries.
struct Origins {
    anthropic: MockServer,
    responses: MockServer,
}

async fn origins(cases: &[Case]) -> Origins {
    let origins = Origins {
        anthropic: MockServer::start().await,
        responses: MockServer::start().await,
    };
    for case in cases {
        let origin = match case.lane {
            Lane::AnthropicApi => &origins.anthropic,
            Lane::OpenaiResponses => &origins.responses,
        };
        Mock::given(method("POST"))
            .and(body_string_contains(case.id))
            .respond_with(case.answer.clone())
            .mount(origin)
            .await;
    }
    origins
}

/// An anthropic-api and an openai-responses provider, one alias each, one
/// attempt per target, with the ledger at `usage_db`.
fn scenario_config(origins: &Origins, usage_db: &std::path::Path) -> Config {
    let key = crate::test_secret::file_ref("k");
    let providers = BTreeMap::from([
        (
            "pa".to_string(),
            ProviderEntry::anthropic_api(key.clone()).with_base_url(origins.anthropic.uri()),
        ),
        (
            "pr".to_string(),
            ProviderEntry::openai_responses(key)
                .with_openai_responses_auth_kind(
                    routectl_providers::openai_responses::AuthKind::ApiKey,
                )
                .with_openai_responses_base_url(origins.responses.uri()),
        ),
    ]);
    let models = BTreeMap::from([
        ("ma".to_string(), ModelEntry::new("pa", UPSTREAM)),
        ("mr".to_string(), ModelEntry::new("pr", UPSTREAM)),
    ]);
    let aliases = BTreeMap::from([
        (
            ANTHROPIC_ALIAS.to_string(),
            AliasValue::Single("ma".to_string()),
        ),
        (
            RESPONSES_ALIAS.to_string(),
            AliasValue::Single("mr".to_string()),
        ),
    ]);
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers,
        models,
        aliases,
        retry,
        ..Default::default()
    };
    config.usage.db_path = usage_db.to_path_buf();
    config
}

async fn boot(config: &Config) -> Daemon {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router: Router = super::build_router_from_config(Arc::new(config.clone()), secrets)
        .await
        .expect("build router");
    Daemon::spawn(config.clone(), router).await
}

/// Send `case` to `daemon` on its lane's ingress and return the status.
async fn send(daemon: &Daemon, case: &Case) -> u16 {
    let request = match case.lane {
        Lane::AnthropicApi => daemon.messages(
            case.id,
            &json!({
                "model": ANTHROPIC_ALIAS, "max_tokens": 16, "stream": case.stream,
                "messages": [{"role": "user", "content": case.id}]
            }),
        ),
        Lane::OpenaiResponses => reqwest::Client::new()
            .post(format!("{}/v1/responses", daemon.base_url))
            .header("x-request-id", case.id)
            .json(&json!({
                "model": RESPONSES_ALIAS, "max_output_tokens": 16, "stream": case.stream,
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": case.id}]}]
            })),
    };
    let response = request.send().await.expect("the daemon answers");
    let status = response.status().as_u16();
    // Drain the body so a streamed answer runs to its end before the row
    // is awaited.
    let _ = response.bytes().await;
    status
}

/// The scenario's ledger after the restart.
struct CacheLedger {
    restarted: Daemon,
    statuses: BTreeMap<&'static str, u16>,
    before_restart: BTreeMap<&'static str, CacheRow>,
    expected: BTreeMap<&'static str, CacheRow>,
    seeded_version: i64,
    seeded_columns: Vec<String>,
    migrated_version: i64,
    _origins: Origins,
    _dir: tempfile::TempDir,
    _xdg: routectl_testkit::ScopedEnv,
    _no_config: routectl_testkit::ScopedEnv,
    _xdg_dir: tempfile::TempDir,
}

impl CacheLedger {
    /// `request_id`'s row as the restarted daemon's file holds it.
    fn row(&self, request_id: &str) -> CacheRow {
        cache_row(&self.restarted.usage_db, request_id)
    }

    /// Every case on `lane`, its row after the restart beside its expected
    /// row.
    fn lane_rows(&self, lane: Lane) -> Vec<(&'static str, CacheRow, CacheRow)> {
        cases()
            .into_iter()
            .filter(|case| case.lane == lane)
            .map(|case| (case.id, self.row(case.id), self.expected[case.id].clone()))
            .collect()
    }
}

/// Seed a schema-20 ledger under a temp config home, serve every case on a
/// daemon that migrates it, stop it, and boot a second daemon on the same
/// file. The caller holds `#[serial_test::serial]` for the env guards.
async fn cache_ledger() -> CacheLedger {
    let xdg_dir = tempfile::tempdir().expect("config home tempdir");
    let xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", xdg_dir.path());
    let no_config = routectl_testkit::ScopedEnv::unset("ROUTECTL_CONFIG");
    let (dir, usage_db) = usage_dir();
    seed_v20_ledger(&usage_db).await;
    let seeded_version = user_version(&usage_db);
    let seeded_columns = requests_columns(&usage_db);
    let cases = cases();
    let origins = origins(&cases).await;
    let config = scenario_config(&origins, &usage_db);

    let first = boot(&config).await;
    let migrated_version = user_version(&usage_db);
    let mut statuses = BTreeMap::new();
    for case in &cases {
        statuses.insert(case.id, send(&first, case).await);
    }
    let mut before_restart = BTreeMap::new();
    for case in &cases {
        first.await_row(case.id).await;
        before_restart.insert(case.id, cache_row(&usage_db, case.id));
    }
    first.shutdown().await;

    let restarted = boot(&config).await;
    CacheLedger {
        restarted,
        statuses,
        before_restart,
        expected: cases.into_iter().map(|c| (c.id, c.expected)).collect(),
        seeded_version,
        seeded_columns,
        migrated_version,
        _origins: origins,
        _dir: dir,
        _xdg: xdg,
        _no_config: no_config,
        _xdg_dir: xdg_dir,
    }
}

/// Mutation check: make the closing `message_delta` that states a write
/// total keep only its own split (the delta split or None) -> red here on
/// the opener-split stream row, whose split reads NULL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn anthropic_rows_carry_the_write_total_and_canonical_lifetimes_with_legacy_columns_null() {
    // Arrange + Act
    let ledger = cache_ledger().await;

    // Assert
    let rows = ledger.lane_rows(Lane::AnthropicApi);
    assert_eq!(rows.len(), 3, "premise: three anthropic-api cases");
    for (id, row, expected) in rows {
        assert_eq!(ledger.statuses[id], 200, "{id}: status");
        assert_eq!(row, expected, "{id}");
    }
    ledger.restarted.shutdown().await;
}

/// The Responses write total lands with no split and with the written
/// tokens taken out of `input_tokens`; a usage object with no write field
/// leaves the total NULL, never 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn responses_rows_carry_the_write_total_without_a_split_and_null_when_unreported() {
    // Arrange + Act
    let ledger = cache_ledger().await;

    // Assert
    let rows = ledger.lane_rows(Lane::OpenaiResponses);
    assert_eq!(rows.len(), 3, "premise: three openai-responses cases");
    for (id, row, expected) in rows {
        assert_eq!(ledger.statuses[id], 200, "{id}: status");
        assert_eq!(row, expected, "{id}");
    }
    ledger.restarted.shutdown().await;
}

/// The seeded file is schema 20 without the cache-write columns; the first
/// boot migrates it, the seeded legacy row keeps its write with both new
/// columns NULL, and every served row reads the same once a second daemon
/// has booted on the migrated file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn every_cache_write_row_reads_the_same_after_a_restart_on_the_migrated_file() {
    // Arrange + Act
    let ledger = cache_ledger().await;

    // Assert
    assert_eq!(ledger.seeded_version, 20, "premise: a schema-20 ledger");
    assert_eq!(
        ledger.seeded_columns.last().map(String::as_str),
        Some(LAST_V20_COLUMN),
        "premise: the seeded file ends after its v20 columns"
    );
    for column in V21_COLUMNS {
        assert!(
            !ledger.seeded_columns.iter().any(|c| c == column),
            "premise: no {column} before the migration"
        );
    }
    assert_eq!(
        ledger.migrated_version,
        routectl_usage::SCHEMA_VERSION,
        "the first boot migrates"
    );
    assert_eq!(
        user_version(&ledger.restarted.usage_db),
        routectl_usage::SCHEMA_VERSION
    );
    assert_eq!(
        ledger.row(SEEDED_ID),
        CacheRow {
            input_tokens: None,
            cache_read: None,
            cache_write_5m: Some(11),
            cache_write_1h: None,
            cache_write_tokens: None,
            cache_write_lifetimes: None,
        },
        "the pre-migration row"
    );
    assert_eq!(ledger.before_restart.len(), cases().len());
    for (id, before) in &ledger.before_restart {
        assert_eq!(&ledger.row(id), before, "{id}");
        assert_eq!(before, &ledger.expected[id], "{id}: served values");
    }
    ledger.restarted.shutdown().await;
}
