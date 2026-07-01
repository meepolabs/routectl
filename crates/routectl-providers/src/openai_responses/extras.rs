//! Reasoning controls + `provider_extras` allowlist for the Responses
//! API egress.
//!
//! Reasoning translation:
//! - `req.reasoning.effort` -> `reasoning.{effort, summary: "auto"}`
//!   ("auto" matches codex's default summary mode so the server emits
//!   reasoning_summary deltas back on stream).
//! - `req.reasoning.max_tokens` -> mapped to the nearest `effort` band
//!   via the effort<->budget table when no explicit effort is set. The
//!   Responses reasoning surface has no budget knob; an explicit effort
//!   still wins.
//!
//! provider_extras allowlist (6 keys): `prompt_cache_key`,
//! `service_tier`, `text`, `include`, `store`, `client_metadata`.
//! Anything else stays unforwarded -- matches the discipline in
//! `routectl-core::is_canonical_request_key`.
//!
//! ChatGPT-OAuth lock: `store` is always written to `false` when the
//! provider authenticates via `AuthKind::ChatgptOauth`, regardless of
//! any operator-supplied `provider_extras["store"]`. Codex sends
//! `store: false` on every ChatGPT subscription request and routectl
//! preserves that behavior to avoid the upstream rejecting the request
//! for a policy mismatch.

use serde_json::Value;

use routectl_core::ChatRequest;

use super::AuthKind;
use super::types::{ResponsesReasoning, ResponsesRequest, TextControls};
use crate::effort::{clamp_effort_to_supported, level_from_budget};

/// Set `request.reasoning` from `req.reasoning`. Effort maps to the
/// `effort` field; the `summary` mode is hardcoded to "auto" to match
/// codex's default and ensure the server emits reasoning_summary
/// deltas back to the client.
pub(super) fn apply_reasoning(request: &mut ResponsesRequest, req: &ChatRequest) {
    let Some(r) = req.reasoning.as_ref() else {
        return;
    };

    // Explicit effort wins. When no effort is set but a budget is, map
    // the budget to the nearest effort band (the Responses API takes
    // effort, not a budget) rather than dropping it.
    let effort = match r.effort.as_deref() {
        Some(e) => {
            Some(clamp_effort_to_supported(e, &req.routectl_internal.effort_levels).into_owned())
        }
        None => r.max_tokens.map(|budget| {
            let level = level_from_budget(budget);
            clamp_effort_to_supported(level, &req.routectl_internal.effort_levels).into_owned()
        }),
    };
    let enabled = r.enabled;

    // If reasoning is explicitly disabled and no effort is set, leave
    // request.reasoning at None so the field is omitted on the wire.
    if effort.is_none() && enabled == Some(false) {
        return;
    }
    // Likewise when nothing usable is present.
    if effort.is_none() && enabled.is_none() && r.max_tokens.is_none() {
        return;
    }

    request.reasoning = Some(ResponsesReasoning {
        effort,
        summary: Some("auto".into()),
    });
}

/// Layer canonical `req.provider_extras` into the Responses request.
/// Only the 6 allowed keys are honored; everything else is left
/// unforwarded so an operator-supplied long-tail field doesn't slip
/// through unaudited. The `store` flag is special-cased: for
/// `ChatgptOauth`, it stays hardcoded to `false` regardless of any
/// `provider_extras["store"]` value.
pub(super) fn merge_provider_extras(
    request: &mut ResponsesRequest,
    req: &ChatRequest,
    auth_kind: AuthKind,
) {
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return;
    };

    for (k, v) in extras {
        match k.as_str() {
            "prompt_cache_key" => {
                if let Some(s) = v.as_str() {
                    request.prompt_cache_key = Some(s.to_string());
                }
            }
            "service_tier" => {
                if let Some(s) = v.as_str() {
                    request.service_tier = Some(s.to_string());
                }
            }
            "text" => {
                request.text = Some(TextControls { inner: v.clone() });
            }
            "include" => {
                if let Some(arr) = v.as_array() {
                    request.include = arr
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                }
            }
            "store" => apply_store_override(request, v, auth_kind),
            "client_metadata" => {
                request.client_metadata = Some(v.clone());
            }
            // Any other key stays unforwarded. The discipline matches
            // routectl-core::is_canonical_request_key -- if the
            // Responses API grows a new top-level field, we add it
            // here explicitly rather than silently passing through.
            _ => {}
        }
    }
}

/// The `include` entry that carries the encrypted reasoning blob back
/// on the wire. Required whenever `store == false`, otherwise the
/// upstream returns empty `encrypted_content` and a later reasoning
/// replay by item id is a no-op (chatgpt-oauth) or a 404 (api.openai.com).
const REASONING_ENCRYPTED_INCLUDE: &str = "reasoning.encrypted_content";

/// Ensure the request asks the server to echo back the encrypted
/// reasoning carrier when the response is not persisted.
///
/// When `store == false` the server only returns a usable
/// `encrypted_content` if `include` carries
/// `"reasoning.encrypted_content"`. We force it in UNLESS the operator
/// supplied an explicit `include` via `provider_extras` (their value is
/// then respected verbatim). When `store == true` the server retains
/// reasoning, so no `include` is forced.
///
/// Runs after `merge_provider_extras` so it reflects a provider_extras
/// override of `store`.
pub(super) fn finalize_reasoning_include(request: &mut ResponsesRequest, req: &ChatRequest) {
    if request.store {
        return;
    }
    if operator_set_include(req) {
        return;
    }
    if request
        .include
        .iter()
        .any(|s| s == REASONING_ENCRYPTED_INCLUDE)
    {
        return;
    }
    request
        .include
        .push(REASONING_ENCRYPTED_INCLUDE.to_string());
}

/// Whether the operator explicitly supplied `include` via
/// `provider_extras` (an array value under the `include` key). An
/// explicit value -- even an empty array -- is honored as-is.
fn operator_set_include(req: &ChatRequest) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("include"))
        .is_some_and(serde_json::Value::is_array)
}

/// Apply an operator-supplied `store` override. For `ChatgptOauth`
/// the value is IGNORED (codex parity); for other auth_kinds the
/// boolean is honored verbatim.
fn apply_store_override(request: &mut ResponsesRequest, v: &Value, auth_kind: AuthKind) {
    if matches!(auth_kind, AuthKind::ChatgptOauth) {
        tracing::debug!(
            requested = ?v,
            "openai-responses: ignoring provider_extras.store on chatgpt-oauth (always false)"
        );
        return;
    }
    if let Some(b) = v.as_bool() {
        request.store = b;
    }
}
