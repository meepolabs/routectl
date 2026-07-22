//! Route-target status facade for the /status surface.

use std::time::Instant;

use super::Router;
use crate::runtime_state::{CircuitPhase, ProviderGateStatus};

/// One dispatch target's read-only health for the status surface: a
/// non-pooled model (one entry keyed by nickname) or a single seat of a
/// pooled model (one entry per seat, keyed by the seat's `state_key`).
/// The wire mapping is owned by the status module downstream; this is an
/// internal read shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteTargetStatus {
    /// Key into the runtime-state map: the bare nickname for a non-pooled
    /// model or the default seat, `"{nickname}#{label}"` for a labeled seat.
    pub state_key: String,
    /// The model entry's `[models]` table key.
    pub nickname: String,
    /// The provider's `[providers]` table key.
    pub provider_name: String,
    /// Wire value of the `model` field this target dispatches to.
    pub upstream: String,
    /// Seat label: `None` for a non-pooled model or the default seat,
    /// `Some(label)` for a labeled seat of a pooled model.
    pub seat_label: Option<String>,
    /// Non-mutating gate health for this target.
    pub gate: ProviderGateStatus,
}

impl Router {
    /// Number of dispatch seats a resolved model expands to: `Some(n)`
    /// for a pooled (multi-seat) model, `None` for a non-pooled
    /// single-target model (a lone seat collapses to the single-target
    /// path). Read-only introspection over the resolved-model table; the
    /// hot-reload coordinator's tests use it to observe that a credentials
    /// reload re-expanded a bare-pool `oauth://provider` into the new seat
    /// count without reaching into the private `resolved_models` map.
    pub fn seat_count_for(&self, nickname: &str) -> Option<usize> {
        self.resolved_models
            .get(nickname)
            .and_then(|m| m.seats.as_ref())
            .map(|seats| seats.len())
    }

    /// Non-mutating gate health for the state slot keyed by `state_key`.
    /// Fails safe when no slot exists: reports a circuit-Open,
    /// not-dispatchable gate rather than panicking, so a status view of a
    /// target with no runtime state treats it as unavailable. Like
    /// `capacity_snapshot_for`, this must never go through the
    /// `try_dispatch`-based `breaker_open_for` anti-pattern, which would
    /// claim a half-open probe slot just to read.
    fn gate_status_for(&self, state_key: &str, now: Instant) -> ProviderGateStatus {
        self.state.get(state_key).map_or(
            ProviderGateStatus {
                rpm_available: None,
                circuit: CircuitPhase::Open,
                half_open_probe_in_flight: false,
                circuit_open_elapsed: None,
                last_outcome: None,
                last_outcome_elapsed: None,
            },
            |s| s.lock().gate_status(now),
        )
    }

    /// Read-only health of every dispatch target, for the status surface.
    /// Iterates the resolved-model table and emits one [`RouteTargetStatus`]
    /// per dispatch target: one entry per seat for a pooled (seat-backed)
    /// model, one entry keyed by the nickname for a non-pooled model. Each
    /// entry's gate is read via the `&self`-borrow `gate_status_for`, which
    /// never claims a half-open probe slot; a target with no state slot
    /// fails safe to circuit-Open rather than panicking.
    pub fn status_targets(&self, now: Instant) -> Vec<RouteTargetStatus> {
        let mut out = Vec::new();
        for model in self.resolved_models.values() {
            match model.seats.as_ref() {
                Some(seats) => {
                    for seat in seats.iter() {
                        out.push(RouteTargetStatus {
                            state_key: seat.state_key.clone(),
                            nickname: model.nickname.clone(),
                            provider_name: model.provider_name.clone(),
                            upstream: model.upstream.clone(),
                            seat_label: seat.label.clone(),
                            gate: self.gate_status_for(&seat.state_key, now),
                        });
                    }
                }
                None => {
                    out.push(RouteTargetStatus {
                        state_key: model.nickname.clone(),
                        nickname: model.nickname.clone(),
                        provider_name: model.provider_name.clone(),
                        upstream: model.upstream.clone(),
                        seat_label: None,
                        gate: self.gate_status_for(&model.nickname, now),
                    });
                }
            }
        }
        out
    }
}
