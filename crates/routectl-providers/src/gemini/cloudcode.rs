//! Cloud Code ("antigravity") egress mode for the Gemini provider.
//!
//! This is a second wire dialect for the same Gemini translation. Instead
//! of the public `generativelanguage.googleapis.com` REST surface (api-key
//! auth, `/models/{model}:generateContent`), Cloud Code talks to
//! `cloudcode-pa.googleapis.com/v1internal:*` with a bearer token and a
//! request envelope: the normalized inner `GenerateContentRequest` is
//! wrapped under a `request` key alongside a `project` and `model`. The
//! response arrives wrapped under a top-level `response` key (per-chunk on
//! the streaming path), which this module unwraps before handing the inner
//! body to the shared response / SSE translation.
//!
//! Onboarding: the project id is not known up front. `resolve_project_id`
//! ports the reference `FetchProjectID` flow -- `loadCodeAssist` to read an
//! already-provisioned project, falling back to `onboardUser` (polled) when
//! none exists. The resolved id is cached by the caller so the onboarding
//! HTTP is never re-run once known.

use std::time::Duration;

use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use routectl_core::{Error, Result, sanitize_for_log, sanitize_upstream_body};

// Wire constants. Kept private to this module: they are part of the
// Cloud Code dialect, not configurable provider knobs.

pub const PROD_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
pub const DAILY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

pub const GENERATE_PATH: &str = "/v1internal:generateContent";
pub const STREAM_PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
const LOAD_CODE_ASSIST_PATH: &str = "/v1internal:loadCodeAssist";
const ONBOARD_USER_PATH: &str = "/v1internal:onboardUser";

/// Short User-Agent sent on generate / stream / loadCodeAssist. Pinned to
/// the reference client's fallback version; routectl does
/// not run a live version-fetcher.
pub const SHORT_USER_AGENT: &str =
    "antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64)";
/// Node User-Agent sent only on onboardUser (the reference uses the
/// google-api-nodejs-client UA there).
const NODE_USER_AGENT: &str = "antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64) google-api-nodejs-client/10.3.0";
const GOOG_API_CLIENT: &str = "gl-node/22.21.1";

const IDE_VERSION: &str = "1.0.13";
const DEFAULT_TIER: &str = "free-tier";

const ONBOARD_MAX_ATTEMPTS: u32 = 5;

/// Selects which Gemini wire dialect a provider speaks.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum GeminiAuthMode {
    /// Public `generativelanguage.googleapis.com` REST surface with an
    /// `x-goog-api-key` header.
    #[default]
    ApiKey,
    /// Cloud Code (`cloudcode-pa.googleapis.com`) with a bearer token and
    /// the request/response envelope.
    CloudCode,
}

/// Wrap a normalized inner request body in the Cloud Code envelope.
///
/// The inner `GenerateContentRequest` carries no top-level `model` field
/// (Gemini's `generateContent` puts the model in the URL); the Cloud Code
/// surface instead names the model in the envelope, so it is added here.
pub fn wrap_envelope(inner: Value, project_id: &str, model: &str) -> Value {
    json!({
        "project": project_id,
        "request": inner,
        "model": model,
    })
}

/// Unwrap a Cloud Code generate response. The upstream returns the real
/// `GenerateContentResponse` nested under a top-level `response` object;
/// peel it off so the shared response translator sees the same shape the
/// public REST surface returns. Lenient: a body that is not wrapped is
/// passed through unchanged (matches the reference's tolerance).
pub fn unwrap_response(raw: Value) -> Value {
    match raw.get("response") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => raw,
    }
}

/// Unwrap one SSE `data:` payload. Each streamed chunk is itself a
/// `{"response": {GenerateContentResponse}}` envelope; return the inner
/// object serialized so the shared SSE parser sees the bare partial
/// response. Anything that is not a wrapped object is returned unchanged.
pub fn unwrap_sse_data(data: &str) -> String {
    let parsed: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return data.to_string(),
    };
    match parsed.get("response") {
        Some(inner) if inner.is_object() => inner.to_string(),
        _ => data.to_string(),
    }
}

/// Apply `Authorization: Bearer <token>`, `Accept: */*` and
/// `Content-Type: application/json` to an onboarding request builder.
fn onboarding_headers(rb: RequestBuilder, token: &str) -> RequestBuilder {
    rb.header("authorization", format!("Bearer {token}"))
        .header("accept", "*/*")
        .header("content-type", "application/json")
}

/// Extract a project id from a Cloud Code JSON object. Tries
/// `cloudaicompanionProject`, `projectId`, then `project`; each value may
/// be a non-empty (trimmed) string, or an object whose `.id` is a
/// non-empty string. Returns `None` when nothing usable is present.
pub fn extract_project_id(obj: &Value) -> Option<String> {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        match obj.get(key) {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            Some(Value::Object(_)) => {
                if let Some(id) = obj[key].get("id").and_then(Value::as_str) {
                    let trimmed = id.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Compute the onboarding tier from a `loadCodeAssist` response: the
/// `allowedTiers[]` entry flagged `isDefault`, else `currentTier.id`, else
/// the `free-tier` fallback.
pub fn default_tier(load_resp: &Value) -> String {
    if let Some(tiers) = load_resp.get("allowedTiers").and_then(Value::as_array) {
        for tier in tiers {
            let is_default = tier.get("isDefault").and_then(Value::as_bool) == Some(true);
            if !is_default {
                continue;
            }
            if let Some(id) = tier.get("id").and_then(Value::as_str) {
                let trimmed = id.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    if let Some(id) = load_resp
        .get("currentTier")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
    {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    DEFAULT_TIER.to_string()
}

/// Trim and sanitize an upstream error body for inclusion in a routectl
/// Error. Two-pass, mirroring the shared reference pattern (bedrock):
/// `sanitize_upstream_body` trims edges, collapses HTML pages to a short
/// marker, and caps length on a char boundary (never panics mid-codepoint
/// on multibyte UTF-8); `sanitize_for_log` then strips the control / ANSI
/// chars the first pass leaves. Never carries the bearer token (the token
/// is only ever set in the Authorization header, never echoed by these
/// endpoints in a body).
fn clean_error_body(body: &str) -> String {
    sanitize_for_log(&sanitize_upstream_body(body))
}

/// Lift the Google Cloud Code error classifier from an onboarding error
/// body. Google's envelope is `{"error":{"code":<num>,"status":"<CANONICAL>"}}`;
/// `status` (e.g. `RESOURCE_EXHAUSTED`, `UNAUTHENTICATED`) becomes the
/// upstream type and the numeric `code` becomes the upstream code, so a
/// quota / auth failure stays distinguishable from a generic 429 / 401
/// downstream. Returns `(None, None)` for a non-JSON or non-enveloped body.
fn parse_cloudcode_error_classifier(body: &str) -> (Option<String>, Option<String>) {
    super::parse_gemini_error_classifier(body)
}

/// Whether `err` signals that the resolved Cloud Code project no longer
/// applies (revoked, deleted, or never valid for this caller), so the
/// cached project id must be invalidated and re-resolved on the next
/// request. The signal is the bare Google canonical classifier only:
/// `PERMISSION_DENIED` or `NOT_FOUND` on an `Upstream` error.
///
/// Everything else is not a mismatch and leaves the cache untouched:
/// `UNAUTHENTICATED` (a credential problem, not a project one),
/// `RESOURCE_EXHAUSTED` (quota), any 5xx or transport-level (`status 0`)
/// failure, and any non-`Upstream` variant. No body-substring inspection --
/// the enum token is the whole signal.
pub(super) fn is_project_mismatch(err: &Error) -> bool {
    matches!(
        err,
        Error::Upstream { upstream_type: Some(t), .. }
            if &**t == "PERMISSION_DENIED" || &**t == "NOT_FOUND"
    )
}

/// Map an onboarding (`loadCodeAssist` / `onboardUser`) HTTP failure into a
/// routectl upstream error, preserving the Google Cloud Code classifier
/// (`error.status` / `error.code`) and any rate-limit reset hint. Lifts the
/// classifier via `Error::upstream_full` -- the path the reference providers
/// use -- so a `RESOURCE_EXHAUSTED` / `UNAUTHENTICATED` is not collapsed
/// into an indistinguishable generic 429 / 401.
///
/// When `hit_cap` is set the body was truncated at the shared response-body
/// cap and is untrustworthy: the client-facing message collapses to the
/// fixed cap-exceeded text (never echoing truncated bytes) while the
/// classifier enum tokens still lift when the prefix happens to parse.
fn map_onboarding_error(
    provider_id: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body_text: &str,
    hit_cap: bool,
) -> Error {
    let retry_after = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    let (upstream_type, upstream_code) = parse_cloudcode_error_classifier(body_text);
    let body = if hit_cap {
        crate::http_client::body_cap_exceeded_message()
    } else {
        clean_error_body(body_text)
    };
    Error::upstream_full(
        provider_id,
        status,
        body,
        retry_after,
        upstream_type,
        upstream_code,
    )
    .with_upstream_request_id(crate::upstream_request_id::parse_upstream_request_id(
        headers,
    ))
}

/// POST `loadCodeAssist` and return the parsed JSON response. The caller
/// reads the project id (or computes the default tier) from it.
pub async fn load_code_assist(
    client: &Client,
    token: &str,
    generate_base: &str,
    provider_id: &str,
) -> Result<Value> {
    let url = format!(
        "{}{}",
        generate_base.trim_end_matches('/'),
        LOAD_CODE_ASSIST_PATH
    );
    let body = json!({"metadata": {"ideType": "ANTIGRAVITY"}});
    let rb = onboarding_headers(client.post(&url), token).header("user-agent", SHORT_USER_AGENT);
    let resp = rb
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::upstream(provider_id, 0, e.to_string()))?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let content_length = resp.content_length();
    let (bytes, hit_cap) = match crate::http_client::read_body_capped(
        resp,
        crate::http_client::MAX_RESPONSE_BODY_BYTES,
    )
    .await
    {
        Ok(read) => read,
        // A mid-read transport failure on an error response is not a cap
        // trip: degrade to an empty body plus a WARN so the real upstream
        // status is preserved, matching the five sibling error-body paths.
        // A read failure on a 2xx still surfaces as a transport error below.
        Err(e) if status >= 400 => {
            tracing::warn!(
                provider = %provider_id,
                status,
                error = %e,
                "failed to read upstream error body",
            );
            (Vec::new(), false)
        }
        Err(e) => return Err(Error::upstream(provider_id, 0, e.to_string())),
    };
    let body_text = String::from_utf8_lossy(&bytes);
    if status >= 400 {
        if hit_cap {
            crate::http_client::warn_body_cap(provider_id, status, content_length, "error_body");
        }
        return Err(map_onboarding_error(
            provider_id,
            status,
            &headers,
            &body_text,
            hit_cap,
        ));
    }
    if hit_cap {
        crate::http_client::warn_body_cap(provider_id, status, content_length, "success_body");
        return Err(Error::upstream(
            provider_id,
            502,
            crate::http_client::body_cap_exceeded_message(),
        ));
    }
    serde_json::from_str(&body_text).map_err(|e| {
        Error::Internal(format!(
            "gemini provider `{provider_id}`: loadCodeAssist decode failed: {e}"
        ))
    })
}

/// POST `onboardUser` (polled up to `ONBOARD_MAX_ATTEMPTS`) and return the
/// resolved project id. On a 200 with `done == true`, reads the id from the
/// nested `response` object; otherwise sleeps `poll_interval` and retries.
/// A non-200 is a hard error; exhausting the attempts is a hard error.
pub async fn onboard_user(
    client: &Client,
    token: &str,
    onboard_base: &str,
    tier_id: &str,
    poll_interval: Duration,
    provider_id: &str,
) -> Result<String> {
    let url = format!(
        "{}{}",
        onboard_base.trim_end_matches('/'),
        ONBOARD_USER_PATH
    );
    let body = json!({
        "tier_id": tier_id,
        "metadata": {
            "ide_type": "ANTIGRAVITY",
            "ide_version": IDE_VERSION,
            "ide_name": "antigravity",
        },
    });

    for attempt in 0..ONBOARD_MAX_ATTEMPTS {
        let rb = onboarding_headers(client.post(&url), token)
            .header("user-agent", NODE_USER_AGENT)
            .header("x-goog-api-client", GOOG_API_CLIENT);
        let resp = rb
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::upstream(provider_id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let content_length = resp.content_length();
        let (bytes, hit_cap) = match crate::http_client::read_body_capped(
            resp,
            crate::http_client::MAX_RESPONSE_BODY_BYTES,
        )
        .await
        {
            Ok(read) => read,
            // A mid-read transport failure on an error response is not a cap
            // trip: degrade to an empty body plus a WARN so the real upstream
            // status is preserved, matching the five sibling error-body paths.
            // A read failure on a 2xx still surfaces as a transport error below.
            Err(e) if status >= 400 => {
                tracing::warn!(
                    provider = %provider_id,
                    status,
                    error = %e,
                    "failed to read upstream error body",
                );
                (Vec::new(), false)
            }
            Err(e) => return Err(Error::upstream(provider_id, 0, e.to_string())),
        };
        let body_text = String::from_utf8_lossy(&bytes);
        if status != 200 {
            if hit_cap {
                crate::http_client::warn_body_cap(
                    provider_id,
                    status,
                    content_length,
                    "error_body",
                );
            }
            return Err(map_onboarding_error(
                provider_id,
                status,
                &headers,
                &body_text,
                hit_cap,
            ));
        }
        if hit_cap {
            crate::http_client::warn_body_cap(provider_id, status, content_length, "success_body");
            return Err(Error::upstream(
                provider_id,
                502,
                crate::http_client::body_cap_exceeded_message(),
            ));
        }

        let data: Value = serde_json::from_str(&body_text).map_err(|e| {
            Error::Internal(format!(
                "gemini provider `{provider_id}`: onboardUser decode failed: {e}"
            ))
        })?;

        if data.get("done").and_then(Value::as_bool) == Some(true) {
            let project = data
                .get("response")
                .filter(|r| r.is_object())
                .and_then(extract_project_id);
            return project.ok_or_else(|| {
                Error::Internal(format!(
                    "gemini provider `{provider_id}`: onboardUser completed without a project id"
                ))
            });
        }

        // Wait between polls only -- never after the final attempt, so an
        // account that never provisions surfaces the error without an
        // extra idle delay.
        if attempt + 1 < ONBOARD_MAX_ATTEMPTS {
            tokio::time::sleep(poll_interval).await;
        }
    }

    Err(Error::Internal(format!(
        "gemini provider `{provider_id}`: onboardUser did not complete after {ONBOARD_MAX_ATTEMPTS} attempts"
    )))
}

/// Resolve the Cloud Code project id: read it from `loadCodeAssist`, or
/// onboard via `onboardUser` when no project is provisioned yet. Ports the
/// reference `FetchProjectID` flow.
pub async fn resolve_project_id(
    client: &Client,
    token: &str,
    generate_base: &str,
    onboard_base: &str,
    poll_interval: Duration,
    provider_id: &str,
) -> Result<String> {
    let load_resp = load_code_assist(client, token, generate_base, provider_id).await?;
    if let Some(project) = extract_project_id(&load_resp) {
        return Ok(project);
    }
    let tier = default_tier(&load_resp);
    onboard_user(
        client,
        token,
        onboard_base,
        &tier,
        poll_interval,
        provider_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wrap_envelope_shapes_project_request_model() {
        let inner = json!({"contents": [{"role": "user"}]});
        let env = wrap_envelope(inner, "proj-1", "gemini-2.5-pro");
        assert_eq!(env["project"], "proj-1");
        assert_eq!(env["model"], "gemini-2.5-pro");
        assert!(env["request"]["contents"].is_array());
    }

    #[test]
    fn unwrap_response_peels_response_object() {
        let raw = json!({"response": {"candidates": []}});
        let inner = unwrap_response(raw);
        assert!(inner.get("candidates").is_some());
        assert!(inner.get("response").is_none());
    }

    #[test]
    fn unwrap_response_passes_through_unwrapped() {
        let raw = json!({"candidates": []});
        let inner = unwrap_response(raw.clone());
        assert_eq!(inner, raw);
    }

    #[test]
    fn unwrap_sse_data_peels_response_object() {
        let data = r#"{"response":{"candidates":[{"index":0}]}}"#;
        let out = unwrap_sse_data(data);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.get("candidates").is_some());
        assert!(parsed.get("response").is_none());
    }

    #[test]
    fn unwrap_sse_data_passes_through_non_envelope() {
        let data = r#"{"candidates":[]}"#;
        assert_eq!(unwrap_sse_data(data), data);
    }

    #[test]
    fn unwrap_sse_data_passes_through_non_json() {
        let data = "not json";
        assert_eq!(unwrap_sse_data(data), data);
    }

    #[test]
    fn extract_project_id_from_string() {
        let obj = json!({"cloudaicompanionProject": "proj-xyz"});
        assert_eq!(extract_project_id(&obj).as_deref(), Some("proj-xyz"));
    }

    #[test]
    fn extract_project_id_trims_and_skips_empty() {
        let obj = json!({"cloudaicompanionProject": "  ", "projectId": "  p2  "});
        assert_eq!(extract_project_id(&obj).as_deref(), Some("p2"));
    }

    #[test]
    fn extract_project_id_from_object_id() {
        let obj = json!({"project": {"id": "proj-obj"}});
        assert_eq!(extract_project_id(&obj).as_deref(), Some("proj-obj"));
    }

    #[test]
    fn extract_project_id_none_when_absent() {
        let obj = json!({"other": "value"});
        assert!(extract_project_id(&obj).is_none());
    }

    #[test]
    fn default_tier_picks_is_default() {
        let resp = json!({"allowedTiers": [
            {"id": "free-tier", "isDefault": false},
            {"id": "pro-tier", "isDefault": true},
        ]});
        assert_eq!(default_tier(&resp), "pro-tier");
    }

    #[test]
    fn default_tier_falls_back_to_current_tier() {
        let resp = json!({"currentTier": {"id": "cur-tier"}});
        assert_eq!(default_tier(&resp), "cur-tier");
    }

    #[test]
    fn default_tier_falls_back_to_free_tier() {
        let resp = json!({});
        assert_eq!(default_tier(&resp), "free-tier");
    }

    #[test]
    fn clean_error_body_handles_multibyte_and_strips_control_chars() {
        // A body far longer than the old fixed 500-byte cap whose byte 500
        // lands inside a multibyte UTF-8 sequence used to panic on a raw
        // byte slice ("byte index 500 is not a char boundary"). It also
        // embeds control / ANSI chars an upstream could use for log
        // injection. Build it, clean it, and assert: no panic, and the
        // control chars do not survive.
        let mut body = String::new();
        body.push('\u{1b}'); // ESC (ANSI control), never trimmed
        body.push('\r');
        body.push('\n');
        for _ in 0..300 {
            body.push('\u{1F680}'); // 4-byte rocket emoji -> ~1200 bytes
        }

        // Must not panic on the mid-codepoint boundary.
        let cleaned = clean_error_body(&body);

        assert!(!cleaned.contains('\r'), "CR must be stripped");
        assert!(!cleaned.contains('\n'), "LF must be stripped");
        assert!(!cleaned.contains('\u{1b}'), "ESC must be stripped");
    }

    #[test]
    fn parse_classifier_lifts_google_status_and_code() {
        // Arrange: the Google Cloud Code error envelope.
        let body = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#;

        // Act
        let (upstream_type, upstream_code) = parse_cloudcode_error_classifier(body);

        // Assert
        assert_eq!(upstream_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
        assert_eq!(upstream_code.as_deref(), Some("429"));
    }

    #[test]
    fn parse_classifier_returns_none_on_non_json_body() {
        let (upstream_type, upstream_code) =
            parse_cloudcode_error_classifier("503 Service Unavailable (plain text)");
        assert!(upstream_type.is_none());
        assert!(upstream_code.is_none());
    }

    #[test]
    fn onboarding_error_preserves_google_classifier() {
        // Arrange: an UNAUTHENTICATED body that a bare Error::upstream
        // would have collapsed into an indistinguishable generic 401.
        let body = r#"{"error":{"code":401,"status":"UNAUTHENTICATED","message":"bad token"}}"#;

        // Act
        let err = map_onboarding_error(
            "gemini-cc",
            401,
            &reqwest::header::HeaderMap::new(),
            body,
            false,
        );

        // Assert: the classifier survives onto the canonical error.
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                ..
            } => {
                assert_eq!(status, 401);
                assert_eq!(upstream_type.as_deref(), Some("UNAUTHENTICATED"));
                assert_eq!(upstream_code.as_deref(), Some("401"));
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn onboarding_error_over_cap_hides_prefix_and_lifts_classifier() {
        // On a cap trip the classifier still lifts from the (parseable)
        // prefix, but the client body collapses to the fixed cap message --
        // the truncated prefix is never echoed -- and the status is preserved.
        let prefix = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"secret upstream detail"}}"#;

        let err = map_onboarding_error(
            "gemini-cc",
            429,
            &reqwest::header::HeaderMap::new(),
            prefix,
            true,
        );

        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                assert_eq!(status, 429, "original status preserved on cap trip");
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("RESOURCE_EXHAUSTED"),
                    "classifier must lift from a parseable prefix"
                );
                assert_eq!(upstream_code.as_deref(), Some("429"));
                assert_eq!(
                    body,
                    crate::http_client::body_cap_exceeded_message(),
                    "client body must be the fixed cap message, never the prefix"
                );
                assert!(
                    !body.contains("secret upstream detail"),
                    "truncated prefix must not be echoed: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    fn upstream_with_type(status: u16, upstream_type: &str) -> Error {
        Error::upstream_full(
            "gemini:test",
            status,
            "{}",
            None,
            Some(upstream_type.to_string()),
            Some(status.to_string()),
        )
    }

    #[test]
    fn is_project_mismatch_true_for_permission_denied_and_not_found() {
        assert!(is_project_mismatch(&upstream_with_type(
            403,
            "PERMISSION_DENIED"
        )));
        assert!(is_project_mismatch(&upstream_with_type(404, "NOT_FOUND")));
    }

    #[test]
    fn is_project_mismatch_false_for_auth_quota_and_server_errors() {
        assert!(!is_project_mismatch(&upstream_with_type(
            401,
            "UNAUTHENTICATED"
        )));
        assert!(!is_project_mismatch(&upstream_with_type(
            429,
            "RESOURCE_EXHAUSTED"
        )));
        assert!(!is_project_mismatch(&upstream_with_type(500, "INTERNAL")));
        // Transport-level failure: status 0, no classifier token.
        assert!(!is_project_mismatch(&Error::upstream(
            "gemini:test",
            0,
            "connection reset"
        )));
    }

    #[test]
    fn is_project_mismatch_false_for_non_upstream_variant() {
        assert!(!is_project_mismatch(&Error::Auth("token expired".into())));
    }

    /// Spawn a one-shot raw TCP server that replies with `status` and a
    /// chunked body that dies mid-stream (one partial chunk, then the socket
    /// closes without the terminating `0\r\n\r\n`). wiremock always sends a
    /// complete, honest-length response, so a raw socket is the only way to
    /// drive a mid-read transport failure while the status line is intact.
    async fn spawn_mid_read_failure_server(status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status} X\r\n\
                 Content-Type: application/json\r\n\
                 Transfer-Encoding: chunked\r\n\
                 \r\n"
            );
            let _ = socket.write_all(head.as_bytes()).await;
            // One partial chunk, then drop -- no terminating chunk, so the
            // client sees an incomplete message mid-body.
            let _ = socket.write_all(b"10\r\n{\"error\":{\"code").await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn load_code_assist_mid_read_failure_preserves_status_and_warns() {
        let base = spawn_mid_read_failure_server(500).await;
        let client = Client::new();

        let (result, events) =
            routectl_testkit::with_capture(load_code_assist(&client, "tok", &base, "gemini-cc"))
                .await;

        match result.expect_err("a mid-read transport failure must be an error") {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 500, "the real upstream status is preserved, not 0");
                assert!(
                    body.is_empty(),
                    "an unreadable error body degrades to empty, got {body:?}"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
        assert!(
            events
                .iter()
                .any(|e| e.message == "failed to read upstream error body"),
            "the error-body read failure must emit the shared WARN"
        );
    }

    #[tokio::test]
    async fn onboard_user_mid_read_failure_preserves_status_and_warns() {
        let base = spawn_mid_read_failure_server(500).await;
        let client = Client::new();

        let (result, events) = routectl_testkit::with_capture(onboard_user(
            &client,
            "tok",
            &base,
            "free-tier",
            Duration::from_millis(0),
            "gemini-cc",
        ))
        .await;

        match result.expect_err("a mid-read transport failure must be an error") {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 500, "the real upstream status is preserved, not 0");
                assert!(
                    body.is_empty(),
                    "an unreadable error body degrades to empty, got {body:?}"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
        assert!(
            events
                .iter()
                .any(|e| e.message == "failed to read upstream error body"),
            "the error-body read failure must emit the shared WARN"
        );
    }
}
