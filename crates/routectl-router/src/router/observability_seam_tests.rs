//! Router-consumer observability at the class-decision point: the
//! stable FeatureUnsupported event, the per-arm class-decision
//! DEBUG/WARN event, and the two RouterMetrics counters -- wired at
//! BOTH dispatch loops. All capture tests run on the `#[tokio::test]`
//! current-thread runtime (the dispatch path never spawns before its
//! error arm), so the thread-local capture subscriber sees every
//! event the arm emits.

use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use async_trait::async_trait;
use routectl_testkit::with_capture;

/// A body string that must NEVER surface in any observability event
/// field or message. Every capture test scans for it.
const SECRET_BODY: &str = "TOP-SECRET-UPSTREAM-BODY-DO-NOT-LOG";

/// A provider whose `complete` / `stream` both fail with a
/// configurable upstream status + classifier tokens, carrying a
/// sentinel body used to prove no body text leaks into the new events.
struct FailingProvider {
    id: String,
    status: u16,
    upstream_type: Option<String>,
    upstream_code: Option<String>,
}

impl FailingProvider {
    fn make_error(&self) -> Error {
        Error::upstream_full(
            &self.id,
            self.status,
            SECRET_BODY,
            None,
            self.upstream_type.clone(),
            self.upstream_code.clone(),
        )
    }
}

#[async_trait]
impl Provider for FailingProvider {
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
        Err(self.make_error())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.make_error())
    }
}

/// Single openai-compat entry `m1 -> p1`, retry capped at one attempt
/// so the failing provider is hit exactly once. The config provider
/// entry exists so the chain expander resolves `provider_kind` to
/// `openai-compat` (used by both the classifier's token table and the
/// FeatureUnsupported event's `provider_kind` field).
fn router_with_failing(status: u16, ty: Option<&str>, code: Option<&str>) -> Router {
    let config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        },
        providers: {
            let mut m = BTreeMap::new();
            m.insert(
                "p1".to_string(),
                ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
            );
            m
        },
        ..Config::default()
    };
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(FailingProvider {
        id: "p1".into(),
        status,
        upstream_type: ty.map(str::to_string),
        upstream_code: code.map(str::to_string),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

fn req_m1() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// The sentinel upstream body must not appear in any event emitted by
/// the NEW observability seam (the FeatureUnsupported event and the
/// per-arm class-decision DEBUG/WARN). Pre-existing dispatch logs that
/// render `error = ?e` are out of scope by design and left untouched.
fn assert_no_body_leak_in_seam(events: &[routectl_testkit::CapturedEvent]) {
    let is_seam = |e: &&routectl_testkit::CapturedEvent| {
        e.target == "routectl::feature_unsupported"
            || e.message == "router failure class decision"
            || e.message == "unknown failure classification on upstream outcome (fail-closed)"
    };
    let seam: Vec<_> = events.iter().filter(is_seam).collect();
    assert!(!seam.is_empty(), "expected at least one seam event");
    for e in seam {
        assert!(
            !e.message.contains(SECRET_BODY),
            "body leaked into seam message: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(SECRET_BODY),
                "body leaked into seam field {k}: {v}"
            );
        }
    }
}

#[tokio::test]
async fn feature_unsupported_event_fires_on_complete_with_safe_fields() {
    // Arrange: openai-compat 400 carrying `unsupported_parameter` on
    // error.code lifts to FeatureUnsupported.
    let router = router_with_failing(400, None, Some("unsupported_parameter"));

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: the request still fails (event is observational only).
    assert!(result.is_err());
    let ev = events
        .iter()
        .find(|e| e.target == "routectl::feature_unsupported")
        .expect("feature_unsupported event must fire");
    assert_eq!(ev.level, tracing::Level::INFO);
    assert_eq!(ev.field("provider"), Some("p1"));
    assert_eq!(ev.field("provider_kind"), Some("openai-compat"));
    assert_eq!(ev.field("model"), Some("m1"));
    assert_eq!(ev.field("capability"), Some("unsupported_parameter"));
    assert_eq!(ev.field("status"), Some("400"));
    assert_eq!(ev.field("upstream_type"), Some(""));
    assert_eq!(ev.field("upstream_code"), Some("unsupported_parameter"));
    assert_eq!(ev.field("matched_by"), Some("upstream_type"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("is_forwarded"), Some("false"));
    assert_eq!(
        ev.field("remapped"),
        Some("false"),
        "a real upstream lift is not an operator remap"
    );

    assert_no_body_leak_in_seam(&events);
    assert_eq!(router.metrics.feature_unsupported_total(), 1);
    assert_eq!(router.metrics.unknown_failure_classifications_total(), 0);
}

#[tokio::test]
async fn feature_unsupported_event_fires_on_stream_surface() {
    // Arrange
    let router = router_with_failing(400, None, Some("unsupported_parameter"));

    // Act: the pre-first-chunk error rides the stream error arm.
    let (result, events) = with_capture(Box::pin(router.stream(req_m1()))).await;

    // Assert
    assert!(result.is_err());
    let ev = events
        .iter()
        .find(|e| e.target == "routectl::feature_unsupported")
        .expect("feature_unsupported event must fire on the stream loop");
    assert_eq!(ev.field("surface"), Some("stream"));
    assert_eq!(ev.field("capability"), Some("unsupported_parameter"));
    assert_no_body_leak_in_seam(&events);
    assert_eq!(router.metrics.feature_unsupported_total(), 1);
}

#[tokio::test]
async fn unknown_upstream_classification_warns_and_counts_on_complete() {
    // Arrange: status 600 is outside every mapped row -> Unknown by
    // status, on a genuine Error::Upstream (fail-closed unknown).
    let router = router_with_failing(600, None, None);

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert
    assert!(result.is_err());
    let ev = events
        .iter()
        .find(|e| e.message == "unknown failure classification on upstream outcome (fail-closed)")
        .expect("unknown-upstream decision must WARN");
    assert_eq!(ev.level, tracing::Level::WARN);
    assert_eq!(ev.field("effective_class"), Some("unknown"));
    assert_eq!(ev.field("original_class"), Some("unknown"));
    assert_eq!(ev.field("remapped"), Some("false"));
    assert_eq!(ev.field("matched_by"), Some("status"));
    assert_eq!(ev.field("status"), Some("Some(600)"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("fallback"), Some("false"));
    assert_eq!(ev.field("debit"), Some("false"));

    assert_no_body_leak_in_seam(&events);
    assert_eq!(router.metrics.unknown_failure_classifications_total(), 1);
    assert_eq!(router.metrics.feature_unsupported_total(), 0);
}

#[tokio::test]
async fn unknown_upstream_classification_warns_and_counts_on_stream() {
    // Arrange
    let router = router_with_failing(600, None, None);

    // Act
    let (result, events) = with_capture(Box::pin(router.stream(req_m1()))).await;

    // Assert
    assert!(result.is_err());
    let ev = events
        .iter()
        .find(|e| e.message == "unknown failure classification on upstream outcome (fail-closed)")
        .expect("unknown-upstream decision must WARN on the stream loop");
    assert_eq!(ev.level, tracing::Level::WARN);
    assert_eq!(ev.field("surface"), Some("stream"));
    assert_no_body_leak_in_seam(&events);
    assert_eq!(router.metrics.unknown_failure_classifications_total(), 1);
}

#[tokio::test]
async fn generic_bad_request_emits_single_debug_decision() {
    // Arrange: a generic 400 stays BadRequest -- exercises the DEBUG
    // (non-WARN, non-feature) class-decision path and its field set.
    let router = router_with_failing(400, Some("invalid_request_error"), None);

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: exactly one class-decision event per error-arm pass.
    assert!(result.is_err());
    let decisions: Vec<_> = events
        .iter()
        .filter(|e| e.message == "router failure class decision")
        .collect();
    assert_eq!(decisions.len(), 1, "one decision event per error-arm pass");
    let ev = decisions[0];
    assert_eq!(ev.level, tracing::Level::DEBUG);
    assert_eq!(ev.field("effective_class"), Some("bad_request"));
    assert_eq!(ev.field("original_class"), Some("bad_request"));
    assert_eq!(ev.field("remapped"), Some("false"));
    assert_eq!(ev.field("matched_by"), Some("status"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("fallback"), Some("true"));
    assert_eq!(ev.field("debit"), Some("false"));
    assert_eq!(ev.field("retry_cap"), Some("0"));
    assert_eq!(ev.field("is_probe"), Some("false"));
    assert_eq!(ev.field("is_forwarded"), Some("false"));

    assert_no_body_leak_in_seam(&events);
    assert_eq!(router.metrics.feature_unsupported_total(), 0);
    assert_eq!(router.metrics.unknown_failure_classifications_total(), 0);
}

#[test]
fn label_helpers_map_stable_tokens() {
    assert_eq!(class_label(&FailureClass::RateLimited), "rate_limited");
    assert_eq!(class_label(&FailureClass::Unknown), "unknown");
    assert_eq!(
        class_label(&FailureClass::FeatureUnsupported {
            capability: "x".into()
        }),
        "feature_unsupported"
    );
    assert_eq!(matched_by_label(MatchedBy::Variant), "variant");
    assert_eq!(matched_by_label(MatchedBy::Status), "status");
    assert_eq!(matched_by_label(MatchedBy::UpstreamType), "upstream_type");
}
