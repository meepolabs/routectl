//! Shared rig for the persisted-opening-diagnostics tests: routers and
//! daemons over the scripted loopback upstream in `opening_rig`, each with a
//! real usage ledger, a stream-gate driver for parser output the loopback
//! endpoint cannot produce, and readers for the client-visible frames.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use futures::StreamExt;
use routectl_core::ChatChunk;
use routectl_providers::anthropic_api::sse::SseState;
use routectl_router::{DispatchMeta, DispatchedStream};
use serde_json::Value;

use super::opening_rig::*;
use crate::handlers::opening_meter::{OpeningMeter, StreamTurn};
use crate::handlers::usage_capture::{UsageCapture, build_usage_draft};
use crate::ingress::StreamRequestContext;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::anthropic::context_anchor::ContextAnchorStore;
use crate::server::confirmation_advance::ConfirmationTracker;

/// The cache-inclusive input a rendered `message_start` shows: its
/// `input_tokens` plus the disjoint write and read fields.
pub(super) fn frame_input(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|field| usage[field].as_u64().unwrap_or(0))
    .sum()
}

/// The per-TTL write breakdown of a rendered usage object.
pub(super) fn frame_ttl_breakdown(usage: &Value) -> u64 {
    ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"]
        .iter()
        .map(|field| usage["cache_creation"][field].as_u64().unwrap_or(0))
        .sum()
}

pub(super) async fn anthropic_router(upstream: &Upstream) -> routectl_router::Router {
    build(config(
        vec![("anth", anthropic_provider(upstream.base(), "/anth"))],
        &[("opus", "anth", "claude-opus-4-7")],
        &[("claude-opus", &["opus"])],
    ))
    .await
}

pub(super) async fn anthropic_ledger_daemon(
    upstream: &Upstream,
) -> (Arc<crate::server::AppState>, Ledger) {
    ledger_daemon(anthropic_router(upstream).await)
}

pub(super) async fn translated_ledger_daemon(
    upstream: &Upstream,
) -> (Arc<crate::server::AppState>, Ledger) {
    ledger_daemon(build(translated_config(upstream.base(), "glm-4.6")).await)
}

pub(super) async fn turn_as(state: &Arc<crate::server::AppState>, body: &Value, id: &str) -> Turn {
    let turn = send_as(
        state,
        AnthropicIngress,
        session_headers(Some(SESSION)),
        body,
        Some(id),
    )
    .await;
    assert_eq!(turn.status, StatusCode::OK, "turn failed: {}", turn.raw);
    turn
}

/// A metered turn over `turn_one` and its capture guard persisting into
/// `ledger` under `id`.
pub(super) fn metered_turn(
    router: &routectl_router::Router,
    ledger: &Ledger,
    id: &str,
) -> (StreamTurn, UsageCapture) {
    let req = canonical(&body(&turn_one()));
    let store = ContextAnchorStore::new();
    let meter = OpeningMeter::admit(&store, router, &req);
    let turn = StreamTurn {
        session_key: req.routectl_internal.inbound_session_key.clone(),
        ctx: StreamRequestContext {
            input_tokens_estimate: meter.raw_tokens(),
            model: req.model.clone(),
        },
        meter: Some(meter),
    };
    let capture = UsageCapture::new(
        build_usage_draft("anthropic", &req, id.to_string()),
        ledger.handle(),
        "anthropic".to_string(),
    );
    (turn, capture)
}

/// Drive `chunks` through the stream gate as an already-resolved dispatch
/// served by the `opus` lane, persisting into `ledger` under `id`.
pub(super) async fn gate_turn(
    router: routectl_router::Router,
    chunks: Vec<ChatChunk>,
    dispatch_delay: Duration,
    ledger: &Ledger,
    id: &str,
) -> Vec<(Option<String>, Value)> {
    let items = chunks.into_iter().map(Ok).collect();
    let resp = gate_response(router, items, dispatch_delay, ledger, id).await;
    frames_of(resp).await
}

/// The stream gate's response for a dispatch resolving after
/// `dispatch_delay` with `items`, served by the `opus` lane.
pub(super) async fn gate_response(
    router: routectl_router::Router,
    items: Vec<routectl_core::Result<ChatChunk>>,
    dispatch_delay: Duration,
    ledger: &Ledger,
    id: &str,
) -> axum::response::Response {
    let stream = futures::stream::iter(items).boxed();
    gate_stream_response(router, stream, dispatch_delay, ledger, id).await
}

pub(super) async fn gate_stream_response(
    router: routectl_router::Router,
    stream: futures::stream::BoxStream<'static, routectl_core::Result<ChatChunk>>,
    dispatch_delay: Duration,
    ledger: &Ledger,
    id: &str,
) -> axum::response::Response {
    let router = Arc::new(router);
    let (turn, capture) = metered_turn(&router, ledger, id);
    let mut meta = DispatchMeta::for_alias("claude-opus");
    meta.attempt_count = 1;
    meta.served_provider = Some("anth".into());
    meta.served_provider_kind = Some("anthropic-api".into());
    meta.served_model = Some("opus".into());
    meta.served_upstream = Some("claude-opus-4-7".into());
    let fut = Box::pin(async move {
        if !dispatch_delay.is_zero() {
            tokio::time::sleep(dispatch_delay).await;
        }
        DispatchedStream {
            meta,
            result: Ok(stream),
        }
    });
    super::stream_dispatch_gated(
        fut,
        AnthropicIngress,
        capture,
        Arc::clone(&router),
        Arc::new(ConfirmationTracker::new()),
        turn,
    )
    .await
}

pub(super) async fn frames_of(resp: axum::response::Response) -> Vec<(Option<String>, Value)> {
    let text = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8");
    text.split("\n\n")
        .filter_map(|block| {
            let event = block
                .lines()
                .find_map(|l| l.strip_prefix("event:"))
                .map(|v| v.trim().to_string());
            let data = block.lines().find_map(|l| l.strip_prefix("data:"))?;
            Some((
                event,
                serde_json::from_str(data.trim()).expect("json frame"),
            ))
        })
        .collect()
}

/// Parse an Anthropic SSE body with the real parser.
pub(super) fn parse(stream: &str, state: &mut SseState) -> Vec<ChatChunk> {
    stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| state.parse_event("test", data).expect("parses"))
        .collect()
}

pub(super) fn opening_usage_of(frames: &[(Option<String>, Value)]) -> &Value {
    let starts: Vec<_> = frames
        .iter()
        .filter(|(event, _)| event.as_deref() == Some("message_start"))
        .collect();
    assert_eq!(starts.len(), 1, "{frames:?}");
    &starts[0].1["message"]["usage"]
}

/// A dispatch that never resolves, as a warm-path turn waiting on it.
pub(super) fn pending_dispatch() -> super::DispatchFut {
    Box::pin(std::future::pending())
}
pub(super) fn upstream_error() -> routectl_core::Error {
    routectl_core::Error::Streaming("upstream stream failed".into())
}
