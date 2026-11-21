//! The config panel's `pools` rows: every configured pool and member, named
//! by the same identity rule as every other seat-naming payload, plus the
//! cache-write price rows of the populated fixture in duration order.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::config::SeatSelection;
use routectl_router::{
    AccountIdentities, ActivationState, CURRENT_CONFIG_VERSION, Config, Router,
    derive_effective_view,
};
use routectl_testkit::ScopedEnv;
use serde_json::Value;
use tower::ServiceExt;

use super::super::daemon_meta::DaemonMetaSnapshot;
use super::fixture::{
    FIXTURE_POOL, FIXTURE_POOL_MEMBERS, populated_config, populated_config_source,
};
use super::{SCHEMA_VERSION, build_panel};
use crate::handlers::status::{DaemonMeta, StatusState, status_router};
use crate::server::AppState;

fn pools(panel: &Value) -> &Vec<Value> {
    panel["data"]["pools"].as_array().expect("a pools array")
}

fn member_entries(pool: &Value) -> Vec<&str> {
    pool["members"]
        .as_array()
        .expect("a members array")
        .iter()
        .map(|m| m["entry"].as_str().expect("an entry"))
        .collect()
}

#[test]
fn the_populated_panel_lists_the_fixture_pool_with_members_in_configured_order() {
    // Arrange
    let panel = populated_config();

    // Act
    let listed: Vec<(&str, Vec<&str>)> = pools(&panel)
        .iter()
        .map(|p| (p["pool"].as_str().expect("a pool key"), member_entries(p)))
        .collect();

    // Assert
    assert_eq!(listed, [(FIXTURE_POOL, FIXTURE_POOL_MEMBERS.to_vec())]);
    let mut sorted = FIXTURE_POOL_MEMBERS;
    sorted.sort_unstable();
    assert_ne!(
        sorted, FIXTURE_POOL_MEMBERS,
        "premise: configured order differs from key order"
    );
    assert_eq!(panel["schema_version"], 6, "pools is additive: no bump");
    assert_eq!(SCHEMA_VERSION, 6);
}

fn one_pool_config(selection: &str) -> Config {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat-a]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n\
         credential = \"oauth://anthropic#a\"\n\n\
         [pools.p]\nmembers = [\"seat-a\"]\n{selection}\n\
         [models.m]\nprovider = \"p\"\nupstream = \"claude-sonnet-4-5\"\n"
    );
    toml::from_str(&text).expect("the pool config parses")
}

fn panel_json(config: &Config) -> Value {
    let daemon = DaemonMetaSnapshot {
        listen_addr: "127.0.0.1:9000".to_string(),
        version: env!("CARGO_PKG_VERSION"),
        config_loaded_age_ms: None,
    };
    let effective = derive_effective_view(config, None, None);
    let panel = build_panel(effective, &ActivationState::default(), None, daemon);
    serde_json::to_value(panel).expect("the panel serializes")
}

#[test]
fn seat_selection_is_the_config_spelling_of_each_strategy() {
    let rows = [
        (
            "fill-first",
            "seat_selection = \"fill-first\"\n",
            SeatSelection::FillFirst,
        ),
        (
            "sticky-least-loaded",
            "seat_selection = \"sticky-least-loaded\"\n",
            SeatSelection::StickyLeastLoaded,
        ),
        ("omitted", "", SeatSelection::StickyLeastLoaded),
    ];
    for (name, line, selection) in rows {
        // Arrange
        let config = one_pool_config(line);

        // Act
        let json = panel_json(&config);

        // Assert
        let expected = serde_json::to_value(selection).expect("the strategy serializes");
        assert_eq!(json["pools"][0]["seat_selection"], expected, "{name}");
    }
}

#[test]
fn each_member_carries_the_account_and_login_label_account_identities_names() {
    // Arrange
    let config = populated_config_source();
    let identities = AccountIdentities::from_config(&config);
    let panel = populated_config();

    // Act
    let members = pools(&panel)[0]["members"]
        .as_array()
        .expect("members")
        .clone();

    // Assert
    assert_eq!(members.len(), FIXTURE_POOL_MEMBERS.len());
    for member in &members {
        let entry = member["entry"].as_str().expect("an entry");
        let account = AccountIdentities::account_of_entry(&config, entry);
        let label = account
            .as_deref()
            .and_then(|account| identities.get(account))
            .and_then(|identity| identity.login_label.clone());
        assert_eq!(member["account"], serde_json::json!(account), "{entry}");
        assert_eq!(member["login_label"], serde_json::json!(label), "{entry}");
    }
    let labels: Vec<&Value> = members.iter().map(|m| &m["login_label"]).collect();
    assert!(
        labels.iter().any(|l| l.is_string()) && labels.iter().any(|l| l.is_null()),
        "positive control: one member labeled, one not: {labels:?}"
    );
}

#[test]
fn the_fixture_model_prices_its_cache_writes_in_duration_order() {
    // Arrange
    let panel = populated_config();

    // Act
    let glm = panel["data"]["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|m| m["nickname"] == "glm")
        .expect("glm on the wire");
    let writes: Vec<(&str, f64)> = glm["prices"]
        .as_array()
        .expect("prices")
        .iter()
        .filter(|p| {
            p["field"]
                .as_str()
                .is_some_and(|f| f.starts_with("cache_write"))
        })
        .map(|p| {
            (
                p["field"].as_str().expect("a field"),
                p["per_mtok"].as_f64().expect("a priced row"),
            )
        })
        .collect();

    // Assert
    assert_eq!(
        writes,
        [
            ("cache_write_5m_per_mtok", 1.25),
            ("cache_write_per_mtok.30m", 1.5),
            ("cache_write_1h_per_mtok", 2.0),
        ]
    );
}

const LOGGED_IN: &str = "anthropic#a";
const UNBUILT_MEMBER: &str = "seat-z";

/// A pool listing a member with no login (`seat-z`, which cannot build)
/// ahead of a logged-in one.
fn degraded_pool_text() -> String {
    let seat = |entry: &str, login: &str| {
        format!(
            "[providers.{entry}]\nformat = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n\
             credential = \"oauth://{login}\"\n\n"
        )
    };
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n{}{}\
         [pools.p]\nmembers = [\"{UNBUILT_MEMBER}\", \"seat-a\"]\n\n\
         [models.m]\nprovider = \"p\"\nupstream = \"claude-sonnet-4-5\"\n",
        seat(UNBUILT_MEMBER, "anthropic#z"),
        seat("seat-a", LOGGED_IN),
    )
}

fn write_one_login(path: &Path, seat: &str) {
    let record = serde_json::json!({
        "access_token": "tok",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": routectl_auth::oauth::unix_now() + 3600,
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

async fn built_router(dir: &Path) -> Router {
    let creds = dir.join("routectl").join("credentials.json");
    write_one_login(&creds, LOGGED_IN);
    let store: Arc<dyn SecretStore> =
        Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
    let config = routectl_router::parse_config(&degraded_pool_text()).expect("parses");
    crate::server::build_router_from_config(Arc::new(config), store)
        .await
        .expect("a pool member with no login does not fail the build")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_served_panel_lists_a_member_whose_provider_did_not_build() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let router = built_router(dir.path()).await;
    let built: Vec<String> = router
        .status_targets(Instant::now())
        .into_iter()
        .filter(|t| t.gate.credential_unusable.is_none())
        .filter_map(|t| t.account)
        .collect();
    assert_eq!(built, [LOGGED_IN], "premise: only seat-a built a provider");
    assert_eq!(
        router.pool_reports()[0].placeholders[0].member,
        UNBUILT_MEMBER
    );
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()));

    // Act
    let resp = status_router()
        .with_state(state)
        .oneshot(
            Request::builder()
                .uri("/status/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Assert
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["data"]["pools"],
        serde_json::json!([{
            "pool": "p",
            "seat_selection": "sticky-least-loaded",
            "members": [
                { "entry": UNBUILT_MEMBER, "account": "anthropic#z", "login_label": "z" },
                { "entry": "seat-a", "account": LOGGED_IN, "login_label": "a" },
            ],
        }]),
        "{json}"
    );
}
