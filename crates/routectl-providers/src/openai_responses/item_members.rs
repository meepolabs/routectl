//! The members of the client's `function_call` and `reasoning` input items
//! that canonical does not model, put back on the items this egress emits
//! under the same identity: a `function_call` under its sanitized
//! `call_id`, a `reasoning` item under the `id` it ships with.
//!
//! An entry with no emitted item to land on (a reasoning item that ships
//! without its id because it carries no signature, a call the request no
//! longer holds, an item the client sent with no identity) is withheld:
//! each member is declared removed at the client's item and the request
//! counts once as `unknown_member_withheld`. A member never lands on an
//! item other than the one the client put it on.

use std::collections::{BTreeMap, BTreeSet};

use routectl_core::wire_check::{EditKind, EditSite, WireTap};
use routectl_core::{ResponsesInputItemMembers, ResponsesItemMemberEntry};

use super::extras::UnknownMemberWithheldTally;
use super::mantle_guard::WithheldCarriers;
use super::types::{
    CarriedMembers, FUNCTION_CALL_ITEM_KEYS, REASONING_ITEM_KEYS, ResponseInputItem,
};
use crate::wire_declare::{declare, member_pointer};

/// Merge each recorded entry onto the emitted item carrying its identity,
/// typed members reserved, and withhold the rest (see the module doc).
/// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=an_unsigned_reasoning_items_members_are_withheld_declared_and_counted_once
pub(super) fn emit(
    id: &str,
    items: &mut [ResponseInputItem],
    record: &ResponsesInputItemMembers,
    tap: Option<&WireTap>,
    withheld: &mut UnknownMemberWithheldTally,
) {
    if record.is_empty() {
        return;
    }
    let calls = calls_by_wire_id(record);
    let mut placed_calls: BTreeSet<&str> = BTreeSet::new();
    let mut placed_reasoning: BTreeSet<&str> = BTreeSet::new();
    for item in items.iter_mut() {
        match item {
            ResponseInputItem::FunctionCall {
                call_id, carried, ..
            } => {
                if let Some((raw, entry)) = calls.get(call_id.as_str())
                    && placed_calls.insert(raw)
                {
                    *carried = CarriedMembers::new(entry.members.iter(), FUNCTION_CALL_ITEM_KEYS);
                }
            }
            ResponseInputItem::Reasoning {
                id: Some(reasoning_id),
                carried,
                ..
            } => {
                if let Some((key, entry)) = record.reasoning.get_key_value(reasoning_id.as_str())
                    && placed_reasoning.insert(key)
                {
                    *carried = CarriedMembers::new(entry.members.iter(), REASONING_ITEM_KEYS);
                }
            }
            _ => {}
        }
    }
    let unplaced: Vec<&ResponsesItemMemberEntry> = record
        .function_call
        .iter()
        .filter(|(raw, _)| !placed_calls.contains(raw.as_str()))
        .chain(
            record
                .reasoning
                .iter()
                .filter(|(key, _)| !placed_reasoning.contains(key.as_str())),
        )
        .map(|(_, entry)| entry)
        .chain(&record.unkeyed)
        .collect();
    if unplaced.is_empty() {
        return;
    }
    withheld.record();
    tracing::debug!(
        provider = id,
        items = unplaced.len(),
        "openai-responses egress: omitting members of input items emitted without their identity"
    );
    if let Some(tap) = tap {
        for entry in unplaced {
            for key in entry.members.keys() {
                if let Some(path) = member_pointer(&entry.client_pointer, key) {
                    declare(tap, EditSite::DialectFieldStrip, path, EditKind::Remove);
                }
            }
        }
    }
}

/// Note every recorded member withheld on a surface that emits none of
/// them; `withheld` declares and counts them with the surface's other
/// withholds.
pub(super) fn withhold_all(record: &ResponsesInputItemMembers, withheld: &WithheldCarriers) {
    for entry in record.entries() {
        withheld.note(&entry.client_pointer, entry.members.keys());
    }
}

/// The recorded `function_call` entries by the `call_id` this egress emits
/// for them. Two client ids that sanitize alike keep the first.
fn calls_by_wire_id(
    record: &ResponsesInputItemMembers,
) -> BTreeMap<String, (&str, &ResponsesItemMemberEntry)> {
    let mut calls = BTreeMap::new();
    for (raw, entry) in &record.function_call {
        calls
            .entry(crate::tool_id::sanitize_tool_id(raw).into_owned())
            .or_insert((raw.as_str(), entry));
    }
    calls
}
