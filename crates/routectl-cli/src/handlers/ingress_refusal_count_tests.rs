//! A request the ingress refuses while parsing it counts once on
//! `rc_request_refused_local_total` under `<ingress>->local` with its
//! reason; a body that is not JSON at all counts there as unparseable.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_router::{Config, Router};

use super::ingress_handle;
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

fn state() -> (Arc<AppState>, Arc<arc_swap::ArcSwap<Router>>) {
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(Router::new(Arc::new(
        Config::default(),
    ))));
    (AppState::for_test(Arc::clone(&swap)), swap)
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

/// Expected `(lane, reason, count)` local-refusal keys.
type Refusals = &'static [(&'static str, &'static str, u64)];

/// Post `body` to `adapter`'s ingress and return the local-refusal keys
/// the router holds afterwards.
async fn refusals_after<A: IngressAdapter + 'static>(
    adapter: A,
    body: &'static str,
) -> Vec<(&'static str, &'static str, u64)> {
    let (state, swap) = state();

    let resp = ingress_handle(
        state,
        json_headers(),
        None,
        Ok(Bytes::from_static(body.as_bytes())),
        adapter,
    )
    .await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    swap.load()
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect()
}

#[tokio::test]
async fn an_ingress_validation_refusal_counts_under_its_local_lane() {
    let refusals = refusals_after(
        ResponsesIngress,
        r#"{"model":"a","input":"hi","previous_response_id":"resp_1"}"#,
    )
    .await;

    assert_eq!(refusals, [("responses->local", "previous-response-id", 1)]);
}

#[tokio::test]
async fn malformed_json_counts_as_unparseable_under_each_ingress_local_lane() {
    const MALFORMED: &str = "{ not valid json";
    let observed = [
        refusals_after(AnthropicIngress, MALFORMED).await,
        refusals_after(OpenAiIngress, MALFORMED).await,
        refusals_after(ResponsesIngress, MALFORMED).await,
    ];

    let expected: [Refusals; 3] = [
        &[("anthropic->local", "unparseable", 1)],
        &[("chat->local", "unparseable", 1)],
        &[("responses->local", "unparseable", 1)],
    ];
    assert_eq!(observed, expected);
}
