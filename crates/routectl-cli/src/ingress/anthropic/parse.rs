use axum::http::HeaderMap;
use serde_json::{Map, Value};

use routectl_core::cache_control;
use routectl_core::{ChatRequest, Error, ReasoningConfig, Result, sanitize_detail_for_log};

// Referenced only by the inline test module (`parse_tests.rs`) via
// `use super::*`; test-gated so they do not flag as unused in the
// non-test build now that the breakpoint walk moved to routectl-core.
#[cfg(test)]
use routectl_core::{ContentPart, MessageContent};

use crate::ingress::read_alias_header;
use crate::ingress::session_key::resolve_session_key;

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

    // Same inbound reasoning-payload normalization the openai ingress
    // runs. Shared so the two dialects cannot drift back apart on which
    // reasoning vocabulary they accept.
    routectl_core::normalize_reasoning_detail_payloads(&mut req);

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
    // ...) arrive on the header surface. The egress emits the merged
    // values on the upstream `anthropic-beta` HTTP header
    // (api.anthropic.com rejects the body-level field on OAuth
    // flavors), so routing through canonical normalizes both wire
    // shapes onto one egress path. Comma-separated header values are
    // split + trimmed
    // and merged with any existing body-level `anthropic_beta`,
    // preserving order and dropping duplicates.
    merge_inbound_anthropic_beta_header(headers, &mut req);

    // Capture inbound X-Claude-Code-* headers so the Anthropic-API
    // egress can forward them upstream for gateway cost attribution
    // (see `capture_claude_code_headers` for the contract).
    capture_claude_code_headers(headers, &mut req);

    // Stamp ingress provenance so downstream observability can attribute
    // the request to the Anthropic Messages dialect.
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;

    // Capture the INBOUND per-conversation key (header wins, then
    // body `metadata.session_id`). Borrows `metadata` so it stays intact
    // for the round-trip re-insertion into extras below. Never logged raw.
    req.routectl_internal.inbound_session_key = resolve_session_key(
        inbound_session_header(headers),
        metadata_session_id(&metadata),
    );

    // Translate thinking config. The raw `display` string rides
    // `routectl_internal` so the Anthropic-API egress can re-emit a
    // vocabulary the canonical boolean does not model.
    if let Some(t) = thinking {
        let (reasoning, raw_display) = translate_thinking(&t)?;
        req.reasoning = Some(reasoning);
        req.routectl_internal.anthropic_thinking_display = raw_display;
    }

    // Lift output_config.effort into canonical req.reasoning.effort.
    // claude-code 2.1.153+ sends thinking and output_config.effort as
    // separate fields; cross-dialect egresses read req.reasoning.effort
    // and would miss the value buried in provider_extras["output_config"]
    // without this lift. output_config stays in extras so the Anthropic-API
    // egress can forward the full object (including format and other beta
    // fields) upstream unchanged.
    if let Some(eff) = extras
        .get("output_config")
        .and_then(|oc| oc.get("effort"))
        .and_then(|v| v.as_str())
    {
        req.reasoning
            .get_or_insert_with(ReasoningConfig::default)
            .effort = Some(eff.to_string());
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

/// The Anthropic dialect's HEADER vocabulary for inbound session identity:
/// the single `x-claude-code-session-id` header (axum lowercases inbound
/// names). Trim, precedence, bounds and the conflict WARN all live in
/// [`resolve_session_key`], shared with the OpenAI-shaped ingresses.
fn inbound_session_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-claude-code-session-id")
        .and_then(|h| h.to_str().ok())
}

/// The Anthropic dialect's BODY vocabulary for inbound session identity:
/// the nested `metadata.session_id`. Borrows, so the `metadata` object
/// still round-trips into `provider_extras`.
fn metadata_session_id(metadata: &Option<Value>) -> Option<&str> {
    metadata
        .as_ref()
        .and_then(|m| m.as_object())
        .and_then(|o| o.get("session_id"))
        .and_then(|v| v.as_str())
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
    for hv in &headers.get_all("anthropic-beta") {
        let Ok(s) = hv.to_str() else {
            tracing::warn!("anthropic ingress: anthropic-beta header is not valid UTF-8; ignoring");
            continue;
        };
        for piece in s.split(',') {
            let trimmed = piece.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !is_safe_beta_value(trimmed) {
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

/// Defense-in-depth filter for inbound `anthropic-beta` header values:
/// reject pieces containing CR or LF.
///
/// `HeaderValue::to_str` already rejects control bytes (so this filter
/// would not currently fire on inbound axum-decoded headers), but a
/// future refactor that switches to a byte-level decode -- or any code
/// path that synthesizes a `Vec<String>` of betas through a different
/// route -- could otherwise allow `legit-beta\r\nX-Injected: evil` to
/// flow through into the outbound `anthropic-beta` HTTP header on the
/// egress side. The http crate would reject the egress emission, but
/// failing here keeps the wire surface explicit and unit-testable in
/// isolation (we can drive the filter directly with CR/LF-bearing
/// strings, bypassing `HeaderValue`'s defense).
fn is_safe_beta_value(s: &str) -> bool {
    !s.contains(['\r', '\n'])
}

/// Capture inbound `x-claude-code-*` headers (case-insensitive prefix
/// match on namespace) into `req.routectl_internal.claude_code_headers`
/// for later filtering at the egress. Per the LLM gateway docs, the
/// three documented gateway-attribution headers are
/// `x-claude-code-session-id`, `x-claude-code-agent-id`, and
/// `x-claude-code-parent-agent-id`, but Anthropic owns the namespace
/// and may add more. The Anthropic-API egress consults its
/// per-provider `forward_client_headers` config to decide which
/// captured names actually go upstream; everything else is dropped.
/// Skips non-UTF-8 values.
///
/// Header name casing: axum/http normalizes inbound `HeaderMap` keys
/// to lowercase on receive, so `name.as_str()` here is always
/// lowercase regardless of how the client wrote the header. The
/// captured Vec stores the lowercase form, and the Anthropic-API
/// egress emits it lowercase upstream. This is standards-compliant:
/// HTTP/2 requires lowercase header names, and HTTP/1.1 servers are
/// case-insensitive on receive (RFC 7230 sec 3.2). Operators
/// expecting a specific case in upstream-traffic captures should not
/// rely on the original wire casing.
fn capture_claude_code_headers(headers: &HeaderMap, req: &mut ChatRequest) {
    for (name, val) in headers {
        if !name
            .as_str()
            .to_ascii_lowercase()
            .starts_with("x-claude-code-")
        {
            continue;
        }
        let Ok(v) = val.to_str() else { continue };
        req.routectl_internal
            .claude_code_headers
            .push((name.as_str().to_string(), v.to_string()));
    }
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
    // A JSON null output_format means "not set"; treat it the same as
    // the field being absent. Promoting null into output_config.format
    // causes a 400 from api.anthropic.com because the structured-output
    // spec requires format to be a non-null object when present.
    if legacy.is_null() {
        return output_config;
    }
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

fn translate_thinking(t: &Value) -> Result<(ReasoningConfig, Option<String>)> {
    let kind = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Anthropic's `budget_tokens` is JSON-int (effectively u64) but
    // the canonical `ReasoningConfig.max_tokens` is u32. A naked cast
    // would silently truncate values above u32::MAX (~4.29B) -- not
    // reachable today since Anthropic caps the field at 100k, but
    // saturating to u32::MAX with a WARN keeps the request consistent
    // with what the caller asked for and surfaces bizarre input
    // instead of corrupting it.
    let budget = t
        .get("budget_tokens")
        .and_then(serde_json::Value::as_u64)
        .map(|n| {
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
    // `display` is read for every thinking type, not only the ones that
    // honor it: the raw string rides the carrier upstream regardless of
    // which arm below consumes the derived boolean.
    let (display, raw_display) = translate_thinking_display(t)?;
    let reasoning = match kind {
        "enabled" => ReasoningConfig {
            enabled: Some(true),
            max_tokens: budget,
            exclude: display,
            ..Default::default()
        },
        "disabled" => ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        },
        "adaptive" => ReasoningConfig {
            enabled: Some(true),
            exclude: display,
            ..Default::default()
        },
        _ => {
            // An unrecognized `type` collapses the WHOLE thinking object:
            // `thinking` is stripped from the body during parse and is a
            // managed key in the egress extras merge, so nothing rides
            // through. Forwarding the object verbatim needs a carrier that
            // no wire shape justifies yet -- only the three `type` values
            // matched above exist upstream today. This WARN is the detector
            // for that: an operator hit means a fourth value shipped and the
            // carrier is now warranted. Accepting instead of rejecting keeps
            // a newer vocabulary from earning a local 400.
            tracing::warn!(
                thinking_type = %sanitize_detail_for_log(kind),
                "anthropic ingress: unrecognized thinking.type; thinking object not forwarded"
            );
            ReasoningConfig::default()
        }
    };
    Ok((reasoning, raw_display))
}

/// Map Anthropic's `thinking.display` onto the canonical
/// `ReasoningConfig.exclude`, and hand back the raw string so the caller
/// can park it on `routectl_internal.anthropic_thinking_display`.
///
/// Any string forwards: the canonical boolean models only the values this
/// hub understands, so a value it does not model gets `exclude = None`
/// and travels on the carrier instead of earning a local 400. Known
/// values also derive the boolean -- `"omitted"` and `"updates"` both
/// suppress the thinking text (their response shape is empty text plus a
/// signature), `"summarized"` keeps it.
///
/// A non-string `display` is still rejected locally: the carrier is a
/// `String`, so there is no shape to forward.
///
/// Absent stays absent (`None`): Anthropic's default is model-dependent,
/// and materializing an explicit value here would override whatever a
/// newer model chooses for itself.
fn translate_thinking_display(t: &Value) -> Result<(Option<bool>, Option<String>)> {
    let Some(raw) = t.get("display") else {
        return Ok((None, None));
    };
    let Some(requested) = raw.as_str() else {
        return Err(Error::Validation(format!(
            "anthropic ingress: thinking.display must be a string, got {}",
            value_type_name(raw)
        )));
    };
    let exclude = match requested {
        "omitted" | "updates" => Some(true),
        "summarized" => Some(false),
        _ => {
            // The unmodeled value goes to the debug log only, never into a
            // response body -- it still reaches upstream verbatim on the
            // carrier, so nothing is lost by keeping it out of the logs at
            // INFO and above.
            tracing::debug!(
                thinking_display = %sanitize_detail_for_log(requested),
                "anthropic ingress: thinking.display not modeled by reasoning.exclude; \
                 forwarding verbatim"
            );
            return Ok((None, Some(requested.to_string())));
        }
    };
    tracing::debug!(
        // Reached only from the arms above, so `requested` is one of three
        // literals here -- never a caller-chosen string.
        thinking_display = ?requested,
        exclude = ?exclude,
        "anthropic ingress: thinking.display translated to reasoning.exclude"
    );
    Ok((exclude, Some(requested.to_string())))
}

fn validate_request_cache_control(req: &ChatRequest) -> Result<()> {
    // The canonical breakpoint walk lives in routectl-core
    // (`CacheBreakpointSource for ChatRequest`); reuse it so the ingress,
    // the Anthropic egress, and the Bedrock egress all validate through
    // one traversal. Owned-control collection (needed for
    // `ToolDef::Other`'s on-demand parse) is handled inside
    // `validate_source`.
    cache_control::validate_source(req)
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "parse_thinking_display_tests.rs"]
mod thinking_display_tests;

#[cfg(test)]
#[path = "parse_thinking_type_tests.rs"]
mod thinking_type_tests;
