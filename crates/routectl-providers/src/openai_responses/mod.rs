//! OpenAI Responses API provider (`openai-responses` provider type).
//!
//! Three auth surfaces (only one wired in the relevant stage):
//!
//!   - `chatgpt-oauth` (the relevant stage, default): ChatGPT subscription surface
//!     at `https://chatgpt.com/backend-api/codex`. Uses
//!     Authorization: Bearer <jwt> + ChatGPT-Account-Id + originator
//!     headers (codex parity).
//!   - `api-key` (the relevant stage, deferred): standard OpenAI surface at
//!     `https://api.openai.com/v1`. Calling today returns a clean
//!     not-implemented Error from auth.rs.
//!   - `bedrock-mantle` (the relevant stage, deferred): AWS Mantle proxy at
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
//! Response / SSE / Provider impl wiring all land in the relevant stage + the relevant stage;
//! `stream()` returns a NotImplemented Error today and `complete()`
//! returns an Error indicating the response path hasn't shipped.
//! That's intentional -- the request side is complete on its own and
//! locked behind the live smoke gate in the relevant stage.

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{
    sanitize_for_log, trace_outgoing_body, ChatChunk, ChatRequest, ChatResponse, Error,
    Provider, Result,
};

pub(crate) mod auth;
pub(crate) mod extras;
pub(crate) mod messages;
pub(crate) mod request;
pub(crate) mod system;
pub(crate) mod tools;
pub(crate) mod types;

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
    /// Standard OpenAI API key. Deferred to the relevant stage.
    ApiKey,
    /// AWS Bedrock Mantle proxy (OpenAI-shape over SigV4). Deferred
    /// to the relevant stage.
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

    fn normalize_response(&self, _raw: Value) -> Result<ChatResponse> {
        // the relevant stage lands the response side. Today's invocation surfaces a
        // clean error so the operator knows which milestone is the
        // fix site rather than getting a vague serde failure.
        Err(Error::normalize_response(
            &self.cfg.id,
            "openai-responses response decoding not yet implemented (the relevant stage)",
        ))
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        trace_outgoing_body("openai-responses", &self.cfg.id, &body);

        // the relevant stage will replace this stub with a full upstream call +
        // response decode. The request body is fully translated, but
        // there's no decoder yet, so we surface the same error
        // shape `normalize_response` would emit.
        let _ = self.build_headers(self.client.post(self.responses_url()))?;
        Err(Error::normalize_response(
            &self.cfg.id,
            "openai-responses complete() not yet implemented (the relevant stage)",
        ))
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        // the relevant stage owns the SSE/eventstream decoder. Emit a NotImplemented-
        // shape error rather than panic so an early caller sees a
        // clean failure path.
        let _ = self.normalize_request(&req)?;
        Err(Error::Streaming(format!(
            "openai-responses provider `{}`: stream() not yet implemented (the relevant stage)",
            self.cfg.id
        )))
    }
}
