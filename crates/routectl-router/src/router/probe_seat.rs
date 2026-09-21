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
    /// - no attributable Anthropic base URL, which is how the Bedrock Mantle
    ///   shape is excluded (its entry reads `anthropic-api` while egressing
    ///   through Mantle, and the accessor answers `None` for it), or a base URL
    ///   naming a LOOPBACK destination. Both through
    ///   `field_repair::attributable_anthropic_base_url`, the same
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
        super::field_repair::attributable_anthropic_base_url(
            &self.config,
            provider_name,
            use_forwarded_credential,
        )
        .is_some()
    }

    /// The resolved seat a probe identity's `state_key` names.
    ///
    /// A pooled identity keys as `nickname#label`, and the LABEL is the
    /// member's `[providers]` key -- so the seat is selected by matching that
    /// label, never by taking seat zero. Probing seat zero for an identity
    /// minted against a different member would ask the wrong account and, on
    /// a failure, attribute it to the wrong one.
    ///
    /// The pooled key is resolved by RE-COMPOSING each candidate through
    /// `SeatTarget::state_key_for` and comparing, rather than by splitting the
    /// key on `#`. Splitting requires knowing which `#` is the separator, and
    /// neither choice is sound: `split_once` mis-parses a nickname containing
    /// `#`, `rsplit_once` mis-parses a label containing one, and nothing in the
    /// config grammar forbids either (see `seat_pool::seat_state_key`'s own
    /// collision note). Comparing against the composer's output is correct for
    /// every key the composer can produce, whatever it contains, and needs no
    /// new validation.
    pub(super) fn probe_seat_for(&self, key: &FieldVerdictKey) -> Option<ProbeSeat> {
        let state_key = key.probe_state_key();
        if let Some(model) = self.resolved_models.get(state_key) {
            // Non-pooled: the state key IS the nickname.
            return Some(ProbeSeat {
                provider_name: model.provider_name.clone(),
                state_key: state_key.to_string(),
                upstream: model.upstream.clone(),
                provider: std::sync::Arc::clone(&model.provider),
            });
        }
        // Pooled. Bounded by the configured model and seat counts, both small
        // and operator-authored; this runs once per probe dial, not per
        // request.
        self.resolved_models.iter().find_map(|(nickname, model)| {
            let seat = model
                .seats
                .as_ref()?
                .iter()
                .find(|s| s.state_key_for(nickname) == state_key)?;
            Some(ProbeSeat {
                provider_name: seat.provider_name.clone(),
                state_key: seat.state_key_for(nickname),
                upstream: model.upstream.clone(),
                provider: std::sync::Arc::clone(&seat.provider),
            })
        })
    }

    /// The configured paid-probe daily cap for the provider behind `key`,
    /// or zero when the lane, the provider, or the entry is absent. Zero is
    /// the default in `[fidelity]`, so an unconfigured provider declines.
    pub(super) fn paid_probe_daily_cap(&self, key: &FieldVerdictKey) -> u32 {
        self.probe_seat_for(key)
            .and_then(|seat| {
                self.config
                    .fidelity
                    .paid_probe_daily_caps
                    .get(&seat.provider_name)
                    .copied()
            })
            .unwrap_or(0)
    }
}
