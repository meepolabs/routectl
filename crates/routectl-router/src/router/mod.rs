//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, failure_class::FailureClass};
use serde_json::Value;

use crate::config::{AliasValue, Config, HistoryReasoning, ReasoningDialect};
use crate::glob::PrefixIndex;
use crate::resolved::ResolvedModel;
use crate::runtime_state::ProviderState;

mod cache_plan;
mod capability_learn;
mod chain;
mod count_tokens;
mod dispatch;
mod feature_filter;
mod overlays;
mod runtime_gate;
mod status;
mod sticky;
pub use capability_learn::CapabilityLearnEvent;
#[cfg(test)]
use feature_filter::FilterSource;
use feature_filter::{StripDecision, catalog_capabilities};
use overlays::{apply_layered_overlays, operator_betas};
pub use overlays::{merge_header_extras, merge_payload_extras};
#[cfg(test)]
use routectl_core::capability::SignalTier;
use runtime_gate::{LearnedProbeGuard, ProbeAdmission};
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

    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let name = name.into();
        // Ensure a gate exists even for providers registered without a
        // matching config entry (test harnesses rely on this).
        self.state
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&Default::default()))));
        self.providers.insert(name, provider);
    }
}

#[cfg(test)]
use chain::{dispatch_target_for_seat, into_one_dispatch_target};
#[cfg(test)]
use dispatch::{
    DispatchSurface, retry_cap_for, should_fallback, should_retry_same_provider,
    would_trim_k_floor_for_meta,
};
#[cfg(test)]
use futures::stream::StreamExt;
#[cfg(test)]
use routectl_core::failure_class::classify;
#[cfg(test)]
use routectl_core::{ChatRequest, Error};
#[cfg(test)]
use std::time::Instant;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

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
