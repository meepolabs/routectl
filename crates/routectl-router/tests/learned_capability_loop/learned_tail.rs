//! The learned-only tail: a chain whose every member carries an acting learned
//! negative is still attempted (route-away-with-floor) rather than hard-emptied,
//! a static `unsupported_features` match returns NotImplemented, and two
//! distinct negatives on one target both re-probe and clear without leaking a
//! probe slot.

use super::*;

// ---------------------------------------------------------------------------
// The learned-only tail: route-away-with-floor vs a statically-empty 501.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_targets_learned_still_attempts_the_tail() {
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "learned_tail",
        &["m_a", "m_b"],
        48,
    )
    .await;

    let d1 = complete(&router, "learned_tail").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        2,
        "both chain members learn the negative",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    let (d2, events) = routectl_testkit::with_capture(complete(&router, "learned_tail")).await;
    assert!(
        matches!(d2.result, Err(Error::Upstream { status: 400, .. })),
        "a learned-only chain must still attempt (not 501): {:?}",
        d2.result,
    );
    assert!(
        !matches!(d2.result, Err(Error::NotImplemented(..))),
        "learned negatives must never hard-empty the chain",
    );
    assert!(
        hits(&a).await >= 2 && hits(&b).await >= 2,
        "the learned tail must be attempted (both targets re-dialed)",
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.message.contains("de-prioritized learned tail")),
        "entering the learned tail must emit a WARN",
    );
}

#[tokio::test]
async fn statically_unsupported_chain_returns_not_implemented() {
    // A STATIC `unsupported_features` match hard-drops the only chain member,
    // emptying the chain: NotImplemented, and the upstream is never dialed.
    let a = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a.unsupported_features = vec![WEB_SEARCH.to_string()];
    let router = build_router(
        vec![Upstream {
            runtime: runtime_a,
            ..Upstream::openai("m_a", "prov_a", &a.uri())
        }],
        "solo",
        &["m_a"],
        48,
    )
    .await;

    let d = complete(&router, "solo").await;
    assert!(
        matches!(d.result, Err(Error::NotImplemented(..))),
        "a statically-empty chain must return NotImplemented: {:?}",
        d.result,
    );
    assert_eq!(
        hits(&a).await,
        0,
        "a statically unsupported target is hard-dropped, never dialed",
    );
}

// ---------------------------------------------------------------------------
// Leg 7: two distinct learned negatives on ONE target both re-probe and
// both settle -- neither admission leaks its in_flight slot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_expired_negatives_on_one_target_both_reprobe_and_clear() {
    let a = upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)),   // req 1: learn F1
        (400, unsupported_body_for(COMPUTER_USE)), // req 2: learn F2
        (200, ok_body()),                          // req 3: double re-probe clears both
        (400, unsupported_body_for(WEB_SEARCH)),   // req 4: fresh learn F1 (proves clear)
        (400, unsupported_body_for(COMPUTER_USE)), // req 5: fresh learn F2 (proves clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = complete_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].capability_key, WEB_SEARCH);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);

    let d2 = complete_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d2.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].capability_key, COMPUTER_USE);
    assert_eq!(hits(&a).await, 2);

    let d3 = complete_with(&router, "solo", &[WEB_SEARCH, COMPUTER_USE]).await;
    assert!(
        d3.result.is_ok(),
        "the double re-probe must reach A and succeed: {:?}",
        d3.result.err()
    );
    assert_eq!(d3.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d3.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 3, "one dispatch carried both probes");

    let d4 = complete_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d4.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d4.meta.learned_capabilities[0].observations, 1,
        "F1 must relearn from scratch -- its probe slot did not leak",
    );

    let d5 = complete_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d5.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d5.meta.learned_capabilities[0].observations, 1,
        "F2 must relearn from scratch -- its probe slot did not leak",
    );
    assert_eq!(hits(&a).await, 5);
}
