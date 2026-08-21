// Coverage for the operator-configured Cloud Code project id: config-first
// resolution, compare-before-put write-through, and the one-way
// rejection latch. `include!`d into the `e2e_tests` module in `mod.rs`; all
// top-level imports live there, so do not add `use` lines here.

/// `CloudProjectCache` that counts `put` calls. Compare-before-put is a
/// disk-write economy on the real (OAuth-store) cache, so the count is the
/// only observable that pins it.
#[derive(Debug)]
struct CountingPutCache {
    inner: InMemoryProjectCache,
    puts: std::sync::atomic::AtomicUsize,
}

impl CountingPutCache {
    fn empty() -> Self {
        Self {
            inner: InMemoryProjectCache::new(),
            puts: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn seeded(project_id: &str) -> Self {
        Self {
            inner: InMemoryProjectCache::with(project_id),
            puts: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn put_count(&self) -> usize {
        self.puts.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl CloudProjectCache for CountingPutCache {
    async fn get(&self) -> Option<String> {
        self.inner.get().await
    }

    async fn put(&self, project_id: String) -> Result<()> {
        self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.put(project_id).await
    }

    async fn clear_if_matches(&self, expected: &str) -> Result<bool> {
        self.inner.clear_if_matches(expected).await
    }
}

/// `CloudProjectCache` whose persistence always fails -- the shape of a
/// credentials-store disk write that cannot land.
#[derive(Debug)]
struct FailingPutCache;

#[async_trait]
impl CloudProjectCache for FailingPutCache {
    async fn get(&self) -> Option<String> {
        None
    }

    async fn put(&self, _project_id: String) -> Result<()> {
        Err(Error::Internal("project cache write failed".into()))
    }

    async fn clear_if_matches(&self, _expected: &str) -> Result<bool> {
        Ok(false)
    }
}

const CONFIGURED_PROJECT: &str = "cfg-proj-1";

fn make_configured_provider(
    base_url: &str,
    cache: Arc<dyn CloudProjectCache>,
    configured: Option<&str>,
) -> GeminiProvider {
    let auth: Arc<dyn TokenSource> = Arc::new(StaticToken::new(CLOUD_CODE_TOKEN));
    let mut cfg = GeminiConfig::new_cloud_code("gemini:test", auth, cache);
    cfg.base_url = base_url.to_string();
    cfg.onboard_poll_interval = std::time::Duration::from_millis(1);
    cfg.cloud_project_id = configured.map(str::to_string);
    GeminiProvider::new(cfg)
}

/// Any discovery call at all is a failure for the configured-id path, so
/// mount both endpoints as forbidden rather than asserting on one.
async fn forbid_discovery(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(LOAD_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(ONBOARD_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

async fn mount_generate_ok_for(server: &MockServer, project: &str) {
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .and(body_partial_json(json!({"project": project})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"response": gemini_ok_response()})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn configured_project_id_serves_a_cold_request_without_discovery() {
    // Arrange: cold cache, so discovery is what an unconfigured entry would
    // do -- the configured id must make it unnecessary, not merely redundant.
    let server = MockServer::start().await;
    forbid_discovery(&server).await;
    mount_generate_ok_for(&server, CONFIGURED_PROJECT).await;
    let cache = Arc::new(CountingPutCache::empty());
    let provider = make_configured_provider(&server.uri(), cache.clone(), Some(CONFIGURED_PROJECT));

    // Act
    let resp = provider
        .complete(base_req())
        .await
        .expect("the configured project id must serve the request");

    // Assert: the generate mock's body matcher pins that the envelope
    // carried the configured id; the forbidden mocks pin zero discovery.
    assert_eq!(resp.id, "resp-abc");
    assert_eq!(
        cache.get().await.as_deref(),
        Some(CONFIGURED_PROJECT),
        "the configured id must write through to the seat's cache"
    );
    assert_eq!(cache.put_count(), 1, "the write-through must have happened");
}

#[tokio::test]
async fn configured_project_id_already_cached_skips_the_write_through() {
    // Arrange
    let server = MockServer::start().await;
    forbid_discovery(&server).await;
    mount_generate_ok_for(&server, CONFIGURED_PROJECT).await;
    let cache = Arc::new(CountingPutCache::seeded(CONFIGURED_PROJECT));
    let provider = make_configured_provider(&server.uri(), cache.clone(), Some(CONFIGURED_PROJECT));

    // Act
    provider.complete(base_req()).await.expect("request serves");

    // Assert
    assert_eq!(
        cache.put_count(),
        0,
        "an equal cached value must not be rewritten"
    );
}

#[tokio::test]
async fn configured_project_id_write_through_failure_still_serves_the_request() {
    // Arrange: persisting the id is an optimization for later requests, not
    // a precondition for this one.
    let server = MockServer::start().await;
    forbid_discovery(&server).await;
    mount_generate_ok_for(&server, CONFIGURED_PROJECT).await;
    let cache: Arc<dyn CloudProjectCache> = Arc::new(FailingPutCache);
    let provider = make_configured_provider(&server.uri(), cache, Some(CONFIGURED_PROJECT));

    // Act
    let (result, events) = routectl_testkit::with_capture(provider.complete(base_req())).await;

    // Assert
    result.expect("a cache write failure must not fail the request");
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.message.contains("configured cloud project id")),
        "the failed write-through must be reported: {events:?}"
    );
}

#[tokio::test]
async fn unset_configured_project_id_still_discovers_through_loadcodeassist() {
    // Regression pin: leaving the knob unset must keep the pre-existing
    // discovery path exactly as it was.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(LOAD_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"cloudaicompanionProject": "proj-discovered"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_generate_ok_for(&server, "proj-discovered").await;
    let cache = Arc::new(CountingPutCache::empty());
    let provider = make_configured_provider(&server.uri(), cache.clone(), None);

    // Act
    provider.complete(base_req()).await.expect("request serves");

    // Assert
    assert_eq!(cache.get().await.as_deref(), Some("proj-discovered"));
}

#[tokio::test]
async fn rejected_configured_project_id_latches_off_and_the_next_request_rediscovers() {
    // Arrange: the host says the configured id does not apply to this seat.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .and(body_partial_json(json!({"project": CONFIGURED_PROJECT})))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"caller lacks access"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(LOAD_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"cloudaicompanionProject": "proj-fresh"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_generate_ok_for(&server, "proj-fresh").await;
    let cache = Arc::new(CountingPutCache::empty());
    let provider = make_configured_provider(&server.uri(), cache.clone(), Some(CONFIGURED_PROJECT));

    // Act
    let (first, events) = routectl_testkit::with_capture(provider.complete(base_req())).await;

    // Assert: the rejected request surfaces its own error -- the generate
    // mock's `expect(1)` pins that no in-request retry happened.
    match first.expect_err("the rejection must surface") {
        Error::Upstream {
            status,
            upstream_type,
            ..
        } => {
            assert_eq!(status, 403);
            assert_eq!(upstream_type.as_deref(), Some("PERMISSION_DENIED"));
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.field("cloud_project_id") == Some(CONFIGURED_PROJECT)
            && e.message.contains("falling back to rediscovery")),
        "the rejection must name the id and the fallback: {events:?}"
    );

    // Act: the next request must ignore the configured id entirely.
    let resp = provider
        .complete(base_req())
        .await
        .expect("rediscovery must serve the next request");

    // Assert
    assert_eq!(resp.id, "resp-abc");
    assert_eq!(cache.get().await.as_deref(), Some("proj-fresh"));
}

#[tokio::test]
async fn stream_rejected_configured_project_id_latches_without_retrying() {
    // Arrange
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(STREAM_PATH))
        .and(body_partial_json(json!({"project": CONFIGURED_PROJECT})))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"project gone"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let cache = Arc::new(CountingPutCache::empty());
    let provider = make_configured_provider(&server.uri(), cache.clone(), Some(CONFIGURED_PROJECT));

    // Act
    let err = provider
        .stream(base_req())
        .await
        .err()
        .expect("the rejection must surface as an error, not open a stream");

    // Assert
    match err {
        Error::Upstream {
            status,
            upstream_type,
            ..
        } => {
            assert_eq!(status, 403);
            assert_eq!(upstream_type.as_deref(), Some("PERMISSION_DENIED"));
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
    assert!(
        cache.get().await.is_none(),
        "the rejected id must be cleared from the cache"
    );
}

#[tokio::test]
async fn quota_on_the_configured_project_id_leaves_the_latch_clear() {
    // Arrange: a 429 is a quota verdict, not a verdict on the project, so it
    // must neither latch the configured id off nor drop it from the cache.
    let server = MockServer::start().await;
    forbid_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .and(body_partial_json(json!({"project": CONFIGURED_PROJECT})))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota exceeded"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_generate_ok_for(&server, CONFIGURED_PROJECT).await;
    let cache = Arc::new(CountingPutCache::empty());
    let provider = make_configured_provider(&server.uri(), cache.clone(), Some(CONFIGURED_PROJECT));

    // Act
    let err = provider
        .complete(base_req())
        .await
        .expect_err("the quota error must surface");

    // Assert
    match err {
        Error::Upstream {
            status,
            upstream_type,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(upstream_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
    assert_eq!(
        cache.get().await.as_deref(),
        Some(CONFIGURED_PROJECT),
        "a quota verdict must leave the cached project intact"
    );

    // The latch is only observable through behavior: the next request must
    // still take the configured id (and the forbidden discovery mocks pin
    // that it did not fall back).
    provider
        .complete(base_req())
        .await
        .expect("the configured id must still serve after a quota failure");
}
