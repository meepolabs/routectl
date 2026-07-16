//! Cross-cutting end-to-end tests for the `/status` surface over a REAL
//! bound listener (the same `serve_on_listener` entry point the `serve`
//! subcommand uses). These validate behaviors that only emerge once the
//! panels, the status-subtree wiring, and the aggregate are all assembled.
//!
//! Scope is deliberately the NOT-yet-covered end-to-end cases. The pure
//! auth-exemption (`/status/*` reachable with tokens set while `/v1/*` stays
//! gated) and host-allowlist (a disallowed `Host` rejects `/status` but not
//! `/v1/*`) scenarios are already pinned over a live listener by
//! `status_subtree_is_auth_exempt_while_v1_still_requires_a_token` and
//! `host_allowlist_rejects_status_but_not_v1` in `tests/server.rs`; they are
//! not duplicated here.
//!
//! The load-shed 503 (its fixed envelope shape AND its per-shed sampling) is
//! covered deterministically at UNIT level in
//! `crate::server::status_gate::tests` via a barrier-held handler that pins
//! four in-flight permits, plus the shed-observability capture test. An
//! HTTP-level shed test is deliberately omitted: saturating the shared
//! concurrency cap over the bound listener needs handlers that block
//! in-flight, but the real status handlers complete fast, so forcing a shed
//! at the HTTP boundary is inherently racy. The unit-level barrier is the
//! faithful, non-flaky substitute.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy, ServerConfig,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

/// Bind to 127.0.0.1:0, spawn the real server, return the base URL once
/// `/health` answers. Mirrors `tests/server.rs`'s harness.
async fn spawn(config: Arc<Config>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    await_health(&base).await;
    base
}

async fn await_health(base: &str) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{base}/health")).send().await
            && resp.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("test server did not become healthy at {base}");
}

/// One openai-compat provider with a 1-failure circuit breaker and a short
/// cooldown, so a single upstream 5xx trips the breaker and it recovers to
/// half-open within the test's poll budget. Usage is isolated to a unique
/// per-process path so the booted writer never touches the real ledger.
fn breaker_config(upstream_base: &str) -> Arc<Config> {
    let mut runtime = ProviderRuntimePolicy::default();
    runtime.circuit_failures = Some(1);
    runtime.circuit_cooldown_ms = Some(200);

    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat(upstream_base, common::file_ref("test-key"))
            .with_runtime(runtime),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;

    common::isolate_usage_db(Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry,
        models,
        ..Default::default()
    }))
}

fn chat_body() -> Value {
    json!({
        "model": "a",
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// Iterate the health panel's route targets.
fn targets(health: &Value) -> impl Iterator<Item = &Value> {
    health["data"]["targets"].as_array().into_iter().flatten()
}

fn any_circuit_is(health: &Value, want: &str) -> bool {
    targets(health).any(|t| t["circuit"] == want)
}

/// `half_open_probe_in_flight` of the (single) half-open target, if present.
fn half_open_probe_flag(health: &Value) -> Option<bool> {
    targets(health)
        .find(|t| t["circuit"] == "half_open_ready")
        .and_then(|t| t["half_open_probe_in_flight"].as_bool())
}

async fn upstream_hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording enabled")
        .len()
}

/// Scenario #2 (#3 in the spec): the HTTP-level analog of the router-unit
/// non-perturbation guard. Drive a target to half-open, then hammer
/// `/status/health` -- no read dials upstream or claims the probe slot -- and
/// prove the FIRST real proxy request afterward still consumes the probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_storm_does_not_perturb_breaker_probe() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&upstream)
        .await;

    let base = spawn(breaker_config(&upstream.uri())).await;
    let client = reqwest::Client::new();

    // Trip the breaker: one attempt, one upstream 5xx, threshold 1 -> Open.
    let _ = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        upstream_hits(&upstream).await,
        1,
        "the trip request hits upstream exactly once"
    );

    // Poll /status/health until the cooldown elapses and the gate reads
    // half-open. Reads are non-mutating, so this never dials upstream.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let health: Value = client
            .get(format!("{base}/status/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if any_circuit_is(&health, "half_open_ready") {
            assert_eq!(
                half_open_probe_flag(&health),
                Some(false),
                "a status read must never claim the probe slot"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "circuit never recovered to half_open_ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        upstream_hits(&upstream).await,
        1,
        "polling /status/health must not dial upstream"
    );

    // HTTP storm: many concurrent /status/health. Some may be load-shed as
    // 503 (the subtree cap), which is fine -- none of them may perturb the
    // breaker or dial upstream. Bodies are drained, not asserted per-response.
    let mut storm = Vec::new();
    for _ in 0..40 {
        let client = client.clone();
        let base = base.clone();
        storm.push(tokio::spawn(async move {
            let _ = client.get(format!("{base}/status/health")).send().await;
        }));
    }
    for handle in storm {
        handle.await.unwrap();
    }
    assert_eq!(
        upstream_hits(&upstream).await,
        1,
        "the /status storm must fire NO upstream call"
    );

    // A settling read (never shed) confirms the slot is still untouched.
    let health: Value = client
        .get(format!("{base}/status/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(any_circuit_is(&health, "half_open_ready"));
    assert_eq!(half_open_probe_flag(&health), Some(false));

    // The first real proxy request consumes the half-open probe slot the
    // reads left available -> exactly one new upstream call.
    let _ = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        upstream_hits(&upstream).await,
        2,
        "the first real request consumed the probe slot the storm never touched"
    );
}

/// Scenario #3 (usage DB unavailable, HTTP level): a daemon pointed at a
/// schema-mismatched usage DB serves `GET /status/usage` as HTTP 200 with a
/// code-only `unavailable` panel and `as_of: null` -- never stale data, never
/// a 500.
#[tokio::test]
async fn usage_db_unavailable_end_to_end_returns_unavailable_not_500() {
    // Pre-seed a usage DB whose on-disk schema version is beyond this binary,
    // so both the writer (which degrades) and the read-only panel refuse it.
    // Kept for the whole test; the detached server is torn down with the
    // process, so a late writer touch of a dropped path cannot matter.
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("usage.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "user_version", routectl_usage::SCHEMA_VERSION + 1)
            .unwrap();
    }

    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", common::file_ref("test-key")),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));

    let mut cfg = Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    };
    cfg.usage.db_path = db_path.clone();
    let base = spawn(Arc::new(cfg)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/status/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "an unreadable usage DB degrades to an unavailable panel, never a 500"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["unavailable"].is_string(),
        "usage panel must be code-only unavailable: {body}"
    );
    assert!(
        body["as_of"].is_null(),
        "an unavailable panel carries no as_of (never stale): {body}"
    );
    assert!(
        body["data"].is_null(),
        "an unavailable panel carries no data: {body}"
    );
}

/// The dashboard shell is served at `GET /` as HTML with a no-store cache
/// directive: a browser never caches the shell (the panel data it polls is
/// always live). Test obligations 1-3.
#[tokio::test]
async fn page_get_returns_html_shell_with_no_store() {
    let base = spawn(breaker_config("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "GET / must serve the dashboard shell");
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8"),
        "the page must be served as UTF-8 HTML"
    );
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "the page response must carry Cache-Control: no-store"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<html"),
        "GET / must return the embedded HTML document"
    );
}
