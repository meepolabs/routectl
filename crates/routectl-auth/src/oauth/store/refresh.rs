//! Token refresh with cooldown + transient-error classification.

use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use routectl_core::{Error, Result};

use crate::oauth::file_io;
use crate::oauth::providers;
use crate::oauth::types::{CredentialsFile, TokenRecord, seat_key, unix_now};
use crate::oauth::{OAuthError, OAuthResult};

use super::{OAuthStore, REFRESH_LEAD_SECS};

impl OAuthStore {
    /// Base cooldown after the first transient refresh failure, in
    /// seconds. Each further consecutive failure doubles the window
    /// (`5 << (consecutive - 1)`) up to `COOLDOWN_CAP_SECS`.
    const COOLDOWN_BASE_SECS: u64 = 5;

    /// Maximum per-seat cooldown window, in seconds. Caps the
    /// exponential backoff so a long outage settles at one probe per
    /// minute rather than growing unbounded.
    const COOLDOWN_CAP_SECS: u64 = 60;

    /// Force a refresh for one seat of `provider`, regardless of expiry.
    /// Used by `routectl refresh <provider> [--label <name>]`. `label`
    /// `None` targets the default (unlabeled) seat -- the bare provider
    /// name -- exactly as before; `Some(label)` targets the labeled seat
    /// `provider#label`. Goes through the per-seat single-flight gate so a
    /// 401 storm collapses to one token-endpoint POST. Returns the
    /// freshly-persisted `TokenRecord` so the CLI can report the new
    /// expiry.
    pub async fn force_refresh(&self, provider: &str, label: Option<&str>) -> Result<TokenRecord> {
        // The explicit CLI `routectl refresh` is the operator escape
        // hatch: it BYPASSES the cooldown check (an operator asking for a
        // refresh during an outage must not be told "temporarily
        // unavailable") but still records the outcome, so a transient
        // failure it hits arms the cooldown for the request-time paths.
        self.force_refresh_seat(provider, &seat_key(provider, label), true)
            .await
    }

    /// Force a refresh for a specific `seat` of `provider`, regardless of
    /// expiry. `provider` selects the `OAuthFlow` (the registry is keyed
    /// by provider id); `seat` selects the credentials-map record, lock,
    /// and persistence target (`seat_key(provider, label)`). For an
    /// unlabeled seat the two coincide. Drives both the CLI
    /// `force_refresh` and the trait `on_auth_failure` paths.
    /// `bypass_cooldown` is set only by the CLI escape hatch; the
    /// request-time 401 path (`on_auth_failure`) never bypasses.
    pub(crate) async fn force_refresh_seat(
        &self,
        provider: &str,
        seat: &str,
        bypass_cooldown: bool,
    ) -> Result<TokenRecord> {
        // Validate the provider id up front so an unknown id does not
        // surface as `NotLoggedIn` (which is misleading).
        providers::lookup(provider).map_err(Error::from)?;
        let current = self.read_record(seat).await.map_err(Error::from)?;
        self.refresh_under_lock(provider, seat, &current, true, bypass_cooldown)
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
    /// Concurrency: take the cache write lock BEFORE reading disk, just
    /// like mutation commits. A reload cannot capture an old disk snapshot,
    /// wait behind a refresh commit, then overwrite the rotated cache.
    /// Refresh POSTs still run outside this lock; their conditional commit
    /// checks THIS seat's credential, not whether any reload occurred.
    pub async fn reload_from_disk(&self) -> OAuthResult<()> {
        self.reload_from(file_io::load(&self.inner.path)).await
    }

    /// Keep lock acquisition ahead of polling the disk-read future. Besides
    /// sharing the reload path, this permits a deterministic ordering test.
    async fn reload_from(
        &self,
        load: impl std::future::Future<Output = OAuthResult<CredentialsFile>>,
    ) -> OAuthResult<()> {
        let mut guard = self.inner.file.write().await;
        *guard = load.await?;
        // A successful load clears any degrade marker: the file is
        // readable again, so the next request resolves normally instead of
        // surfacing the stale cause. This is the ENTIRE recovery mechanism
        // for a degraded store -- the arm stays `Some`, the file-watch
        // reload fires this on an operator fix, and the store is live again
        // without a restart. A FAILED load returns early above and leaves
        // both the cache and this marker untouched (stays degraded).
        *self
            .inner
            .load_error
            .write()
            .expect("load_error lock poisoned") = None;
        // Reset trigger: clear the WHOLE cooldown map. A reload is the
        // file-watch recovery escape hatch (an operator may have fixed
        // a revoked credential out-of-band); a stale per-seat cooldown
        // must never mask that recovery, so every seat gets a clean slate.
        self.inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned")
            .clear();
        Ok(())
    }

    /// Cooldown clock. Production reads the wall clock via `unix_now()`;
    /// tests may pin it through the `now_override` seam so backoff-window
    /// expiry is exercised deterministically without sleeping.
    fn now(&self) -> u64 {
        #[cfg(test)]
        {
            let o = self
                .inner
                .now_override
                .load(std::sync::atomic::Ordering::SeqCst);
            if o != 0 {
                return o;
            }
        }
        unix_now()
    }

    /// Test seam: pin the cooldown clock to `now` unix seconds (`0`
    /// restores the wall clock). Only compiled into test builds.
    #[cfg(test)]
    fn set_test_now(&self, now: u64) {
        self.inner
            .now_override
            .store(now, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test seam: snapshot a seat's cooldown as
    /// `(consecutive, next_allowed_unix, suppressed)`, or `None` when the
    /// seat has no active cooldown entry.
    #[cfg(test)]
    fn cooldown_snapshot(&self, seat: &str) -> Option<(u32, u64, u64)> {
        self.inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned")
            .get(seat)
            .map(|c| (c.consecutive, c.next_allowed_unix, c.suppressed))
    }

    /// If `seat` is inside an active cooldown window, bump its suppressed
    /// counter and return the coarse remaining seconds; otherwise return
    /// `None` (the caller proceeds to the network POST). A stale entry
    /// whose window has already elapsed is left in place -- the next
    /// transient failure re-arms it, a success clears it.
    fn cooldown_remaining(&self, seat: &str) -> Option<u64> {
        let now = self.now();
        let mut map = self
            .inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned");
        let cd = map.get_mut(seat)?;
        if now < cd.next_allowed_unix {
            cd.suppressed = cd.suppressed.saturating_add(1);
            Some(cd.next_allowed_unix.saturating_sub(now))
        } else {
            None
        }
    }

    /// Record a transient refresh failure for `seat`: bump the
    /// consecutive count, extend the exponential backoff window, and emit
    /// the entry/extension WARN exactly once (never per suppressed
    /// attempt). Only called for failures the transient classifier
    /// accepts; `RefreshExpired` and hard 4xx never reach here.
    fn record_transient_failure(&self, provider: &str, seat: &str, err: &OAuthError) {
        let now = self.now();
        let reason = cooldown_reason(err);
        let failure_class = refresh_failure_class(err);
        let mut map = self
            .inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned");
        let cd = map.entry(seat.to_string()).or_default();
        cd.consecutive = cd.consecutive.saturating_add(1);
        // `5 << (consecutive - 1)` capped at 60: 5, 10, 20, 40, then 60.
        // The branch (rather than a raw shift) keeps the exponent from
        // overflowing the shift width once `consecutive` grows large
        // during a prolonged outage.
        let cooldown_secs = if cd.consecutive >= 5 {
            Self::COOLDOWN_CAP_SECS
        } else {
            Self::COOLDOWN_BASE_SECS << (cd.consecutive - 1)
        };
        cd.next_allowed_unix = now.saturating_add(cooldown_secs);
        cd.last_error = reason;
        tracing::warn!(
            provider = %provider,
            seat = %seat,
            failure_class = failure_class,
            consecutive_failures = cd.consecutive,
            cooldown_ms = cooldown_secs.saturating_mul(1000),
            reason = %cd.last_error,
            "oauth_refresh_cooldown_entered"
        );
    }

    /// Clear `seat`'s cooldown after a successful refresh and, if one was
    /// active, emit the recovery INFO with the accumulated suppressed
    /// count. No-op (and silent) when the seat had no cooldown.
    fn clear_cooldown_on_success(&self, provider: &str, seat: &str) {
        let prev = self
            .inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned")
            .remove(seat);
        if let Some(cd) = prev {
            tracing::info!(
                provider = %provider,
                seat = %seat,
                consecutive_failures = cd.consecutive,
                suppressed_attempts = cd.suppressed,
                "oauth_refresh_recovered"
            );
        }
    }

    /// Drop `seat`'s cooldown without logging. Used by the login/logout
    /// reset triggers, where the seat's credential state changed out from
    /// under any in-flight cooldown.
    pub(crate) fn clear_cooldown(&self, seat: &str) {
        self.inner
            .refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns mutex poisoned")
            .remove(seat);
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
    pub(crate) async fn refresh_under_lock(
        &self,
        provider: &str,
        seat: &str,
        current: &TokenRecord,
        force: bool,
        bypass_cooldown: bool,
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

        // Cooldown gate: sits AFTER the reread double-check so a
        // concurrent successful refresh (which cleared the seat's
        // cooldown on its commit path) wins over a stale cooldown -- the
        // double-check above already returned the fresh token in that
        // case. During a transient IdP outage this fails fast without a
        // POST until the exponential window elapses. The CLI escape hatch
        // (`bypass_cooldown`) skips the check but still records outcomes.
        //
        // Accepted bounded delay: if the credential has since expired
        // terminally, the armed transient cooldown still short-circuits
        // here, so the failure reads as "temporarily unavailable" until the
        // window (capped at COOLDOWN_CAP_SECS) elapses and the next attempt
        // discovers the terminal state. This staleness is bounded and
        // accepted; the CLI force-refresh path is the escape hatch that
        // bypasses this gate to surface the terminal state immediately.
        if !bypass_cooldown && let Some(remaining) = self.cooldown_remaining(seat) {
            return Err(Error::Auth(format!(
                "oauth refresh temporarily unavailable for {provider}; \
                 retry after ~{remaining}s"
            )));
        }

        // Step 3: actually refresh. Inspect the `OAuthError` variant
        // BEFORE the blanket `Error::Auth` wrap so a transient failure
        // (network / 5xx / malformed body) arms the per-seat cooldown,
        // while a terminal `RefreshExpired` stays entirely outside the
        // cooldown mechanism (never enters it, never suppressed by it --
        // re-login semantics preserved).
        let flow = self.resolve_flow(provider)?;
        let new_rec = match flow
            .refresh_token(self.http(), rec.refresh_token.expose())
            .await
        {
            Ok(rec) => rec,
            Err(e) => {
                if is_transient_refresh_error(&e) {
                    self.record_transient_failure(provider, seat, &e);
                }
                return Err(Error::Auth(format!(
                    "oauth refresh failed for {provider}: {e}; \
                     re-run `routectl login {provider}`"
                )));
            }
        };

        // Step 4: conditionally commit against the credential used for the
        // POST. No-op reloads and unrelated-seat changes cannot discard a
        // rotating refresh token; a same-seat replacement or logout wins.
        self.commit_refresh(provider, seat, rec, new_rec).await
    }

    async fn commit_refresh(
        &self,
        provider: &str,
        seat: &str,
        prior: TokenRecord,
        refreshed: TokenRecord,
    ) -> Result<TokenRecord> {
        let mut guard = self.inner.file.write().await;
        let cached = guard
            .get(seat)
            .ok_or_else(|| Error::from(OAuthError::NotLoggedIn(seat.to_string())))?;
        if !same_credential(cached, &prior) {
            return Ok(cached.clone());
        }
        // Defensive boundary: a degraded store must never write over a
        // file it could not read, even if a caller bypassed read_record.
        if let Some(cause) = self.load_error_cause() {
            return Err(Error::from(OAuthError::Degraded(cause)));
        }
        let seat_owned = seat.to_string();
        let (merged, committed) = file_io::update_under_lock(&self.inner.path, move |cf| {
            conditional_refresh(cf, &seat_owned, &prior, refreshed)
        })
        .await
        .map_err(Error::from)?;
        // Even a skipped write adopts the disk-fresh state: a sibling
        // login/logout wins immediately, without waiting for the watcher.
        *guard = merged;
        let result = guard
            .get(seat)
            .cloned()
            .ok_or_else(|| Error::from(OAuthError::NotLoggedIn(seat.to_string())))?;
        if committed {
            self.clear_cooldown_on_success(provider, seat);
        }
        // Return the actual winning record, including disk-fresh metadata,
        // not the unmerged response from the token endpoint.
        Ok(result)
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

/// Compare credential incarnations, excluding only locally mutable metadata.
/// Tokens alone are insufficient: a login can keep a refresh token (or even
/// an access token) while changing the other grant/account fields. Comparing
/// all endpoint-owned fields also avoids treating project discovery or session
/// backfill as a replacement. No schema change or global generation needed.
fn same_credential(a: &TokenRecord, b: &TokenRecord) -> bool {
    a.access_token == b.access_token
        && a.refresh_token == b.refresh_token
        && a.token_type == b.token_type
        && a.expires_at_unix == b.expires_at_unix
        && a.scopes == b.scopes
        && a.account == b.account
        && a.obtained_at_unix == b.obtained_at_unix
}

/// Runs only inside the advisory lock, against the disk-fresh seat. Login is
/// the only unconditional upsert; refresh must not replace another login or
/// resurrect logout. Preserve local metadata from THIS unchanged incarnation,
/// including concurrent updates/clears, never from the pre-POST snapshot.
fn conditional_refresh(
    cf: &mut CredentialsFile,
    seat: &str,
    prior: &TokenRecord,
    mut refreshed: TokenRecord,
) -> file_io::Mutation<bool> {
    let Some(fresh) = cf.get(seat).filter(|fresh| same_credential(fresh, prior)) else {
        return file_io::Mutation {
            directive: file_io::WriteDirective::Skip,
            report: false,
        };
    };
    refreshed.session_id = fresh.session_id.clone();
    refreshed.cloud_project_id = fresh.cloud_project_id.clone();
    cf.upsert(seat, refreshed);
    file_io::Mutation {
        directive: file_io::WriteDirective::Write,
        report: true,
    }
}

/// Classify an OAuth refresh failure as transient (cooldown-eligible)
/// versus terminal. This is a narrow local read of the existing
/// `OAuthError` shape -- deliberately NOT a provider-wide typed-error
/// redesign.
///
/// - `Network` -> transient (connection reset, DNS, TLS mid-outage).
/// - `TokenEndpoint` -> parse a leading HTTP status from the message
///   (`"429 https://..."`): `429` and `5xx` are transient; `400`/`401`/
///   `403` are terminal (bad request / dead grant). A message with no
///   parseable leading status (a malformed-body / UTF-8 error) is
///   treated as transient -- during an outage a load balancer commonly
///   returns junk bodies, and damping those is the safe direction.
/// - Everything else (notably `RefreshExpired`) -> terminal: never
///   enters the cooldown.
fn is_transient_refresh_error(err: &OAuthError) -> bool {
    match err {
        OAuthError::Network(_) => true,
        OAuthError::TokenEndpoint(msg) => match leading_http_status(msg) {
            Some(status) => status == 429 || (500..=599).contains(&status),
            None => true,
        },
        _ => false,
    }
}

/// Parse a leading HTTP status code from a `TokenEndpoint` message. The
/// Anthropic/antigravity flows format these as `"{status} {url}"`, so
/// the code is the first whitespace-delimited token. Returns `None` when
/// the leading token is not a plausible status (any non-status message).
fn leading_http_status(msg: &str) -> Option<u16> {
    msg.split_whitespace()
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|s| (100..=599).contains(s))
}

/// Coarse failure-class label for the cooldown observability fields.
const fn refresh_failure_class(err: &OAuthError) -> &'static str {
    match err {
        OAuthError::Network(_) => "network",
        OAuthError::TokenEndpoint(_) => "token_endpoint",
        OAuthError::RefreshExpired(_) => "refresh_expired",
        _ => "other",
    }
}

/// Class-only cooldown failure reason for the retained `last_error` and
/// the observability log field. Provider refresh errors format as
/// `"{status} {url}"` (the token-endpoint URL is vendor infrastructure,
/// not a secret, but the log-hygiene posture is class-only), so this
/// derives a bounded label from the coarse failure class plus the leading
/// HTTP status and drops the URL entirely: `"token_endpoint 503"`,
/// `"network"`. The output is a small fixed vocabulary, so no length cap
/// or control-char stripping is needed.
fn cooldown_reason(err: &OAuthError) -> String {
    match err {
        OAuthError::TokenEndpoint(msg) => match leading_http_status(msg) {
            Some(status) => format!("token_endpoint {status}"),
            None => "token_endpoint".to_string(),
        },
        other => refresh_failure_class(other).to_string(),
    }
}

#[cfg(test)]
#[path = "refresh_tests.rs"]
mod refresh_tests;

#[cfg(test)]
#[path = "refresh_race_tests.rs"]
mod refresh_race_tests;

#[cfg(test)]
#[path = "refresh_cooldown_tests.rs"]
mod refresh_cooldown_tests;
