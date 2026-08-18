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
///
/// The samples are timestamped in the last few seconds because the estimator
/// counts only samples inside the query's TTL window, and the query under
/// test runs on the real dispatch clock.
fn record_calibrated_samples(router: &Router) {
    let base = SystemTime::now() - Duration::from_secs(30);
    for i in 0..12u64 {
        let cache_read = u64::from(i % 3 != 0);
        router.record_k_sample(
            Some(SESSION),
            PROVIDER_KIND,
            SERVED_MODEL,
            cache_read,
            base + Duration::from_secs(i),
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

/// Under an alias, the estimator's window and the shadow misfire monitor's
/// entry must land on ONE key.
///
/// The monitor exists to EXPLAIN the estimator's behavior for a window: a
/// misfire says "the prefix this window's samples were measured over shifted".
/// If the two stores key on different model dimensions -- the estimator on the
/// served nickname, the monitor on the upstream wire id -- they describe
/// different populations under any alias whose nickname differs from the wire
/// id, and a misfire can no longer be attributed to the window it was supposed
/// to explain. Both now derive through the shared `k_query_key` helper, so the
/// only observable key is the served-nickname triple.
#[test]
fn shadow_store_and_k_store_key_identically_under_an_alias() {
    // Arrange: the served nickname and the upstream wire id DIFFER, which is
    // precisely the aliased case where a split keying is observable.
    assert_ne!(
        SERVED_MODEL, UPSTREAM,
        "the divergence this pins only exists when the two labels differ",
    );
    let router = Router::new(Arc::new(Config::default()));
    record_calibrated_samples(&router);
    let req = triggering_req();
    let mut meta = DispatchMeta::for_alias(SERVED_MODEL);
    let effective = effective_row_for(PROVIDER_KIND, UPSTREAM);
    assert!(
        router.shadow_store.is_empty(),
        "a fresh router holds no shadow entries",
    );

    // Act: one dispatch, which both reads the estimator and records into the
    // shadow monitor.
    router.record_would_trim(
        &req,
        Some(PROVIDER_KIND),
        SERVED_MODEL,
        &effective,
        &mut meta,
    );

    // Assert: the shadow entry is addressable by the SAME triple the K window
    // lives under, and nothing exists under the upstream-keyed triple. Probing
    // via `record_and_compare` with the stored fingerprint: a hit on the
    // served-model key reports `Stable` (the entry is already there), while the
    // upstream key would be `FirstSeen` if the store had been keyed on it.
    let fp = trimmed_prefix_fingerprint(
        &req,
        &propose_steady_state_trim(&req, &router.config.trim.to_params())
            .expect("the fixture request trips the trim trigger"),
    );
    assert_eq!(
        router
            .shadow_store
            .record_and_compare(key(SERVED_MODEL), fp, SystemTime::now()),
        crate::k_estimator::ShadowOutcome::Stable,
        "the shadow entry must be keyed under the served nickname, like the K window",
    );
    assert_eq!(
        router
            .shadow_store
            .record_and_compare(key(UPSTREAM), fp, SystemTime::now()),
        crate::k_estimator::ShadowOutcome::FirstSeen,
        "nothing may be keyed under the upstream wire id",
    );
    assert!(
        router.k_session_store.get(&key(SERVED_MODEL)).is_some(),
        "the K window this monitor explains lives under the same triple",
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

#[test]
fn k_query_key_projects_the_same_triple_to_both_sides() {
    let derived = super::k_query_key(Some(SESSION), Some(PROVIDER_KIND), SERVED_MODEL);

    let query = derived.query(Duration::from_mins(5), UNIX_EPOCH);
    let store_key = derived
        .store_key()
        .expect("a keyed request has a store key");

    assert_eq!(query.session_key, Some(store_key.session_key.as_str()));
    assert_eq!(query.provider_kind, store_key.provider_kind);
    assert_eq!(query.model, store_key.model);
    assert_eq!(
        store_key,
        key(SERVED_MODEL),
        "the derived write key must equal the served-model triple",
    );
    assert_ne!(
        store_key,
        key(UPSTREAM),
        "the model dimension is the nickname, never the upstream wire id",
    );
}

#[test]
fn k_query_key_is_deterministic_across_calls() {
    let first = super::k_query_key(Some(SESSION), Some(PROVIDER_KIND), SERVED_MODEL);
    let second = super::k_query_key(Some(SESSION), Some(PROVIDER_KIND), SERVED_MODEL);

    assert_eq!(first.store_key(), second.store_key());

    let a = first.query(Duration::from_mins(5), UNIX_EPOCH);
    let b = second.query(Duration::from_mins(5), UNIX_EPOCH);
    assert_eq!(
        (a.session_key, a.provider_kind, a.model),
        (b.session_key, b.provider_kind, b.model),
    );
}

#[test]
fn k_query_key_normalizes_absent_provider_kind_and_skips_keyless_writes() {
    let no_kind = super::k_query_key(Some(SESSION), None, SERVED_MODEL);
    assert_eq!(
        no_kind
            .query(Duration::from_mins(5), UNIX_EPOCH)
            .provider_kind,
        "",
        "an unstamped provider kind reads deterministically under the empty token",
    );
    assert_eq!(
        no_kind
            .store_key()
            .expect("a keyed request has a store key")
            .provider_kind,
        "",
    );

    let keyless = super::k_query_key(None, Some(PROVIDER_KIND), SERVED_MODEL);
    assert_eq!(
        keyless
            .query(Duration::from_mins(5), UNIX_EPOCH)
            .session_key,
        None,
        "a keyless read carries no session and is Cold by construction",
    );
    assert_eq!(
        keyless.store_key(),
        None,
        "a keyless request has no triple to accumulate against",
    );
}

/// The write the ingress path performs and the read the dispatch path
/// performs must land on ONE window. Pinned end-to-end through the shared
/// helper: a sample written under the derived store key is visible to the
/// estimator queried through the derived query.
#[test]
fn derived_write_key_and_derived_query_address_one_window() {
    let router = Router::new(Arc::new(Config::default()));
    record_calibrated_samples(&router);

    let derived = super::k_query_key(Some(SESSION), Some(PROVIDER_KIND), SERVED_MODEL);

    assert!(
        router
            .k_session_store
            .get(&derived.store_key().expect("keyed"))
            .is_some(),
        "the derived write key must address the window record_k_sample wrote",
    );
    assert_eq!(
        router
            .k_estimator
            .estimate(&derived.query(Duration::from_mins(5), SystemTime::now()))
            .confidence,
        crate::k_estimator::Confidence::Calibrated,
        "the derived query must read that same window, not a cold miss",
    );
}

/// `may_suppress` gates on the confidence CLASS, never on the numeric floor:
/// `Low` force-clamps `k_floor` to 0.0 and `Cold` reports an all-zero
/// default, so a numeric compare would read either clamp as evidence of no
/// reuse.
#[test]
fn may_suppress_is_true_only_for_calibrated() {
    use crate::k_estimator::{Confidence, EstimateSource, KEstimate};

    fn estimate(k_floor: f64, samples: u32, confidence: Confidence) -> KEstimate {
        KEstimate {
            k_floor,
            k_point: k_floor,
            k_ceiling: k_floor,
            samples,
            confidence,
            source: if confidence == Confidence::Cold {
                EstimateSource::ColdDefault
            } else {
                EstimateSource::LiveLedger
            },
        }
    }

    // Calibrated authorizes acting on the floor, whatever its value -- a
    // measured 0.0 is real evidence of no reuse, unlike a clamped one.
    let calibrated_zero = estimate(0.0, 16, Confidence::Calibrated);
    let calibrated_high = estimate(9.0, 16, Confidence::Calibrated);
    assert!(super::may_suppress(&calibrated_zero));
    assert!(super::may_suppress(&calibrated_high));

    // Low: the floor is clamped to 0.0 by the thin-sample rule, which a
    // numeric compare would mistake for a measured no-reuse session.
    assert!(!super::may_suppress(&estimate(0.0, 3, Confidence::Low)));
    // Cold: no samples at all.
    assert!(!super::may_suppress(&estimate(0.0, 0, Confidence::Cold)));
}
