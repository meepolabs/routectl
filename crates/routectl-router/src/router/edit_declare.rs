//! Router-side declarations of the request-body edits routectl makes on
//! purpose. Each transform pushes a [`DeclaredEdit`] onto the attempt's
//! `declared_edits` at the site that makes the edit, so the live wire check
//! can tell a deliberate change from a loss.
//!
//! A declaration names the edit's pointer in the egress wire dialect and is
//! made only on a fidelity lane (the client's dialect is the egress's): on a
//! translation lane the client and egress pointers do not correspond, and the
//! check compares no paths a router edit could explain.

use std::borrow::Cow;

use routectl_core::wire_check::{
    DeclaredEdit, EditKind, EditSite, IngressDialect, Lane, LaneClass, WirePath,
};
use routectl_core::{
    ChatRequest, ContentPart, FrontSlot, KnownContentPart, Message, MessageContent,
    is_canonical_request_key,
};
use serde_json::{Map, Value};

use super::DispatchTarget;
use super::cache_plan::{CacheDecision, CacheInjection};
use super::field_repair::FieldSurface;

/// The wire dialect an attempt on `target` shares with its client, or `None`
/// when the lane translates or either end is unknown.
pub(super) fn fidelity_dialect(
    target: &DispatchTarget,
    req: &ChatRequest,
) -> Option<IngressDialect> {
    let ingress = req.routectl_internal.ingress_wire.as_ref()?.dialect;
    let lane = Lane::of(ingress, target.egress_kind?);
    (lane.class() == LaneClass::Fidelity).then_some(ingress)
}

/// Push one declaration onto `req`. A pattern the edit register refuses is
/// dropped with a DEBUG naming only the site: the unexplained difference then
/// surfaces as a finding, which is the safe direction.
pub(super) fn declare(
    req: &mut ChatRequest,
    site: EditSite,
    path: impl Into<Cow<'static, str>>,
    kind: EditKind,
) {
    match DeclaredEdit::new(site, path, kind) {
        Ok(edit) => req.routectl_internal.declared_edits.push(edit),
        Err(e) => tracing::debug!(
            site = site.label(),
            error = %e,
            "wire-check edit declaration refused"
        ),
    }
}

/// Declare the rename of the client's `client_model` to the alias target's
/// upstream id already written on `req`. Nothing is declared when the two
/// are equal: the wire `model` did not change.
pub(super) fn declare_model_rename(
    req: &mut ChatRequest,
    target: &DispatchTarget,
    client_model: &str,
) {
    if req.model != client_model && fidelity_dialect(target, req).is_some() {
        declare(req, EditSite::ModelRename, "/model", EditKind::Rewrite);
    }
}

/// The edits merging `operator` payload extras over the client's swept
/// extras makes: an Add where the client sent nothing at a leaf, a Rewrite
/// where it sent a different value. Objects on both sides merge key by key,
/// as the merge itself does. Canonical top-level keys are skipped: every
/// egress withholds them from the extras merge, so they never reach the wire.
pub(super) fn operator_extras_edits(
    wire: Option<IngressDialect>,
    client: Option<&Value>,
    operator: Option<&Value>,
) -> Vec<DeclaredEdit> {
    let (Some(_), Some(Value::Object(operator))) = (wire, operator) else {
        return Vec::new();
    };
    let client = client.and_then(Value::as_object);
    let mut leaves = Vec::new();
    for (key, value) in operator {
        if !is_canonical_request_key(key) {
            collect_extras_leaf(client, key, value, &WirePath::root(), &mut leaves);
        }
    }
    leaves
        .into_iter()
        .filter_map(|(path, kind)| {
            DeclaredEdit::new(EditSite::OperatorPayloadExtras, path, kind).ok()
        })
        .collect()
}

fn collect_extras_leaf(
    client: Option<&Map<String, Value>>,
    key: &str,
    value: &Value,
    parent: &WirePath,
    out: &mut Vec<(String, EditKind)>,
) {
    let path = parent.child(key);
    if path.is_coarsened() {
        return;
    }
    match (client.and_then(|c| c.get(key)), value) {
        (Some(Value::Object(existing)), Value::Object(operator)) => {
            for (child, child_value) in operator {
                collect_extras_leaf(Some(existing), child, child_value, &path, out);
            }
        }
        (Some(existing), _) if existing == value => {}
        (Some(_), _) => out.push((path.as_str().to_owned(), EditKind::Rewrite)),
        (None, _) => out.push((path.as_str().to_owned(), EditKind::Add)),
    }
}

/// Declare every string context reduction compacted, comparing the message
/// list it started from with the one it left on `req`.
///
/// The Anthropic wire carries messages and content blocks at the canonical
/// indices, so those pointers are concrete. The Chat ingress lifts system
/// messages out of the list and the Responses wire interleaves other input
/// items, so a tool call's message position is a wildcard there.
pub(super) fn declare_reduction(
    req: &mut ChatRequest,
    wire: Option<IngressDialect>,
    before: &[Message],
) {
    let Some(wire) = wire else {
        return;
    };
    let mut pointers = Vec::new();
    for (index, (old, new)) in before.iter().zip(req.messages.iter()).enumerate() {
        if wire == IngressDialect::Anthropic {
            reduced_part_pointers(index, old, new, &mut pointers);
        }
        reduced_call_pointers(wire, old, new, &mut pointers);
    }
    for pointer in pointers {
        declare(req, EditSite::ContextReduction, pointer, EditKind::Rewrite);
    }
}

fn reduced_part_pointers(index: usize, old: &Message, new: &Message, out: &mut Vec<String>) {
    let (MessageContent::Parts(old), MessageContent::Parts(new)) = (&old.content, &new.content)
    else {
        return;
    };
    for (block, (a, b)) in old.iter().zip(new).enumerate() {
        let field = match (a, b) {
            (
                ContentPart::Known(KnownContentPart::ToolResult { content: a, .. }),
                ContentPart::Known(KnownContentPart::ToolResult { content: b, .. }),
            ) if a != b => "content",
            (
                ContentPart::Known(KnownContentPart::ToolUse { input: a, .. }),
                ContentPart::Known(KnownContentPart::ToolUse { input: b, .. }),
            ) if a != b => "input",
            _ => continue,
        };
        out.push(format!("/messages/{index}/content/{block}/{field}"));
    }
}

fn reduced_call_pointers(
    wire: IngressDialect,
    old: &Message,
    new: &Message,
    out: &mut Vec<String>,
) {
    let (Some(old), Some(new)) = (old.tool_calls.as_ref(), new.tool_calls.as_ref()) else {
        return;
    };
    for (call, (a, b)) in old.iter().zip(new).enumerate() {
        if call_arguments(a) == call_arguments(b) {
            continue;
        }
        match wire {
            IngressDialect::Chat => {
                out.push(format!("/messages/*/tool_calls/{call}/function/arguments"));
            }
            IngressDialect::Responses => out.push("/input/*/arguments".to_owned()),
            IngressDialect::Anthropic => {}
        }
    }
}

fn call_arguments(call: &Value) -> Option<&Value> {
    call.get("function")?.get("arguments")
}

/// Declare the cache markers placement committed. Only the Anthropic wire
/// carries a `cache_control` marker; the other dialects' egresses drop it.
pub(super) fn declare_cache_placement(
    req: &mut ChatRequest,
    target: &DispatchTarget,
    decision: CacheDecision,
    front_slot: Option<FrontSlot>,
) {
    if fidelity_dialect(target, req) != Some(IngressDialect::Anthropic) {
        return;
    }
    if decision.front == CacheInjection::Emitted {
        let pointer = match front_slot {
            Some(FrontSlot::LastSystemBlock { block_index }) => {
                Some(format!("/system/{block_index}/cache_control"))
            }
            Some(FrontSlot::LastCustomTool { tool_index }) => {
                Some(format!("/tools/{tool_index}/cache_control"))
            }
            None => None,
        };
        if let Some(pointer) = pointer {
            declare(req, EditSite::CachePlacement, pointer, EditKind::Add);
        }
    }
    if decision.terminal == CacheInjection::Emitted {
        declare(
            req,
            EditSite::CachePlacement,
            "/cache_control",
            EditKind::Add,
        );
    }
}

/// The wire pointer of a closed-table field surface in `wire`, or `None`
/// when the surface has no pointer this dialect carries.
pub(super) const fn field_wire_pointer(
    surface: FieldSurface,
    wire: IngressDialect,
) -> Option<&'static str> {
    match (surface, wire) {
        (FieldSurface::AnthropicThinkingDisplay, IngressDialect::Anthropic) => {
            Some("/thinking/display")
        }
        (FieldSurface::AnthropicThinkingType(_), IngressDialect::Anthropic) => {
            Some("/thinking/type")
        }
        _ => None,
    }
}

/// Declare a field surface dropped (`Remove`) or rewritten (`Rewrite`).
pub(super) fn declare_field_edit(
    req: &mut ChatRequest,
    wire: Option<IngressDialect>,
    site: EditSite,
    surface: FieldSurface,
    kind: EditKind,
) {
    let Some(pointer) = wire.and_then(|wire| field_wire_pointer(surface, wire)) else {
        return;
    };
    declare(req, site, pointer, kind);
    if matches!(surface, FieldSurface::AnthropicThinkingType(_)) && kind == EditKind::Rewrite {
        declare_thinking_type_rewrite(req, site);
    }
}

/// Declare what a `thinking.type` rewrite changed beyond the type, against
/// the client's own body: each member of the client's `thinking` the
/// rewritten object no longer carries, and a client `output_config.effort`
/// the companion effort replaced. An effort the client did not send is
/// declared by the egress, which compares the merged `output_config` with
/// the client's.
fn declare_thinking_type_rewrite(req: &mut ChatRequest, site: EditSite) {
    let Some(client) = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .and_then(|wire| serde_json::from_slice::<Value>(&wire.bytes).ok())
    else {
        return;
    };
    let kept = rewritten_thinking_keys(req);
    let thinking = WirePath::root().child("thinking");
    let removed: Vec<String> = client
        .get("thinking")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::keys)
        .filter(|key| !kept.contains(&key.as_str()))
        .map(|key| thinking.child(key).as_str().to_owned())
        .collect();
    for pointer in removed {
        declare(req, site, pointer, EditKind::Remove);
    }
    let client_effort = client.get("output_config").and_then(|c| c.get("effort"));
    let sent_effort = req
        .provider_extras
        .as_ref()
        .and_then(|extras| extras.get("output_config"))
        .and_then(|config| config.get("effort"));
    if client_effort.is_some() && client_effort != sent_effort {
        declare(req, site, "/output_config/effort", EditKind::Rewrite);
    }
}

/// The `thinking` members the rewritten carrier emits: its own keys, plus
/// `display` while its own carrier holds one.
fn rewritten_thinking_keys(req: &ChatRequest) -> Vec<&str> {
    let mut keys: Vec<&str> = match &req.routectl_internal.anthropic_thinking {
        routectl_core::AnthropicThinking::Sent(object) => {
            object.as_object().keys().map(String::as_str).collect()
        }
        _ => Vec::new(),
    };
    if req.routectl_internal.anthropic_thinking_display.is_some() {
        keys.push("display");
    }
    keys
}

/// The ids whose every reasoning detail a replay strip is about to remove,
/// when the client spoke the Responses dialect. Only lanes with a replay
/// scheme strip, and every such lane sends the Responses wire, so no other
/// ingress shares a pointer with the stripped items. An id that keeps one of
/// its details is rewritten rather than removed, and is not returned.
pub(super) fn replay_strip_ids(
    req: &ChatRequest,
    removable: impl Fn(&routectl_core::ReasoningDetail) -> bool,
) -> Vec<String> {
    let is_responses = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .is_some_and(|wire| wire.dialect == IngressDialect::Responses);
    if !is_responses {
        return Vec::new();
    }
    let details = || {
        req.messages
            .iter()
            .flat_map(|message| message.reasoning_details.iter())
    };
    let mut ids: Vec<String> = Vec::new();
    for detail in details().filter(|detail| removable(detail)) {
        let Some(id) = detail.id.as_deref() else {
            continue;
        };
        let wholly_removed = details()
            .filter(|other| other.id.as_deref() == Some(id))
            .all(&removable);
        if wholly_removed && !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// Declare each client `reasoning` input item whose id the strip removed.
/// The egress groups details by id, so removing every detail of an item
/// removes the item; the client's own body names its position.
pub(super) fn declare_replay_strip(req: &mut ChatRequest, removed_ids: &[String]) {
    if removed_ids.is_empty() {
        return;
    }
    let Some(body) = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .and_then(|wire| serde_json::from_slice::<Value>(&wire.bytes).ok())
    else {
        return;
    };
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return;
    };
    let positions: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        .filter(|(_, item)| {
            item.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| removed_ids.iter().any(|removed| removed == id))
        })
        .map(|(position, _)| position)
        .collect();
    for position in positions {
        declare(
            req,
            EditSite::ReplayStrip,
            format!("/input/{position}"),
            EditKind::Remove,
        );
    }
}

/// What a learned capability strip may remove, captured before it runs: the
/// serialized tools and the top-level extras keys.
pub(super) struct StripBefore {
    tools: Vec<Value>,
    extras_keys: Vec<String>,
}

/// Capture [`StripBefore`] when the attempt shares the Anthropic wire with its
/// client, the one dialect whose `tools` and top-level body keys sit at the
/// canonical positions.
pub(super) fn strip_before(target: &DispatchTarget, req: &ChatRequest) -> Option<StripBefore> {
    if fidelity_dialect(target, req) != Some(IngressDialect::Anthropic) {
        return None;
    }
    Some(StripBefore {
        tools: tool_values(req),
        extras_keys: req
            .provider_extras
            .as_ref()
            .and_then(Value::as_object)
            .map(|extras| extras.keys().cloned().collect())
            .unwrap_or_default(),
    })
}

fn tool_values(req: &ChatRequest) -> Vec<Value> {
    req.tools
        .iter()
        .flatten()
        .map(|tool| serde_json::to_value(tool).unwrap_or(Value::Null))
        .collect()
}

/// Declare what a learned capability strip removed: each dropped tool by
/// its index, each dropped top-level extras key by name. The strip keeps the
/// surviving tools in order, so a walk over both lists finds the removed
/// indices.
pub(super) fn declare_learned_strip(req: &mut ChatRequest, before: &StripBefore) {
    let after = tool_values(req);
    let mut kept = after.iter().peekable();
    let mut removed_tools = Vec::new();
    for (index, tool) in before.tools.iter().enumerate() {
        if kept.peek() == Some(&tool) {
            kept.next();
        } else {
            removed_tools.push(index);
        }
    }
    let removed_keys: Vec<String> = before
        .extras_keys
        .iter()
        .filter(|key| {
            !req.provider_extras
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|extras| extras.contains_key(key.as_str()))
        })
        .map(|key| WirePath::root().child(key).as_str().to_owned())
        .collect();
    for index in removed_tools {
        declare(
            req,
            EditSite::LearnedRepair,
            format!("/tools/{index}"),
            EditKind::Remove,
        );
    }
    for pointer in removed_keys {
        declare(req, EditSite::LearnedRepair, pointer, EditKind::Remove);
    }
}

#[cfg(test)]
#[path = "edit_declare_tests.rs"]
mod edit_declare_tests;
