//! A checked stream's two documents reach the TRACE body lines a complete
//! response already emits: one `upstream success body` and one `egress
//! response body` per stream, inside the request span, under the same
//! `provider_kind` / `provider` the attempt's outgoing request line carries.
//! Driven through the request-id middleware and the production log sink, so
//! each line reads as a capture of a running daemon would.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::Request;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_router::wire_check_state::CheckOutcome;
use routectl_router::{CURRENT_CONFIG_VERSION, ResolvedModel, Router, parse_config};
use tower::ServiceExt;
use tracing_subscriber::EnvFilter;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::tests::{
    CHECK_EVERY, DIALECTS, Dialect, HangingUpstream, anthropic_unfinished_stream, outcomes, router,
    upstream,
};
use crate::log_sink::{self, Clock};
use crate::server::AppState;
use crate::server::request_id;

const REQUEST_ID: &str = "019eab77-0000-4000-8000-0000000000aa";
const UPSTREAM_MSG: &str = "upstream success body";
const EGRESS_MSG: &str = "egress response body";
const OUTGOING_MSG: &str = "outgoing request body";
const INGRESS_MSG: &str = "ingress request body";
const LOG_SAFE_TARGET: &str = "routectl_core::log_safe: ";

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("sink lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run `body` on a current-thread runtime under the production subscriber
/// at TRACE for the body-trace emitter, and return what the sink wrote. The
/// render task is spawned onto the same thread, so its events land here.
fn traced<F, Fut, T>(body: F) -> (T, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = log_sink::subscriber(
        EnvFilter::new("info,routectl_core::log_safe=trace"),
        move || writer.clone(),
        false,
        Clock::Off,
    );
    // A second live dispatch keeps callsite registration off the
    // single-dispatch shortcut, which can cache a sibling thread's "never".
    let _keepalive = tracing::Dispatch::new(tracing_subscriber::registry());
    let out = tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(body())
    });
    let text = String::from_utf8(sink.0.lock().expect("sink lock").clone()).expect("utf-8");
    (out, text)
}

const fn route(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Anthropic => "/v1/messages",
        Dialect::Chat => "/v1/chat/completions",
        Dialect::Responses => "/v1/responses",
    }
}

type RouterSwap = Arc<arc_swap::ArcSwap<Router>>;

/// The daemon's three ingress routes under the request-id middleware, as
/// production stacks them.
fn app(swap: &RouterSwap) -> axum::Router {
    axum::Router::new()
        .route("/v1/messages", post(crate::handlers::messages::messages))
        .route(
            "/v1/chat/completions",
            post(crate::handlers::chat_completions::chat_completions),
        )
        .route("/v1/responses", post(crate::handlers::responses::responses))
        .with_state(AppState::for_test(Arc::clone(swap)))
        .layer(axum::middleware::from_fn(request_id::middleware))
}

fn post_request(dialect: Dialect, stream: bool) -> Request<Body> {
    let body = serde_json::to_vec(&dialect.request_body(stream)).expect("json");
    Request::builder()
        .method("POST")
        .uri(route(dialect))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-request-id", REQUEST_ID)
        .body(Body::from(body))
        .expect("request builds")
}

/// Send one request through `swap`'s router and read the whole response
/// before returning, so the stream's check has run.
async fn send(swap: &RouterSwap, dialect: Dialect, stream: bool) -> String {
    let resp = app(swap)
        .oneshot(post_request(dialect, stream))
        .await
        .expect("infallible router");
    assert!(resp.status().is_success(), "status {}", resp.status());
    let body = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    String::from_utf8_lossy(&body).into_owned()
}

/// One body-trace line: its message, the field region before `body=`, and
/// whether it sits in the request span.
#[derive(Debug)]
struct BodyLine {
    message: &'static str,
    fields: String,
    in_request_span: bool,
}

/// Every body-trace line the log_safe emitter wrote, anchored on the
/// emitter target and the message right after it.
fn body_lines(log: &str) -> Vec<BodyLine> {
    let request_span = format!("request_id={REQUEST_ID}");
    log.lines()
        .filter_map(|line| {
            let at = line.find(LOG_SAFE_TARGET)?;
            let rest = &line[at + LOG_SAFE_TARGET.len()..];
            let message = [UPSTREAM_MSG, EGRESS_MSG, OUTGOING_MSG, INGRESS_MSG]
                .into_iter()
                .find(|m| rest.starts_with(m))?;
            let fields = rest[message.len()..]
                .split(" body=")
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            Some(BodyLine {
                message,
                fields,
                in_request_span: line[..at].contains(&request_span),
            })
        })
        .collect()
}

fn count(lines: &[BodyLine], message: &str) -> usize {
    lines.iter().filter(|l| l.message == message).count()
}

fn fields_of<'a>(lines: &'a [BodyLine], message: &str) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|l| l.message == message)
        .map(|l| l.fields.as_str())
        .collect()
}

/// The upstream's and the client's lines for one request: what each body
/// line says about its emitter, in order.
struct StreamTrace {
    upstream: Vec<String>,
    egress: Vec<String>,
    outgoing: Vec<String>,
    outside_request_span: usize,
}

fn stream_trace(log: &str) -> StreamTrace {
    let lines = body_lines(log);
    let owned = |message| {
        fields_of(&lines, message)
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    StreamTrace {
        upstream: owned(UPSTREAM_MSG),
        egress: owned(EGRESS_MSG),
        outgoing: owned(OUTGOING_MSG),
        outside_request_span: lines.iter().filter(|l| !l.in_request_span).count(),
    }
}

fn ingress_id(dialect: Dialect) -> &'static str {
    use crate::ingress::IngressAdapter;
    match dialect {
        Dialect::Anthropic => crate::ingress::anthropic::AnthropicIngress.id(),
        Dialect::Chat => crate::ingress::openai::OpenAiIngress.id(),
        Dialect::Responses => crate::ingress::openai_responses::ResponsesIngress.id(),
    }
}

/// One checked request on `dialect`'s fidelity lane, traced.
fn traced_request(
    dialect: Dialect,
    stream: bool,
) -> (Vec<(&'static str, CheckOutcome, u64)>, String) {
    traced(|| async move {
        let server = upstream(dialect, stream, None).await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(dialect, &server.uri(), CHECK_EVERY).await,
        ));
        send(&swap, dialect, stream).await;
        outcomes(&swap.load().wire_check_snapshot())
    })
}

#[test]
fn a_checked_anthropic_stream_traces_each_document_once_in_its_request_span() {
    let (recorded, log) = traced_request(Dialect::Anthropic, true);

    let lines = body_lines(&log);
    assert_eq!(
        recorded,
        [(Dialect::Anthropic.lane(), CheckOutcome::Clean, 2)],
        "both legs checked"
    );
    assert_eq!(count(&lines, UPSTREAM_MSG), 1, "{log}");
    assert_eq!(count(&lines, EGRESS_MSG), 1, "{log}");
    assert!(
        lines.iter().all(|l| l.in_request_span),
        "every body line carries request_id={REQUEST_ID}: {lines:?}"
    );
}

/// An Invoke event-stream frame wrapping one Anthropic stream event.
fn invoke_chunk(event: &serde_json::Value) -> Vec<u8> {
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    let payload = serde_json::json!({"bytes": B64.encode(event.to_string())}).to_string();
    let message = Message::new(payload.into_bytes())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String("chunk".to_string().into()),
        ));
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut buf).expect("encode frame");
    buf
}

/// A whole Anthropic answer as Bedrock InvokeModel streams it.
fn invoke_stream() -> Vec<u8> {
    use serde_json::json;
    [
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "upstream",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .flat_map(invoke_chunk)
    .collect()
}

const BEDROCK_MODEL_ID: &str = "us.anthropic.claude-sonnet-4-5";
const BEDROCK_REGION: &str = "us-east-1";

fn bedrock_key() -> String {
    ["trace", "test", "key"].join("-")
}

/// A router whose one target is a real Bedrock InvokeModel provider aimed
/// at `origin`.
async fn bedrock_invoke_router(origin: &str) -> Router {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.p]\n\
         kind = \"bedrock\"\n\
         region = \"{BEDROCK_REGION}\"\n\
         api_shape = \"invoke\"\n\
         creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n\n\
         [models.m]\n\
         provider = \"p\"\n\
         upstream = \"{BEDROCK_MODEL_ID}\"\n\n\
         [aliases]\n\
         a = \"m\"\n\n\
         [retry]\n\
         max_attempts = 1\n",
        key_ref = crate::test_secret::file_ref(&bedrock_key()),
    );
    let config = parse_config(&text).expect("valid bedrock test config");
    let creds = BedrockCreds::BearerKey { key: bedrock_key() };
    let cfg = BedrockConfig {
        id: "bedrock:p".into(),
        region: BEDROCK_REGION.into(),
        model_id: BEDROCK_MODEL_ID.into(),
        api_shape: BedrockApiShape::Invoke,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, BEDROCK_REGION)
        .await
        .expect("bearer creds resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(origin);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m".to_string(),
        Arc::new(ResolvedModel::new(
            "m",
            "p",
            Arc::new(provider),
            BEDROCK_MODEL_ID,
        )),
    )]));
    router
}

fn traced_bedrock_invoke_stream() -> (Vec<(&'static str, CheckOutcome, u64)>, String) {
    traced(|| async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(invoke_stream(), "application/vnd.amazon.eventstream"),
            )
            .mount(&server)
            .await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            bedrock_invoke_router(&server.uri()).await,
        ));
        send(&swap, Dialect::Anthropic, true).await;
        outcomes(&swap.load().wire_check_snapshot())
    })
}

/// The upstream line names its emitter exactly as the attempt's outgoing
/// line does, in the providers' own vocabulary, one row per fidelity
/// egress; the egress line names the ingress adapter.
#[test]
fn a_traced_stream_document_names_the_provider_as_its_outgoing_request_does() {
    type Run = fn() -> (Vec<(&'static str, CheckOutcome, u64)>, String);
    let rows: [(&str, &str, Dialect, Run); 4] = [
        (
            "anthropic",
            "anthropic->anthropic-api",
            Dialect::Anthropic,
            || traced_request(Dialect::Anthropic, true),
        ),
        (
            "openai-compat",
            "chat->openai-compat",
            Dialect::Chat,
            || traced_request(Dialect::Chat, true),
        ),
        (
            "openai-responses",
            "responses->openai-responses",
            Dialect::Responses,
            || traced_request(Dialect::Responses, true),
        ),
        (
            "bedrock-invoke",
            "anthropic->bedrock-invoke",
            Dialect::Anthropic,
            traced_bedrock_invoke_stream,
        ),
    ];
    for (token, lane, dialect, run) in rows {
        let (recorded, log) = run();

        let trace = stream_trace(&log);
        assert_eq!(recorded, [(lane, CheckOutcome::Clean, 2)], "{token}: {log}");
        assert_eq!(trace.outgoing.len(), 1, "{token}: {log}");
        assert_eq!(trace.upstream, trace.outgoing, "{token}");
        assert!(
            trace.upstream[0].starts_with(&format!("provider_kind=\"{token}\" ")),
            "{token}: {:?}",
            trace.upstream
        );
        assert_eq!(
            trace.egress,
            [format!("ingress=\"{}\"", ingress_id(dialect))],
            "{token}"
        );
        assert_eq!(trace.outside_request_span, 0, "{token}: {log}");
    }
}

/// A non-stream answer is traced by its provider and its renderer; the
/// check adds no second copy.
#[test]
fn a_checked_complete_response_is_still_traced_exactly_once() {
    for dialect in DIALECTS {
        let (recorded, log) = traced_request(dialect, false);

        let lines = body_lines(&log);
        assert_eq!(
            recorded,
            [(dialect.lane(), CheckOutcome::Clean, 2)],
            "{dialect:?}"
        );
        assert_eq!(count(&lines, UPSTREAM_MSG), 1, "{dialect:?}: {log}");
        assert_eq!(count(&lines, EGRESS_MSG), 1, "{dialect:?}: {log}");
    }
}

/// An upstream stream that stops after its first text delta.
fn traced_upstream_cut() -> (Vec<(&'static str, CheckOutcome, u64)>, String) {
    traced(|| async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_unfinished_stream()),
            )
            .mount(&server)
            .await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(Dialect::Anthropic, &server.uri(), CHECK_EVERY).await,
        ));
        let client = send(&swap, Dialect::Anthropic, true).await;
        assert!(client.contains("event: error"), "{client}");
        outcomes(&swap.load().wire_check_snapshot())
    })
}

/// A client that takes the first frame of a held-open stream and leaves.
fn traced_client_cancel() -> (Vec<(&'static str, CheckOutcome, u64)>, String) {
    use futures::StreamExt;
    traced(|| async {
        let upstream = HangingUpstream::start().await;
        let swap: RouterSwap = Arc::new(arc_swap::ArcSwap::from_pointee(
            router(Dialect::Anthropic, &upstream.base, CHECK_EVERY).await,
        ));
        let resp = app(&swap)
            .oneshot(post_request(Dialect::Anthropic, true))
            .await
            .expect("infallible router");
        let mut frames = resp.into_body().into_data_stream();
        let first = frames.next().await.expect("a first frame").expect("bytes");
        assert!(!first.is_empty());
        drop(frames);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let recorded = outcomes(&swap.load().wire_check_snapshot());
                if recorded.len() > 1 {
                    return recorded;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the response leg records once the client leaves")
    })
}

/// A stream either side of which never finished traces no document: a
/// partial one would land in a fixture as if it were the whole answer.
#[test]
fn an_unfinished_stream_traces_neither_document() {
    type Run = fn() -> (Vec<(&'static str, CheckOutcome, u64)>, String);
    let rows: [(&str, Run); 2] = [
        ("upstream cut mid-stream", traced_upstream_cut),
        ("client left mid-stream", traced_client_cancel),
    ];
    for (name, run) in rows {
        let (recorded, log) = run();

        let lines = body_lines(&log);
        assert_eq!(
            recorded,
            [
                (Dialect::Anthropic.lane(), CheckOutcome::Clean, 1),
                (Dialect::Anthropic.lane(), CheckOutcome::Incomplete, 1),
            ],
            "{name}"
        );
        assert_eq!(
            count(&lines, OUTGOING_MSG),
            1,
            "{name}: the trace was captured: {log}"
        );
        assert_eq!(count(&lines, UPSTREAM_MSG), 0, "{name}: {log}");
        assert_eq!(count(&lines, EGRESS_MSG), 0, "{name}: {log}");
    }
}
