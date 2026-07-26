//! Breaker/RPM gate + probe-slot admission RAII guards.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use routectl_core::{ChatRequest, Error, failure_class::LastOutcome};

use crate::config::RetryPolicy;
use crate::runtime_state::{GateDecision, ProviderState};

use super::Router;

impl Router {
    /// Trip the circuit breaker for the state slot keyed by `state_key`
    /// (a model nickname or a per-seat `nickname#label`), returning `false`
    /// when no such slot exists. Test seam for the hot-reload carry-over
    /// assertions; `cfg(test)`-gated so it cannot widen the production
    /// surface.
    #[cfg(test)]
    pub fn force_open_breaker(&self, state_key: &str, cooldown: std::time::Duration) -> bool {
        match self.state.get(state_key) {
            Some(slot) => {
                slot.lock().force_open(std::time::Instant::now(), cooldown);
                true
            }
            None => false,
        }
    }

    /// Whether the breaker for `state_key` currently reads open (parked).
    /// `None` when no state slot exists for the key. Companion test seam to
    /// `force_open_breaker` for the carry-over tests.
    #[cfg(test)]
    pub fn breaker_open_for(&self, state_key: &str) -> Option<bool> {
        self.state.get(state_key).map(|slot| {
            matches!(
                slot.lock().try_dispatch(std::time::Instant::now()),
                GateDecision::CircuitOpen
            )
        })
    }

    /// Run RPM bucket + circuit breaker. Returns `Some((kind, err))` if
    /// the gate refuses this dispatch (pretreated as a fallbackable
    /// status-0 upstream error). The `kind` tag is a stable string
    /// (`"rate_limit"` or `"circuit_breaker"`) used as a `gate_kind`
    /// field on the gate-blocked log so operators can filter by reason.
    ///
    /// `state_key` is the per-model nickname (v0.6.0) or the provider
    /// name (legacy / test path); `provider_name_for_err` is always
    /// the operator-facing provider name and lands in the resulting
    /// error so callers see WHICH provider was gate-blocked, not the
    /// internal nickname.
    pub(super) fn gate_check(
        &self,
        state_key: &str,
        provider_name_for_err: &str,
    ) -> Option<(&'static str, Error)> {
        let state = self.state.get(state_key)?.clone();
        let mut s = state.lock();
        match s.try_dispatch(Instant::now()) {
            GateDecision::Allow => None,
            GateDecision::RateLimited => Some((
                "rate_limit",
                Error::upstream(provider_name_for_err, 0, "local rpm_limit exceeded"),
            )),
            GateDecision::CircuitOpen => Some((
                "circuit_breaker",
                Error::upstream(provider_name_for_err, 0, "circuit breaker open"),
            )),
        }
    }

    pub(super) fn record_success(&self, state_key: &str) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().record_success(Instant::now());
        }
    }

    pub(super) fn record_failure(&self, state_key: &str, outcome: LastOutcome) {
        self.record_failure_opened(state_key, outcome);
    }

    /// Debit one breaker failure for `state_key`, returning whether this
    /// debit tripped (opened) the breaker on this call. The `record_failure`
    /// wrapper discards that signal; a caller that must report the breaker
    /// effect of the debit uses this directly.
    pub(super) fn record_failure_opened(&self, state_key: &str, outcome: LastOutcome) -> bool {
        self.state
            .get(state_key)
            .is_some_and(|state| state.lock().record_failure(Instant::now(), outcome))
    }

    /// Park the provider's breaker open for `cooldown`, bypassing the
    /// consecutive-failure threshold. Used when an upstream sent an
    /// explicit rate-limit reset hint larger than the in-loop sleep cap:
    /// a single such signal opens the circuit at once so the chain skips
    /// this seat until it actually resets, rather than re-probing on the
    /// flat schedule. The caller MUST have already clamped `cooldown` to
    /// `RetryPolicy::max_honored_retry_after` (see `rate_limit_reset_hint`).
    /// `force_open` clears any in-flight half-open slot, so this is a
    /// leak-safe substitute for the `record_failure` it replaces.
    pub(super) fn park_provider(&self, state_key: &str, cooldown: Duration) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().force_open(Instant::now(), cooldown);
        }
    }

    /// Release a half-open probe slot this attempt claimed via the gate
    /// WITHOUT recording success or failure. Used on error paths the
    /// router explicitly chose NOT to count against the breaker (probe
    /// fast-fail on 429/529, auth-refresh failure, non-fallbackable
    /// client error). A no-op when the breaker was not half-open (the
    /// slot was never claimed).
    pub(super) fn release_probe_slot(&self, state_key: &str) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().release_probe_slot();
        }
    }

    /// True when this model's breaker currently holds a half-open probe
    /// slot in flight. Read immediately after the gate grants a dispatch
    /// to capture whether THIS dispatch was admitted as the half-open
    /// probe; the captured value is then carried to the first-chunk Ok
    /// arm (reading the flag there instead would race a concurrent
    /// dispatch that claimed or released the slot in between). A no-op
    /// `false` when the breaker is closed or the model has no state slot.
    pub(super) fn is_half_open_probe(&self, state_key: &str) -> bool {
        self.state
            .get(state_key)
            .is_some_and(|state| state.lock().half_open_probe_in_flight())
    }

    /// Build a `ProbeSlotGuard` for a dispatch that just passed the gate.
    /// Armed iff `state_key` currently holds the half-open probe slot (i.e.
    /// THIS dispatch was admitted as the probe); inert otherwise. The guard
    /// releases the slot on drop unless an outcome disarms it -- the
    /// cancellation-safety backstop for a dropped dispatch future.
    ///
    /// The `is_half_open_probe` read and the `state.get().cloned()` below are
    /// two separate lock acquisitions, but the check is race-free under the
    /// single-probe invariant: `try_dispatch` admits at most one
    /// `half_open_in_flight` caller per cooldown, and the current caller has
    /// not yet settled its slot, so no concurrent caller can clear or re-claim
    /// it between the two reads.
    pub(super) fn probe_slot_guard(&self, state_key: &str) -> ProbeSlotGuard {
        if self.is_half_open_probe(state_key) {
            ProbeSlotGuard::new(self.state.get(state_key).cloned())
        } else {
            ProbeSlotGuard::new(None)
        }
    }
}

/// RAII backstop that releases a half-open circuit-breaker probe slot if the
/// dispatch future is dropped before any outcome settles it.
///
/// `gate_check` claims the single half-open probe slot
/// (`half_open_in_flight = true`) BEFORE the dispatch awaits the upstream.
/// Every synchronous outcome arm already settles the slot (`record_success` /
/// `record_failure` / `park_provider` / `release_probe_slot`). The gap this
/// guards is async CANCELLATION: if the future is dropped while awaiting a
/// hung upstream (client disconnect or client-side timeout), none of those
/// arms run and the slot stays claimed forever -- every later probe then sees
/// `CircuitOpen` and the breaker latches open until process restart.
///
/// Held across the upstream `.await`(s); on drop it frees the slot unless an
/// outcome already settled it (`disarm`, mirroring `BreakerAccounting`'s
/// `settled` flag). Freeing -- rather than recording a failure -- is
/// deliberate: a cancelled probe is no evidence of upstream health, so we free
/// the slot while leaving `circuit_opened_at` + the cooldown intact; the next
/// post-cooldown request becomes the probe and the breaker recovers.
///
/// Every synchronous settle site pairs its outcome call with `disarm()`.
/// `record_failure` / `record_success` / `park_provider` already clear
/// `half_open_in_flight` internally, so disarm there only suppresses a
/// redundant (idempotent, harmless) drop-time release; `release_probe_slot`
/// sites clear it explicitly. A NEW settle site MUST also call `disarm()`, or
/// the guard's drop would free a slot a concurrent probe may have re-claimed.
pub(super) struct ProbeSlotGuard {
    /// `Some` while armed; `None` once an outcome settled the slot or the
    /// dispatch never claimed it.
    state: Option<Arc<Mutex<ProviderState>>>,
}

impl ProbeSlotGuard {
    /// Arm a guard for a dispatch that claimed the half-open probe slot. Pass
    /// `None` for a dispatch that did not (closed breaker): the guard is then
    /// inert and its drop is a no-op.
    const fn new(state: Option<Arc<Mutex<ProviderState>>>) -> Self {
        Self { state }
    }

    /// An outcome has settled the slot; drop must not touch it.
    pub(super) fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for ProbeSlotGuard {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            // release_probe_slot is idempotent (it only sets a bool false). If
            // a concurrent caller re-claimed the slot between our settle and
            // this drop, freeing it here opens at most a transient extra probe
            // window -- never a failure record, never a latch.
            state.lock().release_probe_slot();
        }
    }
}

/// A lapsed learned negative whose single re-probe slot a request claimed
/// while filtering its chain. `feature` is the NORMALIZED capability key, so
/// settling it targets the exact registry entry `acting_negative_for`
/// claimed. Carried out of the filter so the dispatch path can settle the
/// probe.
pub(super) struct ProbeAdmission {
    pub(super) state_key: String,
    pub(super) feature: String,
    pub(super) provider_kind: &'static str,
}

/// Settles the learned-capability re-probes a target's dispatch was admitted
/// to run -- the [`ProbeSlotGuard`] pattern applied to the learned registry's
/// `in_flight` slots rather than the breaker's.
///
/// A single target can be admitted to re-probe several distinct learned
/// negatives at once (one admission per `(state_key, feature)`), so the guard
/// holds EVERY admission that target owns and settles each on the dispatch
/// outcome. Held across the whole chain-iteration (including same-provider
/// retries, which stay within the iteration). A 2xx settles all of them:
/// [`settle_success`](Self::settle_success) clears every held entry (proof the
/// capability is not rejected). [`settle_same_capability`](Self::settle_same_capability)
/// refreshes the one matching entry with capped backoff and drops it from the
/// held set. Any other way of leaving the target -- fallback, terminal error,
/// gate block, cancellation -- drops the guard, which records `OtherError` for
/// each still-held admission: the `in_flight` slot is released and the entry
/// stays expired so the next request re-probes (a transient must never clear a
/// valid negative).
pub(super) struct LearnedProbeGuard {
    /// `Some` while any held admission is unsettled; `None` once every
    /// admission settled or the target was never a re-probe admission.
    registry: Option<Arc<crate::learned_capability::LearnedCapabilityRegistry>>,
    /// The still-unsettled admissions this target owns, each self-describing
    /// its `(state_key, feature, provider_kind)`.
    probes: Vec<ProbeAdmission>,
    /// Dispatch surface for the settlement observability event
    /// (`complete` | `stream`); every reached-target settlement emits under it.
    surface: &'static str,
}

impl LearnedProbeGuard {
    /// Arm a guard for a target admitted to re-probe one or more negatives.
    pub(super) const fn armed(
        registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
        probes: Vec<ProbeAdmission>,
        surface: &'static str,
    ) -> Self {
        Self {
            registry: Some(registry),
            probes,
            surface,
        }
    }

    /// An inert guard for a target that was not a re-probe admission; its
    /// drop is a no-op.
    pub(super) const fn inert() -> Self {
        Self {
            registry: None,
            probes: Vec::new(),
            surface: "",
        }
    }

    /// The dispatch succeeded (2xx): clear every held entry, then disarm.
    /// Returns one [`CapabilityClearedEvent`](super::CapabilityClearedEvent)
    /// per cleared entry so the caller can ride the clears out on the dispatch
    /// meta; this is the ONLY settlement arm that clears a resident negative
    /// (a same-capability rejection refreshes with backoff, a drop records a
    /// transient error -- neither clears), so it is the ONLY arm that emits.
    pub(super) fn settle_success(&mut self) -> Vec<super::CapabilityClearedEvent> {
        let mut cleared = Vec::new();
        if let Some(registry) = self.registry.take() {
            let now = Instant::now();
            for probe in self.probes.drain(..) {
                registry.record_probe_outcome(
                    &probe.state_key,
                    &probe.feature,
                    probe.provider_kind,
                    crate::learned_capability::ProbeOutcome::Success,
                    now,
                );
                emit_probe_settlement(&probe, self.surface, "success", true, "success");
                cleared.push(super::CapabilityClearedEvent {
                    state_key: probe.state_key,
                    capability_key: probe.feature,
                    provider_kind: probe.provider_kind.to_string(),
                });
            }
        }
        cleared
    }

    /// The dispatch hit the same capability rejection for one held probe:
    /// refresh that entry with capped backoff and drop it from the held set.
    /// Returns `true` when a held probe matched.
    pub(super) fn settle_same_capability(
        &mut self,
        state_key: &str,
        feature: &str,
        provider_kind: &str,
    ) -> bool {
        if self.registry.is_none() {
            return false;
        }
        let Some(pos) = self.probes.iter().position(|probe| {
            probe.state_key == state_key
                && probe.feature == feature
                && probe.provider_kind == provider_kind
        }) else {
            return false;
        };
        let probe = self.probes.remove(pos);
        if let Some(registry) = &self.registry {
            registry.record_probe_outcome(
                &probe.state_key,
                &probe.feature,
                probe.provider_kind,
                crate::learned_capability::ProbeOutcome::SameCapabilityRejection,
                Instant::now(),
            );
            emit_probe_settlement(
                &probe,
                self.surface,
                "same_capability",
                true,
                "same_capability",
            );
        }
        // Once the last held admission settles, disarm so drop is a no-op.
        if self.probes.is_empty() {
            self.registry = None;
        }
        true
    }
}

impl Drop for LearnedProbeGuard {
    fn drop(&mut self) {
        if let Some(registry) = &self.registry {
            let now = Instant::now();
            for probe in &self.probes {
                registry.record_probe_outcome(
                    &probe.state_key,
                    &probe.feature,
                    probe.provider_kind,
                    crate::learned_capability::ProbeOutcome::OtherError,
                    now,
                );
                emit_probe_settlement(probe, self.surface, "other_error", true, "terminal");
            }
        }
    }
}

/// Request-scoped owner of the re-probe admissions a chain filter staged,
/// grouped by the target that must settle them. Declared before the chain
/// loop in `complete_inner` and `stream_inner`; holds every admission the
/// filter recorded until the loop either reaches a target (transfer) or the
/// request leaves dispatch (settle-on-drop).
///
/// Transfer semantics: [`take`](Self::take) MOVES a target's admissions out
/// of the set into that target's [`LearnedProbeGuard`] when the loop reaches
/// it -- from that point the guard owns them and settles each on the dispatch
/// outcome (Success / SameCapabilityRejection / drop=OtherError). Whatever is
/// still held when the set drops -- an earlier target already returned success,
/// a terminal non-fallbackable error, a `break 'chain` under disable_fallbacks,
/// `?` propagation, or a client disconnect mid-dispatch -- was NEVER reached,
/// so its `in_flight` slot would otherwise latch forever; the drop settles
/// each held admission as `OtherError`, which releases only `in_flight` (it
/// neither confirms nor extends the negative) so the next request re-probes.
///
/// The move is what makes settlement exact-once STRUCTURAL: an admission is
/// owned by the set OR a target guard, never both, so no admission is ever
/// settled twice.
pub(super) struct ProbeAdmissionSet {
    /// Still-held admissions, grouped by the `state_key` of the target that
    /// would settle them once reached.
    pending: HashMap<String, Vec<ProbeAdmission>>,
    registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
    /// Dispatch surface for the settlement observability event
    /// (`complete` | `stream`).
    pub(super) surface: &'static str,
}

impl ProbeAdmissionSet {
    /// Group the filter's flat admission list by settling `state_key`.
    pub(super) fn new(
        registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
        admissions: Vec<ProbeAdmission>,
        surface: &'static str,
    ) -> Self {
        let mut pending: HashMap<String, Vec<ProbeAdmission>> = HashMap::new();
        for admission in admissions {
            pending
                .entry(admission.state_key.clone())
                .or_default()
                .push(admission);
        }
        Self {
            pending,
            registry,
            surface,
        }
    }

    /// Move this target's admissions out of the set into its
    /// [`LearnedProbeGuard`]. Once taken the set no longer owns them, so the
    /// set's drop cannot settle them a second time.
    pub(super) fn take(&mut self, state_key: &str) -> Option<Vec<ProbeAdmission>> {
        self.pending.remove(state_key)
    }
}

impl Drop for ProbeAdmissionSet {
    fn drop(&mut self) {
        let now = Instant::now();
        for admissions in self.pending.values() {
            for admission in admissions {
                self.registry.record_probe_outcome(
                    &admission.state_key,
                    &admission.feature,
                    admission.provider_kind,
                    crate::learned_capability::ProbeOutcome::OtherError,
                    now,
                );
                emit_probe_settlement(admission, self.surface, "other_error", false, "unreached");
            }
        }
    }
}

/// Emit the probe-settlement observability event for one admission. DEBUG
/// level: routine per-request bookkeeping, not an operator-actionable signal.
/// Capability TOKEN + state_key only -- never a request body. `outcome` is the
/// settlement disposition (`success` | `same_capability` | `other_error`) and
/// `reason` its settlement cause (`success` | `same_capability` | `terminal` |
/// `unreached`); `reached_target` is false only for a never-reached admission.
fn emit_probe_settlement(
    admission: &ProbeAdmission,
    surface: &str,
    outcome: &str,
    reached_target: bool,
    reason: &str,
) {
    tracing::debug!(
        event = "probe_settlement",
        state_key = %admission.state_key,
        capability_key = %admission.feature,
        provider_kind = admission.provider_kind,
        surface,
        outcome,
        reached_target,
        reason,
        "learned re-probe admission settled",
    );
}

/// True when `req` is an availability/quota probe: its `max_tokens`
/// is set and at or below the configured `probe_max_tokens` threshold.
/// Claude Code sends `max_tokens=1` probes to `/v1/messages` whose tiny
/// output is never read; on a rate-limit/overload the router fast-fails
/// them instead of walking the fallback chain (see `should_fallback`).
/// `probe_max_tokens = 0` disables detection (no request is a probe);
/// a request with no `max_tokens` is never a probe.
pub(super) fn is_probe_request(req: &ChatRequest, policy: &RetryPolicy) -> bool {
    policy.probe_max_tokens > 0 && req.max_tokens.is_some_and(|m| m <= policy.probe_max_tokens)
}

/// DEBUG-log a probe fast-fail decision, identically from both dispatch
/// loops. Log-only by design: the caller owns the `return Err(..)` that
/// actually short-circuits (a free fn cannot early-return its caller).
/// `max_tokens` is the request value that tripped probe classification,
/// surfaced so an operator can see which value matched the threshold.
pub(super) fn log_probe_fast_fail(
    provider: &str,
    model: &str,
    status: u16,
    max_tokens: Option<u32>,
) {
    tracing::debug!(
        provider,
        model,
        status,
        max_tokens = ?max_tokens,
        "probe request (max_tokens<=probe_max_tokens): not retrying/falling back on rate-limit",
    );
}

#[cfg(test)]
#[path = "probe_fast_fail_tests.rs"]
mod probe_fast_fail_tests;

#[cfg(test)]
#[path = "gate_error_does_not_mask_real_error_tests.rs"]
mod gate_error_does_not_mask_real_error_tests;

#[cfg(test)]
#[path = "breaker_park_preserves_upstream_error_tests.rs"]
mod breaker_park_preserves_upstream_error_tests;

#[cfg(test)]
#[path = "circuit_breaker_slot_release_tests.rs"]
mod circuit_breaker_slot_release_tests;

#[cfg(test)]
#[path = "probe_admission_settlement_tests.rs"]
mod probe_admission_settlement_tests;
