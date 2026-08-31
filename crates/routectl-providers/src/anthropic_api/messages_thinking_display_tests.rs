// Replay-eligibility pins for the `thinking.display: "omitted"` shape: a
// Thinking block/detail whose TEXT is empty but whose SIGNATURE is a real
// Claude signature must stay replayable. Eligibility keys on the
// signature, never on the text -- otherwise every omitted-display turn
// would be stripped and the next turn would lose its reasoning context.
// Imports live in the host `messages_tests.rs` -- do not add `use` lines
// here.

/// A real-shaped Claude signature so the `is_claude_shaped_signature`
/// gate passes for the right reason: `E`-prefixed, VALID base64 (padding
/// included -- the decoder is strict), decoding to a leading 0x12 byte.
const OMITTED_DISPLAY_SIGNATURE: &str = "EqoCCkYIBxgCKkASDGZvbw==";

fn signed_thinking_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Thinking {
        thinking: text.into(),
        signature: Some(OMITTED_DISPLAY_SIGNATURE.into()),
    })
}

/// The whole point of the pin: empty thinking text with a valid signature
/// survives the replay strip. The populated-text case is the positive
/// control on the same path, so a strip that dropped everything could not
/// make this pass.
#[test]
fn empty_thinking_text_with_a_claude_signature_survives_the_replay_strip() {
    for (text, label) in [("", "omitted-display shape"), ("visible", "control")] {
        // Arrange
        let req = request_of(vec![assistant_msg(vec![
            signed_thinking_part(text),
            text_part("answer"),
        ])]);

        // Act
        let out = normalize_replay_invariants(
            "prov-test",
            &req,
            CoreHistoryReasoning::Strip,
            SystemTurnPolicy::Lift,
        )
        .expect("normalize succeeds");

        // Assert
        let MessageContent::Parts(parts) = &out[0].content else {
            panic!("expected Parts content ({label})");
        };
        assert_eq!(parts.len(), 2, "no block was stripped ({label}): {parts:?}");
        assert!(
            matches!(
                &parts[0],
                ContentPart::Known(KnownContentPart::Thinking { thinking, signature })
                    if thinking == text
                        && signature.as_deref() == Some(OMITTED_DISPLAY_SIGNATURE)
            ),
            "the signed thinking block must pass through verbatim ({label}): {parts:?}"
        );
    }
}

/// A reasoning DETAIL with empty text and a signature is emittable: the
/// eligibility predicate reads the signature, not the text. Paired with
/// the unsigned case, which is correctly NOT emittable.
#[test]
fn empty_text_reasoning_detail_with_a_signature_is_anthropic_emittable() {
    let signed = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("d0".into()),
        format: Some(ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: serde_json::json!({"text": "", "signature": OMITTED_DISPLAY_SIGNATURE}),
    };
    let unsigned = ReasoningDetail {
        payload: serde_json::json!({"text": "", "signature": ""}),
        ..signed.clone()
    };

    assert!(
        is_anthropic_emittable_detail(&signed),
        "empty text plus a signature is emittable -- the omitted-display shape"
    );
    assert!(
        !is_anthropic_emittable_detail(&unsigned),
        "negative control: no signature is still not emittable"
    );
}

/// An unrecognized kind is never emittable: no Anthropic block shape is
/// defined for it. Paired with the recognized `Text` case above (which
/// the enclosing test proves emittable when signed), so a regression
/// that made every kind emittable could not pass this test alone.
#[test]
fn unrecognized_kind_reasoning_detail_is_not_anthropic_emittable() {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Other("future.kind".to_string()),
        id: Some("d0".into()),
        format: Some(ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: serde_json::json!({"text": "x", "signature": OMITTED_DISPLAY_SIGNATURE}),
    };
    assert!(
        !is_anthropic_emittable_detail(&detail),
        "an unrecognized kind must never be emittable, even with a valid signature"
    );
}
