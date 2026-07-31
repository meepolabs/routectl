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
use routectl_router::config::CredentialSource;
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
        let draft = build_usage_draft(dialect, req, request_id.to_string());
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
    /// `http_status` column: the client-transport status. `Some(200)` once
    /// the SSE head commits; the pre-head upstream status when a dispatch
    /// fails before any byte flushes; `None` when the head never committed
    /// (e.g. a disconnect before the first successful send).
    http_status: Option<i64>,
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
             attempt_count, fallback_count, provider, alias, http_status, extra FROM requests \
             ORDER BY rowid",
        )
        .expect("prepare select");

    stmt.query_map([], |r| {
        let extra: Option<String> = r.get(10)?;
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
            http_status: r.get(9)?,
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
        messages: vec![message()].into(),
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

/// Exact-byte pin: the Anthropic non-streaming handler response body is the
/// dialect wire JSON serialized exactly once, carried through unchanged,
/// with an explicit `application/json` content-type. The literal is the
/// byte string the pre-P5 `render_response -> Value` + `axum::Json(Value)`
/// path produced (both directions serialize the same `Value` with
/// `preserve_order` off, so keys sort alphabetically). If this literal
/// drifts, the single-serialize path changed the client-visible bytes.
#[tokio::test]
async fn anthropic_non_stream_ok_body_bytes_are_stable_single_serialize() {
    use crate::ingress::anthropic::AnthropicIngress;

    let body = AnthropicIngress
        .render_response(ok_response_with_usage(11, 7))
        .expect("anthropic render");
    let resp = ok_json_response(body);

    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type set"),
        "application/json",
    );
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    let expected = br#"{"content":[{"text":"hi","type":"text"}],"id":"resp-1","model":"m","role":"assistant","stop_reason":"end_turn","stop_sequence":null,"type":"message","usage":{"input_tokens":11,"output_tokens":7}}"#;
    assert_eq!(
        bytes.as_ref(),
        expected.as_slice(),
        "Anthropic egress body bytes must match the pre-P5 Json(Value) serialization",
    );
}

/// Exact-byte pin: OpenAI dialect. See the Anthropic sibling above for the
/// invariant this literal guards.
#[tokio::test]
async fn openai_non_stream_ok_body_bytes_are_stable_single_serialize() {
    use crate::ingress::openai::OpenAiIngress;

    // Pin `created` explicitly: the OpenAI render synthesizes a live
    // unix-seconds stamp when the upstream omitted it (0), which would
    // make an exact-byte pin non-deterministic. A non-zero value is
    // passed through verbatim.
    let mut resp = ok_response_with_usage(11, 7);
    resp.created = 1_700_000_000;
    let body = OpenAiIngress.render_response(resp).expect("openai render");
    let resp = ok_json_response(body);

    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type set"),
        "application/json",
    );
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    let expected = br#"{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"hi","role":"user"}}],"created":1700000000,"id":"resp-1","model":"m","object":"chat.completion","system_fingerprint":null,"usage":{"completion_tokens":7,"prompt_tokens":11,"total_tokens":18}}"#;
    assert_eq!(
        bytes.as_ref(),
        expected.as_slice(),
        "OpenAI egress body bytes must match the pre-P5 Json(Value) serialization",
    );
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
async fn anthropic_envelope_forwarded_missing_bearer_refuse_maps_to_400_invalid_request() {
    // The ROUTER-side missing-bearer terminal guard refuses a forwarded
    // target with no captured client bearer with `Error::Validation`
    // carrying `reason=missing_forwarded_bearer`. Pin the client-facing
    // contract: HTTP 400, Anthropic invalid_request_error envelope, and
    // the reason survives into the message (never a 5xx, never a passed-
    // through upstream 401).
    let err = Error::Validation(
        "forwarded target has no captured client bearer to authenticate this request \
         (reason=missing_forwarded_bearer)"
            .to_string(),
    );

    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("missing_forwarded_bearer"),
        "the refuse reason must survive into the client envelope message",
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
        body["error"].get("param").is_some() && body["error"]["param"].is_null(),
        "OpenAI envelope carries the nullable `param` key (null when routectl names no offending parameter)"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("nonesuch")
    );
}

#[tokio::test]
async fn openai_envelope_upstream_error_carries_null_param() {
    // Arrange: an upstream error rendered on the OpenAI dialect. Real
    // OpenAI / litellm always emit `param`; routectl has no offending
    // request parameter to name at the proxy boundary, so it is null.
    let err = Error::upstream("p", 400, "bad request");

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    assert!(
        body["error"].get("param").is_some(),
        "the OpenAI error envelope must always carry the `param` key"
    );
    assert!(
        body["error"]["param"].is_null(),
        "`param` is null when routectl names no offending parameter"
    );
}

#[tokio::test]
async fn anthropic_envelope_never_carries_param() {
    // The Anthropic error envelope has no `param` field; FIX 1 is
    // OpenAI-dialect-only and must not cross-contaminate.
    let err = Error::upstream("p", 400, "bad request");

    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    assert!(
        body["error"].get("param").is_none(),
        "the Anthropic error envelope must not gain a `param` field"
    );
}

#[tokio::test]
async fn openai_envelope_forwards_upstream_param_when_present() {
    // Arrange: a proxied OpenAI 400 whose upstream body named the
    // offending request parameter. routectl must forward that param
    // rather than collapsing it to null.
    let err = Error::Upstream {
        provider: "openai_prod".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: "{\"error\":{\"type\":\"invalid_request_error\",\
               \"message\":\"unsupported value\",\"param\":\"temperature\"}}"
            .into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(
        body["error"]["param"], "temperature",
        "upstream-supplied param must be forwarded"
    );
}

#[tokio::test]
async fn openai_envelope_param_null_when_upstream_omits_it() {
    // Arrange: an upstream JSON error body with no `param` key. The
    // envelope must emit `param: null`, not fabricate one.
    let err = Error::Upstream {
        provider: "openai_prod".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: "{\"error\":{\"type\":\"invalid_request_error\",\
               \"message\":\"bad request\"}}"
            .into(),
    };

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    assert!(
        body["error"].get("param").is_some() && body["error"]["param"].is_null(),
        "param is null when the upstream named no offending parameter"
    );
}

#[tokio::test]
async fn openai_envelope_param_null_for_non_upstream_error() {
    // Arrange: a non-upstream error can never carry an upstream param.
    let err = Error::Validation("bad input".into());

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert
    assert!(
        body["error"].get("param").is_some() && body["error"]["param"].is_null(),
        "non-upstream errors emit param: null"
    );
}

#[tokio::test]
async fn anthropic_envelope_ignores_upstream_param() {
    // The Anthropic envelope has no `param` field even when the upstream
    // body carried one.
    let err = Error::Upstream {
        provider: "p".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: "{\"error\":{\"message\":\"bad\",\"param\":\"temperature\"}}".into(),
    };

    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    assert!(
        body["error"].get("param").is_none(),
        "the Anthropic envelope must not surface a param field"
    );
}

#[tokio::test]
async fn anthropic_envelope_402_404_504_map_to_documented_types() {
    // FIX 2: 402/404/504 must no longer fold into the generic api_error.
    // Anthropic error-type spellings per its published error docs.
    let cases = [
        (402u16, "billing_error"),
        (404u16, "not_found_error"),
        (504u16, "timeout_error"),
    ];
    for (status, expected_type) in cases {
        // Arrange
        let err = Error::upstream("p", status, "upstream failure");

        // Act
        let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
        let http_status = resp.status().as_u16();
        let body = body_to_value(resp).await;

        // Assert
        assert_eq!(http_status, status, "HTTP status preserved for {status}");
        assert_eq!(body["type"], "error");
        assert_eq!(
            body["error"]["type"], expected_type,
            "status {status} must map to {expected_type}, not api_error"
        );
    }
}

#[tokio::test]
async fn anthropic_envelope_existing_mappings_intact_and_other_statuses_api_error() {
    // FIX 2 must leave the existing ladder untouched and keep the
    // api_error fallback for statuses outside the ladder.
    let cases = [
        (401u16, "authentication_error"),
        (403u16, "permission_error"),
        (413u16, "request_too_large"),
        (429u16, "rate_limit_error"),
        (503u16, "overloaded_error"),
        (529u16, "overloaded_error"),
        (502u16, "api_error"),
        (500u16, "api_error"),
    ];
    for (status, expected_type) in cases {
        let err = Error::upstream("p", status, "upstream failure");
        let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
        let body = body_to_value(resp).await;
        assert_eq!(
            body["error"]["type"], expected_type,
            "status {status} must still map to {expected_type}"
        );
    }
}

#[tokio::test]
async fn map_error_upstream_echoes_retry_after_header_from_hint() {
    // Arrange: a 429 upstream carrying a multi-second reset hint.
    let err =
        Error::upstream_with_retry_after("p", 429, "rate limited", Some(Duration::from_secs(30)));

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);

    // Assert: the client response carries an integer-second Retry-After.
    let hdr = resp
        .headers()
        .get(RETRY_AFTER)
        .expect("client response must carry a Retry-After header");
    assert_eq!(hdr.to_str().expect("ascii header"), "30");
}

#[tokio::test]
async fn map_error_upstream_rounds_sub_second_hint_up_to_one_second() {
    // Arrange: a sub-second hint (as from a `retry-after-ms` upstream
    // header) must not floor to 0 on the second-granular Retry-After.
    let err =
        Error::upstream_with_retry_after("p", 503, "overloaded", Some(Duration::from_millis(250)));

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);

    // Assert: rounded UP to at least 1s.
    let hdr = resp
        .headers()
        .get(RETRY_AFTER)
        .expect("sub-second hint must still yield a Retry-After header");
    assert_eq!(hdr.to_str().expect("ascii header"), "1");
}

#[tokio::test]
async fn map_error_upstream_omits_retry_after_header_without_hint() {
    // Arrange: a rate-limit upstream with no parsed reset hint.
    let err = Error::upstream("p", 429, "rate limited");

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);

    // Assert: no header fabricated when the upstream sent no hint.
    assert!(
        resp.headers().get(RETRY_AFTER).is_none(),
        "no Retry-After header without an upstream hint"
    );
}

#[tokio::test]
async fn map_error_upstream_omits_retry_after_header_on_zero_hint() {
    // Arrange: a zero reset hint. Reachable from `retry-after-ms: 0`,
    // `Retry-After: 0`, or a past HTTP-date clamped to zero -- all funnel
    // to Some(Duration::ZERO). `Retry-After: 0` means "retry now", which
    // contradicts a 429/503 backoff, so no header must be emitted.
    for status in [429u16, 503u16] {
        let err =
            Error::upstream_with_retry_after("p", status, "rate limited", Some(Duration::ZERO));

        // Act
        let resp = map_error(ErrorEnvelopeShape::OpenAi, err);

        // Assert: no Retry-After header on a zero hint.
        assert!(
            resp.headers().get(RETRY_AFTER).is_none(),
            "zero hint must emit no Retry-After header (status {status})"
        );
    }
}

#[tokio::test]
async fn map_error_upstream_emits_correlation_id_header_when_present() {
    // Arrange: an upstream error carrying the provider's correlation id.
    let err =
        Error::upstream("p", 500, "boom").with_upstream_request_id(Some("req-abc-123".to_string()));

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);

    // Assert: the id rides through on x-upstream-request-id.
    let hdr = resp
        .headers()
        .get(UPSTREAM_REQUEST_ID_HEADER)
        .expect("client response must carry the upstream correlation id");
    assert_eq!(hdr.to_str().expect("ascii header"), "req-abc-123");
}

#[tokio::test]
async fn map_error_upstream_omits_correlation_id_header_when_absent() {
    // Arrange: an upstream error with no lifted correlation id.
    let err = Error::upstream("p", 500, "boom");

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);

    // Assert: no header fabricated when the upstream sent none.
    assert!(
        resp.headers().get(UPSTREAM_REQUEST_ID_HEADER).is_none(),
        "no x-upstream-request-id header without a lifted id"
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
        upstream_request_id: None,
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
        upstream_request_id: None,
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
        upstream_request_id: None,
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
        upstream_request_id: None,
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

#[test]
fn sanitize_upstream_for_client_bounds_oversized_nested_message() {
    // A reverse proxy or custom endpoint fronting the Bedrock lane can return
    // a NESTED `{"error":{"message":"<huge>"}}` 400. Even if such a body
    // reaches the ingress sink intact, the extracted detail is length-bounded
    // so an oversized upstream message can never be reflected verbatim to the
    // caller.
    let long_message = "leak_".repeat(4_000); // ~20 KB, well past any cap
    let nested = serde_json::json!({ "error": { "message": long_message } }).to_string();

    let out = sanitize_upstream_for_client(400, &nested);

    assert!(out.contains("400"), "status present: {out:?}");
    assert!(
        out.len() <= routectl_core::MAX_LOG_BODY_EXCERPT + 64,
        "client message must stay bounded, got {} bytes",
        out.len()
    );
    assert!(
        out.ends_with("... [truncated]"),
        "an oversized nested message must carry the truncation marker: {out:?}"
    );
    assert!(
        !out.contains(&"leak_".repeat(200)),
        "the oversized upstream message must not be reflected verbatim"
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
        upstream_request_id: None,
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
        upstream_request_id: None,
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

#[test]
fn sanitize_stream_error_surfaces_top_level_error_message_like_non_stream() {
    // Arrange: a mid-stream upstream fault whose body carries the
    // upstream's own top-level `error.message`. The stream sanitizer
    // must surface that same short, bounded classifier the non-stream
    // path does -- while still dropping the provider name and any
    // sibling body keys.
    let body = "{\"error\":{\"type\":\"invalid_request_error\",\
                \"message\":\"max_tokens is too large\"},\
                \"internal_quota\":\"tenant-12345 exceeded\"}";
    let err = Error::Upstream {
        provider: "openai_prod".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: body.into(),
    };

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert: the same client-safe extractor output as the non-stream
    // path, only the leading kind tag differs.
    assert_eq!(
        safe,
        sanitize_upstream_for_client(400, body).replace("upstream error", "upstream stream error"),
        "stream detail matches the non-stream extractor output"
    );
    assert!(
        safe.contains("max_tokens is too large"),
        "upstream top-level error.message surfaced on the stream path: {safe:?}"
    );
    assert!(safe.contains("400"), "HTTP status preserved: {safe:?}");
    assert!(
        !safe.contains("openai_prod"),
        "provider config name must not leak: {safe:?}"
    );
    assert!(
        !safe.contains("tenant-12345"),
        "raw sibling body keys must not leak: {safe:?}"
    );
    assert!(
        !safe.contains("internal_quota"),
        "raw sibling body keys must not leak: {safe:?}"
    );
}

#[test]
fn sanitize_stream_error_falls_back_to_status_only_without_message() {
    // Arrange: a non-JSON body has no top-level error.message to
    // surface, so the stream sanitizer falls back to status-only and
    // never forwards the raw body.
    let err = Error::Upstream {
        provider: "bedrock_prod".into(),
        status: 502,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: "<html><body>upstream-host-name gateway timeout</body></html>".into(),
    };

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert
    assert_eq!(safe, "upstream stream error (HTTP 502)");
    assert!(
        !safe.contains("upstream-host-name"),
        "raw non-JSON body must not leak: {safe:?}"
    );
}

#[test]
fn sanitize_stream_error_does_not_forward_oversized_raw_body() {
    // Arrange: an abusive/proxied envelope with an oversized top-level
    // error.message. The stream path must bound it exactly as the
    // non-stream path does -- never reflect the full body verbatim.
    let huge = "A".repeat(MAX_UPSTREAM_DETAIL_CHARS + 500);
    let err = Error::Upstream {
        provider: "p".into(),
        status: 400,
        retry_after: None,
        upstream_type: None,
        upstream_code: None,
        upstream_request_id: None,
        body: format!("{{\"error\":{{\"message\":\"{huge}\"}}}}"),
    };

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert: bounded, with the truncation marker; not the full body.
    assert!(
        safe.chars().count() < huge.chars().count(),
        "oversized message must be bounded, not forwarded verbatim: len {}",
        safe.chars().count()
    );
    assert!(
        safe.contains("... [truncated]"),
        "bounded detail carries the truncation marker: {safe:?}"
    );
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

// -------- drive_stream: immediate cancel on client disconnect --------

/// An upstream that yields its queued items, then parks on
/// `Poll::Pending` forever (never registering a waker) to model an
/// upstream that stalls after the client has gone away. Records its own
/// drop so a test can assert `drive_stream` released the upstream socket
/// on cancel.
struct StallingStream {
    items: std::collections::VecDeque<routectl_core::Result<routectl_core::ChatChunk>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl futures::Stream for StallingStream {
    type Item = routectl_core::Result<routectl_core::ChatChunk>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.items.pop_front() {
            Some(item) => std::task::Poll::Ready(Some(item)),
            None => std::task::Poll::Pending,
        }
    }
}

impl Drop for StallingStream {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Client disconnect mid-stream: once the receiver is dropped, the
/// biased `tx.closed()` select arm must return immediately even though
/// the upstream is still stalled -- releasing the upstream socket and
/// the render task rather than blocking to `STREAM_READ_TIMEOUT`. Pre-
/// fix the loop only awaits `upstream.next()`, so the bounded timeout
/// converts the resulting hang into a failure (RED). Exactly one
/// `client_disconnect` row lands with no `stream_stage` marker, and a
/// single DEBUG breadcrumb records the cancel.
#[tokio::test]
async fn drive_stream_cancels_immediately_on_client_disconnect() {
    use crate::ingress::anthropic::AnthropicIngress;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Arrange: upstream yields one chunk, then stalls forever.
    let dropped = Arc::new(AtomicBool::new(false));
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(StallingStream {
            items: std::collections::VecDeque::from(vec![Ok(streaming_text_chunk("hi"))]),
            dropped: Arc::clone(&dropped),
        });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-disconnect");

    // Act: spawn the render task, drain the first event so the loop is
    // parked on the (now-stalled) upstream, then drop the receiver.
    let (rows, events) = routectl_testkit::with_capture(async move {
        let handle = tokio::spawn(render_stream_task(
            upstream,
            AnthropicIngress,
            capture,
            tx,
            k_test_router(),
            None,
            StreamRequestContext::default(),
        ));
        rx.recv().await.expect("first event before disconnect");
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("drive_stream must return promptly after client disconnect")
            .expect("render task panicked");
        rig.flush_and_read().await
    })
    .await;

    // Assert: upstream released, single client_disconnect row, no stage.
    assert!(
        dropped.load(Ordering::SeqCst),
        "upstream stream must be dropped on cancel"
    );
    assert_eq!(rows.len(), 1, "exactly one usage row");
    assert_eq!(rows[0].outcome, "client_disconnect");
    assert_eq!(
        rows[0].stream_stage, None,
        "client cancel is not an upstream stage failure"
    );
    // Truth-table row 6 (RED before the commit-point fix): the first event
    // drained successfully BEFORE the receiver was dropped, so the SSE head
    // committed to 200. A later disconnect does not un-commit that status --
    // the client did receive a 200 transport head.
    assert_eq!(
        rows[0].http_status,
        Some(200),
        "a disconnect AFTER a successful send keeps the committed 200"
    );

    // Exactly one DEBUG breadcrumb for the disconnect.
    let disconnects: Vec<&routectl_testkit::CapturedEvent> = events
        .iter()
        .filter(|e| e.field("reason") == Some("client_disconnected"))
        .collect();
    assert_eq!(disconnects.len(), 1, "exactly one disconnect breadcrumb");
    assert_eq!(disconnects[0].level, tracing::Level::DEBUG);
}

/// Partial usage observed before the client left must survive on the
/// `client_disconnect` row: the first chunk carries usage, so the
/// persisted row reflects the tokens seen pre-disconnect.
#[tokio::test]
async fn drive_stream_preserves_partial_usage_on_client_disconnect() {
    use crate::ingress::anthropic::AnthropicIngress;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Arrange: first (and only) chunk carries partial usage.
    let dropped = Arc::new(AtomicBool::new(false));
    let mut chunk = streaming_text_chunk("hi");
    chunk.usage = Some(routectl_core::UsageDelta {
        prompt_tokens: Some(11),
        completion_tokens: Some(4),
        total_tokens: Some(15),
        ..Default::default()
    });
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(StallingStream {
            items: std::collections::VecDeque::from(vec![Ok(chunk)]),
            dropped: Arc::clone(&dropped),
        });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    let rig = CaptureRig::new();
    let capture = rig.capture(
        "anthropic",
        &sample_request("m", true),
        "req-disconnect-usage",
    );

    // Act
    let handle = tokio::spawn(render_stream_task(
        upstream,
        AnthropicIngress,
        capture,
        tx,
        k_test_router(),
        None,
        StreamRequestContext::default(),
    ));
    rx.recv().await.expect("first event before disconnect");
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("drive_stream must return promptly after client disconnect")
        .expect("render task panicked");
    let rows = rig.flush_and_read().await;

    // Assert: single disconnect row carrying the pre-disconnect tokens.
    assert!(dropped.load(Ordering::SeqCst), "upstream must be dropped");
    assert_eq!(rows.len(), 1, "exactly one usage row");
    assert_eq!(rows[0].outcome, "client_disconnect");
    assert_eq!(rows[0].input_tokens, Some(11));
    assert_eq!(rows[0].output_tokens, Some(4));
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
        body: &[u8],
    ) -> routectl_core::Result<routectl_core::ChatRequest> {
        self.inner.parse_request(headers, body)
    }
    fn render_response(
        &self,
        resp: routectl_core::ChatResponse,
    ) -> routectl_core::Result<bytes::Bytes> {
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

/// Truth-table row 7: a streaming render failure AFTER the SSE head has
/// committed keeps http_status=200. Mirrors `drive_stream`'s render-Err
/// ordering exactly (the render-failure arm is not cheaply reachable end-
/// to-end -- it needs an adapter that fails `render_chunk` on a canonical
/// chunk -- so this pins the ordering directly, as the non-streaming
/// render-failure test above does): a prior successful send committed the
/// head (`mark_stream_http_committed`), then a later chunk fails to render,
/// so `observe_error` records the class WITHOUT overwriting the 200, and the
/// row finalizes `upstream_error` carrying the 200 transport status.
#[tokio::test]
async fn capture_stream_render_failure_after_head_commit_keeps_200() {
    // Arrange
    let rig = CaptureRig::new();
    let req = sample_request("a", true);
    let mut capture = rig.capture("anthropic", &req, "req-stream-render-502");

    // Act: first content chunk rendered + sent (head committed to 200), then
    // a later chunk fails to render -> observe_error + finalize(UpstreamError).
    capture.mark_first_byte();
    capture.mark_stream_http_committed();
    let render_err = Error::Streaming("chunk render failed".into());
    capture.observe_error(&render_err);
    capture.finalize(Outcome::UpstreamError);
    drop(capture);
    let rows = rig.flush_and_read().await;

    // Assert: one upstream_error row whose transport status stays 200.
    assert_eq!(rows.len(), 1, "exactly one row per request");
    assert_eq!(
        rows[0].outcome, "upstream_error",
        "render failure finalizes upstream_error"
    );
    assert_eq!(
        rows[0].http_status,
        Some(200),
        "a render failure after head commit keeps the client's 200 transport status"
    );
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
    // Truth-table row 3 (RED before the commit-point fix): a clean stream
    // committed its SSE head at the first successful send, so the client's
    // transport status is 200 (previously left NULL for streaming success).
    assert_eq!(
        rows[0].http_status,
        Some(200),
        "clean natural EOS records the committed 200 transport status"
    );
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
    // Truth-table row 4 (RED before the commit-point fix): the first content
    // chunk sent successfully committed the SSE head to 200; the later
    // mid-stream 503 is carried by outcome + stream_stage=mid_stream and must
    // NOT overwrite the client's 200 transport status back to 503.
    assert_eq!(
        rows[0].http_status,
        Some(200),
        "a post-head mid-stream upstream error keeps http_status 200"
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
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k")),
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
    capture.observe_meta(&dispatched.meta, 0, 0);
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
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k"))
            .with_runtime(runtime),
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
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k")),
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
    capture.observe_meta(&dispatched.meta, 0, 0);
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
    capture2.observe_meta(&dispatched2.meta, 0, 0);
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
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k")),
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
    capture.observe_meta(&meta, 0, 0);

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
    capture_err.observe_meta(&meta_err, 0, 0);

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
    // Truth-table row 1 (regression guard): a pre-head fast dispatch error
    // never committed an SSE head, so http_status carries the REAL upstream
    // transport status the client received (529), not a fabricated 200.
    assert_eq!(
        rows[0].http_status,
        Some(529),
        "pre-head dispatch error records the upstream transport status"
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
    // Truth-table row 2 (RED before the commit-point fix): the early frame
    // (message_start) flushed as the first body byte, committing the SSE head
    // to a 200 status line BEFORE the dispatch resolved Err. The mid-flight
    // upstream failure is carried by outcome / stream_stage; the client's
    // transport status stays 200 and must not be overwritten to 529.
    assert_eq!(
        row.http_status,
        Some(200),
        "a committed SSE head keeps http_status 200 even when the dispatch then fails"
    );
}

/// A `DispatchFut` that never resolves and records its own drop, so a test can
/// prove the warm task CANCELLED (dropped) the pending dispatch -- releasing
/// the upstream request and any half-open probe slot the future holds -- rather
/// than polling it to the first-content timeout.
struct DropTrackingPendingDispatch {
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::future::Future for DropTrackingPendingDispatch {
    type Output = routectl_router::DispatchedStream;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}

impl Drop for DropTrackingPendingDispatch {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Regression: the content-commit boundary keeps the router dispatch PENDING
/// through content-free leading chunks (role, metadata) until the first real
/// content chunk. On warm-hold, a client that disconnects during that extended
/// pre-content window must be detected while the dispatch is still pending --
/// the warm task must cancel the dispatch and finalize `client_disconnect`
/// promptly, NOT wait out the full first-content timeout and record
/// `upstream_error`. Cancelling by DROPPING the pending future releases the
/// upstream request + probe slot, and taking the un-finalized exit means the
/// breaker/probe are never debited.
#[tokio::test]
async fn warm_render_cancels_pending_dispatch_on_client_disconnect_before_content() {
    use crate::ingress::anthropic::AnthropicIngress;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Arrange: warm-hold with a dispatch that never resolves (models a
    // provider that emits leading role/metadata then hangs -- from the CLI's
    // vantage the content-boundary rule keeps this future pending).
    let dropped = Arc::new(AtomicBool::new(false));
    let fut: DispatchFut = Box::pin(DropTrackingPendingDispatch {
        dropped: Arc::clone(&dropped),
    });
    let router = k_test_router();
    let rig = CaptureRig::new();
    let capture = rig.capture("anthropic", &sample_request("m", true), "req-warm-disco");
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 5,
        model: "m".into(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act: spawn the warm task, drain the early frame (Anthropic flushes
    // message_start before awaiting the dispatch), then drop the receiver to
    // model the client hanging up while the dispatch is still pending.
    let (rows, events) = routectl_testkit::with_capture(async move {
        let handle = tokio::spawn(warm_render_task(
            fut,
            AnthropicIngress,
            capture,
            tx,
            router,
            None,
            stream_ctx,
        ));
        rx.recv().await.expect("early frame before disconnect");
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("warm task must return promptly after client disconnect")
            .expect("warm task panicked");
        rig.flush_and_read().await
    })
    .await;

    // Assert: the pending dispatch was dropped (upstream + probe slot released).
    assert!(
        dropped.load(Ordering::SeqCst),
        "the pending dispatch must be cancelled (dropped), not polled to timeout"
    );
    // Exactly one row, finalized as client_disconnect (not upstream_error) with
    // no stage marker -- the RAII Drop path, so no breaker/probe debit.
    assert_eq!(rows.len(), 1, "exactly one usage row");
    assert_eq!(
        rows[0].outcome, "client_disconnect",
        "a disconnect during the pending pre-content window finalizes client_disconnect"
    );
    assert_eq!(
        rows[0].stream_stage, None,
        "client cancel is not an upstream stage failure -- no pre_content_dispatch marker"
    );

    // Exactly one DEBUG disconnect breadcrumb from the warm-hold select arm.
    let disconnects: Vec<&routectl_testkit::CapturedEvent> = events
        .iter()
        .filter(|e| e.field("reason") == Some("client_disconnected"))
        .collect();
    assert_eq!(disconnects.len(), 1, "exactly one disconnect breadcrumb");
    assert_eq!(disconnects[0].level, tracing::Level::DEBUG);
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

/// An in-band Anthropic `error` event arriving AFTER the first content
/// chunk (decoded by the real SSE state machine) is terminal -- the
/// client gets exactly one terminal SSE error carrying the preserved
/// `error.type` (`overloaded_error`, not the generic `api_error`), and
/// the request finalizes exactly once (no second provider hit past the
/// first-content-chunk non-retry boundary).
#[tokio::test]
async fn warm_render_post_content_anthropic_error_is_terminal_with_preserved_type() {
    use crate::ingress::anthropic::AnthropicIngress;
    use routectl_providers::anthropic_api::sse::SseState;

    // Decode a real in-band overloaded_error through the SSE state machine
    // -- the exact structured error the provider stream now yields.
    let mut state = SseState::default();
    let decoded = state
        .parse_event(
            "p",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#,
        )
        .expect_err("in-band error event must surface as Err");

    // Arrange: warm-hold, dispatch Ok, one content chunk then the decoded
    // in-stream error (post-first-chunk -> terminal, not retryable).
    let (router, meta) = k_recording_router_and_meta().await;
    let stream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("partial")),
            Err(decoded),
        ]));
    let fut = ready_dispatch(meta, Ok(stream));
    let rig = CaptureRig::new();
    let capture = rig.capture(
        "anthropic",
        &sample_request("m", true),
        "req-warm-overloaded",
    );
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: 10,
        model: "m".into(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    warm_render_task(fut, AnthropicIngress, capture, tx, router, None, stream_ctx).await;
    let events = drain(rx).await;

    // Assert: exactly one terminal error, last, carrying overloaded_error.
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
    let err_ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("terminal error event present");
    let payload: Value = serde_json::from_str(&err_ev.data).unwrap();
    assert_eq!(
        payload["error"]["type"], "overloaded_error",
        "the preserved error.type must reach the client, not api_error"
    );

    // Exactly one usage row: the request finalized once past the non-retry
    // boundary -- no second provider was dispatched.
    let rows = rig.flush_and_read().await;
    assert_eq!(
        rows.len(),
        1,
        "finalized exactly once -> no second provider hit"
    );
    assert_eq!(rows[0].outcome, "upstream_error");
    assert_eq!(
        rows[0].stream_stage.as_deref(),
        Some("mid_stream"),
        "content rendered before the cut -> mid-stream terminal, not pre-content"
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
    // Truth-table row 5 (regression guard): the early-frame send failed, so
    // NO byte ever flushed and the SSE head never committed. http_status must
    // stay NULL -- the client received no transport status from us.
    assert_eq!(
        rows[0].http_status, None,
        "a disconnect before any successful send leaves http_status NULL"
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
//   (b) `router.has_forwarded_provider()` is true (a `credential_source =
//       "forwarded"` provider is configured).
// Every other path (no forwarded provider configured, no seam header, no
// inbound bearer) MUST leave the carrier byte-identical to pre-passthrough
// behavior: `forwarded_bearer == None`. The token, when captured, is the
// scheme-stripped bearer value (what the egress dispatch path reads) and is
// never rendered raw by the carrier's `Debug`.

/// Build an `Arc<Router>` whose `[providers]` table carries `cs`. `None`
/// models "no provider configured at all"; `Some(cs)` models a single
/// `anthropic-api` provider entry with that `credential_source`. `Router::new`
/// does no I/O, so this is a pure carrier for the `has_forwarded_provider()`
/// read the gate performs.
fn router_with_provider_credential_source(
    cs: Option<CredentialSource>,
) -> std::sync::Arc<routectl_router::Router> {
    use routectl_router::{Config, ProviderEntry, Router};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    let mut providers = BTreeMap::new();
    if let Some(credential_source) = cs {
        providers.insert(
            "p".to_string(),
            ProviderEntry::anthropic_api("").with_credential_source(credential_source),
        );
    }
    Arc::new(Router::new(Arc::new(Config {
        providers,
        ..Default::default()
    })))
}

/// Build an inbound `HeaderMap` for the gate tests: optionally the
/// `x-routectl-mitm-proxied` seam header stamped with `nonce`'s value, and
/// optionally a raw `Authorization` value. `nonce` must be the SAME
/// instance passed to `capture_forwarded_bearer`/`capture_stainless_headers`
/// for the seam to be recognized as present.
fn ingress_headers(nonce: Option<&MitmSeamNonce>, authorization: Option<&str>) -> HeaderMap {
    use axum::http::{HeaderName, HeaderValue, header::AUTHORIZATION};
    let mut h = HeaderMap::new();
    if let Some(nonce) = nonce {
        h.insert(
            HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
            nonce.header_value(),
        );
    }
    if let Some(a) = authorization {
        h.insert(AUTHORIZATION, HeaderValue::from_str(a).unwrap());
    }
    h
}

// ---- capture matrix (the security-critical contract) ----

/// No forwarded provider configured is the default capability: even with
/// the seam-header hint AND an inbound bearer present, the config gate is
/// closed, so nothing is captured. An `Own`-credential provider present
/// (not just an EMPTY `[providers]` table) proves the gate keys on
/// `credential_source`, not merely "some provider exists".
#[test]
fn forwarded_bearer_stays_none_without_forwarded_provider_even_with_seam_and_bearer() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Own));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), Some("Bearer sk-own-mode-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "no forwarded provider configured must never capture the inbound bearer"
    );
}

/// Forwarded capability is armed, but the seam-header hint is absent: this
/// request did not arrive via the MITM inference leg, so the bearer is not
/// captured (header-is-a-hint half of the two-key gate).
#[test]
fn forwarded_bearer_stays_none_in_forwarded_mode_without_seam_header() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(None, Some("Bearer sk-no-seam-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "no seam header -> not the MITM inference leg -> no capture"
    );
}

/// Security-critical: a request carrying the seam header with a value that
/// does NOT match the process nonce (a spoof attempt by a direct caller)
/// must downgrade to seam-ABSENT -- exactly like a missing header -- not be
/// treated as present just because the header name exists.
#[test]
fn forwarded_bearer_stays_none_when_seam_header_carries_a_spoofed_wrong_value() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let mut headers = ingress_headers(None, Some("Bearer sk-spoofed-seam-tok"));
    headers.insert(
        axum::http::HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
        axum::http::HeaderValue::from_static("1"),
    );
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "a spoofed seam header value must never be treated as seam-present"
    );
}

/// Both keys turned: forwarded capability AND the seam-header hint AND an
/// inbound bearer -> the scheme-stripped token lands on the carrier. This
/// pins the captured-state contract the egress dispatch path reads
/// (`expose()` yields the token, not the full `Bearer ...` header value).
#[test]
fn forwarded_bearer_captured_when_forwarded_and_seam_and_bearer_present() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), Some("Bearer sk-ant-oat01-passthrough"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

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

/// No provider configured at all: `has_forwarded_provider()` is `false`, so
/// the gate stays closed even with the seam header and a bearer present.
#[test]
fn forwarded_bearer_stays_none_when_no_provider_configured_at_all() {
    // Arrange
    let router = router_with_provider_credential_source(None);
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), Some("Bearer sk-no-mitm-tok"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.forwarded_bearer.is_none(),
        "no configured provider must never capture the inbound bearer"
    );
}

/// Both gates open, but the inbound request carries no `Authorization`
/// header: there is nothing to capture, so the carrier stays `None`.
#[test]
fn forwarded_bearer_stays_none_when_gates_open_but_no_authorization_header() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), None);
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(req.routectl_internal.forwarded_bearer.is_none());
}

/// Both gates open, but the `Authorization` header uses a non-`Bearer`
/// scheme: routectl forwards only bearer credentials, so a `Basic` (or any
/// other) scheme is not captured.
#[test]
fn forwarded_bearer_stays_none_for_non_bearer_authorization_scheme() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), Some("Basic dXNlcjpwYXNz"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(req.routectl_internal.forwarded_bearer.is_none());
}

/// Security guard against the realistic leak vector: something logging the
/// whole carrier via `{:?}` / `?req`. A captured bearer must render as the
/// redaction placeholder, never the raw token.
#[test]
fn captured_bearer_is_redacted_in_carrier_debug() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers(Some(&nonce), Some("Bearer sk-leak-canary-42"));
    let mut req = sample_request("m", false);

    // Act
    capture_forwarded_bearer(&headers, &router, &nonce, &mut req);
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

// ============ forwarded-mode ingress x-stainless-* capture gate ====
//
// `capture_stainless_headers` stashes the inbound `x-stainless-*` SDK
// fingerprint headers on `req.routectl_internal.stainless_headers` under
// the SAME two-key gate as the bearer capture (nonce-matching seam header
// present AND `router.has_forwarded_provider()`). Every path with no
// forwarded provider configured MUST leave the carrier byte-identical:
// `stainless_headers` empty. These are NON-secret SDK fingerprint values,
// so no redaction applies -- the security contract here is only that the
// gate stays closed.

/// Build an inbound `HeaderMap`: optionally the `x-routectl-mitm-proxied`
/// seam header stamped with `nonce`'s value, plus an arbitrary set of
/// `(name, value)` header entries. Used to exercise the stainless-capture
/// namespace filter.
fn ingress_headers_with(nonce: Option<&MitmSeamNonce>, entries: &[(&str, &str)]) -> HeaderMap {
    use axum::http::{HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    if let Some(nonce) = nonce {
        h.insert(
            HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
            nonce.header_value(),
        );
    }
    for (name, value) in entries {
        h.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    h
}

/// Both gates turned (Forwarded capability + seam hint): every inbound
/// `x-stainless-*` header lands on `stainless_headers`, order preserved,
/// so the egress can present the client's real SDK fingerprint on the leg.
#[test]
fn stainless_headers_captured_when_forwarded_and_seam_present() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers_with(
        Some(&nonce),
        &[
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", "0.94.0-client"),
            ("x-stainless-os", "Linux"),
        ],
    );
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert
    assert_eq!(
        req.routectl_internal.stainless_headers,
        vec![
            ("x-stainless-lang".to_string(), "js".to_string()),
            (
                "x-stainless-package-version".to_string(),
                "0.94.0-client".to_string()
            ),
            ("x-stainless-os".to_string(), "Linux".to_string()),
        ],
        "all inbound x-stainless-* headers must be captured in inbound order"
    );
}

/// No forwarded provider configured is the default capability: even with
/// the seam hint AND inbound `x-stainless-*` headers present, the config
/// gate is closed, so nothing is captured (carrier byte-identical to
/// pre-passthrough).
#[test]
fn stainless_headers_stays_empty_without_forwarded_provider_even_with_seam() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Own));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers_with(Some(&nonce), &[("x-stainless-lang", "js")]);
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.stainless_headers.is_empty(),
        "no forwarded provider configured must never capture x-stainless-* headers"
    );
}

/// Forwarded capability armed but the seam hint absent: this request did
/// not arrive via the MITM inference leg, so nothing is captured.
#[test]
fn stainless_headers_stays_empty_without_seam_header() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers_with(None, &[("x-stainless-lang", "js")]);
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.stainless_headers.is_empty(),
        "no seam header -> not the MITM inference leg -> no capture"
    );
}

/// Security-critical, stainless side: a spoofed seam-header value must
/// downgrade to seam-ABSENT just like the bearer-capture gate.
#[test]
fn stainless_headers_stays_empty_when_seam_header_carries_a_spoofed_wrong_value() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let mut headers = ingress_headers_with(None, &[("x-stainless-lang", "js")]);
    headers.insert(
        axum::http::HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
        axum::http::HeaderValue::from_static("1"),
    );
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.stainless_headers.is_empty(),
        "a spoofed seam header value must never be treated as seam-present"
    );
}

/// No provider configured at all: `has_forwarded_provider()` is `false`, so
/// the gate stays closed even with the seam header and stainless headers
/// present.
#[test]
fn stainless_headers_stays_empty_when_no_provider_configured() {
    // Arrange
    let router = router_with_provider_credential_source(None);
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers_with(Some(&nonce), &[("x-stainless-lang", "js")]);
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert
    assert!(
        req.routectl_internal.stainless_headers.is_empty(),
        "no configured provider must never capture x-stainless-* headers"
    );
}

/// The capture is namespace-bounded to `x-stainless-*` (case-insensitive):
/// unrelated headers -- including `x-claude-code-*`, which rides its own
/// dedicated carrier -- are never folded into `stainless_headers`.
#[test]
fn stainless_capture_ignores_non_stainless_headers() {
    // Arrange
    let router = router_with_provider_credential_source(Some(CredentialSource::Forwarded));
    let nonce = MitmSeamNonce::generate();
    let headers = ingress_headers_with(
        Some(&nonce),
        &[
            ("X-Stainless-Arch", "x64"),
            ("x-claude-code-session-id", "sid-9"),
            ("anthropic-version", "2023-06-01"),
            ("content-type", "application/json"),
        ],
    );
    let mut req = sample_request("m", false);

    // Act
    capture_stainless_headers(&headers, &router, &nonce, &mut req);

    // Assert: only the (lowercased) x-stainless entry is captured.
    assert_eq!(
        req.routectl_internal.stainless_headers,
        vec![("x-stainless-arch".to_string(), "x64".to_string())],
        "only x-stainless-* names (case-insensitive) belong on this carrier"
    );
}

// ---- extract_authorization_bearer (pure) ----
#[test]
fn extract_bearer_returns_token_after_scheme() {
    let h = ingress_headers(None, Some("Bearer abc123"));
    assert_eq!(extract_authorization_bearer(&h).as_deref(), Some("abc123"));
}

#[test]
fn extract_bearer_scheme_match_is_case_insensitive() {
    for scheme in ["bearer", "BEARER", "BeArEr"] {
        let h = ingress_headers(None, Some(&format!("{scheme} tok-9")));
        assert_eq!(
            extract_authorization_bearer(&h).as_deref(),
            Some("tok-9"),
            "scheme {scheme} must match case-insensitively"
        );
    }
}

#[test]
fn extract_bearer_trims_surrounding_whitespace_around_token() {
    let h = ingress_headers(None, Some("Bearer    padded-tok  "));
    assert_eq!(
        extract_authorization_bearer(&h).as_deref(),
        Some("padded-tok")
    );
}

#[test]
fn extract_bearer_none_when_header_absent() {
    let h = ingress_headers(None, None);
    assert_eq!(extract_authorization_bearer(&h), None);
}

#[test]
fn extract_bearer_none_for_non_bearer_scheme() {
    let h = ingress_headers(None, Some("Basic dXNlcjpwYXNz"));
    assert_eq!(extract_authorization_bearer(&h), None);
}

#[test]
fn extract_bearer_none_for_empty_token() {
    for val in ["Bearer", "Bearer ", "Bearer     "] {
        let h = ingress_headers(None, Some(val));
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
    let h = ingress_headers(None, Some("Bearer sk-ant-oat01-AbC_-.123"));
    assert_eq!(
        extract_authorization_bearer(&h).as_deref(),
        Some("sk-ant-oat01-AbC_-.123")
    );
}

// ============ forwarded-mode ingress admission rejections ======
//
// The forwarded-mode rejection matrix, enforced at the shared ingress driver
// BEFORE parse/dispatch, firing ONLY when the `x-routectl-mitm-proxied` seam
// header carries the process nonce (a request that arrived through the MITM
// inference path). Three layers of coverage, all deterministic (no log
// capture -- that ONE assertion lives in the isolated integration binary
// `tests/pure_proxy_admission_log.rs`, matching the router-side convention,
// because a thread-local capture subscriber over a shared `warn!` callsite
// is unreliable inside this 600+-test lib binary):
//
//   1. `classify_pure_proxy_rejection`  -- the pure decision core: every
//      case + the precedence rules + seam-absent admits-all.
//   2. `render_pure_proxy_rejection`    -- status + dialect envelope +
//      safe reason tag per case; never a token.
//   3. `enforce_pure_proxy_admission`   -- the real header/envelope wiring
//      the driver calls, plus one end-to-end pass through `ingress_handle`.

use crate::handlers::pure_proxy_admission::{
    PureProxyAdmissionInputs, classify_pure_proxy_rejection, enforce_pure_proxy_admission,
    render_pure_proxy_rejection,
};
use crate::handlers::pure_proxy_metrics::PureProxyRejectionReason;
use crate::ingress::MitmSeamNonce;

/// Header builder for the admission tests: optionally the MITM seam header
/// stamped with `nonce`'s value, an `Authorization` value, and the
/// `x-claude-code-session-id` identity header.
fn admission_headers(
    nonce: Option<&MitmSeamNonce>,
    authorization: Option<&str>,
    session_id: Option<&str>,
) -> HeaderMap {
    use axum::http::{HeaderName, HeaderValue, header::AUTHORIZATION};
    let mut h = HeaderMap::new();
    if let Some(nonce) = nonce {
        h.insert(
            HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
            nonce.header_value(),
        );
    }
    if let Some(a) = authorization {
        h.insert(AUTHORIZATION, HeaderValue::from_str(a).unwrap());
    }
    if let Some(s) = session_id {
        h.insert(
            HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_str(s).unwrap(),
        );
    }
    h
}

// ---- Layer 1: classify_pure_proxy_rejection (the pure decision core) ----

/// Seam header ABSENT always admits, regardless of bearer or session id --
/// a request that did not arrive through the MITM inference path is never
/// examined by this gate. This is the "own-provider and non-Anthropic-
/// dialect traffic stays untouched even while a forwarded provider is
/// configured" guarantee at the decision level.
#[test]
fn classify_admits_every_case_when_seam_absent() {
    for has_bearer in [false, true] {
        for has_session_id in [false, true] {
            assert_eq!(
                classify_pure_proxy_rejection(PureProxyAdmissionInputs {
                    seam_present: false,
                    has_bearer,
                    has_session_id,
                }),
                None,
                "seam absent must always admit (bearer={has_bearer}, session={has_session_id})"
            );
        }
    }
}

/// Seam header PRESENT, no bearer -> `TokenMissing` (CC not logged into
/// claude.ai).
#[test]
fn classify_seam_present_no_bearer_is_token_missing() {
    assert_eq!(
        classify_pure_proxy_rejection(PureProxyAdmissionInputs {
            seam_present: true,
            has_bearer: false,
            has_session_id: true,
        }),
        Some(PureProxyRejectionReason::TokenMissing)
    );
}

/// Precedence: a seam-present request missing BOTH the bearer and the
/// session id surfaces `TokenMissing` (401), not `IdentityMissing` -- the
/// more fundamental missing credential wins.
#[test]
fn classify_token_missing_takes_precedence_over_identity_missing() {
    assert_eq!(
        classify_pure_proxy_rejection(PureProxyAdmissionInputs {
            seam_present: true,
            has_bearer: false,
            has_session_id: false,
        }),
        Some(PureProxyRejectionReason::TokenMissing),
        "no bearer AND no session id -> token_missing, not identity_missing"
    );
}

/// Seam present, bearer present, but no session id -> `IdentityMissing`
/// (fail before minting identity).
#[test]
fn classify_seam_present_bearer_no_session_is_identity_missing() {
    assert_eq!(
        classify_pure_proxy_rejection(PureProxyAdmissionInputs {
            seam_present: true,
            has_bearer: true,
            has_session_id: false,
        }),
        Some(PureProxyRejectionReason::IdentityMissing)
    );
}

/// A fully-formed seam-present request (bearer + session id) is ADMITTED --
/// the gate returns `None` and the request proceeds to parse + dispatch.
#[test]
fn classify_seam_present_fully_valid_admits() {
    assert_eq!(
        classify_pure_proxy_rejection(PureProxyAdmissionInputs {
            seam_present: true,
            has_bearer: true,
            has_session_id: true,
        }),
        None
    );
}

// ---- Layer 2: render_pure_proxy_rejection (status + envelope + reason) ---

#[tokio::test]
async fn render_token_missing_is_401_anthropic_authentication_error() {
    // Act
    let resp = render_pure_proxy_rejection(
        ErrorEnvelopeShape::Anthropic,
        PureProxyRejectionReason::TokenMissing,
    );
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["type"], "error", "Anthropic error envelope");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reason=token_missing"),
        "the safe reason tag survives into the client message: {body}"
    );
}

#[tokio::test]
async fn render_identity_missing_is_400_anthropic_invalid_request_error() {
    let resp = render_pure_proxy_rejection(
        ErrorEnvelopeShape::Anthropic,
        PureProxyRejectionReason::IdentityMissing,
    );
    let status = resp.status();
    let body = body_to_value(resp).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reason=identity_missing")
    );
}

/// A rejection message is token-free by construction: it never embeds a
/// bearer-shaped credential value, in any dialect envelope. (The word
/// "bearer" itself may appear as operator guidance -- it is the credential
/// TYPE, not a token; only an actual token value would be a leak.)
#[tokio::test]
async fn render_rejection_message_is_token_free() {
    for reason in PureProxyRejectionReason::ALL {
        for shape in [ErrorEnvelopeShape::Anthropic, ErrorEnvelopeShape::OpenAi] {
            let resp = render_pure_proxy_rejection(shape, reason);
            let body = body_to_value(resp).await;
            let msg = body["error"]["message"].as_str().unwrap_or("");
            assert!(
                !msg.contains("sk-"),
                "rejection message must not embed a token-shaped value: {msg:?}"
            );
        }
    }
}

// ---- Layer 3: enforce_pure_proxy_admission (real header/envelope wiring) --

/// Case 1: seam-marked request with no `Authorization` -> Some(401
/// token_missing), read off real headers.
#[tokio::test]
async fn enforce_case1_token_missing() {
    // Arrange
    let nonce = MitmSeamNonce::generate();
    let headers = admission_headers(Some(&nonce), None, Some("sess-1"));

    // Act
    let resp = enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::Anthropic, &nonce)
        .expect("seam + no bearer must reject");
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reason=token_missing")
    );
}

/// Case 2: a request without the seam header (a direct
/// :9100 loopback client) is ADMITTED regardless of the other headers --
/// the request-global `not_mitm` rejection was dropped, which used to 400
/// this exact traffic shape.
#[tokio::test]
async fn enforce_case2_no_seam_is_admitted() {
    let nonce = MitmSeamNonce::generate();
    let headers = admission_headers(None, Some("Bearer sk-direct"), Some("sess-2"));

    assert!(
        enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::Anthropic, &nonce).is_none(),
        "no seam header must be admitted, not rejected as not_mitm"
    );
}

/// Security-critical: a spoofed seam-header value (does not match the
/// process nonce) must downgrade to seam-ABSENT and be admitted, exactly
/// like a missing header -- proving a direct caller gains nothing by
/// guessing/spoofing the header.
#[tokio::test]
async fn enforce_spoofed_seam_header_downgrades_to_absent_and_is_admitted() {
    let nonce = MitmSeamNonce::generate();
    let mut headers = admission_headers(None, None, None);
    headers.insert(
        axum::http::HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
        axum::http::HeaderValue::from_static("1"),
    );

    assert!(
        enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::Anthropic, &nonce).is_none(),
        "a spoofed seam header (no bearer, no session id) must be admitted, not rejected as \
         token_missing"
    );
}

/// Case 3: seam-marked request with a bearer but missing
/// `x-claude-code-session-id` -> Some(400 identity_missing). The distinctive
/// bearer in the header must NOT surface in the client response.
#[tokio::test]
async fn enforce_case3_identity_missing_and_never_leaks_bearer() {
    const TOKEN: &str = "sk-ant-oat01-LEAK-CANARY-identity";
    let nonce = MitmSeamNonce::generate();
    let headers = admission_headers(Some(&nonce), Some(&format!("Bearer {TOKEN}")), None);

    let resp = enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::Anthropic, &nonce)
        .expect("seam + bearer + no session id must reject");
    let status = resp.status();
    let body = body_to_value(resp).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reason=identity_missing")
    );
    assert!(
        !body.to_string().contains(TOKEN),
        "the inbound bearer must never appear in the rejection response: {body}"
    );
}

/// Case 4: a fully-formed seam-present request (bearer +
/// session id) is ADMITTED even on the OpenAI-envelope dialect -- the
/// request-global `non_anthropic_dialect` rejection was dropped, which used
/// to 400 this exact case even with every other admission key satisfied.
#[tokio::test]
async fn enforce_case4_seam_present_openai_envelope_is_admitted() {
    let nonce = MitmSeamNonce::generate();
    let headers = admission_headers(Some(&nonce), Some("Bearer sk-x"), Some("sess-4"));

    assert!(
        enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::OpenAi, &nonce).is_none(),
        "a fully-formed seam-present request must be admitted regardless of dialect"
    );
}

/// The two non-Anthropic ingress adapters both declare the OpenAI envelope
/// shape; the Anthropic adapter declares the Anthropic shape. Used
/// throughout ingress error rendering (this admission gate included) to
/// pick the dialect-correct envelope.
#[test]
fn adapter_envelope_shapes_are_dialect_correct() {
    use crate::ingress::IngressAdapter;
    use crate::ingress::anthropic::AnthropicIngress;
    use crate::ingress::openai::OpenAiIngress;
    use crate::ingress::openai_responses::ResponsesIngress;

    assert_eq!(
        OpenAiIngress.error_envelope_shape(),
        ErrorEnvelopeShape::OpenAi,
        "OpenAI chat-completions ingress is a non-Anthropic dialect"
    );
    assert_eq!(
        ResponsesIngress.error_envelope_shape(),
        ErrorEnvelopeShape::OpenAi,
        "OpenAI Responses ingress is a non-Anthropic dialect"
    );
    assert_eq!(
        AnthropicIngress.error_envelope_shape(),
        ErrorEnvelopeShape::Anthropic,
        "the Anthropic ingress is the only Anthropic dialect"
    );
}

/// A fully-formed seam-present request (bearer + session id) is ADMITTED
/// through the real wiring -- the gate returns `None` and the request
/// proceeds to parse + dispatch.
#[test]
fn enforce_seam_present_fully_valid_is_admitted() {
    let nonce = MitmSeamNonce::generate();
    let headers = admission_headers(Some(&nonce), Some("Bearer sk-valid"), Some("sess-ok"));
    assert!(
        enforce_pure_proxy_admission(&headers, ErrorEnvelopeShape::Anthropic, &nonce).is_none(),
        "a well-formed seam-present request must be admitted"
    );
}

/// End-to-end through `ingress_handle`: a seam-marked request with no
/// `Authorization` is rejected at admission BEFORE parse/dispatch, so the
/// client receives the 401 Anthropic envelope directly from the driver.
#[tokio::test]
async fn ingress_handle_rejects_forwarded_token_missing_end_to_end() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: any router (admission no longer reads config) + a
    // seam-marked, bearer-less request. The header MUST carry the state's
    // own nonce -- a bare/wrong value would downgrade to seam-absent and
    // never reach the rejection this test proves.
    let router = k_test_router();
    let swap = Arc::new(arc_swap::ArcSwap::from(router));
    let (state, _dir) = AppState::for_test(swap);
    let headers = admission_headers(Some(&state.mitm_seam_nonce), None, Some("sess-e2e"));
    // Admission runs before body parse, so the body is never inspected on the
    // rejection path.
    let body: std::result::Result<axum::body::Bytes, axum::extract::rejection::BytesRejection> =
        Ok(axum::body::Bytes::from_static(b"{}"));

    // Act
    let resp = ingress_handle(state, headers, None, body, AnthropicIngress).await;
    let status = resp.status();
    let out = body_to_value(resp).await;

    // Assert: the driver returned the admission rejection, not a parse or
    // dispatch error.
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(out["type"], "error");
    assert_eq!(out["error"]["type"], "authentication_error");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reason=token_missing")
    );
}

/// Security-critical end-to-end: a request that spoofs the seam header with
/// a value that does NOT match the process's `MitmSeamNonce` must be
/// admitted (not rejected at admission), and the forwarded-bearer capture
/// gate must never arm -- proving the fix at the full driver level, not
/// just at the pure decision core.
#[tokio::test]
async fn ingress_handle_admits_a_spoofed_seam_header_end_to_end() {
    use crate::ingress::anthropic::AnthropicIngress;

    let router = k_test_router();
    let swap = Arc::new(arc_swap::ArcSwap::from(router));
    let (state, _dir) = AppState::for_test(swap);
    let mut headers = admission_headers(None, None, None);
    headers.insert(
        axum::http::HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER),
        axum::http::HeaderValue::from_static("1"),
    );
    // Content-type so the request flows past the ingress content-type gate
    // into the parse + capture path this test exercises (the `Bytes`
    // extractor no longer enforces it; the handler does).
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let body: std::result::Result<axum::body::Bytes, axum::extract::rejection::BytesRejection> = Ok(
        axum::body::Bytes::from_static(b"{\"model\": \"m\", \"messages\": []}"),
    );

    let resp = ingress_handle(state, headers, None, body, AnthropicIngress).await;
    let status = resp.status();

    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "a spoofed seam header must never trigger the token_missing admission rejection"
    );
}

const LEAK_PROVIDER: &str = "secret-provider-id";
const LEAK_DETAIL: &str = "serde detail xyz";

async fn assert_body_redacts_leaks(
    resp: Response,
    expected_status: StatusCode,
    expected_msg: &str,
) {
    let status = resp.status();
    let body = body_to_value(resp).await;
    let rendered = body.to_string();

    assert_eq!(status, expected_status);
    assert!(
        !rendered.contains(LEAK_PROVIDER),
        "client body leaked the internal provider id: {rendered}"
    );
    assert!(
        !rendered.contains(LEAK_DETAIL),
        "client body leaked the internal normalization detail: {rendered}"
    );
    assert_eq!(
        body["error"]["message"].as_str().unwrap_or(""),
        expected_msg,
        "client body must carry only the fixed opaque message"
    );
}

#[tokio::test]
async fn map_error_normalize_request_redacts_provider_and_detail_both_dialects() {
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let err = Error::NormalizeRequest(LEAK_PROVIDER.into(), LEAK_DETAIL.into());
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::BAD_REQUEST,
            "request could not be prepared for the upstream",
        )
        .await;
    }
}

#[tokio::test]
async fn map_error_normalize_response_redacts_provider_and_detail_both_dialects() {
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let err = Error::NormalizeResponse(LEAK_PROVIDER.into(), LEAK_DETAIL.into());
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::BAD_GATEWAY,
            "upstream response could not be processed",
        )
        .await;
    }
}

#[tokio::test]
async fn map_error_not_implemented_redacts_provider_and_detail_both_dialects() {
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let err = Error::NotImplemented(LEAK_PROVIDER.into(), LEAK_DETAIL.into());
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::NOT_IMPLEMENTED,
            "requested capability is not implemented",
        )
        .await;
    }
}

#[test]
fn map_error_normalize_request_logs_full_detail_server_side() {
    let events = routectl_testkit::capture_events(|| {
        let err = Error::NormalizeRequest(LEAK_PROVIDER.into(), LEAK_DETAIL.into());
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("normalize-request must emit a server-side ERROR log");
    assert_eq!(logged.field("provider"), Some(LEAK_PROVIDER));
    assert_eq!(logged.field("detail"), Some(LEAK_DETAIL));
}

#[test]
fn map_error_normalize_response_logs_full_detail_server_side() {
    let events = routectl_testkit::capture_events(|| {
        let err = Error::NormalizeResponse(LEAK_PROVIDER.into(), LEAK_DETAIL.into());
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("normalize-response must emit a server-side ERROR log");
    assert_eq!(logged.field("provider"), Some(LEAK_PROVIDER));
    assert_eq!(logged.field("detail"), Some(LEAK_DETAIL));
}

#[test]
fn map_error_normalize_request_log_caps_long_user_payload_keeps_provider() {
    // A normalization error can format a raw request fragment into its detail
    // (see the openai_compat tool_use lift). The logging boundary routes the
    // detail through the log-safety helper, so a long embedded user payload is
    // truncated out of the server log line while the provider id stays logged
    // for triage. Complements the short-detail server-side test above, which
    // pins that a small detail is still logged in full.
    let payload_marker = "PAYLOAD_MARKER_PAST_THE_CAP";
    let detail = format!(
        "tool_use block is not an object: {}{payload_marker}",
        "x".repeat(512)
    );

    let events = routectl_testkit::capture_events(|| {
        let err = Error::NormalizeRequest(LEAK_PROVIDER.into(), detail.clone());
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("normalize-request must emit a server-side ERROR log");
    assert_eq!(logged.field("provider"), Some(LEAK_PROVIDER));
    let detail_field = logged.field("detail").expect("detail field logged");
    assert!(
        !detail_field.contains(payload_marker),
        "server log leaked the user payload past the length cap: {detail_field}"
    );
}

#[tokio::test]
async fn map_error_validation_preserves_exact_message_after_exhaustive_match() {
    // Regression pin: an unchanged newly-explicit arm must reproduce
    // the prior `e.to_string()` output byte-for-byte.
    let err = Error::Validation("max_tokens must be positive".into());
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;
    assert_eq!(
        body["error"]["message"].as_str().unwrap_or(""),
        "validation: max_tokens must be positive",
    );
}

// -------- map_error: residual leak arms (UnknownProvider / Io / Json) --

#[tokio::test]
async fn map_error_unknown_provider_redacts_config_id_both_dialects() {
    // The provider id is config-derived routing topology, not caller
    // input: the client body must carry only the opaque class message.
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let err = Error::UnknownProvider(LEAK_PROVIDER.into());
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::NOT_FOUND,
            "requested target is not configured",
        )
        .await;
    }
}

#[test]
fn map_error_unknown_provider_logs_id_server_side() {
    let events = routectl_testkit::capture_events(|| {
        let err = Error::UnknownProvider(LEAK_PROVIDER.into());
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("unknown-provider must emit a server-side ERROR log");
    assert_eq!(logged.field("provider"), Some(LEAK_PROVIDER));
}

#[tokio::test]
async fn map_error_io_redacts_detail_both_dialects() {
    // A bare io::Error Display can embed a filesystem path; the client
    // body must carry only the opaque class message.
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let err = Error::Io(std::io::Error::other(LEAK_DETAIL));
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal I/O error",
        )
        .await;
    }
}

#[test]
fn map_error_io_logs_detail_server_side_through_sanitizer() {
    // A control char in the io detail proves the logged field flowed
    // through sanitize_detail_for_log rather than being logged raw: the
    // sanitizer replaces it with '?', so a future refactor that dropped
    // the sanitizer on this leak boundary would fail here.
    let raw = "disk error\x07detail";
    let events = routectl_testkit::capture_events(|| {
        let err = Error::Io(std::io::Error::other(raw));
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("io error must emit a server-side ERROR log");
    let detail = logged.field("detail").expect("detail field logged");
    assert!(
        !detail.contains('\x07'),
        "control char must be filtered by the sanitizer: {detail:?}"
    );
    assert!(
        detail.contains("disk error") && detail.contains("detail"),
        "printable content must survive sanitization: {detail:?}"
    );
}

#[tokio::test]
async fn map_error_json_redacts_payload_both_dialects() {
    // A serde_json::Error Display can embed a request payload fragment;
    // the client body must carry only the opaque class message.
    for shape in [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic] {
        let json_err = serde_json::from_str::<u64>("\"serde detail xyz\"").unwrap_err();
        let err = Error::Json(json_err);
        let resp = map_error(shape, err);
        assert_body_redacts_leaks(
            resp,
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid JSON in request body",
        )
        .await;
    }
}

#[test]
fn map_error_json_logs_detail_server_side() {
    let json_err = serde_json::from_str::<u64>("\"serde detail xyz\"").unwrap_err();
    let expected = json_err.to_string();
    let events = routectl_testkit::capture_events(|| {
        let err = Error::Json(json_err);
        let _resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    });

    let logged = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("json error must emit a server-side ERROR log");
    assert_eq!(logged.field("detail"), Some(expected.as_str()));
}

/// The `Bytes` extractor does not gate on content-type, so the handler
/// does it via `is_json_content_type`. Pin the essence cases the axum
/// `Json` extractor used to own: `application/json`, a `+json` suffix,
/// charset parameters, case-insensitivity -- and the reject cases (a
/// non-JSON type and an absent header both fall through to 415).
#[test]
fn is_json_content_type_matches_json_essences_and_rejects_others() {
    fn with_ct(value: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    for accepted in [
        "application/json",
        "application/json; charset=utf-8",
        "APPLICATION/JSON",
        "application/vnd.foo+json",
        "application/vnd.foo+json; charset=utf-8",
    ] {
        assert!(
            is_json_content_type(&with_ct(accepted)),
            "should accept {accepted}"
        );
    }

    for rejected in ["text/plain", "application/xml", "application/octet-stream"] {
        assert!(
            !is_json_content_type(&with_ct(rejected)),
            "should reject {rejected}"
        );
    }

    assert!(
        !is_json_content_type(&axum::http::HeaderMap::new()),
        "absent content-type must reject (-> 415)"
    );
}

/// Pre-change ingress rejection contract + forward-compat extras sweep.
///
/// Locks per-endpoint wire behavior across the switch to `Bytes`
/// extraction with hand-rolled 4xx rendering. The rejection tests drive
/// the REAL per-endpoint handler functions mounted behind the same
/// `DefaultBodyLimit` layer `server::serve::build_axum_router` installs,
/// via `oneshot`, so the `Bytes` extractor AND the body-size layer are
/// exercised exactly as in production. The extras tests go through the
/// `IngressAdapter::parse_request` trait boundary.
mod pre_change_ingress_contract {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::DefaultBodyLimit;
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::routing::post;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::ingress::IngressAdapter;
    use crate::ingress::anthropic::AnthropicIngress;
    use crate::ingress::openai::OpenAiIngress;
    use crate::ingress::openai_responses::ResponsesIngress;
    use crate::server::AppState;

    /// Small cap so an oversized body is a few KB, not tens of MB.
    const REJECT_BODY_LIMIT: usize = 1024;

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let swap = Arc::new(arc_swap::ArcSwap::from(super::k_test_router()));
        AppState::for_test(swap)
    }

    fn post_req(uri: &str, content_type: Option<&str>, body: impl Into<Body>) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        builder.body(body.into()).expect("request builds")
    }

    /// A syntactically valid JSON body whose byte length exceeds
    /// `REJECT_BODY_LIMIT`, so the body-size layer -- not the JSON parser
    /// -- is what rejects it.
    fn oversized_body() -> String {
        format!(
            "{{\"model\":\"m\",\"pad\":\"{}\"}}",
            "a".repeat(REJECT_BODY_LIMIT * 2)
        )
    }

    async fn drive(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.oneshot(req).await.expect("router is infallible");
        let status = resp.status();
        let body = super::body_to_value(resp).await;
        (status, body)
    }

    fn app_for_messages(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/v1/messages", post(crate::handlers::messages::messages))
            .layer(DefaultBodyLimit::max(REJECT_BODY_LIMIT))
            .with_state(state)
    }

    fn app_for_chat_completions(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(crate::handlers::chat_completions::chat_completions),
            )
            .layer(DefaultBodyLimit::max(REJECT_BODY_LIMIT))
            .with_state(state)
    }

    fn app_for_responses(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/v1/responses", post(crate::handlers::responses::responses))
            .layer(DefaultBodyLimit::max(REJECT_BODY_LIMIT))
            .with_state(state)
    }

    /// Pin the full Anthropic-dialect rejection envelope. The routectl-owned
    /// shape and classifier are asserted byte-for-byte; the human message
    /// string is asserted only non-empty (the hand-rolled renderer owns its
    /// wording), so the pin never couples to a specific message string.
    fn assert_anthropic_reject(status: StatusCode, body: &Value, expected: StatusCode) {
        assert_eq!(status, expected, "anthropic rejection status");
        let msg = body["error"]["message"]
            .as_str()
            .expect("anthropic envelope carries error.message string");
        assert!(!msg.is_empty(), "rejection message must be non-empty");
        assert_eq!(
            *body,
            json!({
                "type": "error",
                "error": { "type": "invalid_request_error", "message": msg }
            }),
            "exact Anthropic rejection envelope"
        );
    }

    /// Pin the full OpenAI-dialect rejection envelope (same message caveat
    /// as `assert_anthropic_reject`). `expected_type` is the per-mode
    /// classifier surfaced on `error.type`.
    fn assert_openai_reject(
        status: StatusCode,
        body: &Value,
        expected: StatusCode,
        expected_type: &str,
    ) {
        assert_eq!(status, expected, "openai rejection status");
        let msg = body["error"]["message"]
            .as_str()
            .expect("openai envelope carries error.message string");
        assert!(!msg.is_empty(), "rejection message must be non-empty");
        assert_eq!(
            *body,
            json!({
                "error": {
                    "message": msg,
                    "type": expected_type,
                    "param": Value::Null,
                    "code": "invalid_request_error",
                }
            }),
            "exact OpenAI rejection envelope"
        );
    }

    #[tokio::test]
    async fn messages_rejection_contract_pins_status_and_envelope() {
        // JSON syntax error -> 400 + Anthropic envelope.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_messages(state),
            post_req("/v1/messages", Some("application/json"), "{ not valid json"),
        )
        .await;
        assert_anthropic_reject(status, &body, StatusCode::BAD_REQUEST);

        // Wrong content-type -> 415.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_messages(state),
            post_req("/v1/messages", Some("text/plain"), "{}"),
        )
        .await;
        assert_anthropic_reject(status, &body, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Oversized body -> 413 (DefaultBodyLimit layer).
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_messages(state),
            post_req("/v1/messages", Some("application/json"), oversized_body()),
        )
        .await;
        assert_anthropic_reject(status, &body, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn chat_completions_rejection_contract_pins_status_and_envelope() {
        // JSON syntax error -> 400 + OpenAI envelope.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_chat_completions(state),
            post_req(
                "/v1/chat/completions",
                Some("application/json"),
                "{ not valid json",
            ),
        )
        .await;
        assert_openai_reject(status, &body, StatusCode::BAD_REQUEST, "bad_request");

        // Wrong content-type -> 415.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_chat_completions(state),
            post_req("/v1/chat/completions", Some("text/plain"), "{}"),
        )
        .await;
        assert_openai_reject(
            status,
            &body,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        );

        // Oversized body -> 413 (DefaultBodyLimit layer).
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_chat_completions(state),
            post_req(
                "/v1/chat/completions",
                Some("application/json"),
                oversized_body(),
            ),
        )
        .await;
        assert_openai_reject(
            status,
            &body,
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        );
    }

    #[tokio::test]
    async fn responses_rejection_contract_pins_status_and_envelope() {
        // JSON syntax error -> 400 + OpenAI envelope.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_responses(state),
            post_req(
                "/v1/responses",
                Some("application/json"),
                "{ not valid json",
            ),
        )
        .await;
        assert_openai_reject(status, &body, StatusCode::BAD_REQUEST, "bad_request");

        // Wrong content-type -> 415.
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_responses(state),
            post_req("/v1/responses", Some("text/plain"), "{}"),
        )
        .await;
        assert_openai_reject(
            status,
            &body,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        );

        // Oversized body -> 413 (DefaultBodyLimit layer).
        let (state, _dir) = test_state();
        let (status, body) = drive(
            app_for_responses(state),
            post_req("/v1/responses", Some("application/json"), oversized_body()),
        )
        .await;
        assert_openai_reject(
            status,
            &body,
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        );
    }

    #[test]
    fn anthropic_parse_request_sweeps_unknown_top_level_field_into_provider_extras() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "future_unknown_knob": {"nested": [1, 2, 3]}
        });
        let req = AnthropicIngress
            .parse_request_value(&HeaderMap::new(), body)
            .expect("valid Anthropic body parses");
        let extras = req
            .provider_extras
            .expect("unknown top-level field must round-trip into provider_extras");
        assert_eq!(extras["future_unknown_knob"], json!({"nested": [1, 2, 3]}));
    }

    #[test]
    fn openai_parse_request_sweeps_unknown_top_level_field_into_provider_extras() {
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [{"role": "user", "content": "hi"}],
            "future_unknown_knob": "keep-me"
        });
        let req = OpenAiIngress
            .parse_request_value(&HeaderMap::new(), body)
            .expect("valid OpenAI body parses");
        let extras = req
            .provider_extras
            .expect("unknown top-level field must round-trip into provider_extras");
        assert_eq!(extras["future_unknown_knob"], "keep-me");
    }

    #[test]
    fn responses_parse_request_sweeps_unknown_top_level_field_into_provider_extras() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "future_unknown_knob": "keep-me"
        });
        let req = ResponsesIngress
            .parse_request_value(&HeaderMap::new(), body)
            .expect("valid Responses body parses");
        let extras = req
            .provider_extras
            .expect("unknown top-level field must round-trip into provider_extras");
        assert_eq!(extras["future_unknown_knob"], "keep-me");
    }
}
