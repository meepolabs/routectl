//! Canonical `req.system` -> Responses `instructions` translation.
//!
//! The Responses API has no first-class system role: the prior chat-
//! completions `system` message is collapsed into a top-level
//! `instructions` string. When canonical carries
//! `SystemContent::Blocks`, we flatten each block's text joined by
//! `"\n\n"` (one blank line between blocks) so block boundaries remain
//! visible to the model but the field stays a flat string.
//!
//! Lossy seam: per-block `cache_control` markers cannot ride the
//! Responses wire (no Anthropic-style prompt cache surface here yet),
//! so we drop at DEBUG level. The caller's request_id will be on the span
//! emitted by the Provider's `complete()` instrumentation, so the
//! debug event is correlated automatically. The request orchestrator owns
//! that class's per-request counter over every marked surface, so this
//! module logs the system surface without counting it again.
//!
//! Withholding seam: the Claude Code billing/attribution block is stripped
//! before flatten because OpenAI is a third-party upstream that must not
//! receive the client fingerprint. The wire would carry the text, so the
//! loss is routectl's own choice and rides the policy-action counter rather
//! than the drop counter.

use routectl_core::{ChatRequest, SystemContent};

use crate::translation_drop_metrics::record_translation_policy_action;

/// Build the `instructions` field for the Responses API from the
/// canonical `system` field. Returns `None` when no system content is
/// present so the caller can skip the field entirely (the parent
/// `ResponsesRequest` always serializes `instructions`, even when
/// empty; an empty string `""` is accepted by the server as
/// "no system prompt").
///
/// The billing-strip record fires from HERE rather than from inside
/// [`flatten_filtered_system`], which returns `None` on several paths --
/// a record placed after one of those early returns would miss exactly
/// the requests whose whole system was the stripped block, while the
/// lane's denominator still counted them.
pub(super) fn translate_system(req: &ChatRequest) -> Option<String> {
    let mut billing_stripped = false;
    let instructions = flatten_filtered_system(req, &mut billing_stripped);
    // Withheld by routectl, not by the wire: the Responses `instructions`
    // string would carry the block's text fine, and OpenAI is a third-party
    // upstream that must not receive the client fingerprint it holds. Shares
    // the class literal with the other egresses that strip the same content --
    // one operator-facing label per action, keyed apart by lane.
    // TRANSLATION-DROP: policy-action class=client_fingerprint_stripped test=the_billing_strip_counts_one_policy_action_for_the_request
    if billing_stripped {
        tracing::warn!(
            "openai-responses egress: Claude Code billing/attribution system block dropped",
        );
        record_translation_policy_action("openai-responses", "client_fingerprint_stripped");
    }
    instructions
}

/// Flatten the canonical system into `instructions` with the Claude Code
/// billing/attribution block removed, setting `billing_stripped` when a block
/// was removed. Every `None` return here is an absent-or-blank system, not a
/// failure; the caller owns the record so no early return can skip it.
fn flatten_filtered_system(req: &ChatRequest, billing_stripped: &mut bool) -> Option<String> {
    let s = req.system.as_ref()?;
    let filtered = crate::system_filter::strip_billing_attribution(s, billing_stripped);
    let filtered = filtered?;
    match &filtered {
        SystemContent::Text(t) if t.trim().is_empty() => None,
        SystemContent::Text(t) => Some(t.clone()),
        SystemContent::Blocks(blocks) => {
            warn_on_cache_control_loss(blocks);
            let combined: Vec<String> = blocks
                .iter()
                .filter(|b| !b.text.trim().is_empty())
                .map(|b| b.text.clone())
                .collect();
            if combined.is_empty() {
                None
            } else {
                Some(combined.join("\n\n"))
            }
        }
    }
}

/// Emit a debug event for each block carrying a `cache_control` marker that
/// will be dropped on the Responses wire. Operators can raise the log level
/// to DEBUG to see the loss and can either move the prompt to an
/// Anthropic-shape provider or accept the drop.
///
/// Cross-dialect only: the Responses wire models no prompt-cache breakpoint,
/// so its own ingress builds every `SystemBlock` with `cache_control: None`.
/// A marker reaching here came from an Anthropic-shape or OpenAI-shape
/// client. Seed per foundations sec 14, deletion-blocked pending per-lane
/// wire evidence.
///
/// NOT counted here, deliberately. The request orchestrator already records
/// this class once per request over ALL marked surfaces, the system surface
/// included; a second call from this per-block helper would count the same
/// request twice and would count once per marked block rather than once per
/// request. This function owns the surface's DEBUG record only.
/// TRANSLATION-DROP: lane=openai-responses class=cache_control_unsupported test=a_system_only_cache_marker_still_counts_though_the_warn_defers_to_system_rs
fn warn_on_cache_control_loss(blocks: &[routectl_core::SystemBlock]) {
    let dropped = blocks.iter().filter(|b| b.cache_control.is_some()).count();
    if dropped > 0 {
        tracing::debug!(
            dropped_count = dropped,
            "openai-responses: dropping cache_control on system block(s); \
             Responses API has no prompt-cache breakpoint surface yet"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, SystemBlock, SystemContent};
    // NOTE: tracing-test's `#[traced_test]` installs a GLOBAL default
    // subscriber; a future test in this crate that calls
    // `set_global_default` (instead of the thread-local `with_default`)
    // would pre-empt these `logs_contain` / `logs_assert` checks into
    // false-passes. Keep new log-asserting tests on `#[traced_test]`.
    use tracing_test::traced_test;

    /// The `(openai-responses, client_fingerprint_stripped)` policy-action
    /// counter, read through the public snapshot. Zero before its first bump.
    ///
    /// SERIAL GUARDS: this key is process-global and the runner is threaded, so
    /// every test below that drives the strip carries
    /// `openai_responses_client_fingerprint_stripped` -- the ones asserting a
    /// delta AND the ones that only bump it incidentally while asserting
    /// something else. A guard name no sibling shares excludes nothing.
    fn fingerprint_strip_count() -> u64 {
        crate::translation_drop_metrics::translation_policy_action_snapshot()
            .into_iter()
            .find(|e| {
                e.lane == "openai-responses" && e.policy_class == "client_fingerprint_stripped"
            })
            .map_or(0, |e| e.action_count)
    }

    fn block(text: &str) -> SystemBlock {
        SystemBlock {
            kind: "text".into(),
            text: text.into(),
            cache_control: None,
            citations: None,
        }
    }

    fn req_with_system(system: SystemContent) -> ChatRequest {
        ChatRequest {
            system: Some(system),
            ..Default::default()
        }
    }

    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn strips_billing_block_before_flattening_blocks() {
        // Arrange: a billing/attribution block (carrying the client
        // fingerprint) sits alongside a real prompt block. OpenAI is a
        // third-party upstream and must not receive the fingerprint.
        let req = req_with_system(SystemContent::Blocks(vec![
            block("x-anthropic-billing-header: v=1; fp=secret"),
            block("you are helpful"),
        ]));

        // Act
        let instructions = translate_system(&req).expect("prompt block survives");

        // Assert: only the real prompt reaches instructions.
        assert_eq!(instructions, "you are helpful");
        assert!(
            !instructions.contains("fp="),
            "fingerprint must not leak: {instructions}"
        );
    }

    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn pure_billing_blocks_collapse_to_none() {
        // Arrange: the only system content is the billing block.
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "x-anthropic-billing-header: v=1; fp=secret",
        )]));

        // Act
        let instructions = translate_system(&req);

        // Assert: nothing survives, so no instructions field.
        assert!(
            instructions.is_none(),
            "a pure-billing system must collapse to None, got: {instructions:?}"
        );
    }

    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn pure_billing_text_system_collapses_to_none() {
        // Arrange: a Text-variant system that is itself the billing block.
        let req = req_with_system(SystemContent::Text(
            "x-anthropic-billing-header: v=1; fp=secret".into(),
        ));

        // Act
        let instructions = translate_system(&req);

        // Assert
        assert!(
            instructions.is_none(),
            "pure-billing Text system must collapse to None, got: {instructions:?}"
        );
    }

    #[traced_test]
    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn pure_billing_fires_dropped_warn_even_when_collapsing_to_none() {
        // Arrange: the only system content is the billing block, so the
        // function collapses to None. The billing-dropped warn must still
        // fire (this is the bug the reorder fixes: the `?` short-circuit
        // previously skipped the warn on exactly this all-billing case).
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "x-anthropic-billing-header: v=1; fp=secret",
        )]));

        // Act
        let instructions = translate_system(&req);

        // Assert: still None, AND the warn fired.
        assert!(
            instructions.is_none(),
            "pure-billing system must collapse to None, got: {instructions:?}"
        );
        assert!(
            logs_contain("billing/attribution system block dropped"),
            "billing-dropped warn must fire even when the system collapses to None"
        );
    }

    /// Pin: a blank canonical system (empty string, whitespace-only, or
    /// blocks whose every text is blank) collapses to None so the
    /// orchestrator emits an empty `instructions` -- the server's
    /// "no system prompt" -- rather than a meaningless blank instruction.
    #[test]
    fn blank_canonical_system_collapses_to_none() {
        for system in [
            SystemContent::Text(String::new()),
            SystemContent::Text("   \n\t ".into()),
            SystemContent::Blocks(vec![block(""), block("  \n")]),
        ] {
            // Arrange
            let req = req_with_system(system);

            // Act
            let instructions = translate_system(&req);

            // Assert
            assert!(
                instructions.is_none(),
                "a blank canonical system must collapse to None, got: {instructions:?}"
            );
        }
    }

    #[test]
    fn flattens_multiple_prompt_blocks_with_blank_line() {
        // Arrange: two non-billing blocks must join with a blank line.
        let req = req_with_system(SystemContent::Blocks(vec![block("first"), block("second")]));

        // Act
        let instructions = translate_system(&req).expect("blocks survive");

        // Assert
        assert_eq!(instructions, "first\n\nsecond");
    }

    /// The counted half of the strip: one request whose system carries the
    /// block bumps the policy-action counter exactly once, and the withheld
    /// text is absent from what reaches `instructions` while its unmarked
    /// sibling survives.
    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn the_billing_strip_counts_one_policy_action_for_the_request() {
        // Arrange: TWO billing blocks in one request. The count is per
        // REQUEST, so two stripped blocks are still one action -- a
        // per-occurrence bump would read 2 here.
        let req = req_with_system(SystemContent::Blocks(vec![
            block("x-anthropic-billing-header: v=1; fp=secret"),
            block("x-anthropic-billing-header: v=2; fp=other"),
            block("you are helpful"),
        ]));

        // Act
        let before = fingerprint_strip_count();
        let instructions = translate_system(&req).expect("the prompt block survives");
        let after = fingerprint_strip_count();

        // Assert: counted once, the fingerprint is gone, the prompt remains.
        assert_eq!(
            after - before,
            1,
            "two stripped blocks in one request are one policy action"
        );
        assert_eq!(instructions, "you are helpful");
        assert!(
            !instructions.contains("fp="),
            "the withheld fingerprint must not reach instructions: {instructions}"
        );
    }

    /// The flush sits outside the fallible flatten, so the request whose whole
    /// system IS the block -- the one path that returns `None` after the strip
    /// -- still reaches the counter. A record placed after that early return
    /// would miss exactly these requests while the lane denominator counted
    /// them, reading the action rate low for the case that strips the most.
    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn a_system_collapsing_to_none_still_counts_the_policy_action() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "x-anthropic-billing-header: v=1; fp=secret",
        )]));

        // Act
        let before = fingerprint_strip_count();
        let instructions = translate_system(&req);
        let after = fingerprint_strip_count();

        // Assert
        assert!(
            instructions.is_none(),
            "a pure-billing system must collapse to None, got: {instructions:?}"
        );
        assert_eq!(
            after - before,
            1,
            "a request whose system collapsed to None still stripped a block"
        );
    }

    /// The positive control on the counter: a system carrying no billing block
    /// leaves the key untouched, so the delta assertions above cannot pass on
    /// a counter that bumps for every request.
    #[test]
    #[serial_test::serial(openai_responses_client_fingerprint_stripped)]
    fn a_system_with_no_billing_block_records_no_policy_action() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![block("you are helpful")]));

        // Act
        let before = fingerprint_strip_count();
        let instructions = translate_system(&req).expect("the prompt survives");
        let after = fingerprint_strip_count();

        // Assert
        assert_eq!(instructions, "you are helpful");
        assert_eq!(after, before, "nothing was stripped, so nothing is counted");
    }
}
