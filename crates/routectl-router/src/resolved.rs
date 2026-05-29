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

use crate::config::{HistoryReasoning, ReasoningDialect};

/// One fully-resolved model entry: a nickname bound to a concrete
/// provider, an upstream string, and per-model knobs lifted off
/// `[models.X]`. See module docs.
#[derive(Clone)]
pub struct ResolvedModel {
    /// The model entry's table key in `[models]`. Used as the
    /// tracing field `model = <nickname>` so a multi-hop chain shows
    /// which model the operator configured, not just which provider
    /// answered.
    pub nickname: String,
    /// The provider's table key in `[providers]`. Used for runtime-
    /// gate lookups (RPM bucket, circuit breaker) which are keyed by
    /// provider name.
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
    /// Per-model first-byte timeout for streaming responses. Wins
    /// over per-provider + global tiers when set.
    pub stream_first_byte_timeout_ms: Option<u64>,
    /// Source `SecretRef` used to resolve this model's primary auth
    /// credential at provider-build time. Retained on the resolved
    /// model so the router can wire 401 self-heal back through the
    /// originating store (the OAuth store refresh path keys off this).
    /// `None` for provider kinds that don't carry a primary api-key
    /// reference (e.g. Bedrock under DefaultChain / Profile creds).
    pub auth_secret_ref: Option<routectl_auth::SecretRef>,
}

impl ResolvedModel {
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
            auth_secret_ref: None,
        }
    }

    pub fn with_supports_adaptive_thinking(mut self, val: bool) -> Self {
        self.supports_adaptive_thinking = val;
        self
    }

    pub fn with_effort_levels(mut self, levels: Vec<String>) -> Self {
        self.effort_levels = Arc::from(levels.as_slice());
        self
    }

    pub fn with_max_thinking_budget(mut self, budget: u32) -> Self {
        self.max_thinking_budget = budget;
        self
    }

    pub fn with_reasoning_dialect(mut self, d: ReasoningDialect) -> Self {
        self.reasoning_dialect = Some(d);
        self
    }

    pub fn with_history_reasoning(mut self, h: HistoryReasoning) -> Self {
        self.history_reasoning = Some(h);
        self
    }

    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        self.header_extras = headers;
        self
    }

    pub fn with_payload_extras(mut self, extras: Value) -> Self {
        self.payload_extras = Some(extras);
        self
    }

    /// Set the per-model `stream_first_byte_timeout_ms` override.
    /// Wins over per-provider and global resolution. A value of 0 is
    /// an operator-error sentinel (every stream would time out before
    /// the first chunk arrived); flagged in debug builds.
    pub fn with_stream_first_byte_timeout_ms(mut self, ms: u64) -> Self {
        debug_assert!(
            ms > 0,
            "stream_first_byte_timeout_ms must be > 0; 0 would time out every stream",
        );
        self.stream_first_byte_timeout_ms = Some(ms);
        self
    }

    /// Attach the source `SecretRef` that resolved this model's
    /// primary auth credential. Used by the 401 self-heal path so a
    /// refresh hook can run against the originating store.
    pub fn with_auth_secret_ref(mut self, sr: routectl_auth::SecretRef) -> Self {
        self.auth_secret_ref = Some(sr);
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
            .field(
                "auth_secret_ref",
                &self.auth_secret_ref.as_ref().map(|sr| sr.to_string()),
            )
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
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
            Ok(None)
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
