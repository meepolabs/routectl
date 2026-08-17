//! Session-keyed prefix-epoch store for the prefix-rewrite detector.
//!
//! Mirrors the `ShadowStore` shape (bounded LRU map behind a
//! `parking_lot::Mutex`) but is its OWN type with its own entry: the shadow
//! monitor tracks one fingerprint of the TRIMMED cacheable front, this store
//! tracks `(prefix_len, fingerprint)` of the RAW canonical prefix plus an
//! epoch and a compaction count.
//!
//! Keyed by the inbound session key ALONE, not the
//! (session, provider_kind, model) triple: the question is whether the CLIENT
//! rewrote its own bytes, which does not vary by dispatch target. Triple
//! keying would mint a phantom epoch on every fallback hop.
//!
//! The observed object is the raw canonical prefix -- system + tools +
//! `messages[0..len-1]` -- fingerprinted FNV-1a before any strip, reduction,
//! or injection. Excluding the newest turn is mandatory: it grows on every
//! turn by construction, so including it would report a rewrite every time
//! (same reason `context_trim::trimmed_prefix_fingerprint` excludes the tail).
//!
//! Classification per turn:
//!
//! - `prefix_len >= prev`: recompute the fingerprint over the OLD region
//!   (`messages[0..prev]`) and compare. Growth makes that region identical by
//!   construction, so equality means pure append (`Stable`) and a mismatch
//!   means the client rewrote history within the epoch (`Rewritten`: reseed
//!   the baseline, epoch += 1).
//! - `prefix_len < prev`: a shortening is the shape real compaction takes
//!   (a summary replaces history), which is NOT a structural truncation of
//!   the old sequence. Recognizing it by length alone reseeds without ever
//!   classifying legitimate compaction as a rewrite (`Reseeded`, compaction
//!   count += 1). Accepted residual: a rewrite that also shortens the prefix
//!   is unobservable -- a bounded false negative, never a false positive.
//!
//! Recording only: nothing here mutates a request, logs, or reads cache
//! usage. Cause detection (which bytes moved) is structurally separate from
//! symptom detection (`usage_capture::is_cache_thrash`, which reads usage
//! counters); the two share no code and never call each other.

use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;

use lru::LruCache;
use parking_lot::Mutex;
use routectl_core::ChatRequest;

use super::{DispatchMeta, Router};
use crate::context_trim::fnv1a_hash;

/// Bound on the number of distinct sessions tracked at once. Matches the
/// K-estimator / shadow bound so a session alive there also has an epoch
/// entry here.
const PREFIX_REWRITE_CAPACITY: usize = crate::k_estimator::K_SESSION_CAPACITY;

/// Classification of one observed turn against the stored baseline.
///
/// The discriminant values are the wire contract for the usage column and
/// mirror the `would_trim_shadow_misfire` advisory encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EpochEvent {
    /// The prefix is unchanged, or grew by pure append with the old region
    /// byte-identical. Advisory 0.
    Stable,
    /// The old region's bytes changed: the client rewrote history inside the
    /// current epoch. The baseline is reseeded and the epoch incremented.
    /// Advisory 1.
    Rewritten,
    /// The prefix shortened -- the compaction shape. The baseline is reseeded
    /// and the compaction count incremented; deliberately NOT a rewrite.
    /// Advisory 2.
    Reseeded,
}

impl EpochEvent {
    /// Advisory code stamped onto the usage column.
    pub(super) const fn code(self) -> i64 {
        match self {
            Self::Stable => 0,
            Self::Rewritten => 1,
            Self::Reseeded => 2,
        }
    }
}

/// Outcome of a single [`PrefixRewriteStore::observe`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EpochObservation {
    /// `None` when no prior baseline existed for this session (first sight, or
    /// first sight after eviction / process restart): the baseline was
    /// recorded and no classification is emitted.
    pub(super) event: Option<EpochEvent>,
    /// Epoch after this observation. Counts rewrites within the tracked
    /// lifetime of the session; a compaction reseed does not advance it.
    pub(super) epoch: u64,
    /// Number of compaction reseeds observed for this session, saturating.
    pub(super) compactions: u32,
    /// Prefix length of the stored baseline BEFORE this observation.
    pub(super) prev_prefix_len: Option<usize>,
    /// Prefix length observed on this turn.
    pub(super) prefix_len: usize,
}

/// One entry: the baseline prefix the session was last seen with, plus its
/// epoch and compaction counters.
#[derive(Debug, Clone, Copy)]
struct PrefixEpochEntry {
    prefix_len: usize,
    fingerprint: u64,
    epoch: u64,
    compactions: u32,
}

/// Bounded LRU map of inbound session key -> [`PrefixEpochEntry`].
///
/// Interior-mutable via `parking_lot::Mutex` so the store can be read and
/// written from the `&self` dispatch path, held on `Router` behind an `Arc`
/// that a hot-reload SHARES with the replacement Router rather than copying
/// out of (see `Router::carry_over_prefix_epochs_from`).
pub(super) struct PrefixRewriteStore {
    entries: Mutex<LruCache<String, PrefixEpochEntry>>,
}

impl Default for PrefixRewriteStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefixRewriteStore {
    /// Construct an empty store bounded at [`PREFIX_REWRITE_CAPACITY`].
    pub(super) fn new() -> Self {
        let cap = NonZeroUsize::new(PREFIX_REWRITE_CAPACITY).expect("PREFIX_REWRITE_CAPACITY > 0");
        Self {
            entries: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Fingerprint `req`'s canonical prefix, classify it against the stored
    /// baseline for `session_key`, and update the baseline.
    ///
    /// The LRU mutex is held only for the two short state accesses -- read the
    /// baseline, then commit the classification. `fingerprint_prefix_at`
    /// serializes the whole prefix, so running it under the lock would block
    /// every other session's dispatch for the length of one request's history.
    /// Between the two accesses another observation of the SAME session may
    /// advance (or evict) the baseline; the commit therefore verifies the
    /// baseline is still the one that was classified and re-runs against the
    /// new state when it is not, so the recorded outcome always describes the
    /// state it was actually compared with.
    pub(super) fn observe(&self, session_key: &str, req: &ChatRequest) -> EpochObservation {
        let (prefix_len, fingerprint) = fingerprint_prefix(req);
        loop {
            let baseline = {
                let mut guard = self.entries.lock();
                match guard.get(session_key) {
                    Some(entry) => (entry.prefix_len, entry.fingerprint),
                    None => {
                        guard.put(
                            session_key.to_string(),
                            PrefixEpochEntry {
                                prefix_len,
                                fingerprint,
                                epoch: 0,
                                compactions: 0,
                            },
                        );
                        return EpochObservation {
                            event: None,
                            epoch: 0,
                            compactions: 0,
                            prev_prefix_len: None,
                            prefix_len,
                        };
                    }
                }
            };
            let (prev_prefix_len, prev_fingerprint) = baseline;

            // Unlocked: the expensive part. A shortening is classified by
            // length alone, so it needs no second fingerprint at all.
            let old_region_fingerprint = (prefix_len >= prev_prefix_len)
                .then(|| fingerprint_prefix_at(req, prev_prefix_len));

            let mut guard = self.entries.lock();
            let Some(entry) = guard.get_mut(session_key) else {
                // Evicted while unlocked: fall back to the first-seen path.
                continue;
            };
            if entry.prefix_len != prev_prefix_len || entry.fingerprint != prev_fingerprint {
                continue;
            }

            let event = match old_region_fingerprint {
                None => {
                    entry.compactions = entry.compactions.saturating_add(1);
                    EpochEvent::Reseeded
                }
                Some(observed) if observed == prev_fingerprint => EpochEvent::Stable,
                Some(_) => {
                    entry.epoch = entry.epoch.saturating_add(1);
                    EpochEvent::Rewritten
                }
            };
            entry.prefix_len = prefix_len;
            entry.fingerprint = fingerprint;

            return EpochObservation {
                event: Some(event),
                epoch: entry.epoch,
                compactions: entry.compactions,
                prev_prefix_len: Some(prev_prefix_len),
                prefix_len,
            };
        }
    }

    /// Number of sessions currently tracked.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// True when no sessions are tracked.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    /// Session keys in LRU order, least-recently-used FIRST. Test-only: the
    /// hot-reload carry-over shares the store by `Arc` rather than copying
    /// entries, so nothing in production needs to read the recency ordering.
    #[cfg(test)]
    pub(super) fn keys_lru_first(&self) -> Vec<String> {
        let guard = self.entries.lock();
        guard.iter().map(|(k, _)| k.clone()).rev().collect()
    }

    /// The stored baseline `(prefix_len, fingerprint)` for `session_key`.
    /// Test-only: lets a concurrency test assert the pair is self-consistent
    /// rather than inferring it from a later classification.
    #[cfg(test)]
    fn baseline_of(&self, session_key: &str) -> Option<(usize, u64)> {
        let guard = self.entries.lock();
        guard
            .peek(session_key)
            .map(|entry| (entry.prefix_len, entry.fingerprint))
    }
}

/// Message / event name of the prefix-rewrite advisory WARN. Stable,
/// greppable, closed-set.
const PREFIX_REWRITE_EVENT: &str = "cache_prefix_rewritten_in_epoch";

impl Router {
    /// Observe the client's canonical prefix ONCE per client request and stamp
    /// the classification onto `meta.prefix_epoch_event`.
    ///
    /// Called above the `'chain` loop off the ORIGINAL request, so the detector
    /// adds no per-target state to the emission path and cannot influence
    /// marker bytes: it reads the request, never mutates it, and its output is
    /// consumed only by the meta stamp and the WARN below.
    ///
    /// Keyed on `inbound_session_key` ALONE. A request without one records no
    /// state and produces no WARN (`skipped_no_session`) -- there is nothing to
    /// compare a later turn against.
    pub(super) fn observe_prefix_epoch(&self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(session_key) = req.routectl_internal.inbound_session_key.as_deref() else {
            return;
        };
        let observation = self.prefix_epoch_store.observe(session_key, req);
        let Some(event) = observation.event else {
            return;
        };
        meta.prefix_epoch_event = Some(event.code());
        // A `Rewritten` classification only exists against a stored baseline,
        // so `prev_prefix_len` is always `Some` on this arm; the pattern binds
        // it rather than defaulting a length that never occurs.
        if let (EpochEvent::Rewritten, Some(prev_prefix_len)) = (event, observation.prev_prefix_len)
        {
            self.warn_prefix_rewritten(session_key, prev_prefix_len, &observation);
        }
    }

    /// Advisory WARN for an in-epoch prefix rewrite. Edge-triggered
    /// per-process (not per-session, the settled trade for a warn-only
    /// diagnostic): the first rewrite any session shows warns, later ones stay
    /// silent and the `prefix_epoch_event` column carries the unsuppressed
    /// volume. The latch is shared across a hot-reload, so "per-process" holds
    /// literally rather than per-Router.
    ///
    /// A compaction reseed NEVER reaches here -- classifying legitimate
    /// compaction as a rewrite is exactly the false positive the detector is
    /// built to avoid.
    fn warn_prefix_rewritten(
        &self,
        session_key: &str,
        prev_prefix_len: usize,
        observation: &EpochObservation,
    ) {
        if self.prefix_rewrite_warned.swap(true, Ordering::Relaxed) {
            return;
        }
        // `inbound_session_key` is caller-controlled and never-log-raw, but the
        // WARN is only actionable if an operator can correlate the same session
        // across lines -- so it rides as a per-process-salted hash. Lengths and
        // the epoch are counts; no prompt bytes and no content fingerprint.
        tracing::warn!(
            session_key_hash = crate::log_hash::salted_log_hash(session_key),
            previous_prefix_len = prev_prefix_len,
            prefix_len = observation.prefix_len,
            epoch = observation.epoch,
            "{PREFIX_REWRITE_EVENT}",
        );
    }
}

/// Fingerprint the canonical prefix of `req`: system + tools +
/// `messages[0..len-1]`, EXCLUDING the newest turn. Returns the prefix length
/// (message count covered) alongside the fingerprint.
///
/// Pure: no clock, no I/O, no randomness, no request mutation.
pub(super) fn fingerprint_prefix(req: &ChatRequest) -> (usize, u64) {
    let prefix_len = req.messages.len().saturating_sub(1);
    (prefix_len, fingerprint_prefix_at(req, prefix_len))
}

/// Fingerprint the canonical prefix of `req` truncated to `prefix_len`
/// messages. A `prefix_len` past the end covers all messages rather than
/// panicking (defensive against a baseline recorded before a shortening).
pub(super) fn fingerprint_prefix_at(req: &ChatRequest, prefix_len: usize) -> u64 {
    let front = req.messages.get(..prefix_len).unwrap_or(&req.messages);
    let serialized = serde_json::to_string(&(req.system.as_ref(), req.tools.as_ref(), front))
        .unwrap_or_default();
    fnv1a_hash(serialized.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{CustomTool, Message, MessageContent, Role, SystemContent, ToolDef};
    use serde_json::json;
    use std::sync::Arc;

    fn user_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn req_with(texts: &[&str]) -> ChatRequest {
        ChatRequest {
            model: "opus".into(),
            messages: texts.iter().map(|t| user_msg(t)).collect(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            ..ChatRequest::default()
        }
    }

    #[test]
    fn first_observation_records_baseline_without_classifying() {
        // Arrange
        let store = PrefixRewriteStore::new();
        let req = req_with(&["a", "b", "c"]);

        // Act
        let out = store.observe("sess-1", &req);

        // Assert
        assert_eq!(out.event, None);
        assert_eq!(out.epoch, 0);
        assert_eq!(out.prev_prefix_len, None);
        assert_eq!(out.prefix_len, 2, "newest turn is excluded from the prefix");
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
    }

    #[test]
    fn unchanged_prefix_is_stable() {
        // Arrange
        let store = PrefixRewriteStore::new();
        let req = req_with(&["a", "b", "c"]);
        store.observe("sess-1", &req);

        // Act
        let out = store.observe("sess-1", &req);

        // Assert
        assert_eq!(out.event, Some(EpochEvent::Stable));
        assert_eq!(out.epoch, 0);
        assert_eq!(out.compactions, 0);
    }

    #[test]
    fn pure_append_is_stable_and_advances_the_baseline() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b"]));

        // Act: two appended turns, old region untouched.
        let first = store.observe("sess-1", &req_with(&["a", "b", "c"]));
        let second = store.observe("sess-1", &req_with(&["a", "b", "c", "d"]));

        // Assert
        assert_eq!(first.event, Some(EpochEvent::Stable));
        assert_eq!(first.prev_prefix_len, Some(1));
        assert_eq!(first.prefix_len, 2);
        assert_eq!(second.event, Some(EpochEvent::Stable));
        assert_eq!(
            second.prev_prefix_len,
            Some(2),
            "a Stable turn must advance the baseline to the longer prefix"
        );
        assert_eq!(second.epoch, 0);
    }

    #[test]
    fn rewrite_within_an_epoch_reseeds_and_increments_the_epoch() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c"]));

        // Act: message 0 rewritten, same length.
        let rewritten = store.observe("sess-1", &req_with(&["A", "b", "c"]));
        // The reseeded baseline makes the same bytes Stable on the next turn.
        let after = store.observe("sess-1", &req_with(&["A", "b", "c", "d"]));

        // Assert
        assert_eq!(rewritten.event, Some(EpochEvent::Rewritten));
        assert_eq!(rewritten.epoch, 1);
        assert_eq!(rewritten.compactions, 0);
        assert_eq!(after.event, Some(EpochEvent::Stable));
        assert_eq!(after.epoch, 1, "epoch persists across a stable turn");
    }

    #[test]
    fn each_rewrite_advances_the_epoch() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c"]));

        // Act
        let first = store.observe("sess-1", &req_with(&["A", "b", "c"]));
        let second = store.observe("sess-1", &req_with(&["B", "b", "c"]));

        // Assert
        assert_eq!(first.epoch, 1);
        assert_eq!(second.event, Some(EpochEvent::Rewritten));
        assert_eq!(second.epoch, 2);
    }

    #[test]
    fn shortened_prefix_is_a_compaction_reseed_not_a_rewrite() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c", "d", "e"]));

        // Act: a summary replaces history -- shorter AND different bytes.
        let out = store.observe("sess-1", &req_with(&["summary", "e"]));

        // Assert
        assert_eq!(
            out.event,
            Some(EpochEvent::Reseeded),
            "compaction must never be classified as a rewrite"
        );
        assert_eq!(
            out.epoch, 0,
            "a compaction reseed does not advance the epoch"
        );
        assert_eq!(out.compactions, 1);
        assert_eq!(out.prev_prefix_len, Some(4));
        assert_eq!(out.prefix_len, 1);
    }

    #[test]
    fn compaction_reseeds_the_baseline_to_the_shorter_prefix() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c", "d", "e"]));
        store.observe("sess-1", &req_with(&["summary", "e"]));

        // Act
        let out = store.observe("sess-1", &req_with(&["summary", "e", "f"]));

        // Assert
        assert_eq!(out.event, Some(EpochEvent::Stable));
        assert_eq!(out.compactions, 1);
    }

    #[test]
    fn repeated_compactions_saturate_the_counter_upward() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c", "d"]));

        // Act
        let first = store.observe("sess-1", &req_with(&["a", "b", "c"]));
        let second = store.observe("sess-1", &req_with(&["a", "b"]));

        // Assert
        assert_eq!(first.compactions, 1);
        assert_eq!(second.compactions, 2);
    }

    #[test]
    fn advisory_codes_match_the_column_contract() {
        assert_eq!(EpochEvent::Stable.code(), 0);
        assert_eq!(EpochEvent::Rewritten.code(), 1);
        assert_eq!(EpochEvent::Reseeded.code(), 2);
    }

    #[test]
    fn fingerprint_excludes_the_newest_turn() {
        // Arrange: same history, different newest turn.
        let a = req_with(&["a", "b", "c"]);
        let b = req_with(&["a", "b", "ZZZ"]);

        // Act
        let (len_a, fp_a) = fingerprint_prefix(&a);
        let (len_b, fp_b) = fingerprint_prefix(&b);

        // Assert
        assert_eq!(len_a, len_b);
        assert_eq!(fp_a, fp_b);
    }

    #[test]
    fn fingerprint_covers_system_and_tools() {
        // Arrange
        let base = req_with(&["a", "b"]);
        let other_system = ChatRequest {
            system: Some(SystemContent::Text("You are terse.".into())),
            ..base.clone()
        };
        let with_tools = ChatRequest {
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "grep".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            ..base.clone()
        };

        // Act
        let (_, fp_base) = fingerprint_prefix(&base);
        let (_, fp_system) = fingerprint_prefix(&other_system);
        let (_, fp_tools) = fingerprint_prefix(&with_tools);

        // Assert
        assert_ne!(fp_base, fp_system);
        assert_ne!(fp_base, fp_tools);
    }

    #[test]
    fn system_only_request_has_an_empty_message_prefix() {
        // Arrange
        let single = req_with(&["a"]);
        let empty = ChatRequest {
            messages: [].into(),
            ..single.clone()
        };

        // Act
        let (len_single, fp_single) = fingerprint_prefix(&single);
        let (len_empty, fp_empty) = fingerprint_prefix(&empty);

        // Assert
        assert_eq!(len_single, 0);
        assert_eq!(len_empty, 0);
        assert_eq!(
            fp_single, fp_empty,
            "with the newest turn excluded, a one-message request covers system + tools only"
        );
    }

    #[test]
    fn sessions_are_independent() {
        // Arrange
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c"]));
        store.observe("sess-2", &req_with(&["a", "b", "c"]));

        // Act
        let one = store.observe("sess-1", &req_with(&["A", "b", "c"]));
        let two = store.observe("sess-2", &req_with(&["a", "b", "c"]));

        // Assert
        assert_eq!(one.event, Some(EpochEvent::Rewritten));
        assert_eq!(two.event, Some(EpochEvent::Stable));
        assert_eq!(two.epoch, 0);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn store_evicts_the_least_recently_used_session_at_capacity() {
        // Arrange
        let store = PrefixRewriteStore::new();
        let req = req_with(&["a", "b"]);
        for i in 0..PREFIX_REWRITE_CAPACITY {
            store.observe(&format!("sess-{i}"), &req);
        }

        // Act: one session past capacity evicts the oldest.
        store.observe("sess-overflow", &req);

        // Assert
        assert_eq!(store.len(), PREFIX_REWRITE_CAPACITY);
        let evicted = store.observe("sess-0", &req);
        assert_eq!(
            evicted.event, None,
            "the least recently used session was evicted, so it is first-seen again"
        );
    }

    #[test]
    fn session_reuse_after_eviction_starts_from_a_fresh_baseline() {
        // Arrange: build an epoch, then push the session out of the LRU.
        let store = PrefixRewriteStore::new();
        store.observe("sess-1", &req_with(&["a", "b", "c"]));
        let rewritten = store.observe("sess-1", &req_with(&["A", "b", "c"]));
        assert_eq!(rewritten.epoch, 1);
        let filler = req_with(&["x", "y"]);
        for i in 0..PREFIX_REWRITE_CAPACITY {
            store.observe(&format!("filler-{i}"), &filler);
        }

        // Act
        let reused = store.observe("sess-1", &req_with(&["A", "b", "c"]));
        let follow_up = store.observe("sess-1", &req_with(&["A", "b", "c", "d"]));

        // Assert
        assert_eq!(reused.event, None);
        assert_eq!(reused.epoch, 0, "eviction resets the tracked epoch");
        assert_eq!(follow_up.event, Some(EpochEvent::Stable));
    }

    #[test]
    fn recent_use_protects_a_session_from_eviction() {
        // Arrange
        let store = PrefixRewriteStore::new();
        let req = req_with(&["a", "b"]);
        store.observe("sess-keep", &req);

        // Act: fill to capacity - 1 more sessions, touching sess-keep so it
        // stays the most recently used.
        for i in 0..PREFIX_REWRITE_CAPACITY {
            store.observe(&format!("sess-{i}"), &req);
            store.observe("sess-keep", &req);
        }
        let kept = store.observe("sess-keep", &req);

        // Assert
        assert_eq!(kept.event, Some(EpochEvent::Stable));
        assert_eq!(store.len(), PREFIX_REWRITE_CAPACITY);
    }

    #[test]
    fn concurrent_observations_of_one_session_never_tear_the_baseline() {
        // The classification fingerprint runs OUTSIDE the LRU lock, so two
        // threads observing the same session interleave a read and a commit.
        // Every recorded outcome must describe the baseline it was actually
        // compared against: the two threads alternate between two prefixes, so
        // the surviving baseline is always one of the two -- never a mix of one
        // prefix's length with the other's fingerprint.
        // Arrange
        let store = Arc::new(PrefixRewriteStore::new());
        let a = req_with(&["a", "b", "c", "d"]);
        let b = req_with(&["Z", "b", "c", "d"]);
        store.observe("sess-1", &a);

        // Act
        let handles: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|req| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let out = store.observe("sess-1", &req);
                        assert!(out.event.is_some(), "the baseline must never be lost");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("no panic under contention");
        }

        // Assert: the surviving baseline is a self-consistent
        // (prefix_len, fingerprint) pair for ONE of the two requests. A commit
        // that raced -- one prefix's length paired with the other's
        // fingerprint, or a fingerprint over a length nobody observed -- would
        // match neither.
        assert_eq!(store.len(), 1);
        let stored = store.baseline_of("sess-1").expect("baseline retained");
        assert!(
            stored == fingerprint_prefix(&a) || stored == fingerprint_prefix(&b),
            "baseline {stored:?} is neither observed pair",
        );
    }

    #[test]
    fn concurrent_observations_of_distinct_sessions_all_record_a_baseline() {
        // Arrange
        let store = Arc::new(PrefixRewriteStore::new());
        let req = req_with(&["a", "b", "c"]);

        // Act
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let store = Arc::clone(&store);
                let req = req.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        store.observe(&format!("sess-{t}-{i}"), &req);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("no panic under contention");
        }

        // Assert
        assert_eq!(store.len(), 100);
    }
}
