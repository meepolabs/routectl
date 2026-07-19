//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, RoutectlInternal,
    TokenCount,
    cache_control::{MAX_BREAKPOINTS, compute_frozen_floor, validate_source},
    capability::{SignalTier, normalize_capability_key},
    context_reduction::{ReductionOutcome, apply_json_minify},
    failure_class::{ClassifiedFailure, FailureClass, MatchedBy, classify},
    sanitize_for_log, scan_volatile,
};
use serde_json::Value;

use crate::capability_matcher::resolve_requested_capability;
use crate::capability_strip::{Outcome, RequestInterceptor, StripContext, StripInterceptor};
use crate::catalog::{CatalogRow, EffectiveRow};
use crate::config::{
    AliasValue, CacheCapability, Config, HistoryReasoning, ReasoningDialect, RetryPolicy,
};
use crate::context_trim::{
    SteadyStateTrimParams, collect_near_lossless_marks, estimate_total_tokens,
    near_lossless_candidate, propose_steady_state_trim, trimmed_prefix_fingerprint,
};
use crate::cost_gate::break_even_k;
use crate::glob::PrefixIndex;
use crate::resolved::ResolvedModel;
use crate::runtime_state::{CircuitPhase, GateDecision, ProviderGateStatus, ProviderState};

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
pub const ALIAS_MAX_RECURSION_DEPTH: usize = 8;

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
    /// Bounded LRU map of conversation session key -> pinned seat
    /// `state_key`, for `StickyLeastLoaded` selection. In sharp contrast to
    /// `round_robin` (which is dropped on a Router rebuild because resetting
    /// to seat 0 is benign), these pins MUST survive a hot-reload: dropping
    /// them would scatter every live conversation off its warm-cache seat,
    /// causing a mass cold-miss. See `carry_over_sticky_from`.
    sticky_pins: crate::seat_pool::StickyPins,
    /// Per-session K-estimator window store, sibling to `sticky_pins`.
    /// Triple-keyed by (session, provider_kind, model) so a session that
    /// switches provider or model does not bleed its cache-reuse history
    /// onto the new triple. MUST survive a hot-reload for the same reason
    /// the sticky pins do: a wipe collapses every learned estimate back to
    /// `Cold` and silently un-arms the cost gate. See
    /// `carry_over_k_store_from`.
    ///
    /// Held behind an `Arc` so the in-process [`KEstimator`] reader
    /// (`k_estimator` below) can share the SAME store as the dispatch path
    /// that records samples into it -- the reader observes every sample the
    /// writer lands, without any cross-store copy or refresh.
    pub k_session_store: Arc<crate::k_estimator::KSessionStore>,
    /// K-estimator reader over `k_session_store`. The constructor wires the
    /// default [`crate::k_estimator::LedgerBackedK`] over a clone of the
    /// `k_session_store` `Arc`, so a sample recorded into the store is
    /// immediately visible to the next `estimate(...)` call.
    ///
    /// No carry-over field of its own: a hot-reload constructs a fresh
    /// store + a fresh estimator (the estimator points at the fresh store),
    /// then `carry_over_k_store_from` populates the fresh store from the
    /// previous router's entries -- so the fresh estimator transparently
    /// sees the carried samples.
    k_estimator: Arc<dyn crate::k_estimator::KEstimator>,
    /// In-process session-keyed last-fingerprint store for the shadow misfire
    /// monitor. Keyed by the same (session, provider_kind, model) triple as
    /// `k_session_store`. Not carried over on a hot-reload: the monitor
    /// treats the first turn after a reload as `FirstSeen`, which is the
    /// safe default (no false misfire on a fresh fingerprint after a reload).
    shadow_store: Arc<crate::k_estimator::ShadowStore>,
    /// In-memory learned-capability registry (the `k_session_store`
    /// pattern): per-(target, feature) negatives the dispatch path learns
    /// from upstream request faults. Interior-locked and mutated through
    /// `&self`, so it is held behind an `Arc` and shared, never cloned per
    /// request. Constructed from the `[capability]` decay / inferred-window
    /// knobs; the kill switch is NOT read here (the act / learn sites gate
    /// on it). Carried across a hot-reload by `carry_over_learned_from`,
    /// but ONLY when the catalog version and overlay revision are both
    /// unchanged -- either bump invalidates every learned negative because
    /// the fresher pricing / capability truth must win.
    learned_capabilities: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
    /// Operator capability-override read-model, flattened from config at
    /// construction. Pure projection of `config.capability.overrides` plus
    /// the legacy provider / model `unsupported_features` lists -- no
    /// carry-over on reload, since a reload builds a fresh Router from the
    /// new config and this rebuilds deterministically from it.
    override_registry: crate::override_registry::OverrideRegistry,
    /// Baked catalog table version this Router was built against
    /// (`catalog_baked::CATALOG_VERSION`). Compared old-vs-new in
    /// `carry_over_learned_from`: a bump invalidates the learned registry.
    catalog_version: u32,
    /// Catalog-overlay revision this Router was built against. Zero until
    /// `note_overlay_revision` records the revision the resolved-model
    /// table was stamped with. Compared old-vs-new in
    /// `carry_over_learned_from`: a change invalidates the learned registry.
    overlay_revision: u64,
    /// Lock-free router-side observability counters. Not carried over on
    /// a hot-reload rebuild (a reset is benign for observability, same
    /// rationale as `round_robin`).
    metrics: RouterMetrics,
    /// `true` iff `config.providers` contains a `ProviderEntry::AnthropicApi`
    /// with `credential_source == Forwarded`. Computed ONCE here at
    /// construction (a full `config.providers` scan) rather than re-scanned
    /// per request -- this is the "configured capability" half of the
    /// forwarded-mode CAPTURE gate (`forwarded_capture_armed` in
    /// routectl-cli), replacing the removed `[mitm] credential_source` read.
    has_forwarded_provider: bool,
}

/// Lock-free router-side observability counters.
///
/// Consistent with the front-proxy `ProxyMetrics` pattern (see
/// `routectl_cli::proxy::metrics`): routectl has no metrics backend or
/// exporter, so each metric is a plain `AtomicU64` (`Ordering::Relaxed`,
/// never used for control flow) surfaced through structured `tracing`
/// logs. No token, credential, or request/response body ever flows into
/// a counter here -- by construction the only inputs are refusal events.
#[derive(Debug, Default)]
struct RouterMetrics {
    /// Upstream failures the classifier could not confidently categorize
    /// (`FailureClass::Unknown`) that arrived on a real upstream HTTP
    /// outcome (`Error::Upstream`). A fail-closed unknown on a genuine
    /// upstream response is a signal the token vocabulary or status map
    /// may need extending; bumped at both dispatch error arms.
    unknown_failure_classifications_total: AtomicU64,
    /// Upstream failures the classifier lifted to
    /// `FailureClass::FeatureUnsupported` (a requested capability the
    /// upstream rejected). Bumped at both dispatch error arms.
    feature_unsupported_total: AtomicU64,
    /// Learned negatives that reached the acting state (self-identifying
    /// on the first observation, inferred on the confirming second).
    /// Bumped by the learn path (later act / learn wiring).
    learned_negatives_total: AtomicU64,
    /// Re-probes admitted after a learned negative's decay window lapsed.
    /// Bumped by the dispatch path when it claims the single probe slot
    /// (later act / learn wiring).
    probe_attempts_total: AtomicU64,
    /// Admitted re-probes that hit the same capability rejection again.
    /// Bumped when a probe outcome is settled as a same-capability
    /// rejection (later act / learn wiring).
    probe_failures_total: AtomicU64,
    /// Learned registries dropped on a hot-reload because the catalog
    /// version or overlay revision changed. Bumped by
    /// `carry_over_learned_from`.
    invalidations_total: AtomicU64,
    /// Requests whose chain survived only via the de-prioritized learned
    /// tail (soft-drop). Bumped by the dispatch path when the tail is
    /// entered.
    d17_tail_total: AtomicU64,
    /// Capabilities stripped in place at a dispatch path (the interceptor
    /// returned `Stripped`). Bumped once per successful strip run, not per
    /// capability key.
    strip_total: AtomicU64,
    /// Strip runs rolled back because the post-strip check found a
    /// strip-created hazard; the request was restored and the attempt
    /// routed away. Bumped by the strip interceptor hook.
    strip_rollback_total: AtomicU64,
    /// Would-be strips refused because `strict_translation` is on; the
    /// attempt returned a 400 without dispatching. Bumped by the strip
    /// interceptor hook.
    strip_strict_rejected_total: AtomicU64,
    /// Masked (`force_supported`) capability rejections observed on a
    /// dispatch error arm: the operator forced a capability on for a target,
    /// but upstream still rejected it. Bumped once per request per
    /// `(state_key, feature)` by the learn path when it suppresses the learn.
    mask_suppressed_total: AtomicU64,
}

impl RouterMetrics {
    fn incr_unknown_failure_classification(&self) {
        self.unknown_failure_classifications_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_feature_unsupported(&self) {
        self.feature_unsupported_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_learned_negatives(&self) {
        self.learned_negatives_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_probe_attempts(&self) {
        self.probe_attempts_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_probe_failures(&self) {
        self.probe_failures_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_invalidations(&self) {
        self.invalidations_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_d17_tail(&self) {
        self.d17_tail_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_strip(&self) {
        self.strip_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_strip_rollback(&self) {
        self.strip_rollback_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_strip_strict_rejected(&self) {
        self.strip_strict_rejected_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_mask_suppressed(&self) {
        self.mask_suppressed_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the cumulative unknown-upstream-classification count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn unknown_failure_classifications_total(&self) -> u64 {
        self.unknown_failure_classifications_total
            .load(Ordering::Relaxed)
    }

    /// Read the cumulative feature-unsupported classification count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn feature_unsupported_total(&self) -> u64 {
        self.feature_unsupported_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative learned-registry invalidation count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn invalidations_total(&self) -> u64 {
        self.invalidations_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative admitted-re-probe count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn probe_attempts_total(&self) -> u64 {
        self.probe_attempts_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative same-capability re-probe-failure count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn probe_failures_total(&self) -> u64 {
        self.probe_failures_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative learned-tail entry count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn d17_tail_total(&self) -> u64 {
        self.d17_tail_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative in-place-strip count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn strip_total(&self) -> u64 {
        self.strip_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative strip-rollback count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn strip_rollback_total(&self) -> u64 {
        self.strip_rollback_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative strict-rejected-strip count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn strip_strict_rejected_total(&self) -> u64 {
        self.strip_strict_rejected_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative masked-rejection-suppression count.
    /// Test-only read surface today; ungate with the metrics snapshot.
    #[cfg(test)]
    fn mask_suppressed_total(&self) -> u64 {
        self.mask_suppressed_total.load(Ordering::Relaxed)
    }
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

/// A single learned-capability observation captured on a dispatch error
/// arm, riding out on [`DispatchMeta`] to the usage-capture layer. The
/// router does not depend on the ledger writer, so learn events travel
/// on the dispatch meta rather than being written here.
///
/// `capability_key` is already normalized (via `normalize_capability_key`),
/// so the writer and any future warm-rebuild replayer key off identical
/// strings. `remapped` is always `false` by construction (the capture
/// gate rejects a config-remapped class); it is carried so a replay can
/// filter defensively. No request body, prompt, or upstream message text
/// ever enters this struct -- only the classifier's structured facts.
#[derive(Debug, Clone)]
pub struct CapabilityLearnEvent {
    /// Breaker state key (nickname-or-provider) of the rejecting target.
    pub state_key: String,
    /// Normalized capability key the rejection named.
    pub capability_key: String,
    /// Stable provider-kind token of the rejecting target.
    pub provider_kind: String,
    /// Whether the evidence was self-identifying or inferred.
    pub signal_tier: SignalTier,
    /// Observation count on the entry after this observation.
    pub observations: u32,
    /// The upstream request-fault status (400 or 422) that carried the
    /// rejection.
    pub upstream_status: u16,
    /// Always `false` here (a remapped class never reaches capture);
    /// persisted for defensive replay filtering.
    pub remapped: bool,
    /// The request's derived feature set at capture time. Replay verifies
    /// the learned capability was actually in flight.
    pub request_features: Vec<String>,
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
    /// Served wire model id -- the model string actually sent to the
    /// provider. For an OWN-credential target this is `target.upstream`
    /// (the rewritten wire id) as before. For a FORWARDED-credential
    /// target the rewrite is bypassed (model transparency), so this
    /// carries the client's requested model verbatim instead --
    /// `target.upstream` is never sent on the wire for that target and
    /// would misreport what egress actually saw. `None` when no
    /// provider was touched.
    pub served_upstream: Option<String>,
    /// Whether the served/terminal target authenticated with the
    /// forwarded client bearer (`DispatchTarget::use_forwarded_credential`)
    /// rather than an own credential. `false` when no provider was
    /// touched (the default absence of a forwarded target). Post-dispatch
    /// consumers (usage accounting) read this instead of the
    /// request-global bearer-presence check, which cannot tell a
    /// forwarded target from a coexisting own-credential one in the same
    /// chain.
    pub served_forwarded_credential: bool,
    /// The resolved alias key the request routed under (the incoming
    /// `req.model`). Always populated, even when resolution then failed.
    pub resolved_alias: String,
    /// Stable auto-cache decision token for the served target (see
    /// [`CacheInjection::strategy_str`]). `None` when no target was
    /// dispatched (count_tokens, unknown alias, or all entries
    /// gate-blocked before any injection point ran).
    pub cache_strategy: Option<&'static str>,
    /// Stable context-reduction decision token for the served target
    /// (see [`reduction_strategy_token`]). `None` when no target was
    /// dispatched (count_tokens, unknown alias, or all entries
    /// gate-blocked before any reduction point ran).
    pub reduction_strategy: Option<&'static str>,
    /// Stable seat-selection decision token for the served target's home
    /// seat (see `push_seat_targets` for the fixed vocabulary). `None` for
    /// non-sticky / single-seat pools, non-pooled aliases, and when no
    /// target was dispatched.
    ///
    /// LIMITATION: this token is propagated only when the sticky HOME seat
    /// serves. A request that falls back PAST its home (home failed) records
    /// `None`, because the decision is stamped on the home target only and
    /// `mark_target` copies whichever target actually served.
    pub selection_decision: Option<&'static str>,
    /// Non-mutating steady-state would-trim advisory: the freed-token count
    /// `d` of the trimmer's would-cut candidate for the dispatched request.
    /// `None` when the steady-state trimmer proposed no cut (or no target was
    /// dispatched). The live request is NEVER mutated -- this is recording
    /// only (see [`Router::record_would_trim`]).
    pub would_trim_tokens: Option<u64>,
    /// Non-mutating steady-state would-trim advisory: the break-even reuse
    /// count K* the cost gate priced for the would-cut candidate. `None` when
    /// the trimmer proposed no cut, OR the two-layer catalog merge resolved
    /// `Disabled` / `Missing` (an unknown provider or a disabled cell records
    /// the freed-token count but no K* -- no trusted pricing), OR the
    /// resolved row carried no finite break-even. Recording only.
    pub would_trim_break_even_k: Option<f64>,
    /// Non-mutating steady-state would-trim advisory: the lower confidence
    /// bound `k_floor` of the per-session K estimate, recorded ONLY when the
    /// K estimator returned a `Calibrated` confidence (the only bound the
    /// cost gate may consult to authorize a future cut). `None` for a
    /// `Cold` / `Low` estimate (insufficient history), for a `Disabled` /
    /// `Missing` catalog cell (no `break_even_k` to compare against), and
    /// when no would-cut candidate was proposed. Recording only.
    pub would_trim_k_floor: Option<f64>,
    /// Non-mutating shadow misfire monitor: `Some(0)` when the trimmed
    /// cacheable prefix hash matched the prior turn for this session triple
    /// (Stable), `Some(1)` when it differed (Misfire -- the real cut would
    /// have broken the upstream cache), `None` for a `FirstSeen` observation,
    /// when no session key was present, or when no would-cut candidate was
    /// proposed. Recording only -- the dispatched bytes are NEVER mutated.
    pub would_trim_shadow_misfire: Option<i64>,
    /// Non-mutating near-lossless attribution: freed tokens attributed to
    /// the dedup heuristic over the near-lossless scan window. `Some(0)` is
    /// a measured zero (the pass ran, found no exact-byte duplicates);
    /// `None` means the pass did not run (below the estimated-token
    /// trigger). Independent of whether the shipped size-baseline plan
    /// (`would_trim_tokens`) proposed a cut. Recording only.
    pub would_trim_dedup_tokens: Option<u64>,
    /// Non-mutating near-lossless attribution: freed tokens attributed to
    /// the supersession heuristic over the near-lossless scan window.
    /// `Some(0)` is a measured zero; `None` means the pass did not run
    /// (below the estimated-token trigger). See `would_trim_dedup_tokens`.
    /// Recording only.
    pub would_trim_supersession_tokens: Option<u64>,
    /// Non-mutating path-extractability count-pair: the denominator (total
    /// path units considered). Paired with `would_trim_path_extractable` so
    /// the extractability rate is reconstructable offline via SUM/SUM
    /// rather than pre-averaged per row. `None` when the near-lossless pass
    /// did not run (below trigger). Recording only.
    pub would_trim_path_units: Option<u64>,
    /// Non-mutating path-extractability count-pair: the numerator (path
    /// units that were extractable). See `would_trim_path_units`. `None`
    /// when the near-lossless pass did not run (below trigger). Recording
    /// only.
    pub would_trim_path_extractable: Option<u64>,
    /// Recorder-version marker: `None` on pre-M1 rows and on rows where the
    /// near-lossless pass did not run (below the estimated-token trigger);
    /// stamped with [`NEAR_LOSSLESS_RECORDER_VERSION`] by the M1 recorder
    /// (`Router::record_would_trim`) on every trigger-clearing row,
    /// regardless of whether the pass found any marks. Lets reporting
    /// filter to non-NULL rows so aggregates never mix baseline vs M1
    /// semantics.
    pub would_trim_recorder_version: Option<i64>,
    /// Raw-marks JSON blob (uncapped at this layer): the near-lossless
    /// pass's marks (dedup + supersession), captured for a future M3 sweep.
    /// The byte cap is applied downstream by
    /// `routectl_usage::writer::capped_raw_marks_text` so the stored JSON
    /// is always valid. `None` when the near-lossless pass did not run
    /// (below trigger). Recording only.
    pub would_trim_raw_marks: Option<Value>,
    /// Non-mutating context-fraction advisory: `estimate_total_tokens /
    /// max_context_tokens` from the resolved pricing row. `None` when the
    /// near-lossless pass did not run (below trigger) OR the resolved
    /// row's context window is unknown (fail-closed). Recording only.
    pub would_trim_context_fraction: Option<f64>,
    /// Learned-capability observations captured on the dispatch error
    /// arm(s) for this request. Empty on the common (non-capability)
    /// path; carries one event per eligible, deduped, acting observation
    /// (self-identifying on the first, inferred on the confirming second)
    /// so the usage-capture layer can persist them without the router
    /// depending on the ledger writer.
    pub learned_capabilities: Vec<CapabilityLearnEvent>,
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
            served_forwarded_credential: false,
            resolved_alias: alias.to_string(),
            cache_strategy: None,
            reduction_strategy: None,
            selection_decision: None,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
            would_trim_dedup_tokens: None,
            would_trim_supersession_tokens: None,
            would_trim_path_units: None,
            would_trim_path_extractable: None,
            would_trim_recorder_version: None,
            would_trim_raw_marks: None,
            would_trim_context_fraction: None,
            learned_capabilities: Vec::new(),
        }
    }

    /// Record the target currently being dispatched as the served /
    /// terminal target. Called on each chain entry the loop actually
    /// dispatches to, so on the all-failed path the LAST dispatched
    /// target is the terminal one.
    ///
    /// `requested_model` is the client's requested model (`req.model`,
    /// read BEFORE any rewrite) -- the value a forwarded target actually
    /// sends over the wire, since `use_forwarded_credential` bypasses the
    /// `target.upstream` rewrite. Only consulted on that branch; an own
    /// target's `served_upstream` is `target.upstream`, unchanged.
    fn mark_target(&mut self, target: &DispatchTarget, requested_model: &str) {
        self.served_provider = Some(target.provider_name.clone());
        self.served_provider_kind = target.provider_kind.map(std::string::ToString::to_string);
        self.served_model = target.nickname.clone();
        self.served_upstream = Some(if target.use_forwarded_credential {
            requested_model.to_string()
        } else {
            target.upstream.clone()
        });
        self.served_forwarded_credential = target.use_forwarded_credential;
        self.selection_decision = target.selection_decision;
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

/// A request feature key (e.g. `web_search`, `structured_output`). Same
/// vocabulary as `crate::feature_keys`; aliased here so the feature
/// filter's decision seam reads at the right level of intent.
type FeatureKey = String;

/// What flagged a feature as unsupported for a target. The feature
/// filter's decision site returns this so the skip log can distinguish a
/// provider-scoped restriction from a model-scoped one, and so the filter
/// loop can tell a hard static drop from a soft learned de-prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterSource {
    /// Matched a route-away override whose provenance is the legacy
    /// per-provider `unsupported_features` list.
    ProviderStatic,
    /// Matched a route-away override whose provenance is the legacy
    /// per-model `unsupported_features` list.
    ModelStatic,
    /// Matched a route-away override whose provenance is a new
    /// `[capability.overrides.<spec>].unsupported` entry.
    Override,
    /// Matched a non-expired acting negative in the learned-capability
    /// registry. A soft signal: the target is de-prioritized to the tail,
    /// never hard-dropped.
    Learned,
}

impl FilterSource {
    /// Stable lowercase token for the skip-log `source` field. The
    /// `"learned"` and `"override"` tokens are a CONTRACT consumed by
    /// downstream features (action dispatch, doctor labels, the future
    /// status endpoint).
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderStatic => "provider",
            Self::ModelStatic => "model",
            Self::Override => "override",
            Self::Learned => "learned",
        }
    }
}

impl From<crate::override_registry::OverrideProvenance> for FilterSource {
    fn from(provenance: crate::override_registry::OverrideProvenance) -> Self {
        use crate::override_registry::OverrideProvenance;
        match provenance {
            OverrideProvenance::ProviderStatic => Self::ProviderStatic,
            OverrideProvenance::ModelStatic => Self::ModelStatic,
            OverrideProvenance::Override => Self::Override,
        }
    }
}

/// Outcome of dispatching `count_tokens` to one capable seat, driving
/// the walk in [`Router::count_tokens`].
enum CountSeatOutcome {
    /// The seat returned a token count; return it to the caller.
    Count(TokenCount),
    /// A definitive result for this request -- return the error verbatim.
    /// Covers a settled health error (breaker already debited/parked), a
    /// non-fallbackable 4xx, a gate block, or an auth-refresh failure.
    Terminal(Error),
    /// The seat is capable-by-kind but its upstream cannot count (local
    /// `NotImplemented` or a wire 501). The probe slot was released
    /// without a breaker debit; advance to the next capable seat.
    Capability,
}

/// What a dispatch loop must do after the per-attempt strip interceptor
/// runs. The three dispatch paths map this onto their own control flow
/// (return / continue / advance-seat).
#[derive(Debug)]
enum StripDecision {
    /// Nothing to reject: proceed to dispatch `attempt_req` (either
    /// untouched or with the droppable capability stripped in place).
    Proceed,
    /// `strict_translation` refused a would-be strip. No mutation
    /// happened; return this 400 for the attempt without dispatching.
    StrictReject(Error),
    /// The post-strip check found a strip-created hazard; the request was
    /// restored to its pre-strip bytes. Do not dispatch it -- route away
    /// for this attempt as an ordinary route-away verdict would.
    RouteAway(Error),
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
    /// Per-target replacement for the removed request-global forwarded
    /// check. `true` when this target's provider entry is an
    /// `AnthropicApi` provider with `credential_source: Forwarded`;
    /// populated ONCE at chain expansion in `expand_chain_to_targets`,
    /// the same pattern as `provider_kind`, for both seat and non-seat
    /// targets. Read at the three dispatch paths (`complete_inner`,
    /// `count_tokens_try_seat`, `stream_inner`) to bypass the
    /// `attempt_req.model` rewrite: a forwarded target forwards the
    /// client's requested model verbatim; an own target still rewrites
    /// to `upstream`. `false` in both constructors until the post-loop
    /// sets it.
    use_forwarded_credential: bool,
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
    /// Capability keys the feature filter decided to STRIP in place for
    /// this target rather than route away from -- the strip-vs-route
    /// verdict for a learned acting negative whose policy
    /// ([`capability_strip::action_for`]) is `Strip` and that no operator
    /// beta floor pins to the wire. Sorted normalized keys
    /// (`normalize_capability_key`), so the per-session cache prefix an
    /// interceptor derives from them stays stable across requests. Empty
    /// (the default in both constructors) means no capability is stripped
    /// for this target.
    ///
    /// `Arc<[String]>` so cloning per dispatch attempt is a refcount
    /// bump rather than a heap allocation.
    strip_capabilities: std::sync::Arc<[String]>,
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
    /// Stable seat-selection decision token for THIS target, set only on
    /// the home (first) seat target of a sticky / keyless-collapse pool by
    /// `push_seat_targets`. `None` on every other target. Propagated to
    /// `DispatchMeta::selection_decision` via `mark_target` so usage
    /// accounting can record how the seat was chosen. Observability only --
    /// never affects seat order, the target set, or dispatch.
    selection_decision: Option<&'static str>,
    /// Per-status failure-class remap for this target's provider, adapted
    /// ONCE at chain expansion from the operator's
    /// `[providers.X.class_overrides]` config table (`ConfigFailureClass`
    /// -> canonical `FailureClass` via `ConfigFailureClass::to_failure_class`).
    /// Consulted immediately after `classify` at each of the three dispatch
    /// error arms (`apply_remap`): a status-key match replaces the
    /// classifier's native class with the operator's override, keeping the
    /// native `matched_by`. Empty (the default, and the value every
    /// constructor starts with) leaves native classification untouched.
    class_overrides: BTreeMap<u16, FailureClass>,
    /// Catalog capability priors for this target, merged (baked + overlay)
    /// and copied off `ResolvedModel::effective_row` at chain expansion. An
    /// absent key means NO PRIOR (distinct from `Some(false)`, an asserted
    /// absence). Empty when the resolved cell is `Disabled` / `Missing` or
    /// carries no capability data. The third precedence baseline
    /// (override -> learned -> prior); read via
    /// [`DispatchTarget::capability_prior`]. NO filter behavior consumes it
    /// yet -- the prior-driven drop lands when the override layer wires the
    /// full precedence chain.
    capabilities: BTreeMap<String, bool>,
}

impl DispatchTarget {
    /// The catalog capability prior for `feature`: `Some(true)` /
    /// `Some(false)` when the resolved cell asserts support / absence, or
    /// `None` when the cell carries no prior for the key. The lowest-
    /// precedence baseline in the override -> learned -> prior chain; NO
    /// filter consumes it in this increment.
    #[allow(dead_code)]
    fn capability_prior(&self, feature: &str) -> Option<bool> {
        self.capabilities.get(feature).copied()
    }
}

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

        let k_session_store = Arc::new(crate::k_estimator::KSessionStore::default());
        let k_estimator: Arc<dyn crate::k_estimator::KEstimator> = Arc::new(
            crate::k_estimator::LedgerBackedK::new(k_session_store.clone()),
        );
        let shadow_store = Arc::new(crate::k_estimator::ShadowStore::default());
        // Registry tempo comes from the `[capability]` knobs (hours ->
        // Duration); the kill switch is deliberately NOT read here -- the
        // act / learn sites gate on it, so a disabled subsystem keeps the
        // registry resident but inert and a hot re-enable is instant.
        let learned_capabilities =
            Arc::new(crate::learned_capability::LearnedCapabilityRegistry::new(
                Duration::from_hours(config.capability.decay_hours),
                Duration::from_hours(config.capability.inferred_window_hours),
                crate::learned_capability::DEFAULT_MAX_ENTRIES,
            ));
        let has_forwarded_provider = config
            .providers
            .values()
            .any(|entry| entry.forwarded_base_url().is_some());
        let override_registry = crate::override_registry::OverrideRegistry::build(&config);
        Self {
            config,
            providers: Default::default(),
            state,
            resolved_models: BTreeMap::new(),
            alias_glob_index,
            round_robin: Default::default(),
            sticky_pins: Default::default(),
            k_session_store,
            k_estimator,
            shadow_store,
            learned_capabilities,
            override_registry,
            catalog_version: crate::catalog_baked::CATALOG_VERSION,
            overlay_revision: 0,
            metrics: RouterMetrics::default(),
            has_forwarded_provider,
        }
    }

    /// `true` iff a `ProviderEntry::AnthropicApi` with
    /// `credential_source == Forwarded` is configured. Build-time cached
    /// (see [`Router::has_forwarded_provider`] field doc) -- callers never
    /// re-scan `config.providers` per request.
    pub const fn has_forwarded_provider(&self) -> bool {
        self.has_forwarded_provider
    }

    /// The operator capability-override read-model built from this
    /// Router's config. Provenance-preserving projection of the config
    /// overrides plus the legacy provider / model `unsupported_features`
    /// lists.
    pub const fn override_registry(&self) -> &crate::override_registry::OverrideRegistry {
        &self.override_registry
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
        for (nickname, m) in &self.resolved_models {
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
            // bucket are per-seat (probe fast-fail, retry caps, and the
            // reset park all apply per seat).
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
    pub fn carry_over_runtime_state_from(&mut self, previous: &Self) {
        for (key, state) in &previous.state {
            if self.state.contains_key(key.as_str()) {
                self.state.insert(key.clone(), state.clone());
            }
        }
    }

    /// Carry the previous Router's `StickyLeastLoaded` session->seat pins
    /// into this freshly-built Router during a hot-reload.
    ///
    /// This carry-over is MANDATORY. Each pin keeps a live conversation on
    /// the one seat holding its warm prompt cache; dropping the pins on a
    /// reload would re-pin every conversation from scratch and scatter them
    /// across seats -- a mass cold-miss across all in-flight conversations.
    /// (By contrast, the round-robin cursors are deliberately dropped on a
    /// reload because a reset to seat 0 costs at most one mis-rotated
    /// request -- benign. Pins are not benign, so they survive here.)
    ///
    /// Entries are replayed in LRU order (least-recently-used first) so the
    /// destination map preserves the source's recency ordering, keeping the
    /// eviction frontier consistent across the rebuild.
    ///
    /// Called by the hot-reload coordinator in routectl-cli immediately
    /// after building a replacement Router and before swapping it in,
    /// alongside `carry_over_runtime_state_from`.
    pub fn carry_over_sticky_from(&mut self, previous: &Self) {
        for (session_key, pin) in previous.sticky_pins.export_entries() {
            self.sticky_pins.put(&session_key, pin);
        }
    }

    /// Carry the previous Router's per-session K-estimator windows into
    /// this freshly-built Router during a hot-reload.
    ///
    /// Mandatory for the same reason `carry_over_sticky_from` is mandatory:
    /// each window is a learned per-(session, provider_kind, model) cache-
    /// reuse history. Dropping the windows on a rebuild would collapse every
    /// estimate back to `Cold`, which the cost gate refuses to cut on, so a
    /// hot-reload would silently un-arm advisory live-cut work and leave the
    /// operator looking at a wall of cold defaults until traffic re-warmed
    /// the store -- exactly the failure mode the sticky-pin carry-over was
    /// added to prevent, applied to the same key space.
    ///
    /// Entries are replayed in LRU order (least-recently-used first) so the
    /// destination map preserves the source's recency ordering. A scattered
    /// (e.g. HashMap-iteration-order) carry-over would race the eviction
    /// frontier across the rebuild.
    pub fn carry_over_k_store_from(&mut self, previous: &Self) {
        self.k_session_store
            .import_entries(previous.k_session_store.export_entries());
    }

    /// Record the catalog-overlay revision this Router was stamped
    /// against. Called by the CLI builder immediately after the resolved
    /// models are merged with the overlay, so `carry_over_learned_from`
    /// can detect a revision change across a hot-reload and invalidate the
    /// learned registry. Left at zero for build paths that apply no
    /// overlay (which never carry learned state anyway).
    pub const fn note_overlay_revision(&mut self, revision: u64) {
        self.overlay_revision = revision;
    }

    /// Carry the previous Router's learned-capability registry into this
    /// freshly-built Router during a hot-reload -- but ONLY when the
    /// catalog version AND the overlay revision are both unchanged.
    ///
    /// The learned negatives are inferences about how a target priced or
    /// rejected a capability under the catalog / overlay in force when they
    /// were learned. If either the baked catalog version or the overlay
    /// revision changed, that pricing / capability truth is now fresher
    /// than anything the registry holds, so clear-on-change wins: the new
    /// Router starts with an EMPTY registry (its construction default) and
    /// re-learns from live traffic. Restart-re-probe is the accepted model;
    /// a full clear trivially satisfies "fresher truth is never silently
    /// overridden."
    ///
    /// When both are unchanged, every entry rides across at full fidelity
    /// (decay windows, backoff counters intact) so a config-only reload does
    /// not un-learn a valid negative. The one exception is the in-flight
    /// probe slot: a probe outstanding against the pre-swap Router can never
    /// clear a slot copied onto the new one, so it is carried across as free
    /// and the next matching request re-admits a probe normally.
    ///
    /// Called by the hot-reload coordinator in routectl-cli immediately
    /// after building a replacement Router and before swapping it in,
    /// alongside the other carry-over calls.
    pub fn carry_over_learned_from(&mut self, previous: &Self) {
        let catalog_changed = self.catalog_version != previous.catalog_version;
        let overlay_changed = self.overlay_revision != previous.overlay_revision;
        if catalog_changed || overlay_changed {
            self.metrics.incr_invalidations();
            tracing::warn!(
                event = "invalidation",
                catalog_changed,
                overlay_changed,
                previous_catalog_version = previous.catalog_version,
                catalog_version = self.catalog_version,
                previous_overlay_revision = previous.overlay_revision,
                overlay_revision = self.overlay_revision,
                "catalog/overlay changed across reload; clearing learned-capability registry",
            );
            return;
        }
        self.learned_capabilities
            .import_entries(previous.learned_capabilities.export_entries());
        self.expire_learned_on_override_change(previous);
    }

    /// After a config-only carry-over, lapse into a single re-probe every
    /// learned negative whose EFFECTIVE operator override verdict changed
    /// across the reload -- a `force_supported` mask (or any override cell)
    /// added, removed, or flipped for that `(target, capability)`. The
    /// operator's intent for the cell moved, so the resident learned verdict
    /// is re-verified against live upstream behavior rather than trusted; the
    /// entry is expired (decay clock reset), NOT dropped, so its observation
    /// history and backoff survive. Entries whose override resolution is
    /// unchanged ride across intact -- this never clears the whole registry
    /// (that stays keyed to catalog / overlay changes).
    fn expire_learned_on_override_change(&self, previous: &Self) {
        let now = Instant::now();
        for entry in self.learned_capabilities.snapshot() {
            let (provider_name, nickname) = self.override_identity_for(&entry.state_key);
            let provider_kind = self
                .config
                .providers
                .get(&provider_name)
                .map_or("", |p| p.kind_str());
            let before = previous
                .override_registry
                .resolve(&provider_name, &nickname, &entry.feature_key, provider_kind)
                .map(|(verdict, _)| verdict);
            let after = self
                .override_registry
                .resolve(&provider_name, &nickname, &entry.feature_key, provider_kind)
                .map(|(verdict, _)| verdict);
            if before != after {
                self.learned_capabilities.expire_keyed(
                    &entry.state_key,
                    &entry.feature_key,
                    provider_kind,
                    now,
                );
                tracing::debug!(
                    state_key = %entry.state_key,
                    capability_key = %entry.feature_key,
                    "override cell changed across reload; lapsed learned negative into a re-probe",
                );
            }
        }
    }

    /// Map a learned-registry `state_key` to the `(provider_name, nickname)`
    /// pair the override registry resolves against. A per-model target keys
    /// by nickname; a pooled seat keys by `nickname#label` (recover the base
    /// model); a legacy / direct-construction target keys by the provider
    /// name itself (no model scope). Enables comparing a learned entry's
    /// effective override verdict across a reload.
    fn override_identity_for(&self, state_key: &str) -> (String, String) {
        if let Some(model) = self.resolved_models.get(state_key) {
            return (model.provider_name.clone(), state_key.to_string());
        }
        if let Some((base, _label)) = state_key.split_once('#')
            && let Some(model) = self.resolved_models.get(base)
        {
            return (model.provider_name.clone(), base.to_string());
        }
        (state_key.to_string(), String::new())
    }

    /// Record one live cache-reuse observation into the per-session K
    /// estimator store. Called best-effort from the ingress capture path
    /// AFTER a dispatched response's `cache_read` and served target are
    /// known.
    ///
    /// Keyless requests (a one-shot probe, an unauthenticated dev call)
    /// carry no session identity and are not tracked: there is no stable
    /// triple to accumulate against, so the call is a no-op rather than
    /// keyed on an empty string. A keyed request builds the
    /// (session, provider_kind, model) triple and appends a sample whose
    /// `observed_reuse` is `cache_read > 0` (the router owns the reuse
    /// definition, mirroring the ledger rebuild).
    ///
    /// This is a single mutex lock plus a small push; it must never fail,
    /// panic, or meaningfully slow the request, so it returns nothing and
    /// swallows the keyless case silently.
    pub fn record_k_sample(
        &self,
        session_key: Option<&str>,
        provider_kind: &str,
        model: &str,
        cache_read: u64,
        ts: std::time::SystemTime,
    ) {
        let Some(session_key) = session_key else {
            return;
        };
        let key = crate::k_estimator::KSessionKey {
            session_key: session_key.to_string(),
            provider_kind: provider_kind.to_string(),
            model: model.to_string(),
        };
        self.k_session_store.record_sample(
            key,
            crate::k_estimator::Sample {
                ts,
                observed_reuse: cache_read > 0,
            },
        );
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

    /// Non-mutating read of the capacity gate for the seat / model keyed by
    /// `state_key`. Returns `None` when no state slot exists. This is the
    /// `&self`-borrow read surface used by sticky least-loaded selection; it
    /// must never go through the `try_dispatch`-based `breaker_open_for`
    /// anti-pattern, which would claim a half-open probe slot just to read.
    fn capacity_snapshot_for(
        &self,
        state_key: &str,
        now: Instant,
    ) -> Option<crate::runtime_state::CapacitySnapshot> {
        self.state
            .get(state_key)
            .map(|s| s.lock().capacity_snapshot(now))
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

    /// Read-only snapshot of the learned-capability registry: every resident
    /// per-(target, feature) negative in the fixed contract shape. `&self`
    /// delegate over the private `learned_capabilities` registry so the
    /// status surface can surface learned negatives without reaching into
    /// the field.
    pub fn learned_capability_snapshot(
        &self,
    ) -> Vec<crate::learned_capability::LearnedRegistryEntry> {
        self.learned_capabilities.snapshot()
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
    fn dispatch_chain(
        &self,
        model: &str,
        session_key: Option<&str>,
    ) -> Result<Vec<DispatchTarget>> {
        if let Some(chain) = self.resolve_v6_alias(model)? {
            return Ok(self.expand_chain_to_targets(chain, session_key));
        }
        // Wire model could ALSO be a direct nickname.
        if let Some(m) = self.resolve_nickname(model) {
            return Ok(self.expand_chain_to_targets(vec![m], session_key));
        }
        // Catch-all: only consulted after exact alias / glob / direct
        // nickname all miss. This ordering means a wire model that's
        // a known nickname always wins over a configured default.
        if let Some(chain) = self.resolve_default_alias()? {
            return Ok(self.expand_chain_to_targets(chain, session_key));
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
    ///
    /// The post-loop pass also fills `provider_kind` and `class_overrides`
    /// from the target's `[providers.X]` config entry, each only when the
    /// constructor left it empty -- mirroring discipline so a seat target
    /// that already carries its own seat-resolved `provider_kind` (see
    /// `dispatch_target_for_seat`) is never overwritten.
    fn expand_chain_to_targets(
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
                target.provider_kind = provider_entry.map(super::config::ProviderEntry::kind_str);
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
    /// provider, `state_key`, and `auth_secret_ref` so the breaker, RPM
    /// gate, retry caps, probe fast-fail, and the `Retry-After` park all
    /// apply per seat; every other dispatch knob is shared from the model.
    fn push_seat_targets(
        &self,
        m: &Arc<ResolvedModel>,
        seats: &[crate::seat_pool::SeatTarget],
        session_key: Option<&str>,
        out: &mut Vec<DispatchTarget>,
    ) {
        let selection = self
            .config
            .providers
            .get(&m.provider_name)
            .map(|e| e.runtime().seat_selection)
            .unwrap_or_default();

        // Sticky least-loaded only engages with a real session key on a
        // multi-seat pool. Every OTHER case (FillFirst, RoundRobin, or
        // keyless / single-seat StickyLeastLoaded) routes through the
        // existing `seat_order_for_request` path UNCHANGED, so keyless
        // StickyLeastLoaded stays byte-for-byte fill-first.
        //
        // `token` is computed ALONGSIDE the order purely for observability:
        // the order and target set below are byte-for-byte what they were
        // before the token existed. It is `None` for genuinely non-sticky
        // modes (FillFirst, RoundRobin) and single-seat pools, which have no
        // sticky decision to record.
        let (order, token): (Vec<usize>, Option<&'static str>) = match (selection, session_key) {
            (crate::config::SeatSelection::StickyLeastLoaded, Some(key)) if seats.len() > 1 => {
                let (order, tok) = self.sticky_seat_order(seats, key);
                (order, Some(tok))
            }
            // Keyless (or single-seat) StickyLeastLoaded collapses to
            // fill-first; surface that collapse so an operator can spot the
            // silent fill-first regime on a pool configured sticky.
            (crate::config::SeatSelection::StickyLeastLoaded, _) if seats.len() > 1 => (
                crate::seat_pool::seat_order_for_request(
                    &m.nickname,
                    seats.len(),
                    selection,
                    &self.round_robin,
                ),
                Some("keyless_fill_first"),
            ),
            _ => (
                crate::seat_pool::seat_order_for_request(
                    &m.nickname,
                    seats.len(),
                    selection,
                    &self.round_robin,
                ),
                None,
            ),
        };
        let provider_kind = self
            .config
            .providers
            .get(&m.provider_name)
            .map(super::config::ProviderEntry::kind_str);
        let first = out.len();
        for idx in order {
            let seat = &seats[idx];
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

    /// Resolve the sticky least-loaded seat walk order for `key` over a
    /// multi-seat pool. Resolves the pin (with its one-time overflow marker)
    /// FIRST, gathers the per-seat capacity snapshots (one lock each; N is
    /// small and locks are uncontended), then asks the pure selector for the
    /// walk order and a [`SelectionOutcome`]. On a birth it pins the chosen
    /// home (`repinned: false`); on a one-time overflow-repin it pins the new
    /// home (`repinned: true`). A healthy home, an already-repinned home, or a
    /// no-healthy-sibling case stays put with no pin write -- the one-time cap
    /// + hysteresis. Never logs the raw session key.
    ///
    /// Returns the walk order paired with a fixed-vocabulary
    /// `selection_decision` token mapped from the `SelectionOutcome`
    /// (observability only -- the pin writes, logs, and returned order are
    /// byte-for-byte unchanged from before the token was added).
    fn sticky_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
    ) -> (Vec<usize>, &'static str) {
        // A pinned state_key no longer present in this pool resolves to None
        // -> treated as a miss (re-pick), and `repinned` resets to false on
        // the fresh birth -- correct.
        let pin: Option<(usize, bool)> = self.sticky_pins.get(key).and_then(|p| {
            seats
                .iter()
                .position(|s| s.state_key == p.state_key)
                .map(|i| (i, p.repinned))
        });

        let now = Instant::now();
        let snapshots = self.gather_capacity_snapshots(seats, now);

        // Advance the anti-herd counter only when a pick is actually
        // attempted: a miss, or a hit whose home is non-dispatchable and not
        // yet repinned. A sticky-stay does not consume tiebreak.
        let will_attempt_pick = match pin {
            None => true,
            Some((home, repinned)) => !snapshots[home].is_dispatchable() && !repinned,
        };
        let tiebreak = if will_attempt_pick {
            self.sticky_pins.next_tiebreak()
        } else {
            0
        };

        let (order, outcome) = crate::seat_pool::sticky_least_loaded_order(
            seats.len(),
            pin.map(|(i, _)| i),
            pin.is_some_and(|(_, r)| r),
            &snapshots,
            tiebreak,
        );
        let token = self.apply_sticky_outcome(key, seats, outcome);
        (order, token)
    }

    /// Gather the per-seat capacity snapshots for sticky least-loaded
    /// selection (one lock each; N is small and locks are uncontended). The
    /// overflow check needs every seat's snapshot, including the pinned
    /// home's, so this reads ALL seats (both hit and miss).
    fn gather_capacity_snapshots(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        now: Instant,
    ) -> Vec<crate::runtime_state::CapacitySnapshot> {
        seats
            .iter()
            .map(|s| {
                self.capacity_snapshot_for(&s.state_key, now).unwrap_or(
                    // Defensive: a seat with no state slot should never
                    // happen (install creates one per seat). If it does,
                    // fail safe -- treat it as non-dispatchable so it is
                    // excluded from a pick rather than chosen as the most-
                    // attractive home. It still appears in the fallback
                    // order, and the existing gate stays authoritative.
                    crate::runtime_state::CapacitySnapshot {
                        rpm_available: Some(0.0),
                        circuit: crate::runtime_state::CircuitPhase::Open,
                    },
                )
            })
            .collect()
    }

    /// Apply the pin write implied by `outcome` and return the fixed-vocabulary
    /// `selection_decision` token. A birth pins the chosen home
    /// (`repinned: false`); a one-time overflow-repin pins the new home
    /// (`repinned: true`); a stay or no-healthy case writes nothing. Never
    /// logs the raw session key.
    fn apply_sticky_outcome(
        &self,
        key: &str,
        seats: &[crate::seat_pool::SeatTarget],
        outcome: crate::seat_pool::SelectionOutcome,
    ) -> &'static str {
        match outcome {
            crate::seat_pool::SelectionOutcome::Birth { home } => {
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        state_key: seats[home].state_key.clone(),
                        repinned: false,
                    },
                );
                tracing::debug!(
                    state_key = %seats[home].state_key,
                    seat_label = ?seats[home].label,
                    "sticky least-loaded birth pick: pinned session to seat"
                );
                "birth_pick"
            }
            crate::seat_pool::SelectionOutcome::OverflowRepin { home } => {
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        state_key: seats[home].state_key.clone(),
                        repinned: true,
                    },
                );
                tracing::debug!(
                    state_key = %seats[home].state_key,
                    seat_label = ?seats[home].label,
                    "sticky least-loaded overflow-repin: migrated session to healthy sibling"
                );
                "overflow_repin"
            }
            crate::seat_pool::SelectionOutcome::Stay { .. } => "sticky_stay",
            crate::seat_pool::SelectionOutcome::DeferNoHealthy => "defer_no_healthy",
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
    ///
    /// The second tuple element carries the re-probes the filter admitted
    /// (a lapsed learned negative whose single probe slot this request
    /// claimed). Each one MUST be settled by the dispatch path -- success,
    /// same-capability rejection, or other error -- or the entry's
    /// `in_flight` slot latches and the target routes away permanently.
    fn dispatch_chain_for_request(
        &self,
        req: &ChatRequest,
    ) -> Result<(Vec<DispatchTarget>, Vec<ProbeAdmission>)> {
        let chain = self.dispatch_chain(
            &req.model,
            req.routectl_internal.inbound_session_key.as_deref(),
        )?;
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let features =
            crate::feature_keys::derive_feature_keys(tools, req.provider_extras.as_ref());
        let mut admissions = Vec::new();
        let chain = self.filter_chain_by_features(chain, &features, &req.model, &mut admissions)?;
        Ok((chain, admissions))
    }

    /// Filter the resolved chain by request features. Per-provider
    /// `unsupported_features` lists are consulted via the provider
    /// table; the per-model list is carried on the target. An entry
    /// whose union of those two lists intersects the request feature
    /// set is dropped with a DEBUG log (tagging the matching source).
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
        admissions: &mut Vec<ProbeAdmission>,
    ) -> Result<Vec<DispatchTarget>> {
        if features.is_empty() || chain.is_empty() {
            return Ok(chain);
        }
        // SOFT-DROP: a static (provider / model) match hard-drops the
        // target; a learned match moves it to a de-prioritized tail. The
        // result is [supported...] ++ [learned tail], each in original
        // chain order.
        let mut supported: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        let mut tail: Vec<DispatchTarget> = Vec::new();
        let mut route_aways: Vec<(String, String)> = Vec::new();
        for mut target in chain {
            let mut strip_keys: Vec<String> = Vec::new();
            match self.unsupported_feature_for_target(
                &target,
                features,
                admissions,
                &mut strip_keys,
            ) {
                None => {
                    // Strip-in-place verdict: every acting negative on this
                    // target is a droppable, non-pinned capability, so it
                    // STAYS in `supported` (no tail demotion) carrying the
                    // sorted normalized keys the interceptor will strip.
                    if !strip_keys.is_empty() {
                        target.strip_capabilities = std::sync::Arc::from(strip_keys);
                    }
                    supported.push(target);
                }
                Some((feature, FilterSource::Learned)) => {
                    // The route_away event is deferred until the final chain
                    // shape is known: its level distinguishes "an alternative
                    // remains" (INFO) from "the request survives only on the
                    // learned tail" (WARN).
                    route_aways.push((target.state_key.clone(), feature));
                    tail.push(target);
                }
                Some((feature, source)) => {
                    tracing::debug!(
                        provider = %target.provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        capability_key = %feature,
                        source = %source.as_str(),
                        "target skipped: capability in unsupported_features list",
                    );
                }
            }
        }
        // NotImplemented fires ONLY when the static lists hard-dropped
        // every entry (nothing survived, not even the learned tail).
        if supported.is_empty() && tail.is_empty() {
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
        // Route-away observability: each learned-tail demotion emits one
        // route_away event. INFO while a supported alternative remains; WARN
        // when the chain survives ONLY via the de-prioritized learned tail
        // (the route-away floor). Capability TOKEN + state_key only -- never a
        // request body.
        let tail_only = supported.is_empty();
        if tail_only {
            self.metrics.incr_d17_tail();
        }
        for (state_key, capability_key) in route_aways {
            if tail_only {
                tracing::warn!(
                    event = "route_away",
                    state_key = %state_key,
                    capability_key = %capability_key,
                    "learned-capability negative routed this target away; request \
                     survives only via the de-prioritized learned tail",
                );
            } else {
                tracing::info!(
                    event = "route_away",
                    state_key = %state_key,
                    capability_key = %capability_key,
                    "learned-capability negative de-prioritized this target to the tail",
                );
            }
        }
        supported.extend(tail);
        Ok(supported)
    }

    /// The single decision site for "is any requested feature
    /// unsupported for this target, and by which source". Returns the
    /// FIRST matched `(feature, source)` or `None` if the target
    /// supports every requested feature.
    ///
    /// The union is over the operator-override registry plus the learned
    /// registry. The override consult flattens the legacy per-PROVIDER and
    /// per-MODEL `unsupported_features` lists and the
    /// `[capability.overrides]` table into one provenance-preserving
    /// read-model; a `RouteAway` verdict of ANY provenance is consulted
    /// FIRST so it hard-drops (and reports its preserved source label --
    /// `provider`, `model`, or `override`) ahead of any learned signal.
    /// When the kill switch is on, a non-expired acting learned negative
    /// for this `(state_key, feature)` is consulted after.
    ///
    /// A `ForceSupported` override masks a feature: it short-circuits that
    /// feature to Allow BEFORE the learned consult, suppressing an acting
    /// learned negative and -- because the mask precedes probe-admission
    /// logic (the `in_flight` flip happens inside `acting_negative_for`) --
    /// ensuring a masked cell never claims a re-probe slot.
    ///
    /// The learned consult is admission-bearing: an expired negative whose
    /// re-probe slot this caller claims returns `None` (route to the target
    /// and test it), counting the probe attempt as a side effect and
    /// recording the claim in `admissions` so the dispatch path can settle
    /// it. The `in_flight` flip itself happens inside `acting_negative_for`.
    ///
    /// Strip-vs-route verdict: when the learned pass finds acting negatives,
    /// each is classified by [`capability_strip::action_for`]. If EVERY
    /// acting negative is a droppable `Strip` capability that no operator
    /// beta floor pins to the wire, the target is NOT unsupported -- it
    /// returns `None` and the strip keys land in `strip_keys` (sorted,
    /// normalized) for the caller to attach. If ANY acting negative maps to
    /// `RouteAway` or is operator-pinned, the whole target routes away
    /// (`Some((feature, Learned))`, `strip_keys` left empty) -- a target is
    /// never half-stripped. An admitted re-probe is excluded from
    /// `strip_keys` (the full request tests the real capability); its
    /// admission still reaches `admissions`. Override `RouteAway` matches
    /// hard-drop FIRST, ahead of any learned or strip decision.
    fn unsupported_feature_for_target(
        &self,
        target: &DispatchTarget,
        features: &[FeatureKey],
        admissions: &mut Vec<ProbeAdmission>,
        strip_keys: &mut Vec<String>,
    ) -> Option<(FeatureKey, FilterSource)> {
        // Override consult replaces the two raw static-list scans: the
        // registry (built from the legacy provider / model
        // `unsupported_features` lists plus `[capability.overrides]`)
        // hard-drops on a `RouteAway` of ANY provenance, reporting the
        // preserved source label so an existing config's behavior and
        // labels stay byte-identical.
        let nickname = target.nickname.as_deref().unwrap_or("");
        for feature in features {
            if let Some((crate::override_registry::OverrideVerdict::RouteAway, provenance)) =
                self.override_registry.resolve(
                    &target.provider_name,
                    nickname,
                    feature,
                    target.provider_kind.unwrap_or(""),
                )
            {
                return Some((feature.clone(), provenance.into()));
            }
        }
        // Learned pass: consult the adaptive registry only when the kill
        // switch is on and the target carries a provider kind (legacy /
        // direct-construction targets without one skip the registry).
        // Scan EVERY feature: an earlier feature's `ProbeAdmitted` must not
        // short-circuit a later feature's `RouteAway`, and `acting_negative_for`
        // flips `in_flight` as a side effect on `ProbeAdmitted`, so every
        // admission has to reach `admissions` for its guard to settle the slot
        // -- dropping one leaks `in_flight` and blocks that feature from ever
        // re-probing. Any `RouteAway` tail-drops the target after the full scan.
        if self.config.capability.enabled
            && let Some(provider_kind) = target.provider_kind
        {
            let now = Instant::now();
            let mut route_away: Option<FeatureKey> = None;
            let mut strip: Vec<String> = Vec::new();
            for feature in features {
                // ForceSupported mask: an operator `force_supported`
                // override short-circuits this feature to Allow BEFORE
                // `acting_negative_for` runs, so a masked cell never
                // suppresses only the verdict while still claiming a
                // re-probe slot (the `in_flight` flip happens inside
                // `acting_negative_for`). A `RouteAway` override can never
                // reach here -- it hard-dropped in the consult above.
                if self.override_forces_supported(target, feature, provider_kind) {
                    continue;
                }
                match self.learned_capabilities.acting_negative_for(
                    &target.state_key,
                    feature,
                    provider_kind,
                    now,
                ) {
                    crate::learned_capability::RoutingDecision::RouteAway(_) => {
                        // Strip-vs-route: a droppable capability the operator
                        // has not pinned to the wire is stripped in place;
                        // everything else (essentials, unknowns, pinned betas)
                        // routes away. A pinned strip would be re-added
                        // downstream, so its "success" is false -- route away.
                        if matches!(
                            crate::capability_strip::action_for(feature),
                            crate::capability_strip::CapabilityAction::Strip(_)
                        ) && !self.beta_pinned_for_target(target, feature)
                        {
                            strip.push(normalize_capability_key(feature, provider_kind));
                        } else if route_away.is_none() {
                            route_away = Some(feature.clone());
                        }
                    }
                    crate::learned_capability::RoutingDecision::ProbeAdmitted => {
                        self.metrics.incr_probe_attempts();
                        let normalized = normalize_capability_key(feature, provider_kind);
                        // Probe bypass: the admitted feature tests the REAL
                        // capability on the full request, so it is never
                        // stripped -- a stripped success would falsely clear
                        // the negative the probe is meant to re-verify. When
                        // the bypassed feature WOULD otherwise have been
                        // stripped in place (a droppable `Strip` the operator
                        // has not pinned to the wire -- the exact condition the
                        // `RouteAway` arm strips on), surface it: the strip WARN
                        // vocabulary's `probe_bypassed` outcome fires here, with
                        // the same field shape as the per-decision WARN in
                        // `apply_strip_interceptor`. Route-away features do not
                        // fire -- they were never strip-eligible. Capability
                        // TOKEN and state_key only -- never request bodies.
                        if matches!(
                            crate::capability_strip::action_for(feature),
                            crate::capability_strip::CapabilityAction::Strip(_)
                        ) && !self.beta_pinned_for_target(target, feature)
                        {
                            tracing::warn!(
                                event = "strip",
                                state_key = %target.state_key,
                                capability_key = %normalized,
                                outcome = "probe_bypassed",
                                "capability_strip_decision",
                            );
                        }
                        admissions.push(ProbeAdmission {
                            state_key: target.state_key.clone(),
                            feature: normalized,
                            provider_kind,
                        });
                    }
                    crate::learned_capability::RoutingDecision::Allow => {}
                }
            }
            // ANY route-away (or operator-pinned) acting negative demotes the
            // whole target; the strip set is abandoned so a mixed target is
            // never half-stripped-half-routed.
            if let Some(feature) = route_away {
                return Some((feature, FilterSource::Learned));
            }
            if !strip.is_empty() {
                strip.sort_unstable();
                strip.dedup();
                *strip_keys = strip;
            }
        }
        None
    }

    /// Whether stripping `feature` on this target would be silently undone
    /// on the wire by an operator beta floor. A `Strip(BetaFlag)`
    /// capability's beta token can be pinned by the provider `anthropic_beta`
    /// config (Bedrock, re-added post-strip) or a provider/model
    /// `header_extras` `anthropic-beta` contribution (Anthropic-API,
    /// re-added via `operator_betas`). Either source makes the strip
    /// ineffective, so the caller must route away instead. Non-beta strips
    /// (e.g. a tool-shape strip) carry no beta token and are never pinned.
    fn beta_pinned_for_target(&self, target: &DispatchTarget, feature: &str) -> bool {
        let tokens = crate::capability_strip::strip_beta_tokens(feature);
        if tokens.is_empty() {
            return false;
        }
        let provider_entry = self.config.providers.get(&target.provider_name);
        let provider_floor =
            provider_entry.map_or(&[][..], super::config::ProviderEntry::anthropic_beta_floor);
        let header_floor = operator_betas(
            provider_entry.map(super::config::ProviderEntry::header_extras),
            &target.model.header_extras,
        );
        tokens.iter().any(|token| {
            provider_floor.iter().any(|pinned| pinned == token)
                || header_floor.iter().any(|pinned| pinned == token)
        })
    }

    /// Whether an operator `force_supported` override masks `feature` for
    /// this target -- the single consult shared by the act side (which
    /// short-circuits a masked feature to Allow before probe admission) and
    /// the learn side (which suppresses the observe for a masked cell). The
    /// same `(provider, nickname)` two-tier resolve both paths key on, so a
    /// mask is never honored on one side and missed on the other.
    fn override_forces_supported(
        &self,
        target: &DispatchTarget,
        feature: &str,
        provider_kind: &str,
    ) -> bool {
        matches!(
            self.override_registry.resolve(
                &target.provider_name,
                target.nickname.as_deref().unwrap_or(""),
                feature,
                provider_kind,
            ),
            Some((crate::override_registry::OverrideVerdict::ForceSupported, _))
        )
    }

    /// Run the single request interceptor over one per-attempt clone and
    /// map its outcome to a loop-actionable [`StripDecision`], emitting the
    /// per-decision observability (a structured WARN per capability key
    /// plus the matching `RouterMetrics` counter).
    ///
    /// Called at all three dispatch paths immediately after
    /// `apply_layered_overlays` and before context reduction / auto-cache,
    /// so the bytes reduction, cache planning, and dispatch observe are the
    /// stripped bytes, and the strip runs downstream of the beta floor. The
    /// caller's original `req` is never passed here -- only `attempt_req`.
    ///
    /// `target.strip_capabilities` is consumed as-is: it is empty unless an
    /// acting learned negative resolved to a non-pinned droppable, so a
    /// disabled kill switch (or a probe-admitted / operator-pinned feature)
    /// leaves this inert by construction. The keys arrive already sorted
    /// and normalized.
    fn apply_strip_interceptor(
        &self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
    ) -> StripDecision {
        if target.strip_capabilities.is_empty() {
            return StripDecision::Proceed;
        }
        let strict = self.config.server.strict_translation;
        let ctx = StripContext {
            keys: target.strip_capabilities.to_vec(),
            strict,
        };
        let outcome = StripInterceptor.apply(attempt_req, &ctx);
        let outcome_token = match &outcome {
            Outcome::Stripped => "applied",
            Outcome::Unchanged => "noop",
            Outcome::Reject(_) if strict => "strict_rejected",
            Outcome::Reject(_) => "validation_rolled_back",
        };
        // One WARN per strip decision. `capability_key` names the verdict's
        // keys (already sorted + normalized); the outcome is the run-level
        // decision, so joining avoids misreporting a per-key outcome the
        // aggregate `Outcome` cannot distinguish. Capability TOKEN and
        // state_key only -- never request bodies (log hygiene).
        // `probe_bypassed` is emitted upstream at the verdict site (an
        // admitted feature never reaches this verdict -- it arrives empty).
        // `disabled` has no per-decision emission: a disabled kill switch
        // skips the verdict entirely, so no per-decision context exists to
        // name.
        tracing::warn!(
            event = "strip",
            state_key = %target.state_key,
            capability_key = %target.strip_capabilities.join(", "),
            outcome = outcome_token,
            "capability_strip_decision",
        );
        match outcome {
            Outcome::Stripped => {
                self.metrics.incr_strip();
                StripDecision::Proceed
            }
            Outcome::Unchanged => StripDecision::Proceed,
            Outcome::Reject(err) if strict => {
                self.metrics.incr_strip_strict_rejected();
                StripDecision::StrictReject(err)
            }
            Outcome::Reject(err) => {
                self.metrics.incr_strip_rollback();
                StripDecision::RouteAway(err)
            }
        }
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
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
        // Re-probes the chain filter admitted, owned by a request-scoped set
        // that settles them on transfer or on drop. `take(state_key)` moves a
        // target's admissions into its `LearnedProbeGuard` when the loop
        // reaches the target; any admission still held when the set drops (an
        // earlier target succeeded, a terminal error, a `break 'chain`, a
        // client disconnect) was never reached and settles as `OtherError`, so
        // its `in_flight` slot never latches.
        let mut probe_set = ProbeAdmissionSet::new(
            self.learned_capabilities.clone(),
            probe_admissions,
            DispatchSurface::Complete.as_str(),
        );
        // Whether the CLIENT'S request carried a captured forwarded
        // bearer, computed ONCE before the chain loop (mirroring
        // `is_probe` below). This is a request-global observability
        // dimension ONLY (fed to `emit_class_observability`) -- the
        // per-target `use_forwarded_credential` flag, not this, drives
        // the model-rewrite bypass and the 401/403/429 terminal re-key,
        // so a coexisting own-credential Anthropic target in the same
        // chain keeps its normal auth recovery even on a request that
        // also carries a forwarded bearer.
        let is_forwarded = req.routectl_internal.forwarded_bearer.is_some();
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        let hard_cap = policy.hard_retry_cap();
        // Availability-probe detection, computed ONCE: `max_tokens` is
        // stable across the chain (overlays never touch it). Claude
        // Code sends max_tokens=1 probes whose output is unread; on a
        // 429/529 these fast-fail instead of spraying retry+fallback
        // across the all-Anthropic chain. See `should_fallback`.
        let is_probe = is_probe_request(&req, &policy);
        // Auto-cache decision inputs: computed ONCE off the ORIGINAL req
        // (above the chain loop) so they are identical across every retry
        // and fallback target. The per-target capability + provider switch
        // are looked up inside the loop; the request-level facts here do
        // not vary by target.
        let auto_cache_plan =
            AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
        let mut last_err: Option<Error> = None;
        // One learned-capability observation per request per
        // (state_key, feature): the error arm fires per attempt, so this
        // set stops a same-request retry from manufacturing a second
        // observation. See `observe_for_learning`.
        let mut learn_dedupe: HashSet<(String, String)> = HashSet::new();

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            // Learned re-probe settle guard for THIS target, scoped to the
            // whole chain-iteration (persists across same-provider retries,
            // drops -> OtherError on any exit). Inert unless the filter
            // admitted at least one probe for this state_key; the move takes
            // ownership out of the set so the set's drop cannot settle it again.
            let mut learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
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
            meta.mark_target(target, &req.model);
            meta.fallback_count = chain_idx as u32;

            // Missing-bearer terminal guard: a forwarded target with NO
            // captured client bearer must fail cleanly BEFORE egress --
            // never an ambiguous upstream 401. Per-target (checked here,
            // inside the loop), so a mixed chain that never reaches this
            // target is unaffected.
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                return Err(err);
            }

            let mut attempt_req = req.clone();
            // A forwarded target keeps the client's requested model
            // verbatim (model transparency); an own target rewrites to
            // this target's `upstream` as before.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
            }
            // v0.6: layered config compose. The provider's
            // header_extras + payload_extras are looked up by
            // provider_name; the model's contribution lives on the
            // dispatch target.
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK: strip runs after layered config compose,
            // before auto-cache / context reduction. Any future interceptor
            // over the per-attempt canonical clone goes here, ordered by
            // data-dependency. A strict-mode refusal returns the 400 for
            // this attempt; a rolled-back post-strip hazard routes away
            // (advance the chain as an ordinary route-away would).
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => return Err(err),
                StripDecision::RouteAway(err) => {
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            let provider_cfg = self.config.providers.get(provider_name);
            // Context reduction: runs strictly AFTER overlays and strictly
            // BEFORE auto-cache so any auto-emitted breakpoint covers the
            // REDUCED bytes. `apply_json_minify` computes the caller frozen
            // floor itself (off this pre-auto-emit clone) and only touches
            // the mutable tail; it fails closed (no mutation on any doubt).
            // Effective only when the global switch is on AND the provider
            // did not explicitly opt out (`None` inherits the global).
            let reduction_effective = self.config.reduction.enabled
                && provider_cfg.and_then(super::config::ProviderEntry::reduction_enabled)
                    != Some(false);
            let reduction_outcome = if reduction_effective {
                Some(apply_json_minify(&mut attempt_req))
            } else {
                None
            };
            let reduction_token =
                reduction_strategy_token(reduction_effective, reduction_outcome.as_ref());
            meta.reduction_strategy = Some(reduction_token);
            if let Some(ReductionOutcome::Applied(delta)) = &reduction_outcome {
                tracing::debug!(
                    provider = %provider_name,
                    model = %model,
                    strategy = reduction_token,
                    strings_minified = delta.strings_minified,
                    bytes_saved = delta.bytes_saved,
                    est_tokens_saved = delta.est_tokens_saved,
                    "context_reduction",
                );
            }
            // Auto-cache: maybe inject a top-level cache_control breakpoint
            // on THIS per-attempt clone, after overlays (the last
            // dispatch-time touch of cache_control) and before the retry
            // loop, so every retry on this target reuses identical bytes.
            // Never mutates the original `req`; any doubt sends un-injected.
            let cache_injection = maybe_apply_auto_cache_control(
                &mut attempt_req,
                &auto_cache_plan,
                provider_cfg.map(super::config::ProviderEntry::cache_capability),
                provider_cfg
                    .and_then(super::config::ProviderEntry::auto_emit_top_level_breakpoint)
                    .unwrap_or(true),
            );
            // T6 observability: stamp the per-request decision token so the
            // usage DB and the outcome log can see what was decided, and
            // emit the per-request decision at debug. No bodies / secrets:
            // only provider name, model id, and the stable strategy token.
            let strategy_token = cache_injection.strategy_str();
            meta.cache_strategy = Some(strategy_token);
            tracing::debug!(
                provider = %provider_name,
                model = %model,
                strategy = strategy_token,
                "cache_auto_decision",
            );

            // Steady-state would-trim advisory: NON-MUTATING. Reads the
            // dispatched clone, prices the would-cut candidate, and records it
            // onto `meta`. Never touches `attempt_req` (no `apply_trim_plan`),
            // so what is sent upstream is byte-identical with or without this.
            self.record_would_trim(
                &attempt_req,
                target.provider_kind,
                model,
                target.nickname.as_deref().unwrap_or(model),
                &target.model.effective_row,
                meta,
            );

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

                // Cancellation backstop (see ProbeSlotGuard): if the gate
                // admitted THIS dispatch as the half-open probe, free the slot
                // should the future be dropped before an outcome arm settles
                // it. Disarmed at each settle below; inert + a no-op otherwise.
                let mut probe_guard = self.probe_slot_guard(state_key);

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
                        probe_guard.disarm();
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
                        // A 2xx proves the capability is not rejected: clear
                        // any learned negative this dispatch re-probed.
                        learned_probe_guard.settle_success();
                        return Ok(resp);
                    }
                    Err(e) => {
                        let native_cf = classify(&e, target.provider_kind);
                        let original_class = native_cf.class.clone();
                        let remap_candidate_status = upstream_status_for_remap(&e);
                        let (cf, remapped) =
                            apply_remap(native_cf, remap_candidate_status, &target.class_overrides);
                        let remap_status = if remapped {
                            remap_candidate_status
                        } else {
                            None
                        };
                        // A forwarded-credential TARGET that drew
                        // an upstream 401/403/429 is TERMINAL -- bypass BOTH
                        // the on_auth_failure refresh-and-retry (below) AND
                        // the fallback-chain hop, and surface the status
                        // verbatim. A request-scoped forwarded token has no
                        // refresh path and no credential to fall back to, so
                        // both recoveries are useless and wrong; the client
                        // owns its own retry/backoff. Keyed off the CURRENT
                        // target's `use_forwarded_credential`, not
                        // request-global bearer presence, so a coexisting
                        // own-creds Anthropic target in the same chain keeps
                        // normal auth recovery. Checked FIRST so it
                        // precedes auth-retry, same-provider retry, and
                        // fallback. Releases the half-open probe slot WITHOUT
                        // a breaker debit (mirrors the terminal path below):
                        // a forwarded-token failure is not this seat's health
                        // signal, so it must not trip routectl's breaker.
                        // Transport retries (5xx/network) are NOT in this set
                        // and fall through unchanged.
                        if target.use_forwarded_credential
                            && let Some(status) = forwarded_terminal_status(&e)
                        {
                            log_forwarded_auth_terminal(
                                status,
                                req.routectl_internal.inbound_session_key.is_some(),
                            );
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            return Err(e);
                        }
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
                                probe_guard.disarm();
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
                            probe_guard.disarm();
                            continue;
                        }
                        let do_fallback = should_fallback(&e, &cf.class, &policy, is_probe);
                        // Probe fast-fail: a probe (max_tokens <=
                        // probe_max_tokens) that hit a rate-limit/overload
                        // (429/529) returns the status immediately via an
                        // explicit early return -- no retry, no fallback,
                        // no breaker failure debit. The early return below
                        // precedes the debit site, so a probe 429/529 never
                        // reaches health accounting at all. It does still
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
                            probe_guard.disarm();
                            return Err(e);
                        }
                        // The honored upstream reset for THIS error (clamped
                        // to the ceiling), computed once for both the park
                        // decision below and the in-loop sleep bump in the
                        // retry branch. `None` for every non-rate-limit error.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        // The breaker DEBIT keys off the failure CLASS, not
                        // the chain-hop decision: a seat looks unhealthy when
                        // it emits a transient-health failure (429 / 5xx /
                        // status-0 / overload), independent of whether the
                        // operator routes past that status. A non-debiting
                        // class (4xx caller error, capability, auth) that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
                        // Whether THIS attempt force-opened (parked) the
                        // breaker. When it did, the next in-loop gate is
                        // guaranteed CircuitOpen, so a same-provider retry
                        // would be pure waste -- and is exactly the path that
                        // used to discard the genuine error and let the
                        // synthetic status-0 gate error surface in its place.
                        let mut breaker_parked = false;
                        let mut breaker_effect = "none";
                        if debits {
                            match reset_hint {
                                // Non-probe LARGE reset: park the provider for
                                // the honored duration (force_open) instead of
                                // a threshold-gated debit, so an exhausted seat
                                // is skipped until it actually resets. The
                                // in-loop re-gate then diverts to fallback /
                                // fail. Probes never park, and a small
                                // reset is honored as an in-loop sleep (below),
                                // so only the large non-probe case parks here.
                                Some(h) if !is_probe && h > INLOOP_RETRY_AFTER_CAP => {
                                    self.park_provider(state_key, h);
                                    breaker_parked = true;
                                    breaker_effect = "parked";
                                }
                                _ => {
                                    if self.record_failure_opened(state_key) {
                                        breaker_effect = "opened";
                                    }
                                }
                            }
                            probe_guard.disarm();
                        }
                        self.emit_class_observability(
                            &e,
                            &cf,
                            &original_class,
                            remapped,
                            remap_status,
                            DispatchSurface::Complete,
                            provider_name,
                            target,
                            do_fallback,
                            &policy,
                            debits,
                            is_probe,
                            is_forwarded,
                        );
                        self.observe_for_learning(
                            &e,
                            &cf,
                            remapped,
                            target,
                            is_forwarded,
                            &req,
                            &mut learn_dedupe,
                            meta,
                            &mut learned_probe_guard,
                        );
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when a
                            // debiting class already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            return Err(e);
                        }
                        let can_retry_here = attempts_made < hard_cap
                            && should_retry_same_provider(
                                &e,
                                &cf.class,
                                &policy,
                                attempts_made,
                                is_probe,
                            )
                            && !breaker_parked;
                        let facts = upstream_facts(&e);
                        tracing::debug!(
                            provider = provider_name,
                            state_key = %state_key,
                            surface = DispatchSurface::Complete.as_str(),
                            attempt = attempts_made,
                            status = ?facts.status,
                            upstream_type = ?facts.upstream_type,
                            retry_after_ms = ?reset_hint.map(|d| d.as_millis()),
                            breaker_effect,
                            same_provider_retry = can_retry_here,
                            preserved_upstream_error = facts.status.is_some(),
                            "retry decision",
                        );
                        if can_retry_here {
                            tracing::debug!(
                                provider = provider_name,
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                error = ?e,
                                "retrying same provider",
                            );
                            // Keep the genuine upstream error as the running
                            // last_err before re-probing. If the re-gate then
                            // refuses (CircuitOpen / RPM), the `last_err.is_none()`
                            // guard above leaves this real error in place rather
                            // than overwriting it with the synthetic status-0
                            // gate error, so the client sees the true failure.
                            last_err = Some(e);
                            // Honor a SMALL non-probe upstream reset as the
                            // next in-loop sleep: bump `backoff` so the
                            // loop-top sleep waits at least the reset before
                            // re-probing the SAME provider. Only when we were
                            // already going to retry here (can_retry_here is
                            // unchanged), only for a reset within the
                            // in-loop cap (a larger reset parked the provider
                            // above instead of blocking this thread), and never
                            // for a probe.
                            if let Some(h) = reset_hint
                                && !is_probe
                                && h <= INLOOP_RETRY_AFTER_CAP
                            {
                                backoff = backoff.max(h);
                            }
                            // Free the half-open probe slot this attempt
                            // claimed before re-probing: the in-loop re-gate
                            // re-runs `try_dispatch`, which would otherwise see
                            // this caller's still-held slot as
                            // `half_open_in_flight` and return CircuitOpen,
                            // locking the breaker open forever (mirrors the
                            // auth-retry Ok path).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
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
                            // A fallbackable error whose class did NOT debit
                            // the breaker leaves the half-open probe slot
                            // armed; release it without a debit before the hop
                            // so every settle path frees the slot exactly once.
                            // A debiting class already settled + disarmed above.
                            if !debits {
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        probe_guard.disarm();
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Probe call: route a request to a count_tokens-CAPABLE provider in
    /// the dispatch chain and call `Provider::count_tokens`. Used by
    /// claude-code's context-budget display via the
    /// `/v1/messages/count_tokens` endpoint.
    ///
    /// Capability walk (not a try-and-fallback over health): the chain is
    /// scanned for targets whose `provider_kind == "anthropic-api"` -- the
    /// only count_tokens-capable egress kind (it is the only kind that
    /// overrides `Provider::count_tokens`; every other kind uses the trait
    /// default that 501s). Incapable-by-kind targets are skipped BEFORE
    /// dispatch (DEBUG log, no upstream call, no breaker account); a
    /// kind-skip is operator-known config, not upstream health, so it must
    /// not touch the breaker. This mirrors `filter_chain_by_features`
    /// discipline.
    ///
    /// Why walking is safe for tokenizer correctness: `anthropic-api` is
    /// Claude-only, so every capable target the walk can select uses the
    /// SAME Anthropic tokenizer family. Walking therefore never
    /// reintroduces the wrong-tokenizer hazard -- it only steps over kinds
    /// or seats that cannot count.
    ///
    /// Once a CAPABLE target is selected, the outcome decides the walk:
    ///
    /// - `Ok` -> return the count.
    /// - A CAPABILITY error (`is_capability_error`: local
    ///   `NotImplemented`, or a WIRE 501 from a capable-by-kind seat whose
    ///   upstream cannot count) -> release the probe slot WITHOUT debiting
    ///   the breaker, then advance to the NEXT capable seat in the
    ///   already-resolved chain. This is the incident fix: a
    ///   count_tokens-only capability signal must never be recorded as
    ///   health on the per-seat breaker that completions gate on.
    /// - A 401 -> single-flight `on_auth_failure` refresh + one retry of
    ///   the SAME seat.
    /// - Any OTHER fallbackable HEALTH error (429 / 5xx / status-0) ->
    ///   debit-or-park and propagate (NO walk -- health fallback stays
    ///   reserved for the messages path).
    /// - A non-fallbackable 4xx -> release the probe slot and propagate.
    ///
    /// The walk is bounded and single-visit: each seat is dispatched to at
    /// most once (plus at most one 401 auth-retry of that same seat), so
    /// total upstream calls never exceed `2 * chain.len()`. When no
    /// capable seat serves a count (none capable, or every capable seat
    /// returned a capability error), this returns
    /// `Error::NotImplemented` naming the alias -- the handler maps that to
    /// a stable 501, and the last upstream's raw 501 body is never leaked
    /// to the client.
    ///
    /// count_tokens calls consume the same RPM bucket and honor the same
    /// circuit breaker as messages calls: the gate runs on EACH seat
    /// before its upstream is touched, so a walk cannot fan across seats
    /// to bypass an operator rate limit or an open breaker.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
        // A token-count is not a messages-capability test, so a re-probe the
        // filter admitted here settles OtherError: release the in_flight slot
        // and leave the entry expired for the next real request to re-probe,
        // never latching it in flight.
        let now = Instant::now();
        for admission in probe_admissions {
            self.learned_capabilities.record_probe_outcome(
                &admission.state_key,
                &admission.feature,
                admission.provider_kind,
                crate::learned_capability::ProbeOutcome::OtherError,
                now,
            );
        }
        let mut saw_capable = false;
        for candidate in chain {
            if candidate.provider_kind != Some(COUNT_TOKENS_CAPABLE_KIND) {
                tracing::debug!(
                    provider = %candidate.provider_name,
                    kind = candidate.provider_kind.unwrap_or("unknown"),
                    model = %candidate.nickname.as_deref().unwrap_or(""),
                    "provider skipped: kind cannot count_tokens",
                );
                continue;
            }
            saw_capable = true;
            match self.count_tokens_try_seat(&req, candidate).await {
                CountSeatOutcome::Count(tc) => return Ok(tc),
                CountSeatOutcome::Terminal(e) => return Err(e),
                // Capability error: the seat is capable-by-kind but its
                // upstream cannot count. The slot was already released
                // without a breaker debit; advance to the next capable
                // seat in the already-resolved chain (single-visit,
                // never re-resolved or re-queued).
                CountSeatOutcome::Capability => continue,
            }
        }
        // Two distinct terminal shapes, both mapping to a 501 at the
        // handler: no capable-by-kind seat existed at all, versus capable
        // seats existed but every one returned a capability error.
        let detail = if saw_capable {
            "count_tokens: all capable providers returned a capability error (cannot count)"
        } else {
            tracing::warn!(
                alias = %req.model,
                "alias chain has no count_tokens-capable provider; \
                 no target in chain overrides count_tokens",
            );
            "count_tokens: no count_tokens-capable provider in chain"
        };
        Err(Error::NotImplemented(req.model.clone(), detail.into()))
    }

    /// Dispatch `count_tokens` to ONE already-selected capable seat and
    /// classify the outcome for the walk in [`Router::count_tokens`].
    ///
    /// PROBE-SLOT INVARIANT: on every exit the half-open slot this seat
    /// claimed at the gate is settled exactly once, and `probe_guard`
    /// is disarmed only AFTER that settle (never before, never instead).
    /// A `Capability` return releases the slot BEFORE returning, so the
    /// caller's next-seat gate can claim a fresh slot without contending
    /// with this seat's. `auth_retry_attempted` is a fresh per-seat local,
    /// so advancing to a new seat resets it -- safe because seats are
    /// single-visit.
    async fn count_tokens_try_seat(
        &self,
        req: &ChatRequest,
        target: DispatchTarget,
    ) -> CountSeatOutcome {
        let provider = match target.provider.clone() {
            Some(p) => p,
            None => {
                return CountSeatOutcome::Terminal(Error::UnknownProvider(
                    target.provider_name.clone(),
                ));
            }
        };
        let provider_name = target.provider_name.as_str();
        let model_label = target.nickname.as_deref().unwrap_or("");

        // Missing-bearer terminal guard (see `complete_inner`): a
        // forwarded seat with NO captured client bearer must fail
        // cleanly before any upstream touch, never an ambiguous
        // upstream 401.
        if let Some(err) = missing_forwarded_bearer_error(&target, req) {
            return CountSeatOutcome::Terminal(err);
        }

        // Apply the same per-attempt overlays the messages path does so
        // header_extras / payload_extras are consistent -- notably the
        // `anthropic-beta` surface count_tokens must observe or the
        // upstream may reject a request /v1/messages would accept.
        let mut attempt_req = req.clone();
        // See `complete_inner`: a forwarded target keeps the client's
        // requested model verbatim instead of rewriting to `upstream`.
        if !target.use_forwarded_credential {
            attempt_req.model = target.upstream.clone();
        }
        apply_layered_overlays(&self.config, &target, &mut attempt_req);
        // INTERCEPTOR HOOK (see `complete_inner`): strip runs after layered
        // config compose so the estimated prefix matches the shipped
        // prefix. Strict refusal is a terminal 400; a rolled-back hazard
        // advances to the next capable seat (the count_tokens route-away).
        match self.apply_strip_interceptor(&target, &mut attempt_req) {
            StripDecision::Proceed => {}
            StripDecision::StrictReject(err) => return CountSeatOutcome::Terminal(err),
            StripDecision::RouteAway(_) => return CountSeatOutcome::Capability,
        }

        let mut auth_retry_attempted = false;
        let mut attempts_made: u32 = 0;
        loop {
            // Per-attempt gate: rate limit + circuit breaker. Runs on THIS
            // seat before its upstream is touched (and again on the 401
            // retry), so a capability walk cannot fan across seats to
            // bypass an operator rate limit or an open breaker.
            if let Some((gate_kind, gate_err)) =
                self.gate_check(&target.state_key, &target.provider_name)
            {
                tracing::warn!(
                    provider = %target.provider_name,
                    model = %model_label,
                    gate_kind,
                    error = ?gate_err,
                    "count_tokens gate blocked",
                );
                return CountSeatOutcome::Terminal(gate_err);
            }

            // Cancellation backstop (see ProbeSlotGuard): free the
            // half-open probe slot if this future is dropped before an
            // outcome arm settles it.
            let mut probe_guard = self.probe_slot_guard(&target.state_key);

            let result = provider.count_tokens(attempt_req.clone()).await;
            attempts_made += 1;
            match result {
                Ok(tc) => {
                    self.record_success(&target.state_key);
                    probe_guard.disarm();
                    return CountSeatOutcome::Count(tc);
                }
                Err(e) => {
                    // A forwarded-credential 401/403/429 is TERMINAL
                    // -- bypass the on_auth_failure refresh (below) AND any
                    // health park/debit, and surface verbatim as a Terminal
                    // outcome (count_tokens never WALKS on health errors, so
                    // "no fallback" here means also no breaker debit/park).
                    // Keyed off the TARGET's `use_forwarded_credential`, not
                    // request-global bearer presence (see `complete_inner`
                    // for the full rationale). Release the half-open slot
                    // without a breaker debit.
                    if target.use_forwarded_credential
                        && let Some(status) = forwarded_terminal_status(&e)
                    {
                        log_forwarded_auth_terminal(
                            status,
                            req.routectl_internal.inbound_session_key.is_some(),
                        );
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        return CountSeatOutcome::Terminal(e);
                    }
                    // Auth-401 single-flight refresh: rotate the token and
                    // retry the SAME seat exactly once. Release the slot
                    // this attempt claimed BEFORE the `continue` re-enters
                    // the loop and re-gates (or the re-gate sees
                    // half_open_in_flight and returns CircuitOpen, locking
                    // the breaker until restart).
                    if !auth_retry_attempted && matches!(&e, Error::Upstream { status: 401, .. }) {
                        auth_retry_attempted = true;
                        tracing::debug!(
                            provider = provider_name,
                            model = model_label,
                            attempt = attempts_made,
                            "count_tokens 401; refreshing auth and retrying once",
                        );
                        if let Err(refresh_err) = provider.on_auth_failure().await {
                            self.release_probe_slot(&target.state_key);
                            probe_guard.disarm();
                            return CountSeatOutcome::Terminal(refresh_err);
                        }
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        continue;
                    }

                    // CAPABILITY error, checked BEFORE should_fallback so a
                    // wire-501 can never reach record_failure: the seat is
                    // capable-by-kind but its upstream cannot count. Release
                    // the probe slot WITHOUT debiting the breaker, then let
                    // the caller walk to the next capable seat.
                    if is_capability_error(&e) {
                        if let Error::Upstream { status: 501, .. } = &e {
                            // DEBUG, not WARN: post-fix this is the
                            // steady-state happy path (every count_tokens
                            // for a passthrough alias 501s here and walks),
                            // so at WARN it would flood the log on every
                            // client poll and bury real warnings. The new
                            // count_tokens tests guard the regression.
                            tracing::debug!(
                                provider = provider_name,
                                state_key = %target.state_key,
                                status = 501,
                                "count_tokens got wire-501 from capable-by-kind target; \
                                 treating as capability, not debiting breaker",
                            );
                        }
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        return CountSeatOutcome::Capability;
                    }

                    // Health error. Mirror `complete_with_options`: the
                    // breaker DEBIT keys off the failure CLASS, not the
                    // fallback decision. A transient-health class (429 / 5xx
                    // / status-0 / overload) debits (an honored reset hint
                    // parks instead); a caller-shaped 4xx releases the slot
                    // without a debit, so a repeated non-retryable 4xx here
                    // cannot trip the per-seat breaker that also gates
                    // completions and streams. Either way this propagates --
                    // health fallback stays reserved for the messages path,
                    // so a 429 here does NOT walk.
                    let policy = self.policy_for(&req.model);
                    let native_cf = classify(&e, target.provider_kind);
                    let (cf, remapped) = apply_remap(
                        native_cf,
                        upstream_status_for_remap(&e),
                        &target.class_overrides,
                    );
                    let reset_hint = rate_limit_reset_hint(&e, &policy);
                    let debit = class_debits(&cf.class);
                    // The class/remap/debit decision on the token-count path was
                    // otherwise silent (unlike the messages path, which emits a
                    // class-decision event at every error arm). One INFO event at
                    // the settle point makes a count_tokens breaker debit / park
                    // triageable. Safe dimensions only -- NEVER a body or prompt.
                    let facts = upstream_facts(&e);
                    tracing::info!(
                        event = "count_tokens",
                        state_key = %target.state_key,
                        provider = provider_name,
                        status = facts.status.unwrap_or(0),
                        upstream_type = facts.upstream_type.unwrap_or(""),
                        upstream_code = facts.upstream_code.unwrap_or(""),
                        effective_class = class_label(&cf.class),
                        matched_by = matched_by_label(cf.matched_by),
                        remapped,
                        debit,
                        "count_tokens seat terminal; resilience class policy applied",
                    );
                    if debit {
                        match reset_hint {
                            Some(h) => self.park_provider(&target.state_key, h),
                            None => self.record_failure(&target.state_key),
                        }
                        probe_guard.disarm();
                    } else {
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                    }
                    return CountSeatOutcome::Terminal(e);
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
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
        // See `complete_inner`: re-probes the filter admitted, owned by a
        // request-scoped set that settles each on transfer (`take`) or on drop
        // (`OtherError` for any admission the loop never reached).
        let mut probe_set = ProbeAdmissionSet::new(
            self.learned_capabilities.clone(),
            probe_admissions,
            DispatchSurface::Stream.as_str(),
        );
        // See `complete_inner`: request-global observability dimension
        // ONLY, fed to `emit_class_observability`. The per-target
        // `use_forwarded_credential` flag drives the model-rewrite
        // bypass and the 401/403/429 terminal re-key.
        let is_forwarded = req.routectl_internal.forwarded_bearer.is_some();
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        // Availability-probe detection (see `complete_with_options`). A
        // streaming probe that 429/529s fast-fails the chain too.
        let is_probe = is_probe_request(&req, &policy);
        // Auto-cache decision inputs: computed ONCE off the ORIGINAL req
        // (see `complete_inner`). Reused across retries and fallback
        // targets so the decision never drifts.
        let auto_cache_plan =
            AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
        let mut last_err: Option<Error> = None;
        // One learned-capability observation per request per
        // (state_key, feature): the error arm fires per attempt, so this
        // set stops a same-request retry from manufacturing a second
        // observation. See `observe_for_learning`.
        let mut learn_dedupe: HashSet<(String, String)> = HashSet::new();

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            // See `complete_inner`: learned re-probe settle guard, scoped to
            // the whole chain-iteration; holds every admission the target owns
            // and drops -> OtherError on any exit. The move takes ownership out
            // of the set so the set's drop cannot settle it again.
            let mut learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
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
            meta.mark_target(target, &req.model);
            meta.fallback_count = chain_idx as u32;

            // Missing-bearer terminal guard: see `complete_inner`.
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                return Err(err);
            }

            let mut attempt_req = req.clone();
            // See `complete_inner`: a forwarded target keeps the client's
            // requested model verbatim instead of rewriting to `upstream`.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
            }
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK (see `complete_inner`): strip runs after
            // layered config compose, before auto-cache / context
            // reduction. Strict refusal returns the 400; a rolled-back
            // hazard routes away for this attempt.
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => return Err(err),
                StripDecision::RouteAway(err) => {
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            let provider_cfg = self.config.providers.get(provider_name);
            // Context reduction: see `complete_inner`. Runs after overlays
            // and before auto-cache so the auto-emitted breakpoint covers the
            // reduced bytes. Effective only when global on AND provider not
            // explicitly off. `apply_json_minify` fails closed.
            let reduction_effective = self.config.reduction.enabled
                && provider_cfg.and_then(super::config::ProviderEntry::reduction_enabled)
                    != Some(false);
            let reduction_outcome = if reduction_effective {
                Some(apply_json_minify(&mut attempt_req))
            } else {
                None
            };
            let reduction_token =
                reduction_strategy_token(reduction_effective, reduction_outcome.as_ref());
            meta.reduction_strategy = Some(reduction_token);
            if let Some(ReductionOutcome::Applied(delta)) = &reduction_outcome {
                tracing::debug!(
                    provider = %provider_name,
                    model = %model,
                    strategy = reduction_token,
                    strings_minified = delta.strings_minified,
                    bytes_saved = delta.bytes_saved,
                    est_tokens_saved = delta.est_tokens_saved,
                    "context_reduction",
                );
            }
            // Auto-cache: see `complete_inner`. Same once-vs-per-attempt
            // split; injected on this clone after overlays, before the
            // inner loop. Original `req` is never mutated.
            let cache_injection = maybe_apply_auto_cache_control(
                &mut attempt_req,
                &auto_cache_plan,
                provider_cfg.map(super::config::ProviderEntry::cache_capability),
                provider_cfg
                    .and_then(super::config::ProviderEntry::auto_emit_top_level_breakpoint)
                    .unwrap_or(true),
            );
            // T6 observability: see `complete_inner`. Stamp the decision
            // token and emit the per-request decision at debug. No bodies /
            // secrets: only provider name, model id, strategy token.
            let strategy_token = cache_injection.strategy_str();
            meta.cache_strategy = Some(strategy_token);
            tracing::debug!(
                provider = %provider_name,
                model = %model,
                strategy = strategy_token,
                "cache_auto_decision",
            );

            // Steady-state would-trim advisory: see `complete_inner`. The same
            // NON-MUTATING shared helper runs on the streaming path so the two
            // dispatch sites never diverge. Never touches `attempt_req`.
            self.record_would_trim(
                &attempt_req,
                target.provider_kind,
                model,
                target.nickname.as_deref().unwrap_or(model),
                &target.model.effective_row,
                meta,
            );

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
                // Cancellation backstop (see ProbeSlotGuard): free the
                // half-open probe slot if this future is dropped before an
                // outcome arm settles it (e.g. consumer disconnect during the
                // first-chunk wait against a hung upstream). Re-reads the same
                // flag as `was_half_open_probe` above; both reads are
                // consistent under the single-probe invariant.
                let mut probe_guard = self.probe_slot_guard(state_key);

                let r = try_stream_with_first_chunk(
                    provider_name,
                    model,
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
                        if was_half_open_probe && let Some(st) = state.as_ref() {
                            st.lock().record_success();
                        }
                        // The probe (if any) is settled; the wrapped stream's
                        // BreakerAccounting owns the tail. Disarm so a drop here
                        // does not free a slot a later probe may hold.
                        probe_guard.disarm();
                        // A first chunk proves the capability is not rejected:
                        // clear any learned negative this dispatch re-probed.
                        learned_probe_guard.settle_success();
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
                        let native_cf = classify(&e, target.provider_kind);
                        let original_class = native_cf.class.clone();
                        let remap_candidate_status = upstream_status_for_remap(&e);
                        let (cf, remapped) =
                            apply_remap(native_cf, remap_candidate_status, &target.class_overrides);
                        let remap_status = if remapped {
                            remap_candidate_status
                        } else {
                            None
                        };
                        // A forwarded-credential TARGET that drew a
                        // 401/403/429 is TERMINAL -- bypass the
                        // on_auth_failure refresh (below) AND the fallback
                        // hop, surface verbatim. Keyed off the CURRENT
                        // target's `use_forwarded_credential`, not
                        // request-global bearer presence (see
                        // `complete_inner` for the full rationale). This
                        // is the pre-first-chunk window; a mid-stream error
                        // never reaches here (it rides the wrapped stream).
                        // Release the half-open slot without a breaker debit.
                        if target.use_forwarded_credential
                            && let Some(status) = forwarded_terminal_status(&e)
                        {
                            log_forwarded_auth_terminal(
                                status,
                                req.routectl_internal.inbound_session_key.is_some(),
                            );
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            return Err(e);
                        }
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
                                probe_guard.disarm();
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
                            probe_guard.disarm();
                            continue;
                        }
                        let do_fallback = should_fallback(&e, &cf.class, &policy, is_probe);
                        // Probe fast-fail: a probe that hit a rate-limit/
                        // overload (429/529) returns the status immediately
                        // -- no fallback, no breaker failure debit. The early
                        // return below precedes the debit site, so a probe
                        // 429/529 never reaches health accounting. It does
                        // release the half-open slot it may have claimed at
                        // the gate (see below). Streams never retry the same
                        // provider, so there is no can_retry_here to guard.
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
                            probe_guard.disarm();
                            return Err(e);
                        }
                        // Stream dispatch never retries the same provider (no
                        // in-loop sleep), so a reset hint only sizes the
                        // breaker park. A non-probe reset parks the provider
                        // for the honored, clamped duration; a probe never
                        // parks and a no-hint error keeps the
                        // threshold-gated debit.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        // The DEBIT keys off the failure CLASS, not the
                        // chain-hop decision (see `complete_inner`): a seat is
                        // unhealthy when it emits a transient-health failure,
                        // independent of routing. A non-debiting class that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
                        if debits {
                            match reset_hint {
                                Some(h) if !is_probe => self.park_provider(state_key, h),
                                _ => self.record_failure(state_key),
                            }
                            probe_guard.disarm();
                        }
                        self.emit_class_observability(
                            &e,
                            &cf,
                            &original_class,
                            remapped,
                            remap_status,
                            DispatchSurface::Stream,
                            provider_name,
                            target,
                            do_fallback,
                            &policy,
                            debits,
                            is_probe,
                            is_forwarded,
                        );
                        self.observe_for_learning(
                            &e,
                            &cf,
                            remapped,
                            target,
                            is_forwarded,
                            &req,
                            &mut learn_dedupe,
                            meta,
                            &mut learned_probe_guard,
                        );
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when a
                            // debiting class already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
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
                            // A fallbackable error whose class did NOT debit
                            // the breaker leaves the half-open probe slot
                            // armed; release it without a debit before the hop
                            // so every settle path frees the slot exactly once.
                            // A debiting class already settled + disarmed above.
                            if !debits {
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        probe_guard.disarm();
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Observability seam at the router's class-decision point. Bumps the
    /// fail-closed-unknown / feature-unsupported counters, emits the stable
    /// FeatureUnsupported event when applicable, and emits exactly one
    /// class-decision event (DEBUG, or WARN on an Unknown-classified
    /// upstream outcome). Called from BOTH dispatch error arms with only
    /// safe, structured facts -- never the error body / Display string.
    /// `cf` is the EFFECTIVE (post-remap) classification every other
    /// consumer already acted on; `original_class` is the classifier's
    /// pre-remap decision, carried through purely for provenance.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn emit_class_observability(
        &self,
        err: &Error,
        cf: &ClassifiedFailure,
        original_class: &FailureClass,
        remapped: bool,
        remap_status: Option<u16>,
        surface: DispatchSurface,
        provider: &str,
        target: &DispatchTarget,
        fallback: bool,
        policy: &RetryPolicy,
        debit: bool,
        is_probe: bool,
        is_forwarded: bool,
    ) {
        let facts = upstream_facts(err);
        let model = target.nickname.as_deref().unwrap_or("");
        match &cf.class {
            FailureClass::FeatureUnsupported { capability } => {
                self.metrics.incr_feature_unsupported();
                emit_feature_unsupported(
                    provider,
                    target.provider_kind,
                    model,
                    capability,
                    &facts,
                    cf.matched_by,
                    surface,
                    is_forwarded,
                    remapped,
                );
            }
            FailureClass::Unknown if facts.status.is_some() => {
                self.metrics.incr_unknown_failure_classification();
            }
            _ => {}
        }
        emit_class_decision(&ClassDecisionObs {
            provider,
            model,
            surface,
            original_class,
            effective_class: &cf.class,
            matched_by: cf.matched_by,
            facts,
            fallback,
            retry_cap: retry_cap_for(&cf.class, policy),
            hard_retry_cap: policy.hard_retry_cap(),
            debit,
            is_probe,
            is_forwarded,
            remapped,
            remap_status,
        });
    }

    /// Learn-path capture, called from both dispatch error arms beside
    /// [`Router::emit_class_observability`]. On an eligible, deduped
    /// capability rejection it records a learned negative in the registry,
    /// emits a structured WARN, and (once the entry is acting) rides a
    /// [`CapabilityLearnEvent`] out on `meta`.
    ///
    /// Every gate short-circuits, so a common (non-capability) failure
    /// pays only the cheap early checks. The full eligibility gate (all
    /// must hold): the kill switch is on; the upstream fault is a request
    /// fault (400/422); the class was not operator-remapped; the request
    /// did not carry a forwarded bearer; the resolver attributes the fault
    /// to a canonical capability; that capability is not the
    /// operator-remap provenance token; and the capability is a member of
    /// the request's derived feature set (`derive_feature_keys`). The
    /// resolver keys on the request-capability namespace, so this final
    /// membership check learns a negative ONLY for a capability the request
    /// actually carried -- a misbehaving upstream naming an off-request
    /// param never plants a routing entry.
    ///
    /// `dedupe` carries one entry per `(state_key, feature_key)` for the
    /// life of a single request: the error arm fires per attempt, so a
    /// same-request retry (or a per-target re-entry) must never manufacture
    /// a second observation and falsely confirm an inferred signal.
    #[allow(clippy::too_many_arguments)]
    fn observe_for_learning(
        &self,
        err: &Error,
        cf: &ClassifiedFailure,
        remapped: bool,
        target: &DispatchTarget,
        is_forwarded: bool,
        req: &ChatRequest,
        dedupe: &mut HashSet<(String, String)>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        if !self.config.capability.enabled {
            return;
        }
        let Error::Upstream {
            status: status @ (400 | 422),
            upstream_code,
            ..
        } = err
        else {
            return;
        };
        let upstream_status = *status;
        if remapped || is_forwarded {
            return;
        }
        let Some(provider_kind) = target.provider_kind else {
            return;
        };
        let Some((feature_key, tier)) = resolve_requested_capability(provider_kind, err, cf) else {
            return;
        };
        if feature_key == crate::class_policy::OPERATOR_REMAP_CAPABILITY {
            return;
        }
        // Request-membership gate: learn a negative ONLY for a capability the
        // request actually carried. `request_features` is the act-side lookup
        // vocabulary (`derive_feature_keys` output); the resolver now emits
        // canonical act-side keys, so a genuine rejection is a member by
        // construction -- `response_format` -> `structured_output` for a
        // request whose `output_config.format` was set, and a tool-type
        // passthrough for a request that carried that tool type. A resolved
        // key the request never sent (a poisoned or spurious upstream param,
        // or a capability with no act-side derivation -- an inferred `prefill`,
        // a paramless geo-block token) fails the check and never learns, so a
        // misbehaving upstream cannot plant a routing entry the act side could
        // never look up. This is the gate the original cross-namespace check
        // meant to be: correct now that both sides meet on identical keys.
        let request_features = crate::feature_keys::derive_feature_keys(
            req.tools.as_deref().unwrap_or(&[]),
            req.provider_extras.as_ref(),
        );
        if !request_features.contains(&feature_key) {
            return;
        }
        let state_key = target.state_key.clone();
        // MASK: an operator `force_supported` override for this (target,
        // feature) masks the learned negative. The act side already
        // short-circuited both the routing verdict AND the probe admission
        // for a masked cell, so it never claims a re-probe slot; here the
        // learn is suppressed too -- a masked-cell rejection never refreshes
        // or increments the resident entry (its `expires_at` is untouched, so
        // wall-clock decay continues). Upstream still rejected the capability
        // the operator forced on, so surface the contradiction exactly once
        // per request (deduped) with a dedicated counter. Capability TOKEN and
        // state_key only -- never a request body.
        if self.override_forces_supported(target, &feature_key, provider_kind) {
            if dedupe.insert((state_key.clone(), feature_key.clone())) {
                self.metrics.incr_mask_suppressed();
                tracing::warn!(
                    event = "suppression",
                    state_key = %state_key,
                    capability_key = %feature_key,
                    "force_supported override contradicted: masked capability still rejected upstream",
                );
            }
            return;
        }
        // If this target was admitted as the single re-probe for this same
        // capability, the rejection SETTLES the probe (capped backoff owns the
        // observation bump and expiry) instead of feeding the observe path.
        // The dedupe key is inserted too, so a same-request retry that hits
        // this arm again does not re-observe the entry the probe refreshed.
        if probe_guard.settle_same_capability(&state_key, &feature_key, provider_kind) {
            self.metrics.incr_probe_failures();
            dedupe.insert((state_key, feature_key));
            return;
        }
        // One observation per request per (state_key, feature): a retry or
        // per-target re-entry that hits this arm again is dropped here.
        if !dedupe.insert((state_key.clone(), feature_key.clone())) {
            return;
        }

        let outcome = self.learned_capabilities.observe(
            &state_key,
            &feature_key,
            provider_kind,
            tier,
            Instant::now(),
        );
        let acting = matches!(outcome, crate::learned_capability::ObserveOutcome::Acting);
        let observations = self
            .learned_capabilities
            .snapshot()
            .into_iter()
            .find(|entry| entry.state_key == state_key && entry.feature_key == feature_key)
            .map_or(0, |entry| entry.observations);

        let upstream_param = crate::capability_matcher::upstream_param(err);
        // Emit `upstream_param` ONLY when the sanitizer deemed it safe to log
        // verbatim (bounded, single-token, no whitespace/control bytes). An
        // adversarial or buggy upstream can put arbitrary text in `error.param`;
        // dropping the field entirely -- rather than logging a blank or the raw
        // string -- keeps injected content out of the operator log while the
        // closed-set `capability_key` and `upstream_code` still record.
        match upstream_param.as_deref() {
            Some(param) => tracing::warn!(
                event = "learn",
                state_key = %state_key,
                capability_key = %feature_key,
                provider_kind,
                upstream_status,
                upstream_code = upstream_code.as_deref().unwrap_or(""),
                upstream_param = %param,
                signal_tier = tier.as_str(),
                observations,
                acting,
                "learned-capability negative observed",
            ),
            None => tracing::warn!(
                event = "learn",
                state_key = %state_key,
                capability_key = %feature_key,
                provider_kind,
                upstream_status,
                upstream_code = upstream_code.as_deref().unwrap_or(""),
                signal_tier = tier.as_str(),
                observations,
                acting,
                "learned-capability negative observed",
            ),
        }

        if acting {
            self.metrics.incr_learned_negatives();
            meta.learned_capabilities.push(CapabilityLearnEvent {
                state_key,
                capability_key: feature_key,
                provider_kind: provider_kind.to_string(),
                signal_tier: tier,
                observations,
                upstream_status,
                remapped,
                request_features,
            });
        }
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
        self.record_failure_opened(state_key);
    }

    /// Debit one breaker failure for `state_key`, returning whether this
    /// debit tripped (opened) the breaker on this call. The `record_failure`
    /// wrapper discards that signal; a caller that must report the breaker
    /// effect of the debit uses this directly.
    fn record_failure_opened(&self, state_key: &str) -> bool {
        self.state
            .get(state_key)
            .is_some_and(|state| state.lock().record_failure(Instant::now()))
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

    /// Build a `ProbeSlotGuard` for a dispatch that just passed the gate.
    /// Armed iff `state_key` currently holds the half-open probe slot (i.e.
    /// THIS dispatch was admitted as the probe); inert otherwise. The guard
    /// releases the slot on drop unless an outcome disarms it -- the
    /// cancellation-safety backstop for a dropped dispatch future.
    ///
    /// The `is_half_open_probe` read and the `state.get().cloned()` below are
    /// two separate lock acquisitions, but the check is race-free under the
    /// single-probe invariant: `try_dispatch` admits at most one
    /// `half_open_in_flight` caller per cooldown, and the current caller has
    /// not yet settled its slot, so no concurrent caller can clear or re-claim
    /// it between the two reads.
    fn probe_slot_guard(&self, state_key: &str) -> ProbeSlotGuard {
        if self.is_half_open_probe(state_key) {
            ProbeSlotGuard::new(self.state.get(state_key).cloned())
        } else {
            ProbeSlotGuard::new(None)
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
            .map(super::config::ProviderEntry::runtime);
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

    /// NON-MUTATING would-trim advisory recording, gated by ONE independent
    /// check: the request's estimated token count clears
    /// `params.trigger_tokens`. Below the trigger, records nothing at all
    /// (every `would_trim_*` field stays `None`) and returns early. At or
    /// above it, runs TWO independent measurements against the same
    /// resolved `(provider_kind, model)` pricing row:
    ///
    /// 1. The shipped size-baseline plan (`propose_steady_state_trim`):
    ///    freed-token count `d`, break-even K*, `k_floor`, and the shadow
    ///    misfire monitor. UNCHANGED by this task -- still driven entirely
    ///    by the `propose` path, and still records nothing when `propose`
    ///    finds no cut (its own columns stay `None`).
    /// 2. The near-lossless pass (`record_near_lossless_marks`): dedup /
    ///    supersession attribution, path-extractability counts, the
    ///    recorder-version marker, the raw-marks blob, and the
    ///    context-fraction advisory. Runs INDEPENDENTLY of whether (1)
    ///    found a cut, so near-lossless opportunity is measured even where
    ///    the size baseline declines.
    ///
    /// CRITICAL: this NEVER mutates `attempt_req`. Both measurements only
    /// read the request and never call `apply_trim_plan`, so the bytes sent
    /// upstream are identical with or without this call. It is extracted
    /// into ONE helper -- invoked from both `complete_inner` and
    /// `stream_inner` -- so the two byte-identical dispatch blocks cannot
    /// drift.
    ///
    /// An unknown / unresolved-catalog target (e.g. `provider_kind` is
    /// `None` for a legacy / direct-construction target, or the two-layer
    /// merge resolves `Disabled` / `Missing`) records freed-token /
    /// attribution counts but no break-even K*: a merge result other than
    /// `Present` carries no trusted multipliers, so a break-even number
    /// would be misleading. This matches the offline prompt-size advisory
    /// convention (compute `break_even_k` only when the merge resolved
    /// `Present`).
    ///
    /// Additionally, when the size-baseline plan exists, consults the
    /// in-process [`crate::k_estimator::KEstimator`] over the request's
    /// `inbound_session_key` and the SAME pricing row's `ttl_seconds`
    /// (threaded through for provenance / reserved for future
    /// age-conditioning; the per-turn hazard model does not split on it).
    /// The estimate's `k_floor` is stamped onto `meta.would_trim_k_floor` only
    /// for a `Calibrated` confidence (the only bound the cost gate may
    /// consult to authorize a cut). The met/unmet/cold/unpriced verdict is
    /// DERIVED downstream from the numeric advisory columns
    /// (`would_trim_tokens`, `would_trim_break_even_k`, `would_trim_k_floor`);
    /// this function never overwrites `meta.reduction_strategy`, which
    /// remains owned by the reduction path. Advisory only -- the
    /// dispatched bytes never change.
    ///
    /// The two model dimensions are DELIBERATELY split. `model` is the
    /// UPSTREAM wire id and keys the pricing lookup -- pricing cells are baked
    /// per upstream model. `served_model` is the served NICKNAME and keys the
    /// K estimator's [`crate::k_estimator::KQuery`], because
    /// [`Router::record_k_sample`] writes each per-session K window under the
    /// served nickname (`meta.served_model` == `target.nickname`). Keying the
    /// query on `served_model` (not `model`) is what makes the query triple
    /// byte-identical to the sample-write triple; keying it on the upstream id
    /// silently misses the store, holds every estimate at `Cold`, and leaves
    /// `would_trim_k_floor` permanently `None`. When a target has no nickname
    /// (`served_model` falls back to `model`), no sample is ever recorded for
    /// that dispatch either, so the fallback never mis-keys a populated window.
    fn record_would_trim(
        &self,
        attempt_req: &ChatRequest,
        provider_kind: Option<&'static str>,
        model: &str,
        served_model: &str,
        effective: &EffectiveRow,
        meta: &mut DispatchMeta,
    ) {
        let params = self.config.trim.to_params();
        if estimate_total_tokens(attempt_req) <= params.trigger_tokens {
            return;
        }

        // The two-layer merge (baked catalog + overlay) is resolved
        // once at chain-build time and rides the resolved target
        // (`ResolvedModel::effective_row`, stamped by
        // `factory::apply_catalog_overlay`) -- this dispatch-path function
        // only reads the precomputed result, never re-runs
        // `lookup_baked_with_overrides` + `merge` per request. `Present`
        // prices normally; `Disabled` / `Missing` fold to the SAME
        // conservative sentinel behavior (`row` below is `None`). Shared
        // by both the size-baseline block below and the near-lossless
        // pass, so the two measurements price against the identical
        // resolved cell.
        let row = effective.priced();

        if let Some(plan) = propose_steady_state_trim(attempt_req, &params) {
            meta.would_trim_tokens = Some(plan.candidate.d);
            let break_even = row.and_then(|r| break_even_k(r, &plan.candidate));
            meta.would_trim_break_even_k = break_even;

            // Consult the K estimator over the SAME row whose TTL priced K*: the
            // TTL is threaded through for provenance only -- the per-turn
            // hazard model does not split on it -- so a `k_floor` is
            // comparable to `break_even`. The current sample for THIS turn
            // is recorded post-response in `record_k_sample`, so the
            // estimator reads PRIOR-turn samples only. The query keys on
            // `served_model` (the nickname), NOT the upstream `model`, so the
            // query triple matches the triple `record_k_sample` writes under.
            // A `Disabled` / `Missing` cell has no row to read `ttl_seconds`
            // from; the sentinel's TTL is the same conservative default used
            // everywhere else a trusted row is unavailable.
            let ttl_seconds = row.map_or(CatalogRow::sentinel().ttl_seconds, |r| r.ttl_seconds);
            let estimate = self.k_estimator.estimate(&crate::k_estimator::KQuery {
                session_key: attempt_req.routectl_internal.inbound_session_key.as_deref(),
                provider_kind: provider_kind.unwrap_or(""),
                model: served_model,
                ttl: Duration::from_secs(u64::from(ttl_seconds)),
                now: SystemTime::now(),
            });

            meta.would_trim_k_floor = would_trim_k_floor_for_meta(break_even, &estimate);

            // Shadow misfire monitor: recording only, never mutates attempt_req.
            // Compute a fingerprint of the trimmed cacheable prefix and compare it
            // against the stored value for this (session, provider_kind, model) triple.
            // A Misfire means the prefix shifted turn-to-turn -- the canary that a
            // real cut would break the upstream cache.
            if let Some(session_key) = attempt_req.routectl_internal.inbound_session_key.as_deref()
            {
                let fp = trimmed_prefix_fingerprint(attempt_req, &plan);
                let shadow_key = crate::k_estimator::KSessionKey {
                    session_key: session_key.to_string(),
                    provider_kind: provider_kind.unwrap_or("").to_string(),
                    model: model.to_string(),
                };
                let outcome =
                    self.shadow_store
                        .record_and_compare(shadow_key, fp, SystemTime::now());
                match outcome {
                    crate::k_estimator::ShadowOutcome::Stable => {
                        meta.would_trim_shadow_misfire = Some(0);
                    }
                    crate::k_estimator::ShadowOutcome::Misfire => {
                        meta.would_trim_shadow_misfire = Some(1);
                        tracing::warn!(
                            session_key = %session_key,
                            provider_kind = provider_kind.unwrap_or(""),
                            model = %model,
                            "would_trim_shadow_misfire: trimmed cacheable prefix shifted turn-to-turn",
                        );
                    }
                    crate::k_estimator::ShadowOutcome::FirstSeen => {}
                }
            }
        }

        record_near_lossless_marks(attempt_req, &params, row, meta);
    }
}

/// Recorder-version marker stamped onto `meta.would_trim_recorder_version`
/// whenever the near-lossless pass runs (the estimated-token trigger
/// cleared), regardless of whether it found any marks. Lets offline
/// reporting filter M1-era rows from pre-M1 rows without confounding
/// semantics across a deploy boundary.
const NEAR_LOSSLESS_RECORDER_VERSION: i64 = 1;

/// NON-MUTATING near-lossless attribution pass. Measures the dedup +
/// supersession opportunity over the SAME `[head_keep, scan_end)` window the
/// shipped trimmer scans, INDEPENDENT of whether the shipped size-baseline
/// plan (`propose_steady_state_trim`) found a cut -- called unconditionally
/// by `record_would_trim` once the estimated-token trigger clears. Stamps
/// zero-or-more-marks results as `Some(0)` (a measured zero), distinct from
/// the caller's `None` (pass did not run, below trigger).
///
/// `would_trim_context_fraction` is fail-closed: `None` whenever `row`'s
/// context window is unknown, never a guessed value. `row` is `None` when
/// the two-layer merge resolved `Disabled` / `Missing` -- context_fraction
/// and break-even fold to the same `None` in that case.
///
/// Prices the near-lossless candidate via the UNCHANGED `break_even_k` gate,
/// present-row-only, mirroring the `would_trim_break_even_k` convention --
/// but only LOGS the result via `tracing::debug!`. There is no persisted
/// column for it: unlike the shipped size-baseline plan, the near-lossless
/// candidate has no DispatchMeta / UsageRecord economics field in this
/// increment.
///
/// Single O(parts) walk: `collect_near_lossless_marks` performs one forward
/// scan over the request's content parts; this function makes no second
/// pass. It is a SIBLING scan to the one `propose_steady_state_trim` already
/// ran above (by design -- the near-lossless heuristics are a distinct
/// question from the size baseline), not a repeat of it.
fn record_near_lossless_marks(
    attempt_req: &ChatRequest,
    params: &SteadyStateTrimParams,
    row: Option<&CatalogRow>,
    meta: &mut DispatchMeta,
) {
    let n = attempt_req.messages.len();
    let scan_end = n.saturating_sub(params.keep_recent_messages);
    let marks = collect_near_lossless_marks(attempt_req, params.head_keep_messages, scan_end);

    meta.would_trim_recorder_version = Some(NEAR_LOSSLESS_RECORDER_VERSION);
    meta.would_trim_dedup_tokens = Some(marks.dedup_tokens);
    meta.would_trim_supersession_tokens = Some(marks.supersession_tokens);
    meta.would_trim_path_units = Some(marks.path_units);
    meta.would_trim_path_extractable = Some(marks.path_extractable);
    meta.would_trim_context_fraction = row
        .and_then(|r| r.max_context_tokens)
        .map(|window| estimate_total_tokens(attempt_req) as f64 / window as f64);
    meta.would_trim_raw_marks = Some(serde_json::to_value(&marks.marks).unwrap_or(Value::Null));

    if let Some(candidate) = near_lossless_candidate(attempt_req, &marks.marks) {
        let break_even = row.and_then(|r| break_even_k(r, &candidate));
        tracing::debug!(
            dedup_tokens = marks.dedup_tokens,
            supersession_tokens = marks.supersession_tokens,
            break_even_k = ?break_even,
            "near_lossless_candidate priced (log-only, not persisted)",
        );
    }
}

/// Pure helper that selects the `would_trim_k_floor` value recorded by
/// `Router::record_would_trim`. Returns `Some(estimate.k_floor)` only when
/// `break_even` is a Present-row K* AND the estimator's confidence is
/// `Calibrated`; every other case records `None`. The met/unmet/cold/
/// unpriced verdict is derived downstream as a pure query over the numeric
/// advisory columns (`would_trim_break_even_k`, `would_trim_k_floor`).
fn would_trim_k_floor_for_meta(
    break_even: Option<f64>,
    estimate: &crate::k_estimator::KEstimate,
) -> Option<f64> {
    if break_even.is_some() && estimate.confidence == crate::k_estimator::Confidence::Calibrated {
        Some(estimate.k_floor)
    } else {
        None
    }
}

/// Request-level inputs to the auto-cache decision, computed ONCE per
/// request off the original `req` (above the `'chain` loop) and reused
/// for every retry and fallback target. Holding these constant is what
/// makes auto-emit idempotent: retrying the same target sends
/// byte-identical bytes, and a fallback target re-derives nothing.
///
/// The gate reads `has_caller_breakpoints` / `caller_breakpoint_count`
/// (snapshotted from the frozen floor at build time) directly so the
/// predicate stays a cheap field compare.
struct AutoCacheRequestPlan {
    has_caller_breakpoints: bool,
    caller_breakpoint_count: usize,
    volatile_high_veto: bool,
    global_auto_emit_enabled: bool,
}

impl AutoCacheRequestPlan {
    /// Build the plan from the ORIGINAL request. Pure read: never mutates
    /// `req`. Called once per dispatch fn, above the `'chain` loop.
    fn build(req: &ChatRequest, global_auto_emit_enabled: bool) -> Self {
        let frozen_floor = compute_frozen_floor(req);
        let has_caller_breakpoints = frozen_floor.has_caller_breakpoints();
        let caller_breakpoint_count = frozen_floor.caller_breakpoint_count();
        let volatile_high_veto = scan_volatile(req).is_high_confidence_veto();
        Self {
            has_caller_breakpoints,
            caller_breakpoint_count,
            volatile_high_veto,
            global_auto_emit_enabled,
        }
    }
}

/// Outcome of an auto-cache injection decision for one dispatch target.
/// Drives control flow today (and is the stable per-target signal T6 will
/// log). Every non-`Emitted` variant means `attempt_req` was left
/// untouched -- the dispatched bytes equal the un-injected clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheInjection {
    /// A top-level ephemeral_5m breakpoint was injected and validated.
    Emitted,
    /// Global `[cache] auto_emit_top_level_breakpoint = false`.
    SkippedGlobalDisabled,
    /// Per-provider `auto_emit_top_level_breakpoint = false`.
    SkippedProviderDisabled,
    /// Target's provider does not honor a top-level breakpoint (or its
    /// capability is unknown -- fail closed).
    SkippedNoCapability,
    /// The caller already supplied at least one breakpoint; auto-emit
    /// would risk a second marker / byte rewrite, so we defer entirely.
    SkippedCallerSupplied,
    /// The stable cacheable prefix carries high-confidence volatile
    /// tokens; caching it would write-without-read every request.
    SkippedVolatileHigh,
    /// Injecting would push the breakpoint count past `MAX_BREAKPOINTS`.
    SkippedBreakpointCap,
    /// Injection was attempted but post-injection validation failed; the
    /// original `cache_control` was restored and the clone dispatched
    /// unchanged.
    ValidationRolledBack,
}

impl CacheInjection {
    /// Stable operator-facing token for this decision, recorded in the
    /// usage DB (`requests.strategy`) and emitted in the
    /// `cache_auto_decision` log. These tokens are a CONTRACT: do not
    /// rename or repurpose them, only add new ones. The `auto_skipped:`
    /// prefix groups the variants where auto-emit ran but declined.
    /// `caller_supplied` is a request-level fact evaluated FIRST and takes
    /// precedence over every `auto_skipped:*` reason.
    ///
    /// | variant                  | token                              |
    /// |--------------------------|------------------------------------|
    /// | Emitted                  | `auto_emitted`                     |
    /// | SkippedCallerSupplied    | `caller_supplied`                  |
    /// | SkippedVolatileHigh      | `volatile_vetoed`                  |
    /// | SkippedGlobalDisabled    | `auto_skipped:global_disabled`     |
    /// | SkippedProviderDisabled  | `auto_skipped:provider_disabled`   |
    /// | SkippedNoCapability      | `auto_skipped:no_capability`       |
    /// | SkippedBreakpointCap     | `auto_skipped:breakpoint_cap`      |
    /// | ValidationRolledBack     | `auto_skipped:validation_rolled_back` |
    const fn strategy_str(self) -> &'static str {
        match self {
            Self::Emitted => "auto_emitted",
            Self::SkippedCallerSupplied => "caller_supplied",
            Self::SkippedVolatileHigh => "volatile_vetoed",
            Self::SkippedGlobalDisabled => "auto_skipped:global_disabled",
            Self::SkippedProviderDisabled => "auto_skipped:provider_disabled",
            Self::SkippedNoCapability => "auto_skipped:no_capability",
            Self::SkippedBreakpointCap => "auto_skipped:breakpoint_cap",
            Self::ValidationRolledBack => "auto_skipped:validation_rolled_back",
        }
    }
}

/// Map a context-reduction decision to its stable operator-facing token.
/// These tokens are a CONTRACT (recorded in the usage DB and emitted in the
/// `context_reduction` log): do not rename or repurpose them, only add new
/// ones. The `skipped:` prefix groups the variants where reduction did not
/// mutate the request.
///
/// | condition                          | token                     |
/// |------------------------------------|---------------------------|
/// | reduction not effective            | `skipped:disabled`        |
/// | `ReductionOutcome::Applied`        | `applied`                 |
/// | `ReductionOutcome::NoMutableTail`  | `skipped:no-tail`         |
/// | `ReductionOutcome::NothingToStrip` | `skipped:nothing-to-strip`|
/// | unrecognized future outcome        | `skipped:unknown`         |
///
/// `effective == false` short-circuits to `skipped:disabled` WITHOUT calling
/// `apply_json_minify`, so `outcome` is only consulted when reduction ran.
const fn reduction_strategy_token(
    effective: bool,
    outcome: Option<&ReductionOutcome>,
) -> &'static str {
    if !effective {
        return "skipped:disabled";
    }
    match outcome {
        Some(ReductionOutcome::Applied(_)) => "applied",
        Some(ReductionOutcome::NoMutableTail) => "skipped:no-tail",
        Some(ReductionOutcome::NothingToStrip) => "skipped:nothing-to-strip",
        // `ReductionOutcome` is `#[non_exhaustive]`; a future variant we do
        // not yet map means reduction RAN but produced an outcome this build
        // does not recognize -- which is distinct from "disabled", so record
        // it as its own token. `None` only happens when `effective == false`,
        // already handled above.
        _ => "skipped:unknown",
    }
}

/// Decide-and-maybe-inject a single top-level `cache_control` ephemeral_5m
/// breakpoint on the PER-ATTEMPT clone. Mutates ONLY `attempt_req`, never
/// the original request. Never returns `Err` and never panics: any doubt
/// degrades to "dispatch the un-injected clone".
///
/// clone -> set -> validate -> keep-or-rollback: the only mutation is a
/// single assignment to `attempt_req.cache_control`, and it is reverted if
/// `validate_source` rejects the injected shape. Called once per chain
/// entry, AFTER `apply_layered_overlays` (so injection is the last
/// dispatch-time touch of `cache_control`) and before the inner retry
/// loop, so all retries on a target reuse byte-identical bytes.
///
/// `capability` is `None` when the target's provider is absent from the
/// table -> fail closed (no injection). Cheap field checks run before the
/// validate call so the common skip paths never allocate.
///
/// `caller_supplied` is evaluated FIRST as a request-level fact: a request
/// that already carries caller breakpoints is independent of which target /
/// provider is selected, so it takes precedence over every per-target /
/// config `auto_skipped:*` reason (global / provider kill-switch, capability).
fn maybe_apply_auto_cache_control(
    attempt_req: &mut ChatRequest,
    plan: &AutoCacheRequestPlan,
    capability: Option<CacheCapability>,
    provider_auto_emit_enabled: bool,
) -> CacheInjection {
    if plan.has_caller_breakpoints {
        return CacheInjection::SkippedCallerSupplied;
    }
    if !plan.global_auto_emit_enabled {
        return CacheInjection::SkippedGlobalDisabled;
    }
    if !provider_auto_emit_enabled {
        return CacheInjection::SkippedProviderDisabled;
    }
    match capability {
        Some(c) if c.supports_top_level_cache_control => {}
        _ => return CacheInjection::SkippedNoCapability,
    }
    if plan.volatile_high_veto {
        return CacheInjection::SkippedVolatileHigh;
    }
    // Defensive drift guard: no-caller implies 0 today, so +1 is always
    // within MAX. Kept so a future change that injects alongside caller
    // markers cannot silently exceed the cap.
    if plan.caller_breakpoint_count.saturating_add(1) > MAX_BREAKPOINTS {
        return CacheInjection::SkippedBreakpointCap;
    }

    // clone -> set -> validate -> keep-or-rollback, local to this clone.
    let original = attempt_req.cache_control.clone();
    attempt_req.cache_control = Some(CacheControl::ephemeral_5m());
    if validate_source(attempt_req).is_ok() {
        CacheInjection::Emitted
    } else {
        attempt_req.cache_control = original;
        CacheInjection::ValidationRolledBack
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
    let provider_headers = provider_entry.map(super::config::ProviderEntry::header_extras);
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
    // Preserve the ingress-captured inbound per-conversation session key:
    // like `claude_code_headers`, it is inbound-request data, not a
    // per-model knob, so the per-attempt rebuild from `Default::default()`
    // must carry it across or it resets to `None` on the 2nd chain attempt.
    let captured_inbound_session_key = req.routectl_internal.inbound_session_key.take();
    // Preserve the ingress-forwarded bearer token: like
    // `inbound_session_key`, it is inbound-request data, not a per-model
    // knob, so the per-attempt rebuild from `Default::default()` must
    // carry it across or it resets to `None` on the 2nd chain attempt.
    let captured_forwarded_bearer = req.routectl_internal.forwarded_bearer.take();
    // Preserve the ingress-captured forwarded `x-stainless-*` headers:
    // like `forwarded_bearer`, they are inbound-request data (the client's
    // SDK fingerprint captured on the forwarded leg), not a per-model knob,
    // so the per-attempt rebuild from `Default::default()` must carry them
    // across or they reset to empty on the 2nd chain attempt -- which would
    // let routectl's minted fingerprint win on a retry.
    let captured_stainless_headers = std::mem::take(&mut req.routectl_internal.stainless_headers);
    let mut internal = RoutectlInternal::default();
    internal.reasoning_dialect = target.reasoning_dialect.map(std::convert::Into::into);
    internal.history_reasoning = target.history_reasoning.map(std::convert::Into::into);
    internal.claude_code_headers = captured_claude_code_headers;
    internal.provenance = captured_provenance;
    internal.header_extras = composed_header_extras;
    internal.inbound_session_key = captured_inbound_session_key;
    internal.forwarded_bearer = captured_forwarded_bearer;
    internal.stainless_headers = captured_stainless_headers;
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
        .map_or("", |(_, v)| v.as_str());

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
            .map_or("", |(_, v)| v.as_str());

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
        .is_some_and(serde_json::Map::is_empty);
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

/// The merged catalog capability priors for a resolved model, cloned off
/// its `EffectiveRow`. Empty when the cell is `Disabled` / `Missing` (the
/// conservative no-prior baseline) or carries no capability data.
fn catalog_capabilities(effective_row: &EffectiveRow) -> BTreeMap<String, bool> {
    effective_row
        .priced()
        .map(|row| row.capabilities.clone())
        .unwrap_or_default()
}

/// Convert a chain of `Arc<ResolvedModel>` into the `DispatchTarget`
/// shape the dispatch loop walks. Hoisted out of `dispatch_chain`
/// so the three resolution branches share one builder.
fn into_one_dispatch_target(m: Arc<ResolvedModel>) -> DispatchTarget {
    let capabilities = catalog_capabilities(&m.effective_row);
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        provider_kind: None,
        use_forwarded_credential: false,
        // v0.6.0 dispatch keys the breaker by nickname so two models
        // on one provider quarantine independently.
        state_key: m.nickname.clone(),
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

/// Build a dispatch target for one seat of a pooled model. Identical to
/// `into_one_dispatch_target` except the seat overrides the provider
/// instance and `state_key` (its own breaker + RPM bucket); every other
/// knob is shared from the model. The nickname stays the model's nickname
/// for tracing, while `state_key` carries the seat suffix. `provider_kind`
/// is the seat provider's stable kind token (a seat shares its model's
/// provider entry, so the caller resolves it from `provider_name` exactly
/// as the non-seat path does) so error classification keys off the real
/// egress kind rather than the union table.
fn dispatch_target_for_seat(
    m: &Arc<ResolvedModel>,
    seat: &crate::seat_pool::SeatTarget,
    provider_kind: Option<&'static str>,
) -> DispatchTarget {
    let capabilities = catalog_capabilities(&m.effective_row);
    DispatchTarget {
        provider_name: m.provider_name.clone(),
        provider_kind,
        use_forwarded_credential: false,
        state_key: seat.state_key.clone(),
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
        const fn new(state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>) -> Self {
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
            self.with_state(super::runtime_state::ProviderState::record_success);
        }

        fn record_failure(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| {
                state.record_failure(Instant::now());
            });
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

/// RAII backstop that releases a half-open circuit-breaker probe slot if the
/// dispatch future is dropped before any outcome settles it.
///
/// `gate_check` claims the single half-open probe slot
/// (`half_open_in_flight = true`) BEFORE the dispatch awaits the upstream.
/// Every synchronous outcome arm already settles the slot (`record_success` /
/// `record_failure` / `park_provider` / `release_probe_slot`). The gap this
/// guards is async CANCELLATION: if the future is dropped while awaiting a
/// hung upstream (client disconnect or client-side timeout), none of those
/// arms run and the slot stays claimed forever -- every later probe then sees
/// `CircuitOpen` and the breaker latches open until process restart.
///
/// Held across the upstream `.await`(s); on drop it frees the slot unless an
/// outcome already settled it (`disarm`, mirroring `BreakerAccounting`'s
/// `settled` flag). Freeing -- rather than recording a failure -- is
/// deliberate: a cancelled probe is no evidence of upstream health, so we free
/// the slot while leaving `circuit_opened_at` + the cooldown intact; the next
/// post-cooldown request becomes the probe and the breaker recovers.
///
/// Every synchronous settle site pairs its outcome call with `disarm()`.
/// `record_failure` / `record_success` / `park_provider` already clear
/// `half_open_in_flight` internally, so disarm there only suppresses a
/// redundant (idempotent, harmless) drop-time release; `release_probe_slot`
/// sites clear it explicitly. A NEW settle site MUST also call `disarm()`, or
/// the guard's drop would free a slot a concurrent probe may have re-claimed.
struct ProbeSlotGuard {
    /// `Some` while armed; `None` once an outcome settled the slot or the
    /// dispatch never claimed it.
    state: Option<Arc<Mutex<ProviderState>>>,
}

impl ProbeSlotGuard {
    /// Arm a guard for a dispatch that claimed the half-open probe slot. Pass
    /// `None` for a dispatch that did not (closed breaker): the guard is then
    /// inert and its drop is a no-op.
    const fn new(state: Option<Arc<Mutex<ProviderState>>>) -> Self {
        Self { state }
    }

    /// An outcome has settled the slot; drop must not touch it.
    fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for ProbeSlotGuard {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            // release_probe_slot is idempotent (it only sets a bool false). If
            // a concurrent caller re-claimed the slot between our settle and
            // this drop, freeing it here opens at most a transient extra probe
            // window -- never a failure record, never a latch.
            state.lock().release_probe_slot();
        }
    }
}

/// A lapsed learned negative whose single re-probe slot a request claimed
/// while filtering its chain. `feature` is the NORMALIZED capability key, so
/// settling it targets the exact registry entry `acting_negative_for`
/// claimed. Carried out of the filter so the dispatch path can settle the
/// probe.
struct ProbeAdmission {
    state_key: String,
    feature: String,
    provider_kind: &'static str,
}

/// Settles the learned-capability re-probes a target's dispatch was admitted
/// to run -- the [`ProbeSlotGuard`] pattern applied to the learned registry's
/// `in_flight` slots rather than the breaker's.
///
/// A single target can be admitted to re-probe several distinct learned
/// negatives at once (one admission per `(state_key, feature)`), so the guard
/// holds EVERY admission that target owns and settles each on the dispatch
/// outcome. Held across the whole chain-iteration (including same-provider
/// retries, which stay within the iteration). A 2xx settles all of them:
/// [`settle_success`](Self::settle_success) clears every held entry (proof the
/// capability is not rejected). [`settle_same_capability`](Self::settle_same_capability)
/// refreshes the one matching entry with capped backoff and drops it from the
/// held set. Any other way of leaving the target -- fallback, terminal error,
/// gate block, cancellation -- drops the guard, which records `OtherError` for
/// each still-held admission: the `in_flight` slot is released and the entry
/// stays expired so the next request re-probes (a transient must never clear a
/// valid negative).
struct LearnedProbeGuard {
    /// `Some` while any held admission is unsettled; `None` once every
    /// admission settled or the target was never a re-probe admission.
    registry: Option<Arc<crate::learned_capability::LearnedCapabilityRegistry>>,
    /// The still-unsettled admissions this target owns, each self-describing
    /// its `(state_key, feature, provider_kind)`.
    probes: Vec<ProbeAdmission>,
    /// Dispatch surface for the settlement observability event
    /// (`complete` | `stream`); every reached-target settlement emits under it.
    surface: &'static str,
}

impl LearnedProbeGuard {
    /// Arm a guard for a target admitted to re-probe one or more negatives.
    const fn armed(
        registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
        probes: Vec<ProbeAdmission>,
        surface: &'static str,
    ) -> Self {
        Self {
            registry: Some(registry),
            probes,
            surface,
        }
    }

    /// An inert guard for a target that was not a re-probe admission; its
    /// drop is a no-op.
    const fn inert() -> Self {
        Self {
            registry: None,
            probes: Vec::new(),
            surface: "",
        }
    }

    /// The dispatch succeeded (2xx): clear every held entry, then disarm.
    fn settle_success(&mut self) {
        if let Some(registry) = self.registry.take() {
            let now = Instant::now();
            for probe in self.probes.drain(..) {
                registry.record_probe_outcome(
                    &probe.state_key,
                    &probe.feature,
                    probe.provider_kind,
                    crate::learned_capability::ProbeOutcome::Success,
                    now,
                );
                emit_probe_settlement(&probe, self.surface, "success", true, "success");
            }
        }
    }

    /// The dispatch hit the same capability rejection for one held probe:
    /// refresh that entry with capped backoff and drop it from the held set.
    /// Returns `true` when a held probe matched.
    fn settle_same_capability(
        &mut self,
        state_key: &str,
        feature: &str,
        provider_kind: &str,
    ) -> bool {
        if self.registry.is_none() {
            return false;
        }
        let Some(pos) = self.probes.iter().position(|probe| {
            probe.state_key == state_key
                && probe.feature == feature
                && probe.provider_kind == provider_kind
        }) else {
            return false;
        };
        let probe = self.probes.remove(pos);
        if let Some(registry) = &self.registry {
            registry.record_probe_outcome(
                &probe.state_key,
                &probe.feature,
                probe.provider_kind,
                crate::learned_capability::ProbeOutcome::SameCapabilityRejection,
                Instant::now(),
            );
            emit_probe_settlement(
                &probe,
                self.surface,
                "same_capability",
                true,
                "same_capability",
            );
        }
        // Once the last held admission settles, disarm so drop is a no-op.
        if self.probes.is_empty() {
            self.registry = None;
        }
        true
    }
}

impl Drop for LearnedProbeGuard {
    fn drop(&mut self) {
        if let Some(registry) = &self.registry {
            let now = Instant::now();
            for probe in &self.probes {
                registry.record_probe_outcome(
                    &probe.state_key,
                    &probe.feature,
                    probe.provider_kind,
                    crate::learned_capability::ProbeOutcome::OtherError,
                    now,
                );
                emit_probe_settlement(probe, self.surface, "other_error", true, "terminal");
            }
        }
    }
}

/// Request-scoped owner of the re-probe admissions a chain filter staged,
/// grouped by the target that must settle them. Declared before the chain
/// loop in `complete_inner` and `stream_inner`; holds every admission the
/// filter recorded until the loop either reaches a target (transfer) or the
/// request leaves dispatch (settle-on-drop).
///
/// Transfer semantics: [`take`](Self::take) MOVES a target's admissions out
/// of the set into that target's [`LearnedProbeGuard`] when the loop reaches
/// it -- from that point the guard owns them and settles each on the dispatch
/// outcome (Success / SameCapabilityRejection / drop=OtherError). Whatever is
/// still held when the set drops -- an earlier target already returned success,
/// a terminal non-fallbackable error, a `break 'chain` under disable_fallbacks,
/// `?` propagation, or a client disconnect mid-dispatch -- was NEVER reached,
/// so its `in_flight` slot would otherwise latch forever; the drop settles
/// each held admission as `OtherError`, which releases only `in_flight` (it
/// neither confirms nor extends the negative) so the next request re-probes.
///
/// The move is what makes settlement exact-once STRUCTURAL: an admission is
/// owned by the set OR a target guard, never both, so no admission is ever
/// settled twice.
struct ProbeAdmissionSet {
    /// Still-held admissions, grouped by the `state_key` of the target that
    /// would settle them once reached.
    pending: HashMap<String, Vec<ProbeAdmission>>,
    registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
    /// Dispatch surface for the settlement observability event
    /// (`complete` | `stream`).
    surface: &'static str,
}

impl ProbeAdmissionSet {
    /// Group the filter's flat admission list by settling `state_key`.
    fn new(
        registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
        admissions: Vec<ProbeAdmission>,
        surface: &'static str,
    ) -> Self {
        let mut pending: HashMap<String, Vec<ProbeAdmission>> = HashMap::new();
        for admission in admissions {
            pending
                .entry(admission.state_key.clone())
                .or_default()
                .push(admission);
        }
        Self {
            pending,
            registry,
            surface,
        }
    }

    /// Move this target's admissions out of the set into its
    /// [`LearnedProbeGuard`]. Once taken the set no longer owns them, so the
    /// set's drop cannot settle them a second time.
    fn take(&mut self, state_key: &str) -> Option<Vec<ProbeAdmission>> {
        self.pending.remove(state_key)
    }
}

impl Drop for ProbeAdmissionSet {
    fn drop(&mut self) {
        let now = Instant::now();
        for admissions in self.pending.values() {
            for admission in admissions {
                self.registry.record_probe_outcome(
                    &admission.state_key,
                    &admission.feature,
                    admission.provider_kind,
                    crate::learned_capability::ProbeOutcome::OtherError,
                    now,
                );
                emit_probe_settlement(admission, self.surface, "other_error", false, "unreached");
            }
        }
    }
}

/// Emit the probe-settlement observability event for one admission. DEBUG
/// level: routine per-request bookkeeping, not an operator-actionable signal.
/// Capability TOKEN + state_key only -- never a request body. `outcome` is the
/// settlement disposition (`success` | `same_capability` | `other_error`) and
/// `reason` its settlement cause (`success` | `same_capability` | `terminal` |
/// `unreached`); `reached_target` is false only for a never-reached admission.
fn emit_probe_settlement(
    admission: &ProbeAdmission,
    surface: &str,
    outcome: &str,
    reached_target: bool,
    reason: &str,
) {
    tracing::debug!(
        event = "probe_settlement",
        state_key = %admission.state_key,
        capability_key = %admission.feature,
        provider_kind = admission.provider_kind,
        surface,
        outcome,
        reached_target,
        reason,
        "learned re-probe admission settled",
    );
}

/// Open the upstream stream and pull the first chunk. If that initial step
/// fails with a fallbackable error, return it so the caller can try the next
/// provider. If the first chunk arrives, return a `BoxStream` that yields it
/// followed by the rest of the upstream stream -- mid-stream errors propagate.
///
/// `policy.stream_first_byte_timeout_ms` (when set) caps the wait for the
/// stream-open + first-chunk arrival; expiry surfaces as a status-0
/// upstream error which is fallbackable per `should_fallback`.
///
/// Also emits a debug-level first-activity log the moment the upstream
/// response headers arrive, ahead of the first-chunk wait below (M4:
/// see the `attempt_start` comment inside).
async fn try_stream_with_first_chunk(
    provider_name: &str,
    upstream_model: &str,
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    // Per-attempt clock for the first-activity mark below -- NOT the
    // request-level clock `UsageCapture::start` uses for `ttfb_ms`
    // (that lives in a higher crate this one cannot see). For the
    // common case (no prior fallback hop on this request) the two are
    // within noise of each other, so `elapsed_ms` here is directly
    // comparable to the ledger's `ttfb_ms` to derive the first-activity
    // -> first-content gap (M4). A request that fell back through one
    // or more dead chain entries first will show extra unaccounted time
    // on the ttfb side that this attempt's clock does not carry -- that
    // is chain-walk overhead, a separate concern from the prefill gap
    // this instrumentation targets.
    let attempt_start = Instant::now();
    let open_and_first = async {
        let mut upstream = provider.stream(req).await?;
        // First sign of upstream life: response headers arrived (every
        // egress's `stream()` awaits `client.execute()` -- which
        // resolves once the status line + headers are in -- BEFORE
        // constructing the lazy body-byte stream below). Distinct from
        // the first-CONTENT mark (`mark_first_byte` in
        // `ingress_handle.rs`), which additionally waits out any
        // upstream `message_start`/`ping` events the SSE parser
        // swallows. One site here covers every provider (Anthropic,
        // Bedrock, gemini, openai-compat, openai-responses) since they
        // all share this `stream()` -> execute -> byte-stream shape.
        // Debug-only; no bodies/prompts/PII, structured fields only.
        // Manual capture recipe: docs/LOGGING.md, "First-activity mark
        // (M4)" -- run with ROUTECTL_LOG=routectl_router=debug and issue
        // a streaming request; this line's `elapsed_ms` is the gap
        // between upstream headers and the existing first-content
        // ttfb_ms mark.
        tracing::debug!(
            provider = provider_name,
            upstream = upstream_model,
            elapsed_ms = attempt_start.elapsed().as_millis() as u64,
            "stream first-activity: upstream response headers received",
        );
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
const fn probe_fast_fail_status(err: &Error) -> Option<u16> {
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

/// The upstream status of a forwarded-credential auth/rate failure that
/// is TERMINAL for a forwarded (pure-passthrough) request: 401
/// (unauthorized), 403 (forbidden), 429 (rate limited). `None` for every
/// other error class.
///
/// A request-scoped forwarded token has no refresh path (routectl cannot
/// rotate a credential it does not own) and no sibling credential to fall
/// back to, so on such a status the router must bypass BOTH the
/// `on_auth_failure` refresh-and-retry AND the fallback-chain hop, and
/// surface the upstream response verbatim -- Claude Code owns its own
/// retry/backoff. Scoped to a forwarded TARGET ONLY: the caller gates
/// this on the current dispatch target's `use_forwarded_credential`
/// flag, so a coexisting own-credential target in the same chain keeps
/// the existing one-shot auth-refresh + fallback behavior unchanged.
/// Same-request TRANSPORT retries (5xx / network / status-0, which
/// reuse the same token) are NOT in this set, so they fall through to
/// the normal predicates untouched. Pattern-mirrors
/// `probe_fast_fail_status`: a pure decision helper; the caller owns
/// the `return` that short-circuits.
const fn forwarded_terminal_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream {
            status: s @ (401 | 403 | 429),
            ..
        } => Some(*s),
        _ => None,
    }
}

/// WARN when a forwarded-token upstream auth/rate failure is surfaced
/// verbatim (terminal: no refresh-retry, no fallback hop). SAFE
/// dimensions ONLY: the upstream `status`, a fixed `credential_source`
/// token, and a boolean derived from whether an inbound session key was
/// captured -- NEVER the forwarded token itself, in a field or the
/// message. Mirrors `log_probe_fast_fail`: log-only, the caller owns the
/// short-circuiting `return`.
fn log_forwarded_auth_terminal(status: u16, has_client_session_id: bool) {
    tracing::warn!(
        status,
        credential_source = "forwarded",
        has_client_session_id,
        "forwarded-token upstream auth failure surfaced verbatim; \
         bypassing on_auth_failure refresh and provider fallback",
    );
}

/// Missing-bearer terminal guard. `None` when `target` does not use a
/// forwarded credential, or the client's bearer WAS captured; `Some`
/// carries a clean terminal [`Error::Validation`] for the caller to
/// return immediately -- BEFORE any upstream touch of this target.
///
/// A forwarded target with no captured bearer (seam header absent,
/// capture gates closed, or the client simply never sent an
/// `Authorization` header) has no credential to authenticate with; an
/// upstream dispatch would either fail with a confusing anonymous
/// request or -- worse -- silently succeed against the wrong identity
/// if the egress falls back to some other implicit auth. Refusing here
/// gives the client an unambiguous 4xx instead of a passed-through
/// upstream 401. Per-target (the caller checks this inside the chain
/// loop, once per target about to be dispatched), so a mixed chain that
/// never reaches a forwarded target is unaffected. Emits a WARN with
/// SAFE dimensions only (reason, credential_source, provider_kind --
/// never the token, host, or provider name), mirroring the shape of the
/// deleted whole-chain gate's refusal log.
fn missing_forwarded_bearer_error(target: &DispatchTarget, req: &ChatRequest) -> Option<Error> {
    if !target.use_forwarded_credential || req.routectl_internal.forwarded_bearer.is_some() {
        return None;
    }
    tracing::warn!(
        reason = "missing_forwarded_bearer",
        credential_source = "forwarded",
        provider_kind = target.provider_kind.unwrap_or("unknown"),
        "forwarded target has no captured client bearer; refusing before egress",
    );
    Some(Error::Validation(
        "forwarded target has no captured client bearer to authenticate this request \
         (reason=missing_forwarded_bearer)"
            .to_string(),
    ))
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

/// A count_tokens CAPABILITY signal -- the selected target is
/// count_tokens-capable by kind (`anthropic-api`), but its concrete
/// upstream cannot actually count. Two shapes carry this:
///
/// - `Error::NotImplemented` -- the local trait-default (a provider that
///   never overrode `count_tokens`), and
/// - `Error::Upstream { status: 501, .. }` -- a WIRE 501 from an
///   upstream (e.g. an anthropic-api base_url that back-hops to a
///   Bedrock egress with no count_tokens endpoint).
///
/// Both mean "this seat cannot count", NOT "this seat is unhealthy". The
/// count_tokens walk treats them as capability signals: release the
/// probe slot without debiting the breaker and advance to the next
/// capable seat. It must NEVER reach `should_fallback` / `record_failure`
/// -- a capability signal recorded as health would trip the per-seat
/// breaker that completions gate on. Scoped to the count_tokens path
/// ONLY: on the completion path a wire 501 is a genuine upstream fault
/// and must still trip the breaker.
const fn is_capability_error(err: &Error) -> bool {
    matches!(
        err,
        Error::NotImplemented(..) | Error::Upstream { status: 501, .. }
    )
}

/// Which dispatch surface an error arm belongs to. Carried as a stable
/// `surface` field on the router's class-decision observability events so
/// operators can tell a completion failure from a stream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchSurface {
    Complete,
    Stream,
}

impl DispatchSurface {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Stream => "stream",
        }
    }
}

/// Safe, structured facts pulled from an [`Error`] for the router's
/// class-decision observability. Deliberately EXCLUDES the body and the
/// Display/Debug string: only the numeric status and the
/// already-structured classifier tokens are carried, so no prompt, body,
/// header, or free-form upstream message text can leak into a field.
/// `status` is `Some` iff the error is an [`Error::Upstream`].
#[derive(Debug, Clone, Copy)]
struct UpstreamFacts<'a> {
    status: Option<u16>,
    upstream_type: Option<&'a str>,
    upstream_code: Option<&'a str>,
}

fn upstream_facts(err: &Error) -> UpstreamFacts<'_> {
    match err {
        Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => UpstreamFacts {
            status: Some(*status),
            upstream_type: upstream_type.as_deref(),
            upstream_code: upstream_code.as_deref(),
        },
        _ => UpstreamFacts {
            status: None,
            upstream_type: None,
            upstream_code: None,
        },
    }
}

/// Stable, low-cardinality label for a [`FailureClass`]. The
/// `FeatureUnsupported` capability is surfaced in its own field, so the
/// label collapses that variant to a bare token. Fail-closed: any class
/// the classifier gains later renders as `unknown`.
const fn class_label(class: &FailureClass) -> &'static str {
    match class {
        FailureClass::RateLimited => "rate_limited",
        FailureClass::Auth => "auth",
        FailureClass::BadRequest => "bad_request",
        FailureClass::ContentPolicy => "content_policy",
        FailureClass::ContextWindow => "context_window",
        FailureClass::ServerError => "server_error",
        FailureClass::Timeout => "timeout",
        FailureClass::NetworkError => "network_error",
        FailureClass::Overloaded => "overloaded",
        FailureClass::FeatureUnsupported { .. } => "feature_unsupported",
        FailureClass::Unknown => "unknown",
        _ => "unknown",
    }
}

/// Stable label for how the classification was decided.
const fn matched_by_label(matched_by: MatchedBy) -> &'static str {
    match matched_by {
        MatchedBy::Variant => "variant",
        MatchedBy::Status => "status",
        MatchedBy::UpstreamType => "upstream_type",
    }
}

/// The same-provider retry cap for `class` under `policy` -- the value the
/// retry branch compares `attempts_made` against. Delegates to
/// [`RetryPolicy::resolved_class`], which layers any operator per-class
/// `[retry.classes]` override on top of the baked class default. Shared by
/// [`should_retry_same_provider`] and the class-decision observability so
/// the logged cap never drifts from the cap actually enforced.
fn retry_cap_for(class: &FailureClass, policy: &RetryPolicy) -> u32 {
    policy.resolved_class(class).0
}

/// Safe, structured inputs for the per-arm class-decision observability
/// event. Carries only already-structured facts: NEVER a body, prompt,
/// header, token, or the [`Error`] Display/Debug string.
struct ClassDecisionObs<'a> {
    provider: &'a str,
    model: &'a str,
    surface: DispatchSurface,
    /// The classifier's decision BEFORE any operator status remap.
    original_class: &'a FailureClass,
    /// The class every downstream consumer (predicates, debit, this
    /// event) actually acted on -- `original_class` unless a remap fired.
    effective_class: &'a FailureClass,
    /// How the NATIVE classification was decided; unaffected by a remap.
    matched_by: MatchedBy,
    facts: UpstreamFacts<'a>,
    fallback: bool,
    retry_cap: u32,
    /// The policy-wide hard ceiling on same-provider attempts. Invariant:
    /// `hard_retry_cap >= retry_cap` for every class, since the ceiling
    /// folds the per-class overlay. Emitted alongside `retry_cap` so a
    /// drift between the logged cap and the enforced one is visible.
    hard_retry_cap: u32,
    debit: bool,
    is_probe: bool,
    is_forwarded: bool,
    /// Whether the operator's per-provider `class_overrides` replaced the
    /// native class for this error.
    remapped: bool,
    /// The status key that matched an override, iff `remapped`. `None`
    /// when `remapped` is false.
    remap_status: Option<u16>,
}

/// Emit exactly one structured event per error-arm pass at the point the
/// class decision is settled. DEBUG normally; WARN when the classifier
/// failed closed (`FailureClass::Unknown`) on a real upstream outcome --
/// a silent fail-closed-unknown on a genuine upstream response would hide
/// a gap in the status map / token vocabulary. Safe dimensions only.
fn emit_class_decision(obs: &ClassDecisionObs<'_>) {
    let unknown_upstream =
        matches!(obs.effective_class, FailureClass::Unknown) && obs.facts.status.is_some();
    if unknown_upstream {
        tracing::warn!(
            provider = obs.provider,
            model = obs.model,
            surface = obs.surface.as_str(),
            status = ?obs.facts.status,
            upstream_type = obs.facts.upstream_type.unwrap_or(""),
            original_class = class_label(obs.original_class),
            effective_class = class_label(obs.effective_class),
            matched_by = matched_by_label(obs.matched_by),
            fallback = obs.fallback,
            retry_cap = obs.retry_cap,
            hard_retry_cap = obs.hard_retry_cap,
            debit = obs.debit,
            is_probe = obs.is_probe,
            is_forwarded = obs.is_forwarded,
            remapped = obs.remapped,
            remap_status = ?obs.remap_status,
            "unknown failure classification on upstream outcome (fail-closed)",
        );
    } else {
        tracing::debug!(
            provider = obs.provider,
            model = obs.model,
            surface = obs.surface.as_str(),
            status = ?obs.facts.status,
            upstream_type = obs.facts.upstream_type.unwrap_or(""),
            original_class = class_label(obs.original_class),
            effective_class = class_label(obs.effective_class),
            matched_by = matched_by_label(obs.matched_by),
            fallback = obs.fallback,
            retry_cap = obs.retry_cap,
            hard_retry_cap = obs.hard_retry_cap,
            debit = obs.debit,
            is_probe = obs.is_probe,
            is_forwarded = obs.is_forwarded,
            remapped = obs.remapped,
            remap_status = ?obs.remap_status,
            "router failure class decision",
        );
    }
}

/// Emit the stable FeatureUnsupported observability event at a dispatch
/// error arm. Fired only when the classifier lifted the failure to
/// [`FailureClass::FeatureUnsupported`]. Carries only safe, structured
/// dimensions -- NEVER a body, prompt, header, token, or the error's
/// Display/Debug text. `capability` is the upstream token the classifier
/// matched, already best-effort and non-sensitive. `remapped` is true
/// when this FeatureUnsupported came from an operator status remap
/// (carrying the `OPERATOR_REMAP_CAPABILITY` token) rather than a real
/// upstream lift.
#[allow(clippy::too_many_arguments)]
fn emit_feature_unsupported(
    provider: &str,
    provider_kind: Option<&str>,
    model: &str,
    capability: &str,
    facts: &UpstreamFacts<'_>,
    matched_by: MatchedBy,
    surface: DispatchSurface,
    is_forwarded: bool,
    remapped: bool,
) {
    tracing::info!(
        target: "routectl::feature_unsupported",
        provider,
        provider_kind = provider_kind.unwrap_or(""),
        model,
        capability,
        status = facts.status.unwrap_or(0),
        upstream_type = facts.upstream_type.unwrap_or(""),
        upstream_code = facts.upstream_code.unwrap_or(""),
        matched_by = matched_by_label(matched_by),
        surface = surface.as_str(),
        is_forwarded,
        remapped,
        "upstream reported an unsupported capability",
    );
}

/// The upstream HTTP status carried by `err`, for the per-provider class
/// remap lookup ONLY: `Some` for an [`Error::Upstream`] status in
/// `400..=599`, `None` for status 0 and every non-upstream variant. This
/// seam consults the target's own `class_overrides`, never `policy`.
fn upstream_status_for_remap(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream { status, .. } if (400..=599).contains(status) => Some(*status),
        _ => None,
    }
}

/// Apply the operator's per-provider status remap on top of the
/// classifier's native decision. A `status` that keys into `overrides`
/// replaces the class with the operator's override, keeping the NATIVE
/// `matched_by` -- the remap changes WHICH class was decided, not HOW the
/// classifier itself matched. No status, or no matching key (including
/// the empty-map default), returns `native` unchanged. Returns the
/// effective `ClassifiedFailure` plus whether a remap fired.
fn apply_remap(
    native: ClassifiedFailure,
    status: Option<u16>,
    overrides: &BTreeMap<u16, FailureClass>,
) -> (ClassifiedFailure, bool) {
    match status.and_then(|s| overrides.get(&s)) {
        Some(class) => (
            ClassifiedFailure {
                class: class.clone(),
                matched_by: native.matched_by,
            },
            true,
        ),
        None => (native, false),
    }
}

/// Whether a failure of this class debits the per-seat circuit breaker's
/// health accounting. True ONLY for the fixed transient-health set --
/// conditions a fallback or a cooldown can recover from; false for
/// caller-shaped or capability faults that retrying the same seat would
/// never fix, and (fail-closed) for any class the classifier gains later.
///
/// Deliberately independent of the fallback/retry decision: routing
/// (whether to advance the chain) and health accounting (whether the seat
/// looks unhealthy) are separate concerns.
const fn class_debits(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::RateLimited
            | FailureClass::ServerError
            | FailureClass::Timeout
            | FailureClass::NetworkError
            | FailureClass::Overloaded
    )
}

fn should_fallback(
    err: &Error,
    class: &FailureClass,
    policy: &RetryPolicy,
    is_probe: bool,
) -> bool {
    // Availability-probe fast-fail: a probe (max_tokens <=
    // probe_max_tokens) that hits a rate-limit (429) or overload (529)
    // does not fall back. Every OTHER error class -- generic 5xx,
    // network/status-0, Streaming, and every 4xx including the
    // Bedrock-style max_tokens=1 400 -- falls through to the normal
    // predicate below, so real fallback is untouched.
    if is_probe && probe_fast_fail_status(err).is_some() {
        return false;
    }
    // An unknown provider id in the chain is a config-shaped fault the
    // caller routes past: always fallbackable, independent of class.
    if let Error::UnknownProvider(_) = err {
        return true;
    }
    policy.resolved_class(class).1
}

fn should_retry_same_provider(
    err: &Error,
    class: &FailureClass,
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
    let cap = retry_cap_for(class, policy);
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
        .map_or(0, |d| d.subsec_nanos() as u64);
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
        let base = RetryPolicy {
            stream_first_byte_timeout_ms: None, // alias left this unset
            ..RetryPolicy::default()
        };
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
            stream_first_byte_timeout_ms: None, // alias left this unset
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
            stream_first_byte_timeout_ms: None, // alias left this unset
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
        let base = RetryPolicy {
            stream_first_byte_timeout_ms: None, // alias left this unset
            ..RetryPolicy::default()
        };
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
        let base = RetryPolicy {
            stream_first_byte_timeout_ms: None, // alias left this unset
            ..RetryPolicy::default()
        };
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
        // request timeout). `should_fallback` returns true for the
        // network-error class default; the predicate governs HTTP-status
        // outcomes (>= 400) via per-class policy, and a status-0 network
        // error resolves through the NetworkError class, which falls back
        // by default.
        let err = Error::upstream("p", 0, "tcp connect refused");
        let class = classify(&err, None).class;
        let policy = RetryPolicy::default();
        assert!(should_fallback(&err, &class, &policy, false));
    }

    // --- Per-class operator overrides route through `resolved_class` ---

    #[test]
    fn retry_override_on_one_class_leaves_the_sibling_5xx_class_untouched() {
        // Arrange: [retry.classes.overloaded] retry = 0, with a distinct
        // baked retry_on_5xx cap so a leak into ServerError is visible.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::Overloaded,
            ClassPolicy {
                retry: Some(0),
                fallback: None,
            },
        );
        let policy = RetryPolicy {
            retry_on_5xx: Some(3),
            classes,
            ..RetryPolicy::default()
        };
        let overloaded_err = Error::upstream_full(
            "p",
            503,
            "body",
            None,
            Some("overloaded_error".into()),
            None,
        );
        let server_err = Error::upstream("p", 500, "body");
        let overloaded_class = classify(&overloaded_err, None).class;
        let server_class = classify(&server_err, None).class;

        // Act + Assert: the overridden class caps to 0 and cannot retry.
        assert_eq!(retry_cap_for(&overloaded_class, &policy), 0);
        assert!(!should_retry_same_provider(
            &overloaded_err,
            &overloaded_class,
            &policy,
            0,
            false,
        ));
        // The un-overridden sibling class in the same baked 5xx family
        // keeps its own retry_on_5xx cap.
        assert_eq!(retry_cap_for(&server_class, &policy), 3);
        assert!(should_retry_same_provider(
            &server_err,
            &server_class,
            &policy,
            0,
            false,
        ));
        // Fallback is untouched for both -- only the retry leaf was
        // overridden.
        assert!(should_fallback(
            &overloaded_err,
            &overloaded_class,
            &policy,
            false
        ));
        assert!(should_fallback(&server_err, &server_class, &policy, false));
    }

    #[test]
    fn hard_retry_cap_folds_per_class_overlay_above_max_attempts() {
        // A per-class retry override above max_attempts must lift the hard
        // ceiling too, or the retry loop's hard-cap guard silently clips
        // the class cap the resolver honors.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let mut classes = BTreeMap::new();
        classes.insert(
            ConfigFailureClass::ServerError,
            ClassPolicy {
                retry: Some(5),
                fallback: None,
            },
        );
        let policy = RetryPolicy {
            max_attempts: 2,
            classes,
            ..RetryPolicy::default()
        };

        assert_eq!(policy.hard_retry_cap(), 5);

        let server_err = Error::upstream("p", 500, "body");
        let server_class = classify(&server_err, None).class;
        assert_eq!(retry_cap_for(&server_class, &policy), 5);
        assert!(
            policy.hard_retry_cap() >= retry_cap_for(&server_class, &policy),
            "hard cap must never sit below an enforced class cap"
        );
    }

    #[test]
    fn emit_class_observability_logs_enforced_and_hard_retry_cap() {
        // The emitted class-decision event must carry the SAME retry_cap
        // the shared resolver enforces, plus hard_retry_cap, so logging can
        // never drift from enforcement.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};

        struct StubProvider;
        #[async_trait::async_trait]
        impl Provider for StubProvider {
            fn id(&self) -> &'static str {
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

        let router = build_router_with_provider_timeouts(None, None);
        let provider: Arc<dyn Provider> = Arc::new(StubProvider);
        let model = Arc::new(ResolvedModel::new("nick", "p1", provider, "upstream"));
        let target = into_one_dispatch_target(model);

        let mut classes = BTreeMap::new();
        classes.insert(
            ConfigFailureClass::ServerError,
            ClassPolicy {
                retry: Some(5),
                fallback: None,
            },
        );
        let policy = RetryPolicy {
            max_attempts: 2,
            classes,
            ..RetryPolicy::default()
        };

        let err = Error::upstream("p1", 500, "body");
        let cf = classify(&err, None);
        let expected_retry = retry_cap_for(&cf.class, &policy);
        let expected_hard = policy.hard_retry_cap();

        let events = routectl_testkit::capture_events(|| {
            router.emit_class_observability(
                &err,
                &cf,
                &cf.class,
                false,
                None,
                DispatchSurface::Complete,
                "p1",
                &target,
                false,
                &policy,
                false,
                false,
                false,
            );
        });

        let decision = events
            .iter()
            .find(|e| e.message == "router failure class decision")
            .expect("one class-decision event emitted");
        let retry_str = expected_retry.to_string();
        let hard_str = expected_hard.to_string();
        assert_eq!(decision.field("retry_cap"), Some(retry_str.as_str()));
        assert_eq!(decision.field("hard_retry_cap"), Some(hard_str.as_str()));
        assert_eq!(
            expected_retry, 5,
            "class overlay must lift the enforced cap"
        );
        assert!(
            expected_hard >= expected_retry,
            "emitted hard cap must never sit below the enforced retry cap"
        );
    }

    #[test]
    fn fallback_override_on_bad_request_leaves_auth_untouched() {
        // Arrange: [retry.classes.bad-request] fallback = false.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::BadRequest,
            ClassPolicy {
                retry: None,
                fallback: Some(false),
            },
        );
        let policy = RetryPolicy {
            classes,
            ..RetryPolicy::default()
        };
        let bad_request_err = Error::upstream("p", 400, "body");
        let auth_err = Error::upstream("p", 401, "body");
        let bad_request_class = classify(&bad_request_err, None).class;
        let auth_class = classify(&auth_err, None).class;

        // Act + Assert: a plain 400 stops falling back...
        assert!(!should_fallback(
            &bad_request_err,
            &bad_request_class,
            &policy,
            false,
        ));
        // ...but a 401 (different class, no overlay entry) still does.
        assert!(should_fallback(&auth_err, &auth_class, &policy, false));
    }

    #[test]
    fn unknown_provider_falls_back_regardless_of_class_or_override() {
        // Regression: `Error::UnknownProvider` short-circuits to true
        // BEFORE the class match, independent of both the class passed in
        // (classify() never sees an UnknownProvider, so this pins the
        // caller can pass any class here) and any per-class override that
        // would otherwise deny fallback for that class.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let err = Error::UnknownProvider("missing-provider".to_string());
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::BadRequest,
            ClassPolicy {
                retry: None,
                fallback: Some(false),
            },
        );
        let policy = RetryPolicy {
            classes,
            ..RetryPolicy::default()
        };
        assert!(should_fallback(
            &err,
            &FailureClass::BadRequest,
            &policy,
            false,
        ));
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
        old.state.insert("model-x".to_string(), old_only_arc);

        let mut new = Router::new(config);
        let fresh_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
        new.state.insert("model-a".to_string(), fresh_arc);
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

    #[test]
    fn carry_over_sticky_from_preserves_pins() {
        // Regression guard for the silent-collapse trap: a hot-reload must
        // NOT drop StickyLeastLoaded pins, or every live conversation cold-
        // misses its warm-cache seat.

        // Arrange: pin a session in the outgoing Router, with the one-time
        // overflow marker set so we can prove it survives the rebuild.
        let config = Arc::new(Config::default());
        let before = Router::new(config.clone());
        before.sticky_pins.put(
            "sess-1",
            crate::seat_pool::SeatPin {
                state_key: "opus#seat-b".into(),
                repinned: true,
            },
        );

        let mut after = Router::new(config);

        // Act
        after.carry_over_sticky_from(&before);

        // Assert: the pin survived the rebuild, INCLUDING the repinned flag --
        // otherwise a reload would reset the one-time cap and re-open the flap
        // window.
        let entries = after.sticky_pins.export_entries();
        assert!(
            entries.contains(&(
                "sess-1".to_string(),
                crate::seat_pool::SeatPin {
                    state_key: "opus#seat-b".to_string(),
                    repinned: true,
                }
            )),
            "carry_over_sticky_from must preserve session->seat pins (with the \
             repinned flag) across a rebuild",
        );
    }

    #[test]
    fn carry_over_k_store_from_preserves_windows_and_lru_order() {
        // Regression guard for the silent-collapse trap, K-store edition: a
        // hot-reload must NOT drop per-session K windows, and it must keep
        // their LRU ordering so the destination's eviction frontier matches
        // what the source would have evicted next.
        use crate::k_estimator::{KSessionKey, KSessionWindow, Sample};
        use std::time::{Duration, UNIX_EPOCH};

        fn key(session: &str) -> KSessionKey {
            KSessionKey {
                session_key: session.into(),
                provider_kind: "anthropic-api".into(),
                model: "opus".into(),
            }
        }

        fn sample(secs: u64, reused: bool) -> Sample {
            Sample {
                ts: UNIX_EPOCH + Duration::from_secs(secs),
                observed_reuse: reused,
            }
        }

        // Arrange: insert A, B, C in that order, then touch A so the source's
        // LRU order is [B (LRU), C, A (MRU)].
        let config = Arc::new(Config::default());
        let before = Router::new(config.clone());
        let mut win_a = KSessionWindow::new();
        win_a.push(sample(1, true));
        let mut win_b = KSessionWindow::new();
        win_b.push(sample(2, false));
        let mut win_c = KSessionWindow::new();
        win_c.push(sample(3, true));
        before.k_session_store.put(key("A"), win_a.clone());
        before.k_session_store.put(key("B"), win_b.clone());
        before.k_session_store.put(key("C"), win_c.clone());
        let _ = before.k_session_store.get(&key("A"));

        let mut after = Router::new(config);

        // Act
        after.carry_over_k_store_from(&before);

        // Assert: every entry survived AND the LRU order matches the source.
        // A scattered carry-over (e.g. HashMap iteration order) would pass
        // the per-key survival check but fail this ordering one.
        let entries = after.k_session_store.export_entries();
        let observed_keys: Vec<&KSessionKey> = entries.iter().map(|(k, _)| k).collect();
        assert_eq!(
            observed_keys,
            vec![&key("B"), &key("C"), &key("A")],
            "carry_over_k_store_from must preserve LRU recency order",
        );
        let observed_windows: Vec<&KSessionWindow> = entries.iter().map(|(_, w)| w).collect();
        assert_eq!(observed_windows, vec![&win_b, &win_c, &win_a]);
    }

    #[test]
    fn router_new_builds_learned_registry_reflecting_config_knobs() {
        use routectl_core::capability::SignalTier;
        use std::time::{Duration, Instant};

        // Arrange: a `[capability]` block with a non-default 1h decay so the
        // smoke test can prove the registry was built from the config knobs
        // (not the registry's own hard-coded default).
        let mut config = Config::default();
        config.capability.decay_hours = 1;
        config.capability.inferred_window_hours = 1;
        let router = Router::new(Arc::new(config));

        // Assert: a fresh registry is present and empty.
        assert!(router.learned_capabilities.is_empty());

        // A self-identifying negative acts, then lapses into a re-probe
        // exactly at the configured 1h decay -- not the registry default.
        let t0 = Instant::now();
        router.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            t0,
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "web_search",
                "openai-compat",
                t0 + Duration::from_mins(30),
            ),
            crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
            "must still act well inside the configured decay window",
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "web_search",
                "openai-compat",
                t0 + Duration::from_hours(1) + Duration::from_secs(1),
            ),
            crate::learned_capability::RoutingDecision::ProbeAdmitted,
            "must lapse into a re-probe just past the configured 1h decay",
        );
    }

    #[test]
    fn router_new_builds_override_registry_with_static_provenance_from_legacy_config() {
        // Arrange: a legacy-only config (provider + model
        // `unsupported_features`, no `[capability.overrides]`). The
        // override read-model must be built from it at construction and
        // carry the legacy static provenance so labels stay unchanged.
        let toml_text = "\
            [providers.p]\n\
            kind = \"openai-compat\"\n\
            base_url = \"https://x\"\n\
            api_key_ref = \"literal:k\"\n\
            unsupported_features = [\"web_search\"]\n\
            [models.nick]\n\
            provider = \"p\"\n\
            upstream = \"gpt-x\"\n\
            unsupported_features = [\"computer_use\"]\n";
        let config: Config = toml::from_str(toml_text).expect("config parses");

        // Act
        let router = Router::new(Arc::new(config));

        // Assert: the accessor exposes a registry whose legacy entries
        // carry ProviderStatic / ModelStatic provenance.
        let registry = router.override_registry();
        assert_eq!(
            registry.resolve("p", "nick", "web_search", "openai-compat"),
            Some((
                crate::override_registry::OverrideVerdict::RouteAway,
                crate::override_registry::OverrideProvenance::ProviderStatic
            )),
        );
        assert_eq!(
            registry.resolve("p", "nick", "computer_use", "openai-compat"),
            Some((
                crate::override_registry::OverrideVerdict::RouteAway,
                crate::override_registry::OverrideProvenance::ModelStatic
            )),
        );
    }

    #[test]
    fn carry_over_learned_from_carries_when_catalog_and_overlay_unchanged() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // Arrange: learn a negative in the outgoing Router; both Routers
        // share the same catalog version and overlay revision (the
        // config-only reload case).
        let config = Arc::new(Config::default());
        let before = Router::new(config.clone());
        before.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let mut after = Router::new(config);
        assert_eq!(after.catalog_version, before.catalog_version);
        assert_eq!(after.overlay_revision, before.overlay_revision);

        // Act
        after.carry_over_learned_from(&before);

        // Assert: the negative rode across untouched; no invalidation.
        assert_eq!(after.learned_capabilities.snapshot().len(), 1);
        assert_eq!(after.metrics.invalidations_total(), 0);
    }

    #[test]
    fn carry_over_learned_from_clears_in_flight_slot() {
        use crate::learned_capability::RoutingDecision;
        use routectl_core::capability::SignalTier;
        use std::time::{Duration, Instant};

        // Arrange: a 1h decay so the probe slot can be claimed on a lapsed
        // entry. Learn a self-identifying negative, then admit a re-probe on
        // the outgoing Router so its entry carries `in_flight = true`.
        let mut config = Config::default();
        config.capability.decay_hours = 1;
        config.capability.inferred_window_hours = 1;
        let config = Arc::new(config);
        let before = Router::new(config.clone());
        let t0 = Instant::now();
        before.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            t0,
        );
        let t_probe = t0 + Duration::from_hours(1) + Duration::from_secs(1);
        assert_eq!(
            before.learned_capabilities.acting_negative_for(
                "nick",
                "web_search",
                "openai-compat",
                t_probe,
            ),
            RoutingDecision::ProbeAdmitted,
            "outgoing entry must hold an in-flight probe slot to carry across",
        );

        // Act: config-only reload -- same catalog version and overlay
        // revision, so the entry rides across.
        let mut after = Router::new(config);
        assert_eq!(after.catalog_version, before.catalog_version);
        assert_eq!(after.overlay_revision, before.overlay_revision);
        after.carry_over_learned_from(&before);

        // Assert: the entry rode across with its non-in-flight fields intact.
        let carried = after.learned_capabilities.snapshot();
        assert_eq!(carried.len(), 1);
        assert_eq!(carried[0].signal_tier, SignalTier::SelfIdentifying);

        // The carried entry is still acting AND its in-flight slot was
        // cleared, so the next matching request re-admits a probe rather than
        // latching on a slot no outcome on the new Router can ever release.
        let t_query = t_probe + Duration::from_secs(1);
        assert_eq!(
            after.learned_capabilities.acting_negative_for(
                "nick",
                "web_search",
                "openai-compat",
                t_query,
            ),
            RoutingDecision::ProbeAdmitted,
            "carried-over slot must not stay latched after the reload",
        );
    }

    #[test]
    fn carry_over_expires_learned_entries_whose_override_cell_changed() {
        use crate::learned_capability::RoutingDecision;
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // Arrange: the outgoing Router masked `web_search` on provider `p`
        // with a force_supported override; the incoming Router drops that
        // override. Both share catalog version + overlay revision (the
        // config-only reload case), so entries ride across.
        let before_cfg: Config = toml::from_str(
            "version = 3\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n",
        )
        .expect("config parses");
        let before = Router::new(Arc::new(before_cfg));
        let t0 = Instant::now();
        // A masked entry (the cell that changes) plus an unrelated healthy
        // entry (no override in either config).
        before
            .learned_capabilities
            .observe("p", "web_search", "", SignalTier::SelfIdentifying, t0);
        before.learned_capabilities.observe(
            "p",
            "computer_use",
            "",
            SignalTier::SelfIdentifying,
            t0,
        );

        let mut after = Router::new(Arc::new(Config::default()));
        assert_eq!(after.catalog_version, before.catalog_version);
        assert_eq!(after.overlay_revision, before.overlay_revision);

        // Act
        after.carry_over_learned_from(&before);

        // Assert: both entries carried across, no full invalidation.
        assert_eq!(after.learned_capabilities.snapshot().len(), 2);
        assert_eq!(after.metrics.invalidations_total(), 0);

        let now = Instant::now();
        // The changed cell's entry lapsed into a single re-probe...
        assert_eq!(
            after
                .learned_capabilities
                .acting_negative_for("p", "web_search", "", now),
            RoutingDecision::ProbeAdmitted,
            "an override cell that changed across reload must lapse its entry",
        );
        // ...while the unrelated healthy entry survived the reload intact.
        assert_eq!(
            after
                .learned_capabilities
                .acting_negative_for("p", "computer_use", "", now),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
            "an entry with no override change must ride across untouched",
        );
    }

    #[test]
    fn carry_over_learned_from_clears_and_warns_on_catalog_bump() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // Arrange
        let config = Arc::new(Config::default());
        let before = Router::new(config.clone());
        before.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let mut after = Router::new(config);
        // Simulate a baked-catalog version bump across the rebuild.
        after.catalog_version = before.catalog_version + 1;

        // Act
        let events = routectl_testkit::capture_events(|| {
            after.carry_over_learned_from(&before);
        });

        // Assert: fresher catalog truth wins -- registry starts empty, one
        // WARN names the catalog trigger, invalidation counter bumped.
        assert!(after.learned_capabilities.is_empty());
        assert_eq!(after.metrics.invalidations_total(), 1);
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("catalog bump must emit a WARN");
        assert_eq!(warn.field("event"), Some("invalidation"));
        assert_eq!(warn.field("catalog_changed"), Some("true"));
        assert_eq!(warn.field("overlay_changed"), Some("false"));
        let prev_cat = before.catalog_version.to_string();
        let cur_cat = after.catalog_version.to_string();
        assert_eq!(
            warn.field("previous_catalog_version"),
            Some(prev_cat.as_str())
        );
        assert_eq!(warn.field("catalog_version"), Some(cur_cat.as_str()));
        assert_eq!(warn.field("previous_overlay_revision"), Some("0"));
        assert_eq!(warn.field("overlay_revision"), Some("0"));
    }

    #[test]
    fn carry_over_learned_from_clears_and_warns_on_overlay_revision_change() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // Arrange: the outgoing Router was built against overlay revision 3.
        let config = Arc::new(Config::default());
        let mut before = Router::new(config.clone());
        before.note_overlay_revision(3);
        before.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        // The rebuild picked up a newer overlay revision.
        let mut after = Router::new(config);
        after.note_overlay_revision(4);

        // Act
        let events = routectl_testkit::capture_events(|| {
            after.carry_over_learned_from(&before);
        });

        // Assert: overlay change invalidates -- empty registry, one WARN
        // naming the overlay trigger, invalidation counter bumped.
        assert!(after.learned_capabilities.is_empty());
        assert_eq!(after.metrics.invalidations_total(), 1);
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("overlay revision change must emit a WARN");
        assert_eq!(warn.field("event"), Some("invalidation"));
        assert_eq!(warn.field("overlay_changed"), Some("true"));
        assert_eq!(warn.field("catalog_changed"), Some("false"));
        let prev_cat = before.catalog_version.to_string();
        let cur_cat = after.catalog_version.to_string();
        assert_eq!(
            warn.field("previous_catalog_version"),
            Some(prev_cat.as_str())
        );
        assert_eq!(warn.field("catalog_version"), Some(cur_cat.as_str()));
        assert_eq!(warn.field("previous_overlay_revision"), Some("3"));
        assert_eq!(warn.field("overlay_revision"), Some("4"));
    }

    #[test]
    fn record_k_sample_skips_keyless_and_records_keyed() {
        use crate::k_estimator::KSessionKey;
        use std::time::UNIX_EPOCH;

        // Arrange
        let config = Arc::new(Config::default());
        let router = Router::new(config);

        // Act: a keyless request must NOT create any window.
        router.record_k_sample(None, "anthropic-api", "opus", 5, UNIX_EPOCH);

        // Assert: the store stays empty -- keyless requests are untracked.
        assert!(
            router.k_session_store.is_empty(),
            "a keyless request must not be recorded",
        );

        // Act: a keyed request with a cache hit records one reuse sample.
        router.record_k_sample(Some("sess-1"), "anthropic-api", "opus", 7, UNIX_EPOCH);
        // A keyed request with no cache hit records a no-reuse sample.
        router.record_k_sample(Some("sess-1"), "anthropic-api", "opus", 0, UNIX_EPOCH);

        // Assert: both samples landed under the one triple, with
        // observed_reuse tracking cache_read > 0.
        let window = router
            .k_session_store
            .get(&KSessionKey {
                session_key: "sess-1".into(),
                provider_kind: "anthropic-api".into(),
                model: "opus".into(),
            })
            .expect("triple recorded");
        let reuse: Vec<bool> = window.iter().map(|s| s.observed_reuse).collect();
        assert_eq!(reuse, vec![true, false]);
    }

    /// A fresh router's `k_estimator` reads the store entries imported by
    /// `carry_over_k_store_from`: the estimator field needs no carry-over of
    /// its own because the constructor points it at the same store the
    /// carry-over populates. Builds a `Calibrated`-sized window in the source,
    /// carries it over, and proves the new router's estimator returns a
    /// non-cold estimate for the carried triple.
    #[test]
    fn carried_store_is_read_by_new_routers_estimator() {
        use crate::k_estimator::{Confidence, KQuery, KSessionKey, KSessionWindow, Sample};
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        // Arrange: enough TTL-separated runs in the source store that the
        // estimator classifies the triple as `Calibrated` (>= 8 runs). Each
        // run is one reuse hit separated from the next by more than the TTL.
        let ttl = Duration::from_mins(5);
        let mut window = KSessionWindow::new();
        for i in 0..12u64 {
            window.push(Sample {
                ts: UNIX_EPOCH + Duration::from_secs(i * 10_000),
                observed_reuse: true,
            });
        }
        let key = KSessionKey {
            session_key: "carried-sess".into(),
            provider_kind: "anthropic-api".into(),
            model: "opus".into(),
        };

        let config = Arc::new(Config::default());
        let before = Router::new(config.clone());
        before.k_session_store.put(key, window);

        // Act: a freshly-built router imports the source's entries, then its
        // OWN estimator (pointed at its own store at construction) is queried.
        let mut after = Router::new(config);
        after.carry_over_k_store_from(&before);
        let estimate = after.k_estimator.estimate(&KQuery {
            session_key: Some("carried-sess"),
            provider_kind: "anthropic-api",
            model: "opus",
            ttl,
            now: SystemTime::now(),
        });

        // Assert: the estimator saw the carried samples (not a cold default).
        assert_eq!(
            estimate.confidence,
            Confidence::Calibrated,
            "new router's estimator must read the carried-over store",
        );
        assert!(estimate.samples >= 12);
    }

    /// The `would_trim_k_floor_for_meta` truth table, one assertion per row.
    /// The verdict (met / unmet / cold / unpriced) is derived downstream from
    /// the numeric advisory columns; here we only pin the recorded Option.
    #[test]
    fn would_trim_k_floor_for_meta_truth_table() {
        use crate::k_estimator::{Confidence, EstimateSource, KEstimate};

        fn estimate(k_floor: f64, confidence: Confidence) -> KEstimate {
            KEstimate {
                k_floor,
                k_point: k_floor,
                k_ceiling: k_floor,
                samples: 16,
                confidence,
                source: EstimateSource::LiveLedger,
            }
        }

        // Row 1: Some(K*), Calibrated, k_floor >= K* -> Some(k_floor).
        assert_eq!(
            would_trim_k_floor_for_meta(Some(50.0), &estimate(60.0, Confidence::Calibrated)),
            Some(60.0),
        );

        // Row 2: Some(K*), Calibrated, k_floor < K* -> Some(k_floor)
        // (both met and unmet record the floor; the comparison is derived).
        assert_eq!(
            would_trim_k_floor_for_meta(Some(50.0), &estimate(40.0, Confidence::Calibrated)),
            Some(40.0),
        );

        // Row 3a: Some(K*), Low -> None.
        assert_eq!(
            would_trim_k_floor_for_meta(Some(50.0), &estimate(99.0, Confidence::Low)),
            None,
        );

        // Row 3b: Some(K*), Cold -> None.
        assert_eq!(
            would_trim_k_floor_for_meta(Some(50.0), &estimate(0.0, Confidence::Cold)),
            None,
        );

        // Row 4: None (unverified pricing), any confidence -> None.
        for conf in [Confidence::Calibrated, Confidence::Low, Confidence::Cold] {
            assert_eq!(
                would_trim_k_floor_for_meta(None, &estimate(99.0, conf)),
                None,
                "conf={conf:?}",
            );
        }
    }
}

#[cfg(test)]
mod remap_tests {
    //! `apply_remap` / `upstream_status_for_remap`: the pure classify ->
    //! remap seam consulted at all three dispatch error arms.
    use super::*;

    fn native(class: FailureClass, matched_by: MatchedBy) -> ClassifiedFailure {
        ClassifiedFailure { class, matched_by }
    }

    #[test]
    fn apply_remap_empty_map_is_a_no_op() {
        // Arrange: the default, no-op case -- an operator who never
        // configured `class_overrides` must see native classification
        // pass through unchanged, whatever the status.
        let cf = native(FailureClass::ServerError, MatchedBy::Status);
        let overrides = BTreeMap::new();

        // Act
        let (effective, remapped) = apply_remap(cf.clone(), Some(503), &overrides);

        // Assert
        assert_eq!(effective, cf);
        assert!(!remapped);
    }

    #[test]
    fn apply_remap_no_matching_key_is_a_no_op() {
        // Arrange: overrides present, but not for this status.
        let cf = native(FailureClass::ServerError, MatchedBy::Status);
        let mut overrides = BTreeMap::new();
        overrides.insert(429, FailureClass::RateLimited);

        // Act
        let (effective, remapped) = apply_remap(cf.clone(), Some(503), &overrides);

        // Assert
        assert_eq!(effective, cf);
        assert!(!remapped);
    }

    #[test]
    fn apply_remap_none_status_is_a_no_op_even_with_a_matching_key_present() {
        // Arrange: a non-upstream / status-0 error carries no status to
        // key on, so the override table is never consulted at all.
        let cf = native(FailureClass::NetworkError, MatchedBy::Status);
        let mut overrides = BTreeMap::new();
        overrides.insert(503, FailureClass::ContentPolicy);

        // Act
        let (effective, remapped) = apply_remap(cf.clone(), None, &overrides);

        // Assert
        assert_eq!(effective, cf);
        assert!(!remapped);
    }

    #[test]
    fn apply_remap_matching_key_replaces_class_but_keeps_native_matched_by() {
        // Arrange: native classify(503) is ServerError matched_by Status;
        // the operator remaps 503 to ContentPolicy.
        let cf = native(FailureClass::ServerError, MatchedBy::Status);
        let mut overrides = BTreeMap::new();
        overrides.insert(503, FailureClass::ContentPolicy);

        // Act
        let (effective, remapped) = apply_remap(cf, Some(503), &overrides);

        // Assert: the class changed, but matched_by still describes HOW
        // the classifier reached its native decision, not the remap.
        assert_eq!(effective.class, FailureClass::ContentPolicy);
        assert_eq!(effective.matched_by, MatchedBy::Status);
        assert!(remapped);
    }

    #[test]
    fn apply_remap_preserves_upstream_type_matched_by_on_a_lifted_native_class() {
        // Arrange: a native lift (matched_by = UpstreamType) still keeps
        // that provenance after a remap replaces the class.
        let cf = native(
            FailureClass::FeatureUnsupported {
                capability: "unsupported_parameter".to_string(),
            },
            MatchedBy::UpstreamType,
        );
        let mut overrides = BTreeMap::new();
        overrides.insert(400, FailureClass::BadRequest);

        // Act
        let (effective, remapped) = apply_remap(cf, Some(400), &overrides);

        // Assert
        assert_eq!(effective.class, FailureClass::BadRequest);
        assert_eq!(effective.matched_by, MatchedBy::UpstreamType);
        assert!(remapped);
    }

    #[test]
    fn upstream_status_for_remap_extracts_in_range_upstream_status() {
        let err = Error::upstream("p", 503, "body");
        assert_eq!(upstream_status_for_remap(&err), Some(503));
    }

    #[test]
    fn upstream_status_for_remap_none_for_status_zero() {
        let err = Error::upstream("p", 0, "body");
        assert_eq!(upstream_status_for_remap(&err), None);
    }

    #[test]
    fn upstream_status_for_remap_none_for_out_of_range_status() {
        let err = Error::upstream("p", 600, "body");
        assert_eq!(upstream_status_for_remap(&err), None);
    }

    #[test]
    fn upstream_status_for_remap_none_for_non_upstream_variant() {
        let err = Error::Streaming("boom".into());
        assert_eq!(upstream_status_for_remap(&err), None);
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

    fn class_of(err: &Error) -> FailureClass {
        classify(err, None).class
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
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        // Act
        let fall_back = should_fallback(&err, &class, &policy, true);
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
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        // Act
        let retry = should_retry_same_provider(&err, &class, &policy, 0, true);
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
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        assert!(!should_fallback(&err, &class, &policy, true));
    }

    #[test]
    fn probe_529_does_not_retry_same_provider() {
        // Symmetry with the 429 retry short-circuit, for the 529 branch.
        let err = upstream(529);
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        assert!(!should_retry_same_provider(&err, &class, &policy, 0, true));
    }

    #[test]
    fn probe_400_still_falls_back() {
        // Bedrock rejects max_tokens=1 with a 400; a sibling provider
        // may accept it, so a probe must still walk the chain on 4xx.
        let err = upstream(400);
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        assert!(should_fallback(&err, &class, &policy, true));
    }

    #[test]
    fn probe_503_still_falls_back() {
        // 503 is generic unavailability (not the chain-wide 429/529); a
        // sibling provider may be healthy, so the probe still falls back.
        let err = upstream(503);
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        assert!(should_fallback(&err, &class, &policy, true));
    }

    #[test]
    fn real_request_429_still_retries_and_falls_back() {
        // is_probe=false (a real request): a 429 keeps today's behavior
        // -- fallbackable AND retryable up to the policy cap.
        let err = upstream(429);
        let class = class_of(&err);
        let policy = RetryPolicy::default();
        assert!(
            should_fallback(&err, &class, &policy, false),
            "real-request 429 still falls back",
        );
        assert!(
            should_retry_same_provider(&err, &class, &policy, 0, false),
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
        let class = class_of(&err);
        assert!(should_fallback(&err, &class, &policy, is_probe));
        assert!(should_retry_same_provider(
            &err, &class, &policy, 0, is_probe
        ));
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
        let err = upstream(429);
        let class = class_of(&err);
        assert!(
            !should_fallback(&err, &class, &policy, is_probe),
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
        fn id(&self) -> &'static str {
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

    /// Regression guard for the same per-attempt overlay rebuild hazard:
    /// the ingress-captured inbound per-conversation session key must
    /// survive the rebuild rather than reset to `None` on a later attempt.
    #[test]
    fn apply_layered_overlays_preserves_inbound_session_key() {
        let config = Config::default();
        let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            Arc::new(StubProvider),
            "claude-x",
        ));
        let target = into_one_dispatch_target(model);

        let mut req = req_with_betas(vec![]);
        req.routectl_internal.inbound_session_key = Some("sid-abc".into());
        apply_layered_overlays(&config, &target, &mut req);

        assert_eq!(
            req.routectl_internal.inbound_session_key.as_deref(),
            Some("sid-abc"),
            "inbound session key must survive the per-attempt overlay rebuild",
        );
    }

    /// Regression guard for the same per-attempt overlay rebuild hazard:
    /// the ingress-forwarded bearer token must survive the rebuild rather
    /// than reset to `None`, on the first attempt AND every subsequent
    /// chain attempt (the rebuild runs once per dispatch attempt).
    #[test]
    fn apply_layered_overlays_preserves_forwarded_bearer() {
        let config = Config::default();
        let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            Arc::new(StubProvider),
            "claude-x",
        ));
        let target = into_one_dispatch_target(model);

        let mut req = req_with_betas(vec![]);
        req.routectl_internal.forwarded_bearer = Some(routectl_core::schema::ForwardedBearer::new(
            "sk-forwarded".into(),
        ));

        for attempt in 1..=2 {
            apply_layered_overlays(&config, &target, &mut req);
            assert_eq!(
                req.routectl_internal
                    .forwarded_bearer
                    .as_ref()
                    .map(routectl_core::schema::ForwardedBearer::expose),
                Some("sk-forwarded"),
                "forwarded bearer must survive the per-attempt overlay rebuild (attempt {attempt})",
            );
        }
    }

    /// Regression guard for the same per-attempt overlay rebuild hazard:
    /// the ingress-captured forwarded `x-stainless-*` headers must survive
    /// the rebuild rather than reset to empty, so the egress can present
    /// the client's real fingerprint on every chain attempt, not just the
    /// first.
    #[test]
    fn apply_layered_overlays_preserves_stainless_headers() {
        let config = Config::default();
        let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            Arc::new(StubProvider),
            "claude-x",
        ));
        let target = into_one_dispatch_target(model);

        let mut req = req_with_betas(vec![]);
        req.routectl_internal.stainless_headers = vec![
            ("x-stainless-lang".to_string(), "js".to_string()),
            (
                "x-stainless-package-version".to_string(),
                "0.94.0-client".to_string(),
            ),
        ];

        for attempt in 1..=2 {
            apply_layered_overlays(&config, &target, &mut req);
            assert_eq!(
                req.routectl_internal.stainless_headers,
                vec![
                    ("x-stainless-lang".to_string(), "js".to_string()),
                    (
                        "x-stainless-package-version".to_string(),
                        "0.94.0-client".to_string()
                    ),
                ],
                "stainless headers must survive the per-attempt overlay rebuild (attempt {attempt})",
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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
        fn id(&self) -> &'static str {
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
        let via_seat = dispatch_target_for_seat(&m, &seat, None);
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
        let via_seat = dispatch_target_for_seat(&m, &seat, None);
        assert!(!via_seat.visible_routectl_provider);
    }

    #[test]
    fn seat_dispatch_target_carries_provider_kind() {
        // A seat-backed target must classify errors against the seat
        // provider's OWN kind, not the union table. `provider_kind` is
        // config-derived (a seat shares its model's provider entry), so
        // the chain expander resolves it from `provider_name` and threads
        // it onto every seat target -- not left `None`.
        let mut config = Config::default();
        config.providers.insert(
            "test-prov".into(),
            crate::config::ProviderEntry::anthropic_api("literal:k"),
        );
        let router = Router::new(Arc::new(config));

        let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "test-prov".into(),
            calls: AtomicUsize::new(0),
        });
        let seats: Vec<crate::seat_pool::SeatTarget> = ["seat-a", "seat-b"]
            .iter()
            .map(|label| crate::seat_pool::SeatTarget {
                label: Some((*label).to_string()),
                state_key: crate::seat_pool::seat_state_key("nick", Some(label)),
                provider: provider.clone(),
                auth_secret_ref: None,
            })
            .collect();
        let model = Arc::new(
            ResolvedModel::new("nick", "test-prov", provider, "claude-x").with_seats(seats.into()),
        );

        let targets = router.expand_chain_to_targets(vec![model], None);
        assert_eq!(targets.len(), 2, "one dispatch target per seat");
        for target in &targets {
            assert_eq!(
                target.provider_kind,
                Some("anthropic-api"),
                "seat target must carry the configured provider kind",
            );
        }
    }

    #[test]
    fn expand_chain_to_targets_fills_class_overrides_from_provider_config() {
        // The provider's `[class_overrides]` table is adapted to canonical
        // `FailureClass` ONCE at chain expansion, mirroring the
        // `provider_kind` only-when-empty fill discipline. Uses the real
        // `ConfigFailureClass` adapter (`to_failure_class`), not a
        // hand-built `FailureClass`.
        use crate::class_policy::ConfigFailureClass;
        let mut entry = crate::config::ProviderEntry::anthropic_api("literal:k");
        if let crate::config::ProviderEntry::AnthropicApi { runtime, .. } = &mut entry {
            runtime
                .class_overrides
                .insert(503, ConfigFailureClass::ContentPolicy);
            runtime
                .class_overrides
                .insert(529, ConfigFailureClass::FeatureUnsupported);
        }
        let mut config = Config::default();
        config.providers.insert("test-prov".into(), entry);
        let router = Router::new(Arc::new(config));

        let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "test-prov".into(),
            calls: AtomicUsize::new(0),
        });
        let model = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            provider,
            "claude-x",
        ));

        let targets = router.expand_chain_to_targets(vec![model], None);
        assert_eq!(targets.len(), 1);
        let target = &targets[0];
        assert_eq!(
            target.class_overrides.get(&503),
            Some(&FailureClass::ContentPolicy),
        );
        assert_eq!(
            target.class_overrides.get(&529),
            Some(&FailureClass::FeatureUnsupported {
                capability: crate::class_policy::OPERATOR_REMAP_CAPABILITY.to_string(),
            }),
        );
        assert!(!target.class_overrides.contains_key(&500));
    }

    #[test]
    fn expand_chain_to_targets_leaves_class_overrides_empty_with_no_provider_config() {
        // No `[class_overrides]` on the provider entry (the default) must
        // leave every target's map empty -- the no-op case for `apply_remap`.
        let mut config = Config::default();
        config.providers.insert(
            "test-prov".into(),
            crate::config::ProviderEntry::anthropic_api("literal:k"),
        );
        let router = Router::new(Arc::new(config));

        let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "test-prov".into(),
            calls: AtomicUsize::new(0),
        });
        let model = Arc::new(ResolvedModel::new(
            "nick",
            "test-prov",
            provider,
            "claude-x",
        ));

        let targets = router.expand_chain_to_targets(vec![model], None);
        assert!(targets[0].class_overrides.is_empty());
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
            let model = req.model;
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
    async fn status_targets_one_entry_per_nickname_for_non_pooled() {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p-test".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_resolved(vec![
            ("alpha", "p-shared", "u1", p.clone()),
            ("beta", "p-shared", "u2", p.clone()),
        ]);
        let targets = router.status_targets(Instant::now());
        assert_eq!(targets.len(), 2, "one entry per non-pooled nickname");
        let alpha = targets
            .iter()
            .find(|t| t.nickname == "alpha")
            .expect("alpha present");
        assert_eq!(alpha.state_key, "alpha");
        assert_eq!(alpha.provider_name, "p-shared");
        assert_eq!(alpha.upstream, "u1");
        assert_eq!(alpha.seat_label, None);
        // A fresh, unconfigured gate reads Closed with no probe in flight.
        assert_eq!(alpha.gate.circuit, CircuitPhase::Closed);
        assert!(!alpha.gate.half_open_probe_in_flight);
    }

    #[tokio::test]
    async fn status_targets_one_entry_per_seat_for_pooled() {
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
        let targets = router.status_targets(Instant::now());
        assert_eq!(targets.len(), 2, "one entry per seat of a pooled model");
        let mut keys: Vec<&str> = targets.iter().map(|t| t.state_key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["opus#seat-a", "opus#seat-b"]);
        for t in &targets {
            assert_eq!(t.nickname, "opus");
            assert_eq!(t.provider_name, "anthropic-oauth");
            assert_eq!(t.upstream, "claude-opus-4-7-wire");
            assert!(t.seat_label.is_some(), "seat entries carry a label");
        }
    }

    #[tokio::test]
    async fn status_targets_missing_state_slot_fails_safe_open() {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p-test".into(),
            calls: AtomicUsize::new(0),
        });
        let mut router = router_with_resolved(vec![("gamma", "p-shared", "u1", p.clone())]);
        // Drop the state slot: the resolved-model entry survives but has no
        // runtime gate. status_targets must not panic and must fail safe.
        router.state.remove("gamma");
        let targets = router.status_targets(Instant::now());
        assert_eq!(targets.len(), 1);
        let gamma = &targets[0];
        assert_eq!(gamma.state_key, "gamma");
        assert_eq!(
            gamma.gate.circuit,
            CircuitPhase::Open,
            "a target with no state slot fails safe to Open",
        );
        assert!(!gamma.gate.half_open_probe_in_flight);
        assert_eq!(gamma.gate.rpm_available, None);
    }

    #[tokio::test]
    async fn learned_capability_snapshot_surfaces_negatives() {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "p-test".into(),
            calls: AtomicUsize::new(0),
        });
        let router = router_with_resolved(vec![("alpha", "openai-compat", "u1", p.clone())]);
        assert!(
            router.learned_capability_snapshot().is_empty(),
            "fresh registry surfaces no negatives",
        );
        router.learned_capabilities.observe(
            "alpha",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let snap = router.learned_capability_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].state_key, "alpha");
        assert_eq!(snap[0].feature_key, "web_search");
        assert_eq!(snap[0].signal_tier, SignalTier::SelfIdentifying);
    }

    #[tokio::test]
    async fn status_targets_does_not_claim_half_open_probe_slot() {
        // THE non-perturbation guard, at the router seam. Drive one seat to
        // HalfOpenReady, hammer status_targets (serially AND concurrently),
        // and assert the read never claims the probe slot: every entry stays
        // HalfOpenReady with half_open_probe_in_flight == false. Only THEN
        // does a real try_dispatch claim the probe.
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "oauth-pool".into(),
            calls: AtomicUsize::new(0),
        });
        let router = Arc::new(router_with_pooled_model(
            "opus",
            "anthropic-oauth",
            "claude-opus-4-7-wire",
            p.clone(),
            &["seat-a", "seat-b"],
            None,
        ));

        // Park seat-a's breaker; compute an instant past its cooldown so the
        // gate reads HalfOpenReady without any probe in flight.
        let t0 = Instant::now();
        assert!(
            router.force_open_breaker("opus#seat-a", Duration::from_millis(500)),
            "seat-a must own a state slot",
        );
        let t_ready = t0 + Duration::from_millis(600);

        let seat_a_ready = |targets: &[RouteTargetStatus]| {
            let seat = targets
                .iter()
                .find(|t| t.state_key == "opus#seat-a")
                .expect("seat-a present");
            assert_eq!(seat.gate.circuit, CircuitPhase::HalfOpenReady);
            assert!(!seat.gate.half_open_probe_in_flight);
        };

        // Serial hammering.
        for _ in 0..100 {
            seat_a_ready(&router.status_targets(t_ready));
        }

        // Concurrent hammering.
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let r = Arc::clone(&router);
                scope.spawn(move || {
                    for _ in 0..100 {
                        let targets = r.status_targets(t_ready);
                        let seat = targets
                            .iter()
                            .find(|t| t.state_key == "opus#seat-a")
                            .expect("seat-a present");
                        assert_eq!(seat.gate.circuit, CircuitPhase::HalfOpenReady);
                        assert!(!seat.gate.half_open_probe_in_flight);
                    }
                });
            }
        });

        // The reads never perturbed the slot: a real dispatch still gets the
        // probe and claims it.
        let slot = router.state.get("opus#seat-a").expect("slot present");
        assert_eq!(slot.lock().try_dispatch(t_ready), GateDecision::Allow);
        assert!(
            slot.lock().half_open_probe_in_flight(),
            "the real dispatch claimed the probe slot the reads left untouched",
        );
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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
        let res = router.dispatch_chain("does-not-exist", None);
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
        let chain = router.dispatch_chain("a", None).expect("dispatch_chain ok");
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

        let chain_a = router.dispatch_chain("a", None).expect("a resolves");
        assert_eq!(chain_a.len(), 1);
        assert_eq!(chain_a[0].upstream, "u-x");

        let chain_claude = router
            .dispatch_chain("claude-a", None)
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
            .dispatch_chain("claude-haiku-3", None)
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
        let res = router.dispatch_chain("a", None);
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

    // ---- Learned-capability act path (soft-drop + probe admission) ----

    fn learned_provider_config() -> Arc<Config> {
        let mut config = Config::default();
        config.providers.insert(
            "test-prov".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        Arc::new(config)
    }

    fn learned_target(router: &Router, nickname: &str, unsupported: &[&str]) -> DispatchTarget {
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "test-prov".into(),
            calls: AtomicUsize::new(0),
        });
        let model = ResolvedModel::new(nickname, "test-prov", p, "claude-x")
            .with_unsupported_features(unsupported.iter().map(|s| (*s).to_string()).collect());
        router
            .expand_chain_to_targets(vec![Arc::new(model)], None)
            .pop()
            .expect("one target for a non-seat model")
    }

    #[test]
    fn filter_source_learned_stringifies_to_contract_token() {
        assert_eq!(FilterSource::Learned.as_str(), "learned");
    }

    #[test]
    fn learned_negative_deprioritizes_target_to_tail() {
        let router = Router::new(learned_provider_config());
        let front = learned_target(&router, "front", &[]);
        let back = learned_target(&router, "back", &[]);
        router.learned_capabilities.observe(
            "front",
            "web_search",
            "anthropic-api",
            routectl_core::capability::SignalTier::SelfIdentifying,
            std::time::Instant::now(),
        );
        let features = vec!["web_search".to_string()];

        let mut out = Vec::new();
        let events = routectl_testkit::capture_events(|| {
            out = router
                .filter_chain_by_features(vec![front, back], &features, "alias", &mut Vec::new())
                .expect("a supported survivor keeps the chain non-empty");
        });

        // Result = [supported...] ++ [learned tail]: back survives up front,
        // the learned-negative "front" is de-prioritized to the tail.
        let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
        assert_eq!(order, vec!["back", "front"]);
        assert_eq!(
            router.metrics.d17_tail_total(),
            0,
            "a supported survivor is not a tail-only entry",
        );
        // A healthy alternative remains, so the demotion emits route_away at
        // INFO (not WARN) carrying the unified state_key / capability_key.
        let info = events
            .iter()
            .find(|e| e.field("event") == Some("route_away"))
            .expect("a tail demotion must emit a route_away event");
        assert_eq!(info.level, tracing::Level::INFO);
        assert_eq!(info.field("state_key"), Some("front"));
        assert_eq!(info.field("capability_key"), Some("web_search"));
        assert!(
            !events
                .iter()
                .any(|e| e.field("event") == Some("route_away") && e.level == tracing::Level::WARN),
            "a surviving alternative must not raise route_away to WARN",
        );
    }

    #[test]
    fn static_unsupported_emptying_chain_returns_not_implemented() {
        // The model-static list lives in config.models: the override
        // registry is built from config (mirroring build_resolved_models,
        // which copies the same list onto each ResolvedModel).
        let mut config = Config::default();
        config.providers.insert(
            "test-prov".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        config.models.insert(
            "only".into(),
            crate::config::ModelEntry::new("test-prov", "claude-x")
                .with_unsupported_features(vec!["web_search".to_string()]),
        );
        let router = Router::new(Arc::new(config));
        let only = learned_target(&router, "only", &["web_search"]);
        let features = vec!["web_search".to_string()];

        let result =
            router.filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new());

        assert!(
            matches!(result, Err(Error::NotImplemented(..))),
            "a static hard-drop of the whole chain must fail",
        );
    }

    #[test]
    fn sole_learned_tail_target_still_attempts_and_counts_d17() {
        let router = Router::new(learned_provider_config());
        let only = learned_target(&router, "only", &[]);
        router.learned_capabilities.observe(
            "only",
            "web_search",
            "anthropic-api",
            routectl_core::capability::SignalTier::SelfIdentifying,
            std::time::Instant::now(),
        );
        let features = vec!["web_search".to_string()];

        let events = routectl_testkit::capture_events(|| {
            let out = router
                .filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new())
                .expect("a learned-only chain proceeds into the de-prioritized tail");
            assert_eq!(out.len(), 1, "the sole tail target is still attempted");
            assert_eq!(out[0].state_key, "only");
        });

        assert_eq!(router.metrics.d17_tail_total(), 1);
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("entering the learned tail must WARN");
        assert_eq!(warn.field("event"), Some("route_away"));
        assert_eq!(warn.field("state_key"), Some("only"));
        assert_eq!(warn.field("capability_key"), Some("web_search"));
    }

    #[test]
    fn kill_switch_off_skips_the_learned_consult() {
        let mut config = Config::default();
        config.capability.enabled = false;
        config.providers.insert(
            "test-prov".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        let router = Router::new(Arc::new(config));
        let front = learned_target(&router, "front", &[]);
        let back = learned_target(&router, "back", &[]);
        router.learned_capabilities.observe(
            "front",
            "web_search",
            "anthropic-api",
            routectl_core::capability::SignalTier::SelfIdentifying,
            std::time::Instant::now(),
        );
        let features = vec!["web_search".to_string()];

        let out = router
            .filter_chain_by_features(vec![front, back], &features, "alias", &mut Vec::new())
            .expect("kill switch off leaves the chain intact");

        // The learned negative is ignored: original order, nothing tailed.
        let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
        assert_eq!(order, vec!["front", "back"]);
        assert_eq!(router.metrics.d17_tail_total(), 0);
    }

    #[test]
    fn expired_learned_negative_admits_one_probe_through_filter() {
        // A zero decay window makes a negative expired the instant it is
        // observed, so the next filter pass claims the single re-probe slot.
        let mut config = Config::default();
        config.capability.decay_hours = 0;
        config.providers.insert(
            "test-prov".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        let router = Router::new(Arc::new(config));
        let only = learned_target(&router, "only", &[]);
        router.learned_capabilities.observe(
            "only",
            "web_search",
            "anthropic-api",
            routectl_core::capability::SignalTier::SelfIdentifying,
            std::time::Instant::now(),
        );
        let features = vec!["web_search".to_string()];

        // First pass: the lapsed negative admits a probe -> the target stays
        // in the supported set (routed to), and the probe is counted.
        let out = router
            .filter_chain_by_features(vec![only.clone()], &features, "alias", &mut Vec::new())
            .expect("an admitted probe routes to the target");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state_key, "only");
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert_eq!(router.metrics.d17_tail_total(), 0);

        // Second pass: the probe slot is claimed (in_flight) -> concurrent
        // lookups keep routing away, landing the target in the tail.
        let out2 = router
            .filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new())
            .expect("a claimed in-flight probe routes away into the tail");
        assert_eq!(out2.len(), 1);
        assert_eq!(
            router.metrics.probe_attempts_total(),
            1,
            "exactly one probe is admitted per decay lapse",
        );
        assert_eq!(router.metrics.d17_tail_total(), 1);
    }

    #[test]
    fn dispatch_target_carries_catalog_capability_prior() {
        use crate::catalog::{CatalogRow, EffectiveRow, Source};

        let router = Router::new(learned_provider_config());
        let mut row = CatalogRow::sentinel();
        row.capabilities.insert("web_search".to_string(), false);
        let p: Arc<dyn Provider> = Arc::new(CountedProvider {
            id: "test-prov".into(),
            calls: AtomicUsize::new(0),
        });
        let model = ResolvedModel::new("only", "test-prov", p, "claude-x").with_effective_row(
            EffectiveRow::Present {
                row,
                source: Source::Baked,
                verified_at: "2026-01-01".to_string(),
            },
        );

        let target = router
            .expand_chain_to_targets(vec![Arc::new(model)], None)
            .pop()
            .expect("one target");

        // Present key returns the prior; an absent key is NO PRIOR (None),
        // distinct from Some(false). No filter consumes it in this increment.
        assert_eq!(target.capability_prior("web_search"), Some(false));
        assert_eq!(target.capability_prior("computer_use"), None);
    }
}

#[cfg(test)]
mod has_forwarded_provider_tests {
    //! `Router::has_forwarded_provider()`: build-time cached, `true` iff
    //! `config.providers` contains a `ProviderEntry::AnthropicApi` with
    //! `credential_source == Forwarded`. Replaces the removed `[mitm]
    //! credential_source` read as the CAPTURE gate's "configured
    //! capability" half (see `routectl_cli::handlers::ingress_handle`).
    use super::*;
    use crate::config::{CredentialSource, ProviderEntry};
    use std::collections::BTreeMap;

    fn router_with_providers(providers: BTreeMap<String, ProviderEntry>) -> Router {
        Router::new(Arc::new(Config {
            providers,
            ..Default::default()
        }))
    }

    #[test]
    fn false_when_no_providers_configured() {
        let router = router_with_providers(BTreeMap::new());
        assert!(!router.has_forwarded_provider());
    }

    #[test]
    fn false_when_only_own_credential_providers_configured() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "own-anthropic".to_string(),
            ProviderEntry::anthropic_api("literal:k").with_credential_source(CredentialSource::Own),
        );
        providers.insert(
            "own-compat".to_string(),
            ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
        );
        let router = router_with_providers(providers);
        assert!(!router.has_forwarded_provider());
    }

    #[test]
    fn true_when_a_forwarded_anthropic_provider_is_configured() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "forwarded".to_string(),
            ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
        );
        let router = router_with_providers(providers);
        assert!(router.has_forwarded_provider());
    }

    #[test]
    fn true_when_a_forwarded_provider_coexists_with_own_credential_providers() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "own-compat".to_string(),
            ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
        );
        providers.insert(
            "forwarded".to_string(),
            ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
        );
        let router = router_with_providers(providers);
        assert!(
            router.has_forwarded_provider(),
            "coexistence must not hide the forwarded provider"
        );
    }
}

#[cfg(test)]
mod forwarded_model_transparency_tests {
    //! `DispatchTarget::use_forwarded_credential`: populated once at chain
    //! expansion from the provider entry's `credential_source`, then read
    //! at all three dispatch paths (complete, count_tokens, stream) to
    //! bypass the `attempt_req.model` rewrite. A forwarded target forwards
    //! the client's requested model verbatim; an own target still rewrites
    //! to the target's configured `upstream`.
    use super::*;
    use crate::config::{AliasValue, CredentialSource, ProviderEntry};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use parking_lot::Mutex;
    use routectl_core::schema::ForwardedBearer;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider, TokenCount,
    };
    use std::collections::BTreeMap;

    /// Records the `model` field of every request it is dispatched with,
    /// and echoes it straight back on `complete` -- lets a test observe
    /// exactly what the router sent upstream without inspecting private
    /// dispatch state.
    struct ModelSpyProvider {
        id: String,
        seen: Mutex<Vec<String>>,
    }

    impl ModelSpyProvider {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen_models(&self) -> Vec<String> {
            self.seen.lock().clone()
        }
    }

    #[async_trait]
    impl Provider for ModelSpyProvider {
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
            self.seen.lock().push(req.model.clone());
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
            self.seen.lock().push(req.model.clone());
            Ok(Box::pin(futures::stream::once(async {
                Ok(ChatChunk::default())
            })))
        }
        async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
            self.seen.lock().push(req.model.clone());
            Ok(TokenCount::default())
        }
    }

    /// Register one nickname/provider/upstream leg on a fresh router,
    /// with the provider entry's `credential_source` set per `forwarded`.
    fn router_with_leg(
        nickname: &str,
        provider_name: &str,
        upstream: &str,
        forwarded: bool,
    ) -> (Router, Arc<ModelSpyProvider>) {
        let spy = Arc::new(ModelSpyProvider::new(provider_name));
        let mut entry = ProviderEntry::anthropic_api("literal:k");
        if forwarded {
            entry = entry.with_credential_source(CredentialSource::Forwarded);
        }
        let mut config = Config::default();
        config.providers.insert(provider_name.to_string(), entry);
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            nickname.to_string(),
            Arc::new(ResolvedModel::new(
                nickname,
                provider_name,
                spy.clone() as Arc<dyn Provider>,
                upstream,
            )),
        );
        router.install_resolved_models(models);
        (router, spy)
    }

    fn req_for(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![],
            ..Default::default()
        }
    }

    /// Like `req_for`, but stamps a captured forwarded bearer so a
    /// forwarded-credential target's missing-bearer terminal guard (see
    /// `missing_forwarded_bearer_error`) does not refuse the request
    /// before the model-transparency assertion runs.
    fn forwarded_req_for(model: &str) -> ChatRequest {
        let mut req = req_for(model);
        req.routectl_internal.forwarded_bearer =
            Some(ForwardedBearer::new("sk-ant-oat01-test".to_string()));
        req
    }

    #[test]
    fn expand_chain_to_targets_sets_flag_true_for_forwarded_anthropic_provider() {
        let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let model = Arc::new(ResolvedModel::new(
            "opus",
            "fwd-prov",
            spy as Arc<dyn Provider>,
            "claude-opus-upstream",
        ));
        let targets = router.expand_chain_to_targets(vec![model], None);
        assert_eq!(targets.len(), 1);
        assert!(
            targets[0].use_forwarded_credential,
            "a Forwarded AnthropicApi provider entry must set the target flag true",
        );
    }

    #[test]
    fn expand_chain_to_targets_sets_flag_false_for_own_anthropic_provider() {
        let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        let model = Arc::new(ResolvedModel::new(
            "opus",
            "own-prov",
            spy as Arc<dyn Provider>,
            "claude-opus-upstream",
        ));
        let targets = router.expand_chain_to_targets(vec![model], None);
        assert_eq!(targets.len(), 1);
        assert!(
            !targets[0].use_forwarded_credential,
            "the default Own credential source must leave the target flag false",
        );
    }

    #[test]
    fn expand_chain_to_targets_sets_flag_for_every_seat_of_a_forwarded_pool() {
        let spy = Arc::new(ModelSpyProvider::new("fwd-prov"));
        let mut config = Config::default();
        config.providers.insert(
            "fwd-prov".to_string(),
            ProviderEntry::anthropic_api("literal:k")
                .with_credential_source(CredentialSource::Forwarded),
        );
        let router = Router::new(Arc::new(config));
        let seats: Vec<crate::seat_pool::SeatTarget> = ["seat-a", "seat-b"]
            .iter()
            .map(|label| crate::seat_pool::SeatTarget {
                label: Some((*label).to_string()),
                state_key: crate::seat_pool::seat_state_key("nick", Some(label)),
                provider: spy.clone() as Arc<dyn Provider>,
                auth_secret_ref: None,
            })
            .collect();
        let model = Arc::new(
            ResolvedModel::new("nick", "fwd-prov", spy as Arc<dyn Provider>, "claude-x")
                .with_seats(seats.into()),
        );
        let targets = router.expand_chain_to_targets(vec![model], None);
        assert_eq!(targets.len(), 2);
        for target in &targets {
            assert!(
                target.use_forwarded_credential,
                "every seat of a Forwarded provider's pool must carry the flag true",
            );
        }
    }

    #[tokio::test]
    async fn complete_forwards_opus_verbatim_on_a_forwarded_target() {
        let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        router
            .complete(forwarded_req_for("opus"))
            .await
            .expect("forwarded target must dispatch");
        assert_eq!(
            spy.seen_models(),
            vec!["opus".to_string()],
            "the client's requested model must reach egress verbatim",
        );
    }

    #[tokio::test]
    async fn complete_forwards_haiku_verbatim_on_a_forwarded_target() {
        let (router, spy) = router_with_leg("haiku", "fwd-prov", "claude-haiku-upstream", true);
        router
            .complete(forwarded_req_for("haiku"))
            .await
            .expect("forwarded target must dispatch");
        assert_eq!(spy.seen_models(), vec!["haiku".to_string()]);
    }

    #[tokio::test]
    async fn complete_forwards_an_unknown_model_verbatim_via_default_alias() {
        // The requested model matches no alias/glob/nickname and only
        // resolves at all through the `default` catch-all -- exercising
        // "no local model gatekeeping": routing picks the target, but the
        // wire model text is untouched by that routing decision.
        let (mut router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let mut config = (*router.config).clone();
        config.aliases.insert(
            "default".to_string(),
            AliasValue::Single("opus".to_string()),
        );
        router.config = Arc::new(config);

        router
            .complete(forwarded_req_for("some-unlisted-vendor-model"))
            .await
            .expect("default alias must resolve and dispatch");
        assert_eq!(
            spy.seen_models(),
            vec!["some-unlisted-vendor-model".to_string()],
        );
    }

    #[tokio::test]
    async fn complete_rewrites_model_to_upstream_on_an_own_target() {
        let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        router
            .complete(req_for("opus"))
            .await
            .expect("own target must dispatch");
        assert_eq!(
            spy.seen_models(),
            vec!["claude-opus-upstream".to_string()],
            "an own target must still rewrite to the configured upstream",
        );
    }

    #[tokio::test]
    async fn count_tokens_forwards_model_verbatim_on_a_forwarded_target() {
        let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        router
            .count_tokens(forwarded_req_for("opus"))
            .await
            .expect("forwarded count_tokens target must dispatch");
        assert_eq!(spy.seen_models(), vec!["opus".to_string()]);
    }

    #[tokio::test]
    async fn count_tokens_rewrites_model_to_upstream_on_an_own_target() {
        let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        router
            .count_tokens(req_for("opus"))
            .await
            .expect("own count_tokens target must dispatch");
        assert_eq!(spy.seen_models(), vec!["claude-opus-upstream".to_string()]);
    }

    #[tokio::test]
    async fn stream_forwards_model_verbatim_on_a_forwarded_target() {
        let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let _stream = router
            .stream(forwarded_req_for("opus"))
            .await
            .expect("forwarded stream target must dispatch");
        assert_eq!(spy.seen_models(), vec!["opus".to_string()]);
    }

    #[tokio::test]
    async fn stream_rewrites_model_to_upstream_on_an_own_target() {
        let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        let _stream = router
            .stream(req_for("opus"))
            .await
            .expect("own stream target must dispatch");
        assert_eq!(spy.seen_models(), vec!["claude-opus-upstream".to_string()]);
    }

    // -------- DispatchMeta::served_upstream / served_forwarded_credential --
    //
    // `mark_target` mirrors the model-transparency bypass into the
    // accounting meta so post-dispatch usage recording (which never sees
    // the dropped `DispatchTarget` chain) can tell a forwarded row's
    // actual served model apart from `target.upstream`, and flag the row
    // as forwarded without re-deriving it from request-global bearer
    // presence. `served_model` (the K-triple nickname) is asserted
    // unchanged on both lanes -- it must never carry the wire model.

    #[tokio::test]
    async fn complete_forwarded_target_records_client_model_as_served_upstream() {
        let (router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let dispatched = router
            .complete_with_options(forwarded_req_for("opus"), RouterOptions::default())
            .await;
        dispatched.result.expect("forwarded target must dispatch");
        assert_eq!(
            dispatched.meta.served_upstream,
            Some("opus".to_string()),
            "served_upstream must carry the client's requested model, not target.upstream",
        );
        assert_eq!(
            dispatched.meta.served_model,
            Some("opus".to_string()),
            "the K-triple nickname dimension must stay stable on the forwarded lane",
        );
        assert!(
            dispatched.meta.served_forwarded_credential,
            "the forwarded marker must be set for post-dispatch usage disambiguation",
        );
    }

    #[tokio::test]
    async fn complete_forwarded_unlisted_model_records_it_verbatim_as_served_upstream() {
        // Model transparency for an unlisted model routed via the
        // catch-all `default` alias: served_upstream still mirrors the
        // client's exact (unlisted) request, never target.upstream.
        let (mut router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let mut config = (*router.config).clone();
        config.aliases.insert(
            "default".to_string(),
            AliasValue::Single("opus".to_string()),
        );
        router.config = Arc::new(config);

        let dispatched = router
            .complete_with_options(
                forwarded_req_for("some-unlisted-vendor-model"),
                RouterOptions::default(),
            )
            .await;
        dispatched
            .result
            .expect("default alias must resolve and dispatch");
        assert_eq!(
            dispatched.meta.served_upstream,
            Some("some-unlisted-vendor-model".to_string()),
        );
    }

    #[tokio::test]
    async fn complete_own_target_records_target_upstream_as_served_upstream_unchanged() {
        let (router, _spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        let dispatched = router
            .complete_with_options(req_for("opus"), RouterOptions::default())
            .await;
        dispatched.result.expect("own target must dispatch");
        assert_eq!(
            dispatched.meta.served_upstream,
            Some("claude-opus-upstream".to_string()),
            "an own target's served_upstream must stay target.upstream, unchanged",
        );
        assert_eq!(dispatched.meta.served_model, Some("opus".to_string()));
        assert!(
            !dispatched.meta.served_forwarded_credential,
            "an own target must never set the forwarded marker",
        );
    }

    #[tokio::test]
    async fn stream_forwarded_target_records_client_model_as_served_upstream() {
        let (router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
        let dispatched = router
            .stream_with_options(forwarded_req_for("opus"), RouterOptions::default())
            .await;
        let _stream = dispatched
            .result
            .expect("forwarded stream target must dispatch");
        assert_eq!(dispatched.meta.served_upstream, Some("opus".to_string()));
        assert_eq!(dispatched.meta.served_model, Some("opus".to_string()));
        assert!(dispatched.meta.served_forwarded_credential);
    }

    #[tokio::test]
    async fn stream_own_target_records_target_upstream_as_served_upstream_unchanged() {
        let (router, _spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
        let dispatched = router
            .stream_with_options(req_for("opus"), RouterOptions::default())
            .await;
        let _stream = dispatched.result.expect("own stream target must dispatch");
        assert_eq!(
            dispatched.meta.served_upstream,
            Some("claude-opus-upstream".to_string()),
        );
        assert!(!dispatched.meta.served_forwarded_credential);
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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
            router.force_open_breaker("entry2", std::time::Duration::from_hours(1)),
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
mod breaker_park_preserves_upstream_error_tests {
    //! Completion-path guard: when a debiting 429 carries an upstream reset
    //! large enough to force-open (park) the provider's breaker, the SAME
    //! request must surface the real upstream 429 + Retry-After, NOT the
    //! synthetic status-0 "circuit breaker open" gate error. The synthetic
    //! error stays reserved for a request blocked BEFORE dispatch (the next
    //! request that arrives during the active park).
    use super::*;
    use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
    use routectl_testkit::with_capture;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // A reset far above INLOOP_RETRY_AFTER_CAP, so a debiting 429 parks the
    // provider rather than bumping the in-loop sleep. Equal to the default
    // max_honored_retry_after ceiling, so it is honored unclamped.
    const RETRY_AFTER: Duration = Duration::from_hours(1);

    /// Sole chain-entry provider: every dispatch fails with a real,
    /// debiting 429 carrying a large upstream reset hint. Counts how many
    /// times its body is actually reached.
    struct ParkingProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    impl ParkingProvider {
        fn rate_limited(&self) -> Error {
            Error::upstream_with_retry_after(
                &self.id,
                429,
                "rate limited by upstream",
                Some(RETRY_AFTER),
            )
        }
    }

    #[async_trait]
    impl Provider for ParkingProvider {
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
            Err(self.rate_limited())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.rate_limited())
        }
    }

    /// Router with a sole alias entry dispatching to a provider that always
    /// 429s with a large reset. The default retry policy caps RateLimited at
    /// `max_attempts` (2), so a same-provider retry is admitted at attempt 1
    /// -- exactly the branch that used to discard the genuine error. The
    /// large reset parks the provider, so that retry re-gates to CircuitOpen.
    fn router_with_parking_entry() -> (Router, Arc<AtomicUsize>) {
        let mut config = Config::default();
        config
            .aliases
            .insert("solo".into(), AliasValue::Chain(vec!["seat".into()]));
        config.providers.insert(
            "p".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: ProviderRuntimePolicy {
                    circuit_failures: Some(1),
                    circuit_cooldown_ms: Some(60_000),
                    ..Default::default()
                },
            },
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new(Arc::new(config));
        let provider: Arc<dyn Provider> = Arc::new(ParkingProvider {
            id: "p".into(),
            calls: Arc::clone(&calls),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "seat".into(),
            Arc::new(ResolvedModel::new("seat", "p", provider, "u")),
        );
        router.install_resolved_models(models);
        (router, calls)
    }

    fn solo_req() -> ChatRequest {
        ChatRequest {
            model: "solo".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    fn upstream_status(err: &Error) -> Option<u16> {
        match err {
            Error::Upstream { status, .. } => Some(*status),
            _ => None,
        }
    }

    fn upstream_retry_after(err: &Error) -> Option<Duration> {
        match err {
            Error::Upstream { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    #[tokio::test]
    async fn parking_request_surfaces_real_429_not_synthetic_gate_error() {
        let (router, _calls) = router_with_parking_entry();
        let err = router
            .complete(solo_req())
            .await
            .expect_err("a parked sole entry still fails the request");
        assert_eq!(
            upstream_status(&err),
            Some(429),
            "client must receive the genuine upstream 429, not the synthetic status-0"
        );
        assert_eq!(
            upstream_retry_after(&err),
            Some(RETRY_AFTER),
            "the upstream Retry-After must be preserved on the parking request"
        );
        assert!(
            !err.to_string().contains("circuit breaker open"),
            "the synthetic gate error must not surface on the parking request, got: {err}"
        );
    }

    #[tokio::test]
    async fn parking_request_does_not_retry_the_self_parked_provider() {
        let (router, calls) = router_with_parking_entry();
        let (_result, events) = with_capture(router.complete(solo_req())).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the provider must be dialed exactly once on the parking attempt"
        );
        assert!(
            !events.iter().any(|e| e.message == "retrying same provider"),
            "no same-provider retry may be attempted once this attempt parked the breaker"
        );
    }

    #[tokio::test]
    async fn next_request_during_park_still_sees_synthetic_circuit_open() {
        let (router, calls) = router_with_parking_entry();
        // First request parks the provider and surfaces the real 429.
        let first = router
            .complete(solo_req())
            .await
            .expect_err("first request fails with the real 429");
        assert_eq!(upstream_status(&first), Some(429));
        let dials_after_first = calls.load(Ordering::SeqCst);

        // Second request during the active park is blocked BEFORE dispatch:
        // it must see the synthetic status-0 gate error, and the provider
        // body must not be reached again.
        let second = router
            .complete(solo_req())
            .await
            .expect_err("second request fails while the breaker is parked");
        assert_eq!(
            upstream_status(&second),
            Some(0),
            "a request blocked before dispatch keeps the synthetic status-0"
        );
        assert!(
            second.to_string().contains("circuit breaker open"),
            "the pre-dispatch block must surface the synthetic gate error, got: {second}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            dials_after_first,
            "the parked provider must not be dialed by the next request"
        );
    }

    #[tokio::test]
    async fn retry_decision_event_carries_full_field_set() {
        let (router, _calls) = router_with_parking_entry();
        let (_result, events) = with_capture(router.complete(solo_req())).await;
        let ev = events
            .iter()
            .find(|e| e.message == "retry decision")
            .expect("a retry-decision event must be emitted on the completion error path");
        assert_eq!(ev.level, tracing::Level::DEBUG);
        assert_eq!(ev.field("provider"), Some("p"));
        assert_eq!(ev.field("state_key"), Some("seat"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("attempt"), Some("1"));
        assert_eq!(ev.field("status"), Some("Some(429)"));
        assert_eq!(ev.field("upstream_type"), Some("None"));
        assert_eq!(ev.field("retry_after_ms"), Some("Some(3600000)"));
        assert_eq!(ev.field("breaker_effect"), Some("parked"));
        assert_eq!(ev.field("same_provider_retry"), Some("false"));
        assert_eq!(ev.field("preserved_upstream_error"), Some("true"));
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
        chain_state_keys_for(router, None)
    }

    /// Like [`chain_state_keys`] but threads an explicit inbound session key
    /// (the sticky-pin lookup key) into resolution.
    fn chain_state_keys_for(router: &Router, session_key: Option<&str>) -> Vec<String> {
        router
            .dispatch_chain("opus", session_key)
            .expect("chain resolves")
            .into_iter()
            .map(|t| t.state_key)
            .collect()
    }

    /// The `selection_decision` token on each target of the resolved chain
    /// for `opus`. Same-module access to the private field; lets the
    /// observability tests assert which token (if any) landed on the home
    /// seat without changing any routing.
    fn chain_decisions_for(
        router: &Router,
        session_key: Option<&str>,
    ) -> Vec<Option<&'static str>> {
        router
            .dispatch_chain("opus", session_key)
            .expect("chain resolves")
            .into_iter()
            .map(|t| t.selection_decision)
            .collect()
    }

    #[tokio::test]
    async fn fill_first_records_no_selection_decision() {
        // A genuinely non-sticky pool has no sticky decision: every target's
        // selection_decision is None.
        let (router, _counters) = pooled_router(SeatSelection::FillFirst);
        let decisions = chain_decisions_for(&router, None);
        assert_eq!(decisions, vec![None, None, None]);
    }

    #[tokio::test]
    async fn sticky_keyed_records_decision_on_home_only() {
        // A keyed StickyLeastLoaded pool stamps the sticky token on the home
        // (first) target ONLY -- birth_pick on the first request, sticky_stay
        // on a follow-up for the same session. The fallback seats stay None.
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

        let birth = chain_decisions_for(&router, Some("S"));
        assert_eq!(
            birth,
            vec![Some("birth_pick"), None, None],
            "first request for a session is a birth pick on the home seat"
        );

        let stay = chain_decisions_for(&router, Some("S"));
        assert_eq!(
            stay,
            vec![Some("sticky_stay"), None, None],
            "a follow-up for the same session stays on the pinned home seat"
        );
    }

    #[tokio::test]
    async fn keyless_sticky_records_keyless_fill_first() {
        // StickyLeastLoaded on a multi-seat pool WITHOUT a session key
        // collapses to fill-first; the token must surface that collapse so an
        // operator can spot it. Order is unchanged (byte-for-byte fill-first).
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
        let decisions = chain_decisions_for(&router, None);
        assert_eq!(decisions, vec![Some("keyless_fill_first"), None, None]);
        // The collapse must not alter the seat order.
        assert_eq!(
            chain_state_keys_for(&router, None),
            vec!["opus", "opus#seat-b", "opus#seat-c"]
        );
    }

    #[test]
    fn mark_target_copies_selection_decision_into_meta() {
        // mark_target propagates the home target's selection_decision into
        // the per-request DispatchMeta exactly like the served_* fields.
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
        let chain = router
            .dispatch_chain("opus", Some("S"))
            .expect("chain resolves");
        let home = &chain[0];
        assert_eq!(home.selection_decision, Some("birth_pick"));

        let mut meta = DispatchMeta::for_alias("opus");
        meta.mark_target(home, "opus");
        assert_eq!(meta.selection_decision, Some("birth_pick"));
    }

    #[tokio::test]
    async fn sticky_overflow_repin_stamps_overflow_repin_token() {
        // The thrash signal: a session pinned (birth_pick) whose home seat
        // then trips must, on re-request, migrate to a healthy sibling AND
        // stamp `overflow_repin` on the NEW home target. Reuses the
        // park-and-re-request seam from
        // `sticky_overflow_repin_migrates_once_and_does_not_flap`.
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

        // Birth: first request pins session S and stamps birth_pick.
        let birth = chain_decisions_for(&router, Some("S"));
        assert_eq!(birth[0], Some("birth_pick"));
        let home = chain_state_keys_for(&router, Some("S"))[0].clone();

        // Force the pinned home seat's breaker open.
        assert!(
            router.force_open_breaker(&home, Duration::from_hours(1)),
            "home seat must own a state slot to trip"
        );

        // Re-request ONCE: the migration stamps overflow_repin on the new
        // home (first) target only; fallback seats stay None. A single
        // resolution is read so the one-time-cap (repinned=true) does not
        // turn a follow-up into a sticky_stay before we observe the token.
        let migrated = router
            .dispatch_chain("opus", Some("S"))
            .expect("chain resolves");
        let migrated_decisions: Vec<Option<&'static str>> =
            migrated.iter().map(|t| t.selection_decision).collect();
        assert_ne!(
            migrated[0].state_key, home,
            "overflow-repin must migrate off the parked home seat"
        );
        assert_eq!(
            migrated_decisions,
            vec![Some("overflow_repin"), None, None],
            "the thrash signal must land on the migrated home target only"
        );
    }

    #[tokio::test]
    async fn sticky_defer_no_healthy_stamps_defer_token() {
        // A fresh keyed session (pin miss) over a pool whose every seat's
        // breaker is forced open has no dispatchable home: the outcome is
        // DeferNoHealthy -> `defer_no_healthy` token, fill-first order, and
        // NO pin written.
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
        for key in ["opus", "opus#seat-b", "opus#seat-c"] {
            assert!(
                router.force_open_breaker(key, Duration::from_hours(1)),
                "seat {key} must own a state slot to trip"
            );
        }

        let decisions = chain_decisions_for(&router, Some("S"));
        assert_eq!(
            decisions,
            vec![Some("defer_no_healthy"), None, None],
            "a no-healthy-seat miss must stamp defer_no_healthy on the home target"
        );
        // Order is the fill-first walk (a hint, not a filter), and no pin
        // was written for the deferred session.
        assert_eq!(
            chain_state_keys_for(&router, Some("S")),
            vec!["opus", "opus#seat-b", "opus#seat-c"]
        );
        assert!(
            router.sticky_pins.get("S").is_none(),
            "DeferNoHealthy must not write a pin"
        );
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
        router.park_provider("opus", Duration::from_hours(1));

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
        router.park_provider("opus", Duration::from_hours(1));

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
            before.force_open_breaker("opus", Duration::from_hours(1)),
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

    #[tokio::test]
    async fn sticky_pins_on_miss_then_stays_on_session() {
        // A multi-seat StickyLeastLoaded pool: the first request for session
        // "S" picks (and pins) a home seat; a second request for "S" returns
        // the SAME home seat first (it reads the pin rather than re-picking).
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
        let first = chain_state_keys_for(&router, Some("S"));
        let home = first[0].clone();
        let second = chain_state_keys_for(&router, Some("S"));
        assert_eq!(
            second[0], home,
            "second request for the same session must lead with the pinned home seat"
        );
        // Every seat still appears (the order is a hint, not a filter).
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
    }

    #[tokio::test]
    async fn sticky_keyless_matches_fill_first() {
        // Keyless StickyLeastLoaded routes through seat_order_for_request, so
        // its order is identical to a FillFirst pool's.
        let (sticky, _c1) = pooled_router(SeatSelection::StickyLeastLoaded);
        let (fill, _c2) = pooled_router(SeatSelection::FillFirst);
        let sticky_order = chain_state_keys_for(&sticky, None);
        let fill_order = chain_state_keys(&fill);
        assert_eq!(sticky_order, fill_order);
        assert_eq!(sticky_order, vec!["opus", "opus#seat-b", "opus#seat-c"]);
    }

    #[tokio::test]
    async fn sticky_stale_pin_not_in_pool_is_re_picked() {
        // A pin whose state_key no longer exists in the pool resolves to a
        // miss: the request re-picks a valid in-pool seat (and re-pins it).
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
        router.sticky_pins.put(
            "S",
            crate::seat_pool::SeatPin {
                state_key: "opus#seat-gone".to_string(),
                repinned: false,
            },
        );
        let order = chain_state_keys_for(&router, Some("S"));
        let valid = ["opus", "opus#seat-b", "opus#seat-c"];
        assert!(
            valid.contains(&order[0].as_str()),
            "stale pin must re-pick a valid in-pool seat, got {}",
            order[0]
        );
        // The re-pick repaired the pin to an in-pool seat.
        assert!(
            valid.contains(
                &router
                    .sticky_pins
                    .get("S")
                    .expect("re-pinned")
                    .state_key
                    .as_str()
            )
        );
    }

    #[tokio::test]
    async fn sticky_overflow_repin_migrates_once_and_does_not_flap() {
        // End-to-end overflow-repin: pin a session, force its home seat's breaker open,
        // and assert a subsequent call leads with a healthy sibling AND the
        // pin records repinned=true. Then heal the original and assert the
        // session STAYS on the sibling (hysteresis -- no A->B->A flap). Then
        // park the sibling (new home) and assert the already-repinned session
        // STAYS (does not chase a third seat -- one-time cap).
        let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

        // Birth: pin session S to its home seat.
        let first = chain_state_keys_for(&router, Some("S"));
        let home = first[0].clone();
        assert!(
            !router.sticky_pins.get("S").expect("pinned").repinned,
            "birth pin must start un-repinned"
        );

        // Force the home seat's breaker open for a long cooldown.
        assert!(
            router.force_open_breaker(&home, Duration::from_hours(1)),
            "home seat must own a state slot to trip"
        );

        // The session migrates ONCE to a healthy sibling.
        let migrated = chain_state_keys_for(&router, Some("S"));
        assert_ne!(
            migrated[0], home,
            "overflow-repin must migrate off the parked home seat"
        );
        let sibling = migrated[0].clone();
        let pin_after = router.sticky_pins.get("S").expect("re-pinned");
        assert_eq!(
            pin_after.state_key, sibling,
            "pin must point at the sibling"
        );
        assert!(
            pin_after.repinned,
            "overflow-repin must set repinned=true (the one-time cap marker)"
        );

        // Heal the original home seat. The session must NOT flap back: the pin
        // now points at the healthy sibling, so it STAYS there.
        router.record_success(&home);
        let healed = chain_state_keys_for(&router, Some("S"));
        assert_eq!(
            healed[0], sibling,
            "a recovered original must NOT pull the session back (no A->B->A flap)"
        );

        // Park the NEW home (the sibling). An already-repinned session must
        // STAY rather than chase a third seat (one-time cap).
        assert!(
            router.force_open_breaker(&sibling, Duration::from_hours(1)),
            "sibling seat must own a state slot to trip"
        );
        let capped = chain_state_keys_for(&router, Some("S"));
        assert_eq!(
            capped[0], sibling,
            "an already-repinned session must not chase a third seat"
        );
        assert_eq!(
            router.sticky_pins.get("S").expect("still pinned").state_key,
            sibling,
            "the pin must remain on the sibling -- no second migration"
        );
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
            credential_source: Default::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: vec![],
            forward_client_headers: vec![],
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            cloak: routectl_providers::anthropic_api::CloakConfig::default(),
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
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
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
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
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
    async fn single_capable_seat_not_implemented_yields_terminal_not_implemented_once() {
        // A single capable (anthropic-api) seat that returns a local
        // NotImplemented is a capability error: it is dispatched exactly
        // once (no same-seat retry), the walk exhausts, and the CLIENT
        // sees the terminal walk-exhausted NotImplemented (named by the
        // ALIAS), NOT the seat's verbatim error.
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::NotImplemented,
        }]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        match err {
            Error::NotImplemented(model, _) => assert_eq!(
                model, "alias",
                "must surface the terminal (alias-named) error, not the seat's verbatim one",
            ),
            other => panic!("expected Error::NotImplemented, got {other:?}"),
        }
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "selected capable seat is dispatched once, no same-seat retry",
        );
    }

    /// A count_tokens-capable entry (kind == "anthropic-api") whose
    /// breaker is configured: `circuit_failures` trip it, and a tripped
    /// breaker holds for `circuit_cooldown_ms`. Lets a test observe
    /// whether an outcome DEBITED the breaker: a debit (`record_failure`)
    /// re-trips with the baseline cooldown (-> Open), while a no-debit
    /// release leaves the armed zero-cooldown state (-> HalfOpenReady) or
    /// a closed breaker (-> Closed).
    fn anthropic_api_entry_with_breaker(
        circuit_failures: u32,
        circuit_cooldown_ms: u64,
    ) -> ProviderEntry {
        let mut entry = anthropic_api_entry();
        if let ProviderEntry::AnthropicApi { runtime, .. } = &mut entry {
            runtime.circuit_failures = Some(circuit_failures);
            runtime.circuit_cooldown_ms = Some(circuit_cooldown_ms);
        }
        entry
    }

    /// Whether the seat keyed by `state_key` currently holds the half-open
    /// probe slot.
    fn half_open_in_flight(router: &Router, state_key: &str) -> bool {
        router
            .state
            .get(state_key)
            .expect("seat state slot exists")
            .lock()
            .half_open_probe_in_flight()
    }

    /// Non-mutating breaker phase for the seat keyed by `state_key`.
    fn circuit_phase(router: &Router, state_key: &str) -> crate::runtime_state::CircuitPhase {
        router
            .capacity_snapshot_for(state_key, Instant::now())
            .expect("seat state slot exists")
            .circuit
    }

    #[tokio::test]
    async fn wire_501_on_half_open_probe_releases_slot_without_debiting_breaker() {
        // The incident pin. A capable-by-kind seat whose upstream cannot
        // count returns a WIRE 501. On a half-open count_tokens probe this
        // must be treated as a capability signal: release the probe slot
        // and leave the shared breaker un-debited. Recording it as a
        // health failure would re-trip the breaker (baseline cooldown) and
        // starve completions that gate on the same per-seat breaker.
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(501),
        }]);
        assert!(
            router.force_open_breaker("anthropic-only", Duration::ZERO),
            "seat breaker slot must exist to arm half-open",
        );

        let _ = router.count_tokens(count_req()).await;

        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "the half-open probe must reach the upstream exactly once",
        );
        assert!(
            !half_open_in_flight(&router, "anthropic-only"),
            "a capability wire-501 must release the half-open probe slot",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-only"),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "a capability wire-501 must NOT debit the breaker: no record_failure, \
             so the breaker keeps its armed zero-cooldown state (HalfOpenReady) \
             rather than re-tripping Open with the 60s baseline",
        );
    }

    #[tokio::test]
    async fn local_not_implemented_on_half_open_probe_releases_slot_without_debiting() {
        // Guards the already-exempt case: a local Error::NotImplemented
        // from the selected capable seat is a capability signal and must
        // behave exactly like the wire-501 -- release the half-open slot,
        // no breaker debit.
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::NotImplemented,
        }]);
        assert!(
            router.force_open_breaker("anthropic-only", Duration::ZERO),
            "seat breaker slot must exist to arm half-open",
        );

        let _ = router.count_tokens(count_req()).await;

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert!(
            !half_open_in_flight(&router, "anthropic-only"),
            "a capability NotImplemented must release the half-open probe slot",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-only"),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "a capability NotImplemented must NOT debit the breaker",
        );
    }

    #[tokio::test]
    async fn walks_to_next_capable_seat_on_wire_501_and_returns_its_count() {
        // Chain [anthropic-api(501), anthropic-api(ok)]. The selected
        // capable seat returns a capability wire-501; count_tokens must
        // advance to the NEXT capable seat and return its count -- not
        // surface the 501 to the client. The first seat's breaker must
        // NOT be debited.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-first",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry_with_breaker(1, 60_000),
                behavior: CountBehavior::UpstreamError(501),
            },
            Leg {
                nickname: "anthropic-second",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(42),
            },
        ]);

        let tc = router
            .count_tokens(count_req())
            .await
            .expect("walk must reach the second capable seat and return its count");

        assert_eq!(
            tc.input_tokens, 42,
            "the second capable seat serves the count",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "first seat attempted once",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            1,
            "walk advanced to the second seat",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-first"),
            crate::runtime_state::CircuitPhase::Closed,
            "a capability 501 must not debit the first seat's breaker (stays Closed)",
        );
    }

    #[tokio::test]
    async fn walk_terminates_with_not_implemented_when_all_capable_seats_501() {
        // Every capable seat returns a capability error. The walk must
        // visit each seat at most once (bounded upstream calls) and
        // terminate with the stable Error::NotImplemented rather than
        // looping or leaking the last upstream's raw 501 to the client.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-first",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::UpstreamError(501),
            },
            Leg {
                nickname: "anthropic-second",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::UpstreamError(501),
            },
        ]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        match err {
            Error::NotImplemented(model, msg) => {
                assert_eq!(model, "alias");
                assert!(
                    msg.contains("count_tokens"),
                    "message must name the operation; got: {msg}",
                );
            }
            other => panic!("expected a terminal Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "first seat visited exactly once",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            1,
            "second seat visited exactly once (no re-visit, no loop)",
        );
    }

    #[tokio::test]
    async fn non_capability_429_debits_and_returns_without_walking() {
        // Scope guard: a 429 is a HEALTH error, not a capability error. It
        // must keep today's behavior -- debit the breaker and propagate --
        // and must NOT walk to a later capable seat.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-first",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry_with_breaker(1, 60_000),
                behavior: CountBehavior::UpstreamError(429),
            },
            Leg {
                nickname: "anthropic-second",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(42),
            },
        ]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        assert!(
            matches!(err, Error::Upstream { status: 429, .. }),
            "a 429 must propagate verbatim; got {err:?}",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "first seat attempted once",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "a health error must NOT walk to a later capable seat",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-first"),
            crate::runtime_state::CircuitPhase::Open,
            "a 429 must debit the breaker (threshold 1 -> Open)",
        );
    }

    #[tokio::test]
    async fn non_retryable_4xx_leaves_breaker_closed() {
        // A caller-shaped 4xx (BadRequest class) from a capable count_tokens
        // seat must NOT debit the per-seat breaker that also gates
        // completions and streams. The debit keys off the failure CLASS, so
        // a repeated 4xx storm here leaves the shared breaker CLOSED and
        // every dispatch keeps reaching the seat.
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry_with_breaker(2, 60_000),
            behavior: CountBehavior::UpstreamError(400),
        }]);

        for _ in 0..4 {
            let err = router.count_tokens(count_req()).await.unwrap_err();
            assert!(
                matches!(err, Error::Upstream { status: 400, .. }),
                "a count_tokens 4xx must surface verbatim; got {err:?}",
            );
        }

        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            4,
            "a non-debiting 4xx must never trip the breaker, so every \
             dispatch reaches the capable seat",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-only"),
            crate::runtime_state::CircuitPhase::Closed,
            "a non-retryable 4xx storm must leave the count_tokens seat \
             breaker CLOSED (BadRequest class does not debit)",
        );
    }

    #[tokio::test]
    async fn health_5xx_still_debits_breaker() {
        // Complement to the 4xx case: a 5xx (ServerError class) from a
        // capable count_tokens seat is a health failure and must still debit
        // and trip the shared per-seat breaker.
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(503),
        }]);

        let err = router.count_tokens(count_req()).await.unwrap_err();

        assert!(
            matches!(err, Error::Upstream { status: 503, .. }),
            "a count_tokens 5xx must surface verbatim; got {err:?}",
        );
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(
            circuit_phase(&router, "anthropic-only"),
            crate::runtime_state::CircuitPhase::Open,
            "a count_tokens 5xx (ServerError class) must debit and trip the \
             breaker (threshold 1 -> Open)",
        );
    }

    #[tokio::test]
    async fn walk_reruns_gate_on_next_seat_and_respects_open_breaker() {
        // Guardrail: the capability walk must re-run the gate on each new
        // seat. If the next capable seat's breaker is open, the walk must
        // NOT bypass it -- the gate blocks the dispatch and the
        // circuit-open error surfaces (the seat is never called).
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-first",
                provider_name: "anthropic-prov-a",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::UpstreamError(501),
            },
            Leg {
                nickname: "anthropic-second",
                provider_name: "anthropic-prov-b",
                entry: anthropic_api_entry(),
                behavior: CountBehavior::Ok(42),
            },
        ]);
        // Park the second seat's breaker open for a long, un-elapsed
        // cooldown so its gate returns CircuitOpen (not a half-open probe
        // admission).
        assert!(
            router.force_open_breaker("anthropic-second", Duration::from_hours(1)),
            "second seat breaker slot must exist",
        );

        let err = router.count_tokens(count_req()).await.unwrap_err();

        assert!(
            matches!(&err, Error::Upstream { status: 0, body, .. } if body.contains("circuit breaker")),
            "the walk must re-gate the second seat and surface its open-breaker block; got {err:?}",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "first seat attempted once (capability 501)",
        );
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "an open breaker on the walked-to seat must block the dispatch, not be bypassed",
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

    /// Request carrying an Anthropic structured-output `output_config.
    /// format` on `provider_extras` -- the non-tool-derived source of
    /// the `structured_output` feature key.
    fn structured_output_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![],
            tools: None,
            provider_extras: Some(json!({
                "output_config": {
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object"}
                    }
                }
            })),
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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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

    #[tokio::test]
    async fn structured_output_skips_first_provider_when_listed_unsupported() {
        // Chain [bedrock, anthropic]. Bedrock declares structured_output
        // unsupported. Request carries output_config.format. Dispatch
        // must go DIRECTLY to anthropic (Bedrock Invoke can't enforce
        // constrained decoding -> malformed tool_use the client can't
        // parse).
        let (router, captured_bedrock, captured_anthropic) =
            build_router_with_chain(vec!["structured_output".into()], vec![]);
        let req = structured_output_request("alias");
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
    async fn structured_output_empty_chain_returns_not_implemented() {
        // Both chain entries declare structured_output unsupported. The
        // filter eliminates everyone -> 501 NotImplemented naming the
        // feature key, no upstream attempt.
        let (router, captured_bedrock, captured_anthropic) = build_router_with_chain(
            vec!["structured_output".into()],
            vec!["structured_output".into()],
        );
        let req = structured_output_request("alias");
        let err = router.complete(req).await.unwrap_err();
        match err {
            Error::NotImplemented(alias, msg) => {
                assert_eq!(alias, "alias");
                assert!(
                    msg.contains("structured_output"),
                    "error message must name the feature; got: {msg}",
                );
            }
            other => panic!("expected Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(captured_bedrock.lock().len(), 0);
        assert_eq!(captured_anthropic.lock().len(), 0);
    }

    // --- per-MODEL unsupported_features (unioned with the
    // per-provider list, keyed on nickname so two models on one provider
    // filter independently) ---

    /// Build a router whose alias chain is two MODELS on the SAME single
    /// provider: `["mA" -> "mB"]`. The provider itself declares NO
    /// unsupported features; each model carries its own per-model list.
    /// Proves nickname-keying: two nicknames on one provider filter
    /// independently. Returns per-model captured-request logs.
    fn build_router_two_models_one_provider(
        unsupported_model_a: Vec<String>,
        unsupported_model_b: Vec<String>,
    ) -> (Router, CapturedRequests, CapturedRequests) {
        let mut config = Config::default();
        config.providers.insert(
            "shared-prov".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: ProviderRuntimePolicy {
                    unsupported_features: vec![],
                    ..Default::default()
                },
            },
        );
        config.aliases.insert(
            "alias".into(),
            AliasValue::Chain(vec!["mA".into(), "mB".into()]),
        );
        // Model-static lists live in config.models: the override registry
        // is built from config, mirroring the factory's
        // build_resolved_models (which copies these onto each ResolvedModel).
        config.models.insert(
            "mA".into(),
            crate::config::ModelEntry::new("shared-prov", "upstream-a")
                .with_unsupported_features(unsupported_model_a.clone()),
        );
        config.models.insert(
            "mB".into(),
            crate::config::ModelEntry::new("shared-prov", "upstream-b")
                .with_unsupported_features(unsupported_model_b.clone()),
        );

        let mut router = Router::new(Arc::new(config));
        let captured_a: CapturedRequests = Arc::new(ParkingMutex::new(Vec::new()));
        let captured_b: CapturedRequests = Arc::new(ParkingMutex::new(Vec::new()));
        let p_a: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "shared-prov".into(),
            captured: captured_a.clone(),
        });
        let p_b: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "shared-prov".into(),
            captured: captured_b.clone(),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "mA".into(),
            Arc::new(
                ResolvedModel::new("mA", "shared-prov", p_a, "upstream-a")
                    .with_unsupported_features(unsupported_model_a),
            ),
        );
        models.insert(
            "mB".into(),
            Arc::new(
                ResolvedModel::new("mB", "shared-prov", p_b, "upstream-b")
                    .with_unsupported_features(unsupported_model_b),
            ),
        );
        router.install_resolved_models(models);
        (router, captured_a, captured_b)
    }

    #[tokio::test]
    async fn model_unsupported_drops_only_that_nickname_not_sibling() {
        // (a) Two models on ONE provider. mA declares structured_output
        // unsupported; mB does NOT. An SO request must skip mA and land
        // on mB -- proving the model list is keyed on NICKNAME, not on
        // the (shared) provider name.
        let (router, captured_a, captured_b) =
            build_router_two_models_one_provider(vec!["structured_output".into()], vec![]);
        let req = structured_output_request("alias");
        let resp = router.complete(req).await.expect("dispatch must succeed");
        assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
        assert_eq!(
            captured_a.lock().len(),
            0,
            "mA must be skipped on its per-model unsupported list",
        );
        assert_eq!(
            captured_b.lock().len(),
            1,
            "sibling mB on the same provider must still be tried",
        );
    }

    #[tokio::test]
    async fn empty_model_lists_leave_routing_unchanged() {
        // (c) Neither model declares anything unsupported. An SO request
        // dispatches to the first chain entry exactly as before.
        let (router, captured_a, captured_b) = build_router_two_models_one_provider(vec![], vec![]);
        let req = structured_output_request("alias");
        let resp = router.complete(req).await.expect("ok");
        assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
        assert_eq!(
            captured_a.lock().len(),
            1,
            "empty per-model lists -> filter is a no-op, first entry takes it",
        );
        assert_eq!(captured_b.lock().len(), 0);
    }

    #[tokio::test]
    async fn both_models_unsupported_returns_not_implemented_naming_feature() {
        // (d) Both models declare the feature unsupported via the static
        // union. The chain filters to empty -> 501 NotImplemented naming
        // the feature, no upstream attempt.
        let (router, captured_a, captured_b) = build_router_two_models_one_provider(
            vec!["structured_output".into()],
            vec!["structured_output".into()],
        );
        let req = structured_output_request("alias");
        let err = router.complete(req).await.unwrap_err();
        match err {
            Error::NotImplemented(alias, msg) => {
                assert_eq!(alias, "alias");
                assert!(
                    msg.contains("structured_output"),
                    "error message must name the feature; got: {msg}",
                );
            }
            other => panic!("expected Error::NotImplemented; got {other:?}"),
        }
        assert_eq!(captured_a.lock().len(), 0);
        assert_eq!(captured_b.lock().len(), 0);
    }

    #[tokio::test]
    async fn route_not_strip_leaves_output_config_intact() {
        // (f) ROUTE-not-STRIP: mA is incapable, mB is capable. The
        // filter only DROPS the incapable target -- it must never mutate
        // the request body. The dispatched request on mB must still
        // carry the original output_config.format untouched.
        let (router, _captured_a, captured_b) =
            build_router_two_models_one_provider(vec!["structured_output".into()], vec![]);
        let req = structured_output_request("alias");
        let resp = router.complete(req).await.expect("dispatch must succeed");
        assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
        let dispatched = captured_b.lock();
        assert_eq!(dispatched.len(), 1, "capable mB must receive the request");
        let extras = dispatched[0]
            .provider_extras
            .as_ref()
            .expect("filter must not strip provider_extras");
        assert_eq!(
            extras
                .get("output_config")
                .and_then(|c| c.get("format"))
                .and_then(|f| f.get("type"))
                .and_then(|t| t.as_str()),
            Some("json_schema"),
            "output_config.format must reach the capable target unmodified",
        );
    }

    #[test]
    fn helper_distinguishes_provider_and_model_source() {
        // (e) Unit-test the decision seam directly: provider-scoped vs
        // model-scoped matches return distinct FilterSource variants;
        // a supported feature returns None. Also pins the precedence:
        // with features listed at different scopes, the FIRST requested
        // feature that matches wins (iteration order).
        let mut config = Config::default();
        config.providers.insert(
            "prov-blocks-ws".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: ProviderRuntimePolicy {
                    unsupported_features: vec!["web_search".into()],
                    ..Default::default()
                },
            },
        );
        // The per-model list lives in config.models: the override registry
        // is built from config (mirroring build_resolved_models).
        config.models.insert(
            "m".into(),
            crate::config::ModelEntry::new("prov-blocks-ws", "u")
                .with_unsupported_features(vec!["structured_output".into()]),
        );
        let router = Router::new(Arc::new(config));
        let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "prov-blocks-ws".into(),
            captured: Arc::new(ParkingMutex::new(Vec::new())),
        });
        let model = Arc::new(
            ResolvedModel::new("m", "prov-blocks-ws", stub, "u")
                .with_unsupported_features(vec!["structured_output".into()]),
        );
        let target = into_one_dispatch_target(model);

        // Provider-scoped match.
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["web_search".to_string()],
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            Some(("web_search".to_string(), FilterSource::ProviderStatic)),
        );
        // Model-scoped match.
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["structured_output".to_string()],
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            Some(("structured_output".to_string(), FilterSource::ModelStatic)),
        );
        // Supported feature -> None.
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["computer_use".to_string()],
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            None,
        );
        // Both scopes list a (different) feature: the FIRST requested
        // feature that matches wins, and web_search resolves to the
        // provider scope.
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["web_search".to_string(), "structured_output".to_string()],
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            Some(("web_search".to_string(), FilterSource::ProviderStatic)),
        );
    }

    #[test]
    fn multi_feature_scan_routes_away_and_captures_earlier_probe_admission() {
        // Regression: a target carrying an EXPIRED (probe-due) learned
        // negative on one feature AND an acting negative on another, hit by a
        // request that names both. The scan must not stop at the first
        // feature's probe admission -- it has to reach the second feature's
        // RouteAway (tail-drop the target) AND still capture the earlier probe
        // admission. Dropping that admission would latch the `in_flight` slot
        // the probe claimed, so the feature could never re-probe.
        use crate::learned_capability::{ExportedEntry, RoutingDecision};

        let router = Router::new(Arc::new(Config::default()));
        let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "prov".into(),
            captured: Arc::new(ParkingMutex::new(Vec::new())),
        });
        let model = Arc::new(ResolvedModel::new("nick", "prov", stub, "upstream"));
        let mut target = into_one_dispatch_target(model);
        // The learned pass runs only for a target that carries a provider kind.
        target.provider_kind = Some("openai-compat");

        // Seed the registry directly so each `expires_at` is fixed relative to
        // a captured base -- the filter's own `Instant::now()` fires strictly
        // later, so `structured_output` reads expired (probe-due) and
        // `web_search` still acts, with no fragile clock subtraction.
        let base = Instant::now();
        let probe_due_key = normalize_capability_key("structured_output", "openai-compat");
        let acting_key = normalize_capability_key("web_search", "openai-compat");
        router.learned_capabilities.import_entries(vec![
            ExportedEntry {
                state_key: "nick".into(),
                feature_key: probe_due_key.clone(),
                signal: SignalTier::SelfIdentifying,
                observations: 1,
                first_seen: base,
                last_seen: base,
                expires_at: base,
                in_flight: false,
                consecutive_failed_probes: 0,
            },
            ExportedEntry {
                state_key: "nick".into(),
                feature_key: acting_key,
                signal: SignalTier::SelfIdentifying,
                observations: 1,
                first_seen: base,
                last_seen: base,
                expires_at: base + Duration::from_hours(48),
                in_flight: false,
                consecutive_failed_probes: 0,
            },
        ]);

        // Features in [probe-due, acting] order: the pre-fix code
        // short-circuited on the probe admission and returned `None` (target
        // wrongly kept as supported); the fix scans on to the acting negative.
        let mut admissions = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["structured_output".to_string(), "web_search".to_string()],
            &mut admissions,
            &mut Vec::new(),
        );
        assert_eq!(
            decision,
            Some(("web_search".to_string(), FilterSource::Learned)),
            "RouteAway on the acting feature must decide, not the earlier probe admission",
        );

        // The earlier probe admission survived the scan -- not swallowed by a
        // short-circuit -- so the dispatch path can settle its slot.
        assert_eq!(
            admissions.len(),
            1,
            "the probe-due feature's admission must be captured",
        );
        assert_eq!(admissions[0].state_key, "nick");
        assert_eq!(admissions[0].feature, probe_due_key);

        // The probe claimed the single in_flight slot: while it is held a
        // repeat query routes away, proving the slot is genuinely occupied.
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "structured_output",
                "openai-compat",
                Instant::now(),
            ),
            RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
            "the in_flight slot is held until the admission settles",
        );

        // Settle exactly as dispatch does: arm the guard from the captured
        // admissions and drop it (the fallback / other-error settle path).
        {
            let _guard = LearnedProbeGuard::armed(
                router.learned_capabilities.clone(),
                admissions,
                "complete",
            );
        }
        // The released slot makes the feature re-probable; had the admission
        // been dropped the slot would have latched forever.
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "structured_output",
                "openai-compat",
                Instant::now(),
            ),
            RoutingDecision::ProbeAdmitted,
            "settling the captured admission releases in_flight; the feature re-probes",
        );
    }

    #[test]
    fn filter_source_as_str_tokens() {
        assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");
        assert_eq!(FilterSource::ModelStatic.as_str(), "model");
    }

    // --- strip-vs-route verdict (capability-strip wiring) ---

    /// An acting (non-expired) learned negative for `(state_key, feature)`,
    /// normalized under the `openai-compat` kind these strip tests use
    /// (identity normalization for a clean key).
    fn acting_negative(
        state_key: &str,
        feature: &str,
        base: Instant,
    ) -> crate::learned_capability::ExportedEntry {
        crate::learned_capability::ExportedEntry {
            state_key: state_key.into(),
            feature_key: normalize_capability_key(feature, "openai-compat"),
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base + Duration::from_hours(48),
            in_flight: false,
            consecutive_failed_probes: 0,
        }
    }

    /// A probe-due (expired) learned negative: `expires_at == base`, which
    /// the filter's strictly-later `Instant::now()` reads as expired, so
    /// the single re-probe slot is admitted.
    fn probe_due_negative(
        state_key: &str,
        feature: &str,
        base: Instant,
    ) -> crate::learned_capability::ExportedEntry {
        crate::learned_capability::ExportedEntry {
            expires_at: base,
            ..acting_negative(state_key, feature, base)
        }
    }

    fn strip_target(nickname: &str) -> DispatchTarget {
        let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: nickname.into(),
            captured: Arc::new(ParkingMutex::new(Vec::new())),
        });
        let model = Arc::new(ResolvedModel::new(nickname, "prov", stub, "upstream"));
        let mut target = into_one_dispatch_target(model);
        // The learned pass runs only for a target carrying a provider kind.
        target.provider_kind = Some("openai-compat");
        target
    }

    #[test]
    fn all_strip_negatives_keep_target_supported_with_sorted_keys() {
        // Two acting negatives, both droppable: advisor (tool-shape strip)
        // and context_management (beta strip). No route-away, no pin -> the
        // target stays supported carrying both keys in sorted normalized
        // order so a per-session cache prefix stays stable.
        let router = Router::new(Arc::new(Config::default()));
        let target = strip_target("nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            acting_negative("nick", "context_management", base),
            acting_negative("nick", "advisor", base),
        ]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string(), "advisor".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        assert_eq!(
            decision, None,
            "all-strip negatives keep the target supported"
        );
        assert_eq!(
            strip_keys,
            vec!["advisor".to_string(), "context_management".to_string()],
            "strip keys are sorted normalized",
        );
        assert!(admissions.is_empty());
    }

    #[test]
    fn any_route_away_negative_demotes_target_and_leaves_strip_empty() {
        // A droppable negative (context_management) coexists with an
        // essential route-away one (web_search). ANY route-away demotes the
        // whole target to the tail; the strip set is abandoned so the target
        // is never half-stripped.
        let router = Router::new(Arc::new(Config::default()));
        let target = strip_target("nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            acting_negative("nick", "context_management", base),
            acting_negative("nick", "web_search", base),
        ]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string(), "web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        assert_eq!(
            decision,
            Some(("web_search".to_string(), FilterSource::Learned))
        );
        assert!(
            strip_keys.is_empty(),
            "a route-away target never carries strip keys"
        );
    }

    #[test]
    fn admitted_probe_feature_excluded_from_strip_but_admission_recorded() {
        // context_management is probe-due (a would-be strip) and advisor is
        // an acting strip. The admitted re-probe tests the REAL capability on
        // the full request, so context_management is excluded from the strip
        // set -- yet its admission still reaches `admissions` to settle the
        // in_flight slot. advisor still strips.
        let router = Router::new(Arc::new(Config::default()));
        let target = strip_target("nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            probe_due_negative("nick", "context_management", base),
            acting_negative("nick", "advisor", base),
        ]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string(), "advisor".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        assert_eq!(decision, None);
        assert_eq!(
            strip_keys,
            vec!["advisor".to_string()],
            "the admitted-probe feature is never stripped",
        );
        assert_eq!(
            admissions.len(),
            1,
            "the probe admission still settles its slot"
        );
        assert_eq!(
            admissions[0].feature,
            normalize_capability_key("context_management", "openai-compat"),
        );
    }

    #[test]
    fn stripped_success_leaves_negative_while_admitted_probe_success_clears() {
        // The two-sided invariant: an admitted probe's full-request 2xx clears
        // its negative, but a stripped success clears nothing. advisor is an
        // ACTING strip (stripped in place -> NO admission); context_management
        // is PROBE-DUE (admitted, bypassed from the strip set). The filter
        // records only the probe admission, so settling a 2xx over the recorded
        // admissions clears context_management yet leaves the stripped advisor
        // negative acting.
        let router = Router::new(Arc::new(Config::default()));
        let target = strip_target("nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            acting_negative("nick", "advisor", base),
            probe_due_negative("nick", "context_management", base),
        ]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["advisor".to_string(), "context_management".to_string()],
            &mut admissions,
            &mut strip_keys,
        );
        assert_eq!(decision, None);
        assert_eq!(strip_keys, vec!["advisor".to_string()]);
        assert_eq!(
            admissions.len(),
            1,
            "only the probe admits; the strip records no admission",
        );

        // A full-request 2xx settles exactly the recorded admissions.
        for adm in &admissions {
            router.learned_capabilities.record_probe_outcome(
                &adm.state_key,
                &adm.feature,
                adm.provider_kind,
                crate::learned_capability::ProbeOutcome::Success,
                base,
            );
        }

        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "context_management",
                "openai-compat",
                base,
            ),
            crate::learned_capability::RoutingDecision::Allow,
            "the admitted probe's 2xx cleared its negative",
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "nick",
                "advisor",
                "openai-compat",
                base,
            ),
            crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
            "a stripped success never clears the stripped feature's negative",
        );
    }

    #[test]
    fn probe_bypass_of_strip_eligible_feature_emits_probe_bypassed_warn() {
        // context_management is a probe-due droppable Strip (strip-eligible),
        // web_search is a probe-due essential (route-away). Both are admitted
        // for re-probe, so neither is stripped -- but only the strip-eligible
        // bypass surfaces a `probe_bypassed` WARN at the verdict site; the
        // route-away feature was never strip-eligible and stays silent.
        let router = Router::new(Arc::new(Config::default()));
        let target = strip_target("nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            probe_due_negative("nick", "context_management", base),
            probe_due_negative("nick", "web_search", base),
        ]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let mut decision = None;
        let events = routectl_testkit::capture_events(|| {
            decision = router.unsupported_feature_for_target(
                &target,
                &["context_management".to_string(), "web_search".to_string()],
                &mut admissions,
                &mut strip_keys,
            );
        });

        // Both features are admitted probes: the target stays supported, no
        // key is stripped, and both admissions settle their slots.
        assert_eq!(decision, None);
        assert!(strip_keys.is_empty());
        assert_eq!(admissions.len(), 2);

        // Exactly one `probe_bypassed` WARN fires, for the strip-eligible
        // feature, carrying the capability token and the target's state key.
        let bypass_warns: Vec<_> = events
            .iter()
            .filter(|e| {
                e.level == tracing::Level::WARN
                    && e.message == "capability_strip_decision"
                    && e.field("outcome") == Some("probe_bypassed")
            })
            .collect();
        assert_eq!(
            bypass_warns.len(),
            1,
            "one probe_bypassed WARN for the strip-eligible bypassed feature",
        );
        assert_eq!(
            bypass_warns[0].field("capability_key"),
            Some(normalize_capability_key("context_management", "openai-compat").as_str()),
        );
        assert_eq!(bypass_warns[0].field("event"), Some("strip"));
        assert_eq!(bypass_warns[0].field("state_key"), Some("nick"));
    }

    #[test]
    fn operator_pinned_beta_capability_routes_away_never_strips() {
        // context_management is a droppable beta strip, but the operator pins
        // its beta token via the model's header_extras anthropic-beta floor.
        // Bedrock/Anthropic egresses re-add the token AFTER the canonical
        // strip, so a strip would be a false success -> route away instead.
        let router = Router::new(Arc::new(Config::default()));
        let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "nick".into(),
            captured: Arc::new(ParkingMutex::new(Vec::new())),
        });
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-beta".to_string(),
            "context-management-2025-06-27".to_string(),
        );
        let model = Arc::new(
            ResolvedModel::new("nick", "prov", stub, "upstream").with_header_extras(headers),
        );
        let mut target = into_one_dispatch_target(model);
        target.provider_kind = Some("openai-compat");

        let base = Instant::now();
        router
            .learned_capabilities
            .import_entries(vec![acting_negative("nick", "context_management", base)]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        assert_eq!(
            decision,
            Some(("context_management".to_string(), FilterSource::Learned)),
            "an operator-pinned beta strip routes away",
        );
        assert!(strip_keys.is_empty());
    }

    #[test]
    fn beta_pinned_reads_provider_and_model_floors_and_ignores_non_beta_strips() {
        // Provider header_extras pins the beta; a tool-shape strip (advisor)
        // carries no beta token and is never pinned.
        let mut config = Config::default();
        let mut provider_headers = BTreeMap::new();
        provider_headers.insert(
            "anthropic-beta".to_string(),
            "context-management-2025-06-27".to_string(),
        );
        config.providers.insert(
            "prov".into(),
            ProviderEntry::OpenaiCompat {
                base_url: "https://placeholder.invalid/v1".into(),
                api_key_ref: "literal:k".into(),
                header_extras: provider_headers,
                payload_extras: None,
                user_agent: None,
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: crate::config::ProviderRuntimePolicy::default(),
            },
        );
        let router = Router::new(Arc::new(config));
        let target = strip_target("prov");

        assert!(
            router.beta_pinned_for_target(&target, "context_management"),
            "provider header_extras floor pins the beta",
        );
        assert!(
            !router.beta_pinned_for_target(&target, "advisor"),
            "a tool-shape strip carries no beta token",
        );
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_provider_beta_floor_routes_away_never_strips() {
        // Bedrock analogue of `operator_pinned_beta_capability_routes_away_
        // never_strips`: here the beta token is pinned by the Bedrock
        // provider's `anthropic_beta` floor, not by header_extras. The
        // invoke/converse adapters re-add that floor on the wire AFTER the
        // canonical strip, so a BetaFlag strip of a floor-pinned token is a
        // false success -> the target must route away instead of shipping the
        // pinned flag. This pins the `anthropic_beta_floor` source of the
        // guard, which is otherwise exercised only via header_extras.
        use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
        let mut config = Config::default();
        config.providers.insert(
            "prov".into(),
            ProviderEntry::Bedrock {
                region: "us-west-2".into(),
                api_shape: BedrockApiShapeConfig::Invoke,
                creds: BedrockCredsConfig::DefaultChain,
                user_agent: None,
                header_extras: BTreeMap::new(),
                payload_extras: None,
                anthropic_beta: vec!["context-management-2025-06-27".to_string()],
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: ProviderRuntimePolicy::default(),
            },
        );
        let router = Router::new(Arc::new(config));
        let target = strip_target("prov");

        assert!(
            router.beta_pinned_for_target(&target, "context_management"),
            "the Bedrock provider anthropic_beta floor pins the beta token",
        );

        let base = Instant::now();
        router
            .learned_capabilities
            .import_entries(vec![acting_negative("prov", "context_management", base)]);

        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        assert_eq!(
            decision,
            Some(("context_management".to_string(), FilterSource::Learned)),
            "a Bedrock floor-pinned beta strip routes away rather than stripping",
        );
        assert!(
            strip_keys.is_empty(),
            "the pinned strip must not be attached to the target",
        );
    }

    #[test]
    fn filter_chain_keeps_stripped_target_and_tails_route_away() {
        // Two targets: one carrying a droppable-only negative
        // (context_management) STAYS supported with the strip key attached;
        // one carrying an essential negative (web_search) is tail-demoted.
        let router = Router::new(Arc::new(Config::default()));
        let strip_t = strip_target("strip-nick");
        let route_t = strip_target("route-nick");
        let base = Instant::now();
        router.learned_capabilities.import_entries(vec![
            acting_negative("strip-nick", "context_management", base),
            acting_negative("route-nick", "web_search", base),
        ]);

        let mut admissions = Vec::new();
        let out = router
            .filter_chain_by_features(
                vec![strip_t, route_t],
                &["context_management".to_string(), "web_search".to_string()],
                "alias",
                &mut admissions,
            )
            .unwrap();

        assert_eq!(out.len(), 2);
        // The strip target stays first (supported); the route-away target is
        // demoted to the tail.
        assert_eq!(out[0].state_key, "strip-nick");
        assert_eq!(
            &*out[0].strip_capabilities,
            &["context_management".to_string()],
        );
        assert_eq!(out[1].state_key, "route-nick");
        assert!(
            out[1].strip_capabilities.is_empty(),
            "a tail-demoted target carries no strip keys",
        );
    }

    // --- apply_strip_interceptor: outcome mapping, mutation, metrics ---

    fn advisor_tool() -> ToolDef {
        ToolDef::Other(json!({"type": "advisor", "name": "advisor"}))
    }

    fn advisor_request() -> ChatRequest {
        ChatRequest {
            model: "nick".into(),
            messages: vec![],
            tools: Some(vec![advisor_tool()]),
            ..Default::default()
        }
    }

    fn with_strip_keys(mut target: DispatchTarget, keys: &[&str]) -> DispatchTarget {
        target.strip_capabilities =
            std::sync::Arc::from(keys.iter().map(|k| (*k).to_string()).collect::<Vec<_>>());
        target
    }

    fn strict_router() -> Router {
        let mut config = Config::default();
        config.server.strict_translation = true;
        Router::new(Arc::new(config))
    }

    #[test]
    fn strip_helper_applies_and_bumps_strip_total() {
        let router = Router::new(Arc::new(Config::default()));
        let target = with_strip_keys(strip_target("nick"), &["advisor"]);
        let mut attempt_req = advisor_request();

        let mut decision = None;
        let events = routectl_testkit::capture_events(|| {
            decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
        });
        let decision = decision.expect("interceptor ran");

        assert!(matches!(decision, StripDecision::Proceed));
        assert!(
            attempt_req.tools.is_none(),
            "the sole advisor tool is stripped and the emptied list normalizes to None",
        );
        assert_eq!(router.metrics.strip_total(), 1);
        assert_eq!(router.metrics.strip_rollback_total(), 0);
        assert_eq!(router.metrics.strip_strict_rejected_total(), 0);

        let warn = events
            .iter()
            .find(|e| e.message == "capability_strip_decision")
            .expect("a strip must emit a capability_strip_decision WARN");
        assert_eq!(warn.field("event"), Some("strip"));
        assert_eq!(warn.field("state_key"), Some("nick"));
        assert_eq!(warn.field("capability_key"), Some("advisor"));
        assert_eq!(warn.field("outcome"), Some("applied"));
    }

    #[test]
    fn strip_helper_is_noop_when_surface_absent() {
        // The verdict names advisor, but the request carries no advisor
        // surface: a plain no-op, not a strip -- strip_total stays at zero.
        let router = Router::new(Arc::new(Config::default()));
        let target = with_strip_keys(strip_target("nick"), &["advisor"]);
        let mut attempt_req = ChatRequest {
            model: "nick".into(),
            tools: Some(vec![ToolDef::Other(
                json!({"type": "web_search_20250305", "name": "search"}),
            )]),
            ..Default::default()
        };
        let before = serde_json::to_value(&attempt_req).unwrap();

        let mut decision = None;
        let events = routectl_testkit::capture_events(|| {
            decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
        });
        let decision = decision.expect("interceptor ran");

        assert!(matches!(decision, StripDecision::Proceed));
        assert_eq!(serde_json::to_value(&attempt_req).unwrap(), before);
        assert_eq!(router.metrics.strip_total(), 0);

        let warn = events
            .iter()
            .find(|e| e.message == "capability_strip_decision")
            .expect("a no-op strip decision still emits a WARN");
        assert_eq!(warn.field("event"), Some("strip"));
        assert_eq!(warn.field("state_key"), Some("nick"));
        assert_eq!(warn.field("capability_key"), Some("advisor"));
        assert_eq!(warn.field("outcome"), Some("noop"));
    }

    #[test]
    fn strip_helper_strict_rejects_without_mutation() {
        let router = strict_router();
        let target = with_strip_keys(strip_target("nick"), &["advisor"]);
        let mut attempt_req = advisor_request();
        let before = serde_json::to_value(&attempt_req).unwrap();

        let mut decision = None;
        let events = routectl_testkit::capture_events(|| {
            decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
        });
        let decision = decision.expect("interceptor ran");

        match decision {
            StripDecision::StrictReject(Error::Validation(msg)) => {
                assert!(msg.contains("advisor"), "{msg}");
            }
            other => panic!("expected StrictReject(Validation), got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&attempt_req).unwrap(),
            before,
            "strict mode blocks before any mutation",
        );
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);
        assert_eq!(router.metrics.strip_rollback_total(), 0);

        let warn = events
            .iter()
            .find(|e| e.message == "capability_strip_decision")
            .expect("a strict rejection still emits a WARN");
        assert_eq!(warn.field("event"), Some("strip"));
        assert_eq!(warn.field("state_key"), Some("nick"));
        assert_eq!(warn.field("capability_key"), Some("advisor"));
        assert_eq!(warn.field("outcome"), Some("strict_rejected"));
    }

    #[test]
    fn strip_helper_rolls_back_and_routes_away() {
        // tool_choice forces the advisor tool the strip removes: a
        // strip-created hazard the post-strip check rolls back. The
        // request is restored and the attempt routes away.
        let router = Router::new(Arc::new(Config::default()));
        let target = with_strip_keys(strip_target("nick"), &["advisor"]);
        let mut attempt_req = ChatRequest {
            model: "nick".into(),
            tools: Some(vec![advisor_tool()]),
            tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
            ..Default::default()
        };
        let before = serde_json::to_value(&attempt_req).unwrap();

        let mut decision = None;
        let events = routectl_testkit::capture_events(|| {
            decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
        });
        let decision = decision.expect("interceptor ran");

        assert!(matches!(decision, StripDecision::RouteAway(_)));
        assert_eq!(
            serde_json::to_value(&attempt_req).unwrap(),
            before,
            "the rolled-back request is byte-identical to the pre-strip bytes",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);

        let warn = events
            .iter()
            .find(|e| e.message == "capability_strip_decision")
            .expect("a rolled-back strip still emits a WARN");
        assert_eq!(warn.field("event"), Some("strip"));
        assert_eq!(warn.field("state_key"), Some("nick"));
        assert_eq!(warn.field("capability_key"), Some("advisor"));
        assert_eq!(warn.field("outcome"), Some("validation_rolled_back"));
    }

    #[test]
    fn strip_helper_is_inert_with_empty_verdict_even_under_strict() {
        // Kill-switch by construction: an empty strip verdict (disabled
        // learning, probe-admitted, or operator-pinned features) leaves the
        // helper inert -- no mutation, no counter, even under strict.
        let router = strict_router();
        let target = strip_target("nick");
        let mut attempt_req = advisor_request();
        let before = serde_json::to_value(&attempt_req).unwrap();

        let decision = router.apply_strip_interceptor(&target, &mut attempt_req);

        assert!(matches!(decision, StripDecision::Proceed));
        assert_eq!(serde_json::to_value(&attempt_req).unwrap(), before);
        assert_eq!(router.metrics.strip_total(), 0);
        assert_eq!(router.metrics.strip_strict_rejected_total(), 0);
        assert_eq!(router.metrics.strip_rollback_total(), 0);
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
mod forwarded_auth_terminal_tests {
    //! A forwarded-credential request that draws an upstream
    //! 401 / 403 / 429 is TERMINAL. routectl bypasses BOTH the
    //! `on_auth_failure` refresh-and-retry AND the fallback-chain hop,
    //! and surfaces the upstream status verbatim -- a request-scoped
    //! forwarded token has no refresh path and no credential to fall
    //! back to, so both recoveries are useless and wrong. Non-forwarded
    //! requests keep the existing one-shot auth-refresh + fallback
    //! behavior (also asserted here, and in `auth_failure_recovery_tests`).
    //!
    //! The structured-log assertion for the surfaced-verbatim WARN lives
    //! in the isolated integration binary
    //! `tests/forwarded_auth_terminal_log.rs` -- a thread-local capture
    //! subscriber over a shared `warn!` callsite is unreliable inside the
    //! 700+-test lib binary. These lib tests pin the deterministic,
    //! subscriber-independent facts: no on_auth_failure, no fallback,
    //! verbatim status.
    use super::*;
    use crate::config::{ProviderEntry, RetryPolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use routectl_core::schema::ForwardedBearer;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, TokenCount};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The forwarded chain's FIRST (Anthropic) entry: returns
    /// `Error::Upstream { status, .. }` on every complete / stream /
    /// count_tokens call, counting each call and each `on_auth_failure`
    /// invocation so a test can prove the router never tried to rotate
    /// its own credential for a forwarded request.
    struct StatusProvider {
        id: String,
        status: u16,
        complete_calls: AtomicUsize,
        stream_calls: AtomicUsize,
        count_tokens_calls: AtomicUsize,
        on_auth_failure_calls: AtomicUsize,
    }

    impl StatusProvider {
        fn new(id: &str, status: u16) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                status,
                complete_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
                count_tokens_calls: AtomicUsize::new(0),
                on_auth_failure_calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Provider for StatusProvider {
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
            Err(Error::upstream(
                &self.id,
                self.status,
                "forwarded upstream rejected",
            ))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(
                &self.id,
                self.status,
                "forwarded upstream rejected",
            ))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(
                &self.id,
                self.status,
                "forwarded upstream rejected",
            ))
        }
        async fn on_auth_failure(&self) -> Result<()> {
            self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// The chain's SECOND (fallback) entry. Returns a DISTINCT status
    /// (502) so any stray fallback hop flips both the surfaced status
    /// AND this counter -- either assertion catches a leaked fallback.
    /// Counts every dispatch so a test can assert it stayed at 0.
    struct SiblingProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for SiblingProvider {
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
            Err(Error::upstream(&self.id, 502, "sibling reached"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 502, "sibling reached"))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 502, "sibling reached"))
        }
    }

    /// Wire a router whose alias `"alias"` resolves to a two-entry chain
    /// `[m-primary, m-sibling]`, both backed by `anthropic-api` provider
    /// entries on `api.anthropic.com` so the forwarded-passthrough gate
    /// admits the request and dispatch actually reaches the first seat.
    /// The concrete provider Arcs are the counting mocks above.
    ///
    /// Returns the router plus the primary provider handle and the
    /// sibling call counter for post-dispatch assertions. A fast,
    /// no-retry `RetryPolicy` keeps every path single-attempt-per-seat.
    /// `primary_forwarded` marks `p-primary`'s provider entry
    /// `credential_source = Forwarded` (the per-target flag the terminal
    /// re-key now keys off) when `true`; `p-sibling` is always an Own
    /// provider (it must never legitimately be reached by these tests).
    fn build_chain(
        status: u16,
        primary_forwarded: bool,
    ) -> (Router, Arc<StatusProvider>, Arc<AtomicUsize>) {
        let mut config = Config {
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                jitter_ms: 0,
                ..RetryPolicy::default()
            },
            ..Config::default()
        };
        let mut primary_entry = ProviderEntry::anthropic_api("literal:k");
        if primary_forwarded {
            primary_entry =
                primary_entry.with_credential_source(crate::config::CredentialSource::Forwarded);
        }
        config.providers.insert("p-primary".into(), primary_entry);
        config.providers.insert(
            "p-sibling".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        config.aliases.insert(
            "alias".into(),
            AliasValue::Chain(vec!["m-primary".into(), "m-sibling".into()]),
        );

        let primary = StatusProvider::new("p-primary", status);
        let sibling_calls = Arc::new(AtomicUsize::new(0));
        let sibling: Arc<dyn Provider> = Arc::new(SiblingProvider {
            id: "p-sibling".into(),
            calls: sibling_calls.clone(),
        });

        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m-primary".into(),
            Arc::new(ResolvedModel::new(
                "m-primary",
                "p-primary",
                primary.clone() as Arc<dyn Provider>,
                "claude-primary",
            )),
        );
        models.insert(
            "m-sibling".into(),
            Arc::new(ResolvedModel::new(
                "m-sibling",
                "p-sibling",
                sibling,
                "claude-sibling",
            )),
        );

        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(models);
        (router, primary, sibling_calls)
    }

    fn forwarded_req() -> ChatRequest {
        let mut req = ChatRequest {
            model: "alias".into(),
            ..Default::default()
        };
        req.routectl_internal.forwarded_bearer =
            Some(ForwardedBearer::new("sk-ant-oat01-FORWARDED".into()));
        req
    }

    fn plain_req() -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            ..Default::default()
        }
    }

    fn upstream_status(err: &Error) -> u16 {
        match err {
            Error::Upstream { status, .. } => *status,
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    // ---- complete() ----

    #[tokio::test]
    async fn complete_forwarded_401_is_terminal_no_auth_failure_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(401, true);

        let err = router
            .complete(forwarded_req())
            .await
            .expect_err("forwarded 401 must surface verbatim, not recover");

        assert_eq!(upstream_status(&err), 401, "verbatim upstream status");
        assert_eq!(
            primary.on_auth_failure_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT trigger on_auth_failure",
        );
        assert_eq!(
            primary.complete_calls.load(Ordering::SeqCst),
            1,
            "forwarded 401 must not refresh-and-retry the same seat",
        );
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT fall back to the sibling target",
        );
    }

    #[tokio::test]
    async fn complete_forwarded_403_is_terminal_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(403, true);

        let err = router
            .complete(forwarded_req())
            .await
            .expect_err("forwarded 403 must surface verbatim");

        assert_eq!(upstream_status(&err), 403);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(primary.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            0,
            "forwarded 403 must NOT fall back to the sibling target",
        );
    }

    #[tokio::test]
    async fn complete_forwarded_429_is_terminal_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(429, true);

        let err = router
            .complete(forwarded_req())
            .await
            .expect_err("forwarded 429 must surface verbatim");

        assert_eq!(upstream_status(&err), 429);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            primary.complete_calls.load(Ordering::SeqCst),
            1,
            "forwarded 429 is terminal: no same-provider retry",
        );
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            0,
            "forwarded 429 must NOT fall back to the sibling target",
        );
    }

    // ---- stream() ----

    #[tokio::test]
    async fn stream_forwarded_401_is_terminal_no_auth_failure_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(401, true);

        let err = router
            .stream(forwarded_req())
            .await
            .err()
            .expect("forwarded 401 must surface verbatim before any chunk");

        assert_eq!(upstream_status(&err), 401);
        assert_eq!(
            primary.on_auth_failure_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT trigger on_auth_failure on the stream path",
        );
        assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT fall back on the stream path",
        );
    }

    #[tokio::test]
    async fn stream_forwarded_403_is_terminal_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(403, true);

        let err = router
            .stream(forwarded_req())
            .await
            .err()
            .expect("forwarded 403 must surface verbatim");

        assert_eq!(upstream_status(&err), 403);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stream_forwarded_429_is_terminal_no_fallback() {
        let (router, primary, sibling_calls) = build_chain(429, true);

        let err = router
            .stream(forwarded_req())
            .await
            .err()
            .expect("forwarded 429 must surface verbatim");

        assert_eq!(upstream_status(&err), 429);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
    }

    // ---- count_tokens() ----

    #[tokio::test]
    async fn count_tokens_forwarded_401_is_terminal_no_auth_failure() {
        let (router, primary, sibling_calls) = build_chain(401, true);

        let err = router
            .count_tokens(forwarded_req())
            .await
            .expect_err("forwarded 401 must surface verbatim");

        assert_eq!(upstream_status(&err), 401);
        assert_eq!(
            primary.on_auth_failure_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT trigger on_auth_failure on the count_tokens path",
        );
        assert_eq!(
            primary.count_tokens_calls.load(Ordering::SeqCst),
            1,
            "forwarded 401 must not refresh-and-retry the count_tokens seat",
        );
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            0,
            "forwarded 401 must NOT walk to the sibling seat",
        );
    }

    #[tokio::test]
    async fn count_tokens_forwarded_403_is_terminal() {
        let (router, primary, sibling_calls) = build_chain(403, true);

        let err = router
            .count_tokens(forwarded_req())
            .await
            .expect_err("forwarded 403 must surface verbatim");

        assert_eq!(upstream_status(&err), 403);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(primary.count_tokens_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn count_tokens_forwarded_429_is_terminal() {
        let (router, primary, sibling_calls) = build_chain(429, true);

        let err = router
            .count_tokens(forwarded_req())
            .await
            .expect_err("forwarded 429 must surface verbatim");

        assert_eq!(upstream_status(&err), 429);
        assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
        assert_eq!(primary.count_tokens_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
    }

    // ---- non-forwarded regression: the bypass is gated on the target's
    //      use_forwarded_credential, not request-global bearer presence ----

    #[tokio::test]
    async fn complete_non_forwarded_401_still_refreshes_and_falls_back() {
        // Identical router, but no forwarded bearer: the existing
        // one-shot auth-refresh (on_auth_failure) MUST still fire, and
        // after the second 401 the chain MUST still fall back to the
        // sibling. This is the guard that the forwarded-terminal bypass
        // is scoped to a forwarded-credential TARGET only.
        let (router, primary, sibling_calls) = build_chain(401, false);

        let err = router
            .complete(plain_req())
            .await
            .expect_err("non-forwarded chain exhausts to the sibling error");

        assert_eq!(
            upstream_status(&err),
            502,
            "non-forwarded 401 falls back to the sibling (502)",
        );
        assert_eq!(
            primary.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "non-forwarded 401 must still trigger the one-shot refresh",
        );
        assert_eq!(
            primary.complete_calls.load(Ordering::SeqCst),
            2,
            "non-forwarded 401 refreshes and retries the same seat once",
        );
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            1,
            "non-forwarded 401 must still fall back to the sibling",
        );
    }

    #[tokio::test]
    async fn complete_forwarded_bearer_present_but_target_is_own_credential_still_refreshes_and_falls_back()
     {
        // Coexistence regression: a MITM-marked request
        // (a captured forwarded bearer IS present) whose alias resolves
        // to an OWN-credential Anthropic provider must retry/fall back
        // EXACTLY as before the per-target passthrough gate -- the floating
        // bearer is never consumed by
        // an own-creds target, and the terminal bypass never wrongly
        // fires just because a bearer happens to be present on the
        // request. Same router as the plain-request regression above;
        // only the request differs.
        let (router, primary, sibling_calls) = build_chain(401, false);

        let err = router
            .complete(forwarded_req())
            .await
            .expect_err("own-credential chain exhausts to the sibling error");

        assert_eq!(
            upstream_status(&err),
            502,
            "a captured bearer must not change fallback behavior on an own-credential target",
        );
        assert_eq!(
            primary.on_auth_failure_calls.load(Ordering::SeqCst),
            1,
            "an own-credential target must still get the one-shot refresh \
             even though the request carries a forwarded bearer",
        );
        assert_eq!(
            primary.complete_calls.load(Ordering::SeqCst),
            2,
            "own-credential target refreshes and retries the same seat once",
        );
        assert_eq!(
            sibling_calls.load(Ordering::SeqCst),
            1,
            "own-credential target must still fall back to the sibling",
        );
    }
}

#[cfg(test)]
mod circuit_breaker_slot_release_tests {
    //! Regression: a half-open probe must never leave `half_open_in_flight`
    //! stuck `true`, or every later gate check returns CircuitOpen and the
    //! breaker is permanently locked open for that provider until process
    //! restart. Two leak classes are covered here:
    //!   - synchronous early-returns (probe fast-fail on 429/529, 401-refresh)
    //!     that must release the slot before returning/continuing;
    //!   - async CANCELLATION: a dispatch future dropped while awaiting the
    //!     upstream, after the gate claimed the slot but before any settle arm
    //!     runs -- covered by the `ProbeSlotGuard` drop backstop. (A status-0
    //!     transport error is NOT a synchronous leak: it is fallbackable, so
    //!     record_failure already clears the slot and re-trips cleanly.)
    use super::*;
    use crate::config::{ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
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
                credential_source: Default::default(),
                header_extras: BTreeMap::new(),
                payload_extras: None,
                user_agent: None,
                allowed_betas: vec![],
                forward_client_headers: vec![],
                context_management: false,
                max_thinking_entry_bytes: None,
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                cloak: routectl_providers::anthropic_api::CloakConfig::default(),
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
        // (`[retry.classes.rate-limited] fallback = false, retry = 0`).
        // do_fallback is false and the 429 is also non-retryable, so the
        // dispatch surfaces verbatim. Under class-based debit the excluded
        // 429 DOES debit (RateLimited is a health class -- accounting is
        // decoupled from routing), but the half-open slot must still be
        // settled exactly once so the breaker is not left locked open.
        // With a zero cooldown the re-trip is immediately half-open-eligible,
        // so the second dispatch must still reach the upstream.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(Probe429Provider {
            id: "p".into(),
            calls: calls.clone(),
        });
        // rate-limited class fallback=false + retry=0: do_fallback=false AND
        // the 429 is non-retryable, so the attempt is neither retried nor
        // fallen back -- it hits the terminal non-fallbackable release. Zero
        // backoff/jitter keep the test instant.
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(0),
                fallback: Some(false),
            },
        );
        let retry = RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            backoff_multiplier: 1.0,
            jitter_ms: 0,
            classes,
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
            retry_after: Some(Duration::from_mins(1)),
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
            retry_after: Some(Duration::from_hours(1)),
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
    /// fast-fails: NO retry, NO fallback, NO breaker debit, NO park.
    #[tokio::test]
    async fn probe_with_retry_after_does_not_park() {
        // Arrange: a probe-shaped request, a large reset that would park a
        // non-probe. Threshold 5 so any stray debit/park is observable.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: Some(Duration::from_mins(1)),
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
            "a probe reset must NOT park the provider",
        );
    }

    /// A reset on a NON-fallbackable error (a 400 whose class is pinned
    /// non-fallbackable) does not force a retry or a park: the error
    /// terminates exactly as today (the reset never changes a
    /// fallback/retry decision).
    #[tokio::test]
    async fn non_fallbackable_error_with_retry_after_still_terminates() {
        // Arrange: a 400 (client error) that is NOT fallbackable, carrying
        // a large reset hint. Threshold 5 so any stray park is observable.
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 400,
            retry_after: Some(Duration::from_mins(1)),
            calls: calls.clone(),
        });
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::BadRequest,
            ClassPolicy {
                retry: Some(0),
                fallback: Some(false),
            },
        );
        let retry = RetryPolicy {
            classes,
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
            "a non-fallbackable error must not park the provider",
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

    // ---- MEE: cancellation-safety of the half-open probe slot ----
    //
    // A half-open probe claims the single probe slot at the gate BEFORE the
    // dispatch awaits the upstream. If that future is DROPPED while awaiting a
    // hung upstream (client disconnect / client-side timeout), none of the
    // synchronous settle arms run; without `ProbeSlotGuard` the slot stays
    // claimed forever and every later probe sees CircuitOpen -- a permanent
    // latch until restart. These tests drop the dispatch future mid-await and
    // assert the slot is freed and the breaker recovers.

    /// Multi-surface provider that hangs (long sleep) on every surface while
    /// `hang` is set, then succeeds once it is cleared. Per-surface call
    /// counters record that a dispatch reached the (hung) upstream.
    struct HangUntilClearedProvider {
        id: String,
        hang: Arc<std::sync::atomic::AtomicBool>,
        complete_calls: Arc<AtomicUsize>,
        stream_calls: Arc<AtomicUsize>,
        count_tokens_calls: Arc<AtomicUsize>,
    }

    impl HangUntilClearedProvider {
        fn new(id: &str, hang: Arc<std::sync::atomic::AtomicBool>) -> Self {
            Self {
                id: id.into(),
                hang,
                complete_calls: Arc::new(AtomicUsize::new(0)),
                stream_calls: Arc::new(AtomicUsize::new(0)),
                count_tokens_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn maybe_hang(&self) {
            if self.hang.load(Ordering::SeqCst) {
                // Far longer than any test timeout: the dispatch future is
                // dropped while parked here, exercising the cancellation path.
                tokio::time::sleep(Duration::from_hours(1)).await;
            }
        }
    }

    #[async_trait]
    impl Provider for HangUntilClearedProvider {
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
            self.complete_calls.fetch_add(1, Ordering::SeqCst);
            self.maybe_hang().await;
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
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            self.maybe_hang().await;
            let chunk = ChatChunk {
                id: format!("ok-{}", self.id),
                ..Default::default()
            };
            Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
            self.maybe_hang().await;
            Ok(TokenCount {
                input_tokens: 7,
                ..Default::default()
            })
        }
    }

    /// Multi-surface provider that always fails with a status-0 transport
    /// error ("never reached the upstream HTTP layer") on every surface, with
    /// per-surface call counters.
    struct Status0Provider {
        id: String,
        complete_calls: Arc<AtomicUsize>,
        stream_calls: Arc<AtomicUsize>,
        count_tokens_calls: Arc<AtomicUsize>,
    }

    impl Status0Provider {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                complete_calls: Arc::new(AtomicUsize::new(0)),
                stream_calls: Arc::new(AtomicUsize::new(0)),
                count_tokens_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Provider for Status0Provider {
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
            Err(Error::upstream(&self.id, 0, "error sending request"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 0, "error sending request"))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 0, "error sending request"))
        }
    }

    /// CircuitPhase the breaker reads at `now` WITHOUT mutating it (unlike
    /// `breaker_open_at`, which claims a probe slot via `try_dispatch`).
    fn circuit_phase(router: &Router) -> crate::runtime_state::CircuitPhase {
        router
            .capacity_snapshot_for("m", Instant::now())
            .expect("per-model state slot exists")
            .circuit
    }

    #[tokio::test]
    async fn complete_half_open_cancelled_probe_releases_slot() {
        let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        // The half-open probe reaches the hung upstream and stalls; drop the
        // dispatch future mid-await via a short timeout.
        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), router.complete(plain_req())).await;
        assert!(
            cancelled.is_err(),
            "the probe must still be awaiting the hung upstream when the timeout fires",
        );
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            1,
            "the probe must have reached the (hung) upstream",
        );
        // Before the guard, the dropped future skipped every settle arm and the
        // half-open slot stayed `true` forever -> permanent CircuitOpen latch.
        assert!(
            !slot_in_flight(&router),
            "a cancelled half-open probe must release the slot",
        );

        // Recovery: clear the hang; the next dispatch is admitted as a fresh
        // probe (a leaked slot would have latched CircuitOpen and skipped it).
        hang.store(false, Ordering::SeqCst);
        let recovered = router.complete(plain_req()).await;
        assert!(
            recovered.is_ok(),
            "breaker must recover: next dispatch admitted + succeeds, got {recovered:?}",
        );
        assert_eq!(
            provider.complete_calls.load(Ordering::SeqCst),
            2,
            "the recovery dispatch must reach the upstream, not a latched breaker",
        );
    }

    #[tokio::test]
    async fn count_tokens_half_open_cancelled_probe_releases_slot() {
        let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), router.count_tokens(plain_req())).await;
        assert!(
            cancelled.is_err(),
            "the count_tokens probe must still be awaiting the hung upstream",
        );
        assert_eq!(provider.count_tokens_calls.load(Ordering::SeqCst), 1);
        assert!(
            !slot_in_flight(&router),
            "a cancelled count_tokens probe must release the slot",
        );

        hang.store(false, Ordering::SeqCst);
        let recovered = router.count_tokens(plain_req()).await;
        assert!(
            recovered.is_ok(),
            "count_tokens must recover after a cancelled probe, got {recovered:?}",
        );
        assert_eq!(
            provider.count_tokens_calls.load(Ordering::SeqCst),
            2,
            "the recovery count_tokens must reach the upstream",
        );
    }

    #[tokio::test]
    async fn stream_half_open_cancelled_probe_releases_slot() {
        let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), router.stream(plain_req())).await;
        assert!(
            cancelled.is_err(),
            "the stream probe must still be awaiting the hung upstream",
        );
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert!(
            !slot_in_flight(&router),
            "a cancelled stream probe must release the slot",
        );

        hang.store(false, Ordering::SeqCst);
        let recovered = router.stream(plain_req()).await;
        assert!(
            recovered.is_ok(),
            "stream must recover after a cancelled probe, got {:?}",
            recovered.as_ref().err(),
        );
        assert_eq!(
            provider.stream_calls.load(Ordering::SeqCst),
            2,
            "the recovery stream must reach the upstream",
        );
    }

    #[tokio::test]
    async fn complete_half_open_status0_retrips_and_recovers() {
        let provider = Arc::new(Status0Provider::new("p"));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        let r1 = router.complete(plain_req()).await;
        assert!(r1.is_err(), "status-0 probe surfaces an error");
        let calls_after_first = provider.complete_calls.load(Ordering::SeqCst);
        assert!(
            calls_after_first >= 1,
            "the probe (and any same-provider retries) reached the upstream",
        );
        assert!(
            !slot_in_flight(&router),
            "a status-0 half-open probe must release the slot (record_failure clears it)",
        );
        // Re-tripped (circuit_opened_at set) yet half-open-ready (slot free,
        // baseline cooldown elapsed) -- recovered, NOT latched Open.
        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "status-0 probe must re-trip cleanly and leave the breaker recoverable",
        );

        // A fresh probe is admitted and reaches the upstream again.
        let _ = router.complete(plain_req()).await;
        assert!(
            provider.complete_calls.load(Ordering::SeqCst) > calls_after_first,
            "the post-cooldown probe must reach the upstream, not a latched breaker",
        );
    }

    #[tokio::test]
    async fn count_tokens_half_open_status0_retrips_and_recovers() {
        let provider = Arc::new(Status0Provider::new("p"));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        let r1 = router.count_tokens(plain_req()).await;
        assert!(r1.is_err());
        assert_eq!(provider.count_tokens_calls.load(Ordering::SeqCst), 1);
        assert!(
            !slot_in_flight(&router),
            "a status-0 count_tokens probe must release the slot",
        );
        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "status-0 count_tokens probe must re-trip cleanly and stay recoverable",
        );

        let _ = router.count_tokens(plain_req()).await;
        assert_eq!(
            provider.count_tokens_calls.load(Ordering::SeqCst),
            2,
            "the post-cooldown count_tokens probe must reach the upstream",
        );
    }

    #[tokio::test]
    async fn stream_half_open_status0_retrips_and_recovers() {
        let provider = Arc::new(Status0Provider::new("p"));
        let router = build_router_with_provider_and_retry(
            provider.clone() as Arc<dyn Provider>,
            RetryPolicy::default(),
        );
        arm_half_open(&router);

        let r1 = router.stream(plain_req()).await;
        assert!(r1.is_err());
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert!(
            !slot_in_flight(&router),
            "a status-0 stream probe must release the slot",
        );
        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "status-0 stream probe must re-trip cleanly and stay recoverable",
        );

        let _ = router.stream(plain_req()).await;
        assert_eq!(
            provider.stream_calls.load(Ordering::SeqCst),
            2,
            "the post-cooldown stream probe must reach the upstream",
        );
    }

    // ---- Class-based breaker debit (accounting decoupled from routing) ----
    //
    // The debit keys off the failure CLASS, not the fallback decision:
    // only the transient-health set (RateLimited, ServerError, Timeout,
    // NetworkError, Overloaded) debits the per-seat breaker. Caller-shaped
    // 4xx, auth, and capability faults fall back but never debit.

    /// A `[retry]` policy with no same-provider retry and no backoff, so
    /// one dispatch equals exactly one outcome (the debit count is not
    /// inflated by in-loop retries and the tests run instantly).
    fn no_retry_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            backoff_multiplier: 1.0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        }
    }

    /// Provider whose `complete` always fails with a canonical
    /// `Error::Streaming` (classifies `NetworkError`, a health class).
    struct AlwaysStreamingErrProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for AlwaysStreamingErrProvider {
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
            Err(Error::Streaming("wire reset before first chunk".into()))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("not exercised by these tests")
        }
    }

    /// Provider whose `stream` always fails pre-first-chunk with a
    /// configurable upstream status, so the stream dispatch loop's error
    /// arm (not the mid-stream wrap) decides the breaker debit.
    struct PreChunkStatusErrProvider {
        id: String,
        status: u16,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for PreChunkStatusErrProvider {
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(
                &self.id,
                self.status,
                "pre-first-chunk failure",
            ))
        }
    }

    #[tokio::test]
    async fn non_retryable_4xx_storm_leaves_breaker_closed() {
        // The intended feature delta on the completion path. A caller-shaped
        // 4xx is not upstream health, so a storm of them must never trip the
        // per-seat breaker. Before the class rewire a fallbackable 4xx
        // debited (do_fallback was true), so threshold+ consecutive 4xx
        // would trip; now class_debits is false across the whole 4xx
        // caller-error row, so the breaker stays closed and the alias stays
        // in rotation.
        for status in [400u16, 404, 422] {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
                id: "p".into(),
                status,
                retry_after: None,
                calls: calls.clone(),
            });
            // Threshold 2, four consecutive 4xx: a health-debiting error
            // would trip twice over.
            let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

            for _ in 0..4 {
                let _ = router.complete(plain_req()).await.unwrap_err();
            }

            assert_eq!(
                circuit_phase(&router),
                crate::runtime_state::CircuitPhase::Closed,
                "status {status}: a non-retryable 4xx storm must leave the breaker CLOSED",
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                4,
                "status {status}: every dispatch must reach the upstream \
                 (alias stays in rotation, never gated by a tripped breaker)",
            );
        }
    }

    #[tokio::test]
    async fn status_health_errors_still_trip_breaker_after_threshold() {
        // The complementary pin: the transient-health status row still
        // debits and trips at threshold, exactly as before the rewire.
        for status in [429u16, 503, 500, 0] {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
                id: "p".into(),
                status,
                retry_after: None,
                calls: calls.clone(),
            });
            let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

            // First health failure is sub-threshold: still closed.
            let _ = router.complete(plain_req()).await.unwrap_err();
            assert_eq!(
                circuit_phase(&router),
                crate::runtime_state::CircuitPhase::Closed,
                "status {status}: one health failure is below threshold 2",
            );
            // Second reaches the threshold: the breaker trips open.
            let _ = router.complete(plain_req()).await.unwrap_err();
            assert!(
                breaker_open_at(&router, Instant::now()),
                "status {status}: a health-error storm must trip the breaker at threshold",
            );
        }
    }

    #[tokio::test]
    async fn streaming_transport_error_still_debits_breaker() {
        // Error::Streaming classifies NetworkError (a health class), so it
        // must debit like a status-0 transport failure.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(AlwaysStreamingErrProvider {
            id: "p".into(),
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

        let _ = router.complete(plain_req()).await.unwrap_err();

        assert!(
            breaker_open_at(&router, Instant::now()),
            "a Streaming transport error (NetworkError class) must debit and trip the breaker",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn completion_501_debits_breaker() {
        // Contrast with the count_tokens capability walk: on the COMPLETION
        // path a wire-501 is a ServerError (health), not a capability
        // signal, so it debits and trips the breaker. Only count_tokens
        // treats a 501 from a capable-by-kind seat as a capability signal.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 501,
            retry_after: None,
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

        let _ = router.complete(plain_req()).await.unwrap_err();

        assert!(
            breaker_open_at(&router, Instant::now()),
            "a completion-path 501 (ServerError class) must debit and trip the breaker",
        );
    }

    #[tokio::test]
    async fn non_fallbackable_429_still_debits_breaker() {
        // Intended delta: health accounting is decoupled from routing. An
        // operator pinning the rate-limited class non-fallbackable
        // (`[retry.classes.rate-limited] fallback = false`) makes
        // do_fallback false -- before the rewire that suppressed the debit.
        // Now the debit keys off the RateLimited class, not the fallback
        // decision, so a non-fallbackable 429 STILL debits the breaker while
        // surfacing verbatim (no fallback, no retry).
        use crate::class_policy::{ClassPolicy, ConfigFailureClass};
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status: 429,
            retry_after: None,
            calls: calls.clone(),
        });
        let mut classes = std::collections::BTreeMap::new();
        classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(0),
                fallback: Some(false),
            },
        );
        let retry = RetryPolicy {
            classes,
            ..no_retry_policy()
        };
        let router = build_router_with_breaker(provider, retry, 1, 60_000);

        let err = router.complete(plain_req()).await.unwrap_err();

        assert!(
            matches!(err, Error::Upstream { status: 429, .. }),
            "a non-fallbackable 429 must surface verbatim (no fallback); got {err:?}",
        );
        assert!(
            breaker_open_at(&router, Instant::now()),
            "a non-fallbackable 429 must STILL debit the breaker (accounting decoupled from routing)",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "non-fallbackable 429 is terminal: one upstream call, no retry, no fallback",
        );
    }

    #[tokio::test]
    async fn stream_non_retryable_4xx_leaves_breaker_closed() {
        // The intended delta on the STREAM dispatch loop: a pre-first-chunk
        // 4xx falls back but must not debit. Exercises the stream error
        // arm's class-gated debit and the debit-skipped fallback release.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(PreChunkStatusErrProvider {
            id: "p".into(),
            status: 400,
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

        for _ in 0..4 {
            router
                .stream(plain_req())
                .await
                .err()
                .expect("a pre-first-chunk 4xx must error");
        }

        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::Closed,
            "a pre-first-chunk 4xx storm must leave the stream-path breaker CLOSED",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "every stream dispatch must reach the upstream (alias stays in rotation)",
        );
    }

    #[tokio::test]
    async fn stream_health_error_still_debits_breaker() {
        // Complement to the 4xx case: a pre-first-chunk 5xx is a health
        // failure and must still debit + trip the stream-path breaker.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(PreChunkStatusErrProvider {
            id: "p".into(),
            status: 503,
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

        router
            .stream(plain_req())
            .await
            .err()
            .expect("a pre-first-chunk 5xx must error");

        assert!(
            breaker_open_at(&router, Instant::now()),
            "a pre-first-chunk 5xx (ServerError class) must debit and trip the breaker",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod auto_emit_cache_control_tests {
    //! T5 dispatch-path auto-emission of a top-level `cache_control`
    //! ephemeral_5m breakpoint. Tests assert on the captured per-attempt
    //! request (the bytes the egress would see), and the original request
    //! is never mutated.
    use super::*;
    use crate::config::{CacheConfig, ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use parking_lot::Mutex as ParkingMutex;
    use routectl_core::cache_control::compute_frozen_floor;
    use routectl_core::{
        CacheControl, ChatChunk, ChatRequest, ChatResponse, Choice, ContentPart, CustomTool,
        KnownContentPart, Message, MessageContent, Provider, Role, SystemBlock, SystemContent,
        ToolDef,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Captures every dispatched request; can fail the first `fail_first`
    /// attempts with a retryable 503 to drive multi-attempt idempotence.
    struct CapturingProvider {
        id: String,
        captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
        fail_first: usize,
        seen: AtomicUsize,
    }

    fn ok_response(model: String) -> ChatResponse {
        ChatResponse {
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
        }
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
            let n = self.seen.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first {
                return Err(Error::upstream(&self.id, 503, "transient"));
            }
            Ok(ok_response(model))
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

    /// Build a router with one provider entry (its KIND drives capability)
    /// and one resolved model that dispatches to a CapturingProvider.
    /// `global_enabled` / `provider_override` exercise the kill-switches;
    /// `fail_first` drives multi-attempt idempotence.
    fn rig(
        entry: ProviderEntry,
        global_enabled: bool,
        fail_first: usize,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let provider_kind = entry.kind_str();
        let mut config = Config {
            cache: CacheConfig {
                auto_emit_top_level_breakpoint: global_enabled,
            },
            // Zero backoff keeps the multi-attempt test fast.
            retry: RetryPolicy {
                initial_backoff_ms: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        config.providers.insert("p".into(), entry);

        let mut router = Router::new(Arc::new(config));
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
            fail_first,
            seen: AtomicUsize::new(0),
        });
        // Mirror `factory::apply_catalog_overlay` (empty overlay, no
        // `[cache_pricing]` overrides): this test rig builds `ResolvedModel`
        // directly instead of through the factory, so it must stamp
        // `effective_row` itself the same way -- `record_would_trim` now
        // reads the precomputed merge off the resolved target rather than
        // re-resolving `(provider_kind, upstream)` at dispatch time.
        let baked = crate::catalog::lookup_baked_with_overrides(
            provider_kind,
            "upstream-model",
            None,
            &BTreeMap::new(),
        );
        let effective_row = crate::catalog::merge(baked.as_ref(), None);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let resolved = ResolvedModel::new("m", "p", provider, "upstream-model")
            .with_effective_row(effective_row);
        models.insert("m".into(), Arc::new(resolved));
        router.install_resolved_models(models);
        (router, captured)
    }

    fn anthropic_entry() -> ProviderEntry {
        ProviderEntry::anthropic_api("literal:k")
    }

    fn anthropic_entry_provider_disabled() -> ProviderEntry {
        ProviderEntry::AnthropicApi {
            api_key_ref: "literal:k".into(),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: Default::default(),
            credential_source: Default::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: Some(false),
            reduction_enabled: None,
            cloak: routectl_providers::anthropic_api::CloakConfig::default(),
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    fn openai_entry() -> ProviderEntry {
        ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k")
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn capable_target_no_breakpoint_gets_one_ephemeral_5m() {
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let req = base_req();
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            up.cache_control,
            Some(CacheControl::ephemeral_5m()),
            "capable target with no caller breakpoint must get exactly one top-level marker",
        );
    }

    #[tokio::test]
    async fn caller_breakpoint_request_is_byte_identical() {
        // Caller already set a top-level cache_control. Auto-emit must
        // defer entirely; the dispatched request must equal the caller's
        // (no second / rewritten marker). The dispatch path rewrites
        // `model` to the upstream id, so normalize that field out before
        // the byte compare -- everything else, including cache_control,
        // must be untouched.
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let mut req = base_req();
        req.cache_control = Some(CacheControl::ephemeral_1h());
        let mut before = req.clone();
        before.model = "upstream-model".into();
        let before_bytes = serde_json::to_vec(&before).expect("serialize before");
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        // Same marker (no second / rewritten one).
        assert_eq!(up.cache_control, Some(CacheControl::ephemeral_1h()));
        let after = serde_json::to_vec(up).expect("serialize after");
        assert_eq!(
            before_bytes, after,
            "caller-supplied request must dispatch byte-identical (modulo upstream model id)",
        );
    }

    #[tokio::test]
    async fn openai_compat_target_gets_no_injection() {
        let (router, captured) = rig(openai_entry(), true, 0);
        router.complete(base_req()).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            up.cache_control, None,
            "openai-compat (no top-level cache_control capability) must not be injected",
        );
    }

    #[tokio::test]
    async fn volatile_high_prefix_blocks_injection() {
        // A UUIDv4 in the system prompt is a high-confidence volatile veto.
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let mut req = base_req();
        req.system = Some(SystemContent::Text(
            "session 550e8400-e29b-41d4-a716-446655440000 active".into(),
        ));
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            up.cache_control, None,
            "high-confidence volatile prefix must veto auto-emit",
        );
    }

    #[tokio::test]
    async fn global_kill_switch_off_blocks_injection() {
        let (router, captured) = rig(anthropic_entry(), false, 0);
        router.complete(base_req()).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            up.cache_control, None,
            "global switch off must block auto-emit"
        );
    }

    #[tokio::test]
    async fn provider_kill_switch_off_blocks_injection() {
        let (router, captured) = rig(anthropic_entry_provider_disabled(), true, 0);
        router.complete(base_req()).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            up.cache_control, None,
            "per-provider switch off must block even with global on",
        );
    }

    #[tokio::test]
    async fn provider_switch_true_with_global_true_injects() {
        // Per-provider Some(true) + global true -> injects.
        let mut entry = anthropic_entry_provider_disabled();
        if let ProviderEntry::AnthropicApi {
            auto_emit_top_level_breakpoint,
            ..
        } = &mut entry
        {
            *auto_emit_top_level_breakpoint = Some(true);
        }
        let (router, captured) = rig(entry, true, 0);
        router.complete(base_req()).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    }

    #[tokio::test]
    async fn cross_dialect_openai_ingress_to_anthropic_target_emits_marker() {
        // OpenAI-ingress-shaped request (no cache_control vocabulary) to an
        // anthropic-api target: the canonical marker is set and the egress
        // would emit it. We assert the canonical marker is present.
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let req = base_req();
        router.complete(req).await.expect("ok");
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    }

    #[tokio::test]
    async fn injection_is_idempotent_across_attempts() {
        // First attempt 503 (retryable), second ok: both attempt bodies on
        // the same target must be byte-identical -- the decision does not
        // drift between retries.
        let (router, captured) = rig(anthropic_entry(), true, 1);
        router.complete(base_req()).await.expect("ok after retry");
        let captured = captured.lock();
        assert_eq!(captured.len(), 2, "expected one failed + one ok attempt");
        let a = serde_json::to_vec(&captured[0]).expect("serialize attempt 0");
        let b = serde_json::to_vec(&captured[1]).expect("serialize attempt 1");
        assert_eq!(a, b, "retried attempt bodies must be byte-identical");
        assert_eq!(
            captured[0].cache_control,
            Some(CacheControl::ephemeral_5m())
        );
    }

    #[tokio::test]
    async fn injection_lands_on_dispatched_clone_not_the_caller_shape() {
        // The original request is moved into complete(), so it cannot be
        // read back. The meaningful invariant is the SPLIT: the dispatched
        // clone carries the injected marker, while the caller-visible
        // request shape (a freshly built identical request) still carries
        // no cache_control. That gap is exactly what proves the injection
        // touched only the per-attempt clone, never the caller's shape.
        // (The helper-level tests pin the "&mut attempt_req only" contract
        // at the unit boundary.)
        let (router, captured) = rig(anthropic_entry(), true, 0);
        router.complete(base_req()).await.expect("ok");
        // The dispatched clone WAS injected.
        assert_eq!(
            captured.lock().first().expect("dispatch").cache_control,
            Some(CacheControl::ephemeral_5m()),
            "the dispatched clone must carry the injected marker",
        );
        // The caller-visible request shape carries no cache_control.
        assert_eq!(
            base_req().cache_control,
            None,
            "the caller-visible request shape must stay un-injected",
        );
    }

    #[tokio::test]
    async fn stream_path_also_injects() {
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let _ = router
            .stream(base_req())
            .await
            .expect("ok")
            .collect::<Vec<_>>()
            .await;
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    }

    // -- steady-state would-trim advisory (NON-MUTATING recording) ---------

    /// A bulky tool_result message (`tokens` tokens at ~4 bytes/token).
    fn tool_result_msg(tokens: usize) -> Message {
        let payload = "x".repeat(tokens * 4);
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: serde_json::json!(payload),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            refusal: None,
            role,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A long tool-heavy request well above the steady-state trigger, with a
    /// head, several bulky old tool turns, and a small recent tail -- so the
    /// trimmer proposes a would-cut candidate.
    fn long_tool_request() -> ChatRequest {
        let mut messages = vec![
            text_msg(Role::User, "system framing turn one"),
            text_msg(Role::Assistant, "acknowledged"),
        ];
        for _ in 0..12 {
            messages.push(text_msg(Role::Assistant, "calling a tool"));
            messages.push(tool_result_msg(12_000));
        }
        for i in 0..6 {
            messages.push(text_msg(Role::User, &format!("recent turn {i}")));
        }
        ChatRequest {
            model: "m".into(),
            messages,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn would_trim_recorded_for_long_request() {
        // A long request with a would-cut candidate records the freed-token
        // count `d` and a finite break-even K* (anthropic catch-all is a
        // verified write-premium row).
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        let plan =
            propose_steady_state_trim(&long_tool_request(), &SteadyStateTrimParams::default())
                .expect("trimmer proposes a cut for this request");
        assert_eq!(
            dispatched.meta.would_trim_tokens,
            Some(plan.candidate.d),
            "would_trim_tokens must equal the candidate's freed-token count",
        );
        assert!(
            dispatched.meta.would_trim_break_even_k.is_some(),
            "a verified write-premium row must yield a finite break-even K*",
        );
    }

    #[tokio::test]
    async fn would_trim_records_nothing_for_short_request() {
        // A short request has no would-cut candidate, so both advisory columns
        // stay None (recorded as NULL).
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(base_req(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.would_trim_tokens, None);
        assert_eq!(dispatched.meta.would_trim_break_even_k, None);
    }

    #[tokio::test]
    async fn would_trim_provider_catch_all_row_prices_normally_via_baked_match() {
        // An openai-compat target with no specific cell resolves to the
        // provider's `"*"` catch-all -- a REAL baked-table match (tier 2), so
        // it prices normally. K* suppression is reserved for a `Disabled` /
        // `Missing` merge result (see
        // `record_would_trim_folds_missing_baked_row_to_no_break_even`).
        let (router, _captured) = rig(openai_entry(), true, 0);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        let plan =
            propose_steady_state_trim(&long_tool_request(), &SteadyStateTrimParams::default())
                .expect("trimmer proposes a cut for this request");
        assert_eq!(
            dispatched.meta.would_trim_tokens,
            Some(plan.candidate.d),
            "the catch-all row must record the freed-token count",
        );
        assert!(
            dispatched.meta.would_trim_break_even_k.is_some(),
            "a baked-matched provider catch-all prices",
        );
    }

    #[tokio::test]
    async fn would_trim_recording_does_not_mutate_outbound_request() {
        // CRITICAL non-mutation invariant: the outbound bytes are identical
        // whether or not the recording helper fired. A long request (helper
        // DOES fire) must dispatch byte-identical to the same request built
        // without the recording path -- the helper never calls
        // apply_trim_plan. Compare the captured outbound clone against a fresh
        // copy with only the dispatch-time field changes the helper does NOT
        // own (model id rewrite + the auto-cache marker), proving the message
        // payloads were untouched.
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        // The helper recorded a candidate (so it definitely ran).
        assert!(
            dispatched.meta.would_trim_tokens.is_some(),
            "the recording helper must have run for this long request",
        );

        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        // The outbound messages are byte-identical to the un-trimmed input:
        // the recording NEVER substituted a placeholder (no apply_trim_plan).
        let sent_messages = serde_json::to_value(&up.messages).expect("serialize sent");
        let original_messages =
            serde_json::to_value(&long_tool_request().messages).expect("serialize original");
        assert_eq!(
            sent_messages, original_messages,
            "would-trim recording must not change the outbound message payloads",
        );
    }

    #[tokio::test]
    async fn would_trim_recorded_on_stream_path_too() {
        // The shared helper is exercised from the streaming path as well as
        // the non-streaming path (mirrors `stream_path_also_injects`).
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .stream_with_options(long_tool_request(), RouterOptions::new())
            .await;
        let _ = dispatched.result.expect("ok").collect::<Vec<_>>().await;
        assert!(
            dispatched.meta.would_trim_tokens.is_some(),
            "the streaming dispatch path must also record the would-trim advisory",
        );
    }

    // -- near-lossless would-trim advisory (dedup / supersession / path) ---

    /// An assistant `tool_use` turn with the given id and JSON input, for
    /// pairing with [`tool_result_of`] (Anthropic-shape tool linkage).
    fn tool_use_of(id: &str, input: serde_json::Value) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: id.into(),
                name: "Tool".into(),
                input,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A user `tool_result` turn linked to `tool_use_id`, carrying JSON
    /// `content`. Pairs with [`tool_use_of`] via the shared id.
    fn tool_result_of(tool_use_id: &str, content: serde_json::Value) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: tool_use_id.into(),
                    content,
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A request built to exercise the near-lossless dedup + supersession
    /// heuristics together: a protected head, an oversized filler TEXT turn
    /// (clears the estimated-token trigger alone; plain text is never a
    /// near-lossless scan unit -- only `ToolResult.content` / `ToolUse.input`
    /// are -- so it cannot pollute the attribution counts), three tool
    /// call/result pairs sharing path "/a" (t1/v1, t2/v2, t3/v1), and a
    /// protected recent tail. Over path "/a" the LATEST result (t3, v1) is
    /// the supersession survivor: t2 (v2) differs from it and is elided as
    /// stale; t1 (v1) equals it and survives supersession, but is then the
    /// FIRST of an exact-duplicate pair, so dedup elides the later copy (t3)
    /// instead. Mirrors context_trim.rs's own
    /// `supersession_takes_precedence_over_dedup_and_each_unit_marked_once`.
    fn near_lossless_attribution_request() -> ChatRequest {
        let v1 = serde_json::json!("V1".repeat(2_000));
        let v2 = serde_json::json!("V2".repeat(2_000));
        let mut messages = vec![
            text_msg(Role::User, "system framing turn one"),
            text_msg(Role::Assistant, "acknowledged"),
            text_msg(Role::User, &"x".repeat(500_000)),
            tool_use_of("t1", serde_json::json!({"file_path": "/a", "call": 1})),
            tool_result_of("t1", v1.clone()),
            tool_use_of("t2", serde_json::json!({"file_path": "/a", "call": 2})),
            tool_result_of("t2", v2),
            tool_use_of("t3", serde_json::json!({"file_path": "/a", "call": 3})),
            tool_result_of("t3", v1),
        ];
        for i in 0..6 {
            messages.push(text_msg(Role::User, &format!("recent turn {i}")));
        }
        ChatRequest {
            model: "m".into(),
            messages,
            ..Default::default()
        }
    }

    /// Variant of [`rig`] that also installs a `[cache_pricing]` override
    /// table, for exercising `would_trim_context_fraction` against a known
    /// (non-`None`) context window.
    fn rig_with_cache_pricing_override(
        entry: ProviderEntry,
        cache_pricing: BTreeMap<String, crate::catalog::CachePricingOverride>,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let provider_kind = entry.kind_str();
        let mut config = Config {
            cache: CacheConfig {
                auto_emit_top_level_breakpoint: true,
            },
            cache_pricing,
            ..Config::default()
        };
        config.providers.insert("p".into(), entry);

        // Mirror `factory::apply_catalog_overlay` (empty overlay, but the
        // SAME `[cache_pricing]` overrides `config` carries) -- see the
        // matching note on `rig` above.
        let baked = crate::catalog::lookup_baked_with_overrides(
            provider_kind,
            "upstream-model",
            None,
            &config.cache_pricing,
        );
        let effective_row = crate::catalog::merge(baked.as_ref(), None);

        let mut router = Router::new(Arc::new(config));
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
            fail_first: 0,
            seen: AtomicUsize::new(0),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let resolved = ResolvedModel::new("m", "p", provider, "upstream-model")
            .with_effective_row(effective_row);
        models.insert("m".into(), Arc::new(resolved));
        router.install_resolved_models(models);
        (router, captured)
    }

    #[tokio::test]
    async fn would_trim_near_lossless_attribution_records_dedup_and_supersession() {
        // A known duplicate result (t3 is a later exact copy of t1) and a
        // known supersession (t2 differs from the survivor t3) over path
        // "/a" must be attributed to the correct heuristic, with the path
        // count-pair reflecting all three results resolving to a path.
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(near_lossless_attribution_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        assert!(
            dispatched
                .meta
                .would_trim_dedup_tokens
                .is_some_and(|t| t > 0),
            "the exact-duplicate result must be attributed to dedup",
        );
        assert!(
            dispatched
                .meta
                .would_trim_supersession_tokens
                .is_some_and(|t| t > 0),
            "the stale differing result must be attributed to supersession",
        );
        assert_eq!(
            dispatched.meta.would_trim_path_units,
            Some(3),
            "all three tool_result units are path-attribution candidates",
        );
        assert_eq!(
            dispatched.meta.would_trim_path_extractable,
            Some(3),
            "all three results resolve to path \"/a\" via their paired tool_use",
        );
        assert!(
            dispatched.meta.would_trim_raw_marks.is_some(),
            "a trigger-clearing request with marks must record the raw-marks blob",
        );
    }

    #[tokio::test]
    async fn near_lossless_pass_does_not_mutate_outbound_request() {
        // CRITICAL non-mutation invariant, exercised against a request whose
        // near-lossless pass definitely finds marks (unlike
        // `would_trim_recording_does_not_mutate_outbound_request`, which only
        // pins the size-baseline plan): outbound bytes must stay
        // byte-identical to the un-elided input. The near-lossless pass is a
        // pure read -- it never calls `apply_trim_plan`.
        let (router, captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(near_lossless_attribution_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        assert!(
            dispatched
                .meta
                .would_trim_dedup_tokens
                .is_some_and(|t| t > 0)
                || dispatched
                    .meta
                    .would_trim_supersession_tokens
                    .is_some_and(|t| t > 0),
            "the near-lossless pass must have found marks for this request",
        );

        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        let sent_messages = serde_json::to_value(&up.messages).expect("serialize sent");
        let original_messages = serde_json::to_value(&near_lossless_attribution_request().messages)
            .expect("serialize original");
        assert_eq!(
            sent_messages, original_messages,
            "the near-lossless pass must not change the outbound message payloads",
        );
    }

    #[tokio::test]
    async fn would_trim_context_fraction_is_none_when_window_unknown() {
        // The anthropic-api "*" catch-all row has no confirmed context
        // window (`max_context_tokens: None`), so `context_fraction` must
        // fail closed to `None` rather than guess.
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.would_trim_context_fraction, None);
    }

    #[tokio::test]
    async fn would_trim_context_fraction_is_some_when_window_known() {
        // An operator override on the context window turns
        // `context_fraction` into a computed `Some(fraction)`.
        let overrides = BTreeMap::from([(
            "anthropic-api:*".to_string(),
            crate::catalog::CachePricingOverride {
                max_context_tokens: Some(1_000_000),
                ..Default::default()
            },
        )]);
        let (router, captured) = rig_with_cache_pricing_override(anthropic_entry(), overrides);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        // Compute the expected fraction against the ACTUAL dispatched clone
        // (post overlay/reduction/auto-cache mutation), since those run
        // before the advisory records and change the serialized byte count.
        let up = captured.lock().first().cloned().expect("one dispatch");
        let expected_fraction = estimate_total_tokens(&up) as f64 / 1_000_000.0;
        assert_eq!(
            dispatched.meta.would_trim_context_fraction,
            Some(expected_fraction),
        );
    }

    #[tokio::test]
    async fn would_trim_recorder_version_stamped_when_trigger_clears() {
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(
            dispatched.meta.would_trim_recorder_version,
            Some(NEAR_LOSSLESS_RECORDER_VERSION),
            "a trigger-clearing row must be stamped with the recorder version",
        );
    }

    #[tokio::test]
    async fn would_trim_recorder_version_is_none_below_trigger() {
        let (router, _captured) = rig(anthropic_entry(), true, 0);
        let dispatched = router
            .complete_with_options(base_req(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(
            dispatched.meta.would_trim_recorder_version, None,
            "a below-trigger row must not be stamped (the pass never ran)",
        );
    }

    /// Two-entry fallback chain where the two targets make OPPOSITE
    /// injection decisions: target 1 (openai-compat, no capability) always
    /// fails and injects nothing; target 2 (anthropic-api, capable) serves
    /// the request and gets exactly one top-level marker. The marker on
    /// target 2 must derive ONLY from the original request, never
    /// accumulating from target 1's attempt -- the per-target clone is
    /// rebuilt from `req` each hop, so target 2's bytes equal a freshly
    /// injected original.
    #[tokio::test]
    async fn fallback_targets_decide_independently_without_accumulation() {
        let cap_a: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let cap_b: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));

        let mut config = Config {
            retry: RetryPolicy {
                initial_backoff_ms: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        config.providers.insert(
            "p-compat".into(),
            ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k"),
        );
        config.providers.insert(
            "p-anthropic".into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        config.aliases.insert(
            "alias".into(),
            AliasValue::Chain(vec!["m-compat".into(), "m-anthropic".into()]),
        );

        // Target 1 always fails (large fail_first) so dispatch falls back
        // to target 2. openai-compat capability is false -> no injection.
        let prov_a: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "p-compat".into(),
            captured: cap_a.clone(),
            fail_first: usize::MAX,
            seen: AtomicUsize::new(0),
        });
        // Target 2 serves the request; anthropic-api is capable -> inject.
        let prov_b: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "p-anthropic".into(),
            captured: cap_b.clone(),
            fail_first: 0,
            seen: AtomicUsize::new(0),
        });

        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m-compat".into(),
            Arc::new(ResolvedModel::new(
                "m-compat",
                "p-compat",
                prov_a,
                "upstream-compat",
            )),
        );
        models.insert(
            "m-anthropic".into(),
            Arc::new(ResolvedModel::new(
                "m-anthropic",
                "p-anthropic",
                prov_b,
                "upstream-anthropic",
            )),
        );

        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(models);

        let req = ChatRequest {
            model: "alias".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        router.complete(req).await.expect("falls back and serves");

        // Target 1: incapable -> dispatched with no injected marker.
        let a = cap_a.lock();
        let up_a = a.first().expect("target 1 dispatched");
        assert_eq!(
            up_a.cache_control, None,
            "incapable openai-compat target must receive no auto-emitted marker",
        );

        // Target 2: capable -> exactly one top-level ephemeral_5m marker,
        // derived only from the original request (not accumulated).
        let b = cap_b.lock();
        let up_b = b.first().expect("target 2 dispatched");
        assert_eq!(
            up_b.cache_control,
            Some(CacheControl::ephemeral_5m()),
            "capable target must get exactly one top-level marker",
        );

        // Non-accumulation: target 2's bytes equal an independently
        // injected copy of the ORIGINAL request (model normalized to the
        // upstream id), proving the clone was rebuilt from `req`, not
        // carried over from target 1's attempt.
        let mut expected = ChatRequest {
            model: "upstream-anthropic".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        expected.cache_control = Some(CacheControl::ephemeral_5m());
        assert_eq!(
            serde_json::to_vec(up_b).expect("serialize target 2"),
            serde_json::to_vec(&expected).expect("serialize expected"),
            "target 2 bytes must derive only from the original request",
        );
    }

    // ---- helper-level unit tests (direct gate-predicate coverage) ----

    fn plan(
        caller_breakpoints: usize,
        volatile_high: bool,
        global: bool,
        req: &ChatRequest,
    ) -> AutoCacheRequestPlan {
        // Build off the real req for the floor, then override the snapshot
        // fields to construct gate situations precisely.
        let mut p = AutoCacheRequestPlan::build(req, global);
        p.has_caller_breakpoints = caller_breakpoints > 0;
        p.caller_breakpoint_count = caller_breakpoints;
        p.volatile_high_veto = volatile_high;
        p
    }

    #[test]
    fn helper_emits_on_clean_capable_request() {
        let mut req = base_req();
        let p = plan(0, false, true, &req);
        let cap = Some(CacheCapability::new(true, true));
        let out = maybe_apply_auto_cache_control(&mut req, &p, cap, true);
        assert_eq!(out, CacheInjection::Emitted);
        assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));
    }

    #[test]
    fn helper_fails_closed_on_unknown_capability() {
        let mut req = base_req();
        let p = plan(0, false, true, &req);
        let out = maybe_apply_auto_cache_control(&mut req, &p, None, true);
        assert_eq!(out, CacheInjection::SkippedNoCapability);
        assert_eq!(req.cache_control, None);
    }

    #[test]
    fn helper_rolls_back_when_validation_fails() {
        // Craft a situation the black-box gate cannot reach: the plan says
        // "no caller breakpoints" (so the gate proceeds past the
        // SkippedCallerSupplied / cap checks), but the actual attempt_req
        // already carries MAX_BREAKPOINTS caller markers. Injecting the
        // top-level marker pushes the total to MAX+1, so post-injection
        // validate_source fails and the helper restores the original
        // (absent top-level marker).
        let mut req = base_req();
        req.tools = Some(vec![ToolDef::Custom(CustomTool {
            name: "t".into(),
            description: Some("d".into()),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_5m()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]);
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "s".into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            citations: None,
        }]));
        let part = |t: &str| {
            ContentPart::Known(KnownContentPart::Text {
                text: t.into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_5m()),
            })
        };
        req.messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![part("a"), part("b")]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        // Sanity: the request already sits at MAX_BREAKPOINTS (1 tool + 1
        // system + 2 message parts).
        assert_eq!(
            compute_frozen_floor(&req).caller_breakpoint_count(),
            MAX_BREAKPOINTS,
        );
        // Force the plan to claim no caller breakpoints so the gate
        // proceeds to the validate step -- the only path to the rollback
        // branch given the production no-caller gate.
        let p = plan(0, false, true, &req);
        let cap = Some(CacheCapability::new(true, true));
        let out = maybe_apply_auto_cache_control(&mut req, &p, cap, true);
        assert_eq!(out, CacheInjection::ValidationRolledBack);
        assert_eq!(
            req.cache_control, None,
            "rollback must restore the original (absent) top-level marker",
        );
    }

    #[test]
    fn helper_caller_supplied_dominates_all_per_target_skip_reasons() {
        // Arrange: a request that already carries caller breakpoints. This is
        // a request-level fact, so it must dominate every per-target / config
        // skip reason regardless of capability or kill-switch state.
        let mut req = base_req();
        let p = plan(1, false, true, &req);

        // Act + Assert: capability unknown (None) -> caller_supplied, NOT
        // no_capability (the key precedence change).
        let out = maybe_apply_auto_cache_control(&mut req, &p, None, true);
        assert_eq!(out, CacheInjection::SkippedCallerSupplied);
        assert_eq!(
            req.cache_control, None,
            "caller_supplied path must leave attempt_req.cache_control untouched",
        );

        // Global kill-switch off -> caller still dominates.
        let p_global_off = plan(1, false, false, &req);
        let cap = Some(CacheCapability::new(true, true));
        let out = maybe_apply_auto_cache_control(&mut req, &p_global_off, cap, true);
        assert_eq!(out, CacheInjection::SkippedCallerSupplied);
        assert_eq!(req.cache_control, None);

        // Per-provider kill-switch off -> caller still dominates.
        let out = maybe_apply_auto_cache_control(&mut req, &p, cap, false);
        assert_eq!(out, CacheInjection::SkippedCallerSupplied);
        assert_eq!(req.cache_control, None);

        // Volatile-high veto must not override caller_supplied either.
        let p_volatile = plan(1, true, true, &req);
        let out = maybe_apply_auto_cache_control(&mut req, &p_volatile, cap, true);
        assert_eq!(out, CacheInjection::SkippedCallerSupplied);
        assert_eq!(req.cache_control, None);
    }

    #[test]
    fn strategy_str_maps_every_variant_to_stable_token() {
        // Operator-facing contract: these tokens are recorded in the usage
        // DB and matched by the thrash predicate. Pin them exactly.
        assert_eq!(CacheInjection::Emitted.strategy_str(), "auto_emitted");
        assert_eq!(
            CacheInjection::SkippedCallerSupplied.strategy_str(),
            "caller_supplied",
        );
        assert_eq!(
            CacheInjection::SkippedVolatileHigh.strategy_str(),
            "volatile_vetoed",
        );
        assert_eq!(
            CacheInjection::SkippedGlobalDisabled.strategy_str(),
            "auto_skipped:global_disabled",
        );
        assert_eq!(
            CacheInjection::SkippedProviderDisabled.strategy_str(),
            "auto_skipped:provider_disabled",
        );
        assert_eq!(
            CacheInjection::SkippedNoCapability.strategy_str(),
            "auto_skipped:no_capability",
        );
        assert_eq!(
            CacheInjection::SkippedBreakpointCap.strategy_str(),
            "auto_skipped:breakpoint_cap",
        );
        assert_eq!(
            CacheInjection::ValidationRolledBack.strategy_str(),
            "auto_skipped:validation_rolled_back",
        );
    }

    // -----------------------------------------------------------------
    // Overlay end-to-end: a REAL on-disk overlay round-trip, merged via
    // the REAL `factory::apply_catalog_overlay` (not a hand-built
    // `EffectiveRow`), dispatched through the REAL Router -- proving the
    // seam this module's `record_would_trim` reads (`ResolvedModel::
    // effective_row`) actually changes behavior when a loader-shaped
    // overlay is present, not just that the pure `merge` unit resolves
    // correctly in isolation.
    // -----------------------------------------------------------------

    /// Save `cells` to a tempfile then load it straight back, so the
    /// returned `CatalogOverlay` came from a REAL disk round-trip (the
    /// same `catalog_overlay::save` / `load` pair the config loader and
    /// the (future) migrator use), not an in-memory struct literal.
    fn overlay_from_disk(
        cells: BTreeMap<String, Option<crate::catalog_overlay::OverlayCell>>,
    ) -> crate::catalog_overlay::CatalogOverlay {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog_overlay.json");
        crate::catalog_overlay::save(&path, 0, cells).expect("save");
        crate::catalog_overlay::load(&path).expect("load")
    }

    /// Build a router exactly like `rig`, except the resolved model's
    /// `effective_row` is stamped by the REAL `factory::apply_catalog_overlay`
    /// post-pass (the same call `build_router_from_config_with_overlay`
    /// makes) instead of a hand-rolled merge -- so `overlay` must carry
    /// through `[models.m]` / `[providers.p]` resolution exactly as
    /// production config would.
    fn rig_with_overlay(
        entry: ProviderEntry,
        overlay: crate::catalog_overlay::CatalogOverlay,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let mut config = Config {
            cache: CacheConfig {
                auto_emit_top_level_breakpoint: true,
            },
            retry: RetryPolicy {
                initial_backoff_ms: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        config.providers.insert("p".into(), entry);
        config.models.insert(
            "m".into(),
            crate::config::ModelEntry::new("p", "upstream-model"),
        );
        let config = Arc::new(config);

        let mut router = Router::new(config.clone());
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
            fail_first: 0,
            seen: AtomicUsize::new(0),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m".into(),
            Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
        );
        let models = crate::factory::apply_catalog_overlay(models, &config, &overlay);
        router.install_resolved_models(models);
        (router, captured)
    }

    #[tokio::test]
    async fn overlay_override_through_real_load_path_changes_would_trim_pricing() {
        // Arrange: baseline (no overlay) vs. an overlay cell overriding the
        // anthropic-api catch-all's `wm`, round-tripped through disk.
        let (baseline_router, _c) = rig(anthropic_entry(), true, 0);
        let baseline = baseline_router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        baseline.result.expect("ok");
        let baseline_k = baseline
            .meta
            .would_trim_break_even_k
            .expect("baseline (baked catch-all) must price");

        let mut cells = BTreeMap::new();
        cells.insert(
            "anthropic-api:*".to_string(),
            Some(crate::catalog_overlay::OverlayCell {
                source: crate::catalog_overlay::OverlaySource::User,
                verified_at: "2026-07-01".to_string(),
                wm: Some(9.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        let overlay = overlay_from_disk(cells);

        // Act: dispatch the IDENTICAL request through the overlay-stamped
        // router.
        let (router, _captured) = rig_with_overlay(anthropic_entry(), overlay);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");
        let overridden_k = dispatched
            .meta
            .would_trim_break_even_k
            .expect("overlay-priced target must still price");

        // Assert: the overlay's wm actually moved the priced outcome --
        // this fails if `record_would_trim` ever falls back to the baked
        // row instead of reading `ResolvedModel::effective_row`.
        assert_ne!(
            baseline_k, overridden_k,
            "an overlay cell overriding a baked field must change the priced break-even K* \
             through the real load -> merge -> dispatch path",
        );
    }

    #[tokio::test]
    async fn overlay_null_disable_through_real_load_path_folds_to_conservative_sentinel() {
        // Arrange: a null overlay cell (JSON `null`, round-tripped through
        // disk) for the same selector the baseline test prices normally.
        let mut cells = BTreeMap::new();
        cells.insert("anthropic-api:*".to_string(), None);
        let overlay = overlay_from_disk(cells);

        // Act
        let (router, _captured) = rig_with_overlay(anthropic_entry(), overlay);
        let dispatched = router
            .complete_with_options(long_tool_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        // Assert: disabled folds to the SAME conservative sentinel as a
        // catalog miss -- no break-even K -- while the freed-token count
        // (independent of pricing trust) still records.
        assert_eq!(
            dispatched.meta.would_trim_break_even_k, None,
            "a null-disabled overlay cell must fold to the conservative sentinel \
             through the real load -> merge -> dispatch path",
        );
        assert!(
            dispatched.meta.would_trim_tokens.is_some(),
            "the freed-token count records regardless of pricing trust",
        );
    }
}

#[cfg(test)]
mod context_reduction_dispatch_tests {
    //! Context-reduction wiring on the dispatch path. Asserts the
    //! ordering invariant (reduce AFTER overlays, BEFORE auto-cache), the
    //! effective-enablement resolution (global AND provider-not-off), and the
    //! stable `reduction_strategy` token stamped on `DispatchMeta`. Tests
    //! read the captured per-attempt request (the bytes the egress would see)
    //! and the returned meta; the original request is never mutated.
    use super::*;
    use crate::config::{CacheConfig, ProviderEntry, ProviderRuntimePolicy, ReductionConfig};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use parking_lot::Mutex as ParkingMutex;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, ContentPart, KnownContentPart, Message,
        MessageContent, Provider, Role,
    };
    use std::collections::BTreeMap;

    struct CapturingProvider {
        id: String,
        captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
    }

    fn ok_response(model: String) -> ChatResponse {
        ChatResponse {
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
        }
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
            Ok(ok_response(model))
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

    /// Build a router with one provider entry and one resolved model that
    /// dispatches to a CapturingProvider. `reduction_enabled` is the global
    /// `[reduction] enabled`; `auto_cache` is the global auto-emit switch.
    fn rig(
        entry: ProviderEntry,
        reduction_enabled: bool,
        auto_cache: bool,
    ) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
        let mut config = Config {
            cache: CacheConfig {
                auto_emit_top_level_breakpoint: auto_cache,
            },
            reduction: ReductionConfig {
                enabled: reduction_enabled,
            },
            retry: RetryPolicy {
                initial_backoff_ms: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        config.providers.insert("p".into(), entry);

        let mut router = Router::new(Arc::new(config));
        let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
            id: "cap".into(),
            captured: captured.clone(),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let resolved = ResolvedModel::new("m", "p", provider, "upstream-model");
        models.insert("m".into(), Arc::new(resolved));
        router.install_resolved_models(models);
        (router, captured)
    }

    fn anthropic_entry() -> ProviderEntry {
        ProviderEntry::anthropic_api("literal:k")
    }

    /// Anthropic entry with `reduction_enabled = Some(false)` (provider opt-out).
    fn anthropic_entry_reduction_off() -> ProviderEntry {
        ProviderEntry::AnthropicApi {
            api_key_ref: "literal:k".into(),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: Default::default(),
            credential_source: Default::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: Some(false),
            cloak: routectl_providers::anthropic_api::CloakConfig::default(),
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// A request whose single mutable-tail message carries a tool_result
    /// whose content is a pretty (whitespace-laden) JSON STRING.
    fn req_with_pretty_tool_result() -> ChatRequest {
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: serde_json::json!(pretty),
                        is_error: None,
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    /// Read the tool_result content string out of the first message's parts.
    fn first_tool_result_content(req: &ChatRequest) -> &serde_json::Value {
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        content
    }

    #[tokio::test]
    async fn disabled_by_default_dispatches_unchanged() {
        // Global default off -> apply_json_minify is NOT called; the pretty
        // tool_result string survives verbatim and meta reflects disabled.
        let (router, captured) = rig(anthropic_entry(), false, false);
        let dispatched = router
            .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:disabled"));
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
            "disabled reduction must leave the pretty JSON string untouched",
        );
    }

    #[tokio::test]
    async fn enabled_globally_compacts_mutable_tail_json() {
        // Global on, provider inherits -> the pretty tool_result string is
        // compacted and meta reports the applied token.
        let (router, captured) = rig(anthropic_entry(), true, false);
        let dispatched = router
            .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.reduction_strategy, Some("applied"));
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\"rows\":[1,2,3]}"),
            "enabled reduction must compact the JSON string in the mutable tail",
        );
    }

    #[tokio::test]
    async fn caller_top_level_cache_control_blocks_reduction() {
        // Cache-safety guard at the dispatch boundary: a CALLER top-level
        // cache_control selects Anthropic automatic caching, which freezes the
        // entire prefix. Even with reduction enabled, the tool_result must NOT
        // be compacted -- the dispatched bytes stay verbatim and meta reports
        // no mutable tail.
        let mut req = req_with_pretty_tool_result();
        req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
        let (router, captured) = rig(anthropic_entry(), true, false);
        let dispatched = router
            .complete_with_options(req, RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:no-tail"));
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
            "a caller top-level breakpoint must freeze the prefix; reduction must not run",
        );
    }

    #[tokio::test]
    async fn provider_override_off_skips_even_with_global_on() {
        // Global on but provider reduction_enabled = Some(false) -> skipped;
        // the pretty string is untouched.
        let (router, captured) = rig(anthropic_entry_reduction_off(), true, false);
        let dispatched = router
            .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:disabled"));
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
            "provider opt-out must block reduction even with global on",
        );
    }

    #[tokio::test]
    async fn reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes() {
        // ORDERING regression: no caller breakpoint, reduction enabled AND
        // auto-emit enabled + capable target. After dispatch the JSON string
        // is compacted AND a top-level cache_control breakpoint is present --
        // proving reduction ran before auto-cache (the auto-emitted
        // breakpoint covers the reduced bytes).
        let (router, captured) = rig(anthropic_entry(), true, true);
        let dispatched = router
            .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
            .await;
        dispatched.result.expect("ok");
        assert_eq!(dispatched.meta.reduction_strategy, Some("applied"));
        assert_eq!(dispatched.meta.cache_strategy, Some("auto_emitted"));
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\"rows\":[1,2,3]}"),
            "the dispatched bytes must be the REDUCED string",
        );
        assert_eq!(
            up.cache_control,
            Some(CacheControl::ephemeral_5m()),
            "a top-level breakpoint must be auto-emitted over the reduced request",
        );
    }

    #[tokio::test]
    async fn stream_path_also_reduces() {
        let (router, captured) = rig(anthropic_entry(), true, false);
        let _ = router
            .stream(req_with_pretty_tool_result())
            .await
            .expect("ok")
            .collect::<Vec<_>>()
            .await;
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\"rows\":[1,2,3]}"),
            "stream path must apply reduction like the complete path",
        );
    }

    #[tokio::test]
    async fn stream_reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes() {
        // ORDERING regression, streaming analogue of the completion-path
        // `reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes`:
        // no caller breakpoint, reduction enabled AND auto-emit enabled +
        // capable target. After a stream dispatch the captured request's
        // tool_result JSON is compacted AND a top-level cache_control
        // breakpoint is present -- proving reduction ran BEFORE auto-cache on
        // the dominant streaming path. `stream_path_also_reduces` runs with
        // auto_cache OFF, so the interaction is never exercised there: a
        // reorder of the two blocks in `stream_inner` would disable reduction
        // on every auto-breakpoint stream and pass every other test.
        let (router, captured) = rig(anthropic_entry(), true, true);
        let _ = router
            .stream(req_with_pretty_tool_result())
            .await
            .expect("ok")
            .collect::<Vec<_>>()
            .await;
        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            first_tool_result_content(up),
            &serde_json::json!("{\"rows\":[1,2,3]}"),
            "the dispatched bytes must be the REDUCED string",
        );
        assert_eq!(
            up.cache_control,
            Some(CacheControl::ephemeral_5m()),
            "a top-level breakpoint must be auto-emitted over the reduced request on the stream path",
        );
    }

    #[test]
    fn strategy_token_maps_every_case_to_stable_string() {
        // Operator-facing contract: pin these tokens exactly.
        assert_eq!(reduction_strategy_token(false, None), "skipped:disabled");
        // Obtain a real `Applied` outcome (the delta type is non-exhaustive
        // and cannot be hand-constructed) by minifying a pretty JSON string.
        let applied = apply_json_minify(&mut req_with_pretty_tool_result());
        assert!(matches!(applied, ReductionOutcome::Applied(_)));
        assert_eq!(reduction_strategy_token(true, Some(&applied)), "applied");
        assert_eq!(
            reduction_strategy_token(true, Some(&ReductionOutcome::NoMutableTail)),
            "skipped:no-tail",
        );
        assert_eq!(
            reduction_strategy_token(true, Some(&ReductionOutcome::NothingToStrip)),
            "skipped:nothing-to-strip",
        );
    }
}

#[cfg(test)]
mod forwarded_coexistence_tests {
    //! The per-provider passthrough model dissolves the earlier whole-chain
    //! forwarded-passthrough gate (`enforce_forwarded_anthropic_target` /
    //! `target_is_anthropic_egress`
    //! / `FORWARDED_EGRESS_KIND`, deleted): `credential_source = "forwarded"`
    //! is now a PER-PROVIDER config, not a request-global mode switch, so
    //! an alias routes exactly like any other -- no whole-chain refusal, no
    //! steering. These tests cover what replaces it:
    //!
    //! - A request carrying a captured forwarded bearer no longer bends
    //!   routing: an alias to an OWN-credential provider (any kind, mixed
    //!   chain or not) dispatches with that provider's own credentials,
    //!   and is never refused up front.
    //! - A forwarded-CREDENTIAL target (an `anthropic-api` provider with
    //!   `credential_source = "forwarded"`) with NO captured bearer
    //!   refuses cleanly BEFORE egress -- the compensating guard paired
    //!   with the gate deletion -- and the guard is per-target, so a
    //!   chain that never reaches that target is unaffected.
    //!
    //! The broader "still refreshes and falls back" coexistence
    //! regression -- a MITM-marked request routed to an OWN-credential
    //! Anthropic provider behaves exactly as before the change, and the
    //! floating bearer
    //! is never consumed by it -- lives in `forwarded_auth_terminal_tests`,
    //! next to the terminal-bypass mocks it reuses.
    use super::*;
    use crate::config::{CredentialSource, ProviderEntry, ProviderRuntimePolicy};
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::{BoxStream, StreamExt};
    use routectl_core::schema::ForwardedBearer;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The forwarded token used across tests. Distinctive so any leak
    /// into a log field, log message, or client error is unmistakable.
    const FORWARDED_TOKEN: &str = "sk-ant-oat01-FORWARDED-SECRET-must-never-surface";

    /// Mock provider that records every dispatch call so a test can prove
    /// a target was (or was NOT) reached. Every method returns a benign
    /// success, so the ONLY reason a call count stays zero is that the
    /// router refused BEFORE dispatch.
    struct RecordingProvider {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Ok(ChatResponse::default())
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse::default())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(futures::stream::once(async move { Ok(ChatChunk::default()) }).boxed())
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TokenCount::default())
        }
    }

    /// An `anthropic-api` provider entry on the default (api.anthropic.com)
    /// host, with `credential_source` set per `forwarded`.
    fn anthropic_entry(forwarded: bool) -> ProviderEntry {
        let entry = ProviderEntry::anthropic_api("literal:k");
        if forwarded {
            entry.with_credential_source(CredentialSource::Forwarded)
        } else {
            entry
        }
    }

    /// A non-`anthropic-api`, OWN-credential provider entry.
    fn openai_compat_entry() -> ProviderEntry {
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// One leg of a test chain: config entry + matching recording mock.
    struct Leg {
        nickname: &'static str,
        provider_name: &'static str,
        entry: ProviderEntry,
    }

    /// Build a router whose alias `"alias"` resolves to `legs` in order.
    /// Returns the router and the per-leg dispatch-call counters (in leg
    /// order) so a test can prove which target was reached.
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
            let provider: Arc<dyn Provider> = Arc::new(RecordingProvider {
                id: leg.provider_name.to_string(),
                calls,
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

    fn plain_req() -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            ..Default::default()
        }
    }

    fn forwarded_req() -> ChatRequest {
        let mut req = plain_req();
        req.routectl_internal.forwarded_bearer =
            Some(ForwardedBearer::new(FORWARDED_TOKEN.to_string()));
        req
    }

    // ---- coexistence: a captured bearer no longer steers routing ----

    #[tokio::test]
    async fn complete_forwarded_bearer_present_routes_to_own_compat_normally() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "compat",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
        }]);

        router
            .complete(forwarded_req())
            .await
            .expect("an OWN-credential target dispatches regardless of a captured bearer");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn complete_forwarded_bearer_present_mixed_chain_own_first_entry_dispatches_normally() {
        // Chain [own-anthropic, openai-compat], request carries a
        // captured bearer. The earlier whole-chain gate would have refused
        // this up front because entry 1 is non-Anthropic; routing is now
        // purely by
        // alias -- the first entry succeeds and the second is never
        // reached.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic",
                provider_name: "anthropic-prov",
                entry: anthropic_entry(false),
            },
            Leg {
                nickname: "compat",
                provider_name: "compat-prov",
                entry: openai_compat_entry(),
            },
        ]);

        router
            .complete(forwarded_req())
            .await
            .expect("a mixed OWN-credential chain is never refused up front");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "first entry succeeds, so the second is never reached",
        );
    }

    #[tokio::test]
    async fn count_tokens_forwarded_bearer_present_routes_to_own_compat_capability_error() {
        // openai-compat is not count_tokens-capable by kind: without any
        // forwarded gate, the walk simply reports NotImplemented, same as
        // a plain request -- a captured bearer must not change this.
        let (router, counters) = build_router(vec![Leg {
            nickname: "compat",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
        }]);

        let err = router.count_tokens(forwarded_req()).await.unwrap_err();

        assert!(matches!(err, Error::NotImplemented(..)), "got {err:?}");
        assert_eq!(counters[0].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stream_forwarded_bearer_present_routes_to_own_compat_normally() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "compat",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
        }]);

        let _stream = router
            .stream(forwarded_req())
            .await
            .expect("an OWN-credential target dispatches regardless of a captured bearer");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    }

    // ---- missing-bearer terminal guard ----
    //
    // A forwarded-CREDENTIAL target (provider `credential_source =
    // "forwarded"`) with NO captured bearer must refuse cleanly BEFORE
    // egress -- never an ambiguous upstream 401 -- in all three dispatch
    // paths. The guard is per-target: it fires only for the target about
    // to be dispatched to.

    #[tokio::test]
    async fn complete_forwarded_target_missing_bearer_refused_before_dispatch() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        }]);

        let err = router.complete(plain_req()).await.unwrap_err();

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("missing_forwarded_bearer"),
            "refuse message must carry the reason; got: {err}",
        );
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            0,
            "a forwarded target with no captured bearer must never be dispatched to",
        );
    }

    #[tokio::test]
    async fn count_tokens_forwarded_target_missing_bearer_refused_before_dispatch() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        }]);

        let err = router.count_tokens(plain_req()).await.unwrap_err();

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("missing_forwarded_bearer"));
        assert_eq!(counters[0].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stream_forwarded_target_missing_bearer_refused_before_dispatch() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        }]);

        let err = router.stream(plain_req()).await.err().expect("refused");

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("missing_forwarded_bearer"));
        assert_eq!(counters[0].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn complete_forwarded_target_with_captured_bearer_dispatches_normally() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        }]);

        router
            .complete(forwarded_req())
            .await
            .expect("a forwarded target with a captured bearer must dispatch");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn complete_mixed_chain_own_first_entry_succeeds_forwarded_missing_bearer_never_reached()
    {
        // Per-target guard: a chain whose first (OWN-credential) entry
        // succeeds never reaches the forwarded second entry, so the
        // missing-bearer guard never fires even though the request has
        // no captured bearer at all.
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "own",
                provider_name: "own-prov",
                entry: anthropic_entry(false),
            },
            Leg {
                nickname: "fwd",
                provider_name: "fwd-prov",
                entry: anthropic_entry(true),
            },
        ]);

        router
            .complete(plain_req())
            .await
            .expect("the first entry succeeds without ever touching the forwarded target");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "the forwarded second entry must never be reached",
        );
    }

    #[tokio::test]
    async fn missing_bearer_refuse_client_error_carries_no_token() {
        // The client never captured a bearer in this scenario, so there
        // is nothing to leak -- but pin that the refuse message stays
        // generic (reason only) and never echoes request content.
        let (router, _counters) = build_router(vec![Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        }]);

        let err = router.complete(plain_req()).await.unwrap_err();
        let client_msg = err.to_string();

        assert!(!client_msg.contains(FORWARDED_TOKEN));
    }

    // ---- real-factory build integration ----
    //
    // Every test above wires a `RecordingProvider` mock straight onto a
    // `ResolvedModel`, bypassing `crate::factory::build_provider`
    // entirely. That is exactly why a factory-side bug (unconditionally
    // resolving a token from a forwarded entry's guaranteed-empty
    // `api_key_ref`) could break `serve` while every dispatch-behavior
    // test here kept passing. This test drives the REAL factory build
    // for the forwarded leg so the two layers are exercised together.

    #[tokio::test]
    async fn forwarded_target_built_via_real_factory_still_refuses_missing_bearer() {
        let entry = anthropic_entry(true);
        let secrets: Arc<dyn routectl_auth::SecretStore> =
            Arc::new(routectl_auth::MemoryStore::new());
        let provider = crate::factory::build_provider("fwd-prov", &entry, secrets)
            .await
            .expect("a valid forwarded provider entry must build");

        let mut config = Config::default();
        config.providers.insert("fwd-prov".into(), entry);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "fwd".into(),
            Arc::new(ResolvedModel::new(
                "fwd",
                "fwd-prov",
                provider,
                "upstream-fwd".to_string(),
            )),
        );
        config
            .aliases
            .insert("alias".into(), AliasValue::Chain(vec!["fwd".into()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(models);

        // No captured bearer: the router's missing-bearer guard must
        // refuse cleanly before ever calling into the real provider
        // (which would otherwise try to egress with no credential).
        let err = router.complete(plain_req()).await.unwrap_err();

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("missing_forwarded_bearer"),
            "got: {err}",
        );
    }
}

#[cfg(test)]
mod k_query_key_tests {
    //! Regression guard for the K-estimator sample write / query key mismatch.
    //!
    //! `record_k_sample` writes each per-session K window under the SERVED
    //! model nickname (`meta.served_model` == `target.nickname`, via
    //! `observe_meta`). The would-trim query in [`Router::record_would_trim`]
    //! must therefore key its [`crate::k_estimator::KQuery`] on the SAME served
    //! nickname -- not the upstream wire id -- or the query never matches the
    //! store, the estimate stays `Cold`, and `would_trim_k_floor` is recorded
    //! permanently `None` even for a heavily-calibrated session.
    use super::*;
    use routectl_core::content_part::{ContentPart, KnownContentPart};
    use routectl_core::schema::{Message, MessageContent, Role};
    use serde_json::json;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const SESSION: &str = "sess-k-regression";
    const PROVIDER_KIND: &str = "anthropic-api";
    /// Served model nickname -- the label `record_k_sample` writes under and
    /// the query must match.
    const SERVED_MODEL: &str = "pt-opus-4-8";
    /// Upstream wire id -- what the buggy query keyed on. Distinct from the
    /// nickname AND a VERIFIED pricing cell, so pricing (break_even) resolves
    /// on it exactly as it does in production.
    const UPSTREAM: &str = "claude-opus-4-8";

    /// Build the `EffectiveRow` `record_would_trim` now expects to be handed
    /// (mirroring `factory::apply_catalog_overlay`'s chain-build-time merge,
    /// with no overlay cell -- these regression tests exercise the baked
    /// layer only).
    fn effective_row_for(provider_kind: &str, model: &str) -> EffectiveRow {
        use crate::catalog::{lookup_baked_with_overrides, merge};
        let baked = lookup_baked_with_overrides(provider_kind, model, None, &BTreeMap::new());
        merge(baked.as_ref(), None)
    }

    /// A bulky payload of roughly `tokens` tokens (4 bytes/token estimate).
    fn payload_of_tokens(tokens: usize) -> String {
        "x".repeat(tokens * 4)
    }

    fn tool_result_msg(payload: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: json!(payload),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn tool_use_msg(payload: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "search".into(),
                input: json!(payload),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            refusal: None,
            role,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A tool-heavy conversation well above the default 100k trigger with a
    /// large elidable span, carrying `SESSION` as the inbound session key and
    /// `UPSTREAM` as the wire model -- so `record_would_trim` finds a plan and
    /// a verified pricing row.
    fn triggering_req() -> ChatRequest {
        let payload = payload_of_tokens(12_000);
        let mut messages = vec![
            text_msg(Role::User, "system framing turn one"),
            text_msg(Role::Assistant, "acknowledged"),
        ];
        for _ in 0..6 {
            messages.push(tool_use_msg(&payload));
            messages.push(tool_result_msg(&payload));
        }
        for i in 0..6 {
            messages.push(text_msg(Role::User, &format!("recent turn {i}")));
        }
        let mut req = ChatRequest {
            model: UPSTREAM.into(),
            messages,
            ..Default::default()
        };
        req.routectl_internal.inbound_session_key = Some(SESSION.into());
        req
    }

    /// Record a calibrated-size window of MIXED reuse under the SERVED-model
    /// triple, exactly as the ingress capture path does post-response
    /// (`record_k_sample` on `meta.served_model`). ~2/3 hits gives a
    /// strictly-interior reuse rate so the calibrated floor is non-trivially
    /// positive rather than an all-miss zero.
    fn record_calibrated_samples(router: &Router) {
        for i in 0..12u64 {
            let cache_read = u64::from(i % 3 != 0);
            router.record_k_sample(
                Some(SESSION),
                PROVIDER_KIND,
                SERVED_MODEL,
                cache_read,
                UNIX_EPOCH + Duration::from_secs(i * 10_000),
            );
        }
    }

    fn key(model: &str) -> crate::k_estimator::KSessionKey {
        crate::k_estimator::KSessionKey {
            session_key: SESSION.into(),
            provider_kind: PROVIDER_KIND.into(),
            model: model.into(),
        }
    }

    fn estimate_for(router: &Router, model: &str) -> crate::k_estimator::KEstimate {
        router.k_estimator.estimate(&crate::k_estimator::KQuery {
            session_key: Some(SESSION),
            provider_kind: PROVIDER_KIND,
            model,
            ttl: Duration::from_mins(5),
            now: SystemTime::now(),
        })
    }

    #[test]
    fn record_would_trim_queries_k_store_under_served_model_not_upstream() {
        use crate::k_estimator::Confidence;

        // Arrange: a router whose K store is populated ONLY under the served-
        // model triple (the key `record_k_sample` writes), plus a request that
        // trips the trim trigger and prices against a verified upstream cell.
        let router = Router::new(Arc::new(Config::default()));
        record_calibrated_samples(&router);
        let req = triggering_req();
        let mut meta = DispatchMeta::for_alias(SERVED_MODEL);

        // Invariant guard: the sample-write key and the (correct) query key are
        // the SAME triple -- keyed on the served nickname, NOT the upstream id.
        // The store therefore has a window under the served triple and NOTHING
        // under the upstream triple, so a calibrated K result below can only
        // come from a query that keyed on the served nickname.
        assert!(
            router.k_session_store.get(&key(SERVED_MODEL)).is_some(),
            "samples must be recorded under the served-model triple",
        );
        assert!(
            router.k_session_store.get(&key(UPSTREAM)).is_none(),
            "nothing is recorded under the upstream triple",
        );
        assert_eq!(
            estimate_for(&router, SERVED_MODEL).confidence,
            Confidence::Calibrated,
            "12 mixed samples under the served triple must classify Calibrated",
        );
        assert_eq!(
            estimate_for(&router, UPSTREAM).confidence,
            Confidence::Cold,
            "the upstream triple is unpopulated -- a query there is always Cold",
        );

        // Act: drive the would-trim query path with the upstream wire id AND
        // the served nickname threaded separately, mirroring the two dispatch
        // call sites (pricing keys on upstream; K keys on the served model).
        let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
        router.record_would_trim(
            &req,
            Some(PROVIDER_KIND),
            UPSTREAM,
            SERVED_MODEL,
            &effective,
            &mut meta,
        );

        // Assert: pricing resolved on the verified upstream cell (sanity that
        // the query block was reached), AND the K query hit the calibrated
        // served-model window, so the floor is persisted. Before the fix the
        // query keyed on UPSTREAM, missed the store, stayed Cold, and left
        // would_trim_k_floor None.
        assert!(
            meta.would_trim_break_even_k.is_some(),
            "verified upstream pricing must populate break_even",
        );
        assert!(
            meta.would_trim_k_floor.is_some(),
            "K query must key on the served model to match the sample-write key",
        );
    }

    #[test]
    fn record_would_trim_folds_missing_baked_row_to_no_break_even() {
        // Arrange: a provider_kind that names no baked cell at all -- not
        // even a provider catch-all (every routectl-shipped provider kind
        // carries one). The two-layer merge resolves `Missing`, which
        // folds to the SAME conservative sentinel behavior as `Disabled`:
        // no break-even K, even though the freed-token count still
        // records.
        const UNKNOWN_KIND: &str = "totally-unknown-kind";
        let router = Router::new(Arc::new(Config::default()));
        let req = triggering_req();
        let mut meta = DispatchMeta::for_alias(SERVED_MODEL);

        let effective = effective_row_for(UNKNOWN_KIND, UPSTREAM);
        router.record_would_trim(
            &req,
            Some(UNKNOWN_KIND),
            UPSTREAM,
            SERVED_MODEL,
            &effective,
            &mut meta,
        );

        assert!(
            meta.would_trim_tokens.is_some(),
            "the freed-token count records regardless of pricing trust",
        );
        assert_eq!(
            meta.would_trim_break_even_k, None,
            "a Missing catalog row must record K* = None",
        );
    }
}

#[cfg(test)]
mod observability_seam_tests {
    //! Router-consumer observability at the class-decision point: the
    //! stable FeatureUnsupported event, the per-arm class-decision
    //! DEBUG/WARN event, and the two RouterMetrics counters -- wired at
    //! BOTH dispatch loops. All capture tests run on the `#[tokio::test]`
    //! current-thread runtime (the dispatch path never spawns before its
    //! error arm), so the thread-local capture subscriber sees every
    //! event the arm emits.

    use super::*;
    use crate::config::{ProviderEntry, RetryPolicy};
    use async_trait::async_trait;
    use routectl_testkit::with_capture;

    /// A body string that must NEVER surface in any observability event
    /// field or message. Every capture test scans for it.
    const SECRET_BODY: &str = "TOP-SECRET-UPSTREAM-BODY-DO-NOT-LOG";

    /// A provider whose `complete` / `stream` both fail with a
    /// configurable upstream status + classifier tokens, carrying a
    /// sentinel body used to prove no body text leaks into the new events.
    struct FailingProvider {
        id: String,
        status: u16,
        upstream_type: Option<String>,
        upstream_code: Option<String>,
    }

    impl FailingProvider {
        fn make_error(&self) -> Error {
            Error::upstream_full(
                &self.id,
                self.status,
                SECRET_BODY,
                None,
                self.upstream_type.clone(),
                self.upstream_code.clone(),
            )
        }
    }

    #[async_trait]
    impl Provider for FailingProvider {
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
            Err(self.make_error())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(self.make_error())
        }
    }

    /// Single openai-compat entry `m1 -> p1`, retry capped at one attempt
    /// so the failing provider is hit exactly once. The config provider
    /// entry exists so the chain expander resolves `provider_kind` to
    /// `openai-compat` (used by both the classifier's token table and the
    /// FeatureUnsupported event's `provider_kind` field).
    fn router_with_failing(status: u16, ty: Option<&str>, code: Option<&str>) -> Router {
        let config = Config {
            retry: RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
            providers: {
                let mut m = BTreeMap::new();
                m.insert(
                    "p1".to_string(),
                    ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
                );
                m
            },
            ..Config::default()
        };
        let mut router = Router::new(Arc::new(config));
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider {
            id: "p1".into(),
            status,
            upstream_type: ty.map(str::to_string),
            upstream_code: code.map(str::to_string),
        });
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
        );
        router.install_resolved_models(models);
        router
    }

    fn req_m1() -> ChatRequest {
        ChatRequest {
            model: "m1".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    /// The sentinel upstream body must not appear in any event emitted by
    /// the NEW observability seam (the FeatureUnsupported event and the
    /// per-arm class-decision DEBUG/WARN). Pre-existing dispatch logs that
    /// render `error = ?e` are out of scope by design and left untouched.
    fn assert_no_body_leak_in_seam(events: &[routectl_testkit::CapturedEvent]) {
        let is_seam = |e: &&routectl_testkit::CapturedEvent| {
            e.target == "routectl::feature_unsupported"
                || e.message == "router failure class decision"
                || e.message == "unknown failure classification on upstream outcome (fail-closed)"
        };
        let seam: Vec<_> = events.iter().filter(is_seam).collect();
        assert!(!seam.is_empty(), "expected at least one seam event");
        for e in seam {
            assert!(
                !e.message.contains(SECRET_BODY),
                "body leaked into seam message: {}",
                e.message
            );
            for (k, v) in &e.fields {
                assert!(
                    !v.contains(SECRET_BODY),
                    "body leaked into seam field {k}: {v}"
                );
            }
        }
    }

    #[tokio::test]
    async fn feature_unsupported_event_fires_on_complete_with_safe_fields() {
        // Arrange: openai-compat 400 carrying `unsupported_parameter` on
        // error.code lifts to FeatureUnsupported.
        let router = router_with_failing(400, None, Some("unsupported_parameter"));

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: the request still fails (event is observational only).
        assert!(result.is_err());
        let ev = events
            .iter()
            .find(|e| e.target == "routectl::feature_unsupported")
            .expect("feature_unsupported event must fire");
        assert_eq!(ev.level, tracing::Level::INFO);
        assert_eq!(ev.field("provider"), Some("p1"));
        assert_eq!(ev.field("provider_kind"), Some("openai-compat"));
        assert_eq!(ev.field("model"), Some("m1"));
        assert_eq!(ev.field("capability"), Some("unsupported_parameter"));
        assert_eq!(ev.field("status"), Some("400"));
        assert_eq!(ev.field("upstream_type"), Some(""));
        assert_eq!(ev.field("upstream_code"), Some("unsupported_parameter"));
        assert_eq!(ev.field("matched_by"), Some("upstream_type"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("is_forwarded"), Some("false"));
        assert_eq!(
            ev.field("remapped"),
            Some("false"),
            "a real upstream lift is not an operator remap"
        );

        assert_no_body_leak_in_seam(&events);
        assert_eq!(router.metrics.feature_unsupported_total(), 1);
        assert_eq!(router.metrics.unknown_failure_classifications_total(), 0);
    }

    #[tokio::test]
    async fn feature_unsupported_event_fires_on_stream_surface() {
        // Arrange
        let router = router_with_failing(400, None, Some("unsupported_parameter"));

        // Act: the pre-first-chunk error rides the stream error arm.
        let (result, events) = with_capture(Box::pin(router.stream(req_m1()))).await;

        // Assert
        assert!(result.is_err());
        let ev = events
            .iter()
            .find(|e| e.target == "routectl::feature_unsupported")
            .expect("feature_unsupported event must fire on the stream loop");
        assert_eq!(ev.field("surface"), Some("stream"));
        assert_eq!(ev.field("capability"), Some("unsupported_parameter"));
        assert_no_body_leak_in_seam(&events);
        assert_eq!(router.metrics.feature_unsupported_total(), 1);
    }

    #[tokio::test]
    async fn unknown_upstream_classification_warns_and_counts_on_complete() {
        // Arrange: status 600 is outside every mapped row -> Unknown by
        // status, on a genuine Error::Upstream (fail-closed unknown).
        let router = router_with_failing(600, None, None);

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert
        assert!(result.is_err());
        let ev = events
            .iter()
            .find(|e| {
                e.message == "unknown failure classification on upstream outcome (fail-closed)"
            })
            .expect("unknown-upstream decision must WARN");
        assert_eq!(ev.level, tracing::Level::WARN);
        assert_eq!(ev.field("effective_class"), Some("unknown"));
        assert_eq!(ev.field("original_class"), Some("unknown"));
        assert_eq!(ev.field("remapped"), Some("false"));
        assert_eq!(ev.field("matched_by"), Some("status"));
        assert_eq!(ev.field("status"), Some("Some(600)"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("fallback"), Some("false"));
        assert_eq!(ev.field("debit"), Some("false"));

        assert_no_body_leak_in_seam(&events);
        assert_eq!(router.metrics.unknown_failure_classifications_total(), 1);
        assert_eq!(router.metrics.feature_unsupported_total(), 0);
    }

    #[tokio::test]
    async fn unknown_upstream_classification_warns_and_counts_on_stream() {
        // Arrange
        let router = router_with_failing(600, None, None);

        // Act
        let (result, events) = with_capture(Box::pin(router.stream(req_m1()))).await;

        // Assert
        assert!(result.is_err());
        let ev = events
            .iter()
            .find(|e| {
                e.message == "unknown failure classification on upstream outcome (fail-closed)"
            })
            .expect("unknown-upstream decision must WARN on the stream loop");
        assert_eq!(ev.level, tracing::Level::WARN);
        assert_eq!(ev.field("surface"), Some("stream"));
        assert_no_body_leak_in_seam(&events);
        assert_eq!(router.metrics.unknown_failure_classifications_total(), 1);
    }

    #[tokio::test]
    async fn generic_bad_request_emits_single_debug_decision() {
        // Arrange: a generic 400 stays BadRequest -- exercises the DEBUG
        // (non-WARN, non-feature) class-decision path and its field set.
        let router = router_with_failing(400, Some("invalid_request_error"), None);

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: exactly one class-decision event per error-arm pass.
        assert!(result.is_err());
        let decisions: Vec<_> = events
            .iter()
            .filter(|e| e.message == "router failure class decision")
            .collect();
        assert_eq!(decisions.len(), 1, "one decision event per error-arm pass");
        let ev = decisions[0];
        assert_eq!(ev.level, tracing::Level::DEBUG);
        assert_eq!(ev.field("effective_class"), Some("bad_request"));
        assert_eq!(ev.field("original_class"), Some("bad_request"));
        assert_eq!(ev.field("remapped"), Some("false"));
        assert_eq!(ev.field("matched_by"), Some("status"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("fallback"), Some("true"));
        assert_eq!(ev.field("debit"), Some("false"));
        assert_eq!(ev.field("retry_cap"), Some("0"));
        assert_eq!(ev.field("is_probe"), Some("false"));
        assert_eq!(ev.field("is_forwarded"), Some("false"));

        assert_no_body_leak_in_seam(&events);
        assert_eq!(router.metrics.feature_unsupported_total(), 0);
        assert_eq!(router.metrics.unknown_failure_classifications_total(), 0);
    }

    #[test]
    fn label_helpers_map_stable_tokens() {
        assert_eq!(class_label(&FailureClass::RateLimited), "rate_limited");
        assert_eq!(class_label(&FailureClass::Unknown), "unknown");
        assert_eq!(
            class_label(&FailureClass::FeatureUnsupported {
                capability: "x".into()
            }),
            "feature_unsupported"
        );
        assert_eq!(matched_by_label(MatchedBy::Variant), "variant");
        assert_eq!(matched_by_label(MatchedBy::Status), "status");
        assert_eq!(matched_by_label(MatchedBy::UpstreamType), "upstream_type");
    }
}

#[cfg(test)]
mod remap_test_support {
    //! Shared fixtures for [`super::provider_remap_tests`] and
    //! [`super::bedrock_class_remap_tests`]: both exercise the
    //! per-provider status remap (`[providers.X.class_overrides]`)
    //! through the REAL `Config` TOML path against a single-leg
    //! `p1`/`m1` router, so the fixture that builds that router and the
    //! provider that fails it on demand live here once instead of
    //! twice.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider that always fails with a fixed status, counting calls
    /// so a test can pin exactly how many times the SAME provider was
    /// dispatched -- the direct behavioral proof that a same-provider
    /// retry did or did not fire.
    pub(super) struct CountingFailingProvider {
        pub(super) id: String,
        pub(super) status: u16,
        pub(super) calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for CountingFailingProvider {
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
            Err(Error::upstream(&self.id, self.status, "body"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, self.status, "body"))
        }
    }

    /// Parse `toml_text` through the real `Config` deserialize path (so
    /// `[providers.p1.class_overrides]` / `[retry.classes]` genuinely
    /// exercise their adapters), install `provider` under nickname `m1`
    /// on provider `p1`, and return the resulting `Router`.
    pub(super) fn router_from_toml(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
        let config: Config = toml::from_str(toml_text).expect("valid test toml");
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
        );
        router.install_resolved_models(models);
        router
    }

    pub(super) fn req_m1() -> ChatRequest {
        ChatRequest {
            model: "m1".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    pub(super) fn find_decision(
        events: &[routectl_testkit::CapturedEvent],
    ) -> &routectl_testkit::CapturedEvent {
        events
            .iter()
            .find(|e| e.message == "router failure class decision")
            .expect("a class-decision event must fire")
    }
}

#[cfg(test)]
mod provider_remap_tests {
    //! End-to-end coverage for the per-provider status remap
    //! (`[providers.X.class_overrides]`) parsed through the REAL TOML
    //! path -- exercising `ConfigFailureClass::to_failure_class`, not a
    //! hand-built `FailureClass` -- and its effect on debit / same-
    //! provider retry / fallback plus the class-decision provenance
    //! fields (`original_class` / `effective_class` / `remapped` /
    //! `remap_status`) and the `feature_unsupported` event's `remapped`
    //! field.

    use super::remap_test_support::{
        CountingFailingProvider, find_decision, req_m1, router_from_toml,
    };
    use super::*;
    use routectl_testkit::with_capture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn override_stops_debit_and_same_provider_retry_but_keeps_fallback_true() {
        // Arrange: baseline would allow 2 same-provider retries on a 5xx
        // (retry_on_5xx = 2); the operator remaps THIS provider's 503 to
        // content-policy (baked cap 0, fallback true).
        let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
503 = "content-policy"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 503,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider.clone());

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: the remap's retry_cap of 0 means exactly one call --
        // no same-provider retry fired.
        assert!(result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "content-policy's baked retry cap is 0"
        );
        let ev = find_decision(&events);
        assert_eq!(ev.field("remapped"), Some("true"));
        assert_eq!(ev.field("remap_status"), Some("Some(503)"));
        assert_eq!(ev.field("original_class"), Some("server_error"));
        assert_eq!(ev.field("effective_class"), Some("content_policy"));
        assert_eq!(
            ev.field("debit"),
            Some("false"),
            "content-policy never debits"
        );
        assert_eq!(ev.field("retry_cap"), Some("0"));
        assert_eq!(
            ev.field("fallback"),
            Some("true"),
            "content-policy still falls back by baked default"
        );
    }

    #[tokio::test]
    async fn without_override_503_debits_and_retries_per_baseline() {
        // Arrange: identical policy, no `class_overrides` -- 503 stays
        // ServerError and follows the baked debit + retry_on_5xx cap.
        let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 503,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider.clone());

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: the baked retry_on_5xx=2 cap is exhausted before
        // falling back, so the provider is dispatched twice.
        assert!(result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "baseline retries the same provider up to retry_on_5xx"
        );
        let ev = find_decision(&events);
        assert_eq!(ev.field("remapped"), Some("false"));
        assert_eq!(ev.field("remap_status"), Some("None"));
        assert_eq!(ev.field("original_class"), Some("server_error"));
        assert_eq!(
            ev.field("effective_class"),
            ev.field("original_class"),
            "no remap means effective == original"
        );
        assert_eq!(ev.field("debit"), Some("true"));
        assert_eq!(ev.field("retry_cap"), Some("2"));
    }

    #[tokio::test]
    async fn feature_unsupported_event_remapped_true_when_target_is_operator_remap() {
        // Arrange: the operator remaps 429 (native RateLimited) to
        // feature-unsupported -- the classifier never produced this
        // lift; it is entirely config-sourced.
        let toml_text = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
429 = "feature-unsupported"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 429,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider);

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert
        assert!(result.is_err());
        let ev = events
            .iter()
            .find(|e| e.target == "routectl::feature_unsupported")
            .expect("feature_unsupported event must fire on an operator remap");
        assert_eq!(
            ev.field("capability"),
            Some(crate::class_policy::OPERATOR_REMAP_CAPABILITY)
        );
        assert_eq!(
            ev.field("remapped"),
            Some("true"),
            "an operator remap into feature-unsupported must be flagged"
        );
    }

    #[tokio::test]
    async fn retry_classes_fallback_only_override_leaves_retry_cap_at_baked_value() {
        // Review-nit regression: `[retry.classes.server-error]` sets
        // ONLY `fallback`, leaving `retry` unset. A sparse leaf-merge
        // bug would zero out the cap instead of deferring to the baked
        // `retry_on_5xx`. No `class_overrides` involved -- this pins the
        // GLOBAL per-class overlay, independent of the per-provider
        // remap this task adds.
        let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 4

[retry.classes.server-error]
fallback = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 500,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider);

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert
        assert!(result.is_err());
        let ev = find_decision(&events);
        assert_eq!(
            ev.field("retry_cap"),
            Some("4"),
            "a fallback-only override must not disturb the baked retry cap"
        );
        assert_eq!(ev.field("fallback"), Some("false"));
        assert_eq!(ev.field("remapped"), Some("false"));
    }
}

#[cfg(test)]
mod bedrock_class_remap_tests {
    //! Bedrock-specific acceptance coverage for the per-provider status
    //! remap, layered on top of `provider_remap_tests`' 503->content-policy
    //! and provenance coverage: a `kind = "bedrock"` provider entry whose
    //! `[providers.X.class_overrides]` remaps 400 to feature-unsupported.
    //!
    //! The remap is behaviorally inert over the baseline for routing --
    //! `BadRequest` and `FeatureUnsupported` share the same terminal
    //! (retry_cap 0, fallback true, no debit) policy row -- so the
    //! deliverable under test is the label + the observability events, plus
    //! two regression pins: the remapped 400 must not debit the breaker or
    //! retry the same provider (it must still advance the chain to a
    //! fallback target), and an UNRELATED status (500) on the same
    //! provider, with the remap block present, must behave exactly like no
    //! remap at all.

    use super::remap_test_support::{
        CountingFailingProvider, find_decision, req_m1, router_from_toml,
    };
    use super::*;
    use routectl_testkit::with_capture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Parse `toml_text` and install a two-leg alias chain: `m1` on
    /// provider `p1`, `m2` on provider `p2`. `[aliases] alias = ["m1",
    /// "m2"]` must already be present in `toml_text`.
    fn two_leg_router_from_toml(
        toml_text: &str,
        leg1: Arc<dyn Provider>,
        leg2: Arc<dyn Provider>,
    ) -> Router {
        let config: Config = toml::from_str(toml_text).expect("valid test toml");
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", leg1, "wire-model-1")),
        );
        models.insert(
            "m2".to_string(),
            Arc::new(ResolvedModel::new("m2", "p2", leg2, "wire-model-2")),
        );
        router.install_resolved_models(models);
        router
    }

    fn req_alias() -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            messages: vec![],
            ..Default::default()
        }
    }

    /// Non-mutating breaker phase for the seat keyed by `state_key`.
    fn circuit_phase(router: &Router, state_key: &str) -> crate::runtime_state::CircuitPhase {
        router
            .capacity_snapshot_for(state_key, Instant::now())
            .expect("seat state slot exists")
            .circuit
    }

    #[tokio::test]
    async fn bedrock_400_remaps_to_feature_unsupported_with_operator_capability_token() {
        // Arrange: a bedrock-kind provider whose 400 is remapped to
        // feature-unsupported. A plain 400 with no upstream type/code
        // natively classifies as bad_request (checked below), so the
        // remap is the only reason this ends up feature-unsupported.
        let toml_text = r#"
[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 400,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider);

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: the feature_unsupported event fires with the
        // operator-remap capability token and remapped=true.
        assert!(result.is_err());
        let fu = events
            .iter()
            .find(|e| e.target == "routectl::feature_unsupported")
            .expect("feature_unsupported event must fire on an operator remap");
        assert_eq!(
            fu.field("capability"),
            Some(crate::class_policy::OPERATOR_REMAP_CAPABILITY)
        );
        assert_eq!(fu.field("remapped"), Some("true"));
        assert_eq!(fu.field("provider_kind"), Some("bedrock"));

        // Assert: the class-decision event carries the original (native)
        // class alongside the remapped effective class.
        let ev = find_decision(&events);
        assert_eq!(ev.field("remapped"), Some("true"));
        assert_eq!(ev.field("remap_status"), Some("Some(400)"));
        assert_eq!(ev.field("original_class"), Some("bad_request"));
        assert_eq!(ev.field("effective_class"), Some("feature_unsupported"));
    }

    #[tokio::test]
    async fn bedrock_remapped_400_does_not_debit_breaker_and_chain_advances_to_fallback() {
        // Arrange: p1 (bedrock) remaps 400 to feature-unsupported and
        // carries a hair-trigger breaker (circuit_failures = 1); p2 is
        // the fallback leg. Repeated calls well past the threshold must
        // never trip p1's breaker (feature-unsupported never debits), and
        // every call must still reach p2 (no same-provider retry on p1,
        // whose remapped retry_cap is the baked 0).
        let toml_text = r#"
[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }
circuit_failures = 1

[providers.p1.class_overrides]
400 = "feature-unsupported"

[providers.p2]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[aliases]
alias = ["m1", "m2"]
"#;
        let p1 = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 400,
            calls: AtomicUsize::new(0),
        });
        let p2 = Arc::new(CountingFailingProvider {
            id: "p2".into(),
            status: 400,
            calls: AtomicUsize::new(0),
        });
        let router = two_leg_router_from_toml(toml_text, p1.clone(), p2.clone());

        // Act: fire well past the configured circuit_failures=1 threshold.
        const ATTEMPTS: usize = 3;
        for _ in 0..ATTEMPTS {
            let result = router.complete(req_alias()).await;
            assert!(result.is_err());
        }

        // Assert: no same-provider retry on the remap (retry_cap 0) --
        // p1 is dispatched exactly once per request.
        assert_eq!(
            p1.calls.load(Ordering::SeqCst),
            ATTEMPTS,
            "the remapped 400's baked retry_cap is 0: no same-provider retry"
        );
        // Assert: the chain advances to the fallback target every time.
        assert_eq!(
            p2.calls.load(Ordering::SeqCst),
            ATTEMPTS,
            "feature-unsupported still falls back to the next chain entry"
        );
        // Assert: the breaker never trips, even past its threshold --
        // feature-unsupported never debits.
        assert_eq!(
            circuit_phase(&router, "m1"),
            crate::runtime_state::CircuitPhase::Closed,
            "a remapped feature-unsupported outcome must never debit the breaker"
        );
    }

    #[tokio::test]
    async fn bedrock_500_with_remap_block_present_behaves_like_no_remap() {
        // Arrange: identical to `provider_remap_tests::
        // without_override_503_debits_and_retries_per_baseline`'s
        // baseline, but on a bedrock-kind provider that ALSO carries a
        // class_overrides block -- for an unrelated status (400). A 500
        // must debit and retry per retry_on_5xx exactly as if the remap
        // block were absent.
        let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
        let provider = Arc::new(CountingFailingProvider {
            id: "p1".into(),
            status: 500,
            calls: AtomicUsize::new(0),
        });
        let router = router_from_toml(toml_text, provider.clone());

        // Act
        let (result, events) = with_capture(router.complete(req_m1())).await;

        // Assert: the baked retry_on_5xx=2 cap is exhausted before
        // falling back -- the presence of an unrelated remap block for
        // 400 changes nothing about the 500 path.
        assert!(result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "normal 5xx traffic retries the same provider up to retry_on_5xx \
             regardless of an unrelated class_overrides entry"
        );
        let ev = find_decision(&events);
        assert_eq!(ev.field("remapped"), Some("false"));
        assert_eq!(ev.field("remap_status"), Some("None"));
        assert_eq!(ev.field("original_class"), Some("server_error"));
        assert_eq!(
            ev.field("effective_class"),
            ev.field("original_class"),
            "no remap for status 500 means effective == original"
        );
        assert_eq!(ev.field("debit"), Some("true"));
        assert_eq!(ev.field("retry_cap"), Some("2"));
    }
}

#[cfg(test)]
mod learn_capture_tests {
    //! End-to-end coverage for the learn-event capture point
    //! (`observe_for_learning`): the full eligibility gate, per-request
    //! dedupe, registry wiring, structured WARN, and the
    //! `DispatchMeta.learned_capabilities` ride-along -- driven through the
    //! REAL dispatch error arms (complete + stream), so the classifier,
    //! matcher, guardrail, and registry all participate.

    use super::*;
    use routectl_core::ForwardedBearer;
    use routectl_core::ToolDef;
    use routectl_core::capability::SignalTier;
    use routectl_testkit::{CapturedEvent, with_capture};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider whose complete/stream always fail with a fixed upstream
    /// status plus optional type/code, so a test drives a precise
    /// classifier outcome (a self-identifying token or an inferred body).
    /// Counts calls so a test can prove a same-provider retry did fire.
    struct CapabilityRejectingProvider {
        id: &'static str,
        status: u16,
        body: String,
        upstream_type: Option<String>,
        upstream_code: Option<String>,
        calls: AtomicUsize,
    }

    impl CapabilityRejectingProvider {
        fn err(&self) -> Error {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Error::upstream_full(
                self.id,
                self.status,
                self.body.clone(),
                None,
                self.upstream_type.clone(),
                self.upstream_code.clone(),
            )
        }
    }

    #[async_trait::async_trait]
    impl Provider for CapabilityRejectingProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Err(self.err())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(self.err())
        }
    }

    /// Self-identifying openai-compat rejection: a 400 whose `error.code`
    /// is `unsupported_parameter` (the classifier lifts it to
    /// `FeatureUnsupported`) and whose `/error/param` names the real
    /// capability `web_search` -- the field the shared resolver reads.
    fn self_identifying_provider() -> Arc<CapabilityRejectingProvider> {
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search","message":"Unsupported parameter."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
    }

    /// A self-identifying openai-compat 400 whose `/error/param` resolves to
    /// `web_search` once the resolver trims it, but whose RAW field is an
    /// oversized, control-char-laden blob (`web_search` followed by 80
    /// newlines). Models a buggy or adversarial upstream: the closed-set
    /// resolver still attributes the capability, yet the raw param must never
    /// reach the operator log verbatim.
    fn oversized_param_provider() -> Arc<CapabilityRejectingProvider> {
        let param = format!("web_search{}", "\n".repeat(80));
        let body = json!({
            "error": {
                "type": "invalid_request_error",
                "code": "unsupported_parameter",
                "param": param,
                "message": "Unsupported parameter."
            }
        })
        .to_string();
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body,
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
    }

    /// A self-identifying openai-compat 400 whose `error.code` lifts to
    /// `FeatureUnsupported` but whose body carries NO `/error/param`: the
    /// resolver can attribute no capability, so nothing is learned.
    fn paramless_unsupported_provider() -> Arc<CapabilityRejectingProvider> {
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: "{}".into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
    }

    /// A self-identifying openai-compat 400 whose `/error/param` canonicalizes
    /// to the well-known tool-type key `web_search` -- but the triggering
    /// request carries NO web_search tool. Models a misbehaving or compromised
    /// upstream naming an off-request capability.
    fn poisoned_param_provider() -> Arc<CapabilityRejectingProvider> {
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search_20250305","message":"Unsupported parameter."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
    }

    /// An anthropic-api 400 carrying the verbatim prefill-unsupported phrase
    /// in free-text `error.message` -- a generic BadRequest the resolver's
    /// inferred arm maps to `prefill`.
    fn prefill_inferred_provider() -> Arc<CapabilityRejectingProvider> {
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"type":"error","error":{"type":"invalid_request_error","message":"Prefilling assistant messages is not supported for this model."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: None,
            calls: AtomicUsize::new(0),
        })
    }

    fn router_with(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
        let config: Config = toml::from_str(toml_text).expect("valid test toml");
        let mut router = Router::new(Arc::new(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
        );
        router.install_resolved_models(models);
        router
    }

    /// A minimal openai-compat provider config (capability subsystem left
    /// at its default: enabled).
    const OPENAI_P1: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

    /// A minimal anthropic-api provider config (capability subsystem left at
    /// its default: enabled). Serves the inferred-arm dormancy test.
    const ANTHROPIC_P1: &str = r#"
[providers.p1]
kind = "anthropic-api"
"#;

    fn req_with_tool(tool_type: &str) -> ChatRequest {
        ChatRequest {
            model: "m1".into(),
            messages: vec![],
            tools: Some(vec![ToolDef::Other(json!({ "type": tool_type }))]),
            ..Default::default()
        }
    }

    fn learn_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message == "learned-capability negative observed")
            .collect()
    }

    #[tokio::test]
    async fn eligible_self_identifying_records_warns_and_populates_meta() {
        // Arrange: a self-identifying 400 whose capability token is also
        // the request's derived feature -> the guardrail admits it.
        let router = router_with(OPENAI_P1, self_identifying_provider());

        // Act
        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        // Assert: the request still fails (learning never changes the
        // per-request outcome), but the meta ride-along carries the event.
        assert!(dispatched.result.is_err());
        assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
        let ev = &dispatched.meta.learned_capabilities[0];
        assert_eq!(ev.state_key, "m1");
        assert_eq!(ev.capability_key, "web_search");
        assert_eq!(ev.provider_kind, "openai-compat");
        assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
        assert_eq!(ev.observations, 1);
        assert_eq!(ev.upstream_status, 400);
        assert!(!ev.remapped);
        assert_eq!(ev.request_features, vec!["web_search".to_string()]);

        // The structured WARN carries only the safe fields.
        let warns = learn_warns(&events);
        assert_eq!(warns.len(), 1);
        let warn = warns[0];
        assert_eq!(warn.field("event"), Some("learn"));
        assert_eq!(warn.field("state_key"), Some("m1"));
        assert_eq!(warn.field("capability_key"), Some("web_search"));
        assert_eq!(warn.field("provider_kind"), Some("openai-compat"));
        assert_eq!(warn.field("upstream_status"), Some("400"));
        assert_eq!(warn.field("upstream_code"), Some("unsupported_parameter"));
        assert_eq!(warn.field("upstream_param"), Some("web_search"));
        assert_eq!(warn.field("signal_tier"), Some("self-identifying"));
        assert_eq!(warn.field("observations"), Some("1"));
        assert_eq!(warn.field("acting"), Some("true"));
        assert_eq!(warn.field("body"), None, "no body/message/prompt fields");
        assert_eq!(warn.field("message"), None);

        // The registry now holds an acting negative for the target.
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
        );
    }

    #[tokio::test]
    async fn oversized_upstream_param_is_dropped_from_the_learn_warn() {
        // Arrange: a self-identifying 400 whose raw `/error/param` is oversized
        // and control-char-laden but trims to the canonical `web_search` key.
        let router = router_with(OPENAI_P1, oversized_param_provider());

        // Act
        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        // Assert: the capability is still learned (the resolver trims the raw
        // field to a closed-set key), so the safe fields log unchanged.
        assert!(dispatched.result.is_err());
        assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
        assert_eq!(
            dispatched.meta.learned_capabilities[0].capability_key,
            "web_search"
        );
        let warns = learn_warns(&events);
        assert_eq!(warns.len(), 1);
        let warn = warns[0];
        assert_eq!(warn.field("capability_key"), Some("web_search"));
        assert_eq!(warn.field("upstream_code"), Some("unsupported_parameter"));
        // The unbounded, control-char-laden raw param is dropped entirely --
        // the field is absent, not blank, so no injected text reaches the log.
        assert_eq!(warn.field("upstream_param"), None);
    }

    #[tokio::test]
    async fn eligible_self_identifying_captures_on_the_stream_arm_too() {
        // The stream error arm is wired identically to the complete arm.
        let router = router_with(OPENAI_P1, self_identifying_provider());

        let (dispatched, events) = with_capture(
            router.stream_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.result.is_err());
        assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
        assert_eq!(learn_warns(&events).len(), 1);
    }

    #[tokio::test]
    async fn same_request_retry_dedupes_to_one_observation() {
        // Arrange: a non-remapping operator overlay raises the
        // feature-unsupported same-provider retry cap so the ONE request
        // hits the error arm more than once against the SAME (state_key,
        // feature). Per-request dedupe must still count exactly one.
        let toml = r#"
[retry]
max_attempts = 3

[retry.classes.feature-unsupported]
retry = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
        let provider = self_identifying_provider();
        let router = router_with(toml, provider.clone());

        // Act
        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        // Assert: the same provider WAS retried (two dispatches), so the
        // error arm ran twice -- yet only one observation, one WARN, and
        // one meta event survived the dedupe.
        assert!(dispatched.result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "the raised retry cap must drive a same-provider retry",
        );
        assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
        assert_eq!(
            dispatched.meta.learned_capabilities[0].observations, 1,
            "a same-request retry must not manufacture a second observation",
        );
        assert_eq!(learn_warns(&events).len(), 1);
    }

    #[tokio::test]
    async fn kill_switch_off_skips_the_learn_path() {
        let toml = r#"
[capability]
enabled = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
        let router = router_with(toml, self_identifying_provider());

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
        assert!(router.learned_capabilities.is_empty());
    }

    /// The suppression WARN a masked-cell rejection emits.
    const MASK_SUPPRESSION_MSG: &str =
        "force_supported override contradicted: masked capability still rejected upstream";

    fn suppression_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message == MASK_SUPPRESSION_MSG)
            .collect()
    }

    #[tokio::test]
    async fn masked_reject_emits_suppression_warn_and_counter_and_skips_learn() {
        // A force_supported override masks `web_search` on m1: the
        // mask lets the target dispatch (act side short-circuits to Allow),
        // upstream still rejects it, and the learn side suppresses the observe
        // -- one suppression WARN + one counter, no learned negative, no learn
        // event.
        let toml = format!(
            "{OPENAI_P1}\n\
             [capability.overrides.\"p1:m1\"]\n\
             force_supported = [\"web_search\"]\n"
        );
        let router = router_with(&toml, self_identifying_provider());

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        // The request still fails; nothing was learned and the ordinary learn
        // WARN / meta event never fired.
        assert!(dispatched.result.is_err());
        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
        assert!(
            router.learned_capabilities.is_empty(),
            "a masked cell must never create a learned negative",
        );

        // Exactly one suppression WARN, carrying only the safe fields.
        let supp = suppression_warns(&events);
        assert_eq!(supp.len(), 1);
        assert_eq!(supp[0].field("event"), Some("suppression"));
        assert_eq!(supp[0].field("state_key"), Some("m1"));
        assert_eq!(supp[0].field("capability_key"), Some("web_search"));
        assert_eq!(supp[0].field("body"), None, "no body/message/prompt fields");
        assert_eq!(supp[0].field("message"), None);

        // ...and exactly one dedicated counter increment.
        assert_eq!(router.metrics.mask_suppressed_total(), 1);
    }

    #[tokio::test]
    async fn masked_cell_rejection_does_not_refresh_resident_entry() {
        // With an ALREADY-resident learned negative, a masked-cell rejection
        // must neither refresh (expires_at) nor increment (observations) the
        // entry -- its wall-clock decay continues on the original clock.
        let toml = format!(
            "{OPENAI_P1}\n\
             [capability.overrides.\"p1:m1\"]\n\
             force_supported = [\"web_search\"]\n"
        );
        let router = router_with(&toml, self_identifying_provider());

        // Plant a resident acting negative at a fixed instant.
        let t0 = Instant::now();
        router.learned_capabilities.observe(
            "m1",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            t0,
        );
        let before = router.learned_capabilities.snapshot();
        assert_eq!(before.len(), 1);

        let (dispatched, _events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        // No learn event, and the resident entry is untouched: neither
        // incremented nor refreshed by the masked rejection.
        assert!(dispatched.meta.learned_capabilities.is_empty());
        let after = router.learned_capabilities.snapshot();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].observations, before[0].observations,
            "masked rejection must not increment observations",
        );
        assert_eq!(
            after[0].expires_at, before[0].expires_at,
            "masked rejection must not refresh the decay clock",
        );
        assert_eq!(router.metrics.mask_suppressed_total(), 1);
    }

    #[tokio::test]
    async fn non_request_fault_status_is_never_learned() {
        // A 500 is ServerError, not a 400/422 request fault: the status
        // gate rejects it before the matcher ever runs.
        let provider = Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 500,
            body: "{}".into(),
            upstream_type: None,
            upstream_code: None,
            calls: AtomicUsize::new(0),
        });
        let router = router_with(OPENAI_P1, provider);

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
    }

    #[tokio::test]
    async fn unresolvable_openai_rejection_is_not_learned() {
        // A self-identifying 400 whose body carries NO `/error/param`: the
        // resolver can attribute no canonical capability, so nothing is
        // learned. (The old cross-namespace gate is gone; the resolver's
        // no-learn-on-unresolvable is the replacement guardrail.)
        let router = router_with(OPENAI_P1, paramless_unsupported_provider());

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
    }

    #[tokio::test]
    async fn off_request_param_is_not_learned() {
        // Poisoning guard: the upstream names `web_search` in `/error/param`
        // (the resolver canonicalizes the dated variant to the well-known
        // key), but the triggering request carried only `computer_use` -- so
        // `web_search` is NOT in the request's derived feature set. The
        // capture membership gate blocks the learn: a misbehaving upstream
        // cannot teach a capability the request never sent, and no registry
        // entry is planted.
        let router = router_with(OPENAI_P1, poisoned_param_provider());

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("computer_use"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.result.is_err());
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "an off-request param must never produce a learn event",
        );
        assert!(learn_warns(&events).is_empty());
        assert!(
            router.learned_capabilities.is_empty(),
            "an off-request param must never create a registry entry",
        );
    }

    #[tokio::test]
    async fn inferred_prefill_is_dormant_and_not_learned() {
        // The inferred arm resolves the anthropic prefill phrase to `prefill`,
        // but `derive_feature_keys` never produces that key, so a request's
        // derived feature set can never contain it. The capture membership
        // gate blocks the learn end-to-end: the inferred table ships dormant
        // until an act-side derivation for `prefill` exists.
        let router = router_with(ANTHROPIC_P1, prefill_inferred_provider());

        // A prefill request carries no built-in tool / output_config, so its
        // derived feature set is empty.
        let req = ChatRequest {
            model: "m1".into(),
            messages: vec![],
            ..Default::default()
        };

        let (dispatched, events) =
            with_capture(router.complete_with_options(req, RouterOptions::default())).await;

        assert!(dispatched.result.is_err());
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "inferred prefill is dormant: nothing derives it act-side, so it must not learn",
        );
        assert!(learn_warns(&events).is_empty());
        assert!(router.learned_capabilities.is_empty());
    }

    #[tokio::test]
    async fn forwarded_request_is_not_learned() {
        // A request carrying a forwarded client bearer never contributes a
        // learned negative (the forwarded token owns its own retry/backoff).
        let router = router_with(OPENAI_P1, self_identifying_provider());
        let mut req = req_with_tool("web_search");
        req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("t".into()));

        let (dispatched, events) =
            with_capture(router.complete_with_options(req, RouterOptions::default())).await;

        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
    }

    #[tokio::test]
    async fn operator_remapped_class_is_not_learned() {
        // The operator remaps 400 to feature-unsupported: the class is now
        // config-sourced (remapped == true, capability == the operator-remap
        // token), so the learn path skips it -- a synthesized class is not
        // an upstream self-report.
        let toml = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
        let router = router_with(toml, self_identifying_provider());

        let (dispatched, events) = with_capture(
            router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
        )
        .await;

        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(learn_warns(&events).is_empty());
    }

    /// A provider whose `complete` always succeeds with a minimal response,
    /// so a re-probe dispatched to it settles as a success.
    struct SuccessProvider {
        id: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for SuccessProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(self.id, "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                model: req.model,
                ..Default::default()
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(Error::upstream(self.id, 500, "unused"))
        }
    }

    /// A generic 400 that names NO capability (openai-compat has no inferred
    /// matcher), so the matcher yields `None` and a re-probe against it settles
    /// as an OtherError rather than a same-capability rejection.
    fn other_error_provider() -> Arc<CapabilityRejectingProvider> {
        Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: "{}".into(),
            upstream_type: None,
            upstream_code: None,
            calls: AtomicUsize::new(0),
        })
    }

    /// Seed an already-expired, still-acting self-identifying negative so the
    /// next dispatch's filter claims the single re-probe slot. A past
    /// `expires_at` (rather than a zero decay) keeps the registry's real decay
    /// intact, so a same-capability settle can be observed backing off.
    fn seed_expired_negative(router: &Router, state_key: &str, feature: &str) {
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("test clock is well past boot");
        router.learned_capabilities.import_entries(vec![
            crate::learned_capability::ExportedEntry {
                state_key: state_key.into(),
                feature_key: feature.into(),
                signal: SignalTier::SelfIdentifying,
                observations: 1,
                first_seen: past,
                last_seen: past,
                expires_at: past,
                in_flight: false,
                consecutive_failed_probes: 0,
            },
        ]);
    }

    #[tokio::test]
    async fn probe_success_clears_the_learned_negative() {
        // Arrange: an expired negative for the very feature the request asks
        // for; the target's provider then succeeds on the admitted re-probe.
        let router = router_with(OPENAI_P1, Arc::new(SuccessProvider { id: "p1" }));
        seed_expired_negative(&router, "m1", "web_search");

        // Act
        let dispatched = router
            .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
            .await;

        // Assert: the probe was admitted, the 2xx cleared the entry, and a
        // subsequent lookup now allows the target.
        assert!(dispatched.result.is_ok());
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert!(router.learned_capabilities.is_empty());
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::Allow,
        );
    }

    #[tokio::test]
    async fn probe_same_capability_rejection_refreshes_with_backoff() {
        // Arrange: an expired negative; the probe target re-rejects the SAME
        // capability (self-identifying 400).
        let router = router_with(OPENAI_P1, self_identifying_provider());
        seed_expired_negative(&router, "m1", "web_search");
        let before = router.learned_capabilities.snapshot()[0].expires_at;

        // Act
        let dispatched = router
            .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
            .await;

        // Assert: the request still fails (a probe is a real user request),
        // the probe failure was counted, and the entry re-acts on a fresh,
        // later window with a bumped observation -- in_flight released.
        assert!(dispatched.result.is_err());
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert_eq!(router.metrics.probe_failures_total(), 1);
        // A probe re-rejection settles the probe; it is not a fresh learn
        // event, so nothing rides the meta ledger channel.
        assert!(dispatched.meta.learned_capabilities.is_empty());

        let snap = router.learned_capabilities.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].observations, 2);
        assert!(
            snap[0].expires_at > before,
            "same-capability rejection must push expiry into the future with backoff",
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
            "the refreshed negative is non-expired and in_flight released -> route away",
        );
    }

    #[tokio::test]
    async fn probe_other_error_releases_slot_and_re_probes_next_request() {
        // Arrange: an expired negative; the probe target fails with an error
        // that is NOT the same-capability rejection.
        let router = router_with(OPENAI_P1, other_error_provider());
        seed_expired_negative(&router, "m1", "web_search");

        // Act
        let dispatched = router
            .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
            .await;

        // Assert: the probe was admitted but a transient must NOT clear a valid
        // negative or count as a same-capability failure. The entry survives
        // unchanged and expired, so the NEXT request re-probes -- the
        // repeat-re-probe property broken before this wiring.
        assert!(dispatched.result.is_err());
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert_eq!(router.metrics.probe_failures_total(), 0);
        let snap = router.learned_capabilities.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].observations, 1,
            "OtherError leaves observations untouched"
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::ProbeAdmitted,
            "in_flight released + still expired -> the next request admits a NEW probe",
        );
    }

    #[tokio::test]
    async fn stream_probe_same_capability_rejection_settles_on_the_stream_arm() {
        // The stream loop wires the settle guard identically to the complete
        // loop: a pre-first-chunk same-capability rejection settles the probe.
        let router = router_with(OPENAI_P1, self_identifying_provider());
        seed_expired_negative(&router, "m1", "web_search");
        let before = router.learned_capabilities.snapshot()[0].expires_at;

        let dispatched = router
            .stream_with_options(req_with_tool("web_search"), RouterOptions::default())
            .await;

        assert!(dispatched.result.is_err());
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert_eq!(router.metrics.probe_failures_total(), 1);
        let snap = router.learned_capabilities.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].observations, 2);
        assert!(
            snap[0].expires_at > before,
            "same-capability rejection must push expiry into the future with backoff",
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
        );
    }

    #[tokio::test]
    async fn count_tokens_releases_admitted_probe_without_latching() {
        // A probe admitted while filtering a count_tokens request is released
        // (OtherError): the token-count path is not a messages-capability test,
        // so the entry must not latch in_flight.
        let router = router_with(OPENAI_P1, self_identifying_provider());
        seed_expired_negative(&router, "m1", "web_search");

        // openai-compat cannot count_tokens, so the walk terminates without
        // touching the provider -- but the filter still admitted the probe.
        let result = router.count_tokens(req_with_tool("web_search")).await;

        assert!(matches!(result, Err(Error::NotImplemented(..))));
        assert_eq!(router.metrics.probe_attempts_total(), 1);
        assert_eq!(router.metrics.probe_failures_total(), 0);
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                "m1",
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::ProbeAdmitted,
            "in_flight released -> the next request re-probes rather than latching",
        );
    }
}

#[cfg(test)]
mod strip_interceptor_dispatch_tests {
    //! End-to-end wiring of the strip interceptor at the three dispatch
    //! paths (`complete`, `stream`, `count_tokens`). Each test drives a
    //! real acting learned negative so the feature filter populates
    //! `strip_capabilities`, then asserts the bytes that reach the
    //! provider are the stripped bytes -- identically across all three
    //! paths.
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use parking_lot::Mutex as ParkingMutex;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, Message, Provider, TokenCount, ToolDef,
    };
    use routectl_testkit::with_capture;
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// Records every request that reaches the upstream at any of the three
    /// dispatch entry points, and is `anthropic-api`-kind so it also serves
    /// the `count_tokens` walk.
    struct ProbeProvider {
        captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
    }

    #[async_trait]
    impl Provider for ProbeProvider {
        fn id(&self) -> &'static str {
            "probe"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("probe", "unused"))
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
                    model: "m".into(),
                    choices: vec![],
                    usage: None,
                    opaque_events: Vec::new(),
                    upstream_meta: None,
                })
            });
            Ok(s.boxed())
        }
        async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
            self.captured.lock().push(req);
            Ok(TokenCount {
                input_tokens: 7,
                extras: serde_json::Map::new(),
            })
        }
    }

    fn advisor_request() -> ChatRequest {
        ChatRequest {
            model: "haiku".into(),
            messages: vec![],
            tools: Some(vec![ToolDef::Other(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            ..Default::default()
        }
    }

    fn acting_advisor_negative(state_key: &str) -> crate::learned_capability::ExportedEntry {
        let base = Instant::now();
        crate::learned_capability::ExportedEntry {
            state_key: state_key.into(),
            feature_key: "advisor".into(),
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base + Duration::from_hours(48),
            in_flight: false,
            consecutive_failed_probes: 0,
        }
    }

    /// Router with a single `anthropic-api` provider `prov` and a model
    /// `haiku` whose upstream is served by `provider`. When `learning` is
    /// off the kill switch disables the learned pass entirely.
    fn build_router(provider: Arc<dyn Provider>, learning: bool) -> Router {
        build_router_strict(provider, learning, false)
    }

    fn build_router_strict(provider: Arc<dyn Provider>, learning: bool, strict: bool) -> Router {
        let toml = format!(
            "version = 3\n[server]\nstrict_translation = {strict}\n\
             [capability]\nenabled = {learning}\n\
             [providers.prov]\nkind = \"anthropic-api\"\n"
        );
        let config: Config = toml::from_str(&toml).expect("config parses");
        let mut router = Router::new(Arc::new(config));
        let model = ResolvedModel::new("haiku", "prov", provider, "claude-haiku");
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert("haiku".into(), Arc::new(model));
        router.install_resolved_models(models);
        router
    }

    /// Advisor request whose `tool_choice` forces the advisor tool the
    /// strip removes -- a strip-created hazard the post-strip check rolls
    /// back, driving the route-away branch.
    fn advisor_request_forcing_advisor() -> ChatRequest {
        ChatRequest {
            tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
            ..advisor_request()
        }
    }

    /// Advisor request whose `tool_choice` mandates SOME tool (`{"type":
    /// "any"}`) while the advisor is the only tool -- stripping it empties
    /// the list, a strip-created hazard the post-strip check rolls back.
    fn advisor_request_mandatory_choice() -> ChatRequest {
        ChatRequest {
            tool_choice: Some(json!({"type": "any"})),
            ..advisor_request()
        }
    }

    fn captured() -> Arc<ParkingMutex<Vec<ChatRequest>>> {
        Arc::new(ParkingMutex::new(Vec::new()))
    }

    fn dispatched_tool_types(req: &ChatRequest) -> Vec<String> {
        req.tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| match t {
                        ToolDef::Other(v) => {
                            v.get("type").and_then(|x| x.as_str()).map(str::to_string)
                        }
                        ToolDef::Custom(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn complete_strips_advisor_before_dispatch() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        router.complete(advisor_request()).await.expect("ok");

        let cap = cap.lock();
        let upstream = cap.first().expect("one upstream call");
        assert!(
            dispatched_tool_types(upstream).is_empty(),
            "advisor tool is stripped before dispatch",
        );
        assert_eq!(router.metrics.strip_total(), 1);
    }

    #[tokio::test]
    async fn stream_strips_advisor_identically() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let _ = router
            .stream(advisor_request())
            .await
            .expect("ok")
            .collect::<Vec<_>>()
            .await;

        let cap = cap.lock();
        let upstream = cap.first().expect("one upstream call");
        assert!(
            dispatched_tool_types(upstream).is_empty(),
            "the streaming path strips identically to the completion path",
        );
        assert_eq!(router.metrics.strip_total(), 1);
    }

    #[tokio::test]
    async fn count_tokens_strips_so_estimated_prefix_matches_shipped() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let count = router.count_tokens(advisor_request()).await.expect("ok");
        assert_eq!(count.input_tokens, 7);

        let cap = cap.lock();
        let upstream = cap.first().expect("one count_tokens call");
        assert!(
            dispatched_tool_types(upstream).is_empty(),
            "count_tokens counts the stripped prefix, matching the shipped prefix",
        );
        assert_eq!(router.metrics.strip_total(), 1);
    }

    #[tokio::test]
    async fn kill_switch_off_leaves_advisor_intact() {
        // With `[capability] enabled = false` the learned pass never runs,
        // so `strip_capabilities` stays empty and the advisor tool reaches
        // the upstream unstripped -- the helper is inert by construction.
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, false);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        router.complete(advisor_request()).await.expect("ok");

        let cap = cap.lock();
        let upstream = cap.first().expect("one upstream call");
        assert_eq!(
            dispatched_tool_types(upstream),
            vec!["advisor".to_string()],
            "a disabled kill switch leaves the request untouched",
        );
        assert_eq!(router.metrics.strip_total(), 0);
    }

    #[tokio::test]
    async fn complete_strict_rejects_without_dispatching() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router_strict(provider, true, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let err = router
            .complete(advisor_request())
            .await
            .expect_err("strict translation rejects the strip");

        assert!(matches!(err, Error::Validation(_)), "{err:?}");
        assert!(
            cap.lock().is_empty(),
            "a strict-rejected attempt never reaches the upstream",
        );
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);
    }

    #[tokio::test]
    async fn complete_rollback_routes_away_without_dispatching_mutated_request() {
        // The only chain entry rolls back (dangling forced tool_choice), so
        // the mutated request is never dispatched and the single-entry chain
        // exhausts to an error -- with zero upstream calls.
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let result = router.complete(advisor_request_forcing_advisor()).await;

        assert!(result.is_err(), "the rolled-back attempt does not dispatch");
        assert!(
            cap.lock().is_empty(),
            "a rolled-back attempt never dispatches the mutated request",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);
    }

    #[tokio::test]
    async fn stream_strict_rejects_without_dispatching() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router_strict(provider, true, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let err = router
            .stream(advisor_request())
            .await
            .err()
            .expect("strict translation rejects the strip before the stream opens");

        assert!(matches!(err, Error::Validation(_)), "{err:?}");
        assert!(cap.lock().is_empty(), "no upstream call on strict reject");
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
    }

    #[tokio::test]
    async fn count_tokens_strict_rejects_without_dispatching() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router_strict(provider, true, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let err = router
            .count_tokens(advisor_request())
            .await
            .expect_err("strict translation rejects the strip");

        assert!(matches!(err, Error::Validation(_)), "{err:?}");
        assert!(
            cap.lock().is_empty(),
            "count_tokens never reaches the upstream on strict reject",
        );
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
    }

    #[tokio::test]
    async fn count_tokens_rollback_advances_seat_without_dispatching() {
        // The only capable seat rolls back, so count_tokens advances past it
        // and the walk exhausts -- with no provider count_tokens call.
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let result = router.count_tokens(advisor_request_forcing_advisor()).await;

        assert!(result.is_err(), "the only seat rolled back and was skipped");
        assert!(
            cap.lock().is_empty(),
            "a rolled-back seat never calls the upstream count_tokens",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
    }

    #[tokio::test]
    async fn complete_rollback_on_mandatory_choice_emptied_tools() {
        // The sole tool is the advisor; stripping it empties the list while
        // tool_choice mandates a tool. The post-strip check rolls back, so
        // the single-entry chain exhausts with no upstream call.
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let result = router.complete(advisor_request_mandatory_choice()).await;

        assert!(
            result.is_err(),
            "the emptied-tools attempt does not dispatch"
        );
        assert!(
            cap.lock().is_empty(),
            "a rolled-back attempt never dispatches the mutated request",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);
    }

    #[tokio::test]
    async fn stream_rollback_on_mandatory_choice_emptied_tools() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let result = router.stream(advisor_request_mandatory_choice()).await;

        assert!(result.is_err(), "the streaming path rolls back identically");
        assert!(
            cap.lock().is_empty(),
            "a rolled-back stream never dispatches the mutated request",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
        assert_eq!(router.metrics.strip_total(), 0);
    }

    #[tokio::test]
    async fn count_tokens_rollback_on_mandatory_choice_emptied_tools() {
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, true);
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative("haiku")]);

        let result = router
            .count_tokens(advisor_request_mandatory_choice())
            .await;

        assert!(result.is_err(), "the only seat rolled back and was skipped");
        assert!(
            cap.lock().is_empty(),
            "a rolled-back seat never calls the upstream count_tokens",
        );
        assert_eq!(router.metrics.strip_rollback_total(), 1);
    }

    /// An `anthropic-api`-kind provider whose `count_tokens` always fails
    /// with a fixed upstream health status, so the seat reaches the
    /// class/remap/debit settle point rather than returning a count.
    struct HealthErrorProvider {
        status: u16,
    }

    #[async_trait]
    impl Provider for HealthErrorProvider {
        fn id(&self) -> &'static str {
            "health"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("health", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Err(Error::upstream("health", self.status, "unused"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(Error::upstream("health", self.status, "unused"))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            Err(Error::upstream(
                "health",
                self.status,
                "upstream health error",
            ))
        }
    }

    fn plain_count_request() -> ChatRequest {
        ChatRequest {
            model: "haiku".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn count_tokens_class_path_emits_one_observability_event() {
        // A 500 is a health class the seat debits: the terminal walk exits
        // here (count_tokens never falls back on health), so the settle point
        // fires exactly one INFO `count_tokens` event carrying the class
        // decision -- no longer silent.
        let provider: Arc<dyn Provider> = Arc::new(HealthErrorProvider { status: 500 });
        let router = build_router(provider, false);

        let (result, events) = with_capture(router.count_tokens(plain_count_request())).await;

        assert!(result.is_err(), "the 500 health error surfaces terminal");
        let emitted: Vec<_> = events
            .iter()
            .filter(|e| e.field("event") == Some("count_tokens"))
            .collect();
        assert_eq!(
            emitted.len(),
            1,
            "the class/remap/debit path emits exactly one count_tokens event",
        );
        let ev = emitted[0];
        assert_eq!(ev.level, tracing::Level::INFO);
        assert_eq!(ev.field("state_key"), Some("haiku"));
        assert_eq!(ev.field("status"), Some("500"));
        assert_eq!(ev.field("effective_class"), Some("server_error"));
        assert_eq!(ev.field("debit"), Some("true"));
        assert_eq!(ev.field("remapped"), Some("false"));
        assert!(
            !ev.fields.iter().any(|(k, _)| k == "body" || k == "prompt"),
            "the event carries no body or prompt",
        );
    }

    #[tokio::test]
    async fn count_tokens_clean_passthrough_emits_no_class_event() {
        // A successful count never reaches the class/remap/debit settle point,
        // so no count_tokens observability event fires on the happy path.
        let cap = captured();
        let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
            captured: cap.clone(),
        });
        let router = build_router(provider, false);

        let (result, events) = with_capture(router.count_tokens(plain_count_request())).await;

        assert!(result.is_ok(), "a clean count_tokens passthrough succeeds");
        assert!(
            !events
                .iter()
                .any(|e| e.field("event") == Some("count_tokens")),
            "a clean passthrough must not emit the class-decision event",
        );
    }
}

#[cfg(test)]
mod capability_override_filter_tests {
    //! Filter-seam tests for the operator override consult: legacy static
    //! lists keep their provenance labels, new override entries hard-drop
    //! or mask, and a `force_supported` mask precedes probe admission.
    use super::*;

    /// Minimal provider stub so the fixtures can build a real
    /// `Arc<ResolvedModel>`; none of its methods are exercised here.
    struct StubProvider;

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!("override filter tests never dispatch")
        }
        async fn stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("override filter tests never dispatch")
        }
    }

    fn override_router_from_toml(body: &str) -> Router {
        let config: Config =
            toml::from_str(&format!("version = 3\n{body}")).expect("config parses");
        Router::new(Arc::new(config))
    }

    /// A minimal openai-compat dispatch target keyed by `provider:nickname`.
    /// `into_one_dispatch_target` leaves `provider_kind` unset (the legacy
    /// path), so the kind is pinned here for the override / learned consult.
    fn override_test_target(provider_name: &str, nickname: &str) -> DispatchTarget {
        let provider: Arc<dyn Provider> = Arc::new(StubProvider);
        let model = Arc::new(ResolvedModel::new(
            nickname,
            provider_name,
            provider,
            "upstream",
        ));
        let mut target = into_one_dispatch_target(model);
        target.provider_kind = Some("openai-compat");
        target
    }

    const OVERRIDE_PROVIDER_P: &str = "[providers.p]\n\
        kind = \"openai-compat\"\n\
        base_url = \"https://x\"\n\
        api_key_ref = \"literal:k\"\n";

    #[test]
    fn override_consult_legacy_provider_list_hard_drops_with_provider_label() {
        // Arrange -- a legacy per-provider list. The registry preserves its
        // ProviderStatic provenance so the consult reports the same
        // `provider` source label the raw scan always did.
        let router = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}unsupported_features = [\"web_search\"]\n"
        ));
        let target = override_test_target("p", "nick");

        // Act
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let verdict = router.unsupported_feature_for_target(
            &target,
            &["web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        // Assert
        assert_eq!(
            verdict,
            Some(("web_search".to_string(), FilterSource::ProviderStatic)),
        );
        assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");
        assert!(admissions.is_empty());
        assert!(strip_keys.is_empty());
    }

    #[test]
    fn override_consult_legacy_model_list_hard_drops_with_model_label() {
        // Arrange -- a legacy per-model list keyed by `provider:nickname`.
        let router = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             unsupported_features = [\"computer_use\"]\n"
        ));
        let target = override_test_target("p", "nick");

        // Act
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let verdict = router.unsupported_feature_for_target(
            &target,
            &["computer_use".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        // Assert
        assert_eq!(
            verdict,
            Some(("computer_use".to_string(), FilterSource::ModelStatic)),
        );
        assert_eq!(FilterSource::ModelStatic.as_str(), "model");
    }

    #[test]
    fn override_unsupported_hard_drops_and_empties_chain_like_legacy_list() {
        // Arrange -- a NEW `[capability.overrides]` unsupported entry.
        let router = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
        ));
        let target = override_test_target("p", "nick");

        // Act / Assert -- the consult reports the `override` label and
        // hard-drops just as a static list does.
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["web_search".to_string()],
                &mut admissions,
                &mut strip_keys,
            ),
            Some(("web_search".to_string(), FilterSource::Override)),
        );
        assert_eq!(FilterSource::Override.as_str(), "override");

        // The sole target hard-drops, so the chain filters to empty and
        // surfaces the learned-tail NotImplemented (501) -- byte-identical to a legacy
        // static list emptying the chain.
        let mut chain_admissions = Vec::new();
        match router.filter_chain_by_features(
            vec![target],
            &["web_search".to_string()],
            "alias-x",
            &mut chain_admissions,
        ) {
            Err(Error::NotImplemented(_, _)) => {}
            Err(other) => panic!("expected NotImplemented, got {other:?}"),
            Ok(_) => panic!("an override-unsupported sole target must empty the chain"),
        }
    }

    #[test]
    fn force_supported_flips_acting_learned_route_away_to_allow() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // Arrange -- capability enabled with a self-identifying (acting-now)
        // negative on the target's state_key, plus a force_supported mask
        // for the same capability.
        let masked = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}\
             [capability]\n\
             enabled = true\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n"
        ));
        let target = override_test_target("p", "nick");
        masked.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );

        // Act
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let verdict = masked.unsupported_feature_for_target(
            &target,
            &["web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );

        // Assert -- the mask suppresses the acting negative: the feature is
        // Allowed (None), not routed away.
        assert_eq!(
            verdict, None,
            "force_supported must flip the negative to Allow"
        );

        // Contrast: the SAME acting negative without the mask routes away
        // with the learned source, proving the mask is what flipped it.
        let unmasked = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}[capability]\nenabled = true\n"
        ));
        unmasked.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let mut ctrl_admissions = Vec::new();
        let mut ctrl_strip = Vec::new();
        assert_eq!(
            unmasked.unsupported_feature_for_target(
                &override_test_target("p", "nick"),
                &["web_search".to_string()],
                &mut ctrl_admissions,
                &mut ctrl_strip,
            ),
            Some(("web_search".to_string(), FilterSource::Learned)),
        );
    }

    #[test]
    fn force_supported_mask_admits_no_probe_where_unmasked_would() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // A zero-hour decay lapses an observed negative immediately, so the
        // next consult would claim a re-probe slot.
        let base = "[providers.p]\n\
            kind = \"openai-compat\"\n\
            base_url = \"https://x\"\n\
            api_key_ref = \"literal:k\"\n\
            [capability]\n\
            enabled = true\n\
            decay_hours = 0\n\
            inferred_window_hours = 0\n";

        // Control -- unmasked: the lapsed negative admits exactly one probe.
        let control = override_router_from_toml(base);
        control.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let mut ctrl_admissions = Vec::new();
        let mut ctrl_strip = Vec::new();
        let _ = control.unsupported_feature_for_target(
            &override_test_target("p", "nick"),
            &["web_search".to_string()],
            &mut ctrl_admissions,
            &mut ctrl_strip,
        );
        assert_eq!(
            ctrl_admissions.len(),
            1,
            "control: a lapsed negative must admit a re-probe",
        );

        // Masked: the force_supported short-circuit precedes
        // acting_negative_for, so a masked cell never claims a probe slot.
        let masked = override_router_from_toml(&format!(
            "{base}[capability.overrides.p]\n\
                 force_supported = [\"web_search\"]\n"
        ));
        masked.learned_capabilities.observe(
            "nick",
            "web_search",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let verdict = masked.unsupported_feature_for_target(
            &override_test_target("p", "nick"),
            &["web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );
        assert_eq!(verdict, None, "masked feature must Allow");
        assert!(
            admissions.is_empty(),
            "a masked cell must not claim a re-probe slot",
        );
    }

    #[test]
    fn override_route_away_beats_learned_strip_for_non_overridden_precedence() {
        use routectl_core::capability::SignalTier;
        use std::time::Instant;

        // A per-provider override routes `web_search` away; a droppable
        // learned negative on `advisor` would otherwise strip in place.
        // Override RouteAway is consulted first, so it hard-drops (returns
        // the override label) ahead of the learned strip decision -- and the
        // non-overridden `advisor` cell keeps its strip-in-place behavior when
        // web_search is absent.
        let router = override_router_from_toml(&format!(
            "{OVERRIDE_PROVIDER_P}\
             [capability]\n\
             enabled = true\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
        ));
        let target = override_test_target("p", "nick");
        router.learned_capabilities.observe(
            "nick",
            "advisor",
            "openai-compat",
            SignalTier::SelfIdentifying,
            Instant::now(),
        );

        // With web_search present, the override hard-drops first.
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["advisor".to_string(), "web_search".to_string()],
                &mut admissions,
                &mut strip_keys,
            ),
            Some(("web_search".to_string(), FilterSource::Override)),
        );
        assert!(strip_keys.is_empty(), "a hard-drop leaves strip_keys empty");

        // Without web_search, the non-overridden advisor cell still strips
        // in place (behavior unchanged): None with the advisor key.
        let mut advisor_admissions = Vec::new();
        let mut advisor_strip = Vec::new();
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["advisor".to_string()],
                &mut advisor_admissions,
                &mut advisor_strip,
            ),
            None,
        );
        assert_eq!(advisor_strip, vec!["advisor".to_string()]);
    }

    /// Feature acceptance -- legacy-config filter-decision equivalence.
    ///
    /// One config carrying ALL three legacy capability lists (a per-provider
    /// `unsupported_features`, a per-model `unsupported_features`, and the
    /// `[bedrock]` egress allowlists `allowed_betas` / `allowed_body_fields`
    /// -- inert for routing but present so the whole legacy surface coexists)
    /// must route away with the SAME `FilterSource` labels the earlier raw
    /// static-list scan produced: a provider-scoped drop reports
    /// `ProviderStatic` (`"provider"`) and a model-scoped drop reports
    /// `ModelStatic` (`"model"`). Absolute expected labels, not a diff
    /// against a rebuilt old binary. The egress-byte half of this acceptance
    /// bar lives in `routectl-providers`
    /// (`tests/legacy_capability_config_equivalence.rs`).
    #[test]
    fn legacy_config_lists_route_away_with_pre_f3_source_labels() {
        // Arrange -- every legacy list in one config.
        let router = override_router_from_toml(
            "[providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             unsupported_features = [\"web_search\"]\n\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             unsupported_features = [\"computer_use\"]\n\
             [bedrock]\n\
             allowed_betas = [\"some-beta\"]\n\
             allowed_body_fields = [\"messages\", \"anthropic_version\", \"max_tokens\"]\n",
        );
        let target = override_test_target("p", "nick");

        // Act / Assert -- the provider-scoped list keeps the `provider` label.
        let mut admissions = Vec::new();
        let mut strip_keys = Vec::new();
        let provider_verdict = router.unsupported_feature_for_target(
            &target,
            &["web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );
        assert_eq!(
            provider_verdict,
            Some(("web_search".to_string(), FilterSource::ProviderStatic)),
        );
        assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");

        // The model-scoped list keeps the `model` label.
        let mut model_admissions = Vec::new();
        let mut model_strip = Vec::new();
        let model_verdict = router.unsupported_feature_for_target(
            &target,
            &["computer_use".to_string()],
            &mut model_admissions,
            &mut model_strip,
        );
        assert_eq!(
            model_verdict,
            Some(("computer_use".to_string(), FilterSource::ModelStatic)),
        );
        assert_eq!(FilterSource::ModelStatic.as_str(), "model");

        // A request touching no listed feature passes through: no route-away,
        // no probe admission, no strip -- byte-identical to the legacy
        // no-match path.
        let mut clean_admissions = Vec::new();
        let mut clean_strip = Vec::new();
        assert_eq!(
            router.unsupported_feature_for_target(
                &target,
                &["structured_output".to_string()],
                &mut clean_admissions,
                &mut clean_strip,
            ),
            None,
        );
        assert!(clean_admissions.is_empty());
        assert!(clean_strip.is_empty());
    }
}

#[cfg(test)]
mod strip_wire_egress_tests {
    //! Proves the strip-and-proceed ACT side is real end to end: a seeded
    //! learned negative drives a strip through the REAL anthropic-api egress
    //! against a wiremock upstream, and the OUTBOUND wire body is asserted --
    //! not an in-process request clone. The anthropic-api egress passes a
    //! built-in tool through verbatim as `AnthropicTool::Builtin`, so the
    //! advisor tool is wire-visible; a strip that removes it is observable on
    //! the bytes the upstream actually received. Also pins the capture leg's
    //! current dormancy: no droppable capability is learnable yet, because no
    //! grounded rejection envelope exists for one.

    use super::*;
    use crate::config::{ModelEntry, ProviderEntry};
    use crate::factory::{BuildOptions, build_resolved_models};
    use crate::learned_capability::ExportedEntry;
    use routectl_auth::{MemoryStore, SecretStore};
    use routectl_core::{Message, MessageContent, Role, ToolDef};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The grounded `anthropic_beta` token that enables context management.
    /// An operator floor pinning this token makes stripping the capability a
    /// false success (the egress re-adds it), so a pinned capability must
    /// route away instead of stripping.
    const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

    /// Single-attempt, fast-backoff retry so a chain walk falls back promptly
    /// without wall-clock sleeps.
    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            ..RetryPolicy::default()
        }
    }

    /// One chain member: a model nickname pointed at an anthropic-api provider
    /// whose `base_url` is a wiremock URL. `pinned_beta` seeds a provider
    /// `anthropic-beta` header floor so a beta-flag strip on that target would
    /// be re-added on the wire (the operator-pin path).
    struct WireUpstream {
        nickname: &'static str,
        provider_name: &'static str,
        base_url: String,
        pinned_beta: Option<&'static str>,
    }

    impl WireUpstream {
        fn plain(nickname: &'static str, provider_name: &'static str, base_url: &str) -> Self {
            Self {
                nickname,
                provider_name,
                base_url: base_url.to_string(),
                pinned_beta: None,
            }
        }

        fn pinned(
            nickname: &'static str,
            provider_name: &'static str,
            base_url: &str,
            beta: &'static str,
        ) -> Self {
            Self {
                pinned_beta: Some(beta),
                ..Self::plain(nickname, provider_name, base_url)
            }
        }
    }

    /// Build a router whose `alias` resolves to `chain` (nicknames in order),
    /// with `[capability]` enabled. Providers are real anthropic-api egresses
    /// pointed at the wiremock URLs; a `state_key` equals its model nickname.
    async fn build_wire_router(upstreams: &[WireUpstream], alias: &str, chain: &[&str]) -> Router {
        let mut providers = BTreeMap::new();
        let mut models = BTreeMap::new();
        for u in upstreams {
            let mut entry = ProviderEntry::anthropic_api(crate::test_secret::file_ref("test-key"))
                .with_base_url(&u.base_url);
            if let Some(beta) = u.pinned_beta {
                let mut headers = BTreeMap::new();
                headers.insert("anthropic-beta".to_string(), beta.to_string());
                entry = entry.with_header_extras(headers);
            }
            providers.insert(u.provider_name.to_string(), entry);
            models.insert(
                u.nickname.to_string(),
                ModelEntry::new(u.provider_name, "upstream-model"),
            );
        }

        let mut aliases = BTreeMap::new();
        let value = if chain.len() == 1 {
            AliasValue::Single(chain[0].to_string())
        } else {
            AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
        };
        aliases.insert(alias.to_string(), value);

        let mut cfg = Config {
            providers,
            models,
            aliases,
            retry: fast_retry(),
            ..Config::default()
        };
        cfg.capability.enabled = true;
        cfg.capability.decay_hours = 48;

        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("build_resolved_models");
        assert!(failed.is_empty(), "provider build failures: {failed:?}");

        let mut router = Router::new(Arc::new(cfg));
        router.install_resolved_models(resolved);
        router
    }

    /// A wiremock anthropic-api upstream answering `POST /v1/messages` with a
    /// single `(status, body)` on every call.
    async fn upstream(status: u16, body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    /// A minimal valid Anthropic Messages success body.
    fn anthropic_ok() -> Value {
        json!({
            "id": "msg_ok",
            "type": "message",
            "role": "assistant",
            "model": "upstream-model",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [{"type": "text", "text": "ok"}]
        })
    }

    /// A plausible advisor-tool rejection: a generic `invalid_request_error`
    /// whose free-text message names the advisor tool. It classifies as
    /// `BadRequest`, and the resolver has no grounded phrase to attribute it
    /// to a capability -- so today it produces no learn (the dormancy this
    /// suite pins).
    fn advisor_rejection_400() -> Value {
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "The advisor tool is not supported for this model."
            }
        })
    }

    fn user_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A request carrying an advisor built-in tool, so `derive_feature_keys`
    /// yields `advisor` and the anthropic-api egress emits the tool verbatim.
    fn advisor_req(alias: &str) -> ChatRequest {
        ChatRequest {
            model: alias.to_string(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            tools: Some(vec![ToolDef::Other(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            ..Default::default()
        }
    }

    /// A request carrying a `context_management` built-in tool (so the key is
    /// derived) plus the beta token an operator floor would re-add on the
    /// wire.
    fn context_management_req(alias: &str) -> ChatRequest {
        ChatRequest {
            model: alias.to_string(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            tools: Some(vec![ToolDef::Other(
                json!({"type": "context_management", "name": "cm"}),
            )]),
            anthropic_beta: vec![CONTEXT_MANAGEMENT_BETA.to_string()],
            ..Default::default()
        }
    }

    /// An acting (non-expired) self-identifying learned negative for
    /// `(state_key, feature_key)`. `feature_key` is stored verbatim -- the
    /// caller chooses a canonical or non-canonical token.
    fn acting_negative(state_key: &str, feature_key: &str) -> ExportedEntry {
        let base = Instant::now();
        ExportedEntry {
            state_key: state_key.into(),
            feature_key: feature_key.into(),
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base + Duration::from_hours(48),
            in_flight: false,
            consecutive_failed_probes: 0,
        }
    }

    /// The `type` strings of the built-in tools on an outbound wire body.
    fn wire_tool_types(body: &Value) -> Vec<String> {
        body.get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("type").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn hits(server: &MockServer) -> usize {
        server.received_requests().await.map_or(0, |r| r.len())
    }

    async fn last_request_body(server: &MockServer) -> Value {
        let reqs = server.received_requests().await.expect("received requests");
        let last = reqs.last().expect("at least one request received");
        serde_json::from_slice(&last.body).expect("outbound request body is JSON")
    }

    #[tokio::test]
    async fn advisor_strip_removes_tool_from_real_wire_body_and_succeeds() {
        // A seeded acting negative for the canonical `advisor` key drives a
        // strip through the REAL egress. The advisor tool the request carries
        // must be ABSENT from the bytes the upstream received, the request
        // must succeed, and the decision must surface `outcome = applied`.
        let a = upstream(200, anthropic_ok()).await;
        let router = build_wire_router(
            &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
            "solo",
            &["m_a"],
        )
        .await;
        router
            .learned_capabilities
            .import_entries(vec![acting_negative("m_a", "advisor")]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(advisor_req("solo"), RouterOptions::default()),
        )
        .await;

        assert!(
            d.result.is_ok(),
            "the stripped request must succeed on the real egress: {:?}",
            d.result.err(),
        );
        assert_eq!(hits(&a).await, 1);

        // WIRE GUARD: the advisor tool did not cross the wire after the strip.
        let sent = last_request_body(&a).await;
        assert!(
            !wire_tool_types(&sent).iter().any(|t| t == "advisor"),
            "the advisor tool must be removed from the outbound wire body; body = {sent}",
        );

        // The strip decision fired with the applied outcome -- not a
        // probe-bypass or a no-op.
        let warn = events
            .iter()
            .find(|e| e.message == "capability_strip_decision")
            .expect("a real strip must emit a capability_strip_decision WARN");
        assert_eq!(warn.level, tracing::Level::WARN);
        assert_eq!(warn.field("event"), Some("strip"));
        assert_eq!(warn.field("state_key"), Some("m_a"));
        assert_eq!(warn.field("capability_key"), Some("advisor"));
        assert_eq!(warn.field("outcome"), Some("applied"));
        assert_eq!(router.metrics.strip_total(), 1);
    }

    #[tokio::test]
    async fn non_canonical_registry_token_does_not_strip_advisor_tool() {
        // The strip is keyed on the canonical `advisor` capability. A negative
        // seeded under a different token never matches the request-derived
        // canonical key, so the advisor tool survives onto the wire and no
        // strip is applied -- the canonical-key guard.
        let a = upstream(200, anthropic_ok()).await;
        let router = build_wire_router(
            &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
            "solo",
            &["m_a"],
        )
        .await;
        router
            .learned_capabilities
            .import_entries(vec![acting_negative("m_a", "advisor_helper")]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(advisor_req("solo"), RouterOptions::default()),
        )
        .await;

        assert!(d.result.is_ok());
        let sent = last_request_body(&a).await;
        assert!(
            wire_tool_types(&sent).iter().any(|t| t == "advisor"),
            "a non-canonical registry key must not strip the advisor tool; body = {sent}",
        );
        assert_eq!(router.metrics.strip_total(), 0);
        assert!(
            events.iter().all(|e| {
                e.message != "capability_strip_decision" || e.field("outcome") != Some("applied")
            }),
            "no strip must be applied for a non-canonical registry key",
        );
    }

    #[tokio::test]
    async fn pinned_beta_capability_routes_away_instead_of_stripping() {
        // A's provider pins the context-management beta, so stripping the
        // capability would be silently re-added on the wire -- a false
        // success. The target must route away (tail-demoted) and B, which
        // carries no negative, serves first. A is never dialed, and no strip
        // is applied.
        let a = upstream(200, anthropic_ok()).await;
        let b = upstream(200, anthropic_ok()).await;
        let router = build_wire_router(
            &[
                WireUpstream::pinned("m_a", "prov_a", &a.uri(), CONTEXT_MANAGEMENT_BETA),
                WireUpstream::plain("m_b", "prov_b", &b.uri()),
            ],
            "chain",
            &["m_a", "m_b"],
        )
        .await;
        router
            .learned_capabilities
            .import_entries(vec![acting_negative("m_a", "context_management")]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(context_management_req("chain"), RouterOptions::default()),
        )
        .await;

        assert!(d.result.is_ok());
        assert_eq!(
            d.meta.served_provider.as_deref(),
            Some("prov_b"),
            "the pinned-beta target routes away; B serves",
        );
        assert_eq!(
            hits(&a).await,
            0,
            "a pinned-beta capability must route away, never be dialed and stripped",
        );
        assert_eq!(hits(&b).await, 1);
        assert_eq!(router.metrics.strip_total(), 0);
        assert!(
            events.iter().all(|e| {
                e.message != "capability_strip_decision" || e.field("outcome") != Some("applied")
            }),
            "a pinned capability must not be stripped",
        );
    }

    #[tokio::test]
    async fn strip_capture_loop_is_dormant_no_droppable_is_learnable() {
        // DORMANCY PIN. The act side (strip) is grounded and proven above, but
        // the CAPTURE side of the loop for a droppable capability is not: no
        // real advisor-rejection envelope has been captured, so the resolver
        // cannot attribute an advisor 400 to the `advisor` capability, and the
        // request-membership gate never gets the chance to admit a learn.
        //
        // This test drives a genuine advisor-tool request that crosses the
        // wire and meets a plausible capability rejection, and asserts NO
        // learn event occurs. When a grounded advisor rejection envelope is
        // captured and the resolver learns to attribute it, the request-
        // membership gate (the request derives `advisor`) will admit the
        // learn, this `is_empty` assertion will FAIL, and whoever grounds the
        // envelope must convert this dormant guard into the real capture-leg
        // end-to-end test that asserts the learn + subsequent strip. The
        // failure is the signal; the dormancy is load-bearing, not silent.
        let a = upstream(400, advisor_rejection_400()).await;
        let router = build_wire_router(
            &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
            "solo",
            &["m_a"],
        )
        .await;

        // No seed: this exercises the capture (learn) path, not the act path.
        let d = router
            .complete_with_options(advisor_req("solo"), RouterOptions::default())
            .await;

        assert!(
            matches!(d.result, Err(Error::Upstream { status: 400, .. })),
            "the upstream rejected the advisor request: {:?}",
            d.result,
        );
        // The advisor tool genuinely crossed the wire -- the rejection is for a
        // request that really carried the capability, not a stripped shape.
        let sent = last_request_body(&a).await;
        assert!(
            wire_tool_types(&sent).iter().any(|t| t == "advisor"),
            "the advisor tool must have crossed the wire; body = {sent}",
        );
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "a droppable capability is not yet learnable: no grounded rejection envelope exists",
        );
        assert!(
            router.learned_capabilities.is_empty(),
            "the capture path must leave the registry untouched while the loop is dormant",
        );
    }
}

#[cfg(test)]
mod probe_admission_settlement_tests {
    //! An admitted learned re-probe whose chain target the dispatch never
    //! reaches must still release its `in_flight` slot, so the next request
    //! re-probes rather than routing away until reload. The
    //! `ProbeAdmissionSet` settles every unreached admission as `OtherError` on
    //! drop. These tests drive `complete_inner` through the early-exit shapes
    //! -- success on an earlier target, a terminal non-fallbackable error,
    //! `break 'chain` under disable_fallbacks, and a dropped (cancelled)
    //! dispatch future -- and assert the unreached target's slot reset. A
    //! no-double-settle test pins the transfer: an admission the target guard
    //! settled is not settled again by the set. The settlement observability
    //! events assert the guard's own emissions too (reached success and a
    //! reached-then-dropped terminal).
    use super::*;
    use crate::config::{AliasValue, ProviderEntry};
    use crate::learned_capability::ExportedEntry;
    use crate::resolved::ResolvedModel;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{
        ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider,
        Role, ToolDef, Usage,
    };
    use serde_json::json;

    const PROVIDER_KIND: &str = "openai-compat";

    /// What a target's in-process provider returns, chosen per target so a
    /// test can steer the dispatch loop to leave a LATER target unreached.
    #[derive(Clone, Copy)]
    enum Behavior {
        /// 2xx success -> the loop returns at this target.
        Succeed,
        /// A non-`Upstream` error -> classifies as `Unknown` (retry 0,
        /// fallback false), so the loop returns terminally without a hop.
        FailTerminal,
        /// A fallbackable upstream 500 -> the loop would hop, but
        /// disable_fallbacks makes it return at this target.
        FailFallbackable,
        /// 2xx success, but only after a delay long enough for a test to drop
        /// the dispatch future mid-`complete` (leaving a later target
        /// unreached, or exercising the reached-then-cancelled path).
        SucceedAfter(Duration),
    }

    /// An in-process provider whose `complete` outcome is fixed at
    /// construction, so a test controls the dispatch path without a wire.
    struct ScriptedProvider {
        id: String,
        behavior: Behavior,
    }

    impl ScriptedProvider {
        /// The canonical 2xx response echoing the request model.
        fn ok_response(req: ChatRequest) -> ChatResponse {
            ChatResponse {
                id: "ok".into(),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
                        role: Role::Assistant,
                        content: MessageContent::Text("ok".into()),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                    finish_reason: Some("stop".into()),
                    matched_stop_sequence: None,
                }],
                usage: Some(Usage::default()),
                routectl_provider: None,
                extras: Default::default(),
                upstream_meta: None,
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            match self.behavior {
                Behavior::Succeed => Ok(Self::ok_response(req)),
                Behavior::FailTerminal => Err(Error::normalize_response(&self.id, "terminal")),
                Behavior::FailFallbackable => Err(Error::upstream(&self.id, 500, "boom")),
                Behavior::SucceedAfter(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(Self::ok_response(req))
                }
            }
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(Error::normalize_response(&self.id, "stream unused"))
        }
    }

    /// A lapsed (already-expired) self-identifying negative: acting AND due
    /// for a re-probe, so the chain filter admits it and flips `in_flight`.
    fn lapsed_negative(state_key: &str, feature_key: &str) -> ExportedEntry {
        let base = Instant::now();
        ExportedEntry {
            state_key: state_key.into(),
            feature_key: feature_key.into(),
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base.checked_sub(Duration::from_secs(1)).unwrap_or(base),
            in_flight: false,
            consecutive_failed_probes: 0,
        }
    }

    /// Build a router whose alias `chain` resolves to the given
    /// `(nickname, provider_name, behavior)` targets in order, `[capability]`
    /// enabled. Each dispatches to an in-process `ScriptedProvider`; a
    /// `state_key` equals its nickname and every provider is registered
    /// openai-compat so the learned registry sees a provider kind.
    fn build_router(targets: &[(&str, &str, Behavior)]) -> Router {
        let mut providers = BTreeMap::new();
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        for (nickname, provider_name, behavior) in targets {
            providers.insert(
                (*provider_name).to_string(),
                ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k"),
            );
            let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
                id: (*provider_name).to_string(),
                behavior: *behavior,
            });
            models.insert(
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    *provider_name,
                    provider,
                    "upstream-model",
                )),
            );
        }
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "chain".to_string(),
            AliasValue::Chain(targets.iter().map(|(n, _, _)| (*n).to_string()).collect()),
        );
        let mut cfg = Config {
            providers,
            aliases,
            ..Config::default()
        };
        cfg.capability.enabled = true;
        let mut router = Router::new(Arc::new(cfg));
        router.install_resolved_models(models);
        router
    }

    /// A request against the `chain` alias carrying a `web_search` built-in
    /// tool, so `derive_feature_keys` yields `[web_search]` and the seeded
    /// negative is consulted.
    fn req_with_web_search() -> ChatRequest {
        ChatRequest {
            model: "chain".into(),
            messages: vec![],
            tools: Some(vec![ToolDef::Other(
                json!({"type": "web_search", "name": "t"}),
            )]),
            ..Default::default()
        }
    }

    /// The resident registry entry for `(state_key, feature_key)`, or a panic
    /// -- an unreached OtherError settle keeps the entry, so it must persist.
    fn probe_entry(router: &Router, state_key: &str, feature_key: &str) -> ExportedEntry {
        router
            .learned_capabilities
            .export_entries()
            .into_iter()
            .find(|e| e.state_key == state_key && e.feature_key == feature_key)
            .expect("the seeded negative must still be resident after dispatch")
    }

    #[tokio::test]
    async fn success_on_earlier_target_releases_unreached_admission() {
        // m_a succeeds at the head; m_b (admitted for a re-probe) is never
        // reached. Its slot must reset so the next request re-probes m_b.
        let router = build_router(&[
            ("m_a", "prov_a", Behavior::Succeed),
            ("m_b", "prov_b", Behavior::Succeed),
        ]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_b", &cap)]);

        let d = router
            .complete_with_options(req_with_web_search(), RouterOptions::default())
            .await;
        assert!(d.result.is_ok(), "m_a should succeed: {:?}", d.result.err());

        assert!(
            !probe_entry(&router, "m_b", &cap).in_flight,
            "an admission the loop never reached must release in_flight",
        );
    }

    #[tokio::test]
    async fn terminal_error_on_earlier_target_releases_unreached_admission() {
        // m_a returns a non-fallbackable terminal error; the loop returns
        // without hopping to the admitted m_b, whose slot must still reset.
        let router = build_router(&[
            ("m_a", "prov_a", Behavior::FailTerminal),
            ("m_b", "prov_b", Behavior::Succeed),
        ]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_b", &cap)]);

        let d = router
            .complete_with_options(req_with_web_search(), RouterOptions::default())
            .await;
        assert!(
            d.result.is_err(),
            "a non-fallbackable terminal error must not fall back",
        );

        assert!(
            !probe_entry(&router, "m_b", &cap).in_flight,
            "a terminal early return must release the unreached admission",
        );
    }

    #[tokio::test]
    async fn break_under_disable_fallbacks_releases_unreached_admission() {
        // m_a fails with a fallbackable error, but disable_fallbacks breaks the
        // chain before the hop; the admitted m_b is never reached.
        let router = build_router(&[
            ("m_a", "prov_a", Behavior::FailFallbackable),
            ("m_b", "prov_b", Behavior::Succeed),
        ]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_b", &cap)]);

        let mut opts = RouterOptions::new();
        opts.disable_fallbacks = true;
        let d = router
            .complete_with_options(req_with_web_search(), opts)
            .await;
        assert!(
            d.result.is_err(),
            "disable_fallbacks propagates the failure"
        );

        assert!(
            !probe_entry(&router, "m_b", &cap).in_flight,
            "a disable_fallbacks break must release the unreached admission",
        );
    }

    #[tokio::test]
    async fn reached_admission_settled_by_guard_not_by_set() {
        // A solo target reached and settled by its own LearnedProbeGuard (a 2xx
        // clears the negative) emits EXACTLY ONE probe-settlement event -- from
        // the guard (reached_target=true, outcome=success) -- and NOT a second
        // from the set's drop. The take() move makes exact-once settlement
        // structural, so a second event (reached_target=false) would prove a
        // double-settle.
        let router = build_router(&[("m_a", "prov_a", Behavior::Succeed)]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_a", &cap)]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(req_with_web_search(), RouterOptions::default()),
        )
        .await;
        assert!(d.result.is_ok(), "the re-probe reaches m_a and succeeds");

        assert!(
            router
                .learned_capabilities
                .export_entries()
                .iter()
                .all(|e| !(e.state_key == "m_a" && e.feature_key == cap)),
            "a successful re-probe clears the negative via the target guard",
        );
        let settlements: Vec<_> = events
            .iter()
            .filter(|e| e.field("event") == Some("probe_settlement"))
            .collect();
        assert_eq!(
            settlements.len(),
            1,
            "a reached admission settles exactly once (guard only, no set double-settle): {events:?}",
        );
        let ev = settlements[0];
        assert_eq!(ev.field("state_key"), Some("m_a"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("outcome"), Some("success"));
        assert_eq!(ev.field("reached_target"), Some("true"));
        assert_eq!(ev.field("reason"), Some("success"));
    }

    #[tokio::test]
    async fn reached_terminal_drop_emits_terminal_settlement() {
        // A solo target reached by the loop but terminated by a non-capability
        // error is neither a success nor a same-capability settle, so its guard
        // drops with the admission still held: the guard's Drop emits one
        // probe-settlement event tagged reached-then-dropped (outcome=other_error,
        // reached_target=true, reason=terminal) and releases the slot.
        let router = build_router(&[("m_a", "prov_a", Behavior::FailTerminal)]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_a", &cap)]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(req_with_web_search(), RouterOptions::default()),
        )
        .await;
        assert!(d.result.is_err(), "the terminal error returns terminally");

        assert!(
            !probe_entry(&router, "m_a", &cap).in_flight,
            "a reached-then-dropped admission must release in_flight",
        );
        let ev = events
            .iter()
            .find(|e| e.field("event") == Some("probe_settlement"))
            .expect("the guard drop must emit a probe-settlement event");
        assert_eq!(ev.field("state_key"), Some("m_a"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("outcome"), Some("other_error"));
        assert_eq!(ev.field("reached_target"), Some("true"));
        assert_eq!(ev.field("reason"), Some("terminal"));
    }

    #[tokio::test]
    async fn future_drop_releases_unreached_admission() {
        // m_a succeeds only after a long delay; the dispatch future is dropped
        // ~150ms in, while it is still awaiting m_a's completion, so the
        // admitted tail m_b is never reached. Dropping the future runs the set
        // destructor on this current-thread runtime, settling the unreached
        // admission under the capture subscriber (in_flight reset + one
        // reached_target=false / reason=unreached event).
        let router = build_router(&[
            (
                "m_a",
                "prov_a",
                Behavior::SucceedAfter(Duration::from_secs(2)),
            ),
            ("m_b", "prov_b", Behavior::Succeed),
        ]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_b", &cap)]);

        let ((), events) = routectl_testkit::with_capture(async {
            let fut = router.complete_with_options(req_with_web_search(), RouterOptions::default());
            let cancelled = tokio::time::timeout(Duration::from_millis(150), fut).await;
            assert!(
                cancelled.is_err(),
                "the slow completion must keep the future pending until it is dropped",
            );
        })
        .await;

        assert!(
            !probe_entry(&router, "m_b", &cap).in_flight,
            "dropping the dispatch future must release the unreached admission",
        );
        let ev = events
            .iter()
            .find(|e| e.field("event") == Some("probe_settlement"))
            .expect("the dropped future's set destructor must emit a probe-settlement event");
        assert_eq!(ev.field("state_key"), Some("m_b"));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("outcome"), Some("other_error"));
        assert_eq!(ev.field("reached_target"), Some("false"));
        assert_eq!(ev.field("reason"), Some("unreached"));
    }

    #[tokio::test]
    async fn unreached_admission_emits_probe_settlement_event() {
        // The set drop emits one probe-settlement debug event per unreached
        // admission, carrying the full field set.
        let router = build_router(&[
            ("m_a", "prov_a", Behavior::Succeed),
            ("m_b", "prov_b", Behavior::Succeed),
        ]);
        let cap = normalize_capability_key("web_search", PROVIDER_KIND);
        router
            .learned_capabilities
            .import_entries(vec![lapsed_negative("m_b", &cap)]);

        let (d, events) = routectl_testkit::with_capture(
            router.complete_with_options(req_with_web_search(), RouterOptions::default()),
        )
        .await;
        assert!(d.result.is_ok());

        let ev = events
            .iter()
            .find(|e| e.field("event") == Some("probe_settlement"))
            .expect("the set drop must emit a probe-settlement event for the unreached admission");
        assert_eq!(ev.level, tracing::Level::DEBUG);
        assert_eq!(ev.field("state_key"), Some("m_b"));
        assert_eq!(ev.field("capability_key"), Some(cap.as_str()));
        assert_eq!(ev.field("provider_kind"), Some(PROVIDER_KIND));
        assert_eq!(ev.field("surface"), Some("complete"));
        assert_eq!(ev.field("outcome"), Some("other_error"));
        assert_eq!(ev.field("reached_target"), Some("false"));
        assert_eq!(ev.field("reason"), Some("unreached"));
    }
}
