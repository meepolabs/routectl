//! OpenAI Responses request body -> canonical `ChatRequest`.
//!
//! Inverse of the openai-responses egress. The Responses wire body is a
//! flat tagged-union `input[]` array plus top-level controls
//! (`instructions`, `tools`, `tool_choice`, `reasoning`,
//! `max_output_tokens`, `text`, `store`, `previous_response_id`, ...).
//! `translate_request` walks the union back into canonical
//! `messages[]` and lifts the top-level controls into their canonical
//! homes:
//!
//! - `instructions` (string)            -> `system`
//! - `input` (string | item array)      -> `messages[]`
//!   - `message`                        -> `Message` (user/assistant/system)
//!   - `function_call`                  -> assistant `tool_calls[]`
//!   - `function_call_output`           -> `Role::Tool` message
//!   - `reasoning`                      -> assistant `reasoning_details[]`
//!   - unknown item kind                -> skipped with a WARN (never 500)
//! - `tools`                            -> `tools[]` (ToolDef)
//! - `tool_choice`                      -> `tool_choice` (verbatim Value)
//! - `reasoning` (object)               -> `reasoning` (ReasoningConfig)
//! - `max_output_tokens`                -> `max_tokens`
//! - `text.format`                      -> `response_format`
//! - `model`                            -> `model` (alias-header override)
//! - everything else                    -> `provider_extras` (forward-compat)
//!
//! Statefulness contract (deterministic, never a silent wrong answer):
//! - `previous_response_id` present -> 400 (`Error::Validation`). The
//!   client omitted prior context expecting the server to resolve it;
//!   routectl is stateless and cannot, so answering would be wrong.
//! - `store: true` (no previous_response_id) -> accepted, persistence
//!   ignored with a WARN. The full turn is present, so the answer is
//!   correct; retrieval-by-id later just won't work because routectl
//!   never stores.
//! - `store: false` / absent -> normal stateless path.

use axum::http::HeaderMap;
use serde_json::{Map, Value};

use routectl_core::{
    ChatRequest, ContentPart, Error, KnownContentPart, Message, MessageContent, ReasoningConfig,
    ReasoningDetail, ReasoningDetailKind, Result, Role, SystemContent, ToolDef,
};

use super::OPENAI_RESPONSES_FORMAT;
use crate::ingress::read_alias_header;

/// Top-level Responses request fields handled explicitly below. Anything
/// NOT in this set is swept into `provider_extras` so a future Responses
/// field reaches the egress without a code edit (forward-compat seam,
/// mirroring the openai / anthropic ingress sweeps).
const HANDLED_TOP_LEVEL_FIELDS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "reasoning",
    "max_output_tokens",
    "text",
    "stream",
    "temperature",
    "top_p",
    "store",
    "previous_response_id",
];

pub(super) fn translate_request(headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
    let mut obj = match body {
        Value::Object(map) => map,
        _ => {
            return Err(Error::Validation(
                "openai-responses ingress: request body is not an object".into(),
            ));
        }
    };

    // Statefulness contract: reject server-side conversation state before
    // doing any other work. previous_response_id means the client omitted
    // prior context expecting the server to resolve it -- routectl never
    // holds that state, so answering would be a silent wrong answer.
    reject_previous_response_id(&obj)?;
    // store:true is accepted (the turn is self-contained) but the
    // persistence intent is ignored; warn so the operator knows
    // retrieval-by-id won't work against a stateless proxy.
    warn_on_store(&obj);

    // model (overridden by the alias header when present).
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut req = ChatRequest {
        model,
        ..Default::default()
    };

    // instructions -> system.
    if let Some(system) = take_instructions(&mut obj) {
        req.system = Some(system);
    }

    // input -> messages[].
    if let Some(input) = obj.remove("input") {
        req.messages = build_messages(input);
    }

    // Lift in-array system/developer messages into req.system so loose
    // Role::System entries do not reach mutual-exclusion egresses.
    crate::ingress::lift_system_messages(&mut req);

    // tools -> ToolDef[].
    if let Some(tools) = obj.remove("tools") {
        req.tools = build_tools(tools);
    }

    // tool_choice -> canonical tool_choice (verbatim Value; egresses
    // translate per-upstream, mirroring the openai chat ingress).
    if let Some(tc) = obj.remove("tool_choice")
        && !tc.is_null()
    {
        req.tool_choice = Some(tc);
    }

    // reasoning object -> ReasoningConfig.
    if let Some(reasoning) = obj.remove("reasoning")
        && let Some(cfg) = build_reasoning(&reasoning)
    {
        req.reasoning = Some(cfg);
    }

    // max_output_tokens -> max_tokens.
    if let Some(max) = obj.remove("max_output_tokens").and_then(|v| v.as_u64()) {
        req.max_tokens = Some(clamp_u32(max));
    }

    // text.format -> response_format (canonical structured-output slot).
    // The unhandled remainder of the text object (e.g., text.verbosity)
    // is saved for forward-compat: "text" is in HANDLED_TOP_LEVEL_FIELDS
    // so the extras sweep never sees it; we forward the remainder manually.
    let text_remainder = if let Some(text) = obj.remove("text") {
        if let Some(format) = extract_text_format(&text) {
            req.response_format = Some(format);
        }
        text_without_format(text)
    } else {
        None
    };

    // Plain scalar passthroughs that canonical models directly.
    if let Some(stream) = obj.remove("stream").and_then(|v| v.as_bool()) {
        req.stream = Some(stream);
    }
    if let Some(temp) = obj.remove("temperature").and_then(|v| v.as_f64()) {
        req.temperature = Some(temp);
    }
    if let Some(top_p) = obj.remove("top_p").and_then(|v| v.as_f64()) {
        req.top_p = Some(top_p);
    }

    // Forward-compat sweep: anything left that this ingress did not
    // consume is stashed in provider_extras so the egress can forward it
    // verbatim. store / previous_response_id are removed here so neither
    // leaks downstream (previous_response_id already 400'd above; store
    // is intentionally not forwarded -- the upstream the request lands on
    // is chosen by the router, and forwarding a stale persistence flag
    // could surprise it).
    let mut extras = sweep_extras(obj);
    // Merge the text remainder (subfields other than "format") so they
    // survive the boundary even though "text" is a handled top-level key.
    if let Some(rem) = text_remainder {
        extras.insert("text".into(), Value::Object(rem));
    }
    if !extras.is_empty() {
        req.provider_extras = Some(Value::Object(extras));
    }

    // Alias header override (mirrors openai / anthropic ingress): the
    // wire model passes through verbatim unless the harness pins an alias.
    if let Some(alias) = read_alias_header(headers) {
        req.model = alias;
    }

    req.routectl_internal.provenance = routectl_core::RequestProvenance::OpenaiIngress;

    Ok(req)
}

// ---------------------------------------------------------------------------
// Statefulness contract
// ---------------------------------------------------------------------------

/// Reject a request carrying a non-null `previous_response_id`. routectl
/// is stateless: it never persists prior turns, so it cannot resolve a
/// reference to one. Returning the prior context's continuation anyway
/// would be a silent wrong answer, so this is a hard 400.
fn reject_previous_response_id(obj: &Map<String, Value>) -> Result<()> {
    let present = obj
        .get("previous_response_id")
        .is_some_and(|v| !v.is_null());
    if !present {
        return Ok(());
    }
    Err(Error::Validation(
        "openai-responses ingress: routectl is stateless and does not support server-side \
         conversation state (previous_response_id). Configure the client to send the full \
         conversation input each turn (disable store / unset previous_response_id)."
            .into(),
    ))
}

/// Warn when the client asked the server to persist the response
/// (`store: true`) without a `previous_response_id`. The current turn is
/// self-contained, so routectl answers it correctly; it simply never
/// persists, which means a later retrieval-by-id against this proxy would
/// find nothing. Not an error -- only a heads-up for the operator.
fn warn_on_store(obj: &Map<String, Value>) {
    if obj.get("store").and_then(Value::as_bool) == Some(true) {
        tracing::warn!(
            "openai-responses ingress: store=true ignored (routectl is stateless; the current \
             turn is answered from the full input, but the response is never persisted, so a \
             later retrieval by response id will not work)"
        );
    }
}

// ---------------------------------------------------------------------------
// instructions -> system
// ---------------------------------------------------------------------------

/// Lift the top-level `instructions` string into canonical `system`.
/// An empty string is treated as "no system prompt" and dropped.
fn take_instructions(obj: &mut Map<String, Value>) -> Option<SystemContent> {
    let s = obj.remove("instructions")?;
    let text = s.as_str()?;
    if text.is_empty() {
        return None;
    }
    Some(SystemContent::Text(text.to_string()))
}

// ---------------------------------------------------------------------------
// input -> messages[]
// ---------------------------------------------------------------------------

/// Turn the Responses `input` field into canonical `messages[]`. `input`
/// is either a bare string (one user message) or an array of tagged
/// items. Tool calls collected from `function_call` items attach to the
/// most recent assistant message so the canonical assistant turn carries
/// its `tool_calls`.
fn build_messages(input: Value) -> Vec<Message> {
    match input {
        Value::String(text) => vec![user_text_message(text)],
        Value::Array(items) => build_messages_from_items(items),
        // Any other shape is unusable as conversation input; degrade to
        // an empty message list rather than panicking. The request will
        // still carry instructions / tools, and the upstream surfaces its
        // own error if it needs input.
        other => {
            tracing::warn!(
                kind = %value_type_name(&other),
                "openai-responses ingress: `input` is neither a string nor an array; ignoring"
            );
            Vec::new()
        }
    }
}

fn build_messages_from_items(items: Vec<Value>) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::with_capacity(items.len());
    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            // A `message` item that omits `type` is tolerated: the
            // Responses API treats a `{role, content}` object as an
            // implicit message. Handle the empty-kind case as a message
            // when it carries a role.
            "message" => push_message_item(&mut messages, &item),
            "" if item.get("role").is_some() => push_message_item(&mut messages, &item),
            "function_call" => attach_function_call(&mut messages, &item),
            "function_call_output" => messages.push(function_call_output_message(&item)),
            "reasoning" => attach_reasoning(&mut messages, &item),
            other => {
                // Unknown item kind: degrade gracefully. Never panic,
                // never error -- the known items still parse. (Acceptance
                // C3.) `other` is client-controlled, so sanitize before it
                // reaches a structured log field (log-injection guard, per
                // routectl_core::log_safe).
                tracing::warn!(
                    item_kind = %routectl_core::sanitize_for_log(other),
                    "openai-responses ingress: skipping unknown input item kind"
                );
            }
        }
    }
    messages
}

/// Build a `message` input item into a canonical `Message`. Maps
/// `input_text` / `output_text` parts to text; preserves images and any
/// other part shape so nothing is silently dropped.
fn push_message_item(messages: &mut Vec<Message>, item: &Value) {
    let role = parse_role(item.get("role").and_then(Value::as_str));
    let content = parse_message_content(item.get("content"));
    messages.push(Message {
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    });
}

fn parse_role(role: Option<&str>) -> Role {
    match role {
        Some("assistant") => Role::Assistant,
        Some("system" | "developer") => Role::System,
        // user is the default for any other / missing role: a Responses
        // input item with no recognized role is overwhelmingly a user
        // turn, and defaulting to user keeps the conversation coherent.
        _ => Role::User,
    }
}

/// Parse a Responses `message.content` value into canonical
/// `MessageContent`. `content` is either a bare string or an array of
/// typed content blocks (`input_text` / `output_text` / `input_image` /
/// ...). Text blocks collapse; non-text and unknown blocks are preserved
/// as canonical parts (`Other` for unknown) so nothing is dropped.
fn parse_message_content(content: Option<&Value>) -> MessageContent {
    match content {
        None | Some(Value::Null) => MessageContent::Null,
        Some(Value::String(s)) => MessageContent::Text(s.clone()),
        Some(Value::Array(blocks)) => parse_content_blocks(blocks),
        // A non-string scalar content is unexpected; stringify so the
        // text survives rather than dropping it.
        Some(other) => MessageContent::Text(other.to_string()),
    }
}

fn parse_content_blocks(blocks: &[Value]) -> MessageContent {
    let mut parts: Vec<ContentPart> = Vec::with_capacity(blocks.len());
    for block in blocks {
        if let Some(part) = parse_content_block(block) {
            parts.push(part);
        }
    }
    collapse_parts(parts)
}

/// Translate one Responses content block to a canonical `ContentPart`.
/// `input_text` / `output_text` -> canonical text; `input_image` ->
/// canonical OpenAI-shape `ImageUrl`; everything else is sworn to the
/// forward-compat `Other` so an unknown block type survives the ingress.
fn parse_content_block(block: &Value) -> Option<ContentPart> {
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "input_text" | "output_text" => {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            Some(ContentPart::Known(KnownContentPart::Text {
                text: text.to_string(),
                citations: None,
                cache_control: None,
            }))
        }
        "input_image" => {
            // Responses ships the image as a flat `image_url` (a data: URI
            // or https URL) plus optional `detail`. Canonical's
            // OpenAI-shape ImageUrl carries a nested `image_url` object. A
            // block missing the url is malformed; warn + drop (mirrors the
            // egress, keeping ingress/egress behavior symmetric) rather
            // than dropping silently and leaving no triage evidence.
            let url = if let Some(u) = block.get("image_url").and_then(Value::as_str) {
                u
            } else {
                tracing::warn!(
                    "openai-responses ingress: input_image block missing image_url; dropping"
                );
                return None;
            };
            let mut image_url = Map::new();
            image_url.insert("url".into(), Value::String(url.to_string()));
            if let Some(detail) = block.get("detail").and_then(Value::as_str) {
                image_url.insert("detail".into(), Value::String(detail.to_string()));
            }
            Some(ContentPart::Known(KnownContentPart::ImageUrl {
                image_url: Value::Object(image_url),
                cache_control: None,
            }))
        }
        // Unknown / future block type: preserve verbatim as Other so the
        // payload is not silently dropped at the ingress boundary.
        _ => Some(other_part_from_block(kind, block)),
    }
}

/// Build a forward-compat `ContentPart::Other` from an unknown content
/// block, preserving the `type` tag and every other field verbatim.
fn other_part_from_block(kind: &str, block: &Value) -> ContentPart {
    let mut extras = match block {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    extras.remove("type");
    ContentPart::Other {
        type_tag: if kind.is_empty() {
            "unknown".to_string()
        } else {
            kind.to_string()
        },
        cache_control: None,
        extras,
    }
}

/// Collapse a parts vector: empty -> Null, a single text part -> Text,
/// anything else -> Parts. Matches the canonical convention that a
/// pure-text turn carries a flat `content` string.
fn collapse_parts(parts: Vec<ContentPart>) -> MessageContent {
    if parts.is_empty() {
        return MessageContent::Null;
    }
    if parts.len() == 1
        && let ContentPart::Known(KnownContentPart::Text { text, .. }) = &parts[0]
    {
        return MessageContent::Text(text.clone());
    }
    MessageContent::Parts(parts)
}

const fn user_text_message(text: String) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

// ---------------------------------------------------------------------------
// function_call -> assistant tool_calls[]
// ---------------------------------------------------------------------------

/// Attach a Responses `function_call` item to the conversation as an
/// OpenAI-shape `tool_calls` entry. It attaches to the trailing assistant
/// message when one exists; otherwise a fresh assistant turn is opened so
/// the call has a home. This mirrors the egress, which emits a
/// `function_call` input item per assistant tool call.
fn attach_function_call(messages: &mut Vec<Message>, item: &Value) {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The Responses wire carries arguments as a JSON STRING; OpenAI-shape
    // tool_calls also use a string `arguments`, so forward it verbatim.
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let tool_call = serde_json::json!({
        "id": call_id,
        "type": "function",
        "function": { "name": name, "arguments": arguments }
    });

    if let Some(last) = messages.last_mut()
        && matches!(last.role, Role::Assistant)
    {
        last.tool_calls.get_or_insert_with(Vec::new).push(tool_call);
        return;
    }
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Null,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![tool_call]),
        refusal: None,
    });
}

// ---------------------------------------------------------------------------
// function_call_output -> Role::Tool message
// ---------------------------------------------------------------------------

/// Build a canonical `Role::Tool` message from a Responses
/// `function_call_output` item. The `output` field is either a flat
/// string or an array of typed content items; both collapse to canonical
/// text content (the canonical tool message carries `tool_call_id` +
/// content, matching the openai chat shape).
fn function_call_output_message(item: &Value) -> Message {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content = match item.get("output") {
        Some(Value::String(s)) => MessageContent::Text(s.clone()),
        Some(Value::Array(blocks)) => parse_content_blocks(blocks),
        Some(Value::Null) | None => MessageContent::Null,
        Some(other) => MessageContent::Text(other.to_string()),
    };
    Message {
        role: Role::Tool,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: if call_id.is_empty() {
            None
        } else {
            Some(call_id)
        },
        tool_calls: None,
        refusal: None,
    }
}

// ---------------------------------------------------------------------------
// reasoning item -> assistant reasoning_details[]
// ---------------------------------------------------------------------------

/// Attach a Responses `reasoning` input item to the conversation as
/// canonical `reasoning_details`, tagged with `openai-responses-v1` so
/// the egress's reasoning-replay path recognizes it on the next turn.
/// The item carries `summary` (array of `summary_text`), `content`
/// (array of `reasoning_text` / `reasoning_encrypted`), and a top-level
/// `encrypted_content` signature. Each surface becomes one
/// `ReasoningDetail`. Attaches to a trailing assistant turn or opens a
/// fresh one.
fn attach_reasoning(messages: &mut Vec<Message>, item: &Value) {
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let mut details: Vec<ReasoningDetail> = Vec::new();

    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
        for entry in summary {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                details.push(reasoning_detail(
                    ReasoningDetailKind::Summary,
                    id.clone(),
                    serde_json::json!({ "text": text }),
                ));
            }
        }
    }

    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for entry in content {
            push_reasoning_content_detail(&mut details, id.clone(), entry);
        }
    }

    // The replay signature rides on its own Encrypted detail (mirrors the
    // egress response walk), so a multi-turn round-trip can re-inject it.
    if let Some(sig) = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        details.push(reasoning_detail(
            ReasoningDetailKind::Encrypted,
            id,
            serde_json::json!({ "encrypted_content": sig }),
        ));
    }

    if details.is_empty() {
        return;
    }

    if let Some(last) = messages.last_mut()
        && matches!(last.role, Role::Assistant)
    {
        last.reasoning_details.extend(details);
        return;
    }
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Null,
        reasoning: None,
        reasoning_details: details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    });
}

/// Map one inner `content` entry of a reasoning item to a
/// `ReasoningDetail`. `reasoning_text` (and the plain `text` alias) ->
/// Text; `reasoning_encrypted` -> Encrypted. Unknown entry kinds are
/// skipped.
fn push_reasoning_content_detail(
    details: &mut Vec<ReasoningDetail>,
    id: Option<String>,
    entry: &Value,
) {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "reasoning_text" | "text" => {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                details.push(reasoning_detail(
                    ReasoningDetailKind::Text,
                    id,
                    serde_json::json!({ "text": text }),
                ));
            }
        }
        "reasoning_encrypted" => {
            if let Some(sig) = entry.get("encrypted_content").and_then(Value::as_str) {
                details.push(reasoning_detail(
                    ReasoningDetailKind::Encrypted,
                    id,
                    serde_json::json!({ "encrypted_content": sig }),
                ));
            }
        }
        _ => {}
    }
}

fn reasoning_detail(
    kind: ReasoningDetailKind,
    id: Option<String>,
    payload: Value,
) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id,
        format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
        index: None,
        payload,
    }
}

// ---------------------------------------------------------------------------
// tools -> ToolDef[]
// ---------------------------------------------------------------------------

/// Translate the Responses `tools` array into canonical `ToolDef`s. A
/// flat Responses function tool (`{type:"function", name, description?,
/// parameters, strict?}`) becomes `ToolDef::Custom`; any other shape
/// passes through as `ToolDef::Other` verbatim (inverse of the egress
/// tools.rs, which emits the flat function shape from Custom and passes
/// Other through).
fn build_tools(tools: Value) -> Option<Vec<ToolDef>> {
    let arr = tools.as_array()?;
    let mut out: Vec<ToolDef> = Vec::with_capacity(arr.len());
    for tool in arr {
        out.push(build_tool(tool));
    }
    if out.is_empty() { None } else { Some(out) }
}

fn build_tool(tool: &Value) -> ToolDef {
    let is_function = tool.get("type").and_then(Value::as_str) == Some("function");
    let has_name = tool.get("name").and_then(Value::as_str).is_some();
    if is_function && has_name {
        // Rewrite the flat Responses function shape into the canonical
        // CustomTool wire shape (`{name, description?, input_schema,
        // strict?}`) and let serde build the typed variant. parameters ->
        // input_schema is the field rename canonical expects.
        if let Some(custom) = custom_tool_from_responses_function(tool) {
            return ToolDef::Custom(custom);
        }
    }
    // Builtin / unknown / malformed: pass through verbatim so the egress
    // can forward it or surface its own error.
    ToolDef::Other(tool.clone())
}

fn custom_tool_from_responses_function(tool: &Value) -> Option<routectl_core::CustomTool> {
    let obj = tool.as_object()?;
    let name = obj.get("name").and_then(Value::as_str)?.to_string();
    let mut custom = Map::new();
    custom.insert("name".into(), Value::String(name));
    if let Some(desc) = obj.get("description") {
        custom.insert("description".into(), desc.clone());
    }
    if let Some(params) = obj.get("parameters") {
        custom.insert("input_schema".into(), params.clone());
    }
    if let Some(strict) = obj.get("strict") {
        custom.insert("strict".into(), strict.clone());
    }
    serde_json::from_value(Value::Object(custom)).ok()
}

// ---------------------------------------------------------------------------
// reasoning object -> ReasoningConfig
// ---------------------------------------------------------------------------

/// Invert the egress `extras::apply_reasoning`: a Responses `reasoning`
/// object carries `effort` (and a `summary` mode routectl does not model
/// canonically). Lift `effort` into `ReasoningConfig.effort`. Returns
/// None when the object carries nothing canonical models.
fn build_reasoning(reasoning: &Value) -> Option<ReasoningConfig> {
    let obj = reasoning.as_object()?;
    let effort = obj
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    effort.as_ref()?;
    Some(ReasoningConfig {
        effort,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// text.format -> response_format
// ---------------------------------------------------------------------------

/// Extract `text.format` (the Responses structured-output surface) into
/// the canonical `response_format` slot. The egress forwards `text`
/// verbatim from provider_extras; here we lift the `format` sub-object
/// into the canonical home so structured-output config survives the
/// ingress. Returns None when no `format` is present.
fn extract_text_format(text: &Value) -> Option<Value> {
    text.as_object()?.get("format").cloned()
}

/// Strip `format` from a `text` object and return the remaining fields
/// for forward-compat storage in `provider_extras`. Returns `None` when
/// `text` is not an object or when no subfields remain after removing
/// `format` (e.g., a text object that only carried `format`).
fn text_without_format(text: Value) -> Option<Map<String, Value>> {
    let mut obj = match text {
        Value::Object(m) => m,
        _ => return None,
    };
    obj.remove("format");
    if obj.is_empty() { None } else { Some(obj) }
}

// ---------------------------------------------------------------------------
// forward-compat sweep
// ---------------------------------------------------------------------------

/// Move every key NOT handled above out of the request object into a
/// provider_extras map. `store` and `previous_response_id` are dropped
/// (not forwarded): previous_response_id already 400'd, and store is a
/// persistence intent routectl never honors. Mirrors the openai /
/// anthropic ingress forward-compat sweep so a new Responses field
/// reaches the egress without a code edit.
fn sweep_extras(obj: Map<String, Value>) -> Map<String, Value> {
    let mut extras = Map::new();
    for (k, v) in obj {
        if HANDLED_TOP_LEVEL_FIELDS.contains(&k.as_str()) {
            // store / previous_response_id are in the handled set and are
            // intentionally not forwarded.
            continue;
        }
        extras.insert(k, v);
    }
    extras
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Saturating cast of a JSON `max_output_tokens` (u64) to the canonical
/// `max_tokens` (u32). Values above u32::MAX saturate with a WARN rather
/// than wrapping silently (mirrors the anthropic ingress budget clamp).
fn clamp_u32(n: u64) -> u32 {
    if n > u64::from(u32::MAX) {
        tracing::warn!(
            requested = n,
            capped = u32::MAX,
            "openai-responses ingress: max_output_tokens exceeds u32::MAX; saturating"
        );
        u32::MAX
    } else {
        n as u32
    }
}

const fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
