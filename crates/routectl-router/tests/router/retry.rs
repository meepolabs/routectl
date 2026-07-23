//! Tier 1: request timeouts, per-attempt jitter, and per-error retry budgets.

use super::*;

#[tokio::test]
async fn retry_on_429_overrides_max_attempts() {
    // max_attempts = 1, but retry_on_429 = 4 -> 429 retries 4x.
    let p = MockProvider::new(
        "p",
        vec![
            Behavior::Status(429),
            Behavior::Status(429),
            Behavior::Status(429),
            Behavior::Ok,
        ],
    );
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_on_429 = Some(4);
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "p".into(), "m".into())],
        vec![("p".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
    let resp = r.complete(req("fast")).await.expect("ok after retries");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(p.calls(), 4);
}

#[tokio::test]
async fn retry_on_5xx_independent_of_429() {
    // 5xx retries get 1 attempt; 429 retries get 5. A 5xx run that
    // exhausts its budget falls through.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503), Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 5;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_on_5xx = Some(1);
    rp.retry_on_429 = Some(5);
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
    let resp = r.complete(req("fast")).await.expect("eventually ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 made one attempt (per retry_on_5xx=1), then fell through.
    assert_eq!(p1.calls(), 1);
}

#[tokio::test]
async fn request_timeout_translates_to_network_error_and_retries() {
    use std::time::Duration;
    // Provider that sleeps before completing; timeout fires first.
    struct SlowProvider {
        id: String,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for SlowProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            // First two calls hang past the timeout; third returns fast.
            if n < 2 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(ok_response(&self.id, "m"))
        }
        async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }
    let p = Arc::new(SlowProvider {
        id: "slow".into(),
        calls: AtomicUsize::new(0),
    });
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_on_network = Some(3);
    rp.request_timeout_ms = Some(20);
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "slow".into(), "m".into())],
        vec![("slow".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
    let resp = r.complete(req("fast")).await.expect("eventually ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("slow"));
    // Two timeouts retried, then a fast OK.
    assert_eq!(p.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn per_attempt_jitter_does_not_break_retries() {
    // Smoke test: jitter_ms > 0 doesn't crash the retry loop.
    let p = MockProvider::new("p", vec![Behavior::Status(503), Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 2;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.jitter_ms = 5;
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "p".into(), "m".into())],
        vec![("p".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(p.calls(), 2);
}
