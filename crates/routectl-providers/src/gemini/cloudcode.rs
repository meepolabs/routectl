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
use serde_json::{json, Value};

use routectl_core::Error;
use routectl_core::Result;

// Wire constants. Kept private to this module: they are part of the
// Cloud Code dialect, not configurable provider knobs.

pub(crate) const PROD_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const DAILY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

pub(crate) const GENERATE_PATH: &str = "/v1internal:generateContent";
pub(crate) const STREAM_PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
const LOAD_CODE_ASSIST_PATH: &str = "/v1internal:loadCodeAssist";
const ONBOARD_USER_PATH: &str = "/v1internal:onboardUser";

/// Short User-Agent sent on generate / stream / loadCodeAssist. Pinned to
/// the reference client's fallback version for this slice; routectl does
/// not run a live version-fetcher.
pub(crate) const SHORT_USER_AGENT: &str =
    "antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64)";
/// Node User-Agent sent only on onboardUser (the reference uses the
/// google-api-nodejs-client UA there).
const NODE_USER_AGENT: &str =
    "antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64) google-api-nodejs-client/10.3.0";
const GOOG_API_CLIENT: &str = "gl-node/22.21.1";

const IDE_VERSION: &str = "1.0.13";
const DEFAULT_TIER: &str = "free-tier";

const ONBOARD_MAX_ATTEMPTS: u32 = 5;
/// Cap on the upstream error-body excerpt carried into a routectl Error.
const ERROR_BODY_CAP: usize = 500;

/// Selects which Gemini wire dialect a provider speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
pub(crate) fn wrap_envelope(inner: Value, project_id: &str, model: &str) -> Value {
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
pub(crate) fn unwrap_response(raw: Value) -> Value {
    match raw.get("response") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => raw,
    }
}

/// Unwrap one SSE `data:` payload. Each streamed chunk is itself a
/// `{"response": {GenerateContentResponse}}` envelope; return the inner
/// object serialized so the shared SSE parser sees the bare partial
/// response. Anything that is not a wrapped object is returned unchanged.
pub(crate) fn unwrap_sse_data(data: &str) -> String {
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
pub(crate) fn extract_project_id(obj: &Value) -> Option<String> {
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
pub(crate) fn default_tier(load_resp: &Value) -> String {
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

/// Trim and cap an upstream error body for inclusion in a routectl Error.
/// Never carries the bearer token (the token is only ever set in the
/// Authorization header, never echoed by these endpoints in a body).
fn clean_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > ERROR_BODY_CAP {
        trimmed[..ERROR_BODY_CAP].to_string()
    } else {
        trimmed.to_string()
    }
}

/// POST `loadCodeAssist` and return the parsed JSON response. The caller
/// reads the project id (or computes the default tier) from it.
pub(crate) async fn load_code_assist(
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
    let body_text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(Error::upstream(
            provider_id,
            status,
            clean_error_body(&body_text),
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
pub(crate) async fn onboard_user(
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
        let body_text = resp.text().await.unwrap_or_default();
        if status != 200 {
            return Err(Error::upstream(
                provider_id,
                status,
                clean_error_body(&body_text),
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
pub(crate) async fn resolve_project_id(
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
}
