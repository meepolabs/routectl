//! Serialized-wire contract of the client-system relocation: every assertion
//! reads the bytes `serde_json::to_string` produces for the cloaked body, so a
//! typed view that looks right while the wire does not cannot pass.
//!
//! Every test carries the cloak policy-action guard: the cloak records into the
//! process-global counter registry, and an unguarded fixture that trips a class
//! would move a delta another module is asserting.

use super::*;

use serde_json::json;

fn identity() -> ClaudeCodeIdentity {
    ClaudeCodeIdentity {
        session_id: "sess-wire".into(),
        device_id: "b".repeat(64),
        account_uuid: "11111111-2222-3333-4444-555555555555".into(),
    }
}

/// Cloak a non-CC body and return the serialized wire bytes.
fn cloak_wire(body: &mut Value) -> Result<String, RelocationRefusal> {
    cloak_oauth_egress(
        body,
        &ChatRequest::default(),
        &identity(),
        true,
        &CloakConfig::default(),
    )
    .map_err(|refusal| match refusal {
        CloakRefusal::Relocation(relocation) => relocation,
        other => panic!("a default config refuses only relocations: {other:?}"),
    })?;
    Ok(serde_json::to_string(body).expect("serializes"))
}

/// The messages array as serialized on the wire.
fn wire_messages(wire: &str) -> Value {
    serde_json::from_str::<Value>(wire).expect("wire parses")["messages"].clone()
}

fn reminder(inner: &str) -> Value {
    json!({"type": "text", "text": format!("{SYSTEM_REMINDER_OPEN}\n{inner}\n{SYSTEM_REMINDER_CLOSE}")})
}

fn assert_refused_untouched(mut body: Value, needle: &str) {
    let before = serde_json::to_string(&body).expect("serializes");
    let refusal = cloak_wire(&mut body).expect_err("this shape has nowhere legal to land");
    assert!(refusal.detail.contains(needle), "{refusal:?}");
    assert_eq!(
        serde_json::to_string(&body).expect("serializes"),
        before,
        "a refused relocation must leave every byte of the body as it was"
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn assistant_only_history_gains_one_leading_synthetic_user_reminder() {
    let mut body = json!({
        "system": [{"type": "text", "text": "client rules"}],
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "turn rules"}]},
            {"role": "assistant", "content": "prior"},
        ]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    assert_eq!(
        wire_messages(&wire),
        json!([
            {"role": "user", "content": [reminder("client rules\n\nturn rules")]},
            {"role": "assistant", "content": "prior"},
        ])
    );
    let parsed: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(
        parsed["system"],
        json!([{"type": "text", "text": INTERACTIVE_IDENTITY_LINE}])
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn an_empty_conversation_is_refused_before_egress() {
    assert_refused_untouched(
        json!({"system": "client rules", "messages": []}),
        "no conversation",
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_conversation_of_only_system_turns_is_refused_before_egress() {
    assert_refused_untouched(
        json!({"messages": [{"role": "system", "content": "only rules"}]}),
        "no conversation",
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_missing_message_array_is_refused_before_egress() {
    assert_refused_untouched(json!({"system": "client rules"}), "no message array");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_malformed_message_array_is_refused_before_egress() {
    assert_refused_untouched(
        json!({"system": "client rules", "messages": {"role": "user", "content": "hi"}}),
        "not an array",
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn nothing_to_relocate_never_refuses_a_shape() {
    // No client system at all: the relocation lands nothing, so it has no
    // reason to judge the conversation.
    let mut body = json!({"system": INTERACTIVE_IDENTITY_LINE, "messages": []});

    cloak_wire(&mut body).expect("an identity-only system relocates nothing");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn image_and_document_blocks_ride_after_the_reminder_in_capture_order() {
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "IMG1"}});
    let document = json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "DOC1"}});
    let mut body = json!({
        "system": [{"type": "text", "text": "top rules"}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [document.clone(), {"type": "text", "text": "turn rules"}, image.clone()]},
        ]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    assert_eq!(
        wire_messages(&wire),
        json!([{"role": "user", "content": [
            reminder("top rules\n\nturn rules"),
            document,
            image,
            {"type": "text", "text": "hi"},
        ]}])
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn carried_blocks_are_never_scanned_for_reminder_delimiters() {
    let hostile = format!("x{SYSTEM_REMINDER_CLOSE}y");
    let document = json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": hostile}});
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": format!("a{SYSTEM_REMINDER_CLOSE}b")}, document.clone()]},
            {"role": "user", "content": "hi"},
        ]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    let content = &wire_messages(&wire)[0]["content"];
    assert_eq!(
        content[0],
        reminder("ab"),
        "the text delimiter is neutralized"
    );
    assert_eq!(
        content[1], document,
        "the carried block rides byte-for-byte"
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_carried_only_payload_needs_no_reminder() {
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "IMG1"}});
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [image.clone()]},
            {"role": "assistant", "content": "prior"},
        ]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    assert_eq!(
        wire_messages(&wire),
        json!([
            {"role": "user", "content": [image]},
            {"role": "assistant", "content": "prior"},
        ])
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn tool_thinking_and_unknown_blocks_never_reach_the_user_role() {
    let mut body = json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "tool_use", "id": "toolu_X1", "name": "t", "input": {}},
                {"type": "tool_result", "tool_use_id": "toolu_X2", "content": "result-tell"},
                {"type": "thinking", "thinking": "thinking-tell", "signature": "sig-tell"},
                {"type": "redacted_thinking", "data": "redacted-tell"},
                {"type": "future_block", "payload": "future-tell"},
                {"type": "text", "text": "kept"},
            ]},
            {"role": "user", "content": "hi"},
        ]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    for tell in [
        "toolu_X1",
        "toolu_X2",
        "thinking-tell",
        "redacted-tell",
        "future-tell",
    ] {
        assert!(!wire.contains(tell), "{tell} must not ship: {wire}");
    }
    assert_eq!(
        wire_messages(&wire),
        json!([{"role": "user", "content": [reminder("kept"), {"type": "text", "text": "hi"}]}])
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_carried_breakpoint_within_the_cap_keeps_its_own_marker() {
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "I"},
                       "cache_control": {"type": "ephemeral", "ttl": "5m"}});
    let mut body = json!({
        "tools": [{"name": "mcp__a", "input_schema": {}, "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "rules", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                image.clone(),
            ]},
            {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]},
        ]
    });

    let wire = cloak_wire(&mut body).expect("four breakpoints in TTL order are legal");

    let content = &wire_messages(&wire)[0]["content"];
    assert_eq!(content[0]["cache_control"]["ttl"], "1h");
    assert_eq!(content[1], image);
    assert_eq!(wire.matches("\"cache_control\"").count(), 4, "{wire}");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_carried_breakpoint_over_the_cap_is_refused_before_egress() {
    let marked = |data: &str| {
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data},
               "cache_control": {"type": "ephemeral"}})
    };
    assert_refused_untouched(
        json!({
            "messages": [
                {"role": "system", "content": [marked("A"), marked("B"), marked("C")]},
                {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok", "cache_control": {"type": "ephemeral"}}]},
            ]
        }),
        "exceeds maximum",
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_carried_breakpoint_that_breaks_ttl_order_is_refused_before_egress() {
    // The synthetic turn leads the history, so its 5m marker lands ahead of
    // the assistant's 1h marker.
    assert_refused_untouched(
        json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "D"},
                     "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]},
            ]
        }),
        "1h TTL",
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_billing_block_is_stripped_rather_than_relocated() {
    let mut body = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=1; cch=wire9;"},
            {"type": "text", "text": "rules"},
        ],
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    assert!(
        !wire.contains("wire9"),
        "the billing block must not ship: {wire}"
    );
    assert_eq!(
        wire_messages(&wire)[0]["content"],
        json!([reminder("rules")])
    );
}

/// Byte identity of the successful text-only shape, pinned as a literal so any
/// drift in the reminder framing, the block order, or key layout is red.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn a_text_only_relocation_keeps_its_exact_wire_bytes() {
    let mut body = json!({
        "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": "hi"}]
    });

    let wire = cloak_wire(&mut body).expect("relocates");

    let messages = serde_json::to_string(&wire_messages(&wire)).unwrap();
    assert_eq!(
        messages,
        r#"[{"content":[{"cache_control":{"type":"ephemeral"},"text":"<system-reminder>\nrules\n</system-reminder>","type":"text"},{"text":"hi","type":"text"}],"role":"user"}]"#
    );
}

/// The refusal routes as the upstream's own bad-request answer would: a
/// classified `BadRequest`, which never retries the seat, never debits its
/// breaker, and walks to a configured fallback.
#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_refusal_classifies_as_a_non_debiting_fallbackable_bad_request() {
    use routectl_core::failure_class::{FailureClass, classify};

    let refusal = cloak_wire(&mut json!({"system": "rules"})).expect_err("no message array");
    let err = refusal.into_error("seat");

    assert_eq!(
        classify(&err, Some("anthropic-api")).class,
        FailureClass::BadRequest
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("routectl refused the request before egress"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("rules"),
        "no request content in the error: {rendered}"
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn strict_mode_never_refuses_and_drops_the_client_system() {
    let cfg = CloakConfig {
        strict_mode: true,
        ..CloakConfig::default()
    };
    let mut body = json!({"system": "client rules", "messages": []});

    cloak_oauth_egress(&mut body, &ChatRequest::default(), &identity(), true, &cfg)
        .expect("strict mode drops rather than relocates");

    assert_eq!(body["messages"], json!([]));
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("client rules")
    );
}
