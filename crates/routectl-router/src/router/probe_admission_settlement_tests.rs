//! An admitted learned re-probe whose chain target the dispatch never
//! reaches must still release its `in_flight` slot, so the next request
//! re-probes rather than routing away until reload. The
//! `ProbeAdmissionSet` settles every unreached admission as `OtherError` on
//! drop. These tests drive `complete_inner` through the early-exit shapes
//! -- success on an earlier target, a terminal non-fallbackable error,
//! `break 'chain` under disable_fallbacks, and a dropped (cancelled)
//! dispatch future -- and assert the unreached target's slot reset. A
//! no-double-settle test pins the transfer: an admission the target guard
//! settled is not settled again by the set. The settlement observability
//! events assert the guard's own emissions too (reached success and a
//! reached-then-dropped terminal).
use super::*;
use crate::config::{AliasValue, ProviderEntry};
use crate::learned_capability::ExportedEntry;
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::capability::normalize_capability_key;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider, Role,
    ToolDef, Usage,
};
use serde_json::json;

const PROVIDER_KIND: &str = "openai-compat";

/// What a target's in-process provider returns, chosen per target so a
/// test can steer the dispatch loop to leave a LATER target unreached.
#[derive(Clone, Copy)]
enum Behavior {
    /// 2xx success -> the loop returns at this target.
    Succeed,
    /// A non-`Upstream` error -> classifies as `Unknown` (retry 0,
    /// fallback false), so the loop returns terminally without a hop.
    FailTerminal,
    /// A fallbackable upstream 500 -> the loop would hop, but
    /// disable_fallbacks makes it return at this target.
    FailFallbackable,
    /// 2xx success, but only after a delay long enough for a test to drop
    /// the dispatch future mid-`complete` (leaving a later target
    /// unreached, or exercising the reached-then-cancelled path).
    SucceedAfter(Duration),
}

/// An in-process provider whose `complete` outcome is fixed at
/// construction, so a test controls the dispatch path without a wire.
struct ScriptedProvider {
    id: String,
    behavior: Behavior,
}

impl ScriptedProvider {
    /// The canonical 2xx response echoing the request model.
    fn ok_response(req: ChatRequest) -> ChatResponse {
        ChatResponse {
            id: "ok".into(),
            model: req.model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        match self.behavior {
            Behavior::Succeed => Ok(Self::ok_response(req)),
            Behavior::FailTerminal => Err(Error::normalize_response(&self.id, "terminal")),
            Behavior::FailFallbackable => Err(Error::upstream(&self.id, 500, "boom")),
            Behavior::SucceedAfter(delay) => {
                tokio::time::sleep(delay).await;
                Ok(Self::ok_response(req))
            }
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response(&self.id, "stream unused"))
    }
}

/// A lapsed (already-expired) self-identifying negative: acting AND due
/// for a re-probe, so the chain filter admits it and flips `in_flight`.
fn lapsed_negative(state_key: &str, feature_key: &str) -> ExportedEntry {
    let base = Instant::now();
    ExportedEntry {
        state_key: state_key.into(),
        feature_key: feature_key.into(),
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: base,
        last_seen: base,
        expires_at: base.checked_sub(Duration::from_secs(1)).unwrap_or(base),
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

/// Build a router whose alias `chain` resolves to the given
/// `(nickname, provider_name, behavior)` targets in order, `[capability]`
/// enabled. Each dispatches to an in-process `ScriptedProvider`; a
/// `state_key` equals its nickname and every provider is registered
/// openai-compat so the learned registry sees a provider kind.
fn build_router(targets: &[(&str, &str, Behavior)]) -> Router {
    let mut providers = BTreeMap::new();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, provider_name, behavior) in targets {
        providers.insert(
            (*provider_name).to_string(),
            ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k"),
        );
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            id: (*provider_name).to_string(),
            behavior: *behavior,
        });
        models.insert(
            (*nickname).to_string(),
            Arc::new(ResolvedModel::new(
                *nickname,
                *provider_name,
                provider,
                "upstream-model",
            )),
        );
    }
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "chain".to_string(),
        AliasValue::Chain(targets.iter().map(|(n, _, _)| (*n).to_string()).collect()),
    );
    let mut cfg = Config {
        providers,
        aliases,
        ..Config::default()
    };
    cfg.capability.enabled = true;
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(models);
    router
}

/// A request against the `chain` alias carrying a `web_search` built-in
/// tool, so `derive_feature_keys` yields `[web_search]` and the seeded
/// negative is consulted.
fn req_with_web_search() -> ChatRequest {
    ChatRequest {
        model: "chain".into(),
        messages: vec![],
        tools: Some(vec![ToolDef::Other(
            json!({"type": "web_search", "name": "t"}),
        )]),
        ..Default::default()
    }
}

/// The resident registry entry for `(state_key, feature_key)`, or a panic
/// -- an unreached OtherError settle keeps the entry, so it must persist.
fn probe_entry(router: &Router, state_key: &str, feature_key: &str) -> ExportedEntry {
    router
        .learned_capabilities
        .export_entries()
        .into_iter()
        .find(|e| e.state_key == state_key && e.feature_key == feature_key)
        .expect("the seeded negative must still be resident after dispatch")
}

#[tokio::test]
async fn success_on_earlier_target_releases_unreached_admission() {
    // m_a succeeds at the head; m_b (admitted for a re-probe) is never
    // reached. Its slot must reset so the next request re-probes m_b.
    let router = build_router(&[
        ("m_a", "prov_a", Behavior::Succeed),
        ("m_b", "prov_b", Behavior::Succeed),
    ]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_b", &cap)]);

    let d = router
        .complete_with_options(req_with_web_search(), RouterOptions::default())
        .await;
    assert!(d.result.is_ok(), "m_a should succeed: {:?}", d.result.err());

    assert!(
        !probe_entry(&router, "m_b", &cap).in_flight,
        "an admission the loop never reached must release in_flight",
    );
}

#[tokio::test]
async fn terminal_error_on_earlier_target_releases_unreached_admission() {
    // m_a returns a non-fallbackable terminal error; the loop returns
    // without hopping to the admitted m_b, whose slot must still reset.
    let router = build_router(&[
        ("m_a", "prov_a", Behavior::FailTerminal),
        ("m_b", "prov_b", Behavior::Succeed),
    ]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_b", &cap)]);

    let d = router
        .complete_with_options(req_with_web_search(), RouterOptions::default())
        .await;
    assert!(
        d.result.is_err(),
        "a non-fallbackable terminal error must not fall back",
    );

    assert!(
        !probe_entry(&router, "m_b", &cap).in_flight,
        "a terminal early return must release the unreached admission",
    );
}

#[tokio::test]
async fn break_under_disable_fallbacks_releases_unreached_admission() {
    // m_a fails with a fallbackable error, but disable_fallbacks breaks the
    // chain before the hop; the admitted m_b is never reached.
    let router = build_router(&[
        ("m_a", "prov_a", Behavior::FailFallbackable),
        ("m_b", "prov_b", Behavior::Succeed),
    ]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_b", &cap)]);

    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;
    let d = router
        .complete_with_options(req_with_web_search(), opts)
        .await;
    assert!(
        d.result.is_err(),
        "disable_fallbacks propagates the failure"
    );

    assert!(
        !probe_entry(&router, "m_b", &cap).in_flight,
        "a disable_fallbacks break must release the unreached admission",
    );
}

#[tokio::test]
async fn reached_admission_settled_by_guard_not_by_set() {
    // A solo target reached and settled by its own LearnedProbeGuard (a 2xx
    // clears the negative) emits EXACTLY ONE probe-settlement event -- from
    // the guard (reached_target=true, outcome=success) -- and NOT a second
    // from the set's drop. The take() move makes exact-once settlement
    // structural, so a second event (reached_target=false) would prove a
    // double-settle.
    let router = build_router(&[("m_a", "prov_a", Behavior::Succeed)]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_a", &cap)]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(req_with_web_search(), RouterOptions::default()),
    )
    .await;
    assert!(d.result.is_ok(), "the re-probe reaches m_a and succeeds");

    assert!(
        router
            .learned_capabilities
            .export_entries()
            .iter()
            .all(|e| !(e.state_key == "m_a" && e.feature_key == cap)),
        "a successful re-probe clears the negative via the target guard",
    );
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
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("outcome"), Some("success"));
    assert_eq!(ev.field("reached_target"), Some("true"));
    assert_eq!(ev.field("reason"), Some("success"));
}

#[tokio::test]
async fn reached_terminal_drop_emits_terminal_settlement() {
    // A solo target reached by the loop but terminated by a non-capability
    // error is neither a success nor a same-capability settle, so its guard
    // drops with the admission still held: the guard's Drop emits one
    // probe-settlement event tagged reached-then-dropped (outcome=other_error,
    // reached_target=true, reason=terminal) and releases the slot.
    let router = build_router(&[("m_a", "prov_a", Behavior::FailTerminal)]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_a", &cap)]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(req_with_web_search(), RouterOptions::default()),
    )
    .await;
    assert!(d.result.is_err(), "the terminal error returns terminally");

    assert!(
        !probe_entry(&router, "m_a", &cap).in_flight,
        "a reached-then-dropped admission must release in_flight",
    );
    let ev = events
        .iter()
        .find(|e| e.field("event") == Some("probe_settlement"))
        .expect("the guard drop must emit a probe-settlement event");
    assert_eq!(ev.field("state_key"), Some("m_a"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("outcome"), Some("other_error"));
    assert_eq!(ev.field("reached_target"), Some("true"));
    assert_eq!(ev.field("reason"), Some("terminal"));
}

#[tokio::test]
async fn future_drop_releases_unreached_admission() {
    // m_a succeeds only after a long delay; the dispatch future is dropped
    // ~150ms in, while it is still awaiting m_a's completion, so the
    // admitted tail m_b is never reached. Dropping the future runs the set
    // destructor on this current-thread runtime, settling the unreached
    // admission under the capture subscriber (in_flight reset + one
    // reached_target=false / reason=unreached event).
    let router = build_router(&[
        (
            "m_a",
            "prov_a",
            Behavior::SucceedAfter(Duration::from_secs(2)),
        ),
        ("m_b", "prov_b", Behavior::Succeed),
    ]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_b", &cap)]);

    let ((), events) = routectl_testkit::with_capture(async {
        let fut = router.complete_with_options(req_with_web_search(), RouterOptions::default());
        let cancelled = tokio::time::timeout(Duration::from_millis(150), fut).await;
        assert!(
            cancelled.is_err(),
            "the slow completion must keep the future pending until it is dropped",
        );
    })
    .await;

    assert!(
        !probe_entry(&router, "m_b", &cap).in_flight,
        "dropping the dispatch future must release the unreached admission",
    );
    let ev = events
        .iter()
        .find(|e| e.field("event") == Some("probe_settlement"))
        .expect("the dropped future's set destructor must emit a probe-settlement event");
    assert_eq!(ev.field("state_key"), Some("m_b"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("outcome"), Some("other_error"));
    assert_eq!(ev.field("reached_target"), Some("false"));
    assert_eq!(ev.field("reason"), Some("unreached"));
}

#[tokio::test]
async fn unreached_admission_emits_probe_settlement_event() {
    // The set drop emits one probe-settlement debug event per unreached
    // admission, carrying the full field set.
    let router = build_router(&[
        ("m_a", "prov_a", Behavior::Succeed),
        ("m_b", "prov_b", Behavior::Succeed),
    ]);
    let cap = normalize_capability_key("web_search", PROVIDER_KIND);
    router
        .learned_capabilities
        .import_entries(vec![lapsed_negative("m_b", &cap)]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(req_with_web_search(), RouterOptions::default()),
    )
    .await;
    assert!(d.result.is_ok());

    let ev = events
        .iter()
        .find(|e| e.field("event") == Some("probe_settlement"))
        .expect("the set drop must emit a probe-settlement event for the unreached admission");
    assert_eq!(ev.level, tracing::Level::DEBUG);
    assert_eq!(ev.field("state_key"), Some("m_b"));
    assert_eq!(ev.field("capability_key"), Some(cap.as_str()));
    assert_eq!(ev.field("provider_kind"), Some(PROVIDER_KIND));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("outcome"), Some("other_error"));
    assert_eq!(ev.field("reached_target"), Some("false"));
    assert_eq!(ev.field("reason"), Some("unreached"));
}
