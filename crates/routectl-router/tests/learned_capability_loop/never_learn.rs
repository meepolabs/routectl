//! The never-learn cases: health-status errors and unresolvable rejections
//! must each record no learned negative.

use super::*;

// ---------------------------------------------------------------------------
// Leg 3: never-learn cases (health statuses).
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
