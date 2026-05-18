use super::*;

pub(super) fn translate_request(headers: &HeaderMap, mut body: Value) -> Result<ChatRequest> {
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
            // Defense-in-depth: reject pieces containing CR or LF.
            // `HeaderValue::to_str` already rejects control bytes
            // (so this branch would not currently fire on inbound
            // axum-decoded headers), but a future refactor that
            // switches to a byte-level decode -- or a test that
            // synthesizes the value through a different path --
            // would otherwise allow `legit-beta\r\nX-Injected:
            // evil` to flow through into our outbound
            // `anthropic-beta` HTTP header on the egress side.
            // Drop with a WARN; the http crate would reject these
            // on the egress build, but failing here keeps the wire
            // surface explicit.
            if trimmed.contains(['\r', '\n']) {
                tracing::warn!(
                    "anthropic ingress: anthropic-beta value contains CR/LF; \
                     dropping (possible header-injection attempt) value_len={}",
                    trimmed.len(),
                );
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

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
