//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
// The registry tempo now flows through `LearnedCapabilityRegistry::
// from_capability_config`; `Duration` survives only for the router test
// sidecars that build durations and reach it through `use super::*`.
#[cfg(test)]
use std::time::Duration;

use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatResponse, PrefixComponent, Provider, ReplayScheme, Result, VolatileKind,
    failure_class::FailureClass,
};
use serde_json::Value;

use crate::config::{AliasValue, Config, HistoryReasoning, ReasoningDialect};
use crate::glob::PrefixIndex;
use crate::resolved::ResolvedModel;
use crate::runtime_state::ProviderState;

mod cache_plan;
mod capability_cleared;
mod capability_learn;
mod capability_observe;
mod chain;
mod class_observe;
mod count_tokens;
mod dispatch;
mod feature_filter;
mod overlays;
mod prefix_rewrite;
mod replay_repair;
mod runtime_gate;
mod status;
mod sticky;
mod window_gate;
pub use capability_cleared::CapabilityClearedEvent;
pub use capability_learn::CapabilityLearnEvent;
pub use capability_observe::CapabilityObserveEvent;
pub use dispatch::class_debits;
use dispatch::k_query_key;
#[cfg(test)]
use feature_filter::FilterSource;
use feature_filter::{StripDecision, catalog_capabilities};
use overlays::{apply_layered_overlays, operator_betas};
pub use overlays::{merge_header_extras, merge_payload_extras};
use routectl_core::capability::FailurePhase;
#[cfg(test)]
use routectl_core::capability::SignalTier;
use runtime_gate::{LearnedProbeGuard, ProbeAdmission};
pub use status::RouteTargetStatus;

#[cfg(test)]
use crate::runtime_state::CircuitPhase;
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

/// The routing engine: resolves aliases, holds provider implementations
/// and their per-model runtime gates, and drives dispatch over fallback
/// chains.
pub struct Router {
    /// The configuration this router was built from.
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
    /// `seat_selection` is `RoundRobin`. One `AtomicUsize` per POOL, so two
    /// models naming one pool advance the SAME cursor and their traffic
    /// interleaves across the pool's accounts. Advanced once per request to
    /// rotate the starting seat. `FillFirst` pools and non-pooled models have
    /// no entry here.
    ///
    /// Carried across a hot-reload for the pools the new config still declares
    /// (see `carry_over_pool_state_from`): each cursor sits behind its own
    /// `Arc`, so a request still holding the outgoing Router advances the same
    /// atomic the incoming one reads and no seat is handed out twice across
    /// the swap. A pool the new config dropped -- including a RENAMED one,
    /// which is a new pool -- has no key in the fresh map and starts at seat 0.
    round_robin: crate::seat_pool::RoundRobinCursors,
    /// Bounded LRU map of pin lookup key -> pinned pool MEMBER, for
    /// `StickyLeastLoaded` selection. Keyed per session per POOL, so a session
    /// stays on one account across every model of that pool. These pins MUST
    /// survive a hot-reload: dropping
    /// them would scatter every live conversation off its warm-cache seat,
    /// causing a mass cold-miss.
    ///
    /// Held behind an `Arc` so `carry_over_sticky_from` can SHARE the map
    /// with the outgoing Router rather than copying its entries: a pin
    /// written through the outgoing Router in the window between the
    /// carry-over and the swap lands in the same map the incoming Router
    /// reads. See `carry_over_sticky_from`.
    sticky_pins: Arc<crate::seat_pool::StickyPins>,
    /// Per-session K-estimator window store, sibling to `sticky_pins`.
    /// Triple-keyed by (session, provider_kind, model) so a session that
    /// switches provider or model does not bleed its cache-reuse history
    /// onto the new triple. MUST survive a hot-reload for the same reason
    /// the sticky pins do: a wipe collapses every learned estimate back to
    /// `Cold` and silently un-arms the cost gate. See
    /// `carry_over_k_store_from`.
    ///
    /// Held behind an `Arc` so the in-process [`crate::k_estimator::KEstimator`] reader
    /// (`k_estimator` below) can share the SAME store as the dispatch path
    /// that records samples into it -- the reader observes every sample the
    /// writer lands, without any cross-store copy or refresh.
    pub k_session_store: Arc<crate::k_estimator::KSessionStore>,
    /// K-estimator reader over `k_session_store`. The constructor wires the
    /// default [`crate::k_estimator::LedgerBackedK`] over a clone of the
    /// `k_session_store` `Arc`, so a sample recorded into the store is
    /// immediately visible to the next `estimate(...)` call.
    ///
    /// No carry-over field of its own: `carry_over_k_store_from` replaces
    /// both `k_session_store` (with the previous Router's shared `Arc`) and
    /// this field (with a fresh [`crate::k_estimator::LedgerBackedK`] bound
    /// to that same shared `Arc`). The rebind is REQUIRED -- without it this
    /// field would keep pointing at the fresh Router's own store, which the
    /// carry-over is in the process of discarding, and every `estimate(...)`
    /// call would read an empty map.
    k_estimator: Arc<dyn crate::k_estimator::KEstimator>,
    /// In-process session-keyed last-fingerprint store for the shadow misfire
    /// monitor. Keyed by the same (session, provider_kind, model) triple as
    /// `k_session_store`. Not carried over on a hot-reload: the monitor
    /// treats the first turn after a reload as `FirstSeen`, which is the
    /// safe default (no false misfire on a fresh fingerprint after a reload).
    shadow_store: Arc<crate::k_estimator::ShadowStore>,
    /// In-process prefix-epoch store for the prefix-rewrite detector, keyed by
    /// the inbound session key ALONE (see `prefix_rewrite`): the question is
    /// whether the CLIENT rewrote its own bytes, which does not vary by
    /// dispatch target, so triple keying would mint a phantom epoch on every
    /// fallback hop.
    ///
    /// MUST survive a hot-reload (see `carry_over_prefix_epochs_from`). A wiped
    /// store treats every live session's next turn as first-seen, and a
    /// first-seen turn emits no classification at all -- so the loss shows up
    /// as an absence of findings, which is indistinguishable from healthy
    /// traffic. Restart is the bounded false-negative window the detector
    /// accepts; a reload must not widen it.
    ///
    /// Carried by SHARING the `Arc` rather than copying entries: a request that
    /// started against the outgoing Router observes into the same store the
    /// incoming one reads, so an observation landing during the swap is neither
    /// lost nor double-counted.
    prefix_epoch_store: Arc<prefix_rewrite::PrefixRewriteStore>,
    /// Per-lane token-estimate correction evidence, keyed by
    /// `(provider_kind, served nickname)`. Read at the context-window gate's
    /// decision point to correct the estimate it compares, written
    /// post-response from the ingress capture path.
    ///
    /// Not the LRU shape `k_session_store` uses: a lane is an
    /// operator-declared nickname on one of the four provider kinds, so the
    /// keyspace is bounded by the models ever declared in this process
    /// rather than by client traffic.
    ///
    /// MUST survive a hot-reload (see `carry_over_calibration_from`): a wipe
    /// sends every lane back to the uncorrected estimate, and because that IS
    /// the pre-correction behavior the loss reads as health. Carried by
    /// SHARING the `Arc` and then actively pruning any lane the new
    /// resolved-model table no longer serves -- see the store's own doc for
    /// the bounded leak that sharing (rather than a copy) accepts.
    calibration_store: Arc<crate::calibration::CalibrationStore>,
    /// Latest per-seat subscription-quota reading, keyed by the OAuth
    /// `provider#label` ACCOUNT identity (see `crate::quota::key`) and NOT by
    /// the model-scoped `state_key`: quota is reported once per credential
    /// account, so a model-scoped key would shard one account's single reading
    /// across one entry per nickname.
    ///
    /// One latest snapshot per seat, not a ring and not an LRU: there is no
    /// reduction over history here (the freshest reading IS the answer), and the
    /// keyspace is the credential-store-declared seat set rather than anything
    /// client-driven.
    ///
    /// MUST survive a hot-reload (see `carry_over_quota_from`): an emptied store
    /// reads exactly as a fleet of seats that have not reported yet, which is
    /// the cap-dormant fallback -- so the loss would look like health. Carried by
    /// SHARING the `Arc` and then re-declaring write admission against the new
    /// config -- see the store's own doc for the carry-over contract.
    quota_store: Arc<crate::quota::store::QuotaStore>,
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
    /// Reasoning-replay lifecycle over `learned_capabilities`: per-pair
    /// single-flight admission plus the two-phase learn the dispatch repair
    /// arm drives. Holds only the in-flight coordination set -- every
    /// persisted negative lives in the registry above, so a replay negative
    /// carries across a hot reload on the same terms as any other.
    learned_replay: Arc<crate::learned_replay::ReplayLearnRegistry>,
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
    /// `install_catalog_overlay` records the revision the resolved-model
    /// table was stamped with. Compared old-vs-new in
    /// `carry_over_learned_from`: a change invalidates the learned registry.
    overlay_revision: u64,
    /// The catalog overlay this Router's resolved-model table was merged
    /// against -- the generation the daemon ACCEPTED at the last successful
    /// boot or reload. Empty (revision 0) until `install_catalog_overlay`
    /// records one, which is the only writer of it and of
    /// `overlay_revision`, so the two can never diverge.
    ///
    /// Retained (rather than re-read from disk by each reader) so a caller
    /// asking "what catalog truth is IN EFFECT" gets the accepted
    /// generation and not whatever currently sits on disk -- an overlay an
    /// operator edited but the daemon REJECTED is by definition not in
    /// effect, and a corrupt file on disk must not make the in-effect
    /// answer unavailable. Costs one refcount: every build path already
    /// holds the overlay behind an `Arc`.
    catalog_overlay: Arc<crate::catalog_overlay::CatalogOverlay>,
    /// Lock-free router-side observability counters. Carried over on a
    /// hot-reload rebuild by `carry_over_metrics_from`, which shares this
    /// `Arc` rather than resetting it: unlike `round_robin`, these counters
    /// back an operator-facing snapshot, so a reload that silently zeroed
    /// them would make a diffed rate compute garbage with no indication why.
    metrics: Arc<RouterMetrics>,
    /// `true` iff `config.providers` contains a `ProviderEntry::AnthropicApi`
    /// with `credential_source == Forwarded`. Computed ONCE here at
    /// construction (a full `config.providers` scan) rather than re-scanned
    /// per request -- this is the "configured capability" half of the
    /// forwarded-mode CAPTURE gate (`forwarded_capture_armed` in
    /// routectl-cli), replacing the removed `[mitm] credential_source` read.
    has_forwarded_provider: bool,
    /// Edge-trigger dedup for the `cache_volatile_in_caller_prefix` advisory
    /// WARN. Keyed by (structural component, volatile kind); a key admits at
    /// most one WARN per process. Bounded by construction (3 components x the
    /// fixed set of high-precision kinds, so <= a dozen entries -- no eviction
    /// needed) and deliberately per-process rather than per-session: a
    /// per-session set grows with sessions and would need the bounded-store
    /// machinery `shadow_store` carries, which is disproportionate for a
    /// warn-only diagnostic. Reset on a Router rebuild (re-warns once after a
    /// hot-reload), benign like the `round_robin` reset.
    volatile_prefix_warned: Mutex<HashSet<(PrefixComponent, VolatileKind)>>,
    /// Edge-trigger dedup for the `cache_prefix_rewritten_in_epoch` advisory
    /// WARN. A single latch, not a keyed set: the detector has one component,
    /// so the first in-epoch rewrite ANY session shows warns and later ones
    /// stay silent. Per-PROCESS rather than per-session for the same reason as
    /// `volatile_prefix_warned`, and the `prefix_epoch_event` ledger column
    /// carries the suppressed volume.
    ///
    /// Shared across a hot-reload alongside `prefix_epoch_store` (see
    /// `carry_over_prefix_epochs_from`), so once-per-process holds through a
    /// reload -- a per-Router latch would re-warn on every config reload and
    /// turn a one-shot diagnostic into a reload-frequency signal.
    prefix_rewrite_warned: Arc<AtomicBool>,
    /// The sanitized per-pool build reports this Router was built from. Empty
    /// for a config with no pools, and for any caller that built the resolved
    /// table through the report-discarding entry point.
    pool_reports: Vec<crate::pool_build::PoolReport>,
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
    /// Learned negatives that reached the acting state via the F1 wire-token
    /// phase (a droppable capability the provider named directly). The
    /// per-phase split of
    /// [`learned_negatives_total`](Self::learned_negatives_total), so a rising
    /// F2 share is visible against the well-understood F1 baseline. Bumped by
    /// the learn path.
    learned_negatives_f1_total: AtomicU64,
    /// Learned negatives that reached the acting state via the F2
    /// feature-naming phase (a capability the provider named in prose). The
    /// per-phase split of
    /// [`learned_negatives_total`](Self::learned_negatives_total); zero until an
    /// F2 pattern is grounded (the tables ship empty). Bumped by the learn path.
    learned_negatives_f2_total: AtomicU64,
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
    /// Flat Bedrock `ValidationException` 400s the capability matcher could
    /// not attribute to any anchored-template capability. A rising count
    /// means the AWS validation wording drifted (or a new rejection shape
    /// appeared) and the template table needs a captured-envelope refresh --
    /// visible drift instead of silently reintroduced repeat 400s. Bumped
    /// once per request per target by the learn path.
    bedrock_validation_unmatched_total: AtomicU64,
    /// F2 feature-naming candidates dropped because an ACTING F1 negative for
    /// the same capability was already observed earlier in the SAME attempt
    /// chain (a cross-lane fallback must not blind-mint an F2 after an F1
    /// strip on a sibling lane). Bumped once per request per capability
    /// (cross-lane dedupe) by the learn path when it suppresses the F2 observe.
    f2_same_chain_suppressed_total: AtomicU64,
    /// Deterministic (400/422) feature-carrying rejections against a provider
    /// that HAS a feature-naming (F2) table, which matched no template. A
    /// rising count means a real feature-naming rejection shape is arriving
    /// that the shipped-empty table cannot attribute -- the drift signal that
    /// the F2 table needs a captured-envelope refresh, mirroring
    /// [`bedrock_validation_unmatched_total`](Self::bedrock_validation_unmatched_total).
    /// Bumped once per request per target by the learn path.
    feature_naming_unmatched_total: AtomicU64,
    /// Response-evidence VerifiedWorking positives that reached the acting
    /// state (a fresh or refreshed positive; structural proof acts on N=1).
    /// Bumped once per acting positive observation by the response-evidence
    /// observer on the terminal successful non-streaming dispatch.
    verified_working_total: AtomicU64,
    /// Response-evidence F3 suspected-absence negatives that reached the
    /// acting state (inferred, so acting only once corroborated within the
    /// window). Advisory-only under the routing gate. Bumped once per acting
    /// F3 observation by the response-evidence observer.
    f3_suspect_total: AtomicU64,
    /// Chain targets the proactive context-window gate skipped before
    /// dispatch (the estimated request clearly exceeded the target's
    /// catalog window). The authoritative skip count: the skip WARN is
    /// rate-limited per process, this is not. Bumped once per skipped
    /// target by the window gate.
    window_gate_skips_total: AtomicU64,
    /// Birth picks the subscription-quota partition decided by restricting to
    /// the fresh-known-below-cap tier. Bumped once per birth pick by the
    /// sticky chooser.
    quota_placement_below_cap_total: AtomicU64,
    /// Birth picks where every eligible seat was fresh-known and every one was
    /// at or above its cap, so the pick took the most remaining and the
    /// request was NOT failed. The soft-cap-never-fails path made visible.
    quota_placement_all_capped_total: AtomicU64,
    /// Birth picks that fell through to the unchanged capacity ranking on a
    /// MIX of capped-known and unknown seats. Distinguished from the
    /// all-unknown case because the two mean different things operationally:
    /// this one says the pool is partially observed, which on a steady pool is
    /// a signal the feed is missing a seat.
    quota_placement_mixed_unknown_total: AtomicU64,
    /// Birth picks that fell through because EVERY eligible seat was unknown.
    /// The expected state of a fresh process and of an uncurated provider, so
    /// a high count here is not by itself a fault.
    quota_placement_all_unknown_total: AtomicU64,
    /// Dispatch targets that cleared the proactive window gate
    /// ([`window_gate_skips_total`](Self::window_gate_skips_total)) and then
    /// still hit a reactive `FailureClass::ContextWindow` rejection. Paired
    /// with the skip count, this is what makes the gate's safety margin
    /// answerable: skips that likely saved a doomed knock versus overflows
    /// the gate let through anyway.
    ///
    /// CAVEAT, do not gloss: `FailureClass::ContextWindow` is reachable both
    /// natively from the classifier and via an operator class remap
    /// (`apply_remap`), so this count mixes classifier-native rejections
    /// with policy-remapped ones under one number -- the same interpretive
    /// trap the skip counter carries for false skips. Bumped once per
    /// dispatch error arm by the class-observability path.
    context_window_overflow_total: AtomicU64,
    /// Requests whose chain expansion walked a pool-backed model, counted once
    /// per pooled model per chain expansion (a chain naming two pools bumps
    /// twice). The denominator every other pool counter reads against.
    pool_dispatch_total: AtomicU64,
    /// The subset of [`pool_dispatch_total`](Self::pool_dispatch_total) served
    /// by a pool whose compiled seat count sits BELOW its configured member
    /// count -- a degraded pool serving through its survivors. A rising share
    /// means traffic is concentrating on fewer accounts than the operator
    /// configured, which the per-member omission WARN explains once at build
    /// time and this counts continuously.
    pool_degraded_dispatch_total: AtomicU64,
    /// Chain expansions that reached a pool-backed model with an EMPTY seat
    /// set. Defensive: the build refuses a zero-usable pool, so a nonzero
    /// count means a pooled model reached dispatch through some path that
    /// bypassed that refusal.
    pool_unavailable_total: AtomicU64,
    /// Pool members omitted at build time because the member declared no
    /// credential reference. Per-reason splits of the omission WARN, so the
    /// shape of a degraded fleet is answerable without re-reading logs.
    pool_member_omitted_credential_missing_total: AtomicU64,
    /// Pool members omitted because the store could not produce a credential
    /// for the member's reference (not logged in, refresh refused, backing
    /// file unreadable).
    pool_member_omitted_credential_unreadable_total: AtomicU64,
    /// Pool members omitted because the member's credential reference did not
    /// parse.
    pool_member_omitted_credential_invalid_total: AtomicU64,
    /// Pool members omitted because the provider instance failed to construct
    /// despite a usable credential.
    pool_member_omitted_provider_init_failed_total: AtomicU64,
    /// Sticky pins re-picked onto a surviving member because their pinned
    /// member left the pool. Bumped once per moved pin by
    /// `Router::carry_over_pool_state_from`; a burst of them is the operator's
    /// only signal that a credential change scattered live conversations off
    /// their warm-cache accounts.
    pool_removed_pin_repick_total: AtomicU64,
}

/// Running quota-placement totals, partitioned by the partition's arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotaPlacementTotals {
    below_cap: u64,
    all_capped: u64,
    mixed_unknown: u64,
    all_unknown: u64,
}

/// Running pool totals, read once per snapshot line so the emitted fields
/// cannot disagree about which load each came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolTotals {
    dispatch: u64,
    degraded_dispatch: u64,
    unavailable: u64,
    omitted_credential_missing: u64,
    omitted_credential_unreadable: u64,
    omitted_credential_invalid: u64,
    omitted_provider_init_failed: u64,
    removed_pin_repick: u64,
}

impl RouterMetrics {
    fn incr_pool_dispatch(&self) {
        self.pool_dispatch_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_pool_degraded_dispatch(&self) {
        self.pool_degraded_dispatch_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_pool_unavailable(&self) {
        self.pool_unavailable_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one build-time member omission under its allowlisted reason.
    /// Driven by [`Router::install_pool_reports`] once per omission per build.
    fn incr_pool_member_omitted(&self, reason: crate::pool_build::PoolOmissionReason) {
        use crate::pool_build::PoolOmissionReason as Reason;
        let counter = match reason {
            Reason::CredentialMissing => &self.pool_member_omitted_credential_missing_total,
            Reason::CredentialUnreadable => &self.pool_member_omitted_credential_unreadable_total,
            Reason::CredentialInvalid => &self.pool_member_omitted_credential_invalid_total,
            Reason::ProviderInitFailed => &self.pool_member_omitted_provider_init_failed_total,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one re-pick of a pin whose member left the pool. Driven by
    /// [`Router::note_pool_removed_pin_repick`], once per moved pin.
    fn incr_pool_removed_pin_repick(&self) {
        self.pool_removed_pin_repick_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Every pool counter's running total, for the snapshot line.
    fn pool_totals(&self) -> PoolTotals {
        PoolTotals {
            dispatch: self.pool_dispatch_total.load(Ordering::Relaxed),
            degraded_dispatch: self.pool_degraded_dispatch_total.load(Ordering::Relaxed),
            unavailable: self.pool_unavailable_total.load(Ordering::Relaxed),
            omitted_credential_missing: self
                .pool_member_omitted_credential_missing_total
                .load(Ordering::Relaxed),
            omitted_credential_unreadable: self
                .pool_member_omitted_credential_unreadable_total
                .load(Ordering::Relaxed),
            omitted_credential_invalid: self
                .pool_member_omitted_credential_invalid_total
                .load(Ordering::Relaxed),
            omitted_provider_init_failed: self
                .pool_member_omitted_provider_init_failed_total
                .load(Ordering::Relaxed),
            removed_pin_repick: self.pool_removed_pin_repick_total.load(Ordering::Relaxed),
        }
    }

    fn incr_unknown_failure_classification(&self) {
        self.unknown_failure_classifications_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_feature_unsupported(&self) {
        self.feature_unsupported_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_learned_negatives(&self, phase: FailurePhase) {
        self.learned_negatives_total.fetch_add(1, Ordering::Relaxed);
        match phase {
            FailurePhase::F1 => {
                self.learned_negatives_f1_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            FailurePhase::F2 => {
                self.learned_negatives_f2_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            FailurePhase::F3 => {}
        }
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

    fn incr_bedrock_validation_unmatched(&self) {
        self.bedrock_validation_unmatched_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_f2_same_chain_suppressed(&self) {
        self.f2_same_chain_suppressed_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_feature_naming_unmatched(&self) {
        self.feature_naming_unmatched_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn incr_verified_working(&self) {
        self.verified_working_total.fetch_add(1, Ordering::Relaxed);
    }

    fn incr_f3_suspect(&self) {
        self.f3_suspect_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the window-gate skip count, returning the new running total so
    /// the gate's rate-limited WARN can report it without a second load.
    fn incr_window_gate_skip(&self) -> u64 {
        self.window_gate_skips_total.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Bump the paired reactive-overflow count: a target cleared the
    /// proactive gate and still hit a `FailureClass::ContextWindow`
    /// rejection. See [`context_window_overflow_total`](Self::context_window_overflow_total)
    /// for the remap-mixing caveat.
    fn incr_context_window_overflow(&self) {
        self.context_window_overflow_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the counter for one quota-placement arm and return every arm's
    /// running total, so the throttled diagnostic reports the whole partition
    /// without a second pass.
    ///
    /// `Dormant` is not counted: it is the switched-off and
    /// nothing-to-decide-on state, and counting it would make the kill
    /// switch's OFF position observable in the diagnostics it must leave
    /// silent.
    fn incr_quota_placement(
        &self,
        decision: crate::quota::placement::QuotaDecision,
    ) -> QuotaPlacementTotals {
        use crate::quota::placement::QuotaDecision;
        match decision {
            QuotaDecision::Dormant => {}
            QuotaDecision::BelowCapTier => {
                self.quota_placement_below_cap_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            QuotaDecision::AllCappedMostRemaining => {
                self.quota_placement_all_capped_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            QuotaDecision::MixedUnknownFallback => {
                self.quota_placement_mixed_unknown_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            QuotaDecision::AllUnknownFallback => {
                self.quota_placement_all_unknown_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.quota_placement_totals()
    }

    /// Every quota-placement arm's running total.
    fn quota_placement_totals(&self) -> QuotaPlacementTotals {
        QuotaPlacementTotals {
            below_cap: self.quota_placement_below_cap_total.load(Ordering::Relaxed),
            all_capped: self
                .quota_placement_all_capped_total
                .load(Ordering::Relaxed),
            mixed_unknown: self
                .quota_placement_mixed_unknown_total
                .load(Ordering::Relaxed),
            all_unknown: self
                .quota_placement_all_unknown_total
                .load(Ordering::Relaxed),
        }
    }

    /// Read the cumulative unknown-upstream-classification count.
    fn unknown_failure_classifications_total(&self) -> u64 {
        self.unknown_failure_classifications_total
            .load(Ordering::Relaxed)
    }

    /// Read the cumulative feature-unsupported classification count.
    fn feature_unsupported_total(&self) -> u64 {
        self.feature_unsupported_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative acting learned-negative count (F1 + F2 + F3).
    fn learned_negatives_total(&self) -> u64 {
        self.learned_negatives_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative learned-registry invalidation count.
    fn invalidations_total(&self) -> u64 {
        self.invalidations_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative admitted-re-probe count.
    fn probe_attempts_total(&self) -> u64 {
        self.probe_attempts_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative same-capability re-probe-failure count.
    fn probe_failures_total(&self) -> u64 {
        self.probe_failures_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative learned-tail entry count.
    fn d17_tail_total(&self) -> u64 {
        self.d17_tail_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative in-place-strip count.
    fn strip_total(&self) -> u64 {
        self.strip_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative strip-rollback count.
    fn strip_rollback_total(&self) -> u64 {
        self.strip_rollback_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative strict-rejected-strip count.
    fn strip_strict_rejected_total(&self) -> u64 {
        self.strip_strict_rejected_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative masked-rejection-suppression count.
    fn mask_suppressed_total(&self) -> u64 {
        self.mask_suppressed_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative unmatched-Bedrock-validation count.
    #[cfg(feature = "bedrock")]
    fn bedrock_validation_unmatched_total(&self) -> u64 {
        self.bedrock_validation_unmatched_total
            .load(Ordering::Relaxed)
    }

    /// Read the cumulative F1-phase acting-negative count.
    fn learned_negatives_f1_total(&self) -> u64 {
        self.learned_negatives_f1_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative F2-phase acting-negative count.
    fn learned_negatives_f2_total(&self) -> u64 {
        self.learned_negatives_f2_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative same-chain-F1 F2-suppression count.
    fn f2_same_chain_suppressed_total(&self) -> u64 {
        self.f2_same_chain_suppressed_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative unmatched-feature-naming count.
    fn feature_naming_unmatched_total(&self) -> u64 {
        self.feature_naming_unmatched_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative acting-VerifiedWorking-positive count.
    fn verified_working_total(&self) -> u64 {
        self.verified_working_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative acting-F3-suspected-absence count.
    fn f3_suspect_total(&self) -> u64 {
        self.f3_suspect_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative window-gate skip count. The authoritative
    /// count of chain targets the proactive gate skipped before dispatch --
    /// a skip VOLUME signal, not a false-skip oracle: it counts skips, not
    /// outcomes, so it cannot by itself say whether a given skip avoided a
    /// doomed knock or discarded a target that would have served the
    /// request. Pair with
    /// [`context_window_overflow_total`](Self::context_window_overflow_total)
    /// to bound the other half of the margin question.
    fn window_gate_skips_total(&self) -> u64 {
        self.window_gate_skips_total.load(Ordering::Relaxed)
    }

    /// Read the cumulative paired reactive-overflow count. See
    /// [`context_window_overflow_total`](Self::context_window_overflow_total)
    /// for the remap-mixing caveat.
    fn context_window_overflow_total(&self) -> u64 {
        self.context_window_overflow_total.load(Ordering::Relaxed)
    }

    /// Emits one structured `tracing::debug!` line carrying every router
    /// counter's current value, mirroring the front-proxy
    /// `ProxyMetrics::log_snapshot` convention. Fields are all counter
    /// names + numeric values -- no token, credential, or request/response
    /// body content ever reaches this call.
    ///
    /// `quota_refused_by_admission_total` rides this snapshot rather than
    /// living on `RouterMetrics` itself: the counter is owned by
    /// `QuotaStore` (mirroring its own rejection counters), and the caller
    /// -- [`Router::log_metrics_snapshot`] -- is the one place that already
    /// holds both `self.metrics` and `self.quota_store`.
    fn log_snapshot(&self, quota_refused_by_admission_total: u64) {
        let quota = self.quota_placement_totals();
        let pool = self.pool_totals();
        #[cfg(feature = "bedrock")]
        tracing::debug!(
            target: "routectl_router::router::metrics",
            rc_unknown_failure_classifications_total = self.unknown_failure_classifications_total(),
            rc_feature_unsupported_total = self.feature_unsupported_total(),
            rc_learned_negatives_total = self.learned_negatives_total(),
            rc_learned_negatives_f1_total = self.learned_negatives_f1_total(),
            rc_learned_negatives_f2_total = self.learned_negatives_f2_total(),
            rc_probe_attempts_total = self.probe_attempts_total(),
            rc_probe_failures_total = self.probe_failures_total(),
            rc_invalidations_total = self.invalidations_total(),
            rc_d17_tail_total = self.d17_tail_total(),
            rc_strip_total = self.strip_total(),
            rc_strip_rollback_total = self.strip_rollback_total(),
            rc_strip_strict_rejected_total = self.strip_strict_rejected_total(),
            rc_mask_suppressed_total = self.mask_suppressed_total(),
            rc_f2_same_chain_suppressed_total = self.f2_same_chain_suppressed_total(),
            rc_feature_naming_unmatched_total = self.feature_naming_unmatched_total(),
            rc_verified_working_total = self.verified_working_total(),
            rc_f3_suspect_total = self.f3_suspect_total(),
            rc_window_gate_skips_total = self.window_gate_skips_total(),
            rc_context_window_overflow_total = self.context_window_overflow_total(),
            rc_quota_placement_below_cap_total = quota.below_cap,
            rc_quota_placement_all_capped_total = quota.all_capped,
            rc_quota_placement_mixed_unknown_total = quota.mixed_unknown,
            rc_quota_placement_all_unknown_total = quota.all_unknown,
            rc_quota_refused_by_admission_total = quota_refused_by_admission_total,
            rc_pool_dispatch_total = pool.dispatch,
            rc_pool_degraded_dispatch_total = pool.degraded_dispatch,
            rc_pool_unavailable_total = pool.unavailable,
            rc_pool_member_omitted_credential_missing_total = pool.omitted_credential_missing,
            rc_pool_member_omitted_credential_unreadable_total = pool.omitted_credential_unreadable,
            rc_pool_member_omitted_credential_invalid_total = pool.omitted_credential_invalid,
            rc_pool_member_omitted_provider_init_failed_total = pool.omitted_provider_init_failed,
            rc_pool_removed_pin_repick_total = pool.removed_pin_repick,
            rc_bedrock_validation_unmatched_total = self.bedrock_validation_unmatched_total(),
            "router metrics snapshot"
        );
        #[cfg(not(feature = "bedrock"))]
        tracing::debug!(
            target: "routectl_router::router::metrics",
            rc_unknown_failure_classifications_total = self.unknown_failure_classifications_total(),
            rc_feature_unsupported_total = self.feature_unsupported_total(),
            rc_learned_negatives_total = self.learned_negatives_total(),
            rc_learned_negatives_f1_total = self.learned_negatives_f1_total(),
            rc_learned_negatives_f2_total = self.learned_negatives_f2_total(),
            rc_probe_attempts_total = self.probe_attempts_total(),
            rc_probe_failures_total = self.probe_failures_total(),
            rc_invalidations_total = self.invalidations_total(),
            rc_d17_tail_total = self.d17_tail_total(),
            rc_strip_total = self.strip_total(),
            rc_strip_rollback_total = self.strip_rollback_total(),
            rc_strip_strict_rejected_total = self.strip_strict_rejected_total(),
            rc_mask_suppressed_total = self.mask_suppressed_total(),
            rc_f2_same_chain_suppressed_total = self.f2_same_chain_suppressed_total(),
            rc_feature_naming_unmatched_total = self.feature_naming_unmatched_total(),
            rc_verified_working_total = self.verified_working_total(),
            rc_f3_suspect_total = self.f3_suspect_total(),
            rc_window_gate_skips_total = self.window_gate_skips_total(),
            rc_context_window_overflow_total = self.context_window_overflow_total(),
            rc_quota_placement_below_cap_total = quota.below_cap,
            rc_quota_placement_all_capped_total = quota.all_capped,
            rc_quota_placement_mixed_unknown_total = quota.mixed_unknown,
            rc_quota_placement_all_unknown_total = quota.all_unknown,
            rc_quota_refused_by_admission_total = quota_refused_by_admission_total,
            rc_pool_dispatch_total = pool.dispatch,
            rc_pool_degraded_dispatch_total = pool.degraded_dispatch,
            rc_pool_unavailable_total = pool.unavailable,
            rc_pool_member_omitted_credential_missing_total = pool.omitted_credential_missing,
            rc_pool_member_omitted_credential_unreadable_total = pool.omitted_credential_unreadable,
            rc_pool_member_omitted_credential_invalid_total = pool.omitted_credential_invalid,
            rc_pool_member_omitted_provider_init_failed_total = pool.omitted_provider_init_failed,
            rc_pool_removed_pin_repick_total = pool.removed_pin_repick,
            "router metrics snapshot"
        );
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
    /// Persistable credential identity of the served / terminal target
    /// (`DispatchTarget::seat`): the `provider#label` OAuth seat key, bare
    /// `provider` for a default seat. `None` when no target was dispatched
    /// (a pre-dispatch failure), when the served target authenticated with
    /// a non-OAuth ref, and on the forwarded-credential path (routectl does
    /// not own the client's seat). A fallback records the seat that ACTUALLY
    /// served, not the first target's.
    pub served_seat: Option<String>,
    /// The resolved alias key the request routed under (the incoming
    /// `req.model`). Always populated, even when resolution then failed.
    pub resolved_alias: String,
    /// Stable auto-cache decision token for the served target (see
    /// `CacheInjection::strategy_str`). `None` when no target was
    /// dispatched (count_tokens, unknown alias, or all entries
    /// gate-blocked before any injection point ran).
    ///
    /// Carries the TERMINAL marker's decision, so its meaning is
    /// unchanged from before per-marker placement existed. The granular
    /// truth lives in `cache_front_decision` / `cache_terminal_decision`.
    pub cache_strategy: Option<&'static str>,
    /// Stable decision token for the FRONT auto-cache marker (a system
    /// block or a custom tool definition) on the served target, from the
    /// same `CacheInjection::strategy_str` vocabulary as
    /// `cache_strategy`. `None` when no target was dispatched.
    pub cache_front_decision: Option<&'static str>,
    /// Stable decision token for the TERMINAL auto-cache marker (the
    /// top-level `cache_control` field) on the served target, from the
    /// same `CacheInjection::strategy_str` vocabulary. `None` when no
    /// target was dispatched.
    pub cache_terminal_decision: Option<&'static str>,
    /// Stable context-reduction decision token for the served target
    /// (see `reduction_strategy_token`). `None` when no target was
    /// dispatched (count_tokens, unknown alias, or all entries
    /// gate-blocked before any reduction point ran).
    pub reduction_strategy: Option<&'static str>,
    /// Context-reduction counter: strings the minifier actually rewrote.
    /// Aggregated across the fallback-entry preparations of this chain walk
    /// (a same-target network retry reuses the prepared request and never
    /// re-counts). `Some(0)` is a measured zero (reduction ran, rewrote
    /// nothing); `None` means no dispatched target ran reduction. A raw
    /// count, never a rate or an average -- ratios are reconstructed
    /// offline by summing the counters.
    pub reduction_strings_compressed: Option<u64>,
    /// Context-reduction counter: candidate strings left untouched because
    /// they were non-JSON or already compact. Same aggregation and
    /// `Some(0)` / `None` semantics as `reduction_strings_compressed`. A
    /// raw count.
    pub reduction_strings_skipped: Option<u64>,
    /// Context-reduction counter: strings that parsed as JSON but which the
    /// re-parse equality guard declined to replace. Distinct from
    /// `reduction_strings_skipped`: a skip is a permanent ceiling, a reject
    /// is a fail-closed invariant alarm -- structurally unreachable with the
    /// current minifier, so a nonzero count means a minifier defect, never
    /// traffic headroom. Same aggregation and `Some(0)` / `None` semantics.
    /// A raw count.
    pub reduction_strings_rejected: Option<u64>,
    /// Context-reduction counter: bytes removed from the PREPARED outbound
    /// payloads of this chain walk -- payload bytes, never billed tokens
    /// and never a token estimate (any token figure is derived downstream
    /// from this raw count). Same aggregation and `Some(0)` / `None`
    /// semantics as `reduction_strings_compressed`.
    pub reduction_bytes_saved: Option<u64>,
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
    /// only (see `Router::record_would_trim`).
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
    /// Non-mutating prefix-rewrite detector classification for this client
    /// request: `Some(0)` when the client's canonical prefix was unchanged or
    /// grew by pure append (Stable), `Some(1)` when the old region's bytes
    /// changed inside the epoch (Rewritten -- the client broke its own cache
    /// prefix), `Some(2)` when the prefix shortened (Reseeded, the compaction
    /// shape -- deliberately NOT a rewrite). `None` for a first-seen session
    /// (no baseline to classify against, including the first turn after a
    /// process restart) and when the request carried no session key. Recording
    /// only -- the dispatched bytes are NEVER mutated, and the detector runs
    /// once per client request above the chain loop so it adds no per-target
    /// state.
    pub prefix_epoch_event: Option<i64>,
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
    /// Recorder-version marker: `None` on rows written before the recorder
    /// existed and on rows where the near-lossless pass did not run (below
    /// the estimated-token trigger); stamped with
    /// `NEAR_LOSSLESS_RECORDER_VERSION` by the near-lossless recorder
    /// (`Router::record_would_trim`) on every trigger-clearing row,
    /// regardless of whether the pass found any marks. Lets reporting
    /// filter to non-NULL rows so aggregates never mix unstamped baseline
    /// against recorded semantics.
    pub would_trim_recorder_version: Option<i64>,
    /// Raw-marks JSON blob (uncapped at this layer): the near-lossless
    /// pass's marks (dedup + supersession), captured for a future
    /// path-extraction sweep. The byte cap is applied downstream by
    /// `routectl_usage::writer::capped_raw_marks_text` so the stored JSON
    /// is always valid. `None` when the near-lossless pass did not run
    /// (below trigger). Recording only.
    pub would_trim_raw_marks: Option<Value>,
    /// Non-mutating context-fraction advisory: `estimate_total_tokens /
    /// max_context_tokens` from the resolved pricing row. `None` when the
    /// near-lossless pass did not run (below trigger) OR the resolved
    /// row's context window is unknown (fail-closed). Recording only.
    pub would_trim_context_fraction: Option<f64>,
    /// Token-estimate calibration evidence: routectl's own byte-heuristic
    /// token estimate of the payload actually dispatched to the served
    /// target. Stamped for EVERY dispatched attempt, unconditionally -- no
    /// size trigger, no chain-length condition, no kill switch -- so the
    /// evidence population is not skewed toward the large or multi-target
    /// requests that other advisories select for. Last-writer-wins across a
    /// chain walk, which leaves the SERVED attempt's estimate: the one whose
    /// reported usage it will be compared against. `None` when no target was
    /// dispatched. Recording only.
    pub calib_estimated_tokens: Option<u64>,
    /// Learned-capability observations captured on the dispatch error
    /// arm(s) for this request. Empty on the common (non-capability)
    /// path; carries one event per eligible, deduped, acting observation
    /// (self-identifying on the first, inferred on the confirming second)
    /// so the usage-capture layer can persist them without the router
    /// depending on the ledger writer.
    pub learned_capabilities: Vec<CapabilityLearnEvent>,
    /// Response-evidence capability observations captured on the terminal
    /// successful non-streaming dispatch for this request. Empty on the
    /// common path (no clean-stop success, kill switch off, or no positive /
    /// suspected-absence evidence); carries one event per eligible, deduped,
    /// acting observation so the usage-capture layer can persist them without
    /// the router depending on the ledger writer. Additive, defaults empty.
    pub capability_observations: Vec<CapabilityObserveEvent>,
    /// Probe-settled clears captured on the terminal successful dispatch for
    /// this request: a resident learned negative a successful re-probe cleared
    /// in memory. Empty on the common path (no re-probe reached its target, or
    /// the re-probe did not succeed); carries one event per cleared entry so
    /// the usage-capture layer persists the clear and the warm-rebuild replayer
    /// removes the same resident negative on boot rather than resurrecting it.
    /// Collected ONLY at `LearnedProbeGuard::settle_success`. Additive,
    /// defaults empty.
    pub cleared_capabilities: Vec<CapabilityClearedEvent>,
    /// Reasoning-replay degradation record for the whole chain walk.
    /// `Some` exactly when the fixed strip-repair branch fired for this
    /// request -- carried reasoning artifacts drew the proven replay
    /// rejection and were stripped for a same-target re-dispatch. The
    /// single aggregated degradation WARN reads it ONCE at request
    /// resolution; `None` means nothing degraded (no WARN). Carries only
    /// closed-set tokens and counts -- never the artifact bytes, an item
    /// id, a hash, the session key, or the upstream body, at any level.
    pub replay_degradation: Option<ReplayDegradation>,
}

/// Closed-set facts about a reasoning-replay strip-repair that fired
/// during a chain walk, aggregated onto [`DispatchMeta`] for the single
/// per-request degradation WARN. Every field is a stable token or a
/// count: deliberately NO artifact bytes, reasoning item id, hash /
/// digest, session key, or upstream body, at any verbosity. The request
/// span already supplies `request_id` correlation across the retry and
/// fallback hops.
///
/// `#[non_exhaustive]` so a future degradation action can add fields
/// without breaking downstream construction.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ReplayDegradation {
    /// What the router did (closed token), e.g. the fixed strip-repair.
    pub action: &'static str,
    /// The target lane the artifacts were stripped against.
    pub target_lane: ReplayScheme,
    /// Sanitized `[providers]` state key of the repaired target.
    pub state_key: String,
    /// Distinct source schemes of the stripped non-portable artifacts,
    /// first-seen order.
    pub source_schemes: Vec<ReplayScheme>,
    /// Why the strip fired (closed token).
    pub reason: &'static str,
    /// Count of non-portable reasoning artifacts stripped.
    pub artifact_count: usize,
    /// The strip-repair branch fired.
    pub repair_attempted: bool,
    /// The stripped re-dispatch reached success / a first chunk.
    pub repair_succeeded: bool,
    /// The confirmed negative was persisted to the learned registry.
    pub learned: bool,
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
            served_seat: None,
            resolved_alias: alias.to_string(),
            cache_strategy: None,
            cache_front_decision: None,
            cache_terminal_decision: None,
            reduction_strategy: None,
            reduction_strings_compressed: None,
            reduction_strings_skipped: None,
            reduction_strings_rejected: None,
            reduction_bytes_saved: None,
            selection_decision: None,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
            prefix_epoch_event: None,
            would_trim_dedup_tokens: None,
            would_trim_supersession_tokens: None,
            would_trim_path_units: None,
            would_trim_path_extractable: None,
            would_trim_recorder_version: None,
            would_trim_raw_marks: None,
            would_trim_context_fraction: None,
            calib_estimated_tokens: None,
            learned_capabilities: Vec::new(),
            capability_observations: Vec::new(),
            cleared_capabilities: Vec::new(),
            replay_degradation: None,
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
        // A forwarded target authenticates with the client's own bearer, so
        // routectl owns no seat for that row. Gated here rather than relying
        // on the empty-`api_key_ref` validation that keeps such a target's
        // ref unparseable, so the ledger guarantee holds locally.
        self.served_seat = if target.use_forwarded_credential {
            None
        } else {
            target.seat.clone()
        };
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
    /// Router-scoped metadata, valid on both the `Ok` and `Err` arms.
    pub meta: DispatchMeta,
    /// The non-streaming dispatch result.
    pub result: Result<ChatResponse>,
}

/// `stream_with_options` return: the streaming dispatch result paired
/// with its router-scoped [`DispatchMeta`]. The served_* fields are
/// captured synchronously when the winning upstream's first chunk
/// arrives, so they are valid before the stream body is consumed. A
/// fixed two-field pair for the same reason as [`Dispatched`].
pub struct DispatchedStream {
    /// Router-scoped metadata, valid before the stream body is consumed.
    pub meta: DispatchMeta,
    /// The streaming dispatch result.
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
    /// Persistable credential identity of this target's own credential
    /// (see [`crate::seat_pool::seat_identity`]): the `provider#label`
    /// seat key for an `oauth://` ref, `None` for every other scheme and
    /// for a target with no ref. Surfaced through `DispatchMeta` so usage
    /// accounting partitions rows by ACCOUNT rather than by model.
    seat: Option<String>,
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
    /// ([`capability_strip::effective_action_for`](crate::capability_strip::effective_action_for)
    /// -- the baked table as the operator's `[capability] essential` list
    /// tightens it)
    /// is `Strip` and that no operator beta floor pins to the wire. Sorted
    /// normalized keys
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
    /// `DispatchMeta::selection_decision` via `mark_target` for
    /// observability; usage accounting no longer records it (the ledger's
    /// `selection_decision` column is write-stopped). Observability only --
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
    /// [`DispatchTarget::capability_prior`] in the feature filter's prior
    /// pass, where a `Some(false)` prior for an otherwise-open feature
    /// soft-tails the target.
    capabilities: BTreeMap<String, bool>,
}

impl DispatchTarget {
    /// The catalog capability prior for `feature`: `Some(true)` /
    /// `Some(false)` when the resolved cell asserts support / absence, or
    /// `None` when the cell carries no prior for the key. The lowest-
    /// precedence baseline in the override -> learned -> prior chain,
    /// consumed by the feature filter's prior pass: a `Some(false)` prior
    /// for a feature the learned pass left open soft-tails the target.
    fn capability_prior(&self, feature: &str) -> Option<bool> {
        self.capabilities.get(feature).copied()
    }
}

impl Router {
    /// Build a router from a config, provisioning a runtime gate for every
    /// configured provider. Resolved models and providers are registered
    /// separately.
    ///
    /// UNVALIDATED by design: this constructor runs no part of
    /// [`collect_config_validation`](crate::collect_config_validation), so a
    /// caller passing an unchecked `Config` gets a Router whose invalid
    /// settings sit inert. Callers wanting the validated build path go
    /// through the CLI's router builder, which runs the suite first.
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
                    // Both fields carry the operator-written `[aliases]`
                    // key (the parse error quotes it back), and this
                    // fires BEFORE `validate_alias_patterns` rejects the
                    // key -- so a key bearing a newline would forge a
                    // second startup record through the `%` render.
                    tracing::warn!(
                        pattern = %routectl_core::sanitize_for_log(key),
                        error = %routectl_core::sanitize_for_log(&e),
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
        let prefix_epoch_store = Arc::new(prefix_rewrite::PrefixRewriteStore::new());
        let calibration_store = Arc::new(crate::calibration::CalibrationStore::default());
        let quota_store = Arc::new(crate::quota::store::QuotaStore::default());
        // Registry tempo comes from the `[capability]` knobs (hours ->
        // Duration); the kill switch is deliberately NOT read here -- the
        // act / learn sites gate on it, so a disabled subsystem keeps the
        // registry resident but inert and a hot re-enable is instant.
        let learned_capabilities = Arc::new(
            crate::learned_capability::LearnedCapabilityRegistry::from_capability_config(
                &config.capability,
            ),
        );
        let learned_replay = Arc::new(crate::learned_replay::ReplayLearnRegistry::new(Arc::clone(
            &learned_capabilities,
        )));
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
            prefix_epoch_store,
            calibration_store,
            quota_store,
            learned_capabilities,
            learned_replay,
            override_registry,
            pool_reports: Vec::new(),
            catalog_version: crate::catalog_baked::CATALOG_VERSION,
            overlay_revision: 0,
            catalog_overlay: Arc::default(),
            metrics: Arc::new(RouterMetrics::default()),
            has_forwarded_provider,
            volatile_prefix_warned: Mutex::new(HashSet::new()),
            prefix_rewrite_warned: Arc::new(AtomicBool::new(false)),
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
            // each keyed by that seat's per-model `state_key`, so the breaker
            // + RPM bucket are per (model, seat) -- probe fast-fail, retry
            // caps, and the reset park all apply per seat. Each seat's policy
            // comes from its OWN member entry: a pool-backed model's
            // `provider_name` is the POOL name, which has no runtime block, so
            // reading the policy from it would silently hand every seat the
            // defaults instead of the operator's per-account knobs.
            // A non-pooled model (None seats) only gets the nickname slot.
            if let Some(seats) = m.seats.as_ref() {
                for seat in seats.iter() {
                    let seat_policy = self
                        .config
                        .providers
                        .get(&seat.provider_name)
                        .map(|e| e.runtime().clone())
                        .unwrap_or_default();
                    self.state
                        .entry(seat.state_key_for(nickname))
                        .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&seat_policy))));
                }
                // Round-robin pools rotate the starting seat per request;
                // register a cursor only for that selection mode, keyed by the
                // POOL so every model naming it advances one cursor.
                if matches!(
                    self.config.seat_selection_for(&m.provider_name),
                    crate::config::SeatSelection::RoundRobin
                ) {
                    self.round_robin.register(m.rotation_key());
                }
            }
            self.state
                .entry(nickname.clone())
                .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&policy))));
        }
        self.admit_quota_seats();
    }

    /// The nicknames whose outbound output-token ceiling came from the catalog
    /// rather than from config, each with the ceiling that was filled, in
    /// nickname order.
    ///
    /// Read off the INSTALLED table, not re-derived from config: the fill runs
    /// in `factory::apply_catalog_overlay`, whose per-model selector resolves a
    /// pool-backed model's provider kind off a SEAT (a pool name is not a
    /// provider kind). A config-only re-derivation would silently report
    /// nothing for exactly the models a pool serves.
    ///
    /// A model is reported iff its `[models.X] max_output_tokens` is unset (or
    /// the ignored `0`) AND its resolved cell confirms a ceiling -- the same
    /// two conditions the fill itself is gated on, so this can only name a
    /// model the fill actually changed.
    #[must_use]
    pub fn catalog_filled_output_ceilings(&self) -> Vec<(&str, u32)> {
        self.resolved_models
            .iter()
            .filter(|(nickname, _)| {
                self.config
                    .models
                    .get(*nickname)
                    .and_then(|entry| entry.max_output_tokens)
                    .is_none_or(|configured| configured == 0)
            })
            .filter_map(|(nickname, model)| {
                Some((nickname.as_str(), model.output_ceiling_tokens()?))
            })
            .collect()
    }

    /// Declare the OAuth account keys the quota store will hold readings for:
    /// every credential identity reachable from the resolved model table,
    /// counting both a pooled model's seats and a non-pooled model's own
    /// credential.
    ///
    /// This is what bounds the store's keyspace to the loaded config. It runs at
    /// install time (and therefore on every rebuild), so a config that dropped a
    /// seat neither keeps a live reading for it nor accepts a new one.
    fn admit_quota_seats(&self) {
        use crate::quota::key::seat_key_for_secret_ref;

        let mut admitted = Vec::new();
        for m in self.resolved_models.values() {
            if let Some(seats) = m.seats.as_ref() {
                admitted.extend(
                    seats
                        .iter()
                        .filter_map(|seat| seat_key_for_secret_ref(seat.auth_secret_ref.as_ref())),
                );
            }
            admitted.extend(seat_key_for_secret_ref(m.auth_secret_ref.as_ref()));
        }
        self.quota_store.admit_seats(admitted);
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
    /// the one account holding its warm prompt cache; dropping the pins on a
    /// reload would re-pin every conversation from scratch and scatter them
    /// across accounts -- a mass cold-miss across all in-flight conversations.
    ///
    /// Membership is NOT reconciled here. That is
    /// `carry_over_pool_state_from`'s job, and it must run after this call:
    /// a pin whose member left the pool re-picks once onto a survivor there,
    /// while every surviving pin is left byte-for-byte alone.
    ///
    /// Carried by SHARING the `Arc` rather than copying entries, the same
    /// fix `carry_over_k_store_from` applies to the K-estimator store: a
    /// snapshot-and-replay has a window between the snapshot and the new
    /// Router's publish where a pin written through the outgoing Router (an
    /// overflow-repin racing the swap) would land only in the map this
    /// carry-over is about to discard. Sharing the map means both Routers'
    /// writes land on the one LRU, so a pin racing the swap is neither lost
    /// nor written to a copy nobody reads. This also shares the anti-herd
    /// tiebreak counter, which now keeps advancing across a reload instead
    /// of resetting -- see the field's own doc on `StickyPins`.
    ///
    /// Called by the hot-reload coordinator in routectl-cli immediately
    /// after building a replacement Router and before swapping it in,
    /// alongside `carry_over_runtime_state_from`.
    pub fn carry_over_sticky_from(&mut self, previous: &Self) {
        self.sticky_pins = Arc::clone(&previous.sticky_pins);
    }

    /// The usable members of every pool this Router serves, in the operator's
    /// declared order, keyed by pool name.
    ///
    /// Read off the compiled seat sets rather than the `[pools]` config table:
    /// a member the build dropped for a credential failure is not a seat this
    /// Router can pin a session to, so treating it as present would leave a
    /// pin sitting on an account nothing dispatches.
    fn pool_membership(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut by_pool: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for m in self.resolved_models.values() {
            if let Some(seats) = m.seats.as_ref() {
                by_pool
                    .entry(m.rotation_key())
                    .or_insert_with(|| seats.iter().map(|s| s.provider_name.as_str()).collect());
            }
        }
        by_pool
    }

    /// Which surviving member each RETIRED member's pins must move to, for the
    /// pools both Routers serve.
    ///
    /// A member appears here only when its pool still serves at least one
    /// account AND that member is no longer among them. So:
    ///
    /// - a member still serving is absent -- its pins are never touched, which
    ///   is what keeps a membership change from globally resetting affinity;
    /// - a member whose whole pool is gone is absent too. Its pins resolve to
    ///   a miss at read time (there is no pool to walk) and re-pick naturally
    ///   if the pool ever returns; picking a survivor from an unrelated pool
    ///   would be worse than a cold miss.
    /// - a RENAMED pool is a new pool on both sides of this join, so neither
    ///   its cursor nor its pins transfer. Deliberate: no heuristic can tell a
    ///   rename from a decommission-plus-introduction, and guessing wrong
    ///   silently pins sessions onto accounts the operator moved away from.
    ///
    /// The survivor is the pool's FIRST usable member -- the operator's
    /// declared order, the same order fill-first walks -- so the choice is
    /// deterministic across the reload rather than dependent on map iteration.
    fn retired_member_survivors(&self, previous: &Self) -> BTreeMap<String, String> {
        let current = self.pool_membership();
        let mut survivors = BTreeMap::new();
        for (pool, before) in previous.pool_membership() {
            let Some(after) = current.get(pool) else {
                continue;
            };
            let Some(survivor) = after.first() else {
                continue;
            };
            for member in before {
                if !after.contains(&member) {
                    survivors.insert(member.to_string(), (*survivor).to_string());
                }
            }
        }
        survivors
    }

    /// Carry the previous Router's per-pool rotation and affinity state into
    /// this freshly-built Router during a hot-reload, and reconcile the pins
    /// against the new pool membership.
    ///
    /// Call AFTER `carry_over_sticky_from` (the pins this reconciles are the
    /// shared ones) and after `carry_over_metrics_from` (the re-pick count
    /// must land on the shared counter storage, not on a value this Router is
    /// about to discard).
    ///
    /// Two pieces of state, both per pool:
    ///
    /// - the round-robin cursors, adopted into this Router's FRESH map for the
    ///   pools it declares. Building forward rather than mutating the previous
    ///   map is what bounds the keyspace to the current config: mutating in
    ///   place would let a run of reloads over renamed pools accumulate every
    ///   pool name the process ever saw.
    /// - the sticky pins, re-picked once for any member that left a pool still
    ///   serving. A surviving member's pin is never touched, so a membership
    ///   change costs a cold miss only to the sessions whose own account went
    ///   away. Each move is counted, because a burst of them is the operator's
    ///   only signal that a credential change scattered live conversations.
    ///
    /// Quota admission needs nothing here: its keyspace is the account
    /// identity set, re-declared by `carry_over_quota_from` through
    /// `admit_seats`. Lock discipline is unchanged with it -- this call takes
    /// only the pin map's own lock and never holds it across a quota lock.
    pub fn carry_over_pool_state_from(&mut self, previous: &Self) {
        self.round_robin.carry_over_from(&previous.round_robin);

        let survivors = self.retired_member_survivors(previous);
        if survivors.is_empty() {
            return;
        }
        let moved = self
            .sticky_pins
            .repick_members(&mut |member| survivors.get(member).cloned());
        for _ in 0..moved {
            self.note_pool_removed_pin_repick();
        }
        if moved > 0 {
            tracing::info!(
                event = "pool_removed_pin_repick",
                repicked_pins = moved,
                retired_members = survivors.len(),
                "pool members left the fleet; their pinned sessions re-picked onto survivors",
            );
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
    /// Carried by SHARING the `Arc` rather than copying entries, the same fix
    /// `carry_over_prefix_epochs_from` applies to the prefix-epoch store: a
    /// snapshot-and-reimport has a window between the export and the new
    /// Router's publish where a sample recorded through the outgoing Router
    /// (a response completing late) would land only in the store this
    /// carry-over is about to discard. Sharing the store means both Routers'
    /// writes land on the same map, so a sample racing the swap is neither
    /// lost nor written to a copy nobody reads.
    ///
    /// The estimator is rebound over the shared store for the same reason:
    /// `k_estimator` was constructed against this (fresh, about-to-be-
    /// discarded) Router's own store, so leaving it as-is would read an
    /// empty map even after the store field itself is shared.
    pub fn carry_over_k_store_from(&mut self, previous: &Self) {
        self.k_session_store = Arc::clone(&previous.k_session_store);
        self.k_estimator = Arc::new(crate::k_estimator::LedgerBackedK::new(Arc::clone(
            &self.k_session_store,
        )));
    }

    /// Carry the previous Router's prefix-epoch detector state into this
    /// freshly-built Router during a hot-reload.
    ///
    /// Mandatory, and the same silent-collapse shape as
    /// `carry_over_k_store_from`: an emptied store makes every live session's
    /// next turn first-seen, and a first-seen turn is deliberately
    /// unclassified -- no event code, no WARN. So a dropped store does not
    /// error and does not warn; the detector simply stops finding anything
    /// until each session has taken two more turns, which reads exactly like
    /// healthy traffic. A process restart accepts that window (bounded
    /// false-negative, never a false positive); a config reload must not.
    ///
    /// Carried by SHARING the `Arc`s rather than copying entries -- both the
    /// store and the WARN latch. Sharing rather than snapshotting is what makes
    /// the carry-over safe against in-flight requests: a request that read the
    /// outgoing Router observes into the same store the incoming one reads, so
    /// an observation racing the swap is neither lost nor re-classified against
    /// a stale baseline. Sharing the latch is what keeps the WARN
    /// once-per-PROCESS: a fresh latch would re-warn on every reload, making
    /// the line's frequency track reloads rather than rewrites.
    pub fn carry_over_prefix_epochs_from(&mut self, previous: &Self) {
        self.prefix_epoch_store = Arc::clone(&previous.prefix_epoch_store);
        self.prefix_rewrite_warned = Arc::clone(&previous.prefix_rewrite_warned);
    }

    /// Carry the previous Router's observability counters into this
    /// freshly-built Router during a hot-reload, by SHARING the storage
    /// rather than copying values.
    ///
    /// Mandatory. Copying values would lose increments from late-completing
    /// requests still holding the outgoing Router after the swap -- the
    /// same in-flight-observation hazard `carry_over_prefix_epochs_from`
    /// guards against, and the same fix: share the `Arc` so both Routers'
    /// increments land on the one storage. Every counter is already a plain
    /// atomic (`&self` increments), so Arc-sharing needs no further
    /// synchronization.
    ///
    /// Call this BEFORE `carry_over_learned_from` at every reload site: that
    /// call bumps `invalidations_total` on a catalog/overlay change, and
    /// that increment must land on the shared storage, not a value this
    /// Router is about to discard.
    pub fn carry_over_metrics_from(&mut self, previous: &Self) {
        self.metrics = Arc::clone(&previous.metrics);
    }

    /// Carry the previous Router's per-lane token-estimate correction
    /// evidence into this freshly-built Router during a hot-reload.
    ///
    /// Mandatory. Each lane's samples are minutes-to-hours of accumulated
    /// evidence about how far that model's real token count sits from the
    /// router's byte-length estimate. Dropping them on a rebuild sends every
    /// lane back to the uncorrected estimate until traffic re-warms it -- and
    /// because the uncorrected estimate is exactly the pre-correction
    /// behavior, the loss is invisible: nothing errors, nothing warns, every
    /// lane simply reads as not-yet-calibrated, which is indistinguishable
    /// from health.
    ///
    /// Carried by SHARING the `Arc` rather than copying entries: a sample
    /// recorded through the outgoing Router in the window between this call
    /// and the swap lands in the same map the incoming Router reads, so it
    /// is neither lost nor written to a copy nobody reads. Immediately after
    /// the share, `CalibrationStore::retain_lanes`
    /// drops every lane `Router::knows_nickname` refuses -- the same
    /// predicate the boot rebuild filters rows by -- so the map does not
    /// grow past the models ever declared in this process; leaving a run of
    /// reloads with renamed models to carry every past name forward would
    /// break that bound.
    ///
    /// Sharing accepts one bounded leak the old copy-and-filter did not: an
    /// in-flight write through the OLD `Arc` can re-create a lane for a
    /// nickname this prune just dropped. See the store's own doc for why the
    /// leak is bounded and self-healing rather than something to close here.
    pub fn carry_over_calibration_from(&mut self, previous: &Self) {
        self.calibration_store = Arc::clone(&previous.calibration_store);
        self.calibration_store
            .retain_lanes(|key| self.knows_nickname(&key.nickname));
    }

    /// Carry the previous Router's latest per-seat subscription-quota readings
    /// into this freshly-built Router during a hot-reload.
    ///
    /// Mandatory, and for a reason that makes omitting it worse than it looks:
    /// an emptied quota store is indistinguishable from a fleet of seats that
    /// have not reported yet, which is the cap-dormant fallback. So a dropped
    /// store does not error, does not warn, and does not degrade visibly -- it
    /// silently un-arms the placement signal until traffic re-reports on every
    /// seat, and until then reads as health.
    ///
    /// This carry-over MUST be called at every reload site. A single missed site
    /// is a config swap that empties the store on that path only, which no
    /// symptom would distinguish from the same silence.
    ///
    /// Carried by SHARING the `Arc` rather than copying entries, THEN
    /// re-running `Router::admit_quota_seats` against the now-shared
    /// store: `install_resolved_models` already declared admission once
    /// against this Router's own (about-to-be-discarded) store, so without
    /// the re-run the shared store would keep answering from whatever the
    /// PREVIOUS Router last admitted. The re-run both prunes a seat the new
    /// config no longer declares (through `QuotaStore::admit_seats`'s own
    /// fold, so a run of reloads over renamed or removed seats cannot carry
    /// retired keys forward) and admits a seat the new config newly
    /// declares, so a late write from a request still holding the outgoing
    /// Router lands on the one store both Routers read -- neither lost to a
    /// discarded copy nor double-counted.
    pub fn carry_over_quota_from(&mut self, previous: &Self) {
        self.quota_store = Arc::clone(&previous.quota_store);
        self.admit_quota_seats();
    }

    /// Whether `nickname` is still in this Router's resolved model table.
    ///
    /// THE one calibration-lane admission predicate: both the boot rebuild and
    /// the hot-reload carry-over go through it, so a lane the one path refuses
    /// cannot be the lane the other admits.
    fn knows_nickname(&self, nickname: &str) -> bool {
        self.resolved_models.contains_key(nickname)
    }

    /// Install the catalog overlay this Router's resolved-model table was
    /// merged against, stamping its revision at the same time. Called by the
    /// CLI builder immediately after the resolved models are merged with the
    /// overlay, so `carry_over_learned_from` can detect a revision change
    /// across a hot-reload and invalidate the learned registry, and so a
    /// reader asking what catalog truth is in effect reads the ACCEPTED
    /// generation out of memory instead of re-reading the file.
    ///
    /// The single writer of both the overlay and its revision, so the two
    /// cannot drift. Build paths that apply no overlay leave the empty
    /// default (revision zero) in place, which preserves the existing
    /// carry-over semantics: two revision-zero Routers compare equal, so a
    /// config-only reload between them still carries learned state across.
    pub fn install_catalog_overlay(
        &mut self,
        overlay: Arc<crate::catalog_overlay::CatalogOverlay>,
    ) {
        self.overlay_revision = overlay.revision;
        self.catalog_overlay = overlay;
    }

    /// Baked catalog table version this Router was built against. Read at
    /// the boot warm-rebuild seam to stamp a fresh capability-event
    /// tombstone with the current revision, and by the unified drain to
    /// stamp each persisted event.
    pub const fn catalog_version(&self) -> u32 {
        self.catalog_version
    }

    /// Catalog-overlay revision this Router was built against (zero until
    /// `install_catalog_overlay` records it). Read alongside
    /// [`Router::catalog_version`] at the same boundary.
    pub const fn overlay_revision(&self) -> u64 {
        self.overlay_revision
    }

    /// The catalog overlay generation this Router was built against -- the
    /// one the daemon ACCEPTED, never a fresher file on disk. Read by the
    /// status read side to derive the in-effect config view without a disk
    /// read; the private field's own doc carries the full rationale.
    pub fn catalog_overlay(&self) -> &crate::catalog_overlay::CatalogOverlay {
        &self.catalog_overlay
    }

    /// Replay a capability-event ledger slice into the private learned
    /// registry during boot warm-rebuild. Delegates to
    /// [`crate::capability_rebuild::rebuild_capabilities_into`] over the
    /// registry this Router owns, so the registry stays encapsulated behind
    /// the Router rather than being handed out. Best-effort and idempotent
    /// against a fresh registry, mirroring the K-store warm rebuild; the
    /// returned tally is for boot observability.
    pub fn rebuild_learned_from_ledger(
        &self,
        reader: &dyn crate::capability_rebuild::CapabilityLedgerReader,
    ) -> crate::capability_rebuild::CapabilityRebuildSummary {
        crate::capability_rebuild::rebuild_capabilities_into(reader, &self.learned_capabilities)
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
    /// (session, provider_kind, model) triple through the shared
    /// `k_query_key` derivation -- the same one the read side projects its
    /// query from -- and appends a sample whose `observed_reuse` is
    /// `cache_read > 0` (the router owns the reuse definition, mirroring the
    /// ledger rebuild). `model` is the served NICKNAME, matching the read
    /// side's model dimension.
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
        let Some(key) = k_query_key(session_key, Some(provider_kind), model).store_key() else {
            return;
        };
        self.k_session_store.record_sample(
            key,
            crate::k_estimator::Sample {
                ts,
                observed_reuse: cache_read > 0,
            },
        );
    }

    /// Record one live token-estimate observation into the per-lane
    /// calibration store. Called best-effort from the ingress capture path
    /// AFTER the served target and the response's own prompt total are known.
    ///
    /// A lane requires BOTH halves of its key: the served provider kind and
    /// the served model NICKNAME. A target missing either forms no lane and
    /// the call is a no-op -- deliberately NOT falling back to the upstream
    /// wire id, because that label is what a later ledger-driven rebuild
    /// cannot reproduce, and a live path recording under a label the rebuild
    /// filters out would make the two disagree about the same lane.
    ///
    /// `session_key` names the CALLER the observation came from and is used
    /// only to derive an opaque cohort tag, so that no single high-volume
    /// caller defines a lane's correction. It is a reduction dimension only,
    /// never part of the lane key, and it is hashed before it is stored: the
    /// store outlives the request and must hold nothing that identifies one.
    /// Every keyless request shares one cohort, so a lane fed only keyless
    /// traffic never reaches the distinct-cohort floor and stays uncorrected.
    ///
    /// A single mutex lock plus a small push; it must never fail, panic, or
    /// meaningfully slow the request, so it returns nothing and swallows the
    /// no-lane case silently.
    pub fn record_calibration_sample(
        &self,
        provider_kind: Option<&str>,
        nickname: Option<&str>,
        session_key: Option<&str>,
        estimated_tokens: u64,
        prompt_tokens: u64,
        ts: std::time::SystemTime,
    ) {
        let (Some(provider_kind), Some(nickname)) = (provider_kind, nickname) else {
            return;
        };
        let key = crate::calibration::LaneKey {
            provider_kind: provider_kind.to_string(),
            nickname: nickname.to_string(),
        };
        let cohort = crate::calibration::cohort_of(session_key);
        self.calibration_store
            .record(key, estimated_tokens, prompt_tokens, cohort, ts);
    }

    /// Every calibration lane currently holding evidence, as
    /// `(provider_kind, nickname)` pairs.
    ///
    /// The store itself stays encapsulated; this hands out lane IDENTITIES
    /// only, never samples. Read surface for boot / reload observability and
    /// for pinning that a refused request left no live evidence behind.
    pub fn calibration_lanes(&self) -> Vec<(String, String)> {
        self.calibration_store
            .export_entries()
            .into_iter()
            .map(|(key, _)| (key.provider_kind, key.nickname))
            .collect()
    }

    /// Replay a slice of persisted calibration evidence into the private
    /// per-lane store during boot warm-rebuild, returning the tally.
    ///
    /// Delegates to the calibration module's rebuild over the store this
    /// Router owns, so the store stays encapsulated rather than handed out --
    /// same shape as the learned-capability warm. Rows are replayed through
    /// the SAME store write the live path uses, so no second validation or
    /// reduction path exists to diverge from it.
    ///
    /// Rows naming a nickname absent from this Router's resolved model table
    /// are dropped: a history of renamed models would otherwise grow the lane
    /// map with lanes that can never serve a request.
    ///
    /// Bootstrap only. A hot reload carries the live store over instead
    /// (`carry_over_calibration_from`); re-reading history there would clobber
    /// fresher live samples with older evidence.
    pub fn rebuild_calibration_from_ledger(
        &self,
        reader: &dyn crate::calibration::CalibrationLedgerReader,
        now: std::time::SystemTime,
        limit: usize,
    ) -> crate::calibration::CalibrationRebuildSummary {
        crate::calibration::rebuild_into(
            reader,
            &self.calibration_store,
            &|nickname| self.knows_nickname(nickname),
            now,
            limit,
        )
    }

    /// Look up a model nickname in the resolved table.
    fn resolve_nickname(&self, nickname: &str) -> Option<Arc<ResolvedModel>> {
        self.resolved_models.get(nickname).cloned()
    }

    /// Register a provider under a name, ensuring a runtime gate exists
    /// for it even when no matching config entry was present.
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let name = name.into();
        // Ensure a gate exists even for providers registered without a
        // matching config entry (test harnesses rely on this).
        self.state
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&Default::default()))));
        self.providers.insert(name, provider);
    }

    /// Emit this Router's observability counters as one structured
    /// `tracing::debug!` line, mirroring the front-proxy's metrics-snapshot
    /// convention. Intended to be driven on an interval and once more at
    /// graceful shutdown by the owning process, the same shape as the
    /// front-proxy's own driver. No token, credential, or request/response
    /// body content ever reaches this call -- every field is a counter name
    /// plus its numeric value.
    pub fn log_metrics_snapshot(&self) {
        self.metrics
            .log_snapshot(self.quota_store.refused_by_admission_total());
    }

    /// Retain the build's sanitized per-pool reports and count their
    /// omissions.
    ///
    /// Retention rather than re-derivation: a pool's degraded state is only
    /// observable AT BUILD TIME (config alone cannot see a credential
    /// failure), so a read surface that re-derived it from config would report
    /// every pool as fully healthy. Counted here rather than in the factory so
    /// the totals land on the router instance the metrics carry-over shares --
    /// a rejected reload's build never reaches this call, which is why a
    /// rejected candidate cannot inflate the live router's counters.
    pub fn install_pool_reports(&mut self, reports: Vec<crate::pool_build::PoolReport>) {
        for report in &reports {
            for omission in &report.omissions {
                self.metrics.incr_pool_member_omitted(omission.reason);
            }
        }
        self.pool_reports = reports;
    }

    /// The sanitized per-pool build reports this Router was built from: what
    /// each pool was configured with, what it serves, and every member it
    /// lost. Read surface for the operator-facing pools report.
    #[must_use]
    pub fn pool_reports(&self) -> &[crate::pool_build::PoolReport] {
        &self.pool_reports
    }

    /// Record that a sticky pin was re-picked onto a surviving member because
    /// its pinned member left the pool.
    ///
    /// The public driver of the removed-member re-pick counter, called once per
    /// re-picked pin by [`Router::carry_over_pool_state_from`].
    pub fn note_pool_removed_pin_repick(&self) {
        self.metrics.incr_pool_removed_pin_repick();
    }
}

#[cfg(test)]
use chain::{dispatch_target_for_seat, into_one_dispatch_target};
#[cfg(test)]
use class_observe::DispatchSurface;
#[cfg(test)]
use dispatch::{
    retry_cap_for, should_fallback, should_retry_same_provider, would_trim_k_floor_for_meta,
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

#[cfg(test)]
#[path = "quota_feed_dispatch_tests.rs"]
mod quota_feed_dispatch_tests;

#[cfg(test)]
#[path = "quota_placement_dispatch_tests.rs"]
mod quota_placement_dispatch_tests;

#[cfg(test)]
#[path = "prefix_rewrite_dispatch_tests.rs"]
mod prefix_rewrite_dispatch_tests;
