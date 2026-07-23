//! The never-learn cases: health-status errors, operator-remapped
//! classifications, forwarded pass-through requests, and unresolvable
//! rejections must each record no learned negative.

use super::*;

// ---------------------------------------------------------------------------
// Leg 3: never-learn cases (health statuses, operator-remapped, forwarded).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_status_errors_never_learn() {
    for status in [429u16, 500u16] {
        let a = upstream_server(vec![(status, health_body())]).await;
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

        let d = complete(&router, "chain").await;
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "status {status} must not learn",
        );
        assert!(d.result.is_ok(), "status {status}: fall back to B");
        assert_eq!(d.meta.served_provider.as_deref(), Some("prov_b"));
    }
}

#[tokio::test]
async fn remapped_classification_never_learns() {
    // An operator remap of A's 400s to FeatureUnsupported sets `remapped =
    // true`. The 400 body still carries a resolvable `/error/param`, so
    // WITHOUT the remap guard the resolver would learn `web_search`; the
    // `remapped` early return is the only thing that blocks it.
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a
        .class_overrides
        .insert(400, ConfigFailureClass::FeatureUnsupported);
    let router = build_router(
        vec![
            Upstream {
                runtime: runtime_a,
                ..Upstream::openai("m_a", "prov_a", &a.uri())
            },
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        48,
    )
    .await;

    let d = complete(&router, "chain").await;
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "a remapped classification must not learn",
    );
    assert_eq!(hits(&a).await, 1, "A was still dialed (and remapped)");
    assert!(d.result.is_ok(), "remapped still falls back to B");

    let d2 = complete(&router, "chain").await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(
        hits(&a).await,
        2,
        "no learned negative means A is not de-prioritized",
    );
}

#[tokio::test]
async fn forwarded_request_never_learns() {
    // A request carrying a forwarded bearer is a pass-through; the router
    // must never learn a negative from it (the request is not routectl's own
    // catalog request). Even a resolvable 400 records nothing.
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

    let mut req = req_with_feature("chain", WEB_SEARCH);
    req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("fwd-token".to_string()));
    let d = router
        .complete_with_options(req, RouterOptions::default())
        .await;
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "a forwarded request must not learn",
    );
    assert!(d.result.is_ok(), "forwarded still falls back to B");
    assert_eq!(hits(&a).await, 1, "A was dialed (forwarded, not learned)");

    // No negative recorded, so a second forwarded request re-dials A.
    let mut req2 = req_with_feature("chain", WEB_SEARCH);
    req2.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("fwd-token".to_string()));
    let d2 = router
        .complete_with_options(req2, RouterOptions::default())
        .await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "A not de-prioritized");
}

#[tokio::test]
async fn unresolvable_rejection_does_not_learn() {
    // A 400 whose `/error/param` is absent (a paramless `unsupported_value`)
    // names no capability the resolver can attribute -> no learn, and A is
    // never de-prioritized on the next request.
    let paramless = json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_value",
            "message": "Unsupported value."
        }
    });
    let a = upstream_server(vec![(400, paramless)]).await;
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

    let d = complete(&router, "chain").await;
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "an unresolvable rejection must not learn",
    );
    assert!(d.result.is_ok());

    let d2 = complete(&router, "chain").await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "A re-dialed: nothing was learned");
}
