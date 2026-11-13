//! What the Bedrock Mantle Responses lane lets through of the request
//! members this hub does not model.
//!
//! Mantle fetches URLs and file references it finds in a request under the
//! operator's AWS identity and documents no list of the members it reads,
//! so on that lane nothing reaches the wire that routectl has not reviewed:
//!
//! - an unmodeled `input` item forwards only when its kind is in
//!   [`MANTLE_PASSTHROUGH_KINDS`] and every member it carries, at every
//!   depth, is one that table names with a value of the reviewed shape;
//! - a `ToolDef::Other` tool and a namespace container never forward (see
//!   `tools.rs`), so neither does the `namespace` routectl stamps on a call
//!   to a member of one;
//! - the members carried on a modeled message, part or tool output never
//!   forward (see `messages.rs`).
//!
//! Each withheld member is declared as a Remove and the request counts once
//! as `unknown_member_withheld`. Typed top-level request members ride the
//! provider_extras allowlist in `extras.rs`; of those, the two prompt-cache
//! fields forward on this lane only as [`admit_cache_value`] rebuilds them.
//! The serialized-body media URL walk (`mantle_media`) still runs after
//! translation, as defence in depth.
//!
//! Allowlist review, against `ResponseItem` in codex-rs/protocol/src/models.rs:
//!
//! - `agent_message`: `id`, `author` and `recipient` are strings, and
//!   `content` holds only `input_text {text}` and
//!   `encrypted_content {encrypted_content}` (`AgentMessageInputContent`):
//!   no URL or file reference.
//! - `local_shell_call`: `id`, `call_id` and `status` are strings, and
//!   `action` is `exec {command, timeout_ms, working_directory, env, user}`
//!   (`LocalShellExecAction`), a command the client runs, never a fetch.
//! - `compaction` (alias `compaction_summary`) and `context_compaction`: an
//!   `id` and an opaque `encrypted_content` string.
//!
//! `internal_chat_message_metadata_passthrough` is on no kind: codex clears
//! it before sending to a provider that is not OpenAI. Left off on purpose:
//! `function_call_output` and `custom_tool_call_output` (their `output` can
//! hold `input_image {image_url | file_id}` and `input_audio {audio_url}`,
//! `FunctionCallOutputContentItem`), `custom_tool_call` (its output never
//! reaches this lane), `web_search_call` (`WebSearchAction::OpenPage {url}`),
//! `tool_search_call` (free-form `arguments`), and the tool-declaring kinds
//! (`additional_tools`, `tool_search_output`), whose tools may name a remote
//! server.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use routectl_core::ResponsesPassthroughItem;
use routectl_core::wire_check::{EditKind, EditSite, WireTap};

use super::AuthKind;
use super::extras::{PromptCacheField, UnknownMemberWithheldTally};
use super::types::ResponseInputItem;
use crate::wire_declare::{declare, member_pointer};

/// The value a reviewed member may hold. Every shape also admits `null`.
#[derive(Debug, Clone, Copy)]
enum Shape {
    Text,
    Number,
    Texts,
    TextMap,
    /// An array of objects, each one of the listed kinds.
    Objects(&'static [ReviewedKind]),
    /// One object of one of the listed kinds.
    Object(&'static [ReviewedKind]),
}

/// An object kind, by its `type`, and the members it may carry besides
/// `type`.
#[derive(Debug)]
pub(super) struct ReviewedKind {
    tag: &'static str,
    members: &'static [(&'static str, Shape)],
}

const AGENT_MESSAGE_CONTENT: &[ReviewedKind] = &[
    ReviewedKind {
        tag: "input_text",
        members: &[("text", Shape::Text)],
    },
    ReviewedKind {
        tag: "encrypted_content",
        members: &[("encrypted_content", Shape::Text)],
    },
];

const LOCAL_SHELL_ACTION: &[ReviewedKind] = &[ReviewedKind {
    tag: "exec",
    members: &[
        ("command", Shape::Texts),
        ("timeout_ms", Shape::Number),
        ("working_directory", Shape::Text),
        ("env", Shape::TextMap),
        ("user", Shape::Text),
    ],
}];

const COMPACTION_MEMBERS: &[(&str, Shape)] =
    &[("id", Shape::Text), ("encrypted_content", Shape::Text)];

/// The unmodeled `input` item kinds Bedrock Mantle may receive, each with
/// the members it may carry. The module doc records the review of each.
pub(super) const MANTLE_PASSTHROUGH_KINDS: &[ReviewedKind] = &[
    ReviewedKind {
        tag: "agent_message",
        members: &[
            ("id", Shape::Text),
            ("author", Shape::Text),
            ("recipient", Shape::Text),
            ("content", Shape::Objects(AGENT_MESSAGE_CONTENT)),
        ],
    },
    ReviewedKind {
        tag: "local_shell_call",
        members: &[
            ("id", Shape::Text),
            ("call_id", Shape::Text),
            ("status", Shape::Text),
            ("action", Shape::Object(LOCAL_SHELL_ACTION)),
        ],
    },
    ReviewedKind {
        tag: "compaction",
        members: COMPACTION_MEMBERS,
    },
    ReviewedKind {
        tag: "compaction_summary",
        members: COMPACTION_MEMBERS,
    },
    ReviewedKind {
        tag: "context_compaction",
        members: COMPACTION_MEMBERS,
    },
];

impl Shape {
    fn admits(self, value: &Value) -> bool {
        match (self, value) {
            (_, Value::Null)
            | (Self::Text, Value::String(_))
            | (Self::Number, Value::Number(_)) => true,
            (Self::Texts, Value::Array(items)) => items.iter().all(Value::is_string),
            (Self::TextMap, Value::Object(members)) => members.values().all(Value::is_string),
            (Self::Objects(kinds), Value::Array(items)) => {
                items.iter().all(|item| is_reviewed(kinds, item))
            }
            (Self::Object(kinds), Value::Object(_)) => is_reviewed(kinds, value),
            _ => false,
        }
    }
}

/// Whether `value` is an object of one of `kinds` carrying only reviewed
/// members, each of its reviewed shape.
fn is_reviewed(kinds: &[ReviewedKind], value: &Value) -> bool {
    let Some(members) = value.as_object() else {
        return false;
    };
    let tag = members.get("type").and_then(Value::as_str);
    let Some(kind) = kinds.iter().find(|kind| Some(kind.tag) == tag) else {
        return false;
    };
    members
        .iter()
        .filter(|(key, _)| key.as_str() != "type")
        .all(|(key, value)| kind.admits(key, value))
}

impl ReviewedKind {
    fn admits(&self, key: &str, value: &Value) -> bool {
        self.members
            .iter()
            .find(|(name, _)| *name == key)
            .is_some_and(|(_, shape)| shape.admits(value))
    }
}

/// Whether a preserved `input` item forwards on the lane `auth_kind`: on
/// every lane but Bedrock Mantle, and there only when it is reviewed.
pub(super) fn forwards_item(auth_kind: AuthKind, item: &Value) -> bool {
    auth_kind != AuthKind::BedrockMantle || is_reviewed(MANTLE_PASSTHROUGH_KINDS, item)
}

/// One preserved `input` item that forwards, with its position among all of
/// the request's preserved items.
#[derive(Debug, Clone, Copy)]
pub(super) struct ForwardedItem<'a> {
    pub(super) ordinal: usize,
    pub(super) item: &'a ResponsesPassthroughItem,
}

/// The preserved items that forward on `auth_kind`, in inbound order. Each
/// one withheld is declared as a Remove at its inbound `input[]` index, and
/// the request is recorded once on `withheld`.
/// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=mantle_withholds_unreviewed_passthrough_items_declares_each_and_counts_once
pub(super) fn forwarded_passthrough<'a>(
    id: &str,
    auth_kind: AuthKind,
    passthrough: &'a [ResponsesPassthroughItem],
    tap: Option<&WireTap>,
    withheld: &mut UnknownMemberWithheldTally,
) -> Vec<ForwardedItem<'a>> {
    let (forwarded, omitted): (Vec<ForwardedItem<'a>>, Vec<ForwardedItem<'a>>) = passthrough
        .iter()
        .enumerate()
        .map(|(ordinal, item)| ForwardedItem { ordinal, item })
        .partition(|f| forwards_item(auth_kind, &f.item.item));
    if omitted.is_empty() {
        return forwarded;
    }
    withheld.record();
    tracing::debug!(
        provider = id,
        items = omitted.len(),
        "openai-responses egress: omitting unreviewed passthrough input items on bedrock-mantle"
    );
    if let Some(tap) = tap {
        for f in &omitted {
            let inbound = f.item.modeled_prefix.saturating_add(f.ordinal);
            declare(
                tap,
                EditSite::AuthSurfaceGate,
                format!("/input/{inbound}"),
                EditKind::Remove,
            );
        }
    }
    forwarded
}

/// On Bedrock Mantle, which receives no namespace container, leave off the
/// `namespace` the Responses ingress took off each call to a member of one:
/// such a call names a tool Mantle receives flat. Declared once as a Remove
/// on the calls' `namespace` member. Returns whether the lane takes the
/// stamp, so the caller restamps only then.
/// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=mantle_withholds_the_namespace_of_a_call_to_a_flattened_member
pub(super) fn takes_namespace_stamps(
    id: &str,
    auth_kind: AuthKind,
    items: &[ResponseInputItem],
    tool_namespaces: &BTreeMap<String, String>,
    tap: Option<&WireTap>,
    withheld: &mut UnknownMemberWithheldTally,
) -> bool {
    if auth_kind != AuthKind::BedrockMantle {
        return true;
    }
    let calls_a_member = items.iter().any(|item| {
        matches!(item, ResponseInputItem::FunctionCall { name, .. }
            if tool_namespaces.contains_key(name.as_str()))
    });
    if calls_a_member {
        withheld.record();
        tracing::debug!(
            provider = id,
            "openai-responses egress: omitting call namespaces on bedrock-mantle"
        );
        if let Some(tap) = tap {
            declare(
                tap,
                EditSite::AuthSurfaceGate,
                "/input/*/namespace",
                EditKind::Remove,
            );
        }
    }
    false
}

/// Pointer pattern of a member carried on an `input` item.
pub(super) const ITEM_MEMBER: &str = "/input/*";
/// Pointer pattern of a member carried on a content part of an `input` item.
pub(super) const PART_MEMBER: &str = "/input/*/content/*";
/// Pointer pattern of a member carried on an item of a tool output.
pub(super) const TOOL_OUTPUT_PART_MEMBER: &str = "/input/*/output/*";

/// The members carried on modeled input nodes that one request's input
/// build left off the Bedrock Mantle wire. Shared by the build's per-node
/// calls, which see the input policy only by value.
#[derive(Debug, Default)]
pub(super) struct WithheldCarriers {
    withheld: Cell<bool>,
    pointers: RefCell<BTreeSet<String>>,
}

impl WithheldCarriers {
    /// Note each of `keys` withheld from a node at `parent`, a pointer
    /// pattern. A key that is not token-shaped is left undeclared, so it
    /// still surfaces as a finding; the request is recorded either way.
    pub(super) fn note<'k>(&self, parent: &str, keys: impl Iterator<Item = &'k String>) {
        let mut pointers = self.pointers.borrow_mut();
        for key in keys {
            self.withheld.set(true);
            pointers.extend(member_pointer(parent, key));
        }
    }

    /// Declare each withheld member as a Remove and record the request on
    /// `tally`, once.
    /// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=mantle_withholds_message_and_part_members_declares_each_and_counts_once
    pub(super) fn flush(
        self,
        id: &str,
        tap: Option<&WireTap>,
        tally: &mut UnknownMemberWithheldTally,
    ) {
        if !self.withheld.get() {
            return;
        }
        tally.record();
        let pointers = self.pointers.into_inner();
        tracing::debug!(
            provider = id,
            members = pointers.len(),
            "openai-responses egress: omitting carried item and part members on bedrock-mantle"
        );
        if let Some(tap) = tap {
            for pointer in pointers {
                declare(tap, EditSite::AuthSurfaceGate, pointer, EditKind::Remove);
            }
        }
    }
}

/// A `prompt_cache_retention` value captured as accepted on this lane.
#[derive(Debug, Clone, Copy)]
enum CacheRetention {
    InMemory,
    Hours24,
}

impl CacheRetention {
    fn parse(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "in_memory" => Some(Self::InMemory),
            "24h" => Some(Self::Hours24),
            _ => None,
        }
    }

    const fn literal(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Hours24 => "24h",
        }
    }
}

/// A `prompt_cache_options.mode` captured as accepted on this lane.
#[derive(Debug, Clone, Copy)]
enum CacheMode {
    Explicit,
}

impl CacheMode {
    fn parse(value: &Value) -> Option<Self> {
        (value.as_str()? == "explicit").then_some(Self::Explicit)
    }

    const fn literal(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
        }
    }
}

/// A `prompt_cache_options.ttl` this lane echoed back as its own.
#[derive(Debug, Clone, Copy)]
enum CacheTtl {
    Minutes30,
}

impl CacheTtl {
    fn parse(value: &Value) -> Option<Self> {
        (value.as_str()? == "30m").then_some(Self::Minutes30)
    }

    const fn literal(self) -> &'static str {
        match self {
            Self::Minutes30 => "30m",
        }
    }
}

/// A `prompt_cache_options` object of the captured layout: `mode` required,
/// `ttl` optional, no other member.
#[derive(Debug, Clone, Copy)]
struct CacheOptions {
    mode: CacheMode,
    ttl: Option<CacheTtl>,
}

impl CacheOptions {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        if object.keys().any(|key| key != "mode" && key != "ttl") {
            return None;
        }
        let mode = CacheMode::parse(object.get("mode")?)?;
        let ttl = match object.get("ttl") {
            Some(ttl) => Some(CacheTtl::parse(ttl)?),
            None => None,
        };
        Some(Self { mode, ttl })
    }

    fn to_value(self) -> Value {
        let mut object = Map::new();
        object.insert("mode".into(), Value::from(self.mode.literal()));
        if let Some(ttl) = self.ttl {
            object.insert("ttl".into(), Value::from(ttl.literal()));
        }
        Value::Object(object)
    }
}

/// The Bedrock Mantle wire value of a client prompt-cache `field`: the
/// captured shape `value` parses into, built from static literals so no
/// client JSON reaches the signed request. `None` for any other value, and
/// for a field this lane takes no value of.
pub(super) fn admit_cache_value(field: PromptCacheField, value: &Value) -> Option<Value> {
    match field {
        PromptCacheField::Options => CacheOptions::parse(value).map(CacheOptions::to_value),
        PromptCacheField::Retention => {
            CacheRetention::parse(value).map(|retention| Value::from(retention.literal()))
        }
        PromptCacheField::Breakpoint | PromptCacheField::SessionKeyFallback => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AuthKind, PromptCacheField, admit_cache_value, forwards_item};

    fn forwards_on_mantle(item: &serde_json::Value) -> bool {
        forwards_item(AuthKind::BedrockMantle, item)
    }

    #[test]
    fn reviewed_kinds_in_their_codex_shape_forward_on_mantle() {
        let rows = [
            (
                "agent_message",
                json!({"type": "agent_message", "id": "am_1", "author": "a", "recipient": "b",
                       "content": [{"type": "input_text", "text": "caf\u{e9}"},
                                   {"type": "encrypted_content", "encrypted_content": "x"}]}),
            ),
            (
                "local_shell_call",
                json!({"type": "local_shell_call", "id": "ls_1", "call_id": "c1",
                       "status": "completed",
                       "action": {"type": "exec", "command": ["ls", "-la"], "timeout_ms": 1000,
                                  "working_directory": "/w", "env": {"A": "1"}, "user": null}}),
            ),
            (
                "compaction",
                json!({"type": "compaction", "id": "cp_1", "encrypted_content": "x"}),
            ),
            (
                "compaction_summary",
                json!({"type": "compaction_summary", "encrypted_content": "x"}),
            ),
            (
                "context_compaction",
                json!({"type": "context_compaction", "encrypted_content": null}),
            ),
        ];
        for (row, item) in rows {
            assert!(forwards_on_mantle(&item), "{row}: {item}");
        }
    }

    #[test]
    fn anything_outside_the_review_is_withheld_on_mantle() {
        let rows = [
            (
                "unreviewed member",
                json!({"type": "agent_message", "content": [],
                       "source": {"type": "s3", "uri": "s3://b/k"}}),
            ),
            (
                "unreviewed part kind",
                json!({"type": "agent_message",
                       "content": [{"type": "input_image", "image_url": "s3://b/k"}]}),
            ),
            (
                "unreviewed part member",
                json!({"type": "agent_message",
                       "content": [{"type": "input_text", "text": "t", "file_id": "f"}]}),
            ),
            (
                "member of the wrong shape",
                json!({"type": "agent_message", "author": {"url": "s3://b/k"}}),
            ),
            (
                "nested action member",
                json!({"type": "local_shell_call",
                       "action": {"type": "exec", "command": [], "url": "s3://b/k"}}),
            ),
            (
                "command element not a string",
                json!({"type": "local_shell_call",
                       "action": {"type": "exec", "command": [{"image_url": "s3://b/k"}]}}),
            ),
            (
                "unreviewed action kind",
                json!({"type": "local_shell_call", "action": {"type": "fetch"}}),
            ),
            (
                "internal metadata",
                json!({"type": "compaction", "encrypted_content": "x",
                       "internal_chat_message_metadata_passthrough": {"turn_id": "t"}}),
            ),
            (
                "upper-case member",
                json!({"type": "computer_call_output", "call_id": "c",
                       "output": {"IMAGE_URL": "s3://b/k"}}),
            ),
            (
                "tool output",
                json!({"type": "function_call_output", "call_id": "c", "output": "ok"}),
            ),
            (
                "custom tool output",
                json!({"type": "custom_tool_call_output", "call_id": "c", "output": "ok"}),
            ),
            (
                "tool declarations",
                json!({"type": "additional_tools", "tools": []}),
            ),
            (
                "web search",
                json!({"type": "web_search_call",
                       "action": {"type": "open_page", "url": "https://e.com"}}),
            ),
            (
                "mis-cased kind",
                json!({"type": "Compaction", "encrypted_content": "x"}),
            ),
            ("no kind", json!({"encrypted_content": "x"})),
            ("not an object", json!("agent_message")),
        ];
        for (row, item) in rows {
            assert!(!forwards_on_mantle(&item), "{row}: {item}");
            assert!(forwards_item(AuthKind::ApiKey, &item), "{row}");
            assert!(forwards_item(AuthKind::ChatgptOauth, &item), "{row}");
        }
    }

    #[test]
    fn captured_cache_values_are_admitted_value_equal() {
        let rows = [
            (
                "retention in_memory",
                PromptCacheField::Retention,
                json!("in_memory"),
            ),
            ("retention 24h", PromptCacheField::Retention, json!("24h")),
            (
                "options mode",
                PromptCacheField::Options,
                json!({"mode": "explicit"}),
            ),
            (
                "options echoed ttl",
                PromptCacheField::Options,
                json!({"mode": "explicit", "ttl": "30m"}),
            ),
            (
                "options ttl first",
                PromptCacheField::Options,
                json!({"ttl": "30m", "mode": "explicit"}),
            ),
        ];
        for (row, field, value) in rows {
            assert_eq!(admit_cache_value(field, &value), Some(value), "{row}");
        }
    }

    #[test]
    fn uncaptured_cache_values_are_refused() {
        let rows = [
            (
                "retention url",
                PromptCacheField::Retention,
                json!("https://h/x"),
            ),
            (
                "retention case variant",
                PromptCacheField::Retention,
                json!("24H"),
            ),
            (
                "retention uncaptured",
                PromptCacheField::Retention,
                json!("12h"),
            ),
            ("retention number", PromptCacheField::Retention, json!(24)),
            ("retention null", PromptCacheField::Retention, json!(null)),
            (
                "retention object",
                PromptCacheField::Retention,
                json!({"url": "s3://b/k"}),
            ),
            (
                "options string",
                PromptCacheField::Options,
                json!("explicit"),
            ),
            ("options empty", PromptCacheField::Options, json!({})),
            ("options null", PromptCacheField::Options, json!(null)),
            (
                "options implicit",
                PromptCacheField::Options,
                json!({"mode": "implicit"}),
            ),
            (
                "options mode array",
                PromptCacheField::Options,
                json!({"mode": ["explicit"]}),
            ),
            (
                "options null ttl",
                PromptCacheField::Options,
                json!({"mode": "explicit", "ttl": null}),
            ),
            (
                "options uncaptured ttl",
                PromptCacheField::Options,
                json!({"mode": "explicit", "ttl": "12h"}),
            ),
            (
                "options file member",
                PromptCacheField::Options,
                json!({"mode": "explicit", "file_id": "file-1"}),
            ),
            (
                "options prewarm",
                PromptCacheField::Options,
                json!({"mode": "explicit", "prewarm": "https://x"}),
            ),
            (
                "options ttl only",
                PromptCacheField::Options,
                json!({"ttl": "30m"}),
            ),
            ("breakpoint", PromptCacheField::Breakpoint, json!(true)),
            (
                "session key",
                PromptCacheField::SessionKeyFallback,
                json!("k"),
            ),
        ];
        for (row, field, value) in rows {
            assert_eq!(admit_cache_value(field, &value), None, "{row}: {value}");
        }
    }
}
