//! The `default` alias key (v0.6.0 replacement for v0.5's `default_model`).

use super::*;

#[tokio::test]
async fn default_alias_routes_unknown_model_to_default_chain() {
    // Client sends a model name that's not in [aliases] and isn't a
    // direct nickname. With aliases."default" pointing at "m1", the
    // request should land on m1's provider.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("m1".into()));
    let r = build_router_v6(
        aliases,
        vec![("m1".into(), "p1".into(), "m1".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
    );

    let resp = r
        .complete(req("claude-future-model-99-20300101"))
        .await
        .expect("default alias must route unknown model");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
}

#[tokio::test]
async fn default_alias_echoes_client_model_and_preserves_upstream() {
    // The `default` flip routes an UNKNOWN client model to the default
    // chain, but the client-visible label must still echo what the
    // caller asked for (the default flip changes routing, not the echoed
    // label). Meanwhile the served entry's REAL upstream wire id stays
    // in DispatchMeta.served_upstream -- internal truth is intact even
    // though the client sees its own string back.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("m1".into()));
    let r = build_router_v6(
        aliases,
        // m1's real upstream wire id differs from any client string.
        vec![("m1".into(), "p1".into(), "wire-model-internal".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
    );

    let unknown = "claude-future-model-99-20300101";
    let dispatched = r
        .complete_with_options(req(unknown), RouterOptions::default())
        .await;
    let resp = dispatched
        .result
        .expect("default alias must route unknown model");

    // Client-visible label: the default flip echoes the client's
    // requested string, NOT the resolved nickname or the upstream id.
    assert_eq!(resp.model, unknown);
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    // Internal truth: the served entry's real upstream is preserved.
    assert_eq!(
        dispatched.meta.served_upstream.as_deref(),
        Some("wire-model-internal")
    );
    assert_eq!(dispatched.meta.served_model.as_deref(), Some("m1"));
}

#[tokio::test]
async fn default_alias_does_not_override_explicit_alias() {
    // When the requested model IS itself a configured alias key,
    // `default` must NOT preempt it.
    let p_fast = MockProvider::new("p_fast", vec![Behavior::Ok]);
    let p_slow = MockProvider::new("p_slow", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("m_fast".into()));
    aliases.insert("slow".into(), AliasValue::Single("m_slow".into()));
    aliases.insert("default".into(), AliasValue::Single("m_slow".into()));
    let r = build_router_v6(
        aliases,
        vec![
            ("m_fast".into(), "p_fast".into(), "m".into()),
            ("m_slow".into(), "p_slow".into(), "m".into()),
        ],
        vec![
            ("p_fast".into(), p_fast.clone() as Arc<dyn Provider>),
            ("p_slow".into(), p_slow.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p_fast"));
    assert_eq!(p_fast.calls(), 1);
    assert_eq!(
        p_slow.calls(),
        0,
        "default alias must not override an explicit alias hit"
    );
}

#[tokio::test]
async fn default_alias_does_not_override_direct_nickname() {
    // A direct `[models]` nickname must continue to bypass alias
    // resolution; `default` never enters the picture for it.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p_default = MockProvider::new("p_default", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("m_default".into()));
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m_default".into(), "p_default".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p_default".into(), p_default.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("m1")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
    assert_eq!(p_default.calls(), 0);
}

#[tokio::test]
async fn stream_empty_first_provider_falls_back() {
    // A provider whose `stream()` returns Ok but yields zero chunks
    // before EOS must NOT be reported as a successful empty stream.
    // Pre-fix the router treated this as `Ok(empty().boxed())` and
    // the breaker recorded a successful probe for an unhealthy
    // upstream. Now it must surface as a fallbackable streaming
    // error so the chain walks to the next provider AND the breaker
    // records a failure.
    let p1 = MockProvider::new("p1", vec![Behavior::StreamEmpty]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("multi", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let stream = r
        .stream(req("multi"))
        .await
        .expect("router stream() must produce p2's stream after p1 falls back");
    let chunks: Vec<_> = stream.collect().await;
    assert!(
        chunks.iter().any(std::result::Result::is_ok),
        "expected at least one Ok chunk from the fallback provider"
    );
    assert_eq!(p1.calls(), 1, "p1 must have been tried exactly once");
    assert!(p2.calls() >= 1, "p2 must have been called as fallback");
}
