//! Which seat a probe identity names, and whether that seat is one this
//! stage may attribute a rejection to.
//!
//! Both questions are read from the operator's OWN `[providers]` entry rather
//! than from a dispatch target, and the attributability predicate is called by
//! activation AND by the recheck immediately before the dial, so both draw the
//! forwarded, unclassifiable-egress, and loopback refusals from one set.

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
    /// The provider kind a probe on `provider_name`'s CURRENT entry, sending
    /// `upstream`, would act on -- or `None` when this stage may not probe it.
    ///
    /// THE refusal predicate both probe stages call: activation, and the
    /// recheck immediately before the dial. A recheck NARROWER than activation
    /// is the gap a reload slips through, since the queued job was admitted
    /// against the OLD entry and nothing else looks again -- which is why both
    /// read one decision rather than each carrying its own.
    ///
    /// Two halves, both required:
    ///
    /// - the shared attributability decision
    ///   (`field_repair::attributable_field_lane`): the entry exists, does not
    ///   authenticate with a FORWARDED client credential, is of a kind field
    ///   repair acts on, egresses to a classifiable base URL (a Mantle
    ///   sub-lane's is the endpoint its region derives), and that base is not
    ///   LOOPBACK -- the same read the
    ///   reactive arm and the pre-flight planner attribute through;
    /// - a FREE token count for that kind and upstream (the count walk's own
    ///   `seat_can_count_tokens`): a kind with none has no validator to ask, so
    ///   the probe could only no-op.
    ///
    /// `use_forwarded_credential` is the caller's own fact: activation reads it
    /// off the dispatch target, the recheck off the entry (see
    /// [`Self::probe_entry_is_attributable`]).
    pub(super) fn probe_lane_kind(
        &self,
        provider_name: &str,
        use_forwarded_credential: bool,
        upstream: &str,
    ) -> Option<&'static str> {
        let lane = super::field_repair::attributable_field_lane(
            &self.config,
            provider_name,
            use_forwarded_credential,
        )?;
        super::count_tokens::seat_can_count_tokens(Some(lane.provider_kind), upstream)
            .then_some(lane.provider_kind)
    }

    /// Whether a probe on `provider_name`'s CURRENT entry, sending `upstream`,
    /// is one [`Self::probe_lane_kind`] admits, reading the forwarded fact off
    /// the entry.
    ///
    /// The pre-dial recheck has no dispatch target, so the forwarded fact comes
    /// from the operator's ENTRY -- through the same accessor the chain derives
    /// `DispatchTarget::use_forwarded_credential` from, so the two sources
    /// cannot disagree about the same provider.
    pub(super) fn probe_entry_is_attributable(&self, provider_name: &str, upstream: &str) -> bool {
        let use_forwarded_credential = self
            .config
            .providers
            .get(provider_name)
            .is_some_and(|entry| entry.forwarded_base_url().is_some());
        self.probe_lane_kind(provider_name, use_forwarded_credential, upstream)
            .is_some()
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
