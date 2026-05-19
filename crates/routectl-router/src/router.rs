//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    sanitize_for_log, ChatChunk, ChatRequest, ChatResponse, Error, Provider, ReasoningConfig,
    Result,
};

use crate::config::{AliasValue, Config, ReasoningDefaults, RetryPolicy};
use crate::glob::PrefixIndex;
use crate::resolved::ResolvedModel;
use crate::runtime_state::{GateDecision, ProviderState};

pub struct Router {
    pub config: Arc<Config>,
    /// Provider implementations keyed by user-facing name. Private so
    /// every insertion goes through [`Router::register`], which keeps
    /// the parallel `state` map (RPM bucket, circuit breaker) in sync.
    /// A direct insert here would silently disable runtime gating for
    /// that provider -- see `gate_check`.
    providers: BTreeMap<String, Arc<dyn Provider>>,
    /// Per-model runtime gates keyed by `[models.X]` nickname. Two
    /// models on the same provider get independent breakers + RPM
    /// buckets so a flaky upstream-model combination quarantines
    /// itself without taking healthy siblings on the same transport
    /// down with it. Key fallback in legacy / test paths is the
    /// provider name (matches what the old per-provider design did).
    /// Both lookups are eager: every `[models.X]` entry installed via
    /// `install_resolved_models` gets a state slot; every
    /// `[providers.X]` entry installed via `Router::new` also gets a
    /// state slot keyed by the provider name (preserved so legacy
    /// dispatch paths and test fixtures using `register()` still find
    /// a gate).
    state: BTreeMap<String, Arc<Mutex<ProviderState>>>,
    /// v0.6.0 pre-resolved model table. Populated when an external
    /// caller built it via `factory::build_resolved_models`. When
    /// non-empty, the dispatch path walks `Arc<ResolvedModel>` chains
    /// instead of re-parsing `provider:model` strings on every hop.
    /// Empty during the C1->C3 transition for the legacy code path
    /// to keep working.
    resolved_models: BTreeMap<String, Arc<ResolvedModel>>,
    /// Glob index for the v0.6.0 alias table. Walks suffix-glob
    /// patterns (e.g. `claude-opus-*`) on lookup miss; longest prefix
    /// wins.
    alias_glob_index: PrefixIndex<AliasValue>,
}

/// Per-request switches that the HTTP handler can flip via header
/// without polluting the wire schema. Defaults preserve current behavior.
///
/// Marked `#[non_exhaustive]` so adding new options later is a
/// non-breaking change for downstream Rust callers. Construct via
/// [`RouterOptions::new`] (alias for `default()`) and mutate fields:
///
/// ```ignore
/// let mut opts = RouterOptions::new();
/// opts.disable_fallbacks = true;
/// router.complete_with_options(req, opts).await
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct RouterOptions {
    /// When true, do NOT walk past the first provider in the chain.
    /// The first failure (after retries) propagates verbatim.
    /// Wired to header `x-routectl-disable-fallbacks: 1`.
    pub disable_fallbacks: bool,
}

impl RouterOptions {
    /// Construct a `RouterOptions` with all-default values. Use this
    /// instead of `RouterOptions { disable_fallbacks: ... }` literals
    /// so future field additions don't break your code.
    pub fn new() -> Self {
        Self::default()
    }
}

/// One hop in the resolved dispatch chain. Built from either a
/// `Arc<ResolvedModel>` (v0.6.0 path) or a parsed `provider:model`
/// literal (legacy path). The dispatch loop reads from this struct
/// directly so the per-mode resolver only runs once per request.
#[derive(Clone)]
struct DispatchTarget {
    /// Operator-facing provider name (a key in `[providers]`). Used
    /// for tracing/log fields and `compose_attempt_policy` (provider-
    /// level timeouts live on `[providers.X]`, not `[models.X]`).
    provider_name: String,
    /// Key into `Router.state` for the per-attempt rate-limit + circuit-
    /// breaker check. v0.6.0 mode uses the model nickname so two
    /// models on the same provider quarantine independently. Legacy /
    /// test mode (no nickname) falls back to the provider name.
    state_key: String,
    /// Wire model id sent to the provider (e.g. an Anthropic
    /// `claude-haiku-4-5-20251001` or a Bedrock inference profile id).
    upstream: String,
    /// Concrete provider instance. `None` only in legacy mode when
    /// the alias chain referenced a provider that wasn't registered;
    /// the dispatch loop converts that to `Error::UnknownProvider`
    /// + fallback. v0.6.0 mode never produces `None` because
    ///   `Router::new` validates every nickname's provider at startup.
    provider: Option<Arc<dyn Provider>>,
    /// v0.6.0 per-model reasoning defaults. `None` in legacy mode
    /// (the merge step reads `[providers.X] reasoning_defaults`
    /// instead). Removed entirely in C4.
    reasoning: Option<ReasoningDefaults>,
    /// Model nickname (the `[models]` table key) for tracing. `None`
    /// in legacy mode where there's no nickname concept.
    nickname: Option<String>,
    /// Per-model `anthropic_beta` list lifted from `[models.X]`.
    /// Merged into `req.anthropic_beta` at dispatch time. Empty when
    /// the operator left the field unset.
    anthropic_beta: Vec<String>,
    /// Per-model `stream_first_byte_timeout_ms`. When `Some`, this
    /// value wins over the per-provider and global resolution in
    /// `compose_attempt_policy`.
    stream_first_byte_timeout_ms: Option<u64>,
}

impl Router {
    pub fn new(config: Arc<Config>) -> Self {
        let mut state = BTreeMap::new();
        for (name, entry) in &config.providers {
            state.insert(
                name.clone(),
                Arc::new(Mutex::new(ProviderState::new(entry.runtime()))),
            );
        }

        // Build the suffix-glob index from the configured `[aliases]`
        // table. Patterns containing a `*` (e.g. `claude-opus-*`) are
        // routed through `PrefixIndex::longest_match` on lookup miss;
        // exact-match keys stay in the BTreeMap and short-circuit
        // ahead of the glob walk. Bare `*`, embedded `*` (e.g.
        // `foo*bar`), and other invalid patterns are rejected at
        // index-build time so a typo'd glob fails loudly at startup
        // rather than silently mismatching wire models.
        let mut alias_glob_index = PrefixIndex::new();
        for (key, value) in &config.aliases {
            if !key.contains('*') {
                continue;
            }
            match crate::glob::AliasPattern::parse(key) {
                Ok(pattern @ crate::glob::AliasPattern::Prefix(_)) => {
                    alias_glob_index.insert(pattern, value.clone());
                }
                Ok(crate::glob::AliasPattern::Exact(_)) => {
                    // Pattern parsed as Exact even with a `*` -- this
                    // shouldn't happen, but treat as exact-only.
                }
                Err(e) => {
                    tracing::warn!(
                        pattern = %key,
                        error = %e,
                        "rejecting invalid alias glob pattern; entry ignored",
                    );
                }
            }
        }

        Self {
            config,
            providers: Default::default(),
            state,
            resolved_models: BTreeMap::new(),
            alias_glob_index,
        }
    }

    /// Install the v0.6.0 pre-resolved model table. Called after
    /// `factory::build_resolved_models` returns. The dispatch path
    /// walks `Arc<ResolvedModel>` chains keyed by nickname.
    ///
    /// Per-model state slot: each nickname gets its OWN
    /// `ProviderState` entry (RPM bucket + circuit breaker). Two
    /// models on the same `[providers.X]` therefore have independent
    /// gates so a flaky model-on-provider combination quarantines
    /// itself without taking healthy siblings down. Each per-model
    /// state is initialized from the parent provider's
    /// `ProviderRuntimePolicy` (rpm_limit, circuit_failures, etc.) so
    /// the operator's TOML knobs apply per model out of the box.
    pub fn install_resolved_models(&mut self, models: BTreeMap<String, Arc<ResolvedModel>>) {
        self.resolved_models = models;
        // Mirror the per-model providers into the `providers` map
        // (keyed by provider name) so legacy lookups still work, and
        // populate the state map with one entry per nickname so
        // dispatch's gate check is per-model.
        for (nickname, m) in self.resolved_models.iter() {
            self.providers
                .entry(m.provider_name.clone())
                .or_insert_with(|| m.provider.clone());
            // Per-model state: clone the parent provider's runtime
            // policy (timeouts, RPM, circuit). Each model gets a
            // FRESH state instance even when the policy is identical
            // -- the breaker counters and RPM tokens must not be
            // shared across models on one transport.
            let policy = self
                .config
                .providers
                .get(&m.provider_name)
                .map(|e| e.runtime().clone())
                .unwrap_or_default();
            self.state
                .entry(nickname.clone())
                .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&policy))));
        }
    }

    /// Look up a model nickname in the resolved table.
    fn resolve_nickname(&self, nickname: &str) -> Option<Arc<ResolvedModel>> {
        self.resolved_models.get(nickname).cloned()
    }

    /// v0.6.0 alias-table lookup. Precedence: exact match -> longest
    /// prefix glob (no default fallback). Returns the resolved chain
    /// of `Arc<ResolvedModel>` entries on hit, `None` when neither
    /// shape matches. The `default` catch-all is consulted later in
    /// `dispatch_chain` so a wire model that's a known nickname wins
    /// over the default fallback.
    fn resolve_v6_alias(&self, wire_model: &str) -> Option<Vec<Arc<ResolvedModel>>> {
        let aliases = &self.config.aliases;
        let value = aliases
            .get(wire_model)
            .cloned()
            .or_else(|| self.alias_glob_index.longest_match(wire_model))?;
        let mut chain: Vec<Arc<ResolvedModel>> = Vec::new();
        for nickname in value.nicknames() {
            if let Some(m) = self.resolve_nickname(nickname) {
                chain.push(m);
            }
        }
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
            None
        } else {
            Some(chain)
        }
    }

    /// Consult the catch-all `default` alias. Returns the resolved
    /// chain, or `None` if no `default` key is configured.
    fn resolve_default_alias(&self) -> Option<Vec<Arc<ResolvedModel>>> {
        let value = self.config.aliases.get("default").cloned()?;
        let mut chain: Vec<Arc<ResolvedModel>> = Vec::new();
        for nickname in value.nicknames() {
            if let Some(m) = self.resolve_nickname(nickname) {
                chain.push(m);
            }
        }
        if chain.is_empty() {
            None
        } else {
            Some(chain)
        }
    }

    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let name = name.into();
        // Ensure a gate exists even for providers registered without a
        // matching config entry (test harnesses rely on this).
        self.state
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&Default::default()))));
        self.providers.insert(name, provider);
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
    fn dispatch_chain(&self, model: &str) -> Result<Vec<DispatchTarget>> {
        if let Some(chain) = self.resolve_v6_alias(model) {
            return Ok(into_dispatch_targets(chain));
        }
        // Wire model could ALSO be a direct nickname.
        if let Some(m) = self.resolve_nickname(model) {
            return Ok(vec![into_one_dispatch_target(m)]);
        }
        // Catch-all: only consulted after exact alias / glob / direct
        // nickname all miss. This ordering means a wire model that's
        // a known nickname always wins over a configured default.
        if let Some(chain) = self.resolve_default_alias() {
            return Ok(into_dispatch_targets(chain));
        }
        Err(Error::UnknownAlias(model.to_string()))
    }

    pub async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.complete_with_options(req, RouterOptions::default())
            .await
    }

    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn complete_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> Result<ChatResponse> {
        let chain = self.dispatch_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let hard_cap = policy.hard_retry_cap();
        let mut last_err: Option<Error> = None;

        'chain: for target in chain.iter() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            let Some(provider) = target.provider.clone() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();
            // v0.6.0: per-model reasoning defaults come from the
            // resolved table (`[models.X] thinking`/`enabled`).
            if let Some(defaults) = target.reasoning.as_ref() {
                merge_reasoning_defaults_into(&mut attempt_req, defaults);
                tracing::debug!(
                    provider = provider_name,
                    model = %target.nickname.as_deref().unwrap_or(""),
                    "applied resolved-model reasoning defaults",
                );
            }
            // v0.6: per-model anthropic_beta list lifts onto
            // req.anthropic_beta, dedup against existing entries.
            if !target.anthropic_beta.is_empty() {
                merge_model_anthropic_beta_into(&mut attempt_req, &target.anthropic_beta);
            }

            let mut backoff = Duration::from_millis(policy.initial_backoff_ms);
            let mut attempts_made: u32 = 0;

            loop {
                // Per-attempt gate: rate limit + circuit breaker.
                // Charges one RPM token and (when half-open) claims the
                // probe slot. If the gate refuses, treat as a fallback
                // event for THIS provider and move to the next chain
                // entry -- retrying the same provider would just hit
                // the gate again.
                if let Some((gate_kind, gate_err)) = self.gate_check(state_key, provider_name) {
                    tracing::warn!(
                        provider = provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        gate_kind,
                        error = ?gate_err,
                        "gate blocked",
                    );
                    last_err = Some(gate_err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue 'chain;
                }

                if attempts_made > 0 {
                    let jittered = add_jitter(backoff, policy.jitter_ms);
                    tokio::time::sleep(jittered).await;
                    backoff = mul_duration(backoff, policy.backoff_multiplier);
                }

                let attempt_policy = self.compose_attempt_policy(
                    &policy,
                    provider_name,
                    target.stream_first_byte_timeout_ms,
                );
                let result = run_with_timeout(
                    provider_name,
                    provider.as_ref(),
                    &attempt_req,
                    &attempt_policy,
                )
                .await;
                attempts_made += 1;

                match result {
                    Ok(mut resp) => {
                        self.record_success(state_key);
                        resp.routectl_provider = Some(provider_name.to_string());
                        return Ok(resp);
                    }
                    Err(e) => {
                        let do_fallback = should_fallback(&e, &policy);
                        if do_fallback {
                            self.record_failure(state_key);
                        }
                        if opts.disable_fallbacks {
                            return Err(e);
                        }
                        let can_retry_here = attempts_made < hard_cap
                            && should_retry_same_provider(&e, &policy, attempts_made);
                        if can_retry_here {
                            tracing::debug!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                error = ?e,
                                "retrying same provider",
                            );
                            let _ = e;
                            continue;
                        }
                        // Done with this provider. Decide fallback vs propagate.
                        if do_fallback {
                            tracing::warn!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                error = ?e,
                                "fallback to next",
                            );
                            last_err = Some(e);
                            continue 'chain;
                        }
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    pub async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_with_options(req, RouterOptions::default())
            .await
    }

    /// Streaming counterpart. Fallback only happens BEFORE the first
    /// chunk reaches us; once the upstream has emitted a chunk,
    /// mid-stream errors propagate. Gate checks (rate limit / breaker)
    /// run before the upstream is touched.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn stream_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let chain = self.dispatch_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let mut last_err: Option<Error> = None;

        'chain: for target in chain.iter() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            let Some(provider) = target.provider.clone() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };

            // Per-attempt gate: streams don't retry against the same
            // provider, so this is also the "single attempt" gate.
            if let Some((gate_kind, gate_err)) = self.gate_check(state_key, provider_name) {
                tracing::warn!(
                    provider = provider_name,
                    model = %target.nickname.as_deref().unwrap_or(""),
                    gate_kind,
                    error = ?gate_err,
                    "stream gate blocked",
                );
                last_err = Some(gate_err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue 'chain;
            }

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();
            if let Some(defaults) = target.reasoning.as_ref() {
                merge_reasoning_defaults_into(&mut attempt_req, defaults);
            }
            // v0.6: per-model anthropic_beta lifts onto req.anthropic_beta,
            // dedup against existing entries.
            if !target.anthropic_beta.is_empty() {
                merge_model_anthropic_beta_into(&mut attempt_req, &target.anthropic_beta);
            }

            let attempt_policy = self.compose_attempt_policy(
                &policy,
                provider_name,
                target.stream_first_byte_timeout_ms,
            );
            match try_stream_with_first_chunk(provider_name, provider, attempt_req, &attempt_policy)
                .await
            {
                Ok(stream) => {
                    let state = self.state.get(state_key).cloned();
                    let cancel_is_failure = state
                        .as_ref()
                        .is_some_and(|st| st.lock().half_open_probe_in_flight());
                    return Ok(wrap_with_breaker_accounting(
                        stream,
                        state,
                        cancel_is_failure,
                    ));
                }
                Err(e) => {
                    let do_fallback = should_fallback(&e, &policy);
                    if do_fallback {
                        self.record_failure(state_key);
                    }
                    if opts.disable_fallbacks {
                        return Err(e);
                    }
                    if do_fallback {
                        tracing::warn!(
                            provider = provider_name,
                            model = %target.nickname.as_deref().unwrap_or(""),
                            error = ?e,
                            "stream fallback to next",
                        );
                        last_err = Some(e);
                        continue 'chain;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
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
    fn gate_check(
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

    fn record_success(&self, state_key: &str) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().record_success();
        }
    }

    fn record_failure(&self, state_key: &str) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().record_failure(Instant::now());
        }
    }

    /// Resolve the retry policy for the wire `model` field. v0.6.0
    /// removed per-alias retry overrides; the only retry policy is
    /// the top-level `[retry]` table. Pre-v0.6.0 each `[aliases.X]`
    /// could carry a `[aliases.X.retry]` sub-table; that surface was
    /// dropped when `[aliases]` collapsed into a flat
    /// wire-string -> nickname-or-chain map. Operators wanting
    /// different retry behavior per target can split routes into
    /// distinct `[providers.X]` entries (timeouts) or use
    /// per-error-class caps in `[retry]`. The `model` argument is
    /// retained for forward-compat: a future per-model retry surface
    /// would key off this value.
    fn policy_for(&self, _model: &str) -> RetryPolicy {
        self.config.retry.clone()
    }

    /// Overlay the target provider's timeout config onto the
    /// resolved `RetryPolicy`. Provider-level fills in only when the
    /// base left the field None. Both None falls through to reqwest's
    /// default.
    ///
    /// Resolution precedence for `stream_first_byte_timeout_ms`:
    /// per-model (`model_first_byte_timeout_override`) >
    /// per-provider (`ProviderRuntimePolicy.stream_first_byte_timeout_ms`) >
    /// global (`[retry].stream_first_byte_timeout_ms` baked into
    /// `base`). When the per-model override is `Some`, it wins
    /// unconditionally over the provider + global tiers.
    fn compose_attempt_policy(
        &self,
        base: &RetryPolicy,
        provider_name: &str,
        model_first_byte_timeout_override: Option<u64>,
    ) -> RetryPolicy {
        let provider_runtime = self
            .config
            .providers
            .get(provider_name)
            .map(|e| e.runtime());
        let mut out = base.clone();
        if out.request_timeout_ms.is_none() {
            out.request_timeout_ms = provider_runtime.and_then(|p| p.request_timeout_ms);
        }
        // Per-model override wins unconditionally over the
        // provider + global resolution.
        if let Some(ms) = model_first_byte_timeout_override {
            out.stream_first_byte_timeout_ms = Some(ms);
        } else if out.stream_first_byte_timeout_ms.is_none() {
            out.stream_first_byte_timeout_ms =
                provider_runtime.and_then(|p| p.stream_first_byte_timeout_ms);
        }
        out
    }
}

/// Merge a model entry's `anthropic_beta` list into the per-attempt
/// request's `anthropic_beta` field. Deduplicated: request entries
/// preserved first (in their existing order); model entries appended
/// only when not already present.
///
/// Use case: a single provider serves multiple Claude models where
/// only some opt into a beta (e.g. `context-1m-2025-08-07` works on
/// opus/sonnet but is rejected for haiku). Per-model
/// `[models.X] anthropic_beta` lifts the right flags onto the right
/// requests without duplicating the entire provider config.
pub fn merge_model_anthropic_beta_into(req: &mut ChatRequest, model_betas: &[String]) {
    if model_betas.is_empty() {
        return;
    }
    let mut seen: std::collections::BTreeSet<String> = req.anthropic_beta.iter().cloned().collect();
    for entry in model_betas {
        if seen.insert(entry.clone()) {
            req.anthropic_beta.push(entry.clone());
        }
    }
}

/// Convert a chain of `Arc<ResolvedModel>` into the `DispatchTarget`
/// shape the dispatch loop walks. Hoisted out of `dispatch_chain`
/// so the three resolution branches share one builder.
fn into_dispatch_targets(chain: Vec<Arc<ResolvedModel>>) -> Vec<DispatchTarget> {
    chain.into_iter().map(into_one_dispatch_target).collect()
}

fn into_one_dispatch_target(m: Arc<ResolvedModel>) -> DispatchTarget {
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        // v0.6.0 dispatch keys the breaker by nickname so two models
        // on one provider quarantine independently.
        state_key: m.nickname.clone(),
        upstream: m.upstream.clone(),
        provider: Some(m.provider.clone()),
        reasoning: if m.reasoning.is_empty() {
            None
        } else {
            Some(m.reasoning.clone())
        },
        nickname: Some(m.nickname.clone()),
        anthropic_beta: m.anthropic_beta.clone(),
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
    }
}

/// Merge operator-side `ReasoningDefaults` into the per-attempt
/// request's `reasoning` field. Caller's non-None values always win;
/// this only fills in fields the caller left unset. No-op when both
/// `defaults` fields are unset (`is_empty()`).
///
/// Precedence per field: caller (wire) > operator (TOML) > internal
/// default. Composed orthogonally: if the caller supplied `effort`
/// and the operator configured `enabled`, the resulting request
/// carries both.
///
/// `enabled = false` in the TOML is a DEFAULT the caller can still
/// override by sending `reasoning.enabled = true` on the wire. It
/// is not a hard ceiling on reasoning use; operators wanting to
/// pin reasoning off cannot rely on this knob alone.
///
/// Edge case: when the caller has explicitly set
/// `req.reasoning.enabled == Some(false)`, the operator's
/// `thinking` (effort) injection is suppressed. Without this
/// short-circuit the merged result would be
/// `{effort: Some("..."), enabled: Some(false)}`, which different
/// egresses interpret inconsistently (some honor `enabled=false`
/// only when `effort` is also unset, others forward `effort`
/// regardless). Suppressing the effort fill keeps "caller pinned
/// reasoning off" working uniformly across all egresses.
///
/// Why `effort` has an explicit `caller_disabled` guard but
/// `enabled` doesn't: `enabled` injection is naturally guarded by
/// the per-field `cfg.enabled.is_none()` check (a caller-set
/// `enabled = Some(false)` short-circuits the fill on its own).
/// The explicit `caller_disabled` check is needed only on `effort`
/// because the per-field check (`cfg.effort.is_none()`) would
/// otherwise allow operator's `thinking` to inject when the caller
/// left `effort` unset, producing the inconsistent
/// `{effort: Some(...), enabled: Some(false)}` state above. A
/// future operator-side field that should respect the same
/// "caller turned reasoning off" semantics needs the same explicit
/// guard; per-field `is_none()` alone is not sufficient.
pub fn merge_reasoning_defaults_into(req: &mut ChatRequest, defaults: &ReasoningDefaults) {
    if defaults.is_empty() {
        return;
    }

    let caller_disabled = req.reasoning.as_ref().and_then(|r| r.enabled) == Some(false);

    let cfg = req.reasoning.get_or_insert_with(ReasoningConfig::default);

    if cfg.effort.is_none() && !caller_disabled {
        if let Some(t) = &defaults.thinking {
            cfg.effort = Some(t.clone());
        }
    }
    if cfg.enabled.is_none() {
        if let Some(b) = defaults.enabled {
            cfg.enabled = Some(b);
        }
    }
}

/// Execute one upstream `complete()` call with the policy's per-request
/// timeout (if configured). Timeout expiry surfaces as a status-0
/// upstream error, treated as a network class for retry/fallback. The
/// error reports `provider_name` (the config-key the chain walks) so
/// it lines up with `routectl_provider` in responses and with
/// gate-error sources -- never the kind-prefixed `provider.id()`.
async fn run_with_timeout(
    provider_name: &str,
    provider: &dyn Provider,
    req: &ChatRequest,
    policy: &RetryPolicy,
) -> Result<ChatResponse> {
    match policy.request_timeout_ms {
        Some(ms) => {
            match tokio::time::timeout(Duration::from_millis(ms), provider.complete(req.clone()))
                .await
            {
                Ok(r) => r,
                Err(_) => Err(Error::upstream(
                    provider_name,
                    0,
                    format!("request timed out after {ms}ms"),
                )),
            }
        }
        None => provider.complete(req.clone()).await,
    }
}

/// Wrap an upstream stream so the breaker records success on clean
/// completion (None / EOS) and records ONE failure on the first error
/// that bubbles out of the stream. Subsequent errors do not double-count.
///
/// Consumer cancellation after the first upstream chunk is treated as a
/// success for steady-state traffic, but still counts as a failure for a
/// half-open probe because the breaker has not observed a full recovery yet.
fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
    cancel_is_failure: bool,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;
    struct BreakerAccounting {
        state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
        cancel_is_failure: bool,
        settled: bool,
    }

    impl BreakerAccounting {
        fn new(
            state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
            cancel_is_failure: bool,
        ) -> Self {
            Self {
                state,
                cancel_is_failure,
                settled: false,
            }
        }

        fn with_state(&self, f: impl FnOnce(&mut crate::runtime_state::ProviderState)) {
            let Some(st) = &self.state else {
                return;
            };
            f(&mut st.lock());
        }

        fn record_success(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| state.record_success());
        }

        fn record_failure(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| state.record_failure(Instant::now()));
        }
    }

    impl Drop for BreakerAccounting {
        fn drop(&mut self) {
            if !self.settled {
                if self.cancel_is_failure {
                    self.record_failure();
                } else {
                    self.record_success();
                }
            }
        }
    }

    let mut accounting = BreakerAccounting::new(state, cancel_is_failure);
    let s = async_stream::stream! {
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            if item.is_err() {
                accounting.record_failure();
            }
            yield item;
        }
        accounting.record_success();
    };
    Box::pin(s)
}

/// Open the upstream stream and pull the first chunk. If that initial step
/// fails with a fallbackable error, return it so the caller can try the next
/// provider. If the first chunk arrives, return a `BoxStream` that yields it
/// followed by the rest of the upstream stream -- mid-stream errors propagate.
///
/// `policy.stream_first_byte_timeout_ms` (when set) caps the wait for the
/// stream-open + first-chunk arrival; expiry surfaces as a status-0
/// upstream error which is fallbackable per `should_fallback`.
async fn try_stream_with_first_chunk(
    provider_name: &str,
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    let open_and_first = async {
        let mut upstream = provider.stream(req).await?;
        match upstream.next().await {
            Some(Ok(first)) => {
                let merged = futures::stream::once(async move { Ok(first) }).chain(upstream);
                Ok(merged.boxed())
            }
            Some(Err(e)) => Err(e),
            // Upstream returned an empty stream (stream() returned Ok
            // but no chunk ever arrived). This is NOT a successful
            // empty completion -- a healthy provider always emits at
            // least one chunk (even just a usage tail). Treat as a
            // fallbackable streaming error so the chain walks to the
            // next provider AND the breaker records a failed probe.
            // Without this, an upstream that closes the connection
            // before producing any data would be reported as a
            // successful completion to both the client and the
            // router's health accounting.
            None => Err(Error::Streaming(format!(
                "{provider_name} stream closed before any chunk arrived",
            ))),
        }
    };

    match policy.stream_first_byte_timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), open_and_first).await {
            Ok(r) => r,
            Err(_) => Err(Error::upstream(
                provider_name,
                0,
                format!("stream first-byte timeout after {ms}ms"),
            )),
        },
        None => open_and_first.await,
    }
}

fn should_fallback(err: &Error, policy: &RetryPolicy) -> bool {
    match err {
        // status 0 means we never reached the upstream HTTP layer
        // (DNS, TCP connect, TLS handshake, request body, timeout). Always
        // fallbackable -- nothing upstream-specific has happened yet.
        Error::Upstream { status: 0, .. } => true,
        Error::Upstream { status, .. } => policy.fallback_on_status.contains(status),
        Error::Streaming(_) => true,
        Error::UnknownProvider(_) => true,
        _ => false,
    }
}

fn should_retry_same_provider(err: &Error, policy: &RetryPolicy, attempts_made: u32) -> bool {
    let cap = match err {
        Error::Upstream { status, .. } => policy.retries_for_status(*status),
        // Streaming errors are transport-level (broken connection
        // mid-stream, partial frame, decode failure on the wire) --
        // semantically network-class, not 5xx-class. Bucket them under
        // `retry_on_network` so configuration matches the error class.
        Error::Streaming(_) => policy.retry_on_network.unwrap_or(policy.max_attempts),
        _ => 0,
    };
    attempts_made < cap
}

fn add_jitter(base: Duration, jitter_ms: u64) -> Duration {
    if jitter_ms == 0 {
        return base;
    }
    // Non-cryptographic jitter from the wall clock's sub-millisecond
    // bits. Suitable for retry-spreading; not for anything else.
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base.saturating_add(Duration::from_millis(nanos % jitter_ms))
}

fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;

    /// Build a Router with one openai-compat provider that has the given
    /// runtime-policy timeouts, and an alias chain of length 1 pointing
    /// at it. The base RetryPolicy passed to compose_attempt_policy
    /// represents what `policy_for(alias)` resolved to.
    fn build_router_with_provider_timeouts(
        provider_request_timeout: Option<u64>,
        provider_first_byte_timeout: Option<u64>,
    ) -> Router {
        let mut providers = BTreeMap::new();
        let mut entry = ProviderEntry::openai_compat("https://example.test/v1", "literal:k");
        if let ProviderEntry::OpenaiCompat { runtime, .. } = &mut entry {
            runtime.request_timeout_ms = provider_request_timeout;
            runtime.stream_first_byte_timeout_ms = provider_first_byte_timeout;
        }
        providers.insert("p1".to_string(), entry);

        let cfg = Config {
            providers,
            ..Default::default()
        };
        Router::new(Arc::new(cfg))
    }

    #[test]
    fn compose_inherits_timeout_from_provider_when_alias_left_none() {
        // Alias-resolved policy has no timeout overrides.
        // Provider config sets both timeouts.
        // Expected: provider's values land in the per-attempt policy.
        let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
        let base = RetryPolicy::default(); // both timeout fields None
        let composed = router.compose_attempt_policy(&base, "p1", None);
        assert_eq!(composed.request_timeout_ms, Some(180_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(60_000));
    }

    #[test]
    fn compose_alias_override_wins_over_provider() {
        // Alias-resolved policy has BOTH timeouts set explicitly.
        // Provider config also sets values. Alias wins.
        let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
        let base = RetryPolicy {
            request_timeout_ms: Some(30_000),
            stream_first_byte_timeout_ms: Some(5_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1", None);
        assert_eq!(composed.request_timeout_ms, Some(30_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(5_000));
    }

    #[test]
    fn compose_independent_per_field_resolution() {
        // Alias sets ONLY request_timeout_ms; provider sets ONLY
        // stream_first_byte_timeout_ms. Expected: each field falls
        // through independently.
        let router = build_router_with_provider_timeouts(None, Some(120_000));
        let base = RetryPolicy {
            request_timeout_ms: Some(45_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1", None);
        assert_eq!(composed.request_timeout_ms, Some(45_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(120_000));
    }

    #[test]
    fn compose_no_provider_entry_passes_base_through_unchanged() {
        // If the chain entry's provider isn't in config (e.g. test
        // harness that registered a Provider without adding a config
        // ProviderEntry), provider-level lookup returns None and the
        // base policy survives unchanged.
        let router = build_router_with_provider_timeouts(Some(99_999), Some(99_999));
        let base = RetryPolicy {
            request_timeout_ms: Some(7_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "missing-provider", None);
        assert_eq!(composed.request_timeout_ms, Some(7_000));
        assert!(composed.stream_first_byte_timeout_ms.is_none());
    }

    #[test]
    fn compose_no_overrides_anywhere_yields_none() {
        // Belt-and-braces: alias = None, provider = None, default
        // policy = None. composed.request_timeout_ms stays None
        // (router falls through to reqwest's default).
        let router = build_router_with_provider_timeouts(None, None);
        let base = RetryPolicy::default();
        let composed = router.compose_attempt_policy(&base, "p1", None);
        assert!(composed.request_timeout_ms.is_none());
        assert!(composed.stream_first_byte_timeout_ms.is_none());
    }

    #[test]
    fn compose_model_first_byte_timeout_wins_over_provider_and_global() {
        // Per-model > per-provider > global. The per-model override
        // pins 5000 even though global is 90000 and provider is 60000.
        let router = build_router_with_provider_timeouts(None, Some(60_000));
        let base = RetryPolicy {
            stream_first_byte_timeout_ms: Some(90_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1", Some(5_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(5_000));
    }

    #[test]
    fn compose_model_first_byte_timeout_none_falls_back_to_provider_resolution() {
        // No per-model override -> provider + global path resolves
        // exactly as before. With base unset, the provider's value wins.
        let router = build_router_with_provider_timeouts(None, Some(60_000));
        let base = RetryPolicy::default();
        let composed = router.compose_attempt_policy(&base, "p1", None);
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(60_000));
    }

    #[test]
    fn compose_model_first_byte_timeout_wins_over_base_too() {
        // Per-model override beats base (global) even when base is set.
        // Pins the per-model > global precedence regardless of provider state.
        let router = build_router_with_provider_timeouts(None, None);
        let base = RetryPolicy {
            stream_first_byte_timeout_ms: Some(45_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1", Some(10_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(10_000));
    }
}

#[cfg(test)]
mod merge_model_anthropic_beta_tests {
    use super::*;

    fn req_with_betas(betas: Vec<&str>) -> ChatRequest {
        ChatRequest {
            model: "any".into(),
            messages: vec![],
            anthropic_beta: betas.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_model_betas_is_noop() {
        let mut req = req_with_betas(vec!["a"]);
        merge_model_anthropic_beta_into(&mut req, &[]);
        assert_eq!(req.anthropic_beta, vec!["a".to_string()]);
    }

    #[test]
    fn appends_new_entries_preserves_request_order() {
        // Request: ["a"], Model: ["a", "b"] -> result ["a", "b"].
        // Request entries preserved first (in their existing order);
        // model entries appended only when not already present.
        let mut req = req_with_betas(vec!["a"]);
        merge_model_anthropic_beta_into(&mut req, &["a".into(), "b".into()]);
        assert_eq!(req.anthropic_beta, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn dedupes_duplicates_within_request() {
        // Even when the request already had duplicates, the model's
        // entries don't re-add anything already present in the set.
        // Existing request duplicates are preserved (the dedup only
        // gates new entries from the model side).
        let mut req = req_with_betas(vec!["a", "b"]);
        merge_model_anthropic_beta_into(&mut req, &["b".into(), "c".into()]);
        assert_eq!(
            req.anthropic_beta,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn empty_request_betas_with_model_betas_populates_list() {
        let mut req = req_with_betas(vec![]);
        merge_model_anthropic_beta_into(&mut req, &["x".into(), "y".into()]);
        assert_eq!(req.anthropic_beta, vec!["x".to_string(), "y".to_string()]);
    }
}

#[cfg(test)]
mod model_anthropic_beta_lift_tests {
    //! Integration: pin that the per-model `anthropic_beta` list lifts
    //! onto the upstream request on both the complete and stream
    //! dispatch paths.
    use super::*;
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use parking_lot::Mutex as ParkingMutex;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
    use std::collections::BTreeMap;

    struct CapturingProvider {
        id: String,
        captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
            Ok(None)
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            let model = req.model.clone();
            self.captured.lock().push(req);
            Ok(ChatResponse {
                id: "ok".into(),
                model,
                created: 0,
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: routectl_core::Role::Assistant,
                        content: routectl_core::MessageContent::Text("ok".into()),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                    finish_reason: Some("stop".into()),
                    matched_stop_sequence: None,
                }],
                usage: Some(routectl_core::Usage::default()),
                routectl_provider: None,
                extras: Default::default(),
            })
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.captured.lock().push(req);
            // Emit one minimal chunk so the stream path completes.
            let s = futures::stream::once(async move {
                Ok(ChatChunk {
                    id: "c0".into(),
                    model: "x".into(),
                    choices: vec![],
                    usage: None,
                })
            });
            Ok(s.boxed())
        }
    }

    fn router_with_capture(
        anthropic_beta: Vec<String>,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let mut resolved = ResolvedModel::new("haiku", "anthropic", provider, "claude-haiku-4-5");
        if !anthropic_beta.is_empty() {
            resolved = resolved.with_anthropic_beta(anthropic_beta);
        }
        models.insert("haiku".into(), Arc::new(resolved));
        router.install_resolved_models(models);
        (router, captured)
    }

    #[tokio::test]
    async fn complete_path_lifts_model_anthropic_beta_onto_request() {
        let (router, captured) = router_with_capture(vec![
            "context-1m-2025-08-07".into(),
            "prompt-cache-1h".into(),
        ]);
        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            anthropic_beta: vec!["client-supplied".into()],
            ..Default::default()
        };
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let upstream = captured.first().expect("one upstream call");
        // Request entries preserved first, model entries appended.
        assert_eq!(
            upstream.anthropic_beta,
            vec![
                "client-supplied".to_string(),
                "context-1m-2025-08-07".to_string(),
                "prompt-cache-1h".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn stream_path_lifts_model_anthropic_beta_onto_request() {
        let (router, captured) = router_with_capture(vec!["context-1m-2025-08-07".into()]);
        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            anthropic_beta: vec![],
            ..Default::default()
        };
        let _ = router
            .stream(req)
            .await
            .expect("ok")
            .collect::<Vec<_>>()
            .await;
        let captured = captured.lock();
        let upstream = captured.first().expect("one upstream call");
        assert_eq!(
            upstream.anthropic_beta,
            vec!["context-1m-2025-08-07".to_string()]
        );
    }
}

#[cfg(test)]
mod merge_reasoning_defaults_tests {
    //! Tests for the per-attempt reasoning-defaults merge. v0.6.0
    //! drives this from the resolved-model table; the merge logic
    //! itself lives in the free `merge_reasoning_defaults_into`
    //! function.
    use super::*;
    use crate::config::ReasoningDefaults;

    fn req_with_reasoning(reasoning: Option<ReasoningConfig>) -> ChatRequest {
        ChatRequest {
            model: "a".into(),
            messages: vec![],
            reasoning,
            ..Default::default()
        }
    }

    #[test]
    fn merge_no_caller_no_defaults_leaves_none() {
        // Provider has no defaults; caller didn't supply `reasoning`.
        // Result must remain None -- the merge step must not synthesize
        // an empty ReasoningConfig.
        let mut req = req_with_reasoning(None);
        merge_reasoning_defaults_into(&mut req, &ReasoningDefaults::default());
        assert!(req.reasoning.is_none());
    }

    #[test]
    fn merge_no_caller_with_defaults_inserts_full_config() {
        let defaults = ReasoningDefaults::new()
            .with_thinking("high")
            .with_enabled(true);
        let mut req = req_with_reasoning(None);
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning was inserted");
        assert_eq!(cfg.effort.as_deref(), Some("high"));
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    fn merge_caller_effort_minimal_beats_defaults_high() {
        let defaults = ReasoningDefaults::new().with_thinking("high");
        let mut req = req_with_reasoning(Some(ReasoningConfig {
            effort: Some("minimal".into()),
            ..ReasoningConfig::default()
        }));
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning preserved");
        assert_eq!(cfg.effort.as_deref(), Some("minimal"));
    }

    #[test]
    fn merge_caller_enabled_false_beats_defaults_true() {
        // Caller pinned `enabled = false`; operator default is true.
        // Caller wins. Some(false) must NOT collapse to None on the
        // merge path.
        let defaults = ReasoningDefaults::new().with_enabled(true);
        let mut req = req_with_reasoning(Some(ReasoningConfig {
            enabled: Some(false),
            ..ReasoningConfig::default()
        }));
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning preserved");
        assert_eq!(cfg.enabled, Some(false));
    }

    #[test]
    fn merge_caller_enabled_false_blocks_operator_thinking_fill() {
        // Pin: when the caller has explicitly disabled reasoning via
        // `enabled = false`, the operator's `thinking` (effort) must
        // NOT be injected. Otherwise the merged request becomes
        // `{effort: Some("high"), enabled: Some(false)}` which
        // different egresses interpret inconsistently. Suppressing the
        // effort fill keeps "caller pinned reasoning off" uniform
        // across every egress.
        let defaults = ReasoningDefaults::new().with_thinking("high");
        let mut req = req_with_reasoning(Some(ReasoningConfig {
            enabled: Some(false),
            ..ReasoningConfig::default()
        }));
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning preserved");
        assert!(
            cfg.effort.is_none(),
            "operator thinking must NOT be injected when caller disabled reasoning; got effort={:?}",
            cfg.effort,
        );
        assert_eq!(cfg.enabled, Some(false));
    }

    #[test]
    fn merge_fills_both_when_caller_has_neither() {
        // Caller carries an empty ReasoningConfig (e.g. the wire body
        // had `reasoning: {}`); operator defaults supply both.
        let defaults = ReasoningDefaults::new()
            .with_thinking("medium")
            .with_enabled(true);
        let mut req = req_with_reasoning(Some(ReasoningConfig::default()));
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning present");
        assert_eq!(cfg.effort.as_deref(), Some("medium"));
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    fn merge_composes_orthogonal_fields() {
        // Caller supplied `effort` only; operator configured `enabled`
        // only. Result has both -- merge is per-field, not
        // all-or-nothing.
        let defaults = ReasoningDefaults::new().with_enabled(true);
        let mut req = req_with_reasoning(Some(ReasoningConfig {
            effort: Some("low".into()),
            ..ReasoningConfig::default()
        }));
        merge_reasoning_defaults_into(&mut req, &defaults);
        let cfg = req.reasoning.expect("reasoning present");
        assert_eq!(cfg.effort.as_deref(), Some("low"));
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    fn merge_all_none_defaults_leaves_caller_unchanged() {
        // Provider has no defaults configured. Caller supplied a fully
        // populated ReasoningConfig. Merge must be a no-op.
        let initial = ReasoningConfig {
            effort: Some("medium".into()),
            enabled: Some(true),
            max_tokens: Some(2048),
            exclude: Some(false),
        };
        let mut req = req_with_reasoning(Some(initial.clone()));
        merge_reasoning_defaults_into(&mut req, &ReasoningDefaults::default());
        let cfg = req.reasoning.expect("reasoning preserved");
        assert_eq!(cfg.effort, initial.effort);
        assert_eq!(cfg.enabled, initial.enabled);
        assert_eq!(cfg.max_tokens, initial.max_tokens);
        assert_eq!(cfg.exclude, initial.exclude);
    }
}

#[cfg(test)]
mod resolved_models_tests {
    //! Tests for the v0.6.0 dispatch path. Builds a router with an
    //! installed `ResolvedModel` table and verifies dispatch walks
    //! the chain correctly, including the "wire model maps to a
    //! direct nickname" path and the "alias chain that references
    //! an unknown nickname" startup-validation path (the latter
    //! enforced at `install_resolved_models` callers in C4).
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountedProvider {
        id: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for CountedProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
            Ok(None)
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: routectl_core::Role::Assistant,
                        content: routectl_core::MessageContent::Text("ok".into()),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                    finish_reason: Some("stop".into()),
                    matched_stop_sequence: None,
                }],
                usage: Some(routectl_core::Usage::default()),
                routectl_provider: None,
                extras: Default::default(),
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    fn router_with_resolved(table: Vec<(&str, &str, &str, Arc<dyn Provider>)>) -> Router {
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        for (nickname, provider_name, upstream, p) in table {
            models.insert(
                nickname.to_string(),
                Arc::new(ResolvedModel::new(nickname, provider_name, p, upstream)),
            );
        }
        router.install_resolved_models(models);
        router
    }

    #[tokio::test]
    async fn dispatch_resolves_wire_string_to_nickname_directly() {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "anthropic-test".into(),
            calls: AtomicUsize::new(0),
        });
        let router =
            router_with_resolved(vec![("haiku", "anthropic", "claude-haiku-4-5", p.clone())]);
        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
        assert_eq!(resp.model, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn install_resolved_models_creates_runtime_state_per_nickname() {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p-test".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_resolved(vec![
            ("alpha", "p-shared", "u1", p.clone()),
            ("beta", "p-shared", "u2", p.clone()),
        ]);
        // Both nicknames present in the resolved table.
        assert!(router.resolved_models.contains_key("alpha"));
        assert!(router.resolved_models.contains_key("beta"));
        // v0.6.0 keys runtime state by nickname so two models on one
        // provider quarantine independently. Both nicknames must own
        // their own slot.
        assert!(router.state.contains_key("alpha"));
        assert!(router.state.contains_key("beta"));
    }

    #[tokio::test]
    async fn per_model_breaker_isolates_failures() {
        // Pin: when two models share one provider entry, tripping
        // model A's breaker does NOT block model B from dispatching.
        // Pre-rc.2 this regressed because state was keyed by provider
        // name (one breaker shared across all models on that provider).
        struct AlwaysFailing {
            id: String,
        }
        #[async_trait]
        impl Provider for AlwaysFailing {
            fn id(&self) -> &str {
                &self.id
            }
            fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
                Err(Error::normalize_response(&self.id, "unused"))
            }
            fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
                Ok(None)
            }
            async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
                Err(Error::upstream(&self.id, 0, "always fails"))
            }
            async fn stream(
                &self,
                _: ChatRequest,
            ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
                unreachable!()
            }
        }

        // Provider with a 1-failure breaker. Both models share it.
        let mut config = Config::default();
        config.providers.insert(
            "p-shared".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                extra_headers: BTreeMap::new(),
                reasoning_dialect: crate::config::ReasoningDialect::Openai,
                history_reasoning: crate::config::HistoryReasoning::default(),
                user_agent: None,
                runtime: ProviderRuntimePolicy {
                    circuit_failures: Some(1),
                    circuit_cooldown_ms: Some(60_000),
                    ..Default::default()
                },
            },
        );

        let mut router = Router::new(Arc::new(config));
        let p_a: Arc<dyn Provider> = Arc::new(AlwaysFailing { id: "a".into() });
        let p_b: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "b".into(),
            calls: AtomicUsize::new(0),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "alpha".into(),
            Arc::new(ResolvedModel::new("alpha", "p-shared", p_a, "u1")),
        );
        models.insert(
            "beta".into(),
            Arc::new(ResolvedModel::new("beta", "p-shared", p_b, "u2")),
        );
        router.install_resolved_models(models);

        // Trip alpha's breaker: one failed dispatch puts it Open.
        let req_a = ChatRequest {
            model: "alpha".into(),
            messages: vec![],
            ..Default::default()
        };
        let _ = router.complete(req_a).await; // failure, breaker trips

        // Beta MUST still be routable. Pre-fix (state keyed by
        // provider) this returned a circuit_breaker gate-block error.
        let req_b = ChatRequest {
            model: "beta".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req_b).await.expect(
            "beta dispatch must succeed even though alpha's breaker is tripped; \
             same-provider models must not share a breaker",
        );
        assert_eq!(resp.routectl_provider.as_deref(), Some("p-shared"));
    }

    #[test]
    fn dispatch_chain_unknown_nickname_returns_unknown_alias() {
        // When the wire model isn't a known nickname AND has no
        // alias-table hit, dispatch_chain returns UnknownAlias.
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_resolved(vec![("haiku", "anthropic", "u", p)]);
        let res = router.dispatch_chain("does-not-exist");
        assert!(matches!(res, Err(Error::UnknownAlias(_))));
    }

    #[tokio::test]
    async fn alias_entry_shadows_direct_model_nickname() {
        // Pin: when the same string is both a `[models.X]` nickname
        // AND an `[aliases]` key, the alias wins. Operators rely on
        // this to prepend a fallback chain to an existing model
        // without renaming the nickname.
        let p_direct: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "direct-p".into(),
            calls: AtomicUsize::new(0),
        });
        let p_via_alias: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "alias-p".into(),
            calls: AtomicUsize::new(0),
        });
        // Build a config where "foo" is both a nickname AND an alias
        // pointing at a different nickname. Dispatch must hit the
        // alias's target, not the direct nickname.
        let mut config = Config::default();
        config
            .aliases
            .insert("foo".into(), AliasValue::Single("backup".into()));
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "foo".into(),
            Arc::new(ResolvedModel::new(
                "foo",
                "p-direct",
                p_direct.clone(),
                "u-direct",
            )),
        );
        models.insert(
            "backup".into(),
            Arc::new(ResolvedModel::new(
                "backup",
                "p-alias",
                p_via_alias.clone(),
                "u-alias",
            )),
        );
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "foo".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        // Alias wins: dispatch landed on the `backup` model's
        // provider, not the direct `foo` model's provider.
        assert_eq!(resp.routectl_provider.as_deref(), Some("p-alias"));
        assert_eq!(resp.model, "u-alias");
    }
}
