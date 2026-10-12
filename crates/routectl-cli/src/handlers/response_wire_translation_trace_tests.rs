//! A streamed answer on a TRANSLATION lane -- the client speaks one dialect,
//! the egress another -- reaches the same TRACE body lines a checked
//! fidelity stream does: one `upstream success body` in the upstream's own
//! dialect, under the provider's own `provider_kind`, and one `egress
//! response body` in the client's, inside the request span. Neither line is
//! written for a stream either side of which never finished, and nothing is
//! folded while the body trace is off.

use std::sync::Arc;

use routectl_router::wire_check_state::CheckOutcome;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::tests::{CHECK_EVERY, Dialect, HangingUpstream, outcomes, router, upstream};
use super::trace_tests::{
    EGRESS_MSG, OUTGOING_MSG, RouterSwap, UPSTREAM_MSG, app, body_lines, count, ingress_id,
    post_request, send, stream_trace, traced,
};

/// Never admitted: the first-N window is closed and one in 1000 is sampled.
const CHECK_NONE: (u32, u32) = (0, 1000);

type Recorded = Vec<(&'static str, CheckOutcome, u64)>;

/// One stream from a `client`-dialect ingress to an `egress`-dialect
/// upstream, traced.
fn traced_translation(
    client: Dialect,
    egress: Dialect,
    sampling: (u32, u32),
) -> (Recorded, String) {
    traced(|| async move {
        let server = upstream(egress, true, None).await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(egress, &server.uri(), sampling).await,
        ));
        send(&swap, client, true).await;
        outcomes(&swap.load().wire_check_snapshot())
    })
}

const fn provider_kind(egress: Dialect) -> &'static str {
    match egress {
        Dialect::Anthropic => "anthropic",
        Dialect::Chat => "openai-compat",
        Dialect::Responses => "openai-responses",
    }
}

/// The lane label a `client` request on an `egress` provider records.
fn lane(client: Dialect, egress: Dialect) -> String {
    let ingress = client.lane().split("->").next().unwrap_or_default();
    let egress = egress.lane().split("->").nth(1).unwrap_or_default();
    format!("{ingress}->{egress}")
}

#[test]
fn a_translation_stream_traces_each_document_once_under_the_providers_own_token() {
    let rows: [(&str, Dialect, Dialect, (u32, u32)); 4] = [
        (
            "anthropic over openai-compat, admitted",
            Dialect::Anthropic,
            Dialect::Chat,
            CHECK_EVERY,
        ),
        (
            "anthropic over openai-compat, unadmitted",
            Dialect::Anthropic,
            Dialect::Chat,
            CHECK_NONE,
        ),
        (
            "chat over anthropic",
            Dialect::Chat,
            Dialect::Anthropic,
            CHECK_EVERY,
        ),
        (
            "responses over openai-compat",
            Dialect::Responses,
            Dialect::Chat,
            CHECK_EVERY,
        ),
    ];
    for (name, client, egress, sampling) in rows {
        let (recorded, log) = traced_translation(client, egress, sampling);

        let trace = stream_trace(&log);
        assert_eq!(trace.upstream.len(), 1, "{name}: {log}");
        assert_eq!(trace.egress.len(), 1, "{name}: {log}");
        assert_eq!(trace.upstream, trace.outgoing, "{name}");
        assert!(
            trace.upstream[0].starts_with(&format!("provider_kind=\"{}\" ", provider_kind(egress))),
            "{name}: {:?}",
            trace.upstream
        );
        assert_eq!(
            trace.egress,
            [format!("ingress=\"{}\"", ingress_id(client))],
            "{name}"
        );
        assert_eq!(trace.outside_request_span, 0, "{name}: {log}");
        let response_leg = recorded
            .iter()
            .filter(|(_, outcome, n)| *outcome != CheckOutcome::Clean || *n > 1)
            .count();
        assert_eq!(
            response_leg, 0,
            "{name}: no response leg recorded: {recorded:?}"
        );
        assert!(
            recorded.iter().all(|(l, _, _)| *l == lane(client, egress)),
            "{name}: {recorded:?}"
        );
    }
}

/// Each line carries the document in its own dialect: the upstream's as
/// the chat completion it streamed, the client's as the Anthropic message
/// it received.
#[test]
fn each_traced_document_is_in_its_own_sides_dialect() {
    let (_, log) = traced_translation(Dialect::Anthropic, Dialect::Chat, CHECK_EVERY);

    let bodies: Vec<(&str, serde_json::Value)> = log
        .lines()
        .filter_map(|line| {
            let message = [UPSTREAM_MSG, EGRESS_MSG]
                .into_iter()
                .find(|m| line.contains(&format!("routectl_core::log_safe: {m}")))?;
            let body = line.split(" body=").nth(1)?;
            Some((message, serde_json::from_str(body).expect("body is json")))
        })
        .collect();

    let [(UPSTREAM_MSG, upstream), (EGRESS_MSG, client)] = bodies.as_slice() else {
        panic!("one upstream then one egress line: {log}");
    };
    assert_eq!(upstream["object"], json!("chat.completion"), "{upstream}");
    assert_eq!(
        upstream.pointer("/choices/0/message/content"),
        Some(&json!("ok"))
    );
    assert_eq!(client["type"], json!("message"), "{client}");
    assert_eq!(client.pointer("/content/0/text"), Some(&json!("ok")));
}

/// A chat stream that answers once and then reports a mid-stream error.
fn chat_stream_with_error() -> String {
    let first = json!({"id": "c1", "object": "chat.completion.chunk",
        "created": 1_700_000_000_i64, "model": "upstream",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}}]});
    let error = json!({"error": {"message": "overloaded", "type": "server_error"}});
    format!("data: {first}\n\ndata: {error}\n\n")
}

fn traced_upstream_error() -> (Recorded, String) {
    traced(|| async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_stream_with_error()),
            )
            .mount(&server)
            .await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(Dialect::Chat, &server.uri(), CHECK_EVERY).await,
        ));
        let client = send(&swap, Dialect::Anthropic, true).await;
        assert!(client.contains("event: error"), "{client}");
        outcomes(&swap.load().wire_check_snapshot())
    })
}

/// The first chat frame of an answer, then a held-open connection.
fn chat_first_frame() -> String {
    let first = json!({"id": "c1", "object": "chat.completion.chunk",
        "created": 1_700_000_000_i64, "model": "upstream",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}}]});
    format!("data: {first}\n\n")
}

fn traced_client_cancel() -> (Recorded, String) {
    use futures::StreamExt;
    traced(|| async {
        let upstream = HangingUpstream::start_with(chat_first_frame()).await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(Dialect::Chat, &upstream.base, CHECK_EVERY).await,
        ));
        let resp = tower::ServiceExt::oneshot(app(&swap), post_request(Dialect::Anthropic, true))
            .await
            .expect("infallible router");
        let mut frames = resp.into_body().into_data_stream();
        let first = frames.next().await.expect("a first frame").expect("bytes");
        assert!(!first.is_empty());
        drop(frames);
        // The render task holds the router until it has dropped its fold,
        // so the router's count falling back to the swap's own and this
        // probe's marks the fold gone.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while Arc::strong_count(&swap.load_full()) > 2 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the render task ends once the client leaves");
        outcomes(&swap.load().wire_check_snapshot())
    })
}

/// A translation stream either side of which never finished traces no
/// document: a partial one would land in a fixture as the whole answer.
#[test]
fn an_unfinished_translation_stream_traces_neither_document() {
    type Run = fn() -> (Recorded, String);
    let rows: [(&str, Run); 2] = [
        ("upstream error mid-stream", traced_upstream_error),
        ("client left mid-stream", traced_client_cancel),
    ];
    for (name, run) in rows {
        let (_, log) = run();

        let lines = body_lines(&log);
        assert_eq!(
            count(&lines, OUTGOING_MSG),
            1,
            "{name}: the trace was captured: {log}"
        );
        assert_eq!(count(&lines, UPSTREAM_MSG), 0, "{name}: {log}");
        assert_eq!(count(&lines, EGRESS_MSG), 0, "{name}: {log}");
    }
}
