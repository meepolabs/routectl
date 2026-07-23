//! The learn / re-probe / backoff lifecycle: learning a negative
//! de-prioritizes the matching target, expiry admits exactly one re-probe that
//! a 2xx clears, and a repeated same-capability rejection settles on the
//! capped-backoff path rather than clearing.

use super::*;

// ---------------------------------------------------------------------------
// Leg 1: learn -> de-prioritize the matching target.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn learn_then_deprioritizes_matching_target() {
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        48,
    )
    .await;

    let d1 = complete(&router, "chain").await;
    assert!(d1.result.is_ok(), "request 1 falls back to B");
    assert_eq!(d1.meta.served_provider.as_deref(), Some("prov_b"));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    let ev = &d1.meta.learned_capabilities[0];
    assert_eq!(ev.capability_key, WEB_SEARCH);
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert!(ev.request_features.iter().any(|f| f == WEB_SEARCH));
    assert_eq!(hits(&a).await, 1);

    let d2 = complete(&router, "chain").await;
    assert!(d2.result.is_ok());
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_b"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 1, "learned negative de-prioritized A");
    assert_eq!(hits(&b).await, 2);
}

// ---------------------------------------------------------------------------
// Leg 2: expiry -> exactly one re-probe -> 2xx clears.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expiry_admits_single_reprobe_then_success_clears() {
    let a = upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // request 1: learn
        (200, ok_body()),                        // request 2: admitted re-probe clears
        (400, unsupported_body_for(WEB_SEARCH)), // request 3: fresh learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = complete(&router, "solo").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    let d2 = complete(&router, "solo").await;
    assert!(
        d2.result.is_ok(),
        "the re-probe must reach A and succeed: {:?}",
        d2.result.err()
    );
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "exactly one re-probe dialed A");

    let d3 = complete(&router, "solo").await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d3.meta.learned_capabilities.len(), 1);
    assert_eq!(
        d3.meta.learned_capabilities[0].observations, 1,
        "a cleared entry must relearn from scratch",
    );
    assert_eq!(hits(&a).await, 3);
}

// ---------------------------------------------------------------------------
// Leg 4: same-capability probe backoff.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_capability_probe_backs_off_on_repeat_rejection() {
    // decay 0 lapses the negative into a re-probe on each request; every
    // re-probe hits the SAME capability rejection again. Each admission settles
    // via the capped-backoff path -- the probe owns its own observation bump,
    // so it emits NO fresh learn event and keeps the entry acting (never
    // cleared). A cleared-then-relearned entry would instead emit a fresh learn
    // event at observations = 1; the sustained ABSENCE of learn events across
    // repeated re-probes is the proof the entry stayed on the backoff path.
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        0,
    )
    .await;

    // Request 1: A rejects and the negative is learned (one learn event).
    let d1 = complete(&router, "chain").await;
    assert!(d1.result.is_ok(), "request 1 falls back to B");
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    // Requests 2 and 3: the negative lapses (decay 0) and A is admitted for a
    // single re-probe each time. A re-rejects the same capability, so each
    // admission settles as a same-capability backoff refresh -- NO fresh learn
    // event. If the entry had been cleared, this would relearn from scratch and
    // emit an event. The reached same-capability settle also emits one
    // probe-settlement event tagged same_capability (reached_target=true).
    for req in 2..=3 {
        let (d, events) = routectl_testkit::with_capture(complete(&router, "chain")).await;
        assert!(d.result.is_ok(), "re-probe {req} rejected, falls back to B");
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "re-probe {req}: a same-capability settle emits no learn event",
        );
        assert_eq!(hits(&a).await, req, "re-probe {req} dialed A exactly once");
        let ev = events
            .iter()
            .find(|e| {
                e.field("event") == Some("probe_settlement")
                    && e.field("outcome") == Some("same_capability")
            })
            .unwrap_or_else(|| {
                panic!("re-probe {req} must emit a same_capability settlement: {events:?}")
            });
        assert_eq!(ev.field("reached_target"), Some("true"));
        assert_eq!(ev.field("reason"), Some("same_capability"));
        assert_eq!(ev.field("capability_key"), Some(WEB_SEARCH));
    }
}
