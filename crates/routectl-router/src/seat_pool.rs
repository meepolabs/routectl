//! Seat-pool dispatch across the members of a `[pools.<name>]` block.
//!
//! A model whose `[models.X] provider` value names a pool dispatches across
//! that pool's member provider entries. The seat set is fixed at build time:
//! the factory compiles one [`SeatTarget`] (a member's own provider instance)
//! per usable member, computed ONCE per pool and shared by every model naming
//! it.
//!
//! At request time the router asks [`seat_order_for_request`] for the order
//! in which to walk those seats. The seats then slot into the existing
//! fallback chain as ordinary dispatch hops -- the per-target circuit
//! breaker, retry caps, probe fast-fail, and the `Retry-After` park all key
//! off the per-seat, per-model state key, so seat rotation and cooling are
//! delivered by machinery that already exists. This module owns only the
//! dispatch-order glue and the round-robin counter, keeping it out of the
//! oversized `router.rs`.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lru::LruCache;
use parking_lot::Mutex;
use routectl_auth::SecretRef;
use routectl_core::Provider;

use crate::config::SeatSelection;
use crate::quota::placement::{QuotaDecision, SeatQuota};
use crate::runtime_state::{CapacitySnapshot, CircuitPhase};

/// One credential seat of a pooled model: the member provider entry's name
/// plus the provider instance built from that member's OWN credential
/// reference. Built once per pool at startup (the seat set is fixed) and
/// cloned by reference (the `Arc`s) on every dispatch.
///
/// Deliberately carries NO model-scoped key. One seat set is shared by every
/// model that names the pool, so a `state_key` field would have to pick one
/// model's key and hand it to the others; the per-model key is derived where
/// the model is known, through [`SeatTarget::state_key_for`].
#[derive(Clone)]
pub struct SeatTarget {
    /// The seat's `[providers]` table key -- the pool member this seat
    /// dispatches. Every per-provider config lookup on the dispatch path
    /// (runtime policy, class overrides, header extras, beta floor) resolves
    /// against THIS name rather than the model's `provider` value, which for
    /// a pool-backed model names the pool and not a provider entry.
    pub provider_name: String,
    /// Provider instance built from this member's own credential reference.
    pub provider: Arc<dyn Provider>,
    /// Source `SecretRef` for this specific seat. Retained for the
    /// account-scoped quota key and seat identity; the 401 self-heal does NOT
    /// read this field -- it works through the seat's own `ManagedToken`,
    /// which already refreshes the correct credential.
    pub auth_secret_ref: Option<SecretRef>,
}

impl SeatTarget {
    /// This seat's key into `Router.state` under `nickname`: its own circuit
    /// breaker and RPM bucket entry.
    ///
    /// Model-scoped by design: per-model breaker quarantine is a shipped
    /// contract, so a flaky model-on-account combination must not open the
    /// breaker for healthy sibling models on the same account. Stable across
    /// a Router rebuild, so `carry_over_runtime_state_from` matches a
    /// surviving seat and preserves its counters / park.
    #[must_use]
    pub fn state_key_for(&self, nickname: &str) -> String {
        seat_state_key(nickname, Some(&self.provider_name))
    }
}

impl std::fmt::Debug for SeatTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatTarget")
            .field("provider_name", &self.provider_name)
            .field("provider_id", &self.provider.id())
            .field(
                "auth_secret_scheme",
                &self.auth_secret_ref.as_ref().map(secret_ref_scheme),
            )
            .finish()
    }
}

/// The leak-safe scheme token of a seat's credential reference. An
/// ALLOWLIST, not a prefix split: rendering the reference itself would put a
/// `file://` key path (and, for a reference constructed in code rather than
/// parsed, inline secret material) into every log line that debug-formats a
/// seat.
const fn secret_ref_scheme(secret_ref: &SecretRef) -> &'static str {
    match secret_ref {
        SecretRef::OAuth { .. } => "oauth://",
        SecretRef::Env(_) => "env://",
        SecretRef::File(_) => "file://",
        SecretRef::Literal(_) => "literal:",
        _ => "unknown",
    }
}

/// Derive the runtime-state key for one seat of a pooled model.
///
/// The DEFAULT seat (label `None`) keys as the bare `nickname` -- so a
/// single-target model keys by nickname exactly as it always has. A NAMED
/// seat keys as `"{nickname}#{label}"`, mirroring the established
/// `provider#label` convention used by `oauth::seat_key` and `SecretRef`'s
/// `Display`, which keeps the key operator-readable in logs.
///
/// Collision boundary: a labeled-seat key collides with a real model
/// nickname only if an operator declares a SEPARATE `[models.X]` whose
/// nickname is literally `"{nickname}#{label}"` AND that label exists as a
/// seat of the pooled `nickname`. Since labeled-seat keys are only minted
/// for genuinely multi-seat oauth pools, this requires a deliberately
/// adversarial config; the bare-nickname default-seat key (the common
/// single-seat case) can never collide.
pub fn seat_state_key(nickname: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("{nickname}#{label}"),
        None => nickname.to_string(),
    }
}

/// Derive the persistable credential identity of one dispatch target from
/// its source `SecretRef`: the `provider#label` seat key (bare `provider`
/// for the default seat) that `oauth::seat_key` mints, so a usage row
/// partitions by ACCOUNT rather than by model -- several models sharing one
/// OAuth account collapse to one identity.
///
/// Only the OAuth arm yields an identity. `file://` renders a filesystem
/// path and `env://` a variable name; neither may be persisted in the usage
/// ledger, so `None` is the correct -- not merely the conservative --
/// answer for every other arm.
pub fn seat_identity(secret_ref: Option<&SecretRef>) -> Option<String> {
    match secret_ref {
        Some(SecretRef::OAuth { provider, label }) => {
            Some(routectl_auth::oauth::seat_key(provider, label.as_deref()))
        }
        _ => None,
    }
}

/// Per-pool round-robin cursor set. Holds one [`AtomicUsize`] per pooled
/// model nickname; `RoundRobin` selection advances the cursor by one per
/// request via `fetch_add`. Lives on the `Router` alongside `state`.
///
/// The cursor is deliberately NOT carried over on a Router rebuild -- a
/// reset to seat 0 on hot-reload is benign at single-operator scale (the
/// only cost is one request landing on the default seat instead of the
/// next-in-rotation seat). `FillFirst` pools need no cursor and are never
/// inserted here.
#[derive(Debug, Default)]
pub struct RoundRobinCursors {
    cursors: BTreeMap<String, AtomicUsize>,
}

impl RoundRobinCursors {
    /// Register a round-robin cursor for a pooled nickname. Idempotent:
    /// re-registering keeps the existing cursor. Call at install time for
    /// each pooled model whose `seat_selection` is `RoundRobin`.
    pub fn register(&mut self, nickname: &str) {
        self.cursors
            .entry(nickname.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
    }

    /// Return the starting offset for this request and advance the cursor.
    /// `None` when no cursor is registered for `nickname` (the pool is
    /// `FillFirst`, or the nickname is not pooled) -- callers treat that as
    /// "start at seat 0, fixed order".
    fn next_start(&self, nickname: &str) -> Option<usize> {
        self.cursors
            .get(nickname)
            .map(|c| c.fetch_add(1, Ordering::Relaxed))
    }
}

/// Pinned-seat record for one inbound conversation. Carries the seat's
/// stable `state_key` plus a one-time overflow-repin marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPin {
    pub(crate) state_key: String,
    /// True once this session has been migrated off its birth seat by a
    /// one-time overflow-repin. Caps migration at one and prevents an
    /// A->B->A flap when the original seat recovers.
    pub(crate) repinned: bool,
}

/// Maximum number of session->seat pins held at once. A bounded LRU keeps
/// the map at a few-thousand entries so memory stays flat under churn; the
/// least-recently-used pin is evicted when a new conversation arrives at
/// capacity. An evicted conversation simply re-pins (one cold miss) on its
/// next turn, so the bound is safe.
const STICKY_PIN_CAPACITY: usize = 4096;

/// Bounded LRU map of inbound conversation session key -> pinned [`SeatPin`]
/// (the seat's STABLE `state_key` plus the one-time overflow-repin marker;
/// see [`SeatTarget::state_key_for`] / [`seat_state_key`]). A positional seat
/// index is deliberately NOT stored: indices can shift on a Router rebuild,
/// whereas `state_key` is stable across reloads.
///
/// Wraps a `parking_lot::Mutex<LruCache<..>>` for interior mutability so the
/// map is read/written on the `&self` dispatch path.
///
/// UNLIKE [`RoundRobinCursors`], this whole struct is held behind an `Arc` on
/// `Router` and SHARED (not copied) across a hot-reload rebuild (see
/// `Router::carry_over_sticky_from`). Dropping pins mid-incident would
/// scatter every live conversation off its warm-cache seat -- a mass
/// cold-miss across all in-flight conversations -- so the carry-over is
/// mandatory, not benign. Sharing also means a pin written through the
/// outgoing Router in the window between the carry-over and the swap lands
/// in the same map the incoming Router reads, rather than in a snapshot the
/// swap is about to discard.
pub struct StickyPins {
    pins: Mutex<LruCache<String, SeatPin>>,
    /// Deterministic anti-herd tiebreak counter. When a birth pick finds
    /// several equally-least-loaded seats, the chooser rotates across them
    /// by `tiebreak % tied.len()` so concurrent fan-out misses reading the
    /// same capacity snapshot spread over distinct seats instead of herding
    /// onto one. SHARED across a hot-reload along with the rest of
    /// `StickyPins` (see the struct doc): the counter now keeps advancing
    /// rather than resetting to 0 on a rebuild. That is benign in the other
    /// direction from a reset -- it only shifts which tied seat a rotation
    /// lands on, never a pin -- so persistence needs no defense, only this
    /// note for the next reader expecting the old reset.
    tiebreak: AtomicUsize,
}

impl Default for StickyPins {
    fn default() -> Self {
        Self::new()
    }
}

impl StickyPins {
    /// Construct an empty pin map bounded at [`STICKY_PIN_CAPACITY`].
    pub(crate) fn new() -> Self {
        let cap = NonZeroUsize::new(STICKY_PIN_CAPACITY).expect("STICKY_PIN_CAPACITY > 0");
        Self {
            pins: Mutex::new(LruCache::new(cap)),
            tiebreak: AtomicUsize::new(0),
        }
    }

    /// Read the [`SeatPin`] for `session_key`, marking it most-recently used
    /// under the lock. Marking MRU on read keeps an active conversation's pin
    /// hot so it survives LRU eviction while it is still being served. `None`
    /// when the session has no pin.
    pub(crate) fn get(&self, session_key: &str) -> Option<SeatPin> {
        self.pins.lock().get(session_key).cloned()
    }

    /// Return the next deterministic tiebreak value and advance the counter.
    /// Seeds the anti-herd rotation in [`sticky_least_loaded_order`].
    pub(crate) fn next_tiebreak(&self) -> usize {
        self.tiebreak.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert or update the pin for `session_key`, marking it most-recently
    /// used. Single setter: a birth pick passes `repinned: false`, a one-time
    /// overflow-repin passes `repinned: true`.
    pub(crate) fn put(&self, session_key: &str, pin: SeatPin) {
        self.pins.lock().put(session_key.to_string(), pin);
    }

    /// Number of pins currently held. Test read surface only: production
    /// never asks how many conversations are pinned, only whether a given
    /// one is.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pins.lock().len()
    }

    /// True when no session holds a pin. Test read surface only, paired
    /// with [`StickyPins::len`].
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pins.lock().is_empty()
    }
}

/// The walk order for a KEYLESS request on a sticky-configured pool, ordered by
/// remaining subscription budget.
///
/// A keyless request carries no session identity, so there is no warm prompt
/// cache to protect and nothing that outranks the budget: cache preservation is
/// the only thing that ever outranks quota fairness, and a request with no cache
/// has none to preserve. So it places by cap, and it mints no pin -- there is no
/// key to pin under.
///
/// Shares the partition with the keyed birth pick rather than restating it, so
/// keyless and keyed can never disagree about what "below cap" means. The chosen
/// group leads the walk; every remaining eligible seat follows in the fixed
/// order, so the existing fall-through behavior is preserved rather than
/// replaced. When the partition declines -- all readings unknown, a mix of
/// capped-known and unknown, the switch off -- this returns `None` and the
/// caller keeps the unchanged fill-first walk.
pub fn keyless_quota_order(
    seat_count: usize,
    snapshots: &[CapacitySnapshot],
    quota: &[SeatQuota],
    tiebreak: usize,
    decision: &mut QuotaDecision,
) -> Option<Vec<usize>> {
    if seat_count <= 1 {
        return None;
    }
    // The SAME eligibility the keyed pick uses -- dispatchability AND the
    // Closed-over-HalfOpenReady preference. Filtering only dispatchability here
    // would let a keyless request lead with a half-open probe that happened to
    // have more budget than a healthy sibling.
    let all: Vec<usize> = (0..seat_count).collect();
    let preferred = eligible_candidates(&all, snapshots);
    let tied = crate::quota::placement::restrict_by_quota(&preferred, quota, decision)?;
    let lead = tied[tiebreak % tied.len()];
    let mut order = Vec::with_capacity(seat_count);
    order.push(lead);
    order.extend((0..seat_count).filter(|&i| i != lead));
    Some(order)
}

/// Resolve the per-request seat walk order for a pooled model.
///
/// `FillFirst` (or any non-pooled model, where `cursors` has no entry):
/// returns the fixed seat order as built -- default seat first, then sorted
/// labels -- so the chain walk drains one seat until it cools/parks before
/// falling to the next, maximizing prompt-cache locality.
///
/// `RoundRobin`: rotates the STARTING seat by one per request (the cursor's
/// `fetch_add` modulo seat count), then walks the remaining seats in order.
/// The relative order after the start offset is preserved so cooled seats
/// still fall through predictably.
///
/// Returns indices into the model's seat slice; the caller maps them back
/// to [`SeatTarget`]s. An empty or single-element seat set yields the
/// trivial order with no cursor traffic.
pub fn seat_order_for_request(
    nickname: &str,
    seat_count: usize,
    selection: SeatSelection,
    cursors: &RoundRobinCursors,
) -> Vec<usize> {
    if seat_count <= 1 {
        return (0..seat_count).collect();
    }
    let start = match selection {
        SeatSelection::RoundRobin => cursors.next_start(nickname).unwrap_or(0) % seat_count,
        SeatSelection::FillFirst => 0,
        // Keyless / single-seat StickyLeastLoaded resolves here and walks the
        // fixed fill-first order (start seat 0). The keyed sticky-least-loaded
        // ordering lives in `Router::sticky_seat_order`, which needs the
        // inbound session key and per-seat capacity that this pure fn does not
        // receive.
        SeatSelection::StickyLeastLoaded => 0,
    };
    (0..seat_count).map(|i| (start + i) % seat_count).collect()
}

/// Build a walk order with `home` first, then every other seat in ascending
/// index order. The home seat is a best-effort cache-locality hint; the
/// ascending tail preserves the fill-first fallback so the existing
/// sequential gate + chain fallback walk stays authoritative.
fn order_home_first(home: usize, seat_count: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(seat_count);
    order.push(home);
    order.extend((0..seat_count).filter(|&i| i != home));
    order
}

/// Outcome of a single sticky-least-loaded selection. The walk-order hint is
/// returned alongside; this enum tells the caller whether (and how) to update
/// the pin. It also maps to the fixed-vocabulary `selection_decision` token
/// recorded in the usage ledger, so each variant is an observable decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// Pin miss (birth): caller pins `home` with `repinned: false`.
    Birth { home: usize },
    /// Home healthy, OR already-repinned, OR no healthy sibling: no pin write.
    Stay { home: usize },
    /// First migration off a non-dispatchable home: caller pins the new
    /// `home` with `repinned: true`.
    OverflowRepin { home: usize },
    /// Pin miss with no dispatchable seat: fill-first order, no pin.
    DeferNoHealthy,
}

/// Pick the least-loaded HEALTHY index among `candidates`: dispatchable only,
/// Closed preferred over HalfOpenReady, then the subscription-quota partition
/// when it decides, otherwise max rpm_available (None=+inf), ties broken
/// deterministically by `tiebreak`. None if no candidate is dispatchable.
/// `candidates` are indices into `snapshots`.
///
/// `quota` is index-aligned with `snapshots` and EMPTY whenever quota
/// contributes nothing -- the kill switch off, a provider curating no short
/// window, a migration pick. An empty slice makes
/// [`restrict_by_quota`](crate::quota::placement::restrict_by_quota) answer
/// `None`, so the RPM headroom ranking below runs exactly as it did before
/// quota existed. `decision` records which arm ran, for the caller's counters.
/// The candidate set every seat pick starts from: dispatchable seats, then the
/// Closed-over-HalfOpenReady health preference.
///
/// THE one place either layer is expressed. Both the keyed birth pick and the
/// keyless walk go through it, so neither can quietly apply a different notion
/// of "eligible" -- a keyless request that skipped the health preference would
/// lead with a half-open probe over a healthy sibling purely because the probe
/// had more budget left, handing traffic to a breaker still recovering.
/// Empty when nothing is dispatchable.
fn eligible_candidates(candidates: &[usize], snapshots: &[CapacitySnapshot]) -> Vec<usize> {
    let dispatchable: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| {
            snapshots
                .get(i)
                .is_some_and(CapacitySnapshot::is_dispatchable)
        })
        .collect();
    if dispatchable.is_empty() {
        return dispatchable;
    }
    // Health preference: if any candidate is fully Closed, do NOT keep a
    // HalfOpenReady seat.
    let has_closed = dispatchable
        .iter()
        .any(|&i| snapshots[i].circuit == CircuitPhase::Closed);
    if has_closed {
        dispatchable
            .into_iter()
            .filter(|&i| snapshots[i].circuit == CircuitPhase::Closed)
            .collect()
    } else {
        dispatchable
    }
}

fn pick_least_loaded(
    candidates: &[usize],
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
    quota: &[SeatQuota],
    decision: &mut QuotaDecision,
) -> Option<usize> {
    let preferred = eligible_candidates(candidates, snapshots);
    if preferred.is_empty() {
        return None;
    }

    // The subscription-quota partition supersedes the headroom ranking below
    // for a pool whose budget is known, and ONLY that ranking: the
    // dispatchability filter and the health preference above have already run
    // and are not re-derived, and the anti-herd tiebreak below still breaks
    // the tie. When the partition declines -- every reading unknown, a mix of
    // capped-known and unknown, or no readings at all -- the headroom path
    // runs untouched.
    if let Some(tied) = crate::quota::placement::restrict_by_quota(&preferred, quota, decision) {
        return Some(tied[tiebreak % tied.len()]);
    }

    // Least loaded = most available RPM headroom. Treat unlimited (`None`) as
    // +infinity so an unlimited seat always wins the headroom comparison.
    let headroom = |idx: usize| -> f64 { snapshots[idx].rpm_available.unwrap_or(f64::INFINITY) };
    let max_headroom = preferred
        .iter()
        .map(|&i| headroom(i))
        .fold(f64::NEG_INFINITY, f64::max);
    let tied: Vec<usize> = preferred
        .into_iter()
        .filter(|&i| headroom(i) == max_headroom)
        .collect();

    // Anti-herd tiebreak: rotate deterministically across the tied seats.
    Some(tied[tiebreak % tied.len()])
}

/// Pure seat-selection math for `StickyLeastLoaded`. Decides the per-request
/// walk order, the [`SelectionOutcome`] (whether/how to update the pin), and
/// which arm of the subscription-quota partition ran.
///
/// `snapshots` is index-aligned with the seat slice (`len == seat_count`),
/// gathered for the hit AND miss path now (the overflow check reads the
/// pinned home's snapshot). `pinned_index` is the seat this session is pinned
/// to (`Some`), or `None` for a birth pick / pin miss. `already_repinned` is
/// the pin's one-time overflow marker. `tiebreak` seeds the anti-herd
/// rotation among equally-least-loaded candidates. `quota` is index-aligned
/// with the seats and empty whenever quota contributes nothing.
///
/// One-time overflow-repin with hysteresis: a pinned home that goes
/// non-dispatchable is migrated ONCE to the least-loaded healthy sibling and
/// never chased further -- we never compare against or return to the original
/// seat, so a recovered original cannot pull the session back (no A->B->A
/// flap).
///
/// QUOTA REACHES THE BIRTH PICK ONLY. A healthy pin is kept without quota
/// being consulted at all, and the migration pick below is made on health and
/// RPM exactly as before, because the trigger for a migration is a seat that
/// cannot serve -- never a soft cap. A cap that could move a warm session
/// would forfeit the prompt-cache locality the pin exists to hold, which is
/// the one thing this ordering refuses to do.
pub fn sticky_least_loaded_order(
    seat_count: usize,
    pinned_index: Option<usize>,
    already_repinned: bool,
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
    quota: &[SeatQuota],
) -> (Vec<usize>, SelectionOutcome, QuotaDecision) {
    let mut decision = QuotaDecision::Dormant;
    if seat_count <= 1 {
        return (
            (0..seat_count).collect(),
            SelectionOutcome::Stay { home: 0 },
            decision,
        );
    }

    let n = seat_count;
    if let Some(home) = pinned_index {
        // Healthy home: keep serving it; no pin write, no quota read.
        if snapshots[home].is_dispatchable() {
            return (
                order_home_first(home, n),
                SelectionOutcome::Stay { home },
                decision,
            );
        }
        // Already migrated once: do NOT chase further. The gate + fallback
        // walk handles the dead home for this request. One-time cap.
        if already_repinned {
            return (
                order_home_first(home, n),
                SelectionOutcome::Stay { home },
                decision,
            );
        }
        // First migration: pick the least-loaded healthy SIBLING, on health
        // and RPM alone.
        let siblings: Vec<usize> = (0..n).filter(|&i| i != home).collect();
        match pick_least_loaded(&siblings, snapshots, tiebreak, &[], &mut decision) {
            Some(new_home) => (
                order_home_first(new_home, n),
                SelectionOutcome::OverflowRepin { home: new_home },
                decision,
            ),
            // No healthy sibling: nowhere better. Stay (no flap, no pin);
            // the gate handles the dead home.
            None => (
                order_home_first(home, n),
                SelectionOutcome::Stay { home },
                decision,
            ),
        }
    } else {
        // Birth pick: candidate set = all seats.
        let all: Vec<usize> = (0..n).collect();
        match pick_least_loaded(&all, snapshots, tiebreak, quota, &mut decision) {
            Some(home) => (
                order_home_first(home, n),
                SelectionOutcome::Birth { home },
                decision,
            ),
            // All parked/exhausted: home 0, fill-first order, no pin. A
            // later turn re-picks once a seat is healthy.
            None => (
                order_home_first(0, n),
                SelectionOutcome::DeferNoHealthy,
                decision,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SeatTarget` whose credential ref renders a filesystem path -- the
    /// arm that makes the leak concrete rather than hypothetical.
    fn seat_with_file_ref() -> SeatTarget {
        struct NullProvider;

        #[async_trait::async_trait]
        impl routectl_core::Provider for NullProvider {
            fn id(&self) -> &'static str {
                "anthropic-api:member"
            }
            fn normalize_request(
                &self,
                _: &routectl_core::ChatRequest,
            ) -> routectl_core::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            fn normalize_response(
                &self,
                _: serde_json::Value,
            ) -> routectl_core::Result<routectl_core::ChatResponse> {
                Err(routectl_core::Error::normalize_response("member", "unused"))
            }
            async fn complete(
                &self,
                _: routectl_core::ChatRequest,
            ) -> routectl_core::Result<routectl_core::ChatResponse> {
                unreachable!("this seat is never dispatched")
            }
            async fn stream(
                &self,
                _: routectl_core::ChatRequest,
            ) -> routectl_core::Result<
                futures::stream::BoxStream<
                    'static,
                    routectl_core::Result<routectl_core::ChatChunk>,
                >,
            > {
                unreachable!("this seat is never dispatched")
            }
        }

        SeatTarget {
            provider_name: "anthropic-work".to_string(),
            provider: Arc::new(NullProvider),
            auth_secret_ref: Some(SecretRef::File(std::path::PathBuf::from(
                "/var/secrets/anthropic-work.key",
            ))),
        }
    }

    #[test]
    fn seat_target_debug_redacts_the_credential_reference() {
        // A seat is debug-formatted wherever a dispatch target or resolved
        // model is, so rendering the ref publishes the operator's key PATH
        // into ordinary diagnostics. Only the leak-safe scheme token survives.
        let rendered = format!("{:?}", seat_with_file_ref());

        assert!(
            !rendered.contains("/var/secrets"),
            "the credential path must not appear: {rendered}"
        );
        assert!(
            !rendered.contains("anthropic-work.key"),
            "the key filename must not appear: {rendered}"
        );
        assert!(
            rendered.contains("file://"),
            "the leak-safe scheme token identifies the source: {rendered}"
        );
        assert!(
            rendered.contains("anthropic-work"),
            "the member name is config, not credential, and stays: {rendered}"
        );
    }

    #[test]
    fn a_seats_credential_reference_never_reaches_a_state_key() {
        // The other surface a seat's identity ships on: the runtime-state key
        // is persisted (it becomes the usage ledger's lane key) and logged, so
        // it must be derived from the MEMBER name, never the credential ref.
        let key = seat_with_file_ref().state_key_for("opus");

        assert_eq!(key, "opus#anthropic-work");
        assert!(!key.contains("/var/secrets"), "{key}");
        assert!(!key.contains("file://"), "{key}");
    }

    #[test]
    fn seat_state_key_default_seat_is_bare_nickname() {
        // Back-compat pin: the default seat (label None) keys as the bare
        // nickname, so a single-seat pool is identical to a non-pooled
        // model (state_key == nickname).
        assert_eq!(seat_state_key("opus", None), "opus");
    }

    #[test]
    fn seat_state_key_labeled_seat_is_hash_joined() {
        assert_eq!(seat_state_key("opus", Some("seat-b")), "opus#seat-b");
    }

    #[test]
    fn seat_identity_of_unlabeled_oauth_ref_is_bare_provider() {
        // Arrange
        let sr = SecretRef::parse("oauth://codex").expect("parse oauth ref");

        // Act
        let identity = seat_identity(Some(&sr));

        // Assert: several models over one account collapse to one identity.
        assert_eq!(identity, Some("codex".to_string()));
    }

    #[test]
    fn seat_identity_of_labeled_oauth_ref_is_hash_joined_seat_key() {
        // Arrange
        let sr = SecretRef::parse("oauth://anthropic#a").expect("parse oauth ref");

        // Act
        let identity = seat_identity(Some(&sr));

        // Assert
        assert_eq!(identity, Some("anthropic#a".to_string()));
    }

    #[test]
    fn seat_identity_of_env_ref_is_none() {
        // Arrange: an env:// ref renders a variable name, which must never
        // reach the usage ledger.
        let sr = SecretRef::parse("env://ANTHROPIC_API_KEY").expect("parse env ref");

        // Act / Assert
        assert_eq!(seat_identity(Some(&sr)), None);
    }

    #[test]
    fn seat_identity_of_file_ref_is_none() {
        // Arrange: a file:// ref renders a filesystem path, likewise barred
        // from the ledger.
        let sr = SecretRef::parse("file:///etc/routectl/key").expect("parse file ref");

        // Act / Assert
        assert_eq!(seat_identity(Some(&sr)), None);
    }

    #[test]
    fn seat_identity_of_absent_ref_is_none() {
        // Arrange / Act / Assert: a target with no credential ref at all.
        assert_eq!(seat_identity(None), None);
    }

    #[test]
    fn fill_first_order_is_fixed_zero_start() {
        // Arrange
        let cursors = RoundRobinCursors::default();

        // Act: three FillFirst calls.
        let a = seat_order_for_request("opus", 3, SeatSelection::FillFirst, &cursors);
        let b = seat_order_for_request("opus", 3, SeatSelection::FillFirst, &cursors);

        // Assert: stable, default-first order across requests.
        assert_eq!(a, vec![0, 1, 2]);
        assert_eq!(b, vec![0, 1, 2]);
    }

    #[test]
    fn round_robin_advances_start_seat_per_request() {
        // Arrange
        let mut cursors = RoundRobinCursors::default();
        cursors.register("opus");

        // Act: four requests over a 3-seat pool.
        let r0 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r1 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r2 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r3 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);

        // Assert: the starting seat advances by one each request and wraps.
        assert_eq!(r0, vec![0, 1, 2]);
        assert_eq!(r1, vec![1, 2, 0]);
        assert_eq!(r2, vec![2, 0, 1]);
        assert_eq!(r3, vec![0, 1, 2]);
    }

    #[test]
    fn round_robin_without_registered_cursor_falls_back_to_fixed() {
        // A RoundRobin model with no registered cursor (defensive) walks
        // the fixed order rather than panicking.
        let cursors = RoundRobinCursors::default();
        let order = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn single_seat_order_is_trivial() {
        let cursors = RoundRobinCursors::default();
        assert_eq!(
            seat_order_for_request("opus", 1, SeatSelection::RoundRobin, &cursors),
            vec![0]
        );
        assert_eq!(
            seat_order_for_request("opus", 0, SeatSelection::FillFirst, &cursors),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn stickyleastloaded_keyless_order_matches_fillfirst() {
        // Keyless / single-seat StickyLeastLoaded resolves through this pure
        // fn and walks the fixed fill-first order; the keyed sticky ordering
        // lives in Router::sticky_seat_order.
        let cursors = RoundRobinCursors::default();
        let order = seat_order_for_request("m", 3, SeatSelection::StickyLeastLoaded, &cursors);
        assert_eq!(order, vec![0, 1, 2]);
    }

    fn pin(state_key: &str) -> SeatPin {
        SeatPin {
            state_key: state_key.to_string(),
            repinned: false,
        }
    }

    #[test]
    fn sticky_pins_evicts_beyond_capacity() {
        // Arrange
        let pins = StickyPins::new();
        let overflow = 8;

        // Act: fill past capacity with distinct, never-re-touched keys.
        for i in 0..(STICKY_PIN_CAPACITY + overflow) {
            pins.put(&format!("sess-{i}"), pin(&format!("seat-{i}")));
        }

        // Assert: bounded at capacity.
        assert_eq!(pins.len(), STICKY_PIN_CAPACITY);

        // Assert: the earliest-inserted keys were evicted (LRU).
        for i in 0..overflow {
            assert!(
                pins.get(&format!("sess-{i}")).is_none(),
                "earliest-inserted key sess-{i} should have been evicted",
            );
        }
        // And the most-recently-inserted survives.
        assert!(
            pins.get(&format!("sess-{}", STICKY_PIN_CAPACITY + overflow - 1))
                .is_some()
        );
    }

    // ---- sticky_least_loaded_order pure-fn tests ----

    /// A Closed, unlimited (max headroom) snapshot -- the most-dispatchable
    /// possible seat. Used to build all-equal candidate sets.
    fn closed_unlimited() -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Closed,
        }
    }

    fn closed_with(rpm: f64) -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: Some(rpm),
            circuit: CircuitPhase::Closed,
        }
    }

    /// An Open (non-dispatchable) snapshot -- a parked seat.
    fn open() -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Open,
        }
    }

    #[test]
    fn sticky_birth_keyless_equal_snapshots_picks_index_zero() {
        // A birth pick (pinned_index=None) over all-equal candidates with
        // tiebreak=0 chooses seat 0 and yields the fill-first order, pinning 0.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome, _) = sticky_least_loaded_order(3, None, false, &snaps, 0, &[]);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 0 });
    }

    #[test]
    fn healthy_home_stays() {
        // An existing pin whose home is dispatchable stays: order leads with
        // the home, the rest ascending, and NO new pin is minted.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome, _) = sticky_least_loaded_order(3, Some(2), false, &snaps, 7, &[]);
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 2 });
    }

    #[test]
    fn overflow_repin_migrates_to_healthy_sibling_once() {
        // Home (index 0) is Open / non-dispatchable; sibling 1 is Closed. Not
        // yet repinned -> migrate ONCE to sibling 1, order leads with 1.
        let snaps = vec![open(), closed_unlimited(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, Some(0), false, &snaps, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::OverflowRepin { home: 1 });
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn already_repinned_unhealthy_home_stays() {
        // Pinned home non-dispatchable but already_repinned=true: the one-time
        // cap holds -> Stay, no second migration even though sibling 1 is
        // healthy.
        let snaps = vec![open(), closed_unlimited(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, Some(0), true, &snaps, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn overflow_no_healthy_sibling_stays() {
        // Pinned home non-dispatchable, every sibling also non-dispatchable,
        // not repinned -> Stay (no flap, no pin: nowhere better).
        let snaps = vec![open(), open(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, Some(0), false, &snaps, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn no_flap_repinned_sibling_stays_when_original_recovers() {
        // Hysteresis: after a session repins onto sibling 1 (repinned=true),
        // a later call where the ORIGINAL seat 0 has recovered must NOT pull
        // the session back. The pin now points at sibling 1, which is
        // dispatchable -> Stay on 1.
        let recovered = vec![closed_unlimited(), closed_unlimited(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, Some(1), true, &recovered, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 1 });
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn birth_unchanged() {
        // A miss still yields Birth on a healthy pool and DeferNoHealthy when
        // all seats are parked.
        let healthy = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome, _) = sticky_least_loaded_order(3, None, false, &healthy, 0, &[]);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });

        let parked = vec![open(), open(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, None, false, &parked, 0, &[]);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
    }

    #[test]
    fn sticky_birth_picks_least_loaded_seat() {
        // Seat 1 has the most RPM headroom -> chosen as home and pinned.
        let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome, _) = sticky_least_loaded_order(3, None, false, &snaps, 0, &[]);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
    }

    #[test]
    fn sticky_birth_prefers_closed_over_half_open_ready() {
        // Seat 0 is HalfOpenReady with high headroom; seat 1 is Closed with
        // lower headroom. Health preference picks the Closed seat 1.
        let snaps = vec![
            CapacitySnapshot {
                rpm_available: Some(100.0),
                circuit: CircuitPhase::HalfOpenReady,
            },
            closed_with(3.0),
        ];
        let (order, outcome, _) = sticky_least_loaded_order(2, None, false, &snaps, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn sticky_birth_tiebreak_rotates_across_tied_seats() {
        // All three seats tied (equal snapshots): tiebreak rotates the home
        // deterministically and wraps -- the anti-herd spread.
        let snaps = vec![closed_with(5.0), closed_with(5.0), closed_with(5.0)];
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 0, &[]).1,
            SelectionOutcome::Birth { home: 0 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 1, &[]).1,
            SelectionOutcome::Birth { home: 1 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 2, &[]).1,
            SelectionOutcome::Birth { home: 2 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 3, &[]).1,
            SelectionOutcome::Birth { home: 0 }
        );
    }

    #[test]
    fn sticky_birth_all_parked_yields_fill_first_no_pin() {
        // Every seat Open / not dispatchable: home 0, fill-first order, no
        // pin (a later turn re-picks once a seat is healthy).
        let snaps = vec![open(), open(), open()];
        let (order, outcome, _) = sticky_least_loaded_order(3, None, false, &snaps, 0, &[]);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
    }

    #[test]
    fn sticky_order_home_first_in_the_middle() {
        // Home in the middle -> [home, then 0..n excluding home ascending].
        assert_eq!(order_home_first(2, 5), vec![2, 0, 1, 3, 4]);
    }

    #[test]
    fn sticky_single_seat_is_trivial() {
        let snaps = vec![closed_unlimited()];
        assert_eq!(
            sticky_least_loaded_order(1, None, false, &snaps, 0, &[]),
            (
                vec![0],
                SelectionOutcome::Stay { home: 0 },
                QuotaDecision::Dormant
            )
        );
        assert_eq!(
            sticky_least_loaded_order(0, None, false, &[], 0, &[]),
            (
                Vec::<usize>::new(),
                SelectionOutcome::Stay { home: 0 },
                QuotaDecision::Dormant
            )
        );
    }

    #[test]
    fn sticky_next_tiebreak_advances_monotonically() {
        let pins = StickyPins::new();
        assert_eq!(pins.next_tiebreak(), 0);
        assert_eq!(pins.next_tiebreak(), 1);
        assert_eq!(pins.next_tiebreak(), 2);
    }

    #[test]
    fn sticky_pins_get_returns_pinned_state_key() {
        let pins = StickyPins::new();
        assert_eq!(pins.get("sess-x"), None);
        pins.put("sess-x", pin("opus#seat-b"));
        assert_eq!(pins.get("sess-x"), Some(pin("opus#seat-b")));
    }

    // ---- the quota partition inside the chooser ----
    //
    // The three layers the partition must NOT touch are pinned here against
    // quota input that would flip the pick if the partition had swallowed
    // them: a parked-but-empty seat, a HalfOpenReady-but-empty seat, and a tie
    // inside the chosen tier.

    /// A below-cap tier with `remaining` unspent.
    fn below(remaining: f64) -> SeatQuota {
        SeatQuota::BelowCap { remaining }
    }

    /// An at-cap tier with `remaining` unspent.
    fn capped(remaining: f64) -> SeatQuota {
        SeatQuota::AtCap { remaining }
    }

    #[test]
    fn quota_supersedes_the_rpm_ranking_on_a_birth_pick() {
        // Seat 0 has far more RPM headroom, but seat 1 has the emptier
        // subscription window. On a subscription pool the window is the real
        // constraint, so the quota tier decides.
        let snaps = vec![closed_with(100.0), closed_with(1.0)];
        let quota = vec![below(0.2), below(0.9)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(2, None, false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
        assert_eq!(decision, QuotaDecision::BelowCapTier);
    }

    #[test]
    fn quota_never_resurrects_a_non_dispatchable_seat() {
        // Seat 0 is parked and reports an EMPTY window; seat 1 is healthy and
        // nearly spent. The dispatchability filter runs first and is not
        // re-derived, so the parked seat is unreachable however good its
        // reading -- otherwise a cap would route a request onto a seat that
        // cannot serve it.
        let snaps = vec![open(), closed_with(1.0)];
        let quota = vec![below(1.0), capped(0.05)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(2, None, false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
        // Only the healthy seat was eligible, and it is capped-known.
        assert_eq!(decision, QuotaDecision::AllCappedMostRemaining);
    }

    #[test]
    fn a_keyless_order_never_leads_with_a_non_dispatchable_seat() {
        // The keyless path shares the dispatchability filter, so an emptier but
        // PARKED seat cannot lead its walk either -- and the parked seat still
        // appears in the order so the chain can fall through to it.
        let snaps = vec![open(), closed_with(1.0)];
        let quota = vec![below(1.0), below(0.20)];
        let mut decision = QuotaDecision::Dormant;

        let order = keyless_quota_order(2, &snaps, &quota, 0, &mut decision)
            .expect("one dispatchable below-cap seat orders the walk");

        assert_eq!(
            order,
            vec![1, 0],
            "the parked seat's empty window must not lead, and it must still follow"
        );
        assert_eq!(decision, QuotaDecision::BelowCapTier);
    }

    #[test]
    fn a_keyless_order_keeps_the_closed_over_half_open_preference() {
        // The defect this pins: filtering only dispatchability let a keyless
        // request lead with a HalfOpenReady probe purely because it had more
        // budget left than a healthy Closed sibling, handing traffic to a
        // breaker still recovering. Both layers are shared with the keyed pick
        // now, so the Closed seat leads despite being the fuller one.
        let snaps = vec![
            CapacitySnapshot {
                rpm_available: Some(100.0),
                circuit: CircuitPhase::HalfOpenReady,
            },
            closed_with(100.0),
        ];
        let quota = vec![below(0.95), below(0.10)];
        let mut decision = QuotaDecision::Dormant;

        let order = keyless_quota_order(2, &snaps, &quota, 0, &mut decision)
            .expect("the Closed seat is below cap, so quota orders the walk");

        assert_eq!(
            order[0], 1,
            "the healthy Closed seat leads even though the half-open probe has \
             more budget left"
        );
        assert!(
            order.contains(&0),
            "the half-open seat still follows so the chain can fall through"
        );
    }

    #[test]
    fn a_keyless_order_leads_with_the_emptiest_of_several_below_cap_seats() {
        // Distinguishes "picks a below-cap seat" from "picks the MOST REMAINING
        // one": the seat with the most budget left is deliberately not the one
        // the fixed order reaches first. NOTE the helper takes REMAINING, so a
        // larger value is an emptier seat.
        let snaps = vec![closed_with(1.0), closed_with(1.0), closed_with(1.0)];
        let quota = vec![below(0.30), below(0.95), below(0.40)];
        let mut decision = QuotaDecision::Dormant;

        let order = keyless_quota_order(3, &snaps, &quota, 0, &mut decision)
            .expect("three below-cap seats order the walk");

        assert_eq!(order[0], 1, "the most remaining below-cap seat leads");
        assert_eq!(order.len(), 3, "every seat still follows");
        assert_eq!(decision, QuotaDecision::BelowCapTier);
    }

    #[test]
    fn quota_never_overrides_the_closed_over_half_open_preference() {
        // Seat 0 is HalfOpenReady with an EMPTY window; seat 1 is fully Closed
        // and over its cap. The health preference restricts to the Closed seat
        // BEFORE quota is consulted, so a breaker recovering from failures is
        // not handed traffic because its budget looks good.
        let snaps = vec![
            CapacitySnapshot {
                rpm_available: Some(100.0),
                circuit: CircuitPhase::HalfOpenReady,
            },
            closed_with(3.0),
        ];
        let quota = vec![below(1.0), capped(0.05)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(2, None, false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
        assert_eq!(decision, QuotaDecision::AllCappedMostRemaining);
    }

    #[test]
    fn quota_leaves_the_anti_herd_tiebreak_to_break_a_tier_tie() {
        // Three seats tied on remaining inside the below-cap tier: the
        // rotation still spreads a burst of new conversations across them
        // rather than herding every one onto the first.
        let snaps = vec![closed_with(5.0), closed_with(5.0), closed_with(5.0)];
        let quota = vec![below(0.8), below(0.8), below(0.8)];

        for (tiebreak, expected) in [(0, 0), (1, 1), (2, 2), (3, 0)] {
            let (_, outcome, decision) =
                sticky_least_loaded_order(3, None, false, &snaps, tiebreak, &quota);
            assert_eq!(outcome, SelectionOutcome::Birth { home: expected });
            assert_eq!(decision, QuotaDecision::BelowCapTier);
        }
    }

    #[test]
    fn a_mixed_or_unknown_pool_falls_back_to_the_rpm_ranking_exactly() {
        // Cap-dormant: with no below-cap evidence and at least one unknown
        // seat, the pick must be the one the RPM ranking makes on its own.
        let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let baseline = sticky_least_loaded_order(3, None, false, &snaps, 0, &[]);

        let mixed = vec![capped(0.3), SeatQuota::Unknown, SeatQuota::Unknown];
        let (order, outcome, decision) =
            sticky_least_loaded_order(3, None, false, &snaps, 0, &mixed);
        assert_eq!((order, outcome), (baseline.0.clone(), baseline.1));
        assert_eq!(decision, QuotaDecision::MixedUnknownFallback);

        let unknown = vec![SeatQuota::Unknown; 3];
        let (order, outcome, decision) =
            sticky_least_loaded_order(3, None, false, &snaps, 0, &unknown);
        assert_eq!((order, outcome), (baseline.0, baseline.1));
        assert_eq!(decision, QuotaDecision::AllUnknownFallback);
    }

    #[test]
    fn a_warm_pin_is_never_moved_to_honor_a_soft_cap() {
        // THE ONE THING THIS ORDERING REFUSES TO DO. The pinned home is
        // healthy and its window is fully spent, while a sibling reports an
        // empty one. The session stays: a soft cap must never cost the warm
        // prompt cache the pin exists to hold, so the over-cap session runs to
        // ACTUAL exhaustion and is rescued by the reactive breaker path.
        let snaps = vec![closed_with(1.0), closed_with(1.0)];
        let quota = vec![capped(0.0), below(1.0)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(2, Some(0), false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
        assert_eq!(order, vec![0, 1]);
        // Quota was not consulted at all on a healthy pin.
        assert_eq!(decision, QuotaDecision::Dormant);
    }

    #[test]
    fn a_migration_off_an_unhealthy_home_ignores_quota() {
        // The trigger for a migration is a seat that cannot SERVE, never a
        // soft cap, so the sibling pick is made on health and RPM exactly as
        // before: sibling 2 has the most headroom and wins despite sibling 1
        // reporting the emptier window.
        let snaps = vec![open(), closed_with(1.0), closed_with(50.0)];
        let quota = vec![SeatQuota::Unknown, below(1.0), capped(0.0)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(3, Some(0), false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::OverflowRepin { home: 2 });
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(decision, QuotaDecision::Dormant);
    }

    #[test]
    fn a_pool_with_no_dispatchable_seat_defers_regardless_of_quota() {
        // Existing no-healthy behavior, unchanged: no pin is created and the
        // fill-first order stands even though every seat reports an empty
        // window.
        let snaps = vec![open(), open()];
        let quota = vec![below(1.0), below(1.0)];

        let (order, outcome, decision) =
            sticky_least_loaded_order(2, None, false, &snaps, 0, &quota);

        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(decision, QuotaDecision::Dormant);
    }
}
