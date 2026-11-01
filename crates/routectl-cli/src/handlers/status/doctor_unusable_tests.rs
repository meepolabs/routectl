//! An unusable credential is daemon memory: the served `/status/doctor`
//! report names each unusable seat once with its cause, since and fix, and
//! the offline gather, which cannot see the daemon, names none.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures::stream::BoxStream;
use routectl_auth::SecretRef;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{Config, ResolvedModel, Router};
use serde_json::Value;
use tower::ServiceExt;

use super::super::{StatusState, status_router};
use crate::commands::doctor::{GatherSources, build_report_no_network, gather_context_no_network};
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;

/// `(state key, provider entry, credential ref)` for each seat.
const LABELED: (&str, &str, &str) = ("seat-a", "alpha-a", "oauth://anthropic#seat-a");
const UNLABELED: (&str, &str, &str) = ("seat-b", "alpha", "oauth://anthropic");
const ENV_SEAT: (&str, &str, &str) = ("seat-env", "beta", "env://BETA_KEY_FOR_TESTS");
const FREE: (&str, &str, &str) = ("seat-free", "alpha-free", "oauth://anthropic#seat-free");
const SEATS: [(&str, &str, &str); 4] = [LABELED, UNLABELED, ENV_SEAT, FREE];
const MARKED: [((&str, &str, &str), UnusableCause); 3] = [
    (LABELED, UnusableCause::Revoked),
    (UNLABELED, UnusableCause::NotLoggedIn),
    (ENV_SEAT, UnusableCause::Unreadable),
];

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

fn config_text(db_path: &std::path::Path) -> String {
    let mut text = format!(
        "version = {}\n[usage]\ndb_path = \"{}\"\n",
        routectl_router::CURRENT_CONFIG_VERSION,
        db_path.display(),
    );
    for (_, entry, uri) in SEATS {
        let kind = if uri.starts_with("oauth://") {
            "kind = \"anthropic-api\"\n"
        } else {
            "kind = \"openai-compat\"\nbase_url = \"https://beta.example.com\"\n"
        };
        text.push_str(&format!(
            "[providers.{entry}]\n{kind}api_key_ref = \"{uri}\"\n"
        ));
    }
    text
}

/// A router over one non-pooled model per seat, each on its own credential
/// account; the seats in [`MARKED`] are marked unusable when `mark` is set.
fn router(config_text: &str, mark: bool) -> Router {
    let config: Config = toml::from_str(config_text).expect("fixture config parses");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = SEATS
        .iter()
        .map(|(nickname, entry, uri)| {
            let model = ResolvedModel::new(*nickname, *entry, Arc::clone(&provider), "u")
                .with_auth_secret_ref(SecretRef::parse(uri).expect("fixture ref parses"));
            ((*nickname).to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    if mark {
        for ((key, _, _), cause) in MARKED {
            assert!(
                router.mark_credential_unusable_for_tests(key, cause),
                "{key}"
            );
        }
    }
    router
}

fn unusable_findings(findings: &[Value]) -> Vec<Value> {
    findings
        .iter()
        .filter(|f| {
            f["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("unusable seat "))
        })
        .cloned()
        .collect()
}

fn finding_for<'a>(findings: &'a [Value], key: &str) -> &'a Value {
    findings
        .iter()
        .find(|f| f["name"] == format!("unusable seat {key}"))
        .unwrap_or_else(|| panic!("no unusable finding for {key}: {findings:?}"))
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

/// The served report carries one `auth` Warn per unusable seat naming its
/// reason, cause and since, with the fix as its remediation; none for the
/// unmarked seat. The offline gather over the same config names none.
///
/// Mutation check: drop `.with_unusable_seats(..)` from
/// `served_doctor_inputs`, or fabricate a row in `disk_layers` -> red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_served_doctor_names_each_unusable_seat_and_the_offline_doctor_names_none() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    let text = config_text(&dir.path().join("usage.db"));
    std::fs::write(&config_path, &text).unwrap();
    let before_ms = chrono::Utc::now().timestamp_millis();

    // Act
    let served = served_findings(router(&text, true), config_path.clone()).await;
    let after_ms = chrono::Utc::now().timestamp_millis();
    let offline = build_report_no_network(
        &gather_context_no_network(&config_path, GatherSources::Disk, &[]).await,
    );
    let offline: Vec<Value> = offline
        .findings
        .iter()
        .map(|f| serde_json::to_value(f).unwrap())
        .collect();

    // Assert
    let unusable = unusable_findings(&served);
    assert_eq!(unusable.len(), MARKED.len(), "{served:?}");
    for ((key, _, _), cause) in MARKED {
        let finding = finding_for(&unusable, key);
        assert_eq!(finding["section"], "auth");
        assert_eq!(finding["status"], "Warn");
        let detail = finding["detail"].as_str().unwrap();
        assert!(
            detail.starts_with(&format!("credential_unusable ({}) since ", cause.as_str())),
            "{detail}"
        );
        let since = detail
            .split("since ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_else(|| panic!("no since in {detail}"));
        let at = chrono::DateTime::parse_from_rfc3339(since)
            .unwrap_or_else(|e| panic!("since {since} is not RFC3339: {e}"));
        let at_ms = at.timestamp_millis();
        assert!(
            (before_ms - 1_000..=after_ms).contains(&at_ms),
            "the mark was set during this test: {at_ms} not in {before_ms}..={after_ms}"
        );
    }
    assert!(
        !unusable.iter().any(|f| f.to_string().contains(FREE.0)),
        "the unmarked seat has no unusable finding: {unusable:?}"
    );
    assert!(unusable_findings(&offline).is_empty(), "{offline:?}");
}

/// The fix line is the re-login command for the seat's own credential: with
/// `--label` for a labeled OAuth seat, without it for an unlabeled one, and the
/// entry and scheme, never the value, for an env seat.
///
/// Mutation check: make the labeled arm of `oauth_login_remediation` drop the
/// label -> the seat-a row goes red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn each_unusable_seat_names_the_command_that_readmits_it() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    let text = config_text(&dir.path().join("usage.db"));
    std::fs::write(&config_path, &text).unwrap();

    // Act
    let unusable = unusable_findings(&served_findings(router(&text, true), config_path).await);

    // Assert
    let fix = |key: &str| {
        finding_for(&unusable, key)["remediation"]
            .as_str()
            .unwrap_or_else(|| panic!("{key} carries a fix"))
            .to_string()
    };
    let rows: [(&str, &str); 3] = [
        (LABELED.0, "run `routectl login anthropic --label seat-a`"),
        (UNLABELED.0, "run `routectl login anthropic`"),
        (
            ENV_SEAT.0,
            "provide a readable env:// credential for provider entry `beta`, then reload the \
             config",
        ),
    ];
    for (key, expected) in rows {
        assert_eq!(fix(key), expected, "{key}");
    }
    assert!(
        !fix(ENV_SEAT.0).contains("BETA_KEY_FOR_TESTS"),
        "the env fix never names the variable"
    );
}
