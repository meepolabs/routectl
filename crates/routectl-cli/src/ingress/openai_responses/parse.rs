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
//!   - unknown item kind                -> preserved verbatim for a
//!     same-dialect Responses egress to replay (never 500)
//! - `tools`                            -> `tools[]` (ToolDef)
//! - `additional_tools` input item      -> its function declarations merged
//!   into `tools[]`, a later same-name function replacing the earlier one;
//!   every other declaration stays out of `tools[]` (the item is still
//!   preserved verbatim for same-dialect replay). "Function declaration"
//!   means one `CustomTool::from_responses_function` accepts; the Responses
//!   egress applies the same predicate, `CustomTool::responses_function_name`,
//!   to the replayed item.
//! - `tool_choice`                      -> `tool_choice` (named-forcing shape normalized to nested)
//! - `reasoning` (object)               -> `reasoning` (ReasoningConfig)
//! - `max_output_tokens`                -> `max_tokens`
//! - `text.format`                      -> `response_format`
//! - `model`                            -> `model` (alias-header override)
//! - everything else                    -> `provider_extras` (forward-compat)
//!
//! Statefulness: a store-backed adapter replays the full stored conversation
//! (canonical messages AND positioned passthrough items), prior output, then
//! fresh input. A storeless adapter refuses previous_response_id rather than
//! silently answering without history.

use axum::http::HeaderMap;
use serde_json::{Map, Value};

use routectl_core::{
    ChatRequest, ContentPart, CustomTool, Error, KnownContentPart, Message, MessageContent,
    ReasoningConfig, ReasoningDetail, ReasoningDetailKind, ResponsesPassthroughItem, Result, Role,
    SystemContent, ToolDef,
};

use crate::ingress::read_alias_header;
use crate::ingress::session_key::{first_session_header, resolve_session_key};
use routectl_core::OPENAI_RESPONSES_V1;

/// Responses `input[]` item kind that declares tools inline instead of in
/// the top-level `tools` array (responses-lite clients omit `tools`
/// entirely and send every declaration this way).
const ADDITIONAL_TOOLS_ITEM: &str = "additional_tools";

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

pub(super) fn translate_request(
    headers: &HeaderMap,
    body: Value,
    store: Option<&super::store::ResponsesStore>,
) -> Result<ChatRequest> {
    let mut obj = match body {
        Value::Object(map) => map,
        _ => {
            return Err(Error::Validation(
                "openai-responses ingress: request body is not an object".into(),
            ));
        }
    };

    // Statefulness: a `previous_response_id` resolves against the
    // bounded response store when the adapter carries one (server
    // wiring). Resolution replays the FULL stored conversation context
    // plus the prior response's own output items before this turn's
    // input -- matching the official API's server-side semantics.
    // Storeless (library) builds keep the historical contract: a clear
    // 400 instead of a silent wrong answer.
    let prev_entry = if let Some(prev_id) = obj
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    {
        let Some(store) = store else {
            return Err(Error::Validation(
                "openai-responses ingress: previous_response_id requires a response store; \
                 this adapter was built stateless. Send the full conversation each turn."
                    .into(),
            ));
        };
        let entry = store.get_full(&prev_id).ok_or_else(|| {
            Error::Validation(format!(
                "openai-responses ingress: previous_response_id `{prev_id}` not found \
                 (unknown id, evicted from the bounded store, or the daemon restarted)"
            ))
        })?;
        Some(entry)
    } else {
        None
    };
    // store: spec default true. Honored when the store is wired;
    // storeless builds warn that nothing was kept.
    if obj.get("store").and_then(Value::as_bool) == Some(true) && store.is_none() {
        warn_on_store(&obj);
    }

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

    // input -> messages[]. Unmodeled item kinds are captured verbatim
    // into routectl_internal for a same-dialect Responses egress replay
    // (see build_messages) rather than dropped. Tool declarations from
    // `additional_tools` items are held until the top-level tools parse.
    let additional_tools: Vec<Value> = match obj.remove("input") {
        Some(input) => {
            let ParsedInput {
                messages,
                passthrough,
                additional_tools,
            } = build_messages(input);
            req.messages = messages.into();
            req.routectl_internal.responses_input_passthrough = passthrough;
            additional_tools
        }
        None => Vec::new(),
    };

    // Chain resolution AFTER the turn's own input parsed: stored
    // context first (prior turns), then the prior response's own
    // output items, then this turn's fresh input. `build_messages`
    // walked the input into `req.messages` above; the stored replay
    // PREPENDS to it so canonical message order is conversation order.
    if let Some((stored_response, stored_context)) = prev_entry {
        let mut replayed = stored_context.messages;
        req.routectl_internal.responses_system_history =
            (!stored_context.system_history.is_empty())
                .then(|| std::sync::Arc::new(stored_context.system_history));
        let mut passthrough = stored_context.passthrough;
        let output = build_messages(stored_response.get("output").cloned().unwrap_or_default());
        let context_len = message_prefix_len(&replayed);
        passthrough.extend(output.passthrough.into_iter().map(|mut p| {
            p.modeled_prefix += context_len;
            p
        }));
        replayed.extend(output.messages);
        let history_len = message_prefix_len(&replayed);
        passthrough.extend(
            req.routectl_internal
                .responses_input_passthrough
                .drain(..)
                .map(|mut p| {
                    p.modeled_prefix += history_len;
                    p
                }),
        );
        replayed.extend(req.messages.iter().cloned());
        req.messages = replayed.into();
        req.routectl_internal.responses_input_passthrough = passthrough;
    }

    // The Responses `store` flag: spec default is `true`. Read BEFORE the
    // sweep strips the key (it is in the handled set, never forwarded).
    // Rides `routectl_internal` (transport-internal, skip-serialized) so
    // the render/stream paths can honor it without forwarding the flag
    // to any upstream.
    req.routectl_internal.responses_store =
        obj.get("store").and_then(Value::as_bool).unwrap_or(true);

    // Keep only in-array system provenance for chaining. Prior top-level
    // instructions deliberately do not carry across previous_response_id.
    let mut history = req
        .routectl_internal
        .responses_system_history
        .take()
        .map(std::sync::Arc::unwrap_or_clone)
        .unwrap_or_default();
    history.extend(
        req.messages
            .iter()
            .filter(|m| matches!(m.role, Role::System))
            .cloned(),
    );
    let mut messages = history.clone();
    messages.extend(
        req.messages
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .cloned(),
    );
    req.messages = messages.into();
    req.routectl_internal.responses_system_history =
        (!history.is_empty()).then(|| std::sync::Arc::new(history));
    crate::ingress::lift_system_messages(&mut req);

    // tools -> ToolDef[].
    if let Some(tools) = obj.remove("tools") {
        req.tools = build_tools(tools);
    }
    req.tools = merge_additional_tools(req.tools.take(), &additional_tools);

    // tool_choice -> canonical tool_choice. The Responses wire uses a
    // flat named-forcing shape; normalize it to the nested OpenAI form
    // every egress mapper already consumes (see normalize_tool_choice).
    if let Some(tc) = obj.remove("tool_choice")
        && !tc.is_null()
    {
        req.tool_choice = Some(normalize_tool_choice(tc));
    }

    // reasoning object -> ReasoningConfig (effort) + a provider_extras
    // remainder carrying the Responses-dialect sub-keys (summary/context/
    // mode/any future field) canonical ReasoningConfig does not model.
    // Mirrors the text/text_without_format remainder handling below; the
    // remainder is merged into provider_extras after the sweep.
    let reasoning_remainder = match obj.remove("reasoning") {
        Some(reasoning) => {
            let (cfg, remainder) = build_reasoning(reasoning);
            req.reasoning = cfg;
            remainder
        }
        None => None,
    };

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
    // Merge the reasoning remainder (summary/context/mode/future) so the
    // Responses egress can re-emit it; every non-Responses egress drops it
    // as a routectl-managed key (leak-guard lives on those egresses).
    if let Some(rem) = reasoning_remainder {
        extras.insert("reasoning".into(), Value::Object(rem));
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

    // Capture the INBOUND per-conversation key: a curated allowlist header
    // first, else the body's top-level `prompt_cache_key`. Read AFTER the
    // sweep installs the extras so one read covers the whole body-side
    // vocabulary. The copy in provider_extras is left untouched, so the
    // egress still forwards it verbatim.
    // Owned so the read is finished before the assignment borrows `req`.
    let body_session_key = req
        .provider_extras
        .as_ref()
        .and_then(|extras| extras.get("prompt_cache_key"))
        .and_then(Value::as_str)
        .map(str::to_string);
    req.routectl_internal.inbound_session_key =
        resolve_session_key(first_session_header(headers), body_session_key.as_deref());

    Ok(req)
}

// ---------------------------------------------------------------------------
// Statefulness contract
// ---------------------------------------------------------------------------

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

/// The result of walking a Responses `input` field: the canonical
/// conversation turns plus any `input[]` items whose `type` this ingress
/// does not model. The unmodeled items are carried verbatim so a
/// same-dialect Responses egress can replay them (see the passthrough
/// note on [`build_messages_from_items`]).
struct ParsedInput {
    messages: Vec<Message>,
    passthrough: Vec<ResponsesPassthroughItem>,
    /// Tool declarations carried by `additional_tools` input items, in
    /// input order.
    additional_tools: Vec<Value>,
}

/// Turn the Responses `input` field into canonical `messages[]`. `input`
/// is either a bare string (one user message) or an array of tagged
/// items. Tool calls collected from `function_call` items attach to the
/// most recent assistant message so the canonical assistant turn carries
/// its `tool_calls`. Item kinds this ingress does not model are captured
/// into `ParsedInput::passthrough` rather than dropped.
fn build_messages(input: Value) -> ParsedInput {
    match input {
        Value::String(text) => ParsedInput {
            messages: vec![user_text_message(text)],
            passthrough: Vec::new(),
            additional_tools: Vec::new(),
        },
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
            ParsedInput {
                messages: Vec::new(),
                passthrough: Vec::new(),
                additional_tools: Vec::new(),
            }
        }
    }
}

fn build_messages_from_items(items: Vec<Value>) -> ParsedInput {
    let mut messages: Vec<Message> = Vec::with_capacity(items.len());
    let mut passthrough: Vec<ResponsesPassthroughItem> = Vec::new();
    let mut additional_tools: Vec<Value> = Vec::new();
    // Passthrough positions are canonical message boundaries, not estimated
    // wire-item counts: a lane may expand or discard a modeled message.
    let mut modeled = Vec::new();
    let mut modeled_prefix = 0;
    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        // A passthrough item is a grouping barrier. Never attach a later
        // call/reasoning item to an assistant turn before that barrier:
        // egress would then move the native item across the preserved one.
        match kind {
            "message" => push_message_item(&mut modeled, &item),
            "" if item.get("role").is_some() => push_message_item(&mut modeled, &item),
            "function_call" => attach_function_call(&mut modeled, &item),
            "function_call_output" => modeled.push(function_call_output_message(&item)),
            "reasoning" => attach_reasoning(&mut modeled, &item),
            other => {
                modeled_prefix += message_prefix_len(&modeled);
                messages.append(&mut modeled);
                if other == ADDITIONAL_TOOLS_ITEM
                    && let Some(declared) = item.get("tools").and_then(Value::as_array)
                {
                    additional_tools.extend(declared.iter().cloned());
                }
                // Unmodeled item kind: preserve it verbatim for a
                // same-dialect Responses egress to replay, mirroring how
                // unknown CONTENT blocks survive as `ContentPart::Other`.
                // codex's native kinds (local_shell_call,
                // custom_tool_call(_output), tool_search_call,
                // agent_message, ...) would otherwise vanish on replay and
                // degrade multi-turn context. Never panic, never error --
                // the known items still parse. `other` is client-controlled,
                // so sanitize it before it reaches a structured log field
                // (log-injection guard, per routectl_core::log_safe). Logged
                // at debug: these kinds appear on every codex turn, so a
                // WARN would spam a normal session.
                tracing::debug!(
                    item_kind = %routectl_core::sanitize_for_log(other),
                    "openai-responses ingress: preserving unmodeled input item kind for round-trip replay"
                );
                passthrough.push(ResponsesPassthroughItem {
                    modeled_prefix,
                    item,
                });
            }
        }
    }
    messages.extend(modeled);
    ParsedInput {
        messages,
        passthrough,
        additional_tools,
    }
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
        format: Some(OPENAI_RESPONSES_V1.to_string()),
        index: None,
        payload,
    }
}

// ---------------------------------------------------------------------------
// tools -> ToolDef[]
// ---------------------------------------------------------------------------

/// Normalize a Responses named-forcing `tool_choice` into the canonical
/// nested form every egress mapper already consumes.
///
/// The Responses wire forces a named tool with the flat shape
/// `{"type":"function","name":"X"}`. Canonical -- and the shape both the
/// Anthropic and openai-compat egress mappers already translate
/// successfully -- is the nested OpenAI form
/// `{"type":"function","function":{"name":"X"}}`. Normalizing here means
/// every egress sees one shape (the Responses egress reads
/// `function.name` too, so its path is unaffected). Every other shape
/// (bare strings, already-nested objects, unrecognized objects) passes
/// through verbatim.
fn normalize_tool_choice(tc: Value) -> Value {
    let Value::Object(map) = &tc else {
        return tc;
    };
    if map.get("type").and_then(Value::as_str) != Some("function") || map.contains_key("function") {
        return tc;
    }
    match map.get("name").and_then(Value::as_str) {
        Some(name) => serde_json::json!({"type": "function", "function": {"name": name}}),
        None => tc,
    }
}

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

/// Merge the function declarations carried by `additional_tools` input
/// items into the top-level tools, in input order. A later function with an
/// already-merged name replaces the earlier one in place, because the
/// same-dialect replay emits the inline item verbatim and every other lane
/// must route and serialize that same definition.
///
/// Only declarations that normalize to `ToolDef::Custom` are admitted.
/// Hosted, MCP, namespace, unknown, and malformed declarations have no
/// portable canonical form and may carry server URLs or credentials, so they
/// reach only the verbatim Responses replay, never another dialect's wire.
fn merge_additional_tools(
    tools: Option<Vec<ToolDef>>,
    additional: &[Value],
) -> Option<Vec<ToolDef>> {
    if additional.is_empty() {
        return tools;
    }
    let mut merged = tools.unwrap_or_default();
    for candidate in additional
        .iter()
        .filter_map(CustomTool::from_responses_function)
    {
        match merged
            .iter()
            .position(|known| matches!(known, ToolDef::Custom(k) if k.name == candidate.name))
        {
            Some(at) => merged[at] = ToolDef::Custom(candidate),
            None => merged.push(ToolDef::Custom(candidate)),
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// A flat Responses function declaration becomes `ToolDef::Custom`; every
/// other value -- builtin, unknown, or a malformed function -- passes
/// through verbatim so the egress can forward it or surface its own error.
fn build_tool(tool: &Value) -> ToolDef {
    CustomTool::from_responses_function(tool)
        .map_or_else(|| ToolDef::Other(tool.clone()), ToolDef::Custom)
}

// ---------------------------------------------------------------------------
// reasoning object -> ReasoningConfig
// ---------------------------------------------------------------------------

/// The split of a Responses `reasoning` object: the canonical config
/// (effort only) and the Responses-dialect remainder destined for
/// `provider_extras["reasoning"]`.
type ReasoningSplit = (Option<ReasoningConfig>, Option<Map<String, Value>>);

/// Split a Responses `reasoning` object into (canonical config, remainder).
///
/// `effort` lifts into `ReasoningConfig.effort` -- the single knob canonical
/// models. Every other sub-key (`summary`, `context`, `mode`, any future
/// field) is a Responses-dialect control with no canonical home; it is
/// returned as a remainder map the caller stashes under
/// `provider_extras["reasoning"]` so the Responses egress can re-emit it.
/// Sub-key vocabulary is not policed here -- values and types ride verbatim
/// onto the Responses wire, so upstream owns their validity. Returns
/// `(None, None)` when the value is not an object.
fn build_reasoning(reasoning: Value) -> ReasoningSplit {
    let mut obj = match reasoning {
        Value::Object(m) => m,
        _ => return (None, None),
    };

    let effort = obj
        .remove("effort")
        .and_then(|v| v.as_str().map(str::to_string));
    let cfg = effort.map(|e| ReasoningConfig {
        effort: Some(e),
        ..Default::default()
    });

    // A null-valued sub-key means "unset" -- drop it so it neither rides
    // to the egress nor blocks the egress "auto" summary default.
    obj.retain(|_, v| !v.is_null());
    let remainder = if obj.is_empty() { None } else { Some(obj) };
    (cfg, remainder)
}

// ---------------------------------------------------------------------------
// text.format -> response_format
// ---------------------------------------------------------------------------

/// Extract `text.format` (the Responses structured-output surface) into
/// the canonical `response_format` slot, normalized to the nested
/// OpenAI Chat-Completions shape every egress mapper already consumes.
/// Returns None when no `format` is present.
///
/// The Responses wire spells a schema format FLAT
/// (`{"type":"json_schema","name":X,"schema":{...},"strict":B}`).
/// Canonical is the nested form
/// (`{"type":"json_schema","json_schema":{name,schema,strict}}`) --
/// what the openai and anthropic ingresses already produce and what
/// all five egresses read. Normalizing here means every egress sees
/// one shape; the openai-compat egress in particular has no reader at
/// all and serializes the slot verbatim, so a flat value reaches a
/// strict host as an unsupported `response_format` and is rejected.
/// Every other tag (unknown tags, non-objects, objects with no string
/// `type`) passes through verbatim, mirroring `normalize_tool_choice`.
fn extract_text_format(text: &Value) -> Option<Value> {
    let format = text.as_object()?.get("format").cloned()?;
    Some(nest_json_schema_format(format))
}

/// Move every non-`type` key of a flat `json_schema` format into a
/// `json_schema` member. Wholesale rather than a named `name`/`schema`/
/// `strict` triple, so a future sibling on the Responses wire survives
/// the trip. The inverse lives at the Responses egress, which carries
/// the whole member back out.
///
/// A schema-less `json_schema` gains an empty member rather than riding
/// through flat, so an egress reader meets one shape for the tag. The
/// rewrite is total on the tag EXCEPT a format already carrying a
/// `json_schema` object, which is already canonical and rides through.
fn nest_json_schema_format(format: Value) -> Value {
    let Value::Object(mut map) = format else {
        return format;
    };
    // An already-nested value is canonical: a Chat-shaped `text.format`
    // reaches this ingress too, and nesting it again buries the schema one
    // level deeper than every egress reader looks. Same guard as
    // `normalize_tool_choice`.
    if map.get("type").and_then(Value::as_str) != Some("json_schema")
        || map.get("json_schema").is_some_and(Value::is_object)
    {
        return Value::Object(map);
    }
    map.remove("type");
    let mut out = Map::new();
    out.insert("type".into(), Value::from("json_schema"));
    out.insert("json_schema".into(), Value::Object(map));
    Value::Object(out)
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

/// Canonical message boundaries survive system lifting and lane-specific
/// wire expansion. A preserved item is never grouped with later input.
fn message_prefix_len(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .count()
}

/// Move unhandled top-level keys into provider_extras. The local store and
/// previous_response_id controls were already consumed, never forwarded.
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
