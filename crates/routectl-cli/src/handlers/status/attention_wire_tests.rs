//! `/status/attention` on the wire: the aggregate member serializes what the
//! standalone route does, the aggregate runs one doctor gather, the closed
//! token sets are welded to their enums, and the populated fixtures carry
//! every item state and both doctor coverage states.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, Router};
use routectl_testkit::ScopedEnv;
use serde::Serialize;
use serde_json::Value;
use tower::ServiceExt;

use super::super::test_hooks::StatusTestHooks;
use super::super::{DaemonMeta, StatusState, status_router};
use super::fixture::{populated_attention, populated_attention_doctor_unavailable};
use crate::commands::doctor::attention::{
    AttentionKind, ItemState, Severity, VerdictSeverity, ViewTarget,
};
use crate::server::AppState;

/// Keeps a scoped config home alive for one test.
struct Home {
    _xdg: ScopedEnv,
    _dir: tempfile::TempDir,
}

/// A daemon with an on-disk config and a malformed credential store under a
/// scoped config home, so its doctor report builds without reading the
/// operator's own store and fails one finding. Returns the state, the gather
/// count and the home.
fn doctor_state() -> (Arc<StatusState>, Arc<AtomicUsize>, Home) {
    let dir = tempfile::tempdir().expect("tempdir");
    let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {CURRENT_CONFIG_VERSION}\n"),
    )
    .expect("write config");
    let creds = dir.path().join("routectl").join("credentials.json");
    std::fs::create_dir_all(creds.parent().expect("parent")).expect("mkdir");
    std::fs::write(&creds, "not json").expect("write a malformed credential store");
    let router = Router::new(Arc::new(Config::default()));
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let hooks = StatusTestHooks::default();
    let gathers = Arc::clone(&hooks.doctor_gathers);
    let mut status = StatusState::from_app(&app, Some(config_path), DaemonMeta::for_test())
        .with_test_hooks(hooks);
    status.usage_db_path = dir.path().join("absent-usage.db");
    (
        Arc::new(status),
        gathers,
        Home {
            _xdg: xdg,
            _dir: dir,
        },
    )
}

async fn get(state: &Arc<StatusState>, uri: &str) -> Value {
    let resp = status_router()
        .with_state(Arc::clone(state))
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).expect("a JSON body")
}

/// The envelope without its read time, which differs between two reads.
fn without_as_of(mut envelope: Value) -> Value {
    assert!(envelope["as_of"].is_string(), "{envelope}");
    envelope
        .as_object_mut()
        .expect("an envelope object")
        .remove("as_of");
    envelope
}

/// One daemon state read through `panels.attention` on `/status` and then
/// through `/status/attention` serializes the same v1 payload, doctor
/// failures included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_aggregate_member_serializes_the_standalone_payload() {
    // Arrange
    let (state, _gathers, _home) = doctor_state();

    // Act
    let aggregate = get(&state, "/status").await;
    let standalone = get(&state, "/status/attention").await;

    // Assert
    let member = without_as_of(aggregate["panels"]["attention"].clone());
    let route = without_as_of(standalone);
    assert_eq!(member["schema_version"], 1);
    assert_eq!(member["data"]["coverage"]["doctor"], "current");
    let has_doctor_item = member["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .any(|item| item["kind"] == "doctor_failure");
    assert!(
        has_doctor_item,
        "control: the shared report reached the list: {member}"
    );
    assert_eq!(member, route);
}

/// A `/status` read runs the no-network doctor gather once, shared by the
/// doctor and attention panels; the standalone attention route runs its own
/// (control: the counter sees a gather the aggregate did not share).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn one_aggregate_read_runs_one_doctor_gather() {
    // Arrange
    let (state, gathers, _home) = doctor_state();

    // Act
    let aggregate = get(&state, "/status").await;
    let after_aggregate = gathers.load(Ordering::SeqCst);
    let _ = get(&state, "/status/attention").await;
    let after_standalone = gathers.load(Ordering::SeqCst);

    // Assert
    assert_eq!(aggregate["panels"]["doctor"]["unavailable"], Value::Null);
    assert_eq!(
        aggregate["panels"]["attention"]["data"]["coverage"]["doctor"],
        "current"
    );
    assert_eq!(after_aggregate, 1, "one gather per aggregate read");
    assert_eq!(after_standalone, 2, "control: the standalone route gathers");
}

/// Every variant of an enum, guarded by an exhaustive match so an added
/// variant fails to compile here until its token is pinned below.
fn severities() -> [Severity; 2] {
    let all = [Severity::Degraded, Severity::Broken];
    for value in all {
        match value {
            Severity::Degraded | Severity::Broken => {}
        }
    }
    all
}

fn verdict_severities() -> [VerdictSeverity; 3] {
    let all = [
        VerdictSeverity::Ok,
        VerdictSeverity::Degraded,
        VerdictSeverity::Broken,
    ];
    for value in all {
        match value {
            VerdictSeverity::Ok | VerdictSeverity::Degraded | VerdictSeverity::Broken => {}
        }
    }
    all
}

fn item_states() -> [ItemState; 2] {
    let all = [ItemState::Active, ItemState::Recovered];
    for value in all {
        match value {
            ItemState::Active | ItemState::Recovered => {}
        }
    }
    all
}

fn view_targets() -> [ViewTarget; 4] {
    let all = [
        ViewTarget::Accounts,
        ViewTarget::Models,
        ViewTarget::Traffic,
        ViewTarget::Doctor,
    ];
    for value in all {
        match value {
            ViewTarget::Accounts
            | ViewTarget::Models
            | ViewTarget::Traffic
            | ViewTarget::Doctor => {}
        }
    }
    all
}

/// The tokens `values` serialize to, each checked against its `as_str`.
fn tokens<T: Serialize + Copy>(values: &[T], as_str: fn(T) -> &'static str) -> Vec<String> {
    values
        .iter()
        .map(|value| {
            let wire = serde_json::to_value(value).expect("a token serializes");
            assert_eq!(
                wire,
                Value::from(as_str(*value)),
                "wire token equals as_str"
            );
            as_str(*value).to_string()
        })
        .collect()
}

fn pinned(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_string()).collect()
}

/// The closed kind, severity, state and target sets are pinned: each token is
/// its enum's `as_str` and the serialized form, and a removed or renamed
/// token changes a pinned list, which is a schema bump. An added kind is
/// additive; add it here without one.
#[test]
fn the_closed_token_sets_are_welded_to_their_enums() {
    assert_eq!(
        tokens(&AttentionKind::ALL, AttentionKind::as_str),
        pinned(&[
            "pool_empty",
            "route_circuit_open",
            "account_unusable",
            "account_parked",
            "account_refresh_failing",
            "driver_stopped",
            "account_near_limit",
            "usage_poll_failing",
            "records_dropped",
            "doctor_failure",
        ])
    );
    assert_eq!(
        tokens(&severities(), Severity::as_str),
        pinned(&["degraded", "broken"])
    );
    assert_eq!(
        tokens(&verdict_severities(), VerdictSeverity::as_str),
        pinned(&["ok", "degraded", "broken"])
    );
    assert_eq!(
        tokens(&item_states(), ItemState::as_str),
        pinned(&["active", "recovered"])
    );
    assert_eq!(
        tokens(&view_targets(), ViewTarget::as_str),
        pinned(&["accounts", "models", "traffic", "doctor"])
    );
}

fn token_set(values: Vec<String>) -> BTreeSet<String> {
    values.into_iter().collect()
}

/// Every token the populated payloads carry is in its closed set.
#[test]
fn the_populated_payloads_carry_only_closed_tokens() {
    let kinds = token_set(tokens(&AttentionKind::ALL, AttentionKind::as_str));
    let severity = token_set(tokens(&severities(), Severity::as_str));
    let verdict = token_set(tokens(&verdict_severities(), VerdictSeverity::as_str));
    let states = token_set(tokens(&item_states(), ItemState::as_str));
    let targets = token_set(tokens(&view_targets(), ViewTarget::as_str));
    let coverage = token_set(pinned(&["current", "unavailable"]));
    let text = |value: &Value| value.as_str().unwrap_or("<not a string>").to_string();

    for payload in [
        populated_attention(),
        populated_attention_doctor_unavailable(),
    ] {
        let data = &payload["data"];
        assert!(
            verdict.contains(&text(&data["verdict"]["severity"])),
            "{data}"
        );
        assert!(
            targets.contains(&text(&data["verdict"]["target"])),
            "{data}"
        );
        assert!(
            coverage.contains(&text(&data["coverage"]["doctor"])),
            "{data}"
        );
        for item in data["items"].as_array().expect("items") {
            assert!(kinds.contains(&text(&item["kind"])), "{item}");
            assert!(severity.contains(&text(&item["severity"])), "{item}");
            assert!(states.contains(&text(&item["state"])), "{item}");
            assert!(targets.contains(&text(&item["target"])), "{item}");
        }
    }
}

fn items_where(payload: &Value, state: &str, severity: &str) -> Vec<String> {
    payload["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["state"] == state && item["severity"] == severity)
        .map(|item| item["key"].as_str().expect("key").to_string())
        .collect()
}

/// The populated fixture has an active broken item, several active degraded
/// ones, a recovered one and current doctor coverage; its doctor-unavailable
/// twin says so and still holds the earlier doctor failure as active.
#[test]
fn the_fixtures_carry_every_item_state_and_both_doctor_coverages() {
    let current = populated_attention();
    let unavailable = populated_attention_doctor_unavailable();

    assert_eq!(
        items_where(&current, "active", "broken"),
        ["pool_empty:seats"]
    );
    assert!(
        items_where(&current, "active", "degraded").len() > 1,
        "{current}"
    );
    assert_eq!(
        items_where(&current, "recovered", "broken"),
        ["pool_empty:backup"]
    );
    assert_eq!(current["data"]["coverage"]["doctor"], "current");
    assert_eq!(current["data"]["verdict"]["severity"], "broken");
    assert_eq!(unavailable["data"]["coverage"]["doctor"], "unavailable");
    assert!(
        items_where(&unavailable, "active", "degraded")
            .contains(&"doctor_failure:config/schema".to_string()),
        "{unavailable}"
    );
}
