//! The token-estimate calibration numerator is stamped for EVERY dispatched
//! attempt, not only the ones the trim advisories select for.
//!
//! `record_would_trim` returns early for a request below
//! `trigger_tokens`, leaving every `would_trim_*` field `None`. The
//! calibration estimate must survive that early return: gating the evidence
//! on request size would train a per-lane correction factor exclusively on
//! large requests, which is exactly the population bias that makes a learned
//! factor untrustworthy on the ordinary traffic it would then be applied to.
use super::*;
use routectl_core::schema::{Message, MessageContent, Role};

const PROVIDER_KIND: &str = "anthropic-api";
const SERVED_MODEL: &str = "pt-opus-4-8";
const UPSTREAM: &str = "claude-opus-4-8";

/// The merged `EffectiveRow` a chain-build hands `record_would_trim`.
fn effective_row_for(provider_kind: &str, model: &str) -> EffectiveRow {
    use crate::catalog::{lookup_baked_with_overrides, merge};
    let baked = lookup_baked_with_overrides(provider_kind, model, None, &BTreeMap::new());
    merge(baked.as_ref(), None)
}

/// A one-turn request far below any plausible trim trigger.
fn tiny_req() -> ChatRequest {
    ChatRequest {
        model: UPSTREAM.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    }
}

#[test]
fn calibration_estimate_is_stamped_below_the_trim_trigger() {
    // Arrange: a request so small that every would-trim advisory declines.
    let router = Router::new(Arc::new(Config::default()));
    let req = tiny_req();
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);

    // Act
    router.record_would_trim(
        &req,
        Some(PROVIDER_KIND),
        SERVED_MODEL,
        &effective,
        &mut meta,
    );

    // Assert: the trim advisories all declined (proving the early return was
    // taken) while the calibration estimate still landed, carrying exactly
    // the estimator's value for the dispatched payload.
    assert_eq!(
        meta.would_trim_recorder_version, None,
        "sanity: this request is below the trim trigger",
    );
    assert_eq!(meta.would_trim_tokens, None);
    assert_eq!(
        meta.calib_estimated_tokens,
        Some(crate::context_trim::estimate_total_tokens(&req)),
        "the calibration estimate must survive the trim-trigger early return",
    );
}

#[test]
fn calibration_estimate_is_the_last_dispatched_attempts_estimate() {
    // Arrange: two successive attempts against the same meta, the second
    // carrying a materially larger payload -- the shape a chain walk that
    // falls forward produces.
    let router = Router::new(Arc::new(Config::default()));
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);
    let first = tiny_req();
    let mut second = tiny_req();
    second.messages = vec![Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text("x".repeat(4_000)),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();

    // Act
    for req in [&first, &second] {
        router.record_would_trim(
            req,
            Some(PROVIDER_KIND),
            SERVED_MODEL,
            &effective,
            &mut meta,
        );
    }

    // Assert: last-writer-wins, so the value describes the attempt whose
    // reported usage will be the paired actual -- not the first attempt's.
    assert_eq!(
        meta.calib_estimated_tokens,
        Some(crate::context_trim::estimate_total_tokens(&second)),
    );
    assert_ne!(
        meta.calib_estimated_tokens,
        Some(crate::context_trim::estimate_total_tokens(&first)),
        "sanity: the two payloads must estimate differently",
    );
}
