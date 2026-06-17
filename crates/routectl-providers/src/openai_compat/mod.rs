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
//!     dialects delegate to the stateless `parse_event`.
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
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
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

    /// Resolve the per-request reasoning dialect. v0.6.0 moved the
    /// dialect off `[providers.X]` to `[models.X]`; the router lifts
    /// it onto `req.routectl_internal.reasoning_dialect` before
    /// dispatch. When the carrier is empty (library consumer
    /// constructing `ChatRequest` directly with no router) the
    /// `OpenAiCompatConfig` default applies.
    fn dialect_for(&self, req: &ChatRequest) -> ReasoningDialect {
        req.routectl_internal
            .reasoning_dialect
            .map(ReasoningDialect::from)
            .unwrap_or(self.cfg.reasoning_dialect)
    }

    /// Same fallback contract as `dialect_for` but for the history-
    /// reasoning policy.
    fn history_reasoning_for(&self, req: &ChatRequest) -> HistoryReasoning {
        req.routectl_internal
            .history_reasoning
            .map(HistoryReasoning::from)
            .unwrap_or(self.cfg.history_reasoning)
    }

    fn build_headers(&self, req: &ChatRequest) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))
                .map_err(|e| Error::Config(format!("invalid api_key for header: {e}")))?,
        );
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

    /// Stateless per-chunk normalization. Falls back to the
    /// config-side dialect when called outside an active `stream()`
    /// (no request context available). `stream()` itself captures the
    /// per-request dialect from `req.routectl_internal`.
    fn normalize_chunk(&self, raw: &str) -> Result<Option<ChatChunk>> {
        // Stateless trait fallback: with no cross-chunk state the
        // reasoning detail index cannot advance here (same inherent
        // limitation as RawThinkTag's cross-chunk <think> stripping).
        // The real streaming path is `stream()`, which threads a
        // persistent per-stream counter.
        let mut reasoning_index = 0u32;
        sse::parse_event(
            &self.cfg.id,
            raw,
            self.cfg.reasoning_dialect,
            &mut reasoning_index,
        )
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
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

        let request = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth). build_headers
        // assembled the auth into `headers`; capture the full set from
        // the built request. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            // Read headers BEFORE `resp.text()` moves the body; the
            // shared mapper takes `&HeaderMap` and computes the
            // rate-limit-gated retry_after + classifier + WARN split.
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_openai_compat_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume (resp.json() takes ownership). Opt-in via
        // ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
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

        let headers = self.build_headers(&req)?;
        let url = self.completions_url();
        debug!(provider = %self.cfg.id, url = %url, "POST chat/completions (stream)");

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let request = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) for the stream
        // path. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            // Read headers BEFORE `resp.text()` moves the body. Shared
            // with complete(): retry_after is preserved on the stream
            // path via the same upstream_full mapping.
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_openai_compat_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &body_text,
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
            // Per-choice running concatenation of `delta.content` text,
            // indexed by `choice.index` and grown on demand. The terminal
            // chunk (the one carrying `finish_reason`) gets the
            // matched_stop_sequence applied just before yield, mirroring
            // what the non-streaming path does after `normalize_response`.
            // Per-choice (not one shared buffer) so an `n > 1` response
            // never bleeds one choice's content into another choice's
            // suffix match.
            let mut accumulated_text: Vec<String> = Vec::new();
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
                    if dialect == ReasoningDialect::RawThinkTag {
                        if let Some(pending) = think_acc.take_pending() {
                            yield Ok(flush_pending_chunk(&pending));
                        }
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
                        // Only accumulate content text when stop-sequence
                        // detection is actually needed: skip the push_str
                        // when the caller sent no stop sequences, avoiding
                        // allocation and string growth for the common case.
                        if request_stop.is_some() {
                            for choice in chunk.choices.iter() {
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
                            for choice in chunk.choices.iter_mut() {
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
            // Normal stream exhaustion (upstream closed without [DONE]).
            // Same flush logic as the [DONE] path above.
            if dialect == ReasoningDialect::RawThinkTag {
                if let Some(pending) = think_acc.take_pending() {
                    yield Ok(flush_pending_chunk(&pending));
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

/// Upper bound on the per-choice stream accumulator index. A request's
/// legitimate max index is n-1 where n (the `n` sampling param) is small;
/// 128 is far above any real fan-out. A malicious or buggy upstream that
/// echoes `choice.index = 1_000_000` would otherwise force a huge Vec
/// allocation via `resize`. Out-of-range indices are skipped.
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
    let Ok(v) = serde_json::from_str::<Value>(body_text) else {
        return (None, None);
    };
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
/// (`resp.text()` moves the body). This ordering is a programmer
/// convention, NOT enforced by the borrow checker: `headers` is an owned
/// `HeaderMap` clone here, so the compiler does not couple it to the body
/// move. Both call sites clone the headers before calling `resp.text()`.
fn map_openai_compat_upstream_error(
    provider_id: &str,
    status: u16,
    headers: &HeaderMap,
    body_text: &str,
) -> Error {
    // Reset hint from response headers, gated on rate-limit statuses so a
    // stray Retry-After on a 400 doesn't park the provider.
    let retry_after = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    // Full upstream error body at debug level. The truncated WARN excerpt
    // below stays for warn-log scannability.
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, body_text);
    // Best-effort lift of the upstream classifier so an SDK that branches
    // on `error.type` / `error.code` keeps the upstream signal.
    let (upstream_type, upstream_code) = parse_openai_error_classifier(body_text);
    let sanitized = extract_upstream_message(body_text);
    let safe_excerpt = sanitize_for_log(&sanitized);
    // Extend the auth-only WARN to all 4xx/5xx so an operator never has to
    // guess WHY a request failed.
    if status == 401 || status == 403 {
        tracing::warn!(
            provider = %provider_id,
            status,
            body_excerpt = %safe_excerpt,
            "openai-compat upstream auth failed",
        );
    } else {
        tracing::warn!(
            provider = %provider_id,
            status,
            body_excerpt = %safe_excerpt,
            "openai-compat upstream error",
        );
    }
    Error::upstream_full(
        provider_id,
        status,
        sanitized,
        retry_after,
        upstream_type,
        upstream_code,
    )
}

#[cfg(test)]
mod helper_tests {
    use super::{
        accumulate_choice_text, ensure_stream_options_include_usage,
        map_openai_compat_upstream_error, parse_openai_error_classifier, MAX_STREAM_CHOICES,
    };
    use reqwest::header::HeaderMap;
    use routectl_core::Error;
    use serde_json::json;

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
        let err = map_openai_compat_upstream_error("p", 429, &headers, body);

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
        let err = map_openai_compat_upstream_error("p", 429, &headers, "{}");

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
        // A hostile upstream echoes a wildly out-of-range choice.index.
        // The accumulator must neither panic nor resize the buffer to
        // millions of entries -- it drops the write and stays bounded.
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
