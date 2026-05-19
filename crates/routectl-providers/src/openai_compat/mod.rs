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
pub mod dialects;
pub mod request;
pub mod response;
pub mod sse;
pub(crate) mod util;
pub(crate) mod wire_lift;

pub use dialect::ReasoningDialect;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tracing::debug;

use routectl_core::{
    debug_upstream_error_body, extract_upstream_message, sanitize_for_log, trace_outgoing_body,
    trace_upstream_success_body, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
};

use sse::ThinkTagAccumulator;

/// Provider-kind discriminator string used in tracing fields. Single
/// source of truth so call sites grep clean (`provider_kind=openai-compat`)
/// and a typo-on-rename can't silently break operator log filters.
const PROVIDER_KIND: &str = "openai-compat";

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
    /// How to handle reasoning fields on outgoing assistant messages
    /// in multi-turn history. `Auto` defers to the dialect's default
    /// (DeepSeek/Vllm strip, OpenAI/OpenRouter pass through). `Strip`
    /// and `Preserve` are explicit overrides. See
    /// `HistoryReasoning` docs for the v3-vs-v4 motivation.
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
    /// `default_extras` / `provider_extras` always wins -- including an
    /// explicit `include_usage = false`.
    pub disable_stream_include_usage: bool,
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

pub struct OpenAiCompatProvider {
    cfg: OpenAiCompatConfig,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: OpenAiCompatConfig) -> Self {
        let client = crate::http_client::build(cfg.user_agent.as_deref());
        Self { cfg, client }
    }

    fn completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
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
            // Defense-in-depth, parity with anthropic_api / bedrock: refuse
            // to let TOML-supplied `extra_headers` stomp on the auth header
            // we just set. HeaderMap::insert replaces by name, so without
            // this guard `extra_headers = { "authorization" = "..." }` would
            // silently override the Bearer token.
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
            self.cfg.history_reasoning,
            self.cfg.default_extras.as_ref(),
            self.cfg.strict_translation,
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

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        // Force non-streaming.
        body["stream"] = Value::Bool(false);

        let headers = self.build_headers()?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions");

        // Trace-level outgoing body for triage. Gated by
        // `tracing::Level::TRACE`; default `info` filter pays
        // nothing.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

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
            // Full upstream error body at debug level. The truncated
            // WARN excerpt below stays for warn-log scannability.
            debug_upstream_error_body(PROVIDER_KIND, &self.cfg.id, status, &body_text);
            let sanitized = extract_upstream_message(&body_text);
            // Extend the auth-only WARN to all 4xx/5xx so an operator
            // never has to guess WHY a request failed.
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %sanitized,
                    "openai-compat upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %sanitized,
                    "openai-compat upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, sanitized));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;

        // Trace the raw upstream body BEFORE normalization
        // mutates it (`coalesce_reasoning_content_in_response`
        // rewrites `reasoning_content` -> `reasoning` in place).
        // Operators triaging "what did the upstream return" want
        // the wire form, not the post-processed form.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw);

        let mut chat_resp = self.normalize_response(raw)?;
        // OpenAI-compat hosts don't surface the matched stop sequence
        // on the response wire. Recover it from the request stop list
        // so the Anthropic ingress can render `stop_reason:"stop_sequence"`
        // instead of `end_turn` for claude-code structured-output flows.
        response::apply_stop_sequence_heuristic(&mut chat_resp, req.stop.as_deref());
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
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

        let headers = self.build_headers()?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions (stream)");

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

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
            debug_upstream_error_body(PROVIDER_KIND, &self.cfg.id, status, &body_text);
            let sanitized = extract_upstream_message(&body_text);
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %sanitized,
                    "openai-compat upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %sanitized,
                    "openai-compat upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, sanitized));
        }

        let provider_id = self.cfg.id.clone();
        let dialect = self.cfg.reasoning_dialect;
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
            // Running concatenation of `delta.content` text across chunks.
            // The terminal chunk (the one carrying `finish_reason`) gets
            // the matched_stop_sequence applied just before yield, mirroring
            // what the non-streaming path does after `normalize_response`.
            let mut accumulated_text = String::new();
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
                    Ok(Some(mut chunk)) => {
                        // Accumulate content text from EVERY chunk, including
                        // the terminal one (the chunk that carries
                        // finish_reason). Terminal-chunk text is intentionally
                        // included so the suffix-match heuristic sees the
                        // full body: some openai-compat hosts (and the
                        // RawThinkTag dialect's post-strip emit) put real
                        // content on the same chunk as the terminator.
                        for choice in chunk.choices.iter() {
                            if let Some(t) = choice.delta.content.as_deref() {
                                accumulated_text.push_str(t);
                            }
                        }
                        // Apply the stop-sequence heuristic on any choice
                        // that carries the terminal finish_reason and no
                        // matched_stop_sequence yet. The non-streaming
                        // path runs the same recovery in `complete()`
                        // after `normalize_response`.
                        if let Some(stops) = request_stop.as_deref() {
                            for choice in chunk.choices.iter_mut() {
                                if choice.matched_stop_sequence.is_some() {
                                    continue;
                                }
                                if choice.finish_reason.as_deref() != Some("stop") {
                                    continue;
                                }
                                choice.matched_stop_sequence =
                                    response::detect_matched_stop_sequence(
                                        Some(accumulated_text.as_str()),
                                        stops,
                                    );
                            }
                        }
                        yield Ok(chunk);
                    }
                    Err(e) => yield Err(e),
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
pub(crate) fn ensure_stream_options_include_usage(body: &mut Value, disabled: bool) {
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

#[cfg(test)]
mod helper_tests {
    use super::ensure_stream_options_include_usage;
    use serde_json::json;

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
}
