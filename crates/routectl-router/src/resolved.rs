//! Pre-resolved model entry. `Router::new` walks `[models]` once at
//! startup and produces an `Arc<ResolvedModel>` per nickname. Dispatch
//! then walks an `Arc<ResolvedModel>` chain with a single O(1) lookup
//! per hop, instead of re-parsing `provider:model` strings on every
//! request.
//!
//! `ResolvedModel` carries everything the dispatch hot-path needs:
//!
//!   - `nickname`, `provider_name`, `provider`, `upstream`: identity
//!     + transport (see field docs for details).
//!   - `supports_adaptive_thinking`, `effort_levels`, `max_thinking_budget`:
//!     per-model capability knobs projected from `[models.X]`.
//!   - `reasoning_dialect` / `history_reasoning`: per-model
//!     openai-compat knobs (v0.6.0 moved off `[providers.X]`).
//!   - `header_extras` / `payload_extras`: per-model overlays merged
//!     into the request at dispatch (see `Router::merge_header_extras`
//!     + `Router::merge_payload_extras`).
//!   - `stream_first_byte_timeout_ms`: per-model timeout override.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::Provider;
use serde_json::Value;

use crate::catalog::EffectiveRow;
use crate::config::{HistoryReasoning, ReasoningDialect};

/// One fully-resolved model entry: a nickname bound to a concrete
/// provider, an upstream string, and per-model knobs lifted off
/// `[models.X]`. See module docs.
///
/// Hop 2 of 4 in the per-model knob relay -- see the `PER-MODEL KNOB
/// RELAY` note on `crate::config::ModelEntry` before adding a field
/// that the egress reads.
#[derive(Clone)]
pub struct ResolvedModel {
    /// The model entry's table key in `[models]`. Used as the
    /// tracing field `model = <nickname>` so a multi-hop chain shows
    /// which model the operator configured, not just which provider
    /// answered.
    pub nickname: String,
    /// The provider's table key in `[providers]`. Used for the
    /// providers-map mirror, the runtime-policy lookup (which
    /// `ProviderRuntimePolicy` to seed this model's gate from), and
    /// the operator-facing error label. The runtime gate STATE itself
    /// (RPM bucket, circuit breaker) is keyed by nickname (`state_key`),
    /// not by provider name.
    pub provider_name: String,
    /// The concrete provider instance.
    pub provider: Arc<dyn Provider>,
    /// Wire value of the `model` field on outbound requests.
    pub upstream: String,
    /// Whether the model supports adaptive (extended) thinking. When
    /// true, the egress may use `budget_tokens` to cap thinking tokens
    /// instead of a flat enable/disable toggle. Projected from
    /// `[models.X] supports_adaptive_thinking`.
    pub supports_adaptive_thinking: bool,
    /// Operator-declared effort levels for this model (e.g.
    /// `["minimal", "low", "medium", "high"]`). Projected from
    /// `[models.X] effort_levels`. Empty when not configured.
    ///
    /// `Arc<[String]>` so cloning at dispatch time is a refcount bump
    /// rather than a heap allocation.
    pub effort_levels: Arc<[String]>,
    /// Maximum thinking budget in tokens. `0` means no explicit cap
    /// configured. Projected from `[models.X] max_thinking_budget`.
    pub max_thinking_budget: u32,
    /// Per-model openai-compat reasoning dialect (v0.6.0 moved off
    /// `[providers.X]`). `None` means fall back to the provider's
    /// existing default (`ReasoningDialect::OpenAi` today).
    pub reasoning_dialect: Option<ReasoningDialect>,
    /// Per-model openai-compat outgoing-history reasoning policy.
    /// `None` means fall back to provider's default.
    pub history_reasoning: Option<HistoryReasoning>,
    /// Per-model header extras. Merged with the provider's
    /// `header_extras` at dispatch time (model wins on key collision;
    /// list-valued `anthropic-beta` runs through a comma-union
    /// post-pass).
    pub header_extras: BTreeMap<String, String>,
    /// Per-model payload extras. Deep-merged with the provider's
    /// `payload_extras` (model wins on leaf collision).
    pub payload_extras: Option<Value>,
    /// Per-model first-content timeout for streaming responses. Wins
    /// over per-provider + global tiers when set.
    pub stream_first_byte_timeout_ms: Option<u64>,
    /// Operator-declared per-model ceiling for the `max_tokens` value
    /// the Anthropic-shape egresses (anthropic-api, bedrock-invoke)
    /// inject on omitted-by-caller requests. `0` is the sentinel for
    /// "no model override" -- the egress then falls through to its
    /// hardcoded 64000 baseline. Projected from
    /// `[models.X] max_output_tokens`.
    pub max_output_tokens: u32,
    /// Operator-declared label echoed back in the response `model`
    /// field. `None` makes the response echo the client's requested
    /// alias (`req.model`); `Some(label)` overrides with a fixed
    /// public-facing string. An empty string is treated as unset at
    /// stamp time. Projected from `[models.X] reported_model`. Does
    /// not affect internal accounting / observability.
    pub reported_model: Option<String>,
    /// Whether the response `routectl_provider` field is exposed to the
    /// client for this model. `true` (the default) stamps the served
    /// provider name onto the response; `false` suppresses it. Projected
    /// from `[models.X] visible_routectl_provider`. Does not affect
    /// internal accounting / observability.
    pub visible_routectl_provider: bool,
    /// Source `SecretRef` used to resolve this model's primary auth
    /// credential at provider-build time. Retained on the resolved
    /// model so the router can wire 401 self-heal back through the
    /// originating store (the OAuth store refresh path keys off this).
    /// `None` for provider kinds that don't carry a primary api-key
    /// reference (e.g. Bedrock under DefaultChain / Profile creds).
    pub auth_secret_ref: Option<routectl_auth::SecretRef>,
    /// OAuth credential-pool seats. `None` for the common single-seat /
    /// non-pooled case -- dispatch then builds exactly ONE target keyed
    /// by `nickname` (byte-for-byte the pre-pool behavior). `Some` only
    /// when this model's primary `api_key_ref` was a bare-pool
    /// `oauth://<provider>` backed by MORE THAN ONE stored seat; the
    /// slice then holds one seat-pinned provider per seat (default seat
    /// first, then sorted labels), each with its own `state_key`.
    /// `provider` / `auth_secret_ref` above mirror the first (default)
    /// seat. `Arc<[..]>` so cloning at dispatch is a refcount bump.
    pub(crate) seats: Option<Arc<[crate::seat_pool::SeatTarget]>>,
    /// The two-layer catalog merge (baked table + overlay cell) resolved
    /// for this model's `(provider_kind, upstream)` selector at chain-build
    /// time. The merge runs here, once, when the resolved table is built
    /// (see `factory::apply_catalog_overlay`); the dispatch-path pricing
    /// helper (`Router::record_would_trim`) reads this precomputed result
    /// instead of re-running `lookup_baked_with_overrides` + `merge` per
    /// request. Defaults to [`EffectiveRow::Missing`] until
    /// `with_effective_row` stamps the real value.
    pub effective_row: EffectiveRow,
}

impl ResolvedModel {
    /// Build a resolved model from its nickname, provider binding, and
    /// upstream id, with all other knobs at their defaults.
    pub fn new(
        nickname: impl Into<String>,
        provider_name: impl Into<String>,
        provider: Arc<dyn Provider>,
        upstream: impl Into<String>,
    ) -> Self {
        Self {
            nickname: nickname.into(),
            provider_name: provider_name.into(),
            provider,
            upstream: upstream.into(),
            supports_adaptive_thinking: false,
            effort_levels: Arc::default(),
            max_thinking_budget: 0,
            reasoning_dialect: None,
            history_reasoning: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            stream_first_byte_timeout_ms: None,
            max_output_tokens: 0,
            reported_model: None,
            visible_routectl_provider: true,
            auth_secret_ref: None,
            seats: None,
            effective_row: EffectiveRow::Missing,
        }
    }

    /// Set the adaptive-thinking support flag.
    pub const fn with_supports_adaptive_thinking(mut self, val: bool) -> Self {
        self.supports_adaptive_thinking = val;
        self
    }

    /// Set the effort-level allowlist.
    pub fn with_effort_levels(mut self, levels: Vec<String>) -> Self {
        self.effort_levels = Arc::from(levels.as_slice());
        self
    }

    /// Set the maximum thinking-token budget.
    pub const fn with_max_thinking_budget(mut self, budget: u32) -> Self {
        self.max_thinking_budget = budget;
        self
    }

    /// Set the reasoning dialect.
    pub const fn with_reasoning_dialect(mut self, d: ReasoningDialect) -> Self {
        self.reasoning_dialect = Some(d);
        self
    }

    /// Set the outgoing-history reasoning policy.
    pub const fn with_history_reasoning(mut self, h: HistoryReasoning) -> Self {
        self.history_reasoning = Some(h);
        self
    }

    /// Set the per-model header extras.
    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        self.header_extras = headers;
        self
    }

    /// Set the per-model payload extras.
    pub fn with_payload_extras(mut self, extras: Value) -> Self {
        self.payload_extras = Some(extras);
        self
    }

    /// Set the per-model `stream_first_byte_timeout_ms` override.
    /// Wins over per-provider and global resolution. Caps the wait for
    /// the first content-bearing chunk (content-free leading chunks do
    /// not satisfy it). A value of 0 is an operator-error sentinel
    /// (every stream would time out before content arrived); flagged in
    /// debug builds.
    pub fn with_stream_first_byte_timeout_ms(mut self, ms: u64) -> Self {
        debug_assert!(
            ms > 0,
            "stream_first_byte_timeout_ms must be > 0; 0 would time out every stream",
        );
        self.stream_first_byte_timeout_ms = Some(ms);
        self
    }

    /// Set the per-model `max_output_tokens` ceiling. `0` is the
    /// sentinel for "no model override" -- the consuming Anthropic-
    /// shape egresses fall through to their hardcoded 64000 baseline.
    pub const fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    /// Set the client-visible label echoed in the response `model`
    /// field. `None` echoes the client's requested alias; an empty
    /// string is treated as unset at stamp time.
    pub fn with_reported_model(mut self, label: impl Into<String>) -> Self {
        self.reported_model = Some(label.into());
        self
    }

    /// Set whether the response `routectl_provider` field is exposed to
    /// the client for this model. Defaults to `true`.
    pub const fn with_visible_routectl_provider(mut self, b: bool) -> Self {
        self.visible_routectl_provider = b;
        self
    }

    /// Attach the source `SecretRef` that resolved this model's
    /// primary auth credential. Used by the 401 self-heal path so a
    /// refresh hook can run against the originating store.
    pub fn with_auth_secret_ref(mut self, sr: routectl_auth::SecretRef) -> Self {
        self.auth_secret_ref = Some(sr);
        self
    }

    /// Attach the OAuth credential-pool seats for a pooled model. Only
    /// called by the factory when the model's primary ref expanded to
    /// more than one seat; a single-seat / non-pooled model leaves
    /// `seats` as `None` and dispatches a single nickname-keyed target.
    pub(crate) fn with_seats(mut self, seats: Arc<[crate::seat_pool::SeatTarget]>) -> Self {
        self.seats = Some(seats);
        self
    }

    /// Stamp the precomputed two-layer catalog merge for this model.
    /// Called by `factory::apply_catalog_overlay` as a post-pass over the
    /// resolved table so `build_resolved_models` itself never needs to
    /// widen its own signature with an overlay parameter.
    #[must_use]
    pub fn with_effective_row(mut self, effective_row: EffectiveRow) -> Self {
        self.effective_row = effective_row;
        self
    }
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("nickname", &self.nickname)
            .field("provider_name", &self.provider_name)
            .field("provider_id", &self.provider.id())
            .field("upstream", &self.upstream)
            .field(
                "supports_adaptive_thinking",
                &self.supports_adaptive_thinking,
            )
            .field("effort_levels", &self.effort_levels)
            .field("max_thinking_budget", &self.max_thinking_budget)
            .field("reasoning_dialect", &self.reasoning_dialect)
            .field("history_reasoning", &self.history_reasoning)
            .field(
                "header_extras_keys",
                &self.header_extras.keys().collect::<Vec<_>>(),
            )
            .field("payload_extras_present", &self.payload_extras.is_some())
            .field(
                "stream_first_byte_timeout_ms",
                &self.stream_first_byte_timeout_ms,
            )
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reported_model", &self.reported_model)
            .field("visible_routectl_provider", &self.visible_routectl_provider)
            .field(
                "auth_secret_ref",
                &self
                    .auth_secret_ref
                    .as_ref()
                    .map(std::string::ToString::to_string),
            )
            .field(
                "seat_state_keys",
                &self
                    .seats
                    .as_ref()
                    .map(|s| s.iter().map(|t| t.state_key.as_str()).collect::<Vec<_>>()),
            )
            .field("effective_row", &self.effective_row)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

    struct StubProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for StubProvider {
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
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    #[test]
    fn resolved_model_construction_and_debug() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider {
            id: "stub-test".into(),
        });
        let m = ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5");
        assert_eq!(m.nickname, "haiku");
        assert_eq!(m.provider_name, "anthropic");
        assert_eq!(m.upstream, "claude-haiku-4-5");
        assert!(!m.supports_adaptive_thinking);
        assert!(m.effort_levels.is_empty());
        assert_eq!(m.max_thinking_budget, 0);
        let d = format!("{m:?}");
        assert!(d.contains("haiku"));
        assert!(d.contains("stub-test"));
    }

    #[test]
    fn with_capability_fields_set_correctly() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let levels = vec!["low".into(), "medium".into(), "high".into()];
        let m = ResolvedModel::new("x", "p", p, "u")
            .with_supports_adaptive_thinking(true)
            .with_effort_levels(levels.clone())
            .with_max_thinking_budget(16000);
        assert!(m.supports_adaptive_thinking);
        assert_eq!(&*m.effort_levels, levels.as_slice());
        assert_eq!(m.max_thinking_budget, 16000);
    }

    #[test]
    fn with_header_extras_sets_field() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let mut h = BTreeMap::new();
        h.insert("anthropic-beta".into(), "context-1m-2025-08-07".into());
        let m = ResolvedModel::new("x", "p", p, "u").with_header_extras(h);
        assert_eq!(
            m.header_extras.get("anthropic-beta"),
            Some(&"context-1m-2025-08-07".to_string())
        );
    }

    #[test]
    fn with_stream_first_byte_timeout_ms_sets_field() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u").with_stream_first_byte_timeout_ms(15_000);
        assert_eq!(m.stream_first_byte_timeout_ms, Some(15_000));
    }

    #[test]
    fn defaults_have_empty_header_extras_and_none_timeout() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u");
        assert!(m.header_extras.is_empty());
        assert!(m.payload_extras.is_none());
        assert!(m.stream_first_byte_timeout_ms.is_none());
    }

    #[test]
    fn default_effective_row_is_missing_until_stamped() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u");
        assert_eq!(m.effective_row, EffectiveRow::Missing);
    }

    #[test]
    fn with_effective_row_stamps_the_precomputed_merge() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u").with_effective_row(EffectiveRow::Disabled);
        assert_eq!(m.effective_row, EffectiveRow::Disabled);
    }

    #[test]
    fn resolved_model_with_auth_secret_ref_renders_via_display() {
        // Pin: the Debug impl renders auth_secret_ref through Display
        // (which redacts `Literal`), NOT through derived/manual Debug
        // (which would leak the inline value). Operators reading a
        // ResolvedModel out of tracing logs must never see the
        // plaintext literal.
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u")
            .with_auth_secret_ref(routectl_auth::SecretRef::Literal("hunter2".into()));
        let d = format!("{m:?}");
        assert!(
            !d.contains("hunter2"),
            "Debug must redact literal value: {d}"
        );
        assert!(
            d.contains("literal:[REDACTED]"),
            "Debug must show Display-redacted literal: {d}"
        );
    }
}
