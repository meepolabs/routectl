//! Response `model` label: default flip + `reported_model` override.

use super::*;

#[tokio::test]
async fn complete_default_echoes_client_alias_not_upstream() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", None, p1);
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.model, "fast", "default flip echoes the client alias");
}

#[tokio::test]
async fn complete_reported_model_override_wins() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", Some("public-label"), p1);
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.model, "public-label", "override wins over the alias");
}

#[tokio::test]
async fn complete_empty_reported_model_falls_through_to_alias() {
    // Some("") is treated as unset: fall through to req.model.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", Some(""), p1);
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.model, "fast", "empty override is unset");
}

#[tokio::test]
async fn complete_label_does_not_disturb_internal_meta() {
    // Regression: resp.model is the client alias while DispatchMeta still
    // records the real served upstream; routectl_provider unchanged.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1"],
        vec![("m1".into(), "p1".into(), "up1".into())],
        vec![("p1".into(), p1 as Arc<dyn Provider>)],
        default_test_retry(),
    );
    let Dispatched { meta, result } = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;
    let resp = result.expect("ok");
    assert_eq!(resp.model, "fast", "client-visible label is the alias");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(meta.served_model.as_deref(), Some("m1"));
    assert_eq!(
        meta.served_upstream.as_deref(),
        Some("up1"),
        "internal accounting keeps the real upstream"
    );
}

#[tokio::test]
async fn complete_fallback_chain_carries_alias_label() {
    // First entry 503s, second serves; no reported_model on either.
    // The response label is the client alias regardless of which entry
    // wins.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.model, "fast", "served-by-fallback still echoes alias");
}

/// Provider whose stream ends with a usage-only terminal chunk (no
/// delta), exercising the rewrite of the terminal chunk.
struct UsageTailProvider {
    id: String,
}

#[async_trait]
impl Provider for UsageTailProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let id = self.id.clone();
        let text = ok_chunk(&id, &req.model, "hi");
        let tail = ChatChunk {
            id: format!("chunk-{id}-tail"),
            model: req.model,
            choices: Vec::new(),
            usage: Some(routectl_core::UsageDelta::default()),
            opaque_events: Vec::new(),
            upstream_meta: None,
        };
        Ok(futures::stream::iter(vec![Ok(text), Ok(tail)]).boxed())
    }
}

#[tokio::test]
async fn stream_default_rewrites_every_chunk_including_terminal() {
    let p1 = Arc::new(UsageTailProvider { id: "p1".into() }) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", None, p1);
    let mut s = r.stream(req("fast")).await.expect("stream ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let chunk = item.expect("ok chunk");
        assert_eq!(chunk.model, "fast", "every chunk echoes the alias");
        count += 1;
    }
    assert_eq!(count, 2, "text chunk + terminal usage-only chunk");
}

#[tokio::test]
async fn stream_reported_model_override_rewrites_every_chunk() {
    let p1 = Arc::new(UsageTailProvider { id: "p1".into() }) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", Some("public-label"), p1);
    let mut s = r.stream(req("fast")).await.expect("stream ok");
    while let Some(item) = s.next().await {
        let chunk = item.expect("ok chunk");
        assert_eq!(chunk.model, "public-label");
    }
}

/// Provider whose stream yields one Ok chunk then a mid-stream error,
/// to confirm the error propagates byte-for-byte through the relabel map.
struct OkThenErrProvider {
    id: String,
}

#[async_trait]
impl Provider for OkThenErrProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let id = self.id.clone();
        let first = ok_chunk(&id, &req.model, "first");
        let err = Error::Streaming("mid-stream-boom".into());
        Ok(futures::stream::iter(vec![Ok(first), Err(err)]).boxed())
    }
}

#[tokio::test]
async fn stream_relabel_passes_mid_stream_error_through_unchanged() {
    let p1 = Arc::new(OkThenErrProvider { id: "p1".into() }) as Arc<dyn Provider>;
    let r = router_with_reported_model("wire-model", None, p1);
    let mut s = r.stream(req("fast")).await.expect("stream ok");
    let first = s.next().await.expect("first item").expect("ok chunk");
    assert_eq!(first.model, "fast");
    let second = s.next().await.expect("second item");
    match second {
        Err(Error::Streaming(msg)) => assert_eq!(msg, "mid-stream-boom"),
        other => panic!("expected unchanged Streaming error, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_fallback_chain_carries_one_stable_alias_label() {
    // First entry fails to open the stream; second serves. Every chunk
    // carries the single client alias label (no reported_model set).
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );
    let mut s = r.stream(req("fast")).await.expect("stream ok");
    while let Some(item) = s.next().await {
        let chunk = item.expect("ok chunk");
        assert_eq!(chunk.model, "fast", "one stable label across all chunks");
    }
}
