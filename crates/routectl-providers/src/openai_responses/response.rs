//! Translate an OpenAI Responses response body into the canonical
//! `ChatResponse`.
//!
//! Walks `response.output[]` and produces:
//!   - flat assistant `text` (concatenated `output_text` blocks)
//!   - typed `reasoning_details` (summary + content + encrypted_content
//!     surfaces) using the `openai-responses-v1` format tag so multi-
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
use std::time::Duration;
use uuid::Uuid;

use routectl_core::{
    ChatResponse, Choice, ContentPart, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Result, Role, Usage,
};

use super::OPENAI_RESPONSES_FORMAT;
use super::response_types::{
    IncompleteDetails, ReasoningContent, ReasoningSummary, ResponseOutputItem,
    ResponsesOutputContent, ResponsesResponse, ResponsesUsage,
};

/// Translate a deserialized Responses body into canonical `ChatResponse`.
pub fn translate(provider_id: &str, body: ResponsesResponse) -> Result<ChatResponse> {
    let status = body.status.clone();
    let incomplete_reason = body
        .incomplete_details
        .as_ref()
        .and_then(|d: &IncompleteDetails| d.reason.clone());

    let (text, reasoning_details, tool_calls, parts, has_function_call) =
        walk_output(provider_id, &body.output)?;

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
        upstream_meta: None,
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
    output: &[ResponseOutputItem],
) -> Result<(
    String,
    Vec<ReasoningDetail>,
    Option<Vec<Value>>,
    Vec<ContentPart>,
    bool,
)> {
    let mut text_parts: Vec<String> = Vec::new();
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
                            format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
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
                                format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
                                index: Some(detail_index),
                                payload: json!({"text": text}),
                            });
                            detail_index += 1;
                        }
                        ReasoningContent::ReasoningEncrypted { encrypted_content } => {
                            reasoning_details.push(ReasoningDetail {
                                kind: ReasoningDetailKind::Encrypted,
                                id: Some(canonical_id.clone()),
                                format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
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
                        format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
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
                    .unwrap_or_else(|_| Value::String(arguments.clone()));
                parts.push(ContentPart::Known(KnownContentPart::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: input_value,
                    cache_control: None,
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
                    type_tag = %type_tag,
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
/// triple (no Anthropic-style cache splitting at the top level --
/// cached input lives in `input_tokens_details.cached_tokens` instead).
/// We surface cached tokens as `cache_read_input_tokens` so OpenAI
/// clients see consistent cache stats across providers.
fn translate_usage(u: &ResponsesUsage) -> Usage {
    let cache_read = u
        .input_tokens_details
        .as_ref()
        .and_then(|v| v.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
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
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cache_read,
        cache_creation: None,
        server_tool_use: None,
        extras: Default::default(),
    };
    usage.derive_total_if_absent();
    usage
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

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;
