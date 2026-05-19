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

use routectl_core::{
    sanitize_for_log, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
};

pub mod auth;
pub(crate) mod betas;
pub(crate) mod body_fields;
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

impl BedrockApiShape {
    /// Provider-kind string used in tracing fields so operators can
    /// grep `provider_kind=bedrock-invoke` vs `bedrock-converse`
    /// independently. Single source of truth -- both `complete()` and
    /// `stream()` derive their kind via this method instead of
    /// duplicating the match arm.
    pub fn provider_kind_str(self) -> &'static str {
        match self {
            Self::Invoke => "bedrock-invoke",
            Self::Converse => "bedrock-converse",
        }
    }
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
    /// `additionalModelRequestFields.anthropic_beta`. Per-provider
    /// floor that bypasses the `allowed_betas` filter (operator
    /// asserts these by typing them into TOML).
    pub anthropic_beta: Vec<String>,
    /// Bedrock-accepted `anthropic_beta` flags. Sourced from
    /// `[bedrock] allowed_betas` TOML and cloned onto every Bedrock
    /// provider. routectl ships no const default -- AWS schema drift
    /// is operator-tracked. See `examples/bedrock.toml` for the
    /// empirical 2026-05-12 baseline. Empty list = pass-through (no
    /// filter applied), the discovery default.
    pub allowed_betas: Vec<String>,
    /// Bedrock-accepted top-level body fields. On Invoke this filters
    /// the Anthropic-shape body before send; on Converse it filters
    /// the `additionalModelRequestFields` bag. Sourced from
    /// `[bedrock] allowed_body_fields` TOML. Empty list = pass-through
    /// (no filter applied). When non-empty, must include the routectl-
    /// mandatory keys (`messages`, `anthropic_version`, `max_tokens`);
    /// startup validation enforces this.
    pub allowed_body_fields: Vec<String>,
    /// Free-form fields merged into the request body. For `Invoke`,
    /// merged at the top level; for `Converse`, merged into
    /// `additionalModelRequestFields`.
    pub additional_model_request_fields: Option<Value>,
    /// Use the Opus 4.7+ adaptive thinking wire shape on this provider.
    /// Same semantics as `AnthropicApiConfig::adaptive_thinking`. Set
    /// this on Bedrock providers whose `model_id` is an opus-4-7+
    /// inference profile (e.g. `global.anthropic.claude-opus-4-7-v1:0`);
    /// the body normalizer rewrites `thinking: {type:"enabled",
    /// budget_tokens:N}` to `thinking: {type:"adaptive"}` and lifts
    /// effort into top-level `output_config.effort`. `None` and
    /// `Some(false)` both keep the legacy shape.
    pub adaptive_thinking: Option<bool>,
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
        Self {
            cfg,
            resolved,
            client,
        }
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

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), region = %self.cfg.region))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let body = self.normalize_request(&req)?;

        // Trace-level outgoing body for triage. Same gating +
        // sensitivity story as the other two providers -- see
        // `routectl_core::log_safe::trace_outgoing_body`.
        routectl_core::trace_outgoing_body(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &body,
        );
        routectl_core::trace_structural_summary(
            "outgoing",
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &body,
        );

        let url = match self.cfg.api_shape {
            BedrockApiShape::Invoke => {
                endpoint::invoke_url(&self.cfg.region, &self.cfg.model_id, false)
            }
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
            // Defense-in-depth: refuse auth-reserved headers from
            // user-supplied extra_headers. The Bedrock SigV4 path
            // would overwrite Authorization later anyway, but the
            // BearerKey path wouldn't -- so guarding here keeps both
            // paths safe and surfaces the misconfiguration.
            if crate::http_client::is_auth_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring auth-reserved header from extra_headers (would bypass provider auth)"
                );
                continue;
            }
            if crate::http_client::is_managed_header(k) {
                tracing::debug!(
                    provider = %self.cfg.id,
                    header = %k,
                    "dropping managed header from extra_headers; composed dynamically by routectl"
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
            let msg = parse_upstream_error_body(
                self.cfg.api_shape.provider_kind_str(),
                &self.cfg.id,
                resp,
            )
            .await;
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        // Trace upstream success body pre-normalize. Distinct
        // provider_kind per shape so operators can grep
        // `provider_kind=bedrock-invoke` vs `bedrock-converse`.
        routectl_core::trace_upstream_success_body(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &raw_body,
        );
        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), region = %self.cfg.region))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let body = self.normalize_request(&req)?;

        routectl_core::trace_outgoing_body(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &body,
        );
        routectl_core::trace_structural_summary(
            "outgoing",
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &body,
        );

        let url = match self.cfg.api_shape {
            BedrockApiShape::Invoke => {
                endpoint::invoke_url(&self.cfg.region, &self.cfg.model_id, true)
            }
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
            .header(
                reqwest::header::ACCEPT,
                "application/vnd.amazon.eventstream",
            )
            .body(body_str)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        for (k, v) in &self.cfg.extra_headers {
            // Defense-in-depth: refuse auth-reserved headers from
            // user-supplied extra_headers. The Bedrock SigV4 path
            // would overwrite Authorization later anyway, but the
            // BearerKey path wouldn't -- so guarding here keeps both
            // paths safe and surfaces the misconfiguration.
            if crate::http_client::is_auth_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring auth-reserved header from extra_headers (would bypass provider auth)"
                );
                continue;
            }
            if crate::http_client::is_managed_header(k) {
                tracing::debug!(
                    provider = %self.cfg.id,
                    header = %k,
                    "dropping managed header from extra_headers; composed dynamically by routectl"
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
            let msg = parse_upstream_error_body(
                self.cfg.api_shape.provider_kind_str(),
                &self.cfg.id,
                resp,
            )
            .await;
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let stream = match self.cfg.api_shape {
            BedrockApiShape::Invoke => eventstream::invoke_stream(provider_id.clone(), byte_stream),
            BedrockApiShape::Converse => {
                eventstream::converse_stream(provider_id.clone(), byte_stream)
            }
        };
        Ok(routectl_core::wrap_stream_with_summary(
            stream,
            "upstream",
            self.cfg.api_shape.provider_kind_str(),
            provider_id,
        ))
    }
}

/// Best-effort parse of a Bedrock error response body. Tries the common
/// JSON shapes (`/message`, `/Message`, `/error/message`) and falls back
/// to the raw text when the body isn't JSON (gateway 5xx, HTML auth-redirect
/// pages, etc.). Used by both `complete()` and `stream()` so the two paths
/// surface the same error text to callers.
///
/// Side effect: emits a structured `tracing::warn!` (or `error!` for 5xx)
/// classifying the failure. 401 -> "auth rejected". 403 -> attempts to
/// extract the IAM action from the AWS-formatted "User: ... is not
/// authorized to perform: <action>" body and surfaces it. 400 ->
/// "validation error" with body excerpt. 5xx -> "upstream 5xx".
async fn parse_upstream_error_body(
    provider_kind: &str,
    provider: &str,
    resp: reqwest::Response,
) -> String {
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    log_bedrock_upstream_error(provider, status, &body_text);
    // Emit the full upstream error body at debug level alongside the
    // status-specific WARN above. The WARN excerpt (200B) keeps
    // `routectl-warn.log` scannable; DEBUG gives operators field-level
    // detail when they flip log level during triage.
    routectl_core::debug_upstream_error_body(provider_kind, provider, status, &body_text);

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

/// Classify a Bedrock upstream error and emit a structured log line.
///
/// Status-based dispatch:
/// - **401** -> WARN, "auth rejected" (bearer key invalid / SigV4
///   creds expired)
/// - **403** -> WARN with the IAM action extracted from the AWS error
///   body (`User: ... is not authorized to perform: <action>`) and
///   `principal_present` flag. The action name is the actionable bit:
///   if `bedrock-runtime:InvokeModelWithResponseStream` shows up here,
///   the user knows their IAM policy allows InvokeModel but not the
///   streaming variant -- a common gotcha.
/// - **400** -> WARN, "validation error" with body excerpt capped at
///   256 chars
/// - **5xx** -> WARN, "upstream 5xx" (transient AWS issues)
/// - other 4xx -> WARN, generic "upstream error"
///
/// Body excerpt is bounded to keep log lines scannable. Never logs
/// credential material because Bedrock error bodies don't contain any.
fn log_bedrock_upstream_error(provider: &str, status: u16, body: &str) {
    // Sanitize: a Bedrock upstream error body that contains \n, \r, or
    // ANSI escape sequences (a CDN-injected error page, a compromised
    // upstream, a mocked test fixture) would otherwise forge fake log
    // lines or scramble operator terminals. Mirror the openai_compat
    // and anthropic_api providers which both run upstream excerpts
    // through `sanitize_upstream_body_with_cap` before logging.
    let excerpt =
        routectl_core::sanitize_upstream_body_with_cap(body, routectl_core::MAX_LOG_BODY_EXCERPT);
    match status {
        401 => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %excerpt,
                "bedrock upstream auth rejected",
            );
        }
        403 => {
            // Sanitize the extracted action since it's a substring of
            // an upstream-controlled body. AWS error messages are
            // machine-generated today, but a compromised endpoint
            // could embed control chars; defense-in-depth.
            let action = extract_iam_action(body).map(|s| sanitize_for_log(&s));
            let principal_present = body.contains("User:") || body.contains("Principal:");
            tracing::warn!(
                provider = %provider,
                status,
                action = ?action,
                principal_present,
                body_excerpt = %excerpt,
                "bedrock IAM access denied",
            );
        }
        400 => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %excerpt,
                "bedrock validation error",
            );
        }
        s if s >= 500 => {
            tracing::warn!(
                provider = %provider,
                status = s,
                body_excerpt = %excerpt,
                "bedrock upstream 5xx",
            );
        }
        _ => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %excerpt,
                "bedrock upstream error",
            );
        }
    }
}

/// Extract the IAM action name from an AWS error message of the form
/// `User: arn:... is not authorized to perform: bedrock-runtime:InvokeModel
/// on resource: ...`. Returns the substring between "perform: " and the
/// next whitespace. Returns None if the body doesn't match this shape
/// or the action segment is empty.
///
/// **First-match semantics**: we extract the FIRST occurrence of
/// `perform: ` in the body. AWS's error template has historically been
/// stable (the `perform: <action>` seam has held for 10+ years across
/// IAM error messages and is treated as semi-public API for IAM
/// debugging tools). If the template ever changes to embed a second
/// `perform: ` substring (e.g. inside a resource ARN), this extractor
/// would return the FIRST occurrence -- which would still be the
/// correct action for current AWS bodies but could be wrong if the
/// embedded substring appears BEFORE the canonical one. This is a
/// best-effort log field, not a contract; if the template breaks the
/// extractor returns either a stale action or `None` and the
/// surrounding 256-char `body_excerpt` log field still surfaces the
/// raw error text.
///
/// Pure string search rather than `regex` so the bedrock feature does
/// not pull regex into the binary just for this one log call.
fn extract_iam_action(body: &str) -> Option<String> {
    const NEEDLE: &str = "perform: ";
    let start = body.find(NEEDLE)? + NEEDLE.len();
    let rest = &body[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_iam_action_pulls_action_from_aws_403_body() {
        // Real-world AWS error body shape. The `perform: ` substring
        // is the stable seam.
        let body = "User: arn:aws:iam::123456789012:user/foo is not authorized to \
                    perform: bedrock-runtime:InvokeModelWithResponseStream on resource: \
                    arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5";
        assert_eq!(
            extract_iam_action(body),
            Some("bedrock-runtime:InvokeModelWithResponseStream".to_string())
        );
    }

    #[test]
    fn extract_iam_action_returns_none_for_unrelated_body() {
        assert_eq!(extract_iam_action(""), None);
        assert_eq!(extract_iam_action("Some other validation error"), None);
        // Edge case: "perform: " followed by EOF or whitespace -> None.
        assert_eq!(extract_iam_action("perform: "), None);
    }

    #[test]
    fn extract_iam_action_first_match_when_pattern_appears_twice() {
        // First-match semantics: if the AWS template ever embeds a
        // second `perform: ` (e.g. in a resource ARN), we return the
        // FIRST occurrence. Pin this so the contract is explicit.
        let body = "perform: bedrock:InvokeModel on resource: \
                    arn:aws:fake:perform: bedrock:OtherAction";
        assert_eq!(
            extract_iam_action(body),
            Some("bedrock:InvokeModel".to_string())
        );
    }

    #[test]
    fn bedrock_creds_debug_redacts_static_secrets() {
        let creds = BedrockCreds::Static {
            access_key: "AKIAEXAMPLE".into(),
            secret_key: "SECRET-NEVER-SHOW".into(),
            session_token: Some("SESSION-NEVER-SHOW".into()),
        };
        let s = format!("{creds:?}");
        assert!(
            !s.contains("SECRET-NEVER-SHOW"),
            "debug leaked secret_key: {s}"
        );
        assert!(
            !s.contains("SESSION-NEVER-SHOW"),
            "debug leaked session_token: {s}"
        );
        assert!(
            s.contains("AKIA"),
            "expected access-key prefix in debug output: {s}"
        );
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
            allowed_betas: Vec::new(),
            allowed_body_fields: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
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
