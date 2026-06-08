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
use crate::oauth::types::{seat_key, unix_now, CredentialsFile, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};
use crate::{SecretRef, SecretStore};

/// Re-read window: a `get()` call within `near_expiry(REFRESH_LEAD_SECS)`
/// of expiry triggers a refresh through `refresh_under_lock`. 300s
/// matches codex CLI's 5-minute lead in
/// `codex-rs/login/src/auth/manager.rs:87` -- routectl's chatgpt-oauth
/// refresh path runs through the same upstream path as a real
/// codex CLI, so emitting refresh POSTs at the same expiry-window keeps
/// the temporal fingerprint consistent. Wide enough that the
/// refresh POST + atomic disk write completes before expiry on a
/// healthy network; narrow enough that most requests serve from the
/// in-memory cache without touching the token endpoint. Operators on
/// flaky networks may want to widen this; pinned here as a const
/// because no production driver has yet asked for per-deployment
/// tuning.
pub(crate) const REFRESH_LEAD_SECS: u64 = 300;

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
    /// Per-seat single-flight refresh mutex. Only acquired when a
    /// near-expiry (or forced 401) trigger fires; the lock-holder
    /// refreshes and persists, stragglers re-read the freshly-written
    /// record and return without re-refreshing (double-check pattern).
    /// Keyed by SEAT KEY (`seat_key(provider, label)`), not the bare
    /// provider, so concurrent refreshes on distinct seats of the same
    /// provider proceed independently (a refresh on `anthropic#seat-b`
    /// does not serialize behind one on the default `anthropic` seat),
    /// while concurrent gets on the SAME seat still collapse to one
    /// token-endpoint POST. The unlabeled seat keys as the bare provider
    /// (`seat_key(p, None) == p`), so a single-seat deployment behaves
    /// exactly as before. `BTreeMap` rather than a single global
    /// `tokio::sync::Mutex` so per-seat concurrency is preserved; the
    /// cost is one heap-allocated `Arc<Mutex<()>>` per live seat key
    /// (refresh contention is cents-per-day either way). The map itself
    /// is guarded by a sync `std::sync::Mutex` because get-or-insert is
    /// a tiny CPU-only critical section.
    refresh_locks: std::sync::Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
    /// Monotonic counter bumped by every successful `reload_from_disk`
    /// call (under the file `RwLock` write guard). `refresh_under_lock`
    /// snapshots this before the network POST and re-reads it under the
    /// file write lock before committing the refresh result; if the
    /// counter changed, a reload ran while the POST was in-flight and
    /// brought in a newer on-disk state -- the refresh result is
    /// discarded rather than clobbering the reload. `AtomicU64` so
    /// the snapshot read in `refresh_under_lock` does not need to
    /// acquire the file lock twice (once for the double-check and
    /// once for the final write).
    reload_gen: std::sync::atomic::AtomicU64,
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
        //
        // Default headers carry the codex CLI HTTP fingerprint
        // (originator: codex_cli_rs, x-openai-internal-codex-residency:
        // us, codex-style User-Agent). The chatgpt.com upstream
        // inspects every routectl-emitted request claiming
        // `originator: codex_cli_rs` -- including the OAuth refresh
        // POST -- and invalidates sessions whose client identity drifts
        // from a real codex install. Anthropic and any future non-codex
        // OAuth provider see these too; the headers are inert for them
        // (no impact on the Anthropic token endpoint) but pinning a
        // single client keeps the refresh hot path simple.
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (name, value) in routectl_core::identity::codex::codex_default_headers() {
            // The constants are valid header name/value pairs today.
            // Promote any future regression that breaks them into a
            // process-startup panic so a silent drop cannot crack the
            // codex_cli_rs client parity contract by removing the
            // originator or residency header from the OAuth refresh
            // client without operator-visible signal.
            let header_name = name
                .parse::<reqwest::header::HeaderName>()
                .expect("codex_fingerprint constant must be a valid header name");
            let header_value = reqwest::header::HeaderValue::from_str(value)
                .expect("codex_fingerprint constant must be a valid header value");
            default_headers.insert(header_name, header_value);
        }
        let http = reqwest::Client::builder()
            .user_agent(routectl_core::identity::codex::codex_user_agent())
            .default_headers(default_headers)
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
                reload_gen: std::sync::atomic::AtomicU64::new(0),
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

    /// Read the stable account id recorded for `provider`, if any.
    /// Read-only: no expiry check, no refresh, no network. Returns
    /// `None` when the provider has no stored record (not logged in)
    /// OR when the record carries no `account_id` (some token-endpoint
    /// responses omit it). The `chatgpt_account_id` is stable across
    /// token rotations, so the openai-responses factory reads it once
    /// at build time to populate `OpenAiResponsesConfig.account_id`
    /// when the operator omits `account_id_ref` on an `oauth://` ref.
    pub async fn peek_account_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.account.account_id.clone())
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

    /// Force a refresh for `provider`'s default (unlabeled) seat
    /// regardless of expiry. Used by `routectl refresh <provider>`. Goes
    /// through the per-seat single-flight gate so a 401 storm collapses
    /// to one token-endpoint POST. Returns the freshly-persisted
    /// `TokenRecord` so the CLI can report the new expiry.
    pub async fn force_refresh(&self, provider: &str) -> Result<TokenRecord> {
        // The default seat keys as the bare provider name.
        self.force_refresh_seat(provider, &seat_key(provider, None))
            .await
    }

    /// Force a refresh for a specific `seat` of `provider`, regardless of
    /// expiry. `provider` selects the `OAuthFlow` (the registry is keyed
    /// by provider id); `seat` selects the credentials-map record, lock,
    /// and persistence target (`seat_key(provider, label)`). For an
    /// unlabeled seat the two coincide. Drives both the CLI
    /// `force_refresh` and the trait `on_auth_failure` paths.
    async fn force_refresh_seat(&self, provider: &str, seat: &str) -> Result<TokenRecord> {
        // Validate the provider id up front so an unknown id does not
        // surface as `NotLoggedIn` (which is misleading).
        providers::lookup(provider).map_err(Error::from)?;
        let current = self.read_record(seat).await.map_err(Error::from)?;
        self.refresh_under_lock(provider, seat, &current, true)
            .await
    }

    /// Re-read the on-disk credentials file and overwrite the in-memory
    /// cache from it. Used by the file-watch / SIGHUP coordinator to
    /// pick up tokens minted by a sibling `routectl login` (or any
    /// editor / external writer) without a daemon restart.
    ///
    /// Disk-first ordering invariant (matches `write_record`): the
    /// in-memory cache is only overwritten after a successful disk
    /// read. A parse error or IO failure leaves the existing cache
    /// untouched, so a corrupt mid-write file (or a transiently
    /// missing parent dir during a rename) does not destroy the
    /// previously-loaded credentials.
    ///
    /// Concurrency: the per-provider single-flight refresh mutex is
    /// independent of this lock; a concurrent `get()` that crossed
    /// `near_expiry` may rotate a token while a reload is in flight.
    /// The reload acquires the file `RwLock` exclusively and overwrites
    /// the cache wholesale, so the worst case is "the swap-in cache
    /// briefly forgets a freshly-rotated token". The next `get()` will
    /// re-rotate through the same single-flight gate; the only cost is
    /// at most one extra refresh per reload race, which is bounded by
    /// the operator-driven reload cadence (minutes-to-hours).
    pub async fn reload_from_disk(&self) -> OAuthResult<()> {
        let cf = file_io::load(&self.inner.path).await?;
        let mut guard = self.inner.file.write().await;
        *guard = cf;
        // Bump the reload generation counter while the write lock is held.
        // Any concurrent `refresh_under_lock` that snapshots the counter
        // before this line and then checks it again (under its own write
        // lock acquisition, which must come after we release here) will
        // see the mismatch and discard its stale refresh result rather
        // than clobbering the freshly-loaded cache.
        self.inner
            .reload_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Refresh `seat`'s record under the per-seat single-flight mutex.
    /// `provider` selects the `OAuthFlow` (the static registry is keyed
    /// by provider id); `seat` is the credentials-map key
    /// (`seat_key(provider, label)`) that names the record, the lock, and
    /// the persistence target. For an unlabeled seat the two coincide.
    /// The `current` record is the snapshot the caller saw before
    /// contending for the lock; on the `force` path it is the "dead"
    /// token the upstream rejected, used to short-circuit redundant
    /// rotations when another caller already refreshed the SAME seat.
    ///
    /// Single-flight + double-check pattern:
    /// 1. Acquire the per-SEAT mutex (instantiating it lazily). Distinct
    ///    seats take distinct locks, so a refresh on one seat never
    ///    blocks a refresh on another.
    /// 2. Re-read the in-memory record for THIS seat (writes are
    ///    disk-first under the same lock, so a fresh read reflects any
    ///    winner's persisted refresh).
    /// 3. If the lock-waiter sees the record is no longer stale
    ///    (`!near_expiry` for the get path, or the in-memory access
    ///    token differs from the dead token for the force path),
    ///    return it without touching the network.
    /// 4. Otherwise POST to the upstream's refresh endpoint, persist
    ///    atomically, return the new record.
    async fn refresh_under_lock(
        &self,
        provider: &str,
        seat: &str,
        current: &TokenRecord,
        force: bool,
    ) -> Result<TokenRecord> {
        // Step 1: get-or-insert the per-seat tokio mutex. The map is
        // guarded by a `std::sync::Mutex` -- the critical section is
        // CPU-only (BTreeMap entry + Arc clone) and never crosses an
        // await, so a sync mutex is the right primitive here.
        let lock = {
            let mut locks = self
                .inner
                .refresh_locks
                .lock()
                .expect("refresh_locks mutex poisoned");
            locks
                .entry(seat.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Step 2: double-check after acquiring the lock. Another caller
        // may have refreshed THIS seat while we were parked.
        let rec = self.read_record(seat).await.map_err(Error::from)?;
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

        // Snapshot the reload generation before the network round-trip.
        // If `reload_from_disk` completes while this POST is in-flight,
        // it bumps this counter (under the file write lock). We re-check
        // it under our own file write lock acquisition in step 4 so the
        // comparison and the cache write are atomic with respect to any
        // concurrent reload: either we see the old counter (no reload
        // happened yet, we proceed normally) or the new counter (reload
        // already committed a fresher state, we discard our result).
        let gen_before = self
            .inner
            .reload_gen
            .load(std::sync::atomic::Ordering::Acquire);

        // Step 3: actually refresh.
        let flow = self.resolve_flow(provider)?;
        let mut new_rec = flow
            .refresh_token(self.http(), rec.refresh_token.expose())
            .await
            .map_err(|e| {
                Error::Auth(format!(
                    "oauth refresh failed for {provider}: {e}; \
                     re-run `routectl login {provider}`"
                ))
            })?;

        // Preserve the per-credential `session_id` across token
        // rotation. The OAuthFlow trait has no slot for the prior
        // record, so the codex flow always returns a record whose
        // `session_id` is None on refresh; the upstream upstream
        // expects one stable session-id across the credential's
        // lifetime, so a refresh that flipped it would re-trigger
        // re-authentication. Backfilling here also covers the v0.7.0 -> v0.7.1
        // migration: pre-existing records have None; the next refresh
        // (lazy or forced) does NOT mint a fresh session_id, leaving
        // the per-provider factory path to fill it on first use.
        if new_rec.session_id.is_none() {
            new_rec.session_id = rec.session_id.clone();
        }

        // Step 4: persist atomically. Acquire the file write lock and
        // re-check the reload generation counter BEFORE committing. If
        // `reload_from_disk` ran while we were on the network (indicated
        // by a changed counter), the cache already holds a fresher
        // on-disk state; return that rather than clobbering it with our
        // refresh result (which was derived from the pre-reload token).
        {
            let mut wguard = self.inner.file.write().await;
            let gen_now = self
                .inner
                .reload_gen
                .load(std::sync::atomic::Ordering::Acquire);
            if gen_now != gen_before {
                // A reload committed between our double-check and now.
                // Return the current in-memory record. If it is still
                // near-expiry the next `get()` will re-trigger a refresh
                // through the same gate; cost is at most one extra POST
                // per reload race (bounded by the operator-driven reload
                // cadence of minutes to hours).
                let reloaded = wguard
                    .get(seat)
                    .cloned()
                    .ok_or_else(|| Error::from(OAuthError::NotLoggedIn(seat.to_string())))?;
                return Ok(reloaded);
            }
            // No reload raced us: commit the refresh result disk-first.
            // Disk-first ordering: save before committing to memory so a
            // failed save leaves both halves consistent (same invariant
            // as `write_record`).
            let mut staged = wguard.clone();
            staged.upsert(seat, new_rec.clone());
            file_io::save(&self.inner.path, &staged)
                .await
                .map_err(Error::from)?;
            *wguard = staged;
        }
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
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        // Validate provider is known. The lookup also gives operators
        // the authoritative "unknown oauth provider" message rather
        // than a silent miss. Validation keys on the provider id (the
        // flow registry is per-provider), independent of the seat.
        providers::lookup(provider).map_err(Error::from)?;

        // Resolve the credentials-map record by SEAT KEY: a bare ref
        // (label None) keys as the unlabeled provider record exactly as
        // before; a labeled ref keys this seat's record.
        let seat = seat_key(provider, label.as_deref());
        let rec = self.read_record(&seat).await.map_err(Error::from)?;

        if rec.near_expiry(REFRESH_LEAD_SECS, unix_now()) {
            tracing::debug!(
                provider = %provider,
                seat = %seat,
                expires_at_unix = rec.expires_at_unix,
                "oauth access token near expiry; entering refresh single-flight"
            );
            let refreshed = self
                .refresh_under_lock(provider, &seat, &rec, false)
                .await?;
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
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        // Delete targets only the named seat: a labeled ref removes that
        // seat's record and leaves sibling seats untouched; a bare ref
        // removes the unlabeled record exactly as before.
        self.remove_provider(&seat_key(provider, label.as_deref()))
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    async fn on_auth_failure(&self, secret_ref: &SecretRef) -> Result<()> {
        // The router calls this after an upstream 401 against a
        // credential resolved from this store. Force a refresh -- the
        // upstream said the access token is dead regardless of what
        // `expires_at_unix` claims (clock skew, server-side rotation,
        // revocation). The single-flight gate inside `force_refresh_seat`
        // collapses a 401 storm into one POST. Targets only the named
        // seat: a 401 on a labeled seat force-refreshes that seat's
        // record and leaves sibling seats untouched.
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        self.force_refresh_seat(provider, &seat_key(provider, label.as_deref()))
            .await
            .map(|_| ())
    }

    async fn account_id(&self, secret_ref: &SecretRef) -> Result<Option<String>> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            other => {
                return Err(Error::Auth(format!(
                    "OAuthStore only handles oauth:// refs, got {other}",
                )));
            }
        };
        Ok(self
            .peek_account_id(&seat_key(provider, label.as_deref()))
            .await)
    }

    async fn list_seats(&self, secret_ref: &SecretRef) -> Result<Vec<SecretRef>> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            // Non-oauth refs are single-ref by definition; mirror the
            // trait default rather than erroring (the composite store
            // only routes oauth:// refs here, but a direct caller that
            // hands a non-oauth ref to OAuthStore should still get the
            // single-ref answer, not a hard failure).
            other => return Ok(vec![other.clone()]),
        };
        // A labeled ref pins one seat: the operator already selected it,
        // so enumeration returns just that ref.
        if label.is_some() {
            return Ok(vec![secret_ref.clone()]);
        }
        // A bare pool ref expands to one ref per stored seat (default
        // first, then sorted labels). Each seat key is parsed back into
        // a provider + optional label so the returned refs round-trip
        // through `Display`/`parse`.
        let seat_keys = {
            let guard = self.inner.file.read().await;
            guard.seats_for_provider(provider)
        };
        // No stored seats yet (not logged in): fall back to the single
        // bare ref so the caller's downstream "not logged in" guidance
        // fires instead of an empty pool that silently resolves to
        // nothing.
        if seat_keys.is_empty() {
            return Ok(vec![secret_ref.clone()]);
        }
        Ok(seat_keys
            .into_iter()
            .map(|key| seat_ref_from_key(provider, &key))
            .collect())
    }
}

/// Reconstruct a `SecretRef::OAuth` from a credentials-map seat key.
/// The unlabeled/default seat keys as the bare provider (label None);
/// a labeled seat keys as `provider#label` (the text after the first
/// `#` is the label). Inverse of `seat_key`.
fn seat_ref_from_key(provider: &str, seat_key: &str) -> SecretRef {
    let label = seat_key
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('#'))
        .map(str::to_string);
    SecretRef::OAuth {
        provider: provider.to_string(),
        label,
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
            session_id: None,
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

    /// Tracks simultaneous `refresh_token` invocations so a test can
    /// distinguish concurrent per-seat refreshes from serialized
    /// per-provider ones. `in_flight` is the live count; `max` is the
    /// high-water mark observed across the run.
    #[derive(Clone, Default)]
    struct ConcurrencyGauge {
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
    struct CountingFlow {
        calls: Arc<StdMutex<u32>>,
        outcome: RefreshOutcome,
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
        fn new(outcome: RefreshOutcome) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(0)),
                outcome,
                yield_once: false,
                concurrency: None,
                rendezvous: None,
            }
        }

        fn with_yield(mut self) -> Self {
            self.yield_once = true;
            self
        }

        /// Enable the concurrency gauge (see the `concurrency` field).
        /// Implies `with_yield` so the overlap window is observable.
        fn with_concurrency_gauge(mut self) -> Self {
            self.yield_once = true;
            self.concurrency = Some(ConcurrencyGauge::default());
            self
        }

        /// Attach a rendezvous barrier (see the `rendezvous` field). The
        /// barrier's party count must match the number of concurrent
        /// refresh arms the test drives.
        fn with_rendezvous(mut self, barrier: Arc<tokio::sync::Barrier>) -> Self {
            self.rendezvous = Some(barrier);
            self
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }

        /// Max simultaneous `refresh_token` invocations observed. Only
        /// meaningful when built `with_concurrency_gauge`.
        fn max_in_flight(&self) -> u32 {
            self.concurrency
                .as_ref()
                .map(ConcurrencyGauge::max)
                .unwrap_or(0)
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
                reload_gen: std::sync::atomic::AtomicU64::new(0),
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
                label: None,
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
                label: None,
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
                label: None,
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
            label: None,
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
            label: None,
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
                label: None,
            })
            .await
            .expect("forced refresh should succeed");
        assert_eq!(flow.call_count(), 1);

        // Subsequent `get` returns the new token.
        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
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
                label: None,
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
                label: None,
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
                label: None,
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
                label: None,
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
                    label: None,
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
                label: None,
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
                label: None,
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
                reload_gen: std::sync::atomic::AtomicU64::new(0),
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

    /// `reload_from_disk` happy path: an external writer (sibling
    /// `routectl login`, an editor) updated the credentials file. The
    /// next reload must surface the new record via `list()`.
    #[tokio::test]
    async fn reload_from_disk_picks_up_external_mutation() {
        // Arrange: open a store, then mutate the on-disk file from
        // outside the store handle (mirroring a sibling `routectl
        // login` that writes through its own OAuthStore instance).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = OAuthStore::open(&path).await.unwrap();
        // First run: empty cache.
        assert!(store.list().await.is_empty());
        // External write through a fresh OAuthStore handle pinned to
        // the same path.
        let external = OAuthStore::open(&path).await.unwrap();
        external
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(external);
        // The original handle's cache is still empty until reload.
        assert!(store.list().await.is_empty());

        // Act
        store.reload_from_disk().await.unwrap();

        // Assert: the freshly-loaded cache surfaces the new record.
        let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
        assert_eq!(listed, vec!["anthropic"]);
    }

    /// `reload_from_disk` against a corrupted file (garbage bytes
    /// written between snapshots) must surface the parse error AND
    /// leave the in-memory cache untouched. Mirrors the disk-first
    /// ordering invariant of `write_record`.
    #[tokio::test]
    async fn reload_from_disk_corrupt_file_keeps_cache() {
        // Arrange: seed a healthy record on disk and in memory.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        let pre: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
        assert_eq!(pre, vec!["anthropic"]);

        // Overwrite the file with garbage that still passes the
        // mode-600 hygiene check but fails JSON parse.
        std::fs::write(&path, b"<<corrupt-json>>").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Act
        let err = store.reload_from_disk().await.unwrap_err();

        // Assert: error is a CorruptedFile, cache unchanged.
        match err {
            OAuthError::CorruptedFile { .. } => {}
            other => panic!("expected CorruptedFile, got {other:?}"),
        }
        let post: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            pre, post,
            "memory cache must not change when reload parse fails"
        );
    }

    /// `reload_from_disk` against a missing file (deleted between
    /// snapshots) must succeed with an empty cache -- callers treat
    /// this as a degraded state but it is not a crash. Matches
    /// `file_io::load`'s NotFound -> empty semantics.
    #[tokio::test]
    async fn reload_from_disk_missing_file_returns_empty_cache() {
        // Arrange: seed, then delete the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        // Act
        store
            .reload_from_disk()
            .await
            .expect("reload of missing file should succeed (empty cache)");

        // Assert: cache reflects on-disk truth (nothing).
        assert!(
            store.list().await.is_empty(),
            "missing file must yield empty cache"
        );
    }

    /// The OAuth refresh client must carry the codex CLI HTTP
    /// fingerprint on every request. The chatgpt.com upstream
    /// inspects token-endpoint round-trips too: a refresh POST
    /// missing originator + residency or with a non-codex UA
    /// invalidates the session even though the bearer is fine.
    #[tokio::test]
    async fn refresh_client_carries_codex_fingerprint_headers() {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        // Arrange: stand up an OAuthStore so its production client
        // builder runs (default headers + UA wired from
        // routectl_core::identity::codex).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        // Capture the headers of the next inbound request via a
        // wiremock mock that records the body+headers and answers
        // 200.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        // Act: drive a real request through `store.http()`.
        let resp = store
            .http()
            .post(server.uri())
            .send()
            .await
            .expect("request send");
        assert_eq!(resp.status().as_u16(), 200);

        // Assert: inspect the recorded request headers via wiremock.
        let received: Vec<Request> = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "one request reached the mock");
        let req = &received[0];
        let header = |name: &str| -> Option<String> {
            req.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(
            header("originator").as_deref(),
            Some("codex_cli_rs"),
            "refresh client must claim codex_cli_rs originator",
        );
        assert_eq!(
            header("x-openai-internal-codex-residency").as_deref(),
            Some("us"),
            "refresh client must pin US residency",
        );
        let ua = header("user-agent").expect("UA must be set");
        assert!(
            ua.starts_with("codex_cli_rs/"),
            "refresh client UA must start with codex_cli_rs/, got: {ua}",
        );
    }

    /// Refresh preserves session_id across token rotation. The
    /// OAuthFlow trait has no slot for the prior record; the store
    /// backfills `session_id` from the in-memory `current` record
    /// before persisting the freshly-minted one.
    #[tokio::test]
    async fn refresh_preserves_session_id_across_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // Seed a record with a known session_id and a near-expiry
        // access token so the lazy refresh path fires.
        let seed = OAuthStore::open(&path).await.unwrap();
        let mut seeded = rec_at(unix_now() + 10);
        seeded.session_id = Some("seeded-session-uuid".into());
        seed.write_record("anthropic", seeded).await.unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        // Trigger refresh through `get`.
        let _ = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // The persisted record carries the original session_id.
        let listed = store.list().await;
        assert_eq!(listed.len(), 1);
        let post = &listed[0].1;
        assert_eq!(
            post.session_id.as_deref(),
            Some("seeded-session-uuid"),
            "session_id must be preserved across token rotation",
        );
        assert_eq!(post.access_token.expose(), "tok-refreshed");
    }

    /// A `reload_from_disk` that completes while a refresh POST is
    /// in-flight must win: the refresh result must be discarded and the
    /// reloaded token left in cache. This exercises the generation-counter
    /// guard in `refresh_under_lock` step 4.
    ///
    /// Interleaving: `CountingFlow` yields twice inside `refresh_token`.
    /// The reload arm yields once first (so the refresh task starts and
    /// captures `gen_before`), then calls `reload_from_disk` (bumps gen).
    /// When the refresh task resumes it finds `gen_now != gen_before` and
    /// returns the reloaded record without clobbering the cache.
    #[tokio::test]
    async fn reload_during_refresh_wins_over_stale_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");

        // Seed a near-expiry record on disk so `get` triggers a refresh.
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();
        drop(seed);

        // CountingFlow with yield_once: refresh POST suspends mid-flight
        // so the reload arm can run between gen-snapshot and write-back.
        let flow = Arc::new(
            CountingFlow::new(RefreshOutcome::Mint("tok-from-refresh".into())).with_yield(),
        );
        let store = open_with_flow(&path, flow.clone()).await;

        // Write a newer record to disk via a separate handle -- this is
        // what reload_from_disk will pick up.
        let writer = OAuthStore::open(&path).await.unwrap();
        writer
            .write_record("anthropic", rec_named("tok-from-reload", unix_now() + 7200))
            .await
            .unwrap();
        drop(writer);

        // Run both operations concurrently on the same store. The refresh
        // (triggered by near-expiry) wins the per-provider mutex first,
        // captures gen_before, then yields to let the reload arm advance.
        // The reload arm yields once so the refresh arm can start and
        // reach its yield point before reload runs.
        let store_a = store.clone();
        let store_b = store.clone();
        let (get_result, reload_result) = tokio::join!(
            // Arm A: trigger a refresh via the near-expiry path.
            async move {
                store_a
                    .get(&SecretRef::OAuth {
                        provider: "anthropic".into(),
                        label: None,
                    })
                    .await
            },
            // Arm B: yield once (so A starts and captures gen_before),
            // then reload. This bumps the generation counter before A
            // can acquire the file write lock.
            async move {
                tokio::task::yield_now().await;
                store_b.reload_from_disk().await
            }
        );

        assert!(get_result.is_ok(), "get should succeed: {get_result:?}");
        assert!(
            reload_result.is_ok(),
            "reload should succeed: {reload_result:?}"
        );
        // The refresh endpoint was called exactly once (it just
        // discarded its result due to the gen mismatch).
        assert_eq!(flow.call_count(), 1, "refresh endpoint called exactly once");

        // The in-memory cache must reflect the reloaded token, not the
        // refresh result. The generation counter guard must have forced
        // the refresh arm to discard its stale write-back.
        let listed = store.list().await;
        assert_eq!(listed.len(), 1, "exactly one provider in cache");
        let in_memory_tok = listed[0].1.access_token.expose().to_string();
        assert_eq!(
            in_memory_tok, "tok-from-reload",
            "reload must win over in-flight stale refresh; got: {in_memory_tok}"
        );
    }

    // ---- Labeled-seat resolution + per-seat single-flight ----

    #[tokio::test]
    async fn get_resolves_labeled_seat_token() {
        // Arrange: seed the default seat and a labeled seat with DISTINCT
        // tokens, both fresh so no refresh fires.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_named("tok-default", unix_now() + 3600))
            .await
            .unwrap();
        store
            .write_record(
                "anthropic#seat-b",
                rec_named("tok-seat-b", unix_now() + 3600),
            )
            .await
            .unwrap();

        // Act / Assert: the labeled ref resolves seat-b's token; the
        // bare ref resolves the unlabeled record.
        let seat_b = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            })
            .await
            .unwrap();
        assert_eq!(seat_b, "tok-seat-b");

        let default = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(default, "tok-default");
    }

    #[tokio::test]
    async fn bare_oauth_resolves_unlabeled_seat_unchanged() {
        // Back-compat pin: a single unlabeled seat + bare ref behaves
        // exactly as before -- the seat key for `label: None` is the bare
        // provider, so resolution is byte-for-byte identical to today.
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
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-abc");
    }

    #[tokio::test]
    async fn refresh_single_flight_is_per_seat() {
        // Two distinct near-expiry seats refreshed concurrently must run
        // their refreshes CONCURRENTLY -- per-seat single-flight keys the
        // gate on the seat key, so seat-a's refresh takes a different lock
        // than seat-b's and the two overlap. The concurrency gauge in the
        // fake flow observes max-in-flight == 2 only when both arms are
        // inside `refresh_token` at once; a shared per-provider lock would
        // serialize them (max == 1) even though the total count is 2 in
        // both designs (each seat's double-check still finds its own
        // record stale). The gauge is therefore the discriminating
        // assertion; the count is a secondary check.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_named("tok-a-stale", unix_now() + 10))
            .await
            .unwrap();
        seed.write_record(
            "anthropic#seat-b",
            rec_named("tok-b-stale", unix_now() + 10),
        )
        .await
        .unwrap();
        drop(seed);

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let flow = Arc::new(
            CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into()))
                .with_concurrency_gauge()
                .with_rendezvous(barrier.clone()),
        );
        let store = open_with_flow(&path, flow.clone()).await;
        let store2 = store.clone();
        let r_a = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };
        let r_b = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        };

        // Bound the join with a timeout: with per-seat locks both arms
        // reach the rendezvous and proceed; a shared per-provider lock
        // parks the second arm on the lock so it never reaches the
        // barrier, deadlocking -- the timeout turns that into a loud
        // failure rather than a silent pass.
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            tokio::join!(async move { store.get(&r_a).await }, async move {
                store2.get(&r_b).await
            })
        })
        .await
        .expect(
            "per-seat single-flight must let both seats refresh concurrently; \
             a shared per-provider lock would deadlock the rendezvous barrier",
        );
        assert_eq!(a.unwrap(), "tok-refreshed");
        assert_eq!(b.unwrap(), "tok-refreshed");
        assert_eq!(
            flow.max_in_flight(),
            2,
            "distinct seats must refresh concurrently: a shared per-provider \
             lock would serialize them to max-in-flight 1"
        );
        assert_eq!(flow.call_count(), 2, "one refresh per seat",);
    }

    #[tokio::test]
    async fn concurrent_get_same_seat_collapses_to_one_refresh() {
        // Regression pin for the labeled-seat path: two concurrent gets
        // on the SAME labeled seat must still collapse to one refresh
        // through that seat's single-flight gate (mirrors the unlabeled
        // `concurrent_get_calls_collapse_to_single_refresh`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record(
            "anthropic#seat-b",
            rec_named("tok-b-stale", unix_now() + 10),
        )
        .await
        .unwrap();
        drop(seed);

        let flow =
            Arc::new(CountingFlow::new(RefreshOutcome::Mint("tok-refreshed".into())).with_yield());
        let store = open_with_flow(&path, flow.clone()).await;
        let store2 = store.clone();
        let r = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        };
        let r2 = r.clone();

        let (a, b) = tokio::join!(async move { store.get(&r).await }, async move {
            store2.get(&r2).await
        });
        assert_eq!(a.unwrap(), "tok-refreshed");
        assert_eq!(b.unwrap(), "tok-refreshed");
        assert_eq!(
            flow.call_count(),
            1,
            "same-seat concurrent gets must collapse to one refresh"
        );
    }

    #[tokio::test]
    async fn on_auth_failure_targets_only_the_named_seat() {
        // A 401 on a labeled seat force-refreshes that seat's record and
        // leaves the sibling default seat untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        // Both seats healthy: only the force path on seat-b should run.
        seed.write_record("anthropic", rec_named("tok-a-orig", unix_now() + 3600))
            .await
            .unwrap();
        seed.write_record(
            "anthropic#seat-b",
            rec_named("tok-b-orig", unix_now() + 3600),
        )
        .await
        .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-b-rotated".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        store
            .on_auth_failure(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            })
            .await
            .expect("forced refresh of seat-b should succeed");
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // seat-b rotated; the default seat is byte-for-byte unchanged.
        let seat_b = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            })
            .await
            .unwrap();
        assert_eq!(seat_b, "tok-b-rotated");
        let default = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(
            default, "tok-a-orig",
            "the default seat must be untouched by a 401 on seat-b"
        );
    }

    #[tokio::test]
    async fn session_id_preserved_per_seat_across_refresh() {
        // seat-b's session_id must survive its own refresh and be
        // independent of the default seat's session_id. Per-seat map
        // keys make preservation automatic: the refresh reads and
        // re-writes the SAME seat's record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        // Default seat: distinct session id, fresh (no refresh).
        let mut default = rec_named("tok-a", unix_now() + 3600);
        default.session_id = Some("session-default".into());
        seed.write_record("anthropic", default).await.unwrap();
        // seat-b: distinct session id, near-expiry so its refresh fires.
        let mut seat_b = rec_named("tok-b-stale", unix_now() + 10);
        seat_b.session_id = Some("session-seat-b".into());
        seed.write_record("anthropic#seat-b", seat_b).await.unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-b-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        // Trigger seat-b's refresh via the near-expiry get path.
        let _ = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            })
            .await
            .unwrap();
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // Read both seats back from the in-memory cache.
        let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
        assert_eq!(
            listed["anthropic#seat-b"].session_id.as_deref(),
            Some("session-seat-b"),
            "seat-b's session_id must survive its own refresh"
        );
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b-refreshed"
        );
        assert_eq!(
            listed["anthropic"].session_id.as_deref(),
            Some("session-default"),
            "the default seat's session_id must be independent and untouched"
        );
    }

    // ---- list_seats ----

    #[tokio::test]
    async fn oauth_list_seats_returns_default_plus_labeled_refs() {
        // A bare pool ref expands to one SecretRef per stored seat:
        // default first, then labeled seats in sorted order.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        store
            .write_record("anthropic#seat-b", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        store
            .write_record("anthropic#alpha", rec_at(unix_now() + 3600))
            .await
            .unwrap();

        let seats = store
            .list_seats(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();

        assert_eq!(
            seats,
            vec![
                SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: None,
                },
                SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: Some("alpha".into()),
                },
                SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: Some("seat-b".into()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn oauth_list_seats_on_labeled_ref_returns_just_that_seat() {
        // An already-pinned ref returns only itself -- the operator
        // selected the seat, so enumeration does not widen it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        store
            .write_record("anthropic#seat-b", rec_at(unix_now() + 3600))
            .await
            .unwrap();

        let pinned = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some("seat-b".into()),
        };
        let seats = store.list_seats(&pinned).await.unwrap();
        assert_eq!(seats, vec![pinned]);
    }

    #[tokio::test]
    async fn oauth_list_seats_no_record_falls_back_to_single_ref() {
        // No stored seats (not logged in): enumeration returns the bare
        // ref so downstream "not logged in" guidance fires rather than an
        // empty pool that resolves to nothing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        let bare = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };
        let seats = store.list_seats(&bare).await.unwrap();
        assert_eq!(seats, vec![bare]);
    }
}
