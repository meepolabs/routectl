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

use std::collections::BTreeMap;
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
    closed: bool,
    failure: Option<IncompleteReason>,
}

impl fmt::Debug for StreamAssembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamAssembler")
            .field("dialect", &self.dialect)
            .field("bytes", &self.budget.bytes)
            .field("closed", &self.closed)
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
            closed: false,
            failure: None,
        }
    }

    /// Fold the next frame. Keepalive frames (empty data or an SSE comment)
    /// are ignored, as is anything after a failure. After the frame that
    /// closes the stream, only a failure frame still counts (it fails the
    /// assembly); every other frame is ignored.
    pub fn push(&mut self, frame: SseFrame<'_>) {
        if self.failure.is_some() {
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
        if self.closed {
            self.check_after_close(data, frame.event);
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
            Step::Close => self.closed = true,
            Step::UpstreamError => self.fail(IncompleteReason::UpstreamError),
            Step::OverBudget => self.fail(IncompleteReason::Budget),
        }
    }

    /// End the assembly. A failure recorded while folding wins, then a
    /// non-normal `exit`, then a missing terminal frame (reported as
    /// [`IncompleteReason::Disconnect`]: the stream stopped early). Whether
    /// the terminal arrived is judged on the final state, so a later frame
    /// can still un-finish a response that looked whole.
    pub fn finish(mut self, exit: StreamExit) -> Assembled {
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
        if !self.closed && !self.fold.is_whole() {
            return Assembled::Incomplete(IncompleteReason::Disconnect);
        }
        let Some(document) = self.fold.finish(&mut self.budget) else {
            return Assembled::Incomplete(IncompleteReason::Budget);
        };
        if count_nodes(&document, self.budget.max_nodes) > self.budget.max_nodes {
            return Assembled::Incomplete(IncompleteReason::Budget);
        }
        Assembled::Complete(document)
    }

    fn check_after_close(&mut self, data: &str, event: Option<&str>) {
        let Ok(Value::Object(body)) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let kind = body.get("type").and_then(Value::as_str).or(event);
        let failed = match self.dialect {
            IngressDialect::Anthropic => anthropic::is_failure(kind),
            IngressDialect::Chat => chat::is_failure(&body),
            IngressDialect::Responses => responses::is_failure(kind),
        };
        if failed {
            self.fail(IncompleteReason::UpstreamError);
        }
    }

    fn fail(&mut self, reason: IncompleteReason) {
        self.failure = Some(reason);
        self.fold = Fold::Spent;
        self.budget.nodes = 0;
    }

    #[cfg(test)]
    fn with_limits(dialect: IngressDialect, max_bytes: usize, max_nodes: usize) -> Self {
        let mut assembler = Self::new(dialect);
        assembler.budget.max_bytes = max_bytes;
        assembler.budget.max_nodes = max_nodes;
        assembler
    }

    #[cfg(test)]
    const fn retained_nodes(&self) -> usize {
        self.budget.nodes
    }

    #[cfg(test)]
    const fn failure(&self) -> Option<IncompleteReason> {
        self.failure
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
            Self::Anthropic(fold) => fold.apply(input, budget),
            Self::Chat(fold) => fold.apply(input, budget),
            Self::Responses(fold) => fold.apply(input, budget),
            Self::Spent => Step::Fold,
        }
    }

    /// Whether the folded state is a whole response even though no frame
    /// closed the stream.
    fn is_whole(&self) -> bool {
        match self {
            Self::Chat(fold) => fold.every_choice_finished(),
            Self::Anthropic(_) | Self::Responses(_) | Self::Spent => false,
        }
    }

    /// The assembled document, or None when building it would exceed the
    /// node cap.
    fn finish(self, budget: &mut Budget) -> Option<Value> {
        match self {
            Self::Anthropic(fold) => fold.finish(budget),
            Self::Chat(fold) => Some(fold.finish()),
            Self::Responses(fold) => Some(fold.finish()),
            Self::Spent => None,
        }
    }
}

/// Caps on what one assembly reads and keeps. `nodes` tracks the JSON
/// values the fold currently retains: every value is charged before it is
/// stored and credited when it is replaced, so retained state never grows
/// past the cap. Concatenated text adds no nodes.
struct Budget {
    max_bytes: usize,
    max_nodes: usize,
    bytes: usize,
    nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_bytes: MAX_ASSEMBLED_BYTES,
            max_nodes: MAX_ASSEMBLED_NODES,
            bytes: 0,
            nodes: 0,
        }
    }
}

impl Budget {
    const fn charge_bytes(&mut self, len: usize) -> bool {
        self.bytes = self.bytes.saturating_add(len);
        self.bytes <= self.max_bytes
    }

    const fn charge_nodes(&mut self, count: usize) -> bool {
        let next = self.nodes.saturating_add(count);
        if next > self.max_nodes {
            return false;
        }
        self.nodes = next;
        true
    }

    /// Charge every node of `value`, walking no further than the room left.
    fn charge(&mut self, value: &Value) -> bool {
        self.swap(None, value)
    }

    /// Account for `incoming` taking the place of `old`: admitted when the
    /// retained count minus `old` plus `incoming` stays within the cap.
    /// Leaves the count unchanged on refusal.
    fn swap(&mut self, old: Option<&Value>, incoming: &Value) -> bool {
        let freed = old.map_or(0, |old| count_nodes(old, self.max_nodes));
        let base = self.nodes.saturating_sub(freed);
        let room = self.max_nodes.saturating_sub(base);
        let added = count_nodes(incoming, room);
        if added > room {
            return false;
        }
        self.nodes = base.saturating_add(added);
        true
    }
}

/// Nodes in `document`, counting no further than `limit + 1`.
fn count_nodes(document: &Value, limit: usize) -> usize {
    let mut pending = vec![document];
    let mut seen = 0_usize;
    while let Some(node) = pending.pop() {
        seen = seen.saturating_add(1);
        if seen > limit {
            return seen;
        }
        match node {
            Value::Array(items) => pending.extend(items.iter()),
            Value::Object(fields) => pending.extend(fields.values()),
            _ => {}
        }
    }
    seen
}

/// Store `value` at `key`, charging it and crediting what it replaces.
/// Returns false, storing nothing, when the node cap would be exceeded.
fn insert(budget: &mut Budget, target: &mut Map<String, Value>, key: String, value: Value) -> bool {
    if !budget.swap(target.get(&key), &value) {
        return false;
    }
    target.insert(key, value);
    true
}

/// Overwrite `slot` with `incoming`, charging it and crediting the old
/// value. Returns false, changing nothing, when the node cap would be
/// exceeded.
fn replace(budget: &mut Budget, slot: &mut Value, incoming: Value) -> bool {
    if !budget.swap(Some(slot), &incoming) {
        return false;
    }
    *slot = incoming;
    true
}

/// Replace a retained root object, charging the new one and crediting the
/// old.
fn replace_object(
    budget: &mut Budget,
    target: &mut Map<String, Value>,
    incoming: Map<String, Value>,
) -> bool {
    let mut slot = Value::Object(std::mem::take(target));
    let kept = replace(budget, &mut slot, Value::Object(incoming));
    if let Value::Object(fields) = slot {
        *target = fields;
    }
    kept
}

/// Merge one streamed delta field into `target`: strings concatenate,
/// arrays extend, objects merge key by key, null is skipped, anything else
/// replaces. Returns false when the node cap would be exceeded.
fn merge_field(
    budget: &mut Budget,
    target: &mut Map<String, Value>,
    key: String,
    incoming: Value,
) -> bool {
    if incoming.is_null() {
        return true;
    }
    let Some(existing) = target.get_mut(&key) else {
        return insert(budget, target, key, incoming);
    };
    match (existing, incoming) {
        (Value::String(text), Value::String(more)) => {
            text.push_str(&more);
            true
        }
        (Value::Array(items), Value::Array(more)) => {
            if !more.iter().all(|item| budget.charge(item)) {
                return false;
            }
            items.extend(more);
            true
        }
        (Value::Object(fields), Value::Object(more)) => more
            .into_iter()
            .all(|(inner, value)| merge_field(budget, fields, inner, value)),
        (slot, other) => replace(budget, slot, other),
    }
}

/// Set `key` to a non-empty string value, ignoring null and empty strings.
fn set_nonempty(
    budget: &mut Budget,
    target: &mut Map<String, Value>,
    key: String,
    incoming: Value,
) -> bool {
    match incoming {
        Value::Null => true,
        Value::String(text) if text.is_empty() => true,
        other => insert(budget, target, key, other),
    }
}

/// The object at `key`, created (and charged) when absent or not an object.
fn object_at<'a>(
    budget: &mut Budget,
    target: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Map<String, Value>> {
    if !target.get(key).is_some_and(Value::is_object)
        && !insert(budget, target, key.to_owned(), Value::Object(Map::new()))
    {
        return None;
    }
    target.get_mut(key).and_then(Value::as_object_mut)
}

/// The array at `key`, created (and charged) when absent or not an array.
fn array_at<'a>(
    budget: &mut Budget,
    target: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Vec<Value>> {
    if !target.get(key).is_some_and(Value::is_array)
        && !insert(budget, target, key.to_owned(), Value::Array(Vec::new()))
    {
        return None;
    }
    target.get_mut(key).and_then(Value::as_array_mut)
}

/// The entry at `index`, created (and charged one node) when absent.
fn slot_at<'a, T: Default>(
    budget: &mut Budget,
    entries: &'a mut BTreeMap<u64, T>,
    index: u64,
) -> Option<&'a mut T> {
    if !entries.contains_key(&index) && !budget.charge_nodes(1) {
        return None;
    }
    Some(entries.entry(index).or_default())
}

fn index_field(body: &Map<String, Value>, key: &str) -> Option<u64> {
    body.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::indexing_slicing)]
#[path = "assemble_tests.rs"]
mod tests;
