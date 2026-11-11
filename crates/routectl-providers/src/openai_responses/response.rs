//! Translate an OpenAI Responses response body into the canonical
//! `ChatResponse`.
//!
//! Walks `response.output[]` and produces:
//!   - flat assistant `text` (concatenated `output_text` blocks)
//!   - typed `reasoning_details` (summary + content + encrypted_content
//!     surfaces) tagged with the caller lane's format tag so multi-
//!     turn callers echoing reasoning back can be routed correctly
//!   - OpenAI-shape `tool_calls` derived from `function_call` items
//!   - `parts` preserving every block in arrival order so
//!     `select_message_content` can collapse to flat Text when only
//!     text blocks emitted and emit Parts otherwise (multimodal /
//!     forward-compat / refusal preservation)
//!
//! `finish_reason` mapping:
//!   - status "completed" + no function_call output      -> "stop"
//!   - status "completed" + at least one function_call   -> "tool_calls"
//!   - status "incomplete" + reason "max_output_tokens"  -> "length"
//!   - status "incomplete" + reason "content_filter"     -> "content_filter"
//!   - status "failed" / "cancelled"                     -> "error"
//!   - anything else                                     -> passthrough
//!     (mirrors the Bedrock Converse unknown-stop-reason policy).

use chrono::Utc;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::time::Duration;
use uuid::Uuid;

use routectl_core::{
    CacheCreation, ChatRequest, ChatResponse, Choice, ContentPart, Error, KnownContentPart,
    Message, MessageContent, ReasoningDetail, ReasoningDetailKind, RequestObservation,
    ResponsesEnvelope, Result, Role, UpstreamMeta, Usage, parse_cache_lifetime_member,
    sanitize_for_log, wire_check::WireTap,
};

use super::empty_optional::EmptyOptionalFilter;
use super::response_types::{
    IncompleteDetails, ReasoningContent, ReasoningSummary, ResponseOutputItem,
    ResponsesOutputContent, ResponsesResponse, ResponsesUsage,
};
use super::{AuthKind, lane_format_tag};

/// Translate a deserialized Responses body into canonical `ChatResponse`.
///
/// `auth_kind` selects the lane-faithful reasoning format tag stamped on
/// every emitted `ReasoningDetail`; the lanes validate replay
/// incompatibly, so a shared tag would make an artifact unreplayable on
/// the lane that minted it.
pub fn translate(
    provider_id: &str,
    auth_kind: AuthKind,
    body: ResponsesResponse,
) -> Result<ChatResponse> {
    translate_filtered(provider_id, auth_kind, body, None)
}

/// [`translate`] for the response to `req`. On the codex lane, a function
/// call's empty optional arguments (see the `empty_optional` module) are
/// removed against the tools `req` declared, and the removal is counted once
/// per client request and declared on the response leg. Synchronous: the
/// observation scope it opens must never span an `.await`.
pub fn translate_for_request(
    provider_id: &str,
    auth_kind: AuthKind,
    body: ResponsesResponse,
    req: &ChatRequest,
) -> Result<ChatResponse> {
    if !auth_kind.is_codex_lane() {
        return translate(provider_id, auth_kind, body);
    }
    let tools = req.tools.as_deref().unwrap_or_default();
    let mut filter = EmptyOptionalFilter::from_tools(tools);
    let resp = translate_filtered(provider_id, auth_kind, body, Some(&mut filter))?;
    report_empty_optionals_removed(
        provider_id,
        req.routectl_internal.observation.as_ref(),
        crate::wire_declare::tap(req),
        filter.removed(),
    );
    Ok(resp)
}

/// Log, count and declare the empty optional arguments one response had
/// removed, once per client request through `observation`. Synchronous: the
/// observation scope it opens must never span an `.await`.
pub(super) fn report_empty_optionals_removed(
    provider_id: &str,
    observation: Option<&RequestObservation>,
    wire_tap: Option<&WireTap>,
    removed: &[(String, usize)],
) {
    if removed.is_empty() {
        return;
    }
    let per_tool: Vec<String> = removed
        .iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect();
    tracing::debug!(
        provider = provider_id,
        lane = super::LANE,
        removed = %sanitize_for_log(&per_tool.join(",")),
        calls = removed.len(),
        "openai-responses: dropping empty optional function call arguments"
    );
    let _observation = crate::translation_drop_metrics::ObservationScope::enter_slot(observation);
    // TRANSLATION-DROP: lane=openai-responses class=codex_empty_optional_dropped test=two_empty_optionals_in_one_response_count_once
    crate::translation_drop_metrics::record_translation_drop(
        super::LANE,
        "codex_empty_optional_dropped",
    );
    // Records only on a tap that compares the response leg: a Responses
    // client, whose output carries the call's arguments string as rewritten.
    crate::wire_declare::declare_response(
        wire_tap,
        routectl_core::wire_check::EditSite::DialectFieldStrip,
        EMPTY_OPTIONAL_ARGUMENTS_PATH,
        routectl_core::wire_check::EditKind::Rewrite,
    );
}

/// Where a Responses client's body carries the arguments a codex-lane call
/// had its empty optionals removed from.
pub const EMPTY_OPTIONAL_ARGUMENTS_PATH: &str = "/output/*/arguments";

fn translate_filtered(
    provider_id: &str,
    auth_kind: AuthKind,
    body: ResponsesResponse,
    filter: Option<&mut EmptyOptionalFilter>,
) -> Result<ChatResponse> {
    let status = body.status.clone();
    let incomplete_reason = body
        .incomplete_details
        .as_ref()
        .and_then(|d: &IncompleteDetails| d.reason.clone());

    let (text, reasoning_details, tool_calls, parts, has_function_call) =
        walk_output(provider_id, auth_kind, &body.output, filter)?;

    let finish_reason = map_finish_reason(
        status.as_deref(),
        incomplete_reason.as_deref(),
        has_function_call,
    );
    let usage = body.usage.as_ref().map(translate_usage);

    let content = select_message_content(text, parts);
    let message = Message {
        refusal: None,
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details,
        name: None,
        tool_call_id: None,
        tool_calls,
        extras: serde_json::Map::new(),
    };

    let id = if body.id.is_empty() {
        Uuid::now_v7().to_string()
    } else {
        body.id
    };
    let created = if body.created_at == 0 {
        Utc::now().timestamp()
    } else {
        body.created_at
    };

    let upstream_meta = first_message_item_id(&body.output).map(|item_id| {
        UpstreamMeta::from_responses_envelope(ResponsesEnvelope::of_message_item_id(item_id))
    });

    Ok(ChatResponse {
        id,
        model: body.model,
        created,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message,
            finish_reason,
            matched_stop_sequence: None,
        }],
        usage,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta,
    })
}

/// The id of the first `message` output item, when the upstream sent a
/// non-empty one. The render emits a single message item, so the first
/// one's id is the one it echoes.
fn first_message_item_id(output: &[ResponseOutputItem]) -> Option<String> {
    output.iter().find_map(|item| match item {
        ResponseOutputItem::Message { id, .. } if !id.is_empty() => Some(id.clone()),
        _ => None,
    })
}

/// Walk `response.output[]`. Returns:
///   - concatenated `output_text` (flat text)
///   - reasoning_details (summary + content + encrypted_content)
///   - OpenAI-shape tool_calls (None if no function_call items)
///   - parts vector with every block in arrival order
///   - whether at least one function_call was present (drives the
///     `finish_reason` mapping above)
#[allow(clippy::type_complexity)] // multi-tuple return matches the wire walk; alias would obscure intent
fn walk_output(
    provider_id: &str,
    auth_kind: AuthKind,
    output: &[ResponseOutputItem],
    mut filter: Option<&mut EmptyOptionalFilter>,
) -> Result<(
    String,
    Vec<ReasoningDetail>,
    Option<Vec<Value>>,
    Vec<ContentPart>,
    bool,
)> {
    let mut text_parts: Vec<String> = Vec::new();
    let format_tag = lane_format_tag(auth_kind);
    let mut reasoning_details: Vec<ReasoningDetail> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut parts: Vec<ContentPart> = Vec::new();
    let mut detail_index: u32 = 0;
    let mut has_function_call = false;

    for item in output {
        match item {
            ResponseOutputItem::Message { content, .. } => {
                for c in content {
                    match c {
                        ResponsesOutputContent::OutputText { text, .. } => {
                            text_parts.push(text.clone());
                            parts.push(ContentPart::Known(KnownContentPart::Text {
                                text: text.clone(),
                                citations: None,
                                cache_control: None,
                                extras: serde_json::Map::new(),
                            }));
                        }
                        ResponsesOutputContent::Refusal { refusal } => {
                            // Surface as a typed Other so downstream
                            // callers see the refusal as a distinct
                            // block rather than swallowed text. Mirrors
                            // the openai-compat refusal mapping.
                            let mut extras = serde_json::Map::new();
                            extras.insert("refusal".to_string(), Value::String(refusal.clone()));
                            parts.push(ContentPart::Other {
                                type_tag: "refusal".to_string(),
                                cache_control: None,
                                extras,
                            });
                        }
                        ResponsesOutputContent::Other => {
                            tracing::debug!(
                                provider = provider_id,
                                "openai-responses: unknown message content block dropped"
                            );
                        }
                    }
                }
            }
            ResponseOutputItem::Reasoning {
                id: item_id,
                summary,
                content,
                encrypted_content,
                ..
            } => {
                // Propagate the upstream-stable item id (e.g. "rs_1")
                // to every detail emitted for this item. The egress
                // groups by id when re-emitting Reasoning input items
                // on the next turn so the wire round-trip is
                // byte-stable (one upstream item -> one replay item).
                // Mint a UUID only if the upstream omitted an id
                // (defensive; reasoning items always carry one in
                // practice).
                let canonical_id = if item_id.is_empty() {
                    Uuid::new_v4().to_string()
                } else {
                    item_id.clone()
                };
                for s in summary {
                    if let ReasoningSummary::SummaryText { text } = s {
                        reasoning_details.push(ReasoningDetail {
                            kind: ReasoningDetailKind::Summary,
                            id: Some(canonical_id.clone()),
                            format: Some(format_tag.to_string()),
                            index: Some(detail_index),
                            payload: json!({"text": text}),
                        });
                        detail_index += 1;
                    }
                }
                for c in content {
                    match c {
                        // `reasoning_text` and the plain `text` alias
                        // collapse to the same canonical Text detail
                        // (codex's ReasoningItemContent treats them as
                        // sibling tags carrying identical payload).
                        ReasoningContent::ReasoningText { text }
                        | ReasoningContent::Text { text } => {
                            reasoning_details.push(ReasoningDetail {
                                kind: ReasoningDetailKind::Text,
                                id: Some(canonical_id.clone()),
                                format: Some(format_tag.to_string()),
                                index: Some(detail_index),
                                payload: json!({"text": text}),
                            });
                            detail_index += 1;
                        }
                        ReasoningContent::ReasoningEncrypted { encrypted_content } => {
                            reasoning_details.push(ReasoningDetail {
                                kind: ReasoningDetailKind::Encrypted,
                                id: Some(canonical_id.clone()),
                                format: Some(format_tag.to_string()),
                                index: Some(detail_index),
                                payload: json!({"encrypted_content": encrypted_content}),
                            });
                            detail_index += 1;
                        }
                        ReasoningContent::Other => {
                            tracing::debug!(
                                provider = provider_id,
                                "openai-responses: unknown reasoning content block dropped"
                            );
                        }
                    }
                }
                // The replay signature (`encrypted_content` on the
                // Reasoning item itself, NOT the inner content block)
                // rides on its own Encrypted detail so the multi-turn
                // round-trip works -- codex's arc_monitor.rs:325-336
                // re-injects this verbatim on the next turn.
                if let Some(sig) = encrypted_content
                    && !sig.is_empty()
                {
                    reasoning_details.push(ReasoningDetail {
                        kind: ReasoningDetailKind::Encrypted,
                        id: Some(canonical_id.clone()),
                        format: Some(format_tag.to_string()),
                        index: Some(detail_index),
                        payload: json!({"encrypted_content": sig}),
                    });
                    detail_index += 1;
                }
            }
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                has_function_call = true;
                let arguments = zero_arg_as_empty_object(arguments);
                let arguments = match filter.as_deref_mut() {
                    Some(filter) => filter.apply(name, arguments),
                    None => Cow::Borrowed(arguments),
                };
                let arguments = arguments.as_ref();
                tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }));
                // Preserve verbatim on parse failure rather than
                // dropping to Null: the Responses wire contract is
                // `arguments` as a JSON string, but some upstream
                // edge cases ship a partially-formed string and the
                // egress should re-emit the original text so the
                // model sees the same input it produced.
                let input_value: Value = serde_json::from_str(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.to_string()));
                parts.push(ContentPart::Known(KnownContentPart::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: input_value,
                    cache_control: None,
                    extras: serde_json::Map::new(),
                }));
            }
            ResponseOutputItem::Other(raw) => {
                // Forward-compat passthrough. Lift the `type` tag and
                // surface the remaining fields verbatim in
                // `ContentPart::Other.extras` so a future Anthropic /
                // Bedrock egress (or a Responses round-trip that
                // re-emits the same shape) can reconstruct the block.
                let (type_tag, extras) = split_other_value(raw);
                tracing::debug!(
                    provider = provider_id,
                    type_tag = %sanitize_for_log(&type_tag),
                    "openai-responses: forward-compat output_item preserved via ContentPart::Other"
                );
                parts.push(ContentPart::Other {
                    type_tag,
                    cache_control: None,
                    extras,
                });
            }
        }
    }

    let text = text_parts.join("");
    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    Ok((
        text,
        reasoning_details,
        tool_calls_opt,
        parts,
        has_function_call,
    ))
}

/// Choose `MessageContent::Text` over `Parts` when every emitted part
/// is a plain Text entry. Reasoning blocks don't appear in `parts`
/// (they ride on `reasoning_details`) so a text + reasoning response
/// still collapses to Text here. Tool calls + refusals + unknown items
/// force Parts so they survive end-to-end.
fn select_message_content(text: String, parts: Vec<ContentPart>) -> MessageContent {
    let only_text = parts
        .iter()
        .all(|p| matches!(p, ContentPart::Known(KnownContentPart::Text { .. })));
    if only_text {
        MessageContent::Text(text)
    } else {
        MessageContent::Parts(parts)
    }
}

/// Maps upstream status + incomplete_reason to a canonical finish_reason.
pub fn map_finish_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
    has_function_call: bool,
) -> Option<String> {
    let s = status?;
    let mapped = match s {
        "completed" if has_function_call => "tool_calls",
        "completed" => "stop",
        "incomplete" => match incomplete_reason {
            Some("max_output_tokens") => "length",
            Some("content_filter") => "content_filter",
            // Unknown reason -- pass through the literal so callers can
            // dispatch on it. None falls through to a bare "incomplete"
            // which mirrors the Converse unknown-stop-reason policy.
            Some(other) => other,
            None => "incomplete",
        },
        "failed" | "cancelled" => "error",
        other => other,
    };
    Some(mapped.to_string())
}

/// Translate Responses `usage` to canonical `Usage`. The Responses
/// shape has a clean `input_tokens` / `output_tokens` / `total_tokens`
/// triple; cache usage lives in `input_tokens_details` (see
/// [`cache_details`]). `input_tokens` already includes cache reads and
/// writes, matching the cache-inclusive canonical `prompt_tokens`.
fn translate_usage(u: &ResponsesUsage) -> Usage {
    let cache = cache_details(u.input_tokens_details.as_ref());
    let reasoning_tokens = u
        .output_tokens_details
        .as_ref()
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
    let mut usage = Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.total_tokens,
        reasoning_tokens,
        cache_creation_input_tokens: cache.write_total,
        cache_read_input_tokens: cache.read,
        cache_creation: cache.write_split,
        server_tool_use: None,
        extras: Default::default(),
    };
    usage.derive_total_if_absent();
    usage
}

/// Prompt-cache usage read from one Responses `input_tokens_details`
/// object. Each field is `None` when the upstream did not report it; a
/// reported zero stays `Some(0)`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CacheDetails {
    /// `cached_tokens`: tokens read from the cache.
    pub(super) read: Option<u32>,
    /// `cache_write_tokens`: tokens written to the cache, any lifetime.
    /// Without it, the sum of the lifetime split.
    pub(super) write_total: Option<u32>,
    /// Writes per lifetime, from detail members whose name carries a
    /// duration (`cache_write_1h_tokens`). `None` when none parsed.
    pub(super) write_split: Option<CacheCreation>,
}

/// Read the cache counts out of `input_tokens_details`. The complete and
/// the streaming translators both call this, so the two paths cannot
/// disagree about a response's cache usage.
pub(super) fn cache_details(details: Option<&Value>) -> CacheDetails {
    let Some(details) = details.and_then(Value::as_object) else {
        return CacheDetails::default();
    };
    let count = |key: &str| details.get(key).and_then(Value::as_u64).map(saturate_u32);
    let write_split = lifetime_split(details);
    let write_total = count("cache_write_tokens").or_else(|| {
        write_split
            .as_ref()
            .map(|split| saturate_u32(split.total()))
    });
    CacheDetails {
        read: count("cached_tokens"),
        write_total,
        write_split,
    }
}

/// The duration-named members of `details` as a lifetime split. Admission
/// (token value shape, the lifetime cap) is the core type's own, so a
/// member it refuses counts as unknown lifetime inside the write total.
fn lifetime_split(details: &serde_json::Map<String, Value>) -> Option<CacheCreation> {
    let members: serde_json::Map<String, Value> = details
        .iter()
        .filter(|(name, _)| parse_cache_lifetime_member(name).is_some())
        .map(|(name, tokens)| (name.clone(), tokens.clone()))
        .collect();
    if members.is_empty() {
        return None;
    }
    serde_json::from_value::<CacheCreation>(Value::Object(members))
        .ok()
        .filter(|split| !split.is_empty())
}

fn saturate_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Split a forward-compat `ResponseOutputItem::Other` Value into the
/// `type` discriminant + the remaining fields. When the raw JSON
/// isn't an object (or is missing `type`), fall back to a synthetic
/// `"unknown"` tag and empty extras so the downstream `ContentPart`
/// stays well-formed.
fn split_other_value(raw: &Value) -> (String, serde_json::Map<String, Value>) {
    let Value::Object(map) = raw else {
        return ("unknown".to_string(), serde_json::Map::new());
    };
    let mut extras = map.clone();
    let type_tag = extras
        .remove("type")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    (type_tag, extras)
}

/// Lift a reset hint from a Codex / ChatGPT usage-limit error body.
///
/// The ChatGPT-Codex backend signals the 5-hour usage cap with an
/// `error.type == "usage_limit_reached"` body that carries the reset
/// either as a relative `resets_in_seconds` count or an absolute
/// `resets_at` unix epoch. Prefer the relative form; fall back to the
/// absolute form (computed against `now`, clamped to >= 0). Any other
/// error type -- or a body missing both fields -- yields `None`, so a
/// non-usage-limit failure never parks the provider.
pub fn codex_reset_hint(err_body: &Value) -> Option<Duration> {
    let is_usage_limit = err_body
        .pointer("/error/type")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "usage_limit_reached");
    if !is_usage_limit {
        return None;
    }
    if let Some(secs) = err_body
        .pointer("/error/resets_in_seconds")
        .and_then(Value::as_u64)
    {
        return Some(Duration::from_secs(secs));
    }
    let resets_at = err_body
        .pointer("/error/resets_at")
        .and_then(Value::as_u64)?;
    // Absolute epoch -> delay from now. Guard a negative `now` (clock
    // before the unix epoch is impossible in practice but keeps the
    // cast total) and saturate so a past `resets_at` clamps to zero.
    let now = u64::try_from(Utc::now().timestamp()).unwrap_or(0);
    Some(Duration::from_secs(resets_at.saturating_sub(now)))
}

/// Build an `Error::Upstream` for a `status:"failed"` body. Lifts the
/// `error.message` field when present so the operator-facing error
/// is informative; falls back to a generic string otherwise.
pub fn upstream_error_from_failed(provider_id: &str, body: &ResponsesResponse) -> Error {
    let msg = body
        .error
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
        .map_or_else(
            || "openai-responses: response.status=failed".to_string(),
            std::string::ToString::to_string,
        );
    // `body.error` is the bare error object (`{type, message, ...}`);
    // `codex_reset_hint` expects it nested under `/error`, so wrap it.
    let retry_after = body
        .error
        .as_ref()
        .map(|e| json!({ "error": e }))
        .as_ref()
        .and_then(codex_reset_hint);
    Error::upstream_with_retry_after(provider_id, 0, msg, retry_after)
}

/// The Responses wire ships `arguments: ""` for a zero-argument call;
/// `{}` keeps the arguments parseable JSON (and an object, which the
/// Anthropic `tool_use.input` requires) for every downstream client.
pub(super) const fn zero_arg_as_empty_object(arguments: &str) -> &str {
    if arguments.is_empty() {
        "{}"
    } else {
        arguments
    }
}

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;
