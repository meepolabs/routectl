//! Streaming-egress mirror of the learned-capability loop: the SSE mock
//! responder and stream-surface fixtures, plus the stream re-probe / clear
//! mirrors and the early-exit / cancellation matrix that proves the
//! `ProbeAdmissionSet` drop releases every unreached admission's slot.

use super::*;

// ---------------------------------------------------------------------------
// Streaming-egress harness: the SSE mock responder ported from the router
// mock-provider suite, wired onto the SAME wiremock openai-compat egress this
// file already uses so the learned-probe loop runs over `router.stream()`.
//
// A 2xx step serves a byte-accurate `text/event-stream` body the openai-compat
// streaming egress parses into chunks (so `try_stream_with_first_content` yields
// a first chunk and the dispatch returns Ok); a non-2xx step serves the JSON
// error envelope verbatim, so the `provider.stream()` open call fails with the
// SAME real classification the complete path learns from. This keeps the
// learned-capability loop identical across surfaces while exercising the
// streaming dispatch body (`stream_inner`) and its `ProbeAdmissionSet`.
// ---------------------------------------------------------------------------

const PROVIDER_KIND: &str = "openai-compat";

/// A minimal, valid openai-compat SSE success body: one content chunk, one
/// terminal chunk carrying `finish_reason`, then the `[DONE]` sentinel. The
/// egress relabels the model, so the wire model string is a placeholder.
fn sse_ok_body() -> String {
    concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string()
}

/// The streaming sibling of [`SequencedResponder`]: walks a fixed status
/// sequence across calls, serving an SSE body (`text/event-stream`) for a 2xx
/// step and the raw JSON error envelope for a non-2xx step. `success_delay`
/// (applied only to a 2xx step) holds the stream open long enough for a
/// cancellation test to drop the dispatch future before the first chunk lands.
struct StreamSequencedResponder {
    calls: AtomicUsize,
    steps: Vec<(u16, String)>,
    success_delay: Option<Duration>,
}

impl Respond for StreamSequencedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let (status, body) = self.steps.get(i).unwrap_or_else(|| {
            self.steps
                .last()
                .expect("StreamSequencedResponder needs at least one step")
        });
        let is_success = (200..300).contains(status);
        let content_type = if is_success {
            "text/event-stream"
        } else {
            "application/json"
        };
        let mut tpl = ResponseTemplate::new(*status)
            .insert_header("content-type", content_type)
            .set_body_string(body.clone());
        if is_success && let Some(delay) = self.success_delay {
            tpl = tpl.set_delay(delay);
        }
        tpl
    }
}

/// A wiremock upstream that answers `POST /chat/completions` streaming: a 2xx
/// step serves the canonical SSE success body, a non-2xx step serves its JSON
/// envelope. Mirrors [`upstream_server`] for the stream surface.
async fn sse_upstream_server(steps: Vec<(u16, Value)>) -> MockServer {
    sse_upstream_server_delayed(steps, None).await
}

/// [`sse_upstream_server`] with an optional first-byte delay on every 2xx step
/// (the cancellation test uses it to keep the first chunk pending).
async fn sse_upstream_server_delayed(
    steps: Vec<(u16, Value)>,
    success_delay: Option<Duration>,
) -> MockServer {
    let steps = steps
        .into_iter()
        .map(|(status, body)| {
            let payload = if (200..300).contains(&status) {
                sse_ok_body()
            } else {
                body.to_string()
            };
            (status, payload)
        })
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(StreamSequencedResponder {
            calls: AtomicUsize::new(0),
            steps,
            success_delay,
        })
        .mount(&server)
        .await;
    server
}

/// Build a streaming-egress router: same openai-compat wiremock providers as
/// [`build_router`], but with an explicit alias table and retry policy so a
/// test can pin a failure class terminal or point two aliases at shared
/// models.
async fn build_stream_router(
    upstreams: Vec<Upstream>,
    aliases_spec: &[(&str, &[&str])],
    decay_hours: u64,
    retry: RetryPolicy,
) -> Router {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for u in &upstreams {
        providers.insert(
            u.provider_name.clone(),
            ProviderEntry::openai_compat(&u.base_url, common::file_ref("test-key"))
                .with_runtime(u.runtime.clone()),
        );
        models.insert(
            u.nickname.clone(),
            ModelEntry::new(&u.provider_name, "upstream-model"),
        );
    }

    let mut aliases = BTreeMap::new();
    for (alias, chain) in aliases_spec {
        let value = if chain.len() == 1 {
            AliasValue::Single(chain[0].to_string())
        } else {
            AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
        };
        aliases.insert((*alias).to_string(), value);
    }

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry,
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = decay_hours;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");

    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

/// Dispatch a streaming request against `alias` carrying `features`.
async fn stream_with(router: &Router, alias: &str, features: &[&str]) -> DispatchedStream {
    router
        .stream_with_options(req_with_features(alias, features), RouterOptions::default())
        .await
}

/// Dispatch a streaming `web_search` request against `alias`.
async fn stream(router: &Router, alias: &str) -> DispatchedStream {
    stream_with(router, alias, &[WEB_SEARCH]).await
}

/// Fully consume a dispatched stream so its egress runs to `[DONE]`. A no-op
/// for an error dispatch (nothing to drain).
async fn drain(dispatched: DispatchedStream) {
    if let Ok(mut s) = dispatched.result {
        while s.next().await.is_some() {}
    }
}

/// True when `events` carries a probe-settlement event for an UNREACHED
/// admission of `capability` on the streaming surface -- the observable that
/// the `ProbeAdmissionSet` drop released the `in_flight` slot of an admission
/// the dispatch never reached.
fn has_unreached_stream_settlement(
    events: &[routectl_testkit::CapturedEvent],
    capability: &str,
) -> bool {
    events.iter().any(|e| {
        e.field("event") == Some("probe_settlement")
            && e.field("surface") == Some("stream")
            && e.field("capability_key") == Some(capability)
            && e.field("provider_kind") == Some(PROVIDER_KIND)
            && e.field("outcome") == Some("other_error")
            && e.field("reached_target") == Some("false")
            && e.field("reason") == Some("unreached")
    })
}

// ---------------------------------------------------------------------------
// Stream mirror of Leg 2: expiry -> exactly one re-probe -> 2xx clears, driven
// through `router.stream()`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_expiry_admits_single_reprobe_then_success_clears() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // request 1: learn
        (200, ok_body()),                        // request 2: admitted re-probe clears
        (400, unsupported_body_for(WEB_SEARCH)), // request 3: fresh learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = stream(&router, "solo").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    let d2 = stream(&router, "solo").await;
    assert!(
        d2.result.is_ok(),
        "the streaming re-probe must reach A and open a stream",
    );
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "exactly one re-probe dialed A");
    drain(d2).await;

    let d3 = stream(&router, "solo").await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d3.meta.learned_capabilities.len(), 1);
    assert_eq!(
        d3.meta.learned_capabilities[0].observations, 1,
        "a cleared entry must relearn from scratch",
    );
    assert_eq!(hits(&a).await, 3);
}

// ---------------------------------------------------------------------------
// Stream mirror of Leg 7: two distinct learned negatives on ONE target both
// re-probe and both settle on the stream surface -- neither admission leaks
// its in_flight slot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_two_expired_negatives_on_one_target_both_reprobe_and_clear() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)),   // req 1: learn F1
        (400, unsupported_body_for(COMPUTER_USE)), // req 2: learn F2
        (200, ok_body()),                          // req 3: double re-probe clears both
        (400, unsupported_body_for(WEB_SEARCH)),   // req 4: fresh learn F1 (proves clear)
        (400, unsupported_body_for(COMPUTER_USE)), // req 5: fresh learn F2 (proves clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = stream_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].capability_key, WEB_SEARCH);

    let d2 = stream_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d2.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].capability_key, COMPUTER_USE);
    assert_eq!(hits(&a).await, 2);

    let d3 = stream_with(&router, "solo", &[WEB_SEARCH, COMPUTER_USE]).await;
    assert!(
        d3.result.is_ok(),
        "the double streaming re-probe must reach A and open a stream",
    );
    assert_eq!(d3.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d3.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 3, "one dispatch carried both probes");
    drain(d3).await;

    let d4 = stream_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d4.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d4.meta.learned_capabilities[0].observations, 1,
        "F1 must relearn from scratch -- its probe slot did not leak",
    );

    let d5 = stream_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d5.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d5.meta.learned_capabilities[0].observations, 1,
        "F2 must relearn from scratch -- its probe slot did not leak",
    );
    assert_eq!(hits(&a).await, 5);
}

// ---------------------------------------------------------------------------
// Early-exit / cancellation matrix on the stream path. Each case admits a
// re-probe on a target the dispatch never reaches (or a target whose dispatch
// is cancelled), then asserts the `ProbeAdmissionSet` drop released the slot:
// the streaming-surface probe-settlement event for the unreached admission.
// ---------------------------------------------------------------------------

/// success on an earlier target: A (head) re-probes and its stream opens; B
/// (admitted, tail) is never reached and its slot must reset.
#[tokio::test]
async fn stream_success_on_earlier_target_releases_unreached_admission() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
        (200, ok_body()),                        // req 2: re-probe A opens a stream
    ])
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        0,
    )
    .await;

    // req 1: both chain members learn the negative (decay 0 -> both lapse).
    let d1 = stream(&router, "chain").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: A re-probes and succeeds at the head; B's admission is unreached.
    let (d2, events) = Box::pin(routectl_testkit::with_capture(stream(&router, "chain"))).await;
    assert!(d2.result.is_ok(), "the head re-probe opens a stream");
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert_eq!(hits(&b).await, 1, "B (tail) was never reached");
    drain(d2).await;
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "the unreached tail admission must settle on the stream surface: {events:?}",
    );
}

/// terminal (non-fallbackable) error on the head: A's 400 is pinned terminal
/// so the loop returns without hopping to the admitted tail B.
#[tokio::test]
async fn stream_terminal_error_releases_unreached_admission() {
    // A's plain 400 classifies BadRequest; pinning it `fallback = false` makes
    // it terminal so the loop returns at A without reaching B.
    let mut retry = fast_retry();
    retry.classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let plain_400 = json!({"error": {"type": "invalid_request_error", "message": "bad"}});
    let a = sse_upstream_server(vec![(400, plain_400)]).await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_stream_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        &[("learn_b", &["m_b"]), ("chain", &["m_a", "m_b"])],
        0,
        retry,
    )
    .await;

    // Seed B's negative through the solo alias (decay 0 -> lapses to a re-probe).
    let d0 = stream(&router, "learn_b").await;
    assert!(matches!(
        d0.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(hits(&b).await, 1);

    // chain req: A fails terminally; B (admitted, expired) is never reached.
    let (d1, events) = Box::pin(routectl_testkit::with_capture(stream(&router, "chain"))).await;
    assert!(
        matches!(d1.result, Err(Error::Upstream { status: 400, .. })),
        "a non-fallbackable terminal error must not fall back",
    );
    assert_eq!(hits(&b).await, 1, "B was never dialed after the terminal A");
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "the terminal early return must settle the unreached admission: {events:?}",
    );
}

/// `disable_fallbacks` breaks the chain before the hop; the admitted tail B is
/// never reached and its slot must reset.
#[tokio::test]
async fn stream_disable_fallbacks_break_releases_unreached_admission() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
        (500, health_body()),                    // req 2: fallbackable, but broken by opts
    ])
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        0,
    )
    .await;

    // req 1: both learn (decay 0 -> both lapse into re-probes).
    let d1 = stream(&router, "chain").await;
    assert!(d1.result.is_err());
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: A errors; disable_fallbacks breaks before the hop to admitted B.
    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;
    let (d2, events) = Box::pin(routectl_testkit::with_capture(
        router.stream_with_options(req_with_feature("chain", WEB_SEARCH), opts),
    ))
    .await;
    assert!(
        d2.result.is_err(),
        "disable_fallbacks propagates the head failure",
    );
    assert_eq!(
        hits(&b).await,
        1,
        "B was never reached under disable_fallbacks"
    );
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "a disable_fallbacks break must settle the unreached admission: {events:?}",
    );
}

/// future-drop (cancellation): the dispatch future is dropped mid-first-chunk
/// on the head A; the admitted tail B's slot must reset on the drop.
#[tokio::test]
async fn stream_future_drop_releases_unreached_admission() {
    // A's re-probe stream opens slowly (2s first-byte delay) so the dispatch
    // future is still awaiting the first chunk when it is dropped ~150ms in.
    let a = sse_upstream_server_delayed(
        vec![
            (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
            (200, ok_body()),                        // req 2: slow re-probe, cancelled
        ],
        Some(Duration::from_secs(2)),
    )
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        0,
    )
    .await;

    // req 1: both chain members learn.
    let d1 = stream(&router, "chain").await;
    assert!(d1.result.is_err());
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: drive the dispatch, then drop it before the first chunk arrives.
    // The drop runs the guard + set destructors on THIS current-thread runtime,
    // so the unreached tail admission settles under the capture subscriber.
    let ((), events) = Box::pin(routectl_testkit::with_capture(async {
        let fut = router.stream_with_options(
            req_with_feature("chain", WEB_SEARCH),
            RouterOptions::default(),
        );
        let cancelled = tokio::time::timeout(Duration::from_millis(150), fut).await;
        assert!(
            cancelled.is_err(),
            "the slow first chunk must keep the future pending until it is dropped",
        );
    }))
    .await;
    assert_eq!(
        hits(&b).await,
        1,
        "B was never reached (A's stream never opened)"
    );
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "dropping the dispatch future must settle the unreached admission: {events:?}",
    );
}

/// no-double-settle: a solo target reached and cleared by its own guard's 2xx
/// is settled EXACTLY ONCE -- one probe-settlement event from the guard
/// (reached_target=true, outcome=success) and NOT a second from the set drop.
#[tokio::test]
async fn stream_reached_admission_settled_by_guard_not_by_set() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn
        (200, ok_body()),                        // req 2: re-probe reaches A and clears
        (400, unsupported_body_for(WEB_SEARCH)), // req 3: fresh learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = stream(&router, "solo").await;
    assert!(d1.result.is_err());

    let (d2, events) = Box::pin(routectl_testkit::with_capture(stream(&router, "solo"))).await;
    assert!(
        d2.result.is_ok(),
        "the re-probe reaches A and opens a stream"
    );
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "a cleared re-probe emits no fresh learn event",
    );
    drain(d2).await;
    let settlements: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("probe_settlement"))
        .collect();
    assert_eq!(
        settlements.len(),
        1,
        "a reached admission settles exactly once (guard only, no set double-settle): {events:?}",
    );
    let ev = settlements[0];
    assert_eq!(ev.field("state_key"), Some("m_a"));
    assert_eq!(ev.field("surface"), Some("stream"));
    assert_eq!(ev.field("outcome"), Some("success"));
    assert_eq!(ev.field("reached_target"), Some("true"));
    assert_eq!(ev.field("reason"), Some("success"));

    // The negative was cleared by the guard: the next request relearns fresh.
    let d3 = stream(&router, "solo").await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d3.meta.learned_capabilities[0].observations, 1,
        "the successful re-probe cleared the negative via the target guard",
    );
}
