//! The pre-content guard's error names the ordered kinds of the chunks it
//! buffered, and the kind labels are stable per content-free chunk shape.

use super::*;
use crate::router::precontent::precontent_kind;
use async_trait::async_trait;
use routectl_core::{
    AnthropicUnifiedQuota, ChunkChoice, ChunkDelta, Role, UpstreamMeta, UsageDelta,
};

/// A provider whose single `stream()` call yields `chunks` in order, then
/// ends.
struct FixedStream {
    chunks: parking_lot::Mutex<Option<Vec<ChatChunk>>>,
}

impl FixedStream {
    fn provider(chunks: Vec<ChatChunk>) -> Arc<dyn Provider> {
        Arc::new(Self {
            chunks: parking_lot::Mutex::new(Some(chunks)),
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
        let chunks = self.chunks.lock().take().expect("stream() called once");
        Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
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

async fn guard_error(chunks: Vec<ChatChunk>) -> String {
    match try_stream_with_first_content(
        "p1",
        "wire-1",
        FixedStream::provider(chunks),
        ChatRequest::default(),
        &RetryPolicy::default(),
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
    // Eight buffered chunks plus the ninth that overflows the cap.
    let mut chunks = vec![role()];
    chunks.extend(std::iter::repeat_with(empty_reasoning).take(7));
    chunks.push(finish());

    let msg = guard_error(chunks).await;

    assert_eq!(
        msg,
        "p1 emitted more than 8 content-free chunks before any content \
         (buffered: role, empty_reasoning x7, finish)",
    );
}

#[tokio::test]
async fn closed_before_content_error_summarizes_buffered_kinds() {
    let msg = guard_error(vec![role(), empty_reasoning(), empty_reasoning(), finish()]).await;

    assert_eq!(
        msg,
        "p1 stream closed before any content arrived \
         (buffered: role, empty_reasoning x2, finish)",
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
        &RetryPolicy::default(),
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
