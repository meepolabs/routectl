//! The modern OpenAI chat shape on the wire, per dialect: which dialects
//! drop sampling params and rename `max_tokens`, for which ids, and what each
//! declares to the wire tap.

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role};
use serde_json::{Value, json};

use super::dialects::util::SAMPLING_DROP;
use super::{HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect};

const SAMPLING_KEYS: [&str; 6] = [
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
];

fn provider(dialect: ReasoningDialect) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "chat-shape".into(),
        base_url: "http://127.0.0.1:9".into(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: dialect,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn sampled_request(model: &str) -> ChatRequest {
    let mut req = ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        temperature: Some(0.5),
        top_p: Some(0.9),
        presence_penalty: Some(0.1),
        frequency_penalty: Some(0.2),
        logprobs: Some(true),
        top_logprobs: Some(3),
        max_tokens: Some(64),
        ..Default::default()
    };
    req.routectl_internal.wire_tap = Some(WireTap::default());
    req
}

/// The body a dialect sends for `model`, and the request-leg edits it
/// declared, as (site, path, kind).
fn shaped(dialect: ReasoningDialect, model: &str) -> (Value, Vec<(EditSite, String, EditKind)>) {
    let req = sampled_request(model);
    let body = provider(dialect)
        .normalize_request(&req)
        .expect("normalize");
    let tap = req.routectl_internal.wire_tap.expect("tapped");
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect();
    (body, edits)
}

fn assert_sampling_dropped(name: &str, body: &Value, edits: &[(EditSite, String, EditKind)]) {
    for key in SAMPLING_KEYS {
        assert!(body.get(key).is_none(), "{name}: {key} must leave: {body}");
        let path = format!("/{key}");
        assert!(
            edits.contains(&(EditSite::SamplingParamsStrip, path, EditKind::Remove)),
            "{name}: {key} drop declared: {edits:?}"
        );
    }
}

fn declares(edits: &[(EditSite, String, EditKind)], site: EditSite) -> bool {
    edits.iter().any(|(s, _, _)| *s == site)
}

fn assert_unchanged(name: &str, body: &Value, edits: &[(EditSite, String, EditKind)]) {
    for key in SAMPLING_KEYS {
        assert!(body.get(key).is_some(), "{name}: {key} must stay: {body}");
    }
    assert_eq!(body["max_tokens"], json!(64), "{name}: {body}");
    assert!(
        body.get("max_completion_tokens").is_none(),
        "{name}: {body}"
    );
    assert!(
        !declares(edits, EditSite::SamplingParamsStrip),
        "{name}: {edits:?}"
    );
    assert!(
        !declares(edits, EditSite::MaxTokensRename),
        "{name}: {edits:?}"
    );
}

#[test]
fn the_openai_dialect_sends_gpt_4o_in_the_modern_shape() {
    let (body, edits) = shaped(ReasoningDialect::OpenAi, "gpt-4o");

    assert_sampling_dropped("gpt-4o", &body, &edits);
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert_eq!(body["max_completion_tokens"], json!(64), "{body}");
    for (path, kind) in [
        ("/max_tokens", EditKind::Remove),
        ("/max_completion_tokens", EditKind::Add),
    ] {
        assert!(
            edits.contains(&(EditSite::MaxTokensRename, path.into(), kind)),
            "rename declared at {path}: {edits:?}"
        );
    }
}

#[test]
fn the_deepseek_dialect_drops_sampling_for_a_reasoner_without_renaming() {
    let (body, edits) = shaped(ReasoningDialect::DeepSeek, "deepseek-reasoner");

    assert_sampling_dropped("deepseek-reasoner", &body, &edits);
    assert_eq!(body["max_tokens"], json!(64), "{body}");
    assert!(body.get("max_completion_tokens").is_none(), "{body}");
    assert!(!declares(&edits, EditSite::MaxTokensRename), "{edits:?}");
}

#[test]
fn ids_and_dialects_outside_the_modern_shape_send_the_body_unchanged() {
    let rows = [
        (
            "openai dialect, non-OpenAI id",
            ReasoningDialect::OpenAi,
            "llama-3.3-70b",
        ),
        ("vllm dialect, gpt-5", ReasoningDialect::Vllm, "gpt-5"),
    ];
    for (name, dialect, model) in rows {
        let (body, edits) = shaped(dialect, model);
        assert_unchanged(name, &body, &edits);
    }
}

#[test]
fn the_pinned_sampling_keys_are_exactly_the_dropped_set() {
    let mut pinned = SAMPLING_KEYS.to_vec();
    let mut dropped = SAMPLING_DROP.to_vec();
    pinned.sort_unstable();
    dropped.sort_unstable();

    assert_eq!(pinned, dropped);
}
