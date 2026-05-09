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
    const MAX_LEN: usize = crate::MAX_LOG_BODY_EXCERPT;
    let trimmed = body.trim();
    let looks_like_html =
        trimmed.starts_with('<') || trimmed.to_ascii_lowercase().contains("<!doctype");
    if looks_like_html {
        return format!("<html error page, {} bytes>", body.len());
    }
    if trimmed.len() <= MAX_LEN {
        return trimmed.to_string();
    }
    let mut s = trimmed.chars().take(MAX_LEN).collect::<String>();
    s.push_str("... [truncated]");
    s
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
}
