//! The provider a pool-backed model projects when none of its seats built.
//!
//! A `ResolvedModel` always carries one provider, but a pool whose every
//! member is a placeholder has none to offer. This stands in for it. Dispatch
//! walks the model's seats and refuses each placeholder before reading any
//! provider, so this is never dialed; every method answers the same local
//! `credential_unusable` refusal should a caller reach it anyway.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, EligibilityReason, Error, ProbeOutcome, Provider, Result,
    TokenCount,
};
use serde_json::Value;

/// The `id()` of the inert provider.
const INERT_PROVIDER_ID: &str = "unusable-pool";

struct InertProvider;

const fn unusable() -> Error {
    Error::LocallyUnavailable {
        reason: EligibilityReason::CredentialUnusable,
        retry_after: None,
    }
}

#[async_trait]
impl Provider for InertProvider {
    fn id(&self) -> &str {
        INERT_PROVIDER_ID
    }

    fn normalize_request(&self, _req: &ChatRequest) -> Result<Value> {
        Err(unusable())
    }

    fn normalize_response(&self, _raw: Value) -> Result<ChatResponse> {
        Err(unusable())
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(unusable())
    }

    async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(unusable())
    }

    async fn count_tokens(&self, _req: ChatRequest) -> Result<TokenCount> {
        Err(unusable())
    }

    async fn probe(&self) -> ProbeOutcome {
        ProbeOutcome::Skipped("no seat of the pool has a usable credential".into())
    }
}

/// A provider that refuses every call with a local `credential_unusable`.
pub(super) fn inert_provider() -> Arc<dyn Provider> {
    Arc::new(InertProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_dispatch_method_refuses_credential_unusable_without_a_wait() {
        // Arrange
        let provider = inert_provider();
        let is_unusable = |err: Error| {
            matches!(
                err,
                Error::LocallyUnavailable {
                    reason: EligibilityReason::CredentialUnusable,
                    retry_after: None,
                }
            )
        };

        // Act / Assert
        assert!(is_unusable(
            provider.complete(ChatRequest::default()).await.unwrap_err()
        ));
        assert!(is_unusable(
            provider.stream(ChatRequest::default()).await.err().unwrap()
        ));
        assert!(is_unusable(
            provider
                .count_tokens(ChatRequest::default())
                .await
                .unwrap_err()
        ));
        assert!(is_unusable(
            provider
                .normalize_request(&ChatRequest::default())
                .unwrap_err()
        ));
    }
}
