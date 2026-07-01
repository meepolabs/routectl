//! Native AWS Bedrock provider.
//!
//! Speaks SigV4 directly to `bedrock-runtime.<region>.amazonaws.com`,
//! supporting both InvokeModel (per-vendor body shape -- Anthropic
//! Messages JSON for Claude, etc.) and Converse (vendor-neutral
//! envelope). Handles streaming via the AWS eventstream binary frame
//! format.
//!
//! ## Reasoning-shape note: static vs. per-request
//!
//! BedrockConfig.adaptive_thinking is a STATIC, build-time flag (egress
//! reads from cfg at request emission). Contrast with the AnthropicApi
//! path where supports_adaptive_thinking lives on ModelEntry and rides
//! per-request via RoutectlInternal. Asymmetry is intentional per the
//! reasoning-translation refactor scope (Bedrock's per-deployment static
//! flag was kept as-is).
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
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, sanitize_for_log,
};

pub mod auth;
pub(crate) mod betas;
pub(crate) mod body_fields;
pub mod converse;
pub mod endpoint;
pub mod eventstream;
pub(crate) mod frame;
pub mod invoke;
pub mod signing;

/// The client-fingerprint body key both Bedrock seams strip. Anthropic's
/// `metadata` block carries client identity (`user_id`, `account_uuid`)
/// that must not reach AWS -- a third-party upstream. Shared by the
/// Invoke seam (`invoke::normalize_request`) and the Converse seam
/// (`converse::extras::insert_provider_extras`) so the two cannot drift
/// on the key name. Stripped only on the CLIENT path; an operator that
/// deliberately sets `metadata` via provider config keeps it.
pub(crate) const CLIENT_FINGERPRINT_METADATA_KEY: &str = "metadata";

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
    /// Provider-level extra HTTP headers (renamed from
    /// `extra_headers` in v0.6.0). The router merges per-model
    /// `header_extras` into this map at dispatch time.
    pub header_extras: Vec<(String, String)>,
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
    /// When `true`, enables the adaptive thinking shape (`thinking:
    /// {type:"adaptive"}` + `output_config.effort`) for Claude Opus 4.7+
    /// inference profiles on this Bedrock provider. This flag is static --
    /// baked at provider construction, takes effect on next server start.
    ///
    /// Note: the per-request equivalent for the AnthropicApi egress is
    /// ModelEntry.supports_adaptive_thinking projected through
    /// RoutectlInternal.supports_adaptive_thinking at dispatch time.
    /// The Bedrock static flag was kept intentionally -- see module doc
    /// for the asymmetry rationale.
    ///
    /// `None` and `Some(false)` both keep the legacy shape.
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

    /// Build, annotate, and SigV4-sign a Bedrock outbound request.
    ///
    /// Shared by `complete()` and `stream()`; the two methods differ
    /// only in the URL suffix and the Accept header they pass in.
    ///
    /// Steps performed:
    /// 1. Build the reqwest::Request from `url`, `accept`, and the
    ///    serialized JSON body.
    /// 2. Explicitly insert `user-agent` into the request headers so
    ///    the dir-2 header trace sees it and SigV4 signs it.
    /// 3. Merge `header_extras` (provider + model, router-composed),
    ///    skipping auth-reserved names (including the `x-amz-` prefix)
    ///    and routectl-managed names.
    /// 4. Apply SigV4 signing (or BearerKey passthrough) via
    ///    `signing::apply`.
    /// 5. Emit the dir-2 outgoing header trace.
    async fn build_signed_request(
        &self,
        body_str: Vec<u8>,
        req: &ChatRequest,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Request> {
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, accept)
            .body(body_str)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        // Explicitly insert user-agent so it appears in the dir-2
        // header trace and is included in the SigV4 signing scope.
        // The client-level UA (set via common_builder) is not visible
        // on request.headers() -- only headers explicitly set on the
        // request itself show up there.
        if let Some(ua) = &self.cfg.user_agent {
            let ua_value = reqwest::header::HeaderValue::from_str(ua)
                .map_err(|e| Error::Config(format!("invalid user-agent value: {e}")))?;
            request
                .headers_mut()
                .insert(reqwest::header::USER_AGENT, ua_value);
        }

        // Prefer the router-composed map (provider + model merged at
        // dispatch) if present; fall back to `self.cfg.header_extras`
        // for library consumers that built the provider directly.
        //
        // Defense-in-depth: `apply_header_extras` refuses auth-reserved
        // headers, which includes the `x-amz-` prefix -- any x-amz-*
        // injected after SigV4 signing would not appear in the signed
        // string, invalidating the signature. The BearerKey path doesn't
        // sign at all, but guarding here keeps both paths safe.
        let header_source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        crate::http_client::apply_header_extras(
            request.headers_mut(),
            &header_source,
            &self.cfg.id,
            &[],
        );

        signing::apply(&mut request, &self.resolved, &self.cfg.region).await?;
        // Dir 2: outgoing request headers. The SigV4 Authorization /
        // x-amz-* (or Bearer) headers were applied to `request` by
        // signing::apply above, so auth IS visible here. The
        // user-agent is also visible here because it was explicitly
        // inserted above. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            request.headers(),
        );

        Ok(request)
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

        let request = self
            .build_signed_request(body_str, &req, &url, "application/json")
            .await?;

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Capture the reset hint from response headers BEFORE
            // `parse_upstream_error_body` moves `resp`, gated on
            // rate-limit statuses.
            let retry_after = if crate::retry_after::is_rate_limit_status(status) {
                crate::retry_after::parse_retry_after(resp.headers())
            } else {
                None
            };
            let msg = parse_upstream_error_body(
                self.cfg.api_shape.provider_kind_str(),
                &self.cfg.id,
                resp,
            )
            .await;
            return Err(Error::upstream_with_retry_after(
                &self.cfg.id,
                status,
                msg,
                retry_after,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE resp.json()
        // consumes the body. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            resp.headers(),
        );
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

        let request = self
            .build_signed_request(body_str, &req, &url, "application/vnd.amazon.eventstream")
            .await?;

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Capture the reset hint from response headers BEFORE
            // `parse_upstream_error_body` moves `resp`, gated on
            // rate-limit statuses.
            let retry_after = if crate::retry_after::is_rate_limit_status(status) {
                crate::retry_after::parse_retry_after(resp.headers())
            } else {
                None
            };
            let msg = parse_upstream_error_body(
                self.cfg.api_shape.provider_kind_str(),
                &self.cfg.id,
                resp,
            )
            .await;
            return Err(Error::upstream_with_retry_after(
                &self.cfg.id,
                status,
                msg,
                retry_after,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the eventstream byte stream below. The stream path had no
        // dir-3 capture before; this closes the gap so it matches
        // complete(). Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            resp.headers(),
        );

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

/// Best-effort parse of a Bedrock error response body into the message
/// routectl surfaces to the CLIENT. Used by both `complete()` and
/// `stream()` so the two paths return the same text.
///
/// Redaction contract: a Bedrock 403 body names the principal ARN,
/// account id, and resource ARN. None of those reach the client -- only
/// the IAM action (the actionable bit) survives. All other statuses are
/// sanitized and capped at `MAX_LOG_BODY_EXCERPT` so an unbounded raw
/// upstream body never reaches the caller.
///
/// Side effect: emits a structured `tracing::warn!` classifying the
/// failure (full action + principal-present flag stays server-side).
async fn parse_upstream_error_body(
    provider_kind: &str,
    provider: &str,
    resp: reqwest::Response,
) -> String {
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    log_bedrock_upstream_error(provider, status, &body_text);
    // Emit the full upstream error body at debug level alongside the
    // status-specific WARN above. The WARN excerpt (cap via
    // sanitize_for_log) keeps `routectl-warn.log` scannable; DEBUG gives
    // operators field-level detail when they flip log level during triage.
    //
    // A 403 body carries the caller's principal ARN, account id, and the
    // resource ARN. Those must not reach the DEBUG log, so for a 403 we
    // substitute an action-only string derived from the existing
    // classifier (the IAM action is the actionable bit; ARNs/account are
    // dropped). Non-403 statuses log the sanitized raw body as before.
    let debug_body = sanitized_debug_body(status, &body_text);
    routectl_core::debug_upstream_error_body(provider_kind, provider, status, &debug_body);

    classify_client_error_message(status, &body_text)
}

/// Build the CLIENT-facing error message from a Bedrock upstream body.
/// Shares the 403-vs-other split with `log_bedrock_upstream_error` via
/// `classify_bedrock_error` so the client path and log path cannot
/// drift on classification.
///
/// - **403** -> generic "bedrock access denied", optionally suffixed
///   with the extracted IAM action. NEVER the principal ARN, account
///   id, or resource ARN.
/// - **other** -> sanitized body excerpt capped at `MAX_LOG_BODY_EXCERPT`.
fn classify_client_error_message(status: u16, body: &str) -> String {
    match classify_bedrock_error(status, body) {
        BedrockErrorClass::AccessDenied { action, .. } => access_denied_message(action),
        BedrockErrorClass::Other => routectl_core::sanitize_upstream_body_with_cap(
            body,
            routectl_core::MAX_LOG_BODY_EXCERPT,
        ),
    }
}

/// Build the access-denied string shared by the client-facing message
/// (`classify_client_error_message`) and the DEBUG body
/// (`sanitized_debug_body`) so the two cannot drift on the 403 arm.
///
/// The action is only surfaced if it matches an IAM `service:Action`
/// shape (`^[A-Za-z0-9._-]+:[A-Za-z0-9*]+$`). A malformed upstream 403
/// could leave ARN/account text as the post-`perform:` token; an ARN
/// (`arn:aws:...` -- multiple colons, digit-only account segments,
/// slashes) fails the shape check and falls back to the generic string
/// so no principal/account/resource identifier leaks via the action.
fn access_denied_message(action: Option<String>) -> String {
    match action.filter(|a| is_iam_action_shape(a)) {
        Some(action) => format!("bedrock access denied: missing IAM action {action}"),
        None => "bedrock access denied".to_string(),
    }
}

/// True if `s` matches an IAM `service:Action` shape: a service segment
/// of `[A-Za-z0-9._-]+`, a single colon, then an action segment of
/// `[A-Za-z0-9*]+`. Pure char scan (no regex dependency, matching
/// `extract_iam_action`'s rationale). An ARN fails because it has more
/// than one colon and the resource segment carries `/`.
fn is_iam_action_shape(s: &str) -> bool {
    let Some((service, action)) = s.split_once(':') else {
        return false;
    };
    !service.is_empty()
        && !action.is_empty()
        && service
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && action
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '*')
}

/// Build the body string emitted to the DEBUG `upstream error body` line.
///
/// A 403 upstream body carries the caller's principal ARN, account id, and
/// the resource ARN. None of that may reach the DEBUG log. For a 403 we
/// return an action-only string built from the shared classifier (the IAM
/// action survives as the actionable bit; ARNs and account id are dropped).
/// Every other status returns the raw body unchanged -- the shared core
/// helper sanitizes and caps it before it is logged.
fn sanitized_debug_body(status: u16, body: &str) -> String {
    match classify_bedrock_error(status, body) {
        BedrockErrorClass::AccessDenied { action, .. } => access_denied_message(action),
        BedrockErrorClass::Other => body.to_string(),
    }
}

/// Shared 403-vs-other classification for a Bedrock upstream error. Both
/// the client-facing message (`classify_client_error_message`) and the
/// structured log line (`log_bedrock_upstream_error`) derive from this
/// single source so the two cannot drift. For a 403 it pre-extracts the
/// IAM action (sanitized) and whether a principal field is present; the
/// log path surfaces both, the client path surfaces only the action.
enum BedrockErrorClass {
    AccessDenied {
        action: Option<String>,
        principal_present: bool,
    },
    Other,
}

fn classify_bedrock_error(status: u16, body: &str) -> BedrockErrorClass {
    if status == 403 {
        // Sanitize the extracted action since it's a substring of an
        // upstream-controlled body. AWS error messages are machine-
        // generated today, but a compromised endpoint could embed
        // control chars; defense-in-depth.
        let action = extract_iam_action(body).map(|s| sanitize_for_log(&s));
        let principal_present = body.contains("User:") || body.contains("Principal:");
        BedrockErrorClass::AccessDenied {
            action,
            principal_present,
        }
    } else {
        BedrockErrorClass::Other
    }
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
/// - **400** -> WARN, "validation error" with body excerpt
/// - **5xx** -> WARN, "upstream 5xx" (transient AWS issues)
/// - other 4xx -> WARN, generic "upstream error"
///
/// Body excerpt is bounded to keep log lines scannable. Never logs
/// credential material because Bedrock error bodies don't contain any.
/// The 403 action / principal-present fields come from the shared
/// `classify_bedrock_error` so the client and log paths stay aligned.
fn log_bedrock_upstream_error(provider: &str, status: u16, body: &str) {
    // sanitize_upstream_body_with_cap trims edges, collapses HTML pages to
    // a short marker, and caps length. It does NOT filter mid-string control
    // characters (\n, \r, ANSI escapes). sanitize_for_log applied next
    // handles that separately.
    let excerpt =
        routectl_core::sanitize_upstream_body_with_cap(body, routectl_core::MAX_LOG_BODY_EXCERPT);
    let safe_excerpt = sanitize_for_log(&excerpt);
    match status {
        401 => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %safe_excerpt,
                "bedrock upstream auth rejected",
            );
        }
        403 => {
            // A 403 always classifies as AccessDenied (see
            // `classify_bedrock_error`), so destructure that variant
            // directly rather than re-matching on a dead Other arm.
            let BedrockErrorClass::AccessDenied {
                action,
                principal_present,
            } = classify_bedrock_error(status, body)
            else {
                unreachable!("classify_bedrock_error returns AccessDenied for status 403")
            };
            tracing::warn!(
                provider = %provider,
                status,
                action = ?action,
                principal_present,
                body_excerpt = %safe_excerpt,
                "bedrock IAM access denied",
            );
        }
        400 => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %safe_excerpt,
                "bedrock validation error",
            );
        }
        s if s >= 500 => {
            tracing::warn!(
                provider = %provider,
                status = s,
                body_excerpt = %safe_excerpt,
                "bedrock upstream 5xx",
            );
        }
        _ => {
            tracing::warn!(
                provider = %provider,
                status,
                body_excerpt = %safe_excerpt,
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
    fn excerpt_sanitizes_crlf_and_ansi() {
        let body = "boom\r\n[fake INFO] injected\x1b[31mred";
        let excerpt = routectl_core::sanitize_upstream_body_with_cap(
            body,
            routectl_core::MAX_LOG_BODY_EXCERPT,
        );
        let safe_excerpt = sanitize_for_log(&excerpt);
        assert!(
            !safe_excerpt.contains('\r'),
            "CR in excerpt: {safe_excerpt:?}"
        );
        assert!(
            !safe_excerpt.contains('\n'),
            "LF in excerpt: {safe_excerpt:?}"
        );
        assert!(
            !safe_excerpt.contains('\x1b'),
            "ESC in excerpt: {safe_excerpt:?}"
        );
    }

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
    fn sanitized_debug_body_403_drops_arn_and_account_keeps_action() {
        // A real 403 body carries principal ARN + account id + resource
        // ARN. The DEBUG body must surface only the IAM action.
        let body = "User: arn:aws:iam::123456789012:user/foo is not authorized to \
                    perform: bedrock-runtime:InvokeModelWithResponseStream on resource: \
                    arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5";
        let got = sanitized_debug_body(403, body);
        assert!(
            !got.contains("arn:aws:"),
            "403 debug body must not contain any ARN, got: {got}"
        );
        assert!(
            !got.contains("123456789012"),
            "403 debug body must not contain the account id, got: {got}"
        );
        assert!(
            got.contains("bedrock-runtime:InvokeModelWithResponseStream"),
            "403 debug body must keep the IAM action, got: {got}"
        );
    }

    #[test]
    fn sanitized_debug_body_non_403_passes_body_through() {
        // Non-403 statuses keep the current behavior: the raw body is
        // returned (the shared core helper sanitizes + caps it on log).
        let body = "validation error: malformed request";
        assert_eq!(sanitized_debug_body(400, body), body);
        assert_eq!(sanitized_debug_body(500, body), body);
    }

    #[test]
    fn sanitized_debug_body_403_malformed_action_falls_back_to_generic() {
        // A malformed 403 body whose post-`perform:` token is an ARN /
        // account fragment (not a `service:Action` shape) must NOT leak
        // that token into the DEBUG body via the supposedly-safe action
        // string. The shape check rejects it and falls back to the
        // generic message.
        let body = "User: x is not authorized to perform: \
                    arn:aws:iam::123456789012:role/x on resource: y";
        let got = sanitized_debug_body(403, body);
        assert!(
            !got.contains("arn:aws:"),
            "malformed-action 403 debug body must not contain any ARN, got: {got}"
        );
        assert!(
            !got.contains("123456789012"),
            "malformed-action 403 debug body must not contain the account id, got: {got}"
        );
        assert_eq!(
            got, "bedrock access denied",
            "malformed action must fall back to the generic message, got: {got}"
        );
    }

    #[test]
    fn is_iam_action_shape_accepts_service_action_rejects_arn() {
        // Well-formed IAM actions pass; anything ARN-shaped (multiple
        // colons, slashes) or empty-segmented fails.
        assert!(is_iam_action_shape("bedrock:InvokeModel"));
        assert!(is_iam_action_shape(
            "bedrock-runtime:InvokeModelWithResponseStream"
        ));
        assert!(is_iam_action_shape("bedrock:*"));
        assert!(!is_iam_action_shape("arn:aws:iam::123456789012:role/x"));
        assert!(!is_iam_action_shape("noColon"));
        assert!(!is_iam_action_shape(":InvokeModel"));
        assert!(!is_iam_action_shape("bedrock:"));
        assert!(!is_iam_action_shape("svc:action/with/slash"));
    }

    #[test]
    fn bedrock_creds_debug_redacts_static_secrets() {
        let creds = BedrockCreds::Static {
            access_key: "testkey-example-xyz".into(),
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
            s.contains("test"),
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
                access_key: "testkey-example-xyz".into(),
                secret_key: "DEBUG-LEAK-CANARY".into(),
                session_token: None,
            },
            user_agent: None,
            header_extras: Vec::new(),
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

    #[test]
    fn client_403_message_carries_action_not_principal_arn() {
        // A real AWS 403 body names the principal ARN, account id, and
        // resource ARN. The client-facing message must surface ONLY the
        // IAM action -- never the principal/account/resource identifiers.
        let body = "User: arn:aws:iam::123456789012:role/AppRole is not \
                    authorized to perform: bedrock-runtime:InvokeModel on \
                    resource: arn:aws:bedrock:us-east-1::foundation-model/\
                    anthropic.claude-haiku-4-5";
        let msg = classify_client_error_message(403, body);
        assert!(
            msg.contains("bedrock-runtime:InvokeModel"),
            "client message should carry the IAM action: {msg}"
        );
        assert!(
            !msg.contains("arn:aws:iam"),
            "client message leaked the principal ARN: {msg}"
        );
        assert!(
            !msg.contains("123456789012"),
            "client message leaked the account id: {msg}"
        );
        assert!(
            !msg.contains("foundation-model"),
            "client message leaked the resource ARN: {msg}"
        );
    }

    #[test]
    fn client_403_without_action_is_generic_no_arn_leak() {
        // A 403 body that doesn't match the `perform: ` template yields
        // a generic message with NO body leak.
        let body = "User: arn:aws:iam::123456789012:role/AppRole denied for \
                    some other reason";
        let msg = classify_client_error_message(403, body);
        assert!(
            !msg.contains("arn:aws:iam"),
            "generic 403 message leaked the principal ARN: {msg}"
        );
        assert!(
            !msg.contains("123456789012"),
            "generic 403 message leaked the account id: {msg}"
        );
        assert_eq!(msg, "bedrock access denied");
    }

    #[test]
    fn client_non_403_message_capped_at_max_excerpt() {
        // A non-403 oversized body is sanitized and capped so an
        // unbounded raw body never reaches the client. The cap helper
        // keeps at most MAX_LOG_BODY_EXCERPT body chars plus a short
        // fixed truncation marker -- bounded regardless of input size.
        const MARKER_LEN: usize = "... [truncated]".len();
        let oversized = routectl_core::MAX_LOG_BODY_EXCERPT * 4;
        let body = "x".repeat(oversized);
        let msg = classify_client_error_message(400, &body);
        assert!(
            msg.len() <= routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN,
            "non-403 client message exceeded the bounded excerpt: {} > {}",
            msg.len(),
            routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN
        );
        assert!(
            msg.len() < oversized,
            "non-403 client message was not truncated: {} (input {})",
            msg.len(),
            oversized
        );
    }
}
