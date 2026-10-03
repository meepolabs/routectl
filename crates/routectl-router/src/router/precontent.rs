//! Content-free labels for the chunks the streaming pre-content guard
//! buffers before first content.
//!
//! When the guard gives up (buffer overflow, or the upstream closes before
//! any content), its error names the ordered kinds of what it held, so the
//! fallback WARN shows the stream's opening shape without carrying any
//! text, ids, model names or token counts.

use routectl_core::ChatChunk;

/// Stable label for a chunk that is NOT content-bearing. First match wins:
/// role > empty_text > empty_reasoning > finish > usage > upstream_meta >
/// empty_choices > other.
///
/// Callers only pass chunks the content check rejected; a content-bearing
/// chunk is never buffered, so its label here is meaningless.
pub(super) fn precontent_kind(chunk: &ChatChunk) -> &'static str {
    let deltas = || chunk.choices.iter().map(|choice| &choice.delta);
    if deltas().any(|d| d.role.is_some()) {
        "role"
    } else if deltas().any(|d| d.content.is_some()) {
        "empty_text"
    } else if deltas().any(|d| d.reasoning.is_some()) {
        "empty_reasoning"
    } else if chunk.choices.iter().any(|c| c.finish_reason.is_some()) {
        "finish"
    } else if chunk.usage.is_some() {
        "usage"
    } else if chunk.upstream_meta.is_some() {
        "upstream_meta"
    } else if chunk.choices.is_empty() {
        "empty_choices"
    } else {
        "other"
    }
}

/// True when `chunk` has exactly one choice, at index 0, carrying only an
/// empty reasoning string, and the chunk carries nothing else besides its
/// id/model stamps. A long thinking block streams many of these before its
/// first typed detail; they show the upstream is alive and are not
/// metadata. Multi-choice chunks never match, so each choice keeps its own
/// buffered entry for replay.
pub(super) fn is_empty_reasoning_only(chunk: &ChatChunk) -> bool {
    let [choice] = chunk.choices.as_slice() else {
        return false;
    };
    let delta = &choice.delta;
    chunk.usage.is_none()
        && chunk.upstream_meta.is_none()
        && chunk.opaque_events.is_empty()
        && choice.index == 0
        && choice.finish_reason.is_none()
        && choice.matched_stop_sequence.is_none()
        && delta.role.is_none()
        && delta.content.is_none()
        && delta.reasoning.as_deref() == Some("")
        && delta.reasoning_details.is_empty()
        && delta.tool_calls.is_none()
}

/// Ordered run-length summary of the kinds of `chunks`, e.g.
/// `role, empty_reasoning, usage x2, finish`; `none` when there are no chunks.
pub(super) fn precontent_summary<'a>(chunks: impl IntoIterator<Item = &'a ChatChunk>) -> String {
    let mut runs: Vec<(&'static str, usize)> = Vec::new();
    for kind in chunks.into_iter().map(precontent_kind) {
        match runs.last_mut() {
            Some((last, count)) if *last == kind => *count += 1,
            _ => runs.push((kind, 1)),
        }
    }
    if runs.is_empty() {
        return "none".to_owned();
    }
    runs.iter()
        .map(|&(kind, count)| {
            if count == 1 {
                kind.to_owned()
            } else {
                format!("{kind} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
