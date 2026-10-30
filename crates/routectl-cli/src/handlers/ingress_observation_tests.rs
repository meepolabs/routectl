//! The ingress installs the request's observation slot before dispatch, so
//! what the router counts against the request reaches its ledger row, on
//! both the complete and the stream path.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::openai::OpenAiIngress;
use crate::server::AppState;

async fn anthropic_api_router(base: &str) -> Router {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let config = Config {
        providers: BTreeMap::from([(
            "p".to_string(),
            ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")).with_base_url(base),
        )]),
        models: BTreeMap::from([("m".to_string(), ModelEntry::new("p", "upstream"))]),
        aliases: BTreeMap::from([("a".to_string(), AliasValue::Single("m".to_string()))]),
        retry,
        ..Default::default()
    };
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router")
}

/// A chat request the anthropic-api egress refuses locally: a tool result
/// with an empty call id.
fn refused_body(stream: bool) -> Bytes {
    let body = json!({
        "model": "a", "max_tokens": 16, "stream": stream,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "tool", "tool_call_id": "", "content": "r"}
        ]
    });
    Bytes::from(serde_json::to_vec(&body).expect("body serializes"))
}

/// Send one refused request through the real ingress and return the
/// client status and the persisted `local_refusals` of every row.
async fn refusals_persisted(stream: bool) -> (StatusCode, Vec<Option<i64>>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(
        anthropic_api_router(&upstream.uri()).await,
    ));
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state = AppState::for_test_with_usage(swap, usage);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let resp = ingress_handle(
        state,
        headers,
        None,
        Ok(refused_body(stream)),
        OpenAiIngress,
    )
    .await;
    let status = resp.status();
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("response body drains");
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("usage writer shutdown");

    let conn = rusqlite::Connection::open(&db_path).expect("read open");
    let mut query = conn
        .prepare("SELECT local_refusals FROM requests")
        .expect("prepare");
    let rows = query
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<Vec<Option<i64>>, _>>()
        .expect("rows");
    (status, rows)
}

#[tokio::test]
async fn a_router_refusal_reaches_the_persisted_row_on_both_paths() {
    let mut observed = Vec::new();
    for stream in [false, true] {
        observed.push((stream, refusals_persisted(stream).await));
    }

    assert_eq!(
        observed,
        [
            (false, (StatusCode::BAD_REQUEST, vec![Some(1)])),
            (true, (StatusCode::BAD_REQUEST, vec![Some(1)])),
        ]
    );
}
