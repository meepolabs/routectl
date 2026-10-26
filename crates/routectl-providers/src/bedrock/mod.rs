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
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, UpstreamBody,
    sanitize_for_log,
};

use crate::upstream_error::UpstreamErrorRead;

use crate::aws_error::{
    AccessDeniedDetail, BedrockErrorClass, access_denied_message, classify_bedrock_error,
    classify_parsed_client_error_message, sanitized_debug_body,
};

pub mod auth;
pub(crate) mod betas;
pub(crate) mod body_fields;
pub mod converse;
pub(crate) mod count_tokens;
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
/// `Invoke` sends the vendor-native `InvokeModel` body. Today that adapter
/// builds the Anthropic Messages JSON shape only (patched with Bedrock's
/// body-side `anthropic_version`); it shapes no other vendor's body, so
/// non-Anthropic models belong on `Converse`.
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
    /// The per-vendor `InvokeModel` API with a vendor-native body.
    #[default]
    Invoke,
    /// The unified `Converse` API with a vendor-agnostic envelope.
    Converse,
}

impl BedrockApiShape {
    /// Provider-kind string used in tracing fields so operators can
    /// grep `provider_kind=bedrock-invoke` vs `bedrock-converse`
    /// independently. Single source of truth -- both `complete()` and
    /// `stream()` derive their kind via this method instead of
    /// duplicating the match arm.
    pub const fn provider_kind_str(self) -> &'static str {
        match self {
            Self::Invoke => invoke::PROVIDER_KIND,
            Self::Converse => converse::PROVIDER_KIND,
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
    /// Short-term Bedrock API key sent as a bearer credential.
    BearerKey {
        /// The short-term API key.
        key: String,
    },
    /// Raw AWS access key, secret key, and optional session token.
    Static {
        /// AWS access key id.
        access_key: String,
        /// AWS secret access key.
        secret_key: String,
        /// Optional session token for temporary credentials.
        session_token: Option<String>,
    },
    /// A named profile in `~/.aws/credentials`.
    Profile {
        /// Profile name to resolve.
        name: String,
    },
    /// AWS's standard credential provider chain.
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
    /// Provider identifier used in tracing and log fields.
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
    /// Selects the `Invoke` or `Converse` wire shape.
    pub api_shape: BedrockApiShape,
    /// Resolved AWS credentials used to authenticate requests.
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
    /// floor that is never withheld (operator asserts these by typing
    /// them into TOML).
    pub anthropic_beta: Vec<String>,
    /// Free-form fields merged into the request body. For `Invoke`,
    /// merged at the top level; for `Converse`, merged into
    /// `additionalModelRequestFields`.
    pub additional_model_request_fields: Option<Value>,
}

/// Bedrock egress provider (Invoke or Converse shape).
pub struct BedrockProvider {
    cfg: BedrockConfig,
    resolved: auth::ResolvedCreds,
    client: reqwest::Client,
    /// `bedrock-runtime` origin derived from the admitted region; every
    /// request URL is this plus an [`endpoint`] path.
    runtime_origin: String,
}

impl BedrockProvider {
    /// Construct a new BedrockProvider from a (declarative) config + a
    /// (resolved) credential handle. The caller is responsible for
    /// running `auth::resolve` to produce the second arg; this is
    /// typically done once in the router's factory.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `cfg.region` is not a canonical AWS region,
    /// before any client is built.
    pub fn new(cfg: BedrockConfig, resolved: auth::ResolvedCreds) -> Result<Self> {
        // A signed request must never be auto-followed across a 3xx:
        // the SigV4 envelope (`x-amz-*`, including the security token)
        // is not in reqwest's default cross-host strip list, and
        // replaying the signature against a different host would fail
        // anyway. A redirect here is an upstream fault to surface, not
        // to chase.
        let base_url = endpoint::bedrock_runtime_url(&cfg.region)?;
        let client = crate::http_client::build_no_redirect(cfg.user_agent.as_deref(), &base_url)
            .expect("reqwest no-redirect client build failed (TLS init?); fatal at startup");
        Ok(Self {
            cfg,
            resolved,
            client,
            runtime_origin: base_url,
        })
    }

    /// Aim every request at `origin` instead of the region-derived
    /// `bedrock-runtime` host, with a fresh no-redirect client for it, so a
    /// cross-crate test can put a mock upstream behind a real provider.
    ///
    /// # Panics
    ///
    /// When the HTTP client cannot be built, as [`Self::new`] does.
    #[cfg(all(feature = "bedrock", feature = "test-utils"))]
    pub fn with_runtime_origin_for_tests(mut self, origin: &str) -> Self {
        self.client = crate::http_client::build_no_redirect(self.cfg.user_agent.as_deref(), origin)
            .expect("reqwest no-redirect client build failed for the test origin");
        self.runtime_origin = origin.to_string();
        self
    }

    /// One non-streaming InvokeModel / Converse round trip sending `body`,
    /// the normalized form of `req`.
    async fn complete_once(&self, req: &ChatRequest, body: Value) -> Result<ChatResponse> {
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

        let url = self.inference_url(false);

        let body_str = bytes::Bytes::from(serde_json::to_vec(&body).map_err(|e| {
            Error::Internal(format!(
                "provider `{}`: serialize request body: {e}",
                self.cfg.id
            ))
        })?);
        let tap = crate::anthropic_api::wire_edits::tap_of(req);
        let sent_copy = tap.map(|_| body_str.clone());

        let request = self
            .build_signed_request(body_str, req, &url, "application/json")
            .await?;
        if let (Some(tap), Some(sent)) = (tap, sent_copy) {
            tap.deposit(sent);
        }

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
        if status >= 400 {
            return Err(self.map_error_response(resp).await);
        }

        // Dir 3: upstream response headers, read BEFORE the body read
        // consumes `resp`. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            resp.headers(),
        );
        let content_length = resp.content_length();
        let (body_bytes, hit_cap) =
            crate::http_client::read_body_capped(resp, crate::http_client::MAX_RESPONSE_BODY_BYTES)
                .await
                .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        if hit_cap {
            crate::http_client::warn_body_cap(
                &self.cfg.id,
                status,
                content_length,
                "complete_success_body",
            );
        }
        let body_bytes = crate::wire_declare::deposit_response(req, body_bytes);
        let raw_body: Value =
            crate::upstream_error::map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
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

    /// One streaming round trip sending `body`, the normalized form of `req`.
    /// An upstream error status returns `Err` before any stream is built, so
    /// a caller sees either a rejection or a stream, never bytes followed by
    /// an error status.
    async fn stream_once(
        &self,
        req: &ChatRequest,
        body: Value,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
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

        let url = self.inference_url(true);

        let body_str = bytes::Bytes::from(serde_json::to_vec(&body).map_err(|e| {
            Error::Internal(format!(
                "provider `{}`: serialize request body: {e}",
                self.cfg.id
            ))
        })?);
        let tap = crate::anthropic_api::wire_edits::tap_of(req);
        let sent_copy = tap.map(|_| body_str.clone());

        let request = self
            .build_signed_request(body_str, req, &url, "application/vnd.amazon.eventstream")
            .await?;
        if let (Some(tap), Some(sent)) = (tap, sent_copy) {
            tap.deposit(sent);
        }

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
        if status >= 400 {
            return Err(self.map_error_response(resp).await);
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
        let endpoint = resp.url().clone();
        let stream = eventstream::response_stream(
            self.cfg.api_shape,
            provider_id.clone(),
            &self.cfg.region,
            &endpoint,
            resp.bytes_stream(),
            crate::wire_declare::response_tap(req),
        );
        Ok(routectl_core::wrap_stream_with_summary(
            stream,
            "upstream",
            self.cfg.api_shape.provider_kind_str(),
            provider_id,
        ))
    }

    /// One CountTokens round trip for `normalized`, the normalized form of
    /// `req`.
    async fn count_tokens_once(&self, req: &ChatRequest, normalized: Value) -> Result<TokenCount> {
        let body = match self.cfg.api_shape {
            BedrockApiShape::Invoke => count_tokens::invoke_tokens_body(&self.cfg.id, &normalized)?,
            BedrockApiShape::Converse => {
                count_tokens::converse_tokens_body(&self.cfg.id, &normalized)?
            }
        };

        routectl_core::trace_outgoing_body(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &body,
        );

        let url = format!(
            "{}{}",
            self.runtime_origin,
            endpoint::count_tokens_path(&self.cfg.model_id)
        );
        let body_str = bytes::Bytes::from(serde_json::to_vec(&body).map_err(|e| {
            Error::Internal(format!(
                "provider `{}`: serialize request body: {e}",
                self.cfg.id
            ))
        })?);
        let request = self
            .build_signed_request(body_str, req, &url, "application/json")
            .await?;

        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
        if status >= 400 {
            return Err(self.map_error_response(resp).await);
        }

        crate::header_trace::upstream(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            resp.headers(),
        );
        let content_length = resp.content_length();
        let (body_bytes, hit_cap) =
            crate::http_client::read_body_capped(resp, crate::http_client::MAX_RESPONSE_BODY_BYTES)
                .await
                .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        if hit_cap {
            crate::http_client::warn_body_cap(
                &self.cfg.id,
                status,
                content_length,
                "count_tokens_success_body",
            );
        }
        let raw_body: Value =
            crate::upstream_error::map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
        routectl_core::trace_upstream_success_body(
            self.cfg.api_shape.provider_kind_str(),
            &self.cfg.id,
            &raw_body,
        );
        count_tokens::parse_token_count(&self.cfg.id, &raw_body)
    }

    /// Read a `>= 400` response through the shared upstream-error read and
    /// map it to the client-facing error. One chokepoint for complete,
    /// stream, and count_tokens so the three paths log, classify, and lift
    /// identically.
    async fn map_error_response(&self, resp: reqwest::Response) -> Error {
        let read = UpstreamErrorRead::read(&self.cfg.id, resp)
            .await
            .aws_backed(true);
        map_bedrock_upstream_error(self.cfg.api_shape.provider_kind_str(), &self.cfg.id, read)
    }

    /// The InvokeModel or Converse URL for this lane's shape and model.
    fn inference_url(&self, streaming: bool) -> String {
        let path = match self.cfg.api_shape {
            BedrockApiShape::Invoke => endpoint::invoke_path(&self.cfg.model_id, streaming),
            BedrockApiShape::Converse => endpoint::converse_path(&self.cfg.model_id, streaming),
        };
        format!("{}{path}", self.runtime_origin)
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
        body_str: bytes::Bytes,
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
        self.complete_once(&req, body).await
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), region = %self.cfg.region))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let body = self.normalize_request(&req)?;
        self.stream_once(&req, body).await
    }

    /// `POST /model/{modelId}/count-tokens` -- the token count for a
    /// request without running inference.
    ///
    /// Wire reference:
    /// <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_CountTokens.html>
    ///
    /// Body assembly reuses `normalize_request`, so the counted body is the
    /// one that would actually ship, then wraps it per lane: the Invoke
    /// body rides verbatim (base64) in the `invokeModel` union member,
    /// while the Converse body is copied through the allowlist AWS's
    /// `converse` member accepts. Signing, error-body reading, header
    /// tracing, and error propagation are the exact path `complete()`
    /// uses -- no status on this lane is reinterpreted.
    ///
    /// No status is treated as a capability signal, and both halves of
    /// that are measured against live bedrock-runtime rather than assumed:
    ///
    /// - **404** means the model RESOURCE was not found -- observed only
    ///   as an end-of-life model id (`This model version has reached the
    ///   end of its life.`), and returned by a region that served other
    ///   models in the same session. It is not an operation-availability
    ///   signal, so lifting it to a capability error would step a
    ///   fallback chain past a fully capable region and discard AWS's own
    ///   actionable message. No region genuinely lacking the operation
    ///   has ever been observed: reachable regions answer 4xx (the
    ///   operation is present) and unreachable ones fail at auth with
    ///   403.
    /// - **400** is overloaded across three distinct measured causes --
    ///   invalid model id (`The provided model identifier is invalid.`),
    ///   a valid model that cannot count (`The provided model doesn't
    ///   support counting tokens.`), and a malformed request body
    ///   (SerializationException, `NUMBER_VALUE cannot be converted to
    ///   String`). Only the middle one is a capability fact, and nothing
    ///   in the status distinguishes it, so treating 400 as capability
    ///   would hide a body-assembly defect behind a silent walk-past.
    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), region = %self.cfg.region))]
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let body = self.normalize_request(&req)?;
        self.count_tokens_once(&req, body).await
    }

    /// Free reachability probe: resolve the AWS credential chain, no
    /// model invocation. A resolvable chain (Bearer key present, static
    /// keys, or a Profile / DefaultChain that answers) is `Reachable`;
    /// a resolution failure maps via `probe_outcome_for_resolve_error`.
    /// Never signs or sends a Bedrock request, so it can never bill.
    ///
    /// Bounded by the shared `PROBE_TIMEOUT`: Profile / DefaultChain
    /// resolution can do external work (SSO, instance-metadata), so an
    /// unbounded await could hang `doctor`.
    async fn probe(&self) -> routectl_core::ProbeOutcome {
        probe_bounded_resolve(
            crate::probe::PROBE_TIMEOUT,
            auth::resolve(&self.cfg.creds, &self.cfg.region),
        )
        .await
    }
}

/// Bound a credential-resolution future by `timeout` and map it to a
/// probe outcome. Generic over the future so the timeout bound is
/// unit-testable with a deliberately-slow resolver. A timeout collapses
/// to `Unreachable`; a completed resolution defers to
/// [`probe_outcome_for_resolve_error`] on error.
async fn probe_bounded_resolve<F>(
    timeout: std::time::Duration,
    resolve: F,
) -> routectl_core::ProbeOutcome
where
    F: std::future::Future<Output = Result<auth::ResolvedCreds>>,
{
    match tokio::time::timeout(timeout, resolve).await {
        Ok(Ok(_)) => routectl_core::ProbeOutcome::Reachable,
        Ok(Err(e)) => probe_outcome_for_resolve_error(&e),
        Err(_) => {
            routectl_core::ProbeOutcome::Unreachable("credential resolution timed out".into())
        }
    }
}

/// Map a `bedrock::auth::resolve` failure to a probe outcome. A credential
/// error (`Error::Auth` or `Error::Credential`) is an auth problem; any other
/// error is treated as the chain being unreachable. Reason strings are fixed literals so no
/// profile name, ARN, or SDK detail leaks into an operator-facing
/// message.
fn probe_outcome_for_resolve_error(err: &Error) -> routectl_core::ProbeOutcome {
    match err {
        Error::Auth(_) | Error::Credential { .. } => {
            routectl_core::ProbeOutcome::AuthFailed("credential resolution failed".into())
        }
        _ => routectl_core::ProbeOutcome::Unreachable("credential chain unavailable".into()),
    }
}

/// Map a Bedrock `>= 400` response, already read, to the client-facing
/// `Error::Upstream`, emitting the lane's WARN and DEBUG lines.
///
/// Tokens follow the shared precedence rule ([`UpstreamErrorRead::tokens`]):
/// the native lane's 400 body is the flat minimal `{"message":...}` shape with
/// no `__type`, so the `x-amzn-errortype` header is the only in-band source of
/// the `ValidationException` token the capability matcher gates on. Those
/// TYPE/CODE tokens ride on every status class (they name the exception, not
/// a principal); the body/message redaction split by status happens in
/// [`map_error_message`].
///
/// Redaction contract: a Bedrock 403 body names the principal ARN, account
/// id, and resource ARN. None of those reach the client: only the IAM
/// action (the actionable bit) survives. All other statuses are sanitized
/// and capped so an unbounded raw upstream body never reaches the caller.
/// The error carries the parse of the message it carries: the read's own
/// parse when the message is the body verbatim, else a parse of the message.
fn map_bedrock_upstream_error(
    provider_kind: &str,
    provider: &str,
    read: UpstreamErrorRead,
) -> Error {
    log_error_read(provider_kind, provider, &read);
    let message = map_error_message(&read);
    let carried = read.aws_carried(message);
    let retry_after = read.retry_after();
    read.into_error(provider, carried, retry_after)
}

/// Emit the WARN and DEBUG lines for a Bedrock error read.
fn log_error_read(provider_kind: &str, provider: &str, read: &UpstreamErrorRead) {
    let status = read.status();
    // On a request-fault status (400/422) the type token is what the
    // capability matcher gates its learn path on, so an unusable header
    // there (when the body supplied no type) surfaces a bounded,
    // reason-labeled WARN: a stripped or garbled header stays visible
    // instead of a silent non-attributed rejection. Other statuses are not
    // gated on it and would only add noise. The raw header value is never
    // logged.
    if is_request_fault_status(status)
        && let Some(reason) = read.unusable_error_type_header_reason()
    {
        crate::aws_error::incr_error_type_header_unusable(reason);
        tracing::warn!(
            provider = %provider,
            status,
            reason,
            "bedrock upstream error-type header unusable",
        );
    }
    let detail = read.access_denied_detail();
    log_bedrock_upstream_error(provider, status, read.body(), read.hit_cap(), &detail);
    // The full (capped) upstream error body at debug level, beside the
    // status-specific WARN above. A 403 body carries the caller's principal
    // ARN, account id, and the resource ARN, so for a 403 an action-only
    // string derived from the classifier is logged instead.
    let debug_body = sanitized_debug_body(status, read.text(), &detail);
    routectl_core::debug_upstream_error_body(provider_kind, provider, status, &debug_body);
}

/// Bedrock upstream statuses whose flat AWS envelope is a request fault the
/// capability matcher reads to attribute a rejected capability. These
/// carry a `ValidationException` (never a 403 `AccessDenied`), so the body
/// names a rejected request field, not a principal ARN / account id /
/// resource ARN -- safe to carry raw (sanitized, capped at the matcher's
/// shared ceiling) rather than collapsed to a short log excerpt.
const fn is_request_fault_status(status: u16) -> bool {
    matches!(status, 400 | 422)
}

/// Build the CLIENT-facing message from an error body prefix.
///
/// When the body was truncated at the cap (`hit_cap`), the raw prefix is
/// untrustworthy and must never be echoed. Only the classifier-derived IAM
/// action survives (a 403 whose action passes the strict shape check);
/// every other truncated body collapses to the fixed cap-exceeded message.
/// An intact body classifies exactly as before.
///
/// A request-fault (400/422) envelope is carried raw (sanitized, byte-capped
/// at the shared [`routectl_core::MAX_ERROR_BODY_BYTES`] ceiling) rather than
/// the short log excerpt -- but ONLY when it is the flat AWS envelope shape
/// the capability matcher re-parses (a top-level `__type` and/or `message`,
/// via [`crate::aws_error::is_carryable_flat_envelope`]). The matcher reads
/// the flat AWS `ValidationException` JSON, and the 512-char log excerpt
/// truncates a verbose envelope into invalid JSON that silently defeats the
/// match. Any OTHER 400/422 shape (a nested `{"error":{"message":...}}` from
/// a proxy or custom endpoint, an HTML page, plain text) keeps the prior
/// 512-char sanitized excerpt, so the widened cap cannot newly reflect an
/// arbitrary large or nested-shaped body up to the caller via the ingress
/// `error.message` reader. The log-rendering path
/// ([`log_bedrock_upstream_error`]) keeps its own 512-char excerpt
/// independently, so the operator log surface is unchanged. A 403 stays
/// scrubbed on the `Other`/`AccessDenied` split below.
fn map_error_message(read: &UpstreamErrorRead) -> String {
    let (status, prefix, hit_cap) = (read.status(), read.body(), read.hit_cap());
    let detail = read.access_denied_detail();
    if hit_cap {
        match classify_bedrock_error(status, prefix.text()) {
            BedrockErrorClass::AccessDenied { action, .. } => {
                access_denied_message(&detail, action)
            }
            BedrockErrorClass::Other => crate::http_client::body_cap_exceeded_message(),
        }
    } else if is_request_fault_status(status)
        && crate::aws_error::is_carryable_flat_envelope(prefix.parsed())
    {
        routectl_core::sanitize_upstream_body_with_byte_cap(
            &crate::aws_error::redacted_body_text(prefix),
            routectl_core::MAX_ERROR_BODY_BYTES,
        )
    } else {
        classify_parsed_client_error_message(status, prefix, &detail)
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
///
/// On a cap trip (`hit_cap`) the `body` is a truncated prefix: the
/// `body_excerpt` field collapses to the fixed cap message so no prefix
/// bytes reach WARN level. The 403 IAM action (classifier-derived,
/// shape-checked) still lifts -- it is a bounded token, not free-form
/// body text.
fn log_bedrock_upstream_error(
    provider: &str,
    status: u16,
    body: &UpstreamBody,
    hit_cap: bool,
    detail: &AccessDeniedDetail,
) {
    // sanitize_upstream_body_with_cap trims edges, collapses HTML pages to
    // a short marker, and caps length. It does NOT filter mid-string control
    // characters (\n, \r, ANSI escapes). sanitize_for_log applied next
    // handles that separately. A 403 body names the principal ARN, account
    // id and resource ARN, which prompt redaction does not touch, so its
    // excerpt is the same access-denied string the DEBUG line carries.
    let safe_excerpt = if status == 403 {
        sanitize_for_log(&sanitized_debug_body(status, body.text(), detail))
    } else if hit_cap {
        crate::http_client::body_cap_exceeded_message()
    } else {
        let excerpt = routectl_core::sanitize_upstream_body_with_cap(
            &crate::aws_error::redacted_body_text(body),
            routectl_core::MAX_LOG_BODY_EXCERPT,
        );
        sanitize_for_log(&excerpt)
    };
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
            } = classify_bedrock_error(status, body.text())
            else {
                unreachable!("classify_bedrock_error returns AccessDenied for status 403")
            };
            tracing::warn!(
                provider = %provider,
                status,
                action = ?action.filter(|a| crate::aws_error::is_iam_action_shape(a)),
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

#[cfg(test)]
#[path = "redirect_tests.rs"]
mod redirect_tests;

// Each send site deposits exactly the bytes the upstream received.
#[cfg(test)]
#[path = "wire_tap_tests.rs"]
mod wire_tap_tests;

#[cfg(test)]
#[path = "error_parse_tests.rs"]
mod error_parse_tests;

#[cfg(test)]
#[path = "beta_withhold_wire_tests.rs"]
mod beta_withhold_wire_tests;

// The schema repair walk's limits refuse the request before any send.
#[cfg(test)]
#[path = "output_schema_limit_wire_tests.rs"]
mod output_schema_limit_wire_tests;

#[cfg(test)]
#[path = "error_read_tests.rs"]
mod error_read_tests;

#[cfg(test)]
#[path = "access_denied_tests.rs"]
mod access_denied_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws_error::classify_client_error_message;

    /// Map a text body through the lane's error mapper, as `complete()`
    /// does after the shared read; `headers` are `(name, value)` pairs.
    fn map_with_headers(
        provider_id: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
        hit_cap: bool,
    ) -> Error {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            map.append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let read = UpstreamErrorRead::from_parts(status, &map, body, hit_cap);
        map_bedrock_upstream_error("bedrock-invoke", provider_id, read)
    }

    fn map_text(provider_id: &str, status: u16, body: &str, hit_cap: bool) -> Error {
        map_with_headers(provider_id, status, &[], body, hit_cap)
    }

    fn log_bedrock_upstream_error(provider: &str, status: u16, body: &str, hit_cap: bool) {
        super::log_bedrock_upstream_error(
            provider,
            status,
            &UpstreamBody::new(body),
            hit_cap,
            &AccessDeniedDetail::default(),
        );
    }

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

    fn probe_cfg(creds: BedrockCreds) -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:probe".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds,
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        }
    }

    /// A Bearer credential resolves without a network call and probes
    /// as Reachable -- and the probe issues no model invocation (there is
    /// no HTTP client interaction at all on this path).
    #[tokio::test]
    async fn probe_bearer_creds_reachable_without_model_call() {
        let creds = BedrockCreds::BearerKey {
            key: "test-bearer-key".into(),
        };
        let resolved = auth::resolve(&creds, "us-west-2").await.expect("resolve");
        let provider = BedrockProvider::new(probe_cfg(creds), resolved).expect("canonical region");
        assert_eq!(
            provider.probe().await,
            routectl_core::ProbeOutcome::Reachable
        );
    }

    /// Static AWS keys resolve to a SigV4 provider with no network hop
    /// and probe as Reachable.
    #[tokio::test]
    async fn probe_static_creds_reachable() {
        let creds = BedrockCreds::Static {
            access_key: "testkey-access-xyz".into(),
            secret_key: "testkey-secret-xyz".into(),
            session_token: None,
        };
        let resolved = auth::resolve(&creds, "us-west-2").await.expect("resolve");
        let provider = BedrockProvider::new(probe_cfg(creds), resolved).expect("canonical region");
        assert_eq!(
            provider.probe().await,
            routectl_core::ProbeOutcome::Reachable
        );
    }

    /// The resolve-error mapping: a credential error (`Error::Auth` or
    /// `Error::Credential`) is `AuthFailed`; any other error is
    /// `Unreachable`. Reason strings carry no profile name, ARN, or SDK
    /// detail.
    #[test]
    fn probe_outcome_maps_resolve_errors() {
        let rows = [
            (
                "auth",
                Error::Auth("bedrock: failed to load AWS profile `x`: boom".into()),
                true,
            ),
            (
                "credential",
                Error::Credential {
                    detail: "bedrock: profile `x` credential expired".into(),
                    reason: routectl_core::UpstreamReason::CredentialExpired,
                    parked_until_ms: None,
                },
                true,
            ),
            (
                "config",
                Error::Config("some non-auth failure".into()),
                false,
            ),
        ];
        for (name, err, is_auth_failure) in rows {
            match (probe_outcome_for_resolve_error(&err), is_auth_failure) {
                (routectl_core::ProbeOutcome::AuthFailed(reason), true) => {
                    assert!(!reason.contains("profile"), "{name}: {reason}");
                    assert!(!reason.contains('x'), "{name}: {reason}");
                }
                (routectl_core::ProbeOutcome::Unreachable(_), false) => {}
                (other, _) => panic!("{name}: unexpected outcome {other:?}"),
            }
        }
    }

    /// The credential-resolution probe is bounded: a resolver that would
    /// take far longer than the timeout returns `Unreachable` within the
    /// bound, not a hang. Proven WITHOUT a fixed sleep on the happy path
    /// -- the slow future is cancelled by the timeout, so the test
    /// completes in ~the timeout, not the 30s delay.
    #[tokio::test]
    async fn probe_bounded_resolve_times_out_to_unreachable() {
        let slow = async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(auth::ResolvedCreds::Bearer {
                key: "unused".into(),
            })
        };
        let outcome = probe_bounded_resolve(std::time::Duration::from_millis(150), slow).await;
        assert!(
            matches!(outcome, routectl_core::ProbeOutcome::Unreachable(_)),
            "a resolver slower than the bound must map to Unreachable, got {outcome:?}"
        );
    }

    /// A resolver that completes within the bound still maps normally: a
    /// successful resolution is `Reachable` (proves the timeout wrapper
    /// does not mask the happy path).
    #[tokio::test]
    async fn probe_bounded_resolve_fast_success_is_reachable() {
        let fast = async {
            Ok(auth::ResolvedCreds::Bearer {
                key: "unused".into(),
            })
        };
        let outcome = probe_bounded_resolve(std::time::Duration::from_secs(5), fast).await;
        assert_eq!(outcome, routectl_core::ProbeOutcome::Reachable);
    }

    // -----------------------------------------------------------------------
    // Response-body cap seams (the 16 MiB DoS response-body cap). Pure-function mappers exercised
    // without HTTP: the bedrock endpoint is region-derived (no base_url), so
    // there is no mock-HTTP harness -- the (bytes, hit_cap) -> outcome logic
    // is structured as small testable functions instead.
    // -----------------------------------------------------------------------

    #[test]
    fn error_body_over_cap_preserves_status_and_retry_after() {
        let retry = std::time::Duration::from_secs(30);
        let raw_prefix = "a giant throttling body that was truncated at the cap";
        let err = map_with_headers("prov", 429, &[("retry-after", "30")], raw_prefix, true);
        match err {
            Error::Upstream {
                status,
                retry_after,
                body,
                ..
            } => {
                assert_eq!(status, 429, "original upstream status must be preserved");
                assert_eq!(
                    retry_after,
                    Some(retry),
                    "the header retry_after must survive the cap trip"
                );
                assert_eq!(body, crate::http_client::body_cap_exceeded_message());
                assert!(
                    !body.contains("throttling"),
                    "capped error message must not echo the raw body prefix"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn error_body_over_cap_403_keeps_iam_action_only() {
        let prefix = "User: arn:aws:iam::123456789012:user/bob is not authorized to \
                      perform: bedrock:InvokeModel on resource: arn:aws:bedrock:...";
        let err = map_text("prov", 403, prefix, true);
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 403);
                assert_eq!(
                    body,
                    r#"{"error":{"message":"bedrock access denied: missing IAM action bedrock:InvokeModel"}}"#
                );
                assert!(
                    !body.contains("arn:aws"),
                    "principal/resource ARN must not leak"
                );
                assert!(!body.contains("123456789012"), "account id must not leak");
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn error_body_under_cap_classifies_unchanged() {
        let prefix = "some 400 validation detail";
        let err = map_text("prov", 400, prefix, false);
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 400);
                assert_eq!(
                    body,
                    classify_client_error_message(400, prefix, &Default::default()),
                    "under-cap path must classify exactly as before"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    /// A synthetic 403 AccessDenied envelope over the native lane: the
    /// bounded `__type` token rides onto the `Error`, but the scrubbed body
    /// carries NO principal ARN, account id, or `User:`/`Principal:` field.
    #[test]
    fn native_lane_403_access_denied_scrubs_body_but_lifts_type() {
        let fixture = include_str!("../../tests/fixtures/bedrock/error_403_access_denied.json");
        let err = map_text("bedrock-invoke", 403, fixture, false);
        match err {
            Error::Upstream {
                status,
                body,
                upstream_type,
                ..
            } => {
                assert_eq!(status, 403);
                assert!(!body.contains("arn:aws"), "403 body leaked an ARN: {body}");
                assert!(
                    !body.contains("123456789012"),
                    "403 body leaked the account id: {body}"
                );
                assert!(
                    !body.contains("User:"),
                    "403 body leaked the principal: {body}"
                );
                assert!(
                    !body.contains("Principal:"),
                    "403 body leaked the principal: {body}"
                );
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("AccessDeniedException"),
                    "the bounded AWS __type token must ride even on the scrubbed 403 path"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    /// A synthetic 400 ValidationException envelope over the native lane:
    /// the request-fault body carries the sanitized flat AWS envelope (the
    /// downstream matcher needs it) and the lifted `upstream_type` is the
    /// namespace-stripped `ValidationException`.
    #[test]
    fn native_lane_400_validation_carries_envelope_and_lifts_type() {
        let fixture = include_str!("../../tests/fixtures/bedrock/error_400_validation.json");
        let err = map_text("bedrock-invoke", 400, fixture, false);
        match err {
            Error::Upstream {
                status,
                body,
                upstream_type,
                ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("ValidationException"),
                    "the AWS __type must lift namespace-stripped"
                );
                assert!(
                    body.contains("ValidationException"),
                    "400 body must carry the flat AWS envelope: {body}"
                );
                assert!(
                    body.contains("synthetic_unknown_field"),
                    "400 body must carry the envelope message the matcher reads: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    /// A ValidationException envelope whose message exceeds the log excerpt
    /// (`MAX_LOG_BODY_EXCERPT`) but fits the shared matcher ceiling reaches
    /// the stored `Error.body` as intact JSON with the full message. The old
    /// 512-char log excerpt truncated a verbose envelope into unparseable
    /// JSON and silently defeated the capability matcher.
    #[test]
    fn native_lane_400_validation_over_log_excerpt_carries_intact_json() {
        let long_message = format!(
            "Malformed input request: {} is not permitted, please reformat",
            "extraneous_key_".repeat(50)
        );
        assert!(
            long_message.len() > routectl_core::MAX_LOG_BODY_EXCERPT,
            "sanity: message must exceed the log excerpt cap"
        );
        let fixture = serde_json::json!({
            "__type": "com.amazon.coral.validate#ValidationException",
            "message": long_message,
        })
        .to_string();
        let err = map_text("bedrock-invoke", 400, &fixture, false);
        let Error::Upstream { status, body, .. } = err else {
            panic!("expected Error::Upstream");
        };
        assert_eq!(status, 400);
        assert!(
            body.len() > routectl_core::MAX_LOG_BODY_EXCERPT,
            "the matcher's source body must survive past the log excerpt cap"
        );
        let parsed: Value =
            serde_json::from_str(&body).expect("stored request-fault body must be intact JSON");
        assert_eq!(
            parsed.get("message").and_then(Value::as_str),
            Some(long_message.as_str()),
            "the full validation message must reach the matcher intact"
        );
    }

    /// The request-fault path raises the body cap to the matcher ceiling, it
    /// does not remove it: a flat ValidationException whose body runs PAST
    /// `MAX_ERROR_BODY_BYTES` is NOT carried raw -- it collapses to the short
    /// sanitized log excerpt, so the widened cap has a hard ceiling and a
    /// hostile upstream cannot force a 64 KB+ body onto the stored `Error`.
    #[test]
    fn native_lane_400_validation_over_matcher_ceiling_collapses_to_excerpt() {
        const MARKER_LEN: usize = "... [truncated]".len();
        let oversized_message = "x".repeat(routectl_core::MAX_ERROR_BODY_BYTES * 2);
        let fixture = serde_json::json!({
            "__type": "com.amazon.coral.validate#ValidationException",
            "message": oversized_message,
        })
        .to_string();
        assert!(
            fixture.len() > routectl_core::MAX_ERROR_BODY_BYTES,
            "sanity: body must exceed the matcher ceiling"
        );
        let err = map_text("bedrock-invoke", 400, &fixture, false);
        let Error::Upstream { body, .. } = err else {
            panic!("expected Error::Upstream");
        };
        // An over-ceiling body is not carryable, so it falls back to the
        // bounded 512-char log excerpt rather than a 64 KB raw body.
        assert!(
            body.len() <= routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN,
            "an over-ceiling body must collapse to the short excerpt, got {}",
            body.len()
        );
    }

    /// A NESTED `{"error":{"message":...}}` 400 envelope (a reverse proxy or
    /// custom endpoint fronting Bedrock, NOT the flat AWS shape) is NOT carried
    /// raw even below the matcher ceiling: it keeps the bounded 512-char
    /// excerpt, so the widened cap cannot reflect a large nested-shaped message
    /// up to the caller via the ingress `error.message` reader.
    #[test]
    fn native_lane_400_nested_error_envelope_stays_on_excerpt() {
        const MARKER_LEN: usize = "... [truncated]".len();
        let long_message = "leak_".repeat(400);
        assert!(
            long_message.len() > routectl_core::MAX_LOG_BODY_EXCERPT,
            "sanity: nested message exceeds the log excerpt cap"
        );
        let fixture = serde_json::json!({
            "error": { "message": long_message },
        })
        .to_string();
        assert!(
            fixture.len() <= routectl_core::MAX_ERROR_BODY_BYTES,
            "sanity: body is within the matcher ceiling (isolates the shape gate)"
        );
        let err = map_text("bedrock-invoke", 400, &fixture, false);
        let Error::Upstream { body, .. } = err else {
            panic!("expected Error::Upstream");
        };
        assert!(
            body.len() <= routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN,
            "a nested-shaped body must collapse to the short excerpt, got {}",
            body.len()
        );
        assert!(
            !body.contains(&long_message),
            "the oversized nested message must never survive into the stored body"
        );
    }

    /// A cap trip collapses the body to the fixed cap message (the truncated
    /// prefix is never echoed), yet a header-lifted token still rides onto
    /// the `Error`.
    #[test]
    fn cap_trip_keeps_fixed_message_but_tokens_still_ride() {
        let truncated = r#"{"__type":"com.amazonaws.bedrock#ThrottlingExcept"#;
        let err = map_with_headers(
            "prov",
            400,
            &[("x-amzn-errortype", "ThrottlingException")],
            truncated,
            true,
        );
        match err {
            Error::Upstream {
                body,
                upstream_type,
                ..
            } => {
                assert_eq!(
                    body,
                    crate::http_client::body_cap_exceeded_message(),
                    "a cap trip must collapse to the fixed message, never the prefix"
                );
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("ThrottlingException"),
                    "a header-lifted token must still ride past the cap collapse"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn cap_trip_emits_single_warn_with_settled_fields() {
        let events = routectl_testkit::capture_events(|| {
            crate::http_client::warn_body_cap("prov", 200, Some(1234), "complete_success_body");
        });
        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect();
        assert_eq!(warns.len(), 1, "exactly one WARN per cap trip");
        let w = warns[0];
        assert_eq!(w.field("provider"), Some("prov"));
        assert_eq!(w.field("status"), Some("200"));
        assert_eq!(
            w.field("body_cap_bytes"),
            Some(
                crate::http_client::MAX_RESPONSE_BODY_BYTES
                    .to_string()
                    .as_str()
            )
        );
        assert_eq!(w.field("content_length"), Some("Some(1234)"));
        assert_eq!(w.field("body_truncated"), Some("true"));
        assert_eq!(w.field("path"), Some("complete_success_body"));
    }

    /// A non-envelope JSON error echoing a credential, longer than the
    /// excerpt cap, never surfaces it in the WARN excerpt or the client
    /// error's Debug.
    #[test]
    fn error_body_credential_redacted_in_warn_and_client_error() {
        let body = format!(
            r#"{{"detail":{{"input":{{"secretKey":"SENTINEL-BEDROCK-KEY"}}}},"msg":"{}"}}"#,
            "m".repeat(700)
        );
        let events = routectl_testkit::capture_events(|| {
            log_bedrock_upstream_error("prov", 400, &body, false);
        });
        let err = map_text("prov", 400, &body, false);
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("upstream-failure WARN must fire");
        let excerpt = warn.field("body_excerpt").expect("body_excerpt field");
        let debug = format!("{err:?}");

        assert!(!excerpt.contains("SENTINEL-BEDROCK-KEY"), "{excerpt}");
        assert!(!debug.contains("SENTINEL-BEDROCK-KEY"), "{debug}");
    }

    /// On a cap trip the upstream-failure WARN (`log_bedrock_upstream_error`)
    /// emits the FIXED cap message as its `body_excerpt` -- the truncated
    /// prefix must never reach WARN level.
    #[test]
    fn error_body_over_cap_warn_excerpt_is_fixed_message() {
        let prefix = "a giant validation body detail that must not appear at WARN";
        let events = routectl_testkit::capture_events(|| {
            log_bedrock_upstream_error("prov", 400, prefix, true);
        });
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("upstream-failure WARN must fire");
        assert_eq!(
            warn.field("body_excerpt"),
            Some(crate::http_client::body_cap_exceeded_message().as_str()),
            "WARN excerpt must be the fixed cap message on a cap trip"
        );
        assert!(
            warn.fields
                .iter()
                .all(|(_, v)| !v.contains("giant validation body")),
            "no WARN field may echo the truncated prefix"
        );
    }

    /// Spawn a one-shot raw TCP server returning `status` with a chunked
    /// (no Content-Length) `body`, and return the URL to GET. wiremock sets
    /// an honest Content-Length, so a chunked server is the only way to
    /// drive the shared error read down its streaming read path.
    async fn spawn_chunked_error_server(status: u16, body: &'static str) -> String {
        spawn_chunked_error_server_with_headers(status, body, "").await
    }

    /// Read `resp` through the shared seam and map it as the lane does.
    async fn read_and_map(resp: reqwest::Response) -> Error {
        let read = UpstreamErrorRead::read("prov", resp).await;
        map_bedrock_upstream_error("bedrock-invoke", "prov", read)
    }

    /// Variant of [`spawn_chunked_error_server`] that injects
    /// `extra_headers` (each a full `Name: value\r\n` line) into the
    /// response head, so a test can exercise the `x-amzn-errortype` lift.
    async fn spawn_chunked_error_server_with_headers(
        status: u16,
        body: &'static str,
        extra_headers: &'static str,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status} ERR\r\nContent-Type: application/json\r\n{extra_headers}Transfer-Encoding: chunked\r\n\r\n"
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket
                .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                .await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.write_all(b"\r\n0\r\n\r\n").await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}")
    }

    /// A native-lane 400 whose flat `{"message":...}` body omits `__type`
    /// lifts the `ValidationException` discriminator from the
    /// `x-amzn-errortype` header, and the coral URL tail that rides in that
    /// header value never reaches any `Error::Upstream` field.
    #[tokio::test]
    async fn error_type_header_lifts_without_leaking_coral_url() {
        let error_type = "x-amzn-ErrorType: ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/\r\n";
        let body = r#"{"message":"tool type 'advisor' is not supported for this model"}"#;
        let url = spawn_chunked_error_server_with_headers(400, body, error_type).await;
        let resp = reqwest::get(url).await.unwrap();

        let err = read_and_map(resp).await;
        let Error::Upstream {
            body: err_body,
            upstream_type,
            upstream_code,
            ..
        } = err
        else {
            panic!("expected Error::Upstream");
        };
        assert_eq!(
            upstream_type.as_deref(),
            Some("ValidationException"),
            "the bare exception name must lift from the header"
        );
        for field in [
            Some(err_body.as_str()),
            upstream_type.as_deref(),
            upstream_code.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                !field.contains("internal.amazon.com") && !field.contains("coral"),
                "the coral URL tail leaked into an Error field: {field}"
            );
        }
    }

    /// When a native-lane 400 carries BOTH a valid body `__type` and the
    /// `x-amzn-errortype` header, the body token keeps precedence: the
    /// mantle-proxied shape's in-band discriminator wins over the header
    /// fallback so a header-provided token can never override a present
    /// body classifier.
    #[tokio::test]
    async fn body_type_wins_over_error_type_header() {
        let error_type = "x-amzn-ErrorType: ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/\r\n";
        let body =
            r#"{"__type":"com.amazon.coral.service#ThrottlingException","message":"slow down"}"#;
        let url = spawn_chunked_error_server_with_headers(400, body, error_type).await;
        let resp = reqwest::get(url).await.unwrap();

        let Error::Upstream { upstream_type, .. } = read_and_map(resp).await else {
            panic!("expected Error::Upstream");
        };
        assert_eq!(
            upstream_type.as_deref(),
            Some("ThrottlingException"),
            "a present body __type must win over the x-amzn-errortype header",
        );
    }

    #[tokio::test]
    async fn error_read_under_cap_logs_error_but_not_a_cap_trip() {
        // An under-cap error body is read fully: the upstream error WARN
        // fires (on the body), and the cap-trip WARN (the one carrying
        // `path`) must NOT fire. This pins that the cap WARN is gated on an
        // actual trip and is a separate signal from the error-classification
        // WARN.
        let url = spawn_chunked_error_server(500, "upstream boom detail").await;
        let resp = reqwest::get(url).await.unwrap();

        let (err, events) = routectl_testkit::with_capture(read_and_map(resp)).await;

        let Error::Upstream { body, .. } = err else {
            panic!("expected Error::Upstream");
        };
        assert!(body.contains("upstream boom detail"), "body: {body}");
        assert!(
            events.iter().all(|e| e.field("path").is_none()),
            "no cap-trip WARN (carrying `path`) may fire under cap",
        );
        assert!(
            events
                .iter()
                .any(|e| e.level == tracing::Level::WARN && e.message.contains("bedrock upstream")),
            "the upstream error WARN must still fire on the body",
        );
    }
}
