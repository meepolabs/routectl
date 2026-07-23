//! The headline real-envelope route-away proof plus the live-network smoke
//! variant: an openai-compat upstream rejects a structured-output request the
//! egress lifts to a top-level `response_format`, and the router learns the
//! canonical `structured_output` negative and routes away.

use super::*;

// ---------------------------------------------------------------------------
// The headline real-envelope route-away proof. The rejected capability
// (`structured_output` -> `response_format` on the wire) SURVIVES egress, so
// this is a scenario a real openai-compat upstream can genuinely produce -- a
// wire-body assertion guards against the synthetic failure mode where the
// rejected surface was dropped before the wire.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_envelope_response_format_400_learns_structured_output_and_routes_away() {
    // A serves a byte-accurate captured openai `unsupported_parameter` 400
    // whose `/error/param` is `response_format`. The request carries
    // `provider_extras.output_config.format`, so `derive_feature_keys` yields
    // `structured_output` AND the openai-compat egress lifts the format to a
    // top-level `response_format` that actually crosses the wire. The resolver
    // translates the rejected param onto `structured_output`; both sides meet
    // on that canonical key and the router learns the negative and routes away
    // from A. B always succeeds.
    let a = raw_upstream_server(400, OPENAI_UNSUPPORTED_RESPONSE_FORMAT_400).await;
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

    // Request 1: A rejects the structured-output surface it received, the
    // router learns it (self-identifying) under the CANONICAL key, and the
    // chain falls back to B.
    let d1 = router
        .complete_with_options(
            req_with_structured_output("chain"),
            RouterOptions::default(),
        )
        .await;
    assert!(
        d1.result.is_ok(),
        "request 1 should fall back to B: {:?}",
        d1.result.err()
    );
    assert_eq!(d1.meta.served_provider.as_deref(), Some("prov_b"));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        1,
        "A's real-envelope rejection must produce exactly one learn event",
    );
    let ev = &d1.meta.learned_capabilities[0];
    assert_eq!(
        ev.capability_key, STRUCTURED_OUTPUT,
        "the learned key is the canonical capability, not the raw wire param",
    );
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.upstream_status, 400);
    assert_eq!(ev.observations, 1);
    assert!(!ev.remapped);
    assert!(
        ev.request_features.iter().any(|f| f == STRUCTURED_OUTPUT),
        "the request naturally derives the learned capability -- the capture \
         membership gate admits it precisely because it is in this set",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // WIRE GUARD: the surface the upstream rejected (`response_format`) was
    // actually present on the OUTBOUND body A received, and the Anthropic-shape
    // `output_config` did not leak. This is the guard against the synthetic
    // failure mode -- a capability dropped at egress that a real upstream could
    // never have rejected.
    let sent = last_request_body(&a).await;
    assert!(
        sent.get("response_format").is_some(),
        "the rejected surface must have crossed the wire; body = {sent}",
    );
    assert!(
        sent.get("output_config").is_none(),
        "the Anthropic-shape output_config must not leak onto the openai wire; body = {sent}",
    );

    // Request 2: A is now an acting learned negative for `structured_output`
    // (an essential -> RouteAway) -> the chain filter de-prioritizes it to the
    // tail. B serves first and A is never re-dialed.
    let d2 = router
        .complete_with_options(
            req_with_structured_output("chain"),
            RouterOptions::default(),
        )
        .await;
    assert!(d2.result.is_ok());
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_b"));
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "the skip must not manufacture a new learn event",
    );
    assert_eq!(
        hits(&a).await,
        1,
        "A must NOT be re-dialed: the learned negative routed away from it",
    );
    assert_eq!(hits(&b).await, 2);
}

// ---------------------------------------------------------------------------
// Live-network smoke variant (ignored in CI). Run with a real openai-compat
// base URL + key that rejects a structured-output request with a 400 whose
// `/error/param` is `response_format` -- the surface that actually SURVIVES
// egress (a built-in tool the egress drops never crosses the wire, so no real
// upstream could reject it). The resolver translates `response_format` onto
// the canonical `structured_output` key the request derives, and the capture
// membership gate admits it because the request carried that capability:
//   ROUTECTL_LIVE_BASE_URL=... ROUTECTL_LIVE_API_KEY=... \
//     cargo test -p routectl-router --test learned_capability_loop -- --ignored
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live network: requires ROUTECTL_LIVE_BASE_URL + ROUTECTL_LIVE_API_KEY"]
async fn live_openai_unsupported_parameter_is_learned() {
    let (Ok(base_url), Ok(api_key)) = (
        std::env::var("ROUTECTL_LIVE_BASE_URL"),
        std::env::var("ROUTECTL_LIVE_API_KEY"),
    ) else {
        panic!("set ROUTECTL_LIVE_BASE_URL and ROUTECTL_LIVE_API_KEY to run the live smoke");
    };

    let mut providers = BTreeMap::new();
    providers.insert(
        "live".to_string(),
        ProviderEntry::openai_compat(&base_url, common::file_ref(&api_key)),
    );
    let mut models = BTreeMap::new();
    models.insert("m_live".to_string(), ModelEntry::new("live", "gpt-4o-mini"));
    let mut aliases = BTreeMap::new();
    aliases.insert("live".to_string(), AliasValue::Single("m_live".to_string()));

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: fast_retry(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = 48;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);

    let d = router
        .complete_with_options(req_with_structured_output("live"), RouterOptions::default())
        .await;
    assert!(
        !d.meta.learned_capabilities.is_empty(),
        "a real upstream unsupported-parameter 400 must produce a learn event: {:?}",
        d.result,
    );
}
