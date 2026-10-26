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
use routectl_core::wire_check::{EditKind, EditSite};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, ParsedErrorBody, Provider, Result, TokenCount,
    debug_upstream_error_body, sanitize_for_log, trace_outgoing_body, trace_upstream_success_body,
};

use crate::upstream_error::{CarriedBody, UpstreamErrorRead};

mod client;
mod cloak;
pub(crate) mod context_management;
pub(crate) mod envelope_policy;
mod extras;
mod messages;
pub(crate) mod output_schema;
pub(crate) mod parts;
mod ratelimit_unified;
pub mod request;
pub mod response;
pub mod sse;
pub mod sse_opaque;
pub mod sse_unknown;

/// Whether `req` is CLIENT traffic rather than routectl's own background probe.
///
/// A NEUTRAL question, deliberately, because three different boundaries turn on
/// it and only one of them is about statistics:
///
/// - the translation-drop census, whose every counter divides one population and
///   that population is client traffic (`lane_seen` is the denominator and each
///   policy action a numerator over it) -- counting a probe anywhere would move a
///   rate by an amount set by probe scheduling, and counting it in only SOME
///   counters would skew the rates against each other;
/// - the unified-quota claim record and its once-per-transition overage-flip log,
///   which describe what client requests last observed about billing attribution;
/// - the context-management thinking cache, read by the NEXT client turn out of a
///   capacity-bounded LRU.
///
/// Naming it after the statistics alone would have made the two state boundaries
/// read as if they were counting something. Each caller states its own reason; the
/// predicate states only the fact.
///
/// A PREDICATE rather than a wrapper around the recorders, deliberately. The
/// translation-drop census resolves every `record_translation_*` call's lane and
/// class to constants in the call's own file, and it FAILS a call it cannot
/// resolve rather than skipping it. A wrapper taking the lane as a parameter
/// therefore takes each call out of the census -- measured: it broke four census
/// tests. Gating at each call site keeps every call resolvable and still
/// single-sources the decision.
///
/// Other lanes do not consult this and are unaffected.
const fn is_client_traffic(req: &routectl_core::ChatRequest) -> bool {
    !req.routectl_internal.background_probe
}
mod system;
mod tools;
pub(crate) mod types;
pub(crate) mod types_sse;
pub(crate) mod wire_edits;

/// Provider-kind discriminator string used in tracing fields. See
/// the openai_compat module for the rationale.
pub(crate) const PROVIDER_KIND: &str = routectl_core::provider_kind::trace::ANTHROPIC;

/// The lane spelling every `record_translation_drop` /
/// `record_translation_policy_action` / `record_translation_lane_seen`
/// call on this egress uses. Bound to `PROVIDER_KIND` rather than
/// re-spelled so the telemetry lane and the `provider_kind=` tracing
/// field can never drift apart.
pub(crate) const LANE: &str = PROVIDER_KIND;

/// Anthropic wire-format tag for reasoning details. A single canonical
/// definition shared by all sub-modules (context_management, request,
/// response, sse) via `super::ANTHROPIC_FORMAT` paths.
pub(crate) const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

use crate::stream_completion::StreamCompletion;
use sse::SseState;

/// The stream parser state for a response from `base_url`. Only the
/// first-party endpoint's first event counts as the vendor's own input
/// measurement; an Anthropic-compatible proxy's is unverified.
fn stream_state_for(provider_id: &str, base_url: &str) -> SseState {
    let state = SseState::new(provider_id);
    if is_anthropic_api_host(base_url) {
        state.with_vendor_opening()
    } else {
        state
    }
}

#[cfg(feature = "bedrock")]
pub use crate::mantle::MantleAuth;
pub use client::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
pub use cloak::{
    CloakConfig, CloakMode, MAX_SENSITIVE_WORD_FOLDED_CHARS, MAX_SENSITIVE_WORDS,
    SensitiveWordsBoundError, ToolRename, validate_sensitive_words,
};
pub use context_management::CONTEXT_MANAGEMENT_BETA;
/// The `max_tokens` this egress sends when neither the client nor the
/// router-filled `routectl_internal.max_output_tokens` supplies one.
pub use extras::DEFAULT_MAX_OUTPUT_TOKENS;
/// The `thinking.type` this egress composes for a request, for a caller
/// that must reason about the shape before the body is built.
pub use extras::emitted_thinking_type;

#[async_trait]
impl Provider for AnthropicApiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(
            &self.cfg.id,
            req,
            self.cfg.context_management,
            if self.cfg.context_management {
                Some(&*self.thinking_cache)
            } else {
                None
            },
            // The client boundary owns the base_url, so the terminal-host
            // question is answered ONCE here and threaded down as a
            // resolved bool. An anthropic-api provider can be pointed at
            // any Anthropic-compatible host (another routectl hop
            // included), and only the genuine host is terminal.
            is_anthropic_api_host(&self.cfg.base_url),
            // This lane targets the Anthropic Messages API, which accepts a
            // mid-conversation `role: "system"` turn, so a system turn the
            // client sent alongside a canonical top-level system rides
            // through in place instead of being discarded. Stated EXPLICITLY
            // here rather than inherited, so a default changing elsewhere
            // cannot start shipping the shape on a lane whose target API has
            // no such role.
            true,
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
        let forwarded_leg = self.forwarded_leg(&req);
        let mut body = self.normalize_request(&req)?;
        let tap = wire_edits::tap_of(&req);
        // Ensure stream is absent / false for the non-streaming path.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream");
            wire_edits::declare_stream_strip(tap, req.stream);
            // `api.anthropic.com` (especially the OAuth-Bearer
            // flavor) rejects `anthropic_beta` as a top-level body
            // field with `Extra inputs are not permitted`. Betas
            // travel on the `anthropic-beta` HTTP header
            // (build_headers emits the merged value). Bedrock's
            // body-shape egress keeps the field via its own assembly
            // path, so this strip is scoped to the api.anthropic.com
            // egress.
            if obj.remove("anthropic_beta").is_some() {
                wire_edits::declare(
                    tap,
                    EditSite::AnthropicBetaStrip,
                    "/anthropic_beta",
                    EditKind::Remove,
                );
            }
        }

        // Cloak the outgoing body on the OAuth anthropic-api surface:
        // always strip the billing block; for a non-CC client also reduce
        // `system` to the identity line only (relocating the client system
        // into the first user message) and mint the metadata user_id. Also
        // normalize every non-`mcp__` tool name to the `mcp__` prefix. Runs
        // after normalize_request and before serialize/resign. The returned
        // reverse map restores the client's original tool names on the
        // response below.
        let cloak_result = self.cloak_body(&mut body, &req)?;

        // Sampling strip, LAST word on the JSON before the outgoing trace:
        // on the OAuth own-anthropic lane the seat 400s a body carrying
        // `temperature` / `top_p`, so drop them here -- after cloak, after
        // every assembly pass. Gated on the LANE (not `is_non_cc`, not the
        // cloak flag) so a `cloak.mode = never` provider on this credential
        // still ships an accepted body. See `extras::normalize_claude_sampling`.
        if self.is_cloak_lane(&req) {
            let dropped = extras::normalize_claude_sampling(&self.cfg.id, &mut body);
            wire_edits::declare_removed_keys(tap, EditSite::ClaudeSamplingNormalize, &dropped);
        }

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
        let body_bytes = bytes::Bytes::from(body_bytes);
        let sent_copy = tap.map(|_| body_bytes.clone());

        let (rb, beta_decision) = self.build_headers(
            self.client.post(self.messages_url()),
            &req,
            &token,
            Some(&body),
        );
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = rb
            .header("content-type", "application/json")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Mantle lane: SigV4/bearer-sign the built request before any
        // header trace or execute so the trace shows the real auth header
        // and the signed input matches the transmitted bytes. No-op on the
        // first-party lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) from the built
        // request -- auth is only present after build_headers applies
        // the resolved token (and, on the mantle lane, after signing).
        // Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        if let (Some(tap), Some(sent)) = (tap, sent_copy) {
            tap.deposit(sent);
        }
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        // Redirects are disabled on this client (see http_client module
        // docs), so a 3xx is a fault, not a hop -- surface it as an
        // upstream server error BEFORE the success/parse path below,
        // which would otherwise choke on a non-JSON 3xx body.
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
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
            let (msg, err) = read_anthropic_error(&self.cfg.id, resp, self.is_mantle()).await;
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
            // Lane-independent: a mid-conversation system turn is
            // position-gated and model-gated on every lane, so this fires
            // wherever the outgoing body carried one.
            self.log_system_role_turns_on_4xx(
                status,
                AnthropicApiProvider::count_system_role_turns(&body),
                beta_decision.has_mid_conversation_system_beta,
            );
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume (the capped read takes ownership). Opt-in via
        // ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        // Parse the anthropic-ratelimit-unified-* quota family from the
        // same headers (BEFORE the body consume) and run the overage-flip
        // log. Returns None on the api-key path (family absent).
        let upstream_meta = self.observe_unified_quota(&req, resp.headers());
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
        let body_bytes = crate::wire_declare::deposit_response(&req, body_bytes);
        let raw_body: Value =
            crate::upstream_error::map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
        // Trace upstream success body pre-normalize.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);
        // Clone the raw body before normalize consumes it. Only pay the
        // allocation cost on the context_management emulation path; the
        // default false path skips the clone entirely.
        //
        // A BACKGROUND PROBE skips it too, because the only consumer of the
        // clone is the cache write below, which a probe must not perform -- so
        // the probe also pays no allocation for a body nothing will read.
        let raw_for_cache = if self.cfg.context_management && is_client_traffic(&req) {
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
        //
        // A BACKGROUND PROBE never reaches here: `raw_for_cache` is `None` for
        // it above. The cache is keyed by `(provider_id, tool_use_id)` and is
        // read by the NEXT CLIENT turn, so a probe-seeded entry would inject
        // thinking a client's conversation never produced -- and, since the LRU
        // is capacity-bounded, would evict an entry a real conversation is
        // still going to need. Gated where the clone is decided rather than
        // here, so the guard also covers the allocation.
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

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
        let forwarded_leg = self.forwarded_leg(&req);
        let mut body = self.normalize_request(&req)?;
        let tap = wire_edits::tap_of(&req);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
            crate::wire_declare::declare_stream_flag(&req, true);
            // See note on the complete() path: api.anthropic.com
            // rejects `anthropic_beta` as a body field; the HTTP
            // header carries them via build_headers.
            if obj.remove("anthropic_beta").is_some() {
                wire_edits::declare(
                    tap,
                    EditSite::AnthropicBetaStrip,
                    "/anthropic_beta",
                    EditKind::Remove,
                );
            }
        }

        // See complete(): cloak the OAuth anthropic-api body before
        // serialize/resign (billing strip always; identity + metadata for
        // a non-CC client; mcp_ tool-name normalization). The reverse map
        // is threaded into SseState so streamed tool_use names are
        // restored to the client's originals.
        let cloak_result = self.cloak_body(&mut body, &req)?;

        // See complete(): strip caller/thinking-forced sampling the OAuth
        // seat rejects, gated on the lane, as the last body mutation before
        // the trace. See `extras::normalize_claude_sampling`.
        if self.is_cloak_lane(&req) {
            let dropped = extras::normalize_claude_sampling(&self.cfg.id, &mut body);
            wire_edits::declare_removed_keys(tap, EditSite::ClaudeSamplingNormalize, &dropped);
        }

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
        let body_bytes = bytes::Bytes::from(body_bytes);
        let sent_copy = tap.map(|_| body_bytes.clone());

        let (rb, beta_decision) = self.build_headers(
            self.client.post(self.messages_url()),
            &req,
            &token,
            Some(&body),
        );
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = rb
            .header("content-type", "application/json")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Mantle lane: sign the built stream request before trace/execute
        // (see complete()). No-op on the first-party lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) for the stream
        // path. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
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
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- see comment there. Helper extracted at
            // `read_anthropic_error`. Sanitize the excerpt for the
            // same reason as `complete()`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, resp, self.is_mantle()).await;
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
            // Lane-independent: a mid-conversation system turn is
            // position-gated and model-gated on every lane, so this fires
            // wherever the outgoing body carried one.
            self.log_system_role_turns_on_4xx(
                status,
                AnthropicApiProvider::count_system_role_turns(&body),
                beta_decision.has_mid_conversation_system_beta,
            );
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
        // merged into the FIRST canonical chunk yielded by the stream,
        // beside any opening usage the parser put there; consumers must
        // not assume it on later chunks. None on the api-key path (family
        // absent).
        let mut pending_upstream_meta = self.observe_unified_quota(&req, resp.headers());

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        // Capture the context_management flag and a shared reference to
        // the thinking cache so the post-stream write tail can drain
        // pending_cache_writes synchronously without holding the lock
        // across any await point.
        //
        // A BACKGROUND PROBE clears the flag here, which is what keeps it out
        // of the post-stream drain below. Same reasoning as the complete()
        // path: the cache is read by the next CLIENT turn, so a probe-seeded
        // entry injects thinking no client conversation produced and evicts one
        // that a real conversation still needs.
        let context_management_enabled = self.cfg.context_management && is_client_traffic(&req);
        let max_thinking_entry_bytes = self.cfg.max_thinking_entry_bytes;
        let thinking_cache_for_stream = Arc::clone(&self.thinking_cache);
        // Per-request tool-name reverse map (renamed upstream name ->
        // original client name) from the cloak forward pass. Empty / None
        // when the cloak did not run or renamed nothing.
        let tool_reverse = cloak_result.map(|r| r.tool_reverse).unwrap_or_default();

        let base_state = stream_state_for(&provider_id, &self.cfg.base_url);
        let response_tap = crate::wire_declare::response_tap(&req);
        let stream = async_stream::stream! {
            let mut state = base_state;
            state.tool_reverse = tool_reverse;
            let mut completion = StreamCompletion::new(PROVIDER_KIND, sse::is_terminal_event);

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
                        crate::wire_declare::push_frame(response_tap.as_ref(), &event.event, &event.data);
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
                        let parsed = sse::decode_event(&event.data).and_then(|ev| {
                            completion.observe(&ev);
                            state.apply_event(&provider_id, ev)
                        });
                        match parsed {
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
                                // Merge the unified-quota carrier into the
                                // FIRST canonical chunk only; `take()`
                                // leaves None for every subsequent chunk.
                                if let Some(head) = pending_upstream_meta.take() {
                                    chunk.upstream_meta = Some(match chunk.upstream_meta.take() {
                                        Some(body) => head.merge(body),
                                        None => head,
                                    });
                                }
                                yield Ok(chunk);
                            }
                            Ok(None) => {}
                        }
                    }
                }
            }
            if let Err(e) = completion.end_of_stream(&provider_id) {
                tracing::warn!(
                    provider = %provider_id,
                    pending_cache_writes_count = state.pending_cache_writes.len(),
                    "anthropic-api stream: upstream closed before message_stop; \
                     aborting before post-stream cache drain"
                );
                yield Err(e);
                return;
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
    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
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
        // surface to reverse, so the returned reverse map is discarded. A
        // relocation refusal propagates exactly as on the messages paths.
        self.cloak_body(&mut normalized, &req)?;
        let body = build_count_tokens_body(&normalized);

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.resolve_effective_token(&req).await?;

        // Serialized once: the wire tap's deposit and, on the mantle lane,
        // the SigV4 body hash must cover exactly the bytes transmitted.
        // count_tokens is deliberately unsigned on the first-party lane (no
        // cch re-sign, matches upstream).
        let body_bytes = bytes::Bytes::from(
            serde_json::to_vec(&body)
                .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?,
        );
        let tap = wire_edits::tap_of(&req);
        let sent_copy = tap.map(|_| body_bytes.clone());
        // `body` here is the post-allowlist count_tokens body, which is the
        // shape that actually ships -- so a body-derived capability beta is
        // gated on the reshaped body, not on the pre-allowlist `normalized`.
        let (rb, beta_decision) = self.build_headers(
            self.client.post(self.count_tokens_url()),
            &req,
            &token,
            Some(&body),
        );
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = rb
            .header("content-type", "application/json")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) for the
        // count_tokens probe. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
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
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- a non-JSON 502/503 from a misconfigured
            // proxy must not collapse to an opaque serde error.
            // Helper extracted at `read_anthropic_error`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, resp, self.is_mantle()).await;
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
            // Lane-independent: a mid-conversation system turn is
            // position-gated and model-gated on every lane, so this fires
            // wherever the outgoing body carried one.
            self.log_system_role_turns_on_4xx(
                status,
                AnthropicApiProvider::count_system_role_turns(&body),
                beta_decision.has_mid_conversation_system_beta,
            );
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
        let raw_body: Value =
            crate::upstream_error::map_success_body(&self.cfg.id, status, &body_bytes, hit_cap)?;
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
    ///
    /// On the mantle lane (`cfg.mantle` is `Some`, cfg `bedrock`) this
    /// delegates to `mantle::probe` instead: that endpoint signs with
    /// SigV4/bearer and has no free models-list surface, so the probe
    /// resolves the credential rather than dialing the inference host.
    async fn probe(&self) -> routectl_core::ProbeOutcome {
        // Mantle lane: the endpoint signs with SigV4/bearer and exposes no
        // free models-list surface, so probe the credential rather than
        // dialing the inference host with an x-api-key GET.
        #[cfg(feature = "bedrock")]
        if let Some(mantle) = &self.cfg.mantle {
            return crate::mantle::probe(&mantle.creds).await;
        }
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

/// Read a 4xx/5xx upstream response through the shared upstream-error read
/// and build the routectl `Error::Upstream` from it. The single chokepoint
/// for `complete()`, `stream()`, and `count_tokens()`: a non-JSON upstream
/// response (HTML 502 from a misconfigured proxy, a CDN cleartext error
/// page, plain-text 529) must not collapse to an opaque serde error.
/// Returns both the client message (for the caller's `body_excerpt` log)
/// and the constructed `Error::Upstream`.
async fn read_anthropic_error(
    provider_id: &str,
    resp: reqwest::Response,
    aws_backed: bool,
) -> (String, Error) {
    let read = UpstreamErrorRead::read(provider_id, resp)
        .await
        .aws_backed(aws_backed);
    anthropic_error_from_read(provider_id, read)
}

/// Build the `(message, Error::Upstream)` pair from an error response
/// already read. The classifier, the DEBUG body, and the client message all
/// read the read's single parse. Anthropic shape
/// `{type:"error",error:{type,message}}`; tokens come from the shared
/// precedence rule ([`UpstreamErrorRead::tokens`]).
fn anthropic_error_from_read(provider_id: &str, read: UpstreamErrorRead) -> (String, Error) {
    let status = read.status();
    // A native Anthropic error body (`/error/type` present) carries no AWS
    // principal identifiers. A mantle (Bedrock) upstream returns a flat AWS
    // envelope whose 403 AccessDenied message names the caller principal
    // ARN, account id, and resource ARN. The AWS-lift path routes its
    // free-text through the shared Bedrock 403 scrub before any of it
    // reaches the DEBUG body, the WARN excerpt, or the client-facing Error.
    let is_aws_lift = read
        .parsed()
        .and_then(ParsedErrorBody::error_type)
        .is_none()
        || read.needs_access_denied_scrub();
    let debug_body = if is_aws_lift {
        crate::aws_error::sanitized_debug_body(status, read.text(), &read.access_denied_detail())
    } else {
        read.text().to_string()
    };
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, &debug_body);
    let retry_after = read.retry_after();
    if read.hit_cap() {
        // A body truncated at the cap is untrustworthy: never echo it to the
        // client. The client-facing message stays the fixed cap-exceeded
        // string (upstream status and captured reset hint preserved).
        let msg = crate::http_client::body_cap_exceeded_message();
        let err = read.into_error(
            provider_id,
            CarriedBody::Replacement(msg.clone()),
            retry_after,
        );
        return (msg, err);
    }
    // The client-facing message prefers the Anthropic `/error/message`. A
    // 403 needing the access-denied scrub renders routectl's own string
    // instead (its message can name the principal ARN, account id and
    // resource ARN, even inside an `{"error":...}` envelope); any other
    // AWS-lift body is sanitized and capped.
    let msg = if read.needs_access_denied_scrub() {
        read.access_denied_client_message()
    } else {
        match read.parsed().and_then(ParsedErrorBody::error_message) {
            Some(m) => m.to_string(),
            None => crate::aws_error::classify_parsed_client_error_message(
                status,
                read.body(),
                &read.access_denied_detail(),
            ),
        }
    };
    // On the NATIVE Anthropic path the structured `{error:...}` envelope is
    // carried RAW so the ingress can surface the upstream's own
    // `error.message` to the client, letting a client (e.g. Claude Code)
    // self-heal an actionable upstream 400. The AWS-lift path NEVER carries
    // the raw body (its 403 envelope names principal identifiers): it
    // carries the scrubbed message, with the parse of that message. A
    // non-envelope body (HTML page, plain-text gateway error, over-cap JSON)
    // takes the same path, so the ingress falls back to a status-only
    // message, never a raw body dump.
    let carried = if is_aws_lift {
        read.aws_carried(msg.clone())
    } else {
        CarriedBody::Original
    };
    let err = read.into_error(provider_id, carried, retry_after);
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

#[cfg(test)]
#[path = "mod_error_parse_tests.rs"]
mod error_parse_tests;

#[cfg(test)]
#[path = "principal_403_tests.rs"]
mod principal_403_tests;

// Host-gated reasoning-envelope unwrap on the redacted_thinking egress.
#[cfg(test)]
#[path = "mod_envelope_unwrap_tests.rs"]
mod envelope_unwrap_tests;

// This lane's request-volume denominator and the per-request policy action
// for the withheld client fingerprint.
#[cfg(test)]
#[path = "request_drop_counter_tests.rs"]
pub(crate) mod request_drop_counter_tests;

// Background probe traffic must not mutate client-facing shared state: the
// unified-quota claim record and the context-management thinking cache.
#[cfg(test)]
#[path = "mod_background_isolation_tests.rs"]
mod background_isolation_tests;

#[cfg(test)]
#[path = "sse_usage_source_tests.rs"]
mod sse_usage_source_tests;

#[cfg(test)]
#[path = "mod_stream_terminal_tests.rs"]
mod stream_terminal_tests;

// The minted Claude Code identity (Stainless pack, default User-Agent,
// session id, client request id) is scoped to the exact Anthropic host.
#[cfg(test)]
#[path = "mod_identity_host_tests.rs"]
mod identity_host_tests;

#[cfg(test)]
#[path = "mod_redact_display_tests.rs"]
mod redact_display_tests;

// The router's withheld client betas on the `anthropic-beta` header.
#[cfg(test)]
#[path = "client_beta_withhold_tests.rs"]
mod client_beta_withhold_tests;

#[cfg(test)]
#[path = "client_beta_floor_site_tests.rs"]
mod client_beta_floor_site_tests;

// Client, provider-config and operator betas merge into one header list.
#[cfg(test)]
#[path = "client_beta_merge_tests.rs"]
mod client_beta_merge_tests;

// The floor and body-implied beta sets each carrier deposits per attempt.
#[cfg(all(test, feature = "bedrock"))]
#[path = "client_beta_assembly_tests.rs"]
mod client_beta_assembly_tests;

// A cloak relocation refusal halts all three dispatch paths before egress.
#[cfg(test)]
#[path = "mod_cloak_refusal_tests.rs"]
mod cloak_refusal_tests;

// Each send site deposits exactly the bytes the upstream received.
#[cfg(test)]
#[path = "mod_wire_tap_tests.rs"]
mod wire_tap_tests;
