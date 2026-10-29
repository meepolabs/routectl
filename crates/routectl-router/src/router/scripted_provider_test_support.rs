//! The depositing test provider shared by the wire-rejection, rejection
//! location and declared-edit revert sidecars: a stand-in for an egress that
//! deposits the body it sent into the attempt's tap, then answers as told.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::wire_check::DeclaredEdit;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error, Message,
    MessageContent, Provider, Result, Role, TokenCount,
};
use serde_json::{Value, json};

/// How the test provider builds the body it deposits from the client's.
pub(super) type Shape = fn(&ChatRequest, Value) -> Value;
/// How the test provider answers an attempt, given the request.
pub(super) type Answer = fn(&ChatRequest) -> Result<()>;

/// A provider that, like a real egress, deposits the body it sent into the
/// attempt's tap (unless it has no `shape`), then answers as told. Counts the
/// upstream calls it took and keeps the messages each one carried.
pub(super) struct ScriptedProvider {
    shape: Option<Shape>,
    answer: Answer,
    /// An edit the provider declares on every deposit, as an egress
    /// declaring its own body change would.
    declares: Option<DeclaredEdit>,
    pub(super) calls: AtomicUsize,
    pub(super) messages: Mutex<Vec<Value>>,
}

impl ScriptedProvider {
    pub(super) fn new(shape: Option<Shape>, answer: Answer) -> Arc<Self> {
        Arc::new(Self::build(shape, answer, None))
    }

    pub(super) fn boxed(shape: Shape, answer: Answer) -> Arc<dyn Provider> {
        Self::new(Some(shape), answer)
    }

    pub(super) fn declaring(shape: Shape, answer: Answer, edit: DeclaredEdit) -> Arc<Self> {
        Arc::new(Self::build(Some(shape), answer, Some(edit)))
    }

    fn build(shape: Option<Shape>, answer: Answer, declares: Option<DeclaredEdit>) -> Self {
        Self {
            shape,
            answer,
            declares,
            calls: AtomicUsize::new(0),
            messages: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn run(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.messages
            .lock()
            .push(serde_json::to_value(&*req.messages).expect("serialize messages"));
        if let (Some(shape), Some(tap), Some(ingress)) = (
            self.shape,
            req.routectl_internal.wire_tap.as_ref(),
            req.routectl_internal.ingress_wire.as_ref(),
        ) {
            let client: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
            let sent = shape(req, client);
            tap.deposit(serde_json::to_vec(&sent).expect("serialize").into());
            if let Some(edit) = self.declares.as_ref() {
                tap.declare(edit.clone());
            }
        }
        (self.answer)(req)
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.run(&req).map(|()| ok_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.run(&req)
            .map(|()| stream::iter([Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.run(&req).map(|()| TokenCount {
            input_tokens: 1,
            ..Default::default()
        })
    }
}

fn ok_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
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
    }
}
