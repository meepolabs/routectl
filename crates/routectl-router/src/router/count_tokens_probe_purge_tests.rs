use super::*;
use crate::learned_capability::{PurgePreparation, RoutingDecision};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

fn learned_count_router() -> Router {
    build_router(vec![Leg {
        nickname: "nick",
        provider_name: "anthropic",
        entry: anthropic_api_entry(),
        behavior: CountBehavior::Ok(42),
        upstream: None,
    }])
    .0
}

fn request_with_search() -> ChatRequest {
    let mut req = count_req();
    req.tools = Some(vec![routectl_core::ToolDef::Other(
        serde_json::json!({"type": "web_search"}),
    )]);
    req
}

fn seed_lapsed(router: &Router) {
    let now = Instant::now();
    let registry = &router.learned_capabilities;
    registry.observe(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    registry.expire_keyed_in_generation(
        registry.generation(),
        "nick",
        "web_search",
        "anthropic-api",
        now,
    );
}

#[test]
fn count_tokens_early_settlement_releases_probe_before_abandoned_purge_and_reprobe() {
    let router = learned_count_router();
    seed_lapsed(&router);
    let (_, admissions) = router
        .dispatch_chain_for_request(&request_with_search())
        .unwrap();
    assert_eq!(
        admissions.len(),
        1,
        "real feature filter must admit the lapsed key"
    );
    let registry = &router.learned_capabilities;
    assert!(matches!(
        registry.prepare_purge(registry.generation(), "nick", "web_search", "anthropic-api"),
        PurgePreparation::Busy
    ));
    router.settle_count_probe_admissions(admissions);
    let lease = match registry.prepare_purge(
        registry.generation(),
        "nick",
        "web_search",
        "anthropic-api",
    ) {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("count early settlement must release ownership: {other:?}"),
    };
    registry.restore_purge(lease);
    assert_eq!(
        registry
            .acting_negative_in_generation(
                registry.generation(),
                "nick",
                "web_search",
                "anthropic-api",
                Instant::now()
            )
            .unwrap()
            .0,
        RoutingDecision::ProbeAdmitted
    );
}

#[tokio::test]
async fn both_count_tokens_entrypoints_release_admissions_without_clearing_negatives() {
    for with_meta in [false, true] {
        let router = learned_count_router();
        seed_lapsed(&router);
        if with_meta {
            let counted = router.count_tokens_with_meta(request_with_search()).await;
            assert_eq!(counted.result.unwrap().input_tokens, 42);
        } else {
            assert_eq!(
                router
                    .count_tokens(request_with_search())
                    .await
                    .unwrap()
                    .input_tokens,
                42
            );
        }
        let entry = router.learned_capabilities.export_entries().pop().unwrap();
        assert!(!entry.in_flight);
        assert_eq!(entry.observations, 1);
        assert_eq!(entry.consecutive_failed_probes, 0);
        assert!(matches!(
            router.learned_capabilities.prepare_purge(
                router.registry_generation(),
                "nick",
                "web_search",
                "anthropic-api"
            ),
            PurgePreparation::Reserved(_)
        ));
    }
}
