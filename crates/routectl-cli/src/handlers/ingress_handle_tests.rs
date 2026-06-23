//! Unit + integration tests for `ingress_handle`. Split out so
//! `ingress_handle.rs` stays under the project's 800-line file
//! ceiling. Loaded via
//! `#[cfg(test)] #[path = "ingress_handle_tests.rs"] mod tests;` from
//! `ingress_handle.rs`. `super::*` resolves to the `ingress_handle`
//! module since this file is the body of `mod tests` declared inside
//! `ingress_handle`.
//!
//! Coverage:
//!
//! - `map_error` envelope shape per dialect (Anthropic, OpenAI).
//! - `sanitize_stream_error_for_client`: provider names + upstream
//!   bodies must not leak through to the SSE wire bytes.
//! - `render_stream_task`: mid-stream upstream error path drives the
//!   adapter's `render_error_eos` to emit a dialect-specific terminal
//!   error event AFTER the chunks already rendered.
//!
//! The integration tests in
//! `crates/routectl-cli/tests/anthropic_ingress.rs` cover the
//! end-to-end path through axum; these tests pin the in-process
//! mapping without needing a server.

use super::*;
use axum::body::to_bytes;
use routectl_core::Error;
use routectl_usage::{Outcome, UsageWriter, CHANNEL_CAPACITY};

/// A tempdir-backed usage writer + handle for capture tests. Holding the
/// `TempDir` keeps the DB path alive; `flush_and_read` drains the writer
/// and reads the single emitted row back so tests can assert the per-
/// outcome matrix against the persisted record (the real contract).
struct CaptureRig {
    handle: Option<UsageHandle>,
    writer: Option<UsageWriter>,
    db_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl CaptureRig {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("usage tempdir");
        let db_path = dir.path().join("usage.db");
        // retention_days=0 (no prune), enabled=true so try_send accepts.
        let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
        Self {
            handle: Some(handle),
            writer: Some(writer),
            db_path,
            _dir: dir,
        }
    }

    /// Build a `UsageCapture` over a draft for this rig's handle. The
    /// draft is seeded from `req` exactly as the production boundary does.
    fn capture(
        &self,
        dialect: &str,
        req: &routectl_core::ChatRequest,
        request_id: &str,
    ) -> UsageCapture {
        let draft = build_usage_draft(dialect, req, request_id.to_string(), None);
        let handle = self.handle.clone().expect("rig handle present");
        UsageCapture::new(draft, handle, dialect.to_string())
    }

    /// Drop the producer handle, drain the writer, and return every
    /// persisted row. The matrix tests assert exactly one row.
    ///
    /// `UsageWriter::shutdown` joins the writer thread and is blocking, so
    /// it must not run on a runtime worker. Offload it to a blocking thread
    /// via `spawn_blocking` (works on any runtime flavor, including the
    /// default current-thread one) rather than calling it inline.
    async fn flush_and_read(mut self) -> Vec<PersistedRow> {
        // Drop our handle clone so the channel can close once the writer
        // drops its own sender during shutdown.
        drop(self.handle.take());
        let writer = self.writer.take().expect("writer present");
        tokio::task::spawn_blocking(move || writer.shutdown())
            .await
            .expect("usage writer shutdown task");
        let db = routectl_usage::open(&self.db_path).expect("open usage db");
        read_rows(&db)
    }
}

/// The subset of persisted columns the matrix tests assert on.
#[derive(Debug)]
struct PersistedRow {
    request_id: String,
    outcome: String,
    ttfb_ms: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    attempt_count: i64,
    fallback_count: i64,
    provider: Option<String>,
    alias: String,
}

fn read_rows(db: &routectl_usage::UsageDb) -> Vec<PersistedRow> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT request_id, outcome, ttfb_ms, input_tokens, output_tokens, \
             attempt_count, fallback_count, provider, alias FROM requests \
             ORDER BY rowid",
        )
        .expect("prepare select");
    let rows = stmt
        .query_map([], |r| {
            Ok(PersistedRow {
                request_id: r.get(0)?,
                outcome: r.get(1)?,
                ttfb_ms: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                attempt_count: r.get(5)?,
                fallback_count: r.get(6)?,
                provider: r.get(7)?,
                alias: r.get(8)?,
            })
        })
        .expect("query rows")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("collect rows");
    rows
}

/// Minimal canonical request for capture tests.
fn sample_request(model: &str, stream: bool) -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: model.to_string(),
        messages: vec![message()],
        stream: Some(stream),
        ..Default::default()
    }
}

/// A bare user message for request/response fixtures (Message has no Default).
fn message() -> routectl_core::Message {
    use routectl_core::{MessageContent, Role};
    routectl_core::Message {
        role: Role::User,
        content: MessageContent::Text("hi".into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// A `DispatchMeta`-like fixture. The real type is `#[non_exhaustive]`
/// and built by the router, so capture tests that need a meta drive the
/// guard through `observe_meta` using a router-produced meta where
/// possible; where a synthetic meta is required the gate-blocked vs
/// upstream-error distinction is exercised via `outcome_for_dispatch_err`
/// over a real router dispatch in the integration tests. The unit tests
/// below drive the guard's token / outcome / ttfb stamping directly.
fn ok_response_with_usage(prompt: u32, completion: u32) -> routectl_core::ChatResponse {
    routectl_core::ChatResponse {
        id: "resp-1".into(),
        model: "m".into(),
        choices: vec![routectl_core::Choice {
            index: 0,
            message: message(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage: Some(routectl_core::Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn body_to_value(resp: Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn anthropic_envelope_unknown_alias_emits_not_found_error() {
    // Arrange
    let err = Error::UnknownAlias("nonesuch".into());

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("nonesuch"));
}

#[tokio::test]
async fn anthropic_envelope_validation_error_emits_invalid_request_error() {
    // Arrange
    let err = Error::Validation("max_tokens must be positive".into());

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("max_tokens"));
}

#[tokio::test]
async fn anthropic_envelope_5xx_emits_api_error_or_overloaded() {
    // 503 -> overloaded_error
    let err503 = Error::upstream("p", 503, "service unavailable");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err503);
    let status = resp.status();
    let body = body_to_value(resp).await;
    assert_eq!(status.as_u16(), 503);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");

    // 529 -> overloaded_error
    let err529 = Error::upstream("p", 529, "anthropic overloaded");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err529);
    assert_eq!(resp.status().as_u16(), 529);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "overloaded_error");

    // 502 -> api_error
    let err502 = Error::upstream("p", 502, "bad gateway");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err502);
    assert_eq!(resp.status().as_u16(), 502);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "api_error");
}

#[tokio::test]
async fn openai_envelope_unchanged_regression_pin() {
    // Pin the legacy OpenAI envelope shape so a future refactor
    // doesn't accidentally Anthropic-ify it. claude-code's
    // chat-completions adapter parses the flat `{"error":{...}}`
    // shape with `code` populated.
    let err = Error::UnknownAlias("nonesuch".into());

    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.get("type").is_none(), "OpenAI envelope is flat");
    assert_eq!(body["error"]["type"], "unknown_alias");
    assert_eq!(body["error"]["code"], "unknown_alias");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("nonesuch"));
}

// -------- map_error: non-streaming upstream message sanitization ----
//
// The non-streaming `map_error` path must not leak the internal
// provider config section name (routing topology) or the raw upstream
// response body (per-tenant rate-limit detail, upstream-side metadata)
// into the client-facing error envelope. It mirrors the streaming
// path's discipline: surface only the HTTP status plus, when the
// upstream body parsed as JSON with a top-level `error.message` /
// `error.type`, that short classifier.

#[tokio::test]
async fn map_error_upstream_strips_provider_name_and_raw_body() {
    // Arrange: an Upstream error whose Display string carries the
    // internal config section name and a raw body with tenant detail
    // in a sibling key the upstream did not intend for the client.
    let err = Error::Upstream {
        provider: "anthropic_oauth_prod".into(),
        status: 429,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        body: "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"},\
               \"internal_quota\":\"tenant-12345 exceeded 4000/min\"}"
            .into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        !msg.contains("anthropic_oauth_prod"),
        "internal provider config name must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("tenant-12345"),
        "raw upstream body must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("internal_quota"),
        "raw upstream body keys must not leak: {msg:?}"
    );
    assert!(
        msg.contains("429"),
        "HTTP status preserved for triage: {msg:?}"
    );
}

#[tokio::test]
async fn map_error_upstream_surfaces_top_level_error_message_from_json_body() {
    // Arrange: an upstream JSON body with a benign top-level
    // `error.message`. The sanitizer may surface that short message
    // (it is the upstream's own client-facing text) but never the
    // provider config name.
    let err = Error::Upstream {
        provider: "openai_prod".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        body: "{\"error\":{\"type\":\"invalid_request_error\",\
               \"message\":\"max_tokens is too large\"}}"
            .into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        !msg.contains("openai_prod"),
        "internal provider config name must not leak: {msg:?}"
    );
    assert!(
        msg.contains("max_tokens is too large"),
        "upstream top-level error.message should surface: {msg:?}"
    );
    assert!(
        msg.contains("400"),
        "HTTP status preserved for triage: {msg:?}"
    );
}

#[tokio::test]
async fn map_error_upstream_non_json_body_yields_status_only_message() {
    // Arrange: a non-JSON upstream body (e.g. an HTML 502 page or a
    // plain-text gateway error). There is no top-level error.message
    // to surface, so the sanitizer falls back to a status-only message
    // and drops the body entirely.
    let err = Error::Upstream {
        provider: "bedrock_prod".into(),
        status: 502,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        body: "<html><body>upstream-host-name gateway timeout</body></html>".into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        !msg.contains("bedrock_prod"),
        "internal provider config name must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("upstream-host-name"),
        "raw non-JSON body must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("<html>"),
        "raw body markup must not leak: {msg:?}"
    );
    assert!(
        msg.contains("502"),
        "HTTP status preserved for triage: {msg:?}"
    );
}

#[tokio::test]
async fn map_error_upstream_anthropic_envelope_also_sanitizes_message() {
    // Arrange: the Anthropic envelope path must sanitize the message
    // too -- both dialects funnel through the same `public_message`.
    let err = Error::Upstream {
        provider: "anthropic_oauth_prod".into(),
        status: 529,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        body: "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"},\
               \"x-internal\":\"tenant-99 burst\"}"
            .into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    // Assert
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        !msg.contains("anthropic_oauth_prod"),
        "internal provider config name must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("tenant-99"),
        "raw upstream body must not leak: {msg:?}"
    );
    assert!(msg.contains("529"), "HTTP status preserved: {msg:?}");
    // The Anthropic error.type mapping is unchanged by the message
    // sanitization (529 -> overloaded_error).
    assert_eq!(body["error"]["type"], "overloaded_error");
}

// -------- sanitize_upstream_for_client (unit) -----------------------

#[test]
fn sanitize_upstream_for_client_prefers_top_level_error_message() {
    // Arrange + Act
    let out = sanitize_upstream_for_client(
        400,
        "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad field\"}}",
    );

    // Assert
    assert!(out.contains("400"), "status present: {out:?}");
    assert!(out.contains("bad field"), "message surfaced: {out:?}");
}

#[test]
fn sanitize_upstream_for_client_falls_back_to_error_type() {
    // Arrange: a body with a top-level error.type but no message.
    // Act
    let out = sanitize_upstream_for_client(503, "{\"error\":{\"type\":\"overloaded_error\"}}");

    // Assert
    assert!(out.contains("503"), "status present: {out:?}");
    assert!(out.contains("overloaded_error"), "type surfaced: {out:?}");
}

#[test]
fn sanitize_upstream_for_client_status_only_for_non_json() {
    // Arrange + Act
    let out = sanitize_upstream_for_client(502, "raw gateway page with host names");

    // Assert
    assert!(out.contains("502"), "status present: {out:?}");
    assert!(
        !out.contains("host names"),
        "raw non-JSON body must not appear: {out:?}"
    );
}

#[test]
fn sanitize_upstream_for_client_status_only_for_json_without_error_object() {
    // Arrange: valid JSON but no top-level `error` object to mine.
    // Act
    let out = sanitize_upstream_for_client(500, "{\"detail\":\"tenant-7 internal trace\"}");

    // Assert
    assert!(out.contains("500"), "status present: {out:?}");
    assert!(
        !out.contains("tenant-7"),
        "sibling body keys must not leak: {out:?}"
    );
}

#[tokio::test]
async fn map_error_surfaces_anthropic_thinking_block_message_to_client() {
    // Arrange: the exact production 400 shape Anthropic returns when a
    // stale thinking-block signature is replayed. After the provider-side
    // fix, `read_anthropic_error` carries the RAW `{error:...}` envelope in
    // `.body` (not a pre-extracted bare string), so the ingress sanitizer
    // can re-extract the upstream's own `error.message`. A client (Claude
    // Code) needs to SEE this message to self-heal -- strip stale thinking
    // signatures and retry -- instead of hitting a status-only wall.
    let raw = "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\
                \"message\":\"messages.23.content.5: `thinking` or `redacted_thinking` \
                blocks in the latest assistant message cannot be modified. These blocks \
                must remain as they were in the original response.\"}}";
    let err = Error::Upstream {
        provider: "anthropic_oauth_prod".into(),
        status: 400,
        retry_after: None,
        upstream_type: Some("invalid_request_error".into()),
        upstream_code: None,
        body: raw.into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    // Assert: the actionable upstream message reaches the client, the
    // status is preserved, and the upstream error.type is lifted -- but the
    // internal provider config name never leaks.
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("cannot be modified"),
        "upstream self-heal message must reach the client: {msg:?}"
    );
    assert!(msg.contains("400"), "HTTP status preserved: {msg:?}");
    assert!(
        !msg.contains("anthropic_oauth_prod"),
        "internal provider config name must not leak: {msg:?}"
    );
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn map_error_non_json_upstream_body_stays_status_only() {
    // Arrange: a non-JSON upstream error body. The provider reader carries
    // a sanitized excerpt (not raw JSON) in `.body` for this case, so the
    // ingress sanitizer must fall back to a status-only client message and
    // never echo the raw body.
    let err = Error::Upstream {
        provider: "anthropic_oauth_prod".into(),
        status: 502,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        body: "<html>upstream-host gateway timeout</html>".into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    // Assert
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("502"), "HTTP status preserved: {msg:?}");
    assert!(
        !msg.contains("upstream-host"),
        "raw non-JSON body must not leak: {msg:?}"
    );
}

// -------- Layer B: OpenAI ingress preserves upstream type/code -------

#[tokio::test]
async fn openai_envelope_emits_upstream_type_when_present() {
    // Arrange: an upstream 429 carrying its own classifier.
    let err = Error::upstream_full(
        "p",
        429,
        "rate limited",
        None,
        Some("rate_limit_exceeded".into()),
        Some("rate_limited".into()),
    );

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert: the upstream type/code survive instead of "upstream_error".
    assert_eq!(body["error"]["type"], "rate_limit_exceeded");
    assert_eq!(body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn openai_envelope_falls_back_to_upstream_error_without_type() {
    // Arrange: an upstream error with no parsed classifier.
    let err = Error::upstream("p", 500, "boom");

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert: the legacy generic tag stays when no upstream type exists.
    assert_eq!(body["error"]["type"], "upstream_error");
    assert_eq!(body["error"]["code"], "upstream_error");
}

// -------- Layer C: Anthropic ingress non-stream status arms ----------

#[tokio::test]
async fn anthropic_envelope_maps_upstream_status_to_specific_types() {
    // 401 -> authentication_error
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 401, "nope"),
    );
    assert_eq!(resp.status().as_u16(), 401);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "authentication_error");

    // 403 -> permission_error
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 403, "nope"),
    );
    assert_eq!(resp.status().as_u16(), 403);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "permission_error");

    // 413 -> request_too_large
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 413, "too big"),
    );
    assert_eq!(resp.status().as_u16(), 413);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "request_too_large");

    // 503 -> overloaded_error (existing behavior preserved)
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 503, "down"),
    );
    assert_eq!(resp.status().as_u16(), 503);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn anthropic_envelope_passes_through_valid_upstream_type() {
    // Arrange: an upstream type that is already valid Anthropic vocab
    // wins over the status-derived guess (502 would otherwise be
    // api_error).
    let err = Error::upstream_full(
        "p",
        502,
        "slow down",
        None,
        Some("rate_limit_error".into()),
        None,
    );

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

/// Direct table-driven coverage of `anthropic_error_type`: every
/// status-derived arm maps to the expected Anthropic-vocabulary member.
/// Pins the full arm table -- including the 429 -> rate_limit_error arm
/// -- so a future edit to the match cannot silently drop or remap a row.
#[test]
fn anthropic_error_type_table_covers_every_arm() {
    use axum::http::StatusCode;

    let cases: &[(&str, u16, &str)] = &[
        ("unknown_alias", 404, "not_found_error"),
        ("unknown_provider", 404, "not_found_error"),
        ("bad_request", 400, "invalid_request_error"),
        ("validation_error", 422, "invalid_request_error"),
        ("payload_too_large", 413, "invalid_request_error"),
        ("unsupported_media_type", 415, "invalid_request_error"),
        ("auth_error", 401, "authentication_error"),
        ("authentication_error", 401, "authentication_error"),
        ("upstream_error", 401, "authentication_error"),
        ("upstream_error", 403, "permission_error"),
        ("upstream_error", 413, "request_too_large"),
        ("upstream_error", 429, "rate_limit_error"),
        ("upstream_error", 503, "overloaded_error"),
        ("upstream_error", 529, "overloaded_error"),
        ("upstream_error", 502, "api_error"),
        ("streaming_error", 500, "api_error"),
        ("bad_gateway", 502, "api_error"),
        ("something_else", 500, "api_error"),
    ];

    for (err_type, status, expected) in cases {
        let st = StatusCode::from_u16(*status).unwrap();
        let got = anthropic_error_type(err_type, st, None);
        assert_eq!(
            got, *expected,
            "({err_type}, {status}) should map to {expected}, got {got}"
        );
    }
}

/// A 429 upstream surfaces `rate_limit_error` (not `api_error`) on the
/// non-stream Anthropic envelope, so claude-code's per-`error.type`
/// backoff fires.
#[tokio::test]
async fn anthropic_envelope_429_maps_to_rate_limit_error() {
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 429, "slow down"),
    );
    assert_eq!(resp.status().as_u16(), 429);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

// -------- sanitize_stream_error_for_client --------------------------

/// The streaming-error sanitizer must NOT include the upstream
/// provider name or response body in the wire-bound message:
/// those are internal config / attacker-controlled bytes and
/// would leak through to the SDK consumer otherwise. Pin the
/// contract.
#[test]
fn sanitize_stream_error_strips_provider_and_body_from_upstream_error() {
    // Arrange
    let err = Error::upstream(
        "secret-provider-id",
        529,
        "Anthropic Overloaded: tenant-12345 exceeded quota",
    );

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert
    assert!(
        !safe.contains("secret-provider-id"),
        "provider name must not leak: {safe:?}"
    );
    assert!(
        !safe.contains("tenant-12345"),
        "upstream body must not leak: {safe:?}"
    );
    assert!(
        safe.contains("upstream stream error"),
        "kind tag present: {safe:?}"
    );
    assert!(
        safe.contains("529"),
        "HTTP status preserved for triage: {safe:?}"
    );
}

#[test]
fn sanitize_stream_error_uses_generic_message_for_streaming_kind() {
    // Arrange: Error::Streaming has no status; the sanitizer must
    // fall back to a generic "upstream stream error" string with
    // no internal detail.
    let err = Error::Streaming("anthropic in-stream error: overloaded_error".into());

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert
    assert_eq!(safe, "upstream stream error");
    assert!(!safe.contains("anthropic"));
    assert!(!safe.contains("overloaded"));
}

// -------- render_stream_task: mid-stream upstream error path --------

/// Build a one-text-token canonical chunk for use in stream tests.
fn streaming_text_chunk(text: &str) -> routectl_core::ChatChunk {
    use routectl_core::{ChunkChoice, ChunkDelta};
    routectl_core::ChatChunk {
        id: "msg_test".into(),
        model: "test-model".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
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

/// Drain all SseEvents from a closed receiver. Used by the
/// integration tests below to inspect the wire-bound event
/// sequence without going through axum.
async fn drain(mut rx: tokio::sync::mpsc::Receiver<SseEvent>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

/// Anthropic ingress, mid-stream upstream error: the receiver
/// must see the rendered chunk events FIRST (`message_start`,
/// `content_block_start`, `content_block_delta`), then the
/// terminal `event: error` event, then the channel closes.
/// Without this, the stream truncated mid-chunk and Claude Code
/// SDK would retry up to 5 times on suspected truncation.
#[tokio::test]
async fn render_stream_task_anthropic_emits_chunk_then_terminal_error_event() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: a synthesized upstream stream that yields one
    // chunk then an Upstream-shaped error.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hello")),
            Err(Error::upstream(
                "secret-provider-id",
                529,
                "Anthropic Overloaded: tenant-12345",
            )),
        ]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-stream-err");
    render_stream_task(upstream, AnthropicIngress, capture, tx).await;
    let events = drain(rx).await;

    // Assert: prefix chunk events + terminal error event.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().expect("Anthropic events are named"))
        .collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error",
        ],
        "expected chunk events + terminal error: {names:?}"
    );

    // The error event payload matches the Anthropic SSE spec.
    let err_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("error event present");
    let payload: Value = serde_json::from_str(&err_event.data).unwrap();
    assert_eq!(payload["type"], "error");
    // Layer D: a 529 upstream maps to overloaded_error, not api_error.
    assert_eq!(payload["error"]["type"], "overloaded_error");
    let msg = payload["error"]["message"].as_str().unwrap();
    // Sanitized: kind tag + status, NO provider id or body.
    assert!(msg.contains("upstream stream error"));
    assert!(msg.contains("529"));
    assert!(
        !msg.contains("secret-provider-id"),
        "provider must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("tenant-12345"),
        "upstream body must not leak: {msg:?}"
    );
}

/// OpenAI ingress, mid-stream upstream error: the receiver must
/// see the rendered chunk first, then the error chunk, then the
/// `[DONE]` terminator, then the channel closes. `[DONE]` is the
/// OpenAI universal stream terminator; without it the SDK
/// treats the close as a truncation and retries.
#[tokio::test]
async fn render_stream_task_openai_emits_chunk_then_error_chunk_then_done() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one Ok chunk then one Err.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hi")),
            Err(Error::upstream(
                "secret-provider-id",
                503,
                "Service Unavailable",
            )),
        ]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    let rig = CaptureRig::new();
    let capture = rig.capture("openai", &sample_request("m", true), "req-openai-err");
    render_stream_task(upstream, OpenAiIngress, capture, tx).await;
    let events = drain(rx).await;

    // Assert: three events. OpenAI emits unnamed (bare data:)
    // frames, so .event is None on each.
    assert_eq!(events.len(), 3, "expected chunk + error + [DONE]");
    assert!(events.iter().all(|e| e.event.is_none()));

    // Event 0: the rendered chunk's serialized JSON contains
    // the text content.
    assert!(
        events[0].data.contains("\"content\":\"hi\""),
        "chunk event 0 missing content delta: {:?}",
        events[0].data
    );
    // Event 1: error envelope.
    let err_payload: Value = serde_json::from_str(&events[1].data).unwrap();
    // Layer D: a 503 upstream maps to overloaded_error, not api_error.
    assert_eq!(err_payload["error"]["type"], "overloaded_error");
    let msg = err_payload["error"]["message"].as_str().unwrap();
    assert!(msg.contains("upstream stream error"));
    assert!(msg.contains("503"));
    assert!(
        !msg.contains("secret-provider-id"),
        "provider must not leak: {msg:?}"
    );
    // Event 2: the universal [DONE] terminator.
    assert_eq!(events[2].data, "[DONE]");
}

/// Counterpart: the natural EOS path is unchanged. This pins
/// that the helper still emits `render_eos` events (and not
/// the error variant) when the upstream stream finishes
/// without an error.
#[tokio::test]
async fn render_stream_task_natural_eos_emits_render_eos_not_error() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one Ok chunk, then upstream ends naturally.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![Ok(streaming_text_chunk("hi"))]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    let rig = CaptureRig::new();
    let capture = rig.capture("openai", &sample_request("m", true), "req-openai-eos");
    render_stream_task(upstream, OpenAiIngress, capture, tx).await;
    let events = drain(rx).await;

    // Assert: chunk + [DONE]. No error chunk.
    assert_eq!(events.len(), 2);
    assert!(events[0].data.contains("\"content\":\"hi\""));
    assert_eq!(events[1].data, "[DONE]");
    // Only [DONE] terminates a clean stream; if a stray error
    // envelope landed, we'd see three events.
    assert!(!events[0].data.contains("\"error\""));
}

/// Adapter wrapper that fails on the Nth call to `render_chunk`.
/// Used to drive path 3 of `render_stream_task` (chunk-render
/// failure) without simulating an upstream-stream Err. Delegates
/// every other trait method to the inner adapter so the wire
/// shapes (envelope, EOS, error EOS) match the wrapped dialect.
struct RenderChunkFailsOnceAdapter<A: IngressAdapter> {
    inner: A,
    calls: std::sync::atomic::AtomicUsize,
    fail_at_call: usize,
}

impl<A: IngressAdapter> RenderChunkFailsOnceAdapter<A> {
    fn new(inner: A, fail_at_call: usize) -> Self {
        Self {
            inner,
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_at_call,
        }
    }
}

impl<A: IngressAdapter> IngressAdapter for RenderChunkFailsOnceAdapter<A> {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        self.inner.error_envelope_shape()
    }
    fn parse_request(
        &self,
        headers: &HeaderMap,
        body: Value,
    ) -> routectl_core::Result<routectl_core::ChatRequest> {
        self.inner.parse_request(headers, body)
    }
    fn render_response(&self, resp: routectl_core::ChatResponse) -> routectl_core::Result<Value> {
        self.inner.render_response(resp)
    }
    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        self.inner.new_stream_state()
    }
    fn render_chunk(
        &self,
        chunk: routectl_core::ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == self.fail_at_call {
            return Err(Error::Streaming(
                "synthetic render_chunk failure for path-3 coverage".into(),
            ));
        }
        self.inner.render_chunk(chunk, state)
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        self.inner.render_eos(state)
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &crate::ingress::StreamErrorClass,
    ) -> Vec<SseEvent> {
        self.inner.render_error_eos(state, error, class)
    }
}

/// Path 3 of `render_stream_task`: the adapter's `render_chunk`
/// returns `Err` mid-stream. The driver must still emit the
/// dialect-specific terminal error event so SDK consumers see a
/// clean failure rather than a truncated stream. Mirrors
/// `render_stream_task_anthropic_emits_chunk_then_terminal_error_event`
/// but with the failure source on the ingress side rather than the
/// upstream stream. Pre-fix, this path returned without emitting
/// any terminator and the SDK's truncation-retry loop fired.
#[tokio::test]
async fn render_stream_task_anthropic_render_chunk_failure_emits_terminal_error() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: two Ok chunks. The wrapper fails on the second
    // render_chunk so the first chunk goes through cleanly and the
    // wire bytes mirror the upstream-error variant: one set of
    // canonical chunk events followed by the terminal error event.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hello")),
            Ok(streaming_text_chunk(" world")),
        ]));
    let adapter = RenderChunkFailsOnceAdapter::new(AnthropicIngress, 1);
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-render-fail");
    render_stream_task(upstream, adapter, capture, tx).await;
    let events = drain(rx).await;

    // Assert: prefix chunk events from the first chunk, then the
    // terminal error event.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().expect("Anthropic events are named"))
        .collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error",
        ],
        "expected first-chunk events + terminal error: {names:?}"
    );

    // The error event payload matches the Anthropic SSE spec.
    let err_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("error event present");
    let payload: Value = serde_json::from_str(&err_event.data).unwrap();
    assert_eq!(payload["type"], "error");
    assert_eq!(payload["error"]["type"], "api_error");
    let msg = payload["error"]["message"].as_str().unwrap();
    // sanitize_stream_error_for_client falls back to the generic
    // string for non-Upstream errors (Error::Streaming has no HTTP
    // status to surface).
    assert_eq!(msg, "upstream stream error");

    // The chunk-render failure observes the render error and finalizes
    // the row as `upstream_error` so the failure surfaces in
    // `routectl usage` as a non-ok outcome (instead of being mislabeled
    // as a bare client disconnect). Exactly one row, tagged for this
    // request.
    let rows = rig.flush_and_read().await;
    assert_eq!(rows.len(), 1, "exactly one row on render-failure exit");
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(rows[0].request_id, "req-render-fail");
}

// ============ UsageCapture: per-outcome matrix (the core contract) ====
//
// Each test drives ONE exit path through the guard and asserts EXACTLY
// ONE persisted row with the expected outcome. The gate_blocked vs
// upstream_error dispatch distinction (which depends on a router-built
// `DispatchMeta`, a `#[non_exhaustive]` type the router alone constructs)
// is covered end-to-end in `tests/server.rs` against a real dispatch.

/// Non-streaming clean completion -> exactly one `ok` row, with ttfb
/// populated (response ready == first byte) and tokens lifted from the
/// ChatResponse usage. request_id round-trips onto the row.
#[tokio::test]
async fn capture_non_stream_ok_emits_single_ok_row() {
    // Arrange
    let rig = CaptureRig::new();
    let req = sample_request("alias-x", false);
    let mut capture = rig.capture("openai", &req, "req-ok-1");

    // Act: simulate the production complete path.
    capture.mark_first_byte();
    capture.observe_response(&ok_response_with_usage(11, 7));
    capture.finalize(Outcome::Ok);
    drop(capture);
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1, "exactly one row per request");
    let row = &rows[0];
    assert_eq!(row.outcome, "ok");
    assert_eq!(row.request_id, "req-ok-1");
    assert_eq!(row.input_tokens, Some(11));
    assert_eq!(row.output_tokens, Some(7));
    assert!(
        row.ttfb_ms.is_some(),
        "ttfb populated when a response exists"
    );
}

/// Non-streaming render/serialization failure AFTER a good upstream
/// response: the row must be `upstream_error`, NOT `ok`. The guard sets
/// http_status=200 on `observe_response`, but a failed `render_response`
/// means the client receives an error, so `complete_response` calls
/// `observe_error` + `finalize(UpstreamError)` instead of the `ok`
/// finalize. This pins that exact ordering produces a single
/// `upstream_error` row. Pre-fix, `finalize(Ok)` fired before the render
/// check and this row would have said `ok` + http_status=200 while the
/// client got a 502.
#[tokio::test]
async fn capture_non_stream_render_failure_emits_upstream_error_row() {
    // Arrange
    let rig = CaptureRig::new();
    let req = sample_request("alias-x", false);
    let mut capture = rig.capture("openai", &req, "req-render-502");

    // Act: mirror complete_response's render-Err ordering. The response
    // was good (200 stamped) but serialization failed, so we observe the
    // render error and finalize as upstream_error WITHOUT a prior
    // finalize(Ok).
    capture.mark_first_byte();
    capture.observe_response(&ok_response_with_usage(5, 3));
    let render_err = Error::Internal("render serialization failed".into());
    capture.observe_error(&render_err);
    capture.finalize(Outcome::UpstreamError);
    drop(capture);
    let rows = rig.flush_and_read().await;

    // Assert: exactly one row, and it is upstream_error (not ok).
    assert_eq!(rows.len(), 1, "exactly one row per request");
    assert_eq!(
        rows[0].outcome, "upstream_error",
        "render failure must not persist outcome=ok"
    );
    assert_eq!(rows[0].request_id, "req-render-502");
}
#[tokio::test]
async fn capture_finalize_then_drop_does_not_double_send() {
    // Arrange
    let rig = CaptureRig::new();
    let req = sample_request("a", false);
    let mut capture = rig.capture("openai", &req, "req-once");

    // Act: explicit finalize, THEN drop.
    capture.observe_response(&ok_response_with_usage(1, 1));
    capture.finalize(Outcome::Ok);
    drop(capture); // Drop sees finalized=true -> no-op.
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1, "finalize + Drop must emit exactly one row");
    assert_eq!(rows[0].outcome, "ok");
}

/// A guard dropped without an explicit finalize (client hangup / task
/// cancellation) -> exactly one `client_disconnect` row via the Drop
/// fallback.
#[tokio::test]
async fn capture_drop_without_finalize_emits_client_disconnect() {
    // Arrange
    let rig = CaptureRig::new();
    let req = sample_request("a", true);
    let capture = rig.capture("anthropic", &req, "req-dropped");

    // Act: drop without finalizing.
    drop(capture);
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "client_disconnect");
    assert_eq!(rows[0].request_id, "req-dropped");
    assert!(
        rows[0].ttfb_ms.is_none(),
        "no first byte was marked -> ttfb None"
    );
}

/// Streaming natural EOS through `render_stream_task` -> exactly one `ok`
/// row, ttfb populated (first chunk), tokens lifted from the chunk usage.
#[tokio::test]
async fn capture_stream_natural_eos_emits_single_ok_row() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one usage-bearing chunk, then natural EOS.
    let chunk = {
        let mut c = streaming_text_chunk("hi");
        c.usage = Some(routectl_core::UsageDelta {
            prompt_tokens: Some(9),
            completion_tokens: Some(4),
            total_tokens: Some(13),
            ..Default::default()
        });
        c
    };
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![Ok(chunk)]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    let rig = CaptureRig::new();
    let capture = rig.capture("openai", &sample_request("a", true), "req-stream-ok");

    // Act
    render_stream_task(upstream, OpenAiIngress, capture, tx).await;
    let _ = drain(rx).await;
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1, "exactly one row per stream");
    assert_eq!(rows[0].outcome, "ok");
    assert_eq!(rows[0].request_id, "req-stream-ok");
    assert_eq!(rows[0].input_tokens, Some(9));
    assert_eq!(rows[0].output_tokens, Some(4));
    assert!(rows[0].ttfb_ms.is_some(), "first chunk sets ttfb");
}

/// Streaming mid-stream upstream error through `render_stream_task` ->
/// exactly one `upstream_error` row (one Ok chunk then an Err).
#[tokio::test]
async fn capture_stream_mid_stream_error_emits_upstream_error_row() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one chunk, then a mid-stream upstream error.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("partial")),
            Err(Error::upstream("p", 503, "boom")),
        ]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    let rig = CaptureRig::new();
    let capture = rig.capture("openai", &sample_request("a", true), "req-stream-mid-err");

    // Act
    render_stream_task(upstream, OpenAiIngress, capture, tx).await;
    let _ = drain(rx).await;
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(rows[0].request_id, "req-stream-mid-err");
}

/// `outcome_for_dispatch_err` over a real router-built `DispatchMeta`:
/// an unreachable provider yields a meta with `attempt_count > 0`, which
/// must map to `upstream_error` (NOT gate_blocked). Drives the actual
/// router so the meta is genuine, not synthesized.
#[tokio::test]
async fn dispatch_err_with_attempts_maps_to_upstream_error() {
    use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // Arrange: a single unreachable provider, no fallback, one attempt.
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", "literal:k"),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let config = Arc::new(Config {
        providers,
        aliases,
        models,
        retry,
        ..Default::default()
    });
    let router = build_test_router(config).await;

    // Act: drive a real dispatch THROUGH the capture guard so the
    // persisted row carries the router-built DispatchMeta fields.
    let rig = CaptureRig::new();
    let req = sample_request("a", false);
    let mut capture = rig.capture("openai", &req, "req-upstream-err");
    let dispatched = router
        .complete_with_options(req.clone(), Default::default())
        .await;
    capture.observe_meta(&dispatched.meta);
    let err = dispatched
        .result
        .expect_err("unreachable provider must error");
    capture.observe_error(&err);
    let mapped = outcome_for_dispatch_err(&dispatched.meta);
    capture.finalize(mapped);
    drop(capture);
    let rows = rig.flush_and_read().await;

    // Assert: a real upstream attempt was made, so attempts > 0 ->
    // upstream_error (the dispatch-failure mapping under test). The
    // DispatchMeta fields (attempt_count, fallback_count, provider,
    // alias) land on the persisted row.
    assert_eq!(mapped, Outcome::UpstreamError);
    assert!(
        dispatched.meta.attempt_count > 0,
        "an upstream attempt was charged: {:?}",
        dispatched.meta.attempt_count
    );
    assert_eq!(rows.len(), 1, "exactly one row");
    let row = &rows[0];
    assert_eq!(row.outcome, "upstream_error");
    assert_eq!(row.request_id, "req-upstream-err");
    assert_eq!(row.alias, "a", "resolved_alias lands on the row");
    assert_eq!(row.provider.as_deref(), Some("p"), "served_provider lands");
    assert_eq!(row.attempt_count, dispatched.meta.attempt_count as i64);
    assert_eq!(row.fallback_count, dispatched.meta.fallback_count as i64);
}

/// `outcome_for_dispatch_err` for a gate-blocked dispatch: with the RPM
/// gate set to 1/min, the SECOND dispatch is refused BEFORE any upstream
/// contact, so `attempt_count == 0` -> gate_blocked. Single-entry chain
/// so there is no fallback to bump the attempt count.
#[tokio::test]
async fn dispatch_err_gate_blocked_maps_to_gate_blocked() {
    use routectl_router::{
        AliasValue, Config, ModelEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // Arrange: rpm_limit=1 so the second dispatch is RPM-gated.
    let mut runtime = ProviderRuntimePolicy::default();
    runtime.rpm_limit = Some(1);
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", "literal:k").with_runtime(runtime),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let config = Arc::new(Config {
        providers,
        aliases,
        models,
        retry,
        ..Default::default()
    });
    let router = build_test_router(config).await;

    // Act: first dispatch consumes the only RPM token (and fails against
    // the unreachable upstream); the second is gate-blocked pre-dispatch.
    let _first = router
        .complete_with_options(sample_request("a", false), Default::default())
        .await;
    let dispatched = router
        .complete_with_options(sample_request("a", false), Default::default())
        .await;

    // Assert: gate refused before any upstream contact on the second.
    assert!(dispatched.result.is_err());
    assert_eq!(
        dispatched.meta.attempt_count, 0,
        "gate fires before any upstream attempt"
    );
    assert_eq!(
        outcome_for_dispatch_err(&dispatched.meta),
        Outcome::GateBlocked
    );
}

/// Build a `Router` from `config` for the dispatch-meta tests, wiring an
/// in-memory secret store (the `literal:` ref resolves through it).
async fn build_test_router(
    config: std::sync::Arc<routectl_router::Config>,
) -> routectl_router::Router {
    use routectl_auth::{MemoryStore, SecretStore};
    use std::sync::Arc;
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    crate::server::build_router_from_config(config, secrets)
        .await
        .expect("build router")
}
