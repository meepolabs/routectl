//! The response edits the Anthropic stream renderer declares: what it
//! withholds after `message_stop`, a repeated stop reason, and the closing
//! usage counts it fills or restates.

use routectl_core::wire_check::{EditKind, EditSite};
use routectl_core::{ChatChunk, UsageDelta, UsageInputSource};

use crate::ingress::ResponseEdits;

/// A chunk dropped after `message_stop` takes the stop fields and usage
/// counts it carried with it; each is declared at its own path.
pub(super) fn declare_straggler_drop(chunk: &ChatChunk, edits: &mut ResponseEdits) {
    if let Some(choice) = chunk.choices.first()
        && choice.finish_reason.is_some()
    {
        declare_stop_drop(choice.matched_stop_sequence.is_some(), edits);
    }
    let Some(usage) = chunk.usage.as_ref() else {
        return;
    };
    let carried = [
        (usage.prompt_tokens, "/usage/input_tokens"),
        (usage.completion_tokens, "/usage/output_tokens"),
        (
            usage.cache_creation_input_tokens,
            "/usage/cache_creation_input_tokens",
        ),
        (
            usage.cache_read_input_tokens,
            "/usage/cache_read_input_tokens",
        ),
    ];
    for (_, path) in carried.iter().filter(|(count, _)| count.is_some()) {
        edits.declare(EditSite::StreamStragglerDrop, path, EditKind::Rewrite);
        edits.declare(EditSite::StreamStragglerDrop, path, EditKind::Remove);
    }
}

/// The closing usage restates input counts the upstream sent only on its
/// first event (its own closing frame held a zero placeholder or nothing);
/// the egress carried them over, and the raw `input_tokens` is recomputed
/// from that carried total less the cache counts.
pub(super) fn declare_opening_carry(chunk: &ChatChunk, edits: &mut ResponseEdits) {
    let Some(usage) = chunk.usage.as_ref() else {
        return;
    };
    let carried = chunk
        .upstream_meta
        .as_ref()
        .and_then(|meta| meta.usage_input_source)
        .is_some_and(|source| {
            matches!(
                source,
                UsageInputSource::VendorOpening | UsageInputSource::ProxyOpening
            )
        });
    if !carried {
        return;
    }
    let restated = [
        (usage.prompt_tokens, "/usage/input_tokens"),
        (
            usage.cache_creation_input_tokens,
            "/usage/cache_creation_input_tokens",
        ),
        (
            usage.cache_read_input_tokens,
            "/usage/cache_read_input_tokens",
        ),
    ];
    for (_, path) in restated.iter().filter(|(count, _)| count.is_some()) {
        edits.declare(EditSite::AnthropicStreamUsageFill, path, EditKind::Rewrite);
    }
}

/// A withheld stop reason leaves the first one on the wire.
pub(super) fn declare_stop_drop(with_sequence: bool, edits: &mut ResponseEdits) {
    edits.declare(
        EditSite::StreamStragglerDrop,
        "/stop_reason",
        EditKind::Rewrite,
    );
    if with_sequence {
        edits.declare(
            EditSite::StreamStragglerDrop,
            "/stop_sequence",
            EditKind::Rewrite,
        );
    }
}

/// The closing usage writes `0` for a count the canonical usage lacks.
pub(super) fn declare_usage_fill(usage: &UsageDelta, edits: &mut ResponseEdits) {
    if usage.prompt_tokens.is_none() {
        edits.declare(
            EditSite::AnthropicStreamUsageFill,
            "/usage/input_tokens",
            EditKind::Add,
        );
    }
    if usage.completion_tokens.is_none() {
        edits.declare(
            EditSite::AnthropicStreamUsageFill,
            "/usage/output_tokens",
            EditKind::Add,
        );
    }
}

/// The `message_start` already sent carried a minted id in place of the
/// upstream's.
pub(super) fn declare_minted_start_id(minted: bool, edits: &mut ResponseEdits) {
    if minted {
        edits.declare(EditSite::StreamEnvelopeStamp, "/id", EditKind::Rewrite);
    }
}

/// The request-seeded estimate stands in for a usage the upstream's first
/// event did not carry.
pub(super) fn declare_estimated_start_usage(estimated: bool, edits: &mut ResponseEdits) {
    if estimated {
        edits.declare(EditSite::StreamEnvelopeStamp, "/usage", EditKind::Add);
    }
}
