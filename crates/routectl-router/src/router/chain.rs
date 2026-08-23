//! Alias/chain resolution + expansion into dispatch targets.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{ChatRequest, Error, Result};

use crate::config::AliasValue;
use crate::resolved::ResolvedModel;

use super::{
    ALIAS_MAX_RECURSION_DEPTH, DispatchTarget, ProbeAdmission, Router, catalog_capabilities,
};

impl Router {
    /// v0.6.0 alias-table lookup. Precedence: exact match -> longest
    /// prefix glob (no default fallback). Returns the resolved chain
    /// of `Arc<ResolvedModel>` entries on hit, `None` when neither
    /// shape matches. The `default` catch-all is consulted later in
    /// `dispatch_chain` so a wire model that's a known nickname wins
    /// over the default fallback.
    ///
    /// Chain entries that are themselves alias keys are recursively
    /// expanded (DFS, preserving operator-stated fallback order).
    /// `Err(Error::Config)` propagates if the recursion hits the
    /// runtime depth cap (`ALIAS_MAX_RECURSION_DEPTH`); cycles are
    /// caught earlier by `validate_alias_chain_targets`, so this is
    /// only a defensive safety net.
    fn resolve_v6_alias(&self, wire_model: &str) -> Result<Option<Vec<Arc<ResolvedModel>>>> {
        let aliases = &self.config.aliases;
        let value = match aliases
            .get(wire_model)
            .cloned()
            .or_else(|| self.alias_glob_index.longest_match(wire_model))
        {
            Some(v) => v,
            None => return Ok(None),
        };
        let mut chain: Vec<Arc<ResolvedModel>> = Vec::new();
        self.expand_alias_value(&value, &mut chain, 0)?;
        if chain.is_empty() {
            // Alias key matched but every target was disabled or
            // unresolvable. Without this WARN the request silently
            // falls through to the `default` catch-all and the
            // operator gets no breadcrumb back to the misconfigured
            // alias. (Startup validation in
            // `validate_alias_chain_targets` catches the static
            // case; this WARN handles the dynamic case where a
            // ResolvedModel was dropped after install.)
            tracing::warn!(
                wire_model = %wire_model,
                "alias resolved to empty chain (all targets disabled or unresolvable); \
                 falling through to direct nickname lookup or `default`",
            );
            Ok(None)
        } else {
            Ok(Some(chain))
        }
    }

    /// The context window in tokens routectl would report for the wire
    /// model `model`, or `None` when it is unconfirmed. Backs the
    /// `context_length` field on `GET /v1/models`.
    ///
    /// Resolution mirrors `dispatch_chain`'s first two steps only:
    /// exact-then-glob alias match, else a direct `[models]` nickname. The
    /// window is read off the FIRST chain entry through
    /// [`ResolvedModel::context_window_tokens`] -- the same accessor the
    /// proactive window gate reads, so a client is never told a window the
    /// router would not gate on.
    ///
    /// FIRST-CONFIGURED-TARGET semantics, deliberately: at dispatch time the
    /// capability pre-filter runs BEFORE the window gate, so a request can be
    /// served by a later chain target whose window differs from the one
    /// reported here. The reported figure is the first CONFIGURED target's,
    /// not a prediction of which target will serve -- discovery describes the
    /// route's head, it does not simulate a dispatch.
    ///
    /// No `default` catch-all fallback (unlike `dispatch_chain`): every id
    /// this method is asked about is a listed alias key or nickname (the
    /// `default` key is excluded from the discovery payload before emit), so
    /// a catch-all branch would only give arbitrary unrouted input a
    /// dispatch-like answer no caller wants.
    ///
    /// SERVABILITY-SHAPED, unlike the discovery LIST that calls it: both
    /// resolution steps read the INSTALLED resolved-model table, never
    /// `[models]` directly, so a configured model whose provider failed to
    /// build is absent and its window is `None`. The entry stays listed
    /// (discovery is config-shaped: the operator wrote it and wants to see
    /// it); only this enrichment is suppressed, because routectl has no
    /// dispatch target whose window it could honestly report. Any future
    /// rewrite that reaches `self.config.models` for the window instead of
    /// `resolve_nickname` reintroduces that leak.
    ///
    /// Never goes through `dispatch_chain`: that increments pool-dispatch
    /// metrics and rotates round-robin state, and a discovery read must not
    /// perturb routing.
    #[must_use]
    pub fn context_window_for(&self, model: &str) -> Option<u32> {
        let first = match self.resolve_v6_alias(model) {
            Ok(Some(chain)) => chain.into_iter().next(),
            Ok(None) => self.resolve_nickname(model),
            // Alias-recursion-depth config errors are swallowed: discovery
            // must omit the field, never fail the whole list for one entry.
            Err(_) => None,
        }?;
        first.context_window_tokens()
    }

    /// Consult the catch-all `default` alias. Returns the resolved
    /// chain, or `None` if no `default` key is configured. Recurses
    /// through nested alias keys identically to `resolve_v6_alias`.
    fn resolve_default_alias(&self) -> Result<Option<Vec<Arc<ResolvedModel>>>> {
        let value = match self.config.aliases.get("default").cloned() {
            Some(v) => v,
            None => return Ok(None),
        };
        let mut chain: Vec<Arc<ResolvedModel>> = Vec::new();
        self.expand_alias_value(&value, &mut chain, 0)?;
        if chain.is_empty() {
            Ok(None)
        } else {
            Ok(Some(chain))
        }
    }

    /// Recursively expand an `AliasValue` into a flat ordered list of
    /// `Arc<ResolvedModel>`. Each chain entry is FIRST checked against
    /// `[aliases]` keys (exact match); if it hits, the nested chain is
    /// expanded inline DFS-style so the operator's stated fallback
    /// order is preserved (`A = ["B", "C"]` with `B = ["X", "Y"]` and
    /// `C` a model nickname yields `[X, Y, C]`). If the entry is not
    /// an alias key, it is treated as a `[models.X]` nickname and
    /// looked up in the resolved-model table; misses are silently
    /// dropped (the static validator surfaces these at startup).
    ///
    /// `depth` is the current recursion depth; the recursion errors
    /// out with `Error::Config` once it exceeds
    /// `ALIAS_MAX_RECURSION_DEPTH`. This is a defensive safety net
    /// for the case where a glob hit re-introduces a cycle the static
    /// DFS missed.
    fn expand_alias_value(
        &self,
        value: &AliasValue,
        out: &mut Vec<Arc<ResolvedModel>>,
        depth: usize,
    ) -> Result<()> {
        if depth > ALIAS_MAX_RECURSION_DEPTH {
            return Err(Error::Config(format!(
                "alias chain recursion exceeded depth {ALIAS_MAX_RECURSION_DEPTH}; \
                 possible cycle that startup validation missed -- \
                 run `routectl config check` to surface the offending alias"
            )));
        }
        for entry in value.nicknames() {
            // Alias keys win over model nicknames by the same shadowing
            // rule the top-level dispatch uses.
            //
            // Glob-pattern entries (e.g. `claude-haiku*`) are matched by
            // this exact `BTreeMap` lookup because glob keys live in
            // `config.aliases` keyed on the literal pattern string, in
            // addition to being indexed in `glob_index` for prefix
            // matching at dispatch time. If those two are ever
            // de-coupled (e.g. moving glob keys out of
            // `config.aliases`), recursive expansion of glob-targeted
            // chain entries breaks here.
            if let Some(nested) = self.config.aliases.get(entry) {
                self.expand_alias_value(nested, out, depth + 1)?;
            } else if let Some(m) = self.resolve_nickname(entry) {
                out.push(m);
            }
            // Else silently drop -- caught by `validate_alias_chain_targets`
            // at startup.
        }
        Ok(())
    }

    /// Resolve the wire `model` value into a chain of `DispatchTarget`s
    /// the dispatch loop can walk.
    ///
    /// v0.6.0 resolution order:
    ///   1. Exact alias match in `[aliases]` -- chain of nicknames.
    ///   2. Longest suffix-glob match in `[aliases]`.
    ///   3. Direct nickname (the wire `model` IS a `[models]` key).
    ///   4. `default` key in `[aliases]` -- catch-all chain.
    ///   5. Otherwise `Error::UnknownAlias`.
    ///
    /// Shadowing rule: when the same string is both an `[aliases]` key
    /// AND a `[models.X]` nickname, the alias wins. This is intentional
    /// so an operator can shadow a model nickname with a multi-target
    /// fallback chain (e.g. `[aliases] foo = ["foo", "backup"]` to add
    /// a backup behind an existing direct nickname). Glob keys also win
    /// over direct nicknames -- e.g. `"claude-*" = "fallback"` shadows
    /// any nickname starting with `claude-`.
    pub(super) fn dispatch_chain(
        &self,
        model: &str,
        session_key: Option<&str>,
    ) -> Result<Vec<DispatchTarget>> {
        if let Some(chain) = self.resolve_v6_alias(model)? {
            return self.expand_resolved_chain(model, chain, session_key);
        }
        // Wire model could ALSO be a direct nickname.
        if let Some(m) = self.resolve_nickname(model) {
            return self.expand_resolved_chain(model, vec![m], session_key);
        }
        // Catch-all: only consulted after exact alias / glob / direct
        // nickname all miss. This ordering means a wire model that's
        // a known nickname always wins over a configured default.
        if let Some(chain) = self.resolve_default_alias()? {
            return self.expand_resolved_chain(model, chain, session_key);
        }
        Err(Error::UnknownAlias(model.to_string()))
    }

    /// Expand a resolved chain, refusing a chain that expanded to NOTHING.
    ///
    /// A resolved chain whose every entry is a pool-backed model with an empty
    /// seat set yields zero dispatch targets. Letting that fall through would
    /// surface as `UnknownAlias` at the end of the dispatch loop -- a 404
    /// naming a route that IS configured, which sends the operator hunting a
    /// routing typo for a credential outage, and which no client retries. This
    /// returns a retryable, fallbackable upstream-shaped error instead, so the
    /// caller backs off and a client-side chain can move on.
    ///
    /// Defensive: the build refuses a pool with zero usable members, so this
    /// is reachable only if a pooled model with no seats reached dispatch
    /// through some path that bypassed that refusal.
    fn expand_resolved_chain(
        &self,
        model: &str,
        chain: Vec<Arc<ResolvedModel>>,
        session_key: Option<&str>,
    ) -> Result<Vec<DispatchTarget>> {
        let targets = self.expand_chain_to_targets(chain, session_key);
        if targets.is_empty() {
            return Err(empty_pool_error(model));
        }
        Ok(targets)
    }

    /// Expand a resolved-model chain into the per-request dispatch-target
    /// chain. A non-pooled model (`seats == None`) maps to exactly one
    /// target keyed by nickname -- byte-for-byte the pre-pool path. A
    /// pooled model maps to one target per seat, in the order
    /// `seat_pool::seat_order_for_request` returns for the target's
    /// in-force `seat_selection` (FillFirst: fixed default-first order;
    /// RoundRobin: per-request rotated start). The expanded seat targets
    /// slot inline
    /// where the model sat, preserving the operator's fallback order so a
    /// chain `[opus, sonnet]` becomes `[opus-seatA, opus-seatB, sonnet]`.
    ///
    /// The post-loop pass also fills `provider_kind` and `class_overrides`
    /// from the target's `[providers.X]` config entry, each only when the
    /// constructor left it empty -- mirroring discipline so a seat target
    /// that already carries its own seat-resolved `provider_kind` (see
    /// `dispatch_target_for_seat`) is never overwritten.
    pub(super) fn expand_chain_to_targets(
        &self,
        chain: Vec<Arc<ResolvedModel>>,
        session_key: Option<&str>,
    ) -> Vec<DispatchTarget> {
        let mut out: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        for m in chain {
            match m.seats.as_ref() {
                None => out.push(into_one_dispatch_target(m)),
                Some(seats) => self.push_seat_targets(&m, seats, session_key, &mut out),
            }
        }
        for target in &mut out {
            let provider_entry = self.config.providers.get(&target.provider_name);
            if target.provider_kind.is_none() {
                target.provider_kind = provider_entry.map(crate::config::ProviderEntry::kind_str);
            }
            // Provider-entry-derived, identical for a seat and a
            // non-seat target of the same provider -- set unconditionally
            // rather than only-when-unset (unlike `provider_kind`, which
            // a seat constructor may have already populated).
            target.use_forwarded_credential =
                provider_entry.is_some_and(|entry| entry.forwarded_base_url().is_some());
            if target.class_overrides.is_empty()
                && let Some(entry) = provider_entry
            {
                target.class_overrides = entry
                    .runtime()
                    .class_overrides
                    .iter()
                    .map(|(status, class)| (*status, class.to_failure_class()))
                    .collect();
            }
        }
        out
    }

    /// Append one dispatch target per seat of a pooled model, in the
    /// request's resolved seat order. Each target carries the seat's own
    /// provider, member provider name, per-(model, seat) `state_key`, and
    /// `auth_secret_ref` so the breaker, RPM gate, retry caps, probe
    /// fast-fail, and the `Retry-After` park all apply per seat; every other
    /// dispatch knob is shared from the model.
    fn push_seat_targets(
        &self,
        m: &Arc<ResolvedModel>,
        seats: &[crate::seat_pool::SeatTarget],
        session_key: Option<&str>,
        out: &mut Vec<DispatchTarget>,
    ) {
        if seats.is_empty() {
            // Defensive only: the build refuses a pool with no usable member,
            // so a live pooled model always has at least one seat. The
            // dispatch loop turns an empty target set into a retryable
            // upstream-shaped error rather than `UnknownAlias`; see
            // `Router::empty_pool_error`.
            self.metrics.incr_pool_unavailable();
            return;
        }
        self.metrics.incr_pool_dispatch();
        if seats.len() < self.configured_member_count(&m.provider_name) {
            self.metrics.incr_pool_degraded_dispatch();
        }
        let selection = self.config.seat_selection_for(&m.provider_name);

        // Sticky least-loaded only engages with a real session key on a
        // multi-seat pool. Every OTHER case (FillFirst, RoundRobin, or
        // keyless / single-seat StickyLeastLoaded) routes through the
        // existing `seat_order_for_request` path UNCHANGED, so keyless
        // StickyLeastLoaded stays byte-for-byte fill-first -- and therefore
        // mints no pin and consults no quota, since it makes no pick at all.
        //
        // `token` is computed ALONGSIDE the order purely for observability:
        // the order and target set below are byte-for-byte what they were
        // before the token existed. It is `None` for genuinely non-sticky
        // modes (FillFirst, RoundRobin) and single-seat pools, which have no
        // sticky decision to record.
        let (order, token): (Vec<usize>, Option<&'static str>) = match (selection, session_key) {
            (crate::config::SeatSelection::StickyLeastLoaded, Some(key)) if seats.len() > 1 => {
                let pin_key = sticky_pin_key(key, m.rotation_key());
                let (order, tok) = self.sticky_seat_order(seats, &pin_key, &m.nickname);
                (order, Some(tok))
            }
            // Keyless StickyLeastLoaded has no session identity, so it mints
            // no pin -- but it still places by remaining budget, because the
            // only thing that outranks quota fairness is cache preservation
            // and a keyless request has no warm cache to preserve. When quota
            // contributes nothing it collapses to fill-first as it always
            // has; that collapse stays visible in the recorded token so an
            // operator can still spot a silent fill-first regime on a pool
            // configured sticky.
            (crate::config::SeatSelection::StickyLeastLoaded, _) if seats.len() > 1 => {
                self.keyless_seat_order(seats, m)
            }
            _ => (
                crate::seat_pool::seat_order_for_request(
                    m.rotation_key(),
                    seats.len(),
                    selection,
                    &self.round_robin,
                ),
                None,
            ),
        };
        let first = out.len();
        for idx in order {
            let seat = &seats[idx];
            // Resolved per SEAT, not once per model: pool members are
            // same-kind by validation today, but reading the kind off the
            // seat's own entry keeps the target's classification anchored to
            // the entry that actually egresses.
            let provider_kind = self
                .config
                .providers
                .get(&seat.provider_name)
                .map(crate::config::ProviderEntry::kind_str);
            out.push(dispatch_target_for_seat(m, seat, provider_kind));
        }
        // Stamp the decision on the home (first) target pushed for THIS
        // model only -- never the fallback seats. The LIMITATION on
        // `DispatchMeta::selection_decision` applies: a serve past the home
        // records `None`.
        if let Some(tok) = token
            && let Some(t) = out.get_mut(first)
        {
            t.selection_decision = Some(tok);
        }
    }

    /// How many members the `[pools]` block behind a dispatch target declared,
    /// or `0` when the name is not a pool. The compiled seat count sitting
    /// BELOW this is exactly what "degraded" means: the build dropped a
    /// member the operator configured.
    fn configured_member_count(&self, name: &str) -> usize {
        self.config
            .pools
            .get(name)
            .map_or(0, |pool| pool.members.len())
    }

    /// Resolve the dispatch chain for a request and pre-filter against
    /// per-provider `unsupported_features` lists. Wraps `dispatch_chain`
    /// so the three dispatch entry points (`complete_with_options`,
    /// `stream_with_options`, `count_tokens`) share one filter pass.
    ///
    /// When the request carries built-in tools (e.g.
    /// `web_search_20250305`) and the operator declared the feature
    /// unsupported on a chain entry, that entry is dropped from the
    /// chain BEFORE dispatch -- not tried-and-fallback. This avoids
    /// per-target 400s from upstreams that simply don't accept the
    /// tool shape, and keeps the breaker counters honest (a feature
    /// mismatch is operator-known, not upstream health).
    ///
    /// Returns `Error::NotImplemented(alias, ...)` when the original
    /// chain was non-empty AND the request had at least one feature
    /// AND every chain entry got filtered. The error message names the
    /// offending feature key(s) so the operator's triage starts from
    /// the right place.
    ///
    /// A second pass then skips any target whose catalog context window
    /// clearly cannot hold the estimated request, under its own kill
    /// switch. That pass never empties the chain and never errors.
    ///
    /// The second tuple element carries the re-probes the filter admitted
    /// (a lapsed learned negative whose single probe slot this request
    /// claimed). Each one MUST be settled by the dispatch path -- success,
    /// same-capability rejection, or other error -- or the entry's
    /// `in_flight` slot latches and the target routes away permanently.
    pub(super) fn dispatch_chain_for_request(
        &self,
        req: &ChatRequest,
    ) -> Result<(Vec<DispatchTarget>, Vec<ProbeAdmission>)> {
        let chain = self.dispatch_chain(
            &req.model,
            req.routectl_internal.inbound_session_key.as_deref(),
        )?;
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let features = crate::feature_keys::derive_feature_keys(
            tools,
            req.provider_extras.as_ref(),
            req.response_format.as_ref(),
        );
        let mut admissions = Vec::new();
        let chain = self.filter_chain_by_features(chain, &features, &req.model, &mut admissions)?;
        // Window pass AFTER the feature filter, never before: "the last
        // surviving target" must be counted against the chain the HARD
        // capability drops left behind. Reversed, the window pass could skip
        // a target that feature filtering then makes the last one, and the
        // feature filter would fabricate an empty-chain error.
        let chain = self.filter_chain_by_window(chain, req);
        Ok((chain, admissions))
    }
}

/// The error a chain that expanded to zero dispatch targets returns.
///
/// Shaped as an `Upstream` 503 rather than `UnknownAlias`: the route exists,
/// so a 404 would be wrong, and the condition is a credential-side outage that
/// may clear. 503 lands on `FailureClass::ServerError`, which the baked retry
/// matrix marks retryable AND fallbackable, so both the router's own chain walk
/// and a client-side retry treat it as transient. The detail names only the
/// wire model the caller already sent.
pub(super) fn empty_pool_error(model: &str) -> Error {
    Error::upstream(
        model,
        503,
        "no usable credential seat is available for this route; \
         every member of its pool is currently unusable",
    )
}

/// Convert a chain of `Arc<ResolvedModel>` into the `DispatchTarget`
/// shape the dispatch loop walks. Hoisted out of `dispatch_chain`
/// so the three resolution branches share one builder.
pub(super) fn into_one_dispatch_target(m: Arc<ResolvedModel>) -> DispatchTarget {
    let capabilities = catalog_capabilities(&m.effective_row);
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        provider_kind: None,
        use_forwarded_credential: false,
        // v0.6.0 dispatch keys the breaker by nickname so two models
        // on one provider quarantine independently.
        state_key: m.nickname.clone(),
        seat: crate::seat_pool::seat_identity(m.auth_secret_ref.as_ref()),
        upstream: m.upstream.clone(),
        provider: Some(m.provider.clone()),
        supports_adaptive_thinking: m.supports_adaptive_thinking,
        effort_levels: m.effort_levels.clone(),
        strip_capabilities: std::sync::Arc::default(),
        nickname: Some(m.nickname.clone()),
        reasoning_dialect: m.reasoning_dialect,
        history_reasoning: m.history_reasoning,
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
        max_thinking_budget: m.max_thinking_budget,
        max_output_tokens: m.max_output_tokens,
        reported_model: m.reported_model.clone(),
        visible_routectl_provider: m.visible_routectl_provider,
        model: m,
        selection_decision: None,
        class_overrides: BTreeMap::new(),
        capabilities,
    }
}

/// Namespace a sticky pin lookup key by the POOL a target dispatches, so a
/// session holds one pin per pool: it stays on one account across every model
/// of that pool (its warm prompt cache lives on the account, not on the
/// model), while two DIFFERENT pools in one chain keep independent pins for
/// the same inbound session. Without the namespace both pools key by the bare
/// session and clobber each other's pin every turn, defeating the
/// prompt-cache locality StickyLeastLoaded exists to provide.
///
/// `pool` is the pool name for a pool-backed model and the model nickname for
/// a standalone provider-backed one (see `Router::rotation_key_for`);
/// validation rejects a pool name that collides with a nickname, so the two
/// bases cannot name one lane.
///
/// The pool key is length-prefixed so no (session, pool) pair can collide
/// with another regardless of which bytes appear in the session key.
pub(super) fn sticky_pin_key(session: &str, pool: &str) -> String {
    format!("{}:{}:{}", pool.len(), pool, session)
}

/// Build one dispatch target for one seat of a pooled model.
///
/// `provider_name` is the SEAT's member `[providers]` table key, not the
/// model's `provider` value: a pool-backed model's `provider` names the pool,
/// which is not a `[providers]` entry, so keying the target by it would make
/// every per-provider config lookup on the dispatch path (runtime policy,
/// class overrides, header extras, beta floor, context reduction) silently
/// miss. `nickname` still carries the model for tracing, while `state_key`
/// joins the two so the breaker and RPM bucket are per (model, seat).
pub(super) fn dispatch_target_for_seat(
    m: &Arc<ResolvedModel>,
    seat: &crate::seat_pool::SeatTarget,
    provider_kind: Option<&'static str>,
) -> DispatchTarget {
    let capabilities = catalog_capabilities(&m.effective_row);
    DispatchTarget {
        provider_name: seat.provider_name.clone(),
        provider_kind,
        use_forwarded_credential: false,
        state_key: seat.state_key_for(&m.nickname),
        seat: crate::seat_pool::seat_identity(seat.auth_secret_ref.as_ref()),
        upstream: m.upstream.clone(),
        provider: Some(seat.provider.clone()),
        supports_adaptive_thinking: m.supports_adaptive_thinking,
        effort_levels: m.effort_levels.clone(),
        strip_capabilities: std::sync::Arc::default(),
        nickname: Some(m.nickname.clone()),
        reasoning_dialect: m.reasoning_dialect,
        history_reasoning: m.history_reasoning,
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
        max_thinking_budget: m.max_thinking_budget,
        max_output_tokens: m.max_output_tokens,
        reported_model: m.reported_model.clone(),
        visible_routectl_provider: m.visible_routectl_provider,
        model: m.clone(),
        selection_decision: None,
        class_overrides: BTreeMap::new(),
        capabilities,
    }
}
