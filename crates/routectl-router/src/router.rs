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

/// The largest upstream reset hint we honor as an in-loop, same-provider
/// retry sleep (blocking the request thread). A reset at or below this
/// cap is folded into the next backoff sleep; a larger reset parks the
/// provider via the breaker instead, so the request falls over to a
/// sibling rather than blocking on a multi-minute (or hostile) hint.
const INLOOP_RETRY_AFTER_CAP: Duration = Duration::from_secs(5);

/// The single count_tokens-capable egress kind. `anthropic-api` is the
/// only `Provider` impl that overrides `Provider::count_tokens` (every
/// other kind uses the 501-ing trait default), and it is Claude-only,
/// so all capable targets share the same Anthropic tokenizer family.
/// `count_tokens` walks the dispatch chain to the first target whose
/// `provider_kind` matches this token and skips the rest. Matches the
/// `kind = "..."` discriminant from `ProviderEntry::kind_str`.
const COUNT_TOKENS_CAPABLE_KIND: &str = "anthropic-api";

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
    /// Per-pool round-robin cursors for OAuth credential pools whose
    /// `seat_selection` is `RoundRobin`. One `AtomicUsize` per pooled
    /// nickname; advanced once per request to rotate the starting seat.
    /// Deliberately NOT carried over on a Router rebuild (a reset to
    /// seat 0 is benign at single-operator scale). `FillFirst` pools
    /// and non-pooled models have no entry here.
    round_robin: crate::seat_pool::RoundRobinCursors,
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

/// Router-scoped accounting facts about how a single dispatch was
/// served. Returned alongside the result on BOTH the success and the
/// all-attempts-failed paths so a caller can record per-request usage
/// without re-deriving these facts (which would otherwise be trapped in
/// the dispatch loop's locals and lost on the error return).
///
/// This is deliberately separate from the provider-scoped
/// `UpstreamMeta` carrier: those facts describe ONE upstream response,
/// while these describe the WHOLE chain walk (how many attempts, how
/// many fallback hops, which target was terminal).
///
/// `#[non_exhaustive]` so new accounting fields can be added without
/// breaking downstream construction.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DispatchMeta {
    /// Total upstream attempts charged across the entire chain walk:
    /// every per-provider retry on every chain entry that was actually
    /// dispatched. Zero only when no upstream was touched (e.g. a chain
    /// entry with no built provider, or a pre-dispatch gate failure on
    /// the only entry).
    ///
    /// Also zero for a gate-blocked dispatch (the gate fires before any
    /// upstream contact); `served_provider` may still be `Some` in that
    /// case, naming the provider the gate refused.
    pub attempt_count: u32,
    /// Number of fallback hops to a LATER chain entry. Zero when the
    /// first dispatched target served the request or was the terminal
    /// failure; incremented once per move to a subsequent chain entry.
    pub fallback_count: u32,
    /// Config key (a `[providers]` name) of the provider that produced
    /// the success OR was the terminal error. `None` only when no
    /// provider was touched (resolution / feature-gate failure before
    /// any dispatch).
    pub served_provider: Option<String>,
    /// Stable provider-kind token (`anthropic-api` | `openai-compat` |
    /// `bedrock` | `openai-responses`) of `served_provider`. `None`
    /// under the same conditions as `served_provider`, or when the
    /// served target carried no provider-kind (legacy path).
    pub served_provider_kind: Option<String>,
    /// Served model nickname -- the operator-facing model label of the
    /// target that served or terminally failed. `None` when no provider
    /// was touched.
    pub served_model: Option<String>,
    /// Served wire model id -- the upstream id actually sent to the
    /// provider. `None` when no provider was touched.
    pub served_upstream: Option<String>,
    /// The resolved alias key the request routed under (the incoming
    /// `req.model`). Always populated, even when resolution then failed.
    pub resolved_alias: String,
}

impl DispatchMeta {
    /// Construct meta for an alias whose chain walk touched no provider
    /// yet (all served_* fields default to `None`, counters to zero).
    /// Callers populate the served_* fields and counters as the walk
    /// progresses.
    fn for_alias(alias: &str) -> Self {
        Self {
            attempt_count: 0,
            fallback_count: 0,
            served_provider: None,
            served_provider_kind: None,
            served_model: None,
            served_upstream: None,
            resolved_alias: alias.to_string(),
        }
    }

    /// Record the target currently being dispatched as the served /
    /// terminal target. Called on each chain entry the loop actually
    /// dispatches to, so on the all-failed path the LAST dispatched
    /// target is the terminal one.
    fn mark_target(&mut self, target: &DispatchTarget) {
        self.served_provider = Some(target.provider_name.clone());
        self.served_provider_kind = target.provider_kind.map(|k| k.to_string());
        self.served_model = target.nickname.clone();
        self.served_upstream = Some(target.upstream.clone());
    }
}

/// `complete_with_options` return: the non-streaming dispatch result
/// paired with its router-scoped [`DispatchMeta`]. Meta is valid on
/// both the `Ok` and `Err` arms of `result`. This carrier is a fixed
/// two-field pair (`meta`, `result`) so callers can destructure it;
/// the growth point is [`DispatchMeta`], not this wrapper.
#[derive(Debug)]
pub struct Dispatched {
    pub meta: DispatchMeta,
    pub result: Result<ChatResponse>,
}

/// `stream_with_options` return: the streaming dispatch result paired
/// with its router-scoped [`DispatchMeta`]. The served_* fields are
/// captured synchronously when the winning upstream's first chunk
/// arrives, so they are valid before the stream body is consumed. A
/// fixed two-field pair for the same reason as [`Dispatched`].
pub struct DispatchedStream {
    pub meta: DispatchMeta,
    pub result: Result<BoxStream<'static, Result<ChatChunk>>>,
}

/// One hop in the resolved dispatch chain. Built from either a
/// `Arc<ResolvedModel>` (v0.6.0 path) or a parsed `provider:model`
/// literal (legacy path). The dispatch loop reads from this struct
/// directly so the per-mode resolver only runs once per request.
///
/// Hop 3 of 4 in the per-model knob relay -- see the `PER-MODEL KNOB
/// RELAY` note on `crate::config::ModelEntry` before adding a field
/// that the egress reads. `apply_layered_overlays` (this file) copies
/// the verbatim pass-through fields onto `RoutectlInternal` (hop 4).
#[derive(Clone)]
struct DispatchTarget {
    /// Operator-facing provider name (a key in `[providers]`).
    provider_name: String,
    /// Stable provider-kind config token for this target's provider
    /// (`anthropic-api` | `openai-compat` | `bedrock` |
    /// `openai-responses`), copied from `ProviderEntry::kind_str()`
    /// when the chain is expanded. Surfaced through `DispatchMeta` so
    /// usage accounting can record WHICH egress kind served. `None`
    /// when the provider entry could not be looked up (a legacy /
    /// direct-construction path that never set it).
    provider_kind: Option<&'static str>,
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
    /// Operator-declared effort levels for this model. Threaded from
    /// `ResolvedModel.effort_levels`. Empty means passthrough (emit
    /// caller's effort verbatim). Non-empty: OpenAI-shape egresses
    /// clamp `req.reasoning.effort` to the nearest supported level.
    ///
    /// `Arc<[String]>` so cloning per dispatch attempt is a refcount
    /// bump rather than a heap allocation.
    effort_levels: std::sync::Arc<[String]>,
    /// Model nickname for tracing.
    nickname: Option<String>,
    /// The shared resolved model this target dispatches to. Carried as
    /// `Arc<ResolvedModel>` so the per-request dispatch hop is a
    /// refcount bump rather than a deep clone of the model's
    /// `header_extras` (a `BTreeMap`) + `payload_extras` (a JSON
    /// `Value`). `apply_layered_overlays` reads `model.header_extras` /
    /// `model.payload_extras` by shared ref into a fresh merged map and
    /// never mutates them, so sharing the Arc across requests is safe.
    model: Arc<ResolvedModel>,
    /// Per-model openai-compat reasoning dialect. `None` falls back
    /// to the egress's own default.
    reasoning_dialect: Option<ReasoningDialect>,
    /// Per-model openai-compat outgoing-history reasoning policy.
    history_reasoning: Option<HistoryReasoning>,
    /// Per-model `stream_first_byte_timeout_ms`.
    stream_first_byte_timeout_ms: Option<u64>,
    /// Operator-declared maximum thinking-token budget for this model.
    /// Threaded from `ResolvedModel.max_thinking_budget`. Zero means no
    /// operator cap; `apply_layered_overlays` writes this to
    /// `RoutectlInternal.max_thinking_budget` for the egress to read.
    max_thinking_budget: u32,
    /// Operator-declared per-model `max_tokens` ceiling for the
    /// anthropic-api egress. Threaded from
    /// `ResolvedModel.max_output_tokens`. Zero means no model
    /// override -- `apply_layered_overlays` falls through to the
    /// server-side default.
    max_output_tokens: u32,
    /// Operator-declared label echoed back in the response `model`
    /// field. Threaded from `ResolvedModel.reported_model`. `None`
    /// (or an empty string) makes the response echo the client's
    /// requested alias; `Some(non-empty)` overrides it. Does not
    /// affect `DispatchMeta` accounting.
    reported_model: Option<String>,
    /// Whether the response `routectl_provider` field is exposed to the
    /// client for the served target. Threaded from
    /// `ResolvedModel.visible_routectl_provider`. `true` (the default)
    /// stamps the served provider name; `false` suppresses it. Chain
    /// semantics: the served target's value wins. Does not affect
    /// `DispatchMeta` accounting.
    visible_routectl_provider: bool,
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
            round_robin: Default::default(),
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
            // A pooled model (Some seats) gets one state slot per seat,
            // each keyed by the seat's `state_key`, so the breaker + RPM
            // bucket are per-seat (R7/R8/R12 + D1 park apply per seat).
            // The default seat keys as the bare nickname, so the slot
            // below covers it; this loop adds the labeled-seat slots.
            // A non-pooled model (None seats) only gets the nickname slot.
            if let Some(seats) = m.seats.as_ref() {
                for seat in seats.iter() {
                    self.state
                        .entry(seat.state_key.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&policy))));
                }
                // Round-robin pools rotate the starting seat per request;
                // register a cursor only for that selection mode.
                if matches!(
                    policy.seat_selection,
                    crate::config::SeatSelection::RoundRobin
                ) {
                    self.round_robin.register(nickname);
                }
            }
            self.state
                .entry(nickname.clone())
                .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&policy))));
        }
    }

    /// Carry over per-nickname runtime state from a previous Router.
    /// For each key in `previous.state` that also exists in `self.state`,
    /// replaces the fresh-allocated `Arc` with the prior one so that
    /// circuit-breaker counters and RPM token buckets survive a hot-reload.
    /// Nicknames present only in `self` (genuinely new models) keep their
    /// fresh-allocated state unchanged.
    ///
    /// Called by the hot-reload coordinator in routectl-cli immediately
    /// after building a replacement Router and before swapping it in, to
    /// avoid resetting gates that took time to build up across reloads.
    pub fn carry_over_runtime_state_from(&mut self, previous: &Router) {
        for (key, state) in &previous.state {
            if self.state.contains_key(key.as_str()) {
                self.state.insert(key.clone(), state.clone());
            }
        }
    }

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
            return Ok(self.expand_chain_to_targets(chain));
        }
        // Wire model could ALSO be a direct nickname.
        if let Some(m) = self.resolve_nickname(model) {
            return Ok(self.expand_chain_to_targets(vec![m]));
        }
        // Catch-all: only consulted after exact alias / glob / direct
        // nickname all miss. This ordering means a wire model that's
        // a known nickname always wins over a configured default.
        if let Some(chain) = self.resolve_default_alias()? {
            return Ok(self.expand_chain_to_targets(chain));
        }
        Err(Error::UnknownAlias(model.to_string()))
    }

    /// Expand a resolved-model chain into the per-request dispatch-target
    /// chain. A non-pooled model (`seats == None`) maps to exactly one
    /// target keyed by nickname -- byte-for-byte the pre-pool path. A
    /// pooled model maps to one target per seat, in the order
    /// `seat_pool::seat_order_for_request` returns for the provider's
    /// `seat_selection` (FillFirst: fixed default-first order; RoundRobin:
    /// per-request rotated start). The expanded seat targets slot inline
    /// where the model sat, preserving the operator's fallback order so a
    /// chain `[opus, sonnet]` becomes `[opus-seatA, opus-seatB, sonnet]`.
    fn expand_chain_to_targets(&self, chain: Vec<Arc<ResolvedModel>>) -> Vec<DispatchTarget> {
        let mut out: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        for m in chain {
            match m.seats.as_ref() {
                None => out.push(into_one_dispatch_target(m)),
                Some(seats) => self.push_seat_targets(&m, seats, &mut out),
            }
        }
        for target in &mut out {
            target.provider_kind = self
                .config
                .providers
                .get(&target.provider_name)
                .map(|e| e.kind_str());
        }
        out
    }

    /// Append one dispatch target per seat of a pooled model, in the
    /// request's resolved seat order. Each target carries the seat's own
    /// provider, `state_key`, and `auth_secret_ref` so the breaker, RPM
    /// gate, retry caps, probe fast-fail, and D1 `Retry-After` park all
    /// apply per seat; every other dispatch knob is shared from the model.
    fn push_seat_targets(
        &self,
        m: &Arc<ResolvedModel>,
        seats: &[crate::seat_pool::SeatTarget],
        out: &mut Vec<DispatchTarget>,
    ) {
        let selection = self
            .config
            .providers
            .get(&m.provider_name)
            .map(|e| e.runtime().seat_selection)
            .unwrap_or_default();
        let order = crate::seat_pool::seat_order_for_request(
            &m.nickname,
            seats.len(),
            selection,
            &self.round_robin,
        );
        for idx in order {
            let seat = &seats[idx];
            out.push(dispatch_target_for_seat(m, seat));
        }
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
            .result
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn complete_with_options(&self, req: ChatRequest, opts: RouterOptions) -> Dispatched {
        let mut meta = DispatchMeta::for_alias(&req.model);
        let result = self.complete_inner(req, opts, &mut meta).await;
        Dispatched { meta, result }
    }

    /// Dispatch-loop body for `complete_with_options`. Mutates `meta` as
    /// the chain is walked -- `attempt_count` on every upstream attempt,
    /// `fallback_count` on every hop to a later entry, and the served_*
    /// fields on each dispatched target -- so the caller's `meta` is
    /// correct at every early return, including the all-failed Err path.
    async fn complete_inner(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
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
            // This entry is being dispatched: record it as the terminal
            // target (overwritten by a later entry on fallback) and count
            // the hop. `chain_idx` is the index into the resolved chain;
            // a non-zero index means at least one earlier entry was tried
            // or skipped, i.e. we fell back to reach here.
            meta.mark_target(target);
            meta.fallback_count = chain_idx as u32;

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
                    // Keep the FIRST real error: a synthetic gate error
                    // (status 0 "circuit breaker open" / RPM) on a later
                    // chain entry must not overwrite an earlier entry's
                    // genuine upstream failure, or the client sees the
                    // synthetic error instead of the real 503/timeout.
                    if last_err.is_none() {
                        last_err = Some(gate_err);
                    }
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
                meta.attempt_count += 1;

                match result {
                    Ok(mut resp) => {
                        self.record_success(state_key);
                        // Stamp the served upstream provider name only
                        // when the served target opts in
                        // (`visible_routectl_provider`, default true).
                        // When suppressed, leave the field None so
                        // serde's skip_serializing_if drops it from the
                        // response. Internal accounting keys off
                        // `DispatchMeta.served_provider` / `served_upstream`,
                        // not this client-visible field, so suppression
                        // does not affect usage capture.
                        // Authoritative gate: every concrete provider
                        // pre-stamps `routectl_provider` with its own id
                        // before returning, so suppression MUST clear the
                        // field, not merely skip setting it -- otherwise the
                        // provider's value leaks through.
                        if target.visible_routectl_provider {
                            resp.routectl_provider = Some(provider_name.to_string());
                        } else {
                            resp.routectl_provider = None;
                        }
                        // Client-visible label: the serving target's
                        // `reported_model` override when set (and non-empty),
                        // else the client's requested alias. Internal
                        // accounting keys off `DispatchMeta`, not this field.
                        resp.model = resolve_reported_model(target, &req.model);
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
                            // A refresh failure means the OAuth identity is
                            // dead; surface it without walking the chain. But
                            // first release any half-open probe slot this
                            // attempt claimed via the gate, or the breaker
                            // stays locked open until restart.
                            if let Err(refresh_err) = provider.on_auth_failure().await {
                                self.release_probe_slot(state_key);
                                return Err(refresh_err);
                            }
                            // Refresh succeeded. Release the half-open probe
                            // slot this attempt claimed at the gate BEFORE the
                            // `continue` re-enters the loop and re-runs
                            // `gate_check`. While this caller still holds the
                            // slot, the in-loop re-gate's `try_dispatch` sees
                            // `half_open_in_flight` and returns CircuitOpen,
                            // which would leave the breaker locked open until
                            // restart. Releasing here lets the re-gate claim a
                            // fresh slot (the per-attempt accounting the
                            // in-loop gate promises).
                            self.release_probe_slot(state_key);
                            continue;
                        }
                        let do_fallback = should_fallback(&e, &policy, is_probe);
                        // Probe fast-fail: a probe (max_tokens <=
                        // probe_max_tokens) that hit a rate-limit/overload
                        // (429/529) returns the status immediately via an
                        // explicit early return -- no retry, no fallback,
                        // no breaker failure debit (record_failure is gated
                        // on `do_fallback`, the retry branch on
                        // `can_retry_here`, both false here). It does still
                        // release the half-open slot it may have claimed at
                        // the gate (see below).
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
                            // Release the half-open slot this probe claimed
                            // at the gate: a 429/529 probe fast-fail is a
                            // transient upstream condition we deliberately do
                            // NOT count as a provider fault (that is why
                            // should_fallback is false here), so the slot
                            // must be freed without a breaker debit.
                            self.release_probe_slot(state_key);
                            return Err(e);
                        }
                        // The honored upstream reset for THIS error (clamped
                        // to the ceiling), computed once for both the park
                        // decision below and the in-loop sleep bump in the
                        // retry branch. `None` for every non-rate-limit error.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        if do_fallback {
                            match reset_hint {
                                // Non-probe LARGE reset: park the provider for
                                // the honored duration (force_open) instead of
                                // a threshold-gated debit, so an exhausted seat
                                // is skipped until it actually resets. The
                                // in-loop re-gate then diverts to fallback /
                                // fail. Probes never park (R7), and a small
                                // reset is honored as an in-loop sleep (below),
                                // so only the large non-probe case parks here.
                                Some(h) if !is_probe && h > INLOOP_RETRY_AFTER_CAP => {
                                    self.park_provider(state_key, h);
                                }
                                _ => self.record_failure(state_key),
                            }
                        }
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when
                            // do_fallback already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
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
                            // Honor a SMALL non-probe upstream reset as the
                            // next in-loop sleep: bump `backoff` so the
                            // loop-top sleep waits at least the reset before
                            // re-probing the SAME provider. Only when we were
                            // already going to retry here (can_retry_here is
                            // unchanged -- R12), only for a reset within the
                            // in-loop cap (a larger reset parked the provider
                            // above instead of blocking this thread), and never
                            // for a probe (R7).
                            if let Some(h) = reset_hint {
                                if !is_probe && h <= INLOOP_RETRY_AFTER_CAP {
                                    backoff = backoff.max(h);
                                }
                            }
                            // Free the half-open probe slot this attempt
                            // claimed before re-probing: the in-loop re-gate
                            // re-runs `try_dispatch`, which would otherwise see
                            // this caller's still-held slot as
                            // `half_open_in_flight` and return CircuitOpen,
                            // locking the breaker open forever (mirrors the
                            // auth-retry Ok path).
                            self.release_probe_slot(state_key);
                            continue;
                        }
                        // Done with this provider. Decide fallback vs propagate.
                        if do_fallback {
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    state_key = %state_key,
                                    error = ?e,
                                    "fallback to next",
                                );
                            } else {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    state_key = %state_key,
                                    error = ?e,
                                    "chain exhausted; no fallback target available; request will fail",
                                );
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Probe call: route a request to the first count_tokens-CAPABLE
    /// provider in the dispatch chain and call `Provider::count_tokens`.
    /// Used by claude-code's context-budget display via the
    /// `/v1/messages/count_tokens` endpoint.
    ///
    /// Capability walk (not a try-and-fallback): the chain is scanned
    /// for the first target whose `provider_kind == "anthropic-api"` --
    /// the only count_tokens-capable egress kind (it is the only kind
    /// that overrides `Provider::count_tokens`; every other kind uses
    /// the trait default that 501s). Incapable-by-kind targets are
    /// skipped BEFORE dispatch (DEBUG log, no upstream call, no breaker
    /// account); a kind-skip is operator-known, not upstream health, so
    /// it must not touch the breaker. This mirrors
    /// `filter_chain_by_features` discipline.
    ///
    /// Why walking is safe for tokenizer correctness: `anthropic-api`
    /// is Claude-only, so every capable target the walk can select uses
    /// the SAME Anthropic tokenizer family. Walking past incapable
    /// kinds therefore does NOT reintroduce the wrong-tokenizer hazard
    /// that motivated the original first-only rule -- it only steps over
    /// kinds that cannot count at all.
    ///
    /// Once a CAPABLE target is selected, it does NOT walk further on a
    /// real upstream error (4xx/5xx) or on `Error::NotImplemented` --
    /// those propagate verbatim, exactly as before. Only a 401 triggers
    /// the single-flight auth refresh + one retry of the SAME target.
    /// Callers (the count_tokens handler) translate `NotImplemented` to
    /// a 501 response per the gateway-doc contract. When NO target in
    /// the chain is capable, this returns `Error::NotImplemented` naming
    /// the alias so the genuinely-uncapable case still maps to 501.
    ///
    /// count_tokens calls consume the same RPM bucket and honor the
    /// same circuit breaker as messages calls: the gate runs before
    /// the upstream is touched, and a successful or failed probe
    /// records into the breaker exactly like `complete()`. This
    /// prevents probe-spam from bypassing operator rate limits.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let chain = self.dispatch_chain_for_request(&req)?;
        // Capability walk: select the first count_tokens-capable target
        // (provider_kind == "anthropic-api") and skip incapable kinds
        // before dispatch. anthropic-api is the only kind that overrides
        // the 501-ing trait default, and it is Claude-only -- so any
        // target the walk selects shares the same Anthropic tokenizer
        // family. That is why stepping past incapable kinds does NOT
        // reintroduce the wrong-tokenizer hazard the first-only rule
        // once guarded against. A kind-skip is operator-known config,
        // not upstream health, so it never touches the breaker.
        let mut target: Option<DispatchTarget> = None;
        for candidate in chain {
            if candidate.provider_kind == Some(COUNT_TOKENS_CAPABLE_KIND) {
                target = Some(candidate);
                break;
            }
            tracing::debug!(
                provider = %candidate.provider_name,
                kind = candidate.provider_kind.unwrap_or("unknown"),
                model = %candidate.nickname.as_deref().unwrap_or(""),
                "provider skipped: kind cannot count_tokens",
            );
        }
        let target = match target {
            Some(t) => t,
            None => {
                tracing::warn!(
                    alias = %req.model,
                    "alias chain has no count_tokens-capable provider; \
                     no target in chain overrides count_tokens",
                );
                return Err(Error::NotImplemented(
                    req.model.clone(),
                    "count_tokens: no count_tokens-capable provider in chain".into(),
                ));
            }
        };
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
            // Per-attempt gate: rate limit + circuit breaker. Lives
            // INSIDE the loop (mirroring `complete_with_options`) so the
            // auth-401 retry is gated + RPM-debited exactly like the
            // first attempt -- per-attempt accounting is uniform across
            // all three dispatch sites. The gate runs once per attempt,
            // so the first attempt is debited exactly once.
            //
            // Unlike `complete_with_options`, count_tokens does NOT walk
            // to a sibling on a gate block of the SELECTED capable
            // target: the capability walk above already skipped
            // incapable kinds, and every remaining capable target shares
            // one tokenizer family, so a gate block here just propagates
            // (no try-and-fallback over upstream/gate state).
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
                        // Release any half-open probe slot this attempt
                        // claimed before surfacing a dead OAuth identity,
                        // or the breaker stays locked open until restart.
                        if let Err(refresh_err) = provider.on_auth_failure().await {
                            self.release_probe_slot(&target.state_key);
                            return Err(refresh_err);
                        }
                        // Refresh succeeded. Release the half-open probe slot
                        // this attempt claimed at the gate BEFORE the
                        // `continue` re-enters the loop and re-runs
                        // `gate_check`. While this caller still holds the slot,
                        // the in-loop re-gate's `try_dispatch` sees
                        // `half_open_in_flight` and returns CircuitOpen, which
                        // count_tokens propagates as the gate error -- leaving
                        // the breaker locked open until restart. Releasing here
                        // lets the re-gate claim a fresh slot.
                        self.release_probe_slot(&target.state_key);
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
                    // it never falls over to a sibling on an upstream
                    // error (the capability walk runs BEFORE dispatch;
                    // a real upstream error from the selected capable
                    // target propagates), so a 429 here keeps its
                    // existing breaker-accounting behavior.
                    let policy = self.policy_for(&req.model);
                    let reset_hint = rate_limit_reset_hint(&e, &policy);
                    if should_fallback(&e, &policy, false) {
                        // A reset hint (any size) parks the provider for the
                        // honored, clamped duration so the next count_tokens
                        // skips this seat until it actually resets. No probe
                        // split here: count_tokens is not the generation-probe
                        // path (is_probe=false above) and takes no in-loop
                        // sleep, so every honored reset parks. With no hint,
                        // fall through to the threshold-gated debit as today.
                        match reset_hint {
                            Some(h) => self.park_provider(&target.state_key, h),
                            None => self.record_failure(&target.state_key),
                        }
                    } else {
                        // Non-fallbackable client error (NotImplemented,
                        // 4xx): not counted against the breaker, but we
                        // must release any half-open probe slot this
                        // attempt claimed so the breaker is not locked.
                        self.release_probe_slot(&target.state_key);
                    }
                    return Err(e);
                }
            }
        }
    }

    pub async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_with_options(req, RouterOptions::default())
            .await
            .result
    }

    /// Streaming counterpart. Fallback only happens BEFORE the first
    /// chunk reaches us; once the upstream has emitted a chunk,
    /// mid-stream errors propagate. Gate checks (rate limit / breaker)
    /// run before the upstream is touched.
    #[must_use]
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn stream_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> DispatchedStream {
        let mut meta = DispatchMeta::for_alias(&req.model);
        let result = self.stream_inner(req, opts, &mut meta).await;
        DispatchedStream { meta, result }
    }

    /// Dispatch-loop body for `stream_with_options`. Mutates `meta` as
    /// the chain is walked. The served_* fields are captured at the
    /// `Ok(stream)` arm (the winning target is known synchronously,
    /// before any stream body is consumed).
    async fn stream_inner(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
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
            // This entry is being dispatched: record it as the terminal
            // target (overwritten by a later entry on fallback) and count
            // the hop -- see `complete_inner` for the `chain_idx` rationale.
            meta.mark_target(target);
            meta.fallback_count = chain_idx as u32;

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
            loop {
                // Per-attempt gate: rate limit + circuit breaker. Lives
                // INSIDE the loop (mirroring `complete_with_options`) so
                // the auth-401 retry is gated + RPM-debited exactly like
                // the first attempt -- per-attempt accounting is uniform
                // across all three dispatch sites. Streams don't retry on
                // ordinary errors, so the only second iteration is the
                // auth-recovery retry; the gate runs once per attempt, so
                // the first attempt is debited exactly once.
                if let Some((gate_kind, gate_err)) = self.gate_check(state_key, provider_name) {
                    tracing::warn!(
                        provider = provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        gate_kind,
                        error = ?gate_err,
                        "stream gate blocked",
                    );
                    // Keep the FIRST real error: a synthetic gate error
                    // (status 0 "circuit breaker open" / RPM) on a later
                    // chain entry must not overwrite an earlier entry's
                    // genuine upstream failure, or the client sees the
                    // synthetic error instead of the real 503/timeout.
                    if last_err.is_none() {
                        last_err = Some(gate_err);
                    }
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue 'chain;
                }

                // Gate granted Allow. Capture NOW whether this dispatch
                // claimed the half-open probe slot: only a probe's first
                // chunk should close + release the breaker at the Ok arm
                // below. Reading the flag at first-chunk time instead
                // would race a concurrent dispatch.
                let was_half_open_probe = self.is_half_open_probe(state_key);

                let r = try_stream_with_first_chunk(
                    provider_name,
                    provider.clone(),
                    attempt_req.clone(),
                    &attempt_policy,
                )
                .await;
                attempts_made += 1;
                meta.attempt_count += 1;
                match r {
                    Ok(stream) => {
                        // A half-open PROBE that produced a first chunk has
                        // proven the upstream live -- close the breaker NOW
                        // (release the single probe slot) rather than
                        // holding it for the whole stream duration, which
                        // would lock out all concurrent requests to this
                        // model until the stream ends. Gate this on
                        // `was_half_open_probe`: for a HEALTHY (closed)
                        // breaker the first chunk must NOT reset the failure
                        // counter, or mid-stream errors could never
                        // accumulate toward the threshold (each stream's
                        // first-chunk reset would zero the count).
                        //
                        // Closing here clears the half-open flag, so a
                        // mid-stream failure recorded by the wrap below is
                        // counted as a normal failure accumulating toward
                        // `circuit_failures` -- a probe that delivered one
                        // chunk then errors does NOT get a special immediate
                        // re-trip. With circuit_failures = 1 a single
                        // post-close mid-stream error re-quarantines at once
                        // (fast-flap); with >= 2 a still-degraded upstream
                        // may serve up to that many first-chunk-then-error
                        // streams before re-opening -- the throughput-vs-
                        // quarantine tradeoff of closing on the first chunk
                        // (see runtime_state.rs).
                        let state = self.state.get(state_key).cloned();
                        if was_half_open_probe {
                            if let Some(st) = state.as_ref() {
                                st.lock().record_success();
                            }
                        }
                        // Stamp the client-visible label on every Ok chunk
                        // (including the terminal / usage-only chunk) before
                        // the breaker wrap. The closure owns the label String
                        // (moved in via `move`) to satisfy the 'static
                        // BoxStream bound; `chunk.model` needs an owned String,
                        // so the label is cloned per Ok chunk. `Err` passes
                        // through byte-for-byte unchanged.
                        let label = resolve_reported_model(target, &req.model);
                        let relabeled = stream.map(move |item| match item {
                            Ok(mut chunk) => {
                                chunk.model = label.clone();
                                Ok(chunk)
                            }
                            Err(e) => Err(e),
                        });
                        return Ok(wrap_with_breaker_accounting(relabeled.boxed(), state));
                    }
                    Err(e) => {
                        // Auth-401 single-flight refresh + retry once
                        // (pre-first-chunk only). A refresh failure means
                        // the OAuth identity is dead; surface it without
                        // walking the chain, but first release any half-open
                        // probe slot this attempt claimed at the gate or the
                        // breaker stays locked open until restart.
                        if !auth_retry_attempted
                            && matches!(&e, Error::Upstream { status: 401, .. })
                        {
                            auth_retry_attempted = true;
                            tracing::debug!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                "stream 401 pre-first-chunk; refreshing auth and retrying once",
                            );
                            if let Err(refresh_err) = provider.on_auth_failure().await {
                                self.release_probe_slot(state_key);
                                return Err(refresh_err);
                            }
                            // Refresh succeeded. Release the half-open probe
                            // slot this attempt claimed at the gate BEFORE the
                            // `continue` re-enters the loop and re-runs
                            // `gate_check`. While this caller still holds the
                            // slot, the in-loop re-gate's `try_dispatch` sees
                            // `half_open_in_flight` and returns CircuitOpen,
                            // which would leave the breaker locked open until
                            // restart. Releasing here lets the re-gate claim a
                            // fresh slot.
                            self.release_probe_slot(state_key);
                            continue;
                        }
                        let do_fallback = should_fallback(&e, &policy, is_probe);
                        // Probe fast-fail: a probe that hit a rate-limit/
                        // overload (429/529) returns the status immediately
                        // -- no fallback, no breaker failure debit
                        // (record_failure is gated on `do_fallback`, false
                        // here). It does release the half-open slot it may
                        // have claimed at the gate (see below). Streams never
                        // retry the same provider, so there is no
                        // can_retry_here to guard.
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
                            // Release the half-open slot this probe claimed
                            // at the gate: a 429/529 probe fast-fail is a
                            // transient upstream condition we deliberately do
                            // NOT count as a provider fault, so free the slot
                            // without a breaker debit.
                            self.release_probe_slot(state_key);
                            return Err(e);
                        }
                        // Stream dispatch never retries the same provider (no
                        // in-loop sleep), so a reset hint only sizes the
                        // breaker park. A non-probe reset parks the provider
                        // for the honored, clamped duration; a probe never
                        // parks (R7) and a no-hint error keeps the
                        // threshold-gated debit.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        if do_fallback {
                            match reset_hint {
                                Some(h) if !is_probe => self.park_provider(state_key, h),
                                _ => self.record_failure(state_key),
                            }
                        }
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when
                            // do_fallback already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
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
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        return Err(e);
                    }
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

    /// Park the provider's breaker open for `cooldown`, bypassing the
    /// consecutive-failure threshold. Used when an upstream sent an
    /// explicit rate-limit reset hint larger than the in-loop sleep cap:
    /// a single such signal opens the circuit at once so the chain skips
    /// this seat until it actually resets, rather than re-probing on the
    /// flat schedule. The caller MUST have already clamped `cooldown` to
    /// `RetryPolicy::max_honored_retry_after` (see `rate_limit_reset_hint`).
    /// `force_open` clears any in-flight half-open slot, so this is a
    /// leak-safe substitute for the `record_failure` it replaces.
    fn park_provider(&self, state_key: &str, cooldown: Duration) {
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
    fn release_probe_slot(&self, state_key: &str) {
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
    fn is_half_open_probe(&self, state_key: &str) -> bool {
        self.state
            .get(state_key)
            .is_some_and(|state| state.lock().half_open_probe_in_flight())
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
        &target.model.header_extras,
        req,
    );
    merge_payload_extras(
        &target.provider_name,
        provider_payload,
        target.model.payload_extras.as_ref(),
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
    // Preserve the ingress-set provenance: like `claude_code_headers`,
    // it is inbound-request data (which dialect produced the request),
    // not a per-model knob, so the per-attempt rebuild from
    // `Default::default()` must carry it across or it resets to
    // `Library`. `RequestProvenance` is `Copy`, so a plain read suffices.
    let captured_provenance = req.routectl_internal.provenance;
    // Preserve the header_extras map that `merge_header_extras` composed
    // onto the request above. The struct rebuild starts from
    // `Default::default()`, so without this take the merged provider +
    // model header_extras would be dropped before the egress reads them.
    let composed_header_extras = req.routectl_internal.header_extras.take();
    let mut internal = RoutectlInternal::default();
    internal.reasoning_dialect = target.reasoning_dialect.map(|d| d.into());
    internal.history_reasoning = target.history_reasoning.map(|h| h.into());
    internal.claude_code_headers = captured_claude_code_headers;
    internal.provenance = captured_provenance;
    internal.header_extras = composed_header_extras;
    internal.supports_adaptive_thinking = target.supports_adaptive_thinking;
    internal.effort_levels = target.effort_levels.clone();
    internal.max_thinking_budget = target.max_thinking_budget;
    // Per-model `max_tokens` ceiling. Zero means no per-model override;
    // Anthropic-shape egresses (anthropic-api, bedrock-invoke) read this
    // and fall through to their hardcoded 64000 baseline when zero.
    // Other egresses (openai-compat, openai-responses, bedrock-converse)
    // ignore this field and forward `req.max_tokens` omission cleanly.
    internal.max_output_tokens = target.max_output_tokens;
    // Operator-configured beta floor: the provider + model
    // `header_extras["anthropic-beta"]` betas, EXCLUDING the
    // client/ingress betas already on `req.anthropic_beta`. The
    // Anthropic-API egress re-adds these unconditionally after applying
    // the per-provider `allowed_betas` allowlist, so an operator's
    // model-pinned beta bypasses a filter meant only for client betas.
    // `req.anthropic_beta` itself stays the full union (composed by
    // `merge_header_extras`) so Bedrock's `filter_bedrock_betas` and the
    // log-safe summary still see the complete set.
    internal.operator_betas = operator_betas(provider_headers, &target.model.header_extras);
    req.routectl_internal = internal;
}

/// Collect the operator-configured `anthropic-beta` floor: the union of
/// the provider and model `header_extras["anthropic-beta"]` values
/// (comma-split, trimmed, deduplicated, visit order preserved). Client/
/// ingress betas are deliberately excluded -- those ride on
/// `req.anthropic_beta` and stay subject to the per-provider
/// `allowed_betas` allowlist.
fn operator_betas(
    provider_extras: Option<&BTreeMap<String, String>>,
    model_extras: &BTreeMap<String, String>,
) -> Vec<String> {
    let provider_val = provider_extras
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, v)| v.as_str())
        })
        .unwrap_or("");
    let model_val = model_extras
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in [provider_val, model_val] {
        for piece in raw.split(',') {
            let t = piece.trim();
            if !t.is_empty() && seen.insert(t.to_string()) {
                out.push(t.to_string());
            }
        }
    }
    out
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
/// The merged headers are published via `req.routectl_internal.header_extras`
/// and consumed by all four egresses (anthropic-api, openai-compat, bedrock,
/// openai-responses) at request-build time through
/// `crate::http_client::effective_header_extras`. The `anthropic-beta`
/// list-valued header is additionally written back to `req.anthropic_beta` so
/// the Anthropic-API egress (canonical field read) and Bedrock's beta filter
/// both see the fully-unioned set. Library consumers that construct a
/// `ChatRequest` without the router leave `header_extras` as `None`; the
/// egresses fall back to their construction-time `self.cfg.header_extras`
/// snapshot in that case.
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

/// Resolve the client-visible response `model` label for a served
/// dispatch target. Returns the target's `reported_model` override
/// when set to a non-empty string, otherwise the client's requested
/// alias (`req_model`). An empty override string is treated as unset.
/// Computed once per request so a fallback chain / multi-chunk stream
/// carries one stable label.
fn resolve_reported_model(target: &DispatchTarget, req_model: &str) -> String {
    match target.reported_model.as_deref() {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => req_model.to_string(),
    }
}

/// Convert a chain of `Arc<ResolvedModel>` into the `DispatchTarget`
/// shape the dispatch loop walks. Hoisted out of `dispatch_chain`
/// so the three resolution branches share one builder.
fn into_one_dispatch_target(m: Arc<ResolvedModel>) -> DispatchTarget {
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        provider_kind: None,
        // v0.6.0 dispatch keys the breaker by nickname so two models
        // on one provider quarantine independently.
        state_key: m.nickname.clone(),
        upstream: m.upstream.clone(),
        provider: Some(m.provider.clone()),
        supports_adaptive_thinking: m.supports_adaptive_thinking,
        effort_levels: m.effort_levels.clone(),
        nickname: Some(m.nickname.clone()),
        reasoning_dialect: m.reasoning_dialect,
        history_reasoning: m.history_reasoning,
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
        max_thinking_budget: m.max_thinking_budget,
        max_output_tokens: m.max_output_tokens,
        reported_model: m.reported_model.clone(),
        visible_routectl_provider: m.visible_routectl_provider,
        model: m,
    }
}

/// Build a dispatch target for one seat of a pooled model. Identical to
/// `into_one_dispatch_target` except the seat overrides the provider
/// instance and `state_key` (its own breaker + RPM bucket); every other
/// knob is shared from the model. The nickname stays the model's nickname
/// for tracing, while `state_key` carries the seat suffix.
fn dispatch_target_for_seat(
    m: &Arc<ResolvedModel>,
    seat: &crate::seat_pool::SeatTarget,
) -> DispatchTarget {
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        provider_kind: None,
        state_key: seat.state_key.clone(),
        upstream: m.upstream.clone(),
        provider: Some(seat.provider.clone()),
        supports_adaptive_thinking: m.supports_adaptive_thinking,
        effort_levels: m.effort_levels.clone(),
        nickname: Some(m.nickname.clone()),
        reasoning_dialect: m.reasoning_dialect,
        history_reasoning: m.history_reasoning,
        stream_first_byte_timeout_ms: m.stream_first_byte_timeout_ms,
        max_thinking_budget: m.max_thinking_budget,
        max_output_tokens: m.max_output_tokens,
        reported_model: m.reported_model.clone(),
        visible_routectl_provider: m.visible_routectl_provider,
        model: m.clone(),
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
/// For a half-open PROBE the call site already closed the breaker on the
/// first chunk, so a mid-stream failure here re-trips it and a
/// clean completion / consumer cancellation is a benign re-zeroing of the
/// just-closed breaker. For a HEALTHY (closed) breaker the call site did
/// NOT touch the breaker, so this wrap is where mid-stream failures
/// accumulate toward the threshold (and a clean completion resets the
/// counter). Consumer cancellation before any error is treated as success
/// in both cases.
fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;
    struct BreakerAccounting {
        state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
        settled: bool,
    }

    impl BreakerAccounting {
        fn new(state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>) -> Self {
            Self {
                state,
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
            // Clean completion OR consumer cancellation before any error:
            // both record success. For a probe stream the breaker is
            // already closed (call-site first-chunk close), so this is a
            // no-op re-zeroing; for a healthy-breaker stream this resets
            // any accumulated failure count on a fully-consumed success.
            if !self.settled {
                self.record_success();
            }
        }
    }

    let mut accounting = BreakerAccounting::new(state);
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

/// The honored rate-limit reset for `err`, clamped to the configured
/// ceiling, or `None` when `err` carries no reset hint. A reset hint is
/// present ONLY on a rate-limit/overload (429/503/529) where the
/// upstream told us when it resets (`Retry-After`, or the Codex
/// `usage_limit_reached` `resets_at` / `resets_in_seconds`), so its
/// presence is itself the rate-limit signal. The clamp bounds both the
/// in-loop honored sleep and the breaker park to
/// `RetryPolicy::max_honored_retry_after` so a hostile or buggy upstream
/// cannot pin a provider open indefinitely. A zero-duration hint (e.g.
/// `Retry-After: 0`, a past HTTP-date, or a saturated `resets_at`) yields
/// `None` so it falls through to the normal threshold-gated debit instead
/// of a degenerate zero-length park (a `force_open(.., ZERO)` would leave
/// the breaker stuck half-open without ever tripping).
fn rate_limit_reset_hint(err: &Error, policy: &RetryPolicy) -> Option<Duration> {
    match err {
        Error::Upstream {
            retry_after: Some(d),
            ..
        } => Some((*d).min(policy.max_honored_retry_after())).filter(|d| !d.is_zero()),
        _ => None,
    }
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

    #[test]
    fn carry_over_runtime_state_from_preserves_existing_state_arcs() {
        // Arrange: build two fresh Routers; insert named state entries
        // directly to simulate pre-loaded model nicknames without requiring
        // real Provider impls.
        use crate::config::ProviderRuntimePolicy;
        use crate::runtime_state::ProviderState;

        let config = Arc::new(Config::default());
        let policy = ProviderRuntimePolicy::default();

        let mut old = Router::new(config.clone());
        // "model-a" exists in both routers -- state must be carried over.
        let old_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
        old.state.insert("model-a".to_string(), old_arc.clone());
        // "model-x" exists only in the old router -- must NOT be injected.
        let old_only_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
        old.state
            .insert("model-x".to_string(), old_only_arc.clone());

        let mut new = Router::new(config.clone());
        let fresh_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
        new.state.insert("model-a".to_string(), fresh_arc.clone());
        // "model-new" exists only in the new router -- must remain unchanged.
        let new_only_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
        new.state
            .insert("model-new".to_string(), new_only_arc.clone());

        // Act
        new.carry_over_runtime_state_from(&old);

        // Assert: "model-a" holds the old Arc, not the fresh one.
        let after_a = new.state.get("model-a").cloned().unwrap();
        assert!(
            Arc::ptr_eq(&after_a, &old_arc),
            "carry_over must reuse the old Arc for nicknames present in both routers",
        );

        // Assert: "model-new" (new-only) is unchanged.
        let after_new = new.state.get("model-new").cloned().unwrap();
        assert!(
            Arc::ptr_eq(&after_new, &new_only_arc),
            "carry_over must not replace entries absent from the old router",
        );

        // Assert: "model-x" (old-only) is NOT injected into the new router.
        assert!(
            !new.state.contains_key("model-x"),
            "carry_over must not inject old-only nicknames into the new router",
        );
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

    /// Minimal provider stub so the `apply_layered_overlays` fixture can
    /// build a real `Arc<ResolvedModel>` (which requires an
    /// `Arc<dyn Provider>`). None of its methods are called by
    /// `apply_layered_overlays`, which reads only the model's config
    /// overlays.
    struct StubProvider;

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &str {
            "stub"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

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

    #[test]
    fn apply_layered_overlays_records_operator_betas_excluding_client() {
        // Invariant: operator-configured betas (provider + model
        // header_extras) are recorded on `routectl_internal.operator_betas`
        // so the Anthropic-API egress can re-add them as a floor that
        // bypasses the per-provider `allowed_betas` allowlist. The
        // client/ingress betas (on `req.anthropic_beta`) MUST NOT leak
        // into that floor -- the allowlist still gates them.
        let mut config = Config::default();
        config.providers.insert(
            "test-prov".into(),
            crate::config::ProviderEntry::anthropic_api("literal:k")
                .with_header_extras(map(&[("anthropic-beta", "prov-beta")])),
        );

        let model: Arc<ResolvedModel> = Arc::new(
            ResolvedModel::new("nick", "test-prov", Arc::new(StubProvider), "claude-x")
                .with_header_extras(map(&[("anthropic-beta", "model-beta")])),
        );
        let target = into_one_dispatch_target(model);

        let mut req = req_with_betas(vec!["client-beta"]);
        apply_layered_overlays(&config, &target, &mut req);

        assert_eq!(
            req.routectl_internal.operator_betas,
            vec!["prov-beta".to_string(), "model-beta".to_string()],
            "operator_betas must hold the provider + model floor only",
        );
        assert!(
            !req.routectl_internal
                .operator_betas
                .iter()
                .any(|b| b == "client-beta"),
            "client/ingress betas must never enter the operator floor",
        );

        // `req.anthropic_beta` still carries the full union (client +
        // provider + model) so Bedrock's `filter_bedrock_betas` and the
        // log-safe summary see the complete set.
        for expected in ["client-beta", "prov-beta", "model-beta"] {
            assert!(
                req.anthropic_beta.iter().any(|b| b == expected),
                "req.anthropic_beta must carry the full union; missing {expected}",
            );
        }
    }

    /// Regression guard for the per-attempt overlay rebuild hazard:
    /// `apply_layered_overlays` reconstructs `routectl_internal` from
    /// `Default::default()` every dispatch attempt. Ingress-set provenance
    /// must survive that rebuild rather than reset to `Library`.
    #[test]
    fn apply_layered_overlays_preserves_ingress_provenance() {
        let config = Config::default();
        let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            Arc::new(StubProvider),
            "claude-x",
        ));
        let target = into_one_dispatch_target(model);

        let mut req = req_with_betas(vec![]);
        req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
        apply_layered_overlays(&config, &target, &mut req);

        assert_eq!(
            req.routectl_internal.provenance,
            routectl_core::RequestProvenance::AnthropicIngress,
            "ingress provenance must survive the per-attempt overlay rebuild",
        );
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
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            let model = req.model.clone();
            self.captured.lock().push(req);
            Ok(ChatResponse {
                id: "ok".into(),
                model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
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
                    upstream_meta: None,
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
                cache_capability: None,
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
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.captured.lock().unwrap().push(req);
            Ok(ChatResponse {
                id: "ok".into(),
                model: "m".into(),
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
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
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
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

    #[test]
    fn reported_model_survives_config_resolved_dispatch_relay() {
        // Structural-relay sanity check: a configured `reported_model`
        // rides the 4-hop relay (ModelEntry -> ResolvedModel ->
        // DispatchTarget) including the seat-pinned dispatch path used by
        // pooled-OAuth models. The end-to-end BEHAVIOR coverage (that the
        // override actually surfaces in resp.model) lives in
        // `seat_backed_complete_honors_reported_model_override`.
        let entry =
            crate::config::ModelEntry::new("p1", "wire-model").with_reported_model("public-label");
        assert_eq!(entry.reported_model.as_deref(), Some("public-label"));

        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p1".into(),
            calls: AtomicUsize::new(0),
        });
        let mut resolved = ResolvedModel::new("m1", "p1", p.clone(), "wire-model");
        if let Some(label) = entry.reported_model.as_ref() {
            resolved = resolved.with_reported_model(label.clone());
        }
        assert_eq!(resolved.reported_model.as_deref(), Some("public-label"));

        let m = Arc::new(resolved);
        let direct = into_one_dispatch_target(m.clone());
        assert_eq!(direct.reported_model.as_deref(), Some("public-label"));

        let seat = crate::seat_pool::SeatTarget {
            label: Some("seat-a".into()),
            state_key: "m1#seat-a".into(),
            provider: p.clone(),
            auth_secret_ref: None,
        };
        let via_seat = dispatch_target_for_seat(&m, &seat);
        assert_eq!(via_seat.reported_model.as_deref(), Some("public-label"));
    }

    #[test]
    fn visible_routectl_provider_survives_config_resolved_dispatch_relay() {
        // Structural-relay sanity check mirroring
        // `reported_model_survives_config_resolved_dispatch_relay`: a
        // configured `visible_routectl_provider=false` rides the 4-hop
        // relay (ModelEntry -> ResolvedModel -> DispatchTarget) including
        // the seat-pinned dispatch path. The end-to-end BEHAVIOR coverage
        // lives in `visible_routectl_provider_false_suppresses_field`.
        let entry = crate::config::ModelEntry::new("p1", "wire-model")
            .with_visible_routectl_provider(false);
        assert!(!entry.visible_routectl_provider);

        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p1".into(),
            calls: AtomicUsize::new(0),
        });
        let resolved = ResolvedModel::new("m1", "p1", p.clone(), "wire-model")
            .with_visible_routectl_provider(entry.visible_routectl_provider);
        assert!(!resolved.visible_routectl_provider);

        let m = Arc::new(resolved);
        let direct = into_one_dispatch_target(m.clone());
        assert!(!direct.visible_routectl_provider);

        let seat = crate::seat_pool::SeatTarget {
            label: Some("seat-a".into()),
            state_key: "m1#seat-a".into(),
            provider: p.clone(),
            auth_secret_ref: None,
        };
        let via_seat = dispatch_target_for_seat(&m, &seat);
        assert!(!via_seat.visible_routectl_provider);
    }

    #[test]
    fn visible_routectl_provider_defaults_true_across_relay() {
        // DEFAULT-TRUE guard: a model built without the override carries
        // `visible_routectl_provider=true` all the way to the dispatch
        // target, keeping existing consumers (which assert a present
        // `routectl_provider`) green.
        let entry = crate::config::ModelEntry::new("p1", "wire-model");
        assert!(entry.visible_routectl_provider);
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p1".into(),
            calls: AtomicUsize::new(0),
        });
        let m = Arc::new(ResolvedModel::new("m1", "p1", p, "wire-model"));
        assert!(m.visible_routectl_provider);
        assert!(into_one_dispatch_target(m).visible_routectl_provider);
    }

    #[tokio::test]
    async fn visible_routectl_provider_false_suppresses_field() {
        // SUPPRESS: a model with visible_routectl_provider=false yields a
        // response with NO `routectl_provider` (left None -> serde's
        // skip_serializing_if drops the field).
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "anthropic".into(),
            calls: AtomicUsize::new(0),
        });
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "haiku".to_string(),
            Arc::new(
                ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                    .with_visible_routectl_provider(false),
            ),
        );
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert!(
            resp.routectl_provider.is_none(),
            "suppressed model must leave routectl_provider unset"
        );
        // The skip_serializing_if drops the absent field from the wire.
        let body = serde_json::to_value(&resp).expect("serialize");
        assert!(
            body.get("routectl_provider").is_none(),
            "routectl_provider must be absent from the serialized body"
        );
    }

    #[tokio::test]
    async fn suppressed_provider_clears_prestamped_field() {
        // LEAK GUARD: concrete providers pre-stamp `routectl_provider`
        // with their own id before returning. CountedProvider returns
        // None and so cannot exercise the suppression gate's clearing
        // behavior. This provider returns Some("leaked-provider"); with
        // visible_routectl_provider=false the gate MUST clear it to None,
        // and the field MUST be absent from the serialized OpenAI body.
        struct PrestampProvider {
            id: String,
        }
        #[async_trait]
        impl Provider for PrestampProvider {
            fn id(&self) -> &str {
                &self.id
            }
            fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
                Err(Error::normalize_response(&self.id, "unused"))
            }
            async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
                Ok(ChatResponse {
                    id: format!("ok-{}", self.id),
                    model: req.model,
                    created: 0,
                    choices: vec![Choice {
                        logprobs: None,
                        index: 0,
                        message: Message {
                            refusal: None,
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
                    // Pre-stamp, mirroring every concrete provider.
                    routectl_provider: Some("leaked-provider".into()),
                    extras: Default::default(),
                    upstream_meta: None,
                })
            }
            async fn stream(
                &self,
                _: ChatRequest,
            ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
                unreachable!()
            }
        }

        let p: Arc<dyn Provider> = Arc::new(PrestampProvider {
            id: "anthropic".into(),
        });
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "haiku".to_string(),
            Arc::new(
                ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                    .with_visible_routectl_provider(false),
            ),
        );
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert!(
            resp.routectl_provider.is_none(),
            "suppression must clear the provider's pre-stamped routectl_provider"
        );
        let body = serde_json::to_value(&resp).expect("serialize");
        assert!(
            body.get("routectl_provider").is_none(),
            "pre-stamped routectl_provider must be absent from the serialized body"
        );
    }

    #[tokio::test]
    async fn suppressed_provider_still_records_dispatch_meta() {
        // ACCOUNTING GUARD: suppressing the client-visible field must NOT
        // affect internal accounting -- DispatchMeta still records
        // served_provider / served_upstream on the suppressed model.
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "anthropic".into(),
            calls: AtomicUsize::new(0),
        });
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "haiku".to_string(),
            Arc::new(
                ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                    .with_visible_routectl_provider(false),
            ),
        );
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            ..Default::default()
        };
        let dispatched = router
            .complete_with_options(req, RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(
            dispatched.meta.served_provider.as_deref(),
            Some("anthropic"),
            "served_provider must still be recorded when the field is suppressed"
        );
        assert_eq!(
            dispatched.meta.served_upstream.as_deref(),
            Some("claude-haiku-4-5"),
            "served_upstream must still be recorded when the field is suppressed"
        );
    }

    /// Minimal streaming-capable provider for the seat-path end-to-end
    /// tests. Emits a text chunk followed by a usage-only terminal tail
    /// chunk, mirroring a real provider; both carry the upstream wire id
    /// in `model`, so the router's per-chunk relabel (including the
    /// terminal chunk) is the only thing that can change it.
    struct StreamingProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for StreamingProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
            })
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            let model = req.model.clone();
            let id = self.id.clone();
            let text = ChatChunk {
                id: format!("chunk-{id}"),
                model: model.clone(),
                choices: vec![routectl_core::ChunkChoice {
                    index: 0,
                    delta: routectl_core::ChunkDelta {
                        content: Some("ok".into()),
                        ..Default::default()
                    },
                    finish_reason: None,
                    matched_stop_sequence: None,
                }],
                usage: None,
                opaque_events: Vec::new(),
                upstream_meta: None,
            };
            let tail = ChatChunk {
                id: format!("chunk-{id}-tail"),
                model,
                choices: Vec::new(),
                usage: Some(routectl_core::UsageDelta::default()),
                opaque_events: Vec::new(),
                upstream_meta: None,
            };
            Ok(futures::stream::iter(vec![Ok(text), Ok(tail)]).boxed())
        }
    }

    /// Build a router whose single model nickname is pooled onto a fixed
    /// set of seats. Mirrors the factory's seat-expansion path
    /// (`ResolvedModel::with_seats`) so dispatch walks the seat-pinned
    /// `DispatchTarget`s, the path used by pooled-OAuth models. An
    /// optional `reported_model` override is threaded onto the model.
    fn router_with_pooled_model(
        nickname: &str,
        provider_name: &str,
        upstream: &str,
        provider: Arc<dyn Provider>,
        seat_labels: &[&str],
        reported_model: Option<&str>,
    ) -> Router {
        let cfg = Arc::new(Config::default());
        let mut router = Router::new(cfg);

        let seats: Vec<crate::seat_pool::SeatTarget> = seat_labels
            .iter()
            .map(|label| crate::seat_pool::SeatTarget {
                label: Some((*label).to_string()),
                state_key: crate::seat_pool::seat_state_key(nickname, Some(label)),
                provider: provider.clone(),
                auth_secret_ref: None,
            })
            .collect();

        let mut resolved = ResolvedModel::new(nickname, provider_name, provider, upstream)
            .with_seats(seats.into());
        if let Some(label) = reported_model {
            resolved = resolved.with_reported_model(label);
        }

        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(nickname.to_string(), Arc::new(resolved));
        router.install_resolved_models(models);
        router
    }

    #[tokio::test]
    async fn seat_backed_complete_echoes_client_alias_by_default() {
        // A pooled (seat-backed) model with no `reported_model` override
        // must echo the client's requested alias in resp.model, even
        // though dispatch went through a seat-pinned DispatchTarget whose
        // upstream wire id differs from the alias.
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "oauth-pool".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_pooled_model(
            "opus",
            "anthropic-oauth",
            "claude-opus-4-7-wire",
            p.clone(),
            &["seat-a", "seat-b"],
            None,
        );
        let req = ChatRequest {
            model: "opus".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        // Default flip: the seat-served response echoes the requested
        // alias, not the upstream wire id.
        assert_eq!(resp.model, "opus");
    }

    #[tokio::test]
    async fn seat_backed_complete_honors_reported_model_override() {
        // A pooled model WITH a `reported_model` override must surface
        // that override in resp.model on the seat-served path.
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "oauth-pool".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_pooled_model(
            "opus",
            "anthropic-oauth",
            "claude-opus-4-7-wire",
            p.clone(),
            &["seat-a", "seat-b"],
            Some("public-opus"),
        );
        let req = ChatRequest {
            model: "opus".into(),
            messages: vec![],
            ..Default::default()
        };
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.model, "public-opus");
    }

    #[tokio::test]
    async fn seat_backed_stream_relabels_chunk_model() {
        // The seat-served streaming path must relabel every chunk.model
        // to the client-visible label. Default (no override) echoes the
        // requested alias.
        let p: Arc<dyn Provider> = Arc::new(StreamingProvider {
            id: "oauth-pool".into(),
        });
        let router = router_with_pooled_model(
            "opus",
            "anthropic-oauth",
            "claude-opus-4-7-wire",
            p.clone(),
            &["seat-a", "seat-b"],
            None,
        );
        let req = ChatRequest {
            model: "opus".into(),
            messages: vec![],
            ..Default::default()
        };
        let mut stream = router.stream(req).await.expect("stream opens");
        // Per-chunk relabel: every seat-served chunk, including the
        // usage-only terminal tail, carries the requested alias rather
        // than the upstream wire id the provider stamped.
        let mut count = 0;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("ok");
            assert_eq!(chunk.model, "opus");
            count += 1;
        }
        assert_eq!(count, 2, "text + terminal");
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
        // Default flip: the response echoes the client's requested
        // alias, not the upstream wire model id.
        assert_eq!(resp.model, "haiku");
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
                cache_capability: None,
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
        // Default flip: the response echoes the client's requested
        // alias (`foo`), not the served upstream wire model id.
        assert_eq!(resp.model, "foo");
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
        // Default flip: the response echoes the client's requested
        // wire model (`a`), not the resolved upstream id (`u-x`).
        assert_eq!(resp.model, "a");
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
mod gate_error_does_not_mask_real_error_tests {
    //! When the LAST chain entry is gate-refused (breaker open /
    //! RPM) but an EARLIER entry produced a real upstream error, the
    //! client must see the real error, not the synthetic "circuit
    //! breaker open" gate error. The fix keeps the first real error in
    //! `last_err` instead of overwriting it with the gate error.
    use super::*;
    use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
    use std::collections::BTreeMap;

    /// Provider that fails both complete + stream-open with a real,
    /// fallbackable 503 carrying a distinctive message.
    struct Real503Provider {
        id: String,
    }

    #[async_trait]
    impl Provider for Real503Provider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Err(Error::upstream(&self.id, 503, "real upstream down"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(Error::upstream(&self.id, 503, "real upstream down"))
        }
    }

    /// Provider for the second chain entry. Its breaker is force-opened
    /// before dispatch, so its body is never reached -- the gate refuses
    /// first.
    struct UnreachedProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for UnreachedProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!("gate must refuse entry2 before its body runs")
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("gate must refuse entry2 before its body runs")
        }
    }

    /// Build a router with a two-entry chain `flow = [entry1, entry2]`.
    /// entry1 fails 503; entry2 has a breaker and is force-opened so its
    /// gate refuses. Global retry is capped at one attempt so entry1
    /// fails fast without burning backoff sleeps.
    fn router_with_two_entry_chain() -> Router {
        let mut config = Config {
            retry: RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
            ..Config::default()
        };
        config.aliases.insert(
            "flow".into(),
            AliasValue::Chain(vec!["entry1".into(), "entry2".into()]),
        );
        config.providers.insert(
            "p2".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                runtime: ProviderRuntimePolicy {
                    circuit_failures: Some(1),
                    circuit_cooldown_ms: Some(60_000),
                    ..Default::default()
                },
            },
        );

        let mut router = Router::new(Arc::new(config));
        let p1: Arc<dyn Provider> = Arc::new(Real503Provider { id: "p1".into() });
        let p2: Arc<dyn Provider> = Arc::new(UnreachedProvider { id: "p2".into() });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "entry1".into(),
            Arc::new(ResolvedModel::new("entry1", "p1", p1, "u1")),
        );
        models.insert(
            "entry2".into(),
            Arc::new(ResolvedModel::new("entry2", "p2", p2, "u2")),
        );
        router.install_resolved_models(models);
        // Force entry2's breaker open so its gate refuses on dispatch.
        assert!(
            router.force_open_breaker("entry2", std::time::Duration::from_secs(3600)),
            "entry2 breaker must be force-open-able",
        );
        router
    }

    #[tokio::test]
    async fn complete_surfaces_real_error_not_gate_error() {
        let router = router_with_two_entry_chain();
        let req = ChatRequest {
            model: "flow".into(),
            messages: vec![],
            ..Default::default()
        };
        let err = router
            .complete(req)
            .await
            .expect_err("both entries unavailable -> Err");
        let msg = err.to_string();
        assert!(
            msg.contains("real upstream down"),
            "must surface entry1's real 503, got: {msg}"
        );
        assert!(
            !msg.contains("circuit breaker open"),
            "must NOT surface entry2's synthetic gate error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn stream_surfaces_real_error_not_gate_error() {
        let router = router_with_two_entry_chain();
        let req = ChatRequest {
            model: "flow".into(),
            messages: vec![],
            ..Default::default()
        };
        let err = router
            .stream(req)
            .await
            .err()
            .expect("both entries unavailable -> Err");
        let msg = err.to_string();
        assert!(
            msg.contains("real upstream down"),
            "stream must surface entry1's real 503, got: {msg}"
        );
        assert!(
            !msg.contains("circuit breaker open"),
            "stream must NOT surface entry2's synthetic gate error, got: {msg}"
        );
    }
}

#[cfg(test)]
mod seat_pool_dispatch_tests {
    //! Router-level tests for OAuth credential-pool dispatch: a pooled
    //! model expands into one DispatchTarget per seat, each with its own
    //! breaker entry, ordered by the provider's `seat_selection`.

    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy, SeatSelection};
    use crate::seat_pool::SeatTarget;
    use async_trait::async_trait;
    use routectl_core::{Choice, Message};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Provider that records each `complete` call against a shared
    /// counter so a test can assert which seat served a request.
    struct SeatProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for SeatProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    /// Build a pooled model `opus` on provider `anthropic` with three
    /// seats (default + seat-b + seat-c), each backed by its own
    /// `SeatProvider` + call counter. Returns the installed Router plus
    /// the three per-seat counters in seat order.
    fn pooled_router(selection: SeatSelection) -> (Router, Vec<Arc<AtomicUsize>>) {
        pooled_router_with_labels(
            selection,
            &[None, Some("seat-b".into()), Some("seat-c".into())],
        )
    }

    /// Build a pooled `opus` model with one seat per entry in `labels`
    /// (`None` is the default seat). Lets a test stand up pools of
    /// arbitrary seat sets -- e.g. a "before reload" two-seat pool and an
    /// "after reload" three-seat pool -- to exercise the coordinator's
    /// rebuild + per-state_key carry-over.
    fn pooled_router_with_labels(
        selection: SeatSelection,
        labels: &[Option<String>],
    ) -> (Router, Vec<Arc<AtomicUsize>>) {
        let mut counters = Vec::new();
        let mut seats: Vec<SeatTarget> = Vec::new();
        for label in labels {
            let counter = Arc::new(AtomicUsize::new(0));
            counters.push(counter.clone());
            let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
                id: format!("anthropic-{}", label.as_deref().unwrap_or("default")),
                calls: counter,
            });
            seats.push(SeatTarget {
                label: label.clone(),
                state_key: crate::seat_pool::seat_state_key("opus", label.as_deref()),
                provider,
                auth_secret_ref: None,
            });
        }
        let default_provider = seats[0].provider.clone();

        let mut providers = BTreeMap::new();
        let runtime = ProviderRuntimePolicy {
            seat_selection: selection,
            ..Default::default()
        };
        providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic").with_runtime(runtime),
        );
        let cfg = Arc::new(Config {
            providers,
            ..Config::default()
        });

        let mut router = Router::new(cfg);
        let model = ResolvedModel::new("opus", "anthropic", default_provider, "claude-opus-4-7")
            .with_seats(Arc::from(seats));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert("opus".to_string(), Arc::new(model));
        router.install_resolved_models(models);
        (router, counters)
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "opus".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    /// The seat order produced by `dispatch_chain` for `opus`, as a list
    /// of `state_key`s. Same-module access to the private method.
    fn chain_state_keys(router: &Router) -> Vec<String> {
        router
            .dispatch_chain("opus")
            .expect("chain resolves")
            .into_iter()
            .map(|t| t.state_key)
            .collect()
    }

    #[tokio::test]
    async fn each_seat_has_independent_breaker() {
        // Parking seat-a (force_open) must leave seat-b/seat-c
        // dispatchable -- the three seats own distinct state_key slots,
        // so there is no shared breaker.
        let (router, _counters) = pooled_router(SeatSelection::FillFirst);
        // All three seats own a state slot.
        assert!(router.state.contains_key("opus"));
        assert!(router.state.contains_key("opus#seat-b"));
        assert!(router.state.contains_key("opus#seat-c"));

        // Park the default seat for a long cooldown.
        router.park_provider("opus", Duration::from_secs(3600));

        // The default seat's breaker is open; siblings are untouched.
        assert!(
            router.gate_check("opus", "anthropic").is_some(),
            "parked default seat must gate-block"
        );
        assert!(
            router.gate_check("opus#seat-b", "anthropic").is_none(),
            "sibling seat-b must remain dispatchable"
        );
        assert!(
            router.gate_check("opus#seat-c", "anthropic").is_none(),
            "sibling seat-c must remain dispatchable"
        );
    }

    #[tokio::test]
    async fn fill_first_walks_seats_in_fixed_order() {
        // FillFirst: the chain's seat order is stable across requests
        // (default seat first, then sorted labels).
        let (router, _counters) = pooled_router(SeatSelection::FillFirst);
        let first = chain_state_keys(&router);
        let second = chain_state_keys(&router);
        assert_eq!(first, vec!["opus", "opus#seat-b", "opus#seat-c"]);
        assert_eq!(second, vec!["opus", "opus#seat-b", "opus#seat-c"]);
    }

    #[tokio::test]
    async fn round_robin_rotates_start_seat_per_request() {
        // RoundRobin: the starting seat advances by one per request and
        // wraps modulo the seat count.
        let (router, _counters) = pooled_router(SeatSelection::RoundRobin);
        assert_eq!(
            chain_state_keys(&router),
            vec!["opus", "opus#seat-b", "opus#seat-c"]
        );
        assert_eq!(
            chain_state_keys(&router),
            vec!["opus#seat-b", "opus#seat-c", "opus"]
        );
        assert_eq!(
            chain_state_keys(&router),
            vec!["opus#seat-c", "opus", "opus#seat-b"]
        );
        assert_eq!(
            chain_state_keys(&router),
            vec!["opus", "opus#seat-b", "opus#seat-c"]
        );
    }

    #[tokio::test]
    async fn parked_seat_is_skipped_and_sibling_serves() {
        // Full dispatch: park the default seat, then a request must fall
        // to the next seat (seat-b) and that seat's provider serves.
        let (router, counters) = pooled_router(SeatSelection::FillFirst);
        router.park_provider("opus", Duration::from_secs(3600));

        let resp = router.complete(req()).await.expect("sibling serves");
        assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            0,
            "parked default seat must not be hit"
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            1,
            "seat-b must serve the request"
        );
        assert_eq!(
            counters[2].load(Ordering::SeqCst),
            0,
            "seat-c must not be reached once seat-b succeeds"
        );
    }

    #[tokio::test]
    async fn fill_first_serves_default_seat_first() {
        // Sanity: with no seat parked, FillFirst serves the default seat.
        let (router, counters) = pooled_router(SeatSelection::FillFirst);
        let resp = router.complete(req()).await.expect("default serves");
        assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 0);
        assert_eq!(counters[2].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn carry_over_preserves_surviving_seat_breaker_and_starts_new_seat_fresh() {
        // Simulate the credentials-reload rebuild: a two-seat pool
        // (default + seat-b) trips the default seat's breaker, then a seat
        // is added on disk so the coordinator rebuilds a THREE-seat pool
        // (default + seat-b + seat-c) and carries over per-state_key
        // runtime state. The surviving seat's tripped breaker must persist
        // (carry-over by state_key); the freshly-added seat must start
        // closed.
        let (before, _c1) =
            pooled_router_with_labels(SeatSelection::FillFirst, &[None, Some("seat-b".into())]);
        // Trip the default seat's breaker for a long cooldown.
        assert!(
            before.force_open_breaker("opus", Duration::from_secs(3600)),
            "default seat must own a state slot to trip"
        );
        assert_eq!(
            before.breaker_open_for("opus"),
            Some(true),
            "default seat breaker must read open after force_open"
        );

        // Rebuild with the added seat-c, then carry over from `before`.
        let (mut after, _c2) = pooled_router_with_labels(
            SeatSelection::FillFirst,
            &[None, Some("seat-b".into()), Some("seat-c".into())],
        );
        after.carry_over_runtime_state_from(&before);

        // The surviving default seat's tripped breaker carried over.
        assert_eq!(
            after.breaker_open_for("opus"),
            Some(true),
            "surviving seat's breaker state must survive the rebuild"
        );
        // The freshly-added seat-c starts closed (fresh state).
        assert_eq!(
            after.breaker_open_for("opus#seat-c"),
            Some(false),
            "newly-added seat must start with a fresh, closed breaker"
        );
        // And the pool re-expanded to three seats.
        assert_eq!(after.seat_count_for("opus"), Some(3));
    }
}

#[cfg(test)]
mod count_tokens_tests {
    //! Pin: `Router::count_tokens` walks PAST count_tokens-incapable
    //! targets (provider_kind != "anthropic-api") to the first capable
    //! one, returning 501 NotImplemented only when NO target in the
    //! chain is capable. The capability skip is keyed statically on
    //! provider kind BEFORE dispatch -- a kind-skip is operator-known,
    //! not upstream health -- so it never touches the breaker. A
    //! CAPABLE target that returns a real upstream error propagates as
    //! today (no further walk). All anthropic-api targets share the
    //! same Anthropic tokenizer family, so walking past incapable kinds
    //! does NOT reintroduce the wrong-tokenizer hazard.
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
    use routectl_providers::anthropic_api::AuthKind;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How a mock provider's `count_tokens` should respond once it is
    /// actually selected and dispatched to.
    #[derive(Clone, Copy)]
    enum CountBehavior {
        /// Return `Ok(TokenCount { input_tokens })`.
        Ok(u32),
        /// Return `Error::NotImplemented` (the trait-default shape).
        NotImplemented,
        /// Return `Error::Upstream { status, .. }` (a real upstream
        /// error from a capable provider).
        UpstreamError(u16),
    }

    /// Mock provider that records every `count_tokens` call so a test
    /// can prove a target was (or was NOT) dispatched to. Its kind in
    /// the capability walk is decided by the matching `ProviderEntry`
    /// in config, NOT by this impl -- so a Bedrock-kind entry skips the
    /// walk regardless of what this returns.
    struct CountingProvider {
        id: String,
        calls: Arc<AtomicUsize>,
        behavior: CountBehavior,
    }

    #[async_trait]
    impl Provider for CountingProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                CountBehavior::Ok(n) => Ok(TokenCount {
                    input_tokens: n,
                    extras: Default::default(),
                }),
                CountBehavior::NotImplemented => Err(Error::NotImplemented(
                    self.id.clone(),
                    "count_tokens".into(),
                )),
                CountBehavior::UpstreamError(status) => {
                    Err(Error::upstream(self.id.clone(), status, "boom"))
                }
            }
        }
    }

    /// A count_tokens-capable provider entry (kind == "anthropic-api").
    fn anthropic_api_entry() -> ProviderEntry {
        ProviderEntry::AnthropicApi {
            api_key_ref: "literal:k".into(),
            base_url: "https://placeholder.invalid".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: vec![],
            forward_client_headers: vec![],
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// A count_tokens-incapable provider entry (kind == "openai-compat").
    /// Always compiled, regardless of the `bedrock` feature.
    fn openai_compat_entry() -> ProviderEntry {
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// A count_tokens-incapable provider entry (kind == "bedrock").
    /// Mirrors the motivating scenario from the spec. Bedrock has no
    /// count_tokens endpoint, so its kind is skipped before dispatch.
    #[cfg(feature = "bedrock")]
    fn bedrock_entry() -> ProviderEntry {
        use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
        ProviderEntry::Bedrock {
            region: "us-east-1".into(),
            api_shape: BedrockApiShapeConfig::default(),
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: vec![],
            cache_capability: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// One leg of a test chain: a provider entry + the matching mock
    /// provider behavior.
    struct Leg {
        nickname: &'static str,
        provider_name: &'static str,
        entry: ProviderEntry,
        behavior: CountBehavior,
    }

    /// Build a router whose alias `"alias"` resolves to the given legs
    /// in order. Returns the router and the per-leg call counters (same
    /// order as `legs`).
    fn build_router(legs: Vec<Leg>) -> (Router, Vec<Arc<AtomicUsize>>) {
        let mut config = Config::default();
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let mut counters: Vec<Arc<AtomicUsize>> = Vec::with_capacity(legs.len());
        let mut chain: Vec<String> = Vec::with_capacity(legs.len());

        for leg in legs {
            config
                .providers
                .insert(leg.provider_name.to_string(), leg.entry);
            let calls = Arc::new(AtomicUsize::new(0));
            counters.push(calls.clone());
            let provider: Arc<dyn Provider> = Arc::new(CountingProvider {
                id: leg.provider_name.to_string(),
                calls,
                behavior: leg.behavior,
            });
            models.insert(
                leg.nickname.to_string(),
                Arc::new(ResolvedModel::new(
                    leg.nickname,
                    leg.provider_name,
                    provider,
                    format!("upstream-{}", leg.nickname),
                )),
            );
            chain.push(leg.nickname.to_string());
        }

        config
            .aliases
            .insert("alias".into(), AliasValue::Chain(chain));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(models);
        (router, counters)
    }

    fn count_req() -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            ..Default::default()
        }
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn walks_past_incapable_bedrock_to_capable_anthropic() {
        // Arrange: chain [bedrock, anthropic-api]. Bedrock is not
        // count_tokens-capable and must be skipped BEFORE dispatch
        // (no call, no breaker account); the anthropic-api target
        // serves the count.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "bedrock-haiku",
                provider_name: "bedrock-prov",
                entry: bedrock_entry(),
                behavior: CountBehavior::Ok(99),
            },
            Leg {
                nickname: "anthropic-haiku",
                provider_name: "anthropic-prov",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(42),
            },
        ]);

        // Act
        let tc = router
            .count_tokens(count_req())
            .await
            .expect("capable target serves the count");

        // Assert: anthropic-api served (42), bedrock never called.
        assert_eq!(tc.input_tokens, 42);
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            0,
            "incapable bedrock target must be skipped, not dispatched",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            1,
            "capable anthropic-api target must serve the count",
        );
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn all_incapable_chain_returns_not_implemented() {
        // Arrange: chain [bedrock] only -- no capable target anywhere.
        let (router, counters) = build_router(vec![Leg {
            nickname: "bedrock-haiku",
            provider_name: "bedrock-prov",
            entry: bedrock_entry(),
            behavior: CountBehavior::Ok(7),
        }]);

        // Act
        let err = router.count_tokens(count_req()).await.unwrap_err();

        // Assert: terminal 501, provider never touched.
        match err {
            Error::NotImplemented(model, msg) => {
                assert_eq!(model, "alias");
                assert!(
                    msg.contains("count_tokens"),
                    "message must name the operation; got: {msg}",
                );
            }
            other => panic!("expected Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            0,
            "no capable target -> nothing dispatched",
        );
    }

    #[tokio::test]
    async fn all_incapable_openai_compat_chain_returns_not_implemented() {
        // Feature-independent twin of the bedrock-only case: a single
        // openai-compat leg is also count_tokens-incapable.
        let (router, counters) = build_router(vec![Leg {
            nickname: "compat-model",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
            behavior: CountBehavior::Ok(7),
        }]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        match err {
            Error::NotImplemented(model, msg) => {
                assert_eq!(model, "alias");
                assert!(msg.contains("count_tokens"), "got: {msg}");
            }
            other => panic!("expected Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(counters[0].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn walks_past_incapable_openai_compat_to_capable_anthropic() {
        // Feature-independent walk: chain [openai-compat, anthropic-api].
        // The openai-compat leg cannot count_tokens and must be skipped
        // BEFORE dispatch; the anthropic-api leg serves. Pins the
        // skip-then-advance path on builds without the `bedrock` feature
        // (the bedrock-gated twin only runs with that feature compiled in).
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "compat-model",
                provider_name: "compat-prov",
                entry: openai_compat_entry(),
                behavior: CountBehavior::Ok(99),
            },
            Leg {
                nickname: "anthropic-haiku",
                provider_name: "anthropic-prov",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(42),
            },
        ]);

        // Act
        let tc = router
            .count_tokens(count_req())
            .await
            .expect("capable target serves the count");

        // Assert: anthropic-api served (42), openai-compat never called.
        assert_eq!(tc.input_tokens, 42);
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            0,
            "incapable openai-compat target must be skipped, not dispatched",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            1,
            "capable anthropic-api target must serve the count",
        );
    }

    #[tokio::test]
    async fn capable_primary_serves_unchanged() {
        // Arrange: chain [anthropic-api, anthropic-api]. The capable
        // primary serves; the second leg is never reached.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-primary",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(11),
            },
            Leg {
                nickname: "anthropic-secondary",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(22),
            },
        ]);

        let tc = router
            .count_tokens(count_req())
            .await
            .expect("primary serves");

        assert_eq!(tc.input_tokens, 11, "first capable target serves");
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "second target must not be reached when primary is capable",
        );
    }

    #[tokio::test]
    async fn capable_target_upstream_error_propagates_without_walking() {
        // Arrange: chain [anthropic-api(500), anthropic-api(ok)]. The
        // selected capable target returns a real upstream error; it
        // MUST propagate and MUST NOT walk to the later capable entry
        // (try-and-fallback is reserved for the messages path -- a
        // kind-skip is operator-known, an upstream error is not).
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-primary",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::UpstreamError(500),
            },
            Leg {
                nickname: "anthropic-secondary",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(22),
            },
        ]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        assert!(
            matches!(err, Error::Upstream { status: 500, .. }),
            "upstream error must propagate; got {err:?}",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "primary attempted once"
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "must NOT walk to a later target on a real upstream error",
        );
    }

    #[tokio::test]
    async fn capable_target_not_implemented_propagates_without_retry() {
        // A capable (anthropic-api) target that itself returns
        // NotImplemented propagates verbatim with no retry. (Real
        // Anthropic does not do this; the test pins the no-retry
        // contract for the selected capable target.)
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::NotImplemented,
        }]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        assert!(
            matches!(err, Error::NotImplemented(_, _)),
            "expected Error::NotImplemented, got {err:?}",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "selected capable target is dispatched once, no retry",
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
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            let model = req.model.clone();
            let id = self.id.clone();
            self.captured.lock().push(req);
            Ok(ChatResponse {
                id: format!("ok-{id}"),
                model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
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
                upstream_meta: None,
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
                cache_capability: None,
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
                cache_capability: None,
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
                        logprobs: None,
                        index: 0,
                        message: Message {
                            refusal: None,
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
                    upstream_meta: None,
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
                cache_capability: None,
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

#[cfg(test)]
mod circuit_breaker_slot_release_tests {
    //! Regression: a half-open probe that fast-fails on 429/529 must
    //! release the slot it claimed at the gate. Before the fix the
    //! probe-fast-fail early-return skipped record_success/record_failure,
    //! leaving `half_open_in_flight = true` forever -- every later gate
    //! check returned CircuitOpen and the breaker was permanently locked
    //! open for that provider until process restart.
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Provider that counts `complete()` calls and always 429s, so the
    /// test can distinguish "gate granted a probe and reached the
    /// upstream" (call count rises) from "gate returned CircuitOpen and
    /// skipped the upstream" (call count flat).
    struct Probe429Provider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Probe429Provider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 429, "rate limited"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("not exercised by these tests")
        }
    }

    /// Provider that counts `complete()` calls and always fails with a
    /// configurable status + reset hint, so the reset-honoring tests can
    /// drive the park / in-loop-retry decision and assert the resulting
    /// breaker state. `status` shapes fallbackability (429 fallbackable,
    /// 400 not); `retry_after` is the reset hint threaded through
    /// `Error::Upstream.retry_after`.
    struct RetryAfterProvider {
        id: String,
        status: u16,
        retry_after: Option<Duration>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for RetryAfterProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream_with_retry_after(
                &self.id,
                self.status,
                "rate limited",
                self.retry_after,
            ))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("not exercised by these tests")
        }
    }

    /// Like `build_router_with_provider_and_retry` but lets the test pin
    /// the breaker threshold + cooldown so a single recorded failure does
    /// NOT necessarily trip the breaker. Used by the reset-honoring tests
    /// that must distinguish a force-park (breaker open immediately) from
    /// a sub-threshold `record_failure` (breaker still closed).
    fn build_router_with_breaker(
        provider: Arc<dyn Provider>,
        retry: RetryPolicy,
        circuit_failures: u32,
        circuit_cooldown_ms: u64,
    ) -> Router {
        let mut config = Config {
            retry,
            ..Default::default()
        };
        config.providers.insert(
            "p".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                runtime: ProviderRuntimePolicy {
                    circuit_failures: Some(circuit_failures),
                    circuit_cooldown_ms: Some(circuit_cooldown_ms),
                    ..Default::default()
                },
            },
        );
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m".into(),
            Arc::new(ResolvedModel::new("m", "p", provider, "u")),
        );
        router.install_resolved_models(models);
        router
    }

    /// True when the per-model breaker would refuse a dispatch at `now`
    /// (CircuitOpen). The reset-honoring tests use this to assert a park
    /// happened (open) or did not (allow).
    fn breaker_open_at(router: &Router, now: Instant) -> bool {
        let st = router.state.get("m").expect("per-model state slot exists");
        st.lock().try_dispatch(now) == GateDecision::CircuitOpen
    }

    /// Build a single-entry-chain Router around `provider` with a
    /// threshold-1, zero-cooldown breaker. Zero cooldown: the breaker is
    /// immediately half-open-eligible on the next dispatch, so the tests
    /// need no wall-clock sleep to "advance past cooldown".
    fn build_router_with_provider(provider: Arc<dyn Provider>) -> Router {
        build_router_with_provider_and_retry(provider, RetryPolicy::default())
    }

    /// Like `build_router_with_provider` but lets the test pin the
    /// top-level `[retry]` policy (`policy_for` returns `config.retry`).
    fn build_router_with_provider_and_retry(
        provider: Arc<dyn Provider>,
        retry: RetryPolicy,
    ) -> Router {
        let mut config = Config {
            retry,
            ..Default::default()
        };
        // anthropic-api kind so `count_tokens` treats the "p" target as
        // count_tokens-capable (the capability walk keys on
        // provider_kind == "anthropic-api"). The kind is irrelevant to
        // the complete/stream breaker tests that also use this helper;
        // they exercise the half-open probe-slot release, not the
        // count_tokens capability gate.
        config.providers.insert(
            "p".into(),
            ProviderEntry::AnthropicApi {
                api_key_ref: "literal:k".into(),
                base_url: "https://placeholder.invalid".into(),
                anthropic_version: "2023-06-01".into(),
                auth_kind: routectl_providers::anthropic_api::AuthKind::default(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                allowed_betas: vec![],
                forward_client_headers: vec![],
                context_management: false,
                max_thinking_entry_bytes: None,
                cache_capability: None,
                runtime: ProviderRuntimePolicy {
                    circuit_failures: Some(1),
                    circuit_cooldown_ms: Some(0),
                    ..Default::default()
                },
            },
        );
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m".into(),
            Arc::new(ResolvedModel::new("m", "p", provider, "u")),
        );
        router.install_resolved_models(models);
        router
    }

    fn build_router(calls: Arc<AtomicUsize>) -> Router {
        let provider: Arc<dyn Provider> = Arc::new(Probe429Provider {
            id: "p".into(),
            calls,
        });
        build_router_with_provider(provider)
    }

    fn probe_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![],
            // max_tokens <= probe_max_tokens (default 1) => probe-shaped.
            max_tokens: Some(1),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn probe_fast_fail_does_not_permanently_lock_breaker() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = build_router(calls.clone());

        // Trip the breaker directly (threshold = 1 failure).
        {
            let st = router.state.get("m").expect("per-model state slot exists");
            st.lock().record_failure(Instant::now());
        }

        // First probe after the trip: the breaker is half-open, the gate
        // grants the single probe, the upstream 429s, and the probe
        // fast-fail releases the slot.
        let _ = router.complete(probe_req()).await.unwrap_err();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first probe must reach the upstream (gate granted the half-open probe)",
        );

        // Second probe: if the slot had leaked, the gate would return
        // CircuitOpen and the upstream would NOT be touched. With the
        // slot released, the gate grants a fresh probe and the upstream
        // is reached again.
        let _ = router.complete(probe_req()).await.unwrap_err();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "second probe must also reach the upstream; a leaked half-open \
             slot would have locked the breaker (CircuitOpen) and skipped it",
        );
    }

    /// Streaming provider whose first chunk always arrives, after which
    /// the stream either completes cleanly (`mid_stream_error = false`)
    /// or yields one error frame (`mid_stream_error = true`). Lets the
    /// first-chunk-close tests separate the call-site close (on the
    /// first chunk) from the wrap's mid-stream accounting.
    struct FirstChunkProvider {
        id: String,
        mid_stream_error: bool,
        stream_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for FirstChunkProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!("not exercised by these tests")
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            let id = self.id.clone();
            let first = ChatChunk {
                id: "c0".into(),
                ..Default::default()
            };
            if self.mid_stream_error {
                let err = Error::upstream(&id, 503, "mid-stream boom");
                let s = futures::stream::iter(vec![Ok(first), Err(err)]);
                Ok(s.boxed())
            } else {
                let second = ChatChunk {
                    id: "c1".into(),
                    ..Default::default()
                };
                let s = futures::stream::iter(vec![Ok(first), Ok(second)]);
                Ok(s.boxed())
            }
        }
    }

    fn build_first_chunk_router(mid_stream_error: bool) -> (Router, Arc<AtomicUsize>) {
        let stream_calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(FirstChunkProvider {
            id: "p".into(),
            mid_stream_error,
            stream_calls: stream_calls.clone(),
        });
        // Threshold 1 with a long baseline cooldown: an OPEN breaker
        // reads CircuitOpen (so the re-trip / stays-closed assertions are
        // observable), while `force_open_breaker(.., ZERO)` still makes
        // the next dispatch the half-open probe. A re-trip via
        // record_failure restores the long baseline cooldown.
        let router = build_router_with_breaker(provider, RetryPolicy::default(), 1, 60_000);
        (router, stream_calls)
    }

    /// Put the model's breaker into the half-open state for the next
    /// dispatch: open it with a zero-length park so the cooldown is
    /// already elapsed and `try_dispatch` claims the single probe slot.
    fn arm_half_open(router: &Router) {
        assert!(
            router.force_open_breaker("m", Duration::ZERO),
            "model breaker slot must exist",
        );
    }

    /// A half-open probe stream that succeeds on its first
    /// chunk closes the breaker BEFORE the stream is fully consumed.
    /// Before the fix the probe slot was held for the entire stream
    /// duration, locking out concurrent requests.
    #[tokio::test]
    async fn first_chunk_success_closes_breaker_before_stream_consumed() {
        let (router, _calls) = build_first_chunk_router(false);
        arm_half_open(&router);

        let stream = router
            .stream(plain_req())
            .await
            .expect("first-chunk arrives -> Ok stream");

        // The returned stream is UNPOLLED here (not yet consumed). With
        // the first-chunk-close fix the breaker is already closed: the
        // half-open slot is released and the circuit is no longer open.
        assert!(
            !slot_in_flight(&router),
            "half-open probe slot must be released on first-chunk success, \
             not held for the whole stream",
        );
        // A closed breaker grants the next dispatch immediately.
        assert!(
            !breaker_open_at(&router, Instant::now()),
            "breaker must read CLOSED after first-chunk probe success",
        );

        drop(stream);
    }

    /// After the first-chunk close, N=threshold mid-stream
    /// error frames re-trip the breaker (the wrap still records the
    /// mid-stream failure via record_failure).
    #[tokio::test]
    async fn mid_stream_error_after_first_chunk_close_retrips_breaker() {
        let (router, _calls) = build_first_chunk_router(true);
        arm_half_open(&router);

        let stream = router
            .stream(plain_req())
            .await
            .expect("first-chunk arrives -> Ok stream");

        // Drain the stream: first chunk Ok (already closed the breaker),
        // then one error frame (threshold = 1) re-trips it.
        use futures::stream::StreamExt as _;
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 2, "first chunk + one error frame");
        assert!(items[0].is_ok(), "first frame is the success chunk");
        assert!(items[1].is_err(), "second frame is the mid-stream error");

        // The mid-stream error re-tripped the breaker (baseline cooldown
        // restored): the next dispatch is refused.
        assert!(
            breaker_open_at(&router, Instant::now()),
            "a mid-stream error after first-chunk close must re-trip the breaker",
        );
    }

    /// Consumer cancellation (dropping the stream) AFTER a
    /// first-chunk probe success must NOT re-trip the breaker. Proves the
    /// `cancel_is_failure` removal is safe: the breaker was already
    /// closed at the call site, so a cancel is irrelevant to the probe.
    #[tokio::test]
    async fn cancel_after_first_chunk_success_does_not_retrip_breaker() {
        let (router, _calls) = build_first_chunk_router(false);
        arm_half_open(&router);

        let stream = router
            .stream(plain_req())
            .await
            .expect("first-chunk arrives -> Ok stream");

        // Cancel by dropping the unconsumed stream.
        drop(stream);

        // The breaker stays CLOSED: the first-chunk success already
        // closed it, and the drop's benign record_success cannot reopen.
        assert!(
            !slot_in_flight(&router),
            "cancel after first-chunk success must leave the slot released",
        );
        assert!(
            !breaker_open_at(&router, Instant::now()),
            "cancel after first-chunk success must NOT re-trip the breaker",
        );
    }

    /// Multi-surface mock for the half-open-probe-gets-401-then-refresh-
    /// succeeds path the slot-release fix targets. Each of `complete`,
    /// `stream`, and `count_tokens` returns `Error::Upstream { status:
    /// 401, .. }` on its FIRST call and a success on every subsequent call
    /// (independent per-surface counters). `on_auth_failure` always
    /// succeeds and bumps its own counter.
    struct Recovering401MultiProvider {
        id: String,
        complete_calls: Arc<AtomicUsize>,
        stream_calls: Arc<AtomicUsize>,
        count_tokens_calls: Arc<AtomicUsize>,
        on_auth_failure_calls: Arc<AtomicUsize>,
        /// When true, `on_auth_failure` returns `Error::Auth` instead of
        /// `Ok(())` -- the dead-OAuth-identity path.
        refresh_fails: bool,
    }

    #[async_trait]
    impl Provider for Recovering401MultiProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            let n = self.complete_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Err(Error::upstream(&self.id, 401, "stale token"));
            }
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![routectl_core::Choice {
                    logprobs: None,
                    index: 0,
                    message: routectl_core::Message {
                        refusal: None,
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
                upstream_meta: None,
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            let n = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Err(Error::upstream(&self.id, 401, "stale token"));
            }
            let chunk = ChatChunk {
                id: format!("ok-{}", self.id),
                ..Default::default()
            };
            Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            let n = self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Err(Error::upstream(&self.id, 401, "stale token"));
            }
            Ok(TokenCount {
                input_tokens: 7,
                ..Default::default()
            })
        }
        async fn on_auth_failure(&self) -> Result<()> {
            self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
            if self.refresh_fails {
                Err(Error::Auth("oauth refresh failed; re-run login".into()))
            } else {
                Ok(())
            }
        }
    }

    fn build_recovering_router() -> (Router, Arc<Recovering401MultiProvider>) {
        build_recovering_router_inner(false)
    }

    fn build_recovering_router_inner(
        refresh_fails: bool,
    ) -> (Router, Arc<Recovering401MultiProvider>) {
        let provider = Arc::new(Recovering401MultiProvider {
            id: "p".into(),
            complete_calls: Arc::new(AtomicUsize::new(0)),
            stream_calls: Arc::new(AtomicUsize::new(0)),
            count_tokens_calls: Arc::new(AtomicUsize::new(0)),
            on_auth_failure_calls: Arc::new(AtomicUsize::new(0)),
            refresh_fails,
        });
        let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);
        (router, provider)
    }

    /// A plain (non-max_tokens-probe) request so `is_probe` is false and
    /// the only "probe" in play is the breaker's half-open probe slot.
    fn plain_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    /// Trip the threshold-1 breaker directly so the next dispatch is
    /// half-open (zero cooldown).
    fn trip_breaker(router: &Router) {
        let st = router.state.get("m").expect("per-model state slot exists");
        st.lock().record_failure(Instant::now());
    }

    fn slot_in_flight(router: &Router) -> bool {
        let st = router.state.get("m").expect("per-model state slot exists");
        st.lock().half_open_probe_in_flight()
    }

    #[tokio::test]
    async fn complete_half_open_401_refresh_releases_slot() {
        // FAILS without the slot-release fix: the half-open probe 401s,
        // the refresh succeeds, and the Ok-path `continue` re-gates while
        // this caller still holds the slot -> CircuitOpen -> single-entry
        // chain exhausts -> Err with the slot stuck `true` forever. With
        // the fix the slot is released first, the re-gate claims a fresh
        // slot, the retry reaches the upstream and succeeds, and the probe
        // success closes the breaker.
        let (router, provider) = build_recovering_router();
        trip_breaker(&router);

        let resp = router
            .complete(plain_req())
            .await
            .expect("half-open 401 -> refresh -> retry must land on the success branch");
        assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            2,
            "complete must run twice: the 401 probe and the post-refresh retry",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires exactly once (the single 401 -> refresh)",
        );
        assert!(
            !slot_in_flight(&router),
            "half-open slot must be cleared after the recovered probe",
        );
    }

    #[tokio::test]
    async fn count_tokens_half_open_401_refresh_does_not_lock_breaker() {
        // FAILS without the fix: count_tokens propagates the re-gate's
        // CircuitOpen as its gate error and the slot stays `true` forever,
        // so neither this dispatch nor any later one reaches the upstream.
        let (router, provider) = build_recovering_router();
        trip_breaker(&router);

        let first = router.count_tokens(plain_req()).await;
        let calls_after_first = provider.count_tokens_calls.load(Ordering::SeqCst);

        // A leaked half-open slot would have locked the breaker; the
        // second dispatch must still reach the upstream.
        let second = router.count_tokens(plain_req()).await;
        let calls_after_second = provider.count_tokens_calls.load(Ordering::SeqCst);

        assert!(
            first.is_ok(),
            "first count_tokens must recover via refresh+retry, got: {first:?}",
        );
        assert!(
            second.is_ok(),
            "second count_tokens must not hit a permanently-locked breaker, got: {second:?}",
        );
        assert!(
            calls_after_second > calls_after_first,
            "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires exactly once (the single 401 -> refresh)",
        );
        assert!(
            !slot_in_flight(&router),
            "half-open slot must be released, not stuck open",
        );
    }

    #[tokio::test]
    async fn stream_half_open_401_refresh_does_not_lock_breaker() {
        // FAILS without the fix: provider.stream() 401s pre-first-chunk,
        // the refresh succeeds, the Ok-path `continue` re-gates while this
        // caller still holds the slot -> CircuitOpen -> single-entry chain
        // exhausts -> Err with the slot stuck `true` forever.
        let (router, provider) = build_recovering_router();
        trip_breaker(&router);

        let first = router.stream(plain_req()).await;
        let first_is_ok = first.is_ok();
        // Drain the recovered stream to completion so the half-open probe's
        // breaker accounting records success and closes the breaker. (When
        // the fix is absent `first` is the CircuitOpen Err -- nothing to
        // drain.)
        if let Ok(mut s) = first {
            while s.next().await.is_some() {}
        }
        let calls_after_first = provider.stream_calls.load(Ordering::SeqCst);

        let second = router.stream(plain_req()).await;
        let second_is_ok = second.is_ok();
        if let Ok(mut s) = second {
            while s.next().await.is_some() {}
        }
        let calls_after_second = provider.stream_calls.load(Ordering::SeqCst);

        assert!(
            first_is_ok,
            "first stream must recover via refresh+retry, not fail with CircuitOpen",
        );
        assert!(
            second_is_ok,
            "second stream must not hit a permanently-locked breaker",
        );
        assert!(
            calls_after_second > calls_after_first,
            "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires exactly once (the single 401 -> refresh)",
        );
        assert!(
            !slot_in_flight(&router),
            "half-open slot must be released, not stuck open",
        );
    }

    #[tokio::test]
    async fn complete_half_open_non_fallbackable_429_does_not_lock_breaker() {
        // Regression: a NON-probe request hits a half-open provider that
        // returns 429 under a policy that excludes 429 from fallback
        // (`retry_allowlist=[500]`). Because `retries_for_status` honors
        // the fallback predicate, an excluded 429 is also non-retryable
        // (`retries_for_status(429)=0`) even with `retry_on_429` set --
        // exclusion wins. So the dispatch takes the terminal
        // non-fallbackable path, which must release the half-open probe
        // slot before returning. A leaked slot would leave the breaker
        // stuck CircuitOpen forever; the second dispatch must still reach
        // the upstream. (The release at the `can_retry_here && !do_fallback`
        // site is now defense-in-depth -- unreachable while every retryable
        // status is also fallbackable.)
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(Probe429Provider {
            id: "p".into(),
            calls: calls.clone(),
        });
        // retry_allowlist=[500] excludes 429: do_fallback=false AND
        // retries_for_status(429)=0 (exclusion wins over retry_on_429), so
        // the attempt is neither retried nor fallen back -- it hits the
        // terminal non-fallbackable release. Zero backoff/jitter keep the
        // test instant.
        let retry = RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            backoff_multiplier: 1.0,
            jitter_ms: 0,
            retry_allowlist: vec![500],
            retry_on_429: Some(2),
            ..RetryPolicy::default()
        };
        let router = build_router_with_provider_and_retry(provider, retry);
        trip_breaker(&router);

        // Non-probe: max_tokens above the probe threshold (default 1), so
        // is_probe=false and the probe-fast-fail path is not taken.
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![],
            max_tokens: Some(1024),
            ..Default::default()
        };

        let first = router.complete(req.clone()).await;
        let calls_after_first = calls.load(Ordering::SeqCst);

        // A leaked half-open slot would have locked the breaker; the second
        // dispatch must still reach the upstream.
        let second = router.complete(req.clone()).await;
        let calls_after_second = calls.load(Ordering::SeqCst);

        assert!(
            calls_after_second > calls_after_first,
            "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
        );
        // Both dispatches must terminate in the upstream 429, never the
        // gate's status-0 "circuit breaker open" error.
        for (label, r) in [("first", &first), ("second", &second)] {
            match r {
                Err(Error::Upstream { status, .. }) => assert_eq!(
                    *status, 429,
                    "{label} dispatch must surface the upstream 429, not the \
                     gate circuit-breaker error (status 0)",
                ),
                other => panic!("{label} dispatch expected Err(Upstream 429), got: {other:?}"),
            }
        }
        assert!(
            !slot_in_flight(&router),
            "half-open slot must be released after the retry-without-fallback path",
        );
    }

    #[tokio::test]
    async fn complete_half_open_401_refresh_failure_releases_slot() {
        // Coverage for the auth-refresh-FAILURE release path: a half-open
        // probe gets a 401, `on_auth_failure()` returns Err (dead OAuth
        // identity), and the router must release the half-open slot before
        // surfacing the error. If it did not, the breaker would be locked
        // open forever; here we assert the slot is freed and a later
        // dispatch can still probe.
        let (router, provider) = build_recovering_router_inner(true);
        trip_breaker(&router);

        let first = router.complete(plain_req()).await;
        let calls_after_first = provider.complete_calls.load(Ordering::SeqCst);
        match &first {
            Err(Error::Auth(msg)) => assert!(
                msg.contains("oauth refresh failed"),
                "expected the refresh-failure auth error, got: {msg}",
            ),
            other => panic!("expected Err(Auth), got: {other:?}"),
        }
        assert!(
            !slot_in_flight(&router),
            "half-open slot must be released when on_auth_failure errors",
        );
        assert_eq!(
            provider.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "on_auth_failure fires exactly once before the error propagates",
        );

        // Breaker is NOT locked: a second dispatch (zero cooldown) still
        // claims a fresh probe and reaches the upstream.
        let _ = router.complete(plain_req()).await;
        let calls_after_second = provider.complete_calls.load(Ordering::SeqCst);
        assert!(
            calls_after_second > calls_after_first,
            "second dispatch must reach the upstream; a leaked slot would lock \
             the breaker (CircuitOpen) and skip it: {calls_after_first} -> {calls_after_second}",
        );
    }

    /// A non-probe dispatch whose upstream returns a LARGE reset hint
    /// (> INLOOP_RETRY_AFTER_CAP) parks the provider via `force_open` for
    /// the honored duration, rather than blocking the request thread.
    /// The failure threshold is high (5) so the ONLY way the breaker can
    /// be open afterward is the force-park, not a counter-driven trip.
    #[tokio::test]
    async fn large_retry_after_parks_provider_via_force_open() {
        // Arrange: 60s reset, well above the 5s in-loop cap, on a 429.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_secs(60)),
            calls: calls.clone(),
        });
        // High threshold + a non-zero default cooldown (1s) so a stray
        // record_failure would open for only ~1s, distinguishable from a
        // 60s force-park.
        let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
        let t0 = Instant::now();

        // Act.
        let _ = router.complete(plain_req()).await.unwrap_err();

        // Assert: open now, still open at +59s (a 1s record_failure trip
        // would already have elapsed), allowed only after the 60s park.
        assert!(
            breaker_open_at(&router, t0),
            "large reset must park the provider open immediately",
        );
        assert!(
            breaker_open_at(&router, t0 + Duration::from_secs(59)),
            "park must outlast the default cooldown -- proving force_open, not a record_failure trip",
        );
        assert!(
            !breaker_open_at(&router, t0 + Duration::from_secs(61)),
            "park must release once the honored 60s reset elapses",
        );
    }

    /// A SMALL reset (<= INLOOP_RETRY_AFTER_CAP) on a retryable error is
    /// honored as an in-loop sleep, NOT a force-park: the same provider is
    /// retried (call count rises to the retry cap) and a high failure
    /// threshold leaves the breaker closed (no force_open).
    #[tokio::test]
    async fn small_retry_after_honored_in_loop_not_parked() {
        // Arrange: 1ms reset (tiny, keeps the in-loop sleep negligible),
        // 429 -> retryable (default max_attempts = 2). Threshold 5 so a
        // single recorded failure cannot trip the breaker.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_millis(1)),
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
        let t0 = Instant::now();

        // Act.
        let _ = router.complete(plain_req()).await.unwrap_err();

        // Assert: the same provider was retried in-loop (2 = max_attempts),
        // and the breaker was NOT force-parked (closed under the high
        // threshold). A force-park would have opened it after the first
        // attempt and skipped the second.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a small reset must be honored as an in-loop retry, not a park",
        );
        assert!(
            !breaker_open_at(&router, t0),
            "a small reset must NOT force-park the provider (breaker stays closed under a high threshold)",
        );
    }

    /// A reset far larger than `max_honored_retry_after` parks for the
    /// CEILING, not the raw value: open before the ceiling, allowed after.
    #[tokio::test]
    async fn retry_after_clamped_to_ceiling() {
        // Arrange: a 1-hour raw reset, a 10s ceiling.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_secs(3_600)),
            calls: calls.clone(),
        });
        let retry = RetryPolicy {
            max_honored_retry_after_ms: Some(10_000),
            ..RetryPolicy::default()
        };
        let router = build_router_with_breaker(provider, retry, 5, 1_000);
        let t0 = Instant::now();

        // Act.
        let _ = router.complete(plain_req()).await.unwrap_err();

        // Assert: parked for the 10s ceiling, NOT the raw 1h. Still open at
        // +9s; released at +11s (the raw 1h value would still be open).
        assert!(
            breaker_open_at(&router, t0 + Duration::from_secs(9)),
            "park must hold until the ceiling elapses",
        );
        assert!(
            !breaker_open_at(&router, t0 + Duration::from_secs(11)),
            "park must release at the ceiling, not the raw 1h reset",
        );
    }

    /// A probe (max_tokens <= probe_max_tokens) that 429s with a reset hint
    /// fast-fails (R7): NO retry, NO fallback, NO breaker debit, NO park.
    #[tokio::test]
    async fn probe_with_retry_after_does_not_park() {
        // Arrange: a probe-shaped request, a large reset that would park a
        // non-probe. Threshold 5 so any stray debit/park is observable.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_secs(60)),
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
        let t0 = Instant::now();

        // Act: probe_req has max_tokens = 1 <= probe_max_tokens (default 1).
        let _ = router.complete(probe_req()).await.unwrap_err();

        // Assert: fast-fail -- exactly one upstream call (no retry), and the
        // breaker was neither parked nor debited (slot released cleanly).
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a probe must fast-fail on 429 with no retry",
        );
        assert!(
            !breaker_open_at(&router, t0),
            "a probe reset must NOT park the provider (R7)",
        );
    }

    /// A reset on a NON-fallbackable error (a 400 named in the denylist)
    /// does not force a retry or a park: the error terminates exactly as
    /// today (R12 -- the reset never changes a fallback/retry decision).
    #[tokio::test]
    async fn non_fallbackable_error_with_retry_after_still_terminates() {
        // Arrange: a 400 (client error) that is NOT fallbackable, carrying
        // a large reset hint. Threshold 5 so any stray park is observable.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 400,
            retry_after: Some(Duration::from_secs(60)),
            calls: calls.clone(),
        });
        let retry = RetryPolicy {
            retry_denylist: Some(vec![400]),
            ..RetryPolicy::default()
        };
        let router = build_router_with_breaker(provider, retry, 5, 1_000);
        let t0 = Instant::now();

        // Act.
        let result = router.complete(plain_req()).await;

        // Assert: terminated with the 400 (no retry walk), exactly one
        // upstream call, and the breaker was not parked.
        match result {
            Err(Error::Upstream { status: 400, .. }) => {}
            other => panic!("expected terminal Err(Upstream 400), got: {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a non-fallbackable error must not be retried despite a reset hint",
        );
        assert!(
            !breaker_open_at(&router, t0),
            "a non-fallbackable error must not park the provider (R12)",
        );
    }

    /// A SMALL non-probe reset actually LENGTHENS the in-loop retry sleep
    /// (the backoff-bump path), not merely "does not park". With a 1ms
    /// baseline backoff, the 300ms hint must dominate the inter-attempt
    /// wait -- proving the bump took effect (without it the retry would
    /// fire almost immediately off the 1ms baseline).
    #[tokio::test]
    async fn small_retry_after_lengthens_inloop_sleep() {
        // Arrange: 300ms reset (<= the 5s in-loop cap), 429 -> retryable,
        // baseline backoff 1ms so the bump (not the baseline) drives the
        // wait. Threshold 5 so the breaker is not parked.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_millis(300)),
            calls: calls.clone(),
        });
        let retry = RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 1,
            ..RetryPolicy::default()
        };
        let router = build_router_with_breaker(provider, retry, 5, 1_000);

        // Act: time the whole two-attempt dispatch.
        let start = Instant::now();
        let _ = router.complete(plain_req()).await.unwrap_err();
        let elapsed = start.elapsed();

        // Assert: retried once (2 calls), and the inter-attempt sleep was
        // lengthened to honor the 300ms reset, far above the 1ms baseline.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a small reset on a retryable error must still retry the same provider",
        );
        assert!(
            elapsed >= Duration::from_millis(250),
            "the in-loop sleep must be lengthened to ~the 300ms reset (got {elapsed:?}); \
             without the bump the 1ms baseline would fire the retry almost immediately",
        );
    }
}
