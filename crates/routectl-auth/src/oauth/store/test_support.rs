//! Shared unit-test fixtures for the OAuth store module.

use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::oauth::providers::{AuthParams, OAuthFlow};
use crate::oauth::types::{AccountInfo, SecretToken};

use super::Inner;

pub(super) use std::collections::BTreeMap;
pub(super) use std::sync::Arc;
pub(super) use tokio::sync::RwLock;

pub(super) use crate::oauth::file_io;
pub(super) use crate::oauth::types::{CredentialsFile, TokenRecord, seat_key, unix_now};
pub(super) use crate::oauth::{OAuthError, OAuthResult};
pub(super) use crate::{SecretRef, SecretStore};

pub(super) use super::{LocalProbe, OAuthStore, OpenOutcome};

pub(super) fn rec_at(expires_at: u64) -> TokenRecord {
    rec_named("tok-abc", expires_at)
}

pub(super) fn rec_named(access: &str, expires_at: u64) -> TokenRecord {
    TokenRecord {
        access_token: SecretToken::new(access),
        refresh_token: SecretToken::new("rtok-xyz"),
        token_type: "Bearer".into(),
        expires_at_unix: expires_at,
        scopes: vec!["user:inference".into()],
        account: AccountInfo::default(),
        obtained_at_unix: 0,
        session_id: None,
        cloud_project_id: None,
    }
}

/// What the fake `OAuthFlow` should return on the next refresh.
/// Cloned per call so the same fake can be reused across iterations.
#[derive(Clone)]
pub(super) enum RefreshOutcome {
    /// Successful refresh that mints a record with this access
    /// token and a 1h future expiry.
    Mint(String),
    /// Simulate Anthropic's `invalid_grant` mapping. Drives the
    /// "actionable error" assertion.
    RefreshExpired,
    /// Simulate a transient upstream failure (network / 5xx) that
    /// the cooldown classifier accepts. Drives the cooldown tests.
    Transient,
}

/// Tracks simultaneous `refresh_token` invocations so a test can
/// distinguish concurrent per-seat refreshes from serialized
/// per-provider ones. `in_flight` is the live count; `max` is the
/// high-water mark observed across the run.
#[derive(Clone, Default)]
pub(super) struct ConcurrencyGauge {
    in_flight: Arc<StdMutex<u32>>,
    max: Arc<StdMutex<u32>>,
}

impl ConcurrencyGauge {
    /// Record entry into `refresh_token`: bump in-flight and lift the
    /// high-water mark if this is the most simultaneous so far.
    fn enter(&self) {
        let now = {
            let mut g = self.in_flight.lock().unwrap();
            *g += 1;
            *g
        };
        let mut m = self.max.lock().unwrap();
        if now > *m {
            *m = now;
        }
    }

    /// Record exit from `refresh_token`.
    fn leave(&self) {
        *self.in_flight.lock().unwrap() -= 1;
    }

    fn max(&self) -> u32 {
        *self.max.lock().unwrap()
    }
}

/// Fake `OAuthFlow` that counts `refresh_token` invocations and
/// returns canned outcomes. Used as a `cfg(test)` override in
/// `Inner::refresh_flow` so unit tests do not stand up the real
/// claude.ai token endpoint.
pub(super) struct CountingFlow {
    calls: Arc<StdMutex<u32>>,
    outcome: StdMutex<RefreshOutcome>,
    /// When true, `refresh_token` yields once before returning so a
    /// concurrent caller (in `tokio::join!`) can park on the
    /// per-seat single-flight mutex while we hold it.
    yield_once: bool,
    /// Optional concurrency gauge. When set, `refresh_token` records
    /// entry/exit so a test can read the max simultaneous in-flight
    /// count. A per-seat lock lets two arms overlap (max == 2); a
    /// shared per-provider lock serializes them (max == 1). Lets a
    /// test assert that distinct seats refresh concurrently rather
    /// than merely counting total refreshes (which is 2 either way,
    /// since each seat's double-check still finds its own record
    /// stale).
    concurrency: Option<ConcurrencyGauge>,
    /// Optional rendezvous barrier. When set, `refresh_token` waits on
    /// it inside the gauge region (after `enter`, before `leave`) so
    /// max-in-flight == 2 is reachable ONLY when both arms are provably
    /// inside `refresh_token` at once, not as a yield-timing artifact.
    /// A shared per-provider lock can never bring the second arm to the
    /// barrier, so it deadlocks (the test bounds it with a timeout)
    /// instead of silently passing.
    rendezvous: Option<Arc<tokio::sync::Barrier>>,
}

impl CountingFlow {
    pub(super) fn new(outcome: RefreshOutcome) -> Self {
        Self {
            calls: Arc::new(StdMutex::new(0)),
            outcome: StdMutex::new(outcome),
            yield_once: false,
            concurrency: None,
            rendezvous: None,
        }
    }

    /// Swap the outcome the next `refresh_token` will return. Lets a
    /// single injected fake model an outage that later recovers.
    pub(super) fn set_outcome(&self, outcome: RefreshOutcome) {
        *self.outcome.lock().unwrap() = outcome;
    }

    pub(super) fn with_yield(mut self) -> Self {
        self.yield_once = true;
        self
    }

    /// Enable the concurrency gauge (see the `concurrency` field).
    /// Implies `with_yield` so the overlap window is observable.
    pub(super) fn with_concurrency_gauge(mut self) -> Self {
        self.yield_once = true;
        self.concurrency = Some(ConcurrencyGauge::default());
        self
    }

    /// Attach a rendezvous barrier (see the `rendezvous` field). The
    /// barrier's party count must match the number of concurrent
    /// refresh arms the test drives.
    pub(super) fn with_rendezvous(mut self, barrier: Arc<tokio::sync::Barrier>) -> Self {
        self.rendezvous = Some(barrier);
        self
    }

    pub(super) fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }

    /// Max simultaneous `refresh_token` invocations observed. Only
    /// meaningful when built `with_concurrency_gauge`.
    pub(super) fn max_in_flight(&self) -> u32 {
        self.concurrency.as_ref().map_or(0, ConcurrencyGauge::max)
    }
}

#[async_trait]
impl OAuthFlow for CountingFlow {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }
    fn display_name(&self) -> &'static str {
        "Test (counting)"
    }
    fn auth_url(&self, _p: &AuthParams<'_>) -> url::Url {
        unimplemented!("auth_url unused in refresh tests")
    }
    fn manual_redirect_url(&self) -> &'static str {
        "https://example.invalid/callback"
    }
    async fn exchange_code(
        &self,
        _http: &reqwest::Client,
        _code: &str,
        _verifier: &str,
        _state: &str,
        _redirect_uri: &str,
    ) -> OAuthResult<TokenRecord> {
        unimplemented!("exchange_code unused in refresh tests")
    }
    async fn refresh_token(
        &self,
        _http: &reqwest::Client,
        _refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        *self.calls.lock().unwrap() += 1;
        // Concurrency gauge: record entry BEFORE yielding so a
        // concurrent arm that enters during the yield window is
        // observed as overlapping.
        if let Some(gauge) = &self.concurrency {
            gauge.enter();
        }
        // Rendezvous (when set): block until every concurrent arm is
        // inside the gauge region, so max-in-flight reflects genuine
        // simultaneity, not yield ordering. A shared lock never brings
        // the second arm here -> deadlock -> the test's timeout fails
        // loudly instead of a silent false-green.
        if let Some(barrier) = &self.rendezvous {
            barrier.wait().await;
        }
        if self.yield_once {
            // Suspend so a concurrent caller has a chance to enter
            // (per-seat lock) or park on the single-flight mutex
            // (per-provider lock).
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        if let Some(gauge) = &self.concurrency {
            gauge.leave();
        }
        match self.outcome.lock().unwrap().clone() {
            RefreshOutcome::Mint(at) => Ok(rec_named(&at, unix_now() + 3600)),
            RefreshOutcome::RefreshExpired => Err(OAuthError::RefreshExpired("anthropic".into())),
            RefreshOutcome::Transient => {
                Err(OAuthError::Network("simulated upstream outage".into()))
            }
        }
    }
}

/// Build an `OAuthStore` whose refresh path goes through `flow`.
/// Loads any existing credentials at `path` so callers can seed a
/// record via `write_record` before flipping the flow on.
pub(super) async fn open_with_flow<P: Into<PathBuf>>(
    path: P,
    flow: Arc<dyn OAuthFlow>,
) -> OAuthStore {
    let path: PathBuf = path.into();
    let cf = file_io::load(&path).await.unwrap();
    // Bare reqwest client: the injected `OAuthFlow` is a fake that
    // never calls this client, so the production builder's
    // `redirect::Policy::none()` and timeout settings are
    // intentionally omitted. If a future test path leaks into a
    // real HTTP call, mirror the chain from `OAuthStore::open()`.
    let http = reqwest::Client::builder().build().unwrap();
    OAuthStore {
        inner: Arc::new(Inner {
            path,
            file: RwLock::new(cf),
            load_error: std::sync::RwLock::new(None),
            http,
            refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
            reload_gen: std::sync::atomic::AtomicU64::new(0),
            refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
            refresh_flow: Some(flow),
            now_override: std::sync::atomic::AtomicU64::new(0),
        }),
    }
}

// ---- probe_local ----

/// A record with a specific access-token expiry and an EMPTY refresh
/// token -- the "no refresh token stored" shape probe_local reads as
/// non-revivable once expired.
pub(super) fn rec_no_refresh(expires_at: u64) -> TokenRecord {
    TokenRecord {
        access_token: SecretToken::new("tok-abc"),
        refresh_token: SecretToken::new(""),
        token_type: "Bearer".into(),
        expires_at_unix: expires_at,
        scopes: vec!["user:inference".into()],
        account: AccountInfo::default(),
        obtained_at_unix: 0,
        session_id: None,
        cloud_project_id: None,
    }
}

// ---- start-and-degrade + hot-reload recovery ----
//
// `serve` opens the credentials store through `open_or_degraded`: a
// broken file (corrupt / wrong-schema / wrong-perms) does NOT fail
// startup and does NOT drop the oauth arm. The store is kept present
// but degraded -- every read surfaces the TRUE sanitized cause (not a
// misleading "not logged in" or the no-config-dir HOME/XDG string) and
// every write is refused (never overwrite a file we could not read).
// The operator fixes the file, `reload_from_disk` clears the marker,
// and the store is live again with no restart.

pub(super) fn oauth_ref(provider: &str) -> SecretRef {
    SecretRef::OAuth {
        provider: provider.to_string(),
        label: None,
    }
}

/// Write raw bytes to a credentials path with owner-only `0600` perms
/// on Unix, so the file passes the loader's permission hygiene and the
/// degrade under test is the JSON/schema failure, not the perms check.
pub(super) fn write_creds_0600(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Helper: seed a near-expiry record on disk, then open a store whose
/// refresh path is the injected counting fake. Returns the store.
pub(super) async fn seed_near_expiry_with_flow(
    path: &std::path::Path,
    flow: Arc<dyn OAuthFlow>,
) -> OAuthStore {
    let seed = OAuthStore::open(path).await.unwrap();
    seed.write_record("anthropic", rec_at(unix_now() + 10))
        .await
        .unwrap();
    drop(seed);
    open_with_flow(path, flow).await
}

pub(super) fn anthropic_ref() -> SecretRef {
    SecretRef::OAuth {
        provider: "anthropic".into(),
        label: None,
    }
}
