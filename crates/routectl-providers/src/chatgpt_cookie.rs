//! ChatGPT.com consumer-session provider (cookie auth).
//!
//! Hits chatgpt.com/backend-api/* with a browser session captured by
//! `routectl login chatgpt`. Reverse engineered. Out-of-tree responsibility:
//! user accepts ToS implications.
//!
//! Auth surface is more involved than Claude.ai: requires both the session
//! cookie and a valid `cf_clearance` token, and may need periodic
//! re-challenge handling.
//!
//! Status: stub. Implementation deferred.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

#[derive(Debug, Clone)]
pub struct ChatGptCookieConfig {
    pub id: String,
    pub session_ref: String,
}

pub struct ChatGptCookieProvider {
    cfg: ChatGptCookieConfig,
}

impl ChatGptCookieProvider {
    pub fn new(cfg: ChatGptCookieConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Provider for ChatGptCookieProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, _req: &ChatRequest) -> Result<Value> {
        Err(Error::NormalizeRequest(
            self.id().to_string(),
            "chatgpt-cookie not implemented".into(),
        ))
    }

    fn normalize_response(&self, _raw: Value) -> Result<ChatResponse> {
        Err(Error::NormalizeResponse(
            self.id().to_string(),
            "chatgpt-cookie not implemented".into(),
        ))
    }

    fn normalize_chunk(&self, _raw: &str) -> Result<Option<ChatChunk>> {
        Ok(None)
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(Error::Upstream {
            provider: self.id().to_string(),
            status: 501,
            body: "chatgpt-cookie not implemented".into(),
        })
    }

    async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::Upstream {
            provider: self.id().to_string(),
            status: 501,
            body: "chatgpt-cookie stream not implemented".into(),
        })
    }
}
