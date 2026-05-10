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

/// Outgoing-body trace cap. 16 KB is a generous excerpt for diagnosis
/// without flooding logs when a debug session gets left on by accident.
pub const MAX_TRACE_OUTGOING_BODY_BYTES: usize = 16 * 1024;

/// Emit a `tracing::trace!` line carrying the outgoing request body
/// for a given provider. Inherits the parent span's `request_id` so a
/// `grep request_id=<id>` correlates ingress -> outgoing -> upstream
/// response in a single pass.
///
/// Gated by `tracing::Level::TRACE` so production with the default
/// `info` level pays nothing. Operators flip to `trace` only during
/// active triage; CLAUDE.md "Triage recipes" documents the workflow
/// + the sensitivity caveat (bodies contain user prompts).
pub fn trace_outgoing_body(provider_kind: &str, provider_id: &str, body: &serde_json::Value) {
    if !tracing::event_enabled!(tracing::Level::TRACE) {
        return;
    }
    let s = serde_json::to_string(body).unwrap_or_default();
    let truncated = if s.len() > MAX_TRACE_OUTGOING_BODY_BYTES {
        format!(
            "{}... [truncated at {MAX_TRACE_OUTGOING_BODY_BYTES} bytes]",
            &s[..MAX_TRACE_OUTGOING_BODY_BYTES]
        )
    } else {
        s
    };
    tracing::trace!(
        provider_kind,
        provider = provider_id,
        body = %truncated,
        "outgoing request body"
    );
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
mod tests {
    use super::{sanitize_for_log, sanitize_upstream_body};

    #[test]
    fn ascii_printable_passes_through_unchanged() {
        let s = "claude-sonnet-4-5-20250929";
        assert_eq!(sanitize_for_log(s), s);
    }

    #[test]
    fn space_is_preserved() {
        assert_eq!(sanitize_for_log("a b c"), "a b c");
    }

    #[test]
    fn newline_is_replaced_with_placeholder() {
        // Embedded `\n` would forge fake log lines on text-format
        // tracing subscribers. Must be filtered.
        assert_eq!(sanitize_for_log("a\nb"), "a?b");
    }

    #[test]
    fn ansi_escape_is_replaced_with_placeholder() {
        // ANSI escape sequences could re-color terminal output and
        // hide subsequent log content. Each non-printable byte
        // becomes `?`.
        assert_eq!(sanitize_for_log("\x1b[31mred\x1b[0m"), "?[31mred?[0m");
    }

    #[test]
    fn multibyte_utf8_emoji_replaced_per_char() {
        // Non-ASCII chars are not in the printable set; one
        // placeholder per char regardless of byte width.
        assert_eq!(sanitize_for_log("hi-rocket"), "hi-rocket");
        assert_eq!(sanitize_for_log("hi\u{1F680}rocket"), "hi?rocket");
    }

    #[test]
    fn truncates_at_max_chars() {
        let long = "a".repeat(300);
        let got = sanitize_for_log(&long);
        assert_eq!(got.chars().count(), 256);
        assert!(got.chars().all(|c| c == 'a'));
    }

    #[test]
    fn truncation_happens_before_filter() {
        // `take(256).chars()` runs before the printable filter, so
        // the cap counts EVERY input char including ones that will
        // be replaced. Documents the actual behavior.
        let mut s = String::new();
        for _ in 0..300 {
            s.push('\n');
        }
        let got = sanitize_for_log(&s);
        assert_eq!(got.chars().count(), 256);
        assert!(got.chars().all(|c| c == '?'));
    }

    #[test]
    fn upstream_body_html_collapsed_to_marker() {
        // A misconfigured base_url often lands on a CDN error page.
        // Don't dump multi-KB markup into our error envelope.
        let html = "<!DOCTYPE html><html><head><title>404</title></head>...";
        let got = sanitize_upstream_body(html);
        assert!(got.starts_with("<html error page"), "got: {got}");
    }

    #[test]
    fn upstream_body_short_passes_through_trimmed() {
        let body = "  rate limited, retry in 5s  ";
        assert_eq!(sanitize_upstream_body(body), "rate limited, retry in 5s");
    }

    #[test]
    fn upstream_body_long_truncated_with_marker() {
        let long = "x".repeat(crate::MAX_LOG_BODY_EXCERPT + 100);
        let got = sanitize_upstream_body(&long);
        assert!(
            got.ends_with("... [truncated]"),
            "expected truncation marker; got tail: ...{}",
            &got[got.len().saturating_sub(20)..]
        );
    }

    /// PR C / FR-1: the cap-aware variant lets callers pick a larger
    /// limit (4 KB for the debug-level full-body log) while reusing
    /// the same HTML collapse + trim logic. Pin the cap behavior so
    /// debug_upstream_error_body's 4 KB ceiling can't silently drift.
    #[test]
    fn upstream_body_with_cap_respects_explicit_limit() {
        use super::sanitize_upstream_body_with_cap;
        let body = "y".repeat(10_000);
        let got = sanitize_upstream_body_with_cap(&body, super::MAX_DEBUG_BODY_BYTES);
        // 4096 chars + "... [truncated]" tail (15 chars) = 4111
        assert_eq!(
            got.len(),
            super::MAX_DEBUG_BODY_BYTES + "... [truncated]".len()
        );
        assert!(got.ends_with("... [truncated]"));

        // Short bodies pass through unchanged.
        let short = "tiny";
        assert_eq!(
            sanitize_upstream_body_with_cap(short, super::MAX_DEBUG_BODY_BYTES),
            "tiny"
        );

        // HTML collapse still applies regardless of cap.
        let html = "<!DOCTYPE html><html>...500 lines...</html>";
        let got = sanitize_upstream_body_with_cap(html, super::MAX_DEBUG_BODY_BYTES);
        assert!(got.starts_with("<html error page"));
    }
}
