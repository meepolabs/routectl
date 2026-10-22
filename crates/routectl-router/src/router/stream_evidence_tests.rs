//! Coverage for the streamed response-evidence fold and its carrier: the fold
//! hands the detectors what a complete response for the same reply would, it
//! spoils on a shape it cannot represent, the natural end is recorded only
//! when the inner stream ends with no error, and the carrier settles at most
//! once across clones and writes nothing when dropped unsettled.

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use routectl_core::capability::STRUCTURED_OUTPUT;
use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, ContentPart, Error,
    KnownContentPart, Message, MessageContent, OpaqueSseEvent, Provider, ReasoningDetail,
    ReasoningDetailKind, Result, Role, Usage, UsageDelta,
};
use serde_json::json;

use super::{EvidenceFold, STREAM_EVIDENCE_BYTE_CAP};
use crate::capability_detect::{
    CapabilityObservation, DetectorContext, ObservationDirection, detect,
};
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{EvidencePolicy, Router, RouterOptions};

// --- chunk and response builders ----------------------------------------

fn chunk(delta: ChunkDelta, finish_reason: Option<&str>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish_reason.map(str::to_string),
            matched_stop_sequence: None,
        }],
        ..ChatChunk::default()
    }
}

fn text_chunk(text: &str) -> ChatChunk {
    chunk(
        ChunkDelta {
            content: Some(text.into()),
            ..ChunkDelta::default()
        },
        None,
    )
}

fn stop_chunk(reason: &str) -> ChatChunk {
    chunk(ChunkDelta::default(), Some(reason))
}

fn usage_chunk(usage: UsageDelta) -> ChatChunk {
    ChatChunk {
        usage: Some(usage),
        ..ChatChunk::default()
    }
}

fn message(content: MessageContent) -> Message {
    Message {
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn response(message: Message, finish_reason: &str, usage: Option<Usage>) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some(finish_reason.into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage,
        ..ChatResponse::default()
    }
}

fn reasoning_detail() -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("r0".into()),
        format: Some("anthropic-claude-v1".into()),
        index: Some(0),
        payload: json!({"text": "thinking", "signature": "sig"}),
    }
}

fn fold_all(keeps_text: bool, chunks: &[ChatChunk]) -> Option<ChatResponse> {
    let mut fold = EvidenceFold::new(keeps_text);
    for c in chunks {
        fold.absorb(c);
    }
    fold.finish()
}

fn strict_ctx(required: &[&str]) -> DetectorContext {
    DetectorContext {
        strict_output_requested: true,
        requested_schema_required_keys: required.iter().map(|k| (*k).to_string()).collect(),
        ..DetectorContext::default()
    }
}

// --- fold parity with the complete path ------------------------------------

struct ParityRow {
    name: &'static str,
    ctx: DetectorContext,
    chunks: Vec<ChatChunk>,
    complete: ChatResponse,
}

fn parity_rows() -> Vec<ParityRow> {
    vec![
        ParityRow {
            name: "strict output split across chunks",
            ctx: strict_ctx(&["name"]),
            chunks: vec![
                text_chunk(r#"{"na"#),
                text_chunk(r#"me":"ok"}"#),
                stop_chunk("stop"),
            ],
            complete: response(
                message(MessageContent::Text(r#"{"name":"ok"}"#.into())),
                "stop",
                None,
            ),
        },
        ParityRow {
            name: "strict output answered in prose",
            ctx: strict_ctx(&["name"]),
            chunks: vec![text_chunk("not json"), stop_chunk("stop")],
            complete: response(
                message(MessageContent::Text("not json".into())),
                "stop",
                None,
            ),
        },
        ParityRow {
            name: "strict output carried by a tool call",
            ctx: strict_ctx(&["q"]),
            chunks: vec![
                chunk(
                    ChunkDelta {
                        tool_calls: Some(vec![json!({"index": 0, "id": "t1"})]),
                        ..ChunkDelta::default()
                    },
                    None,
                ),
                stop_chunk("tool_calls"),
            ],
            complete: response(
                Message {
                    tool_calls: Some(vec![json!({"id": "t1", "type": "function"})]),
                    ..message(MessageContent::Null)
                },
                "tool_calls",
                None,
            ),
        },
        ParityRow {
            name: "web search counted on the usage",
            ctx: DetectorContext::default(),
            chunks: vec![
                text_chunk("found it"),
                stop_chunk("stop"),
                usage_chunk(UsageDelta {
                    server_tool_use: Some(json!({"web_search_requests": 2})),
                    ..UsageDelta::default()
                }),
            ],
            complete: response(
                message(MessageContent::Text("found it".into())),
                "stop",
                Some(Usage {
                    server_tool_use: Some(json!({"web_search_requests": 2})),
                    ..Usage::default()
                }),
            ),
        },
        ParityRow {
            name: "web search shown by a result block",
            ctx: DetectorContext::default(),
            chunks: vec![
                ChatChunk {
                    opaque_events: vec![OpaqueSseEvent::ContentBlockStart {
                        upstream_index: 1,
                        type_tag: "web_search_tool_result".into(),
                        raw_data: br#"{"type":"web_search_tool_result"}"#.to_vec(),
                    }],
                    ..ChatChunk::default()
                },
                text_chunk("found it"),
                stop_chunk("stop"),
            ],
            complete: response(
                message(MessageContent::Parts(vec![
                    ContentPart::Other {
                        type_tag: "web_search_tool_result".into(),
                        cache_control: None,
                        extras: serde_json::Map::new(),
                    },
                    ContentPart::Known(KnownContentPart::Text {
                        text: "found it".into(),
                        citations: None,
                        cache_control: None,
                    }),
                ])),
                "stop",
                None,
            ),
        },
        ParityRow {
            name: "forced web search with no evidence",
            ctx: DetectorContext {
                forced_web_search: true,
                ..DetectorContext::default()
            },
            chunks: vec![text_chunk("no search"), stop_chunk("stop")],
            complete: response(
                message(MessageContent::Text("no search".into())),
                "stop",
                None,
            ),
        },
        ParityRow {
            name: "prompt cache read and written",
            ctx: DetectorContext::default(),
            chunks: vec![
                text_chunk("hi"),
                usage_chunk(UsageDelta {
                    cache_read_input_tokens: Some(0),
                    ..UsageDelta::default()
                }),
                usage_chunk(UsageDelta {
                    prompt_tokens: Some(10),
                    cache_creation: Some(CacheCreation {
                        ephemeral_5m_input_tokens: Some(64),
                        ephemeral_1h_input_tokens: None,
                    }),
                    ..UsageDelta::default()
                }),
                stop_chunk("stop"),
            ],
            complete: response(
                message(MessageContent::Text("hi".into())),
                "stop",
                Some(Usage {
                    prompt_tokens: 10,
                    cache_read_input_tokens: Some(0),
                    cache_creation: Some(CacheCreation {
                        ephemeral_5m_input_tokens: Some(64),
                        ephemeral_1h_input_tokens: None,
                    }),
                    ..Usage::default()
                }),
            ),
        },
        ParityRow {
            name: "thinking blocks",
            ctx: DetectorContext {
                reasoning_requested: true,
                ..DetectorContext::default()
            },
            chunks: vec![
                chunk(
                    ChunkDelta {
                        reasoning_details: vec![reasoning_detail()],
                        ..ChunkDelta::default()
                    },
                    None,
                ),
                text_chunk("answer"),
                stop_chunk("stop"),
            ],
            complete: response(
                Message {
                    reasoning_details: vec![reasoning_detail()],
                    ..message(MessageContent::Text("answer".into()))
                },
                "stop",
                None,
            ),
        },
        ParityRow {
            name: "thinking tokens counted on the usage",
            ctx: DetectorContext {
                reasoning_requested: true,
                ..DetectorContext::default()
            },
            chunks: vec![
                text_chunk("answer"),
                stop_chunk("stop"),
                usage_chunk(UsageDelta {
                    reasoning_tokens: Some(12),
                    ..UsageDelta::default()
                }),
            ],
            complete: response(
                message(MessageContent::Text("answer".into())),
                "stop",
                Some(Usage {
                    reasoning_tokens: Some(12),
                    ..Usage::default()
                }),
            ),
        },
    ]
}

#[test]
fn a_clean_stream_reads_as_its_complete_response() {
    for row in parity_rows() {
        // Arrange
        let complete_obs = detect(&row.ctx, &row.complete);

        // Act
        let folded = fold_all(row.ctx.strict_output_requested, &row.chunks)
            .unwrap_or_else(|| panic!("{}: a clean single-choice stream folds", row.name));
        let stream_obs = detect(&row.ctx, &folded);

        // Assert
        assert_eq!(stream_obs, complete_obs, "{}", row.name);
    }
}

/// The parity rows are only evidence if they cover every detector on both
/// sides of the evidence: a row set where every detector stays silent passes
/// on a fold that keeps nothing.
#[test]
fn the_parity_rows_exercise_every_detector() {
    let observed: Vec<CapabilityObservation> = parity_rows()
        .iter()
        .flat_map(|row| detect(&row.ctx, &row.complete))
        .collect();
    for key in crate::capability_detect::DETECTABLE_CAPABILITIES {
        assert!(
            observed
                .iter()
                .any(|o| o.capability_key == key && o.direction == ObservationDirection::Verified),
            "no parity row verifies {key}"
        );
    }
    assert!(
        observed
            .iter()
            .any(|o| o.direction == ObservationDirection::SuspectAbsence),
        "no parity row yields a suspected absence"
    );
}

// --- fold rejections ---------------------------------------------------------

#[test]
fn a_second_choice_spoils_the_fold() {
    // Arrange
    let mut second = text_chunk("other");
    second.choices[0].index = 1;
    let rows = [
        (
            "one choice (positive control)",
            vec![text_chunk("a"), stop_chunk("stop")],
            true,
        ),
        (
            "two choices",
            vec![text_chunk("a"), second, stop_chunk("stop")],
            false,
        ),
    ];
    for (name, chunks, folds) in rows {
        // Act
        let folded = fold_all(false, &chunks);

        // Assert
        assert_eq!(folded.is_some(), folds, "{name}");
    }
}

/// A reply whose text arrives in two pieces, so the cap is crossed by the
/// running total rather than by one chunk, closed by `reason`.
fn split_text_reply(text: &str, reason: &str) -> Vec<ChatChunk> {
    let (head, tail) = text.split_at(text.len() / 2);
    vec![text_chunk(head), text_chunk(tail), stop_chunk(reason)]
}

/// A usage update whose `server_tool_use` serializes to exactly `bytes`
/// bytes: a JSON string of `bytes - 2` characters plus its quotes.
fn server_tool_use_chunk(bytes: usize) -> ChatChunk {
    usage_chunk(UsageDelta {
        server_tool_use: Some(json!("x".repeat(bytes - 2))),
        ..UsageDelta::default()
    })
}

#[test]
fn a_fold_past_the_byte_cap_spoils() {
    let cap = STREAM_EVIDENCE_BYTE_CAP;
    let half_reason = "r".repeat(cap / 2 + 1);
    // (name, chunks, the reply text the fold keeps, folds)
    let rows = [
        (
            "exactly at the cap (positive control)",
            split_text_reply(&"x".repeat(cap - "stop".len()), "stop"),
            "x".repeat(cap - "stop".len()),
            true,
        ),
        (
            "one byte past the cap",
            split_text_reply(&"x".repeat(cap - "stop".len() + 1), "stop"),
            String::new(),
            false,
        ),
        (
            "an oversized finish_reason",
            vec![text_chunk("ok"), stop_chunk(&"r".repeat(cap))],
            String::new(),
            false,
        ),
        (
            "a replaced finish_reason is charged once (positive control)",
            vec![stop_chunk(&half_reason), stop_chunk(&half_reason)],
            String::new(),
            true,
        ),
        (
            "an oversized server_tool_use",
            vec![
                text_chunk("ok"),
                server_tool_use_chunk(cap),
                stop_chunk("stop"),
            ],
            String::new(),
            false,
        ),
        (
            "a replaced server_tool_use is charged once (positive control)",
            vec![
                server_tool_use_chunk(cap - "stop".len()),
                server_tool_use_chunk(cap - "stop".len()),
                stop_chunk("stop"),
            ],
            String::new(),
            true,
        ),
    ];
    for (name, chunks, kept_text, folds) in rows {
        // Act
        let folded = fold_all(true, &chunks);

        // Assert
        assert_eq!(folded.is_some(), folds, "{name}");
        if let Some(resp) = folded
            && !kept_text.is_empty()
        {
            let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
                panic!("{name}: kept text folds into parts");
            };
            assert!(
                matches!(&parts[0], ContentPart::Known(KnownContentPart::Text { text: kept, .. }) if *kept == kept_text),
                "{name}: the whole reply is kept"
            );
        }
    }
}

#[test]
fn an_untracked_reply_never_spoils_on_size() {
    let long = "x".repeat(STREAM_EVIDENCE_BYTE_CAP + 1);

    let folded = fold_all(false, &[text_chunk(&long), stop_chunk("stop")]);

    assert!(folded.is_some());
}

#[test]
fn text_is_kept_only_when_strict_output_was_requested() {
    let chunks = [text_chunk("hello"), stop_chunk("stop")];

    let kept = fold_all(true, &chunks).expect("folds");
    let dropped = fold_all(false, &chunks).expect("folds");

    assert!(matches!(
        kept.choices[0].message.content,
        MessageContent::Parts(_)
    ));
    assert!(matches!(
        dropped.choices[0].message.content,
        MessageContent::Null
    ));
}

#[test]
fn a_rejecting_finish_reason_reaches_the_detectors() {
    // (name, chunks, finish_reason the folded response carries)
    let rows = [
        (
            "length",
            vec![text_chunk(r#"{"name":"ok"}"#), stop_chunk("length")],
            Some("length"),
        ),
        (
            "content_filter",
            vec![text_chunk(r#"{"name":"ok"}"#), stop_chunk("content_filter")],
            Some("content_filter"),
        ),
        (
            "no finish_reason at all",
            vec![text_chunk(r#"{"name":"ok"}"#)],
            None,
        ),
        (
            "the last finish_reason wins",
            vec![stop_chunk("stop"), stop_chunk("length")],
            Some("length"),
        ),
    ];
    let ctx = strict_ctx(&["name"]);
    for (name, chunks, want) in rows {
        // Act
        let folded = fold_all(true, &chunks).expect("a single choice folds");

        // Assert
        assert_eq!(
            folded.choices[0].finish_reason.as_deref(),
            want,
            "{name}: finish_reason preserved"
        );
        assert!(detect(&ctx, &folded).is_empty(), "{name}: the gate rejects");
    }
    // Positive control: the same reply with a clean stop is read.
    let clean =
        fold_all(true, &[text_chunk(r#"{"name":"ok"}"#), stop_chunk("stop")]).expect("folds");
    assert_eq!(detect(&ctx, &clean).len(), 1);
}

// --- the carrier through a real streaming dispatch --------------------------

/// What the scripted upstream does after its content chunks.
#[derive(Clone, Copy)]
enum Tail {
    /// End the stream.
    End,
    /// Yield an error, then end.
    Error,
}

struct ScriptedStream {
    chunks: Vec<ChatChunk>,
    tail: Tail,
}

#[async_trait::async_trait]
impl Provider for ScriptedStream {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p1", 500, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut items: Vec<Result<ChatChunk>> = self.chunks.iter().cloned().map(Ok).collect();
        if matches!(self.tail, Tail::Error) {
            items.push(Err(Error::upstream("p1", 502, "mid-stream failure")));
        }
        Ok(futures::stream::iter(items).boxed())
    }
}

const OPENAI_P1: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

const OPENAI_P1_DISABLED: &str = r#"
[capability]
enabled = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

const ANTHROPIC_P1: &str = r#"
[providers.p1]
kind = "anthropic-api"
base_url = "https://example.test"
api_key_ref = "literal:k"
"#;

const ANTHROPIC_P1_FORWARDED: &str = r#"
[providers.p1]
kind = "anthropic-api"
base_url = "https://example.test"
credential_source = "forwarded"
"#;

fn router_with(toml_text: &str, tail: Tail) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(ScriptedStream {
        chunks: vec![
            text_chunk(r#"{"na"#),
            text_chunk(r#"me":"ok"}"#),
            stop_chunk("stop"),
        ],
        tail,
    });
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// A strict-output request whose clean reply above yields one verified
/// observation.
fn strict_request() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": { "type": "object", "required": ["name"] }
                }
            }
        })),
        ..ChatRequest::default()
    }
}

const VERIFIED_ONLY: EvidencePolicy = EvidencePolicy {
    admit_suspect: false,
};

/// How the consumer leaves the stream.
#[derive(Clone, Copy, Debug)]
enum Consume {
    /// Poll until the stream returns `None`.
    ToEnd,
    /// Take the first item, then drop the stream.
    DropAfterFirst,
}

async fn dispatch_and_consume(
    router: &Router,
    req: ChatRequest,
    consume: Consume,
) -> crate::router::DispatchMeta {
    let dispatched = router
        .stream_with_options(req, RouterOptions::default())
        .await;
    let mut stream = dispatched.result.expect("the stream dispatch succeeds");
    match consume {
        Consume::ToEnd => while stream.next().await.is_some() {},
        Consume::DropAfterFirst => {
            assert!(stream.next().await.is_some(), "first content was buffered");
            drop(stream);
        }
    }
    dispatched.meta
}

#[tokio::test]
async fn only_a_natural_end_settles_evidence() {
    // (name, upstream tail, how the consumer leaves, ends naturally)
    let rows = [
        (
            "natural end (positive control)",
            Tail::End,
            Consume::ToEnd,
            true,
        ),
        (
            "consumer drop mid-stream",
            Tail::End,
            Consume::DropAfterFirst,
            false,
        ),
        ("error after content", Tail::Error, Consume::ToEnd, false),
    ];
    for (name, tail, consume, natural) in rows {
        // Arrange
        let router = router_with(OPENAI_P1, tail);
        let meta = dispatch_and_consume(&router, strict_request(), consume).await;
        let evidence = meta
            .stream_evidence
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: an own-credential stream carries evidence"));
        let mut events = Vec::new();

        // Act
        let read = evidence.settle(&router, VERIFIED_ONLY, &mut events);

        // Assert
        assert_eq!(evidence.ended_naturally(), natural, "{name}: natural end");
        assert_eq!(read, natural, "{name}: a response was read");
        let want = usize::from(natural);
        assert_eq!(events.len(), want, "{name}: events");
        if natural {
            assert_eq!(events[0].capability_key, STRUCTURED_OUTPUT, "{name}");
            assert_eq!(
                events[0].direction,
                ObservationDirection::Verified,
                "{name}"
            );
        }
        assert_eq!(
            router.learned_capabilities.snapshot().len(),
            want,
            "{name}: resident entries"
        );
        assert_eq!(
            router.metrics.verified_working_total(),
            want as u64,
            "{name}: counter"
        );
    }
}

#[tokio::test]
async fn a_scope_free_attempt_carries_no_evidence() {
    // (name, config, carries evidence, request carries a forwarded bearer)
    let rows = [
        ("own credential (positive control)", OPENAI_P1, true, false),
        ("kill switch off", OPENAI_P1_DISABLED, false, false),
        (
            "own anthropic credential (positive control)",
            ANTHROPIC_P1,
            true,
            false,
        ),
        ("forwarded credential", ANTHROPIC_P1_FORWARDED, false, true),
    ];
    for (name, config, carries, forwarded) in rows {
        // Arrange
        let router = router_with(config, Tail::End);
        let mut req = strict_request();
        if forwarded {
            req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
                "client-token".to_string(),
            ));
        }

        // Act
        let meta = dispatch_and_consume(&router, req, Consume::ToEnd).await;

        // Assert
        assert_eq!(meta.stream_evidence.is_some(), carries, "{name}");
    }
}

#[tokio::test]
async fn clones_of_the_meta_settle_the_evidence_once() {
    // Arrange
    let router = router_with(OPENAI_P1, Tail::End);
    let meta = dispatch_and_consume(&router, strict_request(), Consume::ToEnd).await;
    let first = meta.clone();
    let second = meta.clone();
    let mut first_events = Vec::new();
    let mut second_events = Vec::new();

    // Act
    let first_read = first
        .stream_evidence
        .as_ref()
        .expect("carries evidence")
        .settle(&router, VERIFIED_ONLY, &mut first_events);
    let second_read = second
        .stream_evidence
        .as_ref()
        .expect("carries evidence")
        .settle(&router, VERIFIED_ONLY, &mut second_events);

    // Assert
    assert!(first_read, "the first settle reads the response");
    assert_eq!(first_events.len(), 1, "the first settle admits");
    assert!(!second_read, "the second settle finds the slot empty");
    assert!(second_events.is_empty(), "the second settle admits nothing");
    assert_eq!(router.metrics.verified_working_total(), 1);
}

#[tokio::test]
async fn dropping_an_unsettled_carrier_writes_nothing() {
    // (name, settle before the drop, resident entries expected)
    let rows = [
        ("settled (positive control)", true, 1),
        ("dropped unsettled", false, 0),
    ];
    for (name, settle, want) in rows {
        // Arrange
        let router = router_with(OPENAI_P1, Tail::End);
        let before = router.learned_capabilities.snapshot().len();
        let meta = dispatch_and_consume(&router, strict_request(), Consume::ToEnd).await;
        let evidence = meta.stream_evidence.clone().expect("carries evidence");
        assert!(evidence.ended_naturally(), "{name}: the stream ended");

        // Act
        if settle {
            evidence.settle(&router, VERIFIED_ONLY, &mut Vec::new());
        }
        drop(evidence);
        drop(meta);

        // Assert
        assert_eq!(
            router.learned_capabilities.snapshot().len(),
            before + want,
            "{name}: resident entries"
        );
        assert_eq!(
            router.metrics.verified_working_total(),
            want as u64,
            "{name}: counter"
        );
    }
}
