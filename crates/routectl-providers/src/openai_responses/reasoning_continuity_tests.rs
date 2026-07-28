//! Reasoning continuity across a dialect that cannot carry an artifact's
//! id and scheme, and the empty-item floor that keeps an unreplayable
//! artifact off the wire.
//!
//! The loop these tests close: an artifact produced by a Responses lane is
//! rendered toward an Anthropic-dialect client, which has no slot for its
//! id or its scheme. The egress wraps it so it stays self-describing; the
//! client echoes it back as a `redacted_thinking` block; this crate's
//! Responses egress restores the pair and replays the artifact onto the
//! lane that issued it.
//!
//! The envelope is minted here with the same codec the render side uses,
//! because that codec IS the contract between the two halves -- a test
//! that hand-rolled the bytes would pass while the real pairing broke.

use routectl_core::{
    BEDROCK_MANTLE, CODEX_OAUTH, ContentPart, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Role, reasoning_envelope,
};
use serde_json::json;

use super::AuthKind;
use super::messages::{build_input, retain_replayable_reasoning, translate_thinking_part};
use super::types::ResponseInputItem;

/// A Fernet-shaped blob, the family the id-validating lanes mint.
const CODEX_BLOB: &str = "gAAAAABmock-codex-artifact-bytes";

/// A content-prefixed blob, the family the content-validating lane mints.
const MANTLE_BLOB: &str = "rsn_mock-mantle-artifact-bytes";

/// An Anthropic-native redacted blob: no envelope, no format tag.
const NATIVE_REDACTED_BLOB: &str = "ErUBCkYIBRgCKkBmock-anthropic-redacted-bytes";

const UPSTREAM_ITEM_ID: &str = "rs_mock_item";

fn user_turn(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_parts(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn redacted(data: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::RedactedThinking { data: data.into() })
}

fn thinking(text: &str, signature: Option<&str>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Thinking {
        thinking: text.into(),
        signature: signature.map(str::to_string),
    })
}

/// The `input[]` a client turn carrying `parts` produces on `lane`.
fn input_for(lane: AuthKind, parts: Vec<ContentPart>) -> Vec<ResponseInputItem> {
    build_input("test", lane, &[user_turn("hi"), assistant_parts(parts)])
        .expect("translation must not fail on a well-formed turn")
}

fn reasoning_items(items: &[ResponseInputItem]) -> Vec<(&Option<String>, &str)> {
    items
        .iter()
        .filter_map(|i| match i {
            ResponseInputItem::Reasoning {
                id,
                encrypted_content,
                ..
            } => Some((id, encrypted_content.as_str())),
            _ => None,
        })
        .collect()
}

/// The exact bytes that leave for the upstream.
fn wire(items: &[ResponseInputItem]) -> String {
    serde_json::to_string(items).expect("input items must serialize")
}

// ---------------------------------------------------------------------
// Empty-item floor
// ---------------------------------------------------------------------

/// The Thinking call site must not push an item for a block whose
/// signature this lane cannot replay: an item with empty
/// `encrypted_content` re-injects nothing and its id dangles.
#[test]
fn thinking_call_site_pushes_no_item_when_the_signature_is_unreplayable() {
    // Arrange: a content-part Thinking block carries no format tag, so it
    // can never be established as replayable on a Responses lane.
    let parts = vec![thinking("some reasoning", Some("anthropic-signature"))];

    // Act
    let out = input_for(AuthKind::ChatgptOauth, parts);

    // Assert
    assert!(
        reasoning_items(&out).is_empty(),
        "an unreplayable Thinking part must produce no reasoning item"
    );
    assert!(
        !wire(&out).contains("anthropic-signature"),
        "an unreplayable signature must not reach the wire"
    );
}

/// The RedactedThinking call site must not push an item for an opaque
/// blob that restores nothing.
#[test]
fn redacted_call_site_pushes_no_item_for_an_opaque_blob() {
    // Arrange
    let parts = vec![redacted(NATIVE_REDACTED_BLOB)];

    // Act
    let out = input_for(AuthKind::ChatgptOauth, parts);

    // Assert
    assert!(
        reasoning_items(&out).is_empty(),
        "an opaque redacted blob must produce no reasoning item"
    );
    assert!(
        !wire(&out).contains(NATIVE_REDACTED_BLOB),
        "an opaque foreign blob must not reach the wire"
    );
}

/// The floor is enforced at the emission point too, not only by each
/// producer: an item that somehow arrived with empty `encrypted_content`
/// is dropped before it can be emitted.
#[test]
fn sweep_drops_an_injected_empty_encrypted_content_item() {
    // Arrange: one emptied item between two replayable ones.
    let mut items = vec![
        ResponseInputItem::Reasoning {
            id: Some("rs_keep_first".into()),
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: CODEX_BLOB.into(),
        },
        ResponseInputItem::Reasoning {
            id: Some("rs_dangling".into()),
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: String::new(),
        },
        ResponseInputItem::Reasoning {
            id: Some("rs_keep_second".into()),
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: MANTLE_BLOB.into(),
        },
    ];

    // Act
    retain_replayable_reasoning(&mut items);

    // Assert: the emptied item is gone, order among survivors is intact.
    let ids: Vec<Option<String>> = reasoning_items(&items)
        .into_iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            Some("rs_keep_first".to_string()),
            Some("rs_keep_second".to_string())
        ],
        "the empty-encrypted_content item must be dropped and the rest preserved in order"
    );
}

/// A non-Reasoning item is never touched by the sweep.
#[test]
fn sweep_leaves_non_reasoning_items_alone() {
    // Arrange
    let mut items = vec![
        ResponseInputItem::Message {
            role: "assistant".into(),
            content: Vec::new(),
        },
        ResponseInputItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: String::new(),
        },
    ];

    // Act
    retain_replayable_reasoning(&mut items);

    // Assert
    assert_eq!(items.len(), 1, "only the emptied Reasoning item is dropped");
    assert!(matches!(items[0], ResponseInputItem::Message { .. }));
}

/// The producer itself can no longer build an emptied item: the return
/// type carries the floor.
#[test]
fn translate_thinking_part_returns_none_rather_than_an_emptied_item() {
    // Arrange / Act: a recognized tag on a compatible lane, but with an
    // empty signature -- there is nothing to replay.
    let empty_signature = translate_thinking_part(
        "reasoned",
        Some(""),
        Some(CODEX_OAUTH),
        AuthKind::ChatgptOauth,
    );
    let absent_signature =
        translate_thinking_part("reasoned", None, Some(CODEX_OAUTH), AuthKind::ChatgptOauth);

    // Assert
    assert!(
        empty_signature.is_none(),
        "an empty signature yields no item"
    );
    assert!(
        absent_signature.is_none(),
        "an absent signature yields no item"
    );
}

// ---------------------------------------------------------------------
// Envelope decode
// ---------------------------------------------------------------------

/// A wrapped artifact restores both the id and the scheme that the
/// intervening dialect could not carry, and replays onto its own lane.
#[test]
fn wrapped_envelope_restores_the_artifact_id_and_scheme() {
    // Arrange: the shape the render side emits for a codex-lane artifact.
    let envelope = reasoning_envelope::wrap(CODEX_OAUTH, Some(UPSTREAM_ITEM_ID), CODEX_BLOB);

    // Act
    let out = input_for(AuthKind::ChatgptOauth, vec![redacted(&envelope)]);

    // Assert
    assert_eq!(
        reasoning_items(&out),
        vec![(&Some(UPSTREAM_ITEM_ID.to_string()), CODEX_BLOB)],
        "the restored item must carry the upstream id and the original blob"
    );
}

/// An artifact that carried no id still restores its scheme, so a
/// content-validating lane can replay it.
#[test]
fn wrapped_envelope_without_an_id_still_restores_the_scheme() {
    // Arrange
    let envelope = reasoning_envelope::wrap(BEDROCK_MANTLE, None, MANTLE_BLOB);

    // Act
    let out = input_for(AuthKind::BedrockMantle, vec![redacted(&envelope)]);

    // Assert
    assert_eq!(
        reasoning_items(&out),
        vec![(&None, MANTLE_BLOB)],
        "an id-less envelope must still replay on its own lane"
    );
}

/// Every input that is not a well-formed envelope of a recognized version
/// degrades to the pre-envelope behavior: no item, no error, no panic.
#[test]
fn a_non_envelope_blob_degrades_to_the_prior_behavior() {
    let hostile_shapes = [
        // A dialect-native redacted blob.
        NATIVE_REDACTED_BLOB,
        // Unknown version prefix.
        &format!("rctl9.{CODEX_OAUTH}.{UPSTREAM_ITEM_ID}.{CODEX_BLOB}"),
        // Truncated: no blob field.
        &format!("rctl1.{CODEX_OAUTH}.{UPSTREAM_ITEM_ID}"),
        // Empty blob field.
        &format!("rctl1.{CODEX_OAUTH}.{UPSTREAM_ITEM_ID}."),
        // Non-token scheme field.
        &format!("rctl1.not a scheme.{UPSTREAM_ITEM_ID}.{CODEX_BLOB}"),
        // Empty input.
        "",
        // Separators only.
        "...",
    ];

    for shape in hostile_shapes {
        // Act
        let out = input_for(AuthKind::ChatgptOauth, vec![redacted(shape)]);

        // Assert
        assert!(
            reasoning_items(&out).is_empty(),
            "a non-envelope blob must produce no reasoning item, got one for: {shape}"
        );
    }
}

// ---------------------------------------------------------------------
// SECURITY: the envelope is a hint, never an authorization
// ---------------------------------------------------------------------

/// A hostile envelope claims a scheme and an id of the attacker's
/// choosing. The claim is fed into the SAME replay gate a natively tagged
/// artifact passes through, so a blob steered at a lane whose validator
/// family rejects the claimed scheme is stripped -- the ladder decides,
/// never the claim.
#[test]
fn a_hostile_envelope_is_stripped_by_the_replay_ladder() {
    // Arrange: a codex-family blob wearing a mantle-scheme claim, aimed at
    // an id-validating lane that the probes prove rejects mantle-scheme
    // artifacts.
    let poison = reasoning_envelope::wrap(BEDROCK_MANTLE, Some(UPSTREAM_ITEM_ID), CODEX_BLOB);

    // Act
    let out = input_for(AuthKind::ChatgptOauth, vec![redacted(&poison)]);

    // Assert: nothing was admitted, and neither the poison blob nor its
    // envelope reached the wire.
    let bytes = wire(&out);
    assert!(
        reasoning_items(&out).is_empty(),
        "a proven-incompatible claim must admit no reasoning item"
    );
    assert!(
        !bytes.contains(CODEX_BLOB),
        "the poison blob must never reach the wire"
    );
    assert!(
        !bytes.contains(BEDROCK_MANTLE),
        "the claimed scheme must never reach the wire"
    );
    assert!(
        !bytes.contains(UPSTREAM_ITEM_ID),
        "the claimed id must never reach the wire"
    );
}

/// The mirror direction: a mantle-family blob claiming a codex scheme,
/// aimed at the content-validating lane, is stripped just the same. A
/// claim cannot unlock a lane in either direction.
#[test]
fn a_hostile_envelope_is_stripped_in_the_mirror_direction() {
    // Arrange
    let poison = reasoning_envelope::wrap(CODEX_OAUTH, Some(UPSTREAM_ITEM_ID), MANTLE_BLOB);

    // Act
    let out = input_for(AuthKind::BedrockMantle, vec![redacted(&poison)]);

    // Assert
    assert!(
        reasoning_items(&out).is_empty(),
        "a proven-incompatible claim must admit no reasoning item"
    );
    assert!(
        !wire(&out).contains(MANTLE_BLOB),
        "the poison blob must never reach the wire"
    );
}

// ---------------------------------------------------------------------
// Continuity round trip
// ---------------------------------------------------------------------

/// The bytes the render side emits toward an Anthropic-dialect client for
/// a Responses-family artifact. Mirrors the egress carve-out: only a
/// Responses-family artifact wraps.
fn rendered_toward_anthropic_client(detail: &ReasoningDetail) -> String {
    let blob = detail
        .payload
        .get("encrypted_content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match detail.format.as_deref() {
        Some(tag) if routectl_core::is_responses_family(Some(tag)) => {
            reasoning_envelope::wrap(tag, detail.id.as_deref(), blob)
        }
        _ => blob.to_string(),
    }
}

fn encrypted_detail(format: &str, id: &str, blob: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(id.into()),
        format: Some(format.into()),
        index: None,
        payload: json!({ "encrypted_content": blob }),
    }
}

/// SINGLE INSTANCE: an artifact issued on a Responses lane, flattened
/// toward an Anthropic-dialect client and echoed back by it, replays onto
/// the lane that issued it with its id and its bytes intact.
#[test]
fn continuity_survives_a_single_instance_round_trip() {
    // Arrange: the artifact as the provider issued it, then as the
    // Anthropic-dialect client received and echoed it back.
    let issued = encrypted_detail(CODEX_OAUTH, UPSTREAM_ITEM_ID, CODEX_BLOB);
    let echoed_by_client = rendered_toward_anthropic_client(&issued);

    // Act: the next request on the same lane.
    let out = input_for(AuthKind::ChatgptOauth, vec![redacted(&echoed_by_client)]);

    // Assert: continuity restored -- without the envelope this turn would
    // carry no reasoning item at all.
    assert_eq!(
        reasoning_items(&out),
        vec![(&Some(UPSTREAM_ITEM_ID.to_string()), CODEX_BLOB)],
        "the echoed artifact must replay with its upstream id and bytes"
    );
}

/// ACROSS AN A->B HOP: the artifact is issued through one instance and
/// replayed through a different one. Nothing is shared between them but
/// the bytes, which is what makes the ambiguity observable here -- a
/// server-side memory of the artifact would not survive the hop.
#[test]
fn continuity_survives_an_a_to_b_hop() {
    // Arrange: instance A renders the artifact toward the client.
    let issued = encrypted_detail(CODEX_OAUTH, UPSTREAM_ITEM_ID, CODEX_BLOB);
    let echoed_by_client = rendered_toward_anthropic_client(&issued);

    // Act: instance B, holding no state whatsoever from instance A,
    // translates the client's next turn.
    let out = input_for(AuthKind::ChatgptOauth, vec![redacted(&echoed_by_client)]);

    // Assert
    assert_eq!(
        reasoning_items(&out),
        vec![(&Some(UPSTREAM_ITEM_ID.to_string()), CODEX_BLOB)],
        "continuity must not depend on any state held by the issuing instance"
    );
}

/// Prompt-cache affinity: the replayed bytes must be byte-identical to
/// what the provider originally issued. A single re-encoded byte breaks
/// the cache prefix.
#[test]
fn replayed_bytes_are_byte_identical_to_the_issued_artifact() {
    // Arrange: a blob deliberately containing the envelope separator and
    // non-token bytes, to prove the remainder rides through untouched.
    let awkward_blob = "gAAAAAB.mock..bytes/with+padding==";
    let envelope = reasoning_envelope::wrap(CODEX_OAUTH, Some(UPSTREAM_ITEM_ID), awkward_blob);

    // Act
    let out = input_for(AuthKind::ChatgptOauth, vec![redacted(&envelope)]);

    // Assert
    let restored = reasoning_items(&out);
    assert_eq!(restored.len(), 1, "expected the artifact to replay");
    assert_eq!(
        restored[0].1, awkward_blob,
        "the replayed blob must be byte-identical to the issued bytes"
    );
}

// ---------------------------------------------------------------------
// No regression
// ---------------------------------------------------------------------

/// A dialect-native artifact is never wrapped on the way out, so it is
/// never unwrapped on the way back: it round-trips byte-verbatim and
/// keeps working the way it does today. Its signature is what makes
/// same-model replay work on its own lane, and re-encoding it would
/// corrupt a mechanism that needs no repair.
#[test]
fn a_dialect_native_artifact_round_trips_byte_verbatim() {
    // Arrange: an artifact whose format is outside the Responses family.
    let native = ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(UPSTREAM_ITEM_ID.into()),
        format: Some("anthropic-claude-v1".into()),
        index: None,
        payload: json!({ "encrypted_content": NATIVE_REDACTED_BLOB }),
    };

    // Act
    let rendered = rendered_toward_anthropic_client(&native);

    // Assert: emitted verbatim, never wrapped, never emptied -- and it
    // stays opaque to an envelope reader, so nothing mis-parses it.
    assert_eq!(
        rendered, NATIVE_REDACTED_BLOB,
        "a dialect-native artifact must never be wrapped"
    );
    assert!(
        reasoning_envelope::unwrap(&rendered).is_none(),
        "a dialect-native artifact must not parse as an envelope"
    );
}

/// A same-family artifact arriving through the tagged channel is
/// unaffected by any of this: it still replays byte-verbatim.
#[test]
fn a_natively_tagged_same_family_artifact_still_replays() {
    // Arrange
    let messages = vec![
        user_turn("hi"),
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Null,
            reasoning: None,
            reasoning_details: vec![encrypted_detail(CODEX_OAUTH, UPSTREAM_ITEM_ID, CODEX_BLOB)],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    // Act
    let out = build_input("test", AuthKind::ChatgptOauth, &messages).expect("translation");

    // Assert
    assert_eq!(
        reasoning_items(&out),
        vec![(&Some(UPSTREAM_ITEM_ID.to_string()), CODEX_BLOB)],
        "a natively tagged same-family artifact must still replay verbatim"
    );
}
