//! A forwarded-credential request that draws an upstream
//! 401 / 403 / 429 is TERMINAL. routectl bypasses BOTH the
//! `on_auth_failure` refresh-and-retry AND the fallback-chain hop,
//! and surfaces the upstream status verbatim -- a request-scoped
//! forwarded token has no refresh path and no credential to fall
//! back to, so both recoveries are useless and wrong. Non-forwarded
//! requests keep the existing one-shot auth-refresh + fallback
//! behavior (also asserted here, and in `auth_failure_recovery_tests`).
//!
//! The structured-log assertion for the surfaced-verbatim WARN lives
//! in the isolated integration binary
//! `tests/forwarded_auth_terminal_log.rs` -- a thread-local capture
//! subscriber over a shared `warn!` callsite is unreliable inside the
//! 700+-test lib binary. These lib tests pin the deterministic,
//! subscriber-independent facts: no on_auth_failure, no fallback,
//! verbatim status.
use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, TokenCount};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The forwarded chain's FIRST (Anthropic) entry: returns
/// `Error::Upstream { status, .. }` on every complete / stream /
/// count_tokens call, counting each call and each `on_auth_failure`
/// invocation so a test can prove the router never tried to rotate
/// its own credential for a forwarded request.
struct StatusProvider {
    id: String,
    status: u16,
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
    count_tokens_calls: AtomicUsize,
    on_auth_failure_calls: AtomicUsize,
}

impl StatusProvider {
    fn new(id: &str, status: u16) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            status,
            complete_calls: AtomicUsize::new(0),
            stream_calls: AtomicUsize::new(0),
            count_tokens_calls: AtomicUsize::new(0),
            on_auth_failure_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Provider for StatusProvider {
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
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            &self.id,
            self.status,
            "forwarded upstream rejected",
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            &self.id,
            self.status,
            "forwarded upstream rejected",
        ))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            &self.id,
            self.status,
            "forwarded upstream rejected",
        ))
    }
    async fn on_auth_failure(&self) -> Result<()> {
        self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// The chain's SECOND (fallback) entry. Returns a DISTINCT status
/// (502) so any stray fallback hop flips both the surfaced status
/// AND this counter -- either assertion catches a leaked fallback.
/// Counts every dispatch so a test can assert it stayed at 0.
struct SiblingProvider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for SiblingProvider {
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
        Err(Error::upstream(&self.id, 502, "sibling reached"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 502, "sibling reached"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 502, "sibling reached"))
    }
}

/// Wire a router whose alias `"alias"` resolves to a two-entry chain
/// `[m-primary, m-sibling]`, both backed by `anthropic-api` provider
/// entries on `api.anthropic.com` so the forwarded-passthrough gate
/// admits the request and dispatch actually reaches the first seat.
/// The concrete provider Arcs are the counting mocks above.
///
/// Returns the router plus the primary provider handle and the
/// sibling call counter for post-dispatch assertions. A fast,
/// no-retry `RetryPolicy` keeps every path single-attempt-per-seat.
/// `primary_forwarded` marks `p-primary`'s provider entry
/// `credential_source = Forwarded` (the per-target flag the terminal
/// re-key now keys off) when `true`; `p-sibling` is always an Own
/// provider (it must never legitimately be reached by these tests).
fn build_chain(
    status: u16,
    primary_forwarded: bool,
) -> (Router, Arc<StatusProvider>, Arc<AtomicUsize>) {
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
    config.providers.insert("p-primary".into(), primary_entry);
    config.providers.insert(
        "p-sibling".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["m-primary".into(), "m-sibling".into()]),
    );

    let primary = StatusProvider::new("p-primary", status);
    let sibling_calls = Arc::new(AtomicUsize::new(0));
    let sibling: Arc<dyn Provider> = Arc::new(SiblingProvider {
        id: "p-sibling".into(),
        calls: sibling_calls.clone(),
    });

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-primary".into(),
        Arc::new(ResolvedModel::new(
            "m-primary",
            "p-primary",
            primary.clone() as Arc<dyn Provider>,
            "claude-primary",
        )),
    );
    models.insert(
        "m-sibling".into(),
        Arc::new(ResolvedModel::new(
            "m-sibling",
            "p-sibling",
            sibling,
            "claude-sibling",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    (router, primary, sibling_calls)
}

fn forwarded_req() -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        ..Default::default()
    };
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new("sk-ant-oat01-FORWARDED".into()));
    req
}

fn plain_req() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        ..Default::default()
    }
}

fn upstream_status(err: &Error) -> u16 {
    match err {
        Error::Upstream { status, .. } => *status,
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

// ---- complete() ----

#[tokio::test]
async fn complete_forwarded_401_is_terminal_no_auth_failure_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(401, true);

    let err = router
        .complete(forwarded_req())
        .await
        .expect_err("forwarded 401 must surface verbatim, not recover");

    assert_eq!(upstream_status(&err), 401, "verbatim upstream status");
    assert_eq!(
        primary.on_auth_failure_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT trigger on_auth_failure",
    );
    assert_eq!(
        primary.complete_calls.load(Ordering::SeqCst),
        1,
        "forwarded 401 must not refresh-and-retry the same seat",
    );
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT fall back to the sibling target",
    );
}

#[tokio::test]
async fn complete_forwarded_403_is_terminal_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(403, true);

    let err = router
        .complete(forwarded_req())
        .await
        .expect_err("forwarded 403 must surface verbatim");

    assert_eq!(upstream_status(&err), 403);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.complete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        0,
        "forwarded 403 must NOT fall back to the sibling target",
    );
}

#[tokio::test]
async fn complete_forwarded_429_is_terminal_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(429, true);

    let err = router
        .complete(forwarded_req())
        .await
        .expect_err("forwarded 429 must surface verbatim");

    assert_eq!(upstream_status(&err), 429);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        primary.complete_calls.load(Ordering::SeqCst),
        1,
        "forwarded 429 is terminal: no same-provider retry",
    );
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        0,
        "forwarded 429 must NOT fall back to the sibling target",
    );
}

// ---- stream() ----

#[tokio::test]
async fn stream_forwarded_401_is_terminal_no_auth_failure_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(401, true);

    let err = router
        .stream(forwarded_req())
        .await
        .err()
        .expect("forwarded 401 must surface verbatim before any chunk");

    assert_eq!(upstream_status(&err), 401);
    assert_eq!(
        primary.on_auth_failure_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT trigger on_auth_failure on the stream path",
    );
    assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT fall back on the stream path",
    );
}

#[tokio::test]
async fn stream_forwarded_403_is_terminal_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(403, true);

    let err = router
        .stream(forwarded_req())
        .await
        .err()
        .expect("forwarded 403 must surface verbatim");

    assert_eq!(upstream_status(&err), 403);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_forwarded_429_is_terminal_no_fallback() {
    let (router, primary, sibling_calls) = build_chain(429, true);

    let err = router
        .stream(forwarded_req())
        .await
        .err()
        .expect("forwarded 429 must surface verbatim");

    assert_eq!(upstream_status(&err), 429);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
}

// ---- count_tokens() ----

#[tokio::test]
async fn count_tokens_forwarded_401_is_terminal_no_auth_failure() {
    let (router, primary, sibling_calls) = build_chain(401, true);

    let err = router
        .count_tokens(forwarded_req())
        .await
        .expect_err("forwarded 401 must surface verbatim");

    assert_eq!(upstream_status(&err), 401);
    assert_eq!(
        primary.on_auth_failure_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT trigger on_auth_failure on the count_tokens path",
    );
    assert_eq!(
        primary.count_tokens_calls.load(Ordering::SeqCst),
        1,
        "forwarded 401 must not refresh-and-retry the count_tokens seat",
    );
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        0,
        "forwarded 401 must NOT walk to the sibling seat",
    );
}

#[tokio::test]
async fn count_tokens_forwarded_403_is_terminal() {
    let (router, primary, sibling_calls) = build_chain(403, true);

    let err = router
        .count_tokens(forwarded_req())
        .await
        .expect_err("forwarded 403 must surface verbatim");

    assert_eq!(upstream_status(&err), 403);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.count_tokens_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn count_tokens_forwarded_429_is_terminal() {
    let (router, primary, sibling_calls) = build_chain(429, true);

    let err = router
        .count_tokens(forwarded_req())
        .await
        .expect_err("forwarded 429 must surface verbatim");

    assert_eq!(upstream_status(&err), 429);
    assert_eq!(primary.on_auth_failure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.count_tokens_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
}

// ---- non-forwarded regression: the bypass is gated on the target's
//      use_forwarded_credential, not request-global bearer presence ----

#[tokio::test]
async fn complete_non_forwarded_401_still_refreshes_and_falls_back() {
    // Identical router, but no forwarded bearer: the existing
    // one-shot auth-refresh (on_auth_failure) MUST still fire, and
    // after the second 401 the chain MUST still fall back to the
    // sibling. This is the guard that the forwarded-terminal bypass
    // is scoped to a forwarded-credential TARGET only.
    let (router, primary, sibling_calls) = build_chain(401, false);

    let err = router
        .complete(plain_req())
        .await
        .expect_err("non-forwarded chain exhausts to the sibling error");

    assert_eq!(
        upstream_status(&err),
        502,
        "non-forwarded 401 falls back to the sibling (502)",
    );
    assert_eq!(
        primary.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "non-forwarded 401 must still trigger the one-shot refresh",
    );
    assert_eq!(
        primary.complete_calls.load(Ordering::SeqCst),
        2,
        "non-forwarded 401 refreshes and retries the same seat once",
    );
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        1,
        "non-forwarded 401 must still fall back to the sibling",
    );
}

#[tokio::test]
async fn complete_forwarded_bearer_present_but_target_is_own_credential_still_refreshes_and_falls_back()
 {
    // Coexistence regression: a MITM-marked request
    // (a captured forwarded bearer IS present) whose alias resolves
    // to an OWN-credential Anthropic provider must retry/fall back
    // EXACTLY as before the per-target passthrough gate -- the floating
    // bearer is never consumed by
    // an own-creds target, and the terminal bypass never wrongly
    // fires just because a bearer happens to be present on the
    // request. Same router as the plain-request regression above;
    // only the request differs.
    let (router, primary, sibling_calls) = build_chain(401, false);

    let err = router
        .complete(forwarded_req())
        .await
        .expect_err("own-credential chain exhausts to the sibling error");

    assert_eq!(
        upstream_status(&err),
        502,
        "a captured bearer must not change fallback behavior on an own-credential target",
    );
    assert_eq!(
        primary.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "an own-credential target must still get the one-shot refresh \
             even though the request carries a forwarded bearer",
    );
    assert_eq!(
        primary.complete_calls.load(Ordering::SeqCst),
        2,
        "own-credential target refreshes and retries the same seat once",
    );
    assert_eq!(
        sibling_calls.load(Ordering::SeqCst),
        1,
        "own-credential target must still fall back to the sibling",
    );
}
