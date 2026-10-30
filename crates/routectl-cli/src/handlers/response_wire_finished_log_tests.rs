//! A request served through the ingress handler under the production log
//! sink at the default `info` level writes exactly one `request finished`
//! line, and nothing the sink writes carries the client's user-agent,
//! session header or prompt text.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;

use super::tests::{Dialect, router, upstream};
use super::trace_tests::{RouterSwap, app, traced_at};
use crate::handlers::request_finished_log::REQUEST_FINISHED;

const PLANTED_ID: &str = "planted-request-id-0c61";
const PLANTED_UA: &str = "planted-agent/9.9 (fingerprint-d2e7)";
const PLANTED_SESSION: &str = "planted-session-a83b";
const PLANTED_PROMPT: &str = "planted-prompt-text-5f19";

fn planted_request(stream: bool) -> Request<Body> {
    let body = serde_json::json!({
        "model": "a", "max_tokens": 16, "stream": stream,
        "messages": [{"role": "user", "content": PLANTED_PROMPT}]
    });
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-request-id", PLANTED_ID)
        .header("user-agent", PLANTED_UA)
        .header("x-claude-code-session-id", PLANTED_SESSION)
        .body(Body::from(serde_json::to_vec(&body).expect("json")))
        .expect("request builds")
}

fn served_log(stream: bool) -> String {
    let ((), log) = traced_at("info", || async move {
        let server = upstream(Dialect::Anthropic, stream, None).await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(Dialect::Anthropic, &server.uri(), (0, 1000)).await,
        ));
        let resp = app(&swap)
            .oneshot(planted_request(stream))
            .await
            .expect("infallible router");
        assert!(resp.status().is_success(), "status {}", resp.status());
        to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    });
    log
}

#[test]
fn a_served_request_logs_one_content_free_finished_line_at_info() {
    for stream in [false, true] {
        // Act
        let log = served_log(stream);

        // Assert
        let lines: Vec<&str> = log
            .lines()
            .filter(|l| l.contains(REQUEST_FINISHED))
            .collect();
        assert_eq!(lines.len(), 1, "stream={stream}: {log}");
        let line = lines[0];
        assert!(line.contains(" INFO "), "stream={stream}: {line}");
        assert!(
            line.contains(&format!("request_id={PLANTED_ID}")),
            "control: the sink renders planted client text: {line}"
        );
        assert!(line.contains("outcome=ok"), "stream={stream}: {line}");
        for planted in [
            PLANTED_UA,
            "fingerprint-d2e7",
            PLANTED_SESSION,
            PLANTED_PROMPT,
        ] {
            assert!(
                !log.contains(planted),
                "stream={stream}: {planted} leaked into the log: {log}"
            );
        }
    }
}
