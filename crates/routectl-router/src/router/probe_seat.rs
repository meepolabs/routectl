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
    /// The CONFIGURED kind of this seat's `[providers]` entry, or `None` when no
    /// entry exists.
    ///
    /// A `&'static str` from the entry's own discriminant rather than operator
    /// text, and read from the operator's entry rather than a dispatch target --
    /// the probe stages have none. Needed because a serializer-specific decision
    /// (which wire shape's minimum output allowance applies) is only meaningful
    /// for the lane whose serializer it describes.
    pub(super) provider_kind: Option<&'static str>,
    /// Whether the resolved model this seat belongs to serializes the ADAPTIVE
    /// thinking wire shape, copied from the resolved model rather than read
    /// from config at dial time.
    ///
    /// The two wire shapes need different minimum output allowances, so a
    /// paid-probe body sized against the wrong one is either refused by the
    /// serializer or larger than the question requires. Copied because the
    /// resolved table is what the egress itself reads: a config re-read here
    /// could disagree with the flag the request is actually serialized under.
    pub(super) supports_adaptive_thinking: bool,
    /// The resolved model's operator-declared `max_output_tokens` ceiling, `0`
    /// meaning no override (the production sentinel).
    ///
    /// Carried because it binds the same request a paid body would be sized as,
    /// so a viability decision reading only the catalog's ceiling could admit an
    /// allowance the operator capped below. Copied off the resolved model for
    /// the same reason as the flag above: the resolved table is what the egress
    /// itself reads.
    pub(super) configured_output_ceiling: u32,
    /// The resolved model's two-layer catalog merge, as stamped at chain-build
    /// time.
    ///
    /// The ROW rather than any digest of it, because the row is what states
    /// whether this cell is priced at all -- and that question, not a
    /// provider-name table, is what decides whether a paid body may be sized.
    /// Cloned once per seat resolution, which happens once per probe decision
    /// rather than per request.
    pub(super) effective_row: crate::catalog::EffectiveRow,
}

/// Hand-written rather than derived because the provider handle is a
/// `dyn Provider` trait object, which carries no `Debug` bound -- and adding one
/// to the trait for a diagnostic would impose it on every implementation.
///
/// The provider is named by its own closed-set `id()` instead. That is strictly
/// more useful than an opaque pointer for the one thing this print is for (a
/// paid-authorization diagnostic on a money-spending path), and every other
/// field is an operator config key or a catalog fact -- no caller text and no
/// credential material reaches here.
impl std::fmt::Debug for ProbeSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeSeat")
            .field("provider_name", &self.provider_name)
            .field("state_key", &self.state_key)
            .field("upstream", &self.upstream)
            .field("provider_id", &self.provider.id())
            .field("provider_kind", &self.provider_kind)
            .field(
                "supports_adaptive_thinking",
                &self.supports_adaptive_thinking,
            )
            .field("configured_output_ceiling", &self.configured_output_ceiling)
            .field("effective_row", &self.effective_row)
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
    /// key on the separator. Splitting requires knowing which separator
    /// occurrence divides the two halves, and neither choice is sound:
    /// `split_once` mis-parses a nickname containing one, `rsplit_once`
    /// mis-parses a label containing one, and nothing in the config grammar
    /// forbids either (see `seat_pool::seat_state_key`'s own collision note).
    ///
    /// RECOMPOSITION IS NOT INJECTIVE, so comparing is only sound together with
    /// a UNIQUENESS requirement -- and the candidates it must be unique across
    /// are of TWO kinds, not one:
    ///
    /// - a DIRECT hit, where the key is a `[models]` nickname outright;
    /// - a POOLED match, where some (model, member) pair recomposes to the key.
    ///
    /// Two pooled pairs can collide with each other (model `a` with member `b#c`
    /// and model `a#b` with member `c` both compose `a#b#c`), and a direct
    /// nickname can collide with a pooled pair (a `[models]` entry literally
    /// named `a#b` alongside model `a`'s member `b`) -- exactly the adversarial
    /// shape `seat_pool::seat_state_key`'s own collision note describes. Each
    /// candidate names a DIFFERENT account, so this counts candidates across
    /// both kinds and answers `None` unless the COMBINED count is exactly one.
    /// The direct hit deliberately does NOT short-circuit: returning it early
    /// would silently prefer one account over an equally-valid pooled one
    /// whenever both exist, which is the same defect as taking the first pooled
    /// match by map order.
    ///
    /// WHICH CONSTRUCTION PATH THIS PROTECTS. Normal factory construction
    /// already rejects a `#` in a model NICKNAME (`factory::build_resolved_models`
    /// drops such a model with a stated reason, precisely to keep a nickname from
    /// colliding with a labeled seat's state key), so a config-built table cannot
    /// present the nickname half of either collision. But
    /// `Router::install_resolved_models` is PUBLIC and installs whatever map it
    /// is handed, performing no such check -- a manually composed table may carry
    /// nicknames containing the separator. Member names are not filtered for it
    /// on either path. So the combined exact-one guard is fail-closed protection
    /// for that construction path rather than a restatement of a factory
    /// invariant, and it is what keeps a probe decision from depending on which
    /// path built the table.
    ///
    /// SCOPE. This uniqueness holds for THIS resolver's answer only. It does not
    /// make the wider runtime state-slot namespace collision-free -- a colliding
    /// pair still shares one breaker and RPM bucket, and nothing here changes
    /// that or validates it elsewhere. What it guarantees is narrower and is the
    /// part this stage owns: no probe decision is made against a seat the key
    /// does not uniquely name. Rejecting the separator globally is not the fix
    /// either; it is legal in both halves, and a global ban would refuse
    /// configurations the composer handles correctly.
    pub(super) fn probe_seat_for(&self, key: &FieldVerdictKey) -> Option<ProbeSeat> {
        let state_key = key.probe_state_key();
        // Both candidate kinds are collected before anything is returned, so the
        // combined count is what decides. Bounded by the configured model and
        // seat counts, both small and operator-authored; this runs once per probe
        // decision, not per request.
        let mut matched: Option<ProbeSeat> = None;
        let mut candidates: usize = 0;

        // The DIRECT candidate: the key is a `[models]` nickname. A map key is
        // unique within the map, so this contributes at most one candidate --
        // but not necessarily the ONLY one.
        if let Some(model) = self.resolved_models.get(state_key) {
            candidates += 1;
            matched = Some(ProbeSeat {
                provider_name: model.provider_name.clone(),
                state_key: state_key.to_string(),
                upstream: model.upstream.clone(),
                provider: std::sync::Arc::clone(&model.provider),
                provider_kind: self.probe_provider_kind(&model.provider_name),
                supports_adaptive_thinking: model.supports_adaptive_thinking,
                configured_output_ceiling: model.max_output_tokens,
                effective_row: model.effective_row.clone(),
            });
        }

        // The POOLED candidates: every (model, member) pair whose composed key
        // equals this one.
        for (nickname, model) in &self.resolved_models {
            let Some(seats) = model.seats.as_ref() else {
                continue;
            };
            for seat in seats.iter() {
                if seat.state_key_for(nickname) != state_key {
                    continue;
                }
                candidates += 1;
                if candidates > 1 {
                    // A second candidate of either kind means the key names no
                    // single account. Refuse rather than choose: the whole point
                    // of this resolution is that the seat a probe reaches is the
                    // seat its identity names.
                    return None;
                }
                matched = Some(ProbeSeat {
                    provider_name: seat.provider_name.clone(),
                    state_key: seat.state_key_for(nickname),
                    upstream: model.upstream.clone(),
                    provider: std::sync::Arc::clone(&seat.provider),
                    provider_kind: self.probe_provider_kind(&seat.provider_name),
                    // These two facts belong to the MODEL, not to the seat: a
                    // pool's members share one wire model id, so they share its
                    // thinking shape and its catalog cell. Reading them off the
                    // seat would require per-seat copies of one model's facts.
                    supports_adaptive_thinking: model.supports_adaptive_thinking,
                    configured_output_ceiling: model.max_output_tokens,
                    effective_row: model.effective_row.clone(),
                });
            }
        }
        matched
    }

    /// The configured provider KIND behind `provider_name`, or `None` when no
    /// entry exists.
    ///
    /// Read off the operator's own `[providers]` entry, the same source
    /// `probe_entry_is_attributable` reads, so the kind a stage gates on and the
    /// entry a dial would use cannot describe two different providers. A
    /// `&'static str` from the entry's own discriminant, never operator text.
    fn probe_provider_kind(&self, provider_name: &str) -> Option<&'static str> {
        self.config
            .providers
            .get(provider_name)
            .map(super::super::config::ProviderEntry::kind_str)
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
