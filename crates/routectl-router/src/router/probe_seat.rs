//! Which seat a probe identity names, and whether that seat is one this
//! stage may attribute a rejection to.
//!
//! Both questions are read from the operator's OWN `[providers]` entry rather
//! than from a dispatch target, and the attributability predicate is called by
//! activation AND by the recheck immediately before the dial, so both draw the
//! forwarded, Mantle, and loopback refusals from one set.

use crate::field_verdict::FieldVerdictKey;

use super::Router;

/// One resolved probe target: the seat actually selected for an identity.
pub(super) struct ProbeSeat {
    /// The seat's `[providers]` table key -- what every per-provider config
    /// lookup and the runtime gate resolve against.
    pub(super) provider_name: String,
    /// The seat's key into `Router.state`: its own breaker and RPM bucket.
    pub(super) state_key: String,
    /// The seat's wire model id.
    pub(super) upstream: String,
    pub(super) provider: std::sync::Arc<dyn routectl_core::Provider>,
}

/// Hand-written rather than derived because the provider handle is a
/// `dyn Provider` trait object, which carries no `Debug` bound -- and adding one
/// to the trait for a diagnostic would impose it on every implementation.
///
/// The provider is named by its own closed-set `id()` instead, and every other
/// field is an operator config key -- no caller text and no credential material
/// reaches here.
impl std::fmt::Debug for ProbeSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeSeat")
            .field("provider_name", &self.provider_name)
            .field("state_key", &self.state_key)
            .field("upstream", &self.upstream)
            .field("provider_id", &self.provider.id())
            .finish()
    }
}

impl Router {
    /// Whether a rejection from `provider_name`'s CURRENT entry would be
    /// attributable to a routectl-owned seat.
    ///
    /// THE refusal predicate both probe stages call: activation, and the
    /// recheck immediately before the dial. A recheck NARROWER than activation
    /// is the gap a reload slips through, since the queued job was admitted
    /// against the OLD entry and nothing else looks again -- which is why both
    /// read one set rather than each carrying its own.
    ///
    /// Three ways to be unattributable, all read from the operator's own entry
    /// rather than from a dispatch target:
    ///
    /// - no entry at all (removed by a reload);
    /// - a FORWARDED credential, where the target authenticates with the
    ///   CLIENT's bearer, so one client's rejection must not mint a permanent
    ///   verdict steering every other client. Read through the same accessor
    ///   the chain derives `DispatchTarget::use_forwarded_credential` from, so
    ///   this cannot disagree with the flag activation refuses on;
    /// - no classifiable egress base URL, which is how the Bedrock Mantle
    ///   shape is excluded (its entry reads `anthropic-api` while egressing
    ///   through Mantle, and the accessor answers `None` for it), a base URL
    ///   naming a LOOPBACK destination, or a kind other than `anthropic-api`
    ///   (the only lane probes act on). All through
    ///   `field_repair::attributable_field_lane`, the same
    ///   entry-and-base-url read the reactive arm and the pre-flight planner
    ///   attribute through.
    ///
    /// What is CHECKED is that every stage reaches the same VERDICT on the
    /// provider shapes a parity test varies: base URL, credential source, entry
    /// presence, and -- where the build can construct one -- a Mantle sub-lane,
    /// the shape a raw base-url read gets wrong rather than merely narrow. A
    /// stage that narrows or widens on one of those reds on that row. A decision
    /// keyed on another entry fact needs a new discriminating row first, and a
    /// local check that merely restates part of the shared set changes no
    /// verdict; calling through is a maintenance convention here, not an
    /// enforced one.
    pub(super) fn probe_entry_is_attributable(&self, provider_name: &str) -> bool {
        // The probe stages have no dispatch target, so the forwarded fact is
        // read off the operator's ENTRY -- through the same accessor the chain
        // derives `DispatchTarget::use_forwarded_credential` from, so the two
        // sources cannot disagree about the same provider. It then feeds the
        // SHARED decision, which owns every other refusal.
        let use_forwarded_credential = self
            .config
            .providers
            .get(provider_name)
            .is_some_and(|entry| entry.forwarded_base_url().is_some());
        super::field_repair::attributable_field_lane(
            &self.config,
            provider_name,
            use_forwarded_credential,
        )
        .is_some_and(|lane| lane.provider_kind == super::field_repair::ANTHROPIC_API_KIND)
    }

    /// The resolved seat a probe identity's lane names.
    ///
    /// The lane is `(provider entry, upstream)`, so the seat is the resolved
    /// model (or pooled member) that egresses exactly that entry with exactly
    /// that upstream. A pooled model contributes the member whose
    /// `[providers]` key IS the lane's entry, never seat zero: probing another
    /// member would ask the wrong account and, on a failure, attribute it to
    /// the wrong one.
    ///
    /// Several nicknames may share one lane. They name the same account and
    /// the same wire model, so any of them reaches the endpoint the identity
    /// was learned on; the FIRST in nickname order is taken so the choice --
    /// and with it the breaker the probe admits through -- is deterministic.
    /// `None` when no resolved model dispatches the lane.
    pub(super) fn probe_seat_for(&self, key: &FieldVerdictKey) -> Option<ProbeSeat> {
        let lane = key.lane();
        let entry = lane.provider_entry();
        let upstream = lane.upstream();
        // Bounded by the configured model and seat counts, both small and
        // operator-authored; this runs once per probe decision, not per request.
        self.resolved_models
            .iter()
            .filter(|(_, model)| model.upstream == upstream)
            .find_map(|(nickname, model)| match model.seats.as_ref() {
                None => (model.provider_name == entry).then(|| ProbeSeat {
                    provider_name: model.provider_name.clone(),
                    state_key: nickname.clone(),
                    upstream: model.upstream.clone(),
                    provider: std::sync::Arc::clone(&model.provider),
                }),
                Some(seats) => seats
                    .iter()
                    .find(|seat| seat.provider_name == entry)
                    .map(|seat| ProbeSeat {
                        provider_name: seat.provider_name.clone(),
                        state_key: seat.state_key_for(nickname),
                        upstream: model.upstream.clone(),
                        provider: std::sync::Arc::clone(&seat.provider),
                    }),
            })
    }
}
