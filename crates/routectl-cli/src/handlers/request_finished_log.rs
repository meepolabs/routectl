//! The one INFO `request finished` line a client request writes, rendered
//! from its usage row.
//!
//! Content-free: the line carries ids, closed-vocabulary tokens, routing
//! names and counts only, never a body, prompt, header, error detail,
//! credential, session id or user-agent. The routing names (alias, model,
//! upstream, provider, seat) can hold the client's requested model id on a
//! forwarded-credential or default-alias route, and the request id can be
//! client-chosen, so every string passes through
//! [`routectl_core::sanitize_for_log`], which escapes control characters and
//! bounds the length.

use std::fmt;

use routectl_usage::UsageRecord;

/// The line's message; one per finished request.
pub const REQUEST_FINISHED: &str = "request finished";

/// Rendering of a column the row leaves unset.
const ABSENT: &str = "-";

/// A column value, or [`ABSENT`] when the row left it unset. Every key is
/// always present on the line, so a reader can split on it without guessing
/// which keys a given outcome carries.
struct OrAbsent<T>(Option<T>);

impl<T: fmt::Display> fmt::Display for OrAbsent<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => value.fmt(f),
            None => f.write_str(ABSENT),
        }
    }
}

fn safe(value: Option<&str>) -> OrAbsent<String> {
    OrAbsent(value.map(routectl_core::sanitize_for_log))
}

/// Emit the request's one INFO summary from its finished row. Called before
/// the row is handed to the writer, so the line exists even when the queue
/// refuses the row.
pub fn emit_request_finished(record: &UsageRecord) {
    tracing::info!(
        request_id = %routectl_core::sanitize_for_log(&record.request_id),
        operation = %record.operation.as_str(),
        outcome = %record.outcome.as_str(),
        http_status = %OrAbsent(record.http_status),
        harness = %record.harness.as_str(),
        probe = record.probe,
        alias = %routectl_core::sanitize_for_log(&record.alias),
        model = %safe(record.model.as_deref()),
        upstream = %safe(record.upstream.as_deref()),
        provider = %safe(record.provider.as_deref()),
        seat = %safe(record.seat.as_deref()),
        attempt_count = record.attempt_count,
        fallback_count = record.fallback_count,
        latency_ms = record.latency_ms,
        ttfb_ms = %OrAbsent(record.ttfb_ms),
        input_tokens = %OrAbsent(record.input_tokens),
        output_tokens = %OrAbsent(record.output_tokens),
        cache_read = %OrAbsent(record.cache_read),
        wire_losses = record.wire_losses,
        translation_drops = record.translation_drops,
        local_refusals = record.local_refusals,
        field_repairs = record.field_repairs,
        missing_terminals = record.missing_terminals,
        "{REQUEST_FINISHED}",
    );
}
