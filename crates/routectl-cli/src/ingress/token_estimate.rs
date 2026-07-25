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

use serde::Serialize;

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
/// Every part of the request contributes: system prompt text, message
/// text (flat strings and typed text parts), tool-definition schemas,
/// per-message `tool_calls` argument payloads, and non-text content
/// parts (tool_use input, tool_result content, images, documents,
/// thinking blocks). Structured (non-plain-text) content is measured by
/// its serialized-JSON character length rather than parsed -- a
/// serialization failure contributes zero rather than panicking. The
/// terminal `message_delta` still carries the authoritative count and
/// overwrites this estimate within seconds, so precision here is not the
/// goal; completeness (not ignoring whole categories of context) is.
pub fn estimate_input_tokens(req: &ChatRequest) -> u64 {
    let chars = request_text_chars(req) + request_structured_chars(req);
    (chars as u64).div_ceil(CHARS_PER_TOKEN)
}

fn request_text_chars(req: &ChatRequest) -> usize {
    let system_chars = req.system.as_ref().map_or(0, system_text_chars);
    let message_chars: usize = req.messages.iter().map(message_text_chars).sum();
    system_chars + message_chars
}

/// Structured content the plain-text walk above does not see: tool
/// definitions on the request and `tool_calls` payloads on individual
/// messages. Additive on top of `request_text_chars` -- see the module
/// doc for why this stays a separate pass rather than folding into it.
fn request_structured_chars(req: &ChatRequest) -> usize {
    let tool_def_chars: usize = req
        .tools
        .as_ref()
        .map_or(0, |tools| tools.iter().map(json_chars).sum());
    let tool_call_chars: usize = req.messages.iter().map(message_tool_call_chars).sum();
    tool_def_chars + tool_call_chars
}

fn message_tool_call_chars(msg: &Message) -> usize {
    msg.tool_calls
        .as_ref()
        .map_or(0, |calls| calls.iter().map(json_chars).sum())
}

/// `io::Write` sink that counts Unicode scalar values instead of buffering
/// bytes. Every UTF-8 continuation byte has its top two bits set to `10`
/// (`(b & 0xC0) == 0x80`); the remaining bytes each begin exactly one
/// scalar. Counting the non-continuation bytes of serde's UTF-8 output
/// therefore equals `chars().count()` of that output by construction,
/// without ever allocating the string.
struct CharCount(usize);

impl std::io::Write for CharCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.iter().filter(|&&b| (b & 0xC0) != 0x80).count();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialized-JSON character length of `value`, or zero if serialization
/// fails. Streams serde's output through a counting sink so no intermediate
/// `String` is allocated. Keeps the estimate total, never panicking on any
/// input.
fn json_chars<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut sink = CharCount(0);
    serde_json::to_writer(&mut sink, value).map_or(0, |()| sink.0)
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
        ContentPart::Known(known) => known_content_part_chars(known),
        ContentPart::Other { extras, .. } => json_chars(extras),
    }
}

fn known_content_part_chars(part: &KnownContentPart) -> usize {
    match part {
        KnownContentPart::Text {
            text, citations, ..
        } => text.chars().count() + citations.as_ref().map_or(0, json_chars),
        KnownContentPart::Image { source, .. } => json_chars(source),
        KnownContentPart::ImageUrl { image_url, .. } => json_chars(image_url),
        KnownContentPart::File { file, .. } => json_chars(file),
        KnownContentPart::Document {
            source, citations, ..
        } => json_chars(source) + citations.as_ref().map_or(0, json_chars),
        KnownContentPart::ToolUse { input, .. } => json_chars(input),
        KnownContentPart::ToolResult { content, .. } => json_chars(content),
        KnownContentPart::Thinking { thinking, .. } => thinking.chars().count(),
        KnownContentPart::RedactedThinking { data } => data.chars().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::content_part::{ContentPart as CP, KnownContentPart as KCP};
    use routectl_core::system_content::{SystemBlock, SystemContent as SC};
    use routectl_core::{CustomTool, MessageContent as MC, Role, ToolDef};
    use serde_json::json;

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
            messages: messages.into(),
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
                citations: None,
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

    #[test]
    fn tool_definitions_count_toward_estimate() {
        // Arrange
        let without_tools = req_with(vec![user_text("hi")], None);
        let mut with_tools = req_with(vec![user_text("hi")], None);
        with_tools.tools = Some(vec![ToolDef::Custom(CustomTool {
            name: "get_weather".into(),
            description: Some("Look up the current weather for a location".into()),
            input_schema: json!({
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"],
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]);

        // Act
        let without_est = estimate_input_tokens(&without_tools);
        let with_est = estimate_input_tokens(&with_tools);

        // Assert
        assert!(
            with_est > without_est,
            "tool definitions must raise the estimate: {with_est} vs {without_est}"
        );
    }

    #[test]
    fn tool_call_argument_payloads_count_toward_estimate() {
        // Arrange
        let without_calls = Message {
            role: Role::Assistant,
            content: MC::Null,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };
        let with_calls = Message {
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "arguments": "{\"location\": \"San Francisco, CA\"}",
                },
            })]),
            ..without_calls.clone()
        };
        let without_req = req_with(vec![without_calls], None);
        let with_req = req_with(vec![with_calls], None);

        // Act
        let without_est = estimate_input_tokens(&without_req);
        let with_est = estimate_input_tokens(&with_req);

        // Assert
        assert!(
            with_est > without_est,
            "tool_calls arguments must raise the estimate: {with_est} vs {without_est}"
        );
    }

    #[test]
    fn non_text_content_part_contributes_positive_chars() {
        // Arrange: previously the `_ => 0` catch-all swallowed this entirely.
        let msg = Message {
            role: Role::Assistant,
            content: MC::Parts(vec![CP::Known(KCP::ToolUse {
                id: "toolu_01".into(),
                name: "calculator".into(),
                input: json!({"operation": "add", "a": 1, "b": 2}),
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
        assert!(
            estimate > 0,
            "a tool_use content part must contribute tokens"
        );
    }

    #[test]
    fn tool_result_and_image_content_parts_contribute_positive_chars() {
        // Arrange
        let tool_result_msg = Message {
            role: Role::User,
            content: MC::Parts(vec![CP::Known(KCP::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: json!({"result": 3, "unit": "count"}),
                is_error: None,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };
        let image_msg = Message {
            role: Role::User,
            content: MC::Parts(vec![CP::Known(KCP::Image {
                source: json!({"type": "base64", "media_type": "image/png", "data": "AAAA"}),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };

        // Act
        let tool_result_est = estimate_input_tokens(&req_with(vec![tool_result_msg], None));
        let image_est = estimate_input_tokens(&req_with(vec![image_msg], None));

        // Assert
        assert!(tool_result_est > 0, "tool_result must contribute tokens");
        assert!(image_est > 0, "image content must contribute tokens");
    }

    #[test]
    fn empty_request_still_estimates_zero_with_no_tools_or_tool_calls() {
        // Arrange: re-confirm the additive extension preserves the
        // empty-request-estimates-zero contract.
        let req = req_with(vec![], None);

        // Act
        let estimate = estimate_input_tokens(&req);

        // Assert
        assert_eq!(estimate, 0);
    }

    #[test]
    fn null_content_message_with_no_tool_calls_still_contributes_zero() {
        // Arrange
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

    /// The pre-streaming-sink formula the counting sink must reproduce
    /// exactly: serialize to a `String`, count its Unicode scalars.
    fn old_formula_chars<T: Serialize + ?Sized>(value: &T) -> usize {
        serde_json::to_string(value).unwrap().chars().count()
    }

    #[test]
    fn json_chars_parity_tool_definition() {
        // Arrange
        let tool = ToolDef::Custom(CustomTool {
            name: "get_weather".into(),
            description: Some("Look up the current weather for a location".into()),
            input_schema: json!({
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"],
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });

        // Act + Assert
        assert_eq!(json_chars(&tool), old_formula_chars(&tool));
    }

    #[test]
    fn json_chars_parity_tool_call_payload() {
        // Arrange: the shape stored in each `tool_calls` element.
        let payload = json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "get_weather",
                "arguments": "{\"location\": \"San Francisco, CA\"}",
            },
        });

        // Act + Assert
        assert_eq!(json_chars(&payload), old_formula_chars(&payload));
    }

    #[test]
    fn content_part_parity_image() {
        // Arrange
        let source = json!({"type": "base64", "media_type": "image/png", "data": "AAAA"});
        let part = CP::Known(KCP::Image {
            source: source.clone(),
            cache_control: None,
        });

        // Act + Assert: the arm counts the serialized `source`.
        assert_eq!(content_part_chars(&part), old_formula_chars(&source));
    }

    #[test]
    fn content_part_parity_document_with_citations() {
        // Arrange
        let source = json!({"type": "text", "media_type": "text/plain", "data": "report body"});
        let citations = json!([{"type": "page", "start": 1, "end": 3}]);
        let part = CP::Known(KCP::Document {
            source: source.clone(),
            title: Some("Q3 report".into()),
            citations: Some(citations.clone()),
            cache_control: None,
        });

        // Act + Assert: the arm sums serialized `source` and `citations`.
        assert_eq!(
            content_part_chars(&part),
            old_formula_chars(&source) + old_formula_chars(&citations)
        );
    }

    #[test]
    fn content_part_parity_tool_use() {
        // Arrange
        let input = json!({"operation": "add", "a": 1, "b": 2});
        let part = CP::Known(KCP::ToolUse {
            id: "toolu_01".into(),
            name: "calculator".into(),
            input: input.clone(),
            cache_control: None,
        });

        // Act + Assert
        assert_eq!(content_part_chars(&part), old_formula_chars(&input));
    }

    #[test]
    fn content_part_parity_tool_result() {
        // Arrange
        let content = json!({"result": 3, "unit": "count"});
        let part = CP::Known(KCP::ToolResult {
            tool_use_id: "toolu_01".into(),
            content: content.clone(),
            is_error: None,
            cache_control: None,
        });

        // Act + Assert
        assert_eq!(content_part_chars(&part), old_formula_chars(&content));
    }

    #[test]
    fn content_part_parity_other_extras() {
        // Arrange: forward-compat catchall carrying unmodeled fields.
        let extras = json!({"custom_field": "future value", "count": 7});
        let extras_map = extras.as_object().unwrap().clone();
        let part = CP::Other {
            type_tag: "future_block_v2".into(),
            cache_control: None,
            extras: extras_map.clone(),
        };

        // Act + Assert: the arm counts the serialized `extras` map.
        assert_eq!(content_part_chars(&part), old_formula_chars(&extras_map));
    }

    #[test]
    fn json_chars_parity_multibyte_counts_scalars_not_bytes() {
        // Arrange: structured content with genuinely multibyte scalars so
        // byte-count and char-count diverge.
        let value = json!({
            "note": "caf\u{e9} na\u{ef}ve \u{6f22}\u{5b57} \u{1f600}\u{1f680}",
        });
        let serialized = serde_json::to_string(&value).unwrap();
        let char_count = serialized.chars().count();
        let byte_count = serialized.len();

        // Act
        let counted = json_chars(&value);

        // Assert: the sink counts Unicode scalars, matching the old formula,
        // and the fixture really is multibyte (bytes exceed chars).
        assert!(
            byte_count > char_count,
            "fixture must be multibyte for a meaningful parity check: {byte_count} bytes vs {char_count} chars"
        );
        assert_eq!(counted, char_count);
    }
}
