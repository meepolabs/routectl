//! Anthropic Messages API provider (api.anthropic.com).
//!
//! Wire format: <https://docs.anthropic.com/en/api/messages>
//! Extended thinking: <https://platform.claude.com/docs/en/docs/build-with-claude/extended-thinking>
//!
//! Reasoning normalization:
//! - Request: `reasoning.max_tokens` -> `thinking.budget_tokens`,
//!   `reasoning.effort` -> proportional `budget_tokens`.
//! - Response: content[] thinking blocks -> `reasoning_details[format="anthropic-claude-v1"]`
//!   with signature preserved for multi-turn tool-use continuity.
//! - Multi-turn: thinking blocks are passed back unmodified; signature is mandatory.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

pub mod request;
pub mod response;
pub mod sse;
pub mod types;

use sse::SseState;

/// How the provider authenticates to the Anthropic Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// Standard `x-api-key: <key>` header. Default for `sk-ant-api03-...` keys.
    #[default]
    ApiKey,
    /// OAuth bearer for subscription tokens (e.g. Claude Code's
    /// `sk-ant-oat01-...` access token). Sends `Authorization: Bearer <key>`
    /// plus the `anthropic-beta: oauth-2025-04-20` gate.
    OauthBearer,
}

#[derive(Debug, Clone)]
pub struct AnthropicApiConfig {
    pub id: String,
    pub api_key: String,
    pub base_url: String,
    pub anthropic_version: String,
    pub auth_kind: AuthKind,
}

impl AnthropicApiConfig {
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
        }
    }
}

pub struct AnthropicApiProvider {
    cfg: AnthropicApiConfig,
    client: Client,
}

impl AnthropicApiProvider {
    pub fn new(cfg: AnthropicApiConfig) -> Self {
        let client = Client::builder()
            .build()
            .expect("failed to build reqwest client");
        Self { cfg, client }
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    fn apply_auth_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.header("anthropic-version", &self.cfg.anthropic_version);
        match self.cfg.auth_kind {
            AuthKind::ApiKey => rb.header("x-api-key", &self.cfg.api_key),
            AuthKind::OauthBearer => rb
                .header("authorization", format!("Bearer {}", self.cfg.api_key))
                .header("anthropic-beta", "oauth-2025-04-20"),
        }
    }
}

#[async_trait]
impl Provider for AnthropicApiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(&self.cfg.id, req)
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw)
    }

    /// Stateless single-frame parse. For full streaming use stream().
    fn normalize_chunk(&self, raw: &str) -> Result<Option<ChatChunk>> {
        sse::parse_stateless(&self.cfg.id, raw)
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        // Ensure stream is absent / false for the non-streaming path.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream");
        }

        let resp = self
            .apply_auth_headers(self.client.post(&self.messages_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;

        if status >= 400 {
            let msg = raw_body
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("upstream error")
                .to_string();
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
        }

        let resp = self
            .apply_auth_headers(self.client.post(&self.messages_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let raw_body: Value = resp
                .json()
                .await
                .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
            let msg = raw_body
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("upstream error")
                .to_string();
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();

        let stream = async_stream::stream! {
            let mut state = SseState::default();

            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        match state.parse_event(&provider_id, &event.data) {
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                            Ok(Some(chunk)) => yield Ok(chunk),
                            Ok(None) => {}
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}
