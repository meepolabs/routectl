//! Canonical tool call / tool result pairing validation.
//!
//! One read-only pass over a request's `messages` that decides whether every
//! tool call and tool result is correctly paired. It never repairs: a
//! malformed transcript is rejected, and nothing is synthesized or dropped.
//!
//! Pairing is keyed on the correlation id alone (tool names never decide a
//! pair) and gathered from every canonical carrier:
//!
//! - calls: `Message.tool_calls[].id`, `ToolUse.id` content parts, and
//!   `ContentPart::Other` blocks whose `type` is `tool_use` (`id` field);
//! - results: `Role::Tool` `tool_call_id`, `ToolResult.tool_use_id` content
//!   parts, and `ContentPart::Other` blocks whose `type` is `tool_result`
//!   (`tool_use_id` field).
//!
//! A single message may carry one logical call on two carriers at once (a
//! content part and a `tool_calls` entry with the same id); that is one call,
//! not two. Any other repeat of a call id, on one carrier or across messages,
//! is a duplicate: it would make every later result naming it ambiguous.
//!
//! The rules are chronological and segment-local. A message carrying calls
//! opens a segment. Every call in it needs exactly one result, delivered by
//! the result-bearing messages (a `Role::Tool` turn, or any turn carrying a
//! tool-result block) that follow before the next message that carries no
//! result, or before the end of the request. Results for parallel calls may
//! arrive in any order. An empty or absent id is neither a call nor a result
//! here: it proves no pair and demands none, leaving id normalization to the
//! egress.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::Value;

use crate::content_part::{ContentPart, KnownContentPart};
use crate::schema::{Message, MessageContent, Role};

/// Which pairing rule a transcript broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolPairingDefect {
    /// A tool result whose id matches no tool call anywhere in the request.
    OrphanResult,
    /// A tool result whose matching call appears only in a later message.
    ResultBeforeCall,
    /// A tool result matching a call from an earlier segment that a
    /// non-result turn already closed.
    CrossSegmentResult,
    /// A second result for a call that already has one.
    DuplicateResult,
    /// A call id already used by another call in the request.
    DuplicateCallId,
    /// A call left without a result when a non-result turn arrived.
    InterruptedCalls,
    /// A call left without a result at the end of the request.
    TrailingPendingCalls,
}

impl ToolPairingDefect {
    const fn token(self) -> &'static str {
        match self {
            Self::OrphanResult => "orphan_result",
            Self::ResultBeforeCall => "result_before_call",
            Self::CrossSegmentResult => "cross_segment_result",
            Self::DuplicateResult => "duplicate_result",
            Self::DuplicateCallId => "duplicate_call_id",
            Self::InterruptedCalls => "interrupted_calls",
            Self::TrailingPendingCalls => "trailing_pending_calls",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::OrphanResult => "tool result matches no tool call",
            Self::ResultBeforeCall => "tool result precedes its tool call",
            Self::CrossSegmentResult => {
                "tool result matches a tool call from an earlier, already closed turn"
            }
            Self::DuplicateResult => "tool call already has a result",
            Self::DuplicateCallId => "tool call id is already used by another tool call",
            Self::InterruptedCalls => "tool call has no result before the next non-result turn",
            Self::TrailingPendingCalls => "tool call has no result before the end of the request",
        }
    }
}

impl fmt::Display for ToolPairingDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// The canonical field a pairing id was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carrier {
    ToolCalls,
    Content,
    ToolCallId,
}

/// A rejected transcript: the defect class plus the bounded position of the
/// offending call or result. Carries no tool content and no raw id, so its
/// `Display` is safe to return to a client and to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPairingError {
    defect: ToolPairingDefect,
    message_index: usize,
    item_index: Option<usize>,
    carrier: Carrier,
}

impl ToolPairingError {
    /// The rule the transcript broke.
    #[must_use]
    pub const fn defect(&self) -> ToolPairingDefect {
        self.defect
    }

    /// Index into `messages` of the offending call or result.
    #[must_use]
    pub const fn message_index(&self) -> usize {
        self.message_index
    }

    /// Index of the offending entry within that message's `content` parts or
    /// `tool_calls` array; `None` when the id came from `tool_call_id`.
    #[must_use]
    pub const fn item_index(&self) -> Option<usize> {
        self.item_index
    }
}

impl fmt::Display for ToolPairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool call/result pairing: {} at messages[{}]",
            self.defect.description(),
            self.message_index
        )?;
        match (self.carrier, self.item_index) {
            (Carrier::ToolCalls, Some(i)) => write!(f, ".tool_calls[{i}]")?,
            (Carrier::Content, Some(i)) => write!(f, ".content[{i}]")?,
            (Carrier::ToolCallId, _) => f.write_str(".tool_call_id")?,
            (Carrier::ToolCalls | Carrier::Content, None) => {}
        }
        write!(f, " ({})", self.defect)
    }
}

impl std::error::Error for ToolPairingError {}

/// Validate tool call / tool result pairing across `messages`.
///
/// Pure and read-only: the transcript is never modified. See the module docs
/// for the carriers read and the rules applied.
///
/// # Errors
///
/// Returns the first [`ToolPairingError`] found in chronological order.
pub fn validate_tool_pairing(messages: &[Message]) -> Result<(), ToolPairingError> {
    let mut open: Option<Segment<'_>> = None;
    let mut closed: HashSet<&str> = HashSet::new();
    let mut called: HashSet<&str> = HashSet::new();
    for (m, msg) in messages.iter().enumerate() {
        match message_results(m, msg) {
            Some(results) => {
                for result in &results {
                    resolve(open.as_mut(), &closed, messages, result)?;
                }
            }
            None => close(&mut open, &mut closed, ToolPairingDefect::InterruptedCalls)?,
        }
        let calls = message_calls(m, msg);
        if !calls.is_empty() {
            close(&mut open, &mut closed, ToolPairingDefect::InterruptedCalls)?;
            if let Some(reused) = calls.iter().find(|c| !called.insert(c.id)) {
                return Err(reused.error(ToolPairingDefect::DuplicateCallId));
            }
            open = Some(Segment::new(calls));
        }
    }
    close(
        &mut open,
        &mut closed,
        ToolPairingDefect::TrailingPendingCalls,
    )
}

/// One call or result id and where it was read.
#[derive(Clone, Copy)]
struct Located<'a> {
    id: &'a str,
    message: usize,
    item: Option<usize>,
    carrier: Carrier,
}

impl Located<'_> {
    const fn error(&self, defect: ToolPairingDefect) -> ToolPairingError {
        ToolPairingError {
            defect,
            message_index: self.message,
            item_index: self.item,
            carrier: self.carrier,
        }
    }
}

/// The calls of the open segment, each with whether it has its result.
struct Segment<'a> {
    calls: Vec<(Located<'a>, bool)>,
    by_id: HashMap<&'a str, usize>,
}

impl<'a> Segment<'a> {
    fn new(calls: Vec<Located<'a>>) -> Self {
        let by_id = calls.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
        Self {
            calls: calls.into_iter().map(|c| (c, false)).collect(),
            by_id,
        }
    }

    fn first_pending(&self) -> Option<&Located<'a>> {
        self.calls
            .iter()
            .find(|(_, resolved)| !resolved)
            .map(|(c, _)| c)
    }
}

/// Close the open segment, if any: every call must have its result, and its
/// ids move to `closed` so a later result naming one is a cross-segment match.
fn close<'a>(
    open: &mut Option<Segment<'a>>,
    closed: &mut HashSet<&'a str>,
    defect: ToolPairingDefect,
) -> Result<(), ToolPairingError> {
    let Some(segment) = open.take() else {
        return Ok(());
    };
    if let Some(pending) = segment.first_pending() {
        return Err(pending.error(defect));
    }
    closed.extend(segment.by_id.into_keys());
    Ok(())
}

fn resolve(
    open: Option<&mut Segment<'_>>,
    closed: &HashSet<&str>,
    messages: &[Message],
    result: &Located<'_>,
) -> Result<(), ToolPairingError> {
    if let Some(segment) = open
        && let Some(&slot) = segment.by_id.get(result.id)
    {
        let resolved = &mut segment.calls[slot].1;
        if *resolved {
            return Err(result.error(ToolPairingDefect::DuplicateResult));
        }
        *resolved = true;
        return Ok(());
    }
    let defect = if closed.contains(result.id) {
        ToolPairingDefect::CrossSegmentResult
    } else if called_later(messages, result.message, result.id) {
        ToolPairingDefect::ResultBeforeCall
    } else {
        ToolPairingDefect::OrphanResult
    };
    Err(result.error(defect))
}

/// Whether any message after `after` carries a call with `id`. Only reached
/// on the rejection path, so the rescan costs nothing on a valid transcript.
fn called_later(messages: &[Message], after: usize, id: &str) -> bool {
    messages.iter().skip(after + 1).any(|msg| {
        part_call_ids(msg).any(|(_, call)| call == id)
            || tool_calls_ids(msg).any(|(_, call)| call == id)
    })
}

/// The message's calls, one entry per logical call: a content-part call and
/// a `tool_calls` entry sharing an id are one call. A repeat on one carrier
/// is kept, so the request-wide duplicate check sees it.
fn message_calls(m: usize, msg: &Message) -> Vec<Located<'_>> {
    merge_carriers(
        locate(m, Carrier::Content, part_call_ids(msg)),
        locate(m, Carrier::ToolCalls, tool_calls_ids(msg)),
    )
}

/// The message's results, one entry per logical result, or `None` when the
/// message is not result-bearing (a semantic turn). A `Role::Tool` turn is
/// always result-bearing, even with an empty id. A repeat on one carrier is
/// kept, so resolution sees it as a duplicate result.
fn message_results(m: usize, msg: &Message) -> Option<Vec<Located<'_>>> {
    let is_tool_turn = matches!(msg.role, Role::Tool);
    if !is_tool_turn && !has_result_part(msg) {
        return None;
    }
    let from_parts = locate(m, Carrier::Content, part_result_ids(msg));
    let from_field: Vec<Located<'_>> = msg
        .tool_call_id
        .as_deref()
        .filter(|id| is_tool_turn && !id.is_empty())
        .map(|id| Located {
            id,
            message: m,
            item: None,
            carrier: Carrier::ToolCallId,
        })
        .into_iter()
        .collect();
    Some(merge_carriers(from_parts, from_field))
}

fn locate<'a>(
    m: usize,
    carrier: Carrier,
    ids: impl Iterator<Item = (usize, &'a str)>,
) -> Vec<Located<'a>> {
    ids.map(|(item, id)| Located {
        id,
        message: m,
        item: Some(item),
        carrier,
    })
    .collect()
}

/// `primary` plus every `secondary` entry whose id `primary` lacks.
fn merge_carriers<'a>(primary: Vec<Located<'a>>, secondary: Vec<Located<'a>>) -> Vec<Located<'a>> {
    let known: HashSet<&str> = primary.iter().map(|l| l.id).collect();
    primary
        .into_iter()
        .chain(secondary.into_iter().filter(|l| !known.contains(l.id)))
        .collect()
}

fn parts(msg: &Message) -> &[ContentPart] {
    match &msg.content {
        MessageContent::Parts(parts) => parts,
        MessageContent::Text(_) | MessageContent::Null => &[],
    }
}

fn part_call_ids(msg: &Message) -> impl Iterator<Item = (usize, &str)> {
    parts(msg).iter().enumerate().filter_map(|(i, part)| {
        let id = match part {
            ContentPart::Known(KnownContentPart::ToolUse { id, .. }) => Some(id.as_str()),
            ContentPart::Other {
                type_tag, extras, ..
            } if type_tag == "tool_use" => str_field(extras.get("id")),
            ContentPart::Known(_) | ContentPart::Other { .. } => None,
        };
        non_empty(id).map(|id| (i, id))
    })
}

fn part_result_ids(msg: &Message) -> impl Iterator<Item = (usize, &str)> {
    parts(msg).iter().enumerate().filter_map(|(i, part)| {
        let id = match part {
            ContentPart::Known(KnownContentPart::ToolResult { tool_use_id, .. }) => {
                Some(tool_use_id.as_str())
            }
            ContentPart::Other {
                type_tag, extras, ..
            } if type_tag == "tool_result" => str_field(extras.get("tool_use_id")),
            ContentPart::Known(_) | ContentPart::Other { .. } => None,
        };
        non_empty(id).map(|id| (i, id))
    })
}

fn has_result_part(msg: &Message) -> bool {
    parts(msg).iter().any(|part| match part {
        ContentPart::Known(KnownContentPart::ToolResult { .. }) => true,
        ContentPart::Other { type_tag, .. } => type_tag == "tool_result",
        ContentPart::Known(_) => false,
    })
}

fn tool_calls_ids(msg: &Message) -> impl Iterator<Item = (usize, &str)> {
    msg.tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(i, call)| non_empty(str_field(call.get("id"))).map(|id| (i, id)))
}

fn str_field(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn non_empty(id: Option<&str>) -> Option<&str> {
    id.filter(|id| !id.is_empty())
}

#[cfg(test)]
#[path = "tool_pairing_tests.rs"]
mod tests;
