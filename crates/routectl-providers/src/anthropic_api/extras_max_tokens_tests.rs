//! Which egress kinds fill a `max_tokens` the client omitted.

use routectl_core::wire_check::EgressKind;

use super::egress_fills_max_tokens;

/// Each egress kind's fill answer, spelled per kind rather than derived from
/// the predicate's own set, so dropping one arm turns exactly its row red.
const fn expected_fill(egress: EgressKind) -> bool {
    match egress {
        EgressKind::AnthropicApi => true,
        EgressKind::AnthropicApiMantle => true,
        EgressKind::BedrockInvoke => true,
        EgressKind::BedrockConverse => false,
        EgressKind::OpenaiCompat => false,
        EgressKind::OpenaiCompatMantle => false,
        EgressKind::OpenaiResponses => false,
        EgressKind::OpenaiResponsesMantle => false,
        EgressKind::Gemini => false,
        EgressKind::Local => false,
    }
}

#[test]
fn only_the_anthropic_messages_egresses_fill_an_omitted_max_tokens() {
    for egress in EgressKind::ALL {
        assert_eq!(
            egress_fills_max_tokens(egress),
            expected_fill(egress),
            "{}",
            egress.label(),
        );
    }
}
