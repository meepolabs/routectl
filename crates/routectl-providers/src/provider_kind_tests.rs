//! Every provider's `PROVIDER_KIND` is the token the shared egress table
//! names for the lane it serves. Expected values are literals: fixture
//! captures and log evidence key on them.

use routectl_core::provider_kind::trace_kind;
use routectl_core::wire_check::EgressKind;

fn assert_provider_token(egress: EgressKind, provider_kind: &str, expected: &str) {
    assert_eq!(provider_kind, expected, "{}", egress.label());
    assert_eq!(trace_kind(egress), Some(expected), "{}", egress.label());
}

#[cfg(feature = "anthropic-api")]
#[test]
fn anthropic_provider_traces_under_the_table_token() {
    assert_provider_token(
        EgressKind::AnthropicApi,
        crate::anthropic_api::PROVIDER_KIND,
        "anthropic",
    );
}

#[cfg(feature = "openai-compat")]
#[test]
fn openai_compat_provider_traces_under_the_table_token() {
    assert_provider_token(
        EgressKind::OpenaiCompat,
        crate::openai_compat::PROVIDER_KIND,
        "openai-compat",
    );
}

#[cfg(feature = "openai-responses")]
#[test]
fn openai_responses_provider_traces_under_the_table_token() {
    assert_provider_token(
        EgressKind::OpenaiResponses,
        crate::openai_responses::PROVIDER_KIND,
        "openai-responses",
    );
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_provider_traces_under_the_table_token() {
    assert_provider_token(EgressKind::Gemini, crate::gemini::PROVIDER_KIND, "gemini");
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_shapes_trace_under_the_table_tokens() {
    use crate::bedrock::BedrockApiShape;
    let rows = [
        (
            EgressKind::BedrockInvoke,
            BedrockApiShape::Invoke,
            "bedrock-invoke",
        ),
        (
            EgressKind::BedrockConverse,
            BedrockApiShape::Converse,
            "bedrock-converse",
        ),
    ];
    for (egress, shape, expected) in rows {
        assert_provider_token(egress, shape.provider_kind_str(), expected);
    }
}
