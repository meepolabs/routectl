//! A daemon whose every pool seat has an unreadable credential, end to end:
//! the real build (no injected router) over an empty credentials file keeps
//! each member as a placeholder seat, the daemon starts, every ingress answers
//! 503 `credential_unusable` with no `Retry-After`, the configured upstream
//! accepts no connection, and the usage row records a gate-blocked request
//! with no attempt and the last seat walked. A config reload with the
//! placeholders unchanged keeps each mark and its `since`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_auth::SecretStore;
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{CURRENT_CONFIG_VERSION, CatalogOverlay, Config, Router, parse_config};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::CompositeStore;
use super::reload::handle_config_reload;
use super::reload_failure::ReloadFailure;
use super::test_support::{drain_usage_writer_strict, isolate_usage_db, never_shutdown};

const WAIT: Duration = Duration::from_secs(10);

/// Alias `a` -> model `grok` on pool `compat-pool` of two `oauth://xai` seats
/// whose entries point at `upstream`; alias `c` -> model `claude` on pool
/// `claude-pool` of two `oauth://anthropic` seats, the token-count lane.
fn config_text(upstream: &str) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 1\n\n\
         [providers.seat-a]\nkind = \"openai-compat\"\nbase_url = \"{upstream}\"\n\
         api_key_ref = \"oauth://xai#seat-a\"\n\n\
         [providers.seat-b]\nkind = \"openai-compat\"\nbase_url = \"{upstream}\"\n\
         api_key_ref = \"oauth://xai#seat-b\"\n\n\
         [providers.claude-a]\nkind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#seat-a\"\nauth_kind = \"oauth-bearer\"\n\n\
         [providers.claude-b]\nkind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#seat-b\"\nauth_kind = \"oauth-bearer\"\n\n\
         [pools.compat-pool]\nmembers = [\"seat-a\", \"seat-b\"]\n\n\
         [pools.claude-pool]\nmembers = [\"claude-a\", \"claude-b\"]\n\n\
         [models.grok]\nprovider = \"compat-pool\"\nupstream = \"grok-4\"\n\n\
         [models.claude]\nprovider = \"claude-pool\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [aliases]\na = \"grok\"\nc = \"claude\"\n"
    )
}

/// A composite store over an empty credentials file in `dir`: every OAuth
/// seat is logged out.
async fn logged_out_secrets(dir: &std::path::Path) -> Arc<dyn SecretStore> {
    let store = CompositeStore::open_at(dir.join("credentials.json"))
        .await
        .expect("open an empty credentials store");
    Arc::new(store)
}

struct Daemon {
    base_url: String,
    router: Arc<ArcSwap<Router>>,
    handle: tokio::task::JoinHandle<()>,
}

/// Serve `config`, building its router from `secrets` the way production
/// does: no injected router.
async fn spawn(config: Config, secrets: Arc<dyn SecretStore>) -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    let (observer_tx, observer_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        super::serve::serve_on_listener_with_injected_router(
            Arc::new(config),
            Arc::new(CatalogOverlay::default()),
            listener,
            None,
            Some(secrets),
            super::serve::DaemonTestSeams {
                router_observer: Some(observer_tx),
                ..Default::default()
            },
        )
        .await
        .expect("a daemon whose every seat is unusable still serves");
    });
    let router = tokio::time::timeout(WAIT, observer_rx)
        .await
        .expect("the daemon publishes its router at boot")
        .expect("the observer is not dropped before publication");
    let daemon = Daemon {
        base_url,
        router,
        handle,
    };
    daemon.await_health().await;
    daemon
}

impl Daemon {
    async fn await_health(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(!self.handle.is_finished(), "the daemon exited at boot");
            assert!(
                Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn post(&self, route: &str, request_id: &str, body: &Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{route}", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("x-request-id", request_id)
            .json(body)
            .send()
            .await
            .expect("the daemon answers")
    }
}

/// `(route, body)` for each ingress, against alias `a`.
fn ingresses() -> [(&'static str, Value); 3] {
    let messages = json!({"model": "a", "max_tokens": 16,
                          "messages": [{"role": "user", "content": "hi"}]});
    [
        ("/v1/chat/completions", messages.clone()),
        ("/v1/messages", messages),
        ("/v1/responses", json!({"model": "a", "input": "hi"})),
    ]
}

/// The status, `Retry-After` and error type of a refusal.
async fn refusal_of(resp: reqwest::Response) -> (u16, Option<String>, Value) {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, retry_after, body["error"]["type"].clone())
}

/// `(attempt_count, outcome, provider, model)` of `request_id`'s row, once
/// the writer has committed it.
async fn usage_row(db: &std::path::Path, request_id: &str) -> (i64, String, String, String) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(conn) = routectl_usage::open_readonly(db)
            && let Ok(row) = conn.conn().query_row(
                "SELECT attempt_count, outcome, provider, model FROM requests \
                 WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        {
            return row;
        }
        assert!(Instant::now() < deadline, "no usage row for {request_id}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_all_placeholder_daemon_starts_and_refuses_every_ingress_without_dialing() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let upstream = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the upstream");
    let origin = format!("http://{}/v1", upstream.local_addr().expect("addr"));
    let mut config = parse_config(&config_text(&origin)).expect("the config parses");
    let _usage_dir = isolate_usage_db(&mut config);
    let usage_db = config.usage.db_path.clone();
    let daemon = spawn(config, logged_out_secrets(dir.path()).await).await;

    for (route, body) in ingresses() {
        // Act
        let request_id = format!("placeholder{}", route.replace('/', "-"));
        let resp = daemon.post(route, &request_id, &body).await;

        // Assert
        let (status, retry_after, error_type) = refusal_of(resp).await;
        assert_eq!(status, 503, "{route}: {error_type}");
        assert_eq!(error_type, "credential_unusable", "{route}");
        assert_eq!(retry_after, None, "{route}: no recovery time is known");
        let (attempts, outcome, provider, model) = usage_row(&usage_db, &request_id).await;
        assert_eq!(attempts, 0, "{route}");
        assert_eq!(outcome, "gate_blocked", "{route}");
        assert_eq!(provider, "seat-b", "{route}: the last seat walked");
        assert_eq!(model, "grok", "{route}");
    }
    let accepted = tokio::time::timeout(Duration::from_millis(200), upstream.accept()).await;
    assert!(accepted.is_err(), "the upstream must accept no connection");

    // A token count over a pool of placeholders is the same local refusal,
    // never an unknown-provider 404.
    let count = json!({"model": "c", "messages": [{"role": "user", "content": "hi"}]});
    let resp = daemon
        .post("/v1/messages/count_tokens", "placeholder-count", &count)
        .await;
    let (status, retry_after, error_type) = refusal_of(resp).await;
    assert_eq!(status, 503, "count_tokens: {error_type}");
    assert_eq!(error_type, "credential_unusable");
    assert_eq!(retry_after, None);
    assert!(
        daemon
            .router
            .load()
            .status_targets(Instant::now())
            .iter()
            .all(|t| t.gate.credential_unusable.is_some()),
        "every seat reports its unusable mark"
    );
    daemon.handle.abort();
}

/// The unusable mark on `state_key`'s account, read off the live router.
fn mark_of(router: &Router, state_key: &str) -> Option<(UnusableCause, Duration)> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .find(|t| t.state_key == state_key)
        .unwrap_or_else(|| panic!("no status target {state_key}"))
        .gate
        .credential_unusable
        .map(|mark| (mark.cause, mark.since_elapsed))
}

#[tokio::test]
async fn a_config_reload_with_an_unchanged_placeholder_keeps_its_mark_and_since() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("config.toml");
    let text = config_text("http://127.0.0.1:1/v1");
    std::fs::write(&cfg_path, &text).expect("write config");
    let secrets = logged_out_secrets(dir.path()).await;
    let mut config = parse_config(&text).expect("the config parses");
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let router = super::build_router_from_config_with_overlay(
        config.clone(),
        &Arc::default(),
        secrets.clone(),
    )
    .await
    .expect("a pool of placeholders builds");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let (usage, writer) = super::serve::build_usage_writer(&config);
    let (cause, first_age) = mark_of(&swap.load(), "grok#seat-a").expect("marked at install");
    assert_eq!(cause, UnusableCause::NotLoggedIn);
    let before = swap.load_full();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Act
    let reloaded: Result<_, ReloadFailure> = Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        super::reload::ReloadTrigger::ConfigFile,
        &mut never_shutdown(),
    ))
    .await;

    // Assert
    reloaded
        .expect("the reload applies")
        .expect("the reload returns its config");
    assert!(!Arc::ptr_eq(&before, &swap.load_full()), "router swapped");
    let (cause, age) = mark_of(&swap.load(), "grok#seat-a").expect("still unusable");
    assert_eq!(cause, UnusableCause::NotLoggedIn);
    assert!(
        age >= first_age + Duration::from_millis(50),
        "`since` is the first mark's, not the reload's: {first_age:?} then {age:?}"
    );
    drop(usage);
    drain_usage_writer_strict(writer).await;
}
