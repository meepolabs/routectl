//! OpenAI Responses API provider (`openai-responses` provider type).
//!
//! Three auth surfaces (only one operational as of CG.B):
//!
//!   - `chatgpt-oauth` (CG.A, default): ChatGPT subscription surface
//!     at `https://chatgpt.com/backend-api/codex`. Uses
//!     Authorization: Bearer <jwt> + ChatGPT-Account-Id + originator
//!     headers (codex parity). Fully wired: `complete()` + `stream()`
//!     both ship.
//!   - `api-key` (CG.E, deferred): standard OpenAI surface at
//!     `https://api.openai.com/v1`. Calling today returns a clean
//!     not-implemented Error from auth.rs.
//!   - `bedrock-mantle` (CG.D, deferred): AWS Mantle proxy at
//!     `https://bedrock-mantle.<region>.api.aws/openai/v1`. Same
//!     behavior as `api-key`: not-implemented Error today.
//!
//! Wire shape: OpenAI Responses API.
//!   - Request reference: `codex-rs/codex-api/src/common.rs::
//!     ResponsesApiRequest`.
//!   - Reasoning replay: `codex-rs/app-server-protocol/schema/
//!     typescript/ResponseItem.ts` -- `{type:"reasoning",
//!     summary:[...], encrypted_content: string|null}`. Routectl
//!     emits empty `encrypted_content: ""` when no signature is
//!     present; codex's `arc_monitor.rs:325-336` treats empty as a
//!     no-op for replay so this is safe.
//!
//! CG.B wires `complete()` and `stream()` end-to-end for the
//! chatgpt-oauth auth_kind; response translation lives in `response.rs`
//! and the streaming state machine in `sse.rs`. The remaining auth
//! surfaces (api-key, bedrock-mantle) still stub via auth.rs and the
//! live-smoke gate lands in CG.C.

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

pub(crate) mod auth;
pub(crate) mod extras;
pub(crate) mod messages;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod response_types;
pub(crate) mod sse;
pub(crate) mod system;
pub(crate) mod tools;
pub(crate) mod types;

/// Format tag stamped on every reasoning_details entry emitted by the
/// Responses provider. Multi-turn callers echoing reasoning back must
/// see the same tag across the non-streaming + streaming paths so a
/// downstream ingress can differentiate the Responses shape from the
/// Anthropic shape (Anthropic carries `signature`, Responses carries
/// `encrypted_content`).
pub(crate) const OPENAI_RESPONSES_FORMAT: &str = "openai-responses-v1";

/// How the provider authenticates to the Responses API.
///
/// Kebab-case on the TOML wire so config writes look natural:
///   `auth_kind = "chatgpt-oauth"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// ChatGPT subscription via OAuth bearer JWT. Default.
    #[default]
    ChatgptOauth,
    /// Standard OpenAI API key. Deferred to CG.E.
    ApiKey,
    /// AWS Bedrock Mantle proxy (OpenAI-shape over SigV4). Deferred
    /// to CG.D.
    BedrockMantle,
}

/// Resolved configuration for one Responses provider entry. The
/// factory builds this from the TOML `ProviderEntry::OpenaiResponses`
/// variant after resolving secret references.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesConfig {
    /// Stable id used in errors and on `routectl_provider` response
    /// fields. Format: `openai-responses:<table-key>`.
    pub id: String,
    /// Resolved auth secret (JWT for ChatgptOauth; API key for
    /// ApiKey; ignored for BedrockMantle which uses SigV4).
    pub api_key: String,
    /// Resolved ChatGPT account ID. Required for ChatgptOauth;
    /// must be None for the other variants (enforced by the factory).
    pub account_id: Option<String>,
    /// Endpoint base URL. Defaults are auth_kind-dependent (resolved
    /// by the factory):
    ///   - ChatgptOauth: `https://chatgpt.com/backend-api/codex`
    ///   - ApiKey: `https://api.openai.com/v1`
    ///   - BedrockMantle: `https://bedrock-mantle.<region>.api.aws/openai/v1`
    pub base_url: String,
    /// Auth dispatch.
    pub auth_kind: AuthKind,
    /// Extra HTTP headers applied to every Responses request. Reserved
    /// header names (`authorization`, `host`, `content-type`, ...) are
    /// rejected at apply-time to keep the auth contract intact.
    pub extra_headers: Vec<(String, String)>,
    /// Override the User-Agent. `None` -> default
    /// `routectl/<version> codex-cli`.
    pub user_agent: Option<String>,
    /// Override the `originator` header sent on ChatgptOauth.
    /// `None` -> `codex_cli_rs` (codex's `DEFAULT_ORIGINATOR`).
    pub originator: Option<String>,
}

impl OpenAiResponsesConfig {
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            api_key: api_key.into(),
            account_id: None,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind: AuthKind::ChatgptOauth,
            extra_headers: Vec::new(),
            user_agent: None,
            originator: None,
        }
    }
}

pub struct OpenAiResponsesProvider {
    cfg: OpenAiResponsesConfig,
    client: Client,
}

impl OpenAiResponsesProvider {
    pub fn new(cfg: OpenAiResponsesConfig) -> Self {
        // Always pass an explicit UA string so the client-level default
        // header carries the codex-derived value. Operator-supplied
        // `cfg.user_agent` wins; otherwise fall back to the canonical
        // "routectl/<version> codex-cli" string from auth::default_user_agent.
        let ua = cfg
            .user_agent
            .clone()
            .unwrap_or_else(auth::default_user_agent);
        let client = crate::http_client::build(Some(&ua));
        Self { cfg, client }
    }

    /// URL for the `/responses` endpoint. ChatgptOauth talks to the
    /// `backend-api/codex` surface; api-key talks to `/v1/responses`
    /// directly. The base_url already encodes the difference -- we
    /// just append `/responses`.
    fn responses_url(&self) -> String {
        format!("{}/responses", self.cfg.base_url.trim_end_matches('/'))
    }

    fn build_headers(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let mut rb = auth::apply(rb, &self.cfg)?;
        for (k, v) in &self.cfg.extra_headers {
            // Defense-in-depth: refuse to let TOML-supplied
            // `extra_headers` stomp on the auth header we just set.
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
        Ok(rb)
    }
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        let r = request::translate(&self.cfg, req)?;
        serde_json::to_value(&r).map_err(|e| Error::normalize_request(&self.cfg.id, e.to_string()))
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        let typed: response_types::ResponsesResponse = serde_json::from_value(raw)
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;
        response::translate(&self.cfg.id, typed)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        // The chatgpt-oauth Responses endpoint is stream-only: it returns
        // HTTP 400 {"detail":"Stream must be set to true"} when stream=false.
        // We implement complete() by forcing stream=true, consuming the SSE
        // until the `response.completed` event fires (which carries the full
        // ResponsesResponse body), then translating that body to ChatResponse.
        // Confirmed stream-only behavior: smoke 2026-05-12.
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        trace_outgoing_body("openai-responses", &self.cfg.id, &body);

        let rb = self.build_headers(self.client.post(self.responses_url()))?;
        let resp = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body_text = resp.text().await.unwrap_or_default();
            debug_upstream_error_body("openai-responses", &self.cfg.id, status, &body_text);
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
                    "openai-responses upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "openai-responses upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        // Drain the SSE stream until `response.completed` (or `response.failed`).
        // The completed event's `response` field is the full ResponsesResponse body.
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        futures::pin_mut!(event_stream);
        let mut completed_body: Option<Value> = None;
        while let Some(result) = event_stream.next().await {
            let event = result.map_err(|e| Error::Streaming(e.to_string()))?;
            if event.data.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&event.data)
                .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
            let kind = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "response.completed" | "response.failed" | "response.cancelled" => {
                    if let Some(r) = parsed.get("response") {
                        completed_body = Some(r.clone());
                    }
                    break;
                }
                _ => {}
            }
        }

        let raw_body = completed_body.ok_or_else(|| {
            Error::upstream(
                &self.cfg.id,
                0,
                "openai-responses: stream ended without response.completed event".to_string(),
            )
        })?;
        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        trace_outgoing_body("openai-responses", &self.cfg.id, &body);

        let rb = self.build_headers(self.client.post(self.responses_url()))?;
        let resp = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body_text = resp.text().await.unwrap_or_default();
            debug_upstream_error_body("openai-responses", &self.cfg.id, status, &body_text);
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
                    "openai-responses upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "openai-responses upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();

        let stream = async_stream::stream! {
            let mut state = sse::ResponsesStreamState::default();
            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        // Filter empty `data:` lines (keepalives).
                        if event.data.is_empty() {
                            continue;
                        }
                        let parsed = match sse::parse_data_line(&provider_id, &event.data) {
                            Ok(p) => p,
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        };
                        match state.process_event(&provider_id, parsed) {
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                            Ok(chunks) => {
                                for c in chunks {
                                    yield Ok(c);
                                }
                            }
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

// ---------------------------------------------------------------------------
// End-to-end tests (wiremock-driven complete + stream paths)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use futures::StreamExt;
    use routectl_core::{ChatRequest, MessageContent};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_provider(base_url: &str) -> OpenAiResponsesProvider {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:test".into(),
            api_key: "test-jwt".into(),
            account_id: Some("acct-uuid".into()),
            base_url: base_url.to_string(),
            auth_kind: AuthKind::ChatgptOauth,
            extra_headers: Vec::new(),
            user_agent: None,
            originator: None,
        };
        OpenAiResponsesProvider::new(cfg)
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gpt-5-codex".into(),
            messages: vec![routectl_core::Message {
                role: routectl_core::Role::User,
                content: MessageContent::Text("ping".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn complete_post_returns_chat_response() {
        // Arrange: complete() forces stream=true and drains SSE until
        // `response.completed`. The mock must return a proper SSE stream
        // with that terminal event (not a plain JSON body).
        let server = MockServer::start().await;
        let completed_body = serde_json::json!({
            "id": "resp_01",
            "object": "response",
            "status": "completed",
            "model": "gpt-5-codex",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "pong"}]
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        // Wrap in a `response.completed` SSE event (the only one we need).
        let event_body = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
            serde_json::to_string(&completed_body).unwrap()
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(event_body),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let resp = provider.complete(base_req()).await.expect("complete");

        // Assert
        assert_eq!(resp.id, "resp_01");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.routectl_provider.as_deref(), Some("openai-responses:test"));
    }

    #[tokio::test]
    async fn complete_non_2xx_returns_upstream_error_with_body_excerpt() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_string("{\"error\":{\"message\":\"oops\"}}"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let err = provider.complete(base_req()).await.expect_err("expected upstream err");

        // Assert
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 500);
                assert!(body.contains("oops"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_yields_error_on_truncated_sse() {
        // Arrange: a wiremock body that opens an SSE event but never
        // terminates it (no final `\n\n` framing, no `[DONE]`). The
        // stream loop should either yield a Streaming Err or simply
        // exhaust without panicking; what it MUST NOT do is loop
        // forever or unwrap a partial event.
        let server = MockServer::start().await;
        // Open `data: ` but no terminating blank line + no JSON body.
        // The eventsource decoder will treat this as a parse error or
        // as no event emitted; in both cases the stream must terminate
        // cleanly without panicking.
        let truncated = "data: {\"type\":\"response.created\",\"resp";
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(truncated)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let mut s = provider.stream(base_req()).await.expect("stream");
        let mut chunks: Vec<Result<ChatChunk, Error>> = Vec::new();
        while let Some(item) = s.next().await {
            chunks.push(item);
            // Bound the loop defensively so a regression doesn't hang
            // the test forever.
            if chunks.len() >= 16 {
                break;
            }
        }

        // Assert: stream terminated (didn't panic) and no chunks
        // beyond what could be parsed (an Err is acceptable too).
        let oks = chunks.iter().filter(|r| r.is_ok()).count();
        let errs = chunks.iter().filter(|r| r.is_err()).count();
        // Either we got 0 successful chunks + an Err, or we got
        // nothing at all (parser ate the partial line). Both are
        // acceptable; what we're guarding against is panic / hang.
        assert!(
            errs >= 1 || (oks == 0 && errs == 0),
            "expected truncated stream to yield either an Err or empty; got {oks} oks + {errs} errs"
        );
    }

    #[tokio::test]
    async fn stream_yields_chat_chunks_for_full_session() {
        // Arrange
        let server = MockServer::start().await;
        // Construct an SSE body with `data: <json>\n\n` framing.
        let events = vec![
            serde_json::json!({"type": "response.created", "response": {"id":"r","model":"m"}}),
            serde_json::json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "id":"m1", "role":"assistant", "content":[]}
            }),
            serde_json::json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id":"r", "status":"completed", "model":"m",
                    "output":[{"type":"message","id":"m1","role":"assistant",
                                "content":[{"type":"output_text","text":"hi"}]}],
                    "usage": {"input_tokens":1, "output_tokens":1, "total_tokens":2}
                }
            }),
        ];
        let sse_body: String = events
            .iter()
            .map(|e| format!("data: {}\n\n", e))
            .collect();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let mut s = provider.stream(base_req()).await.expect("stream");
        let mut chunks: Vec<ChatChunk> = Vec::new();
        while let Some(item) = s.next().await {
            chunks.push(item.expect("chunk ok"));
        }

        // Assert: created (role) + text delta + final = 3 chunks.
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("hi"));
        let final_c = chunks.last().unwrap();
        assert_eq!(final_c.choices[0].finish_reason.as_deref(), Some("stop"));
    }
}
