//! Reports what the Anthropic-shaped egresses (anthropic-api and Bedrock
//! Invoke) change in a request body, as content-free declared edits on the
//! attempt's wire tap.
//!
//! Every helper is a no-op without a tap, and the cloak snapshots are taken
//! only when one is present, so an untapped request pays nothing. Pointers
//! name the location in the body the site operates on.

use std::borrow::Cow;

use serde_json::Value;

use routectl_core::wire_check::{EditKind, EditSite, WireTap};
use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role, SystemContent,
};

pub use crate::wire_declare::tap as tap_of;

/// Declare one request-body edit when a tap is present; see
/// [`crate::wire_declare::declare`] for how a refused pattern is handled.
pub fn declare(
    tap: Option<&WireTap>,
    site: EditSite,
    path: impl Into<Cow<'static, str>>,
    kind: EditKind,
) {
    if let Some(tap) = tap {
        crate::wire_declare::declare(tap, site, path, kind);
    }
}

/// Declare the removal of each top-level key in `keys`.
pub fn declare_removed_keys(tap: Option<&WireTap>, site: EditSite, keys: &[&'static str]) {
    for key in keys {
        declare(tap, site, format!("/{key}"), EditKind::Remove);
    }
}

/// Declare the client's `stream` flag removed from the body.
pub fn declare_stream_strip(tap: Option<&WireTap>, client: Option<bool>) {
    if client.is_some() {
        declare(tap, EditSite::StreamStrip, "/stream", EditKind::Remove);
    }
}

/// Declare `output_config.effort` added, removed or rewritten relative to
/// the `output_config` the client sent through the forward-compat extras.
pub fn declare_effort_edits(tap: Option<&WireTap>, req: &ChatRequest, body: &Value) {
    if tap.is_none() {
        return;
    }
    let client_config = req
        .provider_extras
        .as_ref()
        .and_then(|extras| extras.get("output_config"))
        .and_then(Value::as_object);
    let sent_config = body.get("output_config").and_then(Value::as_object);
    let client_effort = client_config.and_then(|config| config.get("effort"));
    let sent_effort = sent_config.and_then(|config| config.get("effort"));
    let (path, kind) = match (client_effort, sent_effort) {
        (None, Some(_)) if client_config.is_none() => ("/output_config", EditKind::Add),
        (None, Some(_)) => ("/output_config/effort", EditKind::Add),
        (Some(_), None) if sent_config.is_none() => ("/output_config", EditKind::Remove),
        (Some(_), None) => ("/output_config/effort", EditKind::Remove),
        (Some(a), Some(b)) if a != b => ("/output_config/effort", EditKind::Rewrite),
        _ => return,
    };
    declare(tap, EditSite::EffortInjection, path, kind);
}

/// Declare each block of canonical message `message` that `is_stripped`
/// removes as an unsigned thinking block.
pub fn declare_unsigned_thinking_strip(
    tap: Option<&WireTap>,
    message: usize,
    parts: &[ContentPart],
    is_stripped: fn(&ContentPart) -> bool,
) {
    if tap.is_none() {
        return;
    }
    for (block, _) in parts.iter().enumerate().filter(|(_, p)| is_stripped(p)) {
        declare(
            tap,
            EditSite::SignatureStrip,
            format!("/messages/{message}/content/{block}"),
            EditKind::Remove,
        );
    }
}

/// Declare where the thinking-mode sampling clamp moved the client's
/// `temperature` / `top_p` off the values the client sent. The clamp only
/// rewrites or removes a key the client sent, never adds one.
pub fn declare_sampling_clamp(
    tap: Option<&WireTap>,
    req: &ChatRequest,
    temperature: Option<f64>,
    top_p: Option<f64>,
) {
    for (key, client, sent) in [
        ("/temperature", req.temperature, temperature),
        ("/top_p", req.top_p, top_p),
    ] {
        let kind = match (client, sent) {
            (Some(_), None) => EditKind::Remove,
            (Some(a), Some(b)) if a != b => EditKind::Rewrite,
            _ => continue,
        };
        declare(tap, EditSite::ThinkingSamplingClamp, key, kind);
    }
}

/// Declare each `tool_use` id and `tool_result` `tool_use_id` the wire
/// charset forces a rewrite of, at its canonical message and block index --
/// the client's own indices on the Anthropic ingress, the one lane whose diff
/// reads them. A `tool_use` id is rewritten on assistant turns only; a
/// `tool_result` id on any turn translated part by part. Chat-shaped
/// `tool_calls` and tool turns reach this egress only from a translating
/// ingress, where no path is compared. The ids are re-sanitized to learn
/// whether any changes; the walk runs only with a tap.
pub fn declare_tool_id_sanitize(tap: Option<&WireTap>, messages: &[Message]) {
    if tap.is_none() {
        return;
    }
    for (index, msg) in messages.iter().enumerate() {
        let MessageContent::Parts(parts) = &msg.content else {
            continue;
        };
        if matches!(msg.role, Role::Tool | Role::System) {
            continue;
        }
        let is_assistant = matches!(msg.role, Role::Assistant);
        for (block, part) in parts.iter().enumerate() {
            let (id, field) = match part {
                ContentPart::Known(KnownContentPart::ToolUse { id, .. }) if is_assistant => {
                    (id, "id")
                }
                ContentPart::Known(KnownContentPart::ToolResult { tool_use_id, .. }) => {
                    (tool_use_id, "tool_use_id")
                }
                _ => continue,
            };
            if matches!(crate::tool_id::sanitize_tool_id(id), Cow::Owned(_)) {
                declare(
                    tap,
                    EditSite::ToolIdSanitize,
                    format!("/messages/{index}/content/{block}/{field}"),
                    EditKind::Rewrite,
                );
            }
        }
    }
}

/// Declare each `additionalProperties: false` the output-schema repair
/// injected, at its exact pointer: property names masked, schema keywords
/// and indices verbatim, so a key added anywhere else in the schema stays a
/// finding.
pub fn declare_schema_injections(
    tap: Option<&WireTap>,
    repair: &super::output_schema::AdditionalPropertiesRepair,
) {
    if tap.is_none() {
        return;
    }
    for pointer in repair.injected_pointers() {
        declare(
            tap,
            EditSite::RequiredFieldDefault,
            pointer.to_owned(),
            EditKind::Add,
        );
    }
}

/// Declare each client `system` block the always-on billing strip withheld.
/// A `system` string that was wholly the billing block names the whole
/// field, which the register refuses as a blanket, so it stays a finding.
pub fn declare_canonical_billing_strip(tap: Option<&WireTap>, system: &SystemContent) {
    let SystemContent::Blocks(blocks) = system else {
        return;
    };
    for (index, block) in blocks.iter().enumerate() {
        if crate::system_filter::is_billing_attribution_block(&block.text) {
            declare(
                tap,
                EditSite::ClientFingerprintStrip,
                format!("/system/{index}"),
                EditKind::Remove,
            );
        }
    }
}

/// Body facts the cloak transforms change, captured before they run so each
/// one's edit can be declared after it. Taken only when a tap is present.
pub(super) struct CloakView {
    system_blocks: Option<usize>,
    billing_blocks: Vec<usize>,
    system_turns: Vec<usize>,
    first_user: Option<(usize, ContentShape)>,
    message_count: usize,
    user_id: UserIdState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentShape {
    Blocks(usize),
    Text,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserIdState {
    NoMetadata,
    Absent,
    Empty,
    Set,
}

impl CloakView {
    /// `client_system_blocks` is the length of the client's own `system`
    /// array, which can exceed the body's when an earlier pass dropped a
    /// block from it.
    pub(super) fn capture(
        tap: Option<&WireTap>,
        body: &Value,
        client_system_blocks: Option<usize>,
    ) -> Option<Self> {
        tap?;
        let system = body.get("system").and_then(Value::as_array);
        let messages = body.get("messages").and_then(Value::as_array);
        let roles: Vec<Option<&str>> = messages
            .into_iter()
            .flatten()
            .map(|m| m.get("role").and_then(Value::as_str))
            .collect();
        let first_user = roles
            .iter()
            .position(|role| *role == Some("user"))
            .and_then(|index| Some((index, content_shape(messages?.get(index)?))));
        Some(Self {
            system_blocks: system.map(Vec::len).max(client_system_blocks),
            billing_blocks: system
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, block)| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(crate::system_filter::is_billing_attribution_block)
                })
                .map(|(index, _)| index)
                .collect(),
            system_turns: roles
                .iter()
                .enumerate()
                .filter(|(_, role)| **role == Some("system"))
                .map(|(index, _)| index)
                .collect(),
            first_user,
            message_count: roles.len(),
            user_id: user_id_state(body),
        })
    }
}

fn content_shape(message: &Value) -> ContentShape {
    match message.get("content") {
        Some(Value::Array(blocks)) => ContentShape::Blocks(blocks.len()),
        Some(Value::String(_)) => ContentShape::Text,
        _ => ContentShape::Other,
    }
}

fn user_id_state(body: &Value) -> UserIdState {
    let Some(metadata) = body.get("metadata") else {
        return UserIdState::NoMetadata;
    };
    match metadata.get("user_id").and_then(Value::as_str) {
        None => UserIdState::Absent,
        Some("") => UserIdState::Empty,
        Some(_) => UserIdState::Set,
    }
}

/// Declare each billing block the cloak's strip removed from a `system`
/// array. A `system` string that was wholly the billing block names the
/// whole field, which the register refuses as a blanket, so it stays a
/// finding.
pub(super) fn declare_cloak_billing_strip(tap: Option<&WireTap>, before: Option<&CloakView>) {
    for index in before.into_iter().flat_map(|view| &view.billing_blocks) {
        declare(
            tap,
            EditSite::Cloak,
            format!("/system/{index}"),
            EditKind::Remove,
        );
    }
}

/// Declare the client-system relocation: the `system` array replaced by the
/// identity line, the `role: "system"` turns removed, and the relocated
/// blocks landing in the first user turn or a synthetic leading one.
pub(super) fn declare_cloak_relocation(
    tap: Option<&WireTap>,
    before: Option<&CloakView>,
    body: &Value,
) {
    let Some(before) = before else {
        return;
    };
    for index in 0..before.system_blocks.unwrap_or(0) {
        declare(
            tap,
            EditSite::Cloak,
            format!("/system/{index}"),
            EditKind::Remove,
        );
    }
    if before.system_blocks.is_some() {
        declare(tap, EditSite::Cloak, "/system/0", EditKind::Add);
    }
    for index in &before.system_turns {
        declare(
            tap,
            EditSite::Cloak,
            format!("/messages/{index}"),
            EditKind::Remove,
        );
    }
    declare_relocation_landing(tap, before, body);
}

fn declare_relocation_landing(tap: Option<&WireTap>, before: &CloakView, body: &Value) {
    let messages = body.get("messages").and_then(Value::as_array);
    let after_count = messages.map_or(0, Vec::len);
    let kept = before.message_count - before.system_turns.len();
    if after_count > kept {
        declare(tap, EditSite::Cloak, "/messages/0", EditKind::Add);
        return;
    }
    let Some((index, shape)) = before.first_user else {
        return;
    };
    let landed = before
        .system_turns
        .iter()
        .filter(|turn| **turn < index)
        .count();
    let after = messages
        .and_then(|m| m.get(index - landed))
        .map_or(ContentShape::Other, content_shape);
    match (shape, after) {
        (ContentShape::Blocks(was), ContentShape::Blocks(now)) => {
            for block in 0..now.saturating_sub(was) {
                declare(
                    tap,
                    EditSite::Cloak,
                    format!("/messages/{index}/content/{block}"),
                    EditKind::Add,
                );
            }
        }
        (was, now) if was != now => declare(
            tap,
            EditSite::Cloak,
            format!("/messages/{index}/content"),
            EditKind::Rewrite,
        ),
        _ => {}
    }
}

/// Declare the minted `metadata.user_id`. Keys under `metadata` render
/// masked, so the member is named by the wildcard.
pub(super) fn declare_cloak_user_id(
    tap: Option<&WireTap>,
    before: Option<&CloakView>,
    body: &Value,
) {
    let Some(before) = before else {
        return;
    };
    if user_id_state(body) != UserIdState::Set {
        return;
    }
    let (path, kind) = match before.user_id {
        UserIdState::NoMetadata => ("/metadata", EditKind::Add),
        UserIdState::Absent => ("/metadata/*", EditKind::Add),
        UserIdState::Empty => ("/metadata/*", EditKind::Rewrite),
        UserIdState::Set => return,
    };
    declare(tap, EditSite::Cloak, path, kind);
}

/// The custom-tool names and the forced tool name, in body order; taken only
/// with a tap.
pub(super) struct ToolNames {
    tools: Vec<Option<String>>,
    tool_choice: Option<String>,
}

impl ToolNames {
    pub(super) fn capture(tap: Option<&WireTap>, body: &Value) -> Option<Self> {
        tap?;
        Some(Self {
            tools: body
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
                .collect(),
            tool_choice: body
                .get("tool_choice")
                .and_then(|choice| choice.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// Declare the tool-name rewrites: each renamed `tools[]` entry and the
/// forced `tool_choice` name concretely, and the tool-name fields inside
/// message content by pattern when anything was renamed at all.
pub(super) fn declare_cloak_tool_names(
    tap: Option<&WireTap>,
    before: Option<&ToolNames>,
    after: Option<&ToolNames>,
    renamed_any: bool,
) {
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    for (index, (was, now)) in before.tools.iter().zip(&after.tools).enumerate() {
        if was != now {
            declare(
                tap,
                EditSite::Cloak,
                format!("/tools/{index}/name"),
                EditKind::Rewrite,
            );
        }
    }
    if before.tool_choice != after.tool_choice {
        declare(tap, EditSite::Cloak, "/tool_choice/name", EditKind::Rewrite);
    }
    if !renamed_any {
        return;
    }
    for pattern in [
        "/messages/*/content/*/name",
        "/messages/*/content/*/tool_name",
        "/messages/*/content/*/content/*/tool_name",
    ] {
        declare(tap, EditSite::Cloak, pattern, EditKind::Rewrite);
    }
}

/// Declare the sensitive-word rewrites of the text-carrying fields on each
/// surface that changed.
pub(super) fn declare_cloak_obfuscation(
    tap: Option<&WireTap>,
    system_changed: bool,
    messages_changed: bool,
) {
    if system_changed {
        declare(tap, EditSite::Cloak, "/system/*/text", EditKind::Rewrite);
    }
    if messages_changed {
        for pattern in [
            "/messages/*/content",
            "/messages/*/content/*/text",
            "/messages/*/content/*/source/data",
            "/messages/*/content/*/source/content",
            "/messages/*/content/*/source/content/*/text",
        ] {
            declare(tap, EditSite::Cloak, pattern, EditKind::Rewrite);
        }
    }
}

/// Declare every `tools[]` position the tool sort moved an entry out of.
pub(super) fn declare_cloak_tool_sort(
    tap: Option<&WireTap>,
    before: Option<&ToolNames>,
    after: Option<&ToolNames>,
) {
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    for (index, (was, now)) in before.tools.iter().zip(&after.tools).enumerate() {
        if was != now {
            declare(
                tap,
                EditSite::Cloak,
                format!("/tools/{index}"),
                EditKind::Relocate,
            );
        }
    }
}

#[cfg(test)]
#[path = "wire_edits_tests.rs"]
mod tests;
