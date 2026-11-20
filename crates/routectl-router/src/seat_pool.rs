//! Seat-pool dispatch across the members of a `[pools.<name>]` block.
//!
//! A model whose `[models.X] provider` value names a pool dispatches across
//! that pool's member provider entries. The seat set is fixed at build time:
//! the factory compiles one [`SeatTarget`] (a member's own provider instance)
//! per usable member, computed ONCE per pool and shared by every model naming
//! it.
//!
//! At request time the router decides the order in which to walk those
//! seats ([`fill_first_order`], or the sticky least-loaded selection below). The seats then slot into the existing
//! fallback chain as ordinary dispatch hops -- the per-target circuit
//! breaker, retry caps, probe fast-fail, and the `Retry-After` park all key
//! off the per-seat, per-model state key, so seat rotation and cooling are
//! delivered by machinery that already exists. This module owns only the
//! dispatch-order glue, keeping it out of the oversized `router.rs`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lru::LruCache;
use parking_lot::Mutex;
use routectl_auth::{SecretRef, SecretScheme};
use routectl_core::Provider;

use crate::quota::placement::Standing;
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
    /// Provider instance built from this member's own credential reference,
    /// or `None` for a placeholder: a member whose credential could not be
    /// read at build time. A placeholder keeps its seat and account so its
    /// unusable mark is visible, and is never dialed.
    pub provider: Option<Arc<dyn Provider>>,
    /// Source `SecretRef` for this specific seat. Retained for the
    /// account-scoped quota key and seat identity; the 401 self-heal does NOT
    /// read this field -- it works through the seat's own `ManagedToken`,
    /// which already refreshes the correct credential.
    pub auth_secret_ref: Option<SecretRef>,
    /// The seat's credential account ([`credential_account`]), derived once
    /// when the seat set is built so placement and admission read it without
    /// re-deriving it per request. `None` for a forwarded seat.
    account: Option<Arc<str>>,
}

impl SeatTarget {
    /// Build a seat for pool member `provider_name`, deriving its credential
    /// account from `auth_secret_ref` and whether the member's entry
    /// forwards the client's credential.
    pub fn new(
        provider_name: String,
        provider: Arc<dyn Provider>,
        auth_secret_ref: Option<SecretRef>,
        forwarded: bool,
    ) -> Self {
        Self::with_provider(provider_name, Some(provider), auth_secret_ref, forwarded)
    }

    /// A placeholder seat for pool member `provider_name`: it has an
    /// identity and an account but no provider, because the member's
    /// credential could not be read when the seat set was built.
    pub fn placeholder(
        provider_name: String,
        auth_secret_ref: Option<SecretRef>,
        forwarded: bool,
    ) -> Self {
        Self::with_provider(provider_name, None, auth_secret_ref, forwarded)
    }

    fn with_provider(
        provider_name: String,
        provider: Option<Arc<dyn Provider>>,
        auth_secret_ref: Option<SecretRef>,
        forwarded: bool,
    ) -> Self {
        let seat = seat_identity(auth_secret_ref.as_ref());
        let account = credential_account(seat.as_deref(), &provider_name, forwarded).map(Arc::from);
        Self {
            provider_name,
            provider,
            auth_secret_ref,
            account,
        }
    }

    /// Whether this seat has no provider to dial.
    #[must_use]
    pub const fn is_placeholder(&self) -> bool {
        self.provider.is_none()
    }

    /// The seat's credential account, or `None` for a forwarded seat.
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// This seat's key into `Router.state` under `nickname`: its own circuit
    /// breaker and RPM bucket entry.
    ///
    /// Model-scoped by design: per-model breaker quarantine is a shipped
    /// contract, so a flaky model-on-account combination must not open the
    /// breaker for healthy sibling models on the same account. Stable across
    /// a Router rebuild, so `carry_over_runtime_state_from` matches a
    /// surviving seat and preserves its breaker and RPM counters. A credential
    /// park is not keyed here: it lives on the account, in the router-level
    /// account map a rebuild shares whole.
    #[must_use]
    pub fn state_key_for(&self, nickname: &str) -> String {
        seat_state_key(nickname, Some(&self.provider_name))
    }
}

impl std::fmt::Debug for SeatTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatTarget")
            .field("provider_name", &self.provider_name)
            .field("provider_id", &self.provider.as_ref().map(|p| p.id()))
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
fn secret_ref_scheme(secret_ref: &SecretRef) -> &'static str {
    SecretScheme::from(secret_ref).as_prefix()
}

/// The byte that joins a model nickname to a pool member in a seat's
/// runtime-state key. Reserved in every model nickname and provider name
/// (see [`check_state_key_name`]), which is what makes the join injective.
pub const SEAT_KEY_SEPARATOR: char = '#';

/// Derive the runtime-state key for one seat of a pooled model.
///
/// The DEFAULT seat (label `None`) keys as the bare `nickname` -- so a
/// single-target model keys by nickname exactly as it always has. A NAMED
/// seat keys as `"{nickname}#{label}"`, which keeps the key
/// operator-readable in logs. The OAuth `provider#label` seat key happens to
/// use the same byte but is a separate keyspace with its own grammar.
///
/// Collision-free only because neither half may contain the separator: with
/// that reserved, a direct key never contains it, a seat key contains it
/// exactly once, and [`split_seat_state_key`] recovers both halves.
pub fn seat_state_key(nickname: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("{nickname}{SEAT_KEY_SEPARATOR}{label}"),
        None => nickname.to_string(),
    }
}

/// Split a pooled seat's runtime-state key back into `(nickname, member)`,
/// or `None` for a direct model key. The inverse of [`seat_state_key`] for
/// every key whose halves passed [`check_state_key_name`].
pub fn split_seat_state_key(state_key: &str) -> Option<(&str, &str)> {
    state_key.split_once(SEAT_KEY_SEPARATOR)
}

/// Refuse a model nickname or provider name that would make a runtime-state
/// key ambiguous: one carrying the seat-key separator could compose the
/// same bytes as a different (model, member) pair. `role` names what the
/// value is in the refusal message.
pub fn check_state_key_name(role: &str, name: &str) -> Result<(), String> {
    if name.contains(SEAT_KEY_SEPARATOR) {
        return Err(format!(
            "{role} `{}` must not contain `{SEAT_KEY_SEPARATOR}` \
             (reserved as the seat-pool state-key separator)",
            routectl_core::render_operator_name(name),
        ));
    }
    Ok(())
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

/// The credential account a dispatch target's credential park is scoped to:
/// every model on one account is refused while that account's credential is
/// parked.
///
/// An OAuth target's account is its [`seat_identity`]; any other own-credential
/// target's account is its `[providers]` entry name behind
/// [`SEAT_KEY_SEPARATOR`]. The prefix keeps the two domains disjoint: an OAuth
/// identity starts with its provider, which `SecretRef::parse` refuses empty,
/// and an entry name may not contain the separator ([`check_state_key_name`]),
/// so `oauth://x` and entry `x` never share an account. Two entries reading one
/// secret are two accounts. A forwarded credential is the client's, not the
/// operator's, so a forwarded target has no account and is never parked.
pub fn credential_account(
    seat_identity: Option<&str>,
    provider_name: &str,
    forwarded: bool,
) -> Option<String> {
    if forwarded {
        return None;
    }
    Some(seat_identity.map_or_else(
        || format!("{SEAT_KEY_SEPARATOR}{provider_name}"),
        str::to_string,
    ))
}

/// Whether `account`, a [`credential_account`] key, names a `[providers]`
/// entry rather than an OAuth seat. Only the entry form starts with
/// [`SEAT_KEY_SEPARATOR`].
pub fn is_entry_account(account: &str) -> bool {
    account.starts_with(SEAT_KEY_SEPARATOR)
}

/// Pinned-seat record for one inbound conversation: which pool MEMBER
/// (account) the session is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPin {
    /// The pinned seat's member `[providers]` table key -- the ACCOUNT, not a
    /// model-scoped key.
    ///
    /// The account is what carries the warm prompt cache, and one pin is held
    /// per session per POOL (see `sticky_pin_key`), so several models of one
    /// pool resolve this same pin. A model-scoped `state_key` here would
    /// resolve for exactly one of those models and read as a miss for every
    /// other, so each model would re-pick and overwrite the shared pin --
    /// leaving the session flapping between accounts, the opposite of what
    /// pinning it once per pool delivers. Member names are unique within a
    /// pool and stable across a Router rebuild, so the identity survives a
    /// reload the same way a `state_key` did.
    pub(crate) member: String,
}

/// Maximum number of session->seat pins held at once. A bounded LRU keeps
/// the map at a few-thousand entries so memory stays flat under churn; the
/// least-recently-used pin is evicted when a new conversation arrives at
/// capacity. An evicted conversation simply re-pins (one cold miss) on its
/// next turn, so the bound is safe.
const STICKY_PIN_CAPACITY: usize = 4096;

/// Bounded LRU map of pin lookup key -> pinned [`SeatPin`] (the pinned pool
/// MEMBER). The lookup key namespaces
/// the session by pool rather than by model, so a session holds ONE pin per
/// pool -- see `sticky_pin_key`. Neither a positional seat index nor a
/// model-scoped `state_key` is stored: indices shift on a Router rebuild, and
/// a model-scoped key would resolve for only one of the models sharing the
/// pool (see the field doc on [`SeatPin::member`]).
///
/// Wraps a `parking_lot::Mutex<LruCache<..>>` for interior mutability so the
/// map is read/written on the `&self` dispatch path. The dispatch path writes
/// only through [`StickyPins::claim_if_absent`] and [`StickyPins::repin_if`],
/// each a check and a write under one lock acquisition, so two turns of one
/// conversation that both chose a seat cannot both pin it: the second sees
/// the first's pin and adopts it.
///
/// This whole struct is held behind an `Arc` on
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

    /// Insert or overwrite the pin for `session_key` unconditionally. Test
    /// seeding only: dispatch writes through the two conditional setters.
    #[cfg(test)]
    pub(crate) fn put(&self, session_key: &str, pin: SeatPin) {
        self.pins.lock().put(session_key.to_string(), pin);
    }

    /// Pin `session_key` to `pin` only when it holds no pin, marking it
    /// most-recently used either way. `Err` carries the pin another turn
    /// stored first, which the caller adopts in place of its own pick.
    pub(crate) fn claim_if_absent(&self, session_key: &str, pin: SeatPin) -> Result<(), SeatPin> {
        let mut pins = self.pins.lock();
        if let Some(held) = pins.get(session_key) {
            return Err(held.clone());
        }
        pins.put(session_key.to_string(), pin);
        Ok(())
    }

    /// Re-point `session_key` to `pin` only while it is still pinned to
    /// `expected_member`, marking it most-recently used. `Err` carries what
    /// the key holds instead: another turn's pin, or `None` when the pin was
    /// evicted meanwhile.
    pub(crate) fn repin_if(
        &self,
        session_key: &str,
        expected_member: &str,
        pin: SeatPin,
    ) -> Result<(), Option<SeatPin>> {
        let mut pins = self.pins.lock();
        match pins.get_mut(session_key) {
            Some(held) if held.member == expected_member => {
                *held = pin;
                Ok(())
            }
            held => Err(held.cloned()),
        }
    }

    /// Re-pick, in place, every pin whose member `survivor_for` answers for,
    /// and return how many pins moved.
    ///
    /// `survivor_for` is the caller's membership question: given a pinned
    /// member, it answers `Some(survivor)` only when that member is no longer
    /// usable AND the pool it belonged to still serves someone. Every other
    /// pin -- a member still serving, or one whose whole pool is gone -- is
    /// left byte-for-byte alone, so a membership change never globally resets
    /// the pin map.
    ///
    /// One lock acquisition for the whole walk, and `iter_mut` does not
    /// re-order the LRU, so a pin's eviction position is unchanged by being
    /// re-picked.
    pub(crate) fn repick_members(
        &self,
        survivor_for: &mut dyn FnMut(&str) -> Option<String>,
    ) -> usize {
        let mut moved = 0;
        for (_, pin) in self.pins.lock().iter_mut() {
            if let Some(survivor) = survivor_for(&pin.member) {
                pin.member = survivor;
                moved += 1;
            }
        }
        moved
    }

    /// Number of pins currently held. Test read surface only: production
    /// never asks how many conversations are pinned, only whether a given
    /// one is.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pins.lock().len()
    }

    /// Every pinned session key, most recently used first, read without
    /// touching recency. Test read surface only.
    #[cfg(test)]
    pub(crate) fn recency_order(&self) -> Vec<String> {
        self.pins
            .lock()
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// True when no session holds a pin. Test read surface only, paired
    /// with [`StickyPins::len`].
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pins.lock().is_empty()
    }
}

/// The walk order for a KEYLESS request on a sticky-configured pool, led by
/// the seat the pace rule places a new conversation on.
///
/// A keyless request carries no session identity, so there is no warm prompt
/// cache to protect and no pin to mint. It places exactly as a keyed birth
/// does (the same eligibility, the same `standings` bucket, the same headroom
/// and rotation inside it), so the two can never disagree about which seat is
/// best. The chosen seat leads; every other seat follows in the fixed order,
/// so the fall-through walk is preserved. `None` when there is nothing to
/// place on: a single seat, a dormant pool (`standings` empty), no
/// dispatchable seat, or no eligible seat with a fresh reading. The caller
/// then keeps the fill-first walk, so a keyless request with no evidence
/// drains seats in order exactly as it did before the pace rule.
pub fn keyless_quota_order(
    seat_count: usize,
    snapshots: &[CapacitySnapshot],
    standings: &[Standing],
    tiebreak: usize,
) -> Option<Vec<usize>> {
    if seat_count <= 1 || standings.is_empty() {
        return None;
    }
    let all: Vec<usize> = (0..seat_count).collect();
    let eligible = eligible_candidates(&all, snapshots);
    let (tier, _) = crate::quota::placement::best_bucket(standings, &eligible)?;
    if tier == crate::quota::placement::Tier::Unread {
        return None;
    }
    let lead = pick_least_loaded(&all, snapshots, tiebreak, standings)?;
    Some(order_home_first(lead, seat_count))
}

/// The fixed seat walk order for a pooled model: default seat first, then
/// sorted labels, as built. `FillFirst` always walks it, so the chain drains
/// one seat until it cools/parks before falling to the next, maximizing
/// prompt-cache locality; `StickyLeastLoaded` walks it whenever it makes no
/// pick (a single seat, a dormant pool without a session key, or no
/// dispatchable seat).
///
/// Returns indices into the model's seat slice; the caller maps them back
/// to [`SeatTarget`]s.
pub fn fill_first_order(seat_count: usize) -> Vec<usize> {
    (0..seat_count).collect()
}

/// Build a walk order with `home` first, then every other seat in ascending
/// index order. The home seat is a best-effort cache-locality hint; the
/// ascending tail preserves the fill-first fallback so the existing
/// sequential gate + chain fallback walk stays authoritative.
pub fn order_home_first(home: usize, seat_count: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(seat_count);
    order.push(home);
    order.extend((0..seat_count).filter(|&i| i != home));
    order
}

/// Outcome of a single sticky-least-loaded selection. The walk-order hint is
/// returned alongside; this enum tells the caller whether (and how) to update
/// the pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// Pin miss (birth): caller claims a pin on `home`.
    Birth { home: usize },
    /// The pinned home keeps the conversation: it can serve, its outage is
    /// too short to move for, or no sibling is eligible. No pin write.
    Stay { home: usize },
    /// The pinned home is out for `trigger`, long enough to move for: caller
    /// re-points the pin to the new `home`.
    Repin {
        home: usize,
        trigger: routectl_core::EligibilityReason,
    },
    /// Pin miss with no dispatchable seat: fill-first order, no pin.
    DeferNoHealthy,
}

/// The candidate set every seat pick starts from: dispatchable seats, then the
/// Closed-over-HalfOpenReady health preference.
///
/// THE one place either layer is expressed. Both the keyed birth pick and the
/// keyless walk go through it, so neither can quietly apply a different notion
/// of "eligible" -- a keyless request that skipped the health preference would
/// lead with a half-open probe over a healthy sibling purely because the probe
/// had more budget left, handing traffic to a breaker still recovering.
/// Empty when nothing is dispatchable.
pub fn eligible_candidates(candidates: &[usize], snapshots: &[CapacitySnapshot]) -> Vec<usize> {
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

/// Pick the least-loaded HEALTHY index among `candidates`: dispatchable only,
/// Closed preferred over HalfOpenReady, then the pace rule's best bucket when
/// `standings` is non-empty, then max rpm_available (None=+inf), ties broken
/// deterministically by `tiebreak`. None if no candidate is dispatchable.
/// `candidates` are indices into `snapshots`.
///
/// `standings` is index-aligned with `snapshots` and EMPTY for a dormant pool,
/// which then ranks on headroom alone, exactly as before the pace rule
/// existed.
fn pick_least_loaded(
    candidates: &[usize],
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
    standings: &[Standing],
) -> Option<usize> {
    let preferred = eligible_candidates(candidates, snapshots);
    let ranked = if standings.is_empty() {
        preferred
    } else {
        crate::quota::placement::best_bucket(standings, &preferred)
            .map_or_else(Vec::new, |(_, tied)| tied)
    };
    if ranked.is_empty() {
        return None;
    }

    // Least loaded = most available RPM headroom. Treat unlimited (`None`) as
    // +infinity so an unlimited seat always wins the headroom comparison.
    let headroom = |idx: usize| -> f64 { snapshots[idx].rpm_available.unwrap_or(f64::INFINITY) };
    let max_headroom = ranked
        .iter()
        .map(|&i| headroom(i))
        .fold(f64::NEG_INFINITY, f64::max);
    let tied: Vec<usize> = ranked
        .into_iter()
        .filter(|&i| headroom(i) == max_headroom)
        .collect();

    // Anti-herd tiebreak: rotate deterministically across the tied seats.
    Some(tied[tiebreak % tied.len()])
}

/// Pure seat-selection math for `StickyLeastLoaded`. Decides the per-request
/// walk order and the [`SelectionOutcome`] (whether/how to update the pin).
///
/// `snapshots` is index-aligned with the seat slice (`len == seat_count`),
/// gathered for the hit AND miss path (the move check reads the pinned
/// home's snapshot). `pinned_index` is the seat this session is pinned to
/// (`Some`), or `None` for a birth pick / pin miss. `tiebreak` seeds the
/// anti-herd rotation among equally-least-loaded candidates. `standings` is
/// index-aligned with the seats and empty for a dormant pool.
///
/// A pinned home keeps the conversation unless
/// [`move_trigger`](crate::quota::placement::move_trigger) names a lasting
/// outage. A move ranks the eligible siblings exactly as a birth ranks the
/// pool, with the home excluded; there is no record of earlier homes, so a
/// conversation can move again the next time its seat is out for long, and a
/// recovered seat never pulls it back because a serving pin is never
/// re-ranked. A home that is out but not moving, or that has no eligible
/// sibling, leads the walk as before and the gate and fallback handle it.
pub fn sticky_least_loaded_order(
    seat_count: usize,
    pinned_index: Option<usize>,
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
    standings: &[Standing],
) -> (Vec<usize>, SelectionOutcome) {
    if seat_count <= 1 {
        return (
            (0..seat_count).collect(),
            SelectionOutcome::Stay { home: 0 },
        );
    }

    let n = seat_count;
    if let Some(home) = pinned_index {
        let stay = (order_home_first(home, n), SelectionOutcome::Stay { home });
        let Some(trigger) = crate::quota::placement::move_trigger(&snapshots[home]) else {
            return stay;
        };
        let siblings: Vec<usize> = (0..n).filter(|&i| i != home).collect();
        match pick_least_loaded(&siblings, snapshots, tiebreak, standings) {
            Some(new_home) => (
                order_home_first(new_home, n),
                SelectionOutcome::Repin {
                    home: new_home,
                    trigger,
                },
            ),
            None => stay,
        }
    } else {
        // Birth pick: candidate set = all seats.
        let all: Vec<usize> = (0..n).collect();
        match pick_least_loaded(&all, snapshots, tiebreak, standings) {
            Some(home) => (order_home_first(home, n), SelectionOutcome::Birth { home }),
            // All parked/exhausted: home 0, fill-first order, no pin. A
            // later turn re-picks once a seat is healthy.
            None => (order_home_first(0, n), SelectionOutcome::DeferNoHealthy),
        }
    }
}

#[cfg(test)]
#[path = "seat_pool_pace_tests.rs"]
mod seat_pool_pace_tests;

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

        SeatTarget::new(
            "anthropic-work".to_string(),
            Arc::new(NullProvider),
            Some(SecretRef::File(std::path::PathBuf::from(
                "/var/secrets/anthropic-work.key",
            ))),
            false,
        )
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
    fn a_composed_seat_key_splits_back_into_its_own_halves() {
        // Arrange
        let key = seat_state_key("opus", Some("anthropic-work"));

        // Act
        let halves = split_seat_state_key(&key);

        // Assert
        assert_eq!(halves, Some(("opus", "anthropic-work")));
    }

    #[test]
    fn a_direct_model_key_has_no_seat_half() {
        // Arrange
        let key = seat_state_key("opus", None);

        // Act / Assert
        assert_eq!(split_seat_state_key(&key), None);
    }

    #[test]
    fn a_state_key_name_carrying_the_separator_anywhere_is_refused() {
        for name in ["#", "a#b", "#lead", "trail#", "a##b"] {
            // Act
            let err = check_state_key_name("model nickname", name)
                .expect_err("a separator-bearing name must be refused");

            // Assert
            assert!(err.contains("model nickname"), "role named: {err}");
            assert!(err.contains("`#`"), "reserved char named: {err}");
            assert!(
                err.contains("seat-pool state-key separator"),
                "reservation explained: {err}"
            );
        }
    }

    #[test]
    fn ordinary_state_key_names_are_accepted() {
        // Positive control for the refusal above: realistic names, including
        // non-ASCII and punctuation other than the separator, pass untouched.
        for name in [
            "opus",
            "anthropic-work",
            "codex_fast.v2",
            "modele-\u{e9}t\u{e9}",
        ] {
            assert_eq!(
                check_state_key_name("provider name", name),
                Ok(()),
                "{name}"
            );
        }
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

    /// `(name, seat identity, entry, forwarded, expected account)`.
    type AccountRow<'a> = (&'a str, Option<&'a str>, &'a str, bool, Option<&'a str>);

    /// One target's `(seat identity, entry)`.
    type TargetCredential<'a> = (Option<String>, &'a str);

    #[test]
    fn credential_account_keys_oauth_by_seat_and_any_other_entry_by_its_name() {
        let rows: [AccountRow<'_>; 5] = [
            (
                "unlabeled oauth seat",
                Some("codex"),
                "entry-a",
                false,
                Some("codex"),
            ),
            (
                "labeled oauth seat",
                Some("anthropic#a"),
                "entry-a",
                false,
                Some("anthropic#a"),
            ),
            ("non-oauth entry", None, "glm", false, Some("#glm")),
            ("forwarded entry", None, "claude-fwd", true, None),
            (
                "forwarded entry with an oauth seat",
                Some("codex"),
                "entry-a",
                true,
                None,
            ),
        ];
        for (name, seat, entry, forwarded, expected) in rows {
            // Act
            let account = credential_account(seat, entry, forwarded);

            // Assert
            assert_eq!(account.as_deref(), expected, "{name}");
        }
    }

    #[test]
    fn credential_accounts_collide_only_on_one_credential() {
        let oauth =
            |uri: &str| seat_identity(Some(&SecretRef::parse(uri).expect("parse oauth ref")));
        let rows: [(&str, TargetCredential<'_>, TargetCredential<'_>, bool); 4] = [
            (
                "oauth://x vs entry x",
                (oauth("oauth://x"), "a"),
                (None, "x"),
                false,
            ),
            (
                "one oauth seat under two entries",
                (oauth("oauth://p#l"), "a"),
                (oauth("oauth://p#l"), "b"),
                true,
            ),
            (
                "two labels of one oauth provider",
                (oauth("oauth://p#l"), "a"),
                (oauth("oauth://p#m"), "a"),
                false,
            ),
            (
                "two entries on one env var",
                (None, "a"),
                (None, "b"),
                false,
            ),
        ];
        for (name, (left_seat, left_entry), (right_seat, right_entry), same) in rows {
            // Act
            let left = credential_account(left_seat.as_deref(), left_entry, false);
            let right = credential_account(right_seat.as_deref(), right_entry, false);

            // Assert
            assert!(
                left.is_some() && right.is_some(),
                "{name}: both own an account"
            );
            assert_eq!(left == right, same, "{name}: {left:?} vs {right:?}");
        }
    }

    #[test]
    fn fill_first_order_walks_the_seats_as_built() {
        for (seat_count, expected) in [(0, vec![]), (1, vec![0]), (3, vec![0, 1, 2])] {
            assert_eq!(fill_first_order(seat_count), expected, "{seat_count} seats");
        }
    }

    fn pin(member: &str) -> SeatPin {
        SeatPin {
            member: member.to_string(),
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
            credential: crate::runtime_state::CredentialState::Usable,
            outage_remaining: None,
        }
    }

    fn closed_with(rpm: f64) -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: Some(rpm),
            circuit: CircuitPhase::Closed,
            credential: crate::runtime_state::CredentialState::Usable,
            outage_remaining: None,
        }
    }

    /// An Open (non-dispatchable) snapshot -- a parked seat.
    fn open() -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Open,
            credential: crate::runtime_state::CredentialState::Usable,
            outage_remaining: None,
        }
    }

    #[test]
    fn sticky_birth_keyless_equal_snapshots_picks_index_zero() {
        // A birth pick (pinned_index=None) over all-equal candidates with
        // tiebreak=0 chooses seat 0 and yields the fill-first order, pinning 0.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome) = sticky_least_loaded_order(3, None, &snaps, 0, &[]);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 0 });
    }

    #[test]
    fn healthy_home_stays() {
        // An existing pin whose home is dispatchable stays: order leads with
        // the home, the rest ascending, and NO new pin is minted.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(2), &snaps, 7, &[]);
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 2 });
    }

    #[test]
    fn a_lasting_outage_moves_to_a_sibling_and_none_eligible_stays() {
        let lasting = CapacitySnapshot {
            outage_remaining: Some(crate::quota::placement::MOVE_FLOOR),
            ..open()
        };
        let moved =
            sticky_least_loaded_order(3, Some(0), &[lasting, closed_unlimited(), open()], 0, &[]);
        let stranded = sticky_least_loaded_order(3, Some(0), &[lasting, open(), open()], 0, &[]);

        assert_eq!(
            moved,
            (
                vec![1, 0, 2],
                SelectionOutcome::Repin {
                    home: 1,
                    trigger: routectl_core::EligibilityReason::BreakerOpen,
                }
            )
        );
        assert_eq!(
            stranded,
            (vec![0, 1, 2], SelectionOutcome::Stay { home: 0 })
        );
    }

    #[test]
    fn birth_unchanged() {
        // A miss still yields Birth on a healthy pool and DeferNoHealthy when
        // all seats are parked.
        let healthy = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome) = sticky_least_loaded_order(3, None, &healthy, 0, &[]);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });

        let parked = vec![open(), open(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, None, &parked, 0, &[]);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
    }

    #[test]
    fn sticky_birth_picks_least_loaded_seat() {
        // Seat 1 has the most RPM headroom -> chosen as home and pinned.
        let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome) = sticky_least_loaded_order(3, None, &snaps, 0, &[]);
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
                credential: crate::runtime_state::CredentialState::Usable,
                outage_remaining: None,
            },
            closed_with(3.0),
        ];
        let (order, outcome) = sticky_least_loaded_order(2, None, &snaps, 0, &[]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn sticky_birth_tiebreak_rotates_across_tied_seats() {
        // All three seats tied (equal snapshots): tiebreak rotates the home
        // deterministically and wraps -- the anti-herd spread.
        let snaps = vec![closed_with(5.0), closed_with(5.0), closed_with(5.0)];
        assert_eq!(
            sticky_least_loaded_order(3, None, &snaps, 0, &[]).1,
            SelectionOutcome::Birth { home: 0 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, &snaps, 1, &[]).1,
            SelectionOutcome::Birth { home: 1 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, &snaps, 2, &[]).1,
            SelectionOutcome::Birth { home: 2 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, &snaps, 3, &[]).1,
            SelectionOutcome::Birth { home: 0 }
        );
    }

    #[test]
    fn sticky_birth_all_parked_yields_fill_first_no_pin() {
        // Every seat Open / not dispatchable: home 0, fill-first order, no
        // pin (a later turn re-picks once a seat is healthy).
        let snaps = vec![open(), open(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, None, &snaps, 0, &[]);
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
            sticky_least_loaded_order(1, None, &snaps, 0, &[]),
            (vec![0], SelectionOutcome::Stay { home: 0 })
        );
        assert_eq!(
            sticky_least_loaded_order(0, None, &[], 0, &[]),
            (Vec::<usize>::new(), SelectionOutcome::Stay { home: 0 })
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
}
