//! Responses tool declarations -> canonical `ToolDef`s, with namespace
//! containers flattened.
//!
//! A Responses client may group tools under a `type:"namespace"` container,
//! in the top-level `tools` array or inside an `additional_tools` input item.
//! No other dialect has that grouping, so each member is lifted into
//! `ChatRequest.tools` under its bare name and the container is recorded on
//! `RoutectlInternal::responses_namespaces`. A same-dialect Responses egress
//! reads that record to send the containers back as written and to keep
//! from declaring a member twice.
//!
//! Per-member rule, by carrier:
//! - top-level `tools`: the flat-tool rule; a function becomes
//!   `ToolDef::Custom`, anything else `ToolDef::Other` verbatim.
//! - `additional_tools` item: a function becomes `ToolDef::Custom` and a
//!   freeform `type:"custom"` tool `ToolDef::Other`; hosted, MCP and unknown
//!   members stay out of canonical tools, because they may carry server URLs
//!   or credentials. The item itself still replays verbatim.
//!
//! A member whose bare name is also declared flat, or by another member, is
//! not flattened: the flat tool is kept, the clash is logged once per request
//! and counted as the `namespace_tool_name_collision` drop class.

use std::collections::{BTreeMap, BTreeSet};

use routectl_core::schema::{
    ResponsesNamespaceCollision, ResponsesNamespaceContainer, ResponsesNamespaces,
    ResponsesUnnamedMember,
};
use routectl_core::tool_def::responses_namespace_members;
use routectl_core::{ChatRequest, CustomTool, ToolDef, sanitize_for_log};
use routectl_providers::translation_drop_metrics::record_translation_drop;
use serde_json::Value;

/// Census lane the collision class is counted on.
const LANE: &str = routectl_core::provider_kind::trace::OPENAI_RESPONSES;

/// Drop class for namespace members left out of canonical tools because
/// their bare name is declared more than once.
const NAMESPACE_TOOL_NAME_COLLISION: &str = "namespace_tool_name_collision";

/// A freeform (grammar) tool declaration; has no JSON schema, so it rides
/// canonical tools as `ToolDef::Other`.
const RESPONSES_CUSTOM_TOOL_TYPE: &str = "custom";

/// Which request field a declaration arrived in; decides the member rule.
#[derive(Clone, Copy)]
enum Carrier {
    TopLevel,
    AdditionalTools,
}

struct Member<'a> {
    namespace: &'a str,
    raw: &'a Value,
    def: ToolDef,
}

enum Slot<'a> {
    Flat(ToolDef),
    Member(Member<'a>),
}

/// Canonical tools for a request's top-level `tools` value and the
/// declarations its `additional_tools` items carry, plus the namespace
/// record. Counts and logs any name collision.
pub(super) fn build_tools(
    top_level: Option<&Value>,
    additional: &[Value],
) -> (Option<Vec<ToolDef>>, ResponsesNamespaces) {
    let mut namespaces = ResponsesNamespaces::default();
    let mut slots = top_level_slots(top_level, &mut namespaces.containers);
    merge_additional(&mut slots, additional);
    let tools = resolve(slots, &mut namespaces);
    report_collisions(&namespaces.collisions);
    (tools, namespaces)
}

/// Note the collision class on the request's observation slot, so the usage
/// row counts it. The parse ran before the slot existed; call this right
/// after the slot is installed.
pub fn note_collisions_on_observation(req: &ChatRequest) {
    let internal = &req.routectl_internal;
    if internal.responses_namespaces.collisions.is_empty() {
        return;
    }
    if let Some(observation) = internal.observation.as_ref() {
        observation.note_translation_drop(LANE, NAMESPACE_TOOL_NAME_COLLISION);
    }
}

fn top_level_slots<'a>(
    top_level: Option<&'a Value>,
    containers: &mut Vec<ResponsesNamespaceContainer>,
) -> Vec<Slot<'a>> {
    let mut slots = Vec::new();
    let entries = top_level.and_then(Value::as_array).into_iter().flatten();
    for (index, entry) in entries.enumerate() {
        if push_members(&mut slots, entry, Carrier::TopLevel) {
            containers.push(ResponsesNamespaceContainer {
                index,
                raw: entry.clone(),
            });
        } else {
            slots.push(Slot::Flat(flat_tool(entry)));
        }
    }
    slots
}

/// Merge `additional_tools` declarations in input order. A flat function
/// replaces an earlier flat function of the same name in place, because the
/// same-dialect replay emits the inline item verbatim and every other lane
/// must route and serialize that same definition. Flat non-functions stay
/// out of canonical tools (see the module docs).
fn merge_additional<'a>(slots: &mut Vec<Slot<'a>>, additional: &'a [Value]) {
    for entry in additional {
        if push_members(slots, entry, Carrier::AdditionalTools) {
            continue;
        }
        let Some(candidate) = CustomTool::from_responses_function(entry) else {
            continue;
        };
        let known = slots.iter().position(
            |slot| matches!(slot, Slot::Flat(ToolDef::Custom(k)) if k.name == candidate.name),
        );
        match known {
            Some(at) => slots[at] = Slot::Flat(ToolDef::Custom(candidate)),
            None => slots.push(Slot::Flat(ToolDef::Custom(candidate))),
        }
    }
}

/// Push the members of `entry` if it is a namespace container; `false` when
/// it is not one.
fn push_members<'a>(slots: &mut Vec<Slot<'a>>, entry: &'a Value, carrier: Carrier) -> bool {
    let Some((namespace, members)) = responses_namespace_members(entry) else {
        return false;
    };
    for raw in members {
        if let Some(def) = member_tool(raw, carrier) {
            slots.push(Slot::Member(Member {
                namespace,
                raw,
                def,
            }));
        }
    }
    true
}

fn flat_tool(tool: &Value) -> ToolDef {
    CustomTool::from_responses_function(tool)
        .map_or_else(|| ToolDef::Other(tool.clone()), ToolDef::Custom)
}

fn member_tool(raw: &Value, carrier: Carrier) -> Option<ToolDef> {
    match carrier {
        Carrier::TopLevel => Some(flat_tool(raw)),
        Carrier::AdditionalTools => CustomTool::from_responses_function(raw)
            .map(ToolDef::Custom)
            .or_else(|| {
                let is_custom =
                    raw.get("type").and_then(Value::as_str) == Some(RESPONSES_CUSTOM_TOOL_TYPE);
                is_custom.then(|| ToolDef::Other(raw.clone()))
            }),
    }
}

fn tool_name(def: &ToolDef) -> Option<&str> {
    match def {
        ToolDef::Custom(c) => Some(c.name.as_str()),
        ToolDef::Other(v) => v.get("name").and_then(Value::as_str),
    }
}

/// Names that more than one declaration claims: any member name a flat tool
/// also declares, or that two members declare.
fn clashing_names(slots: &[Slot<'_>]) -> BTreeSet<String> {
    let mut flat: BTreeSet<&str> = BTreeSet::new();
    let mut members: BTreeMap<&str, usize> = BTreeMap::new();
    for slot in slots {
        match slot {
            Slot::Flat(def) => flat.extend(tool_name(def)),
            Slot::Member(m) => {
                if let Some(name) = tool_name(&m.def) {
                    *members.entry(name).or_default() += 1;
                }
            }
        }
    }
    members
        .into_iter()
        .filter(|(name, count)| *count > 1 || flat.contains(name))
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn resolve(slots: Vec<Slot<'_>>, namespaces: &mut ResponsesNamespaces) -> Option<Vec<ToolDef>> {
    let clashing = clashing_names(&slots);
    let mut tools = Vec::with_capacity(slots.len());
    for slot in slots {
        let member = match slot {
            Slot::Flat(def) => {
                tools.push(def);
                continue;
            }
            Slot::Member(member) => member,
        };
        match tool_name(&member.def).map(str::to_owned) {
            Some(name) if clashing.contains(&name) => {
                let collision = ResponsesNamespaceCollision {
                    namespace: member.namespace.to_owned(),
                    tool: name,
                };
                if !namespaces.collisions.contains(&collision) {
                    namespaces.collisions.push(collision);
                }
                continue;
            }
            Some(name) => {
                namespaces.members.insert(name, member.namespace.to_owned());
            }
            None => namespaces.unnamed_members.push(ResponsesUnnamedMember {
                occurrence: count_identical_others(&tools, member.raw),
                raw: member.raw.clone(),
            }),
        }
        tools.push(member.def);
    }
    if tools.is_empty() { None } else { Some(tools) }
}

fn count_identical_others(tools: &[ToolDef], raw: &Value) -> usize {
    tools
        .iter()
        .filter(|t| matches!(t, ToolDef::Other(v) if v == raw))
        .count()
}

fn report_collisions(collisions: &[ResponsesNamespaceCollision]) {
    if collisions.is_empty() {
        return;
    }
    let pairs: Vec<String> = collisions
        .iter()
        .map(|c| {
            format!(
                "{}/{}",
                sanitize_for_log(&c.namespace),
                sanitize_for_log(&c.tool)
            )
        })
        .collect();
    tracing::warn!(
        collisions = %pairs.join(","),
        "openai-responses ingress: namespaced tools whose name is declared more than once \
         were not flattened; cross-dialect lanes do not see them"
    );
    record_translation_drop(LANE, NAMESPACE_TOOL_NAME_COLLISION);
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
