//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount,
    cache_control::{MAX_BREAKPOINTS, validate_source},
    context_reduction::{ReductionOutcome, apply_json_minify},
    failure_class::{ClassifiedFailure, FailureClass, LastOutcome, MatchedBy, classify},
    sanitize_for_log,
};
use serde_json::Value;

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
use crate::runtime_state::ProviderState;

mod cache_plan;
mod capability_learn;
mod chain;
mod count_tokens;
mod feature_filter;
mod overlays;
mod runtime_gate;
mod status;
use cache_plan::{AutoCacheRequestPlan, CacheInjection};
pub use capability_learn::CapabilityLearnEvent;
#[cfg(test)]
use feature_filter::FilterSource;
use feature_filter::{StripDecision, catalog_capabilities, emit_feature_unsupported};
use overlays::{apply_layered_overlays, operator_betas};
pub use overlays::{merge_header_extras, merge_payload_extras};
#[cfg(test)]
use routectl_core::capability::SignalTier;
use runtime_gate::{
    LearnedProbeGuard, ProbeAdmission, ProbeAdmissionSet, is_probe_request, log_probe_fast_fail,
};
pub use status::RouteTargetStatus;

#[cfg(test)]
pub(crate) use crate::runtime_state::CircuitPhase;
#[cfg(test)]
use crate::runtime_state::GateDecision;

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

    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let name = name.into();
        // Ensure a gate exists even for providers registered without a
        // matching config entry (test harnesses rely on this).
        self.state
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&Default::default()))));
        self.providers.insert(name, provider);
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
                                    if self.record_failure_opened(
                                        state_key,
                                        LastOutcome::from_failure_class(&cf.class),
                                    ) {
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
                            st.lock().record_success(Instant::now());
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
                        return Ok(wrap_with_breaker_accounting(
                            relabeled.boxed(),
                            state,
                            target.provider_kind,
                        ));
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
                                _ => self.record_failure(
                                    state_key,
                                    LastOutcome::from_failure_class(&cf.class),
                                ),
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
    provider_kind: Option<&'static str>,
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
            self.with_state(|state| state.record_success(Instant::now()));
        }

        fn record_failure(&mut self, outcome: LastOutcome) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| {
                state.record_failure(Instant::now(), outcome);
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
            if let Err(e) = &item {
                accounting.record_failure(LastOutcome::from_failure_class(
                    &classify(e, provider_kind).class,
                ));
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
use chain::{dispatch_target_for_seat, into_one_dispatch_target};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "remap_tests.rs"]
mod remap_tests;

#[cfg(test)]
#[path = "resolved_models_tests.rs"]
mod resolved_models_tests;

#[cfg(test)]
#[path = "has_forwarded_provider_tests.rs"]
mod has_forwarded_provider_tests;

#[cfg(test)]
#[path = "forwarded_model_transparency_tests.rs"]
mod forwarded_model_transparency_tests;

#[cfg(test)]
#[path = "seat_pool_dispatch_tests.rs"]
mod seat_pool_dispatch_tests;

#[cfg(test)]
#[path = "auth_failure_recovery_tests.rs"]
mod auth_failure_recovery_tests;

#[cfg(test)]
#[path = "forwarded_auth_terminal_tests.rs"]
mod forwarded_auth_terminal_tests;

#[cfg(test)]
#[path = "auto_emit_cache_control_tests.rs"]
mod auto_emit_cache_control_tests;

#[cfg(test)]
#[path = "context_reduction_dispatch_tests.rs"]
mod context_reduction_dispatch_tests;

#[cfg(test)]
#[path = "forwarded_coexistence_tests.rs"]
mod forwarded_coexistence_tests;

#[cfg(test)]
#[path = "k_query_key_tests.rs"]
mod k_query_key_tests;

#[cfg(test)]
#[path = "observability_seam_tests.rs"]
mod observability_seam_tests;

#[cfg(test)]
#[path = "remap_test_support.rs"]
mod remap_test_support;

#[cfg(test)]
#[path = "provider_remap_tests.rs"]
mod provider_remap_tests;

#[cfg(test)]
#[path = "bedrock_class_remap_tests.rs"]
mod bedrock_class_remap_tests;
