//! The five walk counts on the persisted ledger row, end to end through the
//! assembled daemon: real HTTP into the Anthropic ingress, the real router
//! and egresses dialing mock origins, the real usage writer, and the rows
//! read back from the file after a restart.
//!
//! The ledger starts as a seeded schema-19 file. The first daemon migrates
//! it and serves four known requests; a second daemon then boots on the
//! migrated file, and every row is read again. That second daemon serves
//! with its config on disk, so its doctor builds the configured report.
//!
//! - `walk`: A (origin X) -> B (origin X) -> C (origin Y). A answers a
//!   generic 400, which excludes origin X, so B is passed over and C serves.
//! - `feature`: one `[models]` entry named twice, then a second entry, all
//!   on origin Z. The first entry answers an `unsupported_parameter` 400,
//!   which excludes its model, so its second chain position is passed over
//!   and the second entry serves.
//! - a token count on `walk`: A answers the count with a generic 400, and a
//!   count does not walk on a health error.
//! - an alias no config names: refused before the walk starts.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, Router};
use routectl_usage::{UsageRecord, UsageWriter};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::observation_reconcile_daemon_tests::{Daemon, WAIT, usage_dir};

const WALK_ALIAS: &str = "walk";
const FEATURE_ALIAS: &str = "feature";
const UNKNOWN_ALIAS: &str = "no-such-alias";

pub(super) const GENERIC_400_ID: &str = "walk-generic-400";
pub(super) const FEATURE_UNSUPPORTED_ID: &str = "walk-feature-unsupported";
pub(super) const COUNT_TOKENS_ID: &str = "walk-count-tokens";
pub(super) const REFUSED_BEFORE_WALK_ID: &str = "walk-refused-before-walk";
const SEEDED_ID: &str = "seeded-before-migration";

/// Every request the first daemon serves, in send order.
pub(super) const SERVED_IDS: [&str; 4] = [
    GENERIC_400_ID,
    FEATURE_UNSUPPORTED_ID,
    COUNT_TOKENS_ID,
    REFUSED_BEFORE_WALK_ID,
];

const WALK_COLUMNS: &str = "request_caused_failures, excluded_endpoint, excluded_model, \
     excluded_target, excluded_probe_run";

/// The walk-count columns, the outcome and the served provider of one row.
/// A count is `None` when its column is NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WalkRow {
    pub(super) outcome: String,
    pub(super) provider: Option<String>,
    pub(super) counts: [Option<i64>; 5],
}

/// Five counts as a row stores them, in [`WALK_COLUMNS`] order.
const fn stored(counts: [i64; 5]) -> [Option<i64>; 5] {
    [
        Some(counts[0]),
        Some(counts[1]),
        Some(counts[2]),
        Some(counts[3]),
        Some(counts[4]),
    ]
}

pub(super) fn walk_row(db_path: &std::path::Path, request_id: &str) -> WalkRow {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    db.conn()
        .query_row(
            &format!(
                "SELECT outcome, provider, {WALK_COLUMNS} FROM requests WHERE request_id = ?1"
            ),
            [request_id],
            |r| {
                Ok(WalkRow {
                    outcome: r.get(0)?,
                    provider: r.get(1)?,
                    counts: [r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
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

fn walk_column_count(db_path: &std::path::Path) -> i64 {
    let names = WALK_COLUMNS
        .split(", ")
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    rusqlite::Connection::open(db_path)
        .expect("raw open")
        .query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('requests') WHERE name IN ({names})"),
            [],
            |r| r.get(0),
        )
        .expect("count walk columns")
}

/// Write one row through the current writer, then take the file back to the
/// schema-19 shape: the five walk columns dropped and both version stamps
/// lowered. The row keeps a recent timestamp so the boot prune keeps it.
async fn seed_v19_ledger(db_path: &std::path::Path) {
    let path = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (handle, writer) = UsageWriter::start(path.clone(), 4, 0, true);
        let now = chrono::Local::now().timestamp_millis();
        handle.try_send(UsageRecord {
            request_id: SEEDED_ID.to_string(),
            alias: WALK_ALIAS.to_string(),
            ts_start: now,
            ts_end: now,
            outcome: routectl_usage::Outcome::Ok,
            ..UsageRecord::default()
        });
        drop(handle);
        writer.shutdown();
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        for column in WALK_COLUMNS.split(", ") {
            conn.execute_batch(&format!("ALTER TABLE requests DROP COLUMN {column}"))
                .expect("drop a walk column");
        }
        conn.execute_batch(
            "PRAGMA user_version = 19;
             UPDATE meta SET value = '19' WHERE key = 'schema_version';",
        )
        .expect("stamp 19");
    })
    .await
    .expect("seed task");
}

fn generic_400() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": "invalid request"}
    }))
}

fn anthropic_served() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_walk", "type": "message", "role": "assistant", "model": "served",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn unsupported_parameter_400() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": {"type": "invalid_request_error", "code": "unsupported_parameter",
                  "message": "Unsupported parameter."}
    }))
}

fn chat_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": "served",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

/// Origin X: `up-a` fails every call with a generic 400; anything else is
/// served, on both the messages and the token-count paths.
async fn origin_x() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"model": "up-a"})))
        .respond_with(generic_400())
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(anthropic_served())
        .mount(&server)
        .await;
    server
}

async fn serving_origin(answer: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

/// Origin Z: every call for `up-fa` answers `unsupported_parameter`; anything
/// else is served.
async fn origin_z() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"model": "up-fa"})))
        .respond_with(unsupported_parameter_400())
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(chat_completion())
        .mount(&server)
        .await;
    server
}

/// The upstream origins the scenario's targets dial.
struct Origins {
    x: MockServer,
    y: MockServer,
    z: MockServer,
}

fn anthropic_at(origin: &MockServer) -> ProviderEntry {
    ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")).with_base_url(origin.uri())
}

fn openai_at(origin: &MockServer) -> ProviderEntry {
    ProviderEntry::openai_compat(origin.uri(), crate::test_secret::file_ref("k"))
}

/// Both aliases over `origins`, one attempt per target, the ledger at
/// `usage_db`.
fn scenario_config(origins: &Origins, usage_db: &std::path::Path) -> Config {
    let mut config = Config {
        providers: BTreeMap::from([
            ("pa".to_string(), anthropic_at(&origins.x)),
            ("pb".to_string(), anthropic_at(&origins.x)),
            ("pc".to_string(), anthropic_at(&origins.y)),
            ("fa".to_string(), openai_at(&origins.z)),
            ("fb".to_string(), openai_at(&origins.z)),
        ]),
        models: BTreeMap::from([
            ("m-a".to_string(), ModelEntry::new("pa", "up-a")),
            ("m-b".to_string(), ModelEntry::new("pb", "up-b")),
            ("m-c".to_string(), ModelEntry::new("pc", "up-c")),
            ("f-a".to_string(), ModelEntry::new("fa", "up-fa")),
            ("f-b".to_string(), ModelEntry::new("fb", "up-fb")),
        ]),
        aliases: BTreeMap::from([
            (
                WALK_ALIAS.to_string(),
                AliasValue::Chain(vec!["m-a".into(), "m-b".into(), "m-c".into()]),
            ),
            (
                FEATURE_ALIAS.to_string(),
                AliasValue::Chain(vec!["f-a".into(), "f-a".into(), "f-b".into()]),
            ),
        ]),
        ..Default::default()
    };
    config.retry.max_attempts = 1;
    config.retry.initial_backoff_ms = 0;
    config.retry.jitter_ms = 0;
    config.usage.db_path = usage_db.to_path_buf();
    config
}

async fn scenario_router(config: &Config) -> Router {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    super::build_router_from_config(Arc::new(config.clone()), secrets)
        .await
        .expect("build router")
}

async fn boot(config: &Config) -> Daemon {
    Daemon::spawn(config.clone(), scenario_router(config).await).await
}

/// A daemon serving with its config file on disk, the branch on which
/// `/status/doctor` builds its report.
pub(super) struct ConfiguredDaemon {
    pub(super) base_url: String,
    pub(super) usage_db: std::path::PathBuf,
    handle: tokio::task::JoinHandle<()>,
}

impl ConfiguredDaemon {
    async fn spawn(config: &Config, config_path: std::path::PathBuf) -> Self {
        let router = scenario_router(config).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
        let served = Arc::new(config.clone());
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let handle = tokio::spawn(async move {
            super::serve::serve_on_listener_with_injected_router(
                served,
                listener,
                Some(config_path),
                Some(secrets),
                super::serve::DaemonTestSeams {
                    injected_router: Some(router),
                    ..Default::default()
                },
            )
            .await
            .expect("the daemon serves");
        });
        let daemon = Self {
            base_url,
            usage_db: config.usage.db_path.clone(),
            handle,
        };
        daemon.await_health().await;
        daemon
    }

    async fn await_health(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// GET a status path; it must answer 200 with a JSON body.
    async fn get_json(&self, status_path: &str) -> Value {
        assert!(!self.handle.is_finished(), "the daemon exited mid-test");
        let response = reqwest::get(format!("{}{status_path}", self.base_url))
            .await
            .expect("the daemon answers");
        assert_eq!(response.status().as_u16(), 200, "{status_path}");
        response.json().await.expect("status body parses")
    }

    pub(super) async fn shutdown(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// The config file the restarted daemon is handed: the same ledger path its
/// in-memory config carries.
fn write_config_file(dir: &std::path::Path, usage_db: &std::path::Path) -> std::path::PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {}\n[usage]\ndb_path = \"{}\"\n",
            routectl_router::CURRENT_CONFIG_VERSION,
            usage_db.display(),
        ),
    )
    .expect("write the config file");
    config_path
}

fn message_body(alias: &str) -> Value {
    json!({
        "model": alias, "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

async fn send_message(daemon: &Daemon, request_id: &str, alias: &str) -> u16 {
    let response = daemon
        .messages(request_id, &message_body(alias))
        .send()
        .await
        .expect("the daemon answers");
    response.status().as_u16()
}

async fn send_count(daemon: &Daemon, request_id: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens", daemon.base_url))
        .header("anthropic-version", "2023-06-01")
        .header("x-request-id", request_id)
        .json(&json!({
            "model": WALK_ALIAS,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("the daemon answers")
        .status()
        .as_u16()
}

/// The scenario's ledger after the restart.
pub(super) struct WalkLedger {
    /// The daemon booted on the migrated file, still serving.
    pub(super) restarted: ConfiguredDaemon,
    /// Each served request's row as the first daemon left it.
    pub(super) before_restart: BTreeMap<&'static str, WalkRow>,
    /// The status each served request drew, keyed like `before_restart`.
    pub(super) statuses: BTreeMap<&'static str, u16>,
    /// The ledger's schema version once the first daemon had booted.
    pub(super) migrated_version: i64,
    /// The config both daemons served.
    pub(super) config: Config,
    _origins: Origins,
    _dir: tempfile::TempDir,
}

impl WalkLedger {
    /// `request_id`'s row as the restarted daemon's file holds it.
    pub(super) fn row(&self, request_id: &str) -> WalkRow {
        walk_row(&self.restarted.usage_db, request_id)
    }
}

/// Seed a schema-19 ledger, serve the four requests on a daemon that
/// migrates it, stop it, and boot a second daemon on the same file.
pub(super) async fn walk_ledger() -> WalkLedger {
    let (dir, usage_db) = usage_dir();
    seed_v19_ledger(&usage_db).await;
    assert_eq!(user_version(&usage_db), 19, "premise: a schema-19 ledger");
    assert_eq!(walk_column_count(&usage_db), 0, "premise: no walk columns");
    let origins = Origins {
        x: origin_x().await,
        y: serving_origin(anthropic_served()).await,
        z: origin_z().await,
    };
    let config = scenario_config(&origins, &usage_db);

    let first = boot(&config).await;
    let migrated_version = user_version(&usage_db);
    let statuses = BTreeMap::from([
        (
            GENERIC_400_ID,
            send_message(&first, GENERIC_400_ID, WALK_ALIAS).await,
        ),
        (
            FEATURE_UNSUPPORTED_ID,
            send_message(&first, FEATURE_UNSUPPORTED_ID, FEATURE_ALIAS).await,
        ),
        (COUNT_TOKENS_ID, send_count(&first, COUNT_TOKENS_ID).await),
        (
            REFUSED_BEFORE_WALK_ID,
            send_message(&first, REFUSED_BEFORE_WALK_ID, UNKNOWN_ALIAS).await,
        ),
    ]);
    let mut before_restart = BTreeMap::new();
    for id in SERVED_IDS {
        first.await_row(id).await;
        before_restart.insert(id, walk_row(&usage_db, id));
    }
    first.shutdown().await;

    let restarted =
        ConfiguredDaemon::spawn(&config, write_config_file(dir.path(), &usage_db)).await;
    WalkLedger {
        restarted,
        before_restart,
        statuses,
        migrated_version,
        config,
        _origins: origins,
        _dir: dir,
    }
}

/// Mutation check: delete the `set_walk_counts` call in `finish_dispatch` ->
/// red here (every count reads 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generic_400_row_records_one_request_caused_failure_and_one_excluded_endpoint_served_by_c()
 {
    // Arrange + Act
    let ledger = walk_ledger().await;

    // Assert
    assert_eq!(ledger.statuses[GENERIC_400_ID], 200, "C serves");
    assert_eq!(
        ledger.row(GENERIC_400_ID),
        WalkRow {
            outcome: "ok".to_string(),
            provider: Some("pc".to_string()),
            counts: stored([1, 1, 0, 0, 0]),
        }
    );
    ledger.restarted.shutdown().await;
}

/// Mutation check: drop `Model` from the request-caused bump in
/// `Disposition::record_on` -> red here (the first count reads 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feature_unsupported_400_row_records_one_request_caused_failure_and_one_excluded_model() {
    // Arrange + Act
    let ledger = walk_ledger().await;

    // Assert
    assert_eq!(ledger.statuses[FEATURE_UNSUPPORTED_ID], 200, "f-b serves");
    assert_eq!(
        ledger.row(FEATURE_UNSUPPORTED_ID),
        WalkRow {
            outcome: "ok".to_string(),
            provider: Some("fb".to_string()),
            counts: stored([1, 0, 1, 0, 0]),
        }
    );
    ledger.restarted.shutdown().await;
}

/// The same 400 that walks an inference on to C ends a token count where it
/// failed: the count row carries its own walk, one failure and nothing
/// passed over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_count_tokens_row_records_the_counts_of_its_own_walk() {
    // Arrange + Act
    let ledger = walk_ledger().await;

    // Assert
    assert_eq!(ledger.statuses[COUNT_TOKENS_ID], 400, "the count ends at A");
    assert_eq!(
        ledger.row(COUNT_TOKENS_ID),
        WalkRow {
            outcome: "upstream_error".to_string(),
            provider: Some("pa".to_string()),
            counts: stored([1, 0, 0, 0, 0]),
        }
    );
    ledger.restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_refused_before_the_walk_records_all_five_counts_as_zero_not_null() {
    // Arrange + Act
    let ledger = walk_ledger().await;

    // Assert
    assert_ne!(ledger.statuses[REFUSED_BEFORE_WALK_ID], 200);
    let row = ledger.row(REFUSED_BEFORE_WALK_ID);
    assert_eq!(row.provider, None, "no target was dialed: {row:?}");
    assert_eq!(row.counts, stored([0; 5]));
    ledger.restarted.shutdown().await;
}

/// The first daemon migrates the seeded file, its seeded row stays NULL in
/// every walk column, and every served row reads the same once a second
/// daemon has booted on the migrated file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_walk_row_reads_the_same_after_a_restart_on_the_migrated_file() {
    // Arrange + Act
    let ledger = walk_ledger().await;

    // Assert
    assert_eq!(
        ledger.migrated_version,
        routectl_usage::SCHEMA_VERSION,
        "the first boot migrates"
    );
    assert_eq!(
        user_version(&ledger.restarted.usage_db),
        routectl_usage::SCHEMA_VERSION
    );
    assert_eq!(ledger.row(SEEDED_ID).counts, [None; 5], "a pre-v20 row");
    for id in SERVED_IDS {
        assert_eq!(ledger.row(id), ledger.before_restart[id], "{id}");
    }
    ledger.restarted.shutdown().await;
}

/// The five walk counts summed over every row the restarted daemon's file
/// holds, a NULL count adding 0.
fn ledger_walk_totals(ledger: &WalkLedger) -> [i64; 5] {
    let mut totals = [0_i64; 5];
    for id in SERVED_IDS.into_iter().chain([SEEDED_ID]) {
        for (total, count) in totals.iter_mut().zip(ledger.row(id).counts) {
            *total += count.unwrap_or(0);
        }
    }
    totals
}

/// The five walk fields of a status panel's `observation` block, in
/// [`WALK_COLUMNS`] order.
fn observation_walk(panel: &Value) -> [i64; 5] {
    let observation = &panel["data"]["observation"];
    let mut fields = WALK_COLUMNS.split(", ").map(|name| {
        observation[name]
            .as_i64()
            .unwrap_or_else(|| panic!("observation.{name} is an integer: {panel}"))
    });
    std::array::from_fn(|_| fields.next().expect("five walk fields"))
}

/// The line `routectl usage` prints for a window whose walk counts are
/// `totals` and whose ledger recorded no gap.
fn walk_line(totals: [i64; 5]) -> String {
    let [caused, endpoint, model, target, probe_run] = totals;
    format!(
        "walk: request-caused failures={caused}  |  excluded endpoint={endpoint} \
         model={model} target={target} probe-run={probe_run}"
    )
}

/// Every window block `routectl usage` prints over `usage_db`, rendered
/// from the file as the command reads it.
fn usage_cli_output(config: &Config, usage_db: &std::path::Path) -> String {
    let db = routectl_usage::open_readonly(usage_db).expect("read-only open");
    let args = crate::commands::usage::UsageArgs {
        window: crate::commands::usage::WindowFlag::None,
        since: None,
        until: None,
        by: None,
        detail: false,
        db: Some(usage_db.to_path_buf()),
        k_calibration: false,
    };
    crate::commands::usage::build_output(
        &db,
        config,
        &crate::commands::pricing::PriceSnapshot::default(),
        &args,
        chrono::Local::now(),
    )
    .expect("the usage report builds")
}

/// After the restart on the migrated file, the window totals of the ledger
/// rows read back unchanged from `routectl usage` (every window block),
/// `/status/usage` (today and all time) and the served `/status/doctor`
/// (today). The totals are pinned to the scenario's figures first, so the
/// comparison cannot pass on an all-zero ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn walk_counts_match_across_usage_cli_status_usage_and_status_doctor() {
    // Arrange
    let xdg = tempfile::tempdir().expect("config home tempdir");
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    let _no_config = routectl_testkit::ScopedEnv::unset("ROUTECTL_CONFIG");
    let ledger = walk_ledger().await;
    let expected = ledger_walk_totals(&ledger);
    assert_eq!(
        expected,
        [3, 1, 1, 0, 0],
        "premise: the scenario's rows sum to three request-caused failures, \
         one endpoint and one model exclusion"
    );

    // Act
    let cli = usage_cli_output(&ledger.config, &ledger.restarted.usage_db);
    let usage_today = ledger
        .restarted
        .get_json("/status/usage?window=today")
        .await;
    let usage_all = ledger.restarted.get_json("/status/usage?window=all").await;
    let doctor = ledger.restarted.get_json("/status/doctor").await;

    // Assert
    let cli_walk_lines: Vec<&str> = cli.lines().filter(|l| l.starts_with("walk:")).collect();
    assert_eq!(
        cli_walk_lines,
        vec![walk_line(expected).as_str(); 4],
        "one walk line per window block: {cli}"
    );
    for (surface, panel, schema) in [
        ("/status/usage?window=today", &usage_today, 6),
        ("/status/usage?window=all", &usage_all, 6),
        ("/status/doctor", &doctor, 30),
    ] {
        assert_eq!(panel["schema_version"], schema, "{surface}: {panel}");
        assert_eq!(observation_walk(panel), expected, "{surface}");
        assert_eq!(panel["data"]["observation"]["complete"], true, "{surface}");
    }
    ledger.restarted.shutdown().await;
}
