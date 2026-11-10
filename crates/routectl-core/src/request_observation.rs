//! Request-owned slot that accumulates what one client request did while it
//! was served, across every attempt and fallback hop.
//!
//! The ingress creates one slot per client request and keeps a clone; the
//! slot rides `RoutectlInternal` through the router and providers, which bump
//! it at the same line that bumps the matching process-global counter. When
//! the request finishes (or is dropped), the holder reads [`snapshot`] to
//! build its record. A request without a slot (a library caller) records
//! nothing.
//!
//! Content-free by contract: no method accepts a request body, header,
//! credential or session value. Counts are integers, translation lanes and
//! drop classes are `&'static str` tokens named at the call site, and the
//! served identity holds routing names (alias, model nickname, upstream id,
//! provider name, provider kind, seat). On a forwarded-credential or
//! default-alias route the alias and upstream id are the client's requested
//! model id, so a consumer rendering them into a log line sanitizes and
//! bounds them like any other client-influenced string.
//!
//! [`snapshot`]: RequestObservation::snapshot

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use crate::EligibilityReason;

/// The target a request was last dispatched to, by its routing names. The
/// alias and upstream id can be the client's requested model id on a
/// forwarded-credential or default-alias route; never body, header,
/// credential or session content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServedIdentity {
    /// Routing alias the request resolved to.
    pub alias: Option<String>,
    /// Served model nickname.
    pub model: Option<String>,
    /// Upstream model id sent on the wire.
    pub upstream: Option<String>,
    /// Provider name from the configuration.
    pub provider: Option<String>,
    /// Config-key provider kind of the served target.
    pub provider_kind: Option<String>,
    /// Seat the served target drew from.
    pub seat: Option<String>,
}

/// Targets a request passed over without an attempt, one count per
/// [`EligibilityReason`]. Each skipped target counts once, under one reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// Targets whose credential was unusable.
    pub credential_unusable: u64,
    /// Targets whose credential account was parked.
    pub account_parked: u64,
    /// Targets whose circuit breaker was open.
    pub breaker_open: u64,
    /// Targets whose local request-rate limit had no capacity left.
    pub rate_limited: u64,
    /// Targets lacking a capability the request needs.
    pub capability: u64,
    /// Targets whose context window could not hold the request.
    pub context_window: u64,
}

impl SkipCounts {
    /// The count kept for `reason`.
    #[must_use]
    pub const fn get(&self, reason: EligibilityReason) -> u64 {
        match reason {
            EligibilityReason::CredentialUnusable => self.credential_unusable,
            EligibilityReason::AccountParked => self.account_parked,
            EligibilityReason::BreakerOpen => self.breaker_open,
            EligibilityReason::RateLimited => self.rate_limited,
            EligibilityReason::Capability => self.capability,
            EligibilityReason::ContextWindow => self.context_window,
        }
    }

    /// Every skipped target, whatever its reason.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.credential_unusable
            .saturating_add(self.account_parked)
            .saturating_add(self.breaker_open)
            .saturating_add(self.rate_limited)
            .saturating_add(self.capability)
            .saturating_add(self.context_window)
    }

    const fn add(&mut self, reason: EligibilityReason) {
        let count = match reason {
            EligibilityReason::CredentialUnusable => &mut self.credential_unusable,
            EligibilityReason::AccountParked => &mut self.account_parked,
            EligibilityReason::BreakerOpen => &mut self.breaker_open,
            EligibilityReason::RateLimited => &mut self.rate_limited,
            EligibilityReason::Capability => &mut self.capability,
            EligibilityReason::ContextWindow => &mut self.context_window,
        };
        *count = count.saturating_add(1);
    }
}

/// How one request's dispatch walk went: targets an upstream failure of the
/// request itself caused it to leave, and targets it passed over because an
/// earlier failure excluded them, one count per exclusion scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WalkCounts {
    /// Targets left because an upstream failure of this request ended them.
    pub request_caused: u64,
    /// Targets skipped because an earlier failure excluded their endpoint.
    pub excluded_endpoint: u64,
    /// Targets skipped because an earlier failure excluded their model.
    pub excluded_model: u64,
    /// Targets skipped because an earlier failure excluded the target itself.
    pub excluded_target: u64,
    /// Targets skipped because an earlier failure excluded them for the
    /// rest of a probe run.
    pub excluded_probe_run: u64,
}

impl WalkCounts {
    /// Counts from every field, in declaration order.
    ///
    /// ```
    /// use routectl_core::WalkCounts;
    ///
    /// let walk = WalkCounts::new(1, 2, 3, 4, 5);
    /// assert_eq!(walk.request_caused, 1);
    /// assert_eq!(walk.excluded_endpoint, 2);
    /// assert_eq!(walk.excluded_model, 3);
    /// assert_eq!(walk.excluded_target, 4);
    /// assert_eq!(walk.excluded_probe_run, 5);
    /// ```
    #[must_use]
    pub const fn new(
        request_caused: u64,
        excluded_endpoint: u64,
        excluded_model: u64,
        excluded_target: u64,
        excluded_probe_run: u64,
    ) -> Self {
        Self {
            request_caused,
            excluded_endpoint,
            excluded_model,
            excluded_target,
            excluded_probe_run,
        }
    }
}

/// Point-in-time copy of a [`RequestObservation`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservationSnapshot {
    /// Wire-check findings where the egress lost something the client sent.
    pub wire_losses: u64,
    /// Distinct `(lane, drop_class)` translation drops seen on this request.
    pub translation_drops: u64,
    /// Requests routectl refused locally without contacting an upstream.
    pub local_refusals: u64,
    /// Field repairs applied to the dispatch, as last stamped.
    pub field_repairs: u64,
    /// Streams that ended without their terminal event.
    pub missing_terminals: u64,
    /// Targets skipped without an attempt, by reason.
    pub skips: SkipCounts,
    /// The dispatch walk's counts, as last stamped.
    pub walk: WalkCounts,
    /// The last target dispatched to, if any.
    pub last_target: Option<ServedIdentity>,
    /// Whether the request is routectl's own probe traffic.
    pub probe: bool,
}

#[derive(Debug, Default)]
struct State {
    wire_losses: u64,
    local_refusals: u64,
    field_repairs: u64,
    missing_terminals: u64,
    translation_drops: BTreeSet<(&'static str, &'static str)>,
    lanes_seen: BTreeSet<&'static str>,
    skips: SkipCounts,
    walk: WalkCounts,
    last_target: Option<ServedIdentity>,
    probe: bool,
}

/// Clone-cheap, request-owned observation slot. Every clone shares one state,
/// so a bump made through the copy riding the request is visible through the
/// copy its creator kept. Content-free: see the module docs.
#[derive(Debug, Clone, Default)]
pub struct RequestObservation {
    state: Arc<Mutex<State>>,
}

impl RequestObservation {
    /// A fresh, empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut state)
    }

    /// Add `n` wire-loss findings.
    pub fn add_wire_losses(&self, n: u64) {
        self.with_state(|s| s.wire_losses = s.wire_losses.saturating_add(n));
    }

    /// Count one local refusal.
    pub fn add_local_refusal(&self) {
        self.with_state(|s| s.local_refusals = s.local_refusals.saturating_add(1));
    }

    /// Stamp the number of field repairs applied; replaces any earlier value.
    pub fn set_field_repairs(&self, n: u64) {
        self.with_state(|s| s.field_repairs = n);
    }

    /// Count one stream that ended without its terminal event.
    pub fn add_missing_terminal(&self) {
        self.with_state(|s| s.missing_terminals = s.missing_terminals.saturating_add(1));
    }

    /// Note a translation drop of `class` on `lane`. Returns `true` only the
    /// first time this request sees the pair, so a caller can advance its
    /// global counter once per request rather than once per attempt.
    pub fn note_translation_drop(&self, lane: &'static str, class: &'static str) -> bool {
        self.with_state(|s| s.translation_drops.insert((lane, class)))
    }

    /// Note that `lane` processed this request. Returns `true` only the first
    /// time this request reaches the lane.
    pub fn note_lane_seen(&self, lane: &'static str) -> bool {
        self.with_state(|s| s.lanes_seen.insert(lane))
    }

    /// Count one target skipped for `reason` without an attempt.
    pub fn add_skip(&self, reason: EligibilityReason) {
        self.with_state(|s| s.skips.add(reason));
    }

    /// Stamp the dispatch walk's counts; replaces any earlier value.
    pub fn set_walk_counts(&self, walk: WalkCounts) {
        self.with_state(|s| s.walk = walk);
    }

    /// Record the target currently being dispatched; the last call wins.
    pub fn set_last_target(&self, identity: ServedIdentity) {
        self.with_state(|s| s.last_target = Some(identity));
    }

    /// Mark whether the request is routectl's own probe traffic.
    pub fn set_probe(&self, probe: bool) {
        self.with_state(|s| s.probe = probe);
    }

    /// Copy the current state.
    #[must_use]
    pub fn snapshot(&self) -> ObservationSnapshot {
        self.with_state(|s| ObservationSnapshot {
            wire_losses: s.wire_losses,
            translation_drops: u64::try_from(s.translation_drops.len()).unwrap_or(u64::MAX),
            local_refusals: s.local_refusals,
            field_repairs: s.field_repairs,
            missing_terminals: s.missing_terminals,
            skips: s.skips,
            walk: s.walk,
            last_target: s.last_target.clone(),
            probe: s.probe,
        })
    }
}

#[cfg(test)]
#[path = "request_observation_tests.rs"]
mod tests;
