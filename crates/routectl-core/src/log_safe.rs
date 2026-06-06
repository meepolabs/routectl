//! Log-safe rendering of client-controlled strings.
//!
//! Tracing's default `fmt` subscriber writes structured fields with their
//! Display impl directly into the formatted line. A field value that
//! contains `\n`, `\r`, or ANSI escape sequences will appear in the log
//! output verbatim -- which lets a malicious caller inject fake log
//! lines, hide subsequent output behind ANSI cursor manipulation, or
//! pad a span field beyond any sane size. `sanitize_for_log` filters
//! non-printable bytes (replacing them with `?` so operators see
//! filtering happened) and caps length so a 1MB field can't bloat
//! every log line.
//!
//! Use everywhere a client-controlled string flows into a tracing
//! field: span fields on `#[instrument(... fields(x = %y))]`,
//! `tracing::info!(x = %y, ...)` calls, or anywhere the value reaches
//! the configured subscriber.
//!
//! ## Triage-tracing surfaces (4 directions)
//!
//! For request-body diagnosis there are four helpers, each gated on
//! `tracing::Level::TRACE` so the default `info` level pays nothing:
//!
//! - [`trace_ingress_body`]          -- direction 1: client -> routectl
//! - [`trace_outgoing_body`]         -- direction 2: routectl -> upstream
//! - [`trace_upstream_success_body`] -- direction 3: upstream -> routectl (2xx)
//! - [`trace_egress_body`]           -- direction 4: routectl -> client
//! - [`trace_stream_summary`]        -- one-line SSE termination summary (NOT per-chunk)
//!
//! Plus 4xx/5xx error bodies via [`debug_upstream_error_body`] at DEBUG.
//!
//! All four body helpers honor `ROUTECTL_LOG_REDACT_PROMPTS=1`: when
//! set, [`redact_prompts_in`] strips known prompt fields (text blocks,
//! tool_use input, instructions, refusal blocks, image data URIs,
//! Bedrock Converse `toolUse.input` and `toolResult.content[*].json`)
//! and replaces them with `<redacted len=N>` while preserving
//! structural fields (model, tools, sampling params, finish_reason,
//! usage). Best-effort: the walker is keyed off known wire shapes
//! and an unknown shape can still leak. Operators flipping TRACE in
//! a sensitive environment should set the redact knob.
//!
//! ## Helper arg shapes
//!
//! Note the asymmetry: ingress / egress helpers take 2 args (the
//! ingress dialect carries one identifier); upstream helpers take 3
//! (provider-kind + provider-id):
//!
//! ```ignore
//! trace_ingress_body(ingress, body)               // direction 1
//! trace_outgoing_body(provider_kind, id, body)    // direction 2
//! trace_upstream_success_body(provider_kind, id, body) // direction 3
//! trace_egress_body(ingress, body)                // direction 4
//! ```
//!
//! ## Side-channel caveat
//!
//! The `<redacted len=N>` placeholder reveals the char count of the
//! original content. For short fixed-vocabulary prompts (e.g. "yes"
//! vs "no" tool confirmations), the length alone disambiguates the
//! prompt. Operators with strict confidentiality requirements should
//! treat redacted traces as a length-leaking side channel rather
//! than as fully sanitized output.

/// Maximum number of *characters* (not bytes) emitted into a log
/// field. Anything past this is silently truncated. 256 chars is
/// large enough to fit any legitimate model id, alias name, or
/// request id, while still preventing megabyte-sized payloads from
/// bloating every log line.
const MAX: usize = 256;

/// Core sanitizer: replace every non-printable-ASCII char with `?`,
/// capped at `cap` characters. Spaces are preserved. Used by both
/// [`sanitize_for_log`] (256-char cap) and [`debug_upstream_error_body`]
/// (4 KB cap) so the control-char stripping logic lives in one place.
fn sanitize_capped(s: &str, cap: usize) -> String {
    let mut out = String::with_capacity(s.len().min(cap));
    for c in s.chars().take(cap) {
        if c.is_ascii_graphic() || c == ' ' {
            out.push(c);
        } else {
            // Visible placeholder so operators see something was
            // filtered without us re-emitting the byte.
            out.push('?');
        }
    }
    out
}

/// Sanitize a client-controlled string for inclusion in a tracing
/// field or log message. Replaces every non-printable-ASCII char with
/// `?` and caps total length at [`MAX`] characters. Spaces are
/// preserved (single-line log fields commonly contain them).
///
/// Returns an owned `String`; the caller passes it to tracing via
/// `%sanitized` (Display) so the formatted output already carries
/// the sanitized form.
pub fn sanitize_for_log(s: &str) -> String {
    sanitize_capped(s, MAX)
}

/// Trim and sanitize an upstream error body for inclusion in routectl's
/// error envelope or `body_excerpt=...` log fields. If the upstream
/// returned HTML (a marketing 404 page from a misconfigured base_url,
/// a CDN error page, etc.), strip it down to a short marker rather
/// than dumping kilobytes of markup. Otherwise truncate to
/// [`crate::MAX_LOG_BODY_EXCERPT`] characters with a `... [truncated]`
/// tail. Used by openai_compat, anthropic_api, and bedrock so
/// operators see consistent excerpt shapes when grepping `body_excerpt`
/// across providers.
pub fn sanitize_upstream_body(body: &str) -> String {
    sanitize_upstream_body_with_cap(body, crate::MAX_LOG_BODY_EXCERPT)
}

/// Extract the human-readable error message from a 4xx/5xx upstream
/// body. Tries the standard `{"error":{"message":"..."}}` shape that
/// OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages
/// all use. Falls back to a sanitized excerpt of the raw body when
/// the shape doesn't match.
///
/// Operators reading `body_excerpt=...` log fields then see a clean
/// error string ("Incorrect API key provided") instead of the JSON
/// envelope ("{\"error\":{\"message\":\"Incorrect API key provided\",...}").
/// Bedrock uses a different AWS-shaped envelope (`/message` or
/// `/Message`) and keeps its own bespoke extractor; this helper is
/// for the OpenAI / Anthropic family.
pub fn extract_upstream_message(body_text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body_text)
        .ok()
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| sanitize_upstream_body(body_text))
}

/// Variant of [`sanitize_upstream_body`] that takes an explicit
/// character cap. Used by [`debug_upstream_error_body`] for the 4 KB
/// debug-level full-body log so a JSON validation error with field
/// detail isn't truncated by the 512-char excerpt cap.
pub fn sanitize_upstream_body_with_cap(body: &str, cap: usize) -> String {
    let trimmed = body.trim();
    let looks_like_html =
        trimmed.starts_with('<') || trimmed.to_ascii_lowercase().contains("<!doctype");
    if looks_like_html {
        return format!("<html error page, {} bytes>", body.len());
    }
    if trimmed.len() <= cap {
        return trimmed.to_string();
    }
    let mut s = trimmed.chars().take(cap).collect::<String>();
    s.push_str("... [truncated]");
    s
}

/// Cap on the full upstream error body emitted at `tracing::debug!`.
/// 4 KB is large enough to fit any field-level JSON validation error
/// Bedrock / Anthropic / OpenAI returns in practice, while still
/// bounded so a malicious or compromised upstream can't drive log
/// volume by returning megabyte-sized error pages.
pub(crate) const MAX_DEBUG_BODY_BYTES: usize = 4096;

/// Default cap on the serialized body emitted at TRACE level by all
/// four body trace helpers (`trace_ingress_body`,
/// `trace_outgoing_body`, `trace_upstream_success_body`,
/// `trace_egress_body`). 16 KB is generous for diagnosis without
/// flooding logs when a debug session gets left on by accident.
///
/// Operators capturing live-traffic fixtures need full bodies, not
/// truncated ones, because real claude-code requests routinely
/// exceed 16 KB (full conversation history + tool defs +
/// cache_control breakpoints). Set `ROUTECTL_TRACE_BODY_BYTES=<n>`
/// to override at process start; a 1 MB value (`1048576`) covers
/// almost all real bodies. See [`trace_body_cap`] for the resolution
/// rule. The const name is kept for downstream consumers that read
/// the default at compile time.
pub const MAX_TRACE_BODY_BYTES: usize = 16 * 1024;

/// Truncate a JSON value to its compact-stringified form, capped at `cap`
/// bytes with a `... [truncated at <cap> bytes]` tail. Used by the four
/// body-trace helpers.
///
/// `serde_json::to_string` preserves non-ASCII codepoints as multi-byte
/// UTF-8 (it does NOT escape them as `\uXXXX`), so a naive `&s[..cap]`
/// slice can land in the middle of a UTF-8 sequence and panic. This
/// helper walks back to the previous char boundary before slicing.
fn truncate_json_for_log(body: &serde_json::Value, cap: usize) -> String {
    let s = serde_json::to_string(body).unwrap_or_default();
    if s.len() > cap {
        let mut end = cap;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... [truncated at {cap} bytes]", &s[..end])
    } else {
        s
    }
}

/// Resolved trace body cap. Reads `ROUTECTL_TRACE_BODY_BYTES` once
/// from the env on first use and freezes the value via `OnceLock`.
/// Resolution order: env value (when set, numeric, and `> 0`); else
/// the [`init_log_overrides`]-seeded `[log]` config override (when
/// set and `> 0`); else [`MAX_TRACE_BODY_BYTES`] (16 KB).
///
/// Same setup caveat as [`redact_enabled`]: set the env var BEFORE
/// launching routectl. The resolved value is announced once at
/// startup via the (module-private) `log_trace_body_cap_status`,
/// invoked by [`init_log_overrides`] so operators can confirm the
/// override took effect.
pub fn trace_body_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("ROUTECTL_TRACE_BODY_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .or_else(|| OVERRIDE_TRACE_BODY_BYTES.get().copied().filter(|&n| n > 0))
            .unwrap_or(MAX_TRACE_BODY_BYTES)
    })
}

/// Read the cap env var and emit a single `info` line so operators
/// can confirm the resolved value at startup. Calling this also
/// seeds the `OnceLock` with the value present at process boot.
/// Mirror of `log_redaction_status`. Module-private: invoked from
/// [`init_log_overrides`] so the seed-then-status order is atomic.
fn log_trace_body_cap_status() {
    static EMITTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let cap = trace_body_cap();
    EMITTED.get_or_init(|| {
        tracing::info!(
            trace_body_cap = cap,
            default = MAX_TRACE_BODY_BYTES,
            "ROUTECTL_TRACE_BODY_BYTES resolved (frozen for the rest of this process)"
        );
    });
}

/// Parse a boolean-ish env var value: trim surrounding whitespace,
/// ASCII-lowercase, then match the accepted truthy spellings
/// (`1`/`true`/`yes`/`on`). Anything else (including empty) is false.
/// Shared by [`redact_enabled`] and [`header_trace_enabled`] so the two
/// toggles agree on spelling -- both are case-insensitive and both
/// accept `on`.
fn parse_bool_env(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether `ROUTECTL_LOG_REDACT_PROMPTS` is set to a truthy value.
///
/// Read once on the FIRST CALL that fires (i.e., the first time TRACE
/// is enabled and a `trace_*_body` helper actually runs). The
/// `OnceLock` then freezes the resolved value. Setting the env var
/// before the first traced body is sufficient; flipping it afterward
/// has no effect.
///
/// When the env var is unset, the resolution falls through to the
/// [`init_log_overrides`]-seeded `[log]` config fallback before
/// landing on the hardcoded default (false).
///
/// Practical implication: operators MUST set
/// `ROUTECTL_LOG_REDACT_PROMPTS=1` BEFORE launching routectl (or
/// before flipping TRACE for the first time). The cached-false case
/// (env var set after the first call) silently disables redaction
/// even though the operator believes they enabled it. There is a
/// matching `info`-level startup line in (module-private)
/// `log_redaction_status`, fired by [`init_log_overrides`], so
/// operators can confirm the resolved value once at server boot.
fn redact_enabled() -> bool {
    static REDACT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REDACT.get_or_init(|| {
        std::env::var("ROUTECTL_LOG_REDACT_PROMPTS")
            .ok()
            .map(|v| parse_bool_env(&v))
            .or_else(|| OVERRIDE_REDACT_PROMPTS.get().copied())
            .unwrap_or(false)
    })
}

/// Whether `ROUTECTL_TRACE_HEADERS` is set to a truthy value
/// (`1`/`true`/`yes`, case-insensitive, trimmed; default false).
///
/// Read once on the FIRST CALL via `OnceLock` and frozen for the rest
/// of the process. When env is unset, falls through to the
/// [`init_log_overrides`]-seeded `[log]` config fallback before
/// landing on the hardcoded default (false). Same setup caveat as
/// [`redact_enabled`]: set the env var BEFORE launching routectl;
/// flipping it afterward has no effect. Default false makes the four
/// `trace_*_headers` helpers a no-op unless the operator opts in --
/// header lines carry auth and other verbatim values and must not
/// flow into logs by accident. The matching startup `info` line is
/// emitted by the (module-private) `log_header_trace_status`, fired
/// by [`init_log_overrides`].
pub fn header_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ROUTECTL_TRACE_HEADERS")
            .ok()
            .map(|v| parse_bool_env(&v))
            .or_else(|| OVERRIDE_TRACE_HEADERS.get().copied())
            .unwrap_or(false)
    })
}

/// Read the redaction env var and emit a single `info` line so
/// operators can confirm the resolved value at startup. Calling this
/// also seeds the `OnceLock` with the value present at process boot,
/// which is the value the rest of the run will see -- making the
/// "set the env var before launching" requirement observable. Safe
/// to call multiple times: the underlying value is frozen by
/// [`redact_enabled`] and the `info` line emits at most once per
/// process. Module-private: invoked from [`init_log_overrides`] so
/// the seed-then-status order is atomic.
fn log_redaction_status() {
    static EMITTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let enabled = redact_enabled();
    EMITTED.get_or_init(|| {
        tracing::info!(
            redact_prompts = enabled,
            "ROUTECTL_LOG_REDACT_PROMPTS resolved (frozen for the rest of this process)"
        );
    });
}

/// Read the header-trace env var and emit a single `info` line so
/// operators can confirm the resolved value at startup. Seeds the
/// `OnceLock` with the boot-time value, making the "set the env var
/// before launching" requirement observable. Mirror of
/// `log_redaction_status` and `log_trace_body_cap_status`.
/// Module-private: invoked from [`init_log_overrides`] so the
/// seed-then-status order is atomic.
fn log_header_trace_status() {
    static EMITTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let enabled = header_trace_enabled();
    EMITTED.get_or_init(|| {
        tracing::info!(
            trace_headers = enabled,
            "ROUTECTL_TRACE_HEADERS resolved (frozen for the rest of this process)"
        );
    });
}

/// Public facade: when `ROUTECTL_LOG_REDACT_PROMPTS=1` is set in the
/// environment, walks `body` and replaces known user-content fields
/// with `<redacted len=N>` placeholders while preserving structural
/// fields (model, tools, sampling params, finish_reason, usage).
/// When the env var is unset, returns a clone unchanged.
///
/// Best-effort redaction: covers the wire shapes used by OpenAI Chat
/// Completions, Anthropic Messages, and OpenAI Responses (request
/// and response bodies on each). An unknown wire field carrying user
/// data could still leak. Document the trade-off when surfacing this
/// knob to operators.
pub fn redact_prompts_in(body: &serde_json::Value) -> serde_json::Value {
    redact_prompts_with_flag(body, redact_enabled())
}

/// Test-friendly variant of [`redact_prompts_in`] that takes the flag
/// explicitly, sidestepping the process-global `OnceLock` so unit
/// tests can pin both branches deterministically.
pub(crate) fn redact_prompts_with_flag(
    body: &serde_json::Value,
    enabled: bool,
) -> serde_json::Value {
    if !enabled {
        return body.clone();
    }
    let mut v = body.clone();
    redact_value(&mut v);
    v
}

/// Recursive in-place redaction. Key-aware: only known user-content
/// keys are replaced; structural keys (model, tools, finish_reason,
/// usage, role, ...) recurse unchanged. Object replacements
/// (`tool_use.input` for Anthropic shape, `toolUse.input` for Bedrock
/// Converse shape) are handled before the per-key sweep.
fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Array(arr) => {
            for elem in arr {
                redact_value(elem);
            }
        }
        serde_json::Value::Object(map) => {
            // Whole-object replacements first.
            //
            // Anthropic-shape tool_use parts carry user-supplied tool
            // inputs; collapse the entire `input` object to an opaque
            // marker rather than walking it.
            if map.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(input) = map.get_mut("input") {
                    *input = redacted_object();
                }
            }
            // Bedrock Converse uses a different wire shape -- the tool
            // call is `{"toolUse": {"toolUseId":..., "name":...,
            // "input": <Value>}}` with no `type` key on the parent.
            // Reach into the toolUse sub-object and redact `input`
            // there. Same for `toolResult.content[*].json` (a Value
            // returned from a tool that may carry user-derived data).
            if let Some(tool_use) = map.get_mut("toolUse") {
                if let Some(obj) = tool_use.as_object_mut() {
                    if let Some(input) = obj.get_mut("input") {
                        *input = redacted_object();
                    }
                }
            }
            if let Some(tool_result) = map.get_mut("toolResult") {
                if let Some(obj) = tool_result.as_object_mut() {
                    if let Some(content) = obj.get_mut("content") {
                        if let Some(arr) = content.as_array_mut() {
                            for part in arr {
                                if let Some(part_obj) = part.as_object_mut() {
                                    // `json` is opaque structured data
                                    // returned by a tool; collapse it
                                    // wholesale rather than walking it
                                    // (no known content-bearing leaf
                                    // shape beneath this key). The
                                    // sibling `text` key is handled by
                                    // the generic per-key sweep below
                                    // when recursion reaches the part
                                    // object via the `_` arm.
                                    if let Some(json_val) = part_obj.get_mut("json") {
                                        *json_val = redacted_object();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Bedrock Converse `reasoningContent` block: the
            // `reasoningText.text` leaf is caught by the generic
            // `text`-key sweep on recursion. But Converse also carries
            // a `redactedContent` variant whose value is an opaque
            // safety-redacted-by-AWS payload (encrypted bytes, often
            // base64). It is NOT user-supplied prompt content per se,
            // but it IS derived from the prompt and must not flow
            // verbatim into a routectl trace log. Replace wholesale.
            if let Some(reasoning) = map.get_mut("reasoningContent") {
                if let Some(obj) = reasoning.as_object_mut() {
                    if let Some(rc) = obj.get_mut("redactedContent") {
                        *rc = redacted_object();
                    }
                }
            }
            // Bedrock Converse `promptVariables`: a bag of named
            // template variables substituted into a prompt template.
            // Values are user-derived strings/objects. Defensive
            // coverage -- the field isn't currently emitted by
            // routectl's Converse adapter, but operators can pass it
            // through `additionalModelRequestFields`. Replace the
            // whole bag with an opaque marker so a future surface
            // doesn't leak.
            if let Some(pv) = map.get_mut("promptVariables") {
                *pv = redacted_object();
            }

            // Per-key sweep. Known user-content keys are redacted at
            // the leaf; everything else recurses.
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let entry = map.get_mut(&k).expect("key from map iteration");
                match k.as_str() {
                    // Always-redact text leaves across all wire shapes.
                    // Covers `{type:"text", text}`, `{type:"input_text",
                    // text}`, `{type:"output_text", text}`, Anthropic
                    // thinking text, OpenAI Responses `summary[*].text`
                    // (reasoning summary), function_call `arguments`,
                    // and the OpenAI Responses `refusal` block where
                    // the safety reason echoes prompt-derived content.
                    "text" | "thinking" | "instructions" | "reasoning" | "arguments"
                    | "refusal" => {
                        redact_string_or_recurse(entry);
                    }
                    // `system` and `content` can be string OR array of
                    // parts. Redact at leaf when string; recurse when
                    // structured (covers Anthropic system blocks +
                    // OpenAI Chat Completions message-content arrays).
                    //
                    // `output` covers the OpenAI Responses
                    // `function_call_output.output` field on outgoing
                    // request items: it's either a flat tool-result
                    // string (leaf -- without this arm, the generic
                    // `_` fell into `redact_value` which is a no-op
                    // on Strings, and the tool-result content leaked
                    // verbatim into the trace log) or an array of
                    // typed parts whose inner `text` leaves are
                    // caught by the `text` arm on recursion. Also
                    // covers the Responses RESPONSE body's top-level
                    // `output: [...]` array and Bedrock Converse
                    // `output: {message: ...}` object -- both
                    // structured shapes recurse cleanly.
                    "system" | "content" | "output" => {
                        redact_string_or_recurse(entry);
                    }
                    // Image / document source data (base64). Only
                    // redact long strings to avoid eating short MIME
                    // type values; the 256-byte threshold (s.len(), not
                    // chars) is well below any real image payload but
                    // well above any MIME string. Base64 is ASCII so
                    // bytes == chars in practice.
                    "data" => {
                        if let serde_json::Value::String(s) = entry {
                            if s.len() > 256 {
                                let n = s.chars().count();
                                *entry = serde_json::Value::String(format!("<redacted len={n}>"));
                            }
                        } else {
                            redact_value(entry);
                        }
                    }
                    // OpenAI Chat Completions / Responses
                    // `image_url.url` carries either an `https://...`
                    // reference (safe) or a `data:image/<mime>;base64,
                    // <data>` URI (sensitive). Redact only the data
                    // URI form so plain URLs flow through unchanged.
                    "url" => {
                        if let serde_json::Value::String(s) = entry {
                            if s.starts_with("data:") && s.len() > 256 {
                                let n = s.chars().count();
                                *entry = serde_json::Value::String(format!("<redacted len={n}>"));
                            }
                        } else {
                            redact_value(entry);
                        }
                    }
                    _ => redact_value(entry),
                }
            }
        }
        _ => {}
    }
}

/// Replace a string leaf with `<redacted len=N>` (chars count); recurse
/// otherwise. Used by the keys whose value is "string OR structured":
/// `system`, `content`, `text`, `thinking`, etc.
fn redact_string_or_recurse(entry: &mut serde_json::Value) {
    if let serde_json::Value::String(s) = entry {
        let n = s.chars().count();
        *entry = serde_json::Value::String(format!("<redacted len={n}>"));
    } else {
        redact_value(entry);
    }
}

/// Opaque placeholder for whole-object replacements (tool_use input,
/// Converse toolUse input, toolResult json content).
fn redacted_object() -> serde_json::Value {
    serde_json::json!({"redacted": true})
}

/// Emit a `tracing::trace!` line carrying the ingress request body
/// (direction 1: client -> routectl). Honors
/// `ROUTECTL_LOG_REDACT_PROMPTS=1`. Inherits the parent span's
/// `request_id` so a `grep request_id=<id>` correlates ingress ->
/// outgoing -> upstream -> egress in one pass.
pub fn trace_ingress_body(ingress: &str, body: &serde_json::Value) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let safe = redact_prompts_in(body);
    let truncated = truncate_json_for_log(&safe, trace_body_cap());
    tracing::trace!(
        ingress,
        body = %truncated,
        "ingress request body"
    );
}

/// Emit a `tracing::trace!` line carrying the outgoing request body
/// for a given provider. Inherits the parent span's `request_id` so a
/// `grep request_id=<id>` correlates ingress -> outgoing -> upstream
/// response in a single pass.
///
/// Gated by `tracing::Level::TRACE` so production with the default
/// `info` level pays nothing. Operators flip to `trace` only during
/// active triage; CLAUDE.md "Triage recipes" documents the workflow
/// and the sensitivity caveat (bodies contain user prompts). Honors
/// `ROUTECTL_LOG_REDACT_PROMPTS=1`.
pub fn trace_outgoing_body(provider_kind: &str, provider_id: &str, body: &serde_json::Value) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let safe = redact_prompts_in(body);
    let truncated = truncate_json_for_log(&safe, trace_body_cap());
    tracing::trace!(
        provider_kind,
        provider = provider_id,
        body = %truncated,
        "outgoing request body"
    );
}

/// Emit a `tracing::trace!` line carrying the upstream's deserialized
/// 2xx success body (direction 3 success path). Fires AFTER the body
/// parses but BEFORE routectl normalizes it, so operators see the wire
/// shape the upstream actually returned -- not routectl's post-processed
/// form. Honors `ROUTECTL_LOG_REDACT_PROMPTS=1`. 4xx/5xx error bodies
/// are covered by [`debug_upstream_error_body`].
pub fn trace_upstream_success_body(
    provider_kind: &str,
    provider_id: &str,
    body: &serde_json::Value,
) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let safe = redact_prompts_in(body);
    let truncated = truncate_json_for_log(&safe, trace_body_cap());
    tracing::trace!(
        provider_kind,
        provider = provider_id,
        body = %truncated,
        "upstream success body"
    );
}

/// Emit a `tracing::trace!` line carrying the egress response body
/// (direction 4: routectl -> client). Fires after the canonical
/// `ChatResponse` is serialized to wire JSON, so the trace shows what
/// the client actually receives. Honors `ROUTECTL_LOG_REDACT_PROMPTS=1`.
pub fn trace_egress_body(ingress: &str, body: &serde_json::Value) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let safe = redact_prompts_in(body);
    let truncated = truncate_json_for_log(&safe, trace_body_cap());
    tracing::trace!(
        ingress,
        body = %truncated,
        "egress response body"
    );
}

// ---------------------------------------------------------------------
// Header tracing (4 directions). Opt-in via ROUTECTL_TRACE_HEADERS,
// gated on TRACE, RAW (no redaction) so fixture captures see the real
// auth / beta / version headers.
//
// PARSING CONTRACT (scripts/capture_fixtures.sh::extract_headers reads
// these lines back into per-request fixtures):
//   1. The four canonical message strings are the `HDR_MSG_*` consts
//      below. extract_headers greps the log for these EXACT strings;
//      renaming one here means updating the script's needles to match.
//   2. On every emitted line, `headers` MUST be the LAST structured
//      field (its JSON value runs to end-of-line). The script takes
//      everything after the first `headers=` to the line end. Keep the
//      `message = HDR_MSG_*` field FIRST and `headers = ...` LAST in
//      each trace! call so this holds regardless of subscriber field
//      ordering.
//   3. Values are emitted verbatim and the JSON is a single compact
//      string (serde_json escapes \n / \r), so a header value cannot
//      forge a second log line.
// ---------------------------------------------------------------------

/// Canonical dir-1 (client -> routectl) header-trace message. Part of
/// the parsing contract documented above and mirrored by
/// scripts/capture_fixtures.sh::extract_headers.
pub const HDR_MSG_INGRESS: &str = "ingress request headers";
/// Canonical dir-2 (routectl -> upstream) header-trace message. See the
/// header-trace section contract above.
pub const HDR_MSG_OUTGOING: &str = "outgoing request headers";
/// Canonical dir-3 (upstream -> routectl) header-trace message. See the
/// header-trace section contract above.
pub const HDR_MSG_UPSTREAM: &str = "upstream response headers";
/// Canonical dir-4 (routectl -> client) header-trace message. See the
/// header-trace section contract above.
pub const HDR_MSG_EGRESS: &str = "egress response headers";

/// Pure gate decision shared by the four `trace_*_headers` emitters:
/// emit only when header tracing is opted in AND the subscriber has
/// TRACE enabled. Extracted as a pure fn (no env reads, no global
/// state) so both arms are unit-testable without touching the
/// process-frozen [`header_trace_enabled`] `OnceLock` or installing a
/// shared tracing subscriber.
fn header_trace_should_emit(header_trace_on: bool, trace_level_on: bool) -> bool {
    header_trace_on && trace_level_on
}

/// Build a JSON ARRAY of `[name, value]` two-element arrays from a
/// sequence of header pairs. An array (not an object) so iteration
/// ORDER and DUPLICATE names (`set-cookie`, repeated `via`, ...)
/// survive the round-trip; a JSON object would silently collapse
/// duplicates and reorder keys. Values decode with
/// `String::from_utf8_lossy` so a non-UTF-8 byte (rare but legal on
/// the wire) becomes the replacement char rather than dropping the
/// header.
///
/// Call sites pass plain `&str` / `&[u8]` (e.g.
/// `map.iter().map(|(k, v)| (k.as_str(), v.as_bytes()))`), which keeps
/// core decoupled from the axum / reqwest `http` crate version --
/// only the standard-library types cross the boundary.
pub fn headers_to_json<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> serde_json::Value {
    serde_json::Value::Array(
        pairs
            .into_iter()
            .map(|(name, value)| {
                serde_json::Value::Array(vec![
                    serde_json::Value::String(name.to_string()),
                    serde_json::Value::String(String::from_utf8_lossy(value).into_owned()),
                ])
            })
            .collect(),
    )
}

/// Header names whose VALUES carry a bearer secret on the outgoing
/// (routectl -> upstream) direction and MUST be redacted before any
/// log emission. Compared case-insensitively.
///
/// `authorization`            -- `Bearer <jwt>` for OAuth-managed
///                                providers (codex / chatgpt-oauth,
///                                anthropic-oauth) and api-key bearer.
/// `x-api-key`                -- Anthropic-API key.
/// `proxy-authorization`      -- mirror of `authorization` for
///                                proxy-tunneled requests.
const REDACT_HEADER_NAMES: &[&str] = &["authorization", "x-api-key", "proxy-authorization"];

/// True if the given header name carries a secret value that the
/// outgoing-direction header trace MUST redact. Case-insensitive --
/// `Authorization`, `AUTHORIZATION`, and `authorization` all match.
fn is_redact_header(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    REDACT_HEADER_NAMES.contains(&lc.as_str())
}

/// Replacement value for `Bearer <token>` Authorization headers. Keeps
/// the scheme prefix so an operator reading the trace can confirm the
/// scheme was set without exposing the token.
const REDACTED_BEARER: &str = "Bearer [REDACTED]";

/// Replacement value for any other secret-carrying header (raw api key,
/// non-Bearer `Authorization`, etc.).
const REDACTED_SECRET: &str = "[REDACTED]";

/// Redact a single secret-carrying header value. Bearer-scheme
/// `authorization` values keep the literal `"Bearer "` prefix so the
/// scheme remains visible in traces; everything else collapses to a
/// bare `[REDACTED]`. Pure fn -- no allocation when the input would
/// echo back unchanged is NOT a goal; this only runs on the redaction
/// path of the outgoing-headers trace, which is itself opt-in via
/// `ROUTECTL_TRACE_HEADERS`.
fn redact_header_value(value: &str) -> String {
    // Match `Bearer <something>` case-insensitively on the scheme. We
    // do NOT anchor on the trailing token because some upstreams
    // accept `Bearer ` followed by either a JWT or an opaque key, and
    // we want both shapes to redact identically.
    let trimmed = value.trim_start();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("Bearer ") {
        return REDACTED_BEARER.to_string();
    }
    REDACTED_SECRET.to_string()
}

/// Walk a `[[name, value], ...]` JSON array (the shape produced by
/// [`headers_to_json`]) and replace the `value` half of every pair
/// whose `name` matches [`REDACT_HEADER_NAMES`] (case-insensitive).
/// Mutates in place. Other entries (and any non-pair shape that slipped
/// in) are left untouched.
pub(crate) fn redact_outgoing_header_values(headers: &mut serde_json::Value) {
    let Some(arr) = headers.as_array_mut() else {
        return;
    };
    for entry in arr.iter_mut() {
        let Some(pair) = entry.as_array_mut() else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let name_is_secret = pair[0].as_str().is_some_and(is_redact_header);
        if !name_is_secret {
            continue;
        }
        let redacted = match pair[1].as_str() {
            Some(v) => redact_header_value(v),
            None => REDACTED_SECRET.to_string(),
        };
        pair[1] = serde_json::Value::String(redacted);
    }
}

/// Emit a `tracing::trace!` line carrying the ingress request headers
/// (direction 1: client -> routectl). Opt-in via
/// `ROUTECTL_TRACE_HEADERS=1` and gated on TRACE so the default `info`
/// level pays nothing. RAW -- names and values are emitted verbatim
/// (no redaction) so fixture captures see the real auth / beta /
/// version headers. The JSON is emitted as a single compact string
/// (`serde_json` escapes `\n`/`\r`, so the value stays single-line and
/// cannot forge log lines) and is the LAST field on the line.
pub fn trace_ingress_headers(ingress: &str, headers: &serde_json::Value) {
    if !header_trace_should_emit(
        header_trace_enabled(),
        tracing::event_enabled!(tracing::Level::TRACE),
    ) {
        return;
    }
    tracing::trace!(
        message = HDR_MSG_INGRESS,
        ingress,
        headers = %serde_json::to_string(headers).unwrap_or_default(),
    );
}

/// Emit a `tracing::trace!` line carrying the outgoing request headers
/// for a given provider (direction 2: routectl -> upstream). Opt-in via
/// `ROUTECTL_TRACE_HEADERS=1`; gated on TRACE. Bearer JWTs and api keys
/// in `authorization` / `x-api-key` / `proxy-authorization` are
/// redacted before emission via [`redact_outgoing_header_values`] so
/// `journalctl` / log archives never carry a live access token. Other
/// headers (anthropic-beta, anthropic-version, originator, ...) emit
/// verbatim since they are not secrets and the fixture-capture
/// pipeline depends on the round-trip. `headers` is the LAST field on
/// the line.
pub fn trace_outgoing_headers(provider_kind: &str, id: &str, headers: &serde_json::Value) {
    if !header_trace_should_emit(
        header_trace_enabled(),
        tracing::event_enabled!(tracing::Level::TRACE),
    ) {
        return;
    }
    let mut redacted = headers.clone();
    redact_outgoing_header_values(&mut redacted);
    tracing::trace!(
        message = HDR_MSG_OUTGOING,
        provider_kind,
        provider = id,
        headers = %serde_json::to_string(&redacted).unwrap_or_default(),
    );
}

/// Emit a `tracing::trace!` line carrying the upstream response headers
/// (direction 3: upstream -> routectl). Opt-in via
/// `ROUTECTL_TRACE_HEADERS=1`; gated on TRACE. See
/// [`trace_ingress_headers`] for the single-line / no-redaction
/// rationale. `headers` is the LAST field on the line.
pub fn trace_upstream_response_headers(provider_kind: &str, id: &str, headers: &serde_json::Value) {
    if !header_trace_should_emit(
        header_trace_enabled(),
        tracing::event_enabled!(tracing::Level::TRACE),
    ) {
        return;
    }
    tracing::trace!(
        message = HDR_MSG_UPSTREAM,
        provider_kind,
        provider = id,
        headers = %serde_json::to_string(headers).unwrap_or_default(),
    );
}

/// Emit a `tracing::trace!` line carrying the egress response headers
/// (direction 4: routectl -> client). Opt-in via
/// `ROUTECTL_TRACE_HEADERS=1`; gated on TRACE. See
/// [`trace_ingress_headers`] for the single-line / no-redaction
/// rationale. `headers` is the LAST field on the line.
pub fn trace_egress_headers(ingress: &str, headers: &serde_json::Value) {
    if !header_trace_should_emit(
        header_trace_enabled(),
        tracing::event_enabled!(tracing::Level::TRACE),
    ) {
        return;
    }
    tracing::trace!(
        message = HDR_MSG_EGRESS,
        ingress,
        headers = %serde_json::to_string(headers).unwrap_or_default(),
    );
}

/// Stable subset of request-body fields that the operator's
/// structural validator (smart heartbeat) needs to confirm wire-shape
/// invariants WITHOUT trying to grep through a truncated 16 KB body
/// blob.
///
/// Field-name stability contract: the field names emitted by
/// [`trace_structural_summary`] are operator-facing API. Adding a new
/// field is allowed without a major bump; renaming or removing an
/// existing field is a breaking change and requires a CLAUDE.md entry
/// under "Triage trace-level surfaces". Treat as a v-minor bump.
///
/// All fields are prompt-content-free by design. Counts are emitted in
/// place of arrays; opaque shape discriminators are emitted in place
/// of arbitrary string values (`thinking_shape`, `tool_choice_shape`).
/// The exception is `anthropic_beta` which is a small list of
/// operator-greppable enum-like flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuralSummary {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    /// One of `"enabled:<budget>"`, `"adaptive:<effort>"`, or None.
    /// The raw budget integer and effort string are encoded into the
    /// discriminator so the field stays a single string-typed value.
    pub thinking_shape: Option<String>,
    /// `output_config.effort` verbatim (when present).
    pub output_config_effort: Option<String>,
    /// One of `"auto"`, `"required"`, `"none"`, `"function:<name>"`,
    /// `"object:<discriminator>"`, or None.
    pub tool_choice_shape: Option<String>,
    /// Walk count of `cache_control` keys anywhere in the body
    /// (system + messages + tools + top-level + any future
    /// forward-compat extras that carry one). The recursive walk
    /// matches the canonical's actual cache-control surface: the
    /// auto-cache top-level breakpoint counts alongside the
    /// per-block breakpoints.
    pub cache_control_count: u32,
    /// `messages.len()` (or `input.len()` for Responses-shape bodies).
    pub messages_len: u32,
    /// `tools.len()`.
    pub tools_len: u32,
    /// The `anthropic_beta` array verbatim if non-empty. Small +
    /// operator-greppable enum-like flag set.
    pub anthropic_beta: Vec<String>,
    /// Sorted top-level keys of `provider_extras` if present (for
    /// forward-compat sweep visibility).
    pub provider_extras_keys: Vec<String>,
    pub stream: Option<bool>,
}

/// Walk `body` (a request-side JSON value) and extract a stable set of
/// structural fields for diagnosis. Pure function, no side effects --
/// the TRACE emit lives in [`trace_structural_summary`]. Designed so
/// unit tests can pin every field without touching a tracing harness.
///
/// Tolerates missing keys (returns the type's default). Tolerates
/// type-mismatch (e.g. `model: 5` rather than a string) by returning
/// None for that field.
pub(crate) fn extract_structural_summary(body: &serde_json::Value) -> StructuralSummary {
    let obj = match body.as_object() {
        Some(o) => o,
        None => return StructuralSummary::default(),
    };

    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let max_tokens = obj
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let stream = obj.get("stream").and_then(|v| v.as_bool());

    let thinking_shape = obj.get("thinking").and_then(|t| t.as_object()).map(|t| {
        match t.get("type").and_then(|v| v.as_str()) {
            Some("enabled") => {
                let budget = t
                    .get("budget_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string());
                format!("enabled:{budget}")
            }
            Some("adaptive") => {
                // Adaptive thinking pairs with `output_config.effort`;
                // the discriminator pulls that effort string into the
                // shape so the structural value is self-contained.
                let effort = obj
                    .get("output_config")
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get("effort"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                format!("adaptive:{effort}")
            }
            Some(other) => other.to_string(),
            None => "absent".to_string(),
        }
    });

    let output_config_effort = obj
        .get("output_config")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("effort"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let tool_choice_shape = obj.get("tool_choice").map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => {
            // OpenAI nested shape: {type:"function", function:{name:"X"}}
            // Anthropic / OpenAI Responses flat shape: {type:"function", name:"X"}
            let kind = o.get("type").and_then(|v| v.as_str());
            let nested_name = o
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|fo| fo.get("name"))
                .and_then(|n| n.as_str());
            let flat_name = o.get("name").and_then(|v| v.as_str());
            let name = nested_name.or(flat_name);
            match (kind, name) {
                (Some("function"), Some(n)) => format!("function:{n}"),
                (Some(t), _) => format!("object:{t}"),
                (None, _) => "object:?".to_string(),
            }
        }
        _ => "other".to_string(),
    });

    let cache_control_count = count_cache_control(body);

    // Messages-shape bodies (Anthropic + openai-compat ingress) use
    // `messages`; Responses-shape bodies use `input`. Either field
    // contributes to `messages_len`; assume only one is present.
    let messages_len = obj
        .get("messages")
        .or_else(|| obj.get("input"))
        .and_then(|v| v.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);

    let tools_len = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);

    let anthropic_beta = obj
        .get("anthropic_beta")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut provider_extras_keys: Vec<String> = obj
        .get("provider_extras")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    provider_extras_keys.sort();

    StructuralSummary {
        model,
        max_tokens,
        thinking_shape,
        output_config_effort,
        tool_choice_shape,
        cache_control_count,
        messages_len,
        tools_len,
        anthropic_beta,
        provider_extras_keys,
        stream,
    }
}

/// Recursive walk over `v` counting every object map that carries a
/// `cache_control` key. Counts the key itself (one per object that has
/// it) rather than the keys-on-leaves it might decompose into.
fn count_cache_control(v: &serde_json::Value) -> u32 {
    let mut count: u32 = 0;
    walk_cache_control(v, &mut count);
    count
}

fn walk_cache_control(v: &serde_json::Value, count: &mut u32) {
    match v {
        serde_json::Value::Object(map) => {
            if map.contains_key("cache_control") {
                *count += 1;
            }
            for (_, child) in map {
                walk_cache_control(child, count);
            }
        }
        serde_json::Value::Array(arr) => {
            for elem in arr {
                walk_cache_control(elem, count);
            }
        }
        _ => {}
    }
}

/// Emit a single TRACE line summarizing the structural shape of a
/// REQUEST-side body, so the operator's smart-heartbeat validator can
/// grep stable field names without fighting the 16 KB body cap.
///
/// `direction` is `"ingress"` (direction 1: client -> routectl) or
/// `"outgoing"` (direction 2: routectl -> upstream). `kind` and `id`
/// reuse the body-trace helpers' semantics:
///   - For direction 1 (`"ingress"`), pass `kind = "ingress"` and
///     `id` = the ingress name (`"openai"` / `"anthropic"`).
///   - For direction 2 (`"outgoing"`), pass `kind` = the provider-kind
///     literal (`"openai-compat"` etc.) and `id` = the provider id.
///
/// Scope: REQUEST bodies only. Response bodies have different
/// structure and the operator's validator only consumes request-side
/// summaries. Skip directions 3 and 4 deliberately.
///
/// Field-name stability: see [`StructuralSummary`]. Renaming or
/// removing fields requires a CLAUDE.md note; adding new fields is
/// allowed without ceremony.
pub fn trace_structural_summary(direction: &str, kind: &str, id: &str, body: &serde_json::Value) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let s = extract_structural_summary(body);
    // Every client-controlled string passes through `sanitize_for_log`
    // before reaching tracing. `model`, the `effort` substring of
    // `thinking_shape`, `output_config_effort`, the function-name
    // substring of `tool_choice_shape`, and the entries of
    // `anthropic_beta` / `provider_extras_keys` are all controllable
    // by an authenticated client; a malicious payload with control
    // characters or oversize content would otherwise mangle TRACE
    // log lines the operator's smart-heartbeat validator depends on.
    let model = sanitize_for_log(s.model.as_deref().unwrap_or(""));
    let thinking_shape = sanitize_for_log(s.thinking_shape.as_deref().unwrap_or(""));
    let output_config_effort = sanitize_for_log(s.output_config_effort.as_deref().unwrap_or(""));
    let tool_choice_shape = sanitize_for_log(s.tool_choice_shape.as_deref().unwrap_or(""));
    let anthropic_beta = sanitize_for_log(&s.anthropic_beta.join(","));
    let provider_extras_keys = sanitize_for_log(&s.provider_extras_keys.join(","));
    tracing::trace!(
        direction,
        kind,
        id,
        model = %model,
        max_tokens = s.max_tokens.unwrap_or(0),
        thinking_shape = %thinking_shape,
        output_config_effort = %output_config_effort,
        tool_choice_shape = %tool_choice_shape,
        cache_control_count = s.cache_control_count,
        messages_len = s.messages_len,
        tools_len = s.tools_len,
        anthropic_beta = %anthropic_beta,
        provider_extras_keys = %provider_extras_keys,
        stream = s.stream.unwrap_or(false),
        "structural summary"
    );
}

/// Emit a single TRACE line summarizing a streaming response at
/// termination. NOT a per-chunk firehose: chunk_count + final
/// finish_reason + final usage are typically the only fields operators
/// need to triage a streaming session, and per-chunk dumps flood the
/// log without adding signal.
///
/// `direction` is the literal `"upstream"` (provider-side wrapper)
/// or `"egress"` (ingress-side after wire rendering). `kind` and `id`
/// reuse provider-kind/provider-id semantics; for the egress side,
/// pass `kind = "ingress"` and `id` = the ingress name (`"openai"` /
/// `"anthropic"`).
///
/// Field names are flat numeric (`chunks=`, `finish_reason=`,
/// `prompt_tokens=`, `completion_tokens=`, `total_tokens=`) so a
/// `grep body=` only hits actual body lines, never the summary.
pub fn trace_stream_summary(
    direction: &str,
    kind: &str,
    id: &str,
    chunk_count: u64,
    finish_reason: Option<&str>,
    usage: Option<&crate::Usage>,
) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let prompt_tokens = usage.map(|u| u.prompt_tokens).unwrap_or(0);
    let completion_tokens = usage.map(|u| u.completion_tokens).unwrap_or(0);
    let total_tokens = usage.map(|u| u.total_tokens).unwrap_or(0);
    tracing::trace!(
        direction,
        kind,
        id,
        chunks = chunk_count,
        finish_reason = finish_reason.unwrap_or("unknown"),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        "stream summary"
    );
}

/// Wrap a `Stream<Item = Result<ChatChunk>>` so that on termination
/// (clean exit OR consumer drop) a single [`trace_stream_summary`]
/// line fires. Provider-side stream impls call this once before
/// returning the BoxStream so operators get one summary per session
/// without per-provider boilerplate.
///
/// Each yielded `Ok(chunk)` is observed before forwarding: chunk count
/// increments; `last_finish_reason` updates from the most recent
/// non-None value across `chunk.choices`; usage tracks last-wins on
/// each `UsageDelta` field. Errors flow through unchanged.
///
/// Direction is `"upstream"` for provider wrappers; `"egress"` should
/// keep using the inline pattern in the ingress driver since the
/// ingress side wraps an mpsc::Receiver around SSE rendering rather
/// than a `Stream<ChatChunk>` directly.
pub fn wrap_stream_with_summary<S>(
    stream: S,
    direction: &'static str,
    kind: &'static str,
    id: String,
) -> futures::stream::BoxStream<'static, crate::Result<crate::ChatChunk>>
where
    S: futures::Stream<Item = crate::Result<crate::ChatChunk>> + Send + 'static,
{
    Box::pin(StreamWithSummary {
        inner: Box::pin(stream),
        chunks: 0,
        last_finish: None,
        last_prompt: 0,
        last_completion: 0,
        last_total: 0,
        direction,
        kind,
        id,
        emitted: false,
    })
}

struct StreamWithSummary {
    inner: futures::stream::BoxStream<'static, crate::Result<crate::ChatChunk>>,
    chunks: u64,
    last_finish: Option<String>,
    last_prompt: u32,
    last_completion: u32,
    last_total: u32,
    direction: &'static str,
    kind: &'static str,
    id: String,
    emitted: bool,
}

impl StreamWithSummary {
    fn emit_summary(&mut self) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        let usage = (self.last_prompt != 0 || self.last_completion != 0 || self.last_total != 0)
            .then_some(crate::Usage {
                prompt_tokens: self.last_prompt,
                completion_tokens: self.last_completion,
                total_tokens: self.last_total,
                ..Default::default()
            });
        trace_stream_summary(
            self.direction,
            self.kind,
            &self.id,
            self.chunks,
            self.last_finish.as_deref(),
            usage.as_ref(),
        );
    }
}

impl futures::Stream for StreamWithSummary {
    type Item = crate::Result<crate::ChatChunk>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // BoxStream is Pin<Box<...>> which is Unpin, so the rest of
        // the fields can be accessed safely via Pin::get_mut.
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(item)) => {
                if let Ok(chunk) = &item {
                    this.chunks += 1;
                    // Last-wins on the most recent choice carrying a
                    // finish_reason; some providers emit empty choices
                    // before the terminal one.
                    for choice in chunk.choices.iter().rev() {
                        if let Some(fr) = &choice.finish_reason {
                            this.last_finish = Some(fr.clone());
                            break;
                        }
                    }
                    if let Some(u) = &chunk.usage {
                        if let Some(p) = u.prompt_tokens {
                            this.last_prompt = p;
                        }
                        if let Some(c) = u.completion_tokens {
                            this.last_completion = c;
                        }
                        if let Some(t) = u.total_tokens {
                            this.last_total = t;
                        }
                    }
                }
                std::task::Poll::Ready(Some(item))
            }
            std::task::Poll::Ready(None) => {
                this.emit_summary();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl Drop for StreamWithSummary {
    fn drop(&mut self) {
        // Consumer dropped the stream early (cancellation, axum
        // disconnect). Emit the partial summary so operators can see
        // where the cancellation landed.
        self.emit_summary();
    }
}

/// Emit a `tracing::debug!` line carrying the full upstream error
/// body on a 4xx/5xx response. The provider's existing WARN with
/// `body_excerpt` (200-512 chars) stays at WARN so
/// `routectl-warn.log` remains scannable; this DEBUG line gives
/// operators the full picture (capped at 4 KB) when they flip log
/// level during triage. Inherits parent span for `request_id`
/// correlation.
///
/// HTML pages from misconfigured proxies / CDN error pages are
/// collapsed via [`sanitize_upstream_body_with_cap`] so the log
/// doesn't fill with markup.
pub fn debug_upstream_error_body(provider_kind: &str, provider_id: &str, status: u16, body: &str) {
    if !tracing::event_enabled!(tracing::Level::DEBUG) {
        return;
    }
    let cleaned = sanitize_upstream_body_with_cap(body, MAX_DEBUG_BODY_BYTES);
    // Strip control chars (CR, LF, ANSI escapes) that sanitize_upstream_body_with_cap
    // does NOT remove -- it only HTML-collapses + length-caps. Without this step a
    // malicious/compromised upstream can forge fake log lines up to 4 KB on any
    // text-format subscriber when the operator runs at DEBUG during triage.
    let cleaned = sanitize_capped(&cleaned, MAX_DEBUG_BODY_BYTES);
    tracing::debug!(
        provider_kind,
        provider = provider_id,
        status,
        body = %cleaned,
        "upstream error body"
    );
}

// Config-side fallback seeds for the three `[log]` knobs. None until
// `init_log_overrides` populates them; consulted by the matching
// reader's OnceLock-init closure in the env-unset branch (env wins,
// then override, then hardcoded default).
static OVERRIDE_TRACE_HEADERS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static OVERRIDE_TRACE_BODY_BYTES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
static OVERRIDE_REDACT_PROMPTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Single entrypoint: seed the config-side fallback for each
/// `Some(_)` knob, then fire the three status emitters so every
/// reader's OnceLock freezes on the resolved value AND the
/// confirmation `info` line lands at startup. `None` skips the seed
/// for that knob -- "no config-side fallback; env or hardcoded
/// default wins" -- but the matching status line still emits, so an
/// operator always sees one `info` line per knob at boot.
///
/// Atomic seam: callers do not separately invoke the (now
/// module-private) `log_*_status` helpers; calling this once per
/// process is sufficient. Idempotent per OnceLock -- a second call
/// with a different `Some(_)` value emits a single `debug!` line
/// per knob (the value seeded at first call wins).
///
/// Resolution rule per knob: env wins when set; otherwise the seeded
/// override; otherwise the hardcoded default (false / 16 KB / false).
pub fn init_log_overrides(
    trace_headers: Option<bool>,
    trace_body_bytes: Option<usize>,
    redact_prompts: Option<bool>,
) {
    if let Some(v) = trace_headers {
        if let Err(existing) = OVERRIDE_TRACE_HEADERS.set(v) {
            tracing::debug!(
                knob = "trace_headers",
                existing = existing,
                new = v,
                "init_log_overrides: knob already frozen; ignoring new value"
            );
        }
    }
    if let Some(v) = trace_body_bytes {
        if let Err(existing) = OVERRIDE_TRACE_BODY_BYTES.set(v) {
            tracing::debug!(
                knob = "trace_body_bytes",
                existing = existing,
                new = v,
                "init_log_overrides: knob already frozen; ignoring new value"
            );
        }
    }
    if let Some(v) = redact_prompts {
        if let Err(existing) = OVERRIDE_REDACT_PROMPTS.set(v) {
            tracing::debug!(
                knob = "redact_prompts",
                existing = existing,
                new = v,
                "init_log_overrides: knob already frozen; ignoring new value"
            );
        }
    }
    // After seeding: emit the three status lines. Each call freezes
    // the matching reader's OnceLock to env-or-override-or-default
    // and emits the operator-facing `info` confirmation exactly once
    // per process.
    log_redaction_status();
    log_trace_body_cap_status();
    log_header_trace_status();
}

#[cfg(test)]
#[path = "log_safe_tests.rs"]
mod tests;
