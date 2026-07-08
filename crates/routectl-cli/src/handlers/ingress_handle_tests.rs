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
use routectl_usage::{CHANNEL_CAPACITY, Outcome, UsageWriter};

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
    /// `extra.stream_stage`, parsed from the JSON `extra` column. `None`
    /// when no stage marker was stamped (fast HTTP-status failures, clean
    /// completions). Distinguishes a warm-hold pre-content dispatch failure
    /// (`pre_content_dispatch`) from a mid-stream cut (`mid_stream`).
    stream_stage: Option<String>,
}

fn read_rows(db: &routectl_usage::UsageDb) -> Vec<PersistedRow> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT request_id, outcome, ttfb_ms, input_tokens, output_tokens, \
             attempt_count, fallback_count, provider, alias, extra FROM requests \
             ORDER BY rowid",
        )
        .expect("prepare select");

    stmt.query_map([], |r| {
        let extra: Option<String> = r.get(9)?;
        let stream_stage = extra
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| {
                v.get("stream_stage")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
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
            stream_stage,
        })
    })
    .expect("query rows")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("collect rows")
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
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("nonesuch")
    );
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
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("max_tokens")
    );
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
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("nonesuch")
    );
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
    render_stream_task(
        upstream,
        AnthropicIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
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
    render_stream_task(
        upstream,
        OpenAiIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
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
    render_stream_task(
        upstream,
        OpenAiIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
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
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        self.inner.new_stream_state(ctx)
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
    render_stream_task(
        upstream,
        adapter,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
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
    render_stream_task(
        upstream,
        OpenAiIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
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
    render_stream_task(
        upstream,
        OpenAiIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    )
    .await;
    let _ = drain(rx).await;
    let rows = rig.flush_and_read().await;

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(rows[0].request_id, "req-stream-mid-err");
    assert_eq!(
        rows[0].stream_stage.as_deref(),
        Some("mid_stream"),
        "a genuine mid-stream cut must not collapse into the pre_content_dispatch stage"
    );
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

/// Live K-sample recording through the capture guard: a completed dispatch
/// whose served target is known lands one sample in the router's per-session
/// K store when the request carried a session key, and lands NONE when it
/// did not. Drives the same `observe_meta` -> `record_k_sample` sequence the
/// handler runs, against a real router-built `DispatchMeta`.
#[tokio::test]
async fn record_k_sample_lands_keyed_sample_and_skips_keyless() {
    use routectl_router::{
        AliasValue, Config, KSessionKey, ModelEntry, ProviderEntry, RetryPolicy,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // Arrange: a single-entry chain pointed at an unreachable upstream.
    // The dispatch fails, but the chain walk still stamps the served
    // provider_kind / model onto the meta -- enough for a recording.
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
    let router = Arc::new(build_test_router(config).await);

    // Act (keyed): run a dispatch, stamp the meta onto a capture, then
    // record with a session key.
    let rig = CaptureRig::new();
    let req = sample_request("a", false);
    let mut capture = rig.capture("openai", &req, "req-k");
    let dispatched = router.complete_with_options(req, Default::default()).await;
    capture.observe_meta(&dispatched.meta);
    let provider_kind = dispatched
        .meta
        .served_provider_kind
        .clone()
        .expect("served provider_kind is stamped on the failing meta");
    let model = dispatched
        .meta
        .served_model
        .clone()
        .expect("served model is stamped on the failing meta");
    capture.record_k_sample(&router, Some("sess-live"));

    // Assert: exactly one sample under the served triple.
    let window = router
        .k_session_store
        .get(&KSessionKey {
            session_key: "sess-live".into(),
            provider_kind,
            model,
        })
        .expect("keyed request recorded a sample");
    assert_eq!(window.len(), 1);

    // Act (keyless): a fresh capture over a fresh router records nothing.
    let router2 = Arc::new(build_test_router(Arc::clone(&router.config)).await);
    let req2 = sample_request("a", false);
    let mut capture2 = rig.capture("openai", &req2, "req-k-none");
    let dispatched2 = router2
        .complete_with_options(req2, Default::default())
        .await;
    capture2.observe_meta(&dispatched2.meta);
    capture2.record_k_sample(&router2, None);

    // Assert: a keyless request leaves the store empty.
    assert!(
        router2.k_session_store.is_empty(),
        "a keyless request must not be recorded",
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

/// A bare `Arc<Router>` for the `render_stream_task` tests that only need a
/// recording target for the POST-EOS K-sample call. The default config has
/// no routes, but recording never dispatches -- it only takes a lock on the
/// session store -- so an empty router is sufficient.
fn k_test_router() -> std::sync::Arc<routectl_router::Router> {
    use std::sync::Arc;
    Arc::new(routectl_router::Router::new(Arc::new(
        routectl_router::Config::default(),
    )))
}

/// Build an `Arc<Router>` over a one-entry chain (unreachable upstream) and a
/// `DispatchMeta` whose served `provider_kind` / `model` are stamped, so a
/// stream capture seeded via `observe_meta(&meta)` has the triple a K-sample
/// recording keys on. The dispatch fails (the upstream is unreachable) but the
/// chain walk still stamps the served identity onto the meta.
async fn k_recording_router_and_meta() -> (
    std::sync::Arc<routectl_router::Router>,
    routectl_router::DispatchMeta,
) {
    use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;
    use std::sync::Arc;

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
    let router = Arc::new(build_test_router(config).await);
    let req = sample_request("a", true);
    let dispatched = router.complete_with_options(req, Default::default()).await;
    (router, dispatched.meta)
}

/// End-to-end K-sample recording through `render_stream_task`: a natural-EOS
/// stream carrying a session key lands EXACTLY ONE sample in the router's
/// per-session store, and a mid-stream-error stream lands ZERO (the error
/// path returns before the post-EOS recording). Drives the real streaming
/// helper with a seeded capture so the served triple is present.
#[tokio::test]
async fn render_stream_task_records_one_k_sample_on_eos_and_none_on_error() {
    use crate::ingress::openai::OpenAiIngress;
    use routectl_router::KSessionKey;

    // Arrange (EOS): a router + a meta with the served triple stamped.
    let (router, meta) = k_recording_router_and_meta().await;
    let provider_kind = meta
        .served_provider_kind
        .clone()
        .expect("served provider_kind stamped on meta");
    let model = meta
        .served_model
        .clone()
        .expect("served model stamped on meta");

    let rig = CaptureRig::new();
    let mut capture = rig.capture("openai", &sample_request("a", true), "req-k-eos");
    capture.observe_meta(&meta);

    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![Ok(streaming_text_chunk("hi"))]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act (EOS): drive the stream to natural completion with a session key.
    render_stream_task(
        upstream,
        OpenAiIngress,
        capture,
        tx,
        Arc::clone(&router),
        Some("sess".to_string()),
        StreamRequestContext::default(),
    )
    .await;
    let _ = drain(rx).await;

    // Assert: exactly one sample under the served triple.
    let window = router
        .k_session_store
        .get(&KSessionKey {
            session_key: "sess".into(),
            provider_kind: provider_kind.clone(),
            model: model.clone(),
        })
        .expect("natural-EOS stream recorded a sample");
    assert_eq!(
        window.len(),
        1,
        "natural EOS must record exactly one sample"
    );

    // Arrange (mid-stream error): a fresh router so the store starts empty.
    let (router_err, meta_err) = k_recording_router_and_meta().await;
    let rig_err = CaptureRig::new();
    let mut capture_err = rig_err.capture("openai", &sample_request("a", true), "req-k-err");
    capture_err.observe_meta(&meta_err);

    let upstream_err: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hi")),
            Err(Error::upstream("p", 503, "Service Unavailable")),
        ]));
    let (tx_err, rx_err) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act (error): drive the stream to a mid-stream upstream error.
    render_stream_task(
        upstream_err,
        OpenAiIngress,
        capture_err,
        tx_err,
        Arc::clone(&router_err),
        Some("sess".to_string()),
        StreamRequestContext::default(),
    )
    .await;
    let _ = drain(rx_err).await;

    // Assert: the error path returns before the post-EOS recording, so the
    // store stays empty.
    assert!(
        router_err.k_session_store.is_empty(),
        "a mid-stream-error stream must not record a K sample",
    );
}

/// A degenerate `[cache_pricing]` override (rm <= 0.0) must fail the
/// server bootstrap, surfaced as a config error rather than silently going
/// inert. Drives the real `build_router_from_config` startup path.
#[tokio::test]
async fn bootstrap_rejects_degenerate_cache_pricing_override() {
    use routectl_auth::{MemoryStore, SecretStore};
    use routectl_router::{CachePricingOverride, Config};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // Arrange: a minimal config carrying one degenerate override.
    let mut cache_pricing = BTreeMap::new();
    cache_pricing.insert(
        "openai-compat:grok-*".to_string(),
        CachePricingOverride {
            rm: Some(0.0),
            ..Default::default()
        },
    );
    let config = Arc::new(Config {
        cache_pricing,
        ..Default::default()
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // Act
    let result = crate::server::build_router_from_config(config, secrets).await;

    // Assert: startup fails, naming the offending selector. `Router` is not
    // `Debug`, so match the Result rather than calling `expect_err`.
    let msg = match result {
        Ok(_) => panic!("degenerate override must fail bootstrap"),
        Err(e) => e.to_string(),
    };
    assert!(msg.contains("openai-compat:grok-*"), "msg: {msg}");
    assert!(msg.contains("rm must be > 0.0"), "msg: {msg}");
}

// ============ stream_response inversion: grace-gated commit (b') =======
//
// The four branches of the inverted `stream_response`
// (`stream_dispatch_gated` + `warm_render_task`):
//   1. FAST-Ok         -- dispatch resolves within grace -> render normally
//   2. FAST-Err-HTTP   -- dispatch resolves Err within grace -> real HTTP status
//   3. GRACE-warm-byte -- grace expired -> early frame is the FIRST body byte
//   4. GRACE-Err       -- grace expired then dispatch Err -> one in-stream error
// plus a grace-expiry SELECTION test proving a pending dispatch commits the
// SSE response instead of blocking.
//
// The FAST tests drive the real grace-gate seam (`stream_dispatch_gated`)
// with an immediately-ready dispatch so the biased `select!` takes the fast
// branch deterministically. The WARM render behavior (early-frame-first,
// dedup, stage marker, single row) is asserted against `warm_render_task`
// directly by draining the `mpsc::Receiver<SseEvent>` -- no axum/KeepAlive
// layer to add spurious comment frames.

/// A `DispatchFut` that resolves IMMEDIATELY with the given result, carrying
/// a real (cloned) `DispatchMeta`. Immediate readiness makes the biased
/// `select!` in `stream_dispatch_gated` take the FAST branch.
fn ready_dispatch(
    meta: routectl_router::DispatchMeta,
    result: routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    >,
) -> DispatchFut {
    Box::pin(async move { routectl_router::DispatchedStream { meta, result } })
}

/// A `DispatchFut` that never resolves -- forces the grace timer to elapse.
fn pending_dispatch() -> DispatchFut {
    Box::pin(std::future::pending())
}

/// Collect the body of an SSE `Response` and parse it into `SseEvent`s.
/// Keep-alive comment frames (no `data:` line) are skipped so the parsed
/// list is exactly the render task's real event sequence.
async fn drain_sse_body(resp: Response) -> Vec<SseEvent> {
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event: Option<String> = None;
        let mut data: Option<String> = None;
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("data:") {
                data = Some(v.trim().to_string());
            }
        }
        if let Some(d) = data {
            out.push(SseEvent { event, data: d });
        }
    }
    out
}

/// Branch 1 (FAST-Ok): a dispatch resolving WITHIN the grace window with
/// content renders exactly as before -- `message_start` on the first content
/// chunk carrying the local input-token estimate, and NO early frame ahead
/// of it (the fast path never flushes one).
#[tokio::test]
async fn stream_gate_fast_ok_renders_message_start_with_estimate_no_early_frame() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: dispatch ready immediately (inside grace) with one content
    // chunk; stream_ctx carries a non-zero estimate.
    let (router, meta) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> = Box::pin(
        futures::stream::iter(vec![Ok(streaming_text_chunk("hello"))]),
    );
    let fut = ready_dispatch(meta, Ok(stream));
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-fast-ok");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 42,
        model: "m".into(),
    };

    // Act
    let resp =
        stream_dispatch_gated(fut, AnthropicIngress, capture, router, None, stream_ctx).await;

    // Assert: a 200 SSE response whose FIRST event is message_start.
    assert_eq!(resp.status(), StatusCode::OK);
    let events = drain_sse_body(resp).await;
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        names.first().copied(),
        Some("message_start"),
        "fast-Ok renders message_start first (no early frame ahead of it): {names:?}"
    );
    // Exactly one message_start: no early frame + no dedup miss.
    assert_eq!(
        names.iter().filter(|n| **n == "message_start").count(),
        1,
        "exactly one message_start on the fast path: {names:?}"
    );
    // The synthesized message_start carries the local estimate.
    let ms = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start present");
    let payload: Value = serde_json::from_str(&ms.data).unwrap();
    assert_eq!(
        payload["message"]["usage"]["input_tokens"], 42,
        "message_start carries the input-token estimate"
    );
}

/// Branch 2 (FAST-Err-HTTP): a dispatch resolving `Err` WITHIN grace returns
/// a REAL HTTP error status via `map_error` (preserving the SDK's pre-stream
/// 529 retry), NOT a 200 SSE stream carrying an in-stream error frame.
#[tokio::test]
async fn stream_gate_fast_err_returns_http_status_not_in_stream_frame() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: dispatch ready immediately (inside grace) with a 529 chain
    // exhaustion.
    let (router, meta) = k_recording_router_and_meta().await;
    let fut = ready_dispatch(meta, Err(Error::upstream("p", 529, "overloaded")));
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-fast-err");

    // Act
    let resp = stream_dispatch_gated(
        fut,
        AnthropicIngress,
        capture,
        router,
        None,
        StreamRequestContext::default(),
    )
    .await;

    // Assert: a real HTTP 529 with the Anthropic error envelope -- NOT a 200
    // SSE stream. This is what keeps the SDK's pre-stream retry alive.
    assert_eq!(
        resp.status().as_u16(),
        529,
        "fast dispatch Err must return a real HTTP status, not an in-stream frame"
    );
    let body = body_to_value(resp).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");

    // Exactly one usage row, finalized upstream_error (attempt-charged),
    // WITHOUT any stream stage marker (it never became an in-stream failure).
    let rows = rig.flush_and_read().await;
    assert_eq!(rows.len(), 1, "exactly one row on the fast-Err path");
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(rows[0].request_id, "req-fast-err");
    assert_eq!(
        rows[0].stream_stage, None,
        "an HTTP-status failure carries no stream stage marker"
    );
}

/// Branch 3 (GRACE-warm-byte): on warm-hold the FIRST body byte is the
/// synthesized Anthropic `message_start` (carrying the estimate), flushed
/// BEFORE the dispatch is awaited; when the real content chunk later renders,
/// there is NO duplicate `message_start` (the early frame set `state.started`).
#[tokio::test]
async fn warm_render_first_byte_is_message_start_with_estimate_no_duplicate() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: warm-hold path. Once the dispatch resolves Ok, one content
    // chunk arrives.
    let (router, meta) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> = Box::pin(
        futures::stream::iter(vec![Ok(streaming_text_chunk("hello"))]),
    );
    let fut = ready_dispatch(meta, Ok(stream));
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-warm-ok");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 77,
        model: "m".into(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act: drive the warm render task; it flushes the early frame before
    // awaiting the (here-immediate) dispatch, then renders content.
    warm_render_task(fut, AnthropicIngress, capture, tx, router, None, stream_ctx).await;
    let events = drain(rx).await;

    // Assert: the FIRST event is the early message_start with the estimate.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        names.first().copied(),
        Some("message_start"),
        "warm path flushes message_start first, before the dispatch content: {names:?}"
    );
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(
        payload["message"]["usage"]["input_tokens"], 77,
        "early message_start carries the input-token estimate"
    );
    // No duplicate message_start when the content chunk renders (dedup).
    assert_eq!(
        names.iter().filter(|n| **n == "message_start").count(),
        1,
        "the first content chunk must NOT re-emit message_start: {names:?}"
    );
    // Content was rendered after the early frame.
    assert!(
        names.contains(&"content_block_delta"),
        "content rendered after the early frame: {names:?}"
    );
}

/// Branch 4 (GRACE-Err): warm-hold, then the dispatch resolves `Err`. The
/// SSE head is already committed, so the failure surfaces as EXACTLY ONE
/// terminal in-stream error via `render_error_eos`, and finalizes a single
/// `upstream_error` row bearing the pre-content stage marker (distinct from a
/// mid-stream cut). `observe_meta` ran inside the render task; the Drop
/// `client_disconnect` fallback did NOT fire.
#[tokio::test]
async fn warm_render_dispatch_err_emits_one_terminal_error_and_pre_content_row() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: warm-hold, then a slow all-lanes-down 529.
    let (router, meta) = k_recording_router_and_meta().await;
    let fut = ready_dispatch(meta, Err(Error::upstream("p", 529, "overloaded")));
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-warm-err");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 10,
        model: "m".into(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    warm_render_task(fut, AnthropicIngress, capture, tx, router, None, stream_ctx).await;
    let events = drain(rx).await;

    // Assert: the early frame, then exactly ONE terminal error event.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        vec!["message_start", "error"],
        "warm dispatch Err: early frame + exactly one terminal error: {names:?}"
    );
    let err_ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("terminal error event present");
    let payload: Value = serde_json::from_str(&err_ev.data).unwrap();
    assert_eq!(payload["type"], "error");
    assert_eq!(payload["error"]["type"], "overloaded_error");

    // Exactly one usage row: upstream_error, pre-content stage marker,
    // observe_meta fields stamped, NOT client_disconnect, ttfb None.
    let rows = rig.flush_and_read().await;
    assert_eq!(rows.len(), 1, "exactly one row on the warm-Err path");
    let row = &rows[0];
    assert_eq!(
        row.outcome, "upstream_error",
        "warm dispatch Err finalizes upstream_error, not the client_disconnect Drop fallback"
    );
    assert_eq!(row.request_id, "req-warm-err");
    assert_eq!(
        row.stream_stage.as_deref(),
        Some("pre_content_dispatch"),
        "the pre-content stage marker keeps it distinct from a mid-stream cut"
    );
    assert_eq!(
        row.provider.as_deref(),
        Some("p"),
        "observe_meta ran INSIDE the render task (served_provider stamped)"
    );
    assert_eq!(
        row.alias, "a",
        "observe_meta stamped the resolved alias in the render task"
    );
    assert!(row.ttfb_ms.is_none(), "no content ever flowed -> ttfb None");
}

/// Grace-expiry SELECTION: a dispatch that never resolves must make the grace
/// timer elapse and COMMIT the SSE `Response` (warm-hold), rather than block
/// the handler. Uses paused time so the grace deadline auto-advances.
#[tokio::test(start_paused = true)]
async fn stream_gate_grace_expiry_commits_sse_response() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: a dispatch that never resolves; an empty router (no I/O).
    let router = k_test_router();
    let fut = pending_dispatch();
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-grace");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 5,
        model: "m".into(),
    };

    // Act: with paused time + a pending dispatch, the runtime auto-advances to
    // the grace deadline; the grace arm fires and commits the SSE Response
    // (flush-and-continue), it does NOT abort the request.
    let resp =
        stream_dispatch_gated(fut, AnthropicIngress, capture, router, None, stream_ctx).await;

    // Assert: a committed 200 SSE stream.
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "grace expiry must commit an SSE stream, got content-type {content_type:?}"
    );
}

/// Warm-hold, dispatch resolves `Ok`, then the content stream itself yields a
/// mid-stream error (one chunk, then Err). This must land the `mid_stream`
/// stage marker, NOT `pre_content_dispatch` -- the latter is reserved for a
/// dispatch future that resolves `Err` before any content ever rendered.
#[tokio::test]
async fn warm_render_ok_then_mid_stream_error_marks_mid_stream_stage() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: warm-hold, dispatch resolves Ok with a stream that renders one
    // chunk then dies mid-stream.
    let (router, meta) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("partial")),
            Err(Error::upstream("p", 503, "boom")),
        ]));
    let fut = ready_dispatch(meta, Ok(stream));
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-warm-mid-err");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 10,
        model: "m".into(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    warm_render_task(fut, AnthropicIngress, capture, tx, router, None, stream_ctx).await;
    let events = drain(rx).await;

    // Assert: the early frame, the rendered chunk, then EXACTLY ONE terminal
    // in-stream error.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        names.iter().filter(|n| **n == "error").count(),
        1,
        "exactly one terminal in-stream error: {names:?}"
    );
    assert_eq!(
        names.last().copied(),
        Some("error"),
        "the terminal error is the last event: {names:?}"
    );

    // Exactly one usage row, finalized once, tagged mid_stream (NOT
    // pre_content_dispatch -- content DID render before the cut).
    let rows = rig.flush_and_read().await;
    assert_eq!(rows.len(), 1, "finalized exactly once");
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(rows[0].request_id, "req-warm-mid-err");
    assert_eq!(
        rows[0].stream_stage.as_deref(),
        Some("mid_stream"),
        "content rendered before the cut, so this is a mid-stream error, not pre-content"
    );
}

/// Client disconnects before the warm-hold render task can flush its early
/// frame (the receiver is already gone). Pins Q4's Drop reservation: the
/// finalize path never runs (the early-frame send fails and the task returns
/// immediately), so the `UsageCapture` guard's `Drop` fallback stamps
/// `client_disconnect` -- NOT `upstream_error` -- and no stage marker is set.
#[tokio::test]
async fn warm_render_client_disconnect_before_flush_drops_to_client_disconnect() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: a dispatch that would resolve fine, but the receiver is
    // dropped before the render task gets a chance to send anything.
    let (router, meta) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> = Box::pin(
        futures::stream::iter(vec![Ok(streaming_text_chunk("hello"))]),
    );
    let fut = ready_dispatch(meta, Ok(stream));
    let rig = CaptureRig::new();
    let capture = rig.capture(
        "anthropic",
        &sample_request("m", true),
        "req-warm-disconnect",
    );
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 10,
        model: "m".into(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    drop(rx);

    // Act: the early-frame send fails immediately (no receiver); the task
    // returns without ever finalizing, leaving the Drop fallback to stamp
    // the row.
    warm_render_task(fut, AnthropicIngress, capture, tx, router, None, stream_ctx).await;

    // Assert: exactly one row, client_disconnect, no stage marker -- a
    // genuine cancellation, not an upstream failure.
    let rows = rig.flush_and_read().await;
    assert_eq!(
        rows.len(),
        1,
        "the Drop fallback still emits exactly one row"
    );
    assert_eq!(
        rows[0].outcome, "client_disconnect",
        "a pre-flush disconnect must not be misreported as an upstream error"
    );
    assert_eq!(rows[0].request_id, "req-warm-disconnect");
    assert_eq!(
        rows[0].stream_stage, None,
        "the Drop fallback path never calls mark_stream_stage"
    );
}

/// OpenAI dialect keeps the default no-op `early_frame` (Q5: both dialects
/// share the handler unforked). On warm-hold there is therefore nothing to
/// flush ahead of content -- the SSE response still commits on grace expiry,
/// and (checked directly against the render task, since a `pending_dispatch`
/// never reaches EOS so the body cannot be drained to completion) the FIRST
/// event the warm task ever sends is real content, NOT a synthetic
/// `message_start` (that framing is Anthropic-only).
#[tokio::test(start_paused = true)]
async fn warm_render_openai_dialect_commits_with_no_leading_early_frame() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange (commit check): a dispatch that never resolves, forcing grace
    // expiry and the warm-hold commit path, with the OpenAI dialect.
    let router = k_test_router();
    let fut = pending_dispatch();
    let rig = CaptureRig::new();
    let capture = rig.capture("openai", &sample_request("m", true), "req-warm-openai");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 5,
        model: "m".into(),
    };

    // Act
    let resp = stream_dispatch_gated(fut, OpenAiIngress, capture, router, None, stream_ctx).await;

    // Assert: the SSE response still commits (OpenAI has no early frame to
    // flush, but the warm-hold path must still commit the response head).
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "grace expiry must commit an SSE stream, got content-type {content_type:?}"
    );

    // Arrange + Act (leading-frame check): drive the warm render task
    // directly with a resolvable dispatch so the mpsc channel can be drained
    // to completion -- a `pending_dispatch`'s body never reaches EOS, so this
    // part cannot be checked at the HTTP body level.
    let (router2, meta2) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> = Box::pin(
        futures::stream::iter(vec![Ok(streaming_text_chunk("hello"))]),
    );
    let fut2 = ready_dispatch(meta2, Ok(stream));
    let rig2 = CaptureRig::new();
    let capture2 = rig2.capture("openai", &sample_request("m", true), "req-warm-openai-2");
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    warm_render_task(
        fut2,
        OpenAiIngress,
        capture2,
        tx,
        router2,
        None,
        StreamRequestContext {
            input_tokens_estimate: 5,
            model: "m".into(),
        },
    )
    .await;
    let events = drain(rx).await;

    // Assert: no leading synthetic message_start -- the default no-op
    // early_frame means the first event the task ever sends is real content.
    assert!(
        events.first().and_then(|e| e.event.as_deref()) != Some("message_start"),
        "OpenAI dialect must not emit a leading synthetic message_start: {events:?}"
    );
}

// ============ forwarded-mode ingress bearer capture gate ========
//
// `capture_forwarded_bearer` stashes the inbound Authorization bearer on
// `req.routectl_internal.forwarded_bearer` ONLY when BOTH:
//   (a) the `x-routectl-mitm-proxied` seam header is present (the MITM
//       proxy stamps it exclusively on the re-injected inference leg), AND
//   (b) the resolved `[mitm] credential_source` is `Forwarded`.
// Every other path (own mode, no `[mitm]` block, no seam header, no
// inbound bearer) MUST leave the carrier byte-identical to pre-passthrough
// behavior: `forwarded_bearer == None`. The token, when captured, is the
// scheme-stripped bearer value (what the egress dispatch path reads) and is
// never rendered raw by the carrier's `Debug`.

/// Build an `Arc<Router>` whose `[mitm]` config carries `cs`. `None`
/// models "no `[mitm]` block at all"; `Some(cs)` models a present block
/// with that credential_source. `Router::new` does no I/O, so this is a
/// pure carrier for the `router.config.mitm` read the gate performs.
fn router_with_mitm(cs: Option<CredentialSource>) -> std::sync::Arc<routectl_router::Router> {
    use routectl_router::{Config, MitmConfig, Router};
    use std::sync::Arc;
    let mitm = cs.map(|credential_source| MitmConfig {
        credential_source,
        ..Default::default()
    });
    Arc::new(Router::new(Arc::new(Config {
        mitm,
        ..Default::default()
    })))
}

/// Build an inbound `HeaderMap` for the gate tests: optionally the
/// `x-routectl-mitm-proxied` seam header (value is irrelevant -- the gate
/// checks presence only) and optionally a raw `Authorization` value.
fn ingress_headers(seam: bool, authorization: Option<&str>) -> HeaderMap {
    use axum::http::{HeaderName, HeaderValue, header::AUTHORIZATION};
    let mut h = HeaderMap::new();
    if seam {
        h.insert(
            HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
            HeaderValue::from_static("1"),
        );
    }
    if let Some(a) = authorization {
        h.insert(AUTHORIZATION, HeaderValue::from_str(a).unwrap());
    }
    h
}

// ---- capture matrix (the security-critical contract) ----

/// Own mode is the default capability: even with the seam-header hint AND
/// an inbound bearer present, the config gate is closed, so nothing is
/// captured.
#[test]
fn forwarded_bearer_stays_none_in_own_mode_even_with_seam_and_bearer() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Own));
    let headers = ingress_headers(true, Some("Bearer sk-own-mode-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "own mode must never capture the inbound bearer"
    );
}

/// Forwarded capability is armed, but the seam-header hint is absent: this
/// request did not arrive via the MITM inference leg, so the bearer is not
/// captured (header-is-a-hint half of the two-key gate).
#[test]
fn forwarded_bearer_stays_none_in_forwarded_mode_without_seam_header() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Forwarded));
    let headers = ingress_headers(false, Some("Bearer sk-no-seam-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "no seam header -> not the MITM inference leg -> no capture"
    );
}

/// Both keys turned: forwarded capability AND the seam-header hint AND an
/// inbound bearer -> the scheme-stripped token lands on the carrier. This
/// pins the captured-state contract the egress dispatch path reads
/// (`expose()` yields the token, not the full `Bearer ...` header value).
#[test]
fn forwarded_bearer_captured_when_forwarded_and_seam_and_bearer_present() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Forwarded));
    let headers = ingress_headers(true, Some("Bearer sk-ant-oat01-passthrough"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    let captured = req
        .routectl_internal
        .forwarded_bearer
        .as_ref()
        .expect("both gates open -> bearer captured");
    assert_eq!(
        captured.expose(),
        "sk-ant-oat01-passthrough",
        "the captured value is the scheme-stripped token"
    );
}

/// No `[mitm]` block at all resolves to `credential_source == None`, which
/// is not `Forwarded`, so the gate stays closed even with the seam header
/// and a bearer present.
#[test]
fn forwarded_bearer_stays_none_when_no_mitm_config_at_all() {
    // Arrange
    let router = router_with_mitm(None);
    let headers = ingress_headers(true, Some("Bearer sk-no-mitm-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "absent [mitm] block must never capture the inbound bearer"
    );
}

/// Both gates open, but the inbound request carries no `Authorization`
/// header: there is nothing to capture, so the carrier stays `None`.
#[test]
fn forwarded_bearer_stays_none_when_gates_open_but_no_authorization_header() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Forwarded));
    let headers = ingress_headers(true, None);
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    assert!(req.routectl_internal.forwarded_bearer.is_none());
}

/// Both gates open, but the `Authorization` header uses a non-`Bearer`
/// scheme: routectl forwards only bearer credentials, so a `Basic` (or any
/// other) scheme is not captured.
#[test]
fn forwarded_bearer_stays_none_for_non_bearer_authorization_scheme() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Forwarded));
    let headers = ingress_headers(true, Some("Basic dXNlcjpwYXNz"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Assert
    assert!(req.routectl_internal.forwarded_bearer.is_none());
}

/// Security guard against the realistic leak vector: something logging the
/// whole carrier via `{:?}` / `?req`. A captured bearer must render as the
/// redaction placeholder, never the raw token.
#[test]
fn captured_bearer_is_redacted_in_carrier_debug() {
    // Arrange
    let router = router_with_mitm(Some(CredentialSource::Forwarded));
    let headers = ingress_headers(true, Some("Bearer sk-leak-canary-42"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &mut req);
    let rendered = format!("{:?}", req.routectl_internal);

    // Assert
    assert!(
        !rendered.contains("sk-leak-canary-42"),
        "carrier Debug must never render the raw bearer: {rendered}"
    );
    assert!(
        rendered.contains("<redacted>"),
        "redaction placeholder present on the captured bearer: {rendered}"
    );
}

// ---- extract_authorization_bearer (pure) ----

#[test]
fn extract_bearer_returns_token_after_scheme() {
    let h = ingress_headers(false, Some("Bearer abc123"));
    assert_eq!(extract_authorization_bearer(&h).as_deref(), Some("abc123"));
}

#[test]
fn extract_bearer_scheme_match_is_case_insensitive() {
    for scheme in ["bearer", "BEARER", "BeArEr"] {
        let h = ingress_headers(false, Some(&format!("{scheme} tok-9")));
        assert_eq!(
            extract_authorization_bearer(&h).as_deref(),
            Some("tok-9"),
            "scheme {scheme} must match case-insensitively"
        );
    }
}

#[test]
fn extract_bearer_trims_surrounding_whitespace_around_token() {
    let h = ingress_headers(false, Some("Bearer    padded-tok  "));
    assert_eq!(
        extract_authorization_bearer(&h).as_deref(),
        Some("padded-tok")
    );
}

#[test]
fn extract_bearer_none_when_header_absent() {
    let h = ingress_headers(false, None);
    assert_eq!(extract_authorization_bearer(&h), None);
}

#[test]
fn extract_bearer_none_for_non_bearer_scheme() {
    let h = ingress_headers(false, Some("Basic dXNlcjpwYXNz"));
    assert_eq!(extract_authorization_bearer(&h), None);
}

#[test]
fn extract_bearer_none_for_empty_token() {
    for val in ["Bearer", "Bearer ", "Bearer     "] {
        let h = ingress_headers(false, Some(val));
        assert_eq!(
            extract_authorization_bearer(&h),
            None,
            "value {val:?} carries no token"
        );
    }
}

#[test]
fn extract_bearer_none_for_non_utf8_header_value() {
    use axum::http::{HeaderValue, header::AUTHORIZATION};
    let mut h = HeaderMap::new();
    // obs-text bytes form a valid HeaderValue whose `to_str()` fails.
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_bytes(&[0xC0, 0xFF]).unwrap(),
    );
    assert_eq!(extract_authorization_bearer(&h), None);
}

#[test]
fn extract_bearer_preserves_token_internal_structure() {
    let h = ingress_headers(false, Some("Bearer sk-ant-oat01-AbC_-.123"));
    assert_eq!(
        extract_authorization_bearer(&h).as_deref(),
        Some("sk-ant-oat01-AbC_-.123")
    );
}
