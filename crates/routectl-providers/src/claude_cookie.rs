//! Claude.ai consumer-session provider (cookie auth).
//!
//! NOT api.anthropic.com — this hits claude.ai's internal endpoints and
//! reuses a browser session captured by `routectl login claude`. Reverse
//! engineered from the public claude.ai client. Out-of-tree responsibility:
//! user accepts ToS implications.
//!
//! Reasoning shape: claude.ai's internal API surfaces extended thinking
//! similar to the public Messages API but with envelope differences.
//! Normalization target is the same `anthropic-claude-v1` reasoning_details
//! format as the API provider.
//!
//! Status: stub. Implementation deferred until cookie capture flow lands.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

#[derive(Debug, Clone)]
pub struct ClaudeCookieConfig {
    pub id: String,
    /// Reference into routectl-auth keyring; resolves at request time.
    pub session_ref: String,
    /// Organization UUID from claude.ai (visible in URL after login).
    pub organization_id: Option<String>,
}

pub struct ClaudeCookieProvider {
    cfg: ClaudeCookieConfig,
}

impl ClaudeCookieProvider {
    pub fn new(cfg: ClaudeCookieConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Provider for ClaudeCookieProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, _req: &ChatRequest) -> Result<Value> {
        Err(Error::NormalizeRequest(
            self.id().to_string(),
            "claude-cookie not implemented".into(),
        ))
    }

    fn normalize_response(&self, _raw: Value) -> Result<ChatResponse> {
        Err(Error::NormalizeResponse(
            self.id().to_string(),
            "claude-cookie not implemented".into(),
        ))
    }

    fn normalize_chunk(&self, _raw: &str) -> Result<Option<ChatChunk>> {
        Ok(None)
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(Error::Upstream {
            provider: self.id().to_string(),
            status: 501,
            body: "claude-cookie not implemented".into(),
        })
    }

    async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::Upstream {
            provider: self.id().to_string(),
            status: 501,
            body: "claude-cookie stream not implemented".into(),
        })
    }
}
