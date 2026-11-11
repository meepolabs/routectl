//! The one INFO `request finished` line a client request writes, rendered
//! from its usage row.
//!
//! Content-free: the line carries ids, closed-vocabulary tokens, routing
//! names and counts only, never a body, prompt, error detail, credential,
//! session id or user-agent. The request id is the only header value the
//! line carries: a client's `x-request-id` is used only after it passes the
//! request-id allowlist (ASCII letters, digits and `-_.:`, length-bounded),
//! otherwise a generated id. `harness` is a closed token classified from the
//! user-agent; no raw header or other header value appears. The routing
//! names (alias, model, upstream, provider, seat) can hold the client's
//! requested model id on a forwarded-credential or default-alias route, and
//! the request id can be client-chosen, so every string passes through
//! [`routectl_core::sanitize_for_log`], which escapes control characters and
//! bounds the length, and is then written as a quoted, escaped value, so a
//! name carrying ` outcome=ok` cannot add a key to the line. Closed tokens,
//! booleans and counts stay bare.

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

/// A client-influenced string, sanitized and written as a quoted, escaped
/// value; an unset column is the bare [`ABSENT`], so it never reads as a
/// value spelled `-`.
struct Quoted(Option<String>);

impl fmt::Debug for Quoted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => fmt::Debug::fmt(value.as_str(), f),
            None => f.write_str(ABSENT),
        }
    }
}

fn quoted(value: Option<&str>) -> Quoted {
    Quoted(value.map(routectl_core::sanitize_for_log))
}

/// Emit the request's one INFO summary from its finished row. Called before
/// the row is handed to the writer, so the line exists even when the queue
/// refuses the row.
pub fn emit_request_finished(record: &UsageRecord) {
    tracing::info!(
        request_id = ?quoted(Some(&record.request_id)),
        operation = %record.operation.as_str(),
        outcome = %record.outcome.as_str(),
        http_status = %OrAbsent(record.http_status),
        harness = %record.harness.as_str(),
        probe = record.probe,
        alias = ?quoted(Some(&record.alias)),
        model = ?quoted(record.model.as_deref()),
        upstream = ?quoted(record.upstream.as_deref()),
        provider = ?quoted(record.provider.as_deref()),
        seat = ?quoted(record.seat.as_deref()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sanitize cap on a client-chosen string, in characters.
    const CAP: usize = 256;

    /// A request id made only of characters the quoted rendering escapes
    /// renders at its worst case: every kept character doubled, plus the two
    /// quotes. Past the cap the escaping cannot grow the field further.
    #[test]
    fn a_quote_heavy_request_id_renders_within_twice_the_cap_plus_its_quotes() {
        // Arrange: well past the cap, every character escaped when quoted.
        let request_id = "\"\\".repeat(CAP);
        let record = UsageRecord {
            request_id,
            ..Default::default()
        };

        // Act
        let events = routectl_testkit::capture_events(|| emit_request_finished(&record));

        // Assert
        let line = events
            .iter()
            .find(|e| e.message == REQUEST_FINISHED)
            .expect("the finished line is emitted");
        let rendered = line.field("request_id").expect("request_id is on the line");
        assert_eq!(
            rendered.chars().count(),
            2 * CAP + 2,
            "the bound is reached and not exceeded: {rendered}"
        );
    }
}
