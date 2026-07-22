//! Anthropic Messages API provider (api.anthropic.com).
//!
//! Wire format: <https://docs.anthropic.com/en/api/messages>
//! Extended thinking: <https://platform.claude.com/docs/en/docs/build-with-claude/extended-thinking>
//!
//! Reasoning normalization:
//! - Request: `reasoning.max_tokens` -> `thinking.budget_tokens`,
//!   `reasoning.effort` -> proportional `budget_tokens`.
//! - Response: content[] thinking blocks -> `reasoning_details[format="anthropic-claude-v1"]`
//!   with signature preserved for multi-turn tool-use continuity.
//! - Multi-turn: thinking blocks are passed back unmodified; signature is mandatory.

use std::sync::Arc;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::Value;

use routectl_core::identity::anthropic::is_anthropic_api_host;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount,
    debug_upstream_error_body, is_json_error_envelope, sanitize_for_log, sanitize_upstream_body,
    trace_outgoing_body, trace_upstream_success_body,
};

mod client;
mod cloak;
pub(crate) mod context_management;
mod extras;
mod messages;
pub(crate) mod parts;
mod ratelimit_unified;
pub mod request;
pub mod response;
pub mod sse;
pub mod sse_opaque;
pub mod sse_unknown;
mod system;
mod tools;
pub(crate) mod types;
pub(crate) mod types_sse;

/// Provider-kind discriminator string used in tracing fields. See
/// the openai_compat module for the rationale.
const PROVIDER_KIND: &str = "anthropic";

/// Anthropic wire-format tag for reasoning details. A single canonical
/// definition shared by all sub-modules (context_management, request,
/// response, sse) via `super::ANTHROPIC_FORMAT` paths.
pub(crate) const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

use sse::SseState;

pub use client::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
pub use cloak::{CloakConfig, CloakMode, ToolRename};

#[async_trait]
impl Provider for AnthropicApiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(
            &self.cfg.id,
            req,
            req.routectl_internal.supports_adaptive_thinking,
            &self.cfg.allowed_betas,
            self.cfg.context_management,
            if self.cfg.context_management {
                Some(&*self.thinking_cache)
            } else {
                None
            },
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let forwarded_leg = self.forwarded_leg(&req);
        let mut body = self.normalize_request(&req)?;
        // Ensure stream is absent / false for the non-streaming path.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream");
            // `api.anthropic.com` (especially the OAuth-Bearer
            // flavor) rejects `anthropic_beta` as a top-level body
            // field with `Extra inputs are not permitted`. Betas
            // travel on the `anthropic-beta` HTTP header
            // (build_headers emits the merged value). Bedrock's
            // body-shape egress keeps the field via its own assembly
            // path, so this strip is scoped to the api.anthropic.com
            // egress.
            obj.remove("anthropic_beta");
        }

        // Cloak the outgoing body on the OAuth anthropic-api surface:
        // always strip the billing block; for a non-CC client also reduce
        // `system` to the identity line only (relocating the client system
        // into the first user message) and mint the metadata user_id. Also
        // normalize every non-`mcp__` tool name to the `mcp__` prefix. Runs
        // after normalize_request and before serialize/resign. The returned
        // reverse map restores the client's original tool names on the
        // response below.
        let cloak_result = self.cloak_body(&mut body, &req);

        // Emit the outgoing body at trace level so a grep by
        // request_id correlates ingress -> egress -> upstream
        // response in one pass during triage. Gated by the
        // `tracing::Level::TRACE` filter -- production with default
        // info level pays nothing.
        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Host-pinned per-request token resolution. On the
        // api.anthropic.com surface a forwarded first-party bearer
        // (forwarded / pure-proxy mode) is used verbatim; otherwise the
        // provider resolves its own token -- for static refs the in-memory
        // `StaticToken` cache, for `oauth://<provider>` refs the `OAuthStore`
        // current value (including the v0.7+ refresh path). See
        // `resolve_effective_token` for the host pin.
        let token = self.resolve_effective_token(&req).await?;

        // Serialize first so the billing-header checksum can be re-signed
        // over the exact bytes transmitted. routectl mutates the canonical
        // body upstream of this point (effort injection, tool-id sanitize,
        // signature strip), which invalidates any checksum the ingress
        // client computed. Re-sign only on the Claude-Code OauthBearer
        // api.anthropic.com surface; every other path is a no-op.
        let mut body_bytes = serde_json::to_vec(&body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        if !forwarded_leg
            && self.cfg.auth_kind == AuthKind::OauthBearer
            && is_anthropic_api_host(&self.cfg.base_url)
        {
            crate::claude_signing::resign_cch_in_place(&mut body_bytes);
        }

        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.messages_url()), &req, &token);
        let request = rb
            .header("content-type", "application/json")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) from the built
        // request -- auth is only present after build_headers applies
        // the resolved token. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        // On non-2xx, read the body as text FIRST so a non-JSON
        // upstream response (HTML 502 from a misconfigured proxy,
        // a CDN cleartext "rate limited" page, Anthropic's
        // occasional plain-text 529 "overloaded") doesn't get
        // collapsed into an opaque serde error. JSON parse is
        // attempted opportunistically to lift `error.message`; on
        // parse failure we fall back to a sanitized text excerpt
        // matching the openai-compat / bedrock pattern. Operators
        // grepping `body_excerpt=...` get a consistent shape across
        // providers.
        if status >= 400 {
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            // Extend the auth-only WARN to all 4xx/5xx so an operator
            // never has to guess WHY a request failed. Sanitize before
            // tracing: the upstream may return untrusted control bytes
            // (CRLF, control chars, very long lines) that would otherwise
            // corrupt log output on text-format subscribers.
            let safe_excerpt = sanitize_for_log(&msg);
            crate::upstream_log::warn_upstream_failure(
                &self.cfg.id,
                status,
                Some(&self.cfg.auth_kind),
                &safe_excerpt,
                "anthropic",
            );
            // Beta-decision context: own-token OauthBearer +
            // api.anthropic.com lane only -- the BetaDecision only carries
            // meaning there. Fires on ANY 4xx on that lane (no error-text
            // matching), so a beta-caused 400 recurrence is diagnosable
            // without enabling header tracing. Gate is `should_log_beta_4xx`
            // (shared with stream() and count_tokens()).
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume (the capped read takes ownership). Opt-in via
        // ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        // Parse the anthropic-ratelimit-unified-* quota family from the
        // same headers (BEFORE the body consume) and run the overage-flip
        // log. Returns None on the api-key path (family absent).
        let upstream_meta = self.observe_unified_quota(resp.headers());
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
        let raw_body: Value = map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
        // Trace upstream success body pre-normalize.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);
        // Clone the raw body before normalize consumes it. Only pay the
        // allocation cost on the context_management emulation path; the
        // default false path skips the clone entirely.
        let raw_for_cache = if self.cfg.context_management {
            Some(raw_body.clone())
        } else {
            None
        };
        let mut chat_resp = self.normalize_response(raw_body)?;
        // Restore the client's original tool names on the response. The
        // forward pass normalized non-`mcp__` names to the `mcp__` prefix
        // on the wire; reverse only the names this request actually
        // renamed so a client that legitimately used `mcp__` names is
        // unaffected.
        if let Some(result) = cloak_result.as_ref() {
            response::reverse_tool_names(&mut chat_resp, &result.tool_reverse);
        }
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        chat_resp.upstream_meta = upstream_meta;
        // Context-management cache write. Extracts (tool_use_id, thinking)
        // pairs from the upstream content blocks and inserts them into the
        // shared thinking cache for re-injection on the next turn. The write
        // lock is acquired synchronously here -- no .await after this point --
        // so it is never held across an async yield.
        if let Some(raw) = raw_for_cache {
            let blocks: Vec<types::ContentBlock> = raw
                .pointer("/content")
                .and_then(|v| serde_json::from_value::<Vec<types::ContentBlock>>(v.clone()).ok())
                .unwrap_or_default();
            let pairs = context_management::extract_tool_thinking(&blocks);
            for (tool_use_id, thinking) in pairs {
                context_management::snapshot_to_cache(
                    &self.thinking_cache,
                    &self.cfg.id,
                    &tool_use_id,
                    thinking,
                    self.cfg.max_thinking_entry_bytes,
                    context_management::THINKING_CACHE_TTL,
                    "complete",
                );
            }
        }
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let forwarded_leg = self.forwarded_leg(&req);
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
            // See note on the complete() path: api.anthropic.com
            // rejects `anthropic_beta` as a body field; the HTTP
            // header carries them via build_headers.
            obj.remove("anthropic_beta");
        }

        // See complete(): cloak the OAuth anthropic-api body before
        // serialize/resign (billing strip always; identity + metadata for
        // a non-CC client; mcp_ tool-name normalization). The reverse map
        // is threaded into SseState so streamed tool_use names are
        // restored to the client's originals.
        let cloak_result = self.cloak_body(&mut body, &req);

        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.resolve_effective_token(&req).await?;

        // See the complete() path: re-sign the billing-header checksum
        // over the exact transmitted bytes on the Claude-Code OauthBearer
        // api.anthropic.com surface; a no-op everywhere else.
        let mut body_bytes = serde_json::to_vec(&body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        if !forwarded_leg
            && self.cfg.auth_kind == AuthKind::OauthBearer
            && is_anthropic_api_host(&self.cfg.base_url)
        {
            crate::claude_signing::resign_cch_in_place(&mut body_bytes);
        }

        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.messages_url()), &req, &token);
        let request = rb
            .header("content-type", "application/json")
            .body(body_bytes)
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
        if status >= 400 {
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- see comment there. Helper extracted at
            // `read_anthropic_error`. Sanitize the excerpt for the
            // same reason as `complete()`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            let safe_excerpt = sanitize_for_log(&msg);
            crate::upstream_log::warn_upstream_failure(
                &self.cfg.id,
                status,
                Some(&self.cfg.auth_kind),
                &safe_excerpt,
                "anthropic",
            );
            // See complete(): own-token OauthBearer + api.anthropic.com
            // lane, 4xx only, via the shared `should_log_beta_4xx` gate.
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream. The stream path had no dir-3 capture
        // before; this closes the gap so it matches the complete() path.
        // Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        // Parse the anthropic-ratelimit-unified-* quota family from the
        // response head (BEFORE `resp` is moved into the byte stream) and
        // run the overage-flip log once here. The parsed carrier is
        // attached to the FIRST canonical chunk yielded by the stream;
        // consumers must not assume it on later chunks. None on the
        // api-key path (family absent).
        let mut pending_upstream_meta = self.observe_unified_quota(resp.headers());

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        // Capture the context_management flag and a shared reference to
        // the thinking cache so the post-stream write tail can drain
        // pending_cache_writes synchronously without holding the lock
        // across any await point.
        let context_management_enabled = self.cfg.context_management;
        let max_thinking_entry_bytes = self.cfg.max_thinking_entry_bytes;
        let thinking_cache_for_stream = Arc::clone(&self.thinking_cache);
        // Per-request tool-name reverse map (renamed upstream name ->
        // original client name) from the cloak forward pass. Empty / None
        // when the cloak did not run or renamed nothing.
        let tool_reverse = cloak_result.map(|r| r.tool_reverse).unwrap_or_default();

        let stream = async_stream::stream! {
            let mut state = SseState::new(&provider_id);
            state.tool_reverse = tool_reverse;

            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        // Surface the cache-write count we are abandoning
                        // so triage can correlate a torn stream with any
                        // pending context_management snapshots that never
                        // made it into the LRU.
                        tracing::debug!(
                            provider = %provider_id,
                            pending_cache_writes_count = state.pending_cache_writes.len(),
                            "anthropic-api stream: SSE event error; aborting before post-stream cache drain"
                        );
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        let trimmed = event.data.trim();
                        // OpenRouter's `/v1/messages` endpoint appends
                        // an OpenAI-style `data: [DONE]` sentinel after
                        // `message_stop`. Real api.anthropic.com does
                        // not emit it. Treat it as a clean EOS: skip
                        // it (parse_event would fail with
                        // `bad sse json: expected value at line 1
                        // column 2`) and return so the outer stream
                        // ends naturally, letting the egress wrapper
                        // mark_clean_close and report the actual
                        // finish_reason instead of synthesizing
                        // `truncated`. Mirrors `openai_compat::stream`.
                        if trimmed == "[DONE]" {
                            tracing::debug!(
                                provider = %provider_id,
                                "anthropic-api stream: received OpenAI-style \
                                 [DONE] sentinel after message_stop (typical of \
                                 OpenRouter's /v1/messages passthrough); \
                                 closing stream cleanly"
                            );
                            break;
                        }
                        // Keepalive comment line or empty data field.
                        if trimmed.is_empty() {
                            continue;
                        }
                        match state.parse_event(&provider_id, &event.data) {
                            Err(e) => {
                                // Same triage hint as the event-stream Err
                                // arm above: log the abandoned cache-write
                                // count before yielding so a parse failure
                                // mid-stream is correlatable.
                                tracing::debug!(
                                    provider = %provider_id,
                                    pending_cache_writes_count = state.pending_cache_writes.len(),
                                    "anthropic-api stream: SSE parse error; aborting before post-stream cache drain"
                                );
                                yield Err(e);
                                return;
                            }
                            Ok(Some(mut chunk)) => {
                                // Attach the unified-quota carrier to the
                                // FIRST canonical chunk only; `take()`
                                // leaves None for every subsequent chunk.
                                if pending_upstream_meta.is_some() {
                                    chunk.upstream_meta = pending_upstream_meta.take();
                                }
                                yield Ok(chunk);
                            }
                            Ok(None) => {}
                        }
                    }
                }
            }
            // Post-stream cache-write tail for context_management emulation.
            // Drains pending_cache_writes accumulated during SSE parsing into
            // the thinking cache. Each call to snapshot_to_cache acquires and
            // releases the write lock synchronously -- no await points here.
            if context_management_enabled && !state.pending_cache_writes.is_empty() {
                for (tool_use_id, thinking) in state.pending_cache_writes.drain(..) {
                    context_management::snapshot_to_cache(
                        &thinking_cache_for_stream,
                        &provider_id,
                        &tool_use_id,
                        thinking,
                        max_thinking_entry_bytes,
                        context_management::THINKING_CACHE_TTL,
                        "stream",
                    );
                }
            }
        };

        Ok(routectl_core::wrap_stream_with_summary(
            stream,
            "upstream",
            PROVIDER_KIND,
            self.cfg.id.clone(),
        ))
    }

    /// `POST /v1/messages/count_tokens` -- a probe call that returns
    /// the input-token count for a request without invoking model
    /// inference. claude-code uses this for context-budget display.
    /// Wire reference:
    /// <https://docs.anthropic.com/en/api/messages-count-tokens>
    ///
    /// Body assembly: `normalize_request` produces a fully-built
    /// `/v1/messages` body. We then BUILD the count_tokens body from
    /// scratch using only the allowlist of fields the count_tokens
    /// endpoint accepts (per the Anthropic docs URL above):
    /// `model`, `messages`, `system`, `tools`, `tool_choice`,
    /// `thinking`, `mcp_servers`, `metadata`. This is more defensive
    /// than strip-by-blocklist: a future addition to
    /// `normalize_request` (e.g. `output_config.format`, which IS
    /// rejected by `/v1/messages/count_tokens`) won't accidentally
    /// leak into count_tokens.
    ///
    /// Headers are identical to `complete()` (anthropic-beta union,
    /// anthropic-version, header_extras, X-Claude-Code-* allowlist
    /// filter, auth) -- so a count_tokens call observes the same
    /// merged beta surface as the messages endpoint.
    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        // No `forwarded_leg` local here: count_tokens is unsigned (no cch
        // re-sign) and its only body mutation is `cloak_body`, which
        // self-gates on the forwarded leg (see the FORWARDING TRANSPARENCY
        // CONTRACT). `build_count_tokens_body` copies only the field
        // allowlist, which never includes `anthropic_beta`.
        let mut normalized = self.normalize_request(&req)?;
        // Cloak before build_count_tokens_body reads `normalized`. In own
        // mode the metadata user_id is dropped by the count_tokens allowlist
        // (it is not in that schema), but the system-identity stamp, the
        // billing strip, and the mcp_ tool-name normalization apply to the
        // outgoing body. On the forwarded leg cloak_body self-gates to a
        // no-op, so none of these run. count_tokens has no response tool_use
        // surface to reverse, so the returned reverse map is discarded.
        self.cloak_body(&mut normalized, &req);
        let body = build_count_tokens_body(&normalized);

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.resolve_effective_token(&req).await?;

        // count_tokens is deliberately unsigned (matches upstream).
        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.count_tokens_url()), &req, &token);
        let request = rb
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) for the
        // count_tokens probe. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- a non-JSON 502/503 from a misconfigured
            // proxy must not collapse to an opaque serde error.
            // Helper extracted at `read_anthropic_error`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            // Sanitize before tracing: the upstream may return
            // untrusted control bytes (CRLF, control chars, very
            // long lines) and `body_excerpt = %msg` would otherwise
            // emit them verbatim into operator logs. Same posture as
            // the `complete()` and `stream()` paths above.
            let safe_excerpt = sanitize_for_log(&msg);
            if status == 501 {
                // A 501 on count_tokens is a CAPABILITY signal, not a
                // health failure: the upstream (e.g. an anthropic-api
                // base_url that back-hops to a Bedrock egress) does not
                // implement count_tokens. The router already handles this
                // by walking to the next capable seat, so logging it at
                // WARN would flood operator logs on every client poll.
                // DEBUG mirrors the router-layer treatment.
                tracing::debug!(
                    provider = %self.cfg.id,
                    status,
                    context = "anthropic count_tokens",
                    body_excerpt = %safe_excerpt,
                    "count_tokens unsupported by upstream (501); router walks to next capable seat",
                );
            } else {
                crate::upstream_log::warn_upstream_failure(
                    &self.cfg.id,
                    status,
                    Some(&self.cfg.auth_kind),
                    &safe_excerpt,
                    "anthropic count_tokens",
                );
            }
            // See complete(): own-token OauthBearer + api.anthropic.com
            // lane, 4xx only (naturally excludes the 501 capability signal
            // above, which is a 5xx), via the shared `should_log_beta_4xx`
            // gate.
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
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
        let raw_body: Value = map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);
        let token_count: TokenCount = serde_json::from_value(raw_body).map_err(|e| {
            Error::normalize_response(&self.cfg.id, format!("count_tokens response parse: {e}"))
        })?;
        Ok(token_count)
    }

    /// Forward upstream-401 to the underlying token source so an
    /// `oauth://` ref can force-refresh through the OAuth store's
    /// per-provider single-flight gate. Static-auth providers
    /// (`env://`, `file://`, `literal:`) inherit the no-op default
    /// from `TokenSource::on_auth_failure`. Errors propagate so the
    /// router surfaces an actionable auth message rather than walking
    /// the fallback chain over a dead OAuth identity.
    async fn on_auth_failure(&self) -> Result<()> {
        self.cfg.auth.on_auth_failure().await
    }

    /// Free reachability probe: a single GET against `/v1/models`.
    ///
    /// BINDING read-only guard: only the `ApiKey` lane holds a static,
    /// non-refreshing credential. An `OauthBearer` provider resolves its
    /// token through the refreshing `token()` path, which a reachability
    /// probe must never trigger -- so it reports `UnsupportedFreeProbe`
    /// and the CLI orchestration layer owns oauth reachability. On the
    /// `ApiKey` lane the resolved key is a `StaticToken`, so reading it
    /// here does no refresh.
    async fn probe(&self) -> routectl_core::ProbeOutcome {
        if self.cfg.auth_kind != AuthKind::ApiKey {
            return routectl_core::ProbeOutcome::UnsupportedFreeProbe;
        }
        let token = match self.cfg.auth.token().await {
            Ok(t) => t,
            Err(_) => {
                return routectl_core::ProbeOutcome::AuthFailed(
                    "provider credential unavailable".into(),
                );
            }
        };
        let url = format!("{}/v1/models", self.cfg.base_url.trim_end_matches('/'));
        let mut headers = reqwest::header::HeaderMap::new();
        match reqwest::header::HeaderValue::from_str(&token) {
            Ok(v) => {
                headers.insert("x-api-key", v);
            }
            Err(_) => {
                return routectl_core::ProbeOutcome::Unreachable(
                    "credential could not form an auth header".into(),
                );
            }
        }
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&self.cfg.anthropic_version) {
            headers.insert("anthropic-version", v);
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

/// Map a non-streaming success body `(bytes, hit_cap)` to the parsed JSON.
///
/// A cap trip on a 2xx means the upstream returned an unreadable success
/// response -- an invalid upstream protocol result. It maps to a 502
/// `Error::upstream`, which classifies as a ServerError (debits the
/// breaker, retries/fallbacks) exactly like any other upstream protocol
/// failure. Otherwise the buffered bytes are parsed once.
fn map_success_body(provider_id: &str, status: u16, bytes: &[u8], hit_cap: bool) -> Result<Value> {
    if hit_cap {
        return Err(Error::upstream(
            provider_id,
            502,
            crate::http_client::body_cap_exceeded_message(),
        ));
    }
    serde_json::from_slice(bytes).map_err(|e| Error::upstream(provider_id, status, e.to_string()))
}

/// Lift the Anthropic upstream classifier (`error.type`) from an
/// already-parsed error body. Best-effort: `None` when the body was not
/// JSON (a truncated/incomplete prefix, an HTML gateway page) or carries no
/// `error.type`. Shared by the cap-trip and intact error paths so a
/// truncated prefix that still parses keeps the classifier signal.
fn parse_anthropic_error_type(parsed: Option<&Value>) -> Option<String> {
    parsed
        .and_then(|v| v.pointer("/error/type"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Read a 4xx/5xx upstream response body and build a routectl
/// `Error::Upstream` from it. Encapsulates the
/// "text-first-then-opportunistic-JSON" pattern shared by
/// `complete()`, `stream()`, and `count_tokens()`: a non-JSON
/// upstream response (HTML 502 from a misconfigured proxy, a CDN
/// cleartext error page, plain-text 529) must not collapse to an
/// opaque serde error. Returns both the parsed message (for the
/// caller's `body_excerpt` log) and the constructed `Error::Upstream`.
///
/// The body is read under the shared [`crate::http_client::read_body_capped`]
/// ceiling: a lying or hostile upstream error body is bounded like any
/// other. On a cap trip a single WARN records the truncation and the
/// client-facing message collapses to a fixed cap-exceeded string --
/// the truncated prefix is never echoed or classified.
async fn read_anthropic_error(
    provider_id: &str,
    status: u16,
    resp: reqwest::Response,
) -> (String, Error) {
    // Capture the reset hint from response headers BEFORE the body read
    // moves `resp`, gated on rate-limit statuses. This is the single
    // chokepoint for complete/stream/count_tokens, so all three HTTP
    // paths pick up the hint here.
    let retry_after = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(resp.headers())
    } else {
        None
    };
    let content_length = resp.content_length();
    let (bytes, hit_cap) = match crate::http_client::read_body_capped(
        resp,
        crate::http_client::MAX_RESPONSE_BODY_BYTES,
    )
    .await
    {
        Ok(read) => read,
        Err(e) => {
            // A transport failure while reading the error body is not a
            // cap trip; surface it so the error path is not silently
            // blind (the 2xx path turns the same failure into an Error).
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
    let body_text = String::from_utf8_lossy(&bytes).into_owned();
    // Emit the FULL (capped) upstream error body at debug level so triage
    // doesn't have to reproduce. The caller's WARN excerpt stays
    // unchanged for `routectl-warn.log` scannability.
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, &body_text);
    // Parse once; both the cap-trip and intact paths lift the classifier
    // from the same parsed value. Anthropic shape
    // `{type:"error",error:{type,message}}`; errors carry no separate `code`,
    // so only `upstream_type` is populated.
    let parsed = serde_json::from_str::<Value>(&body_text).ok();
    let upstream_type = parse_anthropic_error_type(parsed.as_ref());
    if hit_cap {
        // A body truncated at the cap is untrustworthy: never echo it to the
        // client. The client-facing message stays the fixed cap-exceeded
        // string (upstream status and captured reset hint preserved), while
        // the classifier lifted above still rides along when `error.type`
        // survived truncation.
        let msg = crate::http_client::body_cap_exceeded_message();
        let err = Error::upstream_full(
            provider_id,
            status,
            msg.clone(),
            retry_after,
            upstream_type,
            None,
        );
        return (msg, err);
    }
    let msg = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map_or_else(
            || sanitize_upstream_body(&body_text),
            std::string::ToString::to_string,
        );
    // When the upstream returned a structured `{error:...}` JSON envelope,
    // carry the RAW body so the ingress sanitizer can re-extract the
    // upstream's own top-level `error.message` and surface it to the
    // client. A client (e.g. Claude Code) can then recognize and
    // self-heal an actionable upstream 400 instead of hitting a
    // status-only wall. When the body was NOT a `{error:...}` envelope
    // (HTML page, plain-text gateway error), carry the sanitized excerpt
    // so the sanitizer falls back to a status-only message -- never a raw
    // body dump.
    let err_body = if is_json_error_envelope(&body_text) {
        body_text
    } else {
        msg.clone()
    };
    let err = Error::upstream_full(
        provider_id,
        status,
        err_body,
        retry_after,
        upstream_type,
        None,
    );
    (msg, err)
}

/// Build the body for `POST /v1/messages/count_tokens` from the
/// already-normalized `/v1/messages` body. Only an explicit allowlist
/// of fields gets copied through:
/// `model`, `messages`, `system`, `tools`, `tool_choice`, `thinking`,
/// `mcp_servers`.
///
/// The count_tokens schema accepts `messages`, `model`, `cache_control`,
/// `output_config`, `system`, `thinking`, `tool_choice`, and `tools`
/// (`cache_control` rides inside the message/system/tool blocks that are
/// forwarded wholesale). `metadata` is NOT part of that schema, so it must
/// be dropped or the upstream 400s with `Extra inputs are not permitted`.
/// `output_config` IS accepted but is intentionally omitted here because it
/// does not affect the input token count.
///
/// This allowlist is more defensive than strip-by-blocklist: future
/// additions to `normalize_request` won't accidentally leak into
/// count_tokens.
fn build_count_tokens_body(normalized: &Value) -> Value {
    const ALLOWED: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        // Accepted only by the MCP-connector beta variant of count_tokens
        // (routectl unions that beta header through).
        "mcp_servers",
    ];
    let mut out = serde_json::Map::new();
    let Some(src) = normalized.as_object() else {
        return Value::Object(out);
    };
    for &k in ALLOWED {
        if let Some(v) = src.get(k)
            && !v.is_null()
        {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
