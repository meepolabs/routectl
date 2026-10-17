//! A seat whose upstream rejects the credential itself, with a typed reason,
//! is parked; a parked seat is skipped and the chain falls back; the first
//! success through the seat lifts the park. Driven through every dispatch
//! surface (complete, stream, count_tokens), because each owns its own error
//! arm, success arm, and auth-refresh exit.

use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::runtime_state::{CREDENTIAL_PARK_BACKSTOP, CredentialParkStatus};
use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, TokenCount, UpstreamReason,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

const EXPIRED: Option<UpstreamReason> = Some(UpstreamReason::CredentialExpired);
const INVALID: Option<UpstreamReason> = Some(UpstreamReason::CredentialInvalid);

#[derive(Clone, Copy, Debug)]
enum Step {
    Ok,
    Reject(u16, Option<UpstreamReason>),
}

/// The reason an auth refresh fails with: `Some` is a terminal refresh
/// (`Error::Credential`), `None` any other refresh failure (`Error::Auth`).
type RefreshErr = Option<UpstreamReason>;

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
    CountTokens,
}

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

/// Answers every surface from one script, one step per dispatch, and repeats
/// the last step once the script runs out.
struct ScriptedProvider {
    id: String,
    script: Mutex<VecDeque<Step>>,
    refresh_err: Option<RefreshErr>,
    calls: AtomicUsize,
    hold: Option<Arc<Hold>>,
}

/// Holds the first dispatch inside the upstream call until released, so a
/// test can act between the attempt's admission and its outcome.
#[derive(Default)]
struct Hold {
    entered: Notify,
    release: Notify,
    taken: AtomicBool,
}

impl Hold {
    async fn wait_once(&self) {
        if !self.taken.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

impl ScriptedProvider {
    fn new(id: &str, script: &[Step]) -> Arc<Self> {
        Self::with_refresh(id, script, None)
    }

    fn with_refresh(id: &str, script: &[Step], refresh_err: Option<RefreshErr>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            script: Mutex::new(script.iter().copied().collect()),
            refresh_err,
            calls: AtomicUsize::new(0),
            hold: None,
        })
    }

    fn held(
        id: &str,
        script: &[Step],
        refresh_err: Option<RefreshErr>,
        hold: Arc<Hold>,
    ) -> Arc<Self> {
        let mut provider = Arc::into_inner(Self::with_refresh(id, script, refresh_err))
            .expect("freshly built provider is unshared");
        provider.hold = Some(hold);
        Arc::new(provider)
    }

    async fn held_next(&self) -> Result<()> {
        if let Some(hold) = &self.hold {
            hold.wait_once().await;
        }
        self.next()
    }

    fn next(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock();
        let step = if script.len() > 1 {
            script.pop_front()
        } else {
            script.front().copied()
        };
        match step.unwrap_or(Step::Ok) {
            Step::Ok => Ok(()),
            Step::Reject(status, reason) => Err(rejection(&self.id, status, reason)),
        }
    }
}

fn rejection(id: &str, status: u16, reason: Option<UpstreamReason>) -> Error {
    Error::upstream(id, status, "credential rejected").with_upstream_reason(reason)
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![routectl_core::ChunkChoice {
            index: 0,
            delta: routectl_core::ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
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
        self.held_next().await?;
        Ok(ChatResponse {
            model: req.model,
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        use futures::stream::StreamExt as _;
        self.held_next().await?;
        Ok(futures::stream::iter(vec![Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.held_next().await?;
        Ok(TokenCount {
            input_tokens: 7,
            extras: Default::default(),
        })
    }
    async fn on_auth_failure(&self) -> Result<()> {
        match self.refresh_err {
            Some(Some(reason)) => Err(Error::Credential {
                detail: format!("oauth refresh failed for {}", self.id),
                reason,
                parked_until_ms: None,
            }),
            Some(None) => Err(Error::Auth(format!("oauth refresh failed for {}", self.id))),
            None => Ok(()),
        }
    }
}

/// Alias `alias` -> `[m-a, m-b]`, both anthropic-api entries so every surface
/// (count_tokens included) admits them. Single-attempt retry policy, so each
/// seat is dialed once per request plus the one auth-recovery retry.
fn chain_router(
    primary: Arc<ScriptedProvider>,
    sibling: Arc<ScriptedProvider>,
    primary_forwarded: bool,
) -> Router {
    chain_router_with_auth_fallback(primary, sibling, primary_forwarded, None)
}

/// [`chain_router`] with the operator's `auth` class fallback override.
fn chain_router_with_auth_fallback(
    primary: Arc<ScriptedProvider>,
    sibling: Arc<ScriptedProvider>,
    primary_forwarded: bool,
    auth_fallback: Option<bool>,
) -> Router {
    let mut retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        jitter_ms: 0,
        ..RetryPolicy::default()
    };
    if let Some(fallback) = auth_fallback {
        retry.classes.insert(
            crate::class_policy::ConfigFailureClass::Auth,
            crate::class_policy::ClassPolicy {
                retry: None,
                fallback: Some(fallback),
            },
        );
    }
    let mut config = Config {
        retry,
        ..Config::default()
    };
    let mut primary_entry = ProviderEntry::anthropic_api("literal:k");
    if primary_forwarded {
        primary_entry =
            primary_entry.with_credential_source(crate::config::CredentialSource::Forwarded);
    }
    config.providers.insert("p-a".into(), primary_entry);
    config
        .providers
        .insert("p-b".into(), ProviderEntry::anthropic_api("literal:k"));
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["m-a".into(), "m-b".into()]),
    );
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-a".into(),
        Arc::new(ResolvedModel::new(
            "m-a",
            "p-a",
            primary as Arc<dyn Provider>,
            "claude-a",
        )),
    );
    models.insert(
        "m-b".into(),
        Arc::new(ResolvedModel::new(
            "m-b",
            "p-b",
            sibling as Arc<dyn Provider>,
            "claude-b",
        )),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn request(max_tokens: Option<u32>, forwarded: bool) -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        max_tokens,
        ..Default::default()
    };
    if forwarded {
        req.routectl_internal.forwarded_bearer =
            Some(ForwardedBearer::new("sk-ant-oat01-FORWARDED".into()));
    }
    req
}

/// Dispatch one request on `surface`, draining a stream, and report whether
/// it succeeded.
async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> bool {
    use futures::stream::StreamExt as _;
    match surface {
        Surface::Complete => router.complete(req).await.is_ok(),
        Surface::Stream => match router.stream(req).await {
            Ok(mut stream) => {
                while stream.next().await.is_some() {}
                true
            }
            Err(_) => false,
        },
        Surface::CountTokens => router.count_tokens(req).await.is_ok(),
    }
}

fn park_reason(router: &Router, state_key: &str) -> Option<UpstreamReason> {
    router
        .gate_status_for_tests(state_key)
        .credential_park
        .map(|park| park.reason)
}

#[tokio::test]
async fn a_terminal_rejection_parks_the_seat_only_when_it_carries_a_reason() {
    let cases = [
        ("401 expired", Step::Reject(401, EXPIRED), EXPIRED),
        ("403 invalid", Step::Reject(403, INVALID), INVALID),
        ("401 unclassified", Step::Reject(401, None), None),
        ("403 unclassified", Step::Reject(403, None), None),
    ];
    for surface in SURFACES {
        for (name, step, expected) in cases {
            // Arrange
            let primary = ScriptedProvider::new("p-a", &[step]);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary, sibling, false);

            // Act
            dispatch(&router, surface, request(None, false)).await;

            // Assert
            assert_eq!(park_reason(&router, "m-a"), expected, "{surface:?} {name}");
        }
    }
}

#[tokio::test]
async fn a_rejection_that_must_not_judge_the_seat_never_parks_it() {
    let cases = [
        ("probe", request(Some(1), false), false),
        ("forwarded credential", request(None, true), true),
    ];
    for surface in SURFACES {
        for (name, req, primary_forwarded) in cases.clone() {
            // Arrange
            let primary = ScriptedProvider::new("p-a", &[Step::Reject(403, INVALID)]);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary.clone(), sibling, primary_forwarded);

            // Act
            dispatch(&router, surface, req).await;

            // Assert
            assert!(
                primary.calls.load(Ordering::SeqCst) > 0,
                "{surface:?} {name}: the seat must have been dialed",
            );
            assert_eq!(park_reason(&router, "m-a"), None, "{surface:?} {name}");
        }
    }
}

#[tokio::test]
async fn a_terminal_refresh_failure_with_a_reason_parks_the_seat() {
    let cases = [
        ("refresh rejected with a reason", INVALID, INVALID),
        ("refresh failed without a reason", None, None),
    ];
    for surface in SURFACES {
        for (name, refresh_reason, expected) in cases {
            // Arrange
            let primary = ScriptedProvider::with_refresh(
                "p-a",
                &[Step::Reject(401, None)],
                Some(refresh_reason),
            );
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary, sibling, false);

            // Act
            dispatch(&router, surface, request(None, false)).await;

            // Assert
            assert_eq!(park_reason(&router, "m-a"), expected, "{surface:?} {name}");
        }
    }
}

#[tokio::test]
async fn a_parked_seat_is_skipped_and_the_chain_falls_back() {
    let policies = [
        ("default auth policy", None),
        ("auth fallback disabled", Some(false)),
    ];
    for surface in SURFACES {
        for (name, auth_fallback) in policies {
            // Arrange
            let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router_with_auth_fallback(
                primary.clone(),
                sibling.clone(),
                false,
                auth_fallback,
            );
            let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
            assert!(router.park_credential_for_tests(
                "m-a",
                UpstreamReason::CredentialExpired,
                until
            ));

            // Act
            let served = dispatch(&router, surface, request(None, false)).await;

            // Assert
            let label = format!("{surface:?} {name}");
            assert!(served, "{label}: the sibling must serve");
            assert_eq!(primary.calls.load(Ordering::SeqCst), 0, "{label}");
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 1, "{label}");
            assert_eq!(park_reason(&router, "m-a"), EXPIRED, "{label}");
        }
    }
}

#[tokio::test]
async fn the_retest_of_a_lapsed_park_settles_it() {
    let cases: [(&str, &[Step], Option<UpstreamReason>); 3] = [
        ("success clears", &[Step::Ok], None),
        (
            "refreshed retry success clears",
            &[Step::Reject(401, None), Step::Ok],
            None,
        ),
        ("a reason re-parks", &[Step::Reject(403, INVALID)], INVALID),
    ];
    for surface in SURFACES {
        for (name, script, expected) in cases {
            // Arrange
            let primary = ScriptedProvider::new("p-a", script);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary.clone(), sibling, false);
            let lapsed = Instant::now();
            assert!(router.park_credential_for_tests(
                "m-a",
                UpstreamReason::CredentialExpired,
                lapsed
            ));

            // Act
            dispatch(&router, surface, request(None, false)).await;

            // Assert
            assert!(
                primary.calls.load(Ordering::SeqCst) > 0,
                "{surface:?} {name}: the lapsed seat must be re-tested",
            );
            let park = router.gate_status_for_tests("m-a").credential_park;
            assert_eq!(park.map(|p| p.reason), expected, "{surface:?} {name}");
            if let Some(CredentialParkStatus { remaining, .. }) = park {
                assert!(
                    remaining + Duration::from_mins(1) > CREDENTIAL_PARK_BACKSTOP,
                    "{surface:?} {name}: a re-park starts a fresh backstop, got {remaining:?}",
                );
            }
        }
    }
}

#[tokio::test]
async fn clear_all_credential_parks_lifts_every_seat() {
    // Arrange
    let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
    let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
    let router = chain_router(primary, sibling, false);
    let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
    for key in ["m-a", "m-b"] {
        assert!(router.park_credential_for_tests(key, UpstreamReason::CredentialInvalid, until));
    }

    // Act
    let cleared = router.clear_all_credential_parks();

    // Assert
    assert_eq!(cleared, 2);
    assert_eq!(park_reason(&router, "m-a"), None);
    assert_eq!(park_reason(&router, "m-b"), None);
}

#[tokio::test]
async fn a_success_settles_the_breaker_and_the_park_in_one_critical_section() {
    for surface in SURFACES {
        // Arrange: a lapsed park whose re-test succeeds, and a rejection that
        // tries to park the seat from inside the success settlement.
        let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary, sibling, false);
        assert!(router.park_credential_for_tests(
            "m-a",
            UpstreamReason::CredentialExpired,
            Instant::now()
        ));
        let slot = router.state.get("m-a").cloned().expect("m-a slot");
        let fired = Arc::new(AtomicBool::new(false));
        let waited_for_the_settle = Arc::new(AtomicBool::new(false));
        {
            let (slot, fired, waited) =
                (slot.clone(), fired.clone(), waited_for_the_settle.clone());
            super::credential_park::set_within_success_settle_hook(move || {
                fired.store(true, Ordering::SeqCst);
                match slot.try_lock() {
                    Some(mut state) => {
                        state.park_credential(Instant::now(), UpstreamReason::CredentialInvalid)
                    }
                    None => waited.store(true, Ordering::SeqCst),
                }
            });
        }

        // Act
        let served = dispatch(&router, surface, request(None, false)).await;
        if waited_for_the_settle.load(Ordering::SeqCst) {
            slot.lock()
                .park_credential(Instant::now(), UpstreamReason::CredentialInvalid);
        }

        // Assert
        assert!(served, "{surface:?}");
        assert!(
            fired.load(Ordering::SeqCst),
            "{surface:?}: the success must settle through the seat"
        );
        assert!(
            waited_for_the_settle.load(Ordering::SeqCst),
            "{surface:?}: the rejection must wait out the settlement's lock"
        );
        assert_eq!(
            park_reason(&router, "m-a"),
            INVALID,
            "{surface:?}: a rejection after the success must stay parked"
        );
    }
}

#[tokio::test]
async fn a_rejection_admitted_before_a_reload_parks_only_when_no_reload_landed() {
    let rejections: [(&str, &[Step], Option<RefreshErr>); 2] = [
        ("terminal rejection", &[Step::Reject(403, INVALID)], None),
        (
            "terminal refresh failure",
            &[Step::Reject(401, None)],
            Some(INVALID),
        ),
    ];
    for surface in SURFACES {
        for (name, script, refresh_err) in rejections {
            for (reload, expected) in [(true, None), (false, INVALID)] {
                // Arrange: the seat is healthy at admission; the attempt is
                // held inside the upstream call.
                let hold = Arc::new(Hold::default());
                let primary = ScriptedProvider::held("p-a", script, refresh_err, hold.clone());
                let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
                let router = chain_router(primary, sibling, false);

                // Act: a reload lands while the attempt is in flight, then
                // the attempt's rejection comes back.
                let in_flight = dispatch(&router, surface, request(None, false));
                let reloader = async {
                    hold.entered.notified().await;
                    if reload {
                        router.clear_all_credential_parks();
                    }
                    hold.release.notify_one();
                };
                tokio::join!(in_flight, reloader);

                // Assert
                assert_eq!(
                    park_reason(&router, "m-a"),
                    expected,
                    "{surface:?} {name} reload={reload}",
                );
            }
        }
    }
}

fn gate_logs(events: &[routectl_testkit::CapturedEvent]) -> Vec<(tracing::Level, String)> {
    const MESSAGES: [&str; 4] = [
        "gate blocked",
        "stream gate blocked",
        "count_tokens gate blocked",
        "seat credential rejected; parking the seat",
    ];
    events
        .iter()
        .filter(|event| MESSAGES.contains(&event.message.as_str()))
        .map(|event| {
            let kind = event.field("gate_kind").unwrap_or("park").to_string();
            (event.level, kind)
        })
        .collect()
}

#[tokio::test]
async fn requests_against_a_parked_seat_warn_once_at_the_park_and_not_per_request() {
    const REQUESTS: usize = 4;
    for surface in SURFACES {
        // Arrange
        let primary = ScriptedProvider::new("p-a", &[Step::Reject(403, INVALID)]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary.clone(), sibling, false);

        // Act
        let ((), events) = routectl_testkit::with_capture(Box::pin(async {
            for _ in 0..REQUESTS {
                dispatch(&router, surface, request(None, false)).await;
            }
        }))
        .await;

        // Assert
        let logs = gate_logs(&events);
        let warns: Vec<_> = logs
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .collect();
        let parked_debugs = logs
            .iter()
            .filter(|(level, kind)| *level == tracing::Level::DEBUG && kind == "credential_parked")
            .count();
        assert_eq!(
            primary.calls.load(Ordering::SeqCst),
            1,
            "{surface:?}: only the first request dials"
        );
        assert_eq!(
            warns,
            [&(tracing::Level::WARN, "park".to_string())],
            "{surface:?}"
        );
        assert_eq!(
            parked_debugs,
            REQUESTS - 1,
            "{surface:?}: every later request hit the park"
        );
    }
}

#[tokio::test]
async fn requests_against_an_open_breaker_still_warn_per_request() {
    const REQUESTS: usize = 3;
    for surface in SURFACES {
        // Arrange
        let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary, sibling, false);
        assert!(router.force_open_breaker("m-a", Duration::from_hours(1)));

        // Act
        let ((), events) = routectl_testkit::with_capture(Box::pin(async {
            for _ in 0..REQUESTS {
                dispatch(&router, surface, request(None, false)).await;
            }
        }))
        .await;

        // Assert
        let breaker_warns = gate_logs(&events)
            .into_iter()
            .filter(|(level, kind)| *level == tracing::Level::WARN && kind == "circuit_breaker")
            .count();
        assert_eq!(breaker_warns, REQUESTS, "{surface:?}");
    }
}

fn epoch_ms_in(from_now: Duration) -> u64 {
    let at = std::time::SystemTime::now() + from_now;
    let ms = at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    u64::try_from(ms).unwrap()
}

/// Dispatch one request on `surface` and return its error, if any.
async fn dispatch_err(router: &Router, surface: Surface, req: ChatRequest) -> Option<Error> {
    match surface {
        Surface::Complete => router.complete(req).await.err(),
        Surface::Stream => router.stream(req).await.err(),
        Surface::CountTokens => router.count_tokens(req).await.err(),
    }
}

#[tokio::test]
async fn every_seat_parked_fails_fast_with_the_earliest_deadline_and_its_reason() {
    const LATE: Duration = Duration::from_hours(1);
    const EARLY: Duration = Duration::from_mins(30);
    let rows = [
        ("earliest first", (INVALID, EARLY), (EXPIRED, LATE), INVALID),
        ("earliest last", (INVALID, LATE), (EXPIRED, EARLY), EXPIRED),
    ];
    for surface in SURFACES {
        for (name, (a_reason, a_in), (b_reason, b_in), expected_reason) in rows {
            // Arrange
            let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary.clone(), sibling.clone(), false);
            let now = Instant::now();
            for (key, reason, park_for) in [("m-a", a_reason, a_in), ("m-b", b_reason, b_in)] {
                assert!(router.park_credential_for_tests(key, reason.unwrap(), now + park_for));
            }

            // Act
            let err = dispatch_err(&router, surface, request(None, false)).await;

            // Assert
            let label = format!("{surface:?} {name}");
            assert_eq!(primary.calls.load(Ordering::SeqCst), 0, "{label}");
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 0, "{label}");
            let Some(Error::Credential {
                reason,
                parked_until_ms: Some(until_ms),
                ..
            }) = err
            else {
                panic!("{label}: expected an all-parked credential error, got {err:?}");
            };
            assert_eq!(Some(reason), expected_reason, "{label}");
            let expected_ms = epoch_ms_in(EARLY);
            assert!(
                until_ms.abs_diff(expected_ms) < 5_000,
                "{label}: deadline {until_ms} is not the earliest park ({expected_ms})",
            );
        }
    }
}

#[tokio::test]
async fn a_dialed_seat_error_outranks_a_parked_seat() {
    let orders = [("parked first", "m-a"), ("parked last", "m-b")];
    for surface in SURFACES {
        for (name, parked_key) in orders {
            // Arrange: one seat parked, the other answers a plain 401.
            let primary = ScriptedProvider::new("p-a", &[Step::Reject(401, None)]);
            let sibling = ScriptedProvider::new("p-b", &[Step::Reject(401, None)]);
            let router = chain_router(primary, sibling, false);
            let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
            assert!(router.park_credential_for_tests(
                parked_key,
                UpstreamReason::CredentialExpired,
                until
            ));

            // Act
            let err = dispatch_err(&router, surface, request(None, false)).await;

            // Assert
            assert!(
                matches!(err, Some(Error::Upstream { status: 401, .. })),
                "{surface:?} {name}: the dialed seat's own error must surface, got {err:?}",
            );
        }
    }
}

/// Alias `alias` -> `[pooled, m-b]`: `pooled` is a two-seat pool on its own
/// provider pair, `m-b` a single seat on another provider.
fn pool_then_sibling_router(
    seats: [Arc<ScriptedProvider>; 2],
    sibling: Arc<ScriptedProvider>,
) -> Router {
    use crate::config::PoolEntry;
    use crate::seat_pool::SeatTarget;
    let members = ["pool-a", "pool-b"];
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    for member in members {
        config.providers.insert(
            member.into(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    config
        .providers
        .insert("p-b".into(), ProviderEntry::anthropic_api("literal:k"));
    config.pools.insert(
        "the-pool".into(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect()),
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["pooled".into(), "m-b".into()]),
    );
    let seat_targets: Vec<SeatTarget> = members
        .iter()
        .zip(seats)
        .map(|(member, provider)| SeatTarget {
            provider_name: (*member).to_string(),
            provider: provider as Arc<dyn Provider>,
            auth_secret_ref: Some(routectl_auth::SecretRef::OAuth {
                provider: (*member).to_string(),
                label: None,
            }),
        })
        .collect();
    let default_provider = seat_targets[0].provider.clone();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "pooled".into(),
        Arc::new(
            ResolvedModel::new("pooled", "the-pool", default_provider, "claude-a")
                .with_seats(Arc::from(seat_targets)),
        ),
    );
    models.insert(
        "m-b".into(),
        Arc::new(ResolvedModel::new(
            "m-b",
            "p-b",
            sibling as Arc<dyn Provider>,
            "claude-b",
        )),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn every_seat_of_one_provider_parked_still_falls_back_to_another_provider() {
    for surface in SURFACES {
        // Arrange
        let seats = [
            ScriptedProvider::new("pool-a", &[Step::Ok]),
            ScriptedProvider::new("pool-b", &[Step::Ok]),
        ];
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = pool_then_sibling_router(seats.clone(), sibling.clone());
        let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
        let keys: Vec<String> = router
            .dispatch_chain("alias", None)
            .expect("chain resolves")
            .into_iter()
            .map(|t| t.state_key)
            .filter(|key| key.starts_with("pooled#"))
            .collect();
        assert_eq!(keys.len(), 2, "both pool seats are in the chain: {keys:?}");
        for key in &keys {
            assert!(router.park_credential_for_tests(
                key,
                UpstreamReason::CredentialInvalid,
                until
            ));
        }

        // Act
        let served = dispatch(&router, surface, request(None, false)).await;

        // Assert
        assert!(served, "{surface:?}: the other provider must serve");
        for seat in &seats {
            assert_eq!(
                seat.calls.load(Ordering::SeqCst),
                0,
                "{surface:?} {}",
                seat.id
            );
        }
        assert_eq!(sibling.calls.load(Ordering::SeqCst), 1, "{surface:?}");
    }
}
