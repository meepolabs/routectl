//! Response evidence for a streamed dispatch: a fold over the stream's Ok
//! chunks and the carrier that holds it, with the attempt's
//! [`EvidenceScope`], until the caller settles it at the stream's end.
//!
//! A stream has no assembled response, and first content is not the end of
//! one, so the router cannot read evidence where it hands the stream over.
//! Instead the router's stream wrap folds each Ok chunk into the few facts the
//! detectors read and, only when the inner stream ends with no error seen,
//! records a minimal [`ChatResponse`] built from them. The caller settles the
//! carrier after it has delivered the whole stream, through the Router it
//! holds at that moment; the evidence path re-checks that Router's config and
//! generation, exactly as for a complete response.
//!
//! The natural end is recorded only on the inner stream's `None` after no
//! `Err`, never in a `Drop`: a consumer that stops polling (a client
//! disconnect) drops the stream mid-way, and that must read as no end at all.
//!
//! Log hygiene: the fold may hold response text (only when strict output was
//! requested, for the schema check) and never logs it; the carrier's `Debug`
//! prints state flags only.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::{fmt, io};

use futures::stream::{BoxStream, StreamExt as _};
use routectl_core::{
    ChatChunk, ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent,
    OpaqueSseEvent, ReasoningDetail, ReasoningDetailKind, Result, Role, Usage, UsageDelta,
};
use serde_json::Value;

use super::Router;
use super::capability_observe::{CapabilityObserveEvent, EvidencePolicy, EvidenceScope};
use crate::capability_detect::SEARCH_EVIDENCE_TYPE_TAGS;

/// Most variable-length bytes one fold retains, summed over the kept
/// strict-output text, the last `finish_reason`, and the serialized usage
/// `server_tool_use` object. A reply that would take the fold past this size
/// spoils it, so its evidence is not read rather than read from a truncated
/// body.
pub(super) const STREAM_EVIDENCE_BYTE_CAP: usize = 1024 * 1024;

/// The response evidence of one streamed attempt: its [`EvidenceScope`] and,
/// once the stream has ended naturally, the minimal response the detectors
/// read.
///
/// Rides out on [`super::DispatchMeta::stream_evidence`]. `DispatchMeta` is
/// `Clone`, so every clone of this carrier shares ONE take-once scope slot:
/// the first [`Self::settle`] consumes the scope and any later settle, through
/// any clone, reads nothing. A carrier dropped unsettled writes nothing.
#[derive(Clone)]
pub struct StreamEvidence {
    shared: Arc<Shared>,
}

struct Shared {
    scope: Mutex<Option<EvidenceScope>>,
    keeps_text: bool,
    natural_end: Mutex<Option<NaturalEnd>>,
}

/// What the wrap records when the inner stream ends with no error seen.
struct NaturalEnd {
    /// The folded response, or `None` when the fold was spoiled.
    response: Option<ChatResponse>,
}

impl fmt::Debug for StreamEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamEvidence")
            .field("pending", &lock(&self.shared.scope).is_some())
            .field("ended_naturally", &self.ended_naturally())
            .finish_non_exhaustive()
    }
}

impl StreamEvidence {
    /// A carrier for a successful streamed attempt's `scope`, with no end
    /// recorded yet.
    pub(super) fn new(scope: EvidenceScope) -> Self {
        let keeps_text = scope.strict_output_requested();
        Self {
            shared: Arc::new(Shared {
                scope: Mutex::new(Some(scope)),
                keeps_text,
                natural_end: Mutex::new(None),
            }),
        }
    }

    /// Whether the wrapped stream reached its natural end: the inner stream
    /// returned `None` and no item before it was an `Err`.
    pub fn ended_naturally(&self) -> bool {
        lock(&self.shared.natural_end).is_some()
    }

    /// Read the attempt's evidence against `router` under `policy`, pushing
    /// each acting observation onto `events`. Takes the scope out of the
    /// shared slot, so this reads at most once across every clone. Returns
    /// whether the detectors read a folded response: `false` when the scope
    /// was already taken, the stream did not end naturally, the fold was
    /// spoiled, or `router`'s config refuses the scope (see
    /// [`Router::observe_response_evidence`]). In each `false` case the scope
    /// is consumed and nothing is admitted.
    pub fn settle(
        &self,
        router: &Router,
        policy: EvidencePolicy,
        events: &mut Vec<CapabilityObserveEvent>,
    ) -> bool {
        let Some(scope) = lock(&self.shared.scope).take() else {
            return false;
        };
        let natural_end = lock(&self.shared.natural_end);
        let Some(response) = natural_end.as_ref().and_then(|end| end.response.as_ref()) else {
            return false;
        };
        router.observe_response_evidence(scope, response, policy, events)
    }

    /// Wrap `inner` so each Ok chunk is folded, and the natural end is
    /// recorded only when `inner` returns `None` with no `Err` seen. Items pass
    /// through unchanged.
    pub(super) fn wrap(
        &self,
        inner: BoxStream<'static, Result<ChatChunk>>,
    ) -> BoxStream<'static, Result<ChatChunk>> {
        let carrier = self.clone();
        let s = async_stream::stream! {
            let mut inner = inner;
            let mut fold = EvidenceFold::new(carrier.shared.keeps_text);
            let mut saw_err = false;
            while let Some(item) = inner.next().await {
                match &item {
                    Ok(chunk) => fold.absorb(chunk),
                    Err(_) => saw_err = true,
                }
                yield item;
            }
            if !saw_err {
                *lock(&carrier.shared.natural_end) = Some(NaturalEnd {
                    response: fold.finish(),
                });
            }
        };
        Box::pin(s)
    }
}

/// Recover a poisoned lock: no code holding one of these panics, and a
/// recovered slot still settles at most once.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The facts a stream's Ok chunks carry that the detectors read, and nothing
/// else.
#[derive(Debug, Default)]
pub(super) struct EvidenceFold {
    keeps_text: bool,
    choice_index: Option<u32>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    has_reasoning_details: bool,
    has_tool_call: bool,
    has_search_evidence: bool,
    text: String,
    /// Bytes of `server_tool_use` as serialized, charged to the budget.
    server_tool_use_bytes: usize,
    /// Variable-length bytes retained across every field the budget covers.
    retained: usize,
    spoiled: bool,
}

impl EvidenceFold {
    /// An empty fold; `keeps_text` retains message text for the strict-output
    /// schema check.
    pub(super) fn new(keeps_text: bool) -> Self {
        Self {
            keeps_text,
            ..Self::default()
        }
    }

    /// Fold one Ok chunk.
    pub(super) fn absorb(&mut self, chunk: &ChatChunk) {
        if self.spoiled {
            return;
        }
        self.has_search_evidence |= chunk.opaque_events.iter().any(|event| {
            matches!(
                event,
                OpaqueSseEvent::ContentBlockStart { type_tag, .. }
                    if SEARCH_EVIDENCE_TYPE_TAGS.contains(&type_tag.as_str())
            )
        });
        if let Some(delta) = &chunk.usage {
            if let Some(server_tool_use) = &delta.server_tool_use {
                let bytes = serialized_len(server_tool_use);
                if !self.recharge(self.server_tool_use_bytes, bytes) {
                    return;
                }
                self.server_tool_use_bytes = bytes.unwrap_or(0);
            }
            merge_usage(self.usage.get_or_insert_with(Usage::default), delta);
        }
        for choice in &chunk.choices {
            if *self.choice_index.get_or_insert(choice.index) != choice.index {
                self.spoil();
                return;
            }
            if let Some(reason) = &choice.finish_reason {
                let held = self.finish_reason.as_ref().map_or(0, String::len);
                if !self.recharge(held, Some(reason.len())) {
                    return;
                }
                self.finish_reason = Some(reason.clone());
            }
            let delta = &choice.delta;
            self.has_reasoning_details |= !delta.reasoning_details.is_empty();
            self.has_tool_call |= delta.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
            if self.keeps_text
                && let Some(text) = &delta.content
            {
                self.push_text(text);
                if self.spoiled {
                    return;
                }
            }
        }
    }

    /// The minimal response the detectors read, or `None` when the fold was
    /// spoiled (more than one choice, or past [`STREAM_EVIDENCE_BYTE_CAP`]).
    pub(super) fn finish(self) -> Option<ChatResponse> {
        if self.spoiled {
            return None;
        }
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(ContentPart::Known(KnownContentPart::Text {
                text: self.text,
                citations: None,
                cache_control: None,
            }));
        }
        if self.has_search_evidence {
            parts.push(search_evidence_part());
        }
        let content = if parts.is_empty() {
            MessageContent::Null
        } else {
            MessageContent::Parts(parts)
        };
        let message = Message {
            role: Role::Assistant,
            content,
            reasoning: None,
            reasoning_details: if self.has_reasoning_details {
                vec![placeholder_reasoning_detail()]
            } else {
                Vec::new()
            },
            name: None,
            tool_call_id: None,
            tool_calls: self
                .has_tool_call
                .then(|| vec![Value::Object(serde_json::Map::new())]),
            refusal: None,
        };
        Some(ChatResponse {
            choices: vec![Choice {
                index: self.choice_index.unwrap_or(0),
                message,
                finish_reason: self.finish_reason,
                matched_stop_sequence: None,
                logprobs: None,
            }],
            usage: self.usage,
            ..ChatResponse::default()
        })
    }

    fn push_text(&mut self, text: &str) {
        if self.recharge(0, Some(text.len())) {
            self.text.push_str(text);
        }
    }

    /// Replace `released` retained bytes with `charged` ones, spoiling the
    /// fold instead when the result would pass [`STREAM_EVIDENCE_BYTE_CAP`]
    /// or the size is unknown (`None`). Call before retaining the new value.
    fn recharge(&mut self, released: usize, charged: Option<usize>) -> bool {
        let next = charged.and_then(|bytes| (self.retained - released).checked_add(bytes));
        match next {
            Some(next) if next <= STREAM_EVIDENCE_BYTE_CAP => {
                self.retained = next;
                true
            }
            _ => {
                self.spoil();
                false
            }
        }
    }

    fn spoil(&mut self) {
        self.spoiled = true;
        self.text = String::new();
        self.finish_reason = None;
        self.usage = None;
        self.retained = 0;
        self.server_tool_use_bytes = 0;
    }
}

/// The byte length of `value` serialized as JSON, or `None` when it does not
/// serialize.
fn serialized_len(value: &Value) -> Option<usize> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

/// An `io::Write` that keeps only the number of bytes written to it.
struct ByteCounter(usize);

impl io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Fold a streamed usage update into the running usage, last writer wins per
/// field: upstreams send cumulative counters, often only on the final chunk.
fn merge_usage(usage: &mut Usage, delta: &UsageDelta) {
    if let Some(v) = delta.prompt_tokens {
        usage.prompt_tokens = v;
    }
    if let Some(v) = delta.completion_tokens {
        usage.completion_tokens = v;
    }
    if let Some(v) = delta.total_tokens {
        usage.total_tokens = v;
    }
    if delta.reasoning_tokens.is_some() {
        usage.reasoning_tokens = delta.reasoning_tokens;
    }
    if delta.cache_creation_input_tokens.is_some() {
        usage.cache_creation_input_tokens = delta.cache_creation_input_tokens;
    }
    if delta.cache_read_input_tokens.is_some() {
        usage.cache_read_input_tokens = delta.cache_read_input_tokens;
    }
    if delta.cache_creation.is_some() {
        usage.cache_creation.clone_from(&delta.cache_creation);
    }
    if delta.server_tool_use.is_some() {
        usage.server_tool_use.clone_from(&delta.server_tool_use);
    }
}

/// A content part the search detector recognizes, carrying none of the
/// upstream block's data.
fn search_evidence_part() -> ContentPart {
    ContentPart::Other {
        type_tag: SEARCH_EVIDENCE_TYPE_TAGS[0].to_string(),
        cache_control: None,
        extras: serde_json::Map::new(),
    }
}

/// A reasoning detail standing for "the stream carried typed reasoning",
/// carrying none of its content.
fn placeholder_reasoning_detail() -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: None,
        index: None,
        payload: Value::Object(serde_json::Map::new()),
    }
}

#[cfg(test)]
#[path = "stream_evidence_tests.rs"]
mod tests;
