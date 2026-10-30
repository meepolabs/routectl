//! Read-only lookups an ingress makes BEFORE dispatch to seed a client-facing
//! opening token count: the Router's publication generation, the lane the
//! request's route would try first, and that lane's calibrated estimate.
//!
//! Nothing here dispatches, and nothing here moves routing state: no
//! round-robin cursor, sticky pin, quota reading, pool counter, breaker or
//! RPM bucket is read through a path that writes it.

use std::sync::Arc;
use std::time::SystemTime;

use crate::calibration::{Factor, LaneKey};
use crate::resolved::ResolvedModel;

use super::Router;

/// The model id a target sends on the wire: the client's requested model
/// verbatim on a forwarded-credential target (dispatch bypasses the rewrite
/// there), else the target's configured upstream.
///
/// The ONE derivation shared by the dispatch metadata the usage ledger
/// records, the window gate's calibration lookup and the opening lookup, so
/// the lane a sample is written under is the lane both readers form.
pub(super) const fn wire_upstream<'a>(
    forwarded: bool,
    requested_model: &'a str,
    configured_upstream: &'a str,
) -> &'a str {
    if forwarded {
        requested_model
    } else {
        configured_upstream
    }
}

/// The lane a route would try first, as known before dispatch.
///
/// A prediction, not a record of what served: the capability pre-filter,
/// the window gate, a parked seat or a failed attempt can each move the
/// request to a later target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpeningLane {
    /// Stable provider-kind token of the head target.
    pub provider_kind: &'static str,
    /// Served model nickname of the head target. Identity for the context
    /// anchor only; the calibration lane keys on `upstream_model`.
    pub nickname: String,
    /// Model id the head target would send on the wire: its configured
    /// upstream, or the client's requested model verbatim on a
    /// forwarded-credential target (the same rule dispatch metadata uses).
    pub upstream_model: String,
    /// [`Router::publication_generation`] of the Router that answered.
    pub generation: u64,
}

impl Router {
    /// This Router's publication generation.
    ///
    /// Advances on EVERY publication through the reload pipeline, including a
    /// config-only reload that leaves the learned-capability registry, and so
    /// [`Self::registry_generation`], unchanged. Stable for the life of one
    /// published Router, so two reads on the Router a request holds agree.
    /// A value a later publication draws is strictly greater.
    #[must_use]
    pub fn publication_generation(&self) -> u64 {
        self.probe_incarnation()
    }

    /// The route head for `model`, resolved without dispatching.
    ///
    /// Resolution order is dispatch's: exact-then-glob alias, direct
    /// nickname, then the `default` catch-all. `None` when nothing resolves,
    /// on an alias-recursion config error, when the head target has no
    /// provider kind (no lane can be formed for it), or when a pooled head
    /// mixes forwarded and own-credential seats (its wire id then depends on
    /// which seat is chosen). For a pooled model the lane is the model's,
    /// not a seat's: seats of one pool share kind and upstream, and a seat
    /// rotation must not look like a lane change.
    #[must_use]
    pub fn opening_lane(&self, model: &str) -> Option<OpeningLane> {
        let head = self.head_resolved_model(model)?;
        let provider_kind = self.config.provider_kind_for_target(&head.provider_name);
        if provider_kind.is_empty() {
            return None;
        }
        let upstream_model = wire_upstream(
            self.head_uses_forwarded_credential(&head)?,
            model,
            &head.upstream,
        )
        .to_string();
        Some(OpeningLane {
            provider_kind,
            nickname: head.nickname.clone(),
            upstream_model,
            generation: self.publication_generation(),
        })
    }

    /// `raw_tokens` corrected by the learned factor of the lane
    /// `(provider_kind, upstream)`, or `None` when that lane has no usable
    /// correction. `upstream` is the wire id, [`OpeningLane::upstream_model`]
    /// for an opening lookup.
    ///
    /// The same lookup and the same `[calibration]` kill switch the window
    /// gate applies: disabled, cold, thin, stale and out-of-band all answer
    /// `None`. The corrected value is returned only; it is never recorded.
    #[must_use]
    pub fn calibrated_estimate(
        &self,
        provider_kind: &str,
        upstream: &str,
        raw_tokens: u64,
    ) -> Option<u64> {
        self.lane_calibration_factor(provider_kind, upstream, SystemTime::now())
            .map(|factor| factor.apply(raw_tokens))
    }

    /// The learned correction for one lane, behind the `[calibration]` kill
    /// switch. The one factor lookup both the window gate and
    /// [`Self::calibrated_estimate`] go through.
    pub(super) fn lane_calibration_factor(
        &self,
        provider_kind: &str,
        upstream: &str,
        now: SystemTime,
    ) -> Option<Factor> {
        if !self.config.calibration.enabled {
            return None;
        }
        let key = LaneKey {
            provider_kind: provider_kind.to_string(),
            upstream: upstream.to_string(),
        };
        self.calibration_store.factor_for(&key, now)
    }

    /// First resolved model of the chain `model` routes to, in dispatch's
    /// resolution order, read off the installed table only.
    fn head_resolved_model(&self, model: &str) -> Option<Arc<ResolvedModel>> {
        let chain = match self.resolve_v6_alias(model) {
            Ok(Some(chain)) => chain,
            Ok(None) => match self.resolve_nickname(model) {
                Some(direct) => return Some(direct),
                None => self.resolve_default_alias().ok().flatten()?,
            },
            Err(_) => return None,
        };
        chain.into_iter().next()
    }

    /// Whether dispatch would authenticate `head` with the client's own
    /// bearer, and so send the requested model verbatim.
    ///
    /// Dispatch decides this per SEAT, from each member's own entry. A pool
    /// whose members disagree has no single wire id before the seat is
    /// chosen, so it answers `None` rather than guessing.
    fn head_uses_forwarded_credential(&self, head: &ResolvedModel) -> Option<bool> {
        let is_forwarded = |name: &str| {
            self.config
                .providers
                .get(name)
                .is_some_and(|entry| entry.forwarded_base_url().is_some())
        };
        let Some(seats) = head.seats.as_deref() else {
            return Some(is_forwarded(&head.provider_name));
        };
        let forwarded = seats
            .iter()
            .filter(|seat| is_forwarded(&seat.provider_name))
            .count();
        match forwarded {
            0 => Some(false),
            n if n == seats.len() => Some(true),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "opening_lookup_tests.rs"]
mod opening_lookup_tests;

#[cfg(test)]
#[path = "calibration_lane_tests.rs"]
mod calibration_lane_tests;
