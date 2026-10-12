//! The response-leg carrier: an admitted attempt on a FIDELITY lane rides
//! its tap out on `DispatchMeta::response_check`, the router declares the
//! response edits it makes itself, and the check records the comparison
//! with leg `response`. A test provider stands in for the egress deposit.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::wire_check::{
    Assembled, IncompleteReason, IngressDialect, IngressWire, Leg, SseFrame, StreamExit,
};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
};
use serde_json::{Value, json};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{Router, RouterOptions};
use crate::wire_check_state::{CheckOutcome, WireCheckSnapshot};

const ALIAS: &str = "a";
const UPSTREAM_BODY: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-x","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

/// A provider that answers like an Anthropic egress with a response tap:
/// the complete path deposits the upstream body, the stream path folds the
/// upstream's frames.
struct AnsweringProvider;

fn upstream_frames() -> [(&'static str, String); 5] {
    [
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-x",
                "content": [], "stop_reason": null, "usage": {"input_tokens": 1}
            }})
            .to_string(),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}})
            .to_string(),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "ok"}})
            .to_string(),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 1}})
            .to_string(),
        ),
        ("message_stop", json!({"type": "message_stop"}).to_string()),
    ]
}

#[async_trait::async_trait]
impl Provider for AnsweringProvider {
    fn id(&self) -> &'static str {
        "answering"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("answering", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
            tap.deposit(
                req.routectl_internal
                    .ingress_wire
                    .as_ref()
                    .map(|w| w.bytes.clone())
                    .unwrap_or_default(),
            );
            if tap.checks_response() {
                tap.deposit_response(UPSTREAM_BODY.into());
            }
        }
        Ok(serde_json::from_value(json!({
            "id": "msg_1", "model": "claude-x", "created": 0,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}]
        }))
        .expect("canonical response"))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
            tap.deposit(
                req.routectl_internal
                    .ingress_wire
                    .as_ref()
                    .map(|w| w.bytes.clone())
                    .unwrap_or_default(),
            );
            for (event, data) in upstream_frames() {
                tap.push_response_frame(SseFrame::new(Some(event), &data));
            }
        }
        let chunk = ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(stream::iter([Ok(chunk)]).boxed())
    }
}

/// A router over one anthropic-api target (`kind` picks the egress) whose
/// sampler admits per `wire_check`.
fn router(kind: &str, wire_check: &str) -> Router {
    let toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [wire_check]\n{wire_check}\n[providers.p0]\nkind = \"{kind}\"\n\
         api_key_ref = \"literal:k\"\n[aliases]\n{ALIAS} = [\"m0\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new("m0", "p0", Arc::new(AnsweringProvider), "upstream");
    router.install_resolved_models(BTreeMap::from([("m0".to_string(), Arc::new(model))]));
    router
}

const CHECK_EVERY: &str = "first_n = 8\nsample_one_in = 1";
const CHECK_NONE: &str = "first_n = 0\nsample_one_in = 1000";

fn request(dialect: IngressDialect, stream: bool) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        stream: stream.then_some(true),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: br#"{"model":"a","messages":[]}"#.to_vec().into(),
    });
    req
}

fn response_outcomes(snapshot: &WireCheckSnapshot) -> Vec<(CheckOutcome, u64)> {
    snapshot
        .checks
        .iter()
        .map(|c| (c.outcome, c.reading.count))
        .collect()
}

/// The client-side document as the Anthropic renderer would build it from
/// the upstream answer, with routectl's alias label in place of the model.
fn client_body() -> Value {
    let mut body: Value = serde_json::from_str(UPSTREAM_BODY).expect("json");
    body["model"] = json!(ALIAS);
    body
}

#[tokio::test]
async fn an_admitted_fidelity_attempt_carries_its_check_and_records_a_clean_response() {
    let router = router("anthropic-api", CHECK_EVERY);

    let dispatched = Box::pin(router.complete_with_options(
        request(IngressDialect::Anthropic, false),
        RouterOptions::default(),
    ))
    .await;
    let check = dispatched
        .meta
        .response_check
        .expect("an admitted fidelity attempt carries one");
    let mut body = client_body();
    body["routectl_provider"] = json!("p0");
    check.compare_body(&serde_json::to_vec(&body).expect("json"), &[]);

    let snapshot = router.wire_check_snapshot();
    assert_eq!(response_outcomes(&snapshot), [(CheckOutcome::Clean, 2)]);
    assert!(snapshot.findings.is_empty(), "{:?}", snapshot.findings);
}

#[tokio::test]
async fn a_dropped_upstream_key_is_counted_on_the_response_leg_named_by_path() {
    let router = router("anthropic-api", CHECK_EVERY);

    let dispatched = Box::pin(router.complete_with_options(
        request(IngressDialect::Anthropic, false),
        RouterOptions::default(),
    ))
    .await;
    let mut body = client_body();
    body.as_object_mut().expect("object").remove("stop_reason");
    let check = dispatched.meta.response_check.expect("carried");
    check.compare_body(&serde_json::to_vec(&body).expect("json"), &[]);

    let snapshot = router.wire_check_snapshot();
    assert_eq!(snapshot.loss.len(), 1);
    assert_eq!(snapshot.loss[0].path_class.label(), "missing");
    let finding = snapshot.findings.first().expect("one finding");
    assert_eq!(finding.leg, Leg::Response);
    assert_eq!(finding.path.as_str(), "/stop_reason");
}

#[tokio::test]
async fn the_router_declares_its_model_relabel_and_provider_attribution() {
    let rows: [(&str, Option<&str>); 2] = [
        ("relabel without attribution", None),
        ("relabel with attribution", Some("p0")),
    ];
    for (name, provider) in rows {
        let router = router("anthropic-api", CHECK_EVERY);
        let dispatched = Box::pin(router.complete_with_options(
            request(IngressDialect::Anthropic, false),
            RouterOptions::default(),
        ))
        .await;
        let mut body = client_body();
        if let Some(provider) = provider {
            body["routectl_provider"] = json!(provider);
        }
        let check = dispatched.meta.response_check.expect("carried");
        check.compare_body(&serde_json::to_vec(&body).expect("json"), &[]);

        assert!(
            router.wire_check_snapshot().findings.is_empty(),
            "{name}: {:?}",
            router.wire_check_snapshot().findings
        );
    }
}

/// A complete body over the inline-compare limit on either side records
/// `budget` and is never parsed: each oversize body here is not JSON, so a
/// parse would record `incomplete` instead.
#[tokio::test]
async fn an_oversize_complete_body_records_budget_unparsed() {
    let oversize = vec![b'x'; super::MAX_INLINE_COMPARE_BYTES + 1];
    let rows: [(&str, bool, bool); 2] = [
        ("client body over the limit", true, false),
        ("upstream body over the limit", false, true),
    ];
    for (name, client_over, upstream_over) in rows {
        let router = router("anthropic-api", CHECK_EVERY);
        let dispatched = Box::pin(router.complete_with_options(
            request(IngressDialect::Anthropic, false),
            RouterOptions::default(),
        ))
        .await;
        let check = dispatched.meta.response_check.expect("carried");
        if upstream_over {
            check.tap.deposit_response(oversize.clone().into());
        }
        let client = if client_over {
            oversize.clone()
        } else {
            serde_json::to_vec(&client_body()).expect("json")
        };

        check.compare_body(&client, &[]);

        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            response_outcomes(&snapshot),
            [(CheckOutcome::Budget, 1), (CheckOutcome::Clean, 1)],
            "{name}"
        );
        assert!(snapshot.findings.is_empty(), "{name}");
    }
}

/// A body dropped unread is cut on the client side, so the check records
/// `incomplete` naming the client without parsing the upstream deposit:
/// the deposit here is oversize and not JSON, so a parse would name the
/// upstream side as unreadable instead.
#[tokio::test]
async fn an_unread_drop_records_incomplete_without_parsing_the_upstream_body() {
    let router = router("anthropic-api", CHECK_EVERY);
    let dispatched = Box::pin(router.complete_with_options(
        request(IngressDialect::Anthropic, false),
        RouterOptions::default(),
    ))
    .await;
    let check = dispatched.meta.response_check.expect("carried");
    check
        .tap
        .deposit_response(vec![b'x'; super::MAX_INLINE_COMPARE_BYTES + 1].into());

    let events = routectl_testkit::capture_events(|| {
        check.compare_stream(
            Assembled::Incomplete(IncompleteReason::Cancelled),
            StreamExit::Cancelled,
            &[],
        );
    });

    assert_eq!(
        response_outcomes(&router.wire_check_snapshot()),
        [(CheckOutcome::Clean, 1), (CheckOutcome::Incomplete, 1)]
    );
    let sides: Vec<(Option<&str>, Option<&str>)> = events
        .iter()
        .filter(|e| e.message == "wire check had no complete response to compare")
        .map(|e| (e.field("side"), e.field("reason")))
        .collect();
    assert_eq!(sides, [(Some("client"), Some("cancelled"))]);
}

#[tokio::test]
async fn a_streamed_response_compares_assembled_documents_and_an_unfinished_side_is_incomplete() {
    // The request leg's own check records `clean` once on the same lane, so
    // each row expects that plus the response leg's outcome.
    type Row = (
        &'static str,
        StreamExit,
        Option<IncompleteReason>,
        &'static [(CheckOutcome, u64)],
    );
    let rows: [Row; 3] = [
        (
            "whole stream",
            StreamExit::Ended,
            None,
            &[(CheckOutcome::Clean, 2)],
        ),
        (
            "client went away",
            StreamExit::Disconnect,
            Some(IncompleteReason::Disconnect),
            &[(CheckOutcome::Clean, 1), (CheckOutcome::Incomplete, 1)],
        ),
        (
            "upstream failed mid-stream",
            StreamExit::UpstreamError,
            Some(IncompleteReason::UpstreamError),
            &[(CheckOutcome::Clean, 1), (CheckOutcome::Incomplete, 1)],
        ),
    ];
    for (name, exit, client_reason, expected) in rows {
        let router = router("anthropic-api", CHECK_EVERY);
        let dispatched = Box::pin(router.stream_with_options(
            request(IngressDialect::Anthropic, true),
            RouterOptions::default(),
        ))
        .await;
        let _chunks: Vec<_> = dispatched.result.expect("stream opens").collect().await;
        let client =
            client_reason.map_or(Assembled::Complete(client_body()), Assembled::Incomplete);
        let check = dispatched.meta.response_check.expect("carried");

        check.compare_stream(client, exit, &[]);

        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            response_outcomes(&snapshot),
            expected,
            "{name}: {:?}",
            snapshot.findings
        );
        assert!(
            snapshot.findings.is_empty(),
            "{name}: {:?}",
            snapshot.findings
        );
    }
}

#[tokio::test]
async fn no_response_check_rides_out_of_an_unchecked_attempt() {
    let rows = [
        (
            "unadmitted success",
            "anthropic-api",
            CHECK_NONE,
            IngressDialect::Anthropic,
        ),
        (
            "admitted translation lane",
            "anthropic-api",
            CHECK_EVERY,
            IngressDialect::Chat,
        ),
    ];
    for (name, kind, wire_check, dialect) in rows {
        for stream in [false, true] {
            let router = router(kind, wire_check);
            let meta = if stream {
                Box::pin(
                    router.stream_with_options(request(dialect, true), RouterOptions::default()),
                )
                .await
                .meta
            } else {
                Box::pin(
                    router.complete_with_options(request(dialect, false), RouterOptions::default()),
                )
                .await
                .meta
            };

            assert!(meta.response_check.is_none(), "{name} stream={stream}");
        }
    }
}

/// The body-trace lines one finished stream compare emitted, as
/// (message, provider_kind or ingress, provider).
fn body_traces(events: &[routectl_testkit::CapturedEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|e| e.message.ends_with(" body"))
        .map(|e| {
            (
                e.message.clone(),
                e.field("provider_kind")
                    .or_else(|| e.field("ingress"))
                    .unwrap_or_default()
                    .to_string(),
                e.field("provider").unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// A traced stream compare emits each whole document once, the upstream's
/// under the serving provider's own id and its trace token; the untraced
/// compare and an unfinished side emit nothing.
#[tokio::test]
async fn a_traced_stream_compare_emits_both_documents_only_when_both_are_whole() {
    let both = vec![
        (
            "upstream success body".to_string(),
            "anthropic".to_string(),
            "answering".to_string(),
        ),
        (
            "egress response body".to_string(),
            "anthropic".to_string(),
            String::new(),
        ),
    ];
    let rows: [(&str, bool, StreamExit, Option<IncompleteReason>, Vec<_>); 4] = [
        ("whole stream, traced", true, StreamExit::Ended, None, both),
        (
            "whole stream, untraced",
            false,
            StreamExit::Ended,
            None,
            vec![],
        ),
        (
            "client went away",
            true,
            StreamExit::Disconnect,
            Some(IncompleteReason::Disconnect),
            vec![],
        ),
        (
            "upstream failed mid-stream",
            true,
            StreamExit::UpstreamError,
            Some(IncompleteReason::UpstreamError),
            vec![],
        ),
    ];
    for (name, traced, exit, client_reason, expected) in rows {
        let router = router("anthropic-api", CHECK_EVERY);
        let dispatched = Box::pin(router.stream_with_options(
            request(IngressDialect::Anthropic, true),
            RouterOptions::default(),
        ))
        .await;
        let _chunks: Vec<_> = dispatched.result.expect("stream opens").collect().await;
        let client =
            client_reason.map_or(Assembled::Complete(client_body()), Assembled::Incomplete);
        let check = dispatched.meta.response_check.expect("carried");

        let events = routectl_testkit::capture_events(|| {
            if traced {
                check.compare_traced_stream(client, exit, &[], "anthropic");
            } else {
                check.compare_stream(client, exit, &[]);
            }
        });

        assert_eq!(body_traces(&events), expected, "{name}");
    }
}

/// Each egress kind traces under the token its provider spells its own
/// request lines with; the config kind and the Mantle suffix never appear.
#[cfg(feature = "bedrock")]
#[test]
fn each_egress_traces_under_its_providers_own_token() {
    use routectl_core::wire_check::EgressKind;
    use routectl_providers::bedrock::BedrockApiShape;
    let rows = [
        (EgressKind::AnthropicApi, Some("anthropic")),
        (EgressKind::AnthropicApiMantle, Some("anthropic")),
        (
            EgressKind::BedrockInvoke,
            Some(BedrockApiShape::Invoke.provider_kind_str()),
        ),
        (
            EgressKind::BedrockConverse,
            Some(BedrockApiShape::Converse.provider_kind_str()),
        ),
        (EgressKind::OpenaiCompat, Some("openai-compat")),
        (EgressKind::OpenaiCompatMantle, Some("openai-compat")),
        (EgressKind::OpenaiResponses, Some("openai-responses")),
        (EgressKind::OpenaiResponsesMantle, Some("openai-responses")),
        (EgressKind::Gemini, Some("gemini")),
        (EgressKind::Local, None),
    ];
    assert_eq!(rows.len(), EgressKind::ALL.len());
    for (egress, token) in rows {
        assert_eq!(
            super::provider_trace_kind(egress),
            token,
            "{}",
            egress.label()
        );
    }
}
