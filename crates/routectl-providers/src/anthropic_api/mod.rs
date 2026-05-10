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

use routectl_core::{
    debug_upstream_error_body, sanitize_for_log, sanitize_upstream_body, trace_outgoing_body,
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
};

pub(crate) mod parts;
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
    /// `sk-ant-oat01-...` access token). Sends `Authorization: Bearer <key>`.
    /// You must declare the `anthropic-beta: oauth-2025-04-20` gate
    /// yourself via `extra_headers` -- routectl no longer auto-injects it.
    OauthBearer,
}

#[derive(Debug, Clone)]
pub struct AnthropicApiConfig {
    pub id: String,
    pub api_key: String,
    pub base_url: String,
    pub anthropic_version: String,
    pub auth_kind: AuthKind,
    /// Extra HTTP headers applied to every Anthropic API request.
    /// Use this to declare `anthropic-beta` flags (e.g. `context-1m-2025-08-07`,
    /// `prompt-caching-2024-07-31`) or any other vendor-required header.
    /// Applied AFTER auth headers, so callers can override `anthropic-version`
    /// or `anthropic-beta` if they need to.
    pub extra_headers: Vec<(String, String)>,
    /// Override the User-Agent on outbound requests. Useful for IAM
    /// policies that gate access on `aws:UserAgent` (e.g. Claude Code's
    /// Bedrock role). `None` keeps reqwest's default UA.
    pub user_agent: Option<String>,
    /// Use the Opus 4.7+ adaptive thinking wire shape. When `Some(true)`,
    /// `request::normalize` rewrites `thinking: {type:"enabled",
    /// budget_tokens:N}` to `thinking: {type:"adaptive"}` and lifts
    /// `reasoning.effort` (verbatim string) into top-level
    /// `output_config.effort`. Older Claude models (4.5/4.6 family) keep
    /// the legacy shape so the flag is opt-in per provider rather than a
    /// compiled model-name match -- adaptive thinking is rolling out
    /// gradually and there is no clean naming pattern to gate on.
    /// `None` and `Some(false)` both mean "legacy shape".
    pub adaptive_thinking: Option<bool>,
}

impl AnthropicApiConfig {
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            extra_headers: Vec::new(),
            user_agent: None,
            adaptive_thinking: None,
        }
    }
}

pub struct AnthropicApiProvider {
    cfg: AnthropicApiConfig,
    client: Client,
}

impl AnthropicApiProvider {
    pub fn new(cfg: AnthropicApiConfig) -> Self {
        let client = crate::http_client::build(cfg.user_agent.as_deref());
        Self { cfg, client }
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    fn build_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb.header("anthropic-version", &self.cfg.anthropic_version);
        rb = match self.cfg.auth_kind {
            AuthKind::ApiKey => rb.header("x-api-key", &self.cfg.api_key),
            AuthKind::OauthBearer => {
                rb.header("authorization", format!("Bearer {}", self.cfg.api_key))
            }
        };
        for (k, v) in &self.cfg.extra_headers {
            // Defense-in-depth: refuse to let a TOML-supplied
            // `extra_headers` entry stomp on the auth header we just
            // set. Override of `anthropic-version` / `anthropic-beta`
            // remains intentional and supported.
            if crate::http_client::is_reserved_extra_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring reserved header from extra_headers (would bypass provider auth)"
                );
                continue;
            }
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb
    }
}

#[async_trait]
impl Provider for AnthropicApiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(
            &self.cfg.id,
            req,
            self.cfg.adaptive_thinking.unwrap_or(false),
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw)
    }

    /// Stateless single-frame parse. For full streaming use stream().
    fn normalize_chunk(&self, raw: &str) -> Result<Option<ChatChunk>> {
        sse::parse_stateless(&self.cfg.id, raw)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        // Ensure stream is absent / false for the non-streaming path.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream");
        }

        // PR C / FR-1: emit the outgoing body at trace level so a
        // grep by request_id correlates ingress -> egress -> upstream
        // response in one pass during triage. Gated by the
        // `tracing::Level::TRACE` filter -- production with default
        // info level pays nothing.
        trace_outgoing_body("anthropic", &self.cfg.id, &body);

        let resp = self
            .build_headers(self.client.post(self.messages_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        // On non-2xx, read the body as text FIRST so a non-JSON
        // upstream response (HTML 502 from a misconfigured proxy,
        // a CDN cleartext "rate limited" page, Anthropic's
        // occasional plain-text 529 "overloaded") doesn't get
        // collapsed into an opaque serde error. JSON parse is
        // attempted opportunistically to lift `error.message`; on
        // parse failure we fall back to a sanitized text excerpt
        // matching the openai-compat / bedrock pattern. Operators
        // grepping `body_excerpt=...` get a consistent shape across
        // providers.
        if status >= 400 {
            let body_text = resp.text().await.unwrap_or_default();
            // PR C / FR-1: emit the FULL upstream error body at debug
            // level so triage doesn't have to reproduce. The 200B
            // WARN excerpt below stays unchanged for `routectl-warn.log`
            // scannability.
            debug_upstream_error_body("anthropic", &self.cfg.id, status, &body_text);
            let msg = serde_json::from_str::<Value>(&body_text)
                .ok()
                .as_ref()
                .and_then(|v| v.pointer("/error/message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitize_upstream_body(&body_text));
            // PR C / FR-1: extend the auth-only WARN to all 4xx/5xx
            // so an operator never has to guess WHY a request failed.
            // Auth failures keep the auth_kind field for parity with
            // the documented log shape; other errors get a generic
            // "anthropic upstream error" tag.
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %msg,
                    "anthropic upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "anthropic upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
        }

        trace_outgoing_body("anthropic", &self.cfg.id, &body);

        let resp = self
            .build_headers(self.client.post(self.messages_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- see comment there.
            let body_text = resp.text().await.unwrap_or_default();
            debug_upstream_error_body("anthropic", &self.cfg.id, status, &body_text);
            let msg = serde_json::from_str::<Value>(&body_text)
                .ok()
                .as_ref()
                .and_then(|v| v.pointer("/error/message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitize_upstream_body(&body_text));
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %msg,
                    "anthropic upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "anthropic upstream error",
                );
            }
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
