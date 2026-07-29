//! Availability-probe seat failover: a `max_tokens=1` probe that draws a
//! 429/529 from one seat of a pooled model must fail over to a SIBLING seat
//! of the SAME pool (which may still have quota) before fast-failing. It
//! still fast-fails once every seat in the pool is exhausted, and it still
//! fast-fails across DISTINCT chain targets (walking a shared-limit chain is
//! futile). Complete and stream paths are pinned symmetrically.

use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, SeatSelection};
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use routectl_core::Result;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Seat provider that always returns the configured rate-limit/overload
/// status on both complete and stream-open, counting calls.
struct RateLimitedSeat {
    id: String,
    status: u16,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RateLimitedSeat {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, self.status, "rate limited"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, self.status, "rate limited"))
    }
}

/// Seat provider that serves a healthy response on both paths, counting calls.
struct HealthySeat {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for HealthySeat {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            id: format!("ok-{}", self.id),
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
            upstream_meta: None,
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let chunk = ChatChunk {
            id: format!("ok-{}", self.id),
            ..Default::default()
        };
        Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
    }
}

/// Build a single pooled `opus` model with two FillFirst seats: the default
/// seat backed by `default_provider`, seat-b backed by `seat_b_provider`.
/// FillFirst walks the default seat first, then seat-b -- so seat-b is the
/// sibling a probe fails over to.
fn pooled_two_seat_router(
    default_provider: Arc<dyn Provider>,
    seat_b_provider: Arc<dyn Provider>,
) -> Router {
    let seats = vec![
        SeatTarget {
            label: None,
            state_key: crate::seat_pool::seat_state_key("opus", None),
            provider: default_provider.clone(),
            auth_secret_ref: None,
        },
        SeatTarget {
            label: Some("seat-b".into()),
            state_key: crate::seat_pool::seat_state_key("opus", Some("seat-b")),
            provider: seat_b_provider,
            auth_secret_ref: None,
        },
    ];

    let mut providers = BTreeMap::new();
    let runtime = ProviderRuntimePolicy {
        seat_selection: SeatSelection::FillFirst,
        ..Default::default()
    };
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic").with_runtime(runtime),
    );
    let cfg = Arc::new(Config {
        providers,
        ..Config::default()
    });

    let mut router = Router::new(cfg);
    let model = ResolvedModel::new("opus", "anthropic", default_provider, "claude-opus")
        .with_seats(Arc::from(seats));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

fn probe_req(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.into(),
        messages: vec![].into(),
        max_tokens: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn complete_probe_fails_over_to_sibling_seat_on_429() {
    // seatA (default) is rate-limited, seatB has quota. A probe must NOT
    // fast-fail on seatA's 429 -- it must hop to seatB and succeed.
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let seat_a: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-a".into(),
        status: 429,
        calls: a_calls.clone(),
    });
    let seat_b: Arc<dyn Provider> = Arc::new(HealthySeat {
        id: "seat-b".into(),
        calls: b_calls.clone(),
    });
    let router = pooled_two_seat_router(seat_a, seat_b);

    let resp = router
        .complete(probe_req("opus"))
        .await
        .expect("probe must fail over to the healthy sibling seat");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        a_calls.load(Ordering::SeqCst),
        1,
        "seatA drew the 429 probe"
    );
    assert_eq!(b_calls.load(Ordering::SeqCst), 1, "seatB served the probe");
}

#[tokio::test]
async fn stream_probe_fails_over_to_sibling_seat_on_529() {
    // Stream-path twin: seatA overloads (529), seatB serves the first chunk.
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let seat_a: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-a".into(),
        status: 529,
        calls: a_calls.clone(),
    });
    let seat_b: Arc<dyn Provider> = Arc::new(HealthySeat {
        id: "seat-b".into(),
        calls: b_calls.clone(),
    });
    let router = pooled_two_seat_router(seat_a, seat_b);

    let mut stream = router
        .stream(probe_req("opus"))
        .await
        .expect("stream probe must fail over to the healthy sibling seat");
    let first = stream.next().await.expect("a first chunk");
    assert!(first.is_ok(), "the sibling seat delivered a chunk");
    assert_eq!(
        a_calls.load(Ordering::SeqCst),
        1,
        "seatA drew the 529 probe"
    );
    assert_eq!(b_calls.load(Ordering::SeqCst), 1, "seatB served the probe");
}

#[tokio::test]
async fn complete_probe_fast_fails_when_all_pool_seats_exhausted() {
    // Both seats rate-limited: the probe hops seatA -> seatB, then fast-fails
    // once the pool is exhausted (no further sibling) rather than looping.
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let seat_a: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-a".into(),
        status: 429,
        calls: a_calls.clone(),
    });
    let seat_b: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-b".into(),
        status: 429,
        calls: b_calls.clone(),
    });
    let router = pooled_two_seat_router(seat_a, seat_b);

    let err = router
        .complete(probe_req("opus"))
        .await
        .expect_err("an exhausted pool must fast-fail");
    match err {
        Error::Upstream { status, .. } => assert_eq!(status, 429, "surfaces the real 429"),
        other => panic!("expected an upstream 429, got {other:?}"),
    }
    assert_eq!(a_calls.load(Ordering::SeqCst), 1, "seatA tried once");
    assert_eq!(b_calls.load(Ordering::SeqCst), 1, "seatB tried once");
}

#[tokio::test]
async fn stream_probe_fast_fails_when_all_pool_seats_exhausted() {
    // Stream twin of the exhausted-pool fast-fail.
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let seat_a: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-a".into(),
        status: 429,
        calls: a_calls.clone(),
    });
    let seat_b: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "seat-b".into(),
        status: 429,
        calls: b_calls.clone(),
    });
    let router = pooled_two_seat_router(seat_a, seat_b);

    let err = router
        .stream(probe_req("opus"))
        .await
        .err()
        .expect("an exhausted pool must fast-fail on the stream path");
    match err {
        Error::Upstream { status, .. } => assert_eq!(status, 429, "surfaces the real 429"),
        other => panic!("expected an upstream 429, got {other:?}"),
    }
    assert_eq!(a_calls.load(Ordering::SeqCst), 1, "seatA tried once");
    assert_eq!(b_calls.load(Ordering::SeqCst), 1, "seatB tried once");
}

/// Build a two-entry chain `flow = [poolModel, other]` of DISTINCT models,
/// each single-seat non-pooled. Used to pin that a probe fast-fail does NOT
/// hop across distinct chain targets.
fn distinct_target_chain_router(
    first_provider: Arc<dyn Provider>,
    second_provider: Arc<dyn Provider>,
) -> Router {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    let mut config = Config {
        providers,
        ..Config::default()
    };
    config.aliases.insert(
        "flow".into(),
        AliasValue::Chain(vec!["m1".into(), "m2".into()]),
    );

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".into(),
        Arc::new(ResolvedModel::new(
            "m1",
            "anthropic",
            first_provider,
            "wire-1",
        )),
    );
    models.insert(
        "m2".into(),
        Arc::new(ResolvedModel::new(
            "m2",
            "anthropic",
            second_provider,
            "wire-2",
        )),
    );
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn complete_probe_does_not_fall_over_to_distinct_chain_target() {
    // A probe fast-fails ACROSS distinct chain entries: m1's 429 must NOT
    // reach m2, even though m2 is healthy. The failover is same-pool only.
    let m1_calls = Arc::new(AtomicUsize::new(0));
    let m2_calls = Arc::new(AtomicUsize::new(0));
    let m1: Arc<dyn Provider> = Arc::new(RateLimitedSeat {
        id: "m1".into(),
        status: 429,
        calls: m1_calls.clone(),
    });
    let m2: Arc<dyn Provider> = Arc::new(HealthySeat {
        id: "m2".into(),
        calls: m2_calls.clone(),
    });
    let router = distinct_target_chain_router(m1, m2);

    let err = router
        .complete(probe_req("flow"))
        .await
        .expect_err("a probe must fast-fail rather than walk to a distinct target");
    match err {
        Error::Upstream { status, .. } => assert_eq!(status, 429),
        other => panic!("expected an upstream 429, got {other:?}"),
    }
    assert_eq!(m1_calls.load(Ordering::SeqCst), 1, "m1 drew the 429 probe");
    assert_eq!(
        m2_calls.load(Ordering::SeqCst),
        0,
        "the distinct target m2 must never be reached by a probe fast-fail"
    );
}
