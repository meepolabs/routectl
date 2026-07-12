//! Ingress adapter trait and shared types.
//!
//! routectl supports multiple ingress dialects: OpenAI Chat Completions
//! (`/v1/chat/completions`) and, in v0.4.0, Anthropic Messages
//! (`/v1/messages`). Each adapter knows how to:
//!
//! 1. Parse a request body into the canonical `ChatRequest`.
//! 2. Render a canonical `ChatResponse` into wire JSON for the client.
//! 3. Render a canonical `ChatChunk` into one or more SSE events.
//! 4. Produce an end-of-stream marker (e.g. OpenAI's `[DONE]`,
//!    Anthropic's `message_stop`).
//!
//! The trait is small on purpose: anything beyond translation belongs in
//! the router (alias resolution, retry, fallback) or core.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ContentPart, Error, KnownContentPart, MessageContent,
    Result, Role, SystemBlock, SystemContent,
};
use serde_json::Value;

pub mod anthropic;
pub mod openai;
pub mod openai_responses;
pub mod token_estimate;

/// Header used by harnesses that can override the canonical `model`
/// field directly to pin routing to a specific configured alias.
pub const ALIAS_HEADER: &str = "x-routectl-alias";

/// Seam header the MITM front-proxy (`proxy::split`) stamps on the
/// re-injected api.anthropic.com inference leg, and ONLY that leg. It is a
/// HINT consumed by the forwarded-mode capture gate in
/// `handlers::ingress_handle`; the config-side per-provider forwarded
/// credential (`Router::has_forwarded_provider`) is the real authority.
/// Shared between the proxy set site and the ingress read site so the two
/// cannot drift on the literal.
pub(crate) const MITM_PROXIED_HEADER: &str = "x-routectl-mitm-proxied";

/// Number of random bytes backing [`MitmSeamNonce`] -- 128 bits, ample
/// guess-resistance for a loopback-only, per-process value whose only job is
/// proving "this request came through the reinject leg", not resisting a
/// cryptographic adversary.
const MITM_SEAM_NONCE_BYTES: usize = 16;

/// Per-process random value that makes [`MITM_PROXIED_HEADER`] unspoofable
/// in practice. The MITM front-proxy's re-inject leg (`proxy::split`) is the
/// ONLY legitimate stamper: it stamps this exact value, generated once at
/// server bootstrap (`server::serve_on_listener_with_overlay`) and shared
/// with every ingress checker (`handlers::pure_proxy_admission`,
/// `handlers::ingress_handle`) via `AppState`.
///
/// Bare `headers.contains_key(...)` used to be the whole check -- any direct
/// caller could set the header and be treated as having arrived through the
/// MITM inference path. [`MitmSeamNonce::is_present_in`] instead compares the
/// header's VALUE against this nonce: a wrong value and a missing header are
/// BOTH `false` (indistinguishable seam-absent), so a spoofer gains nothing
/// over sending no header at all.
///
/// Plain byte-equality (not constant-time) is deliberate: the threat model
/// is a local, loopback-only caller guessing an exact 128-bit value, not a
/// remote timing side channel.
pub struct MitmSeamNonce(String);

impl MitmSeamNonce {
    /// Generate a fresh nonce from the OS CSPRNG. Panics if the CSPRNG
    /// fails -- mirrors `routectl_auth::oauth::pkce::Pkce::generate`: on
    /// Linux/macOS this means the kernel is out of entropy at boot, which is
    /// unrecoverable for any cryptographic use in this process anyway.
    pub fn generate() -> Self {
        let mut bytes = [0u8; MITM_SEAM_NONCE_BYTES];
        SysRng
            .try_fill_bytes(&mut bytes)
            .expect("SysRng failed to fill the MITM seam nonce");
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// The exact `HeaderValue` the reinject leg stamps and every checker
    /// compares against.
    pub fn header_value(&self) -> HeaderValue {
        HeaderValue::from_str(&self.0).expect("base64url output is always a valid header value")
    }

    /// `true` only when `headers` carries [`MITM_PROXIED_HEADER`] with a
    /// value that matches this nonce byte-for-byte. A wrong value and an
    /// absent header both return `false`.
    pub(crate) fn is_present_in(&self, headers: &HeaderMap) -> bool {
        headers
            .get(MITM_PROXIED_HEADER)
            .is_some_and(|v| v.as_bytes() == self.0.as_bytes())
    }
}

impl std::fmt::Debug for MitmSeamNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MitmSeamNonce(<redacted>)")
    }
}

/// Fallback wire `error.type` for a non-status error -- a render or
/// transport failure that carries no HTTP status to classify. Status-
/// bearing upstream errors instead map through `anthropic_error_type`.
/// Matches the Anthropic SSE `error.type` vocabulary
/// (`api_error` / `overloaded_error` / ...) which both OpenAI and
/// Anthropic clients tolerate. Routed through one constant so the
/// dialect impls and their tests cannot drift on the magic string.
pub(crate) const STREAM_ERROR_TYPE: &str = "api_error";

/// Resolved classifier for a mid-stream terminal error event, derived
/// from the upstream `Error` so the terminal SSE event carries a
/// status-aware (and, when present, upstream-aware) `error.type` instead
/// of a hardcoded `api_error`. Built once in the handler and handed to
/// each dialect's `render_error_eos`, which reads the field matching its
/// wire shape.
///
/// - `anthropic_type`: the Anthropic-vocabulary `error.type` (delegated
///   to `handlers::ingress_handle::anthropic_error_type` so stream and
///   non-stream agree on every status -> vocab mapping, including
///   `rate_limit_error` for 429, `overloaded_error` for 503/529, and
///   upstream-supplied valid-vocab pass-through).
/// - `openai_type` / `openai_code`: the upstream's own classifier when
///   present, else the Anthropic type / a generic fallback -- mirroring
///   the non-stream OpenAI envelope so stream and non-stream agree.
#[derive(Debug, Clone)]
pub struct StreamErrorClass {
    pub anthropic_type: String,
    pub openai_type: String,
    pub openai_code: String,
}

impl StreamErrorClass {
    /// Derive the terminal-error classifier from the upstream `Error`.
    /// Non-`Upstream` errors (render failures, transport errors with no
    /// HTTP status) fall back to the generic `api_error` bucket.
    pub(crate) fn from_error(e: &Error) -> Self {
        match e {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                ..
            } => {
                let st = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                let anthropic_type = crate::handlers::ingress_handle::anthropic_error_type(
                    "upstream_error",
                    st,
                    upstream_type.as_deref(),
                )
                .to_string();
                let openai_type = upstream_type
                    .clone()
                    .unwrap_or_else(|| anthropic_type.clone());
                let openai_code = upstream_code
                    .clone()
                    .or_else(|| upstream_type.clone())
                    .unwrap_or_else(|| anthropic_type.clone());
                Self {
                    anthropic_type,
                    openai_type,
                    openai_code,
                }
            }
            _ => Self {
                anthropic_type: STREAM_ERROR_TYPE.to_string(),
                openai_type: STREAM_ERROR_TYPE.to_string(),
                openai_code: STREAM_ERROR_TYPE.to_string(),
            },
        }
    }
}

/// If any `Role::System` messages are in `req.messages`, lift their text
/// content into `req.system` and remove them from the messages array.
/// No-op when there are no System messages.
///
/// Lifting strategy:
/// - `MessageContent::Text` messages contribute a `SystemBlock` with no
///   `cache_control` (same as before v0.7.0).
/// - `MessageContent::Parts` messages contribute one `SystemBlock` per text
///   part, preserving `cache_control` and `citations` from each part.
///
/// Output shape:
/// - When no lifted block carries `cache_control` or `citations`, the result
///   is concatenated into `SystemContent::Text` (backward-compatible).
/// - When any block carries `cache_control` or `citations`, the result is
///   `SystemContent::Blocks` so the per-block cache breakpoints survive the
///   ingress boundary and reach the Anthropic / Bedrock egress intact.
pub(crate) fn lift_system_messages(req: &mut ChatRequest) {
    let mut lifted_blocks: Vec<SystemBlock> = Vec::new();
    req.messages.retain(|m| {
        if !matches!(m.role, Role::System) {
            return true;
        }
        match &m.content {
            MessageContent::Text(t) => {
                lifted_blocks.push(SystemBlock {
                    kind: "text".into(),
                    text: t.clone(),
                    cache_control: None,
                    citations: None,
                });
            }
            MessageContent::Parts(parts) => {
                // Non-text parts (images, documents) in a System message are
                // not meaningful in canonical and would be dropped by egresses
                // that do not support them; skip them here as before.
                for p in parts {
                    if let ContentPart::Known(KnownContentPart::Text {
                        text,
                        cache_control,
                    }) = p
                    {
                        lifted_blocks.push(SystemBlock {
                            kind: "text".into(),
                            text: text.clone(),
                            cache_control: cache_control.clone(),
                            citations: None,
                        });
                    }
                }
            }
            MessageContent::Null => {}
        }
        false
    });

    if lifted_blocks.is_empty() {
        return;
    }

    let needs_blocks = lifted_blocks
        .iter()
        .any(|b| b.cache_control.is_some() || b.citations.is_some());

    match req.system.take() {
        Some(SystemContent::Text(existing)) if !needs_blocks => {
            let lifted_text = lifted_blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            req.system = Some(SystemContent::Text(format!("{existing}\n{lifted_text}")));
        }
        Some(SystemContent::Text(existing)) => {
            let mut blocks = vec![SystemBlock {
                kind: "text".into(),
                text: existing,
                cache_control: None,
                citations: None,
            }];
            blocks.extend(lifted_blocks);
            req.system = Some(SystemContent::Blocks(blocks));
        }
        Some(SystemContent::Blocks(mut blocks)) => {
            blocks.extend(lifted_blocks);
            req.system = Some(SystemContent::Blocks(blocks));
        }
        None if !needs_blocks => {
            let lifted_text = lifted_blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            req.system = Some(SystemContent::Text(lifted_text));
        }
        None => {
            req.system = Some(SystemContent::Blocks(lifted_blocks));
        }
    }
}

/// Read the `x-routectl-alias` header. Returns the trimmed header
/// value when present and non-empty; otherwise `None`. The ingress
/// uses this to override the wire `model` field when the client
/// can't easily change it (e.g. Claude Code, the OpenAI SDK behind
/// a fixed `model:` config).
///
/// v0.6.0 collapsed the `[ingress.X.aliases]` per-dialect maps into
/// the top-level `[aliases]` table. The ingress is now alias-agnostic:
/// it reads the wire `model` value (or the override header) and
/// forwards it verbatim to the router, which does all the alias
/// resolution. See `routectl_router::Router::resolve_v6_alias`.
pub fn read_alias_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ALIAS_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod read_alias_header_tests {
    use super::*;

    fn h(name: &str, val: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            val.parse().unwrap(),
        );
        h
    }

    #[test]
    fn header_with_value_returns_trimmed_string() {
        assert_eq!(
            read_alias_header(&h(ALIAS_HEADER, "  fast  ")),
            Some("fast".into())
        );
    }

    #[test]
    fn missing_header_returns_none() {
        assert_eq!(read_alias_header(&HeaderMap::new()), None);
    }

    #[test]
    fn empty_header_value_returns_none() {
        assert_eq!(read_alias_header(&h(ALIAS_HEADER, "   ")), None);
    }
}

#[cfg(test)]
mod mitm_seam_nonce_tests {
    use axum::http::{HeaderName, HeaderValue};

    use super::*;

    fn headers_with(name: &str, value: HeaderValue) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HeaderName::from_bytes(name.as_bytes()).unwrap(), value);
        h
    }

    #[test]
    fn is_present_in_matches_the_exact_stamped_value() {
        let nonce = MitmSeamNonce::generate();
        let headers = headers_with(MITM_PROXIED_HEADER, nonce.header_value());
        assert!(nonce.is_present_in(&headers));
    }

    #[test]
    fn is_present_in_rejects_a_spoofed_wrong_value() {
        let nonce = MitmSeamNonce::generate();
        let headers = headers_with(MITM_PROXIED_HEADER, HeaderValue::from_static("1"));
        assert!(
            !nonce.is_present_in(&headers),
            "a bare/spoofed header value must never match the real nonce"
        );
    }

    #[test]
    fn is_present_in_rejects_a_different_processs_nonce() {
        let a = MitmSeamNonce::generate();
        let b = MitmSeamNonce::generate();
        let headers = headers_with(MITM_PROXIED_HEADER, b.header_value());
        assert!(!a.is_present_in(&headers));
    }

    #[test]
    fn is_present_in_is_false_when_the_header_is_absent() {
        let nonce = MitmSeamNonce::generate();
        assert!(!nonce.is_present_in(&HeaderMap::new()));
    }

    #[test]
    fn two_generates_produce_different_values() {
        let a = MitmSeamNonce::generate();
        let b = MitmSeamNonce::generate();
        assert_ne!(a.header_value(), b.header_value());
    }

    #[test]
    fn debug_never_renders_the_raw_value() {
        let nonce = MitmSeamNonce::generate();
        let rendered = format!("{nonce:?}");
        assert!(!rendered.contains(&nonce.0));
        assert!(rendered.contains("<redacted>"));
    }
}

#[cfg(test)]
mod stream_error_class_tests {
    use super::*;

    /// The stream-path classifier delegates to the same status -> vocab
    /// table as the non-stream envelope, so a 429 upstream surfaces
    /// `rate_limit_error` on the terminal SSE error event instead of the
    /// generic `api_error`. Pins parity with the non-stream path.
    #[test]
    fn upstream_429_maps_to_rate_limit_error() {
        let class = StreamErrorClass::from_error(&Error::upstream("p", 429, "slow down"));
        assert_eq!(class.anthropic_type, "rate_limit_error");
    }

    /// Regression guard: 401/403/413 must keep their specific Anthropic
    /// types on the stream path (they previously collapsed to
    /// `api_error` because the stream path hand-rolled a narrower match).
    #[test]
    fn upstream_status_maps_to_specific_types() {
        let cases: &[(u16, &str)] = &[
            (401, "authentication_error"),
            (403, "permission_error"),
            (413, "request_too_large"),
            (503, "overloaded_error"),
            (529, "overloaded_error"),
        ];
        for (status, expected) in cases {
            let class = StreamErrorClass::from_error(&Error::upstream("p", *status, "x"));
            assert_eq!(
                class.anthropic_type, *expected,
                "{status} should map to {expected}"
            );
        }
    }

    /// A valid upstream-supplied Anthropic vocab member wins over the
    /// status-derived guess (502 would otherwise be api_error).
    #[test]
    fn valid_upstream_type_passes_through() {
        let err = Error::upstream_full("p", 502, "x", None, Some("rate_limit_error".into()), None);
        let class = StreamErrorClass::from_error(&err);
        assert_eq!(class.anthropic_type, "rate_limit_error");
    }

    /// Non-`Upstream` errors (no HTTP status) fall back to the generic
    /// `api_error` bucket on every field.
    #[test]
    fn non_upstream_error_falls_back_to_api_error() {
        let class = StreamErrorClass::from_error(&Error::Streaming("render failed".into()));
        assert_eq!(class.anthropic_type, STREAM_ERROR_TYPE);
        assert_eq!(class.openai_type, STREAM_ERROR_TYPE);
        assert_eq!(class.openai_code, STREAM_ERROR_TYPE);
    }
}

/// One server-sent event ready to write to the response stream.
///
/// `event` carries the SSE `event:` field for named events (Anthropic
/// emits `message_start`, `content_block_delta`, etc.). OpenAI omits
/// the field, which serializes as a bare `data: ...` frame.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn unnamed(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }
}

/// Per-stream state. Each ingress adapter defines a concrete state
/// type; the handler holds a `Box<dyn IngressStreamState>` while the
/// stream runs. Marker trait so we don't leak concrete state shapes
/// into the trait surface.
pub trait IngressStreamState: Send {
    /// Allow downcasting to the concrete state when the adapter needs
    /// to read its own typed fields. Implementations should return
    /// `self` cast to `&mut dyn Any`.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Wire shape used to render error responses for a given ingress
/// dialect. The Anthropic ingress emits Anthropic's standard error
/// envelope (`{"type":"error","error":{"type","message"}}`); the
/// OpenAI ingress emits OpenAI's (`{"error":{"message","type","code"}}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorEnvelopeShape {
    OpenAi,
    Anthropic,
}

/// Request-derived context handed to `IngressAdapter::new_stream_state`
/// so a dialect's initial stream state can carry request-scoped values
/// the upstream chunk stream does not surface early enough. The handler
/// builds this from the `ChatRequest` before dispatch moves the request
/// into the router.
///
/// Today it carries:
/// - `input_tokens_estimate`: a local heuristic count (see
///   `token_estimate`) so the synthesized early `message_start` frame
///   can report a non-zero `usage.input_tokens` on the pre-inversion
///   fast path instead of a stuck-at-zero context meter.
/// - `model`: the resolved request model, used as the early-frame model
///   id when the upstream stream's first chunk carries no model string.
///
/// The Anthropic adapter reads both; the OpenAI and Responses adapters
/// ignore the context entirely (their state carries neither field).
#[derive(Debug, Clone, Default)]
pub struct StreamRequestContext {
    pub input_tokens_estimate: u64,
    pub model: String,
}

/// Translation surface for one ingress dialect. See module docs.
pub trait IngressAdapter: Send + Sync {
    fn id(&self) -> &str;

    /// Wire shape this ingress uses to render error envelopes. The
    /// generic ingress driver (`handlers::ingress_handle`) branches on
    /// this so 4xx/5xx responses match the dialect the client expects:
    /// Anthropic clients (claude-code, official SDK) parse
    /// `{"type":"error","error":{...}}`; OpenAI clients parse the
    /// flat `{"error":{...}}`. NO default impl: every adapter must
    /// declare its envelope so the choice is reviewable in code.
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape;

    /// Parse an incoming JSON body + headers into the canonical
    /// `ChatRequest`. Errors map to 4xx in the handler.
    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest>;

    /// Render a canonical `ChatResponse` into wire JSON for the client.
    fn render_response(&self, resp: ChatResponse) -> Result<Value>;

    /// Initial state for a new streaming response. `ctx` carries
    /// request-scoped values (local input-token estimate, resolved
    /// model) the dialect state may need before the upstream stream
    /// surfaces them; adapters that need neither ignore it.
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState>;

    /// SSE events to flush as the FIRST body bytes when the handler goes
    /// warm-hold (the dispatch outran the early-flush grace window).
    /// Emitting these BEFORE awaiting the still-pending dispatch is what
    /// forces the response head out and defeats the client-side headers
    /// wall: a first body-stream poll that blocks on the dispatch never
    /// flushes the head. Emit-then-dispatch is the invariant the warm
    /// render task relies on.
    ///
    /// The Anthropic adapter emits a synthesized `message_start` carrying
    /// the local input-token estimate from `state` and marks the state
    /// started, so the real first-content chunk dedups it (no duplicate
    /// `message_start`). The OpenAI and Responses dialects emit nothing
    /// here -- their axum `KeepAlive` comment frame is the flush trigger.
    /// Default: no-op, so a dialect with no synthetic head compiles
    /// against the trait unchanged.
    fn early_frame(&self, _state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        Vec::new()
    }

    /// Render one canonical chunk into zero or more SSE events. Adapters
    /// own per-stream state via `state` (block-index counter for
    /// Anthropic, no-op for OpenAI).
    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>>;

    /// Final SSE events to emit after the upstream stream ends. OpenAI
    /// emits `[DONE]`; Anthropic may emit a final `message_stop` if
    /// the stream ended without an explicit stop event.
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent>;

    /// Final SSE events to emit when an upstream stream errored
    /// mid-stream. Returns dialect-specific TERMINAL ERROR events so
    /// SDK consumers see a clean failure rather than network
    /// truncation. This is NOT the same as `render_eos`: that emits a
    /// SUCCESS terminator (`[DONE]` / `message_stop`) which would
    /// falsely signal a clean completion. The error variant emits a
    /// FAILURE terminator that the SDK can distinguish.
    ///
    /// `error` carries a sanitized client-safe summary of the failure.
    /// The caller in `handlers::ingress_handle` strips provider names,
    /// upstream response bodies, and tokens before passing it here,
    /// but adapters MUST further filter via
    /// `routectl_core::sanitize_for_log` to drop control characters
    /// that would otherwise break SSE wire framing or forge log lines
    /// on text-format subscribers downstream.
    ///
    /// `class` carries the status-aware (and, when present,
    /// upstream-aware) `error.type` / `error.code` so the terminal event
    /// reflects the real failure class (`overloaded_error` for 503/529,
    /// the upstream's own type when valid) instead of a hardcoded
    /// `api_error`. Each adapter reads the field matching its wire shape.
    ///
    /// Wire shapes:
    ///
    /// - Anthropic: one `event: error` named SSE event with payload
    ///   `{"type":"error","error":{"type":<class.anthropic_type>,"message":...}}`.
    ///   The error event is itself terminal in the Anthropic SSE
    ///   format -- no further events follow.
    /// - OpenAI: one bare `data: {"error":{...}}` chunk followed by
    ///   one bare `data: [DONE]` chunk. OpenAI clients consume
    ///   `[DONE]` as the universal stream terminator; the preceding
    ///   error chunk tells the SDK the stream failed cleanly.
    fn render_error_eos(
        &self,
        _state: &mut dyn IngressStreamState,
        _error: &dyn std::fmt::Display,
        _class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        // Default no-op so a third-party adapter that does not yet have
        // a dialect-specific error envelope can compile against the
        // trait. The handler in `handlers::ingress_handle` falls back
        // to a network-truncation close (the Drop summary still emits
        // `finish_reason=truncated`) when an adapter returns no events.
        // First-party adapters (`openai.rs`, `anthropic/`) override.
        Vec::new()
    }
}
