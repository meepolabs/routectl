//! Bounded, in-memory store of per-session anchor records.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lru::LruCache;
use parking_lot::Mutex;
use routectl_core::ChatRequest;
use routectl_router::K_SESSION_CAPACITY;

use super::{AnchorKey, AnchorLane, AnchorRecord, RequestIdentity};

/// Bound on the number of anchored (session, requested model) pairs.
/// Matches the session LRU used for K windows and sticky pins, so a
/// session alive there can also hold an anchor.
pub const ANCHOR_CAPACITY: usize = K_SESSION_CAPACITY;

/// A turn's place in one store's order, for one key, reserved at admission
/// before any hashing so a slow-to-measure older request stays older.
///
/// Owns a handle to the store that issued it and the key it was reserved
/// for, so the eventual publication can only land in that store under that
/// key.
#[derive(Debug)]
pub struct TurnTicket {
    store: Arc<Shared>,
    key: AnchorKey,
    turn: u64,
}

impl TurnTicket {
    /// The session's current record, to evaluate the measured request
    /// against. `None` when the session is cold.
    pub fn current_record(&self) -> Option<Arc<AnchorRecord>> {
        self.store.get(&self.key)
    }

    /// Measure the request this turn carries. The only way to obtain a
    /// [`PendingAnchor`], so measurement always follows the reservation.
    pub fn measure(self, req: &ChatRequest, prior_message_count: Option<usize>) -> PendingAnchor {
        PendingAnchor {
            identity: RequestIdentity::measure(req, prior_message_count),
            ticket: self,
        }
    }
}

/// A measured request waiting for its terminal outcome. Not `Clone`:
/// [`PendingAnchor::settle`] consumes it, so a turn settles at most once.
/// Dropping it unsettled publishes nothing.
///
/// ```
/// # use routectl_cli::ingress::anthropic::context_anchor::*;
/// # use routectl_core::ChatRequest;
/// let store = ContextAnchorStore::new();
/// let key = AnchorKey::new("session", "model").unwrap();
/// let pending = store.reserve_turn(key).measure(&ChatRequest::default(), None);
/// let _ = pending.settle(TurnOutcome::Failed);
/// ```
///
/// Settling twice does not compile:
///
/// ```compile_fail,E0382
/// # use routectl_cli::ingress::anthropic::context_anchor::*;
/// # use routectl_core::ChatRequest;
/// let store = ContextAnchorStore::new();
/// let key = AnchorKey::new("session", "model").unwrap();
/// let pending = store.reserve_turn(key).measure(&ChatRequest::default(), None);
/// let _ = pending.settle(TurnOutcome::Failed);
/// let _ = pending.settle(TurnOutcome::Failed);
/// ```
///
/// Nor does copying a measurement to settle it again:
///
/// ```compile_fail,E0599
/// # use routectl_cli::ingress::anthropic::context_anchor::*;
/// # use routectl_core::ChatRequest;
/// let store = ContextAnchorStore::new();
/// let key = AnchorKey::new("session", "model").unwrap();
/// let pending = store.reserve_turn(key).measure(&ChatRequest::default(), None);
/// let copy = pending.clone();
/// let _ = pending.settle(TurnOutcome::Failed);
/// let _ = copy.settle(TurnOutcome::Failed);
/// ```
#[derive(Debug)]
pub struct PendingAnchor {
    ticket: TurnTicket,
    identity: RequestIdentity,
}

impl PendingAnchor {
    /// The measured identity, for checking against the session's anchor.
    pub const fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    /// Publish this turn into the store and under the key it was reserved
    /// for, if `outcome` is a measured success and no newer turn could
    /// already anchor the session.
    pub fn settle(self, outcome: TurnOutcome) -> SettleOutcome {
        let Self { ticket, identity } = self;
        let Some(record) = measured_record(&identity, ticket.turn, outcome) else {
            return SettleOutcome::NotPublished;
        };
        ticket.store.publish(ticket.key, record)
    }
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The stream completed naturally.
    Completed {
        /// Lane that actually served the turn.
        served_lane: AnchorLane,
        /// Provider-reported cache-inclusive input total, when reported.
        cache_inclusive_input: Option<u64>,
    },
    /// The turn ended in an error.
    Failed,
    /// The client went away or the stream was interrupted.
    Cancelled,
}

/// What settling a turn did to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// A new record now anchors the session.
    Published,
    /// A newer turn already anchors the session; nothing changed.
    Superseded,
    /// The outcome carries no usable measurement; nothing changed.
    NotPublished,
}

/// Bounded LRU of [`AnchorKey`] -> immutable [`AnchorRecord`].
///
/// Turn order is global to the store. `evicted_through` is the newest turn
/// any evicted record carried: a turn at or below it may be older than a
/// record the store has forgotten, so it is refused rather than allowed to
/// republish stale state. Only turns in flight across an eviction pay this.
///
/// Handler flow: [`ContextAnchorStore::reserve_turn`] at admission, then
/// [`TurnTicket::current_record`] and [`TurnTicket::measure`], then
/// [`super::evaluate`] against [`PendingAnchor::identity`], and finally
/// [`PendingAnchor::settle`] with the turn's outcome.
pub struct ContextAnchorStore {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<StoreState>,
    next_turn: AtomicU64,
}

#[derive(Debug)]
struct StoreState {
    records: LruCache<AnchorKey, Arc<AnchorRecord>>,
    evicted_through: Option<u64>,
}

impl Default for ContextAnchorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextAnchorStore {
    /// An empty store bounded at the session LRU capacity.
    pub fn new() -> Self {
        let cap = NonZeroUsize::new(ANCHOR_CAPACITY).expect("ANCHOR_CAPACITY > 0");
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(StoreState {
                    records: LruCache::new(cap),
                    evicted_through: None,
                }),
                next_turn: AtomicU64::new(0),
            }),
        }
    }

    /// The current record for `key`, if any.
    pub fn get(&self, key: &AnchorKey) -> Option<Arc<AnchorRecord>> {
        self.shared.get(key)
    }

    /// Reserve the next turn in this store's order for `key`. Call at
    /// admission, before measuring the request.
    pub fn reserve_turn(&self, key: AnchorKey) -> TurnTicket {
        TurnTicket {
            turn: self.shared.next_turn.fetch_add(1, Ordering::Relaxed),
            store: Arc::clone(&self.shared),
            key,
        }
    }

    /// Number of anchored sessions.
    pub fn len(&self) -> usize {
        self.shared.state.lock().records.len()
    }

    /// True when no session is anchored.
    pub fn is_empty(&self) -> bool {
        self.shared.state.lock().records.is_empty()
    }
}

impl Shared {
    fn get(&self, key: &AnchorKey) -> Option<Arc<AnchorRecord>> {
        self.state.lock().records.get(key).cloned()
    }

    fn publish(&self, key: AnchorKey, record: AnchorRecord) -> SettleOutcome {
        let record = Arc::new(record);
        let mut state = self.state.lock();
        let newer_held = state
            .records
            .peek(&key)
            .is_some_and(|held| held.turn > record.turn);
        let maybe_newer_evicted = state
            .evicted_through
            .is_some_and(|through| record.turn <= through);
        if newer_held || maybe_newer_evicted {
            return SettleOutcome::Superseded;
        }
        // `push` also hands back the entry it replaced for this same key;
        // only a different key leaving the cache is an eviction.
        if let Some((evicted_key, evicted)) = state.records.push(key.clone(), record)
            && evicted_key != key
        {
            state.evicted_through = Some(
                state
                    .evicted_through
                    .map_or(evicted.turn, |through| through.max(evicted.turn)),
            );
        }
        SettleOutcome::Published
    }
}

/// The record a turn would publish: only a natural completion that
/// reported its input and whose request could be fingerprinted.
fn measured_record(
    identity: &RequestIdentity,
    turn: u64,
    outcome: TurnOutcome,
) -> Option<AnchorRecord> {
    let TurnOutcome::Completed {
        served_lane,
        cache_inclusive_input: Some(actual_input),
    } = outcome
    else {
        return None;
    };
    Some(AnchorRecord {
        message_count: identity.message_count(),
        prefix_digest: identity.digest()?,
        normalized_estimate: identity.normalized_estimate(),
        actual_input,
        lane: served_lane,
        turn,
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
