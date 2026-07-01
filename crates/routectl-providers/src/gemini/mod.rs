//! Native Google Gemini egress provider.
//!
//! Sends requests to `POST {base_url}/models/{model}:generateContent`
//! (non-streaming) or `:streamGenerateContent?alt=sse` (streaming) using
//! the Gemini REST API (v1beta). Authentication is via the
//! `x-goog-api-key` header.
//!
//! Wire reference: <https://ai.google.dev/api/generate-content>

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use routectl_core::{
    debug_upstream_error_body, extract_upstream_message, is_json_error_envelope, sanitize_for_log,
    trace_outgoing_body, trace_upstream_success_body, ChatChunk, ChatRequest, ChatResponse,
    CloudProjectCache, Error, Provider, Result, StaticToken, TokenSource,
};

pub(crate) mod auth;
pub(crate) mod cloudcode;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod sse;
pub(crate) mod types;

pub use cloudcode::GeminiAuthMode;

/// Format tag stamped on every reasoning_details entry emitted by the
/// Gemini provider. A downstream ingress echoing reasoning back must see
/// the same tag across the non-streaming + streaming paths so the
/// request translator can recognize Gemini-origin reasoning (which
/// carries a `thought_signature`) and replay it as thought parts.
pub(crate) const GEMINI_FORMAT: &str = "gemini-v1";

const PROVIDER_KIND: &str = "gemini";
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Resolved configuration for one Gemini provider entry.
#[derive(Clone)]
pub struct GeminiConfig {
    /// Stable id used in errors and on `routectl_provider` response fields.
    /// Format: `gemini:<table-key>`.
    pub id: String,
    /// API key source. Resolved per request via `auth.token().await`.
    pub auth: Arc<dyn TokenSource>,
    /// Gemini API base URL. Default: `https://generativelanguage.googleapis.com/v1beta`.
    pub base_url: String,
    /// Provider-level extra HTTP headers.
    pub header_extras: Vec<(String, String)>,
    /// Override the outbound User-Agent.
    pub user_agent: Option<String>,
    /// Selects the wire dialect: public REST (`ApiKey`) or Cloud Code.
    pub mode: GeminiAuthMode,
    /// Project-id cache for the Cloud Code path. `None` in `ApiKey` mode.
    pub project_cache: Option<Arc<dyn CloudProjectCache>>,
    /// Base URL for the Cloud Code `onboardUser` endpoint (the reference
    /// onboards against the "daily" host). Unused in `ApiKey` mode.
    pub onboard_base_url: String,
    /// Poll interval between `onboardUser` attempts.
    pub onboard_poll_interval: Duration,
}

impl std::fmt::Debug for GeminiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiConfig")
            .field("id", &self.id)
            .field("auth", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("header_extras_len", &self.header_extras.len())
            .field("user_agent", &self.user_agent)
            .field("mode", &self.mode)
            .field("project_cache", &self.project_cache.is_some())
            .field("onboard_base_url", &self.onboard_base_url)
            .field("onboard_poll_interval", &self.onboard_poll_interval)
            .finish()
    }
}

/// Default poll interval between `onboardUser` attempts.
const ONBOARD_POLL_INTERVAL: Duration = Duration::from_secs(2);

impl GeminiConfig {
    /// Construct with a static API key string.
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new_with_auth(id, Arc::new(StaticToken::new(api_key)))
    }

    /// Construct with a custom `TokenSource`. Defaults to `ApiKey` mode;
    /// the Cloud Code fields are populated with inert defaults so callers
    /// (and the router factory) need not name them.
    pub fn new_with_auth(id: impl Into<String>, auth: Arc<dyn TokenSource>) -> Self {
        Self {
            id: id.into(),
            auth,
            base_url: DEFAULT_BASE_URL.to_string(),
            header_extras: Vec::new(),
            user_agent: None,
            mode: GeminiAuthMode::ApiKey,
            project_cache: None,
            onboard_base_url: cloudcode::DAILY_BASE_URL.to_string(),
            onboard_poll_interval: ONBOARD_POLL_INTERVAL,
        }
    }

    /// Construct a Cloud Code ("antigravity") egress: bearer-token auth
    /// against `cloudcode-pa.googleapis.com`, project id resolved lazily
    /// through `project_cache` (onboarding against the daily host).
    pub fn new_cloud_code(
        id: impl Into<String>,
        auth: Arc<dyn TokenSource>,
        project_cache: Arc<dyn CloudProjectCache>,
    ) -> Self {
        Self {
            id: id.into(),
            auth,
            base_url: cloudcode::PROD_BASE_URL.to_string(),
            header_extras: Vec::new(),
            user_agent: Some(cloudcode::SHORT_USER_AGENT.to_string()),
            mode: GeminiAuthMode::CloudCode,
            project_cache: Some(project_cache),
            onboard_base_url: cloudcode::DAILY_BASE_URL.to_string(),
            onboard_poll_interval: ONBOARD_POLL_INTERVAL,
        }
    }
}

pub struct GeminiProvider {
    cfg: GeminiConfig,
    client: Client,
}

impl GeminiProvider {
    pub fn new(cfg: GeminiConfig) -> Self {
        let client = crate::http_client::build(cfg.user_agent.as_deref());
        Self { cfg, client }
    }

    fn generate_url(&self, model: &str) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        format!("{base}/models/{model}:generateContent")
    }

    fn stream_url(&self, model: &str) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        format!("{base}/models/{model}:streamGenerateContent?alt=sse")
    }

    /// Cloud Code generate URL: a fixed `v1internal` path (the model lives
    /// in the request envelope, not the URL).
    fn cloudcode_generate_url(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        format!("{base}{}", cloudcode::GENERATE_PATH)
    }

    fn cloudcode_stream_url(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        format!("{base}{}", cloudcode::STREAM_PATH)
    }

    /// Resolve the Cloud Code project id, preferring the cache. On a cache
    /// miss this runs the onboarding HTTP once and seeds the cache, so a
    /// populated cache short-circuits onboarding on every later request.
    async fn cloud_project_id(&self, token: &str) -> Result<String> {
        let cache = self.cfg.project_cache.as_ref().ok_or_else(|| {
            Error::Internal(format!(
                "gemini provider `{}`: cloud_code mode without project_cache",
                self.cfg.id
            ))
        })?;
        if let Some(p) = cache.get().await {
            return Ok(p);
        }
        let p = cloudcode::resolve_project_id(
            &self.client,
            token,
            &self.cfg.base_url,
            &self.cfg.onboard_base_url,
            self.cfg.onboard_poll_interval,
            &self.cfg.id,
        )
        .await?;
        cache.put(p.clone()).await?;
        Ok(p)
    }

    fn build_headers(
        &self,
        rb: reqwest::RequestBuilder,
        req: &ChatRequest,
        key: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let mut rb = auth::apply(rb, key)?;

        let source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        let mut header_map = reqwest::header::HeaderMap::new();
        crate::http_client::apply_header_extras(&mut header_map, &source, &self.cfg.id, &[]);
        if !header_map.is_empty() {
            rb = rb.headers(header_map);
        }
        Ok(rb)
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        let r = request::translate(&self.cfg.id, req)?;
        let mut body = serde_json::to_value(&r)
            .map_err(|e| Error::normalize_request(&self.cfg.id, e.to_string()))?;
        // Merge dispatch-time provider + model `payload_extras` (carried
        // on `req.provider_extras`) so operator knobs like safetySettings
        // / topK reach the wire.
        if let Some(extras) = req.provider_extras.as_ref() {
            request::merge_payload_extras(&self.cfg.id, &mut body, extras);
        }
        Ok(body)
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        let typed: types::GenerateContentResponse = serde_json::from_value(raw)
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;
        response::translate(&self.cfg.id, typed)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        match self.cfg.mode {
            GeminiAuthMode::ApiKey => self.complete_api_key(req).await,
            GeminiAuthMode::CloudCode => self.complete_cloud_code(req).await,
        }
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        match self.cfg.mode {
            GeminiAuthMode::ApiKey => self.stream_api_key(req).await,
            GeminiAuthMode::CloudCode => self.stream_cloud_code(req).await,
        }
    }

    async fn on_auth_failure(&self) -> Result<()> {
        self.cfg.auth.on_auth_failure().await
    }
}

impl GeminiProvider {
    async fn complete_api_key(&self, req: ChatRequest) -> Result<ChatResponse> {
        let body = self.normalize_request(&req)?;
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let key = self.cfg.auth.token().await?;
        let url = self.generate_url(&req.model);
        let rb = self.build_headers(self.client.post(&url), &req, &key)?;
        let request = rb
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        if status >= 400 {
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_gemini_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
            ));
        }

        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);

        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    async fn complete_cloud_code(&self, req: ChatRequest) -> Result<ChatResponse> {
        let inner = self.normalize_request(&req)?;
        // Resolve credential + project id BEFORE tracing the wrapped body
        // so the trace reflects exactly what hits the wire (envelope with
        // the resolved project, never the raw inner body).
        let token = self.cfg.auth.token().await?;
        let project = self.cloud_project_id(&token).await?;
        let body = cloudcode::wrap_envelope(inner, &project, &req.model);
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let url = self.cloudcode_generate_url();
        // Cloud Code whitelists Content-Type / User-Agent / Authorization
        // only -- header_extras must NOT flow on this path. The short UA is
        // applied to the shared client at build time.
        let rb = auth::apply_bearer(self.client.post(&url), &token)?;
        let request = rb
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        if status >= 400 {
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_gemini_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
            ));
        }

        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);

        // Cloud Code nests the real response under `response`; peel it off
        // so the shared translator sees the public-surface shape.
        let mut chat_resp = self.normalize_response(cloudcode::unwrap_response(raw_body))?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    async fn stream_api_key(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let body = self.normalize_request(&req)?;
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let key = self.cfg.auth.token().await?;
        let url = self.stream_url(&req.model);
        let rb = self.build_headers(self.client.post(&url), &req, &key)?;
        let request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_gemini_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
            ));
        }

        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        Ok(self.drain_stream(resp, false))
    }

    async fn stream_cloud_code(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let inner = self.normalize_request(&req)?;
        let token = self.cfg.auth.token().await?;
        let project = self.cloud_project_id(&token).await?;
        let body = cloudcode::wrap_envelope(inner, &project, &req.model);
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let url = self.cloudcode_stream_url();
        let rb = auth::apply_bearer(self.client.post(&url), &token)?;
        let request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_gemini_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
            ));
        }

        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        Ok(self.drain_stream(resp, true))
    }

    /// Drain an SSE response into canonical chunks via the shared Gemini
    /// state machine. When `unwrap_cloud_code` is set, each event's data is
    /// peeled out of its `{"response": ...}` envelope before parsing.
    fn drain_stream(
        &self,
        resp: reqwest::Response,
        unwrap_cloud_code: bool,
    ) -> BoxStream<'static, Result<ChatChunk>> {
        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();

        let stream = async_stream::stream! {
            let mut state = sse::GeminiStreamState::default();
            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        if event.data.is_empty() {
                            continue;
                        }
                        let data = if unwrap_cloud_code {
                            cloudcode::unwrap_sse_data(&event.data)
                        } else {
                            event.data.clone()
                        };
                        let parsed = match sse::parse_data_line(&provider_id, &data) {
                            Ok(p) => p,
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        };
                        match state.parse_event(&provider_id, parsed) {
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

        routectl_core::wrap_stream_with_summary(
            stream,
            "upstream",
            PROVIDER_KIND,
            self.cfg.id.clone(),
        )
    }
}

/// Best-effort lift of the Gemini error classifier from a 4xx/5xx body.
/// The Gemini error envelope is `{"error":{"code":<int>,"message":...,
/// "status":"<UPPER_SNAKE>"}}`. `error.status` (e.g. `RESOURCE_EXHAUSTED`,
/// `INVALID_ARGUMENT`, `PERMISSION_DENIED`) is the string an SDK branches
/// on, so it lifts to `upstream_type`; the numeric `error.code` lifts to
/// `upstream_code` (stringified). Either is `None` when absent or the body
/// is not JSON. The Gemini envelope names its classifier `status`, unlike
/// the OpenAI `error.type`, hence the dedicated parser.
fn parse_gemini_error_classifier(body_text: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(body_text) else {
        return (None, None);
    };
    let upstream_type = v
        .pointer("/error/status")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let upstream_code = v.pointer("/error/code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    (upstream_type, upstream_code)
}

fn map_gemini_upstream_error(
    provider_id: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body_text: &str,
) -> Error {
    let header_hint = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, body_text);
    let (upstream_type, upstream_code) = parse_gemini_error_classifier(body_text);
    let msg = extract_upstream_message(body_text);
    let safe_excerpt = sanitize_for_log(&msg);
    crate::upstream_log::warn_upstream_failure(provider_id, status, None, &safe_excerpt, "gemini");
    let err_body = if is_json_error_envelope(body_text) {
        body_text.to_string()
    } else {
        msg
    };
    Error::upstream_full(
        provider_id,
        status,
        err_body,
        header_hint,
        upstream_type,
        upstream_code,
    )
}

// ---------------------------------------------------------------------------
// End-to-end tests (wiremock-driven complete path)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use routectl_core::{ChatRequest, MessageContent, TokenSource};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_provider(base_url: &str) -> GeminiProvider {
        let mut cfg = GeminiConfig::new("gemini:test", "test-api-key");
        cfg.base_url = base_url.to_string();
        GeminiProvider::new(cfg)
    }

    const CLOUD_CODE_TOKEN: &str = "ya29.test-bearer";

    fn make_cloud_code_provider(
        base_url: &str,
        cache: Arc<dyn routectl_core::CloudProjectCache>,
    ) -> GeminiProvider {
        let auth: Arc<dyn TokenSource> = Arc::new(StaticToken::new(CLOUD_CODE_TOKEN));
        let mut cfg = GeminiConfig::new_cloud_code("gemini:test", auth, cache);
        cfg.base_url = base_url.to_string();
        cfg.onboard_base_url = base_url.to_string();
        cfg.onboard_poll_interval = std::time::Duration::from_millis(1);
        GeminiProvider::new(cfg)
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![routectl_core::Message {
                refusal: None,
                role: routectl_core::Role::User,
                content: MessageContent::Text("ping".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    fn gemini_ok_response() -> serde_json::Value {
        serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "pong"}],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 1,
                "totalTokenCount": 6
            },
            "modelVersion": "gemini-2.5-pro-001",
            "responseId": "resp-abc"
        })
    }

    #[tokio::test]
    async fn complete_returns_chat_response_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:generateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(gemini_ok_response()),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        let resp = provider.complete(base_req()).await.expect("complete ok");

        assert_eq!(resp.id, "resp-abc");
        assert_eq!(resp.model, "gemini-2.5-pro-001");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.routectl_provider.as_deref(), Some("gemini:test"));
    }

    #[tokio::test]
    async fn complete_non_2xx_returns_upstream_error_with_body_excerpt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:generateContent"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#,
            ))
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        let err = provider
            .complete(base_req())
            .await
            .expect_err("expected err");
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 400);
                assert!(body.contains("API key not valid"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn parse_classifier_lifts_gemini_status_and_code() {
        // Gemini error envelope: `status` is the string classifier,
        // `code` is the numeric HTTP-status duplicate.
        let body = r#"{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}"#;

        let (upstream_type, upstream_code) = parse_gemini_error_classifier(body);

        assert_eq!(upstream_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
        assert_eq!(upstream_code.as_deref(), Some("429"));
    }

    #[test]
    fn parse_classifier_returns_none_on_non_json_body() {
        let (upstream_type, upstream_code) =
            parse_gemini_error_classifier("503 Service Unavailable (plain text from a proxy)");

        assert!(upstream_type.is_none());
        assert!(upstream_code.is_none());
    }

    #[test]
    fn map_error_lifts_classifier_and_preserves_retry_after() {
        // A 429 carrying its own classifier AND a Retry-After header: the
        // mapper must lift `status`/`code` onto upstream_type/upstream_code
        // (previously dropped by the `upstream_with_retry_after` path) while
        // still parking the provider via the reset hint.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        let body =
            r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;

        let err = map_gemini_upstream_error("gemini:test", 429, &headers, body);

        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                retry_after,
                body,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(upstream_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
                assert_eq!(upstream_code.as_deref(), Some("429"));
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
                // Structured envelope is carried raw so the ingress can
                // re-extract the upstream `error.message`.
                assert!(body.contains("Quota exceeded"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    fn gemini_stream_sse_body() -> &'static str {
        // Two SSE events mirroring what :streamGenerateContent?alt=sse
        // emits: a first partial carrying id/model + text, then a terminal
        // partial carrying more text + finishReason + usageMetadata.
        concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"pong\"}],\"role\":\"model\"},\"index\":0}],\"responseId\":\"resp-stream\",\"modelVersion\":\"gemini-2.5-pro-001\"}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" more\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n",
        )
    }

    #[tokio::test]
    async fn stream_returns_canonical_chunks_on_200() {
        use futures::StreamExt;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(gemini_stream_sse_body()),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        let mut req = base_req();
        req.stream = Some(true);
        let mut stream = provider.stream(req).await.expect("stream open");
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("chunk decoded without error"));
        }

        // Text deltas reassemble in arrival order across both events.
        let text: String = chunks
            .iter()
            .flat_map(|c| c.choices.iter())
            .filter_map(|ch| ch.delta.content.clone())
            .collect();
        assert_eq!(text, "pong more");

        // id / model threaded from the first event onto every chunk.
        assert!(chunks.iter().all(|c| c.id == "resp-stream"));
        assert!(chunks.iter().all(|c| c.model == "gemini-2.5-pro-001"));

        // Terminal chunk carries finish_reason + usage.
        let terminal = chunks.last().expect("at least one chunk");
        assert_eq!(terminal.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = terminal.usage.as_ref().expect("terminal usage");
        assert_eq!(usage.total_tokens, Some(7));
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let cfg = GeminiConfig::new("gemini:test", "super-secret-api-key");
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("super-secret-api-key"),
            "Debug must not leak the API key; got: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "Debug must mark the auth field redacted; got: {dbg}"
        );
    }

    #[tokio::test]
    async fn on_auth_failure_delegates_to_token_source() {
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct CountingToken {
            calls: AtomicUsize,
        }
        impl std::fmt::Debug for CountingToken {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("CountingToken").finish()
            }
        }
        #[async_trait]
        impl TokenSource for CountingToken {
            async fn token(&self) -> Result<String> {
                Ok("key".into())
            }
            async fn on_auth_failure(&self) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let src = Arc::new(CountingToken::default());
        let mut cfg = GeminiConfig::new("gemini:test", "unused");
        cfg.auth = src.clone() as Arc<dyn TokenSource>;
        let provider = GeminiProvider::new(cfg);

        provider.on_auth_failure().await.expect("ok");
        provider.on_auth_failure().await.expect("ok");
        assert_eq!(src.calls.load(Ordering::SeqCst), 2);
    }

    // -----------------------------------------------------------------
    // Cloud Code ("antigravity") egress
    // -----------------------------------------------------------------

    use routectl_core::{CloudProjectCache, InMemoryProjectCache};
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, header, header_exists};

    const GENERATE_PATH: &str = "/v1internal:generateContent";
    const STREAM_PATH: &str = "/v1internal:streamGenerateContent";
    const LOAD_PATH: &str = "/v1internal:loadCodeAssist";
    const ONBOARD_PATH: &str = "/v1internal:onboardUser";

    #[tokio::test]
    async fn envelope_wrap_and_response_unwrap() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(GENERATE_PATH))
            .and(header(
                "authorization",
                format!("Bearer {CLOUD_CODE_TOKEN}").as_str(),
            ))
            .and(body_partial_json(json!({
                "project": "proj-1",
                "model": "gemini-2.5-pro",
            })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).expect("json body");
                assert!(
                    body["request"]["contents"].is_array(),
                    "envelope must carry request.contents; got {body}"
                );
                assert!(
                    req.headers.get("x-goog-api-key").is_none(),
                    "Cloud Code path must not send x-goog-api-key"
                );
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"response": gemini_ok_response()}))
            })
            .mount(&server)
            .await;

        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::with("proj-1"));
        let provider = make_cloud_code_provider(&server.uri(), cache);

        let resp = provider.complete(base_req()).await.expect("complete ok");

        assert_eq!(resp.id, "resp-abc");
        assert_eq!(resp.model, "gemini-2.5-pro-001");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.routectl_provider.as_deref(), Some("gemini:test"));
    }

    #[tokio::test]
    async fn onboards_via_loadcodeassist() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(LOAD_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"cloudaicompanionProject": "proj-xyz"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(GENERATE_PATH))
            .and(body_partial_json(json!({"project": "proj-xyz"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"response": gemini_ok_response()})),
            )
            .mount(&server)
            .await;

        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::new());
        let provider = make_cloud_code_provider(&server.uri(), cache);

        provider.complete(base_req()).await.expect("first ok");
        provider.complete(base_req()).await.expect("second ok");
        // Mock `.expect(1)` on loadCodeAssist verifies on server drop that
        // onboarding ran exactly once -- the cache short-circuits the second.
    }

    #[tokio::test]
    async fn onboards_via_onboarduser() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(LOAD_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "allowedTiers": [{"id": "free-tier", "isDefault": true}]
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(ONBOARD_PATH))
            .and(header_exists("x-goog-api-client"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "done": true,
                        "response": {"cloudaicompanionProject": "proj-onboard"}
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(GENERATE_PATH))
            .and(body_partial_json(json!({"project": "proj-onboard"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"response": gemini_ok_response()})),
            )
            .mount(&server)
            .await;

        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::new());
        let provider = make_cloud_code_provider(&server.uri(), cache);

        let resp = provider.complete(base_req()).await.expect("complete ok");
        assert_eq!(resp.id, "resp-abc");
    }

    #[tokio::test]
    async fn stream_unwraps_response_envelope() {
        let sse_body = concat!(
            "data: {\"response\": {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"po\"}],\"role\":\"model\"},\"index\":0}],\"responseId\":\"r1\",\"modelVersion\":\"gemini-2.5-pro-001\"}}\n\n",
            "data: {\"response\": {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ng\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(STREAM_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::with("proj-1"));
        let provider = make_cloud_code_provider(&server.uri(), cache);

        let stream = provider.stream(base_req()).await.expect("stream ok");
        let chunks: Vec<ChatChunk> = stream
            .map(|r| r.expect("chunk ok"))
            .collect::<Vec<_>>()
            .await;

        let role_seen = chunks.iter().any(|c| {
            matches!(
                c.choices[0].delta.role,
                Some(routectl_core::Role::Assistant)
            )
        });
        assert!(role_seen, "expected an opening role chunk");

        let text: String = chunks
            .iter()
            .filter_map(|c| c.choices[0].delta.content.clone())
            .collect();
        assert_eq!(text, "pong");

        let terminal = chunks
            .iter()
            .find(|c| c.choices[0].finish_reason.is_some())
            .expect("terminal chunk");
        assert_eq!(terminal.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = terminal.usage.as_ref().expect("usage on terminal");
        assert_eq!(usage.prompt_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(7));
    }

    #[tokio::test]
    async fn preserves_reasoning_and_structured_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(GENERATE_PATH))
            .and(body_partial_json(json!({
                "request": {
                    "generationConfig": {
                        "responseMimeType": "application/json",
                        "responseSchema": {"type": "object", "properties": {"answer": {"type": "string"}}}
                    }
                }
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "response": {
                            "candidates": [{
                                "content": {
                                    "parts": [
                                        {"thought": true, "text": "reasoning...", "thoughtSignature": "sig"},
                                        {"text": "answer"}
                                    ],
                                    "role": "model"
                                },
                                "finishReason": "STOP",
                                "index": 0
                            }],
                            "modelVersion": "gemini-2.5-pro-001",
                            "responseId": "resp-rs"
                        }
                    })),
            )
            .mount(&server)
            .await;

        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::with("proj-1"));
        let provider = make_cloud_code_provider(&server.uri(), cache);

        let req = ChatRequest {
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object", "properties": {"answer": {"type": "string"}}}}
            })),
            ..base_req()
        };

        let resp = provider.complete(req).await.expect("complete ok");

        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "answer"),
            other => panic!("expected Text answer, got {other:?}"),
        }
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1, "one reasoning detail expected");
        assert_eq!(details[0].format.as_deref(), Some(GEMINI_FORMAT));
        assert_eq!(details[0].payload["text"], "reasoning...");
        assert_eq!(details[0].payload["thought_signature"], "sig");
    }
}
