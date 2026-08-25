//! InvokeModel adapter -- vendor-specific request body, with Anthropic
//! Messages JSON for Claude models.
//!
//! For Anthropic on Bedrock, the body shape is identical to the
//! Anthropic Messages API except:
//!   - `anthropic_version` is `"bedrock-2023-05-31"` (Bedrock-specific
//!     version string) and lives in the BODY, not in a header.
//!   - No auth headers (replaced by SigV4 signing on the HTTP layer).
//!   - Beta flags travel in the body's `anthropic_beta` array.
//!
//! We delegate body construction to `anthropic_api::request::normalize`
//! so the reasoning / cache_control / tool-use logic stays in one place,
//! then patch the result for Bedrock-specific fields.
//!
//! Response parsing is a direct call to `anthropic_api::response::normalize`
//! -- the Bedrock InvokeModel response shape for Anthropic models is
//! identical to the Anthropic Messages API response shape (which is
//! the whole point of the Invoke shape: pure passthrough).

use serde_json::Value;

use routectl_core::{ChatRequest, ChatResponse, Error, Result, sanitize_for_log};

use super::BedrockConfig;
use super::betas::filter_bedrock_betas;

/// The Bedrock-required `anthropic_version` body field. Distinct from
/// the Anthropic API's `anthropic-version: 2023-06-01` header.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Build the InvokeModel request body from a routectl `ChatRequest`.
///
/// For Anthropic Claude models the body is Anthropic Messages JSON
/// patched with Bedrock-specific fields. For non-Anthropic Bedrock-hosted
/// vendors, prefer Converse (vendor-neutral) -- the InvokeModel adapter
/// here does not currently shape Mistral/Llama/Cohere bodies.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    let (mut body, deferred) = crate::anthropic_api::request::normalize_deferring_format_key_warn(
        &cfg.id,
        req,
        cfg.adaptive_thinking.unwrap_or(false),
        // Bedrock-Invoke applies its own beta filter via
        // `crate::bedrock::betas::filter_bedrock_betas` (called below
        // on the assembled body); pass an empty allowlist here so the
        // anthropic-api egress's filter is a no-op pass-through.
        &[],
        // Bedrock does not emulate context_management beta; no cache.
        false,
        None,
        // Bedrock Invoke is never the genuine Anthropic host: this lane
        // egresses to a Bedrock endpoint, so a routectl reasoning
        // envelope in a `redacted_thinking` block rides through
        // byte-for-byte. Stated EXPLICITLY here rather than inherited, so
        // the passthrough cannot be flipped by a default changing
        // elsewhere.
        false,
        // Bedrock InvokeModel is not the Anthropic Messages API: support for
        // a mid-conversation `role: "system"` turn is not established on this
        // lane, so system turns stay lift-consumed here. Stated EXPLICITLY
        // for the same reason as the flag above -- a default changing
        // elsewhere must not start shipping the shape to Bedrock.
        false,
    )?;
    let obj = body.as_object_mut().ok_or_else(|| {
        Error::NormalizeRequest(
            cfg.id.clone(),
            "anthropic_api::request assembly returned non-object".into(),
        )
    })?;

    // Bedrock requires the body-side anthropic_version. Override
    // anything the upstream normalizer may have added.
    obj.insert(
        "anthropic_version".into(),
        Value::String(BEDROCK_ANTHROPIC_VERSION.into()),
    );

    // Merge the configured beta flags. If the body already has its own
    // anthropic_beta (e.g. from a per-request override), prepend the
    // provider-level flags so user overrides take precedence on
    // duplicate keys.
    if !cfg.anthropic_beta.is_empty() {
        let combined: Vec<Value> = cfg
            .anthropic_beta
            .iter()
            .cloned()
            .map(Value::String)
            .chain(
                obj.get("anthropic_beta")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            )
            .collect();
        obj.insert("anthropic_beta".into(), Value::Array(combined));
    }

    // Filter the merged anthropic_beta against the operator-supplied
    // `[bedrock] allowed_betas` list (routectl ships no const default).
    // Operator-supplied flags from `cfg.anthropic_beta`
    // (`[providers.X] anthropic_beta`) pass through unconditionally;
    // flags lifted from the inbound `anthropic-beta` HTTP header that
    // are not on the operator's accepted list drop at DEBUG. Without
    // this filter, claude-code's TS SDK 400s every Bedrock request
    // because it ships up to ten betas, only a subset of which AWS
    // gates for distribution. Shared with the Converse adapter via
    // `super::betas`.
    //
    // Empty `cfg.allowed_betas` puts the filter in pass-through mode
    // (every flag survives) -- the discovery default for operators
    // bringing up routectl against a fresh AWS account.
    filter_bedrock_betas(&cfg.id, obj, &cfg.anthropic_beta, &cfg.allowed_betas);

    // Merge any additional model request fields at the top level
    // with the same routectl-managed-keys allow-list as the Anthropic
    // and openai-compat egresses. The Bedrock-Invoke body is in
    // Anthropic shape (today); without this filter, a config-file
    // `additional_model_request_fields = { "messages" = [...] }`
    // would silently replace the assembled history. Filter here
    // (rather than at config load) so the WARN line carries the
    // provider id at request time and matches the operator's
    // existing log workflow.
    if let Some(extras) = cfg.additional_model_request_fields.as_ref()
        && let Some(extras_obj) = extras.as_object()
    {
        for (k, v) in extras_obj {
            if is_bedrock_invoke_managed_key(k) {
                tracing::warn!(
                    provider = %cfg.id,
                    key = %sanitize_for_log(k),
                    "additional_model_request_fields attempted to override \
                     routectl-managed key; dropped"
                );
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }

    // Stream flag is decided at HTTP level (different endpoint suffix),
    // not via a body field, so strip any leftover.
    obj.remove("stream");

    // Drop the Claude Code billing/attribution system block before the
    // body hits AWS. The body is in Anthropic wire shape (the shared
    // anthropic_api normalizer forwards `system` verbatim, which is
    // correct for the all-Anthropic path), but Bedrock is a third-party
    // upstream, so the client fingerprint the block carries must not
    // leave routectl here.
    strip_billing_system_field(&cfg.id, obj);

    // Drop the Anthropic `metadata` block before the body hits AWS. It
    // carries the client fingerprint (`user_id`, `account_uuid`) lifted
    // from the inbound request via the Anthropic ingress forward-compat
    // sweep (provider_extras -> body, merged inside the shared
    // normalizer above). Bedrock is always a third-party upstream, so
    // the CLIENT-path strip is unconditional. An operator who
    // deliberately set `metadata` via `additional_model_request_fields`
    // keeps it -- that is the operator's choice, not a client
    // fingerprint. Shared key with the Converse seam via
    // `super::CLIENT_FINGERPRINT_METADATA_KEY`.
    strip_client_metadata(obj, cfg.additional_model_request_fields.as_ref());

    // AWS Bedrock InvokeModel REJECTS a top-level `cache_control` body
    // field with HTTP 400 ("Extra inputs are not permitted") -- proven by
    // a live probe. The shared anthropic_api normalizer emits the
    // canonical `req.cache_control` as a top-level marker (correct for
    // Anthropic-direct, which honors it). Bedrock InvokeModel for
    // Anthropic instead honors a PER-BLOCK marker inside a content block,
    // where a single marker caches all prefixes up to ~20 blocks before
    // it. Lower the top-level marker to the last eligible block.
    lower_top_level_cache_control_to_per_block(&cfg.id, obj);

    // Bedrock's InvokeModel takes the model identifier in the URL path
    // (`/model/{model_id}/invoke`), not the body. The Anthropic API
    // path requires it in the body, which `anthropic_api::request::
    // normalize` honors -- but Bedrock's strict-schema validator
    // rejects a body-side `model` with "Extra inputs are not
    // permitted". Strip it here on the Bedrock-Invoke seam.
    obj.remove("model");

    // Filter the assembled body against `[bedrock] allowed_body_fields`
    // so Anthropic-ingress forward-compat sweeps (`mcp_servers`,
    // `diagnostics`, `context_hint`, `speed`, ...) drop on the egress
    // before the request hits AWS. Without this filter, every
    // claude-code request 400s on the first unrecognized field --
    // Bedrock validates with strict-schema "Extra inputs are not
    // permitted". See `super::body_fields` for the full contract.
    super::body_fields::filter_bedrock_body_fields(
        &cfg.id,
        obj,
        &cfg.allowed_body_fields,
        super::body_fields::FilterContext::InvokeBody,
    );

    // Re-run both `output_config` passes the shared assembly already ran, on
    // the body that ACTUALLY ships. The `additional_model_request_fields`
    // merge above is a post-normalize write path that
    // `is_bedrock_invoke_managed_key` does not cover for `output_config`, so
    // an operator-supplied object can both reintroduce the `format` keys
    // Anthropic cannot represent and replace the whole repaired schema with
    // an unrepaired one (missing the mandatory `additionalProperties: false`).
    // Each diagnostic folds into ONE WARN: the shared pass deliberately
    // deferred its emission so this lane does not warn twice for a single
    // request.
    deferred.rescanning(&cfg.id, obj)?.warn(&cfg.id);

    // Capability union, LAST so it reads the body that actually ships: a
    // body carrying `output_config.format` gains the structured-outputs beta
    // in `anthropic_beta`. Retained as belt-and-braces, NOT a proven hard
    // requirement -- a 2026-08-11 live capture on api.anthropic.com (one
    // lane, one seat, one model) accepted the field both WITH and WITHOUT
    // the beta, refuting the older "rejected unless the flag rides along"
    // claim on that surface. Whether AWS rejects an ungated body is
    // UNMEASURED, and an older account/model tier may still gate the field,
    // so the union stays; revisit it if Anthropic retires the flag (whether
    // an unknown beta string is itself rejected is also unmeasured).
    // Deliberately AFTER both Bedrock allowlist filters:
    //   - after `filter_bedrock_betas`, because the flag is a
    //     routectl-derived capability signal implied by the shipped body,
    //     not a client-opted beta, so it bypasses `[bedrock] allowed_betas`
    //     with the same standing the operator's `cfg.anthropic_beta` floor
    //     has. Unioning it earlier lets a restrictive allowlist that omits
    //     the flag drop it again.
    //   - after `filter_bedrock_body_fields`, so an `allowed_body_fields`
    //     list that drops `output_config` entirely produces no flag either.
    //     When `output_config.format` DOES survive that filter, the flag it
    //     implies survives too, even if the operator's list omits
    //     `anthropic_beta`.
    // Feature-triggered and idempotent: no `output_config.format` means no
    // flag, and an already-present flag is neither duplicated nor reordered.
    crate::anthropic_api::request::apply_structured_outputs_beta_to_body(&mut body);

    Ok(body)
}

/// Top-level Bedrock-Invoke body keys (Anthropic-shape today) that
/// `additional_model_request_fields` is NOT permitted to override.
/// Mirrors the Anthropic egress allow-list plus the Bedrock-specific
/// `anthropic_version` field. Long-tail Anthropic-only knobs
/// (`top_k`, `metadata`, `service_tier`) still pass through.
fn is_bedrock_invoke_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "system"
            | "max_tokens"
            | "thinking"
            | "tools"
            | "tool_choice"
            | "stream"
            | "stop_sequences"
            | "temperature"
            | "top_p"
            | "anthropic_beta"
            | "anthropic_version"
            | "cache_control"
    )
}

// `filter_bedrock_betas` moved to `super::betas`; the Converse adapter
// applies the identical filter via the same helper.

/// Drop the Claude Code billing/attribution block from the assembled
/// Anthropic-shape `system` body field. Handles both wire shapes: a flat
/// string `system` and an array of `{type:"text", text}` blocks. Removes
/// the `system` key entirely when nothing survives. Reuses the canonical
/// predicate in `crate::system_filter` for the prefix match.
fn strip_billing_system_field(id: &str, obj: &mut serde_json::Map<String, Value>) {
    let Some(system) = obj.get("system") else {
        return;
    };
    // `Some(v)` -> replace `system` with `v`; `None` -> remove `system`.
    // We only reach the assignment when a billing block was found, so the
    // warn fires exactly when the body actually changes.
    let replacement: Option<Value> = match system {
        Value::String(s) if crate::system_filter::is_billing_attribution_block(s) => None,
        Value::Array(blocks) => {
            let kept: Vec<Value> = blocks
                .iter()
                .filter(|b| {
                    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
                    !crate::system_filter::is_billing_attribution_block(text)
                })
                .cloned()
                .collect();
            if kept.len() == blocks.len() {
                return;
            }
            if kept.is_empty() {
                None
            } else {
                Some(Value::Array(kept))
            }
        }
        // A normal string system, or a non-string/non-array shape: nothing
        // to strip.
        _ => return,
    };
    tracing::warn!(
        provider = id,
        "bedrock-invoke egress: Claude Code billing/attribution system block dropped",
    );
    match replacement {
        Some(v) => {
            obj.insert("system".into(), v);
        }
        None => {
            obj.remove("system");
        }
    }
}

/// Drop the client-fingerprint `metadata` block from the assembled
/// Bedrock-Invoke body. The block (carrying `user_id` / `account_uuid`)
/// rides in via the Anthropic ingress forward-compat sweep, so it is
/// always client-derived. An operator who deliberately set `metadata`
/// via `additional_model_request_fields` keeps it -- that config value
/// is restored after the strip. Shares the key name with the Converse
/// seam via `super::CLIENT_FINGERPRINT_METADATA_KEY`.
fn strip_client_metadata(
    obj: &mut serde_json::Map<String, Value>,
    operator_extras: Option<&Value>,
) {
    let key = super::CLIENT_FINGERPRINT_METADATA_KEY;
    let operator_metadata = operator_extras
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(key))
        .cloned();
    obj.remove(key);
    if let Some(v) = operator_metadata {
        obj.insert(key.to_string(), v);
    }
}

/// Lower a TOP-LEVEL `cache_control` body field to a PER-BLOCK marker on
/// the last cache_control-eligible content block. ALWAYS removes the
/// top-level field first -- that removal alone is what stops the Bedrock
/// 400; the per-block re-placement preserves the caller's caching intent.
///
/// The re-placement is computed on a CLONE of the messages array, the
/// resulting full breakpoint sequence is re-validated against the
/// canonical invariants, and only a valid arrangement is committed. The
/// pre-lowering validation in `anthropic_api::request::normalize` checked
/// a DIFFERENT arrangement (with the top-level marker, no per-block
/// insertion), so the lowered shape -- which can interact with an
/// already-marked forward-compat `Other` block the insertion scan skips
/// -- must be re-checked here before it ships.
///
/// Drop-only fallback (top-level removed, no per-block added) is the safe
/// path under any uncertainty: the original top-level body is broken, so
/// re-emitting it is never correct.
fn lower_top_level_cache_control_to_per_block(
    provider_id: &str,
    obj: &mut serde_json::Map<String, Value>,
) {
    // Always remove the top-level marker first; that alone stops the 400.
    let Some(cc) = obj.remove("cache_control") else {
        return;
    };
    // `cc` stays in scope for the whole function, so borrow the ttl for
    // logging rather than allocating a String.
    let ttl = cc.get("ttl").and_then(Value::as_str).unwrap_or("");

    let Some(messages) = obj.get("messages").and_then(Value::as_array) else {
        tracing::warn!(
            provider = provider_id,
            "bedrock-invoke egress: top-level cache_control dropped (no messages array to host a per-block marker)"
        );
        return;
    };

    // Compute the insertion on a CANDIDATE clone; do NOT mutate the live
    // `obj["messages"]` until the result is validated.
    let mut candidate = messages.clone();
    let Some(kind) = insert_marker_on_candidate(&mut candidate, &cc) else {
        tracing::warn!(
            provider = provider_id,
            "bedrock-invoke egress: top-level cache_control dropped (no eligible content block to host a per-block marker)"
        );
        return;
    };

    // Re-validate the FULL breakpoint sequence across tools -> system ->
    // (candidate) messages before committing. This catches arrangements
    // the pre-lowering validator never saw -- e.g. inserting a 5m marker
    // ahead of an already-marked 1h forward-compat `Other` block.
    if let Err(e) = validate_lowered_breakpoints(obj, &candidate) {
        tracing::warn!(
            provider = provider_id,
            error = %e,
            "bedrock-invoke egress: top-level cache_control dropped (lowering would violate the breakpoint invariants)"
        );
        return;
    }

    // Valid -> commit the candidate.
    obj.insert("messages".into(), Value::Array(candidate));
    match kind {
        InsertKind::NewTextBlock => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: lowered top-level cache_control to a new trailing text block"
        ),
        InsertKind::ExistingBlock => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: lowered top-level cache_control to the last eligible content block"
        ),
        InsertKind::AlreadyMarked => tracing::debug!(
            provider = provider_id,
            ttl = %sanitize_for_log(ttl),
            "bedrock-invoke egress: top-level cache_control removed; last eligible block already marked"
        ),
    }
}

/// How the candidate placed the marker; drives the commit-time debug log.
enum InsertKind {
    /// A trailing string-content message was converted to a one-element
    /// text block carrying the marker.
    NewTextBlock,
    /// The marker was inserted onto an existing eligible block.
    ExistingBlock,
    /// The last eligible block already carried a marker; nothing was added.
    AlreadyMarked,
}

/// Apply the per-block lowering to a CANDIDATE messages array in place.
/// Scans messages last -> first; within each, blocks last -> first. The
/// first eligible block found is the target. A trailing string-content
/// message converts to a single text block carrying the marker.
///
/// Returns `Some(kind)` describing the placement, or `None` if there was
/// no eligible target (caller drops the marker).
fn insert_marker_on_candidate(messages: &mut [Value], cc: &Value) -> Option<InsertKind> {
    for msg in messages.iter_mut().rev() {
        let Some(content) = msg.get_mut("content") else {
            continue;
        };

        if let Some(s) = content.as_str() {
            // Convert the trailing string turn to a one-element text block
            // carrying the marker. Realistic traffic (and the live probe)
            // sends a flat-string system + a string user message, so this
            // is the ONLY way to place a marker that caches the stable
            // system prefix; drop-only would cache nothing. The cacheable
            // prefix (tools / system / earlier messages) stays
            // byte-identical -- only this trailing turn is rewritten, and
            // deterministically so within a request.
            let text = s.to_owned();
            *content = Value::Array(vec![serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": cc.clone(),
            })]);
            return Some(InsertKind::NewTextBlock);
        }

        let Some(blocks) = content.as_array_mut() else {
            continue;
        };
        for block in blocks.iter_mut().rev() {
            let Some(map) = block.as_object_mut() else {
                continue;
            };
            let is_eligible = map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_cache_control_eligible_block_type);
            if !is_eligible {
                continue;
            }
            if map.contains_key("cache_control") {
                // Already caches at/after this point; adding would
                // duplicate and risk the breakpoint cap. Top-level
                // removal alone is the fix here.
                return Some(InsertKind::AlreadyMarked);
            }
            map.insert("cache_control".into(), cc.clone());
            return Some(InsertKind::ExistingBlock);
        }
    }
    None
}

/// Re-validate the post-lowering breakpoint sequence against the canonical
/// invariants. Walks the `cache_control` markers in cache-prefix order
/// across tools -> system -> (candidate) messages, builds
/// `routectl_core::cache_control::Breakpoint` values, and delegates to
/// `routectl_core::cache_control::validate` -- the single source of truth
/// for the count cap (<= MAX_BREAKPOINTS) and the TTL-ordering rule (1h
/// before 5m). Markers are pulled from the assembled JSON body directly
/// because the lowering operates on JSON, not the typed request.
fn validate_lowered_breakpoints(
    obj: &serde_json::Map<String, Value>,
    candidate_messages: &[Value],
) -> Result<()> {
    use routectl_core::cache_control::{Breakpoint, BreakpointPosition, CacheControl, validate};

    fn parse_marker(v: &Value) -> Option<CacheControl> {
        serde_json::from_value::<CacheControl>(v.clone()).ok()
    }

    let mut owned: Vec<(BreakpointPosition, CacheControl)> = Vec::new();

    // Tools first in the cache prefix.
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        for t in tools {
            if let Some(cc) = t.get("cache_control").and_then(parse_marker) {
                owned.push((BreakpointPosition::Tools, cc));
            }
        }
    }

    // Then system blocks (array shape only carries per-block markers).
    if let Some(blocks) = obj.get("system").and_then(Value::as_array) {
        for b in blocks {
            if let Some(cc) = b.get("cache_control").and_then(parse_marker) {
                owned.push((BreakpointPosition::System, cc));
            }
        }
    }

    // Then the candidate messages, in order.
    for msg in candidate_messages {
        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
            for b in blocks {
                if let Some(cc) = b.get("cache_control").and_then(parse_marker) {
                    owned.push((BreakpointPosition::Messages, cc));
                }
            }
        }
    }

    let bps: Vec<Breakpoint<'_>> = owned
        .iter()
        .map(|(position, control)| Breakpoint {
            position: *position,
            control,
        })
        .collect();
    validate(&bps)
}

/// The INSERTION-TARGET allow-list: the content block `type`s the
/// Bedrock-Invoke lowering will place a `cache_control` marker onto.
/// Forward-compat `Other` (unknown `type`) blocks and
/// `thinking` / `redacted_thinking` blocks are intentionally NEVER
/// insertion targets -- `Other` because its shape is opaque and
/// `thinking` / `redacted_thinking` because they are not valid cache
/// breakpoint targets. This predicate is deliberately NOT a mirror of the
/// sibling `anthropic_api::request::content_block_cache_control` walk
/// (which counts `Other` via a catch-all and does not list
/// `search_result` separately); that walk enumerates existing markers,
/// this list selects where to ADD one. Ordering safety against an
/// already-marked `Other` block is provided by the
/// clone -> validate -> rollback gate in
/// `lower_top_level_cache_control_to_per_block`, not by this predicate.
fn is_cache_control_eligible_block_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "text" | "image" | "document" | "tool_use" | "tool_result" | "search_result"
    )
}

/// Parse the Bedrock InvokeModel response body into a `ChatResponse`.
///
/// For Anthropic Claude models this is exactly the Anthropic Messages
/// API response shape, so we delegate.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    crate::anthropic_api::response::normalize(provider_id, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    use crate::bedrock::{BedrockApiShape, BedrockCreds};

    fn fake_cfg() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: vec!["context-1m-2025-08-07".into()],
            allowed_betas: vec![
                "context-1m-2025-08-07".into(),
                "claude-code-20250219".into(),
                "interleaved-thinking-2025-05-14".into(),
                "context-management-2025-06-27".into(),
                "effort-2025-11-24".into(),
                "fine-grained-tool-streaming-2025-05-14".into(),
                "computer-use-2025-01-24".into(),
                "computer-use-2024-10-22".into(),
                "mcp-client-2025-04-04".into(),
                "search-results-2025-06-09".into(),
            ],
            allowed_body_fields: full_body_fields(),
            // `top_p` is canonical and would now be filtered out;
            // use `top_k` here as a real long-tail Anthropic-only
            // knob the allow-list lets through.
            additional_model_request_fields: Some(json!({"top_k": 40})),
            adaptive_thinking: None,
        }
    }

    /// Empirical 2026-05-12 Bedrock body-field allowlist + `top_k`,
    /// reused across the Invoke test fixtures so a request lifted from
    /// the Anthropic ingress survives the body-field filter.
    fn full_body_fields() -> Vec<String> {
        vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ]
    }

    fn user_req() -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(64),
            stream: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn body_includes_bedrock_anthropic_version_and_beta_flags() {
        let cfg = fake_cfg();
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07"]),);
        assert_eq!(body["top_k"], json!(40));
        assert!(body.get("stream").is_none(), "stream should be stripped");
        assert!(
            body.get("model").is_none(),
            "model must be stripped: Bedrock takes it in the URL, not the body"
        );
    }

    /// Lane pin: Bedrock InvokeModel never ships a mid-conversation
    /// `role: "system"` turn. This lane passes `forward_system_turns:
    /// false`, so a canonical system present ALONGSIDE `Role::System`
    /// messages still leaves the messages array free of the wire role --
    /// a default flip on the anthropic-api side cannot leak the shape
    /// here.
    #[test]
    fn system_role_turns_stay_absent_from_the_invoke_body() {
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(routectl_core::SystemContent::Text("be brief".into()));
        req.messages = vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("mid-conversation note".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert_eq!(body["system"], json!("be brief"));
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 2, "got: {body}");
        assert!(
            msgs.iter().all(|m| m["role"] != json!("system")),
            "the Invoke lane must not ship a system-role turn: {body}"
        );
    }

    #[test]
    fn additional_model_request_fields_cannot_override_managed_keys() {
        // A misconfigured
        // `additional_model_request_fields = { "messages" = [...] }`
        // would silently replace the assembled history. Now blocked
        // with a WARN, matching the openai-compat / anthropic_api
        // egress filters.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            // managed -- MUST be dropped:
            "messages": [{"role": "user", "content": "INJECTED"}],
            "system": "INJECTED",
            "anthropic_version": "evil-version",
            "max_tokens": 1,
            // long-tail -- MUST pass through:
            "metadata": {"user_id": "u-1"},
        }));
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        // Anthropic-version stays as the Bedrock-required value.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        // Messages stays the user_req() text, NOT "INJECTED".
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        // System didn't get stomped.
        assert!(body.get("system").is_none() || body["system"] != "INJECTED");
        // max_tokens stays the request's value, not the override.
        assert_ne!(body["max_tokens"], 1);
        // Long-tail extras land verbatim.
        assert_eq!(body["metadata"]["user_id"], "u-1");
    }

    #[test]
    fn cache_control_on_user_text_round_trips_to_bedrock_invoke_body() {
        // Bedrock InvokeModel REJECTS a top-level cache_control body field
        // (HTTP 400), so the Bedrock-Invoke egress LOWERS the canonical
        // top-level marker to a per-block marker. Per-block markers the
        // caller already placed (here: the 1h system block and the 5m user
        // text block) are preserved byte-for-byte; the body must carry NO
        // top-level cache_control key after normalize.
        use routectl_core::{
            KnownContentPart, SystemBlock, cache_control::CacheControl, content_part::ContentPart,
            system_content::SystemContent,
        };

        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]);

        let body = normalize_request(&cfg, &req).unwrap();

        // Top-level cache_control is lowered away (Bedrock 400s on it).
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed for Bedrock-Invoke: {body}"
        );
        // System block preserved with its original cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
        // User text block already carried its own 5m marker; it is the
        // last eligible block and already marked, so it stays 5m.
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["ttl"], "5m");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// Top-level 5m marker + a trailing Blocks-array user message whose
    /// last block carries no cache_control: the marker lowers onto that
    /// last block, and the top-level key is gone.
    #[test]
    fn top_level_cache_control_lowers_to_last_unmarked_block() {
        use routectl_core::{
            KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: None,
            })]);

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["type"], "ephemeral");
        assert_eq!(blk["cache_control"]["ttl"], "5m");
    }

    /// Top-level 5m marker + last user message content is a STRING: the
    /// string converts to a one-element text-block array carrying the 5m
    /// marker; the top-level key is gone.
    #[test]
    fn top_level_cache_control_lowers_onto_stringified_content() {
        use routectl_core::cache_control::CacheControl;

        // Arrange: user_req() builds MessageContent::Text("hello"), which
        // the shared normalizer emits as a JSON string `content`.
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("string content converted to a block array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[0]["cache_control"]["ttl"], "5m");
    }

    /// Top-level marker + the last eligible block already carries its own
    /// marker: the top-level key is removed, and the existing block marker
    /// is left UNCHANGED (no duplicate, no overwrite). The block's TTL is
    /// 1h and the top-level is 5m so the shared normalizer's prefix-order
    /// validation (longer TTLs before shorter) is satisfied before lowering
    /// runs.
    #[test]
    fn top_level_cache_control_does_not_override_existing_block_marker() {
        use routectl_core::{
            KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_1h()),
            })]);

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "top-level cache_control must be removed: {body}"
        );
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(
            blk["cache_control"]["ttl"], "1h",
            "existing block marker must be left unchanged, not overwritten: {body}"
        );
    }

    /// No top-level cache_control, but a per-block marker on a system
    /// block: the body is unchanged -- the per-block marker survives and
    /// no top-level key is introduced.
    #[test]
    fn no_top_level_cache_control_leaves_per_block_marker_untouched() {
        use routectl_core::{
            SystemBlock, cache_control::CacheControl, system_content::SystemContent,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = None;
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("cache_control").is_none(),
            "no top-level cache_control must be introduced: {body}"
        );
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
    }

    /// Determinism: lowering is a pure function of the input body, so two
    /// identical requests must serialize to byte-identical bodies.
    #[test]
    fn lowering_is_deterministic_across_identical_requests() {
        use routectl_core::{
            KnownContentPart, SystemBlock, cache_control::CacheControl, content_part::ContentPart,
            system_content::SystemContent,
        };

        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        std::sync::Arc::make_mut(&mut req.messages)[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                citations: None,
                cache_control: None,
            })]);

        // Act
        let first = normalize_request(&cfg, &req).unwrap().to_string();
        let second = normalize_request(&cfg, &req).unwrap().to_string();

        // Assert
        assert_eq!(first, second, "lowering must be byte-identical");
    }

    /// Review concern 8 (load-bearing rollback): a top-level 5m marker plus
    /// a trailing user message whose blocks are [eligible text (unmarked),
    /// trailing unknown `Other` block carrying a 1h marker]. Inserting the
    /// 5m onto the text block would place a 5m breakpoint BEFORE the 1h
    /// `Other` breakpoint -- a TTL-ordering violation. The clone -> validate
    /// gate must ROLL BACK to drop-only: top-level removed, no 5m inserted,
    /// the trailing 1h `Other` marker untouched.
    #[test]
    fn lowering_rolls_back_to_drop_only_on_ttl_order_violation() {
        // Arrange
        let mut obj = serde_json::Map::new();
        obj.insert(
            "cache_control".into(),
            json!({"type": "ephemeral", "ttl": "5m"}),
        );
        obj.insert(
            "messages".into(),
            json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {
                        "type": "some_future_block",
                        "cache_control": {"type": "ephemeral", "ttl": "1h"}
                    }
                ]
            }]),
        );

        // Act
        lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

        // Assert
        assert!(
            obj.get("cache_control").is_none(),
            "top-level cache_control must always be removed: {obj:?}"
        );
        let blocks = obj["messages"][0]["content"].as_array().unwrap();
        assert!(
            blocks[0].get("cache_control").is_none(),
            "5m marker must NOT be inserted on the text block (rolled back): {obj:?}"
        );
        assert_eq!(
            blocks[1]["cache_control"]["ttl"], "1h",
            "trailing Other block's 1h marker must be left untouched: {obj:?}"
        );
    }

    /// No `messages` array to host a per-block marker: the top-level marker
    /// is dropped (drop-only via the no-messages warn path).
    #[test]
    fn top_level_cache_control_dropped_when_no_messages_array() {
        // Arrange
        let mut obj = serde_json::Map::new();
        obj.insert(
            "cache_control".into(),
            json!({"type": "ephemeral", "ttl": "5m"}),
        );

        // Act
        lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

        // Assert
        assert!(
            obj.get("cache_control").is_none(),
            "top-level cache_control must be removed even with no messages: {obj:?}"
        );
    }

    /// The only block is a `thinking` block (not an insertion target): the
    /// top-level marker is dropped and the thinking block is untouched.
    #[test]
    fn top_level_cache_control_dropped_when_no_eligible_block() {
        // Arrange
        let mut obj = serde_json::Map::new();
        obj.insert(
            "cache_control".into(),
            json!({"type": "ephemeral", "ttl": "5m"}),
        );
        obj.insert(
            "messages".into(),
            json!([{
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "hmm"}]
            }]),
        );

        // Act
        lower_top_level_cache_control_to_per_block("bedrock:test", &mut obj);

        // Assert
        assert!(
            obj.get("cache_control").is_none(),
            "top-level cache_control must be removed: {obj:?}"
        );
        let block = &obj["messages"][0]["content"][0];
        assert!(
            block.get("cache_control").is_none(),
            "ineligible thinking block must not receive a marker: {obj:?}"
        );
    }

    /// Direct unit test of the post-lowering validator: a 1h-before-5m
    /// sequence is VALID, a 5m-before-1h sequence is INVALID, and a
    /// 5-marker sequence is INVALID (exceeds MAX_BREAKPOINTS).
    #[test]
    fn validate_lowered_breakpoints_enforces_count_and_ttl_order() {
        let mk = |ttl: &str| json!({"type": "ephemeral", "ttl": ttl});

        // VALID: 1h (system) then 5m (messages).
        let mut valid = serde_json::Map::new();
        valid.insert(
            "system".into(),
            json!([{"type": "text", "text": "s", "cache_control": mk("1h")}]),
        );
        let valid_msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "u", "cache_control": mk("5m")}]
        })];
        assert!(validate_lowered_breakpoints(&valid, &valid_msgs).is_ok());

        // INVALID: 5m (system) then 1h (messages).
        let mut bad_order = serde_json::Map::new();
        bad_order.insert(
            "system".into(),
            json!([{"type": "text", "text": "s", "cache_control": mk("5m")}]),
        );
        let bad_order_msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "u", "cache_control": mk("1h")}]
        })];
        assert!(validate_lowered_breakpoints(&bad_order, &bad_order_msgs).is_err());

        // INVALID: 5 markers exceeds MAX_BREAKPOINTS (4).
        let five = serde_json::Map::new();
        let five_msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "a", "cache_control": mk("5m")},
                {"type": "text", "text": "b", "cache_control": mk("5m")},
                {"type": "text", "text": "c", "cache_control": mk("5m")},
                {"type": "text", "text": "d", "cache_control": mk("5m")},
                {"type": "text", "text": "e", "cache_control": mk("5m")}
            ]
        })];
        assert!(validate_lowered_breakpoints(&five, &five_msgs).is_err());
    }
    /// through `normalize_request` -> `anthropic_api::request::normalize`
    /// and produces the Opus 4.7+ wire shape (no `budget_tokens`,
    /// top-level `output_config.effort`). This is the integration
    /// point that lets a Bedrock provider opt into adaptive thinking
    /// without touching the shared anthropic_api normalizer's
    /// signature beyond the `bool` flag.
    #[test]
    fn adaptive_thinking_propagates_from_bedrock_cfg() {
        use routectl_core::ReasoningConfig;
        let mut cfg = fake_cfg();
        cfg.adaptive_thinking = Some(true);

        let mut req = user_req();
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: Some(2000),
            exclude: None,
            enabled: Some(true),
        });
        let body = normalize_request(&cfg, &req).unwrap();

        // thinking is the adaptive shape (no budget_tokens) and the
        // effort moves to top-level output_config.
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// Structured output (`output_config.format`) flows through
    /// provider_extras -> Bedrock-Invoke body verbatim. Bedrock-Invoke
    /// for Claude is pure Anthropic-shape passthrough (only the
    /// `anthropic_version` body field differs from api.anthropic.com),
    /// so when the canonical request carries `output_config` in
    /// provider_extras (placed there by the Anthropic ingress, see
    /// `routectl_cli::ingress::anthropic::translate_request`), the
    /// Bedrock egress must emit the field unchanged. Bedrock-Invoke for
    /// Claude is pure Anthropic-shape passthrough; structured output
    /// (output_config.format) needs no extra translation on this seam.
    #[test]
    fn structured_output_format_passes_through_to_bedrock_invoke_body() {
        let cfg = fake_cfg();
        let mut req = user_req();
        req.provider_extras = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        }));
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
        // The gating beta rides along with the field it gates. `fake_cfg`'s
        // `allowed_betas` omits it, so this also pins the carve-out.
        let betas = body["anthropic_beta"]
            .as_array()
            .expect("a structured-output body must carry anthropic_beta");
        assert!(
            betas
                .iter()
                .any(|b| b.as_str()
                    == Some(routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA)),
            "output_config.format must never ship without its gating beta; got: {betas:?}"
        );
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// REGRESSION: a NON-EMPTY `[bedrock] allowed_betas` that omits the
    /// structured-outputs flag must not strip it off a body that carries
    /// `output_config.format`. The flag is a routectl-derived server
    /// requirement implied by the shipped field, so the union runs AFTER
    /// `filter_bedrock_betas` -- running it earlier let the filter drop the
    /// flag again and shipped the gated field ungated, which AWS 400s.
    #[test]
    fn structured_outputs_beta_survives_a_restrictive_bedrock_allowlist() {
        let flag = routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;
        let mut cfg = fake_cfg();
        cfg.allowed_betas = vec!["context-1m-2025-08-07".into()];
        assert!(
            !cfg.allowed_betas.iter().any(|b| b == flag),
            "precondition: the allowlist must omit the structured-outputs flag"
        );
        assert!(
            !cfg.anthropic_beta.iter().any(|b| b == flag),
            "precondition: the operator floor must not supply the flag either"
        );

        let mut req = user_req();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": {"type": "object"}},
        }));

        let body = normalize_request(&cfg, &req).unwrap();
        assert!(
            body["output_config"].get("format").is_some(),
            "precondition: the structured-output directive must reach the body; got: {body}"
        );
        let betas: Vec<&str> = body["anthropic_beta"]
            .as_array()
            .expect("the gating beta must be on the final body")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            betas.iter().filter(|b| **b == flag).count(),
            1,
            "the flag must survive the allowlist exactly once; got: {betas:?}"
        );
    }

    // -----------------------------------------------------------------
    // anthropic_beta filter against Bedrock-accepted set
    // -----------------------------------------------------------------

    fn fake_cfg_no_betas() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: vec![],
            allowed_betas: vec![
                "context-1m-2025-08-07".into(),
                "claude-code-20250219".into(),
                "interleaved-thinking-2025-05-14".into(),
                "context-management-2025-06-27".into(),
                "effort-2025-11-24".into(),
                "fine-grained-tool-streaming-2025-05-14".into(),
                "computer-use-2025-01-24".into(),
                "computer-use-2024-10-22".into(),
                "mcp-client-2025-04-04".into(),
                "search-results-2025-06-09".into(),
            ],
            allowed_body_fields: full_body_fields(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        }
    }

    /// Pre-canned request whose canonical anthropic_beta has 4 flags,
    /// 2 in the operator-supplied `allowed_betas` (from
    /// `fake_cfg_no_betas()`) and 2 not. After normalize_request, only
    /// the two accepted survive in the body.
    #[test]
    fn invoke_filters_unsupported_anthropic_beta_against_accepted_set() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec![
            "context-1m-2025-08-07".into(),           // accepted
            "oauth-2025-04-20".into(),                // rejected by Bedrock
            "interleaved-thinking-2025-05-14".into(), // accepted
            "redact-thinking-2026-02-12".into(),      // rejected by Bedrock
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: only the accepted subset survives.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            strs.contains(&"context-1m-2025-08-07"),
            "missing accepted: {strs:?}"
        );
        assert!(
            strs.contains(&"interleaved-thinking-2025-05-14"),
            "missing accepted: {strs:?}"
        );
        assert!(
            !strs.contains(&"oauth-2025-04-20"),
            "rejected flag leaked through: {strs:?}"
        );
        assert!(
            !strs.contains(&"redact-thinking-2026-02-12"),
            "rejected flag leaked through: {strs:?}"
        );
    }

    /// Provider-config anthropic_beta survives the filter even when its
    /// contents are not in the routectl-shipped accepted set. This is
    /// the documented operator escape hatch for AWS allowlist drift.
    #[test]
    fn invoke_provider_config_betas_bypass_filter() {
        // Arrange
        let mut cfg = fake_cfg_no_betas();
        cfg.anthropic_beta = vec![
            // A flag NOT in the operator-supplied `allowed_betas`
            // list, but the operator typed it into
            // `[providers.X] anthropic_beta` -- they assert it is
            // safe (e.g. AWS gated it for their account before the
            // next routectl release).
            "future-flag-2026-12-31".into(),
        ];
        let req = user_req();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: cfg-asserted flag survives.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            strs.contains(&"future-flag-2026-12-31"),
            "operator-asserted flag was filtered out: {strs:?}"
        );
    }

    /// When all input flags are rejected, the field is removed from the
    /// body entirely (not emitted as `anthropic_beta: []`). This matches
    /// the wire shape callers without any betas already produce.
    #[test]
    fn invoke_strips_field_when_filter_empties_array() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec!["oauth-2025-04-20".into(), "files-api-2025-04-14".into()];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("anthropic_beta").is_none(),
            "filter emptied the array but field survived: {body}"
        );
    }

    /// Dedup-vs-allowlist edge case. The Anthropic ingress's
    /// `merge_inbound_anthropic_beta_header` dedupes header-vs-body,
    /// but a direct caller (e.g. a library user constructing
    /// ChatRequest by hand) could supply duplicates. The filter must
    /// also dedup so a flag appearing twice in the canonical does not
    /// appear twice in the upstream body.
    #[test]
    fn invoke_preserves_dedup_when_header_and_body_share_flag() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec![
            "context-1m-2025-08-07".into(),
            "context-1m-2025-08-07".into(), // duplicate
            "claude-code-20250219".into(),
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: duplicate collapses; both unique accepted flags survive.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs.iter()
                .filter(|s| **s == "context-1m-2025-08-07")
                .count(),
            1,
            "duplicate flag was not deduped: {strs:?}"
        );
        assert!(strs.contains(&"claude-code-20250219"), "missing: {strs:?}");
    }

    /// `cfg.allowed_betas` is sourced from the global
    /// `[bedrock] allowed_betas` TOML field. Lets operators add flags
    /// AWS gated post-release, or remove flags AWS deprecated, without
    /// a routectl rebuild.
    #[test]
    fn invoke_allowed_betas_filters_against_operator_list() {
        // Arrange: a request with two flags. One is in the operator's
        // list (kept), the other is not (dropped).
        let mut cfg = fake_cfg_no_betas();
        cfg.allowed_betas = vec!["future-flag-2026-12-31".into()];
        let mut req = user_req();
        req.anthropic_beta = vec![
            // NOT in operator list: should be DROPPED.
            "context-1m-2025-08-07".into(),
            // In operator list: should be ACCEPTED.
            "future-flag-2026-12-31".into(),
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs,
            vec!["future-flag-2026-12-31"],
            "allowed_betas filter did not match operator list: {strs:?}"
        );
    }

    /// The Claude Code billing/attribution block must be dropped from the
    /// assembled Bedrock-Invoke body (Anthropic wire shape, Blocks form):
    /// AWS is a third-party upstream and must not receive the fingerprint.
    /// A normal sibling block survives.
    #[test]
    fn billing_block_dropped_from_invoke_body_keeps_normal_block() {
        use routectl_core::{SystemBlock, SystemContent};
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "x-anthropic-billing-header: v=1; fp=secret".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "you are helpful".into(),
                cache_control: None,
                citations: None,
            },
        ]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let sys = body["system"].as_array().expect("system survives as array");
        assert_eq!(sys.len(), 1, "only the normal block survives, got: {body}");
        assert_eq!(sys[0]["text"], "you are helpful");
    }

    /// A mid-string occurrence of the billing prefix in the Invoke body is
    /// a normal prompt and must be preserved.
    #[test]
    fn invoke_body_preserves_mid_string_billing_prefix() {
        use routectl_core::{SystemBlock, SystemContent};
        // Arrange
        let cfg = fake_cfg();
        let mut req = user_req();
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "intro x-anthropic-billing-header: not at start".into(),
            cache_control: None,
            citations: None,
        }]));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let sys = body["system"].as_array().expect("system survives as array");
        assert_eq!(sys.len(), 1);
        assert_eq!(
            sys[0]["text"],
            "intro x-anthropic-billing-header: not at start"
        );
    }

    /// The Anthropic `metadata` block carries client identity
    /// (`user_id`, `account_uuid`) and must NOT reach AWS -- a
    /// third-party upstream. It arrives via `provider_extras` (the
    /// Anthropic ingress forward-compat sweep), merges into the body
    /// inside `anthropic_api::request::normalize`, and is then stripped
    /// unconditionally on the Bedrock-Invoke seam.
    #[test]
    fn client_metadata_fingerprint_stripped_from_invoke_body() {
        use serde_json::json;
        // Arrange: client supplies a metadata fingerprint via
        // provider_extras and sets req.user (the canonical mirror).
        let cfg = fake_cfg();
        let mut req = user_req();
        req.user = Some("u-1".into());
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "u-1", "account_uuid": "a-2"}
        }));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: no metadata key, and no fingerprint substring anywhere.
        assert!(
            body.get("metadata").is_none(),
            "client metadata fingerprint leaked to Bedrock-Invoke body: {body}"
        );
        let serialized = body.to_string();
        assert!(
            !serialized.contains("u-1"),
            "user_id fingerprint leaked into Invoke body: {serialized}"
        );
        assert!(
            !serialized.contains("a-2"),
            "account_uuid fingerprint leaked into Invoke body: {serialized}"
        );
    }

    /// Mirror of the Converse `operator_metadata_survives_in_converse_bag`:
    /// operator-config `metadata` (set via
    /// `additional_model_request_fields`) SURVIVES the strip while a
    /// client `metadata` fingerprint (via `provider_extras`) is removed.
    /// Pins that `strip_client_metadata` restores from the OPERATOR
    /// CONFIG, not from the assembled body -- the security-critical
    /// property is that operator intent is sourced from cfg, never from
    /// the client-derived obj.
    #[test]
    fn operator_metadata_survives_while_client_metadata_stripped_from_invoke_body() {
        use serde_json::json;
        // Arrange: operator deliberately sets metadata via config; client
        // supplies its own metadata fingerprint via provider_extras.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({"metadata": {"trace": "operator-set"}}));
        let mut req = user_req();
        req.user = Some("u-1".into());
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "u-1", "account_uuid": "a-2"}
        }));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: operator metadata survives, client fingerprint is gone.
        assert_eq!(
            body["metadata"]["trace"], "operator-set",
            "operator-deliberate metadata must survive: {body}"
        );
        assert!(
            body["metadata"].get("user_id").is_none(),
            "client metadata fingerprint must not survive: {body}"
        );
        let serialized = body.to_string();
        assert!(
            !serialized.contains("u-1"),
            "user_id fingerprint leaked into Invoke body: {serialized}"
        );
        assert!(
            !serialized.contains("a-2"),
            "account_uuid fingerprint leaked into Invoke body: {serialized}"
        );
    }

    #[test]
    fn response_format_inherited_from_anthropic_normalize_reaches_invoke_body() {
        // Bedrock Invoke delegates body construction to the Anthropic-API
        // normalizer, so honoring req.response_format there means the
        // output_config.format field rides through onto the Invoke body
        // (it survives the anthropic_beta + body-field allowlist passes).
        // `name` is NOT carried: Anthropic's format object accepts only
        // `type` and `schema`, and AWS forwards the bag verbatim to the same
        // validator.
        let cfg = fake_cfg();
        let req = ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(1024),
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {"name": "widget", "schema": {"type": "object"}}
            })),
            ..Default::default()
        };

        let body = normalize_request(&cfg, &req).unwrap();

        assert_eq!(
            body["output_config"]["format"]["type"], "json_schema",
            "response_format must reach the Invoke body: {body}"
        );
        let fmt = body["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        let mut keys: Vec<&str> = fmt.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["schema", "type"],
            "Invoke output_config.format must carry exactly {{type, schema}}; got: {body}"
        );
    }

    /// An operator `additional_model_request_fields` entry is merged onto the
    /// assembled Invoke body AFTER the shared normalizer ran, and
    /// `output_config` is not on `is_bedrock_invoke_managed_key`, so that
    /// merge is a second write path for the rejected keys. The Invoke-side
    /// scrub closes it.
    #[test]
    fn operator_supplied_output_config_format_loses_name_and_strict() {
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "operator-widget",
                    "schema": {"type": "object"},
                    "strict": true
                }
            }
        }));
        let req = user_req();

        let body = normalize_request(&cfg, &req).unwrap();

        let fmt = body["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "operator-supplied name/strict must not reach the Invoke wire: {body}"
        );
        assert!(
            !body.to_string().contains("operator-widget"),
            "the operator's schema name must not reach the wire: {body}"
        );
    }

    /// The Invoke lane delegates body construction to the anthropic-api
    /// normalizer, so the shared sampling leak-guard fires here too --
    /// attributed to the Bedrock provider id.
    #[test]
    #[tracing_test::traced_test]
    fn sampling_fields_warn_once_naming_dropped_fields() {
        let cfg = fake_cfg();
        let mut req = user_req();
        req.seed = Some(42);
        req.top_logprobs = Some(5);

        let body = normalize_request(&cfg, &req).unwrap();

        assert!(body.get("seed").is_none(), "got: {body}");
        logs_assert(crate::sampling_drop_guard::test_support::exactly_one_sampling_warn);
        assert!(logs_contain("top_logprobs"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn no_sampling_warn_when_no_sampling_field_set() {
        let cfg = fake_cfg();
        let req = user_req();

        let _ = normalize_request(&cfg, &req).unwrap();

        assert!(!logs_contain("sampling fields dropped"));
    }

    /// Both sources of the unrepresentable `output_config.format` keys are
    /// live on this lane at once: the canonical `response_format` (scrubbed by
    /// the shared assembly) and an operator `additional_model_request_fields`
    /// object merged AFTER it (scrubbed here). The two records fold into ONE
    /// WARN -- emitting at both sites would report a single request twice, and
    /// an operator counting these lines would double-count every such request.
    #[test]
    #[tracing_test::traced_test]
    fn dropped_format_keys_warn_once_across_both_write_paths() {
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "operator-widget",
                    "schema": {"type": "object"},
                    "strict": true
                }
            }
        }));
        let mut req = user_req();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "caller-widget",
                "schema": {"type": "object"},
                "strict": true
            }
        }));

        let body = normalize_request(&cfg, &req).unwrap();

        let fmt = &body["output_config"]["format"];
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "neither key may reach the wire from either path: {body}"
        );
        logs_assert(|lines: &[&str]| {
            let warns = lines
                .iter()
                .filter(|l| {
                    l.contains(crate::anthropic_api::request::OUTPUT_FORMAT_KEY_DROP_EVENT)
                        && l.contains("WARN")
                })
                .count();
            if warns == 1 {
                return Ok(());
            }
            Err(format!(
                "one request must produce exactly one dropped-format-key WARN; got {warns}"
            ))
        });
    }

    /// An operator `additional_model_request_fields` object replaces the whole
    /// `output_config` the shared assembly repaired, so its schema arrives on
    /// this seam never having seen the `additionalProperties: false` pass.
    /// Anthropic (and AWS, which forwards to the same validator) rejects an
    /// object schema omitting the key, so the re-run is what makes the
    /// operator's schema shippable.
    #[test]
    fn operator_supplied_output_schema_is_repaired_on_the_invoke_body() {
        // Arrange
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "address": {
                                "type": "object",
                                "properties": {"street": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        }));
        let req = user_req();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "the merge-supplied root schema must ship repaired: {body}"
        );
        assert_eq!(
            schema["properties"]["address"]["additionalProperties"],
            json!(false),
            "a nested object omitting the key 400s even when the root carries it: {body}"
        );
    }

    /// The repair runs twice on this lane -- once inside the shared assembly,
    /// once on the merged body -- and a non-`false` value present on both
    /// passes must still yield ONE WARN. An operator counting these lines
    /// would otherwise double-count every such request.
    #[test]
    #[tracing_test::traced_test]
    fn additional_properties_forward_warns_once_across_both_repair_passes() {
        // Arrange
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "additionalProperties": true}
                }
            }
        }));
        let mut req = user_req();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "caller-widget",
                "schema": {"type": "object", "additionalProperties": true}
            }
        }));

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert_eq!(
            body["output_config"]["format"]["schema"]["additionalProperties"],
            json!(true),
            "a present non-false value is forwarded verbatim, never overwritten: {body}"
        );
        logs_assert(|lines: &[&str]| {
            let warns = lines
                .iter()
                .filter(|l| {
                    l.contains("output_schema_additional_properties_not_false")
                        && l.contains("WARN")
                })
                .count();
            if warns == 1 {
                return Ok(());
            }
            Err(format!(
                "one request must produce exactly one additionalProperties WARN; got {warns}"
            ))
        });
    }

    /// This lane egresses to a Bedrock endpoint, never to the genuine
    /// Anthropic host, so a routectl reasoning envelope inside a
    /// `redacted_thinking` block rides through byte-for-byte. Pins the
    /// EXPLICIT passthrough this call site selects, so a later edit that
    /// flips the shared normalizer's terminal-host argument here shows up
    /// as a failing test rather than as a silent wire change.
    #[test]
    fn reasoning_envelope_passes_through_verbatim_on_bedrock_invoke() {
        // Arrange
        use routectl_core::{ContentPart, KnownContentPart, reasoning_envelope};
        let envelope = reasoning_envelope::wrap(
            routectl_core::OPENAI_RESPONSES_V1,
            Some("rs_42"),
            "rsn_abc123-payload",
        );
        let cfg = fake_cfg();
        let mut req = user_req();
        req.messages = vec![
            req.messages[0].clone(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::RedactedThinking {
                        data: envelope.clone(),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let data: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "redacted_thinking")
            .filter_map(|b| b["data"].as_str())
            .collect();
        assert_eq!(
            data,
            vec![envelope.as_str()],
            "Bedrock Invoke must keep the envelope verbatim: {body}"
        );
    }
}
