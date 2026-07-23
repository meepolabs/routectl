//! Core complete/stream dispatch and fallback-chain behavior.

use super::*;

#[tokio::test]
async fn complete_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m1".into()),
            ("m2".into(), "p2".into(), "m2".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn complete_falls_back_on_5xx() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_falls_back_on_429() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(429)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_does_not_fall_back_on_4xx_when_class_pins_no_fallback() {
    // Post raw-status retirement, fallback is decided per failure class.
    // A bare 400 (BadRequest) falls back by baked default; an operator
    // pins it terminal with `[retry.classes.bad-request] fallback = false`,
    // the replacement for the retired raw allow/deny lists.
    use routectl_router::class_policy::{ClassPolicy, ConfigFailureClass};
    let p1 = MockProvider::new("p1", vec![Behavior::Status(400)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);

    let mut retry = default_test_retry();
    retry.classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        retry,
    );

    let err = r
        .complete(req("fast"))
        .await
        .expect_err("400 should propagate");
    assert!(matches!(err, Error::Upstream { status: 400, .. }));
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn complete_all_providers_fail_returns_last_error() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Status(502)]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
    );

    let err = r.complete(req("fast")).await.expect_err("all-fail");
    assert!(matches!(err, Error::Upstream { status: 502, .. }));
}

#[tokio::test]
async fn complete_unknown_alias_errors() {
    let r = build_router_v6(BTreeMap::new(), vec![], vec![]);
    let err = r.complete(req("nothing")).await.expect_err("unknown alias");
    assert!(matches!(err, Error::UnknownAlias(_)));
}

#[tokio::test]
async fn complete_direct_nickname_target_works() {
    // v0.6.0: the wire model can be a direct `[models]` table key,
    // bypassing the `[aliases]` table. (This replaces the old
    // `provider:model` literal escape hatch from v0.5.)
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let r = build_router_v6(
        BTreeMap::new(),
        vec![("m1".into(), "p1".into(), "wire-model".into())],
        vec![("p1".into(), p1 as Arc<dyn Provider>)],
    );
    let resp = r.complete(req("m1")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    // Default flip: the response echoes the client's requested model
    // (`m1`), not the served upstream wire id (`wire-model`).
    assert_eq!(resp.model, "m1");
}

#[tokio::test]
async fn complete_retries_within_provider_then_falls_back() {
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 3;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 3);
}

#[tokio::test]
async fn stream_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2);
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn stream_falls_back_when_first_chunk_errors() {
    let p1 = MockProvider::new("p1", vec![Behavior::StreamFirstChunkErrors(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2, "p2 should have been used");
}

#[tokio::test]
async fn stream_falls_back_when_first_chunk_is_anthropic_overloaded() {
    let p1 = MockProvider::new("p1", vec![Behavior::StreamFirstChunkOverloaded]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2, "client should see the fallback target's chunks");
    assert_eq!(p1.calls(), 1, "overloaded target dispatched once");
    assert_eq!(p2.calls(), 1, "fallback target dispatched once");
}

#[tokio::test]
async fn stream_falls_back_when_open_stream_call_fails() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
    );

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2);
}

#[tokio::test]
async fn stream_propagates_mid_stream_error_no_fallback() {
    let p1 = MockProvider::new("p1", vec![Behavior::StreamMidErrors]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
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

    let mut s = r.stream(req("fast")).await.expect("ok");
    let first = s.next().await.expect("first chunk").expect("ok");
    let _ = first;
    let second = s.next().await.expect("second item");
    assert!(matches!(second, Err(Error::Streaming(_))));
    // p2 was never used because we already started streaming from p1.
    assert_eq!(p2.calls(), 0);
}

/// Before forward-compat handling, an Anthropic SSE stream containing
/// an unknown `content_block` type (e.g. `server_tool_use`) crashed
/// deserialization with `Error::Streaming`; the router's
/// `should_fallback` returned true and the chain walked across
/// providers, multiplying upstream calls for a local forward-compat
/// bug (production logs showed 11+ retries / 3 minutes). With the
/// catchall + sink-drain plus opaque-event replay in place,
/// unknown variants travel through the canonical chunk
/// stream as `opaque_events` payload and the router never sees
/// `Error::Streaming`. This test pins the router-side regression
/// gate: a streaming response carrying opaque events completes
/// cleanly and the backstop provider is NEVER touched.
#[tokio::test]
async fn stream_with_unknown_anthropic_block_does_not_walk_chain() {
    // Arrange: chain with primary + backstop. Primary emits an
    // opaque-only chunk (server_tool_use start/stop) followed by a
    // normal text chunk. Backstop is wired with `Ok` behavior so a
    // regression that walks the chain produces visible call-count
    // drift instead of a confusingly-empty failure.
    let primary = MockProvider::new("primary", vec![Behavior::StreamWithOpaqueEvents]);
    let backstop = MockProvider::new("backstop", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "primary".into(), "m".into()),
            ("m2".into(), "backstop".into(), "m".into()),
        ],
        vec![
            ("primary".into(), primary.clone() as Arc<dyn Provider>),
            ("backstop".into(), backstop.clone() as Arc<dyn Provider>),
        ],
    );

    // Act: drain the stream to completion. Each item must be Ok --
    // a single Err here means the regression is back.
    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("opaque-event chunks must not surface as Err");
        count += 1;
    }

    // Assert: stream completed without error, primary served the
    // entire response, and the backstop was never reached.
    assert_eq!(count, 2, "expected opaque-only chunk + text chunk");
    assert_eq!(primary.calls(), 1, "primary should be called exactly once");
    assert_eq!(
        backstop.calls(),
        0,
        "backstop must NEVER be called -- chain-walk regression gate",
    );
}
