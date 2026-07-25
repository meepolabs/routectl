//! Regression guard for the K-estimator sample write / query key mismatch.
//!
//! `record_k_sample` writes each per-session K window under the SERVED
//! model nickname (`meta.served_model` == `target.nickname`, via
//! `observe_meta`). The would-trim query in [`Router::record_would_trim`]
//! must therefore key its [`crate::k_estimator::KQuery`] on the SAME served
//! nickname -- not the upstream wire id -- or the query never matches the
//! store, the estimate stays `Cold`, and `would_trim_k_floor` is recorded
//! permanently `None` even for a heavily-calibrated session.
use super::*;
use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::schema::{Message, MessageContent, Role};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION: &str = "sess-k-regression";
const PROVIDER_KIND: &str = "anthropic-api";
/// Served model nickname -- the label `record_k_sample` writes under and
/// the query must match.
const SERVED_MODEL: &str = "pt-opus-4-8";
/// Upstream wire id -- what the buggy query keyed on. Distinct from the
/// nickname AND a VERIFIED pricing cell, so pricing (break_even) resolves
/// on it exactly as it does in production.
const UPSTREAM: &str = "claude-opus-4-8";

/// Build the `EffectiveRow` `record_would_trim` now expects to be handed
/// (mirroring `factory::apply_catalog_overlay`'s chain-build-time merge,
/// with no overlay cell -- these regression tests exercise the baked
/// layer only).
fn effective_row_for(provider_kind: &str, model: &str) -> EffectiveRow {
    use crate::catalog::{lookup_baked_with_overrides, merge};
    let baked = lookup_baked_with_overrides(provider_kind, model, None, &BTreeMap::new());
    merge(baked.as_ref(), None)
}

/// A bulky payload of roughly `tokens` tokens (4 bytes/token estimate).
fn payload_of_tokens(tokens: usize) -> String {
    "x".repeat(tokens * 4)
}

fn tool_result_msg(payload: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: json!(payload),
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn tool_use_msg(payload: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_1".into(),
            name: "search".into(),
            input: json!(payload),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A tool-heavy conversation well above the default 100k trigger with a
/// large elidable span, carrying `SESSION` as the inbound session key and
/// `UPSTREAM` as the wire model -- so `record_would_trim` finds a plan and
/// a verified pricing row.
fn triggering_req() -> ChatRequest {
    let payload = payload_of_tokens(12_000);
    let mut messages = vec![
        text_msg(Role::User, "system framing turn one"),
        text_msg(Role::Assistant, "acknowledged"),
    ];
    for _ in 0..6 {
        messages.push(tool_use_msg(&payload));
        messages.push(tool_result_msg(&payload));
    }
    for i in 0..6 {
        messages.push(text_msg(Role::User, &format!("recent turn {i}")));
    }
    let mut req = ChatRequest {
        model: UPSTREAM.into(),
        messages: messages.into(),
        ..Default::default()
    };
    req.routectl_internal.inbound_session_key = Some(SESSION.into());
    req
}

/// Record a calibrated-size window of MIXED reuse under the SERVED-model
/// triple, exactly as the ingress capture path does post-response
/// (`record_k_sample` on `meta.served_model`). ~2/3 hits gives a
/// strictly-interior reuse rate so the calibrated floor is non-trivially
/// positive rather than an all-miss zero.
fn record_calibrated_samples(router: &Router) {
    for i in 0..12u64 {
        let cache_read = u64::from(i % 3 != 0);
        router.record_k_sample(
            Some(SESSION),
            PROVIDER_KIND,
            SERVED_MODEL,
            cache_read,
            UNIX_EPOCH + Duration::from_secs(i * 10_000),
        );
    }
}

fn key(model: &str) -> crate::k_estimator::KSessionKey {
    crate::k_estimator::KSessionKey {
        session_key: SESSION.into(),
        provider_kind: PROVIDER_KIND.into(),
        model: model.into(),
    }
}

fn estimate_for(router: &Router, model: &str) -> crate::k_estimator::KEstimate {
    router.k_estimator.estimate(&crate::k_estimator::KQuery {
        session_key: Some(SESSION),
        provider_kind: PROVIDER_KIND,
        model,
        ttl: Duration::from_mins(5),
        now: SystemTime::now(),
    })
}

#[test]
fn record_would_trim_queries_k_store_under_served_model_not_upstream() {
    use crate::k_estimator::Confidence;

    // Arrange: a router whose K store is populated ONLY under the served-
    // model triple (the key `record_k_sample` writes), plus a request that
    // trips the trim trigger and prices against a verified upstream cell.
    let router = Router::new(Arc::new(Config::default()));
    record_calibrated_samples(&router);
    let req = triggering_req();
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);

    // Invariant guard: the sample-write key and the (correct) query key are
    // the SAME triple -- keyed on the served nickname, NOT the upstream id.
    // The store therefore has a window under the served triple and NOTHING
    // under the upstream triple, so a calibrated K result below can only
    // come from a query that keyed on the served nickname.
    assert!(
        router.k_session_store.get(&key(SERVED_MODEL)).is_some(),
        "samples must be recorded under the served-model triple",
    );
    assert!(
        router.k_session_store.get(&key(UPSTREAM)).is_none(),
        "nothing is recorded under the upstream triple",
    );
    assert_eq!(
        estimate_for(&router, SERVED_MODEL).confidence,
        Confidence::Calibrated,
        "12 mixed samples under the served triple must classify Calibrated",
    );
    assert_eq!(
        estimate_for(&router, UPSTREAM).confidence,
        Confidence::Cold,
        "the upstream triple is unpopulated -- a query there is always Cold",
    );

    // Act: drive the would-trim query path with the upstream wire id AND
    // the served nickname threaded separately, mirroring the two dispatch
    // call sites (pricing keys on upstream; K keys on the served model).
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
    router.record_would_trim(
        &req,
        Some(PROVIDER_KIND),
        UPSTREAM,
        SERVED_MODEL,
        &effective,
        &mut meta,
    );

    // Assert: pricing resolved on the verified upstream cell (sanity that
    // the query block was reached), AND the K query hit the calibrated
    // served-model window, so the floor is persisted. Before the fix the
    // query keyed on UPSTREAM, missed the store, stayed Cold, and left
    // would_trim_k_floor None.
    assert!(
        meta.would_trim_break_even_k.is_some(),
        "verified upstream pricing must populate break_even",
    );
    assert!(
        meta.would_trim_k_floor.is_some(),
        "K query must key on the served model to match the sample-write key",
    );
}

#[test]
fn record_would_trim_folds_missing_baked_row_to_no_break_even() {
    // Arrange: a provider_kind that names no baked cell at all -- not
    // even a provider catch-all (every routectl-shipped provider kind
    // carries one). The two-layer merge resolves `Missing`, which
    // folds to the SAME conservative sentinel behavior as `Disabled`:
    // no break-even K, even though the freed-token count still
    // records.
    const UNKNOWN_KIND: &str = "totally-unknown-kind";
    let router = Router::new(Arc::new(Config::default()));
    let req = triggering_req();
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);

    let effective = effective_row_for(UNKNOWN_KIND, UPSTREAM);
    router.record_would_trim(
        &req,
        Some(UNKNOWN_KIND),
        UPSTREAM,
        SERVED_MODEL,
        &effective,
        &mut meta,
    );

    assert!(
        meta.would_trim_tokens.is_some(),
        "the freed-token count records regardless of pricing trust",
    );
    assert_eq!(
        meta.would_trim_break_even_k, None,
        "a Missing catalog row must record K* = None",
    );
}
