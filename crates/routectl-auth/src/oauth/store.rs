//! `OAuthStore`: routectl's `SecretStore` for `oauth://<provider>` refs.
//!
//! Owns the in-memory cache of `CredentialsFile` and serialises all
//! disk reads/writes through async-aware locks. Read resolution +
//! login writeback ship together with the refresh single-flight gate
//! and the `on_auth_failure -> refresh + persist` hook. The refresh
//! body POSTs through the per-provider `OAuthFlow::refresh_token`,
//! double-checks under a per-provider mutex so concurrent callers do
//! not stampede the token endpoint, and persists atomically via
//! `file_io::save` (write-temp + fsync + rename).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use routectl_core::{Error, Result};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::oauth::file_io;
use crate::oauth::providers;
use crate::oauth::types::{unix_now, CredentialsFile, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};
use crate::{SecretRef, SecretStore};

/// Re-read window: a `get()` call within `near_expiry(REFRESH_LEAD_SECS)`
/// of expiry triggers a refresh through `refresh_under_lock`. 60s is
/// wide enough that the refresh POST + atomic disk write completes
/// before expiry on a healthy network, and narrow enough that most
/// requests serve from the in-memory cache without touching the token
/// endpoint. The eight tokens-per-day amortized cost of an unnecessary
/// early refresh is negligible compared to the user-visible latency
/// of a 401 -> on_auth_failure -> retry round-trip if the lead is too
/// short. Operators on flaky networks may want to widen this; pinned
/// here as a const because no production driver has yet asked for
/// per-deployment tuning.
pub(crate) const REFRESH_LEAD_SECS: u64 = 60;

/// Routectl-managed OAuth credentials store. Cheap to clone; the
/// inner state is `Arc`-shared so multiple `Provider` instances can
/// share one store (and one per-provider refresh single-flight gate).
#[derive(Clone)]
pub struct OAuthStore {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    file: RwLock<CredentialsFile>,
    /// HTTP client used by login + refresh. Pooled so repeated POSTs
    /// reuse one TCP connection. `connect_timeout` short-circuits
    /// hung-during-DNS situations; the overall `timeout` keeps a slow
    /// or hostile token endpoint from holding a login attempt open
    /// forever.
    http: reqwest::Client,
    /// Per-provider single-flight refresh mutex. Only acquired when a
    /// near-expiry (or forced 401) trigger fires; the lock-holder
    /// refreshes and persists, stragglers re-read the freshly-written
    /// record and return without re-refreshing (double-check pattern).
    /// `BTreeMap` rather than a single global `tokio::sync::Mutex` so
    /// per-provider concurrency is preserved when a future codex
    /// provider lands; the cost is one heap-allocated `Arc<Mutex<()>>`
    /// per provider id (refresh contention is cents-per-day either
    /// way). The map itself is guarded by a sync `std::sync::Mutex`
    /// because get-or-insert is a tiny CPU-only critical section.
    refresh_locks: std::sync::Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
    /// Test seam: when set, `refresh_under_lock` uses this flow instead
    /// of `providers::lookup`. Lets the unit tests inject a counting
    /// fake without standing up the real token endpoint. The field is
    /// `cfg(test)`-gated so production binaries cannot carry an
    /// override at all.
    #[cfg(test)]
    refresh_flow: Option<Arc<dyn providers::OAuthFlow>>,
}

impl OAuthStore {
    /// HTTP-client connect timeout for OAuth token-endpoint POSTs. 10s
    /// is generous for a single TCP/TLS handshake to a public IdP and
    /// short enough that a hung firewall fails the request rather than
    /// stalling an in-flight chat-completions call indefinitely.
    pub(crate) const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;

    /// HTTP-client total request timeout for OAuth token-endpoint
    /// POSTs (exchange + refresh). 30s caps the worst case where the
    /// IdP accepts the connection but stalls the response. The
    /// router's request-timeout policy is independent; this guards
    /// only the OAuth control plane.
    pub(crate) const HTTP_TOTAL_TIMEOUT_SECS: u64 = 30;

    /// Open the store at `path`, loading any existing credentials. A
    /// missing file yields an empty in-memory store -- first run is
    /// not an error.
    pub async fn open(path: impl Into<PathBuf>) -> OAuthResult<Self> {
        let path: PathBuf = path.into();
        let cf = file_io::load(&path).await?;
        // Disable redirect-following: the refresh POST carries the
        // long-lived refresh token in the body, and a 307/308 from
        // the IdP would replay that POST to the redirect target.
        // Treat any 3xx from the token endpoint as an upstream
        // failure rather than silently re-sending the secret.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(
                Self::HTTP_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(std::time::Duration::from_secs(
                Self::HTTP_TOTAL_TIMEOUT_SECS,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OAuthError::Internal(format!("reqwest client: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                file: RwLock::new(cf),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                #[cfg(test)]
                refresh_flow: None,
            }),
        })
    }

    /// Open with the default path
    /// (`$XDG_CONFIG_HOME/routectl/credentials.json`).
    pub async fn open_default() -> OAuthResult<Self> {
        let path = file_io::default_path()?;
        Self::open(path).await
    }

    /// Where the credentials live on disk. Useful for `routectl
    /// whoami` output and error messages.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Shared HTTP client. `pub(crate)` so login/refresh modules can
    /// reuse the configured pool.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    /// Read a record without expiry checking. Internal use only --
    /// the public `get()` enforces near-expiry semantics.
    async fn read_record(&self, provider: &str) -> OAuthResult<TokenRecord> {
        let guard = self.inner.file.read().await;
        guard
            .get(provider)
            .cloned()
            .ok_or_else(|| OAuthError::NotLoggedIn(provider.to_string()))
    }

    /// Persist a token record atomically. Disk write happens FIRST,
    /// off a clone of the current in-memory state; the in-memory cache
    /// only commits on a successful save. This way a failed disk write
    /// (full FS, EIO, permissions glitch) leaves both halves consistent
    /// rather than diverging into "memory says new token, disk still
    /// has the old one".
    pub(crate) async fn write_record(&self, provider: &str, rec: TokenRecord) -> OAuthResult<()> {
        let mut guard = self.inner.file.write().await;
        let mut staged = guard.clone();
        staged.upsert(provider, rec);
        file_io::save(&self.inner.path, &staged).await?;
        *guard = staged;
        Ok(())
    }

    /// Remove a provider's tokens (used by `routectl logout`). Same
    /// disk-first ordering as `write_record`: stage the removal,
    /// persist, then commit to memory only on `Ok(())`.
    pub(crate) async fn remove_provider(&self, provider: &str) -> OAuthResult<bool> {
        let mut guard = self.inner.file.write().await;
        if !guard.providers.contains_key(provider) {
            return Ok(false);
        }
        let mut staged = guard.clone();
        staged.remove(provider);
        file_io::save(&self.inner.path, &staged).await?;
        *guard = staged;
        Ok(true)
    }

    /// Snapshot all known provider records (for `routectl whoami`).
    pub async fn list(&self) -> Vec<(String, TokenRecord)> {
        self.inner
            .file
            .read()
            .await
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Remove a provider's tokens by name (used by `routectl logout`).
    /// Returns `Ok(true)` when a record existed and was removed,
    /// `Ok(false)` when no record was present (first-time logout is not
    /// an error). Named `logout` rather than `delete(&str)` to avoid
    /// shadowing the `SecretStore::delete(&SecretRef)` trait method.
    pub async fn logout(&self, provider: &str) -> OAuthResult<bool> {
        self.remove_provider(provider).await
    }

    /// Force a refresh for `provider` regardless of expiry. Used by
    /// `routectl refresh <provider>` and by `on_auth_failure` after an
    /// upstream 401. Goes through the per-provider single-flight gate
    /// so a 401 storm collapses to one token-endpoint POST. Returns the
    /// freshly-persisted `TokenRecord` so the CLI can report the new
    /// expiry.
    pub async fn force_refresh(&self, provider: &str) -> Result<TokenRecord> {
        // Validate the provider id up front so an unknown id does not
        // surface as `NotLoggedIn` (which is misleading).
        providers::lookup(provider).map_err(Error::from)?;
        let current = self.read_record(provider).await.map_err(Error::from)?;
        self.refresh_under_lock(provider, &current, true).await
    }

    /// Refresh `provider`'s record under the per-provider single-flight
    /// mutex. The `current` record is the snapshot the caller saw
    /// before contending for the lock; on the `force` path it is the
    /// "dead" token the upstream rejected, used to short-circuit
    /// redundant rotations when another caller already refreshed.
    ///
    /// Single-flight + double-check pattern:
    /// 1. Acquire the per-provider mutex (instantiating it lazily).
    /// 2. Re-read the in-memory record (writes are disk-first under
    ///    the same lock, so a fresh read reflects any winner's
    ///    persisted refresh).
    /// 3. If the lock-waiter sees the record is no longer stale
    ///    (`!near_expiry` for the get path, or the in-memory access
    ///    token differs from the dead token for the force path),
    ///    return it without touching the network.
    /// 4. Otherwise POST to the upstream's refresh endpoint, persist
    ///    atomically, return the new record.
    async fn refresh_under_lock(
        &self,
        provider: &str,
        current: &TokenRecord,
        force: bool,
    ) -> Result<TokenRecord> {
        // Step 1: get-or-insert the per-provider tokio mutex. The map
        // is guarded by a `std::sync::Mutex` -- the critical section is
        // CPU-only (BTreeMap entry + Arc clone) and never crosses an
        // await, so a sync mutex is the right primitive here.
        let lock = {
            let mut locks = self
                .inner
                .refresh_locks
                .lock()
                .expect("refresh_locks mutex poisoned");
            locks
                .entry(provider.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Step 2: double-check after acquiring the lock. Another caller
        // may have refreshed while we were parked.
        let rec = self.read_record(provider).await.map_err(Error::from)?;
        let still_stale = if force {
            // Forced (401) path: skip the expiry check, but if the
            // on-disk access token has already changed since the dead
            // token we were handed, somebody else refreshed and we
            // should not rotate the freshly-minted refresh token a
            // second time. Comparing access_token specifically is
            // enough: every refresh rotates both, so a difference here
            // unambiguously means a successful refresh ran.
            rec.access_token == current.access_token
        } else {
            rec.near_expiry(REFRESH_LEAD_SECS, unix_now())
        };
        if !still_stale {
            return Ok(rec);
        }

        // Step 3: actually refresh.
        let flow = self.resolve_flow(provider)?;
        let new_rec = flow
            .refresh_token(self.http(), rec.refresh_token.expose())
            .await
            .map_err(|e| {
                Error::Auth(format!(
                    "oauth refresh failed for {provider}: {e}; \
                     re-run `routectl login {provider}`"
                ))
            })?;

        // Step 4: persist atomically. `write_record` already does the
        // disk-first ordering (temp + fsync + rename, then commit
        // memory) so a failed save leaves the in-memory cache pointing
        // at the still-valid old record.
        self.write_record(provider, new_rec.clone())
            .await
            .map_err(Error::from)?;
        Ok(new_rec)
    }

    /// Resolve the `OAuthFlow` for `provider`. Production lookup goes
    /// through the static registry in `providers::lookup`. The
    /// `cfg(test)` branch lets unit tests inject a counting fake.
    fn resolve_flow(&self, provider: &str) -> Result<&dyn providers::OAuthFlow> {
        #[cfg(test)]
        if let Some(f) = &self.inner.refresh_flow {
            return Ok(f.as_ref());
        }
        let flow: &'static dyn providers::OAuthFlow =
            providers::lookup(provider).map_err(Error::from)?;
        Ok(flow)
    }
}

#[async_trait]
impl SecretStore for OAuthStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        let provider = match secret_ref {
            SecretRef::OAuth { provider } => provider,
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        // Validate provider is known. The lookup also gives operators
        // the authoritative "unknown oauth provider" message rather
        // than a silent miss.
        providers::lookup(provider).map_err(Error::from)?;

        let rec = self.read_record(provider).await.map_err(Error::from)?;

        if rec.near_expiry(REFRESH_LEAD_SECS, unix_now()) {
            tracing::debug!(
                provider = %provider,
                expires_at_unix = rec.expires_at_unix,
                "oauth access token near expiry; entering refresh single-flight"
            );
            let refreshed = self.refresh_under_lock(provider, &rec, false).await?;
            return Ok(refreshed.access_token.expose().to_string());
        }
        Ok(rec.access_token.expose().to_string())
    }

    async fn set(&self, _secret_ref: &SecretRef, _value: &str) -> Result<()> {
        // OAuth tokens are minted by `routectl login`, not by manual
        // assignment. Refuse loudly so a typo (e.g. config builder
        // calling `set` with a static string) does not silently
        // overwrite real credentials.
        Err(Error::Auth(
            "oauth tokens are managed via `routectl login <provider>`; \
             direct `set` is not supported"
                .into(),
        ))
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<()> {
        let provider = match secret_ref {
            SecretRef::OAuth { provider } => provider,
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        self.remove_provider(provider)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    async fn on_auth_failure(&self, secret_ref: &SecretRef) -> Result<()> {
        // The router calls this after an upstream 401 against a
        // credential resolved from this store. Force a refresh -- the
        // upstream said the access token is dead regardless of what
        // `expires_at_unix` claims (clock skew, server-side rotation,
        // revocation). The single-flight gate inside `force_refresh`
        // collapses a 401 storm into one POST.
        let provider = match secret_ref {
            SecretRef::OAuth { provider } => provider,
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        self.force_refresh(provider).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::providers::{AuthParams, OAuthFlow};
    use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord};
    use std::sync::Mutex as StdMutex;

    fn rec_at(expires_at: u64) -> TokenRecord {
        rec_named("tok-abc", expires_at)
    }

    fn rec_named(access: &str, expires_at: u64) -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new(access),
            refresh_token: SecretToken::new("rtok-xyz"),
            token_type: "Bearer".into(),
            expires_at_unix: expires_at,
            scopes: vec!["user:inference".into()],
            account: AccountInfo::default(),
            obtained_at_unix: 0,
        }
    }

    /// What the fake `OAuthFlow` should return on the next refresh.
    /// Cloned per call so the same fake can be reused across iterations.
    #[derive(Clone)]
    enum RefreshOutcome {
        /// Successful refresh that mints a record with this access
        /// token and a 1h future expiry.
        Mint(String),
        /// Simulate Anthropic's `invalid_grant` mapping. Drives the
        /// "actionable error" assertion.
        RefreshExpired,
    }

    /// Fake `OAuthFlow` that counts `refresh_token` invocations and
    /// returns canned outcomes. Used as a `cfg(test)` override in
    /// `Inner::refresh_flow` so unit tests do not stand up the real
    /// claude.ai token endpoint.
    struct CountingFlow {
        calls: Arc<StdMutex<u32>>,
        outcome: RefreshOutcome,
        /// When true, `refresh_token` yields once before returning so a
        /// concurrent caller (in `tokio::join!`) can park on the
        /// per-provider single-flight mutex while we hold it.
        yield_once: bool,
    }

    impl CountingFlow {
        fn new(outcome: RefreshOutcome) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(0)),
                outcome,
                yield_once: false,
            }
        }

        fn with_yield(mut self) -> Self {
            self.yield_once = true;
            self
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
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
            if self.yield_once {
                // Suspend so a concurrent caller has a chance to park
                // on the per-provider refresh mutex.
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
            }
            match &self.outcome {
                RefreshOutcome::Mint(at) => Ok(rec_named(at, unix_now() + 3600)),
                RefreshOutcome::RefreshExpired => {
                    Err(OAuthError::RefreshExpired("anthropic".into()))
                }
            }
        }
    }

    /// Build an `OAuthStore` whose refresh path goes through `flow`.
    /// Loads any existing credentials at `path` so callers can seed a
    /// record via `write_record` before flipping the flow on.
    async fn open_with_flow<P: Into<PathBuf>>(path: P, flow: Arc<dyn OAuthFlow>) -> OAuthStore {
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
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                refresh_flow: Some(flow),
            }),
        }
    }

    #[tokio::test]
    async fn get_returns_token_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();

        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-abc");
    }

    #[tokio::test]
    async fn get_near_expiry_triggers_refresh_and_returns_new_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // Seed a near-expiry record on disk first (no flow yet).
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-refreshed");
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // The refreshed record must have been persisted: a fresh open
        // sees the new access token.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let listed = reopened.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.access_token.expose(), "tok-refreshed");
    }

    #[tokio::test]
    async fn get_does_not_refresh_when_token_is_fresh_via_seam() {
        // Same wiring as the seam-based test above, but with a
        // not-near-expiry seed: refresh must NOT fire, even though the
        // override is set.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-should-not-be-used".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-abc");
        assert_eq!(
            flow.call_count(),
            0,
            "no refresh should fire on fresh token"
        );
    }

    #[tokio::test]
    async fn concurrent_get_calls_collapse_to_single_refresh() {
        // Two concurrent get() calls on a near-expiry token must
        // collapse to exactly one refresh through the per-provider
        // single-flight mutex. The double-check after acquiring the
        // lock returns the freshly-written record without re-POSTing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();
        drop(seed);

        let flow =
            Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into())).with_yield());
        let store = open_with_flow(&path, flow.clone()).await;
        let store2 = store.clone();
        let r = SecretRef::OAuth {
            provider: "anthropic".into(),
        };
        let r2 = r.clone();

        let (a, b) = tokio::join!(async move { store.get(&r).await }, async move {
            store2.get(&r2).await
        });
        let tok_a = a.unwrap();
        let tok_b = b.unwrap();
        assert_eq!(tok_a, "tok-refreshed");
        assert_eq!(tok_b, "tok-refreshed");
        assert_eq!(
            flow.call_count(),
            1,
            "single-flight gate should collapse two concurrent gets to one refresh"
        );
    }

    #[tokio::test]
    async fn concurrent_on_auth_failure_calls_collapse_to_single_refresh() {
        // Mirror of `concurrent_get_calls_collapse_to_single_refresh`
        // for the force-refresh path. Two concurrent
        // `on_auth_failure` calls (e.g., a 401 storm where multiple
        // in-flight requests all simultaneously detect their tokens
        // are dead) must collapse to exactly one refresh through the
        // per-provider single-flight mutex. This pins the
        // double-check semantics on the force path: the second
        // waiter compares the in-memory access token against its
        // dead-token snapshot and short-circuits when the first
        // waiter already rotated. Without this test the
        // double-check could regress to "always refresh under the
        // lock" and the test suite would not catch the redundant
        // refresh-token rotation.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // Seed a healthy (not near expiry) record so the lazy path
        // does NOT fire; only the force path should run.
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);

        let flow =
            Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-after-401".into())).with_yield());
        let store = open_with_flow(&path, flow.clone()).await;
        let store2 = store.clone();
        let r = SecretRef::OAuth {
            provider: "anthropic".into(),
        };
        let r2 = r.clone();

        let (a, b) = tokio::join!(async move { store.on_auth_failure(&r).await }, async move {
            store2.on_auth_failure(&r2).await
        });
        a.expect("first concurrent on_auth_failure should succeed");
        b.expect("second concurrent on_auth_failure should succeed");
        assert_eq!(
            flow.call_count(),
            1,
            "single-flight + double-check should collapse two concurrent 401-recoveries to one refresh",
        );
    }

    #[tokio::test]
    async fn on_auth_failure_forces_refresh_even_when_token_not_near_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // Seed a healthy (not near expiry) record. on_auth_failure
        // must refresh anyway -- the upstream said the token is dead.
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-after-401".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        store
            .on_auth_failure(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .expect("forced refresh should succeed");
        assert_eq!(flow.call_count(), 1);

        // Subsequent `get` returns the new token.
        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-after-401");
    }

    #[tokio::test]
    async fn refresh_failure_surfaces_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::RefreshExpired));
        let store = open_with_flow(&path, flow).await;

        let err = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("oauth refresh failed for anthropic"),
            "expected wrapping prefix, got: {msg}"
        );
        assert!(
            msg.contains("routectl login anthropic"),
            "expected actionable login hint, got: {msg}"
        );
        // The wrapped root cause must include the Anthropic provider's
        // RefreshExpired Display string (its `invalid_grant` bucketing).
        assert!(
            msg.contains("refresh token expired or revoked"),
            "expected RefreshExpired display, got: {msg}"
        );
    }

    #[tokio::test]
    async fn force_refresh_returns_new_record_for_cli() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-cli-refresh".into(),
        )));
        let store = open_with_flow(&path, flow).await;

        let new_rec = store.force_refresh("anthropic").await.unwrap();
        assert_eq!(new_rec.access_token.expose(), "tok-cli-refresh");
        assert!(new_rec.expires_at_unix > unix_now());
    }

    #[tokio::test]
    async fn get_errors_when_provider_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        let err = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no credentials"));
    }

    #[tokio::test]
    async fn get_errors_for_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let err = store
            .get(&SecretRef::OAuth {
                provider: "made-up".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown oauth provider"));
    }

    #[tokio::test]
    async fn get_rejects_non_oauth_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let err = store.get(&SecretRef::Env("FOO".into())).await.unwrap_err();
        assert!(err.to_string().contains("oauth://"));
    }

    #[tokio::test]
    async fn delete_removes_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        store
            .delete(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn logout_returns_true_when_record_existed_and_persists_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();

        let removed = store.logout("anthropic").await.unwrap();
        assert!(removed, "logout should report a record was removed");
        assert!(store.list().await.is_empty());

        // Re-opening from disk must not surface the removed record.
        let reopened = OAuthStore::open(&path).await.unwrap();
        assert!(reopened.list().await.is_empty());
    }

    #[tokio::test]
    async fn logout_returns_false_when_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let removed = store.logout("anthropic").await.unwrap();
        assert!(!removed, "logout on empty store reports no removal");
    }

    #[tokio::test]
    async fn set_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let err = store
            .set(
                &SecretRef::OAuth {
                    provider: "anthropic".into(),
                },
                "tok",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("routectl login"));
    }

    #[tokio::test]
    async fn on_auth_failure_without_record_returns_provider_specific_error() {
        // No record on disk -> force_refresh reads the missing record
        // first and surfaces NotLoggedIn ("...run `routectl login
        // anthropic` first"). Pinned because `CompositeStore` and
        // upstream callers rely on the actionable login hint.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let err = store
            .on_auth_failure(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("routectl login anthropic"),
            "expected provider-specific guidance, got: {msg}"
        );
    }

    #[tokio::test]
    async fn list_returns_all_providers_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        // Future codex provider, written through the store's
        // back-channel. (Not gated on `lookup` -- write_record is
        // pub(crate); only `get` validates.)
        store
            .write_record("codex", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
        assert_eq!(listed, vec!["anthropic", "codex"]); // BTreeMap = sorted
    }

    #[tokio::test]
    async fn open_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        {
            let store = OAuthStore::open(&path).await.unwrap();
            store
                .write_record("anthropic", rec_at(unix_now() + 3600))
                .await
                .unwrap();
        }
        // Re-open and verify state persisted.
        let store2 = OAuthStore::open(&path).await.unwrap();
        let tok = store2
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-abc");
    }

    #[tokio::test]
    async fn write_record_failure_does_not_corrupt_memory_cache() {
        // If the disk save fails, the in-memory cache MUST keep its
        // pre-write state. We construct an OAuthStore by hand whose
        // path has a non-directory component in it -- save_blocking's
        // `create_dir_all` then fails with ENOTDIR, exercising the
        // disk-first ordering invariant.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bad_path = blocker.join("credentials.json");

        let http = reqwest::Client::builder().build().unwrap();
        let store = OAuthStore {
            inner: Arc::new(Inner {
                path: bad_path,
                file: RwLock::new(CredentialsFile::empty()),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                refresh_flow: None,
            }),
        };

        // Pre-populate the in-memory cache so we can verify it is NOT
        // mutated by a failed save.
        store
            .inner
            .file
            .write()
            .await
            .upsert("anthropic", rec_at(unix_now() + 3600));
        let pre_cache: Vec<String> = store
            .inner
            .file
            .read()
            .await
            .providers
            .keys()
            .cloned()
            .collect();
        assert_eq!(pre_cache, vec!["anthropic"]);

        // Try to write a different provider. Save should fail (blocker
        // is a regular file, can't create dir under it). The in-memory
        // cache must not pick up the new "codex" entry.
        let result = store.write_record("codex", rec_at(unix_now() + 3600)).await;
        assert!(result.is_err(), "save should have failed (ENOTDIR)");

        let post_cache: Vec<String> = store
            .inner
            .file
            .read()
            .await
            .providers
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            pre_cache, post_cache,
            "memory cache must not change when disk save fails"
        );
    }
}
