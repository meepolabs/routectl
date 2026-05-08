//! Native AWS Bedrock provider.
//!
//! Speaks SigV4 directly to `bedrock-runtime.<region>.amazonaws.com`,
//! supporting both InvokeModel (per-vendor body shape -- Anthropic
//! Messages JSON for Claude, etc.) and Converse (vendor-neutral
//! envelope). Handles streaming via the AWS eventstream binary frame
//! format.
//!
//! ## Why not AWS SDK
//!
//! We could have pulled in `aws-sdk-bedrockruntime` whole. We don't,
//! because:
//!
//! 1. It would force every routectl user onto Smithy's HTTP client and
//!    duplicate the connection pool we already have via reqwest.
//! 2. The SDK's per-operation builders are ergonomic for callers but
//!    inflexible when you want to stream the body through a different
//!    protocol layer (we re-emit to clients as OpenAI SSE chunks or
//!    Anthropic Messages SSE).
//! 3. We need only signing + eventstream framing, not the full SDK.
//!
//! So we use the SDK's *building blocks* (`aws-config` for credential
//! chain, `aws-sigv4` for request signing, `aws-smithy-eventstream` for
//! frame parsing) and own the request/response shape end-to-end.
//!
//! ## Topology
//!
//! ```text
//! ChatRequest
//!     |
//!     v
//! { invoke.rs | converse.rs }   -- pick body-shape adapter by api_shape
//!     |
//!     v
//! signing.rs                    -- SigV4-sign the reqwest::Request
//!     |
//!     v
//! reqwest::Client (UA-overridden) -> bedrock-runtime endpoint
//!     |
//!     v non-stream                       v stream
//! response::parse_invoke_response     eventstream.rs (frame -> ChatChunk)
//! response::parse_converse_response
//!     |                                   |
//!     v                                   v
//! ChatResponse                         BoxStream<Result<ChatChunk>>
//! ```

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

pub mod auth;
pub mod converse;
pub mod endpoint;
pub mod eventstream;
pub mod invoke;
pub mod signing;

/// How routectl talks to Bedrock for a given provider entry.
///
/// `Invoke` sends the per-vendor body shape directly (e.g. Anthropic
/// Messages JSON for Claude, Mistral instruction shape for Mistral).
/// `Converse` sends AWS's vendor-neutral `{messages, inferenceConfig,
/// toolConfig, additionalModelRequestFields}` envelope and lets AWS
/// translate per vendor internally.
///
/// Default: `Invoke`. Wire fidelity with the existing `anthropic_api`
/// provider for Claude models, then opt into Converse on a per-provider
/// basis when adding non-Anthropic Bedrock models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BedrockApiShape {
    #[default]
    Invoke,
    Converse,
}

/// Resolved AWS credentials for a Bedrock provider. The TOML-level
/// `BedrockCreds` (in `routectl-router/src/config.rs`) carries
/// `SecretRef` URIs; the factory resolves them through the workspace
/// `SecretStore` and constructs this plaintext-side enum before
/// handing it to `BedrockProvider::new`. Same pattern as
/// `OpenAiCompatConfig::api_key` / `AnthropicApiConfig::api_key` -- the
/// providers crate never sees a `SecretRef`.
///
/// Four shapes:
///
/// - `BearerKey` -- short-term Bedrock API key from the AWS console.
///   Routectl skips SigV4 signing and sends `Authorization: Bearer <key>`.
/// - `Static` -- raw AWS access key + secret key + optional session token.
/// - `Profile` -- a named profile in `~/.aws/credentials`. Inherits SSO
///   refresh behavior from `aws-config`.
/// - `DefaultChain` -- AWS's standard provider chain: env -> profile ->
///   SSO -> web identity (IRSA) -> EC2/ECS metadata.
///
/// `Debug` is implemented manually below so panic messages and
/// `tracing` events with `?cfg` never leak the secret material.
/// `aws-credential-types::Credentials` follows the same pattern.
#[derive(Clone)]
#[non_exhaustive]
pub enum BedrockCreds {
    BearerKey {
        key: String,
    },
    Static {
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
    },
    Profile {
        name: String,
    },
    DefaultChain,
}

impl std::fmt::Debug for BedrockCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BearerKey { .. } => f.write_str("BedrockCreds::BearerKey { key: <redacted> }"),
            Self::Static {
                access_key,
                session_token,
                ..
            } => {
                // The access key id is widely treated as semi-public
                // (it appears in log lines AWS itself emits and in
                // Authorization headers on signed requests). Show a
                // short prefix so operators can tell which key is in
                // use, but never the secret_access_key or
                // session_token.
                let access_prefix: String = access_key.chars().take(4).collect();
                let has_session = session_token.is_some();
                write!(
                    f,
                    "BedrockCreds::Static {{ access_key: \"{access_prefix}***\", \
                     secret_key: <redacted>, session_token: {} }}",
                    if has_session { "<redacted>" } else { "None" }
                )
            }
            Self::Profile { name } => write!(f, "BedrockCreds::Profile {{ name: {name:?} }}"),
            Self::DefaultChain => f.write_str("BedrockCreds::DefaultChain"),
        }
    }
}

/// `Debug` is derived. `BedrockConfig` only contains a `BedrockCreds`
/// (which has its own redacting Debug impl above) plus non-secret
/// fields, so the derived impl is safe.
#[derive(Debug, Clone)]
pub struct BedrockConfig {
    pub id: String,
    /// AWS region (e.g. `us-west-2`). Affects the endpoint hostname and
    /// is part of the SigV4 signing scope.
    pub region: String,
    /// Bedrock model identifier or inference profile (e.g.
    /// `us.anthropic.claude-opus-4-7`, `global.anthropic.claude-opus-4-7`,
    /// `meta.llama4-maverick-17b-instruct-v1`). Cross-region inference
    /// profiles (`us.`, `global.`) and bare foundation model IDs have
    /// different streaming-permission boundaries; the `routectl doctor`
    /// command surfaces this.
    pub model_id: String,
    pub api_shape: BedrockApiShape,
    pub creds: BedrockCreds,
    /// Override the User-Agent on outbound requests. Required when the
    /// IAM policy gating Bedrock access uses an `aws:UserAgent`
    /// condition (e.g. Claude Code's role).
    pub user_agent: Option<String>,
    /// Extra HTTP headers applied to every request (after auth/UA).
    pub extra_headers: Vec<(String, String)>,
    /// Anthropic beta gates passed through to the model. For `Invoke`
    /// shape, these go in the request body's top-level `anthropic_beta`
    /// array; for `Converse`, they go in
    /// `additionalModelRequestFields.anthropic_beta`.
    pub anthropic_beta: Vec<String>,
    /// Free-form fields merged into the request body. For `Invoke`,
    /// merged at the top level; for `Converse`, merged into
    /// `additionalModelRequestFields`.
    pub additional_model_request_fields: Option<Value>,
}

pub struct BedrockProvider {
    cfg: BedrockConfig,
    resolved: auth::ResolvedCreds,
    client: reqwest::Client,
}

impl BedrockProvider {
    /// Construct a new BedrockProvider from a (declarative) config + a
    /// (resolved) credential handle. The caller is responsible for
    /// running `auth::resolve` to produce the second arg; this is
    /// typically done once in the router's factory.
    pub fn new(cfg: BedrockConfig, resolved: auth::ResolvedCreds) -> Self {
        let client = crate::http_client::build(cfg.user_agent.as_deref());
        Self { cfg, resolved, client }
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        match self.cfg.api_shape {
            BedrockApiShape::Invoke => invoke::normalize_request(&self.cfg, req),
            BedrockApiShape::Converse => converse::normalize_request(&self.cfg, req),
        }
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        match self.cfg.api_shape {
            BedrockApiShape::Invoke => invoke::normalize_response(&self.cfg.id, raw),
            BedrockApiShape::Converse => converse::normalize_response(&self.cfg.id, raw),
        }
    }

    fn normalize_chunk(&self, _raw: &str) -> Result<Option<ChatChunk>> {
        // Bedrock streaming uses binary eventstream frames, not text
        // SSE -- the stateless `normalize_chunk` shape doesn't apply.
        // The router never calls this for Bedrock streams; the actual
        // chunk decoding happens inside `stream()` via `eventstream`.
        Ok(None)
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let body = self.normalize_request(&req)?;

        let url = match self.cfg.api_shape {
            BedrockApiShape::Invoke => endpoint::invoke_url(&self.cfg.region, &self.cfg.model_id, false),
            BedrockApiShape::Converse => {
                endpoint::converse_url(&self.cfg.region, &self.cfg.model_id, false)
            }
        };

        let body_str = serde_json::to_vec(&body)
            .map_err(|e| Error::NormalizeRequest(self.cfg.id.clone(), e.to_string()))?;

        let mut request = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body_str)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        for (k, v) in &self.cfg.extra_headers {
            // Defense-in-depth: refuse reserved headers from
            // user-supplied extra_headers. The Bedrock SigV4 path
            // would overwrite Authorization later anyway, but the
            // BearerKey path wouldn't -- so guarding here keeps both
            // paths safe and surfaces the misconfiguration.
            if crate::http_client::is_reserved_extra_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring reserved header from extra_headers (would bypass provider auth)"
                );
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("invalid header name `{k}`: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("invalid header value for `{k}`: {e}")))?;
            request.headers_mut().insert(name, value);
        }

        signing::apply_auth(&mut request, &self.resolved, &self.cfg.region).await?;

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let msg = parse_upstream_error_body(resp).await;
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

    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let body = self.normalize_request(&req)?;

        let url = match self.cfg.api_shape {
            BedrockApiShape::Invoke => endpoint::invoke_url(&self.cfg.region, &self.cfg.model_id, true),
            BedrockApiShape::Converse => {
                endpoint::converse_url(&self.cfg.region, &self.cfg.model_id, true)
            }
        };

        let body_str = serde_json::to_vec(&body)
            .map_err(|e| Error::NormalizeRequest(self.cfg.id.clone(), e.to_string()))?;

        let mut request = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/vnd.amazon.eventstream")
            .body(body_str)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        for (k, v) in &self.cfg.extra_headers {
            // Defense-in-depth: refuse reserved headers from
            // user-supplied extra_headers. The Bedrock SigV4 path
            // would overwrite Authorization later anyway, but the
            // BearerKey path wouldn't -- so guarding here keeps both
            // paths safe and surfaces the misconfiguration.
            if crate::http_client::is_reserved_extra_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring reserved header from extra_headers (would bypass provider auth)"
                );
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("invalid header name `{k}`: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("invalid header value for `{k}`: {e}")))?;
            request.headers_mut().insert(name, value);
        }

        signing::apply_auth(&mut request, &self.resolved, &self.cfg.region).await?;

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let msg = parse_upstream_error_body(resp).await;
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let stream = match self.cfg.api_shape {
            BedrockApiShape::Invoke => eventstream::invoke_stream(provider_id, byte_stream),
            BedrockApiShape::Converse => eventstream::converse_stream(provider_id, byte_stream),
        };
        Ok(stream)
    }
}

/// Best-effort parse of a Bedrock error response body. Tries the common
/// JSON shapes (`/message`, `/Message`, `/error/message`) and falls back
/// to the raw text when the body isn't JSON (gateway 5xx, HTML auth-redirect
/// pages, etc.). Used by both `complete()` and `stream()` so the two paths
/// surface the same error text to callers.
async fn parse_upstream_error_body(resp: reqwest::Response) -> String {
    let body_text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<Value>(&body_text)
        .ok()
        .as_ref()
        .and_then(|v| {
            v.pointer("/message")
                .or_else(|| v.pointer("/Message"))
                .or_else(|| v.pointer("/error/message"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if body_text.is_empty() {
                "upstream error (empty body)".into()
            } else {
                body_text
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_creds_debug_redacts_static_secrets() {
        let creds = BedrockCreds::Static {
            access_key: "AKIAEXAMPLE".into(),
            secret_key: "SECRET-NEVER-SHOW".into(),
            session_token: Some("SESSION-NEVER-SHOW".into()),
        };
        let s = format!("{creds:?}");
        assert!(!s.contains("SECRET-NEVER-SHOW"), "debug leaked secret_key: {s}");
        assert!(!s.contains("SESSION-NEVER-SHOW"), "debug leaked session_token: {s}");
        assert!(s.contains("AKIA"), "expected access-key prefix in debug output: {s}");
        assert!(s.contains("redacted"), "expected redaction marker: {s}");
    }

    #[test]
    fn bedrock_creds_debug_redacts_bearer_key() {
        let creds = BedrockCreds::BearerKey {
            key: "bedrock-api-key-NEVER-SHOW".into(),
        };
        let s = format!("{creds:?}");
        assert!(!s.contains("NEVER-SHOW"), "debug leaked bearer key: {s}");
        assert!(s.contains("redacted"), "expected redaction marker: {s}");
    }

    #[test]
    fn bedrock_config_debug_does_not_leak_via_creds() {
        // BedrockConfig derives Debug. Verify the transitive
        // BedrockCreds Debug redaction holds when BedrockConfig is
        // formatted with `{:?}`.
        let cfg = BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::Static {
                access_key: "AKIAEXAMPLE".into(),
                secret_key: "DEBUG-LEAK-CANARY".into(),
                session_token: None,
            },
            user_agent: None,
            extra_headers: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("DEBUG-LEAK-CANARY"),
            "BedrockConfig debug leaked secret_key: {s}"
        );
    }

    #[test]
    fn bedrock_creds_default_chain_debug_is_safe() {
        let s = format!("{:?}", BedrockCreds::DefaultChain);
        assert_eq!(s, "BedrockCreds::DefaultChain");
    }
}
