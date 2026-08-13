//! Log-hygiene guard for the shadow-misfire WARN.
//!
//! The inbound session key is caller-controlled and the canonical schema
//! declares it must not be logged raw. The WARN must still identify the
//! affected (session, provider_kind, model) triple so an operator can
//! correlate misfires across lines, so the key rides as a stable hash.
use super::*;
use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::schema::{Message, MessageContent, Role};
use serde_json::json;

/// A caller-controlled value distinctive enough that any raw rendering of
/// it is unambiguous in a captured event.
const RAW_SESSION_KEY: &str = "sess-raw-leak-canary-9f3b";
const PROVIDER_KIND: &str = "anthropic-api";
const SERVED_MODEL: &str = "pt-opus-4-8";
const UPSTREAM: &str = "claude-opus-4-8";

fn effective_row_for(provider_kind: &str, model: &str) -> EffectiveRow {
    use crate::catalog::{lookup_baked_with_overrides, merge};
    let baked = lookup_baked_with_overrides(provider_kind, model, None, &BTreeMap::new());
    merge(baked.as_ref(), None)
}

fn payload_of_tokens(tokens: usize) -> String {
    "x".repeat(tokens * 4)
}

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn tool_use_msg(payload: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_1".into(),
            name: "search".into(),
            input: json!(payload),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn tool_result_msg(payload: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: json!(payload),
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A tool-heavy conversation above the default trim trigger, carrying the
/// caller-controlled key and a verified pricing cell, with `head` as its
/// first (cache-anchored) message so perturbing it shifts the trimmed
/// prefix fingerprint.
fn triggering_req(head: &str) -> ChatRequest {
    let payload = payload_of_tokens(12_000);
    let mut messages = vec![text_msg(Role::User, head), text_msg(Role::Assistant, "ack")];
    for _ in 0..6 {
        messages.push(tool_use_msg(&payload));
        messages.push(tool_result_msg(&payload));
    }
    for i in 0..6 {
        messages.push(text_msg(Role::User, &format!("recent turn {i}")));
    }
    let mut req = ChatRequest {
        model: UPSTREAM.into(),
        messages: messages.into(),
        ..Default::default()
    };
    req.routectl_internal.inbound_session_key = Some(RAW_SESSION_KEY.into());
    req
}

fn record(router: &Router, req: &ChatRequest) {
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
    router.record_would_trim(
        req,
        Some(PROVIDER_KIND),
        UPSTREAM,
        SERVED_MODEL,
        &effective,
        &mut meta,
    );
    assert_eq!(
        meta.would_trim_shadow_misfire,
        Some(1),
        "the shadow-misfire branch must be the one that ran",
    );
}

#[test]
fn shadow_misfire_warn_hashes_session_key_instead_of_logging_it_raw() {
    // Arrange: prime the shadow store for this triple (FirstSeen emits no
    // WARN), then perturb the cache-anchored head so the trimmed-prefix
    // fingerprint shifts and the next record is a Misfire.
    let router = Router::new(Arc::new(Config::default()));
    record_first_seen(&router);

    // Act
    let events =
        routectl_testkit::capture_events(|| record(&router, &triggering_req("shifted head XXXX")));

    // Assert: the WARN fired, still identifies the triple, and carries the
    // session key only as a stable hash.
    let warn = events
        .iter()
        .find(|e| e.message.starts_with("would_trim_shadow_misfire"))
        .unwrap_or_else(|| panic!("expected shadow-misfire WARN, got events: {events:?}"));
    assert_eq!(warn.level, tracing::Level::WARN);

    for event in &events {
        assert!(
            !event.message.contains(RAW_SESSION_KEY),
            "raw session key must never be logged: {event:?}",
        );
        for (_, v) in &event.fields {
            assert!(
                !v.contains(RAW_SESSION_KEY),
                "raw session key must never appear in a structured field: {event:?}",
            );
        }
    }

    assert_eq!(warn.field("provider_kind"), Some(PROVIDER_KIND));
    assert_eq!(warn.field("model"), Some(UPSTREAM));
    assert_eq!(
        warn.field("session_key_hash"),
        Some(
            crate::context_trim::fnv1a_hash(RAW_SESSION_KEY.as_bytes())
                .to_string()
                .as_str()
        ),
        "the triple must stay correlatable via a stable hash of the key",
    );
}

/// Prime the shadow store so the NEXT differing fingerprint for the same
/// triple lands on the Misfire branch rather than FirstSeen.
fn record_first_seen(router: &Router) {
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
    router.record_would_trim(
        &triggering_req("original head"),
        Some(PROVIDER_KIND),
        UPSTREAM,
        SERVED_MODEL,
        &effective,
        &mut meta,
    );
    assert_eq!(
        meta.would_trim_shadow_misfire, None,
        "the first record for a triple is FirstSeen, not a verdict",
    );
}
