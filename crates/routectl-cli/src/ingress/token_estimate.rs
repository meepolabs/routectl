//! Local, zero-dependency input-token estimate for a canonical
//! `ChatRequest`.
//!
//! routectl synthesizes an early `message_start` frame before the
//! upstream has reported real usage (see the Anthropic ingress stream
//! renderer). Emitting `input_tokens: 0` there leaves the client's
//! context meter stuck at zero for the whole turn on the pre-inversion
//! fast path; a rough local estimate keeps the meter live until the
//! terminal `message_delta` overwrites it with the authoritative count.
//!
//! This is a deliberately coarse char/token heuristic behind a single
//! swappable seam: no bundled tokenizer, no provider call. The Anthropic
//! tokenizer is private, and Claude Code itself estimates locally the
//! same way, so a character heuristic is the consistent choice.

use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, SystemContent,
};

/// Average characters per token for natural-language text. English-ish
/// prose runs about four characters per token, which is also the ratio
/// Claude Code uses for its own local estimate -- keeping routectl's
/// synthesized count consistent with the client's meter. This is the one
/// swappable knob of the heuristic; a future real-tokenizer seam replaces
/// the whole function, not this constant.
const CHARS_PER_TOKEN: u64 = 4;

/// Estimate the input-token count of `req` from the character length of
/// its textual content. Pure and total: no allocation beyond iteration,
/// no panics on any input (empty, Unicode, `null` content), and no
/// external dependency. An empty request estimates zero tokens; any
/// non-empty text rounds up to at least one token.
///
/// Only human-readable text contributes: system prompt text and message
/// text (flat strings and typed text parts). Non-text parts (images,
/// documents, tool-call payloads) are ignored -- the terminal
/// `message_delta` carries the authoritative count and overwrites this
/// estimate within seconds, so precision here is not the goal.
pub fn estimate_input_tokens(req: &ChatRequest) -> u64 {
    let chars = request_text_chars(req);
    (chars as u64).div_ceil(CHARS_PER_TOKEN)
}

fn request_text_chars(req: &ChatRequest) -> usize {
    let system_chars = req.system.as_ref().map_or(0, system_text_chars);
    let message_chars: usize = req.messages.iter().map(message_text_chars).sum();
    system_chars + message_chars
}

fn system_text_chars(system: &SystemContent) -> usize {
    match system {
        SystemContent::Text(t) => t.chars().count(),
        SystemContent::Blocks(blocks) => blocks.iter().map(|b| b.text.chars().count()).sum(),
    }
}

fn message_text_chars(msg: &Message) -> usize {
    match &msg.content {
        MessageContent::Text(t) => t.chars().count(),
        MessageContent::Parts(parts) => parts.iter().map(content_part_chars).sum(),
        MessageContent::Null => 0,
    }
}

fn content_part_chars(part: &ContentPart) -> usize {
    match part {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => text.chars().count(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::content_part::{ContentPart as CP, KnownContentPart as KCP};
    use routectl_core::system_content::{SystemBlock, SystemContent as SC};
    use routectl_core::{MessageContent as MC, Role};

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MC::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }
    }

    fn req_with(messages: Vec<Message>, system: Option<SystemContent>) -> ChatRequest {
        ChatRequest {
            model: "test-model".into(),
            messages,
            system,
            ..Default::default()
        }
    }

    #[test]
    fn empty_request_estimates_zero_tokens() {
        // Arrange
        let req = req_with(vec![], None);

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert_eq!(estimate, 0);
    }

    #[test]
    fn request_with_only_empty_text_estimates_zero_tokens() {
        // Arrange
        let req = req_with(vec![user_text("")], Some(SC::Text(String::new())));

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert_eq!(estimate, 0);
    }

    #[test]
    fn larger_message_body_yields_strictly_larger_estimate() {
        // Arrange
        let small = req_with(vec![user_text("hi")], None);
        let large = req_with(
            vec![
                user_text("hi"),
                user_text("this is a substantially longer follow-up message body"),
            ],
            None,
        );

        // Act
        let small_est = estimate_input_tokens(&small);
        let large_est = estimate_input_tokens(&large);

        // Assert
        assert!(
            large_est > small_est,
            "larger body must estimate strictly more tokens: {large_est} vs {small_est}"
        );
    }

    #[test]
    fn system_prompt_text_counts_toward_estimate() {
        // Arrange
        let without = req_with(vec![user_text("hi")], None);
        let with_system = req_with(
            vec![user_text("hi")],
            Some(SC::Text("you are a careful and precise assistant".into())),
        );

        // Act
        let without_est = estimate_input_tokens(&without);
        let with_est = estimate_input_tokens(&with_system);

        // Assert
        assert!(
            with_est > without_est,
            "adding a system prompt must raise the estimate: {with_est} vs {without_est}"
        );
    }

    #[test]
    fn system_blocks_text_counts_toward_estimate() {
        // Arrange
        let blocks = SC::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "first block of system guidance text".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "second block of system guidance text".into(),
                cache_control: None,
                citations: None,
            },
        ]);
        let req = req_with(vec![], Some(blocks));

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert!(estimate > 0, "system blocks text must contribute tokens");
    }

    #[test]
    fn typed_text_parts_count_toward_estimate() {
        // Arrange
        let msg = Message {
            role: Role::User,
            content: MC::Parts(vec![CP::Known(KCP::Text {
                text: "a text content part with several words in it".into(),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };
        let req = req_with(vec![msg], None);

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert!(estimate > 0, "typed text parts must contribute tokens");
    }

    #[test]
    fn unicode_and_empty_content_do_not_panic() {
        // Arrange: multibyte scalars, emoji, and empty strings mixed.
        let req = req_with(
            vec![
                user_text("caf\u{e9} na\u{ef}ve r\u{e9}sum\u{e9} \u{1f600}\u{1f680}"),
                user_text(""),
            ],
            Some(SC::Text("\u{6f22}\u{5b57} test".into())),
        );

        // Act: must complete without panicking.
        let estimate = estimate_input_tokens(&req);

        // Assert: non-empty multibyte content yields a positive estimate.
        assert!(estimate > 0);
    }

    #[test]
    fn null_content_messages_contribute_zero() {
        // Arrange: a tool-call turn carrying no textual content.
        let msg = Message {
            role: Role::Assistant,
            content: MC::Null,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };
        let req = req_with(vec![msg], None);

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert_eq!(estimate, 0);
    }
}
