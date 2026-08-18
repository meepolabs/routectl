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
//!   - `stream`: owns a `ThinkTagAccumulator` for RawThinkTag state; all other
//!     dialects delegate to the stateless `parse_event`.
//!
//! NOTE on `stream` statefulness:
//!   The `<think>` tag state machine needs to track whether we are inside or
//!   outside a tag across multiple SSE chunks. The stateless `parse_event`
//!   handles the stateless dialects (DeepSeek, vLLM, OpenAI, etc.); the
//!   stateful `ThinkTagAccumulator` lives inside `stream()` as a local
//!   variable captured by the stream future for RawThinkTag.

pub mod dialect;
pub mod dialects;
pub mod request;
pub mod response;
pub mod sse;
pub(crate) mod util;
pub(crate) mod wire_lift;

pub use dialect::ReasoningDialect;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::debug;

use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, debug_upstream_error_body,
    extract_upstream_message, is_json_error_envelope, sanitize_for_log, trace_outgoing_body,
    trace_upstream_success_body,
};

use sse::ThinkTagAccumulator;

#[cfg(feature = "bedrock")]
use crate::mantle::MantleAuth;

/// Provider-kind discriminator string used in tracing fields. Single
/// source of truth so call sites grep clean (`provider_kind=openai-compat`)
/// and a typo-on-rename can't silently break operator log filters.
const PROVIDER_KIND: &str = "openai-compat";

/// Configuration for an openai-compat egress provider.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Provider identifier used in tracing and log fields.
    pub id: String,
    /// Base URL of the upstream; the completions path is appended to it.
    pub base_url: String,
    /// Bearer credential sent in the `Authorization` header.
    pub api_key: String,
    /// Provider-level extra headers (renamed from `extra_headers` in
    /// v0.6.0). Operators set `[providers.X] header_extras = { ... }`;
    /// the router-side dispatch merges these with per-model
    /// `header_extras` before reaching the egress (see
    /// `Router::merge_header_extras`).
    pub header_extras: Vec<(String, String)>,
    /// Provider-level payload extras (renamed from `default_extras` in
    /// v0.6.0). Lives here as a fallback for library consumers
    /// constructing `OpenAiCompatConfig` directly without the router;
    /// when the router is in the loop, deep-merged with per-model
    /// `payload_extras` and landed on `req.provider_extras` before
    /// reaching this egress, so this slot is typically `None`.
    pub payload_extras: Option<Value>,
    /// Default reasoning dialect used by library consumers that build
    /// a provider directly (no router). With the router in the loop,
    /// every request carries the per-model dialect via
    /// `req.routectl_internal.reasoning_dialect` and the dispatch
    /// path reads from there; this field is the fallback for direct
    /// construction.
    pub reasoning_dialect: ReasoningDialect,
    /// Default outgoing-history reasoning policy for library consumers
    /// that build a provider directly. With the router in the loop,
    /// every request carries the per-model policy via
    /// `req.routectl_internal.history_reasoning`.
    pub history_reasoning: HistoryReasoning,
    /// Override the User-Agent on outbound requests. `None` keeps reqwest's default.
    pub user_agent: Option<String>,
    /// When `true`, requests carrying canonical-only fields (`cache_control`,
    /// `anthropic_beta`, `ToolDef::Other`, `ContentPart::Other`,
    /// `SystemContent::Blocks` with cache_control, etc.) are rejected with
    /// `Error::Validation` instead of warn-and-dropped. Set from
    /// `[server] strict_translation` at provider build time.
    pub strict_translation: bool,
    /// When `true`, suppress the auto-injected
    /// `stream_options.include_usage = true` on streaming requests.
    /// Default `false` (auto-inject). Use for openai-compat hosts that
    /// 400 on unknown fields. Operator-supplied `stream_options` in
    /// `payload_extras` / `provider_extras` always wins -- including an
    /// explicit `include_usage = false`.
    pub disable_stream_include_usage: bool,
    /// Bedrock mantle authentication. `Some` selects the mantle lane: the
    /// request body is SigV4/bearer-signed under the `bedrock-mantle`
    /// scope, the first-party `Authorization: Bearer <api_key>` insert is
    /// skipped (the signer owns auth for both credential shapes; the
    /// `api_key` is empty by config validation), a no-redirect client is
    /// used, and the probe resolves the credential rather than dialing
    /// `/models`. `None` (default) keeps the first-party openai-compat
    /// behavior byte-for-byte. Resolved at the factory from a
    /// `bedrock_mantle` sub-config.
    #[cfg(feature = "bedrock")]
    pub mantle: Option<MantleAuth>,
}

/// Outgoing-history reasoning policy. Sibling of the router-side
/// `HistoryReasoning` TOML enum; the factory maps between the two.
///
/// Background: DeepSeek v3 explicitly REJECTED `reasoning_content` in
/// echo-back history (would 400). DeepSeek v4 inverted the contract --
/// it now REQUIRES `reasoning_content` to be passed back, with a 400
/// reading `"reasoning_content in the thinking mode must be passed
/// back to the API"` if the field is missing. The same wire dialect
/// (`reasoning_dialect = "deepseek"`) supports both versions, so the
/// strip-or-preserve choice cannot live on the dialect itself; this
/// per-provider knob carries the operator's intent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HistoryReasoning {
    /// Use the dialect's default. DeepSeek and Vllm strip; OpenAI and
    /// OpenRouter pass through. Backward-compatible default.
    #[default]
    Auto,
    /// Force-strip reasoning fields from outgoing assistant messages.
    Strip,
    /// Force-emit the dialect-native preserve shape on outgoing
    /// assistant messages (DeepSeek/Vllm: `reasoning_content`;
    /// OpenRouter: `reasoning_details`).
    Preserve,
}

impl From<routectl_core::CoreHistoryReasoning> for HistoryReasoning {
    fn from(h: routectl_core::CoreHistoryReasoning) -> Self {
        match h {
            routectl_core::CoreHistoryReasoning::Auto => Self::Auto,
            routectl_core::CoreHistoryReasoning::Strip => Self::Strip,
            routectl_core::CoreHistoryReasoning::Preserve => Self::Preserve,
        }
    }
}

/// openai-compat chat-completions egress provider.
pub struct OpenAiCompatProvider {
    cfg: OpenAiCompatConfig,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    /// Build a provider from its configuration.
    pub fn new(cfg: OpenAiCompatConfig) -> Self {
        // Both the mantle and first-party lanes use a no-redirect client.
        // Mantle: a signed POST must never be auto-followed across a 3xx,
        // since replaying the SigV4 signature against a different host
        // always fails. First-party: the `Authorization: Bearer` header
        // reqwest DOES strip on a cross-host hop, but keeping the same
        // no-redirect posture as every other lane on this crate avoids a
        // per-lane accident if a future header_extras addition attaches
        // something reqwest's default list does not cover. Either way a
        // redirect on this lane is an upstream fault to surface, not to
        // chase.
        let client = crate::http_client::build_no_redirect(cfg.user_agent.as_deref())
            .expect("reqwest no-redirect client build failed (TLS init?); fatal at startup");
        Self { cfg, client }
    }

    fn completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    /// True when this provider egresses on the Bedrock mantle lane. On
    /// this lane the signer owns auth (no `Authorization: Bearer
    /// <api_key>`), the body is serialized to signable bytes, and a
    /// no-redirect client is used. Always `false` in a build without the
    /// `bedrock` feature.
    const fn is_mantle(&self) -> bool {
        #[cfg(feature = "bedrock")]
        {
            self.cfg.mantle.is_some()
        }
        #[cfg(not(feature = "bedrock"))]
        {
            false
        }
    }

    /// SigV4/bearer-sign a built request in place on the mantle lane; a
    /// no-op on the first-party lane. Signing runs AFTER the request is
    /// fully built (method, URL, headers, body bytes) and BEFORE any
    /// header trace or execute, so the trace shows the real auth header
    /// and the signed input matches the transmitted bytes.
    #[cfg(feature = "bedrock")]
    async fn sign_mantle(&self, request: &mut reqwest::Request) -> Result<()> {
        if let Some(mantle) = self.cfg.mantle.as_ref() {
            crate::mantle::sign(request, &mantle.creds, &mantle.region).await?;
        }
        Ok(())
    }

    /// Record the mantle lane context (`lane`, `auth_mode`, `region`) on
    /// the current tracing span so every event within it -- including the
    /// shared upstream-failure WARN -- carries the lane fields. A no-op on
    /// the first-party lane, where the span's `Empty` fields stay unset
    /// and never render.
    #[cfg(feature = "bedrock")]
    fn record_mantle_span_fields(&self) {
        if let Some(mantle) = self.cfg.mantle.as_ref() {
            let span = tracing::Span::current();
            span.record("lane", crate::mantle::MANTLE_SERVICE);
            span.record("auth_mode", mantle_auth_mode(&mantle.creds));
            span.record("region", mantle.region.as_str());
        }
    }

    /// Resolve the per-request reasoning dialect. v0.6.0 moved the
    /// dialect off `[providers.X]` to `[models.X]`; the router lifts
    /// it onto `req.routectl_internal.reasoning_dialect` before
    /// dispatch. When the carrier is empty (library consumer
    /// constructing `ChatRequest` directly with no router) the
    /// `OpenAiCompatConfig` default applies.
    fn dialect_for(&self, req: &ChatRequest) -> ReasoningDialect {
        req.routectl_internal
            .reasoning_dialect
            .map_or(self.cfg.reasoning_dialect, ReasoningDialect::from)
    }

    /// Same fallback contract as `dialect_for` but for the history-
    /// reasoning policy.
    fn history_reasoning_for(&self, req: &ChatRequest) -> HistoryReasoning {
        req.routectl_internal
            .history_reasoning
            .map_or(self.cfg.history_reasoning, HistoryReasoning::from)
    }

    fn build_headers(&self, req: &ChatRequest) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        // Mantle lane: the SigV4/bearer signer attaches Authorization
        // post-build and the `api_key` is empty by config validation, so
        // skip the first-party Bearer insert entirely. The first-party
        // lane stamps the bearer here. CONTENT_TYPE is stamped on both
        // lanes below.
        if !self.is_mantle() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))
                    .map_err(|e| Error::Config(format!("invalid api_key for header: {e}")))?,
            );
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Prefer the router-composed map (provider + model merged at
        // dispatch) if present; fall back to `self.cfg.header_extras`
        // for library consumers that built the provider directly.
        let source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        crate::http_client::apply_header_extras(&mut headers, &source, &self.cfg.id, &[]);
        Ok(headers)
    }

    /// Build the outgoing chat/completions request. On the mantle lane the
    /// body is serialized to bytes (the content-type is already stamped by
    /// `build_headers`) so the signer can hash the exact transmitted body;
    /// the first-party lane keeps the stock `.json(&body)` builder
    /// byte-for-byte. The returned request is UNSIGNED -- the caller signs
    /// it on the mantle lane via `sign_mantle`.
    fn build_request(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &Value,
    ) -> Result<reqwest::Request> {
        let rb = self.client.post(url).headers(headers);
        let rb = if self.is_mantle() {
            let body_bytes = serde_json::to_vec(body)
                .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
            rb.body(body_bytes)
        } else {
            rb.json(body)
        };
        rb.build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        let dialect = self.dialect_for(req);
        let history = self.history_reasoning_for(req);
        request::normalize(
            &self.cfg.id,
            req,
            dialect,
            history,
            self.cfg.payload_extras.as_ref(),
            self.cfg.strict_translation,
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        // No `req` context here (the trait signature is response-only);
        // fall back to the config-side default. Streaming callers
        // route through `stream()` which captures the per-request
        // dialect into the SSE state machine via `dialect_for(req)`.
        response::normalize(&self.cfg.id, raw, self.cfg.reasoning_dialect)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
        let mut body = self.normalize_request(&req)?;
        // Force non-streaming.
        body["stream"] = Value::Bool(false);

        let headers = self.build_headers(&req)?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions");

        // Trace-level outgoing body for triage. Gated by
        // `tracing::Level::TRACE`; default `info` filter pays
        // nothing.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Mantle lane: serialize to signable bytes (content-type already
        // stamped by build_headers) so the signer can hash the exact
        // transmitted body; the first-party lane keeps the stock `.json()`
        // builder byte-for-byte.
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = self.build_request(&url, headers, &body)?;
        // Mantle lane: sign the built request before any header trace or
        // execute so the trace shows the real auth header and the signed
        // input matches the transmitted bytes. A no-op on the first-party
        // lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth). build_headers
        // assembled the auth into `headers` (or, on the mantle lane, the
        // signer attached it above); capture the full set from the built
        // request. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
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
        if !resp.status().is_success() {
            // Read headers BEFORE the capped body read moves `resp`; the
            // shared mapper takes `&HeaderMap` and computes the
            // rate-limit-gated retry_after + classifier + WARN split.
            let (headers, body_text, hit_cap) =
                read_capped_error_body(&self.cfg.id, status, resp).await;
            return Err(map_openai_compat_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
                hit_cap,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE the capped body
        // read moves `resp`. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        let content_length = resp.content_length();
        // Cloned BEFORE the capped read consumes `resp`: an inline
        // `{"error":{...}}` envelope on a 2xx body routes through the same
        // mapper the non-2xx path uses, which reads these for the
        // rate-limit-gated `retry_after`.
        let response_headers = resp.headers().clone();
        let (body_bytes, hit_cap) =
            crate::http_client::read_body_capped(resp, crate::http_client::MAX_RESPONSE_BODY_BYTES)
                .await
                .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;
        if hit_cap {
            // An unreadable 2xx body is an invalid upstream response: a
            // 502-class upstream error (ServerError) so it debits the
            // breaker and retries/fallbacks like any protocol failure.
            crate::http_client::warn_body_cap(
                &self.cfg.id,
                status,
                content_length,
                "complete_success_body",
            );
            return Err(Error::upstream(
                &self.cfg.id,
                502,
                crate::http_client::body_cap_exceeded_message(),
            ));
        }
        let raw: Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;

        // An OpenRouter-style inline `{"error":{...}}` envelope can arrive
        // in a 200-OK body. Classify it here BEFORE normalize so it surfaces
        // as an upstream error instead of landing in extras (or hard-failing
        // the deserialize on absent `choices`). Mirrors the streaming
        // mid-frame detection and reuses the same mapper as the non-2xx path.
        if let Some(err) = detect_inline_error_2xx(
            &self.cfg.id,
            &raw,
            &response_headers,
            &String::from_utf8_lossy(&body_bytes),
        ) {
            return Err(err);
        }

        // Trace the raw upstream body BEFORE normalization
        // mutates it (`coalesce_reasoning_content_in_response`
        // rewrites `reasoning_content` -> `reasoning` in place).
        // Operators triaging "what did the upstream return" want
        // the wire form, not the post-processed form.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw);

        // Use the per-request dialect (lifted from the router's
        // RoutectlInternal carrier; falls back to config when the
        // request bypassed the router) so a request that landed on a
        // DeepSeek-dialect model lifts reasoning_content even though
        // the trait-level `normalize_response` only sees the
        // config-side default.
        let dialect = self.dialect_for(&req);
        let mut chat_resp = response::normalize(&self.cfg.id, raw, dialect)?;
        response::apply_stop_sequence_heuristic(&mut chat_resp, req.stop.as_deref());
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
        let mut body = self.normalize_request(&req)?;
        body["stream"] = Value::Bool(true);

        // Auto-inject `stream_options.include_usage = true` so the
        // upstream emits a terminal usage chunk + finish_reason.
        // Without this, many openai-compat hosts end the stream with a
        // bare `data: [DONE]` and routectl's stream summary reports
        // `chunks=N finish_reason=unknown total_tokens=0` even on a
        // fully successful stream. Operator-supplied `stream_options`
        // (default_extras / provider_extras) always wins; the opt-out
        // toggle is `disable_stream_include_usage = true`.
        ensure_stream_options_include_usage(&mut body, self.cfg.disable_stream_include_usage);

        let headers = self.build_headers(&req)?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions (stream)");

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Mantle lane: serialize to signable bytes (see complete()); the
        // first-party lane keeps the stock `.json()` builder byte-for-byte.
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = self.build_request(&url, headers, &body)?;
        // Mantle lane: sign the built stream request before trace/execute
        // (see complete()). No-op on the first-party lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) for the stream
        // path. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
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
        if !resp.status().is_success() {
            // Read headers BEFORE the capped body read moves `resp`. Shared
            // with complete(): retry_after is preserved on the stream
            // path via the same upstream_full mapping.
            let (headers, body_text, hit_cap) =
                read_capped_error_body(&self.cfg.id, status, resp).await;
            return Err(map_openai_compat_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
                hit_cap,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream below. The stream path had no dir-3
        // capture before; this closes the gap so it matches complete().
        // Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        let provider_id = self.cfg.id.clone();
        let dialect = self.dialect_for(&req);
        // Capture the request stop list for the openai-compat stop-sequence
        // heuristic (see `response::apply_stop_sequence_heuristic`). Cloned
        // up front because `req` doesn't survive into the async_stream
        // closure.
        let request_stop: Option<Vec<String>> = req.stop.clone();

        // The ThinkTagAccumulator owns state across chunks; it lives in the
        // stream task closure for RawThinkTag dialect.
        // async_stream::stream! lets us hold mutable local state (think_acc)
        // across yield points, which filter_map/FnMut cannot do.
        let mut event_stream = resp.bytes_stream().eventsource();

        let out = async_stream::stream! {
            let mut think_acc = ThinkTagAccumulator::new();
            // Per-stream, monotonically incrementing reasoning detail
            // index for the stateless dialects (DeepSeek/Vllm). Threaded
            // into `parse_event` so successive streamed reasoning deltas
            // carry distinct `index` values instead of collapsing onto 0.
            // RawThinkTag threads its own counter inside ThinkTagAccumulator.
            let mut reasoning_index: u32 = 0;
            // Per-stream synthesizer for missing streamed tool-call ids.
            // Applied to every emitted chunk (both the stateless
            // parse_event path and the RawThinkTag process path) so an
            // id-less indexed tool_call gets a stable id before it reaches
            // the openai->anthropic pairing that keys on the id.
            let mut tool_call_ids = sse::StreamedToolCallIds::default();
            // Per-choice running concatenation of `delta.content` text,
            // indexed by `choice.index` and grown on demand. The terminal
            // chunk (the one carrying `finish_reason`) gets the
            // matched_stop_sequence applied just before yield, mirroring
            // what the non-streaming path does after `normalize_response`.
            // Per-choice (not one shared buffer) so an `n > 1` response
            // never bleeds one choice's content into another choice's
            // suffix match.
            let mut accumulated_text: Vec<String> = Vec::new();
            // Per-choice terminal-signal tracking for the no-`[DONE]` tail
            // below, keyed by `choice.index`. Global tracking mis-handled
            // `n > 1`: a sibling choice's `finish_reason` would mask a
            // different choice whose tool-call args were still in flight at
            // FIN, committing a truncated tool call as complete. Per-choice
            // state flags that truncation instead.
            let mut choice_tails: Vec<ChoiceTail> = Vec::new();
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
                    if dialect == ReasoningDialect::RawThinkTag
                        && let Some(pending) = think_acc.take_pending() {
                            yield Ok(flush_pending_chunk(&pending));
                        }
                    return;
                }
                if trimmed.is_empty() {
                    continue;
                }

                let result = if dialect == ReasoningDialect::RawThinkTag {
                    think_acc.process(&provider_id, &data)
                } else {
                    sse::parse_event(&provider_id, &data, dialect, &mut reasoning_index)
                };

                match result {
                    Ok(None) => {}
                    Ok(Some(mut chunk)) => {
                        // Synthesize any missing streamed tool-call id
                        // before the terminal/tool-call classification and
                        // the yield, so the emitted chunk always carries a
                        // pairing-stable id. An unpairable tool_call (bad
                        // index or a genuinely ambiguous id collision) fails
                        // the stream rather than mispairing a tool_result.
                        if let Err(e) = tool_call_ids.fill_missing_ids(&provider_id, &mut chunk) {
                            yield Err(e);
                            return;
                        }
                        // Mark terminal + tool-call state PER choice.index so
                        // an `n > 1` stream where one choice finishes and a
                        // sibling truncates mid tool-call is classified by
                        // the truncated sibling, not masked by the finisher.
                        for choice in &chunk.choices {
                            if choice_carries_finish_reason(choice)
                                && let Some(t) = choice_tail_mut(&mut choice_tails, choice.index)
                            {
                                t.saw_terminal = true;
                            }
                            if choice_carries_tool_call_delta(choice)
                                && let Some(t) = choice_tail_mut(&mut choice_tails, choice.index)
                            {
                                t.saw_tool_call_delta = true;
                            }
                        }
                        // Only accumulate content text when stop-sequence
                        // detection is actually needed: skip the push_str
                        // when the caller sent no stop sequences, avoiding
                        // allocation and string growth for the common case.
                        if request_stop.is_some() {
                            for choice in &chunk.choices {
                                if let Some(t) = choice.delta.content.as_deref() {
                                    accumulate_choice_text(
                                        &mut accumulated_text,
                                        choice.index,
                                        t,
                                    );
                                }
                            }
                        }
                        // Apply the stop-sequence heuristic on any choice
                        // that carries the terminal finish_reason and no
                        // matched_stop_sequence yet. The non-streaming
                        // path runs the same recovery in `complete()`
                        // after `normalize_response`. Each choice matches
                        // against its OWN accumulated buffer.
                        if let Some(stops) = request_stop.as_deref() {
                            for choice in &mut chunk.choices {
                                if choice.matched_stop_sequence.is_some() {
                                    continue;
                                }
                                if choice.finish_reason.as_deref() != Some("stop") {
                                    continue;
                                }
                                let text = accumulated_text
                                    .get(choice.index as usize)
                                    .map(String::as_str);
                                choice.matched_stop_sequence =
                                    response::detect_matched_stop_sequence(text, stops);
                            }
                        }
                        yield Ok(chunk);
                    }
                    Err(e) => yield Err(e),
                }
            }
            // Stream exhausted without a `data: [DONE]` terminator (the
            // `[DONE]` path returns above). Flush any RawThinkTag pending
            // buffer FIRST -- those held-back bytes are real visible
            // content the client must receive regardless of truncation.
            if dialect == ReasoningDialect::RawThinkTag
                && let Some(pending) = think_acc.take_pending() {
                    yield Ok(flush_pending_chunk(&pending));
                }
            // Then classify the tail: a clean FIN with neither `[DONE]`
            // nor a `finish_reason` is truncation, not completion.
            match classify_stream_tail(&choice_tails) {
                StreamTail::Terminated => {}
                StreamTail::TruncatedText => {
                    tracing::warn!(
                        provider = %provider_id,
                        "openai-compat stream closed without a terminal signal \
                         ([DONE] or finish_reason); response may be truncated"
                    );
                }
                StreamTail::TruncatedToolCall => {
                    tracing::warn!(
                        provider = %provider_id,
                        "openai-compat stream closed mid tool-call without a terminal \
                         signal ([DONE] or finish_reason); tool-call arguments are \
                         likely truncated"
                    );
                    yield Err(Error::Streaming(format!(
                        "provider `{provider_id}`: upstream closed the stream without a \
                         terminal signal ([DONE] or finish_reason) while tool-call \
                         arguments were still accumulating; the tool call is likely truncated"
                    )));
                }
            }
        };

        Ok(routectl_core::wrap_stream_with_summary(
            out,
            "upstream",
            PROVIDER_KIND,
            self.cfg.id.clone(),
        ))
    }

    async fn probe(&self) -> routectl_core::ProbeOutcome {
        // Mantle lane: the endpoint signs with SigV4/bearer and exposes no
        // free `/models` surface, so probe the credential rather than
        // dialing the inference host with a Bearer GET.
        #[cfg(feature = "bedrock")]
        if let Some(mantle) = &self.cfg.mantle {
            return crate::mantle::probe(&mantle.creds).await;
        }
        let url = format!("{}/models", self.cfg.base_url.trim_end_matches('/'));
        let mut headers = HeaderMap::new();
        match HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key)) {
            Ok(v) => {
                headers.insert(AUTHORIZATION, v);
            }
            Err(_) => {
                return routectl_core::ProbeOutcome::Unreachable(
                    "credential could not form an auth header".into(),
                );
            }
        }
        crate::probe::http_get_probe(
            self.cfg.user_agent.as_deref(),
            &url,
            headers,
            crate::probe::PROBE_TIMEOUT,
        )
        .await
    }
}

/// Observability discriminator for a resolved mantle credential shape:
/// `"bearer"` for a Bedrock console API key, `"sigv4"` for a signed AWS
/// credential. Never carries any secret material. Local to this lane so
/// the compat runtime does not reach into the anthropic-api module's
/// credential internals.
#[cfg(feature = "bedrock")]
const fn mantle_auth_mode(creds: &crate::bedrock::auth::ResolvedCreds) -> &'static str {
    match creds {
        crate::bedrock::auth::ResolvedCreds::Bearer { .. } => "bearer",
        crate::bedrock::auth::ResolvedCreds::Sigv4 { .. } => "sigv4",
    }
}

/// Build a content-only `ChatChunk` from the ThinkTagAccumulator's
/// pending buffer. Called at stream-end when the accumulator held back
/// bytes waiting to see if a `<think>` / `</think>` tag completes --
/// but the stream terminated first. Those bytes are real visible
/// content the client must receive.
///
/// The flushed content is attributed to choice 0: RawThinkTag is a
/// single-choice dialect in practice (the providers that embed
/// `<think>` tags do not fan out n>1), so there is no per-choice index
/// to carry through here.
fn flush_pending_chunk(text: &str) -> ChatChunk {
    use routectl_core::schema::{ChunkChoice, ChunkDelta};
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.to_string()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// True when this choice carries a terminal `finish_reason`. A
/// `finish_reason` is a valid stream terminator even when the upstream
/// never sends `data: [DONE]`.
const fn choice_carries_finish_reason(choice: &routectl_core::schema::ChunkChoice) -> bool {
    choice.finish_reason.is_some()
}

/// True when this choice carries a non-empty `tool_calls` delta. While
/// these deltas are arriving the assistant is streaming tool-call
/// arguments; a stream that ends here without a terminal signal for this
/// choice leaves those arguments half-formed.
fn choice_carries_tool_call_delta(choice: &routectl_core::schema::ChunkChoice) -> bool {
    choice
        .delta
        .tool_calls
        .as_ref()
        .is_some_and(|t| !t.is_empty())
}

/// Classification of a stream that exhausted its event source WITHOUT a
/// `data: [DONE]` terminator. The `[DONE]` path returns before the tail
/// runs, so this only ever sees the no-`[DONE]` case; it decides whether
/// a `finish_reason` still terminated the stream cleanly or the upstream
/// closed mid-response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTail {
    /// A `finish_reason` terminal signal was seen; nothing extra to emit.
    Terminated,
    /// No terminal signal, and no tool-call arguments were in flight. A
    /// truncated text response: surface a loud warn (parity with the
    /// gemini egress) but do not error the stream.
    TruncatedText,
    /// No terminal signal while tool-call arguments were still
    /// accumulating. The dangerous case: half-streamed arguments must not
    /// be committed as complete, so the stream is errored.
    TruncatedToolCall,
}

/// Per-choice terminal-signal state accumulated across a stream, keyed by
/// `choice.index` (the vec index). Tracks whether a choice saw its own
/// terminal `finish_reason` and whether it was streaming tool-call
/// argument deltas.
#[derive(Debug, Default, Clone, Copy)]
struct ChoiceTail {
    saw_terminal: bool,
    saw_tool_call_delta: bool,
}

/// Classify a no-`[DONE]` stream tail from PER-CHOICE terminal state. The
/// dangerous case dominates: if ANY choice was streaming tool-call
/// arguments and never got its own terminal `finish_reason`, the tail is
/// `TruncatedToolCall` even when a sibling choice finished cleanly -- the
/// `n > 1` masking bug. Otherwise any observed choice (or an empty stream)
/// that never saw a terminal signal is a possibly-truncated text response,
/// and a stream where every observed choice terminated is clean.
fn classify_stream_tail(tails: &[ChoiceTail]) -> StreamTail {
    if tails
        .iter()
        .any(|t| t.saw_tool_call_delta && !t.saw_terminal)
    {
        StreamTail::TruncatedToolCall
    } else if tails.is_empty() || tails.iter().any(|t| !t.saw_terminal) {
        StreamTail::TruncatedText
    } else {
        StreamTail::Terminated
    }
}

/// Upper bound on the per-choice stream accumulator index. A request's
/// legitimate max index is n-1 where n (the `n` sampling param) is small;
/// 128 is far above any real fan-out. An out-of-range `choice.index`
/// from upstream would otherwise force an oversized Vec allocation via
/// `resize`. Out-of-range indices are skipped.
const MAX_STREAM_CHOICES: usize = 128;

/// Grow the per-choice accumulator and append text for the given choice
/// index. The vec is lazily expanded so the common n=1 case allocates
/// only one entry. Indices at or above `MAX_STREAM_CHOICES` are dropped
/// to bound the allocation against a hostile upstream.
fn accumulate_choice_text(buffers: &mut Vec<String>, index: u32, text: &str) {
    let idx = index as usize;
    if idx >= MAX_STREAM_CHOICES {
        return;
    }
    if buffers.len() <= idx {
        buffers.resize(idx + 1, String::new());
    }
    buffers[idx].push_str(text);
}

/// Grow the per-choice terminal-state vec to cover `index` and return a
/// mutable handle, or `None` when the index is at/above the hostile-upstream
/// cap (mirrors `accumulate_choice_text`). Lazy expansion keeps the common
/// n=1 case at a single entry.
fn choice_tail_mut(tails: &mut Vec<ChoiceTail>, index: u32) -> Option<&mut ChoiceTail> {
    let idx = index as usize;
    if idx >= MAX_STREAM_CHOICES {
        return None;
    }
    if tails.len() <= idx {
        tails.resize_with(idx + 1, ChoiceTail::default);
    }
    Some(&mut tails[idx])
}

/// Ensure `body.stream_options.include_usage = true` on outgoing
/// streaming requests so the upstream emits a terminal usage chunk +
/// finish_reason. Precedence rules:
///
///   1. If `disabled == true`, return early (operator opt-out).
///   2. If `stream_options.include_usage` is already present (operator
///      supplied via `default_extras` / `provider_extras`), leave it
///      verbatim -- even an explicit `false` wins over the auto-inject.
///   3. Otherwise, create the `stream_options` object if missing and
///      set `include_usage = true`.
///
/// Body MUST be a JSON object (caller is the openai-compat egress; its
/// `normalize_request` always returns an object). Non-object bodies are
/// no-ops for safety.
/// Auto-inject `stream_options.include_usage = true` on streaming
/// request bodies so openai-compat hosts emit the terminal
/// `finish_reason` + usage object that routectl's stream summary
/// (and downstream token accounting) needs. Order-dependent: caller
/// MUST run `normalize_request` first so operator-supplied
/// `stream_options` from `default_extras` / `provider_extras` are
/// already in `body` when this helper runs. The helper preserves any
/// existing value (including explicit `false`) and short-circuits
/// when `disabled` is `true`.
///
/// Private to the module: the single caller is `stream()` below.
/// Promoting to `pub(crate)` would expose an order-dependent contract
/// the type system cannot enforce.
fn ensure_stream_options_include_usage(body: &mut Value, disabled: bool) {
    if disabled {
        return;
    }
    if body.pointer("/stream_options/include_usage").is_some() {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let entry = obj
        .entry("stream_options".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(so) = entry.as_object_mut() {
        so.insert("include_usage".to_string(), Value::Bool(true));
    }
}

/// Best-effort parse of the OpenAI-shape error envelope
/// (`{"error":{"type":...,"code":...}}`) to lift the upstream
/// classifier. Returns `(upstream_type, upstream_code)`; either is
/// `None` when absent or the body is not JSON. `code` is stringified
/// because the OpenAI wire admits both a string code
/// (`"context_length_exceeded"`) and a numeric one. The full body is
/// NOT logged here -- the caller's debug/warn lines already cover that.
fn parse_openai_error_classifier(body_text: &str) -> (Option<String>, Option<String>) {
    serde_json::from_str::<Value>(body_text).map_or((None, None), |v| classify_error_value(&v))
}

/// Lift the OpenAI-shape error classifier (`error.type` / `error.code`)
/// from an already-parsed value. `code` is stringified because the wire
/// admits both a string code (`"context_length_exceeded"`) and a numeric
/// one. Single source of truth shared by `parse_openai_error_classifier`
/// (the non-2xx mapper), the 2xx inline-error detection, and the streaming
/// mid-frame detection so all three surface the same classifier tokens.
pub(super) fn classify_error_value(v: &Value) -> (Option<String>, Option<String>) {
    let upstream_type = v
        .pointer("/error/type")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let upstream_code = v.pointer("/error/code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    (upstream_type, upstream_code)
}

/// Map a non-success openai-compat HTTP response into a canonical
/// `Error::Upstream`. Single source of truth shared by both
/// `complete()` and `stream()`: computes the rate-limit-gated
/// `retry_after`, lifts the upstream `error.type` / `error.code`
/// classifier, sanitizes the message for the error body, and folds the
/// 401/403-vs-else WARN split. The full upstream body is emitted once at
/// debug level here so call sites do not duplicate it.
///
/// `headers` MUST be read from the response BEFORE the body is consumed
/// (the capped body read moves the response). This ordering is a programmer
/// convention, NOT enforced by the borrow checker: `headers` is an owned
/// `HeaderMap` clone here, so the compiler does not couple it to the body
/// move. Both call sites clone the headers before reading the body.
///
/// `hit_cap` is true when the body was truncated at the shared response-body
/// cap. A truncated body is untrustworthy: the client message collapses to a
/// fixed cap-exceeded string (never an echo of the partial body) while the
/// original status and rate-limit-gated `retry_after` are preserved. The
/// classifier is still attempted on the prefix (a partial envelope yields
/// None), and the upstream-failure WARN still fires -- with the fixed cap
/// message as its excerpt, never prefix-derived text.
fn map_openai_compat_upstream_error(
    provider_id: &str,
    status: u16,
    headers: &HeaderMap,
    body_text: &str,
    hit_cap: bool,
) -> Error {
    // Reset hint from response headers, gated on rate-limit statuses so a
    // stray Retry-After on a 400 doesn't park the provider.
    let retry_after = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    // A first-party OpenAI error is a nested `{"error":{...}}` envelope; a
    // mantle (Bedrock) upstream returns a flat body instead (`{"__type":...}`
    // or a bare `{"message":...}` -- the AWS exception type often arrives
    // ONLY in the `x-amzn-errortype` header, not the body). The native
    // envelope WINS: its `error.type` / `error.code` classifier is lifted and
    // its raw body is carried unchanged. Any NON-envelope body is treated as
    // a potential AWS body: its classifier tokens are lifted best-effort and,
    // crucially, its free text routes through the shared Bedrock scrub. This
    // gates the scrub on shape (not on a lifted token) so a 403 AccessDenied
    // body cannot leak the principal ARN / account / resource even when AWS
    // omits the top-level `__type` / `code` tokens.
    let is_native_envelope = is_json_error_envelope(body_text);
    let (upstream_type, upstream_code) = if is_native_envelope {
        parse_openai_error_classifier(body_text)
    } else {
        let parsed = serde_json::from_str::<Value>(body_text).ok();
        crate::aws_error::lift_aws_error_tokens(parsed.as_ref())
    };
    // A non-envelope body routes its free text through the shared Bedrock
    // scrub: a 403 collapses to the IAM action only, while every other status
    // yields the same sanitized-and-capped excerpt `extract_upstream_message`
    // already produces for a body with no `/error/message`, so non-403
    // non-envelope bodies stay byte-unchanged. A native envelope keeps its
    // first-party `/error/message` verbatim.
    let client_msg = if is_native_envelope {
        extract_upstream_message(body_text)
    } else {
        crate::aws_error::classify_client_error_message(status, body_text)
    };
    // The upstream-failure WARN fires on every 4xx/5xx -- cap trips included,
    // so an operator sees the same failure signal regardless. On a cap trip
    // its excerpt is the fixed cap message: the truncated prefix never appears
    // at WARN level. openai-compat carries no AuthKind, so the auth_kind field
    // is omitted.
    let warn_excerpt = if hit_cap {
        crate::http_client::body_cap_exceeded_message()
    } else {
        sanitize_for_log(&client_msg)
    };
    crate::upstream_log::warn_upstream_failure(
        provider_id,
        status,
        None,
        &warn_excerpt,
        "openai-compat",
    );
    // Full (capped) upstream body at debug level -- the only path where the
    // truncated prefix bytes may surface, DEBUG-gated and bounded. A
    // non-envelope 403 body collapses to the IAM action only so no ARN /
    // account / resource identifier reaches the DEBUG log; native envelopes
    // and non-403 bodies log unchanged.
    let debug_body = if is_native_envelope {
        body_text.to_string()
    } else {
        crate::aws_error::sanitized_debug_body(status, body_text)
    };
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, &debug_body);
    // Client body: on a cap trip the truncated body is untrustworthy, so
    // collapse to the fixed cap message (never an echo of the partial body)
    // while preserving the status + retry_after. A native `{error:...}`
    // envelope is carried RAW so the ingress sanitizer can re-extract the
    // upstream message. A non-envelope body carries the already-scrubbed
    // message -- a raw AWS envelope naming principal identifiers is never
    // echoed.
    let err_body = if hit_cap {
        crate::http_client::body_cap_exceeded_message()
    } else if is_native_envelope {
        body_text.to_string()
    } else {
        client_msg
    };
    Error::upstream_full(
        provider_id,
        status,
        err_body,
        retry_after,
        upstream_type,
        upstream_code,
    )
    .with_upstream_request_id(crate::upstream_request_id::parse_upstream_request_id(
        headers,
    ))
}

/// Detect a top-level `{"error":{...}}` envelope returned inside a
/// non-error (2xx) NON-streaming HTTP body -- the OpenRouter-style inline
/// error some hosts return with a 200 status instead of a 4xx/5xx.
/// Mirrors the streaming path's mid-stream error-frame detection and
/// routes a detected envelope through the SAME canonical mapper the
/// non-2xx status path uses, so the surfaced `Error::Upstream` carries an
/// identical status / type / code / message treatment. Returns `None` for
/// a well-formed body (no `error` key, or a null / empty `error` sentinel
/// that LiteLLM and some proxies attach to healthy responses) so a normal
/// 2xx response with `choices` is unaffected.
///
/// The HTTP status is derived from a numeric `error.code` when present (an
/// inline error carries no failing HTTP status of its own), defaulting to
/// 502 -- the same rule the streaming mid-frame detection applies.
fn detect_inline_error_2xx(
    provider_id: &str,
    raw: &Value,
    headers: &HeaderMap,
    body_text: &str,
) -> Option<Error> {
    let err = raw.get("error")?;
    if err.is_null() || err.as_object().is_some_and(serde_json::Map::is_empty) {
        return None;
    }
    let status = raw
        .pointer("/error/code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(502);
    Some(map_openai_compat_upstream_error(
        provider_id,
        status,
        headers,
        body_text,
        false,
    ))
}

/// Read an openai-compat upstream error body under the shared response-body
/// cap. `headers` are cloned BEFORE the read consumes `resp` (the mapper
/// needs them for the rate-limit-gated `retry_after`). On a cap trip a single
/// WARN records the truncation and the returned prefix is bounded to the cap;
/// a transport failure is logged and degrades to an empty body.
async fn read_capped_error_body(
    provider_id: &str,
    status: u16,
    resp: reqwest::Response,
) -> (HeaderMap, String, bool) {
    let headers = resp.headers().clone();
    let content_length = resp.content_length();
    let (bytes, hit_cap) = match crate::http_client::read_body_capped(
        resp,
        crate::http_client::MAX_RESPONSE_BODY_BYTES,
    )
    .await
    {
        Ok(read) => read,
        Err(e) => {
            // A transport failure reading the error body is not a cap trip;
            // surface it so the error path is not silently blind, then fall
            // back to an empty body (the mapper still returns a status-only
            // client error).
            tracing::warn!(
                provider = %provider_id,
                status,
                error = %e,
                "failed to read upstream error body",
            );
            (Vec::new(), false)
        }
    };
    if hit_cap {
        crate::http_client::warn_body_cap(provider_id, status, content_length, "error_body");
    }
    (
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
        hit_cap,
    )
}

#[cfg(test)]
mod helper_tests {
    use super::{
        ChoiceTail, MAX_STREAM_CHOICES, StreamTail, accumulate_choice_text,
        choice_carries_finish_reason, choice_carries_tool_call_delta, choice_tail_mut,
        classify_stream_tail, detect_inline_error_2xx, ensure_stream_options_include_usage,
        map_openai_compat_upstream_error, parse_openai_error_classifier,
    };
    use crate::http_client::body_cap_exceeded_message;
    use reqwest::header::HeaderMap;
    use routectl_core::Error;
    use routectl_core::schema::{ChunkChoice, ChunkDelta};
    use serde_json::json;

    /// A well-formed stream tail (every choice saw its own terminal
    /// `finish_reason`) classifies as `Terminated` and emits nothing extra,
    /// even when tool-call deltas were also present earlier.
    #[test]
    fn terminal_finish_reason_classifies_as_terminated() {
        let terminated = [ChoiceTail {
            saw_terminal: true,
            saw_tool_call_delta: false,
        }];
        assert_eq!(classify_stream_tail(&terminated), StreamTail::Terminated);

        let terminated_with_tc = [ChoiceTail {
            saw_terminal: true,
            saw_tool_call_delta: true,
        }];
        assert_eq!(
            classify_stream_tail(&terminated_with_tc),
            StreamTail::Terminated,
            "a finish_reason terminates a choice cleanly regardless of tool-call activity"
        );
    }

    /// A choice that ends with NO terminal signal while its tool-call
    /// arguments were still accumulating is the dangerous case: classified
    /// as `TruncatedToolCall` so the stream is errored.
    #[test]
    fn no_terminal_signal_mid_tool_call_classifies_as_truncated_tool_call() {
        let tails = [ChoiceTail {
            saw_terminal: false,
            saw_tool_call_delta: true,
        }];
        assert_eq!(classify_stream_tail(&tails), StreamTail::TruncatedToolCall);
    }

    /// A choice that ends with no terminal signal and no tool-call activity
    /// (and the empty-stream case) is a truncated text response: a loud
    /// warn, not an error.
    #[test]
    fn no_terminal_signal_plain_text_classifies_as_truncated_text() {
        let tails = [ChoiceTail {
            saw_terminal: false,
            saw_tool_call_delta: false,
        }];
        assert_eq!(classify_stream_tail(&tails), StreamTail::TruncatedText);
        assert_eq!(
            classify_stream_tail(&[]),
            StreamTail::TruncatedText,
            "a stream that FINs with no observed choice is treated as truncated text"
        );
    }

    /// The `n > 1` regression: choice 0 finishes cleanly while choice 1
    /// streams tool-call args and never gets its own `finish_reason`. The
    /// per-choice tail must classify the tail as a truncated tool call even
    /// though a sibling terminated -- the exact bug global tracking masked.
    #[test]
    fn sibling_choice_truncation_detected_with_n_gt_1() {
        // Arrange: drive the same per-choice marking the stream loop uses.
        let finish0 = ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        };
        let toolcall1 = ChunkChoice {
            index: 1,
            delta: ChunkDelta {
                tool_calls: Some(vec![
                    json!({"index": 0, "function": {"arguments": "{\"pa"}}),
                ]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        };
        let mut tails: Vec<ChoiceTail> = Vec::new();

        // Act
        for choice in [&finish0, &toolcall1] {
            if choice_carries_finish_reason(choice)
                && let Some(t) = choice_tail_mut(&mut tails, choice.index)
            {
                t.saw_terminal = true;
            }
            if choice_carries_tool_call_delta(choice)
                && let Some(t) = choice_tail_mut(&mut tails, choice.index)
            {
                t.saw_tool_call_delta = true;
            }
        }

        // Assert
        assert_eq!(
            classify_stream_tail(&tails),
            StreamTail::TruncatedToolCall,
            "a truncated sibling choice must not be masked by a finished sibling"
        );
    }

    /// The finish-reason detector fires on a choice carrying a terminal
    /// reason and is silent otherwise.
    #[test]
    fn choice_carries_finish_reason_detects_terminal_choice() {
        let terminal = ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        };
        assert!(choice_carries_finish_reason(&terminal));

        let open = ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("hi".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        };
        assert!(!choice_carries_finish_reason(&open));
    }

    /// The tool-call detector fires on a non-empty `tool_calls` delta and
    /// stays silent for an absent or empty one.
    #[test]
    fn choice_carries_tool_call_delta_detects_accumulating_args() {
        let with_tc = ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![json!({
                    "index": 0,
                    "function": {"arguments": "{\"pa"}
                })]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        };
        assert!(choice_carries_tool_call_delta(&with_tc));

        let empty_tc = ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        };
        assert!(!choice_carries_tool_call_delta(&empty_tc));

        let no_tc = ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("text".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        };
        assert!(!choice_carries_tool_call_delta(&no_tc));
    }

    /// End-to-end over the accumulator bits for a single choice: a
    /// well-formed tool-call stream (a terminal finish_reason arrived)
    /// passes unchanged, while a stream whose tool-call args were still
    /// accumulating at FIN is flagged as a truncated tool call.
    #[test]
    fn tool_call_stream_truncation_distinguished_by_terminal_signal() {
        // Well-formed: a choice with a tool-call delta AND its finish_reason.
        let terminated = [ChoiceTail {
            saw_terminal: true,
            saw_tool_call_delta: true,
        }];
        assert_eq!(
            classify_stream_tail(&terminated),
            StreamTail::Terminated,
            "a tool-call stream with a terminal finish_reason must pass unchanged"
        );

        // Truncated: only the tool-call delta arrived, no finish_reason.
        let truncated = [ChoiceTail {
            saw_terminal: false,
            saw_tool_call_delta: true,
        }];
        assert_eq!(
            classify_stream_tail(&truncated),
            StreamTail::TruncatedToolCall,
            "a tool-call stream that FINs with no terminal signal must be flagged truncated"
        );
    }

    #[test]
    fn capped_error_body_preserves_status_without_echo() {
        // A cap trip whose prefix is an INCOMPLETE JSON envelope: the client
        // message collapses to the fixed cap message (never echoing the
        // prefix), status is preserved, and the classifier attempt over the
        // partial envelope fails to parse -> None (the attempt still runs;
        // see `capped_error_body_lifts_classifier_when_prefix_parses`).
        let prefix = r#"{"error":{"type":"server_error","message":"aaaaaaaaaaaa"#;
        let err = map_openai_compat_upstream_error("prov", 500, &HeaderMap::new(), prefix, true);
        match err {
            Error::Upstream {
                status,
                body,
                upstream_type,
                upstream_code,
                ..
            } => {
                assert_eq!(
                    status, 500,
                    "capped error must preserve the upstream status"
                );
                assert_eq!(body, body_cap_exceeded_message());
                assert!(
                    !body.contains("aaaa") && !body.contains("server_error"),
                    "capped message must not echo the truncated prefix: {body:?}"
                );
                assert_eq!(
                    upstream_type, None,
                    "an incomplete envelope prefix fails to parse -> no classifier"
                );
                assert_eq!(upstream_code, None);
            }
            other => panic!("expected Upstream, got: {other:?}"),
        }
    }

    #[test]
    fn capped_error_body_lifts_classifier_when_prefix_parses() {
        // A cap trip whose prefix happens to be a COMPLETE small envelope
        // (the cap landed after the classifier fields, before trailing
        // padding). The classifier IS attempted on the prefix and lifts the
        // enum tokens, while the client body stays the fixed cap message and
        // NEVER echoes the prefix message text.
        let prefix = r#"{"error":{"type":"rate_limit_exceeded","code":"slow_down","message":"x"}}"#;
        let err = map_openai_compat_upstream_error("prov", 429, &HeaderMap::new(), prefix, true);
        match err {
            Error::Upstream {
                status,
                body,
                upstream_type,
                upstream_code,
                ..
            } => {
                assert_eq!(
                    status, 429,
                    "capped error must preserve the upstream status"
                );
                assert_eq!(
                    body,
                    body_cap_exceeded_message(),
                    "client body must be the fixed cap message, never the prefix"
                );
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("rate_limit_exceeded"),
                    "classifier type must lift from a prefix that parses"
                );
                assert_eq!(upstream_code.as_deref(), Some("slow_down"));
            }
            other => panic!("expected Upstream, got: {other:?}"),
        }
    }

    #[test]
    fn capped_error_warn_excerpt_is_fixed_message() {
        // The upstream-failure WARN fires on a cap trip (it was previously
        // skipped), but its excerpt is the fixed cap message -- the truncated
        // prefix must never appear at WARN level.
        let prefix = r#"{"error":{"type":"server_error","message":"secret upstream detail"#;
        let events = routectl_testkit::capture_events(|| {
            let _ = super::map_openai_compat_upstream_error(
                "prov",
                500,
                &HeaderMap::new(),
                prefix,
                true,
            );
        });
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN && e.field("context") == Some("openai-compat")
            })
            .expect("upstream-failure WARN must fire on a cap trip");
        assert_eq!(
            warn.field("body_excerpt"),
            Some(body_cap_exceeded_message().as_str()),
            "WARN excerpt must be the fixed cap message on a cap trip"
        );
        assert!(
            events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN)
                .all(|e| e
                    .fields
                    .iter()
                    .all(|(_, v)| !v.contains("secret upstream detail"))),
            "no WARN-level event may echo the truncated prefix"
        );
    }

    #[test]
    fn excerpt_sanitizes_crlf_and_ansi() {
        let body = "boom\r\n[fake INFO] injected\x1b[31mred";
        let sanitized = routectl_core::extract_upstream_message(body);
        let safe_excerpt = routectl_core::sanitize_for_log(&sanitized);
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
    fn injects_when_absent_and_not_disabled() {
        let mut body = json!({"model": "x", "stream": true});
        ensure_stream_options_include_usage(&mut body, false);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn opt_out_suppresses_injection() {
        let mut body = json!({"model": "x", "stream": true});
        ensure_stream_options_include_usage(&mut body, true);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn preserves_operator_supplied_true() {
        let mut body = json!({"stream_options": {"include_usage": true, "other": 1}});
        ensure_stream_options_include_usage(&mut body, false);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // Existing sibling fields untouched.
        assert_eq!(body["stream_options"]["other"], 1);
    }

    #[test]
    fn preserves_operator_supplied_false() {
        // Operator opted out via extras -- explicit `false` must win
        // over the auto-inject, not silently flip to true.
        let mut body = json!({"stream_options": {"include_usage": false}});
        ensure_stream_options_include_usage(&mut body, false);
        assert_eq!(body["stream_options"]["include_usage"], false);
    }

    #[test]
    fn creates_stream_options_object_when_other_keys_present_outside() {
        let mut body = json!({"stream_options": {"unrelated_key": "v"}});
        ensure_stream_options_include_usage(&mut body, false);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream_options"]["unrelated_key"], "v");
    }

    #[test]
    fn parse_classifier_lifts_string_type_and_code() {
        // Arrange
        let body = r#"{"error":{"type":"rate_limit_exceeded","code":"rate_limited","message":"slow down"}}"#;

        // Act
        let (upstream_type, upstream_code) = parse_openai_error_classifier(body);

        // Assert
        assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(upstream_code.as_deref(), Some("rate_limited"));
    }

    #[test]
    fn parse_classifier_stringifies_numeric_code() {
        // Arrange
        let body = r#"{"error":{"type":"invalid_request_error","code":429}}"#;

        // Act
        let (upstream_type, upstream_code) = parse_openai_error_classifier(body);

        // Assert
        assert_eq!(upstream_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(upstream_code.as_deref(), Some("429"));
    }

    #[test]
    fn parse_classifier_returns_none_on_non_json_body() {
        // Arrange
        let body = "503 Service Unavailable (plain text from a proxy)";

        // Act
        let (upstream_type, upstream_code) = parse_openai_error_classifier(body);

        // Assert
        assert!(upstream_type.is_none());
        assert!(upstream_code.is_none());
    }

    /// The shared mapper lifts the upstream classifier
    /// (`error.type` / `error.code`) onto the canonical error. Proves
    /// stream()/complete() parity: before the fix the stream() path used
    /// `upstream_with_retry_after`, which dropped both classifier fields
    /// to None. Driving a 429 rate-limit body through the helper must now
    /// surface the upstream type and code regardless of caller.
    #[test]
    fn map_upstream_error_lifts_classifier_for_both_callers() {
        // Arrange: a rate-limit error envelope at status 429.
        let body = r#"{"error":{"type":"rate_limit_exceeded","code":"slow_down","message":"too many requests"}}"#;
        let headers = HeaderMap::new();

        // Act
        let err = map_openai_compat_upstream_error("p", 429, &headers, body, false);

        // Assert
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(upstream_code.as_deref(), Some("slow_down"));
                assert!(
                    body.contains("too many requests"),
                    "sanitized message must reach the error body, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// retry_after preservation: a parseable `Retry-After` on a
    /// rate-limit status must reach the canonical error from the shared
    /// helper (the stream() path previously preserved it via
    /// `upstream_with_retry_after`; the new helper keeps that for both
    /// callers).
    #[test]
    fn map_upstream_error_preserves_retry_after_on_rate_limit() {
        // Arrange
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());

        // Act
        let err = map_openai_compat_upstream_error("p", 429, &headers, "{}", false);

        // Assert
        match err {
            Error::Upstream { retry_after, .. } => {
                assert_eq!(
                    retry_after,
                    Some(std::time::Duration::from_secs(30)),
                    "retry_after must be preserved for both callers"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A structured `{error:...}` body must be carried RAW in
    /// `Error::Upstream.body` so the ingress sanitizer can re-extract the
    /// upstream's own `error.message` and surface it to the client.
    #[test]
    fn map_upstream_error_carries_raw_envelope_for_structured_body() {
        // Arrange: a JSON error envelope with sibling keys around `error`.
        let body = r#"{"error":{"type":"invalid_request_error","message":"bad model id"},"x_trace":"t-7"}"#;
        let headers = HeaderMap::new();

        // Act
        let err = map_openai_compat_upstream_error("p", 400, &headers, body, false);

        // Assert: the RAW JSON reaches `.body` so ingress can re-parse
        // `/error/message`.
        match err {
            Error::Upstream { body, .. } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body).expect("body must be the raw JSON envelope");
                assert_eq!(
                    parsed.pointer("/error/message").and_then(|v| v.as_str()),
                    Some("bad model id")
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A non-JSON upstream body must NOT be carried raw; the sanitized
    /// excerpt is stored so the ingress sanitizer yields status-only.
    #[test]
    fn map_upstream_error_sanitizes_non_json_body() {
        // Arrange
        let headers = HeaderMap::new();

        // Act
        let err = map_openai_compat_upstream_error(
            "p",
            502,
            &headers,
            "<html>upstream-host gateway timeout</html>",
            false,
        );

        // Assert
        match err {
            Error::Upstream { body, .. } => {
                assert!(
                    !body.contains("upstream-host"),
                    "raw non-JSON body must not be carried in .body: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A mantle 403 carrying a namespaced AWS `__type` (the flat AWS
    /// envelope, no native OpenAI `/error` shape) must lift the bare
    /// exception token into `upstream_type`. 403 already classifies Auth by
    /// status; the lifted token is what makes the log truthful.
    #[test]
    fn map_upstream_error_lifts_aws_signature_token_from_403() {
        let body = r#"{"__type":"com.amazonaws.bedrock#SignatureDoesNotMatch","message":"The request signature we calculated does not match."}"#;
        let err = map_openai_compat_upstream_error("p", 403, &HeaderMap::new(), body, false);
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                assert_eq!(status, 403);
                assert_eq!(upstream_type.as_deref(), Some("SignatureDoesNotMatch"));
                assert_eq!(upstream_code, None);
                // A 403 free-text message collapses to the generic scrub; the
                // raw AWS envelope must never be carried.
                assert_eq!(body, "bedrock access denied");
                assert!(!body.contains("__type"));
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A mantle 429 carrying a bare AWS `code` token must lift it into
    /// `upstream_code` so the rate-limit failure logs truthfully.
    #[test]
    fn map_upstream_error_lifts_aws_throttling_code_from_429() {
        let body = r#"{"code":"ThrottlingException","Message":"Too many requests"}"#;
        let err = map_openai_compat_upstream_error("p", 429, &HeaderMap::new(), body, false);
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(upstream_type, None);
                assert_eq!(upstream_code.as_deref(), Some("ThrottlingException"));
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A real AWS 403 AccessDenied body names the principal ARN, account id,
    /// and resource ARN. The client body, the WARN excerpt, and the DEBUG
    /// line must surface ONLY the IAM action -- never the principal / account
    /// / resource identifiers.
    #[test]
    fn map_upstream_error_403_scrubs_aws_access_denied_arn() {
        let body = r#"{"__type":"com.amazonaws.bedrock#AccessDeniedException","message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock-runtime:InvokeModel on resource: arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5"}"#;
        let err = map_openai_compat_upstream_error("p", 403, &HeaderMap::new(), body, false);
        match err {
            Error::Upstream {
                upstream_type,
                body,
                ..
            } => {
                assert_eq!(upstream_type.as_deref(), Some("AccessDeniedException"));
                assert_eq!(
                    body,
                    "bedrock access denied: missing IAM action bedrock-runtime:InvokeModel"
                );
                assert!(!body.contains("arn:aws:"), "leaked ARN: {body}");
                assert!(!body.contains("123456789012"), "leaked account id: {body}");
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A mantle 403 whose body carries the ARN-laden AccessDenied message but
    /// NO top-level `__type` / `code` token (the AWS exception type arrives
    /// only in the `x-amzn-errortype` header) must STILL be scrubbed on every
    /// surface. The scrub is gated on the non-envelope shape, not on a lifted
    /// token, so this leak class cannot slip through.
    #[test]
    fn map_upstream_error_403_scrubs_aws_body_without_type_token() {
        let body = r#"{"message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock-runtime:InvokeModel on resource: arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5"}"#;
        let events = routectl_testkit::capture_events(|| {
            let err = map_openai_compat_upstream_error("p", 403, &HeaderMap::new(), body, false);
            match err {
                Error::Upstream { body, .. } => {
                    assert_eq!(
                        body,
                        "bedrock access denied: missing IAM action bedrock-runtime:InvokeModel"
                    );
                    assert!(!body.contains("arn:aws:"), "client body leaked ARN: {body}");
                    assert!(
                        !body.contains("123456789012"),
                        "client body leaked account id: {body}"
                    );
                }
                other => panic!("expected Error::Upstream, got: {other:?}"),
            }
        });
        // No log event at any level may echo the principal ARN or account id.
        assert!(
            events.iter().all(|e| e
                .fields
                .iter()
                .all(|(_, v)| !v.contains("arn:aws:") && !v.contains("123456789012"))),
            "a log event leaked an ARN / account id"
        );
    }

    /// The native OpenAI `/error` shape WINS over any sibling top-level AWS
    /// key: a body carrying both keeps the OpenAI classifier and carries the
    /// raw envelope, never routing through the AWS scrub.
    #[test]
    fn map_upstream_error_openai_shape_wins_over_aws() {
        let body = r#"{"error":{"type":"rate_limit_exceeded","code":"slow_down","message":"slow down"},"__type":"com.amazonaws.bedrock#ThrottlingException"}"#;
        let err = map_openai_compat_upstream_error("p", 429, &HeaderMap::new(), body, false);
        match err {
            Error::Upstream {
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(upstream_code.as_deref(), Some("slow_down"));
                // Native shape -> raw envelope carried (not the AWS scrub).
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(
                    parsed.pointer("/error/type").and_then(|v| v.as_str()),
                    Some("rate_limit_exceeded")
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// Arbitrary, non-JSON, empty, and oversized bodies must never panic and
    /// must degrade to a canonical `Error::Upstream` with no lifted tokens.
    #[test]
    fn map_upstream_error_never_panics_on_malformed_bodies() {
        let huge = "x".repeat(crate::http_client::MAX_RESPONSE_BODY_BYTES * 2);
        let cases: [&str; 5] = [
            "",
            "not json at all",
            r#"{"random":[1,2,3],"nested":{"deep":true}}"#,
            r#"{"__type":42,"code":{"not":"a string"}}"#,
            &huge,
        ];
        for body in cases {
            let err = map_openai_compat_upstream_error("p", 400, &HeaderMap::new(), body, false);
            match err {
                Error::Upstream {
                    upstream_type,
                    upstream_code,
                    ..
                } => {
                    assert_eq!(upstream_type, None);
                    assert_eq!(upstream_code, None);
                }
                other => panic!("expected Error::Upstream, got: {other:?}"),
            }
        }
    }

    /// A 200-OK non-streaming body carrying a top-level `{"error":{...}}`
    /// envelope (OpenRouter-style inline error) must surface as an
    /// `Error::Upstream` with the status / type / code / message the
    /// non-2xx path would produce -- not flow into normalize.
    #[test]
    fn inline_error_on_2xx_body_surfaces_as_upstream() {
        // Arrange: a 200-OK body whose only top-level content is an error
        // envelope with a string code.
        let body = r#"{"error":{"type":"rate_limit_exceeded","code":"slow_down","message":"upstream is rate limiting"}}"#;
        let raw: serde_json::Value = serde_json::from_str(body).unwrap();

        // Act
        let err = detect_inline_error_2xx("p", &raw, &HeaderMap::new(), body)
            .expect("inline error envelope must be detected");

        // Assert
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                // A string code carries no numeric HTTP status -> 502 default.
                assert_eq!(status, 502);
                assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(upstream_code.as_deref(), Some("slow_down"));
                assert!(
                    body.contains("upstream is rate limiting"),
                    "the raw envelope (with the upstream message) must reach .body, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A numeric `error.code` on the inline 2xx envelope is treated as the
    /// HTTP status (mirroring the streaming mid-frame rule), so the
    /// surfaced status is preserved rather than collapsed to 502.
    #[test]
    fn inline_error_on_2xx_body_derives_status_from_numeric_code() {
        let body = r#"{"error":{"type":"server_error","code":503,"message":"overloaded"}}"#;
        let raw: serde_json::Value = serde_json::from_str(body).unwrap();

        let err = detect_inline_error_2xx("p", &raw, &HeaderMap::new(), body)
            .expect("inline error envelope must be detected");

        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A well-formed 200-OK response carrying `choices` (and no top-level
    /// `error`, or only a null / empty `error` sentinel) is unaffected:
    /// detection returns `None` so the body flows into normalize.
    #[test]
    fn well_formed_2xx_body_is_not_flagged_as_inline_error() {
        // A normal completion body.
        let ok = serde_json::json!({
            "id": "cmpl-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}]
        });
        assert!(detect_inline_error_2xx("p", &ok, &HeaderMap::new(), "").is_none());

        // A healthy body carrying a null `error` sentinel (LiteLLM shape).
        let null_err = serde_json::json!({
            "choices": [{"index": 0, "message": {"content": "hi"}}],
            "error": null
        });
        assert!(detect_inline_error_2xx("p", &null_err, &HeaderMap::new(), "").is_none());

        // A healthy body carrying an empty `error: {}` sentinel.
        let empty_err = serde_json::json!({
            "choices": [{"index": 0, "message": {"content": "hi"}}],
            "error": {}
        });
        assert!(detect_inline_error_2xx("p", &empty_err, &HeaderMap::new(), "").is_none());
    }

    #[test]
    fn accumulate_grows_buffer_for_in_range_index() {
        // Arrange
        let mut buffers: Vec<String> = Vec::new();

        // Act
        accumulate_choice_text(&mut buffers, 0, "hello ");
        accumulate_choice_text(&mut buffers, 0, "world");
        accumulate_choice_text(&mut buffers, 2, "third");

        // Assert: index 2 lazily grows the vec to length 3; text appends.
        assert_eq!(buffers.len(), 3);
        assert_eq!(buffers[0], "hello world");
        assert_eq!(buffers[1], "");
        assert_eq!(buffers[2], "third");
    }

    #[test]
    fn accumulate_skips_out_of_range_index_without_over_allocating() {
        // An out-of-range choice.index from upstream: the accumulator
        // must neither panic nor resize the buffer to an oversized
        // length -- it drops the write and stays bounded.
        let mut buffers: Vec<String> = Vec::new();

        accumulate_choice_text(&mut buffers, 10_000, "evil");

        assert!(
            buffers.is_empty(),
            "out-of-range index must not allocate any entries, got len {}",
            buffers.len()
        );
    }

    #[test]
    fn accumulate_admits_highest_in_range_index_and_rejects_the_cap() {
        // Boundary: index == MAX_STREAM_CHOICES - 1 is the last admitted
        // index; index == MAX_STREAM_CHOICES is the first rejected one.
        let mut buffers: Vec<String> = Vec::new();

        accumulate_choice_text(&mut buffers, (MAX_STREAM_CHOICES - 1) as u32, "edge");
        assert_eq!(buffers.len(), MAX_STREAM_CHOICES);
        assert_eq!(buffers[MAX_STREAM_CHOICES - 1], "edge");

        // The cap index itself is dropped; the buffer does not grow.
        accumulate_choice_text(&mut buffers, MAX_STREAM_CHOICES as u32, "over");
        assert_eq!(
            buffers.len(),
            MAX_STREAM_CHOICES,
            "index == cap must be rejected, buffer must not grow"
        );
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod mantle_header_tests {
    use super::{HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect};
    use crate::bedrock::auth::ResolvedCreds;
    use crate::mantle::MantleAuth;
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
    use routectl_core::ChatRequest;

    fn mantle_provider() -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(OpenAiCompatConfig {
            id: "mantle-compat".into(),
            base_url: "https://bedrock-mantle.us-west-2.api.aws/openai/v1".into(),
            // Empty by config validation on the mantle lane; the signer
            // owns Authorization.
            api_key: String::new(),
            header_extras: vec![],
            payload_extras: None,
            reasoning_dialect: ReasoningDialect::OpenAi,
            history_reasoning: HistoryReasoning::Auto,
            user_agent: None,
            strict_translation: false,
            disable_stream_include_usage: false,
            mantle: Some(MantleAuth {
                region: "us-west-2".into(),
                creds: ResolvedCreds::Bearer {
                    key: "mantle-bearer-key".into(),
                },
            }),
        })
    }

    /// build_headers on the mantle lane must NOT insert the first-party
    /// `Authorization: Bearer <api_key>` header -- the SigV4/bearer signer
    /// owns auth and attaches it post-build. CONTENT_TYPE still stamps.
    #[test]
    fn build_headers_skips_bearer_on_mantle_lane() {
        let provider = mantle_provider();
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            ..Default::default()
        };

        let headers = provider.build_headers(&req).unwrap();

        assert!(
            headers.get(AUTHORIZATION).is_none(),
            "mantle lane must not stamp a first-party Bearer; the signer owns auth"
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "content-type must stamp on both lanes"
        );
    }
}
