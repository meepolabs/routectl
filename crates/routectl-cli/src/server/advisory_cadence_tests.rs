//! Advisory config warnings log once per serve startup and once per accepted
//! config reload, and never from a router rebuild the config did not cause.
//! Each transition is driven through its production entry point (the serve
//! body, the config reload handler, the credentials reload handler) and the
//! WARN events it emitted are counted.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{ActivationState, CURRENT_CONFIG_VERSION, Config, parse_config};
use routectl_testkit::{CapturedEvent, with_capture};
use serde_json::json;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::CompositeStore;
use super::super::reload::{handle_config_reload, handle_credentials_reload};
use super::super::serve::{DaemonTestSeams, build_usage_writer};
use super::super::test_support::{drain_usage_writer_strict, isolate_usage_db, never_shutdown};
use super::build_router_from_config;

const RETRY_DEPRECATION_CATEGORY: &str = "class policy warning";
const AUTH_RESERVED_CATEGORY: &str = "auth-reserved header warning";
const OVERLAY_DROP_MESSAGE: &str = "ignoring auth-reserved header from [models.X] header_extras";

fn retry_on_config_text() -> String {
    format!("version = {CURRENT_CONFIG_VERSION}\n[retry]\nretry_on_429 = 2\n")
}

/// WARN events for the deprecated `retry_on_429` key.
fn retry_deprecation_warns(events: &[CapturedEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message == RETRY_DEPRECATION_CATEGORY
                && e.field("warning")
                    .is_some_and(|w| w.contains("retry_on_429"))
        })
        .count()
}

fn count(events: &[CapturedEvent], level: tracing::Level, message: &str) -> usize {
    events
        .iter()
        .filter(|e| e.level == level && e.message == message)
        .count()
}

/// Run the serve body over `config` with the real router build, driving
/// `client` against its address before triggering the graceful shutdown, and
/// return every event the whole run emitted.
async fn serve_capturing<F, Fut>(config: Config, client: F) -> Vec<CapturedEvent>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let (trigger, shutdown) = tokio::sync::oneshot::channel();
    let serve = Box::pin(super::super::serve::serve_on_listener_with_injected_router(
        Arc::new(config),
        listener,
        None,
        Some(secrets),
        DaemonTestSeams {
            shutdown: Some(shutdown),
            ..DaemonTestSeams::default()
        },
    ));
    let drive = async move {
        client(base_url).await;
        trigger
            .send(())
            .expect("the daemon holds the shutdown receiver");
    };
    let ((outcome, ()), events) = with_capture(async { tokio::join!(serve, drive) }).await;
    outcome.expect("the daemon serves and shuts down cleanly");
    events
}

#[tokio::test]
async fn serve_startup_logs_an_advisory_warning_once() {
    // Arrange
    let mut config = parse_config(&retry_on_config_text()).expect("config parses");
    let _usage_dir = isolate_usage_db(&mut config);

    // Act
    let events = Box::pin(serve_capturing(config, |_| async {})).await;

    // Assert
    assert_eq!(retry_deprecation_warns(&events), 1, "{events:#?}");
}

#[tokio::test]
async fn an_accepted_config_reload_logs_an_advisory_warning_once() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let usage_db = dir.path().join("usage.db");
    let cfg_path = dir.path().join("config.toml");
    let text = format!(
        "{}[usage]\ndb_path = \"{}\"\n",
        retry_on_config_text(),
        usage_db.display()
    );
    std::fs::write(&cfg_path, &text).expect("write config");
    let config = Arc::new(parse_config(&text).expect("config parses"));
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let (usage, writer) = build_usage_writer(&config);

    // Act
    let (reloaded, events) = with_capture(Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        &mut never_shutdown(),
    )))
    .await;

    // Assert
    reloaded
        .expect("the reload applies")
        .expect("the reload returns its config");
    assert_eq!(retry_deprecation_warns(&events), 1, "{events:#?}");
    drop(usage);
    drain_usage_writer_strict(writer).await;
}

/// A credentials file holding one stored record per key in `seats`.
fn write_credentials(path: &std::path::Path, seats: &[&str]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    let providers: serde_json::Map<String, serde_json::Value> = seats
        .iter()
        .map(|seat| {
            let record = json!({
                "access_token": "cadence-access",
                "refresh_token": "cadence-refresh",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": null },
                "obtained_at_unix": now
            });
            ((*seat).to_string(), record)
        })
        .collect();
    let doc = json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().expect("creds parent")).expect("mkdir creds parent");
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&doc).expect("serialize creds"),
    )
    .expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

#[tokio::test]
async fn a_seat_change_rebuild_logs_no_advisory_warning() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let creds = dir.path().join("routectl").join("credentials.json");
    write_credentials(&creds, &["anthropic"]);
    let mut config = parse_config(&retry_on_config_text()).expect("config parses");
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let composite = CompositeStore::open_at(&creds)
        .await
        .expect("open the credentials store");
    let oauth = composite.oauth_store().expect("oauth arm present");
    let secrets: Arc<dyn SecretStore> = Arc::new(composite);
    let router = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let before = swap.load_full();
    write_credentials(&creds, &["anthropic", "anthropic#seat-b"]);

    // Act
    let ((), events) = with_capture(Box::pin(handle_credentials_reload(
        &Some(oauth),
        &config,
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )))
    .await;

    // Assert: the seat set changed, so the router was rebuilt and swapped,
    // and that rebuild repeated no advisory finding.
    assert!(
        !Arc::ptr_eq(&before, &swap.load_full()),
        "a seat-set change must rebuild the router"
    );
    assert_eq!(retry_deprecation_warns(&events), 0, "{events:#?}");
}

fn chat_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": "glm-4.6",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

/// Alias `a` -> model `glm` on an openai-compat entry dialing `origin`, with
/// an auth-reserved key in the model's `header_extras`.
fn auth_reserved_config_text(origin: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 1\n\n\
         [providers.compat]\nkind = \"openai-compat\"\nbase_url = \"{origin}\"\n\
         api_key_ref = \"{}\"\n\n\
         [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n\n\
         [models.glm.header_extras]\nAuthorization = \"Bearer operator-typed\"\n\n\
         [aliases]\na = \"glm\"\n",
        crate::test_secret::file_ref("cadence-key")
    )
}

#[tokio::test]
async fn an_auth_reserved_model_header_warns_at_startup_and_debugs_per_request() {
    // Arrange
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_completion())
        .mount(&upstream)
        .await;
    let origin = format!("{}/v1", upstream.uri());
    let mut config = parse_config(&auth_reserved_config_text(&origin)).expect("config parses");
    let _usage_dir = isolate_usage_db(&mut config);
    let body = json!({"model": "a", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]});

    // Act: boot, then two requests through the model.
    let events = Box::pin(serve_capturing(config, |base_url| async move {
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let resp = client
                .post(format!("{base_url}/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .expect("the daemon answers");
            assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
        }
    }))
    .await;

    // Assert
    let advisories: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == AUTH_RESERVED_CATEGORY)
        .collect();
    assert_eq!(advisories.len(), 1, "{events:#?}");
    let advisory = advisories[0].field("warning").unwrap_or_default();
    assert!(
        advisory.contains("[models.glm.header_extras]") && advisory.contains("Authorization"),
        "{advisory}"
    );
    assert!(
        !advisory.contains("operator-typed"),
        "the advisory never quotes the header value: {advisory}"
    );
    assert_eq!(
        count(&events, tracing::Level::WARN, OVERLAY_DROP_MESSAGE),
        0,
        "{events:#?}"
    );
    assert_eq!(
        count(&events, tracing::Level::DEBUG, OVERLAY_DROP_MESSAGE),
        2,
        "{events:#?}"
    );
}
