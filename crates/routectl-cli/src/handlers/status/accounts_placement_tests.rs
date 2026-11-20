//! `/status/accounts` `placement` and `corrections_confirmed` over a real
//! build: the wire keys the dashboard joins on, a ranked and an unavailable
//! seat, and the counters' starting values.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Router, UsageVendor};
use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::tests::read_accounts;
use crate::handlers::status::{DaemonMeta, StatusState};
use crate::server::AppState;

const UPSTREAM: &str = "claude-sonnet-4-6";

fn credentials(path: &std::path::Path, seats: &[&str]) {
    let expires = routectl_auth::oauth::unix_now() + 3600;
    let record = json!({
        "access_token": "tok",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": 0
    });
    let providers: serde_json::Map<String, Value> = seats
        .iter()
        .map(|seat| ((*seat).to_string(), record.clone()))
        .collect();
    let doc = json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// A sticky-least-loaded pool of three Anthropic logins behind one model.
fn config_text(usage_db: &std::path::Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for member in ["seat-a", "seat-b", "seat-c"] {
        text.push_str(&format!(
            "[providers.{member}]\nformat = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\n\
             credential = \"oauth://anthropic#{member}\"\nauth = \"bearer\"\n\n"
        ));
    }
    text.push_str(&format!(
        "[pools.the-pool]\nmembers = [\"seat-a\", \"seat-b\", \"seat-c\"]\n\
         seat_selection = \"sticky-least-loaded\"\n\n\
         [models.pooled]\nprovider = \"the-pool\"\nupstream = \"{UPSTREAM}\"\n"
    ));
    text
}

fn poll(router: &Router, account: &str, percent: u32) {
    let reset = OffsetDateTime::from(SystemTime::now() + Duration::from_mins(150))
        .format(&Rfc3339)
        .expect("a formattable instant");
    let body = format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{reset}"}}, "seven_day": null}}"#
    );
    let generation = router.acquire_credential_generation(account);
    router
        .observe_polled_usage(account, UsageVendor::Anthropic, body.as_bytes(), generation)
        .expect("the body is accepted");
}

/// Over a real build with seat-b never logged in (a placeholder), the
/// panel ranks the two read seats, gives the placeholder no rank and its
/// eligibility token, and carries the rule, its zero counters and the
/// meter's correction count at the top level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_panel_carries_the_placement_ranking_and_the_correction_count() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let creds = dir.path().join("routectl").join("credentials.json");
    credentials(&creds, &["anthropic#seat-a", "anthropic#seat-c"]);
    let secrets: Arc<dyn SecretStore> =
        Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
    let config = routectl_router::parse_config(&config_text(&dir.path().join("usage.db")))
        .expect("fixture parses");
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("a pool with a logged-out member still builds");
    poll(&router, "anthropic#seat-a", 60);
    poll(&router, "anthropic#seat-c", 10);
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()));

    // Act
    let json = read_accounts(&state).await;

    // Assert
    let data = &json["data"];
    assert_eq!(data["corrections_confirmed"], 0);
    let placement = &data["placement"];
    let keys: Vec<&str> = placement
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "births",
            "cap_forced_moves",
            "moves",
            "pools",
            "rule",
            "since_start"
        ]
    );
    assert_eq!(placement["rule"], "pace_v1");
    assert_eq!(placement["since_start"], true);
    assert_eq!(
        placement["births"],
        json!({"roomy": 0, "low_room": 0, "unread": 0})
    );
    assert_eq!(
        placement["moves"],
        json!({"credential_unusable": 0, "account_parked": 0, "breaker_open": 0})
    );
    assert_eq!(placement["cap_forced_moves"], 0);
    let pools = placement["pools"].as_array().expect("pools");
    assert_eq!(pools.len(), 1, "{placement}");
    assert_eq!(pools[0]["pool"], "the-pool");
    assert_eq!(pools[0]["model"], UPSTREAM);
    let seats = pools[0]["seats"].as_array().expect("seats");
    let summary: Vec<(Value, Value, Value, Value)> = seats
        .iter()
        .map(|seat| {
            (
                seat["account"].clone(),
                seat["rank"].clone(),
                seat["tier"].clone(),
                seat["reason"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                json!("anthropic#seat-a"),
                json!(2),
                json!("roomy"),
                json!("pace")
            ),
            (
                json!("anthropic#seat-b"),
                Value::Null,
                json!("unread"),
                json!("credential_unusable")
            ),
            (
                json!("anthropic#seat-c"),
                json!(1),
                json!("roomy"),
                json!("pace")
            ),
        ]
    );
    assert!(seats[0]["score"].is_f64(), "{}", seats[0]);
    assert!(seats[1]["score"].is_null(), "{}", seats[1]);
    for seat in seats {
        let keys: Vec<&str> = seat
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["account", "rank", "reason", "score", "tier"]);
    }
}
