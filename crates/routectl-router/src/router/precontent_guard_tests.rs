//! The pre-content guard's error names the ordered kinds of the chunks it
//! buffered, and the kind labels are stable per content-free chunk shape.

use super::*;
use crate::router::precontent::{is_carriage_only, precontent_kind};
use crate::router::stream_commit::{
    MAX_PRECONTENT_CARRIAGE_CHUNKS, MAX_PRECONTENT_CHUNKS, is_content_bearing,
};
use async_trait::async_trait;
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    AnthropicUnifiedQuota, ChunkChoice, ChunkDelta, OpaqueSseEvent, ReasoningDetail,
    ReasoningDetailKind, ResponsesEnvelope, ResponsesItemMembers, Role, UpstreamMeta, UsageDelta,
};
use serde_json::Value;

/// A provider whose single `stream()` call returns a preset stream.
struct FixedStream {
    stream: parking_lot::Mutex<Option<BoxStream<'static, Result<ChatChunk>>>>,
}

impl FixedStream {
    /// Yields `chunks` in order, then ends.
    fn provider(chunks: Vec<ChatChunk>) -> Arc<dyn Provider> {
        Self::from_stream(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
    }

    fn from_stream(stream: BoxStream<'static, Result<ChatChunk>>) -> Arc<dyn Provider> {
        Arc::new(Self {
            stream: parking_lot::Mutex::new(Some(stream)),
        })
    }
}

#[async_trait]
impl Provider for FixedStream {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("streaming tests only")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Ok(self.stream.lock().take().expect("stream() called once"))
    }
    async fn on_auth_failure(&self) -> Result<()> {
        Ok(())
    }
}

fn choice_chunk(delta: ChunkDelta, finish_reason: Option<&str>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: finish_reason.map(Into::into),
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

fn role() -> ChatChunk {
    choice_chunk(
        ChunkDelta {
            role: Some(Role::Assistant),
            ..Default::default()
        },
        None,
    )
}

fn empty_text() -> ChatChunk {
    choice_chunk(
        ChunkDelta {
            content: Some(String::new()),
            ..Default::default()
        },
        None,
    )
}

fn empty_reasoning() -> ChatChunk {
    choice_chunk(
        ChunkDelta {
            reasoning: Some(String::new()),
            ..Default::default()
        },
        None,
    )
}

fn finish() -> ChatChunk {
    choice_chunk(ChunkDelta::default(), Some("end_turn"))
}

fn usage_only() -> ChatChunk {
    ChatChunk {
        usage: Some(UsageDelta {
            completion_tokens: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn meta_only() -> ChatChunk {
    ChatChunk {
        upstream_meta: Some(UpstreamMeta::from_anthropic_unified(
            AnthropicUnifiedQuota::default(),
        )),
        ..Default::default()
    }
}

fn text(t: &str) -> ChatChunk {
    choice_chunk(
        ChunkDelta {
            content: Some(t.into()),
            ..Default::default()
        },
        None,
    )
}

/// An empty-reasoning chunk carrying the id/model stamps a real decoder
/// puts on every chunk of a thinking block.
fn stamped_empty_reasoning() -> ChatChunk {
    ChatChunk {
        id: "msg-1".into(),
        model: "model-1".into(),
        ..empty_reasoning()
    }
}

fn reasoning_detail() -> ChatChunk {
    choice_chunk(
        ChunkDelta {
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Summary,
                id: None,
                format: None,
                index: None,
                payload: serde_json::json!({"text": "thought"}),
            }],
            ..Default::default()
        },
        None,
    )
}

/// True when `chunk` is a single-choice delta carrying only `reasoning: ""`.
fn is_bare_empty_reasoning(chunk: &ChatChunk) -> bool {
    chunk.usage.is_none()
        && chunk.upstream_meta.is_none()
        && chunk.opaque_events.is_empty()
        && chunk.choices.len() == 1
        && chunk.choices[0].finish_reason.is_none()
        && matches!(
            &chunk.choices[0].delta,
            ChunkDelta {
                role: None,
                content: None,
                reasoning: Some(r),
                reasoning_details,
                tool_calls: None,
            } if r.is_empty() && reasoning_details.is_empty()
        )
}

async fn guard_commit(chunks: Vec<ChatChunk>) -> Vec<ChatChunk> {
    let committed = try_stream_with_first_content(
        "p1",
        "wire-1",
        FixedStream::provider(chunks),
        ChatRequest::default(),
    )
    .await
    .expect("the guard commits");
    committed
        .map(|r| r.expect("no mid-stream error"))
        .collect()
        .await
}

async fn guard_error(chunks: Vec<ChatChunk>) -> String {
    match try_stream_with_first_content(
        "p1",
        "wire-1",
        FixedStream::provider(chunks),
        ChatRequest::default(),
    )
    .await
    {
        Err(Error::Streaming(msg)) => msg,
        Err(other) => panic!("expected a streaming error, got {other:?}"),
        Ok(_) => panic!("expected the guard to fail, but it committed"),
    }
}

#[test]
fn precontent_kind_labels_each_content_free_shape() {
    let mut role_and_finish = role();
    role_and_finish.choices[0].finish_reason = Some("end_turn".into());
    let mut finish_with_usage = finish();
    finish_with_usage.usage = usage_only().usage;
    let mut usage_with_meta = usage_only();
    usage_with_meta.upstream_meta = meta_only().upstream_meta;

    let rows: Vec<(&str, ChatChunk, &str)> = vec![
        ("role opener", role(), "role"),
        ("empty text delta", empty_text(), "empty_text"),
        (
            "empty reasoning delta",
            empty_reasoning(),
            "empty_reasoning",
        ),
        ("bare finish_reason", finish(), "finish"),
        ("usage only", usage_only(), "usage"),
        ("upstream_meta only", meta_only(), "upstream_meta"),
        ("empty choices", ChatChunk::default(), "empty_choices"),
        (
            "empty delta, nothing else",
            choice_chunk(ChunkDelta::default(), None),
            "other",
        ),
        (
            "empty tool_calls vec",
            choice_chunk(
                ChunkDelta {
                    tool_calls: Some(vec![]),
                    ..Default::default()
                },
                None,
            ),
            "other",
        ),
        ("role wins over finish", role_and_finish, "role"),
        ("finish wins over usage", finish_with_usage, "finish"),
        ("usage wins over upstream_meta", usage_with_meta, "usage"),
    ];

    for (name, chunk, want) in rows {
        assert!(
            !is_content_bearing(&chunk),
            "{name}: fixture must be content-free"
        );
        assert_eq!(precontent_kind(&chunk), want, "{name}");
    }
}

#[tokio::test]
async fn overflow_error_summarizes_buffered_kinds_in_order() {
    // Eight counted chunks plus the ninth that overflows the cap; the
    // empty-reasoning run is held as one entry and is not counted.
    let mut chunks = vec![role()];
    chunks.extend(std::iter::repeat_with(empty_reasoning).take(3));
    chunks.extend(std::iter::repeat_with(usage_only).take(6));
    chunks.push(meta_only());
    chunks.push(finish());

    let msg = guard_error(chunks).await;

    assert_eq!(
        msg,
        "p1 emitted more than 8 content-free chunks before any content \
         (buffered: role, empty_reasoning, usage x6, upstream_meta, finish)",
    );
}

#[tokio::test]
async fn closed_before_content_error_summarizes_buffered_kinds() {
    let msg = guard_error(vec![role(), empty_reasoning(), empty_reasoning(), finish()]).await;

    assert_eq!(
        msg,
        "p1 stream closed before any content arrived \
         (buffered: role, empty_reasoning, finish)",
    );
}

#[tokio::test]
async fn closed_on_empty_stream_reports_no_buffered_chunks() {
    let msg = guard_error(vec![]).await;

    assert_eq!(
        msg,
        "p1 stream closed before any content arrived (buffered: none)"
    );
}

#[tokio::test]
async fn content_chunk_commits_instead_of_being_summarized() {
    // Positive control: the same content-free prefix followed by real text
    // commits, so no summary is ever produced for a content-bearing chunk.
    let committed = try_stream_with_first_content(
        "p1",
        "wire-1",
        FixedStream::provider(vec![role(), empty_reasoning(), text("hi"), finish()]),
        ChatRequest::default(),
    )
    .await
    .expect("text commits the stream");

    let yielded: Vec<ChatChunk> = committed
        .map(|r| r.expect("no mid-stream error"))
        .collect()
        .await;

    assert_eq!(yielded.len(), 4, "buffered head, content and tail replayed");
    assert_eq!(yielded[2].choices[0].delta.content.as_deref(), Some("hi"));
}

#[tokio::test]
async fn long_empty_reasoning_run_commits_and_replays_one_empty_reasoning_chunk() {
    let mut chunks = vec![role()];
    chunks.extend(std::iter::repeat_with(stamped_empty_reasoning).take(50));
    chunks.extend([reasoning_detail(), text("hi"), finish()]);

    let yielded = guard_commit(chunks).await;

    assert_eq!(yielded.len(), 5, "role, one empty reasoning, detail, tail");
    assert!(yielded[0].choices[0].delta.role.is_some(), "role first");
    assert!(is_bare_empty_reasoning(&yielded[1]), "one empty reasoning");
    assert_eq!(yielded[2].choices[0].delta.reasoning_details.len(), 1);
    assert_eq!(yielded[3].choices[0].delta.content.as_deref(), Some("hi"));
    assert_eq!(
        yielded[4].choices[0].finish_reason.as_deref(),
        Some("end_turn")
    );
}

#[tokio::test]
async fn only_non_reasoning_chunks_count_toward_the_cap_when_interleaved() {
    let counted_kinds: [fn() -> ChatChunk; 9] = [
        role, usage_only, meta_only, usage_only, meta_only, usage_only, meta_only, usage_only,
        meta_only,
    ];
    let interleaved = |counted: usize| -> Vec<ChatChunk> {
        let mut chunks = Vec::new();
        for make in &counted_kinds[..counted] {
            chunks.push(make());
            chunks.extend(std::iter::repeat_with(empty_reasoning).take(20));
        }
        chunks
    };

    // Eight counted chunks fit under the cap; the stream commits on text.
    let mut at_cap = interleaved(8);
    at_cap.push(text("hi"));
    let yielded = guard_commit(at_cap).await;

    // Nine counted chunks overflow regardless of the reasoning between them.
    let msg = guard_error(interleaved(9)).await;

    assert_eq!(
        yielded.len(),
        17,
        "8 counted + 8 collapsed empty-reasoning runs + the text chunk"
    );
    assert_eq!(yielded[16].choices[0].delta.content.as_deref(), Some("hi"));
    assert!(
        msg.starts_with("p1 emitted more than 8 content-free chunks before any content"),
        "{msg}"
    );
}

#[tokio::test]
async fn empty_reasoning_with_any_other_field_counts_toward_the_cap() {
    let with = |edit: fn(&mut ChatChunk)| {
        let mut chunk = empty_reasoning();
        edit(&mut chunk);
        chunk
    };
    let rows: Vec<(&str, ChatChunk)> = vec![
        (
            "role",
            with(|c| c.choices[0].delta.role = Some(Role::Assistant)),
        ),
        (
            "empty text",
            with(|c| c.choices[0].delta.content = Some(String::new())),
        ),
        (
            "empty tool_calls",
            with(|c| c.choices[0].delta.tool_calls = Some(vec![])),
        ),
        (
            "finish_reason",
            with(|c| c.choices[0].finish_reason = Some("end_turn".into())),
        ),
        (
            "matched_stop_sequence",
            with(|c| {
                c.choices[0].matched_stop_sequence = Some("STOP".into());
            }),
        ),
        ("usage", with(|c| c.usage = usage_only().usage)),
        (
            "upstream_meta",
            with(|c| c.upstream_meta = meta_only().upstream_meta),
        ),
        (
            "second choice with a role",
            with(|c| {
                let mut second = role().choices.remove(0);
                second.index = 1;
                c.choices.push(second);
            }),
        ),
        ("empty choices", with(|c| c.choices.clear())),
        (
            "two empty-reasoning choices",
            with(|c| {
                let mut second = empty_reasoning().choices.remove(0);
                second.index = 1;
                c.choices.push(second);
            }),
        ),
        ("single choice at index 1", with(|c| c.choices[0].index = 1)),
    ];

    for (name, chunk) in rows {
        assert!(
            !is_content_bearing(&chunk),
            "{name}: fixture must be content-free"
        );
        let msg = guard_error(vec![chunk; 9]).await;
        assert!(
            msg.starts_with("p1 emitted more than 8 content-free chunks"),
            "{name}: {msg}"
        );
    }
}

/// A one-leg router whose only target, `m1` on `p1`, waits at most
/// `first_content_ms` for its first content chunk.
fn one_leg_router(provider: Arc<dyn Provider>, first_content_ms: u64) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p1".into(),
        crate::config::ProviderEntry::anthropic_api("literal:k"),
    );
    config
        .aliases
        .insert("alias".into(), AliasValue::Chain(vec!["m1".into()]));
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new("m1", "p1", provider, "wire-1")
        .with_stream_first_byte_timeout_ms(first_content_ms);
    router.install_resolved_models(BTreeMap::from([("m1".to_string(), Arc::new(model))]));
    router
}

#[test]
fn endless_ready_empty_reasoning_stream_still_hits_the_first_content_timeout() {
    const TIMEOUT_MS: u64 = 50;
    let wall_limit = Duration::from_secs(10);
    // Endless for as long as the test waits; once it gives up, the stream ends
    // so a regressed guard loop terminates instead of spinning a stranded
    // thread for the rest of the test binary's life.
    let abandoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_abandoned = Arc::clone(&abandoned);
    let provider = FixedStream::from_stream(
        futures::stream::repeat_with(|| Ok(stamped_empty_reasoning()))
            .take_while(move |_| {
                futures::future::ready(!stream_abandoned.load(std::sync::atomic::Ordering::Relaxed))
            })
            .boxed(),
    );
    let (tx, rx) = std::sync::mpsc::channel();

    // A guard loop that never yields would starve every timer on its own
    // runtime, so the wall-clock limit is enforced from a separate thread.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime builds");
        let router = one_leg_router(provider, TIMEOUT_MS);
        let req = ChatRequest {
            model: "alias".into(),
            ..ChatRequest::default()
        };
        let outcome = rt.block_on(router.stream(req));
        let _ = tx.send(outcome.map(|_| ()));
    });
    let outcome = rx.recv_timeout(wall_limit);
    abandoned.store(true, std::sync::atomic::Ordering::Relaxed);
    let outcome =
        outcome.expect("the guard returns instead of spinning past the first-content timeout");

    match outcome {
        Err(Error::Upstream {
            provider,
            status: 0,
            body,
            ..
        }) => {
            assert_eq!(provider, "p1");
            assert!(
                body.starts_with(&format!(
                    "stream first-content timeout after {TIMEOUT_MS}ms"
                )),
                "{body}"
            );
        }
        other => panic!("expected the first-content timeout, got {other:?}"),
    }
}

/// A chunk carrying only a known event's unmodeled members, shaped as the
/// Responses parser stamps one for an `output_item.added`: one empty choice,
/// the event members, and the Responses wire stamp.
fn event_carriage() -> ChatChunk {
    ChatChunk {
        id: "resp-1".into(),
        model: "model-1".into(),
        opaque_events: vec![OpaqueSseEvent::KnownEventExtras {
            event_type: "response.output_item.added".into(),
            upstream_index: Some(0),
            members: serde_json::json!({"item": {"type": "reasoning", "id": "rs_1"}})
                .as_object()
                .cloned()
                .expect("object"),
        }],
        upstream_meta: Some(UpstreamMeta::default().with_wire_dialect(IngressDialect::Responses)),
        ..choice_chunk(ChunkDelta::default(), None)
    }
}

/// A chunk carrying only an output item's members, as the Responses parser
/// stamps one for an `output_item.done`.
fn item_members_carriage() -> ChatChunk {
    let mut members = ResponsesItemMembers::default();
    members.reasoning.insert(
        "rs_1".into(),
        serde_json::json!({"status": "completed"})
            .as_object()
            .cloned()
            .expect("object"),
    );
    let mut envelope = ResponsesEnvelope::default();
    envelope.item_members = members;
    ChatChunk {
        upstream_meta: Some(
            UpstreamMeta::from_responses_envelope(envelope)
                .with_wire_dialect(IngressDialect::Responses),
        ),
        ..choice_chunk(ChunkDelta::default(), None)
    }
}

/// An Anthropic block-stop carrier: no choices, one known-event member set.
fn block_stop_carriage() -> ChatChunk {
    ChatChunk {
        opaque_events: vec![OpaqueSseEvent::KnownEventExtras {
            event_type: "content_block_stop".into(),
            upstream_index: Some(0),
            members: serde_json::Map::new(),
        }],
        ..Default::default()
    }
}

#[test]
fn carriage_only_chunks_are_classed_apart_and_anything_else_beside_them_is_not() {
    let with = |edit: fn(&mut ChatChunk)| {
        let mut chunk = event_carriage();
        edit(&mut chunk);
        chunk
    };
    let carriage: Vec<(&str, ChatChunk)> = vec![
        ("event members", event_carriage()),
        ("item members", item_members_carriage()),
        ("block stop, no choices", block_stop_carriage()),
    ];
    let not_carriage: Vec<(&str, ChatChunk)> = vec![
        (
            "no carriage at all",
            choice_chunk(ChunkDelta::default(), None),
        ),
        ("wire stamp only", with(|c| c.opaque_events.clear())),
        (
            "role",
            with(|c| c.choices[0].delta.role = Some(Role::Assistant)),
        ),
        (
            "empty text",
            with(|c| c.choices[0].delta.content = Some(String::new())),
        ),
        (
            "empty reasoning",
            with(|c| c.choices[0].delta.reasoning = Some(String::new())),
        ),
        (
            "empty tool_calls",
            with(|c| c.choices[0].delta.tool_calls = Some(vec![])),
        ),
        (
            "finish_reason",
            with(|c| c.choices[0].finish_reason = Some("stop".into())),
        ),
        (
            "matched_stop_sequence",
            with(|c| c.choices[0].matched_stop_sequence = Some("STOP".into())),
        ),
        ("usage", with(|c| c.usage = usage_only().usage)),
        (
            "quota meta",
            with(|c| c.upstream_meta = meta_only().upstream_meta),
        ),
        (
            "created_at",
            with(|c| {
                c.upstream_meta = Some(UpstreamMeta::from_responses_envelope(
                    ResponsesEnvelope::of_created_at(1),
                ));
            }),
        ),
    ];

    for (name, chunk) in carriage {
        assert!(!is_content_bearing(&chunk), "{name}: content-free");
        assert!(is_carriage_only(&chunk), "{name}");
        assert_eq!(precontent_kind(&chunk), "carriage", "{name}");
    }
    for (name, chunk) in not_carriage {
        assert!(!is_content_bearing(&chunk), "{name}: content-free");
        assert!(!is_carriage_only(&chunk), "{name}");
    }
}

#[tokio::test]
async fn carriage_beyond_the_metadata_cap_commits_in_order_with_its_meta() {
    // Arrange: a role opener, then more carriage-only chunks than the
    // metadata cap allows counted chunks, then content.
    let carriers = MAX_PRECONTENT_CHUNKS * 3;
    let mut chunks = vec![role()];
    for i in 0..carriers {
        chunks.push(if i % 2 == 0 {
            event_carriage()
        } else {
            item_members_carriage()
        });
    }
    chunks.push(text("hi"));

    // Act
    let yielded = guard_commit(chunks.clone()).await;

    // Assert
    assert_eq!(yielded.len(), chunks.len(), "every chunk replayed");
    for (i, (got, sent)) in yielded.iter().zip(&chunks).enumerate() {
        assert_eq!(got.upstream_meta, sent.upstream_meta, "chunk {i} meta");
        assert_eq!(
            format!("{:?}", got.opaque_events),
            format!("{:?}", sent.opaque_events),
            "chunk {i} carriage"
        );
    }
    assert_eq!(
        yielded[carriers + 1].choices[0].delta.content.as_deref(),
        Some("hi")
    );
}

#[tokio::test]
async fn carriage_only_chunks_have_their_own_bound() {
    // Arrange
    let at_bound = || std::iter::repeat_with(event_carriage).take(MAX_PRECONTENT_CARRIAGE_CHUNKS);
    let mut fits: Vec<ChatChunk> = at_bound().collect();
    fits.push(text("hi"));
    let mut over: Vec<ChatChunk> = at_bound().collect();
    over.push(event_carriage());

    // Act
    let yielded = guard_commit(fits).await;
    let msg = guard_error(over).await;

    // Assert
    assert_eq!(yielded.len(), MAX_PRECONTENT_CARRIAGE_CHUNKS + 1);
    assert_eq!(
        msg,
        format!(
            "p1 emitted more than {MAX_PRECONTENT_CARRIAGE_CHUNKS} carriage-only chunks \
             before any content (buffered: carriage x{})",
            MAX_PRECONTENT_CARRIAGE_CHUNKS + 1
        ),
    );
}

#[tokio::test]
async fn carriage_does_not_widen_the_metadata_cap() {
    // A genuinely content-free stream still fails at the cap, with carriage
    // interleaved or not.
    let mut plain: Vec<ChatChunk> = vec![role()];
    plain.extend(std::iter::repeat_with(usage_only).take(MAX_PRECONTENT_CHUNKS));
    let mut interleaved = Vec::new();
    for chunk in plain.clone() {
        interleaved.push(chunk);
        interleaved.push(event_carriage());
    }

    for (name, chunks) in [("plain", plain), ("interleaved", interleaved)] {
        let msg = guard_error(chunks).await;
        assert!(
            msg.starts_with(&format!(
                "p1 emitted more than {MAX_PRECONTENT_CHUNKS} content-free chunks"
            )),
            "{name}: {msg}"
        );
    }
}

#[tokio::test]
async fn the_buffer_retains_at_most_twice_both_bounds_plus_one() {
    // Arrange: an empty-reasoning run before, between and after every
    // counted and carriage-only chunk, each bound filled exactly.
    let mut chunks = vec![empty_reasoning()];
    let bounded = std::iter::repeat_with(usage_only)
        .take(MAX_PRECONTENT_CHUNKS)
        .chain(std::iter::repeat_with(event_carriage).take(MAX_PRECONTENT_CARRIAGE_CHUNKS));
    for chunk in bounded {
        chunks.push(chunk);
        chunks.extend(std::iter::repeat_with(empty_reasoning).take(3));
    }
    chunks.push(text("hi"));

    // Act
    let yielded = guard_commit(chunks).await;

    // Assert: everything before the text is what the guard retained.
    let retained = yielded.len() - 1;
    assert_eq!(
        retained,
        2 * (MAX_PRECONTENT_CHUNKS + MAX_PRECONTENT_CARRIAGE_CHUNKS) + 1
    );
    assert_eq!(retained, 145);
}

#[tokio::test]
async fn without_carriage_the_buffer_retains_at_most_twice_the_cap_plus_one() {
    // Arrange
    let mut chunks = vec![empty_reasoning()];
    for _ in 0..MAX_PRECONTENT_CHUNKS {
        chunks.push(usage_only());
        chunks.push(empty_reasoning());
    }
    chunks.push(text("hi"));

    // Act
    let yielded = guard_commit(chunks).await;

    // Assert
    assert_eq!(yielded.len() - 1, 2 * MAX_PRECONTENT_CHUNKS + 1);
    assert_eq!(yielded.len() - 1, 17);
}
