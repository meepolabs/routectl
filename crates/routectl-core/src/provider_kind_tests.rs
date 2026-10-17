use super::{config, trace_kind};
use crate::wire_check::EgressKind;

/// The emitted `provider_kind=` values are keyed on by fixture captures and
/// log evidence, so every lane's token is pinned as a literal.
#[test]
fn every_egress_traces_under_its_frozen_token() {
    let rows = [
        (EgressKind::AnthropicApi, Some("anthropic")),
        (EgressKind::AnthropicApiMantle, Some("anthropic")),
        (EgressKind::BedrockInvoke, Some("bedrock-invoke")),
        (EgressKind::BedrockConverse, Some("bedrock-converse")),
        (EgressKind::OpenaiCompat, Some("openai-compat")),
        (EgressKind::OpenaiCompatMantle, Some("openai-compat")),
        (EgressKind::OpenaiResponses, Some("openai-responses")),
        (EgressKind::OpenaiResponsesMantle, Some("openai-responses")),
        (EgressKind::Gemini, Some("gemini")),
        (EgressKind::Local, None),
    ];
    assert_eq!(rows.len(), EgressKind::ALL.len());
    for (egress, token) in rows {
        assert_eq!(trace_kind(egress), token, "{}", egress.label());
    }
}

/// The config tokens are what the usage ledger persists per row.
#[test]
fn config_kinds_keep_their_persisted_spelling() {
    let rows = [
        ("anthropic-api", config::ANTHROPIC_API),
        ("openai-compat", config::OPENAI_COMPAT),
        ("bedrock", config::BEDROCK),
        ("openai-responses", config::OPENAI_RESPONSES),
        ("gemini", config::GEMINI),
    ];
    for (expected, actual) in rows {
        assert_eq!(actual, expected);
    }
}
