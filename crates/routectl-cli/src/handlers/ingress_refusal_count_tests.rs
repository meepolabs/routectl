//! A request the ingress refuses while parsing it counts once on
//! `rc_request_refused_local_total` under `<ingress>->local` with its
//! reason; a body that is not JSON at all is a malformed request, not a
//! refusal, and does not count.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_router::{Config, Router};

use super::ingress_handle;
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

#[tokio::test]
async fn an_ingress_validation_refusal_counts_under_its_local_lane_and_malformed_json_does_not() {
    let cases: [(&str, &'static str, Refusals); 2] = [
        (
            "server_side_state",
            r#"{"model":"a","input":"hi","previous_response_id":"resp_1"}"#,
            &[("responses->local", "previous-response-id", 1)],
        ),
        ("malformed_json", "{ not valid json", &[]),
    ];

    for (name, body, expected) in cases {
        let (state, swap) = state();

        let resp = ingress_handle(
            state,
            json_headers(),
            None,
            Ok(Bytes::from_static(body.as_bytes())),
            ResponsesIngress,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name}");
        let refusals: Vec<(&str, &str, u64)> = swap
            .load()
            .wire_check_snapshot()
            .local_refusals
            .iter()
            .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
            .collect();
        assert_eq!(refusals, expected, "{name}");
    }
}
