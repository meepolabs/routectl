//! A credential park is daemon state: the served `/status/doctor` report
//! names each parked seat once, and the offline gather, which cannot see the
//! daemon, names none.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, UpstreamReason,
};
use routectl_router::{Config, ResolvedModel, Router};
use serde_json::Value;
use tower::ServiceExt;

use super::super::{StatusState, status_router};
use crate::commands::doctor::{GatherSources, build_report_no_network, gather_context_no_network};
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;

const PARKED: [(&str, UpstreamReason); 2] = [
    ("expired-seat", UpstreamReason::CredentialExpired),
    ("invalid-seat", UpstreamReason::CredentialInvalid),
];
const FREE: &str = "free-seat";

struct Unused;

#[async_trait::async_trait]
impl Provider for Unused {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p", 500, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

/// A router over non-pooled models given as `(nickname, provider entry)`;
/// models naming one entry share one credential account.
fn router_over(models: &[(&str, &str)]) -> Router {
    let mut router = Router::new(Arc::new(Config::default()));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = models
        .iter()
        .map(|(nickname, entry)| {
            (
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    *entry,
                    Arc::clone(&provider),
                    "u",
                )),
            )
        })
        .collect();
    router.install_resolved_models(models);
    router
}

/// Three non-pooled seats, each on its own account; the two in [`PARKED`]
/// are parked for an hour when `park` is set.
fn router(park: bool) -> Router {
    let router = router_over(&[
        (PARKED[0].0, "p-expired"),
        (PARKED[1].0, "p-invalid"),
        (FREE, "p-free"),
    ]);
    if park {
        let until = Instant::now() + Duration::from_hours(1);
        for (key, reason) in PARKED {
            assert!(router.park_credential_for_tests(key, reason, until));
        }
    }
    router
}

fn config_text(db_path: &std::path::Path) -> String {
    format!(
        "version = {}\n[usage]\ndb_path = \"{}\"\n",
        routectl_router::CURRENT_CONFIG_VERSION,
        db_path.display(),
    )
}

fn parked_findings(findings: &[Value]) -> Vec<Value> {
    findings
        .iter()
        .filter(|f| {
            f["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("parked seat "))
        })
        .cloned()
        .collect()
}

/// The served report's findings for a daemon serving `router`.
async fn served_findings(router: Router, config_path: std::path::PathBuf) -> Vec<Value> {
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = Arc::new(StatusState::from_app(
        &app,
        Some(config_path),
        DaemonMeta::for_test(),
    ));
    let resp = status_router()
        .with_state(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status/doctor")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    json["data"]["report"]["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("the served report carries findings: {json}"))
        .clone()
}

/// The served report carries one Warn per parked seat naming its state key,
/// reason and deadline, and none for the unparked seat or an unparked daemon.
/// The offline gather over the same config names no park at all.
///
/// Mutation check: drop `.with_parked_seats(..)` from
/// `served_doctor_inputs`, or the `extend` in `section_auth` -> red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_served_doctor_warns_once_per_parked_seat_and_the_offline_doctor_names_none() {
    let dir = tempfile::tempdir().unwrap();
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_text(&dir.path().join("usage.db"))).unwrap();
    let before_ms = chrono::Utc::now().timestamp_millis();

    let served = served_findings(router(true), config_path.clone()).await;
    let unparked = served_findings(router(false), config_path.clone()).await;
    let offline = build_report_no_network(
        &gather_context_no_network(&config_path, GatherSources::Disk, &[]).await,
    );
    let offline: Vec<Value> = offline
        .findings
        .iter()
        .map(|f| serde_json::to_value(f).unwrap())
        .collect();

    let parked = parked_findings(&served);
    assert_eq!(parked.len(), PARKED.len(), "{served:?}");
    for (key, reason) in PARKED {
        let finding = parked
            .iter()
            .find(|f| f["name"] == format!("parked seat {key}"))
            .unwrap_or_else(|| panic!("no finding for {key}: {parked:?}"));
        assert_eq!(finding["section"], "auth");
        assert_eq!(finding["status"], "Warn");
        let detail = finding["detail"].as_str().unwrap();
        assert!(detail.contains(reason.as_str()), "{detail}");
        let deadline = detail
            .split("until ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_else(|| panic!("no deadline in {detail}"));
        let at = chrono::DateTime::parse_from_rfc3339(deadline)
            .unwrap_or_else(|e| panic!("deadline {deadline} is not RFC3339: {e}"));
        let lead_ms = at.timestamp_millis() - before_ms;
        assert!(
            (3_500_000..=3_700_000).contains(&lead_ms),
            "the deadline is about an hour out: {lead_ms} ms"
        );
    }
    assert!(
        !served.iter().any(|f| f.to_string().contains(FREE)),
        "the unparked seat has no finding: {served:?}"
    );
    assert!(parked_findings(&unparked).is_empty(), "{unparked:?}");
    assert!(parked_findings(&offline).is_empty(), "{offline:?}");
}

/// One parked account under N targets reports N parked rows, one per target,
/// and none for a target on another account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_served_doctor_names_every_target_of_a_parked_account() {
    let dir = tempfile::tempdir().unwrap();
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_text(&dir.path().join("usage.db"))).unwrap();
    let siblings = ["sibling-a", "sibling-b", "sibling-c"];
    let router = router_over(&[
        (siblings[0], "shared"),
        (siblings[1], "shared"),
        (siblings[2], "shared"),
        (FREE, "p-free"),
    ]);
    assert!(router.park_credential_for_tests(
        siblings[0],
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_hours(1),
    ));

    let served = served_findings(router, config_path).await;

    let mut named: Vec<String> = parked_findings(&served)
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    named.sort();
    let expected: Vec<String> = siblings
        .iter()
        .map(|s| format!("parked seat {s}"))
        .collect();
    assert_eq!(named, expected, "{served:?}");
}
