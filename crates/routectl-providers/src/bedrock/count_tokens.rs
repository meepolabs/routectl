//! Bedrock CountTokens lane -- request-body assembly and response parse.
//!
//! No status on this lane is mapped to a capability signal: every error
//! status propagates as the client error the shared error path built. See
//! the status posture recorded at the call site in `bedrock/mod.rs`.
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

    /// Structural guard on routectl's own status posture, not on AWS
    /// behavior: no Bedrock seam may convert an upstream status into a
    /// capability signal. A 404 here means the model resource was not
    /// found (measured: an end-of-life model id, from a region that
    /// served other models in the same session), so lifting it would
    /// walk a fallback chain past a capable region. Any reintroduction
    /// has to construct `Error::NotImplemented`, which is what this
    /// scans for -- a rename of the helper cannot evade it.
    #[test]
    fn no_bedrock_seam_lifts_an_upstream_status_to_a_capability_signal() {
        // The needles are assembled from fragments so this test's own
        // source lines are not counted as matches.
        let opener = concat!("mod ", "tests {");
        let lift = concat!("Error::", "NotImplemented");
        for (name, src) in [
            ("count_tokens.rs", include_str!("count_tokens.rs")),
            ("mod.rs", include_str!("mod.rs")),
        ] {
            let occurrences = src.matches(opener).count();
            assert_eq!(
                occurrences, 1,
                "{name}: the production cut is ambiguous with {occurrences} \
                 test-module openers; decide explicitly what this guard covers"
            );
            let production = &src[..src.find(opener).expect("an inline test module")];
            assert!(
                !production.contains(lift),
                "{name}: the count-tokens lane must propagate the upstream \
                 status, never lift it to a capability error"
            );
        }
    }
}
