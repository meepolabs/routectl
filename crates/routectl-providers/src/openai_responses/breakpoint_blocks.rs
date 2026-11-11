//! The `input_text` blocks the Responses egress emits from canonical
//! messages, in emission order. A block's position in that list is the
//! ordinal a per-block `prompt_cache_breakpoint` is recorded against at the
//! ingress and matched against at the egress, so both sides read it here.

use routectl_core::{ContentPart, KnownContentPart, Message, MessageContent, Role};

use crate::system_filter::{is_billing_attribution_part, system_message_carries_billing_block};

/// One `input_text` block the Responses egress emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BreakpointBlock<'a> {
    /// Index of the message in the slice the blocks were read from.
    pub message: usize,
    /// Position of the block among its message's non-empty text parts,
    /// counting the ones the egress withholds.
    pub text_rank: usize,
    /// The block's text.
    pub text: &'a str,
}

/// Every `input_text` block the Responses egress emits for `messages`, in
/// the order it emits them; a block's index is its breakpoint ordinal.
///
/// A user turn and an unrecognized-role turn emit each non-empty text part.
/// A system turn emits them only when it stays in place (a liftable one is
/// lifted into `instructions`). A system-authority turn
/// emits none of its billing/attribution parts. Assistant text ships as
/// `output_text` and tool output as a `function_call_output`, so neither
/// adds a block.
pub fn breakpoint_blocks(messages: &[Message]) -> Vec<BreakpointBlock<'_>> {
    let mut blocks = Vec::new();
    for (message, msg) in messages.iter().enumerate() {
        if !emits_input_text(msg) {
            continue;
        }
        let screened = system_message_carries_billing_block(msg);
        let texts = non_empty_text_parts(&msg.content);
        for (text_rank, (text, part)) in texts.into_iter().enumerate() {
            let withheld = screened && part.is_none_or(is_billing_attribution_part);
            if !withheld {
                blocks.push(BreakpointBlock {
                    message,
                    text_rank,
                    text,
                });
            }
        }
    }
    blocks
}

fn emits_input_text(msg: &Message) -> bool {
    match msg.role {
        Role::User | Role::Other(_) => true,
        Role::System => msg.stays_in_place(),
        Role::Assistant | Role::Tool => false,
    }
}

/// Each non-empty text of `content`, with the part it came from (`None` for
/// a flat text content).
fn non_empty_text_parts(content: &MessageContent) -> Vec<(&str, Option<&ContentPart>)> {
    match content {
        MessageContent::Text(text) if !text.is_empty() => vec![(text.as_str(), None)],
        MessageContent::Text(_) | MessageContent::Null => Vec::new(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Known(KnownContentPart::Text { text, .. }) if !text.is_empty() => {
                    Some((text.as_str(), Some(part)))
                }
                _ => None,
            })
            .collect(),
    }
}
