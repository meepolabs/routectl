//! Pre-resolved model entry. `Router::new` walks `[models]` once at
//! startup and produces an `Arc<ResolvedModel>` per nickname. Dispatch
//! then walks an `Arc<ResolvedModel>` chain with a single O(1) lookup
//! per hop, instead of re-parsing `provider:model` strings on every
//! request.
//!
//! `ResolvedModel` carries the four pieces dispatch needs together:
//!
//!   - `nickname`: stable identifier for tracing (`model = <nickname>`)
//!   - `provider_name`: the operator-facing provider key (used for
//!     the same per-provider gates / runtime state map keys)
//!   - `provider`: the concrete `Arc<dyn Provider>` instance.
//!     Multiple `ResolvedModel` entries may share one Arc when they
//!     route to the same non-Bedrock provider; Bedrock fans out to
//!     one Arc per model (each gets its own `BedrockConfig.model_id`).
//!   - `upstream`: the wire `model` value the provider sends upstream
//!   - `reasoning`: per-model operator-side `ReasoningDefaults`
//!     (lifted from `[models.X]` -- transport-side defaults are gone
//!     in v0.6.0).
//!
//! Built once and immutable thereafter -- if you find yourself
//! mutating one of these you almost certainly want to mint a new
//! resolved entry instead.

use std::sync::Arc;

use routectl_core::Provider;

use crate::config::ReasoningDefaults;

/// One fully-resolved model entry: a nickname bound to a concrete
/// provider, an upstream string, and per-model reasoning defaults.
/// See module docs.
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
    /// The concrete provider instance. Cached `Arc` for non-Bedrock
    /// providers (one per `[providers.X]`); per-model `Arc` for
    /// Bedrock since each model carries its own `BedrockConfig.model_id`.
    pub provider: Arc<dyn Provider>,
    /// Wire value of the `model` field on outbound requests. For
    /// openai-compat egresses this is the upstream model id; for
    /// Bedrock this is the AWS inference profile id (also stored
    /// internally on `BedrockConfig.model_id`).
    pub upstream: String,
    /// Operator-side reasoning defaults from `[models.X] thinking`
    /// and `[models.X] enabled`. Empty when the operator left both
    /// fields unset; the merge step short-circuits on empty.
    pub reasoning: ReasoningDefaults,
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
        }
    }

    pub fn with_reasoning(mut self, defaults: ReasoningDefaults) -> Self {
        self.reasoning = defaults;
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
}
