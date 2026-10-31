//! A target the router's own gate refuses answers every ingress with the local
//! refusal: HTTP 503 whose error type is the reason token and a `Retry-After`
//! of the known wait, without dialing the upstream. The refusal logs at DEBUG
//! only, since each request lands on it while the target stays refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Bytes, to_bytes};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_core::{EligibilityReason, Error};
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy, Router,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{ErrorEnvelopeShape, ingress_handle, map_error};
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const BREAKER_WINDOW: Duration = Duration::from_mins(1);

#[derive(Clone, Copy, Debug)]
enum Ingress {
    Chat,
    Messages,
    Responses,
}

const INGRESSES: [Ingress; 3] = [Ingress::Chat, Ingress::Messages, Ingress::Responses];

fn body_for(ingress: Ingress) -> Bytes {
    let body = match ingress {
        Ingress::Chat => json!({
            "model": ALIAS, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        Ingress::Messages => json!({
            "model": ALIAS, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        Ingress::Responses => json!({"model": ALIAS, "input": "hi"}),
    };
    Bytes::from(serde_json::to_vec(&body).expect("body serializes"))
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers
}

/// A mock upstream that fails every call, so only the router's gate decides
/// whether a request is refused.
async fn failing_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

async fn upstream_calls(server: &MockServer) -> usize {
    server.received_requests().await.map_or(0, |r| r.len())
}

/// Alias `a` -> model `m` on the single openai-compat provider `p` at `base`.
async fn single_target(base: &str, runtime: ProviderRuntimePolicy) -> Arc<AppState> {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let entry =
        ProviderEntry::openai_compat(base, crate::test_secret::file_ref("k")).with_runtime(runtime);
    let config = Config {
        providers: BTreeMap::from([("p".to_string(), entry)]),
        models: BTreeMap::from([(NICKNAME.to_string(), ModelEntry::new("p", "gpt-4o"))]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    let router: Router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router");
    AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)))
}

/// One request through `ingress`: its status, `Retry-After` and error object.
async fn send(state: &Arc<AppState>, ingress: Ingress) -> (StatusCode, Option<u64>, Value) {
    let state = Arc::clone(state);
    let body = Ok(body_for(ingress));
    let resp = Box::pin(async {
        match ingress {
            Ingress::Chat => ingress_handle(state, headers(), None, body, OpenAiIngress).await,
            Ingress::Messages => {
                ingress_handle(state, headers(), None, body, AnthropicIngress).await
            }
            Ingress::Responses => {
                ingress_handle(state, headers(), None, body, ResponsesIngress).await
            }
        }
    })
    .await;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body reads");
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, retry_after, value["error"].clone())
}

#[tokio::test]
async fn an_open_breaker_answers_every_ingress_503_breaker_open_without_dialing() {
    for ingress in INGRESSES {
        // Arrange
        let upstream = failing_upstream().await;
        let state = single_target(&upstream.uri(), ProviderRuntimePolicy::default()).await;
        assert!(
            state
                .router
                .load()
                .force_open_breaker(NICKNAME, BREAKER_WINDOW),
            "premise: the target has a breaker slot"
        );

        // Act
        let (status, retry_after, error) = send(&state, ingress).await;

        // Assert
        let row = format!("{ingress:?}");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{row}: {error}");
        assert_eq!(error["type"], "breaker_open", "{row}: {error}");
        assert_ne!(error["type"], "upstream_error", "{row}");
        let secs = retry_after.unwrap_or_else(|| panic!("{row}: Retry-After is set"));
        assert!((1..=60).contains(&secs), "{row}: Retry-After {secs}");
        assert_eq!(upstream_calls(&upstream).await, 0, "{row}: never dialed");
    }
}

#[tokio::test]
async fn an_exhausted_rpm_limit_answers_every_ingress_503_rate_limited_with_a_wait() {
    for ingress in INGRESSES {
        // Arrange: one request per minute, spent by a first request.
        let upstream = failing_upstream().await;
        let mut runtime = ProviderRuntimePolicy::default();
        runtime.rpm_limit = Some(1);
        let state = single_target(&upstream.uri(), runtime).await;
        let (first, _, _) = send(&state, ingress).await;
        let row = format!("{ingress:?}");
        assert_eq!(
            upstream_calls(&upstream).await,
            1,
            "{row}: premise: token spent"
        );
        assert_ne!(
            first,
            StatusCode::SERVICE_UNAVAILABLE,
            "{row}: premise: dialed"
        );

        // Act
        let (status, retry_after, error) = send(&state, ingress).await;

        // Assert
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{row}: {error}");
        assert_eq!(error["type"], "rate_limited", "{row}: {error}");
        let secs = retry_after.unwrap_or_else(|| panic!("{row}: Retry-After is set"));
        assert!((1..=60).contains(&secs), "{row}: Retry-After {secs}");
        assert_eq!(
            upstream_calls(&upstream).await,
            1,
            "{row}: the refusal never dialed"
        );
    }
}

#[test]
fn rendering_a_local_refusal_logs_debug_never_warn() {
    // Arrange
    let refusal = || Error::LocallyUnavailable {
        reason: EligibilityReason::BreakerOpen,
        retry_after: Some(Duration::from_secs(30)),
    };

    // Act
    let events = routectl_testkit::capture_events(|| {
        for _ in 0..10 {
            let _resp = map_error(ErrorEnvelopeShape::OpenAi, refusal());
        }
    });

    // Assert
    let refusals: Vec<_> = events
        .iter()
        .filter(|e| e.message == "no eligible target; refused before dispatch")
        .collect();
    assert_eq!(refusals.len(), 10, "one event per refusal: {events:?}");
    assert!(
        refusals.iter().all(|e| e.level == tracing::Level::DEBUG),
        "{refusals:?}"
    );
    let warns = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .count();
    assert_eq!(warns, 0, "{events:?}");
}
