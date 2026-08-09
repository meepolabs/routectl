//! Bedrock CountTokens lane -- request-body assembly, response parse, and
//! the capability mapping for a missing operation.
//!
//! AWS reference: `POST /model/{modelId}/count-tokens` with a body of
//! `{"input": {...}}`, where `input` is a union of EXACTLY ONE of
//! `converse` or `invokeModel`. The response is `{"inputTokens":
//! <integer>}`.
//!
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_CountTokens.html>
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_CountTokensInput.html>
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseTokensRequest.html>
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModelTokensRequest.html>
//!
//! The two lanes are deliberately asymmetric. `invokeModel` carries a
//! single required `body` field holding the InvokeModel request "formatted
//! according to the model's expected input format" -- so the body ships
//! VERBATIM (base64-encoded) and must keep `anthropic_version` /
//! `max_tokens`, which InvokeModel requires. `converse` is a distinct
//! four-field type that accepts a strict subset of the Converse request,
//! so that lane copies an allowlist and drops everything else.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64_STANDARD;

use serde_json::{Map, Value, json};

use routectl_core::{Error, Result, TokenCount};

/// The only keys AWS's Converse token-count request type accepts. Every
/// other field the Converse request adapter emits (`inferenceConfig`,
/// `additionalModelResponseFieldPaths`) is not part of that type and is
/// dropped here rather than risking a strict-schema rejection.
///
/// Copied by allowlist rather than stripped by blocklist so a future
/// field added to the Converse request adapter cannot leak into this
/// body without an explicit decision.
const CONVERSE_TOKENS_KEYS: [&str; 4] = [
    "messages",
    "system",
    "toolConfig",
    "additionalModelRequestFields",
];

/// Reason string carried on the capability error a missing CountTokens
/// operation produces.
const COUNT_TOKENS_UNAVAILABLE: &str =
    "count_tokens: CountTokens is unavailable for this model or region";

/// Wrap an assembled InvokeModel body as a CountTokens `invokeModel`
/// input. The body is carried verbatim, base64-encoded -- no field
/// filtering: the token-count lane must see the same bytes InvokeModel
/// would, including the fields InvokeModel requires.
pub(super) fn invoke_tokens_body(provider_id: &str, invoke_body: &Value) -> Result<Value> {
    let bytes = serde_json::to_vec(invoke_body)
        .map_err(|e| Error::normalize_request(provider_id, e.to_string()))?;
    Ok(json!({ "input": { "invokeModel": { "body": B64_STANDARD.encode(bytes) } } }))
}

/// Wrap an assembled Converse body as a CountTokens `converse` input,
/// keeping only [`CONVERSE_TOKENS_KEYS`]. Absent optional keys stay
/// absent rather than serializing as null.
pub(super) fn converse_tokens_body(provider_id: &str, converse_body: &Value) -> Result<Value> {
    let obj = converse_body.as_object().ok_or_else(|| {
        Error::normalize_request(provider_id, "converse body is not a JSON object")
    })?;
    let mut kept = Map::new();
    for key in CONVERSE_TOKENS_KEYS {
        if let Some(value) = obj.get(key) {
            kept.insert(key.to_string(), value.clone());
        }
    }
    Ok(json!({ "input": { "converse": Value::Object(kept) } }))
}

/// Parse a CountTokens success body into the canonical count. AWS returns
/// the single `inputTokens` integer, so `extras` is always empty on this
/// lane.
pub(super) fn parse_token_count(provider_id: &str, raw: &Value) -> Result<TokenCount> {
    let reported = raw
        .get("inputTokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            Error::normalize_response(
                provider_id,
                "count-tokens response missing integer `inputTokens`",
            )
        })?;
    let input_tokens = u32::try_from(reported).map_err(|_| {
        Error::normalize_response(provider_id, "count-tokens `inputTokens` out of range")
    })?;
    Ok(TokenCount {
        input_tokens,
        extras: Map::new(),
    })
}

/// Lift a 404 on the count-tokens path to a capability signal, leaving
/// every other status as the client error the shared error path built.
///
/// A model or region that does not offer the operation answers 404, whose
/// documented meaning ("the specified resource was not found") is the only
/// listed CountTokens error that fits a missing operation. As a capability
/// error it lets the caller step past this seat instead of failing the
/// request.
///
/// Deliberately NOT extended to 400: a malformed body assembled here also
/// answers 400, and treating that as capability would hide the defect
/// behind a silent walk-past. This is routectl's mapping choice; AWS ties
/// neither status to availability.
pub(super) fn map_capability_status(provider_id: &str, status: u16, err: Error) -> Error {
    if status == 404 {
        Error::NotImplemented(provider_id.to_string(), COUNT_TOKENS_UNAVAILABLE.into())
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converse_body() -> Value {
        json!({
            "messages": [{"role": "user", "content": [{"text": "hi"}]}],
            "system": [{"text": "be brief"}],
            "inferenceConfig": {"maxTokens": 64},
            "toolConfig": {"tools": []},
            "additionalModelRequestFields": {"top_k": 5},
            "additionalModelResponseFieldPaths": ["/stop_sequence"],
        })
    }

    fn invoke_body() -> Value {
        json!({
            "anthropic_version": "bedrock-2023-05-31",
            "max_tokens": 128,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    #[test]
    fn converse_tokens_body_keeps_only_the_accepted_keys() {
        let wrapped = converse_tokens_body("prov", &converse_body()).expect("build");

        let input = wrapped["input"]["converse"]
            .as_object()
            .expect("converse input object");
        let mut keys: Vec<&str> = input.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "additionalModelRequestFields",
                "messages",
                "system",
                "toolConfig"
            ],
        );
        assert!(
            !input.contains_key("inferenceConfig"),
            "inferenceConfig is not part of the token-count type"
        );
        assert!(
            !input.contains_key("additionalModelResponseFieldPaths"),
            "additionalModelResponseFieldPaths is not part of the token-count type"
        );
        assert!(
            wrapped["input"].get("invokeModel").is_none(),
            "the input union carries exactly one member"
        );
    }

    #[test]
    fn converse_tokens_body_omits_absent_optional_keys() {
        let minimal = json!({"messages": [{"role": "user", "content": [{"text": "hi"}]}]});

        let wrapped = converse_tokens_body("prov", &minimal).expect("build");

        let input = wrapped["input"]["converse"]
            .as_object()
            .expect("converse input object");
        assert_eq!(
            input.len(),
            1,
            "absent optionals must not serialize as null"
        );
        assert!(input.contains_key("messages"));
    }

    #[test]
    fn converse_tokens_body_rejects_non_object_body() {
        let err = converse_tokens_body("prov", &json!([1, 2, 3]))
            .expect_err("a non-object body must not silently build");
        assert!(matches!(err, Error::NormalizeRequest(..)), "got {err:?}");
    }

    #[test]
    fn invoke_tokens_body_carries_the_verbatim_body_base64_encoded() {
        let body = invoke_body();

        let wrapped = invoke_tokens_body("prov", &body).expect("build");

        let encoded = wrapped["input"]["invokeModel"]["body"]
            .as_str()
            .expect("base64 body string");
        let decoded = B64_STANDARD.decode(encoded).expect("valid base64");
        let round_tripped: Value = serde_json::from_slice(&decoded).expect("valid json");
        assert_eq!(
            round_tripped, body,
            "the invoke body must ship byte-for-byte"
        );
        assert_eq!(
            round_tripped["anthropic_version"],
            json!("bedrock-2023-05-31"),
            "anthropic_version is required by the invoke body shape"
        );
        assert_eq!(
            round_tripped["max_tokens"],
            json!(128),
            "max_tokens is required by the invoke body shape"
        );
        assert!(
            wrapped["input"].get("converse").is_none(),
            "the input union carries exactly one member"
        );
    }

    #[test]
    fn token_count_parses_input_tokens_with_empty_extras() {
        let parsed = parse_token_count("prov", &json!({"inputTokens": 4711})).expect("parse");

        assert_eq!(parsed.input_tokens, 4711);
        assert!(parsed.extras.is_empty(), "this lane reports no extras");
    }

    #[test]
    fn token_count_rejects_a_body_without_input_tokens() {
        let err = parse_token_count("prov", &json!({"tokens": 4711}))
            .expect_err("a body without the documented field must error");
        assert!(matches!(err, Error::NormalizeResponse(..)), "got {err:?}");
    }

    /// Asserts routectl's own status mapping, not AWS behavior: a 404
    /// becomes a capability signal so the caller can step past the seat.
    #[test]
    fn capability_mapping_lifts_404_to_not_implemented() {
        let client_err = Error::upstream("prov", 404, "ResourceNotFoundException");

        let mapped = map_capability_status("prov", 404, client_err);

        match mapped {
            Error::NotImplemented(provider, op) => {
                assert_eq!(provider, "prov");
                assert!(op.contains("count_tokens"), "got {op}");
            }
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    /// The other half of routectl's mapping: a 400 stays a loud client
    /// error, because a malformed body assembled here answers 400 and must
    /// not be mistaken for a missing operation.
    #[test]
    fn capability_mapping_leaves_400_as_a_client_error() {
        let client_err = Error::upstream("prov", 400, "ValidationException");

        let mapped = map_capability_status("prov", 400, client_err);

        match mapped {
            Error::Upstream { status, .. } => assert_eq!(status, 400),
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }
}
