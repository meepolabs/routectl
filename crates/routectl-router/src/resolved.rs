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
//!   - `reasoning`: per-model `ReasoningDefaults` projected from the
//!     model's `thinking` + `effort` knobs.
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

use crate::config::{HistoryReasoning, ReasoningDefaults, ReasoningDialect};

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
    /// Operator-side reasoning defaults projected from `[models.X]
    /// thinking` + `[models.X] effort`. Empty when neither knob was
    /// set; the merge step short-circuits on empty.
    pub reasoning: ReasoningDefaults,
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
            reasoning: ReasoningDefaults::default(),
            reasoning_dialect: None,
            history_reasoning: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            stream_first_byte_timeout_ms: None,
        }
    }

    pub fn with_reasoning(mut self, defaults: ReasoningDefaults) -> Self {
        self.reasoning = defaults;
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
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("nickname", &self.nickname)
            .field("provider_name", &self.provider_name)
            .field("provider_id", &self.provider.id())
            .field("upstream", &self.upstream)
            .field("reasoning", &self.reasoning)
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
        assert!(m.reasoning.is_empty());
        let d = format!("{m:?}");
        assert!(d.contains("haiku"));
        assert!(d.contains("stub-test"));
    }

    #[test]
    fn with_reasoning_replaces_defaults() {
        let p: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let m = ResolvedModel::new("x", "p", p, "u")
            .with_reasoning(ReasoningDefaults::new().with_thinking("high"));
        assert_eq!(m.reasoning.thinking.as_deref(), Some("high"));
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
}
