//! Folds a streamed response, frame by frame, into the document the same
//! dialect returns for a non-streamed request, so a streamed response can be
//! compared like a complete one.
//!
//! Input is one [`SseFrame`] per server-sent event: the optional `event:`
//! name and the `data:` payload as text. Both the provider stream readers and
//! the ingress renderers already hold exactly that per event, so neither side
//! re-parses its stream to feed an assembler. The frame's kind is the
//! payload's `type` field, falling back to the `event:` name.
//!
//! The fold is fail-closed: [`StreamAssembler::finish`] returns a document
//! only after the dialect's terminal frame arrived on a stream that ended
//! normally. Every other ending is [`Assembled::Incomplete`] with its reason;
//! a partial document is never returned. Neither `Debug` impl here renders
//! body content.

use std::fmt;

use serde_json::{Map, Value};

use super::lane::IngressDialect;

mod anthropic;
mod chat;
mod responses;

/// Cumulative frame bytes one assembler accepts before it gives up with
/// [`IncompleteReason::Budget`].
const MAX_ASSEMBLED_BYTES: usize = 16 * 1024 * 1024;

/// JSON nodes the assembled document may hold before
/// [`IncompleteReason::Budget`].
const MAX_ASSEMBLED_NODES: usize = 200_000;

const DONE_SENTINEL: &str = "[DONE]";

/// One server-sent event of a streamed response.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SseFrame<'a> {
    /// The SSE `event:` field, when the stream names its events.
    pub event: Option<&'a str>,
    /// The SSE `data:` payload as text.
    pub data: &'a str,
}

impl<'a> SseFrame<'a> {
    /// A frame from its `event:` name and `data:` payload.
    pub const fn new(event: Option<&'a str>, data: &'a str) -> Self {
        Self { event, data }
    }
}

impl fmt::Debug for SseFrame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseFrame")
            .field("event", &self.event)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// How the stream that fed an assembler ended, as its reader saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamExit {
    /// The stream ran to its natural end.
    Ended,
    /// The peer went away mid-stream.
    Disconnect,
    /// The task driving the stream was cancelled.
    Cancelled,
    /// A frame could not be rendered for the client.
    RenderFailure,
    /// The upstream stream failed mid-stream.
    UpstreamError,
}

/// Why an assembly produced no document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncompleteReason {
    /// The stream stopped before its terminal frame.
    Disconnect,
    /// The task driving the stream was cancelled.
    Cancelled,
    /// A frame could not be rendered for the client.
    RenderFailure,
    /// The stream reported an error, failed, or carried a frame that is not
    /// a JSON object.
    UpstreamError,
    /// The stream exceeded the byte or node cap.
    Budget,
}

impl IncompleteReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Disconnect,
        Self::Cancelled,
        Self::RenderFailure,
        Self::UpstreamError,
        Self::Budget,
    ];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disconnect => "disconnect",
            Self::Cancelled => "cancelled",
            Self::RenderFailure => "render_failure",
            Self::UpstreamError => "upstream_error",
            Self::Budget => "budget",
        }
    }
}

/// The outcome of an assembly.
#[derive(Clone, PartialEq, Eq)]
pub enum Assembled {
    /// The whole response, shaped like the dialect's non-streamed body.
    Complete(Value),
    /// No document; why.
    Incomplete(IncompleteReason),
}

impl fmt::Debug for Assembled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Complete(_) => f.write_str("Complete(..)"),
            Self::Incomplete(reason) => f.debug_tuple("Incomplete").field(reason).finish(),
        }
    }
}

/// Folds one streamed response into one document. Pure and total: no I/O,
/// and no input makes it panic or grow past its caps.
pub struct StreamAssembler {
    dialect: IngressDialect,
    fold: Fold,
    budget: Budget,
    terminal: bool,
    closed: bool,
    failure: Option<IncompleteReason>,
}

impl fmt::Debug for StreamAssembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamAssembler")
            .field("dialect", &self.dialect)
            .field("bytes", &self.budget.bytes)
            .field("terminal", &self.terminal)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl StreamAssembler {
    /// An empty assembler for a stream in `dialect`.
    pub fn new(dialect: IngressDialect) -> Self {
        let fold = match dialect {
            IngressDialect::Anthropic => Fold::Anthropic(anthropic::AnthropicFold::default()),
            IngressDialect::Chat => Fold::Chat(chat::ChatFold::default()),
            IngressDialect::Responses => Fold::Responses(responses::ResponsesFold::default()),
        };
        Self {
            dialect,
            fold,
            budget: Budget::default(),
            terminal: false,
            closed: false,
            failure: None,
        }
    }

    /// Fold the next frame. Keepalive frames (empty data or an SSE comment)
    /// are ignored, as is anything after a failure or after the frame that
    /// closes the stream.
    pub fn push(&mut self, frame: SseFrame<'_>) {
        if self.failure.is_some() || self.closed {
            return;
        }
        let frame_len = frame
            .data
            .len()
            .saturating_add(frame.event.map_or(0, str::len));
        if !self.budget.charge_bytes(frame_len) {
            self.fail(IncompleteReason::Budget);
            return;
        }
        let data = frame.data.trim();
        if data.is_empty() || data.starts_with(':') {
            return;
        }
        let input = if data == DONE_SENTINEL {
            Input::Done
        } else {
            match serde_json::from_str::<Value>(data) {
                Ok(Value::Object(body)) => {
                    let kind = body
                        .get("type")
                        .and_then(Value::as_str)
                        .or(frame.event)
                        .map(str::to_owned);
                    Input::Event { kind, body }
                }
                _ => {
                    self.fail(IncompleteReason::UpstreamError);
                    return;
                }
            }
        };
        match self.fold.apply(input, &mut self.budget) {
            Step::Fold => {}
            Step::Terminal => self.terminal = true,
            Step::Close => {
                self.terminal = true;
                self.closed = true;
            }
            Step::UpstreamError => self.fail(IncompleteReason::UpstreamError),
            Step::OverBudget => self.fail(IncompleteReason::Budget),
        }
    }

    /// End the assembly. A failure recorded while folding wins, then a
    /// non-normal `exit`, then a missing terminal frame (reported as
    /// [`IncompleteReason::Disconnect`]: the stream stopped early).
    pub fn finish(self, exit: StreamExit) -> Assembled {
        if let Some(reason) = self.failure {
            return Assembled::Incomplete(reason);
        }
        let exit_reason = match exit {
            StreamExit::Ended => None,
            StreamExit::Disconnect => Some(IncompleteReason::Disconnect),
            StreamExit::Cancelled => Some(IncompleteReason::Cancelled),
            StreamExit::RenderFailure => Some(IncompleteReason::RenderFailure),
            StreamExit::UpstreamError => Some(IncompleteReason::UpstreamError),
        };
        if let Some(reason) = exit_reason {
            return Assembled::Incomplete(reason);
        }
        if !self.terminal {
            return Assembled::Incomplete(IncompleteReason::Disconnect);
        }
        let document = self.fold.finish();
        if node_count_exceeds(&document, self.budget.max_nodes) {
            return Assembled::Incomplete(IncompleteReason::Budget);
        }
        Assembled::Complete(document)
    }

    fn fail(&mut self, reason: IncompleteReason) {
        self.failure = Some(reason);
        self.fold = Fold::Spent;
    }

    #[cfg(test)]
    fn with_limits(dialect: IngressDialect, max_bytes: usize, max_nodes: usize) -> Self {
        let mut assembler = Self::new(dialect);
        assembler.budget.max_bytes = max_bytes;
        assembler.budget.max_nodes = max_nodes;
        assembler
    }
}

/// A parsed frame handed to a dialect fold.
enum Input {
    /// The `[DONE]` sentinel.
    Done,
    /// A JSON object payload; `kind` is its `type`, else the `event:` name.
    Event {
        kind: Option<String>,
        body: Map<String, Value>,
    },
}

/// What a dialect fold made of one frame.
enum Step {
    Fold,
    /// The response is whole; later frames may still add to it.
    Terminal,
    /// The response is whole and nothing after this frame belongs to it.
    Close,
    UpstreamError,
    OverBudget,
}

enum Fold {
    Anthropic(anthropic::AnthropicFold),
    Chat(chat::ChatFold),
    Responses(responses::ResponsesFold),
    /// A failed assembly drops its partial state.
    Spent,
}

impl Fold {
    fn apply(&mut self, input: Input, budget: &mut Budget) -> Step {
        match self {
            Self::Anthropic(fold) => fold.apply(input),
            Self::Chat(fold) => fold.apply(input),
            Self::Responses(fold) => fold.apply(input, budget),
            Self::Spent => Step::Fold,
        }
    }

    fn finish(self) -> Value {
        match self {
            Self::Anthropic(fold) => fold.finish(),
            Self::Chat(fold) => fold.finish(),
            Self::Responses(fold) => fold.finish(),
            Self::Spent => Value::Null,
        }
    }
}

struct Budget {
    max_bytes: usize,
    max_nodes: usize,
    bytes: usize,
    padded: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_bytes: MAX_ASSEMBLED_BYTES,
            max_nodes: MAX_ASSEMBLED_NODES,
            bytes: 0,
            padded: 0,
        }
    }
}

impl Budget {
    const fn charge_bytes(&mut self, len: usize) -> bool {
        self.bytes = self.bytes.saturating_add(len);
        self.bytes <= self.max_bytes
    }

    /// Null slots created to reach a sparse array index are the only nodes
    /// a document gains that no input byte paid for, so they are charged
    /// against the node cap before they are allocated.
    const fn pad(&mut self, slots: usize) -> bool {
        self.padded = self.padded.saturating_add(slots);
        self.padded <= self.max_nodes
    }
}

fn node_count_exceeds(document: &Value, max_nodes: usize) -> bool {
    let mut pending = vec![document];
    let mut seen = 0_usize;
    while let Some(node) = pending.pop() {
        seen = seen.saturating_add(1);
        if seen > max_nodes {
            return true;
        }
        match node {
            Value::Array(items) => pending.extend(items.iter()),
            Value::Object(fields) => pending.extend(fields.values()),
            _ => {}
        }
    }
    false
}

/// Merge one streamed delta field into `target`: strings concatenate,
/// arrays extend, objects merge key by key, null is skipped, anything else
/// replaces.
fn merge_field(target: &mut Map<String, Value>, key: String, incoming: Value) {
    if incoming.is_null() {
        return;
    }
    let Some(existing) = target.get_mut(&key) else {
        target.insert(key, incoming);
        return;
    };
    match (existing, incoming) {
        (Value::String(text), Value::String(more)) => text.push_str(&more),
        (Value::Array(items), Value::Array(more)) => items.extend(more),
        (Value::Object(fields), Value::Object(more)) => {
            for (inner, value) in more {
                merge_field(fields, inner, value);
            }
        }
        (slot, other) => *slot = other,
    }
}

/// Set `key` to a non-empty string value, ignoring null and empty strings.
fn set_nonempty(target: &mut Map<String, Value>, key: String, incoming: Value) {
    match incoming {
        Value::Null => {}
        Value::String(text) if text.is_empty() => {}
        other => {
            target.insert(key, other);
        }
    }
}

fn index_field(body: &Map<String, Value>, key: &str) -> Option<u64> {
    body.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::indexing_slicing)]
#[path = "assemble_tests.rs"]
mod tests;
