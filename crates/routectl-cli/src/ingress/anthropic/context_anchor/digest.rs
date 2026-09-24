//! Streaming prefix digest and normalized size over a canonical request.
//!
//! The normalized stream is a version tag, one JSON object holding the
//! prompt-affecting request fields, then each message as its own JSON
//! value. JSON values are self-delimiting, so a fixed sequence of them is
//! unambiguous; a newline between them keeps the stream readable when
//! debugging. The same bytes feed the hasher and the byte counter, so the
//! anchor's size delta can never count a field the digest ignores. The
//! digest of the first `n` messages is taken from a clone of the running
//! hasher, so verifying a prior prefix and fingerprinting the whole request
//! cost one pass.
//!
//! A `cache_control` is removed only at recognized annotation positions: a
//! system block, a typed custom tool, a top-level typed content block (and
//! the unmodeled `server_tool_use`), and a `text` / `image` / `document`
//! block directly inside a tool result's content array. Anything else --
//! tool arguments, a passthrough tool, any other unmodeled block type,
//! deeper payload -- is hashed verbatim, so an ambiguous shape misses
//! rather than hits.

use std::io::{self, Write};

use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, SystemBlock,
    SystemContent, ToolDef,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::PrefixDigest;

/// Bumped whenever the hashed representation changes, so a digest from an
/// older layout can never equal one from a newer layout.
const STREAM_VERSION: &[u8] = b"context-anchor/v1\n";

const CACHE_CONTROL_KEY: &str = "cache_control";

/// Must match the provider-side billing-block rule: a leading occurrence
/// after trimming leading whitespace.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// Unmodeled content-block types whose top-level `cache_control` is a
/// block annotation. Only `server_tool_use` is listed: it is the one such
/// type the core schema already round-trips with a block-level marker.
/// Every other unmodeled type is hashed verbatim, marker included.
const ANNOTATED_PASSTHROUGH_BLOCKS: [&str; 1] = ["server_tool_use"];

/// Typed block kinds that may sit in a tool result's content array and
/// carry their own `cache_control` annotation. Anything else in that array
/// is hashed verbatim.
const TOOL_RESULT_BLOCKS: [&str; 3] = ["text", "image", "document"];

/// Digests of one request: the whole request, and optionally its first
/// `n` messages. `None` when the request could not be represented.
pub(super) struct Digests {
    pub(super) full: Option<PrefixDigest>,
    pub(super) prefix: Option<(usize, PrefixDigest)>,
    pub(super) normalized_bytes: u64,
}

/// Hash the prompt-affecting fields and messages of `req`, also capturing
/// the digest after `prefix_len` messages when that many exist.
pub(super) fn digest_request(req: &ChatRequest, prefix_len: Option<usize>) -> Digests {
    let prefix_len = prefix_len.filter(|n| *n <= req.messages.len());
    match stream_digests(req, prefix_len) {
        Ok((full, prefix, normalized_bytes)) => Digests {
            full: Some(full),
            prefix: prefix_len.zip(prefix),
            normalized_bytes,
        },
        Err(_) => Digests {
            full: None,
            prefix: None,
            normalized_bytes: 0,
        },
    }
}

fn stream_digests(
    req: &ChatRequest,
    prefix_len: Option<usize>,
) -> serde_json::Result<(PrefixDigest, Option<PrefixDigest>, u64)> {
    let mut sink = HashSink::default();
    sink.feed(STREAM_VERSION);
    write_value(&mut sink, &stable_fields(req)?)?;

    let mut prefix = None;
    for (index, message) in req.messages.iter().enumerate() {
        if prefix_len == Some(index) {
            prefix = Some(finish(sink.hasher.clone()));
        }
        write_message(&mut sink, message)?;
    }
    let bytes = sink.bytes;
    let full = finish(sink.hasher);
    if prefix_len == Some(req.messages.len()) {
        prefix = Some(full);
    }
    Ok((full, prefix, bytes))
}

fn finish(hasher: Sha256) -> PrefixDigest {
    hasher.finalize().into()
}

fn write_value(sink: &mut HashSink, value: &Value) -> serde_json::Result<()> {
    serde_json::to_writer(&mut *sink, value)?;
    sink.feed(b"\n");
    Ok(())
}

/// The request fields that shape the prompt, minus cache markers and the
/// per-request billing block. Every passthrough extra is kept: the egress
/// forwards them, and an extra not proven inert must miss. Sampling knobs (temperature, max tokens,
/// stop sequences, ...) change the output, not the input, and are left out.
#[derive(Serialize)]
struct StableFields<'a> {
    model: &'a str,
    system: Option<Value>,
    tools: Option<Vec<Value>>,
    tool_choice: Option<&'a Value>,
    response_format: Option<&'a Value>,
    reasoning: Option<&'a routectl_core::ReasoningConfig>,
    anthropic_beta: &'a [String],
    chat_template_kwargs: Option<&'a Value>,
    provider_extras: Option<&'a Value>,
}

fn stable_fields(req: &ChatRequest) -> serde_json::Result<Value> {
    let fields = StableFields {
        model: &req.model,
        system: req.system.as_ref().map(system_value).transpose()?.flatten(),
        tools: req
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(tool_value).collect())
            .transpose()?,
        tool_choice: req.tool_choice.as_ref(),
        response_format: req.response_format.as_ref(),
        reasoning: req.reasoning.as_ref(),
        anthropic_beta: &req.anthropic_beta,
        chat_template_kwargs: req.chat_template_kwargs.as_ref(),
        provider_extras: req.provider_extras.as_ref(),
    };
    serde_json::to_value(fields)
}

/// The system prompt as the upstream sees it: billing blocks removed, and
/// `None` when nothing else remains, so a billing-only system hashes and
/// counts exactly like an absent one. Non-billing text is kept verbatim.
fn system_value(system: &SystemContent) -> serde_json::Result<Option<Value>> {
    match system {
        SystemContent::Text(text) if is_billing_block(text) => Ok(None),
        SystemContent::Text(text) => Ok(Some(Value::String(text.clone()))),
        SystemContent::Blocks(blocks) => {
            let kept = blocks
                .iter()
                .filter(|block| !is_billing_block(&block.text))
                .map(system_block_value)
                .collect::<serde_json::Result<Vec<_>>>()?;
            Ok((!kept.is_empty()).then_some(Value::Array(kept)))
        }
    }
}

fn system_block_value(block: &SystemBlock) -> serde_json::Result<Value> {
    serde_json::to_value(block).map(without_cache_control)
}

fn is_billing_block(text: &str) -> bool {
    text.trim_start().starts_with(BILLING_PREFIX)
}

fn tool_value(tool: &ToolDef) -> serde_json::Result<Value> {
    match tool {
        ToolDef::Custom(_) => serde_json::to_value(tool).map(without_cache_control),
        ToolDef::Other(value) => Ok(value.clone()),
    }
}

/// Hash one message with its cache markers removed. The common case -- no
/// marker anywhere in the message -- serializes the borrowed message
/// directly; only a marked message pays for a stripped clone. Tool-use
/// `input` is the model's own argument object and is hashed verbatim.
fn write_message(sink: &mut HashSink, message: &Message) -> serde_json::Result<()> {
    if carries_cache_marker(message) {
        serde_json::to_writer(&mut *sink, &without_markers(message))?;
    } else {
        serde_json::to_writer(&mut *sink, message)?;
    }
    sink.feed(b"\n");
    Ok(())
}

fn carries_cache_marker(message: &Message) -> bool {
    let MessageContent::Parts(parts) = &message.content else {
        return false;
    };
    parts
        .iter()
        .any(|part| part_annotation_is_strippable(part) || nested_result_marker(part))
}

fn nested_result_marker(part: &ContentPart) -> bool {
    let ContentPart::Known(KnownContentPart::ToolResult {
        content: Value::Array(inner),
        ..
    }) = part
    else {
        return false;
    };
    inner
        .iter()
        .any(|block| is_tool_result_block(block) && block.get(CACHE_CONTROL_KEY).is_some())
}

fn part_annotation_is_strippable(part: &ContentPart) -> bool {
    part.cache_control().is_some()
        && match part {
            ContentPart::Known(_) => true,
            ContentPart::Other { type_tag, .. } => {
                ANNOTATED_PASSTHROUGH_BLOCKS.contains(&type_tag.as_str())
            }
        }
}

fn is_tool_result_block(block: &Value) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|tag| TOOL_RESULT_BLOCKS.contains(&tag))
}

fn without_markers(message: &Message) -> Message {
    let content = match &message.content {
        MessageContent::Parts(parts) => {
            MessageContent::Parts(parts.iter().map(part_without_markers).collect())
        }
        other => other.clone(),
    };
    Message {
        role: message.role.clone(),
        content,
        reasoning: message.reasoning.clone(),
        reasoning_details: message.reasoning_details.clone(),
        name: message.name.clone(),
        tool_call_id: message.tool_call_id.clone(),
        tool_calls: message.tool_calls.clone(),
        refusal: message.refusal.clone(),
    }
}

fn part_without_markers(part: &ContentPart) -> ContentPart {
    match part {
        ContentPart::Known(known) => ContentPart::Known(known_without_markers(known)),
        ContentPart::Other {
            type_tag, extras, ..
        } if ANNOTATED_PASSTHROUGH_BLOCKS.contains(&type_tag.as_str()) => ContentPart::Other {
            type_tag: type_tag.clone(),
            cache_control: None,
            extras: extras.clone(),
        },
        other @ ContentPart::Other { .. } => other.clone(),
    }
}

fn known_without_markers(known: &KnownContentPart) -> KnownContentPart {
    let mut stripped = known.clone();
    match &mut stripped {
        KnownContentPart::Text { cache_control, .. }
        | KnownContentPart::Image { cache_control, .. }
        | KnownContentPart::ImageUrl { cache_control, .. }
        | KnownContentPart::File { cache_control, .. }
        | KnownContentPart::Document { cache_control, .. }
        | KnownContentPart::ToolUse { cache_control, .. } => *cache_control = None,
        KnownContentPart::ToolResult {
            cache_control,
            content,
            ..
        } => {
            *cache_control = None;
            if let Value::Array(inner) = content {
                let blocks = std::mem::take(inner);
                *inner = blocks
                    .into_iter()
                    .map(|block| {
                        if is_tool_result_block(&block) {
                            without_cache_control(block)
                        } else {
                            block
                        }
                    })
                    .collect();
            }
        }
        KnownContentPart::Thinking { .. } | KnownContentPart::RedactedThinking { .. } => {}
    }
    stripped
}

fn without_cache_control(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.remove(CACHE_CONTROL_KEY);
            Value::Object(map)
        }
        other => other,
    }
}

/// `io::Write` adapter feeding serialized bytes straight into the hasher
/// and the byte count, so no serialized copy of the request is ever held.
#[derive(Default)]
struct HashSink {
    hasher: Sha256,
    bytes: u64,
}

impl HashSink {
    fn feed(&mut self, buf: &[u8]) {
        self.hasher.update(buf);
        self.bytes = self.bytes.saturating_add(buf.len() as u64);
    }
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
