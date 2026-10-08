//! Unconditional structural and security drops for Bedrock egress bodies.
//!
//! Bedrock forwards the long-tail Anthropic fields on two carriers: the
//! whole Anthropic Messages body on Invoke, and the
//! `additionalModelRequestFields` bag on Converse. Every other key -- including
//! forward-compat fields the Anthropic ingress sweeps into `provider_extras`
//! (`diagnostics`, `speed`, ...) -- passes through unchanged.
//!
//! Two drops apply on every request, with no operator knob:
//!
//! - `mcp_servers` (`drop_unrepresentable_body_fields`): Bedrock rejects it on
//!   both carriers on every account, and its entries can carry a connector
//!   credential meant for a remote MCP server. Runs as the last mutation of
//!   each carrier's egress body.
//! - an orphan `tool_choice` (`drop_orphan_tool_choice`): a `tool_choice` left
//!   with no non-empty `tools` list, which Anthropic rejects. Runs on the
//!   Invoke body that actually ships.
//!
//! Both log the field name only, at DEBUG.

use serde_json::{Map, Value};

/// Which carrier body a drop runs on. Drives logging context only.
#[derive(Debug, Clone, Copy)]
pub(super) enum FilterContext {
    /// Top-level Anthropic Messages body (Invoke).
    InvokeBody,
    /// `additionalModelRequestFields` bag (Converse).
    ConverseAdditionalFields,
}

impl FilterContext {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvokeBody => "invoke_body",
            Self::ConverseAdditionalFields => "converse_additional_fields",
        }
    }
}

/// Remove a `tool_choice` left in `bag` without a non-empty `tools` list.
///
/// Anthropic (and Bedrock Invoke, which carries the Anthropic body) rejects
/// a `tool_choice` with no tools to select, so any path that leaves a
/// `tool_choice` without a non-empty `tools` list would ship that invalid
/// shape. Run this on the body that actually ships. Logs the field name only.
pub(super) fn drop_orphan_tool_choice(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    surface: FilterContext,
) {
    if !bag.contains_key("tool_choice") {
        return;
    }
    let has_wire_tools = bag
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if has_wire_tools {
        return;
    }
    bag.remove("tool_choice");
    tracing::debug!(
        provider = %provider_id,
        field = "tool_choice",
        surface = surface.as_str(),
        "dropping tool_choice with no tools on the wire (upstream rejects the pairing)"
    );
}

/// Body fields Bedrock rejects on both carriers whatever the account's
/// schema: InvokeModel and Converse `additionalModelRequestFields` both
/// answer `mcp_servers` with a generic 400 that names no field, so it is not
/// representable on any account. Its entries can also carry a
/// connector credential issued for a remote MCP server, never for AWS.
const BEDROCK_UNREPRESENTABLE_BODY_FIELDS: &[&str] = &["mcp_servers"];

/// Remove every [`BEDROCK_UNREPRESENTABLE_BODY_FIELDS`] key from `bag`,
/// regardless of which writer put it there. Callers run this as the LAST mutation of the egress body so no
/// later writer can reintroduce a key. Logs the key name only: a value may
/// hold a credential.
pub(super) fn drop_unrepresentable_body_fields(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    surface: FilterContext,
) {
    for &key in BEDROCK_UNREPRESENTABLE_BODY_FIELDS {
        if bag.remove(key).is_some() {
            tracing::debug!(
                provider = %provider_id,
                field = key,
                surface = surface.as_str(),
                "bedrock cannot represent body field; dropped before egress"
            );
        }
    }
}

#[cfg(test)]
#[path = "body_fields_unrepresentable_tests.rs"]
mod unrepresentable_tests;
