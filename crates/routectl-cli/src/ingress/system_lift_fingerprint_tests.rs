//! A billing/attribution fingerprint carried in one lifted system message is
//! withheld on its own, wherever it sits: the Chat and Responses ingresses
//! lift system-role messages into `req.system`, and every egress must still
//! see each message as its own entry when one of them is the fingerprint.
//! Fingerprint-free requests keep the exact bytes the joined lift produces.

use axum::http::HeaderMap;
use routectl_core::{ChatRequest, Provider};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;

const FINGERPRINT: &str = "x-anthropic-billing-header: v=1; fp=abc";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

#[derive(Clone, Copy, Debug)]
enum Ingress {
    Chat,
    Responses,
}

#[derive(Clone, Copy, Debug)]
enum Egress {
    OpenAiCompat,
    OpenAiResponses,
}

const LANES: [(Ingress, Egress); 4] = [
    (Ingress::Chat, Egress::OpenAiCompat),
    (Ingress::Chat, Egress::OpenAiResponses),
    (Ingress::Responses, Egress::OpenAiCompat),
    (Ingress::Responses, Egress::OpenAiResponses),
];

fn system_item(ingress: Ingress, text: &str) -> Value {
    match ingress {
        Ingress::Chat => json!({"role": "system", "content": text}),
        Ingress::Responses => json!({"type": "message", "role": "system", "content": text}),
    }
}

fn user_item(ingress: Ingress) -> Value {
    match ingress {
        Ingress::Chat => json!({"role": "user", "content": "hi"}),
        Ingress::Responses => json!({"type": "message", "role": "user", "content": "hi"}),
    }
}

fn body(ingress: Ingress, systems: &[&str]) -> Value {
    let mut items: Vec<Value> = systems.iter().map(|t| system_item(ingress, t)).collect();
    items.push(user_item(ingress));
    match ingress {
        Ingress::Chat => json!({"model": "m", "messages": items}),
        Ingress::Responses => json!({"model": "m", "input": items}),
    }
}

fn parse(ingress: Ingress, body: Value) -> ChatRequest {
    let headers = HeaderMap::new();
    match ingress {
        Ingress::Chat => OpenAiIngress.parse_request_value(&headers, body),
        Ingress::Responses => ResponsesIngress.parse_request_value(&headers, body),
    }
    .expect("ingress parse")
}

fn egress_body(egress: Egress, req: &ChatRequest) -> Value {
    match egress {
        Egress::OpenAiCompat => OpenAiCompatProvider::new(OpenAiCompatConfig {
            id: "openai-compat:test".into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: "test-key".into(),
            header_extras: Vec::new(),
            payload_extras: None,
            reasoning_dialect: ReasoningDialect::OpenAi,
            history_reasoning: HistoryReasoning::Auto,
            user_agent: None,
            strict_translation: false,
            mantle: None,
        })
        .normalize_request(req),
        Egress::OpenAiResponses => OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
            "openai-responses:test",
            "literal:test",
        ))
        .normalize_request(req),
    }
    .expect("egress normalize")
}

/// The system text the egress put on the wire: the leading system message on
/// openai-compat, `instructions` on openai-responses.
fn wire_system(egress: Egress, wire: &Value) -> Option<String> {
    let value = match egress {
        Egress::OpenAiCompat => {
            let first = &wire["messages"][0];
            if first["role"] != "system" {
                return None;
            }
            first["content"].clone()
        }
        Egress::OpenAiResponses => wire["instructions"].clone(),
    };
    value.as_str().map(str::to_string)
}

fn round_trip(ingress: Ingress, egress: Egress, body: Value) -> Value {
    egress_body(egress, &parse(ingress, body))
}

#[test]
fn a_leading_fingerprint_message_keeps_the_legitimate_system_text_after_it() {
    for (ingress, egress) in LANES {
        // Arrange
        let body = body(ingress, &[FINGERPRINT, "be terse"]);

        // Act
        let wire = round_trip(ingress, egress, body);

        // Assert
        assert_eq!(
            wire_system(egress, &wire).as_deref(),
            Some("be terse"),
            "{ingress:?} -> {egress:?}: {wire}"
        );
    }
}

#[test]
fn a_fingerprint_message_after_legitimate_system_text_never_reaches_the_wire() {
    for (ingress, egress) in LANES {
        // Arrange
        let body = body(ingress, &["be terse", FINGERPRINT]);

        // Act
        let wire = round_trip(ingress, egress, body);

        // Assert
        assert!(
            !wire.to_string().contains(BILLING_PREFIX),
            "{ingress:?} -> {egress:?}: fingerprint on the wire: {wire}"
        );
        assert_eq!(
            wire_system(egress, &wire).as_deref(),
            Some("be terse"),
            "{ingress:?} -> {egress:?}: {wire}"
        );
    }
}

#[test]
fn fingerprint_free_system_messages_keep_the_joined_wire_text() {
    for (ingress, egress) in LANES {
        // Arrange
        let body = body(ingress, &["be terse", "be polite"]);

        // Act
        let wire = round_trip(ingress, egress, body);

        // Assert
        assert_eq!(
            wire_system(egress, &wire).as_deref(),
            Some("be terse\nbe polite"),
            "{ingress:?} -> {egress:?}: {wire}"
        );
    }
}

#[test]
fn a_fingerprint_in_responses_instructions_keeps_the_lifted_system_text() {
    for egress in [Egress::OpenAiCompat, Egress::OpenAiResponses] {
        // Arrange
        let body = json!({
            "model": "m",
            "instructions": FINGERPRINT,
            "input": [system_item(Ingress::Responses, "be terse"), user_item(Ingress::Responses)],
        });

        // Act
        let wire = round_trip(Ingress::Responses, egress, body);

        // Assert
        assert!(
            !wire.to_string().contains(BILLING_PREFIX),
            "{egress:?}: fingerprint on the wire: {wire}"
        );
        assert_eq!(
            wire_system(egress, &wire).as_deref(),
            Some("be terse"),
            "{egress:?}: {wire}"
        );
    }
}
