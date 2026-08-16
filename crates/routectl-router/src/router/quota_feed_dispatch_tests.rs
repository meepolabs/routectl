//! Router-level tests that the post-response quota feed fires on BOTH
//! completion paths against a real dispatch, not just against the feed helper.
//!
//! The STREAMING case is the one that needs a router-level test. Streaming
//! quota rides the first canonical chunk, lifted inside the stream wrapper the
//! dispatch loop installs -- a test of the helper alone proves nothing about
//! whether that wrapper is wired, and a client that streams is the ordinary
//! case. Wiring only the non-streaming path would leave every seat reading as
//! no-evidence with no symptom.

use super::*;

use async_trait::async_trait;
use routectl_core::upstream_meta::{AnthropicUnifiedQuota, UpstreamMeta};
use routectl_core::{Choice, ChunkChoice, ChunkDelta, Message};

use crate::config::ProviderEntry;
use crate::quota::key::SeatKey;
use crate::quota::window::QuotaWindow;

const SEAT_PROVIDER: &str = "anthropic";
const SEAT_LABEL: &str = "seat-b";

/// The Anthropic family with a reset an hour ahead of the wall clock, so it is
/// plausible for the curated five-hour window at the instant the feed stamps.
fn live_quota(utilization: &str) -> AnthropicUnifiedQuota {
    let reset_secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs()
        + 3_600;
    let mut quota = AnthropicUnifiedQuota::default();
    quota.utilization = Some(utilization.to_string());
    quota.extras = vec![("5h-reset".into(), reset_secs.to_string())];
    quota
}

/// Provider that answers both completion paths with quota metadata attached
/// exactly where each path carries it: on the assembled response for
/// non-streaming, and on the FIRST chunk only for streaming.
struct QuotaProvider {
    utilization: String,
    /// Utilization the SECOND and later chunks would carry if a feed read them.
    /// Different from the first so a per-chunk feed is visible as a wrong stored
    /// value rather than as no difference at all.
    later_utilization: String,
}

#[async_trait]
impl Provider for QuotaProvider {
    fn id(&self) -> &'static str {
        "anthropic-quota"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("anthropic-quota", "unused"))
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            id: "resp".into(),
            model: req.model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: Some(UpstreamMeta::from_anthropic_unified(live_quota(
                &self.utilization,
            ))),
        })
    }

    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let first = content_chunk("ok", Some(self.utilization.clone()));
        let second = content_chunk(" more", Some(self.later_utilization.clone()));
        let third = content_chunk(" still", None);
        Ok(futures::stream::iter(vec![Ok(first), Ok(second), Ok(third)]).boxed())
    }
}

/// A content-bearing chunk, optionally carrying quota metadata.
fn content_chunk(text: &str, utilization: Option<String>) -> ChatChunk {
    ChatChunk {
        id: "chunk".into(),
        model: "opus".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(text.to_string()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: vec![],
        upstream_meta: utilization.map(|u| UpstreamMeta::from_anthropic_unified(live_quota(&u))),
    }
}

/// A Router serving one model on a labeled OAuth seat, backed by the quota
/// provider. The seat's `SecretRef` is what both key derivations bottom out in.
fn router_on_one_oauth_seat(utilization: &str, later_utilization: &str) -> Router {
    let provider: Arc<dyn Provider> = Arc::new(QuotaProvider {
        utilization: utilization.to_string(),
        later_utilization: later_utilization.to_string(),
    });
    let mut providers = BTreeMap::new();
    providers.insert(
        SEAT_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(format!("oauth://{SEAT_PROVIDER}#{SEAT_LABEL}")),
    );
    let cfg = Arc::new(Config {
        providers,
        ..Config::default()
    });
    let mut router = Router::new(cfg);
    let model = ResolvedModel::new("opus", SEAT_PROVIDER, provider, "claude-opus-4-7")
        .with_auth_secret_ref(routectl_auth::SecretRef::OAuth {
            provider: SEAT_PROVIDER.to_string(),
            label: Some(SEAT_LABEL.to_string()),
        });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// The served seat's key, derived through the EXPOSED read-side helper -- the
/// same helper the write side bottoms out in. A hand-built key here would pass
/// whichever key the store used, which is the whole failure this feature has to
/// avoid.
fn served_seat_key() -> SeatKey {
    crate::quota::key::seat_key_for_secret_ref(Some(&routectl_auth::SecretRef::OAuth {
        provider: SEAT_PROVIDER.to_string(),
        label: Some(SEAT_LABEL.to_string()),
    }))
    .expect("an oauth ref yields a key")
}

/// The stored FAST fraction for the served seat, or `None` when it reads as no
/// evidence.
fn stored_fast_fraction(router: &Router) -> Option<f64> {
    let reading = router.quota_store.reading_for(
        &served_seat_key(),
        &crate::quota::freshness::ObservationStamp::now(),
    )?;
    match reading.fast {
        QuotaWindow::Known { utilization, .. } => Some(utilization.fraction()),
        QuotaWindow::Unknown => None,
    }
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "opus".into(),
        messages: Arc::from(vec![Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_non_streaming_dispatch_feeds_the_served_seats_reading() {
    let router = router_on_one_oauth_seat("0.21", "0.99");

    router.complete(req()).await.expect("the dispatch succeeds");

    assert_eq!(
        stored_fast_fraction(&router),
        Some(0.21),
        "the reading on the assembled response must reach the store, under the \
         key the read side derives"
    );
}

/// The half a partial job would leave invisible. Streaming quota rides the FIRST
/// chunk, lifted inside the stream wrapper; if that wrapper is not wired, a
/// deployment whose client streams stores nothing and every seat reads as
/// no-evidence with no error and no warning.
#[tokio::test]
async fn a_streaming_dispatch_feeds_from_the_first_chunk() {
    let router = router_on_one_oauth_seat("0.33", "0.99");

    let stream = router.stream(req()).await.expect("the dispatch succeeds");
    let chunks: Vec<_> = stream.collect().await;

    assert_eq!(chunks.len(), 3, "the whole stream reaches the caller");
    assert_eq!(
        stored_fast_fraction(&router),
        Some(0.33),
        "the first chunk's reading must reach the store"
    );
}

/// Exactly once per response. The second chunk carries a DIFFERENT utilization,
/// so a per-chunk feed shows up as the wrong stored value rather than as no
/// observable difference.
#[tokio::test]
async fn a_streaming_dispatch_does_not_re_feed_from_later_chunks() {
    let router = router_on_one_oauth_seat("0.33", "0.99");

    let stream = router.stream(req()).await.expect("the dispatch succeeds");
    let _chunks: Vec<_> = stream.collect().await;

    assert_eq!(
        stored_fast_fraction(&router),
        Some(0.33),
        "a later chunk's metadata must not overwrite the first chunk's reading"
    );
}

/// The reading is available to a reader the moment the stream is consumed, not
/// only after end-of-stream: the feed lifts it as the chunk passes, before the
/// caller renders it.
#[tokio::test]
async fn a_streaming_reading_is_stored_before_the_stream_ends() {
    let router = router_on_one_oauth_seat("0.33", "0.99");

    let mut stream = router.stream(req()).await.expect("the dispatch succeeds");
    let _first = stream.next().await.expect("a first chunk");

    assert_eq!(
        stored_fast_fraction(&router),
        Some(0.33),
        "waiting for end-of-stream would lose a reading the head carried"
    );
}
