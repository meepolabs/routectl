//! A configured account whose provider failed to build is listed on every
//! status surface that names accounts, under the identity its config ref
//! gives it, and the accounts panel lists exactly the accounts the usage
//! poll reads.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Router};
use routectl_testkit::ScopedEnv;
use serde_json::Value;
use tower::ServiceExt;

use super::tests::codex_block;
use crate::handlers::status::{DaemonMeta, StatusState, status_router};
use crate::server::AppState;
use crate::server::seat_driver::managed_seats;
use crate::server::usage_poll::UsagePollJob;

const CODEX: &str = "codex";
const CODEX_ENTRY: &str = "codex-one";
const SEAT_A: &str = "anthropic#a";

/// A daemon over a real build: `anthropic#a` logged in, and an
/// `oauth://codex` entry with no login, whose provider cannot build without
/// the login's account id. The usage poll runs over the same store.
struct Rig {
    state: Arc<StatusState>,
    router: Arc<Router>,
    _xdg: ScopedEnv,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let creds = dir.path().join("routectl").join("credentials.json");
        write_one_login(&creds, SEAT_A);
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let secrets: Arc<dyn SecretStore> = Arc::new((*store).clone());
        let usage_db = dir.path().join("usage.db");
        let config = routectl_router::parse_config(&config_text(&usage_db)).expect("parses");
        let router = crate::server::build_router_from_config(Arc::new(config), secrets)
            .await
            .expect("a failed provider no route names does not fail the build");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let job = UsagePollJob::new(
            store,
            routectl_usage::handle_over_channel(tx),
            Arc::clone(&swap),
        )
        .expect("job");
        let meta = DaemonMeta::for_test();
        assert!(meta.install_usage_poll_state(job.state()).is_ok());
        let app = AppState::for_test(Arc::clone(&swap));
        let state = Arc::new(StatusState::from_app(&app, None, meta));
        Self {
            state,
            router: swap.load_full(),
            _xdg: xdg,
            _dir: dir,
        }
    }

    /// Premise: the router built no target for the codex entry.
    fn assert_codex_did_not_build(&self) {
        let built: Vec<String> = self
            .router
            .status_targets(Instant::now())
            .into_iter()
            .map(|target| target.provider_name)
            .collect();
        assert!(
            built.iter().any(|entry| entry == "seat-a"),
            "premise: the logged-in seat built: {built:?}"
        );
        assert!(
            !built.iter().any(|entry| entry == CODEX_ENTRY),
            "premise: the codex entry did not build: {built:?}"
        );
    }

    async fn send(&self, method: &str, uri: &str, body: &str) -> Value {
        let resp = status_router()
            .with_state(Arc::clone(&self.state))
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["unavailable"].is_null(), "{uri}: {json}");
        json
    }
}

fn config_text(usage_db: &Path) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n\
         [providers.seat-a]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n\
         credential = \"oauth://{SEAT_A}\"\n\n\
         [providers.{CODEX_ENTRY}]\n{}credential = \"oauth://codex\"\n\n\
         [models.m-a]\nprovider = \"seat-a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.m-codex]\nprovider = \"{CODEX_ENTRY}\"\nupstream = \"gpt-5\"\n",
        usage_db.display(),
        codex_block(),
    )
}

fn write_one_login(path: &Path, seat: &str) {
    let expires = routectl_auth::oauth::unix_now() + 3600;
    let record = serde_json::json!({
        "access_token": "tok",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": 0
    });
    let doc = serde_json::json!({ "schema_version": 1, "providers": { seat: record } });
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// One ledger row per seat, recent enough for every window.
fn seed_seat_rows(path: &Path, seats: &[&str]) {
    let db = routectl_usage::open(path).expect("open ledger");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    for (i, seat) in seats.iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens, seat) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 0, 'ok', \
                 5, 0, 0, 1, 0, 10, 20, ?3)",
                rusqlite::params![now + i64::try_from(i).unwrap_or(0), format!("r{i}"), seat],
            )
            .expect("seed row");
    }
}

fn by_key<'a>(rows: &'a Value, key: &str, value: &str) -> &'a Value {
    rows.as_array()
        .expect("an array")
        .iter()
        .find(|row| row[key] == value)
        .unwrap_or_else(|| panic!("no {key} = {value}: {rows}"))
}

fn accounts_of(json: &Value) -> Vec<String> {
    json["data"]["accounts"]
        .as_array()
        .expect("accounts array")
        .iter()
        .map(|row| row["account"].as_str().expect("a string").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_configured_login_whose_provider_did_not_build_is_listed_with_its_poll_state() {
    // Arrange
    let rig = Rig::new().await;
    rig.assert_codex_did_not_build();

    // Act
    let json = rig.send("GET", "/status/accounts", "").await;

    // Assert
    let codex = by_key(&json["data"]["accounts"], "account", CODEX);
    assert_eq!(codex["provider_entry"], CODEX_ENTRY, "{codex}");
    assert!(codex["login_label"].is_null(), "{codex}");
    assert_eq!(codex["report"], "no_reading", "{codex}");
    assert_eq!(codex["poll"]["applicability"], "polled", "{codex}");
    assert_eq!(codex["poll"]["vendor"], "codex", "{codex}");
    let seat_a = by_key(&json["data"]["accounts"], "account", SEAT_A);
    assert_eq!(seat_a["provider_entry"], "seat-a", "{seat_a}");
    assert_eq!(seat_a["login_label"], "a", "{seat_a}");
}

/// An account no built target names reads `not_built` with every other
/// credential field null: the router holds nothing about it, so it is never
/// reported usable. The built seat beside it reads usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_login_whose_provider_did_not_build_reads_credential_not_built() {
    // Arrange
    let rig = Rig::new().await;
    rig.assert_codex_did_not_build();

    // Act
    let json = rig.send("GET", "/status/accounts", "").await;

    // Assert
    let codex = by_key(&json["data"]["accounts"], "account", CODEX);
    assert_eq!(
        codex["credential"],
        serde_json::json!({
            "state": "not_built", "cause": null, "since_ms": null, "until_ms": null, "fix": null,
        }),
        "{codex}"
    );
    let seat_a = by_key(&json["data"]["accounts"], "account", SEAT_A);
    assert_eq!(seat_a["credential"]["state"], "usable", "{seat_a}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_accounts_panel_lists_exactly_the_accounts_the_usage_poll_reads() {
    // Arrange
    let rig = Rig::new().await;
    rig.assert_codex_did_not_build();
    let polled: Vec<String> = managed_seats(&rig.router.config)
        .into_iter()
        .map(|seat| seat.account)
        .collect();

    // Act
    let json = rig.send("GET", "/status/accounts", "").await;

    // Assert
    assert_eq!(accounts_of(&json), polled, "{json}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn query_seat_groups_name_a_login_whose_provider_did_not_build() {
    // Arrange
    let rig = Rig::new().await;
    rig.assert_codex_did_not_build();
    seed_seat_rows(&rig.state.usage_db_path, &[SEAT_A, CODEX]);

    // Act
    let query = rig
        .send(
            "QUERY",
            "/status/query",
            r#"{"window":"all","group_by":"seat"}"#,
        )
        .await;

    // Assert
    let group = by_key(&query["data"]["groups"], "label", CODEX);
    assert_eq!(group["account"], CODEX, "{group}");
    assert_eq!(group["provider_entry"], CODEX_ENTRY, "{group}");
    assert!(group["login_label"].is_null(), "{group}");
    let seat_a = by_key(&query["data"]["groups"], "label", SEAT_A);
    assert_eq!(seat_a["provider_entry"], "seat-a", "{seat_a}");
}
