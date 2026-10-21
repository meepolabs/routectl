//! An expired Bedrock Mantle credential, end to end: a real daemon, real HTTP
//! into its ingress, a real openai-compat Mantle egress against a mock origin
//! answering with the recorded expired-key 401. The terminal row carries the
//! reason, the health panel shows the park, the served doctor warns about it,
//! and the next request never reaches the origin.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use routectl_providers::bedrock::auth::ResolvedCreds;
use routectl_providers::mantle::MantleAuth;
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, ResolvedModel, Router, parse_config};
use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::wire_loss_daemon_tests::Daemon;

const MANTLE_CAPTURE: &str = include_str!(
    "../../../routectl-providers/tests/fixtures/bedrock_mantle_credential_capture.json"
);
const ALIAS: &str = "a";
const SEAT: &str = "mantle";
const BACKUP: &str = "backup";
const UPSTREAM: &str = "openai.gpt-oss-120b-1:0";
const REGION: &str = "us-east-1";
const EXPIRED: &str = "credential-expired";

/// The recorded Mantle chat-vocabulary 401 for an expired API key.
fn expired_401() -> ResponseTemplate {
    let capture: Value = serde_json::from_str(MANTLE_CAPTURE).expect("capture is JSON");
    let row = &capture["responses"]["mantle-chat-expired"];
    let status = u16::try_from(row["status"].as_u64().expect("status")).expect("u16 status");
    ResponseTemplate::new(status)
        .insert_header("content-type", "application/json")
        .set_body_string(row["body"].as_str().expect("body"))
}

fn ok_200() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM, "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

fn bearer_key() -> String {
    ["mantle", "park", "test", "key"].join("-")
}

/// The on-disk config: the Mantle seat (and, with `backup`, a first-party
/// openai-compat fallback) behind one alias.
fn config_text(backup: Option<&str>) -> String {
    let key_ref = crate::test_secret::file_ref(&bearer_key());
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{SEAT}]\n\
         kind = \"openai-compat\"\n\
         base_url = \"\"\n\
         api_key_ref = \"\"\n\
         bedrock_mantle = {{ region = \"{REGION}\", creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }} }}\n\n\
         [models.{SEAT}]\n\
         provider = \"{SEAT}\"\n\
         upstream = \"{UPSTREAM}\"\n\n"
    );
    let chain = match backup {
        None => format!("\"{SEAT}\""),
        Some(origin) => {
            let backup_key = crate::test_secret::file_ref("backup-key");
            text.push_str(&format!(
                "[providers.{BACKUP}]\n\
                 kind = \"openai-compat\"\n\
                 base_url = \"{origin}\"\n\
                 api_key_ref = \"{backup_key}\"\n\n\
                 [models.{BACKUP}]\n\
                 provider = \"{BACKUP}\"\n\
                 upstream = \"{UPSTREAM}\"\n\n"
            ));
            format!("[\"{SEAT}\", \"{BACKUP}\"]")
        }
    };
    text.push_str(&format!(
        "[aliases]\n{ALIAS} = {chain}\n\n[retry]\nmax_attempts = 1\n"
    ));
    text
}

fn compat_provider(id: &str, base_url: &str, mantle: Option<MantleAuth>) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: id.into(),
        base_url: base_url.into(),
        api_key: if mantle.is_some() {
            String::new()
        } else {
            "backup-key".into()
        },
        header_extras: Vec::new(),
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        mantle,
    })
}

/// The router: the Mantle seat aimed at `mantle_origin`, and the backup seat
/// at `backup_origin` when one is given.
fn router(config: Arc<Config>, mantle_origin: &str, backup_origin: Option<&str>) -> Router {
    let mantle = MantleAuth {
        region: REGION.into(),
        creds: ResolvedCreds::Bearer { key: bearer_key() },
    };
    let mut models = BTreeMap::from([(
        SEAT.to_string(),
        Arc::new(ResolvedModel::new(
            SEAT,
            SEAT,
            Arc::new(compat_provider(SEAT, mantle_origin, Some(mantle))),
            UPSTREAM,
        )),
    )]);
    if let Some(origin) = backup_origin {
        models.insert(
            BACKUP.to_string(),
            Arc::new(ResolvedModel::new(
                BACKUP,
                BACKUP,
                Arc::new(compat_provider(BACKUP, origin, None)),
                UPSTREAM,
            )),
        );
    }
    let mut router = Router::new(config);
    router.install_resolved_models(models);
    router
}

async fn mock(answer: ResponseTemplate) -> MockServer {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer)
        .mount(&origin)
        .await;
    origin
}

async fn hits(origin: &MockServer) -> usize {
    origin.received_requests().await.expect("recording").len()
}

async fn chat(daemon: &Daemon) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", daemon.base_url))
        .json(&json!({
            "model": ALIAS, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("the daemon answers");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn get_json(daemon: &Daemon, path: &str) -> Value {
    reqwest::get(format!("{}{path}", daemon.base_url))
        .await
        .unwrap_or_else(|e| panic!("GET {path}: {e}"))
        .json()
        .await
        .unwrap_or_else(|e| panic!("{path} is JSON: {e}"))
}

/// Each persisted row's `(outcome, extra.upstream_reason)`, waiting until
/// `want` rows have landed: the writer persists off the request path.
async fn ledger_rows(daemon: &Daemon, want: usize) -> Vec<(String, Option<String>)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = read_rows(&daemon.usage_db);
        if rows.len() >= want {
            return rows;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of {want} ledger rows landed: {rows:?}",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn read_rows(db_path: &std::path::Path) -> Vec<(String, Option<String>)> {
    let Ok(db) = routectl_usage::open_readonly(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = db
        .conn()
        .prepare("SELECT outcome, extra FROM requests ORDER BY ts_start, rowid")
    else {
        return Vec::new();
    };
    stmt.query_map([], |row| {
        let outcome: String = row.get(0)?;
        let extra: Option<String> = row.get(1)?;
        Ok((outcome, extra))
    })
    .expect("query rows")
    .map(|row| {
        let (outcome, extra) = row.expect("row");
        let reason = extra
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|extra| extra["upstream_reason"].as_str().map(str::to_string));
        (outcome, reason)
    })
    .collect()
}

fn health_row(health: &Value, state_key: &str) -> Value {
    health["data"]["targets"]
        .as_array()
        .expect("health targets")
        .iter()
        .find(|t| t["state_key"] == state_key)
        .unwrap_or_else(|| panic!("no health row {state_key}: {health}"))
        .clone()
}

fn park_warns(doctor: &Value) -> Vec<Value> {
    doctor["data"]["report"]["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("the served doctor carries findings: {doctor}"))
        .iter()
        .filter(|f| {
            f["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("parked seat "))
        })
        .cloned()
        .collect()
}

/// Boot a daemon over `text` written to a fresh config file, serving `router`.
async fn spawn(dir: &tempfile::TempDir, text: &str, router: Router) -> Daemon {
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, text).expect("write config");
    let config = parse_config(text).expect("valid mantle test config");
    Daemon::spawn_at(config, router, Some(config_path)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn an_expired_mantle_key_is_stamped_parked_reported_and_then_skipped() {
    let dir = tempfile::tempdir().expect("config tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let origin = mock(expired_401()).await;
    let text = config_text(None);
    let config = Arc::new(parse_config(&text).expect("valid mantle test config"));
    let daemon = spawn(&dir, &text, router(config, &origin.uri(), None)).await;

    let healthy = get_json(&daemon, "/status/health").await;
    let (first_status, first_body) = chat(&daemon).await;
    let dialed = hits(&origin).await;
    let health = get_json(&daemon, "/status/health").await;
    let doctor = get_json(&daemon, "/status/doctor").await;
    let (second_status, second_body) = chat(&daemon).await;
    let rows = ledger_rows(&daemon, 2).await;
    let redialed = hits(&origin).await;
    daemon.shutdown().await;

    assert!(
        health_row(&healthy, SEAT)["park_reason"].is_null(),
        "positive control: the seat starts unparked: {healthy}"
    );
    assert_eq!(first_status, 401, "{first_body}");
    assert!(dialed >= 1, "the first request reached the origin");
    assert_eq!(
        rows[0],
        ("upstream_error".to_string(), Some(EXPIRED.into()))
    );

    let parked = health_row(&health, SEAT);
    assert_eq!(parked["park_reason"], EXPIRED, "{parked}");
    let until = parked["parked_until_ms"].as_i64().expect("park deadline");
    assert!(until > chrono::Utc::now().timestamp_millis(), "{parked}");

    let warns = park_warns(&doctor);
    assert_eq!(warns.len(), 1, "{doctor}");
    assert_eq!(warns[0]["status"], "Warn");
    assert_eq!(warns[0]["name"], format!("parked seat {SEAT}"));
    assert!(
        warns[0]["detail"].as_str().unwrap().contains(EXPIRED),
        "{}",
        warns[0]
    );

    assert_eq!(redialed, dialed, "the parked seat is never dialed again");
    assert_eq!(second_status, 503, "{second_body}");
    assert_eq!(rows[1], ("gate_blocked".to_string(), Some(EXPIRED.into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_first_401_a_fallback_recovers_from_writes_no_reason() {
    let dir = tempfile::tempdir().expect("config tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let mantle_origin = mock(expired_401()).await;
    let backup_origin = mock(ok_200()).await;
    let text = config_text(Some(&backup_origin.uri()));
    let config = Arc::new(parse_config(&text).expect("valid mantle test config"));
    let router = router(config, &mantle_origin.uri(), Some(&backup_origin.uri()));
    let daemon = spawn(&dir, &text, router).await;

    let (status, body) = chat(&daemon).await;
    let rows = ledger_rows(&daemon, 1).await;
    let health = get_json(&daemon, "/status/health").await;
    let mantle_hits = hits(&mantle_origin).await;
    daemon.shutdown().await;

    assert_eq!(status, 200, "{body}");
    assert!(mantle_hits >= 1, "the Mantle seat answered 401 first");
    assert_eq!(
        health_row(&health, SEAT)["park_reason"],
        EXPIRED,
        "positive control: the recovered 401 really carried the reason"
    );
    assert_eq!(rows, [("ok".to_string(), None)]);
}
