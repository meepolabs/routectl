//! Bounded in-memory response store backing the Responses ingress's
//! `previous_response_id` chaining and `GET /v1/responses/{id}`
//! retrieval.
//!
//! This replaces the statefulness contract documented in `mod.rs`'
//! history: routectl now persists responses the client asked it to keep
//! (`store`, spec default `true`) in a bounded per-process FIFO, and a
//! chained request resolves against that store instead of 400-ing.
//!
//! Deliberately process-local and bounded:
//! - **In memory only.** A restart drops the store; a chain referencing
//!   a pre-restart id fails with a clear 400 naming the cause, never a
//!   silent wrong answer.
//! - **FIFO eviction at the cap.** The oldest response (by insertion)
//!   leaves first; an evicted id fails the same clear 400.
//! - **Full-context entries.** Each stored entry keeps the wire
//!   `response` object (what `GET /v1/responses/{id}` serves) AND the
//!   canonical request context that produced it (messages + passthrough items: prior
//!   turns + this turn's input, no output). Chaining replays the
//!   context, then the prior response's own output items -- matching
//!   the official API's server-side conversation semantics rather than
//!   "last output only".
//!
//! The store is a SIBLING of the router on `AppState` (like
//! `context_anchors`): a config hot-reload keeps it; only a process
//! restart starts cold.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use routectl_core::{ChatRequest, Message, ResponsesPassthroughItem};
use serde_json::Value;

/// Default store capacity (responses held). Bounded so a long-running
/// daemon cannot grow without limit; each entry is one response object
/// plus its canonical context, so a few hundred cover any realistic
/// local session horizon.
pub const DEFAULT_STORE_CAPACITY: usize = 512;

/// One stored response: the wire `response` object (served by retrieve)
/// plus the canonical request context that produced it.
#[derive(Debug, Clone)]
pub(super) struct StoredResponse {
    pub(super) response: Value,
    pub(super) context: ResponseContext,
}

/// Full canonical conversation context, including same-dialect input items
/// that have no Message representation. Their positions are canonical message
/// boundaries, independent of lane-specific wire expansion or content drops.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    pub messages: Vec<Message>,
    pub passthrough: Vec<ResponsesPassthroughItem>,
    pub system_history: Vec<Message>,
}

impl From<Vec<Message>> for ResponseContext {
    fn from(messages: Vec<Message>) -> Self {
        Self {
            messages,
            passthrough: Vec::new(),
            system_history: Vec::new(),
        }
    }
}

impl From<&ChatRequest> for ResponseContext {
    fn from(req: &ChatRequest) -> Self {
        Self {
            messages: req.messages.to_vec(),
            passthrough: req.routectl_internal.responses_input_passthrough.clone(),
            system_history: req
                .routectl_internal
                .responses_system_history
                .as_deref()
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// Bounded FIFO response store. Shared as `Arc` from `AppState` into
/// the Responses ingress adapter and its stream state.
#[derive(Debug)]
pub struct ResponsesStore {
    inner: Mutex<StoreInner>,
    cap: usize,
}

#[derive(Debug, Default)]
struct StoreInner {
    map: HashMap<String, StoredResponse>,
    order: VecDeque<String>,
}

impl Default for ResponsesStore {
    fn default() -> Self {
        Self::new(DEFAULT_STORE_CAPACITY)
    }
}

impl ResponsesStore {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                map: HashMap::with_capacity(cap),
                order: VecDeque::with_capacity(cap),
            }),
            cap,
        }
    }

    /// Insert (or overwrite) one response with its producing context.
    /// Overwriting an existing id keeps its original eviction position
    /// (it was already queued) rather than re-queueing it at the tail.
    pub fn insert(&self, id: String, response: Value, context: impl Into<ResponseContext>) {
        let context = context.into();
        let mut inner = self.inner.lock().expect("responses store poisoned");
        if inner
            .map
            .insert(id.clone(), StoredResponse { response, context })
            .is_none()
        {
            inner.order.push_back(id);
        }
        while inner.map.len() > self.cap {
            let Some(evict) = inner.order.pop_front() else {
                break;
            };
            // Every stored id is queued exactly once (a re-insert over an
            // existing id keeps its original slot), so the front slot
            // always names a live entry.
            inner.map.remove(&evict);
        }
    }

    /// The wire `response` object (retrieve endpoint).
    pub fn get(&self, id: &str) -> Option<Value> {
        self.inner
            .lock()
            .expect("responses store poisoned")
            .map
            .get(id)
            .map(|e| e.response.clone())
    }

    /// The full entry (chaining): wire response + request context.
    pub fn get_full(&self, id: &str) -> Option<(Value, ResponseContext)> {
        self.inner
            .lock()
            .expect("responses store poisoned")
            .map
            .get(id)
            .map(|e| (e.response.clone(), e.context.clone()))
    }

    /// Count of stored entries (tests + diagnostics).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("responses store poisoned")
            .map
            .len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("responses store poisoned")
            .map
            .is_empty()
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
