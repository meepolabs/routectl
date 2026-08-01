//! Reasoning controls + `provider_extras` allowlist for the Responses
//! API egress.
//!
//! Reasoning translation:
//! - `req.reasoning.effort` -> `reasoning.effort`.
//! - `reasoning.summary` defaults to `"auto"` (so the server emits
//!   reasoning_summary deltas back on stream) UNLESS the caller supplied
//!   one via `provider_extras["reasoning"].summary`.
//! - `reasoning.context` / `reasoning.mode` / any future Responses-dialect
//!   sub-key ride through `provider_extras["reasoning"]` onto the wire.
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

use serde_json::{Map, Value};

use routectl_core::ChatRequest;

use super::AuthKind;
use super::types::{ResponsesReasoning, ResponsesRequest, TextControls};
use crate::effort::{clamp_effort_to_supported, level_from_budget};

/// Set `request.reasoning` from `req.reasoning` plus the Responses-dialect
/// remainder the ingress stashed under `provider_extras["reasoning"]`.
///
/// `effort` comes from the computed canonical value; `summary` defaults to
/// `"auto"` ONLY when the caller supplied none (a caller value wins).
/// `context` / `mode` / any future sub-key ride through the overlay onto
/// the wire object. summary / context / mode are independently meaningful:
/// a summary-only, context-only, or mode-only request still emits a
/// reasoning object. An explicit canonical `enabled: false` WINS
/// unconditionally and omits reasoning entirely -- regardless of any
/// computed effort, budget, or overlay sub-key.
pub(super) fn apply_reasoning(request: &mut ResponsesRequest, req: &ChatRequest) {
    let overlay = responses_reasoning_overlay(req);

    let (effort, enabled, budget) = match req.reasoning.as_ref() {
        Some(r) => {
            // Explicit effort wins. When no effort is set but a budget is,
            // map the budget to the nearest effort band (the Responses API
            // takes effort, not a budget) rather than dropping it.
            let effort = match r.effort.as_deref() {
                Some(e) => Some(
                    clamp_effort_to_supported(e, &req.routectl_internal.effort_levels).into_owned(),
                ),
                None => r.max_tokens.map(|budget| {
                    let level = level_from_budget(budget);
                    clamp_effort_to_supported(level, &req.routectl_internal.effort_levels)
                        .into_owned()
                }),
            };
            (effort, r.enabled, r.max_tokens)
        }
        None => (None, None, None),
    };

    // Explicit disable wins unconditionally: `enabled: false` is the caller
    // turning reasoning off, so it beats a computed effort, a budget-derived
    // effort, and any provider_extras["reasoning"] overlay sub-key.
    if enabled == Some(false) {
        return;
    }
    // Nothing to emit: no effort, no enable flag, no budget, no caller
    // sub-key. A summary-only / context-only / mode-only request DOES emit
    // because `overlay` is `Some`.
    if effort.is_none() && enabled.is_none() && budget.is_none() && overlay.is_none() {
        return;
    }

    let mut extra = overlay.unwrap_or_default();
    // `effort` is owned by the typed field / computed canonical value; drop
    // any overlay copy so it can never override it through the flatten.
    extra.remove("effort");
    // summary: a caller value wins; default to "auto" only when unset.
    let summary = match extra.remove("summary") {
        Some(Value::String(s)) => Some(s),
        Some(other) => {
            extra.insert("summary".into(), other);
            None
        }
        None => Some("auto".into()),
    };

    request.reasoning = Some(ResponsesReasoning {
        effort,
        summary,
        extra,
    });
}

/// The Responses-dialect reasoning remainder the ingress stashed under
/// `provider_extras["reasoning"]` (summary/context/mode/future). Returns
/// `None` when absent or empty so it never forces an otherwise-omitted
/// reasoning object into existence.
fn responses_reasoning_overlay(req: &ChatRequest) -> Option<Map<String, Value>> {
    req.provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("reasoning"))
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
        .cloned()
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

/// Honor the canonical structured-output directive by mapping
/// `req.response_format` (OpenAI Chat-Completions shape) onto the Responses
/// API `text.format` field. This closes the same-protocol round-trip: the
/// Responses ingress parses inbound `text.format` INTO `req.response_format`
/// (saving the remainder of `text`, e.g. `verbosity`, into
/// `provider_extras["text"]`), so the egress must re-emit it or strict JSON
/// decode fails.
///
///   `{type:json_schema, json_schema:{schema, name?, strict?}}`
///       -> `text.format = {type:json_schema, name, schema, strict?}`
///   `{type:json_object}` -> `text.format = {type:json_object}`
///
/// Runs AFTER `merge_provider_extras`, so a `verbosity` sibling lifted into
/// `provider_extras["text"]` survives and the format is merged alongside it.
/// A caller-supplied `text.format` (already present) is left untouched.
pub(super) fn apply_response_format(request: &mut ResponsesRequest, req: &ChatRequest) {
    let Some(rf) = req.response_format.as_ref() else {
        return;
    };
    let Some(format) = responses_text_format(rf) else {
        return;
    };
    match request.text.as_mut() {
        Some(tc) => {
            if let Some(obj) = tc.inner.as_object_mut() {
                obj.entry("format").or_insert(format);
            }
        }
        None => {
            request.text = Some(TextControls {
                inner: serde_json::json!({ "format": format }),
            });
        }
    }
}

/// Convert the canonical OpenAI Chat-shape `response_format` into the
/// Responses API `text.format` object (flattened: `name`/`schema`/`strict`
/// at the top level, not nested under `json_schema`). Returns `None` for an
/// absent or unrecognized shape. The Responses API requires `name` on a
/// json_schema format, so a missing name defaults to `"response"` (matching
/// the openai-compat wire-lift default).
fn responses_text_format(response_format: &Value) -> Option<Value> {
    let Some(obj) = response_format.as_object() else {
        tracing::warn!(
            "response_format is not an object; dropping structured-output \
             directive on Responses egress"
        );
        return None;
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        tracing::warn!(
            "response_format carries no string type token; dropping \
             structured-output directive on Responses egress"
        );
        return None;
    };
    match kind {
        "json_schema" => {
            let Some(js) = obj.get("json_schema").and_then(Value::as_object) else {
                tracing::warn!(
                    "response_format json_schema is absent or not an object; \
                     dropping structured-output directive on Responses egress"
                );
                return None;
            };
            let Some(schema) = js.get("schema").cloned() else {
                tracing::warn!(
                    "response_format json_schema carries no json_schema.schema; \
                     dropping structured-output directive on Responses egress"
                );
                return None;
            };
            let name = js.get("name").and_then(Value::as_str).unwrap_or("response");
            let mut fmt = serde_json::Map::new();
            fmt.insert("type".into(), Value::from("json_schema"));
            fmt.insert("name".into(), Value::from(name));
            fmt.insert("schema".into(), schema);
            if js.get("strict").and_then(Value::as_bool) == Some(true) {
                fmt.insert("strict".into(), Value::Bool(true));
            }
            Some(Value::Object(fmt))
        }
        "json_object" => Some(serde_json::json!({"type": "json_object"})),
        other => {
            tracing::warn!(
                response_format_type = other,
                "unrecognized response_format shape; dropping structured-output \
                 directive on Responses egress"
            );
            None
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

/// Apply an operator-supplied `store` override. For `ChatgptOauth` (codex
/// parity) and `BedrockMantle` (the mantle Responses lane, which must never
/// persist) the value is IGNORED and `store` stays `false`; for other
/// auth_kinds the boolean is honored verbatim.
///
/// `req.provider_extras` is the FINAL merged value at dispatch (the router
/// deep-merges provider-level and model-level `payload_extras` into it), so
/// forcing `store` here catches a model-level `store = true` the
/// config-time provider-level reject cannot see. Combined with the `false`
/// default in `request.rs`, no origin of `store = true` survives on the
/// mantle lane.
fn apply_store_override(request: &mut ResponsesRequest, v: &Value, auth_kind: AuthKind) {
    if matches!(auth_kind, AuthKind::ChatgptOauth | AuthKind::BedrockMantle) {
        tracing::debug!(
            requested = ?v,
            ?auth_kind,
            "openai-responses: ignoring provider_extras.store (lane forces store=false)"
        );
        return;
    }
    if let Some(b) = v.as_bool() {
        request.store = b;
    }
}
