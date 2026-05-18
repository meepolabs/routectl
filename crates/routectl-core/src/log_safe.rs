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

/// Sanitize a client-controlled string for inclusion in a tracing
/// field or log message. Replaces every non-printable-ASCII char with
/// `?` and caps total length at [`MAX`] characters. Spaces are
/// preserved (single-line log fields commonly contain them).
///
/// Returns an owned `String`; the caller passes it to tracing via
/// `%sanitized` (Display) so the formatted output already carries
/// the sanitized form.
pub fn sanitize_for_log(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX));
    for c in s.chars().take(MAX) {
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
pub const MAX_DEBUG_BODY_BYTES: usize = 4096;

/// Cap on the serialized body emitted at TRACE level by all four body
/// trace helpers (`trace_ingress_body`, `trace_outgoing_body`,
/// `trace_upstream_success_body`, `trace_egress_body`). 16 KB is
/// generous for diagnosis without flooding logs when a debug session
/// gets left on by accident. Operators can bump this locally for
/// full-body debugging during a campaign.
pub const MAX_TRACE_BODY_BYTES: usize = 16 * 1024;

/// Backward-compatible alias for the old name. Prefer
/// [`MAX_TRACE_BODY_BYTES`] -- the rename clarifies that this cap
/// applies to all four body-trace directions, not only the outgoing
/// one. Kept so downstream consumers do not break on rename.
#[deprecated(
    since = "0.5.0",
    note = "renamed to MAX_TRACE_BODY_BYTES (cap applies to all four body trace helpers)"
)]
#[allow(non_upper_case_globals)]
pub const MAX_TRACE_OUTGOING_BODY_BYTES: usize = MAX_TRACE_BODY_BYTES;

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

/// Whether `ROUTECTL_LOG_REDACT_PROMPTS` is set to a truthy value.
///
/// Read once on the FIRST CALL that fires (i.e., the first time TRACE
/// is enabled and a `trace_*_body` helper actually runs). The
/// `OnceLock` then freezes the resolved value. Setting the env var
/// before the first traced body is sufficient; flipping it afterward
/// has no effect.
///
/// Practical implication: operators MUST set
/// `ROUTECTL_LOG_REDACT_PROMPTS=1` BEFORE launching routectl (or
/// before flipping TRACE for the first time). The cached-false case
/// (env var set after the first call) silently disables redaction
/// even though the operator believes they enabled it. There is a
/// matching `info`-level startup line in
/// [`log_redaction_status`] so operators can confirm the resolved
/// value once at server boot.
fn redact_enabled() -> bool {
    static REDACT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REDACT.get_or_init(|| {
        std::env::var("ROUTECTL_LOG_REDACT_PROMPTS")
            .ok()
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
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
/// process.
pub fn log_redaction_status() {
    static EMITTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let enabled = redact_enabled();
    EMITTED.get_or_init(|| {
        tracing::info!(
            redact_prompts = enabled,
            "ROUTECTL_LOG_REDACT_PROMPTS resolved (frozen for the rest of this process)"
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
pub fn redact_prompts_with_flag(body: &serde_json::Value, enabled: bool) -> serde_json::Value {
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
                    "system" | "content" => {
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
    let truncated = truncate_json_for_log(&safe, MAX_TRACE_BODY_BYTES);
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
    let truncated = truncate_json_for_log(&safe, MAX_TRACE_BODY_BYTES);
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
    let truncated = truncate_json_for_log(&safe, MAX_TRACE_BODY_BYTES);
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
    let truncated = truncate_json_for_log(&safe, MAX_TRACE_BODY_BYTES);
    tracing::trace!(
        ingress,
        body = %truncated,
        "egress response body"
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
    tracing::debug!(
        provider_kind,
        provider = provider_id,
        status,
        body = %cleaned,
        "upstream error body"
    );
}

#[cfg(test)]
#[path = "log_safe_tests.rs"]
mod tests;
