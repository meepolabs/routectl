//! Generic OpenAI-compatible provider.
//!
//! Covers DeepSeek, OpenRouter, OpenAI, vLLM, NIM, llama.cpp, Together, Groq,
//! Fireworks, and any endpoint that speaks the OpenAI chat completions schema.
//! Distinguished by `base_url` + `api_key` config + a `ReasoningDialect` flag.
//!
//! Reasoning normalization overview:
//!   - `normalize_request`: strip/translate per dialect before sending upstream.
//!   - `normalize_response`: lift provider-specific reasoning fields into
//!     `reasoning_details` (DeepSeek `reasoning_content`, vLLM same, `<think>`
//!     tags for RawThinkTag).
//!   - `normalize_chunk`: stateless per-frame parsing (see NOTE below).
//!   - `stream`: owns a `ThinkTagAccumulator` for RawThinkTag state; all other
//!     dialects delegate to the stateless `parse_chunk`.
//!
//! NOTE on `normalize_chunk` vs `stream` statefulness:
//!   The `Provider` trait exposes `normalize_chunk(&self, raw: &str)` which is
//!   stateless by design (takes `&self`, no `&mut self`). The `<think>` tag
//!   state machine needs to track whether we are inside or outside a tag
//!   across multiple SSE chunks. This cannot live in `normalize_chunk`.
//!   Solution: `normalize_chunk` handles the stateless dialects (DeepSeek,
//!   vLLM, OpenAI, etc.) and is a no-op dispatcher for RawThinkTag.
//!   The stateful `ThinkTagAccumulator` lives inside `stream()` as a local
//!   variable captured by the stream future.

pub mod dialect;
pub mod request;
pub mod response;
pub mod sse;

pub use dialect::ReasoningDialect;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tracing::debug;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

use sse::ThinkTagAccumulator;

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub id: String,
    pub base_url: String,
    pub api_key: String,
    /// Optional extra headers (e.g. OpenRouter `HTTP-Referer`, `X-Title`).
    pub extra_headers: Vec<(String, String)>,
    /// Provider-level default extras merged into every request body.
    pub default_extras: Option<Value>,
    /// Which reasoning wire-format quirks apply.
    pub reasoning_dialect: ReasoningDialect,
}

pub struct OpenAiCompatProvider {
    cfg: OpenAiCompatConfig,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: OpenAiCompatConfig) -> Self {
        let client = reqwest::Client::new();
        Self { cfg, client }
    }

    fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'))
    }

    fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))
                .map_err(|e| Error::Config(format!("invalid api_key for header: {e}")))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (k, v) in &self.cfg.extra_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("invalid header name `{k}`: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("invalid header value for `{k}`: {e}")))?;
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(
            &self.cfg.id,
            req,
            self.cfg.reasoning_dialect,
            self.cfg.default_extras.as_ref(),
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw, self.cfg.reasoning_dialect)
    }

    /// Stateless per-chunk normalization. Handles DeepSeek/vLLM
    /// `reasoning_content` lifting. Returns `Ok(None)` for `[DONE]` and
    /// keepalive frames.
    ///
    /// For `RawThinkTag` dialect, callers that need cross-chunk state must
    /// use `ThinkTagAccumulator::process` directly (as `stream()` does).
    /// This method still parses the frame but skips tag-splitting.
    fn normalize_chunk(&self, raw: &str) -> Result<Option<ChatChunk>> {
        sse::parse_chunk(&self.cfg.id, raw, self.cfg.reasoning_dialect)
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        // Force non-streaming.
        body["stream"] = Value::Bool(false);

        let headers = self.build_headers()?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions");

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Error::upstream(&self.cfg.id, status, sanitize_upstream_body(&body_text)));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;

        let mut chat_resp = self.normalize_response(raw)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        body["stream"] = Value::Bool(true);

        let headers = self.build_headers()?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions (stream)");

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Error::upstream(&self.cfg.id, status, sanitize_upstream_body(&body_text)));
        }

        let provider_id = self.cfg.id.clone();
        let dialect = self.cfg.reasoning_dialect;

        // The ThinkTagAccumulator owns state across chunks; it lives in the
        // stream task closure for RawThinkTag dialect.
        // async_stream::stream! lets us hold mutable local state (think_acc)
        // across yield points, which filter_map/FnMut cannot do.
        let mut event_stream = resp.bytes_stream().eventsource();

        let out = async_stream::stream! {
            let mut think_acc = ThinkTagAccumulator::new();
            while let Some(event_result) = event_stream.next().await {
                let event = match event_result {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(Error::Streaming(format!(
                            "provider `{provider_id}`: SSE error: {e}"
                        )));
                        return;
                    }
                };

                let data = event.data;
                let trimmed = data.trim();
                if trimmed == "[DONE]" {
                    // Per OpenAI spec, `[DONE]` is the terminator; some
                    // providers (e.g. OpenCode-Go) emit cost-tracking
                    // trailer chunks after it, which we must not try to
                    // parse as ChatChunk.
                    return;
                }
                if trimmed.is_empty() {
                    continue;
                }

                let result = if dialect == ReasoningDialect::RawThinkTag {
                    think_acc.process(&provider_id, &data)
                } else {
                    sse::parse_chunk(&provider_id, &data, dialect)
                };

                match result {
                    Ok(None) => {}
                    Ok(Some(chunk)) => yield Ok(chunk),
                    Err(e) => yield Err(e),
                }
            }
        };

        Ok(Box::pin(out))
    }
}

/// Trim and sanitize an upstream error body for inclusion in our own error
/// messages. If the upstream returned HTML (a marketing 404 page from a
/// misconfigured base_url, for example), we strip it down to a short marker
/// rather than dumping kilobytes of markup through routectl's error envelope.
fn sanitize_upstream_body(body: &str) -> String {
    const MAX_LEN: usize = 512;
    let trimmed = body.trim();
    let looks_like_html = trimmed.starts_with('<')
        || trimmed.to_ascii_lowercase().contains("<!doctype");
    if looks_like_html {
        return format!("<html error page, {} bytes>", body.len());
    }
    if trimmed.len() <= MAX_LEN {
        return trimmed.to_string();
    }
    let mut s = trimmed.chars().take(MAX_LEN).collect::<String>();
    s.push_str("... [truncated]");
    s
}
