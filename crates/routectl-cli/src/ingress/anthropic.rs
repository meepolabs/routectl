//! Anthropic Messages API ingress (`POST /v1/messages`).
//!
//! Translates Anthropic Messages requests to the canonical `ChatRequest`
//! and back. The canonical type is already designed to absorb Anthropic
//! shape losslessly (typed `ContentPart`, typed `SystemContent`, typed
//! `ToolDef`, top-level `cache_control`, `anthropic_beta`), so the
//! request side is mostly a serde pass-through with a small fix-up
//! step for `thinking`, `metadata`, and `top_k`.
//!
//! Streaming convention: Anthropic emits a sequence of named events
//! (`message_start`, `content_block_start`, `content_block_delta`,
//! `content_block_stop`, `message_delta`, `message_stop`, `ping`).
//! `render_chunk` runs a state machine that tracks the currently
//! open content block and emits the right sequence as canonical
//! chunks arrive. See `AnthropicStreamState`.

use std::any::Any;

use axum::http::HeaderMap;
use routectl_core::cache_control::{self, Breakpoint, BreakpointPosition};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ContentPart, Error, Message, MessageContent,
    ReasoningConfig, ReasoningDetail, Result,
};
use serde_json::{json, Map, Value};

use super::{read_alias_header, IngressAdapter, IngressStreamState, SseEvent};

/// The format tag the canonical layer uses for Anthropic-shape
/// reasoning details (from the Anthropic-API egress on the upstream
/// side). The Anthropic ingress renderer no longer filters by format
/// when emitting thinking blocks (every dialect's reasoning_details
/// surface to cc), but this constant is still used in tests that build
/// canonical responses with Anthropic-format details to assert renderer
/// behavior.
#[cfg(test)]
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

#[derive(Debug, Default)]
pub struct AnthropicIngress;

// ---------------------------------------------------------------------------
// Streaming state
// ---------------------------------------------------------------------------

/// Identity of the currently-open Anthropic content block. Tagged
/// with the canonical reasoning-detail index for `Thinking` so two
/// distinct upstream thinking blocks (`detail_index=0` then
/// `detail_index=1`) emit as two separate Anthropic blocks rather
/// than getting merged into one. Without this, multi-block thinking
/// streams reserialize as a single block and break protocol fidelity
/// with downstream consumers that count blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlockKind {
    Text,
    Thinking { detail_index: u32 },
    ToolUse,
}

#[derive(Debug, Default)]
pub struct AnthropicStreamState {
    /// Have we emitted `message_start`? Set on first chunk.
    started: bool,
    /// True once we've emitted `message_stop`.
    finished: bool,
    /// Currently open content block (if any).
    open: Option<(usize, OpenBlockKind)>,
    /// Next block index to allocate.
    next_index: usize,
    /// Stream identifiers cached from first chunk.
    msg_id: Option<String>,
    msg_model: Option<String>,
    /// Buffered tool-use deltas keyed by tool call index. We flush them
    /// sequentially at the end of the stream so OpenAI-style interleaved
    /// tool call chunks still produce valid Anthropic block ordering.
    tool_blocks: Vec<ToolBlockState>,
    /// Finish reason seen on a chunk that arrived WITHOUT usage. Many
    /// openai-compat hosts (OpenAI, OpenRouter, vLLM when
    /// `stream_options.include_usage=true`) emit the finish_reason on
    /// one chunk and a final usage-only chunk after it. The Anthropic
    /// streaming spec emits one `message_delta` carrying BOTH stop_reason
    /// and usage, then `message_stop`. We buffer the fr until the usage
    /// chunk arrives (or `render_eos` runs) so we emit the combined
    /// message_delta + message_stop in the correct order.
    pending_finish_reason: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct ToolBlockState {
    id: String,
    name: String,
    partial_json: String,
}

impl IngressStreamState for AnthropicStreamState {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body into the canonical
/// `ChatRequest`. Most fields land via direct serde because canonical
/// types accept Anthropic shape; the explicit fix-ups handle:
///
/// - `thinking` -> `reasoning` (canonical `ReasoningConfig`).
/// - `metadata.user_id` -> `user`.
/// - `output_format` (legacy) -> `output_config.format` (current).
/// - `stop_sequences` -> `stop` (canonical name).
/// - `model` is overridden by the `x-routectl-alias` request header
///   when present; otherwise passes through verbatim. v0.6.0 moved
///   alias resolution from per-ingress maps to the router (see
///   `routectl_router::Router::resolve_v6_alias`).
///
/// Any top-level field the canonical doesn't model -- for example
/// Anthropic-only knobs `top_k`, `service_tier`, `output_config`,
/// `container`, `inference_geo`, `context_management`, `context_hint`,
/// `speed`, `diagnostics`, `mcp_servers`, plus anything Anthropic
/// adds in the future -- is swept into `provider_extras` so the
/// egress merges it back into the upstream body verbatim. This keeps
/// routectl forward-compatible: Anthropic ships a new top-level
/// field, claude-code starts emitting it, routectl forwards it
/// without a code change. Without this sweep, serde's
/// silently-drop-unknown behavior would lose the field at the ingress
/// boundary (the original `output_format` bug).
fn translate_request(headers: &HeaderMap, mut body: Value) -> Result<ChatRequest> {
    let obj = body.as_object_mut().ok_or_else(|| {
        Error::Validation("anthropic ingress: request body is not an object".into())
    })?;

    // Pull out fields that need explicit translation BEFORE we let
    // serde have a go at the rest. Each of these is either renamed
    // or split across multiple canonical fields, so the catch-all
    // sweep must not see them.
    let thinking = obj.remove("thinking");
    let metadata = obj.remove("metadata");
    let output_format = obj.remove("output_format");
    if let Some(stops) = obj.remove("stop_sequences") {
        obj.insert("stop".into(), stops);
    }

    // Catch-all sweep: anything left that isn't a canonical
    // ChatRequest field gets stashed in provider_extras. This is the
    // forward-compat seam -- new Anthropic fields land in extras and
    // the egress's merge_provider_extras forwards them upstream
    // without ever needing to touch the ingress strip list.
    let extras = sweep_anthropic_extras(obj);
    let mut extras = match extras {
        Value::Object(map) => map,
        _ => Map::new(),
    };

    // Fold legacy top-level `output_format` into `output_config.format`.
    // claude-code 2.1.x sends one of three shapes: top-level
    // `output_format` (deprecated), nested `output_config.format`
    // (current), or both (claude-code itself logs a warning when both
    // are present and prefers the nested form -- mirror that here).
    let output_config = merge_output_format(extras.remove("output_config"), output_format);
    if let Some(oc) = output_config {
        extras.insert("output_config".into(), oc);
    }

    let mut req: ChatRequest = serde_json::from_value(body)
        .map_err(|e| Error::Validation(format!("anthropic ingress: invalid body: {e}")))?;

    // v0.6.0: alias resolution lives entirely in the router. The
    // ingress only honors the `x-routectl-alias` header override
    // (otherwise the wire `model` value passes through verbatim).
    if let Some(alias) = read_alias_header(headers) {
        req.model = alias;
    }

    // Lift the inbound `anthropic-beta` HTTP header into canonical
    // `req.anthropic_beta`. The Anthropic TypeScript SDK translates
    // `betas: [...]` (a typed SDK option) into the
    // `anthropic-beta: a,b,c` HTTP header, so claude-code's first-party
    // betas (context-management, prompt-cache-1h, adaptive-thinking,
    // ...) arrive on the header surface. The egress emits
    // `anthropic_beta` as a body field (Anthropic accepts either), so
    // routing through canonical normalizes both wire shapes onto one
    // egress path. Comma-separated header values are split + trimmed
    // and merged with any existing body-level `anthropic_beta`,
    // preserving order and dropping duplicates.
    merge_inbound_anthropic_beta_header(headers, &mut req);

    // Translate thinking config.
    if let Some(t) = thinking {
        req.reasoning = Some(translate_thinking(&t));
    }

    // Translate metadata.user_id AND preserve the full metadata
    // object so it round-trips to Anthropic-shape egresses verbatim.
    // Without the round-trip preservation, request attribution
    // (`metadata.session_id`, custom keys some operators set) is
    // silently dropped at the canonical seam. Bedrock-Invoke and
    // anthropic-api both honor `metadata` when present in the
    // provider_extras-merged body. `metadata` is in the
    // pass-through key list (`is_canonical_request_key` returns false
    // for it), so the egress's `merge_provider_extras` lets it through.
    if let Some(m) = metadata {
        if let Some(uid) = m
            .as_object()
            .and_then(|o| o.get("user_id"))
            .and_then(|v| v.as_str())
        {
            req.user = Some(uid.to_string());
        }
        extras.insert("metadata".into(), m);
    }

    if !extras.is_empty() {
        req.provider_extras = Some(Value::Object(extras));
    }

    // Run cache_control validation up front so a malformed request
    // returns 400 before it touches the egress.
    validate_request_cache_control(&req)?;

    Ok(req)
}

/// Field names the canonical `ChatRequest` deserializes directly from
/// an Anthropic-shape wire body. Anything NOT in this list and not
/// otherwise pre-handled (`thinking`, `metadata`, `output_format`,
/// `stop_sequences`) is Anthropic-only and gets stashed in
/// `provider_extras`.
///
/// Keep this list in sync with `routectl_core::schema::ChatRequest`
/// field names. The build pins the contract: a missing entry here is
/// a silent drop, exactly like the bug that motivated this design.
const CANONICAL_CHAT_REQUEST_WIRE_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "temperature",
    "top_p",
    "max_tokens",
    "stop",
    "stream",
    "n",
    "seed",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "presence_penalty",
    "frequency_penalty",
    "user",
    "tools",
    "tool_choice",
    "response_format",
    "cache_control",
    "anthropic_beta",
    "reasoning",
    "chat_template_kwargs",
    "provider_extras",
];

/// Move every key not in `CANONICAL_CHAT_REQUEST_WIRE_FIELDS` out of
/// `obj` and return them as a JSON object. The caller threads the
/// returned object into `provider_extras`.
fn sweep_anthropic_extras(obj: &mut Map<String, Value>) -> Value {
    let extra_keys: Vec<String> = obj
        .keys()
        .filter(|k| !CANONICAL_CHAT_REQUEST_WIRE_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    let mut extras = Map::new();
    for k in extra_keys {
        if let Some(v) = obj.remove(&k) {
            extras.insert(k, v);
        }
    }
    Value::Object(extras)
}

/// Parse the inbound `anthropic-beta` HTTP header(s) and merge the
/// values into `req.anthropic_beta` (deduplicated, preserving order).
/// Multiple header instances and comma-separated values within one
/// instance both expand correctly.
fn merge_inbound_anthropic_beta_header(headers: &HeaderMap, req: &mut ChatRequest) {
    let mut all: Vec<String> = req.anthropic_beta.clone();
    for hv in headers.get_all("anthropic-beta").iter() {
        let Ok(s) = hv.to_str() else {
            tracing::warn!("anthropic ingress: anthropic-beta header is not valid UTF-8; ignoring");
            continue;
        };
        for piece in s.split(',') {
            let trimmed = piece.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !all.iter().any(|existing| existing == trimmed) {
                all.push(trimmed.to_string());
            }
        }
    }
    req.anthropic_beta = all;
}

/// Fold legacy top-level `output_format` into `output_config.format`.
///
/// Anthropic's current wire shape for structured outputs is
/// `output_config.format`; the top-level `output_format` is the
/// SDK-side field name claude-code itself documents as deprecated.
/// If both shapes arrive on the same request, prefer the nested
/// (current) form and drop the legacy one with a WARN so the
/// operator sees the conflict. Otherwise rewrite the legacy field
/// into the nested form, preserving any `output_config.effort` the
/// caller already set.
fn merge_output_format(
    output_config: Option<Value>,
    output_format: Option<Value>,
) -> Option<Value> {
    let Some(legacy) = output_format else {
        return output_config;
    };
    let nested_format_present = output_config
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|o| o.get("format"))
        .is_some();
    if nested_format_present {
        tracing::warn!(
            "anthropic ingress: both output_format and output_config.format provided; \
             dropping the deprecated output_format"
        );
        return output_config;
    }
    // No conflict. Promote the legacy field into output_config.format,
    // preserving any other output_config keys (like `effort`).
    let mut obj = match output_config {
        Some(Value::Object(o)) => o,
        Some(other) => {
            tracing::warn!(
                kind = %value_type_name(&other),
                "anthropic ingress: output_config is not an object; replacing with \
                 {{format: <output_format>}} so structured output reaches upstream"
            );
            Map::new()
        }
        None => Map::new(),
    };
    obj.insert("format".into(), legacy);
    Some(Value::Object(obj))
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn translate_thinking(t: &Value) -> ReasoningConfig {
    let kind = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Anthropic's `budget_tokens` is JSON-int (effectively u64) but
    // the canonical `ReasoningConfig.max_tokens` is u32. A naked cast
    // would silently truncate values above u32::MAX (~4.29B) -- not
    // reachable today since Anthropic caps the field at 100k, but
    // saturating to u32::MAX with a WARN keeps the request consistent
    // with what the caller asked for and surfaces bizarre input
    // instead of corrupting it.
    let budget = t.get("budget_tokens").and_then(|v| v.as_u64()).map(|n| {
        if n > u64::from(u32::MAX) {
            tracing::warn!(
                requested = n,
                capped = u32::MAX,
                "anthropic ingress: budget_tokens exceeds u32::MAX; saturating",
            );
            u32::MAX
        } else {
            n as u32
        }
    });
    match kind {
        "enabled" => ReasoningConfig {
            enabled: Some(true),
            max_tokens: budget,
            ..Default::default()
        },
        "disabled" => ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        },
        "adaptive" => ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        },
        _ => ReasoningConfig::default(),
    }
}

fn validate_request_cache_control(req: &ChatRequest) -> Result<()> {
    // Collect owned cache_control values first so refs into them live
    // long enough for the Breakpoint borrows. This is required for
    // `ToolDef::Other` whose `cache_control()` returns owned because
    // it's parsed on demand from the inner Value.
    let mut owned: Vec<(BreakpointPosition, routectl_core::CacheControl)> = Vec::new();

    if let Some(tools) = &req.tools {
        for t in tools {
            // Covers both ToolDef::Custom (typed) and ToolDef::Other
            // (Anthropic builtins like bash_*, web_search_*) -- the
            // latter would otherwise silently bypass the 4-cap.
            if let Some(cc) = t.cache_control() {
                owned.push((BreakpointPosition::Tools, cc));
            }
        }
    }

    if let Some(routectl_core::SystemContent::Blocks(blocks)) = &req.system {
        for b in blocks {
            if let Some(cc) = b.cache_control.as_ref() {
                owned.push((BreakpointPosition::System, cc.clone()));
            }
        }
    }

    for m in &req.messages {
        if let MessageContent::Parts(parts) = &m.content {
            for p in parts {
                if let Some(cc) = part_cache_control(p) {
                    owned.push((BreakpointPosition::Messages, cc.clone()));
                }
            }
        }
    }

    if let Some(cc) = req.cache_control.as_ref() {
        owned.push((BreakpointPosition::TopLevel, cc.clone()));
    }

    let bps: Vec<Breakpoint<'_>> = owned
        .iter()
        .map(|(pos, cc)| Breakpoint {
            position: *pos,
            control: cc,
        })
        .collect();
    cache_control::validate(&bps)
}

fn part_cache_control(p: &ContentPart) -> Option<&routectl_core::CacheControl> {
    match p {
        ContentPart::Known(k) => k.cache_control(),
        ContentPart::Other { cache_control, .. } => cache_control.as_ref(),
    }
}

// ---------------------------------------------------------------------------
// Response rendering
// ---------------------------------------------------------------------------

/// Render canonical `ChatResponse` into an Anthropic Messages response
/// body shape. Mirrors what `api.anthropic.com /v1/messages` would
/// emit: `{id, type:"message", role:"assistant", model, content[],
/// stop_reason, stop_sequence, usage}`.
fn render_messages_response(resp: ChatResponse) -> Value {
    let mut body = Map::new();
    body.insert("id".into(), Value::String(resp.id));
    body.insert("type".into(), Value::String("message".into()));
    body.insert("role".into(), Value::String("assistant".into()));
    body.insert("model".into(), Value::String(resp.model));

    let content = resp
        .choices
        .first()
        .map(|c| build_content_array(&c.message))
        .unwrap_or_default();
    body.insert("content".into(), Value::Array(content));

    let stop_reason = resp
        .choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(|fr| openai_finish_to_anthropic_stop(fr).to_string());
    body.insert(
        "stop_reason".into(),
        stop_reason.map(Value::String).unwrap_or(Value::Null),
    );
    body.insert("stop_sequence".into(), Value::Null);

    let usage = resp.usage.as_ref().map(|u| {
        // Anthropic's `input_tokens` is the RAW input portion; cache
        // fields are separate. Canonical `prompt_tokens` is the summed
        // total (OpenAI semantics), so subtract cache fields to recover
        // the raw input that the Anthropic spec wants on the wire.
        let raw_input = u
            .prompt_tokens
            .saturating_sub(u.cache_creation_input_tokens.unwrap_or(0))
            .saturating_sub(u.cache_read_input_tokens.unwrap_or(0));
        let mut usage_obj = json!({
            "input_tokens": raw_input,
            "output_tokens": u.completion_tokens,
            "cache_creation_input_tokens": u.cache_creation_input_tokens,
            "cache_read_input_tokens": u.cache_read_input_tokens,
            "cache_creation": u.cache_creation.as_ref().map(|c| json!({
                "ephemeral_5m_input_tokens": c.ephemeral_5m_input_tokens,
                "ephemeral_1h_input_tokens": c.ephemeral_1h_input_tokens,
            })),
        });
        // Forward-compat: emit unknown usage sub-fields (e.g.
        // `service_tier`) that flowed into `extras` from upstream.
        // Typed fields above win on key conflict.
        if let Some(map) = usage_obj.as_object_mut() {
            for (k, v) in &u.extras {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        usage_obj
    });
    if let Some(u) = usage {
        body.insert("usage".into(), u);
    }

    // Forward-compat: emit unknown response top-level fields (e.g.
    // `context_management`) that flowed into `ChatResponse.extras`
    // from upstream. Typed fields above win on key conflict. The
    // drop-list belongs here -- routectl owns the egress wire policy.
    for (k, v) in &resp.extras {
        // `stop_details` is a Bedrock-only Anthropic-API extension
        // not in the public Anthropic Messages baseline.
        if k == "stop_details" {
            continue;
        }
        body.entry(k.clone()).or_insert_with(|| v.clone());
    }

    Value::Object(body)
}

/// Build the Anthropic `content[]` array from a canonical assistant
/// `Message`. Order: thinking blocks (in detail-index order) first,
/// then tool_use blocks (one per tool_call), then any text content.
fn build_content_array(msg: &Message) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    // Thinking + redacted_thinking blocks from reasoning_details.
    // We emit for ALL dialect formats (anthropic-claude-v1,
    // deepseek-v1, vllm-reasoning-v1, openai-responses-v1,
    // openrouter, raw-think-tag-v1, ...). Anthropic's wire spec
    // doesn't carry a format tag -- only the canonical kind + payload
    // are needed. For Anthropic-sourced details, the upstream
    // signature passes through. For non-Anthropic, signature is
    // null/empty; cc + Anthropic SDK accept this. The openai-compat
    // egress's wire_lift/thinking extraction picks the blocks back
    // up on multi-turn echo so deepseek-v4-pro and vLLM (which
    // require reasoning_content echo-back) get a clean round-trip.
    let mut details = msg.reasoning_details.clone();
    details.sort_by_key(|d| d.index.unwrap_or(0));
    for d in &details {
        match &d.kind {
            routectl_core::ReasoningDetailKind::Text => {
                let text = d
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let signature = d.payload.get("signature").cloned().unwrap_or(Value::Null);
                blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": signature,
                }));
            }
            routectl_core::ReasoningDetailKind::Encrypted => {
                // Encrypted detail; field name on the wire differs by
                // dialect:
                //   - Anthropic: `data` (opaque encrypted payload)
                //   - OpenAI Responses: `encrypted_content`
                //   - OpenRouter passthrough: `data`
                // Read whichever is populated.
                let data = d
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .or_else(|| d.payload.get("encrypted_content").and_then(|v| v.as_str()))
                    .unwrap_or_default();
                blocks.push(json!({
                    "type": "redacted_thinking",
                    "data": data,
                }));
            }
            routectl_core::ReasoningDetailKind::Summary => {
                // OpenAI Responses emits per-step reasoning summaries
                // as Summary-kind details. Surface them as thinking
                // blocks so cc displays them (and so they round-trip
                // on multi-turn echo if the upstream uses summaries
                // as its reasoning carrier).
                let text = d
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !text.is_empty() {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": Value::Null,
                    }));
                }
            }
        }
    }

    // Pre-scan `msg.content` for ToolUse parts and collect their ids.
    // Some egresses populate BOTH the OpenAI-shape `msg.tool_calls`
    // AND the typed `ContentPart::ToolUse` part for the same upstream
    // function_call (openai-responses, anthropic-api response paths).
    // Without dedup, the renderer below would emit the same tool_use
    // block twice in the Anthropic `content` array -- once from the
    // tool_calls loop, once from the parts iteration. cc and the
    // Anthropic SDK tolerate the dupes during streaming (where the
    // dedup runs on chunk-state) but they break the non-streaming
    // path with two identical blocks back-to-back. Source of truth on
    // dedup: parts wins -- it carries the Anthropic-native shape and
    // any cache_control on the tool_use block, whereas the
    // tool_calls loop synthesizes a fresh block without those
    // sub-fields. Bug D (cc-via-* 2026-05-18).
    let parts_tool_use_ids: std::collections::HashSet<String> = match &msg.content {
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Known(routectl_core::KnownContentPart::ToolUse { id, .. }) => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect(),
        _ => std::collections::HashSet::new(),
    };

    // Tool-use blocks from tool_calls (OpenAI shape). Skip any whose
    // id already appears as a ContentPart::ToolUse in `msg.content`
    // (the parts iteration below will emit those in Anthropic-native
    // shape).
    if let Some(tcs) = msg.tool_calls.as_ref() {
        for tc in tcs {
            let id_value = tc.get("id").cloned().unwrap_or(Value::Null);
            if let Some(id_str) = id_value.as_str() {
                if parts_tool_use_ids.contains(id_str) {
                    continue;
                }
            }
            let func = tc.get("function").and_then(|v| v.as_object());
            let name = func
                .and_then(|f| f.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            blocks.push(json!({
                "type": "tool_use",
                "id": id_value,
                "name": name,
                "input": args,
            }));
        }
    }

    // Text content -- last so it follows thinking + tool_use, matching
    // Anthropic's natural ordering.
    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            blocks.push(json!({"type": "text", "text": t}));
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let Ok(v) = serde_json::to_value(p) {
                    blocks.push(v);
                }
            }
        }
        _ => {}
    }

    blocks
}

/// Reverse of `routectl_providers::anthropic_api::response::map_stop_reason`.
///
/// The egress maps Anthropic stop_reasons -> OpenAI finish_reasons:
/// `end_turn` / `stop_sequence` -> `stop`, `max_tokens` -> `length`,
/// `tool_use` -> `tool_calls`. Anything else (incl. forward-compat
/// values like `pause_turn`, `refusal`,
/// `model_context_window_exceeded`) passes through unchanged.
///
/// The ingress side here reverses the OpenAI overlap and PASSES
/// THROUGH any value not in that set so future-proof Anthropic-only
/// stop reasons don't get clobbered to `end_turn`. This had been
/// silently rewriting `pause_turn`, `refusal`, and
/// `model_context_window_exceeded` to `end_turn`, breaking
/// claude-code's per-stop-reason error handling.
///
/// The `stop_sequence` -> `stop` -> `end_turn` ambiguity remains
/// (information lost at the canonical layer); the more common
/// `end_turn` wins on the reverse path. To preserve `stop_sequence`
/// faithfully, the egress would need to set an additional native
/// finish-reason field on the canonical Choice.
fn openai_finish_to_anthropic_stop(fr: &str) -> &str {
    match fr {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        // Forward-compat: any value the egress passed through verbatim
        // (i.e. an Anthropic stop_reason that doesn't have an OpenAI
        // analogue) must survive the ingress reverse mapping or it
        // gets squashed to "end_turn" and the caller's error handling
        // breaks.
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Chunk rendering (state machine)
// ---------------------------------------------------------------------------

fn anthropic_state_mut(s: &mut dyn IngressStreamState) -> &mut AnthropicStreamState {
    s.as_any_mut()
        .downcast_mut::<AnthropicStreamState>()
        .expect("AnthropicIngress::render_chunk got a non-Anthropic stream state")
}

fn render_chunk_internal(
    chunk: ChatChunk,
    state: &mut AnthropicStreamState,
) -> Result<Vec<SseEvent>> {
    let mut events: Vec<SseEvent> = Vec::new();

    // Cache id/model from the first chunk that carries them; both
    // are tolerated as missing on the wire.
    if !chunk.id.is_empty() && state.msg_id.is_none() {
        state.msg_id = Some(chunk.id.clone());
    }
    if !chunk.model.is_empty() && state.msg_model.is_none() {
        state.msg_model = Some(chunk.model.clone());
    }

    // Emit message_start once.
    if !state.started {
        emit_message_start(state, &mut events);
        state.started = true;
    }

    // Walk the (single) choice's delta. Anthropic responses are
    // single-choice; n>1 would produce nonsense here.
    if let Some(choice) = chunk.choices.first() {
        emit_delta_events(&choice.delta, state, &mut events)?;

        if let Some(fr) = choice.finish_reason.as_deref() {
            flush_tool_blocks(state, &mut events);
            close_open_block(state, &mut events);
            if chunk.usage.is_some() {
                // Inline usage: emit combined delta + stop now.
                emit_message_delta(Some(fr), chunk.usage.as_ref(), &mut events);
                emit_message_stop(state, &mut events);
            } else {
                // Defer message_delta + message_stop until the usage
                // chunk arrives (or render_eos runs). OpenAI / OpenRouter
                // emit usage in a separate trailing chunk; emitting the
                // delta without usage now and a second delta after
                // message_stop is a protocol violation (the trailing
                // delta arrives post-stop).
                state.pending_finish_reason = Some(fr.to_string());
            }
        }
    } else if let Some(usage) = chunk.usage.as_ref() {
        if let Some(fr) = state.pending_finish_reason.take() {
            // Finalize the buffered finish_reason now that we have usage.
            emit_message_delta(Some(&fr), Some(usage), &mut events);
            emit_message_stop(state, &mut events);
        } else if !state.finished {
            // Mid-stream usage update (rare; some hosts emit interim
            // usage). Forward as a usage-only message_delta without
            // terminating the stream.
            emit_message_delta(None, Some(usage), &mut events);
        }
    }

    Ok(events)
}

fn emit_message_start(state: &AnthropicStreamState, events: &mut Vec<SseEvent>) {
    let msg = json!({
        "type": "message_start",
        "message": {
            "id": state.msg_id.clone().unwrap_or_else(|| format!("msg_{}", random_msg_id())),
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": state.msg_model.clone().unwrap_or_default(),
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }
    });
    events.push(SseEvent::named(
        "message_start",
        serde_json::to_string(&msg).unwrap_or_default(),
    ));
}

fn emit_delta_events(
    delta: &routectl_core::ChunkDelta,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) -> Result<()> {
    // Text content -> text_delta on the current text block.
    if let Some(text) = delta.content.as_deref() {
        if !text.is_empty() {
            let idx = ensure_block(state, OpenBlockKind::Text, events);
            push_block_delta(events, idx, json!({"type": "text_delta", "text": text}));
        }
    }

    // Reasoning details -> thinking_delta / signature_delta /
    // redacted_thinking blocks. Emits for ALL dialect formats
    // (deepseek-v1, vllm-reasoning-v1, openai-responses-v1,
    // openrouter, raw-think-tag-v1, anthropic-claude-v1). Anthropic
    // wire spec carries no format tag, just kind + payload. For
    // non-Anthropic formats the signature is null/empty; the
    // openai-compat egress's wire_lift/thinking extraction picks
    // these blocks back up on multi-turn echo so providers requiring
    // reasoning_content echo-back (DeepSeek-v4+, recent vLLM) get
    // a clean round-trip.
    for d in &delta.reasoning_details {
        match d.kind {
            routectl_core::ReasoningDetailKind::Text => {
                // Pass the upstream detail index so two distinct
                // thinking blocks (e.g. provider emits index=0 then
                // index=1 in the same response) emit as two separate
                // Anthropic content blocks. `ensure_block` compares
                // the full `OpenBlockKind` via PartialEq, so an
                // index change forces a content_block_stop +
                // content_block_start pair.
                let detail_index = d.index.unwrap_or(0);
                let idx = ensure_block(state, OpenBlockKind::Thinking { detail_index }, events);
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        push_block_delta(
                            events,
                            idx,
                            json!({"type": "thinking_delta", "thinking": text}),
                        );
                    }
                }
                if let Some(sig) = d.payload.get("signature").and_then(|v| v.as_str()) {
                    push_block_delta(
                        events,
                        idx,
                        json!({"type": "signature_delta", "signature": sig}),
                    );
                }
            }
            routectl_core::ReasoningDetailKind::Encrypted => {
                // Redacted thinking: no incremental delta path; emit a
                // stand-alone block_start + block_stop with the data.
                close_open_block(state, events);
                let idx = state.next_index;
                state.next_index += 1;
                // `data` (Anthropic / OpenRouter passthrough) or
                // `encrypted_content` (OpenAI Responses).
                let data = d
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .or_else(|| d.payload.get("encrypted_content").and_then(|v| v.as_str()))
                    .unwrap_or_default();
                events.push(SseEvent::named(
                    "content_block_start",
                    serde_json::to_string(&json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "redacted_thinking", "data": data},
                    }))
                    .unwrap_or_default(),
                ));
                events.push(SseEvent::named(
                    "content_block_stop",
                    serde_json::to_string(&json!({
                        "type": "content_block_stop",
                        "index": idx,
                    }))
                    .unwrap_or_default(),
                ));
            }
            routectl_core::ReasoningDetailKind::Summary => {
                // OpenAI Responses summary text -> thinking_delta.
                // Lets cc display the summary AND keeps the text in
                // the round-trip history for any preserve-mode echo.
                let detail_index = d.index.unwrap_or(0);
                let idx = ensure_block(state, OpenBlockKind::Thinking { detail_index }, events);
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        push_block_delta(
                            events,
                            idx,
                            json!({"type": "thinking_delta", "thinking": text}),
                        );
                    }
                }
            }
        }
        // Suppress unused warning when ReasoningDetail comes in handy
        // for downstream readers.
        let _: &ReasoningDetail = d;
    }

    // Tool calls -> tool_use blocks with input_json_delta.
    if let Some(tcs) = delta.tool_calls.as_ref() {
        for tc in tcs {
            apply_tool_call_delta(tc, state, events)?;
        }
    }

    Ok(())
}

/// Maximum permitted tool_call index in a streaming response. Caps
/// `state.tool_blocks` Vec growth so a malicious or malformed upstream
/// chunk with `index: 1_000_000` cannot allocate gigabytes per stream.
/// Anthropic-API limits are far below this; openai-compat upstreams
/// rarely exceed 16 parallel tool calls.
const MAX_TOOL_CALL_INDEX: usize = 64;

fn apply_tool_call_delta(
    tc: &Value,
    state: &mut AnthropicStreamState,
    _events: &mut Vec<SseEvent>,
) -> Result<()> {
    // OpenAI shape: {index, id?, type, function: {name?, arguments?}}.
    let call_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    if call_index > MAX_TOOL_CALL_INDEX {
        return Err(Error::Streaming(format!(
            "anthropic ingress: tool_call index {call_index} exceeds maximum of {MAX_TOOL_CALL_INDEX}"
        )));
    }

    // Allocate a new tool_use block on first sight of this call_index.
    while state.tool_blocks.len() <= call_index {
        state.tool_blocks.push(ToolBlockState::default());
    }
    let block = &mut state.tool_blocks[call_index];
    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
        block.id = id.to_string();
    }
    if let Some(name) = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
    {
        block.name = name.to_string();
    }

    if let Some(args) = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
    {
        if !args.is_empty() {
            block.partial_json.push_str(args);
        }
    }
    Ok(())
}

fn flush_tool_blocks(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    let buffered = std::mem::take(&mut state.tool_blocks);
    for block in buffered
        .into_iter()
        .filter(|b| !b.id.is_empty() || !b.name.is_empty())
    {
        close_open_block(state, events);
        let idx = state.next_index;
        state.next_index += 1;
        state.open = Some((idx, OpenBlockKind::ToolUse));
        events.push(SseEvent::named(
            "content_block_start",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": block.id,
                    "name": block.name,
                    "input": {},
                },
            }))
            .unwrap_or_default(),
        ));
        if !block.partial_json.is_empty() {
            push_block_delta(
                events,
                idx,
                json!({"type": "input_json_delta", "partial_json": block.partial_json}),
            );
        }
    }
}

/// Open a content block of `kind` if one isn't already open. Returns
/// the block index in either case (whether it was already open or
/// just opened). Returning the index removes the
/// `state.open.unwrap().0` foot-gun at every call site.
fn ensure_block(
    state: &mut AnthropicStreamState,
    kind: OpenBlockKind,
    events: &mut Vec<SseEvent>,
) -> usize {
    if let Some((idx, k)) = state.open {
        if k == kind {
            return idx;
        }
    }
    close_open_block(state, events);
    let idx = state.next_index;
    state.next_index += 1;
    state.open = Some((idx, kind));
    let block = match kind {
        OpenBlockKind::Text => json!({"type": "text", "text": ""}),
        OpenBlockKind::Thinking { .. } => json!({"type": "thinking", "thinking": ""}),
        OpenBlockKind::ToolUse => json!({"type": "tool_use", "id": "", "name": "", "input": {}}),
    };
    events.push(SseEvent::named(
        "content_block_start",
        serde_json::to_string(&json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": block,
        }))
        .unwrap_or_default(),
    ));
    idx
}

fn close_open_block(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    if let Some((idx, _)) = state.open.take() {
        events.push(SseEvent::named(
            "content_block_stop",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": idx,
            }))
            .unwrap_or_default(),
        ));
    }
}

fn push_block_delta(events: &mut Vec<SseEvent>, idx: usize, delta: Value) {
    events.push(SseEvent::named(
        "content_block_delta",
        serde_json::to_string(&json!({
            "type": "content_block_delta",
            "index": idx,
            "delta": delta,
        }))
        .unwrap_or_default(),
    ));
}

fn emit_message_delta(
    finish_reason: Option<&str>,
    usage: Option<&routectl_core::UsageDelta>,
    events: &mut Vec<SseEvent>,
) {
    let mut delta = Map::new();
    delta.insert(
        "stop_reason".into(),
        finish_reason
            .map(|fr| Value::String(openai_finish_to_anthropic_stop(fr).into()))
            .unwrap_or(Value::Null),
    );
    delta.insert("stop_sequence".into(), Value::Null);

    let mut payload = Map::new();
    payload.insert("type".into(), Value::String("message_delta".into()));
    payload.insert("delta".into(), Value::Object(delta));

    if let Some(u) = usage {
        let mut wire_usage = Map::new();
        // Anthropic's wire semantics: `input_tokens` is the RAW input
        // portion (cache_creation and cache_read are separate fields).
        // Canonical `prompt_tokens` is the summed total, so subtract
        // cache fields to recover the raw value. Real Anthropic emits
        // `input_tokens` on message_delta mirroring the message_start
        // value, and downstream consumers need it because routectl's
        // emit_message_start hardcodes {input_tokens:0, output_tokens:0}.
        if let Some(prompt) = u.prompt_tokens {
            let raw_input = prompt
                .saturating_sub(u.cache_creation_input_tokens.unwrap_or(0))
                .saturating_sub(u.cache_read_input_tokens.unwrap_or(0));
            wire_usage.insert("input_tokens".into(), json!(raw_input));
        }
        if let Some(n) = u.completion_tokens {
            wire_usage.insert("output_tokens".into(), json!(n));
        }
        if let Some(n) = u.cache_creation_input_tokens {
            wire_usage.insert("cache_creation_input_tokens".into(), json!(n));
        }
        if let Some(n) = u.cache_read_input_tokens {
            wire_usage.insert("cache_read_input_tokens".into(), json!(n));
        }
        if let Some(c) = u.cache_creation.as_ref() {
            let mut cc = Map::new();
            if let Some(n) = c.ephemeral_5m_input_tokens {
                cc.insert("ephemeral_5m_input_tokens".into(), json!(n));
            }
            if let Some(n) = c.ephemeral_1h_input_tokens {
                cc.insert("ephemeral_1h_input_tokens".into(), json!(n));
            }
            wire_usage.insert("cache_creation".into(), Value::Object(cc));
        }
        payload.insert("usage".into(), Value::Object(wire_usage));
    }

    events.push(SseEvent::named(
        "message_delta",
        serde_json::to_string(&Value::Object(payload)).unwrap_or_default(),
    ));
}

fn emit_message_stop(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    if state.finished {
        return;
    }
    state.finished = true;
    events.push(SseEvent::named(
        "message_stop",
        serde_json::to_string(&json!({"type": "message_stop"})).unwrap_or_default(),
    ));
}

// ---------------------------------------------------------------------------
// IngressAdapter impl
// ---------------------------------------------------------------------------

impl IngressAdapter for AnthropicIngress {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
        // Trace-level ingress body for triage. Same gating +
        // sensitivity story as the openai ingress. Honors
        // ROUTECTL_LOG_REDACT_PROMPTS=1.
        routectl_core::trace_ingress_body("anthropic", &body);
        translate_request(headers, body)
    }

    fn render_response(&self, resp: ChatResponse) -> Result<Value> {
        Ok(render_messages_response(resp))
    }

    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        Box::new(AnthropicStreamState::default())
    }

    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        render_chunk_internal(chunk, anthropic_state_mut(state))
    }

    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        let s = anthropic_state_mut(state);
        let mut events = Vec::new();
        if !s.finished {
            flush_tool_blocks(s, &mut events);
            close_open_block(s, &mut events);
            // Flush a deferred finish_reason (no usage chunk arrived).
            if let Some(fr) = s.pending_finish_reason.take() {
                emit_message_delta(Some(&fr), None, &mut events);
            }
            emit_message_stop(s, &mut events);
        }
        events
    }
}

/// Synthesized message id for `message_start` frames when the
/// upstream chunk omitted one. UUID-based to avoid collisions on hosts
/// with no monotonic clock or under burst load (the prior
/// `SystemTime::now()` nanosecond version could collide on
/// concurrent requests).
fn random_msg_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh_state() -> AnthropicStreamState {
        AnthropicStreamState::default()
    }

    // -------- request parsing --------

    #[test]
    fn parse_request_with_system_blocks_and_cache_control() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "system": [{
                "type": "text",
                "text": "you are helpful",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "max_tokens": 1024
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.model, "claude-opus-4-7");
        assert!(matches!(
            req.system,
            Some(routectl_core::SystemContent::Blocks(_))
        ));
    }

    #[test]
    fn parse_request_translates_thinking_to_reasoning() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 5000}
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        let r = req.reasoning.unwrap();
        assert_eq!(r.enabled, Some(true));
        assert_eq!(r.max_tokens, Some(5000));
    }

    #[test]
    fn parse_request_translates_metadata_user_id_to_user() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "metadata": {"user_id": "abc-123"}
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.user.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_request_anthropic_only_fields_land_in_provider_extras() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "top_k": 40,
            "service_tier": "auto",
            "container": "ctr_01"
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        let extras = req.provider_extras.unwrap();
        assert_eq!(extras["top_k"], 40);
        assert_eq!(extras["service_tier"], "auto");
        assert_eq!(extras["container"], "ctr_01");
    }

    #[test]
    fn parse_request_anthropic_beta_round_trips() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "anthropic_beta": ["context-1m-2025-08-07"]
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(
            req.anthropic_beta,
            vec!["context-1m-2025-08-07".to_string()]
        );
    }

    #[test]
    fn parse_request_rejects_too_many_breakpoints() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "d", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "e", "cache_control": {"type": "ephemeral"}}
                ]
            }],
            "max_tokens": 1024
        });
        let err = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn parse_request_rejects_5m_then_1h_ordering() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "five", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                    {"type": "text", "text": "one", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }],
            "max_tokens": 1024
        });
        let err = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        assert!(err.to_string().contains("after a 5m"));
    }

    #[test]
    fn parse_request_unknown_block_type_passes_through() {
        let body = json!({
            "model": "claude-opus-4-7",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "server_tool_use",
                    "id": "srvtu_01",
                    "name": "web_search",
                    "input": {"query": "rust"}
                }]
            }],
            "max_tokens": 1024
        });
        let req = AnthropicIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        if let MessageContent::Parts(parts) = &req.messages[0].content {
            assert!(matches!(&parts[0], ContentPart::Other { .. }));
        } else {
            panic!("expected Parts");
        }
    }

    // -------- response rendering --------

    #[test]
    fn render_response_emits_messages_shape() {
        use routectl_core::{schema::Choice, Message, Role, Usage};
        let resp = ChatResponse {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("hi there".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            routectl_provider: None,
            extras: Default::default(),
        };
        let v = AnthropicIngress.render_response(resp).unwrap();
        assert_eq!(v["id"], "msg_01");
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hi there");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
    }

    /// Bug D (cc-via-* 2026-05-18): openai-responses and anthropic-api
    /// non-streaming responses populate BOTH `msg.tool_calls`
    /// (OpenAI shape) AND a typed `ContentPart::ToolUse` on
    /// `msg.content` for the same upstream function_call. Without
    /// dedup, the renderer emits two identical tool_use blocks
    /// back-to-back in the Anthropic `content` array. Pin: only ONE
    /// tool_use block per call_id, with the parts-native shape
    /// preserved.
    #[test]
    fn render_response_dedupes_tool_use_when_present_in_both_tool_calls_and_parts() {
        use routectl_core::{
            schema::Choice, ContentPart, KnownContentPart, Message, MessageContent, Role, Usage,
        };
        let resp = ChatResponse {
            id: "msg_dup".into(),
            model: "gpt-5".into(),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "I'll compute that".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::ToolUse {
                            id: "call_dup".into(),
                            name: "calculator".into(),
                            input: json!({"x": 1, "y": 2}),
                            cache_control: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "call_dup",
                        "type": "function",
                        "function": {
                            "name": "calculator",
                            "arguments": "{\"x\":1,\"y\":2}"
                        }
                    })]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 10,
                total_tokens: 15,
                ..Default::default()
            }),
            routectl_provider: None,
            extras: Default::default(),
        };
        let v = AnthropicIngress.render_response(resp).unwrap();
        let content = v["content"].as_array().expect("content is array");
        // Count tool_use blocks for the dup id.
        let tool_uses_for_id: Vec<&Value> = content
            .iter()
            .filter(|b| b["type"] == "tool_use" && b["id"] == "call_dup")
            .collect();
        assert_eq!(
            tool_uses_for_id.len(),
            1,
            "expected exactly one tool_use block for call_dup, got content: {content:?}",
        );
        // The surviving block carries Anthropic-native shape (parts source).
        assert_eq!(tool_uses_for_id[0]["name"], "calculator");
        assert_eq!(tool_uses_for_id[0]["input"]["x"], 1);
        assert_eq!(tool_uses_for_id[0]["input"]["y"], 2);
        // Text block also rendered.
        let text_blocks: Vec<&Value> = content.iter().filter(|b| b["type"] == "text").collect();
        assert_eq!(text_blocks.len(), 1);
        assert_eq!(text_blocks[0]["text"], "I'll compute that");
    }

    /// Counterpart: openai-compat populates ONLY `msg.tool_calls`
    /// (parts is empty / Text). The renderer must still emit one
    /// tool_use block per call so this code path doesn't regress
    /// when the dedup set is empty.
    #[test]
    fn render_response_emits_tool_use_from_tool_calls_when_parts_has_no_tool_use() {
        use routectl_core::{schema::Choice, Message, MessageContent, Role, Usage};
        let resp = ChatResponse {
            id: "msg_oc".into(),
            model: "qwen-3-coder".into(),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    // openai-compat doesn't populate parts.ToolUse; content
                    // is the model's plain text reply.
                    content: MessageContent::Text("running tool now".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "call_oc",
                        "type": "function",
                        "function": {
                            "name": "ls",
                            "arguments": "{\"path\":\"/tmp\"}"
                        }
                    })]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 10,
                total_tokens: 15,
                ..Default::default()
            }),
            routectl_provider: None,
            extras: Default::default(),
        };
        let v = AnthropicIngress.render_response(resp).unwrap();
        let content = v["content"].as_array().expect("content is array");
        let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0]["id"], "call_oc");
        assert_eq!(tool_uses[0]["name"], "ls");
        assert_eq!(tool_uses[0]["input"]["path"], "/tmp");
    }

    // -------- streaming --------

    fn ingress() -> AnthropicIngress {
        AnthropicIngress
    }

    fn text_chunk(text: &str, finish: Option<&str>) -> ChatChunk {
        use routectl_core::{ChunkChoice, ChunkDelta};
        ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some(text.into()),
                    ..Default::default()
                },
                finish_reason: finish.map(|s| s.into()),
            }],
            usage: None,
        }
    }

    #[test]
    fn stream_emits_message_start_then_text_block() {
        let mut s = fresh_state();
        let events = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
        let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
    }

    /// Finish_reason WITHOUT usage on the same chunk: the renderer must
    /// buffer the finish_reason and emit `message_delta + message_stop`
    /// either on the next usage chunk OR at `render_eos`. Emitting
    /// `message_delta(fr, None)` immediately and then a SECOND
    /// `message_delta` after `message_stop` when a trailing usage
    /// chunk arrives is a protocol violation -- the trailing delta
    /// arrives post-stop.
    #[test]
    fn stream_finish_without_usage_defers_until_eos() {
        let mut state = ingress().new_stream_state();
        let _ = ingress()
            .render_chunk(text_chunk("hello", None), state.as_mut())
            .unwrap();
        let finish_events = ingress()
            .render_chunk(text_chunk("", Some("stop")), state.as_mut())
            .unwrap();
        let finish_names: Vec<&str> = finish_events
            .iter()
            .filter_map(|e| e.event.as_deref())
            .collect();
        // Without usage, the finish chunk only closes the open block.
        assert_eq!(finish_names, vec!["content_block_stop"]);
        // Stream ends with no usage chunk: render_eos flushes the
        // buffered finish_reason as a delta (no usage) + stop.
        let eos_events = ingress().render_eos(state.as_mut());
        let eos_names: Vec<&str> = eos_events
            .iter()
            .filter_map(|e| e.event.as_deref())
            .collect();
        assert_eq!(eos_names, vec!["message_delta", "message_stop"]);
    }

    /// Finish_reason WITH inline usage: emit `message_delta(fr, usage)`
    /// + `message_stop` immediately on the same chunk. No deferral.
    #[test]
    fn stream_finish_with_inline_usage_emits_immediately() {
        use routectl_core::{ChunkChoice, ChunkDelta, UsageDelta};
        let mut s = fresh_state();
        let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
        let closing = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some("stop".into()),
            }],
            usage: Some(UsageDelta {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
        };
        let events = render_chunk_internal(closing, &mut s).unwrap();
        let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
        assert_eq!(
            names,
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
    }

    /// OpenRouter / OpenAI pattern: finish_reason on one chunk, usage
    /// on the next. The renderer must hold message_delta + message_stop
    /// until the usage chunk arrives, then emit ONE message_delta
    /// carrying BOTH stop_reason and usage, followed by message_stop.
    /// Two message_deltas wrapping message_stop is a protocol violation.
    #[test]
    fn stream_finish_then_separate_usage_chunk_emits_single_delta() {
        use routectl_core::UsageDelta;
        let mut s = fresh_state();
        // Body text.
        let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
        // Finish chunk with no usage -- buffered, only close fires.
        let finish_events = render_chunk_internal(text_chunk("", Some("stop")), &mut s).unwrap();
        let finish_names: Vec<&str> = finish_events
            .iter()
            .filter_map(|e| e.event.as_deref())
            .collect();
        assert_eq!(finish_names, vec!["content_block_stop"]);
        // Trailing usage-only chunk -- emits combined delta + stop.
        let usage_chunk = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![],
            usage: Some(UsageDelta {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
        };
        let usage_events = render_chunk_internal(usage_chunk, &mut s).unwrap();
        let usage_names: Vec<&str> = usage_events
            .iter()
            .filter_map(|e| e.event.as_deref())
            .collect();
        assert_eq!(usage_names, vec!["message_delta", "message_stop"]);
        // The single message_delta carries both stop_reason AND usage.
        let payload: Value = serde_json::from_str(&usage_events[0].data).unwrap();
        assert_eq!(payload["delta"]["stop_reason"], "end_turn");
        assert_eq!(payload["usage"]["input_tokens"], 100);
        assert_eq!(payload["usage"]["output_tokens"], 50);
    }

    #[test]
    fn stream_eos_emits_message_stop_when_not_yet_finished() {
        let mut state = ingress().new_stream_state();
        // Drive at least one chunk so message_start fires.
        let _ = ingress()
            .render_chunk(text_chunk("hi", None), state.as_mut())
            .unwrap();
        let events = ingress().render_eos(state.as_mut());
        let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
        assert_eq!(names, vec!["content_block_stop", "message_stop"]);
    }

    #[test]
    fn stream_two_concurrent_tool_calls_each_get_their_own_block() {
        // Verify both tool calls open their own blocks
        // and arguments-deltas land on the right block index.
        use routectl_core::{ChunkChoice, ChunkDelta};
        let chunk = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![
                        json!({
                            "index": 0,
                            "id": "toolu_01",
                            "type": "function",
                            "function": {"name": "calc", "arguments": "{\"a\":"}
                        }),
                        json!({
                            "index": 1,
                            "id": "toolu_02",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"q\":"}
                        }),
                    ]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let mut s = fresh_state();
        let events = render_chunk_internal(chunk, &mut s).unwrap();
        let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
        assert_eq!(names, vec!["message_start"]);
        assert_eq!(s.tool_blocks.len(), 2);
        assert_eq!(s.tool_blocks[0].id, "toolu_01");
        assert_eq!(s.tool_blocks[1].id, "toolu_02");
    }

    #[test]
    fn stream_interleaved_tool_call_chunks_flush_in_valid_order_at_finish() {
        use routectl_core::{ChunkChoice, ChunkDelta};
        let mut s = fresh_state();
        let first = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![
                        json!({
                            "index": 0,
                            "id": "toolu_01",
                            "type": "function",
                            "function": {"name": "calc", "arguments": "{\"a\":"}
                        }),
                        json!({
                            "index": 1,
                            "id": "toolu_02",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"q\":"}
                        }),
                    ]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        // Second chunk carries inline usage so the renderer emits
        // message_delta + message_stop immediately. Hosts that split
        // finish_reason and usage across two chunks are covered by
        // `stream_finish_then_separate_usage_chunk_emits_single_delta`.
        let second = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![
                        json!({
                            "index": 1,
                            "function": {"arguments": "\"rust\"}"}
                        }),
                        json!({
                            "index": 0,
                            "function": {"arguments": "1}"}
                        }),
                    ]),
                    ..Default::default()
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(routectl_core::UsageDelta {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
        };

        let _ = render_chunk_internal(first, &mut s).unwrap();
        let events = render_chunk_internal(second, &mut s).unwrap();
        let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
        assert_eq!(
            names,
            vec![
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
    }

    #[test]
    fn usage_only_chunk_emits_null_stop_reason() {
        use routectl_core::UsageDelta;
        let mut s = fresh_state();
        let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
        let usage_only = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![],
            usage: Some(UsageDelta::default()),
        };
        let events = render_chunk_internal(usage_only, &mut s).unwrap();
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        assert!(payload["delta"]["stop_reason"].is_null());
    }

    /// Closing chunk's `usage.prompt_tokens` must be rendered into
    /// Anthropic's `input_tokens` on message_delta carries the RAW
    /// input portion (cache_creation and cache_read are separate
    /// fields per spec). routectl computes raw = prompt_tokens -
    /// cache_creation - cache_read so the wire body matches the
    /// Anthropic spec rather than echoing canonical's summed
    /// prompt_tokens. A receiver-side anthropic SSE state machine
    /// (`anthropic_api/sse.rs`) sums raw + cache fields back into
    /// canonical prompt_tokens.
    #[test]
    fn message_delta_renders_raw_input_tokens_per_anthropic_spec() {
        use routectl_core::{schema::CacheCreation, UsageDelta};
        let mut s = fresh_state();
        let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
        let closing = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![],
            usage: Some(UsageDelta {
                prompt_tokens: Some(12345),
                completion_tokens: Some(50),
                total_tokens: Some(12395),
                cache_creation_input_tokens: Some(100),
                cache_read_input_tokens: Some(200),
                cache_creation: Some(CacheCreation {
                    ephemeral_5m_input_tokens: Some(50),
                    ephemeral_1h_input_tokens: Some(50),
                }),
                ..Default::default()
            }),
        };
        let events = render_chunk_internal(closing, &mut s).unwrap();
        let delta_event = events
            .iter()
            .find(|e| e.event.as_deref() == Some("message_delta"))
            .expect("message_delta emitted");
        let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
        let wire_usage = &payload["usage"];
        // raw input = 12345 - 100 - 200 = 12045 (per Anthropic spec).
        assert_eq!(wire_usage["input_tokens"], 12045);
        assert_eq!(wire_usage["output_tokens"], 50);
        assert_eq!(wire_usage["cache_creation_input_tokens"], 100);
        assert_eq!(wire_usage["cache_read_input_tokens"], 200);
    }

    #[test]
    fn stream_distinct_thinking_indices_emit_separate_blocks() {
        // Two reasoning details with different `index` values must
        // emit as TWO Anthropic content blocks, not one merged
        // block. Pre-fix, `ensure_block` only checked
        // `OpenBlockKind == Thinking` and merged them. Now the
        // OpenBlockKind variant carries `detail_index` so the
        // second detail forces a content_block_stop on the first
        // block and a content_block_start on the new block.
        use routectl_core::{
            schema::{ChunkChoice, ChunkDelta},
            ReasoningDetail, ReasoningDetailKind,
        };
        let first = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning_details: vec![ReasoningDetail {
                        kind: ReasoningDetailKind::Text,
                        id: None,
                        format: Some(ANTHROPIC_FORMAT.into()),
                        index: Some(0),
                        payload: json!({"text": "first thought"}),
                    }],
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let second = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning_details: vec![ReasoningDetail {
                        kind: ReasoningDetailKind::Text,
                        id: None,
                        format: Some(ANTHROPIC_FORMAT.into()),
                        index: Some(1),
                        payload: json!({"text": "second thought"}),
                    }],
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let mut s = fresh_state();
        let first_events = render_chunk_internal(first, &mut s).unwrap();
        let second_events = render_chunk_internal(second, &mut s).unwrap();

        // First chunk: message_start + content_block_start (idx=0 thinking)
        // + content_block_delta. Find the start event and capture its idx.
        let first_start_idx = first_events
            .iter()
            .find_map(|ev| {
                ev.event
                    .as_deref()
                    .filter(|n| *n == "content_block_start")
                    .map(|_| ())
            })
            .map(|_| 0_usize);
        assert!(
            first_start_idx.is_some(),
            "first chunk must open a thinking block; got events: {:?}",
            first_events
                .iter()
                .map(|e| e.event.as_deref())
                .collect::<Vec<_>>()
        );

        // Second chunk MUST emit content_block_stop for the previous
        // index THEN content_block_start for a new index. Without
        // the fix, neither would appear (just delta on the same
        // open block).
        let names: Vec<&str> = second_events
            .iter()
            .filter_map(|ev| ev.event.as_deref())
            .collect();
        let stop_pos = names.iter().position(|n| *n == "content_block_stop");
        let start_pos = names.iter().position(|n| *n == "content_block_start");
        assert!(
            stop_pos.is_some(),
            "second-chunk thinking with new detail_index must emit content_block_stop; events: {names:?}"
        );
        assert!(
            start_pos.is_some(),
            "second-chunk thinking with new detail_index must emit content_block_start; events: {names:?}"
        );
        assert!(
            stop_pos.unwrap() < start_pos.unwrap(),
            "content_block_stop must precede content_block_start; events: {names:?}"
        );
    }

    #[test]
    fn stream_tool_call_index_above_cap_returns_streaming_error() {
        // CRITICAL C2: tool_blocks Vec growth bound.
        use routectl_core::{ChunkChoice, ChunkDelta};
        let chunk = ChatChunk {
            id: "msg_01".into(),
            model: "claude-opus-4-7".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![json!({
                        "index": 1_000_000_u64,
                        "id": "toolu_evil",
                        "type": "function",
                        "function": {"name": "x", "arguments": "{}"}
                    })]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let mut s = fresh_state();
        let err = render_chunk_internal(chunk, &mut s).unwrap_err();
        assert!(
            err.to_string().contains("exceeds maximum"),
            "expected streaming error with 'exceeds maximum', got: {err}"
        );
    }
}
