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
use std::sync::atomic::{AtomicUsize, Ordering};

const EXPIRED: Option<UpstreamReason> = Some(UpstreamReason::CredentialExpired);
const INVALID: Option<UpstreamReason> = Some(UpstreamReason::CredentialInvalid);

#[derive(Clone, Copy, Debug)]
enum Step {
    Ok,
    Reject(u16, Option<UpstreamReason>),
}

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
    refresh_err: Option<(u16, Option<UpstreamReason>)>,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(id: &str, script: &[Step]) -> Arc<Self> {
        Self::with_refresh(id, script, None)
    }

    fn with_refresh(
        id: &str,
        script: &[Step],
        refresh_err: Option<(u16, Option<UpstreamReason>)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            script: Mutex::new(script.iter().copied().collect()),
            refresh_err,
            calls: AtomicUsize::new(0),
        })
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
        self.next()?;
        Ok(ChatResponse {
            model: req.model,
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        use futures::stream::StreamExt as _;
        self.next()?;
        Ok(futures::stream::iter(vec![Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.next()?;
        Ok(TokenCount {
            input_tokens: 7,
            extras: Default::default(),
        })
    }
    async fn on_auth_failure(&self) -> Result<()> {
        match self.refresh_err {
            Some((status, reason)) => Err(rejection(&self.id, status, reason)),
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
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
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
                Some((400, refresh_reason)),
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
    for surface in [Surface::Complete, Surface::Stream] {
        // Arrange
        let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary.clone(), sibling.clone(), false);
        let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
        assert!(router.park_credential_for_tests("m-a", UpstreamReason::CredentialExpired, until));

        // Act
        let served = dispatch(&router, surface, request(None, false)).await;

        // Assert
        assert!(served, "{surface:?}: the sibling must serve");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 0, "{surface:?}");
        assert_eq!(sibling.calls.load(Ordering::SeqCst), 1, "{surface:?}");
        assert_eq!(park_reason(&router, "m-a"), EXPIRED, "{surface:?}");
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
