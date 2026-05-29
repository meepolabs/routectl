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
    sanitize_for_log, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
    RoutectlInternal, TokenCount,
};
use serde_json::Value;

use crate::config::{AliasValue, Config, HistoryReasoning, ReasoningDialect, RetryPolicy};
use crate::glob::PrefixIndex;
use crate::resolved::ResolvedModel;
use crate::runtime_state::{GateDecision, ProviderState};

/// Auth-reserved header names. Dispatch-layer compose WARN-drops any
/// `header_extras` entry matching one of these so an operator-typed
/// override can't silently replace routectl's resolved auth header.
/// Case-insensitive.
const AUTH_HEADERS: &[&str] = &["authorization", "x-api-key", "anthropic-version"];

/// Managed-reserved header names. Dispatch-layer compose DEBUG-drops
/// these (operator's wire-shape mistake, not a security issue).
/// `anthropic-beta` is intentionally NOT here -- operators own that
/// slot via `header_extras` and the list-valued post-pass unions all
/// three sources. Case-insensitive.
const MANAGED_HEADERS: &[&str] = &["host", "content-type", "content-length"];

/// Header names whose values are comma-separated lists and must be
/// unioned across sources (ingress -> provider -> model) rather than
/// last-writer-wins on collision. Currently just `anthropic-beta`.
const LIST_VALUED_HEADERS: &[&str] = &["anthropic-beta"];

/// Maximum recursion depth for nested alias resolution. Belt-and-
/// suspenders for the dispatch path: cycles are caught at startup by
/// `factory::validate_alias_chain_targets`, but a glob-shadow edge
/// case the static walk missed could still introduce one at runtime.
/// Depth > 8 hops fails the request with a clear `Error::Config`
/// rather than recurse indefinitely.
const ALIAS_MAX_RECURSION_DEPTH: usize = 8;

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
    /// Operator-facing provider name (a key in `[providers]`).
    provider_name: String,
    /// Key into `Router.state` for the per-attempt rate-limit + circuit-
    /// breaker check.
    state_key: String,
    /// Wire model id sent to the provider.
    upstream: String,
    /// Concrete provider instance.
    provider: Option<Arc<dyn Provider>>,
    /// Whether the model supports adaptive (extended) thinking.
    /// Threaded from `ResolvedModel.supports_adaptive_thinking` so
    /// `apply_layered_overlays` can set `RoutectlInternal` without
    /// reaching back into `ResolvedModel`.
    supports_adaptive_thinking: bool,
    /// Model nickname for tracing.
    nickname: Option<String>,
    /// Per-model `header_extras`. Merged with the provider's
    /// `header_extras` at dispatch (model wins on key collision;
    /// list-valued post-pass for `anthropic-beta`).
    model_header_extras: BTreeMap<String, String>,
    /// Per-model `payload_extras`. Deep-merged with the provider's
    /// `payload_extras` (model wins on leaf collision).
    model_payload_extras: Option<Value>,
    /// Per-model openai-compat reasoning dialect. `None` falls back
    /// to the egress's own default.
    reasoning_dialect: Option<ReasoningDialect>,
    /// Per-model openai-compat outgoing-history reasoning policy.
    history_reasoning: Option<HistoryReasoning>,
    /// Per-model `stream_first_byte_timeout_ms`.
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
        if let Some(chain) = self.resolve_v6_alias(model)? {
            return Ok(into_dispatch_targets(chain));
        }
        // Wire model could ALSO be a direct nickname.
        if let Some(m) = self.resolve_nickname(model) {
            return Ok(vec![into_one_dispatch_target(m)]);
        }
        // Catch-all: only consulted after exact alias / glob / direct
        // nickname all miss. This ordering means a wire model that's
        // a known nickname always wins over a configured default.
        if let Some(chain) = self.resolve_default_alias()? {
            return Ok(into_dispatch_targets(chain));
        }
        Err(Error::UnknownAlias(model.to_string()))
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
    fn dispatch_chain_for_request(&self, req: &ChatRequest) -> Result<Vec<DispatchTarget>> {
        let chain = self.dispatch_chain(&req.model)?;
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let features = crate::feature_keys::derive_feature_keys(tools);
        self.filter_chain_by_features(chain, &features, &req.model)
    }

    /// Filter the resolved chain by request features. Per-provider
    /// `unsupported_features` lists are consulted via the provider
    /// table; an entry whose `unsupported_features` intersects the
    /// request feature set is dropped with a DEBUG log.
    ///
    /// No-ops when `features` is empty (no built-in tool in the
    /// request -> nothing to filter against). Returns
    /// `Error::NotImplemented` only when the input chain was non-empty,
    /// at least one feature is in the request, AND every entry got
    /// filtered out -- the architect's "terminal empty-chain" path. A
    /// chain that was empty before filtering surfaces via the existing
    /// `Err(Error::UnknownAlias(...))` path on `dispatch_chain`.
    fn filter_chain_by_features(
        &self,
        chain: Vec<DispatchTarget>,
        features: &[String],
        alias: &str,
    ) -> Result<Vec<DispatchTarget>> {
        if features.is_empty() || chain.is_empty() {
            return Ok(chain);
        }
        let mut filtered: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        for target in chain {
            let unsupported_intersect = self
                .config
                .providers
                .get(&target.provider_name)
                .map(|e| {
                    let unsupported = &e.runtime().unsupported_features;
                    features
                        .iter()
                        .find(|f| unsupported.iter().any(|u| u == *f))
                        .cloned()
                })
                .unwrap_or(None);
            match unsupported_intersect {
                Some(feature) => {
                    tracing::debug!(
                        provider = %target.provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        feature = %feature,
                        "provider skipped: feature in unsupported_features list",
                    );
                }
                None => {
                    filtered.push(target);
                }
            }
        }
        if filtered.is_empty() {
            let feature_list = features.join(", ");
            tracing::warn!(
                alias = %alias,
                features = %feature_list,
                "alias chain filtered to empty by unsupported_features; \
                 no provider in chain supports the requested features",
            );
            return Err(Error::NotImplemented(
                alias.to_string(),
                format!("no provider in chain supports features: {feature_list}"),
            ));
        }
        Ok(filtered)
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
        let chain = self.dispatch_chain_for_request(&req)?;
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        let hard_cap = policy.hard_retry_cap();
        // Availability-probe detection, computed ONCE: `max_tokens` is
        // stable across the chain (overlays never touch it). Claude
        // Code sends max_tokens=1 probes whose output is unread; on a
        // 429/529 these fast-fail instead of spraying retry+fallback
        // across the all-Anthropic chain. See `should_fallback`.
        let is_probe = is_probe_request(&req, &policy);
        let mut last_err: Option<Error> = None;

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
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
            // v0.6: layered config compose. The provider's
            // header_extras + payload_extras are looked up by
            // provider_name; the model's contribution lives on the
            // dispatch target.
            apply_layered_overlays(&self.config, target, &mut attempt_req);

            let mut backoff = Duration::from_millis(policy.initial_backoff_ms);
            let mut attempts_made: u32 = 0;
            // Per-chain-entry one-shot auth-recovery flag. Set after a
            // 401 triggers `provider.on_auth_failure()`; ensures we
            // retry the SAME chain entry with the freshly-rotated
            // token at most once. Reset implicitly when the outer
            // 'chain loop moves to the next target. Note: if the
            // operator has the same provider in the chain twice
            // (different model overrides), each entry gets its own
            // flag -- the per-provider single-flight in OAuthStore
            // (double-check on access_token equality) is the safety
            // net that prevents redundant rotations across entries.
            let mut auth_retry_attempted = false;

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
                        // Auth-401 single-flight refresh: when the
                        // upstream rejects the token (typically a 401
                        // on a stale oauth:// credential that slipped
                        // past the near-expiry check), ask the provider
                        // to rotate via on_auth_failure, then retry
                        // exactly once. The OAuth store's per-provider
                        // mutex collapses concurrent 401-storms to a
                        // single token-endpoint POST. A non-Ok return
                        // means the OAuth identity is dead (revoked,
                        // network-failure to token endpoint); surface
                        // that immediately rather than walk the
                        // fallback chain over a known-broken
                        // credential.
                        if !auth_retry_attempted
                            && matches!(&e, Error::Upstream { status: 401, .. })
                        {
                            auth_retry_attempted = true;
                            tracing::debug!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                "upstream 401; refreshing auth and retrying once",
                            );
                            provider.on_auth_failure().await?;
                            continue;
                        }
                        let do_fallback = should_fallback(&e, &policy, is_probe);
                        // Probe fast-fail: a probe (max_tokens <=
                        // probe_max_tokens) that hit a rate-limit/overload
                        // (429/529) returns the status immediately via an
                        // explicit early return -- no retry, no fallback.
                        // The span below is a deliberate no-op for this
                        // path: record_failure is gated on `do_fallback`
                        // and the retry branch on `can_retry_here`, both of
                        // which are false for a fast-failed probe. Returning
                        // here is behavior-preserving today AND robust if
                        // either predicate ever drifts.
                        let probe_fast_failed = if is_probe {
                            probe_fast_fail_status(&e)
                        } else {
                            None
                        };
                        if let Some(status) = probe_fast_failed {
                            log_probe_fast_fail(
                                provider_name,
                                target.nickname.as_deref().unwrap_or(""),
                                status,
                                req.max_tokens,
                            );
                            return Err(e);
                        }
                        if do_fallback {
                            self.record_failure(state_key);
                        }
                        if opts.disable_fallbacks {
                            return Err(e);
                        }
                        let can_retry_here = attempts_made < hard_cap
                            && should_retry_same_provider(&e, &policy, attempts_made, is_probe);
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
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    error = ?e,
                                    "fallback to next",
                                );
                            } else {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    error = ?e,
                                    "chain exhausted; no fallback target available; request will fail",
                                );
                            }
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

    /// Probe call: route a request to the FIRST provider in the
    /// dispatch chain and call `Provider::count_tokens`. Used by
    /// claude-code's context-budget display via the
    /// `/v1/messages/count_tokens` endpoint.
    ///
    /// Why first-only (no fallback chain walk): count_tokens reports
    /// tokens for the upstream's tokenizer. Falling back to a
    /// different model would return tokens computed by a different
    /// tokenizer, which would silently miscount the caller's budget.
    /// On `Error::NotImplemented`, the error propagates to the caller
    /// verbatim; this function does not enter the dispatch retry
    /// loop, so no retry/fallback semantics apply. Callers (the
    /// count_tokens handler) translate `NotImplemented` to a 501
    /// response per the gateway-doc contract.
    ///
    /// count_tokens calls consume the same RPM bucket and honor the
    /// same circuit breaker as messages calls: the gate runs before
    /// the upstream is touched, and a successful or failed probe
    /// records into the breaker exactly like `complete()`. This
    /// prevents probe-spam from bypassing operator rate limits.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let chain = self.dispatch_chain_for_request(&req)?;
        let target = chain
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnknownAlias(req.model.clone()))?;
        let provider = target
            .provider
            .clone()
            .ok_or_else(|| Error::UnknownProvider(target.provider_name.clone()))?;
        // Bind locals up front so the 401-recovery debug log carries
        // the same field shape (`provider`, `model`) as the sibling
        // dispatch sites in `complete_with_options` and
        // `stream_with_options`. Without these the count_tokens log
        // line looks subtly different from the other two and an
        // operator filtering by model loses count_tokens entries.
        let provider_name = target.provider_name.as_str();
        let model_label = target.nickname.as_deref().unwrap_or("");

        // Gate: rate limit + circuit breaker. Mirrors
        // `complete_with_options` and `stream_with_options` so a
        // count_tokens probe cannot bypass operator rate limits.
        // Unlike `complete_with_options`, count_tokens does NOT walk
        // the fallback chain on a gate block (tokenizer correctness
        // rules out walking the chain), so we propagate the gate
        // error directly.
        if let Some((gate_kind, gate_err)) =
            self.gate_check(&target.state_key, &target.provider_name)
        {
            tracing::warn!(
                provider = %target.provider_name,
                model = %target.nickname.as_deref().unwrap_or(""),
                gate_kind,
                error = ?gate_err,
                "count_tokens gate blocked",
            );
            return Err(gate_err);
        }

        // Apply the same per-attempt overlays the messages path does
        // so header_extras / payload_extras are consistent. This matters
        // for `anthropic-beta` flags -- count_tokens must observe the
        // same beta surface as the messages endpoint or the upstream may
        // reject a request that would have been accepted on /v1/messages.
        let mut attempt_req = req.clone();
        attempt_req.model = target.upstream.clone();
        apply_layered_overlays(&self.config, &target, &mut attempt_req);

        let mut auth_retry_attempted = false;
        let mut attempts_made: u32 = 0;
        loop {
            let result = provider.count_tokens(attempt_req.clone()).await;
            attempts_made += 1;
            match result {
                Ok(tc) => {
                    self.record_success(&target.state_key);
                    return Ok(tc);
                }
                Err(e) => {
                    // Auth-401 single-flight refresh: if the upstream
                    // rejected our token, ask the provider to rotate
                    // (oauth:// refs land in the OAuth store's per-
                    // provider mutex), then retry the same provider
                    // exactly once. A failure to refresh propagates
                    // immediately rather than masking a dead OAuth
                    // identity by walking the fallback chain.
                    if !auth_retry_attempted && matches!(&e, Error::Upstream { status: 401, .. }) {
                        auth_retry_attempted = true;
                        tracing::debug!(
                            provider = provider_name,
                            model = model_label,
                            attempt = attempts_made,
                            "count_tokens 401; refreshing auth and retrying once",
                        );
                        provider.on_auth_failure().await?;
                        continue;
                    }
                    // Mirror `complete_with_options::should_fallback`:
                    // status-0 / 5xx-class errors record a breaker
                    // failure; client-class errors (NotImplemented,
                    // 4xx) do NOT count against the breaker. Use the
                    // chain-resolved policy (same source as the
                    // sibling dispatch sites) so a future per-model
                    // retry surface stays aligned for count_tokens
                    // when it lands.
                    //
                    // is_probe=false: count_tokens is NOT a generation
                    // probe. Its token-count result IS consumed by the
                    // caller (claude-code's context-budget display) and
                    // it never walks the fallback chain, so a 429 here
                    // keeps its existing breaker-accounting behavior.
                    if should_fallback(&e, &self.policy_for(&req.model), false) {
                        self.record_failure(&target.state_key);
                    }
                    return Err(e);
                }
            }
        }
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
        let chain = self.dispatch_chain_for_request(&req)?;
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        // Availability-probe detection (see `complete_with_options`). A
        // streaming probe that 429/529s fast-fails the chain too.
        let is_probe = is_probe_request(&req, &policy);
        let mut last_err: Option<Error> = None;

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
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
            apply_layered_overlays(&self.config, target, &mut attempt_req);

            let attempt_policy = self.compose_attempt_policy(
                &policy,
                provider_name,
                target.stream_first_byte_timeout_ms,
            );
            // Per-target one-shot auth-recovery: a 401 from the
            // first-chunk attempt triggers on_auth_failure (forced
            // refresh through the OAuth store's per-provider mutex)
            // and exactly one retry. Streams don't have their own
            // retry policy (mid-stream errors propagate), and this
            // recovery only covers the PRE-FIRST-CHUNK window --
            // once `wrap_with_breaker_accounting` is wrapping a live
            // stream, a 401 surfacing as a mid-stream chunk error
            // propagates to the caller without auth-recovery (rare
            // for current upstreams that don't revalidate per-chunk;
            // documented gap if a future provider does). A refresh
            // failure propagates immediately rather than walking the
            // fallback chain over a dead OAuth identity.
            let mut auth_retry_attempted = false;
            let mut attempts_made: u32 = 0;
            let attempt_outcome = loop {
                let r = try_stream_with_first_chunk(
                    provider_name,
                    provider.clone(),
                    attempt_req.clone(),
                    &attempt_policy,
                )
                .await;
                attempts_made += 1;
                if let Err(ref err) = r {
                    if !auth_retry_attempted && matches!(err, Error::Upstream { status: 401, .. }) {
                        auth_retry_attempted = true;
                        tracing::debug!(
                            provider = provider_name,
                            model = %target.nickname.as_deref().unwrap_or(""),
                            attempt = attempts_made,
                            "stream 401 pre-first-chunk; refreshing auth and retrying once",
                        );
                        provider.on_auth_failure().await?;
                        continue;
                    }
                }
                break r;
            };
            match attempt_outcome {
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
                    let do_fallback = should_fallback(&e, &policy, is_probe);
                    // Probe fast-fail: a probe that hit a rate-limit/overload
                    // (429/529) returns the status immediately via an
                    // explicit early return -- no fallback. The span below
                    // is a deliberate no-op for this path: record_failure is
                    // gated on `do_fallback`, which is false for a
                    // fast-failed probe. Returning here is behavior-
                    // preserving today AND robust if the predicate ever
                    // drifts. (Streams never retry the same provider, so
                    // there is no can_retry_here to guard.)
                    let probe_fast_failed = if is_probe {
                        probe_fast_fail_status(&e)
                    } else {
                        None
                    };
                    if let Some(status) = probe_fast_failed {
                        log_probe_fast_fail(
                            provider_name,
                            target.nickname.as_deref().unwrap_or(""),
                            status,
                            req.max_tokens,
                        );
                        return Err(e);
                    }
                    if do_fallback {
                        self.record_failure(state_key);
                    }
                    if opts.disable_fallbacks {
                        return Err(e);
                    }
                    if do_fallback {
                        let has_next = chain_idx + 1 < chain_len;
                        if has_next {
                            tracing::warn!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                error = ?e,
                                "stream fallback to next",
                            );
                        } else {
                            // The previous shape WARNed "fallback to next"
                            // with `provider=<self> model=<self>` because
                            // we always log the SOURCE of the hop, not the
                            // target. On a single-entry chain (or the
                            // final entry of a longer chain) there is no
                            // next target -- the loop exits and the
                            // request fails. Log accordingly so an
                            // operator triaging a misleading "fallback
                            // happened" line sees what actually
                            // happened.
                            tracing::warn!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                error = ?e,
                                "stream chain exhausted; no fallback target available; request will fail",
                            );
                        }
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

/// Compose the layered configuration overlays into the per-attempt
/// request. v0.6.0 introduces three knobs that ride from operator
/// TOML through the dispatch layer onto the egress:
///
///   - `header_extras` (provider + model, with list-valued
///     `anthropic-beta` unioned)
///   - `payload_extras` (provider + model, deep-merged with model
///     winning on leaf collision)
///   - `routectl_internal` (per-model reasoning dialect + history
///     reasoning policy that the openai-compat egress reads)
///
/// All three are no-ops when neither the provider nor the model
/// configured them.
fn apply_layered_overlays(config: &Config, target: &DispatchTarget, req: &mut ChatRequest) {
    let provider_entry = config.providers.get(&target.provider_name);
    let provider_headers = provider_entry.map(|e| e.header_extras());
    let provider_payload = provider_entry.and_then(|e| e.payload_extras());

    merge_header_extras(
        &target.provider_name,
        provider_headers,
        &target.model_header_extras,
        req,
    );
    merge_payload_extras(
        &target.provider_name,
        provider_payload,
        target.model_payload_extras.as_ref(),
        req,
    );

    // Transport-internal carrier: the egress reads dialect +
    // history-reasoning from `req.routectl_internal` so the
    // `Provider` trait surface stays stable. Use struct-update on
    // Default so adding a new field on `RoutectlInternal` later
    // doesn't break this construction site (the type is
    // `#[non_exhaustive]`).
    //
    // Preserve `claude_code_headers` captured by the ingress: those
    // are inbound-request data, not per-model knobs, and the
    // Anthropic-API egress reads them downstream to forward
    // X-Claude-Code-* headers for gateway cost attribution.
    let captured_claude_code_headers =
        std::mem::take(&mut req.routectl_internal.claude_code_headers);
    let mut internal = RoutectlInternal::default();
    internal.reasoning_dialect = target.reasoning_dialect.map(|d| d.into());
    internal.history_reasoning = target.history_reasoning.map(|h| h.into());
    internal.claude_code_headers = captured_claude_code_headers;
    internal.supports_adaptive_thinking = target.supports_adaptive_thinking;
    req.routectl_internal = internal;
}

/// Merge per-provider and per-model `header_extras` into the
/// per-attempt request. Three-source compose:
///
///   1. Clone the provider entry's `header_extras` into a working map.
///   2. Iterate the model's `header_extras`. Auth-reserved keys
///      (`authorization`, `x-api-key`, `anthropic-version`) WARN +
///      drop; managed-reserved keys (`host`, `content-type`,
///      `content-length`) DEBUG + drop. Other keys overwrite the
///      provider's value on collision (model wins).
///   3. For every list-valued header in `LIST_VALUED_HEADERS` (today
///      just `anthropic-beta`), run a comma-split-union-rejoin post-
///      pass over the three sources in visit order: `req.anthropic_beta`
///      (ingress lift) -> provider value -> model value. The unioned
///      string lands back on the merged map AND on `req.anthropic_beta`
///      so downstream readers (e.g. Bedrock's `filter_bedrock_betas`)
///      see the same fully-composed list.
///
/// The merged headers replace the provider's `header_extras` slot on
/// the request's resolved config view via... actually, the providers
/// crate egresses read `self.cfg.header_extras` (snapshot at construct
/// time) NOT a per-request slot. So this merge ALSO writes the
/// composed `anthropic-beta` back into `req.anthropic_beta` so the
/// Anthropic-API egress (which reads canonical for the wire header)
/// and Bedrock's beta filter (same canonical read) both see the
/// unioned set. Other merged headers are emitted via a per-request
/// canonical channel that future egresses can read; today the
/// per-model `header_extras` on non-`anthropic-beta` keys is reserved
/// for forward use -- this helper still composes it for the log and
/// for `anthropic-beta` correctness.
pub fn merge_header_extras(
    provider_name: &str,
    provider_extras: Option<&BTreeMap<String, String>>,
    model_extras: &BTreeMap<String, String>,
    req: &mut ChatRequest,
) {
    // Start with a clone of the provider's headers.
    let mut merged: BTreeMap<String, String> = provider_extras.cloned().unwrap_or_default();

    // Layer the model's headers on top, gating against reserved
    // buckets. Model wins on plain-key collision.
    for (k, v) in model_extras {
        if is_auth_reserved(k) {
            tracing::warn!(
                provider = %provider_name,
                header = %k,
                "ignoring auth-reserved header from [models.X] header_extras",
            );
            continue;
        }
        if is_managed_reserved(k) {
            tracing::debug!(
                provider = %provider_name,
                header = %k,
                "dropping managed-reserved header from [models.X] header_extras",
            );
            continue;
        }
        merged.insert(k.clone(), v.clone());
    }

    // List-valued post-pass. For `anthropic-beta`, comma-split-union-
    // rejoin in visit order: req.anthropic_beta (ingress) -> provider
    // value -> model value. The unioned string lands back on the
    // merged map AND on req.anthropic_beta.
    for list_key in LIST_VALUED_HEADERS {
        let provider_val = provider_extras
            .and_then(|m| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(list_key))
                    .map(|(_, v)| v.as_str())
            })
            .unwrap_or("");
        let model_val = model_extras
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(list_key))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");

        // Visit order: ingress (req.anthropic_beta) -> provider -> model.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut unioned: Vec<String> = Vec::new();
        if list_key.eq_ignore_ascii_case("anthropic-beta") {
            for entry in &req.anthropic_beta {
                let t = entry.trim();
                if !t.is_empty() && seen.insert(t.to_string()) {
                    unioned.push(t.to_string());
                }
            }
        }
        for raw in [provider_val, model_val] {
            for piece in raw.split(',') {
                let t = piece.trim();
                if !t.is_empty() && seen.insert(t.to_string()) {
                    unioned.push(t.to_string());
                }
            }
        }

        if unioned.is_empty() {
            // Nothing to write; remove any inherited blank entry on
            // the merged map to keep the dump clean.
            let keys_to_drop: Vec<String> = merged
                .keys()
                .filter(|k| k.eq_ignore_ascii_case(list_key))
                .cloned()
                .collect();
            for k in keys_to_drop {
                merged.remove(&k);
            }
            continue;
        }

        let joined = unioned.join(",");
        // Drop any case-variant of the key already present, then
        // insert under the canonical lowercase name.
        let keys_to_drop: Vec<String> = merged
            .keys()
            .filter(|k| k.eq_ignore_ascii_case(list_key))
            .cloned()
            .collect();
        for k in keys_to_drop {
            merged.remove(&k);
        }
        merged.insert((*list_key).to_string(), joined);

        if list_key.eq_ignore_ascii_case("anthropic-beta") {
            req.anthropic_beta = unioned;
        }
    }

    // Strip `anthropic-beta` from the merged map before publishing it
    // to the egress -- it rides on `req.anthropic_beta` instead and
    // double-handling would cause the Anthropic-API egress to emit
    // duplicate values. The list-valued post-pass above already
    // wrote the unioned set there.
    let keys_to_strip: Vec<String> = merged
        .keys()
        .filter(|k| k.eq_ignore_ascii_case("anthropic-beta"))
        .cloned()
        .collect();
    for k in keys_to_strip {
        merged.remove(&k);
    }

    if !merged.is_empty() {
        tracing::debug!(
            provider = %provider_name,
            header_keys = ?merged.keys().collect::<Vec<_>>(),
            "composed header_extras (provider + model + list-valued union)",
        );
    }

    // Publish the merged map to the egress via the transport-internal
    // carrier. Egresses read this in `build_headers` and union it with
    // their construction-time `self.cfg.header_extras` snapshot (model
    // wins on key collision). Library consumers that construct a
    // ChatRequest without the router leave this `None`, and the egress
    // falls back to its `self.cfg.header_extras` alone.
    req.routectl_internal.header_extras = Some(merged);
}

/// Merge per-provider and per-model `payload_extras` into the
/// per-attempt request. Deep recursive merge with model winning on
/// leaf collision; the result lands on `req.provider_extras` so each
/// egress's existing `provider_extras` reader picks it up.
///
/// Layer order: `req.provider_extras` (ingress forward-compat sweep,
/// pre-existing on the request) -> provider `payload_extras` ->
/// model `payload_extras`. The provider's payload IS deep-merged
/// over the ingress sweep on key collision, and the model's payload
/// then deep-merges over both. Net precedence: model > provider >
/// ingress sweep on shared leaf keys; ingress-only keys survive
/// untouched because no other source set them.
pub fn merge_payload_extras(
    provider_name: &str,
    provider_extras: Option<&Value>,
    model_extras: Option<&Value>,
    req: &mut ChatRequest,
) {
    if provider_extras.is_none() && model_extras.is_none() {
        return;
    }

    // Start with the request's existing provider_extras (if any),
    // then layer provider, then model.
    let mut accumulated: Value = req
        .provider_extras
        .clone()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    if let Some(p) = provider_extras {
        deep_merge_value(&mut accumulated, p, provider_name, "provider");
    }
    if let Some(m) = model_extras {
        deep_merge_value(&mut accumulated, m, provider_name, "model");
    }

    // If nothing landed (both were empty objects), don't synthesize
    // an empty provider_extras on the request.
    let is_empty_object = accumulated
        .as_object()
        .map(serde_json::Map::is_empty)
        .unwrap_or(false);
    if is_empty_object && req.provider_extras.is_none() {
        return;
    }
    req.provider_extras = Some(accumulated);
}

/// Deep recursive merge of `src` into `dst`. Same-key object values
/// merge recursively; scalar / array collisions take the `src` value
/// with a DEBUG log naming the key (so an operator who shadowed a
/// provider scalar with a model value can correlate at triage).
fn deep_merge_value(dst: &mut Value, src: &Value, provider_name: &str, src_layer: &str) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        deep_merge_value(existing, v, provider_name, src_layer);
                    }
                    Some(_) => {
                        tracing::debug!(
                            provider = %provider_name,
                            layer = %src_layer,
                            key = %k,
                            "payload_extras: leaf collision; {src_layer} wins",
                        );
                        d.insert(k.clone(), v.clone());
                    }
                    None => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (dst, src) => {
            *dst = src.clone();
        }
    }
}

fn is_auth_reserved(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    AUTH_HEADERS.contains(&lc.as_str())
}

fn is_managed_reserved(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    MANAGED_HEADERS.contains(&lc.as_str())
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
        supports_adaptive_thinking: m.supports_adaptive_thinking,
        nickname: Some(m.nickname.clone()),
        model_header_extras: m.header_extras.clone(),
        model_payload_extras: m.payload_extras.clone(),
        reasoning_dialect: m.reasoning_dialect,
        history_reasoning: m.history_reasoning,
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
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

/// True when `req` is an availability/quota probe: its `max_tokens`
/// is set and at or below the configured `probe_max_tokens` threshold.
/// Claude Code sends `max_tokens=1` probes to `/v1/messages` whose tiny
/// output is never read; on a rate-limit/overload the router fast-fails
/// them instead of walking the fallback chain (see `should_fallback`).
/// `probe_max_tokens = 0` disables detection (no request is a probe);
/// a request with no `max_tokens` is never a probe.
fn is_probe_request(req: &ChatRequest, policy: &RetryPolicy) -> bool {
    policy.probe_max_tokens > 0 && req.max_tokens.is_some_and(|m| m <= policy.probe_max_tokens)
}

/// The upstream status of a probe-fast-fail-eligible error, or `None`
/// when `err` is not one. 429 is a rate-limit; 529 is Anthropic's
/// overload status. Both surface as `Error::Upstream { status, .. }`
/// (the anthropic-api egress forwards the raw upstream status). A probe
/// that hits one of these skips retry+fallback: on the all-Anthropic
/// chain every hop shares the same limit, so walking it is futile and
/// the probe's output is unread. A generic 5xx or a capability 4xx
/// (e.g. Bedrock's `max_tokens=1` 400) returns `None` here so it keeps
/// walking the chain -- a healthy sibling provider can still answer.
fn probe_fast_fail_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream {
            status: s @ (429 | 529),
            ..
        } => Some(*s),
        _ => None,
    }
}

/// DEBUG-log a probe fast-fail decision, identically from both dispatch
/// loops. Log-only by design: the caller owns the `return Err(..)` that
/// actually short-circuits (a free fn cannot early-return its caller).
/// `max_tokens` is the request value that tripped probe classification,
/// surfaced so an operator can see which value matched the threshold.
fn log_probe_fast_fail(provider: &str, model: &str, status: u16, max_tokens: Option<u32>) {
    tracing::debug!(
        provider,
        model,
        status,
        max_tokens = ?max_tokens,
        "probe request (max_tokens<=probe_max_tokens): not retrying/falling back on rate-limit",
    );
}

fn should_fallback(err: &Error, policy: &RetryPolicy, is_probe: bool) -> bool {
    // Availability-probe fast-fail: a probe (max_tokens <=
    // probe_max_tokens) that hits a rate-limit (429) or overload (529)
    // does not fall back. Every OTHER error class -- generic 5xx,
    // network/status-0, Streaming, and every 4xx including the
    // Bedrock-style max_tokens=1 400 -- falls through to the normal
    // predicate below, so real fallback is untouched.
    if is_probe && probe_fast_fail_status(err).is_some() {
        return false;
    }
    match err {
        // status 0 means we never reached the upstream HTTP layer
        // (DNS, TCP connect, TLS handshake, request body, timeout). Always
        // fallbackable -- nothing upstream-specific has happened yet.
        Error::Upstream { status: 0, .. } => true,
        Error::Upstream { status, .. } => policy.is_fallbackable_status(*status),
        Error::Streaming(_) => true,
        Error::UnknownProvider(_) => true,
        _ => false,
    }
}

fn should_retry_same_provider(
    err: &Error,
    policy: &RetryPolicy,
    attempts_made: u32,
    is_probe: bool,
) -> bool {
    // Probe fast-fail mirrors `should_fallback`: a probe must not burn
    // retry attempts against a rate-limited/overloaded provider (429 /
    // 529). All other error classes fall through to the cap below.
    if is_probe && probe_fast_fail_status(err).is_some() {
        return false;
    }
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

    #[test]
    fn should_fallback_status_zero_is_always_true() {
        // status 0 == network error (DNS, TCP, TLS, request body,
        // request timeout). `should_fallback` returns true unconditionally
        // regardless of how the operator set `retry_allowlist` or
        // `retry_denylist`; the predicate only governs HTTP-status
        // outcomes (>= 400). This pins the always-true contract so a
        // future refactor of `is_fallbackable_status` can't accidentally
        // break network-error fallback.
        let err = Error::upstream("p", 0, "tcp connect refused");

        // (1) Default policy (allowlist populated, denylist None).
        let policy_default = RetryPolicy::default();
        assert!(should_fallback(&err, &policy_default, false));

        // (2) Empty allowlist (would otherwise mean "no HTTP fallback").
        let policy_empty_allow = RetryPolicy {
            retry_allowlist: vec![],
            retry_denylist: None,
            ..RetryPolicy::default()
        };
        assert!(should_fallback(&err, &policy_empty_allow, false));

        // (3) Denylist set (governs HTTP statuses, not status 0).
        let policy_deny = RetryPolicy {
            retry_allowlist: vec![],
            retry_denylist: Some(vec![501]),
            ..RetryPolicy::default()
        };
        assert!(should_fallback(&err, &policy_deny, false));
    }
}

#[cfg(test)]
mod probe_fast_fail_tests {
    //! Availability-probe fast-fail. Claude Code sends `max_tokens=1`
    //! quota/health probes to `/v1/messages`. On a rate-limit (429) or
    //! overload (529) these skip retry+fallback -- walking the
    //! all-Anthropic chain is futile (every hop shares the limit) and
    //! the 1-token output is unread. Every OTHER error class is
    //! unaffected, so real requests and 4xx-capability fallback keep
    //! today's behavior. Each test names the (is_probe, status) shape
    //! it pins.
    use super::*;

    fn upstream(status: u16) -> Error {
        Error::upstream("probe-test-provider", status, "x")
    }

    fn req_with_max_tokens(max_tokens: Option<u32>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![],
            max_tokens,
            ..Default::default()
        }
    }

    #[test]
    fn probe_429_does_not_fall_back() {
        // Arrange
        let err = upstream(429);
        let policy = RetryPolicy::default();
        // Act
        let fall_back = should_fallback(&err, &policy, true);
        // Assert
        assert!(
            !fall_back,
            "a max_tokens<=probe_max_tokens probe must not walk the chain on 429",
        );
    }

    #[test]
    fn probe_429_does_not_retry_same_provider() {
        // Arrange: attempts_made=0 so the ONLY reason not to retry is
        // the probe short-circuit (the cap would otherwise allow it).
        let err = upstream(429);
        let policy = RetryPolicy::default();
        // Act
        let retry = should_retry_same_provider(&err, &policy, 0, true);
        // Assert
        assert!(
            !retry,
            "a probe must not burn retry attempts against a rate-limited provider",
        );
    }

    #[test]
    fn probe_529_does_not_fall_back() {
        // 529 is Anthropic's overload status; on an all-Anthropic chain
        // every hop shares it, so a probe fast-fails it like a 429.
        let err = upstream(529);
        let policy = RetryPolicy::default();
        assert!(!should_fallback(&err, &policy, true));
    }

    #[test]
    fn probe_529_does_not_retry_same_provider() {
        // Symmetry with the 429 retry short-circuit, for the 529 branch.
        let err = upstream(529);
        let policy = RetryPolicy::default();
        assert!(!should_retry_same_provider(&err, &policy, 0, true));
    }

    #[test]
    fn probe_400_still_falls_back() {
        // Bedrock rejects max_tokens=1 with a 400; a sibling provider
        // may accept it, so a probe must still walk the chain on 4xx.
        let err = upstream(400);
        let policy = RetryPolicy::default();
        assert!(should_fallback(&err, &policy, true));
    }

    #[test]
    fn probe_503_still_falls_back() {
        // 503 is generic unavailability (not the chain-wide 429/529); a
        // sibling provider may be healthy, so the probe still falls back.
        let err = upstream(503);
        let policy = RetryPolicy::default();
        assert!(should_fallback(&err, &policy, true));
    }

    #[test]
    fn real_request_429_still_retries_and_falls_back() {
        // is_probe=false (a real request): a 429 keeps today's behavior
        // -- fallbackable AND retryable up to the policy cap.
        let err = upstream(429);
        let policy = RetryPolicy::default();
        assert!(
            should_fallback(&err, &policy, false),
            "real-request 429 still falls back",
        );
        assert!(
            should_retry_same_provider(&err, &policy, 0, false),
            "real-request 429 still retries (attempts_made=0 < cap)",
        );
    }

    #[test]
    fn is_probe_request_predicate_boundary() {
        // Default threshold is 1.
        let policy = RetryPolicy::default();
        assert_eq!(policy.probe_max_tokens, 1, "default probe_max_tokens is 1");

        assert!(
            is_probe_request(&req_with_max_tokens(Some(1)), &policy),
            "max_tokens=1 at threshold 1 IS a probe",
        );
        assert!(
            !is_probe_request(&req_with_max_tokens(Some(2)), &policy),
            "max_tokens=2 above threshold 1 is NOT a probe",
        );
        assert!(
            !is_probe_request(&req_with_max_tokens(None), &policy),
            "max_tokens=None is NEVER a probe",
        );
    }

    #[test]
    fn probe_disabled_when_threshold_zero() {
        // probe_max_tokens=0 disables probe detection entirely: a
        // max_tokens=1 request is NOT a probe, so a 429 behaves like a
        // real request (falls back + retries) -- today's behavior.
        let policy = RetryPolicy {
            probe_max_tokens: 0,
            ..RetryPolicy::default()
        };
        let req = req_with_max_tokens(Some(1));
        assert!(
            !is_probe_request(&req, &policy),
            "threshold 0 disables probe detection",
        );

        let is_probe = is_probe_request(&req, &policy); // false
        let err = upstream(429);
        assert!(should_fallback(&err, &policy, is_probe));
        assert!(should_retry_same_provider(&err, &policy, 0, is_probe));
    }

    #[test]
    fn custom_probe_max_tokens_threshold_is_inclusive() {
        // A non-default threshold (probe_max_tokens=2) pins the `<=`
        // boundary the default-1 tests cannot distinguish from `<`:
        // max_tokens=2 IS a probe (at the threshold), max_tokens=3 is
        // NOT (above it). A `<` regression would misclassify the
        // at-threshold value as a real request.
        let policy = RetryPolicy {
            probe_max_tokens: 2,
            ..RetryPolicy::default()
        };
        assert!(
            is_probe_request(&req_with_max_tokens(Some(2)), &policy),
            "max_tokens=2 is AT the custom probe_max_tokens=2 threshold (inclusive)",
        );
        assert!(
            !is_probe_request(&req_with_max_tokens(Some(3)), &policy),
            "max_tokens=3 is ABOVE the custom threshold; a real request",
        );
        // Downstream: an at-threshold probe still fast-fails a 429.
        let is_probe = is_probe_request(&req_with_max_tokens(Some(2)), &policy);
        assert!(
            !should_fallback(&upstream(429), &policy, is_probe),
            "an at-threshold probe must not fall back on 429 at a custom threshold",
        );
    }
}

#[cfg(test)]
mod merge_header_extras_tests {
    //! Unit tests for the v0.6.0 `merge_header_extras` helper.
    use super::*;

    fn req_with_betas(betas: Vec<&str>) -> ChatRequest {
        ChatRequest {
            model: "any".into(),
            messages: vec![],
            anthropic_beta: betas.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_both_is_noop() {
        let mut req = req_with_betas(vec![]);
        merge_header_extras("p", None, &BTreeMap::new(), &mut req);
        assert!(req.anthropic_beta.is_empty());
    }

    #[test]
    fn anthropic_beta_unions_three_sources_in_visit_order() {
        let mut req = req_with_betas(vec!["foo"]);
        let provider = map(&[("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")]);
        let model = map(&[("anthropic-beta", "context-1m-2025-08-07")]);
        merge_header_extras("p", Some(&provider), &model, &mut req);
        assert_eq!(
            req.anthropic_beta,
            vec![
                "foo".to_string(),
                "claude-code-20250219".to_string(),
                "oauth-2025-04-20".to_string(),
                "context-1m-2025-08-07".to_string(),
            ]
        );
    }

    #[test]
    fn anthropic_beta_dedups_across_sources() {
        let mut req = req_with_betas(vec!["foo", "bar"]);
        let provider = map(&[("anthropic-beta", "foo,baz")]);
        let model = map(&[("anthropic-beta", "bar,qux")]);
        merge_header_extras("p", Some(&provider), &model, &mut req);
        assert_eq!(
            req.anthropic_beta,
            vec![
                "foo".to_string(),
                "bar".to_string(),
                "baz".to_string(),
                "qux".to_string()
            ]
        );
    }

    #[test]
    fn model_only_anthropic_beta_lifts_onto_req() {
        let mut req = req_with_betas(vec![]);
        let model = map(&[("anthropic-beta", "context-1m-2025-08-07")]);
        merge_header_extras("p", None, &model, &mut req);
        assert_eq!(
            req.anthropic_beta,
            vec!["context-1m-2025-08-07".to_string()]
        );
    }

    #[test]
    fn auth_reserved_keys_drop_but_other_keys_survive() {
        // Pairing the reserved key with a non-reserved key gives a real
        // observable: the reserved key MUST NOT land on the merged map
        // published via `req.routectl_internal.header_extras`, while
        // the non-reserved sibling MUST land.
        let mut req = req_with_betas(vec![]);
        let model = map(&[("authorization", "Bearer evil"), ("x-app", "ok")]);
        merge_header_extras("p", None, &model, &mut req);
        let published = req
            .routectl_internal
            .header_extras
            .expect("merger publishes a map");
        assert!(
            !published.contains_key("authorization"),
            "auth-reserved key must drop, not propagate",
        );
        assert_eq!(
            published.get("x-app").map(String::as_str),
            Some("ok"),
            "non-reserved sibling on the same model entry must reach the published map",
        );
    }

    #[test]
    fn managed_reserved_keys_drop_but_other_keys_survive() {
        let mut req = req_with_betas(vec![]);
        let model = map(&[
            ("host", "evil.example.com"),
            ("content-type", "text/plain"),
            ("x-app", "ok"),
        ]);
        merge_header_extras("p", None, &model, &mut req);
        let published = req
            .routectl_internal
            .header_extras
            .expect("merger publishes a map");
        assert!(!published.contains_key("host"));
        assert!(!published.contains_key("content-type"));
        assert_eq!(published.get("x-app").map(String::as_str), Some("ok"));
    }

    #[test]
    fn non_list_header_model_wins_on_key_collision() {
        // Pin the model > provider precedence for plain key-value
        // headers. Without this contract, per-model header_extras
        // would only matter for `anthropic-beta`.
        let mut req = req_with_betas(vec![]);
        let provider = map(&[("x-foo", "provider-value")]);
        let model = map(&[("x-foo", "model-value")]);
        merge_header_extras("p", Some(&provider), &model, &mut req);
        let published = req
            .routectl_internal
            .header_extras
            .expect("merger publishes a map");
        assert_eq!(
            published.get("x-foo").map(String::as_str),
            Some("model-value"),
            "model header_extras must win on key collision (last-writer-wins)",
        );
    }

    #[test]
    fn non_list_provider_only_header_propagates_to_published_map() {
        // Pure provider header (no per-model override) must still
        // reach the egress via the published map.
        let mut req = req_with_betas(vec![]);
        let provider = map(&[("x-stainless-arch", "x64")]);
        merge_header_extras("p", Some(&provider), &BTreeMap::new(), &mut req);
        let published = req
            .routectl_internal
            .header_extras
            .expect("merger publishes a map");
        assert_eq!(
            published.get("x-stainless-arch").map(String::as_str),
            Some("x64"),
        );
    }

    #[test]
    fn anthropic_beta_stripped_from_published_map() {
        // After the list-valued union writes to `req.anthropic_beta`,
        // the merger MUST remove `anthropic-beta` from the published
        // header_extras map. Leaving it would cause the Anthropic-API
        // egress (which also unions with req.anthropic_beta) to emit
        // duplicate values on the wire.
        let mut req = req_with_betas(vec![]);
        let provider = map(&[("anthropic-beta", "claude-code-20250219")]);
        merge_header_extras("p", Some(&provider), &BTreeMap::new(), &mut req);
        let published = req
            .routectl_internal
            .header_extras
            .expect("merger publishes a map");
        assert!(
            !published.contains_key("anthropic-beta"),
            "anthropic-beta must be stripped from the published map (it rides on req.anthropic_beta instead)",
        );
        assert_eq!(req.anthropic_beta, vec!["claude-code-20250219".to_string()]);
    }

    #[test]
    fn router_side_auth_and_managed_constants_are_disjoint() {
        // The router defines its own `AUTH_HEADERS` / `MANAGED_HEADERS`
        // constants for the merge_header_extras dispatch. http_client
        // has its own copies (the egress-side gate). Both copies must
        // be disjoint independently.
        for h in AUTH_HEADERS {
            assert!(
                !MANAGED_HEADERS.contains(h),
                "router-side: {h:?} appears in both AUTH and MANAGED",
            );
        }
        for h in MANAGED_HEADERS {
            assert!(
                !AUTH_HEADERS.contains(h),
                "router-side: {h:?} appears in both MANAGED and AUTH",
            );
        }
    }
}

#[cfg(test)]
mod merge_payload_extras_tests {
    use super::*;
    use serde_json::json;

    fn req() -> ChatRequest {
        ChatRequest {
            model: "any".into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_both_is_noop() {
        let mut r = req();
        merge_payload_extras("p", None, None, &mut r);
        assert!(r.provider_extras.is_none());
    }

    #[test]
    fn provider_only_lands_on_req() {
        let mut r = req();
        let p = json!({"top_k": 5, "metadata": {"x": 1}});
        merge_payload_extras("p", Some(&p), None, &mut r);
        let v = r.provider_extras.expect("set");
        assert_eq!(v["top_k"], json!(5));
        assert_eq!(v["metadata"]["x"], json!(1));
    }

    #[test]
    fn deep_merge_objects_recursively() {
        let mut r = req();
        let p = json!({"a": {"shared": 1, "p_only": "p"}});
        let m = json!({"a": {"shared": 2, "m_only": "m"}});
        merge_payload_extras("p", Some(&p), Some(&m), &mut r);
        let v = r.provider_extras.expect("set");
        // Nested objects merge recursively.
        assert_eq!(v["a"]["shared"], json!(2), "model wins on leaf collision");
        assert_eq!(v["a"]["p_only"], json!("p"));
        assert_eq!(v["a"]["m_only"], json!("m"));
    }

    #[test]
    fn scalar_collision_model_wins() {
        let mut r = req();
        let p = json!({"k": "provider"});
        let m = json!({"k": "model"});
        merge_payload_extras("p", Some(&p), Some(&m), &mut r);
        let v = r.provider_extras.expect("set");
        assert_eq!(v["k"], json!("model"));
    }

    #[test]
    fn array_collision_model_wins() {
        let mut r = req();
        let p = json!({"k": [1, 2]});
        let m = json!({"k": [3]});
        merge_payload_extras("p", Some(&p), Some(&m), &mut r);
        let v = r.provider_extras.expect("set");
        assert_eq!(v["k"], json!([3]));
    }

    #[test]
    fn ingress_sweep_preserved_underneath_provider_and_model() {
        // ingress's forward-compat sweep populates req.provider_extras;
        // the merge layers provider + model on top.
        let mut r = req();
        r.provider_extras = Some(json!({"mcp_servers": ["s1"], "k": "ingress"}));
        let p = json!({"k": "provider"});
        let m = json!({"other": true});
        merge_payload_extras("p", Some(&p), Some(&m), &mut r);
        let v = r.provider_extras.expect("set");
        assert_eq!(v["mcp_servers"], json!(["s1"]));
        // Provider overrode the ingress sweep value on `k`.
        assert_eq!(v["k"], json!("provider"));
        assert_eq!(v["other"], json!(true));
    }
}

#[cfg(test)]
mod three_source_anthropic_beta_lift_tests {
    //! Integration: pin that ingress + provider + model anthropic-beta
    //! all union onto the upstream request on both dispatch paths.
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
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
            let s = futures::stream::once(async move {
                Ok(ChatChunk {
                    id: "c0".into(),
                    model: "x".into(),
                    choices: vec![],
                    usage: None,
                    opaque_events: Vec::new(),
                })
            });
            Ok(s.boxed())
        }
    }

    fn router_with_capture(
        provider_betas: Option<&str>,
        model_betas: Option<&str>,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let mut config = Config::default();
        // Provider-side `header_extras`.
        let mut provider_headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(v) = provider_betas {
            provider_headers.insert("anthropic-beta".into(), v.to_string());
        }
        config.providers.insert(
            "anthropic".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: provider_headers,
                payload_extras: None,
                user_agent: None,
                runtime: ProviderRuntimePolicy::default(),
            },
        );

        let mut router = Router::new(Arc::new(config));
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let mut resolved = ResolvedModel::new("haiku", "anthropic", provider, "claude-haiku-4-5");
        if let Some(v) = model_betas {
            let mut h = BTreeMap::new();
            h.insert("anthropic-beta".into(), v.to_string());
            resolved = resolved.with_header_extras(h);
        }
        models.insert("haiku".into(), Arc::new(resolved));
        router.install_resolved_models(models);
        (router, captured)
    }

    #[tokio::test]
    async fn complete_path_unions_three_sources() {
        // ingress: "foo", provider: "claude-code-20250219,oauth-2025-04-20",
        // model: "context-1m-2025-08-07" -- all unioned.
        let (router, captured) = router_with_capture(
            Some("claude-code-20250219,oauth-2025-04-20"),
            Some("context-1m-2025-08-07"),
        );
        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            anthropic_beta: vec!["foo".into()],
            ..Default::default()
        };
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let upstream = captured.first().expect("one upstream call");
        assert_eq!(
            upstream.anthropic_beta,
            vec![
                "foo".to_string(),
                "claude-code-20250219".to_string(),
                "oauth-2025-04-20".to_string(),
                "context-1m-2025-08-07".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn stream_path_unions_three_sources() {
        let (router, captured) =
            router_with_capture(Some("oauth-2025-04-20"), Some("context-1m-2025-08-07"));
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
            vec![
                "oauth-2025-04-20".to_string(),
                "context-1m-2025-08-07".to_string(),
            ]
        );
    }
}

#[cfg(test)]
mod reasoning_passthrough_tests {
    //! Regression: `req.reasoning` passes through dispatch unchanged
    //! when no operator overlay applies. The merge step is gone; the
    //! caller's reasoning config must arrive at the egress unmodified.
    use super::*;
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, Message, Provider, ReasoningConfig,
    };
    use std::sync::{Arc, Mutex};

    struct CapturingProvider {
        captured: Arc<Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            "capturing"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("capturing", "unused"))
        }
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
            Ok(None)
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.captured.lock().unwrap().push(req);
            Ok(ChatResponse {
                id: "ok".into(),
                model: "m".into(),
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

    fn router_with_capturing(provider: Arc<dyn Provider>) -> Router {
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m".to_string(),
            Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
        );
        router.install_resolved_models(models);
        router
    }

    #[tokio::test]
    async fn caller_reasoning_passes_through_dispatch_unchanged() {
        // When the caller supplies a ReasoningConfig and no operator
        // merge step applies, the egress must see the caller's
        // reasoning field verbatim. The merge step is gone; nothing
        // in the dispatch path should modify req.reasoning.
        let captured: Arc<Mutex<Vec<ChatRequest>>> = Arc::new(Mutex::new(vec![]));
        let provider = Arc::new(CapturingProvider {
            captured: captured.clone(),
        });
        let router = router_with_capturing(provider);

        let caller_reasoning = ReasoningConfig {
            effort: Some("medium".into()),
            enabled: Some(true),
            max_tokens: Some(4096),
            exclude: Some(false),
        };
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![],
            reasoning: Some(caller_reasoning.clone()),
            ..Default::default()
        };
        router.complete(req).await.expect("dispatch succeeded");

        let calls = captured.lock().unwrap();
        let upstream = calls.first().expect("one upstream call");
        let got = upstream.reasoning.as_ref().expect("reasoning preserved");
        assert_eq!(got.effort, caller_reasoning.effort, "effort unchanged");
        assert_eq!(got.enabled, caller_reasoning.enabled, "enabled unchanged");
        assert_eq!(
            got.max_tokens, caller_reasoning.max_tokens,
            "max_tokens unchanged"
        );
        assert_eq!(got.exclude, caller_reasoning.exclude, "exclude unchanged");
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
                header_extras: BTreeMap::new(),
                payload_extras: None,
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

    // ----- Recursive alias-chain resolution (Task #5) -----
    //
    // Pin the runtime DFS expansion: an alias entry that is itself an
    // alias key gets recursively expanded inline so the operator's
    // stated fallback order is preserved. Globs follow the same rule
    // as exact matches. The depth cap is exercised via a forced cycle
    // (whose static walk would normally have rejected it) to confirm
    // the belt-and-suspenders runtime guard fires.

    fn make_provider(id: &str) -> Arc<dyn Provider> {
        Arc::new(CountedProvider {
            id: id.to_string(),
            calls: AtomicUsize::new(0),
        })
    }

    /// Build a `Router` whose alias map references both alias keys
    /// and model nicknames (so the recursive resolver has something
    /// to walk). `aliases` is a slice of `(key, AliasValue)` pairs;
    /// `models` is a slice of `(nickname, provider_name, upstream)`
    /// tuples. Every provider name in `models` gets a fresh
    /// `CountedProvider` instance, so the test can assert which model
    /// landed in the dispatch chain by reading
    /// `resp.routectl_provider`.
    fn router_with_recursive_aliases(
        aliases: &[(&str, AliasValue)],
        models: &[(&str, &str, &str)],
    ) -> Router {
        let mut config = Config::default();
        for (key, value) in aliases {
            config.aliases.insert((*key).into(), value.clone());
        }
        let mut router = Router::new(Arc::new(config));
        let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        for (nickname, provider_name, upstream) in models {
            let provider = make_provider(provider_name);
            resolved.insert(
                (*nickname).into(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    *provider_name,
                    provider,
                    *upstream,
                )),
            );
        }
        router.install_resolved_models(resolved);
        router
    }

    #[tokio::test]
    async fn alias_pointing_to_another_alias_resolves_two_deep() {
        // A = ["B"], B = ["model-x"]. Wire model "a" must dispatch
        // to model-x's provider (one hop through B).
        let router = router_with_recursive_aliases(
            &[
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("model-x".into())),
            ],
            &[("model-x", "p-x", "u-x")],
        );
        let req = ChatRequest {
            model: "a".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("p-x"));
        assert_eq!(resp.model, "u-x");
    }

    #[tokio::test]
    async fn alias_three_deep_resolves_to_full_chain() {
        // A = ["B"], B = ["C"], C = ["model-x", "model-y"]. Wire
        // model "a" should dispatch to model-x first; if model-x
        // were absent, would fall back to model-y. We just confirm
        // the head of the resolved chain.
        let router = router_with_recursive_aliases(
            &[
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("c".into())),
                (
                    "c",
                    AliasValue::Chain(vec!["model-x".into(), "model-y".into()]),
                ),
            ],
            &[("model-x", "p-x", "u-x"), ("model-y", "p-y", "u-y")],
        );
        let req = ChatRequest {
            model: "a".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("p-x"));
    }

    #[test]
    fn alias_chain_preserves_fallback_order_across_recursion() {
        // A = ["B", "model-c"], B = ["model-d", "model-e"]. Static
        // expansion must yield [model-d, model-e, model-c] -- B's
        // chain expanded inline before C, preserving the operator's
        // stated fallback order. We test via dispatch_chain directly
        // to inspect ordering without bringing up the full async
        // dispatch loop.
        let router = router_with_recursive_aliases(
            &[
                ("a", AliasValue::Chain(vec!["b".into(), "model-c".into()])),
                (
                    "b",
                    AliasValue::Chain(vec!["model-d".into(), "model-e".into()]),
                ),
            ],
            &[
                ("model-c", "p-c", "u-c"),
                ("model-d", "p-d", "u-d"),
                ("model-e", "p-e", "u-e"),
            ],
        );
        let chain = router.dispatch_chain("a").expect("dispatch_chain ok");
        let upstreams: Vec<&str> = chain.iter().map(|t| t.upstream.as_str()).collect();
        assert_eq!(
            upstreams,
            vec!["u-d", "u-e", "u-c"],
            "B's chain must expand inline before C, preserving fallback order"
        );
    }

    #[test]
    fn dry_single_pointer_alias_resolves_to_underlying_model() {
        // The DRY operator-config pattern from the spec:
        // `a = ["model-x"]`, `claude-a = ["a"]`. Both wire models
        // must dispatch to model-x. This is the shape that lets the
        // operator collapse the inline-duplicated `claude-cheap`,
        // `claude-codex-pro`, etc. wrappers in the user config.
        let router = router_with_recursive_aliases(
            &[
                ("a", AliasValue::Single("model-x".into())),
                ("claude-a", AliasValue::Single("a".into())),
            ],
            &[("model-x", "p-x", "u-x")],
        );

        let chain_a = router.dispatch_chain("a").expect("a resolves");
        assert_eq!(chain_a.len(), 1);
        assert_eq!(chain_a[0].upstream, "u-x");

        let chain_claude = router
            .dispatch_chain("claude-a")
            .expect("claude-a resolves");
        assert_eq!(chain_claude.len(), 1);
        assert_eq!(chain_claude[0].upstream, "u-x");
    }

    #[test]
    fn glob_alias_expands_through_nested_alias() {
        // Per architect's verdict F: glob keys follow the same
        // recursion rule as exact aliases. `claude-haiku*` -> `a` ->
        // `model-x`. A wire model "claude-haiku-3" hits the glob and
        // must resolve through `a` to model-x's provider.
        let router = router_with_recursive_aliases(
            &[
                ("claude-haiku*", AliasValue::Single("a".into())),
                ("a", AliasValue::Single("model-x".into())),
            ],
            &[("model-x", "p-x", "u-x")],
        );
        let chain = router
            .dispatch_chain("claude-haiku-3")
            .expect("glob match resolves");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].upstream, "u-x");
    }

    #[test]
    fn recursion_depth_cap_fires_on_cycle_at_dispatch_time() {
        // Belt-and-suspenders: if the static walk somehow missed a
        // cycle (e.g. operator hot-edited the live Config without
        // re-running validation), the runtime resolver must fail
        // fast with `Error::Config` rather than recurse forever.
        // We force the case here by building a router with a
        // self-cycle directly (skipping `validate_alias_chain_targets`).
        let router = router_with_recursive_aliases(&[("a", AliasValue::Single("a".into()))], &[]);
        let res = router.dispatch_chain("a");
        match res {
            Err(Error::Config(msg)) => {
                assert!(
                    msg.contains("recursion exceeded depth"),
                    "expected depth-cap error, got: {msg}"
                );
            }
            Err(other) => panic!("expected Error::Config from depth cap, got {other:?}"),
            Ok(_) => panic!("expected Error::Config from depth cap, got Ok(...)"),
        }
    }
}

#[cfg(test)]
mod count_tokens_tests {
    //! Pin: `Router::count_tokens` does NOT walk the fallback chain
    //! and propagates `Error::NotImplemented` from the provider as-is
    //! (no retries). Tokenizer correctness rules out walking the
    //! chain -- a count from the wrong tokenizer would silently
    //! miscount the caller's budget.
    use super::*;
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tracks how many times `count_tokens` was called so the test
    /// can assert there's no retry on `NotImplemented`.
    struct NotImplProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for NotImplProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            unreachable!()
        }
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
            Ok(None)
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::NotImplemented(
                self.id.clone(),
                "count_tokens".into(),
            ))
        }
    }

    #[tokio::test]
    async fn not_implemented_propagates_without_retry() {
        // Arrange
        let calls = Arc::new(AtomicUsize::new(0));
        let p: Arc<dyn Provider> = Arc::new(NotImplProvider {
            id: "no-count".into(),
            calls: calls.clone(),
        });
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "haiku".to_string(),
            Arc::new(ResolvedModel::new(
                "haiku",
                "no-count",
                p.clone(),
                "claude-haiku-4-5",
            )),
        );
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "haiku".into(),
            ..Default::default()
        };

        // Act
        let err = router.count_tokens(req).await.unwrap_err();

        // Assert: no retry (single call), NotImplemented surfaces
        // verbatim.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "count_tokens must NOT retry on NotImplemented"
        );
        assert!(
            matches!(err, Error::NotImplemented(_, _)),
            "expected Error::NotImplemented, got {err:?}"
        );
    }
}

#[cfg(test)]
mod feature_filter_tests {
    //! Tests for the v0.6.0 per-provider `unsupported_features`
    //! pre-filter. Confirms that providers listing a request feature
    //! get skipped BEFORE dispatch (no upstream call, no breaker
    //! account) and that a chain reduced to empty surfaces as
    //! `Error::NotImplemented` rather than walking and 400ing.
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use parking_lot::Mutex as ParkingMutex;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, CustomTool, Error, Message, Provider, ToolDef,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    /// Provider stub that records every `complete()` call. The test
    /// asserts on `captured.len()` to prove a provider was (or was not)
    /// dispatched to.
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
            let id = self.id.clone();
            self.captured.lock().push(req);
            Ok(ChatResponse {
                id: format!("ok-{id}"),
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
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    fn web_search_tool() -> ToolDef {
        ToolDef::Other(json!({
            "type": "web_search_20250305",
            "name": "search"
        }))
    }

    fn web_search_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![],
            tools: Some(vec![web_search_tool()]),
            ..Default::default()
        }
    }

    /// Per-provider captured-request log for test introspection.
    type CapturedRequests = Arc<ParkingMutex<Vec<ChatRequest>>>;

    /// Build a router with a 2-entry alias chain `["bedrock-opus" ->
    /// "anthropic-opus"]`. Each provider entry carries the
    /// `unsupported_features` list passed by the caller.
    fn build_router_with_chain(
        unsupported_first: Vec<String>,
        unsupported_second: Vec<String>,
    ) -> (Router, CapturedRequests, CapturedRequests) {
        let mut config = Config::default();
        config.providers.insert(
            "bedrock-prov".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                runtime: ProviderRuntimePolicy {
                    unsupported_features: unsupported_first,
                    ..Default::default()
                },
            },
        );
        config.providers.insert(
            "anthropic-prov".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                runtime: ProviderRuntimePolicy {
                    unsupported_features: unsupported_second,
                    ..Default::default()
                },
            },
        );
        config.aliases.insert(
            "alias".into(),
            AliasValue::Chain(vec!["bedrock-opus".into(), "anthropic-opus".into()]),
        );

        let mut router = Router::new(Arc::new(config));
        let captured_first: Arc<ParkingMutex<Vec<ChatRequest>>> =
            Arc::new(ParkingMutex::new(Vec::new()));
        let captured_second: Arc<ParkingMutex<Vec<ChatRequest>>> =
            Arc::new(ParkingMutex::new(Vec::new()));
        let p_first: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "bedrock-prov".into(),
            captured: captured_first.clone(),
        });
        let p_second: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "anthropic-prov".into(),
            captured: captured_second.clone(),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "bedrock-opus".into(),
            Arc::new(ResolvedModel::new(
                "bedrock-opus",
                "bedrock-prov",
                p_first,
                "opus-via-bedrock",
            )),
        );
        models.insert(
            "anthropic-opus".into(),
            Arc::new(ResolvedModel::new(
                "anthropic-opus",
                "anthropic-prov",
                p_second,
                "opus-via-anthropic",
            )),
        );
        router.install_resolved_models(models);
        (router, captured_first, captured_second)
    }

    #[tokio::test]
    async fn web_search_skips_first_provider_when_listed_unsupported() {
        // Chain [bedrock, anthropic]. Bedrock declares web_search
        // unsupported. Request carries web_search_20250305. Dispatch
        // must go DIRECTLY to anthropic (no bedrock attempt, no
        // breaker accounting on bedrock).
        let (router, captured_bedrock, captured_anthropic) =
            build_router_with_chain(vec!["web_search".into()], vec![]);
        let req = web_search_request("alias");
        let resp = router.complete(req).await.expect("dispatch must succeed");
        assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
        assert_eq!(
            captured_bedrock.lock().len(),
            0,
            "bedrock must be skipped, not tried-and-fallback",
        );
        assert_eq!(captured_anthropic.lock().len(), 1);
    }

    #[tokio::test]
    async fn empty_chain_after_filter_returns_not_implemented() {
        // Both chain entries declare the feature unsupported. The
        // filter eliminates everyone, so the router synthesizes a
        // 501 NotImplemented naming the feature key. No upstream
        // attempt happens.
        let (router, captured_bedrock, captured_anthropic) =
            build_router_with_chain(vec!["web_search".into()], vec!["web_search".into()]);
        let req = web_search_request("alias");
        let err = router.complete(req).await.unwrap_err();
        match err {
            Error::NotImplemented(alias, msg) => {
                assert_eq!(alias, "alias");
                assert!(
                    msg.contains("web_search"),
                    "error message must name the feature; got: {msg}",
                );
            }
            other => panic!("expected Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(captured_bedrock.lock().len(), 0);
        assert_eq!(captured_anthropic.lock().len(), 0);
    }

    #[tokio::test]
    async fn no_features_in_request_is_no_op_filter() {
        // Even when bedrock declares web_search unsupported, a
        // request without tools (no feature keys derived) dispatches
        // to bedrock first per the chain order.
        let (router, captured_bedrock, _captured_anthropic) =
            build_router_with_chain(vec!["web_search".into()], vec![]);
        let req = ChatRequest {
            model: "alias".into(),
            messages: vec![],
            tools: None,
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("bedrock-prov"));
        assert_eq!(
            captured_bedrock.lock().len(),
            1,
            "no features -> filter is a no-op, bedrock takes the request",
        );
    }

    #[tokio::test]
    async fn dated_suffix_versions_normalize_to_same_key() {
        // `web_search_20250305` and a hypothetical
        // `web_search_20251102` both reduce to the same key
        // `web_search`. Bedrock declares `web_search` unsupported, so
        // both versions get filtered identically.
        let (router, captured_bedrock, captured_anthropic) =
            build_router_with_chain(vec!["web_search".into()], vec![]);
        let req = ChatRequest {
            model: "alias".into(),
            messages: vec![],
            tools: Some(vec![ToolDef::Other(json!({
                "type": "web_search_20251102",
                "name": "search"
            }))]),
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
        assert_eq!(captured_bedrock.lock().len(), 0);
        assert_eq!(captured_anthropic.lock().len(), 1);
    }

    #[tokio::test]
    async fn custom_tools_dont_contribute_feature_keys() {
        // A user-defined `ToolDef::Custom` tool has no version-stamped
        // `type` and therefore contributes NO feature key. The filter
        // is a no-op even when bedrock has unsupported_features set.
        let (router, captured_bedrock, _captured_anthropic) =
            build_router_with_chain(vec!["web_search".into()], vec![]);
        let req = ChatRequest {
            model: "alias".into(),
            messages: vec![],
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calculator".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("bedrock-prov"));
        assert_eq!(
            captured_bedrock.lock().len(),
            1,
            "Custom tools must not be treated as feature keys",
        );
    }
}

#[cfg(test)]
mod auth_failure_recovery_tests {
    //! Router-level tests for the 401 -> `provider.on_auth_failure()`
    //! -> retry-once dispatch path. The OAuth store has its own
    //! lower-level tests for `refresh_under_lock` semantics; these
    //! tests pin the router-side wiring: that a 401 from a provider
    //! actually triggers `on_auth_failure`, that the retry happens
    //! exactly once, and that a refresh failure propagates without
    //! walking the fallback chain.
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock provider that returns `Error::Upstream { status: 401, .. }`
    /// on its first `complete()` call and a 200-shaped success on
    /// every subsequent call. `on_auth_failure_calls` increments on
    /// each `on_auth_failure()` invocation so the test can assert the
    /// router actually dispatched through the trait method.
    struct Recovering401Provider {
        id: String,
        complete_calls: AtomicUsize,
        on_auth_failure_calls: AtomicUsize,
        /// If set, `on_auth_failure` returns this error string wrapped
        /// in `Error::Auth` (simulating a refresh-token-revoked path).
        refresh_failure: Option<String>,
    }

    #[async_trait]
    impl Provider for Recovering401Provider {
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
            let n = self.complete_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(Error::upstream(&self.id, 401, "stale token"))
            } else {
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
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("not exercised by these tests")
        }
        async fn on_auth_failure(&self) -> Result<()> {
            self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = &self.refresh_failure {
                Err(Error::Auth(msg.clone()))
            } else {
                Ok(())
            }
        }
    }

    fn build_router_with_provider(provider: Arc<dyn Provider>) -> Router {
        let mut config = Config::default();
        config.providers.insert(
            "p-recover".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                runtime: ProviderRuntimePolicy::default(),
            },
        );
        config
            .aliases
            .insert("alias".into(), AliasValue::Single("recover-model".into()));
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "recover-model".into(),
            Arc::new(ResolvedModel::new(
                "recover-model",
                "p-recover",
                provider,
                "u-recover",
            )),
        );
        router.install_resolved_models(models);
        router
    }

    fn req_for(alias: &str) -> ChatRequest {
        ChatRequest {
            model: alias.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn router_401_triggers_on_auth_failure_and_retries_once() {
        let provider = Arc::new(Recovering401Provider {
            id: "p-recover".into(),
            complete_calls: AtomicUsize::new(0),
            on_auth_failure_calls: AtomicUsize::new(0),
            refresh_failure: None,
        });
        let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

        let resp = router
            .complete(req_for("alias"))
            .await
            .expect("401 -> on_auth_failure -> retry should land on the success branch");
        assert_eq!(resp.routectl_provider.as_deref(), Some("p-recover"));
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            2,
            "complete should be called twice: the 401 attempt and the retry",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure should fire exactly once between the 401 and the retry",
        );
    }

    #[tokio::test]
    async fn router_refresh_failure_propagates_without_fallback() {
        // When provider.on_auth_failure() itself errors (e.g.,
        // invalid_grant from the IdP), the router must surface that
        // error directly rather than walking the fallback chain. The
        // OAuth identity is dead; falling back over a known-broken
        // credential masks the failure.
        let provider = Arc::new(Recovering401Provider {
            id: "p-recover".into(),
            complete_calls: AtomicUsize::new(0),
            on_auth_failure_calls: AtomicUsize::new(0),
            refresh_failure: Some(
                "oauth refresh failed for anthropic: invalid_grant; \
                 re-run `routectl login anthropic`"
                    .into(),
            ),
        });
        let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

        let err = router
            .complete(req_for("alias"))
            .await
            .expect_err("refresh failure must surface as an error, not a fallback success");
        match err {
            Error::Auth(msg) => {
                assert!(
                    msg.contains("oauth refresh failed"),
                    "auth error must carry the refresh-failure message: {msg}",
                );
                assert!(
                    msg.contains("re-run"),
                    "auth error must carry the actionable hint: {msg}",
                );
            }
            other => panic!("expected Error::Auth, got: {other:?}"),
        }
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            1,
            "no retry should fire when on_auth_failure errors",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires exactly once before the auth error propagates",
        );
    }

    #[tokio::test]
    async fn router_second_consecutive_401_does_not_retry_again() {
        // After a successful refresh, if the SAME chain entry returns
        // 401 again (e.g., the upstream is broken in a way the
        // refresh can't fix), the auth_retry_attempted flag prevents
        // an infinite loop. The second 401 falls through to
        // should_fallback like any other 4xx error.
        struct AlwaysReturns401 {
            id: String,
            complete_calls: AtomicUsize,
            on_auth_failure_calls: AtomicUsize,
        }
        #[async_trait]
        impl Provider for AlwaysReturns401 {
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
                self.complete_calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::upstream(&self.id, 401, "still 401"))
            }
            async fn stream(
                &self,
                _: ChatRequest,
            ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
                unreachable!()
            }
            async fn on_auth_failure(&self) -> Result<()> {
                self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let provider = Arc::new(AlwaysReturns401 {
            id: "p-recover".into(),
            complete_calls: AtomicUsize::new(0),
            on_auth_failure_calls: AtomicUsize::new(0),
        });
        let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

        let _ = router
            .complete(req_for("alias"))
            .await
            .expect_err("perpetual 401 must surface as an error after the one retry");
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            2,
            "exactly two completes: the original 401 and the post-refresh 401 retry",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires once; the auth_retry_attempted flag blocks the second call",
        );
    }
}
