//! `OAuthStore`: routectl's `SecretStore` for `oauth://<provider>` refs.
//!
//! Owns the in-memory cache of `CredentialsFile` and serialises all
//! disk reads/writes through async-aware locks. Read resolution +
//! login writeback ship together with the refresh single-flight gate
//! and the `on_auth_failure -> refresh + persist` hook. The refresh
//! body POSTs through the per-provider `OAuthFlow::refresh_token`,
//! double-checks under a per-provider mutex so concurrent callers do
//! not stampede the token endpoint, and persists every mutation through
//! `file_io::update_under_lock`: take the in-process write guard, re-read
//! the disk-fresh state under a cross-process advisory lock, merge the
//! one-seat change, atomic-write, then commit the returned merged file to
//! the cache. Re-reading under the lock is what stops a stale cache from
//! clobbering a seat a sibling `routectl` process wrote concurrently.
//!
//! # Start-and-degrade (deliberate runtime contract)
//!
//! A long-running `serve` MUST NOT fail startup, nor drop its oauth arm
//! for the whole process, just because `credentials.json` is corrupt,
//! wrong-schema, or wrong-perms. `open_or_degraded` therefore ALWAYS
//! constructs a store on a resolvable path: on a load failure it records
//! the true (sanitized, path-free, value-free) cause in `load_error` and
//! keeps the store PRESENT with an empty in-memory cache. Every request
//! then surfaces that cause instead of a misleading "not logged in", and
//! every write is refused (a store that could not READ the file must never
//! OVERWRITE it -- that would lose a schema-mismatched or momentarily
//! unreadable file). The operator fixes the file, the existing file-watch
//! reload runs `reload_from_disk`, a clean load clears `load_error`, and
//! the store is live again -- with no restart. Only the genuine
//! no-config-dir case (neither `HOME` nor `XDG_CONFIG_HOME` set) drops the
//! arm, via `open_default_degradable` returning `OpenOutcome::NoConfigDir`.
//! The CLI login/whoami/refresh/logout path keeps using the fail-fast
//! `open`/`open_default`, which hard-fail on a broken file by design.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use routectl_core::{Error, Result};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::oauth::file_io;
use crate::oauth::providers;
use crate::oauth::types::{CredentialsFile, TokenRecord, seat_key, unix_now};
use crate::oauth::{OAuthError, OAuthResult};
use crate::{SecretRef, SecretStore};

/// Re-read window: a `get()` call within `near_expiry(REFRESH_LEAD_SECS)`
/// of expiry triggers a refresh through `refresh_under_lock`. 300s
/// matches codex CLI's 5-minute refresh lead in
/// `codex-rs/login/src/auth/manager.rs:87`. Wide enough that the
/// refresh POST + atomic disk write completes before expiry on a
/// healthy network; narrow enough that most requests serve from the
/// in-memory cache without touching the token endpoint. Operators on
/// flaky networks may want to widen this; pinned here as a const
/// because no production driver has yet asked for per-deployment
/// tuning.
pub const REFRESH_LEAD_SECS: u64 = 300;

/// Outcome of a local-only credential probe (`probe_local`). Reports
/// token presence for one provider from the in-memory cache without any
/// network I/O -- consumed by the activation compute to decide whether a
/// routectl-owned provider is usable. Discriminants only: no field ever
/// carries a token, path, or other secret value.
///
/// `#[non_exhaustive]` because future credential sources (e.g. a managed
/// file producer) may add outcome variants without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalProbe {
    /// A usable credential exists: the access token is unexpired, OR a
    /// refresh token is stored (an expired access token with a refresh
    /// token revives transparently on first use, so calling it
    /// deactivated would flap inventory across idle periods).
    Present,
    /// A record exists but the access token is expired AND no refresh
    /// token is stored -- nothing can revive it without a fresh login.
    Expired,
    /// No record for the provider in the cache.
    Missing,
    /// No oauth store exists to probe (HOME/XDG absent). Produced by the
    /// caller when the composite store has no oauth arm, never by
    /// `probe_local` itself.
    StoreUnavailable,
}

/// Outcome of [`OAuthStore::open_default_degradable`]. A resolvable
/// config path ALWAYS yields `Present` (the store may be degraded -- see
/// [`OAuthStore::open_or_degraded`]); only a genuinely absent config
/// directory (neither `HOME` nor `XDG_CONFIG_HOME` set) yields
/// `NoConfigDir`, the one case where `serve` drops the oauth arm.
///
/// Deliberately NOT a typed open-error enum: a degraded store keeps its
/// cause as a sanitized string inside the handle, so this outcome has
/// exactly two branches (a store to wire up, or no config dir at all).
pub enum OpenOutcome {
    /// A store handle to wire into the composite. Live on a clean or
    /// missing file; degraded (present-but-erroring) on a broken one.
    Present(OAuthStore),
    /// No config directory could be located (no `HOME`, no
    /// `XDG_CONFIG_HOME`). The composite drops the oauth arm silently --
    /// configs that only use `env://` / `file://` / `literal:` still run.
    NoConfigDir,
}

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
    /// Sanitized degrade cause, `Some` when this store was opened over a
    /// credentials file that failed to load (`open_or_degraded`). While
    /// `Some`, every request surfaces this cause instead of resolving and
    /// every write is refused (never overwrite a file we could not read).
    /// A successful `reload_from_disk` clears it back to `None`. Path-free
    /// and value-free by construction (see `sanitize_open_error`). Guarded
    /// by a `std::sync::RwLock`: the critical section is a tiny
    /// `Option<String>` clone/set that never crosses an await, so a sync
    /// lock is the right primitive and keeps it off the tokio file lock's
    /// ordering entirely.
    load_error: std::sync::RwLock<Option<String>>,
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
    /// Per-seat transient-failure cooldown. During an IdP outage the
    /// per-seat single-flight gate collapses each request wave to one
    /// refresh POST, but nothing damps successive waves -- every wave
    /// re-POSTs the dead token endpoint. This map applies an in-memory
    /// exponential cooldown per seat: after a transient refresh failure
    /// the seat is barred from re-POSTing until `next_allowed_unix`,
    /// failing fast instead. Restart-forgetful by design (single-host
    /// scale target); guarded by a sync `std::sync::Mutex` because every
    /// access is a tiny CPU-only critical section that never crosses an
    /// await. Keyed by SEAT KEY, mirroring `refresh_locks`. A successful
    /// refresh, a login (`write_record`), a logout (`remove_provider`),
    /// and any `reload_from_disk` clear it (the last clears the WHOLE
    /// map so file-watch recovery is never masked by a stale entry).
    refresh_cooldowns: std::sync::Mutex<BTreeMap<String, SeatCooldown>>,
    /// Test seam: when set, `refresh_under_lock` uses this flow instead
    /// of `providers::lookup`. Lets the unit tests inject a counting
    /// fake without standing up the real token endpoint. The field is
    /// `cfg(test)`-gated so production binaries cannot carry an
    /// override at all.
    #[cfg(test)]
    refresh_flow: Option<Arc<dyn providers::OAuthFlow>>,
    /// Test seam: overrides the cooldown clock. `0` means "use
    /// `unix_now()`". Lets a test advance past `next_allowed_unix`
    /// deterministically instead of sleeping. `cfg(test)`-gated so it
    /// cannot exist in a production binary.
    #[cfg(test)]
    now_override: std::sync::atomic::AtomicU64,
}

/// In-memory transient-failure cooldown state for one seat. Lives in
/// `Inner::refresh_cooldowns`; never persisted (restart-forgetful).
#[derive(Debug, Default)]
struct SeatCooldown {
    /// Consecutive transient failures observed since the last success.
    /// Drives the exponential backoff exponent.
    consecutive: u32,
    /// Unix second before which a refresh POST for this seat is barred.
    next_allowed_unix: u64,
    /// Count of request-time refresh attempts failed fast by this
    /// cooldown since it was entered. Reported once on recovery, never
    /// per suppressed attempt.
    suppressed: u64,
    /// Class-only last transient failure reason (e.g. `"token_endpoint
    /// 503"` / `"network"`, never a URL). Kept for the recovery/entry
    /// observability only.
    last_error: String,
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

    /// Base cooldown after the first transient refresh failure, in
    /// seconds. Each further consecutive failure doubles the window
    /// (`5 << (consecutive - 1)`) up to `COOLDOWN_CAP_SECS`.
    const COOLDOWN_BASE_SECS: u64 = 5;

    /// Maximum per-seat cooldown window, in seconds. Caps the
    /// exponential backoff so a long outage settles at one probe per
    /// minute rather than growing unbounded.
    const COOLDOWN_CAP_SECS: u64 = 60;

    /// Open the store at `path`, loading any existing credentials. A
    /// missing file yields an empty in-memory store -- first run is
    /// not an error. Any OTHER load failure (corrupt / wrong-schema /
    /// wrong-perms / io) hard-fails: this is the fail-fast path the CLI
    /// login/whoami/refresh/logout commands want. `serve` uses
    /// `open_or_degraded` instead so a broken file degrades rather than
    /// crashing the daemon.
    pub async fn open(path: impl Into<PathBuf>) -> OAuthResult<Self> {
        let path: PathBuf = path.into();
        let cf = file_io::load(&path).await?;
        let http = Self::build_http_client()?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                file: RwLock::new(cf),
                load_error: std::sync::RwLock::new(None),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                reload_gen: std::sync::atomic::AtomicU64::new(0),
                refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
                #[cfg(test)]
                refresh_flow: None,
                #[cfg(test)]
                now_override: std::sync::atomic::AtomicU64::new(0),
            }),
        })
    }

    /// Open the store at `path` WITHOUT ever failing on a load error --
    /// the start-and-degrade path used by `serve`. A clean file (or a
    /// missing one, first run) yields a live store. A corrupt /
    /// wrong-schema / wrong-perms / io load failure yields a DEGRADED
    /// store: an empty in-memory cache plus the true (sanitized) cause in
    /// `load_error`. A degraded store surfaces that cause at request time
    /// and refuses writes, and recovers on the next successful
    /// `reload_from_disk` -- all without a restart (see the module doc).
    pub async fn open_or_degraded(path: impl Into<PathBuf>) -> OAuthResult<Self> {
        let path: PathBuf = path.into();
        let (cf, load_error) = match file_io::load(&path).await {
            Ok(cf) => (cf, None),
            Err(e) => (CredentialsFile::empty(), Some(sanitize_open_error(&e))),
        };
        let http = Self::build_http_client()?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                file: RwLock::new(cf),
                load_error: std::sync::RwLock::new(load_error),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                reload_gen: std::sync::atomic::AtomicU64::new(0),
                refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
                #[cfg(test)]
                refresh_flow: None,
                #[cfg(test)]
                now_override: std::sync::atomic::AtomicU64::new(0),
            }),
        })
    }

    /// Build the shared OAuth transport client.
    ///
    /// Disable redirect-following: the refresh POST carries the
    /// long-lived refresh token in the body, and a 307/308 from
    /// the IdP would replay that POST to the redirect target.
    /// Treat any 3xx from the token endpoint as an upstream
    /// failure rather than silently re-sending the secret.
    ///
    /// The client is identity-neutral transport: connect/total
    /// timeouts plus no-redirect. Per-provider identity (the codex
    /// originator/residency/User-Agent, or the Anthropic
    /// claude-cli User-Agent) is stamped per-request inside each
    /// `OAuthFlow`, so one provider's fingerprint never leaks onto
    /// another provider's token endpoint.
    fn build_http_client() -> OAuthResult<reqwest::Client> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(
                Self::HTTP_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(std::time::Duration::from_secs(
                Self::HTTP_TOTAL_TIMEOUT_SECS,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OAuthError::Internal(format!("reqwest client: {e}")))
    }

    /// Open with the default path
    /// (`$XDG_CONFIG_HOME/routectl/credentials.json`).
    pub async fn open_default() -> OAuthResult<Self> {
        let path = file_io::default_path()?;
        Self::open(path).await
    }

    /// Open the default-path store for a long-running `serve`, NEVER
    /// failing startup on a broken credentials file. A resolvable path
    /// ALWAYS yields `Present` (the store may be degraded -- see
    /// `open_or_degraded`); only a genuinely absent config directory
    /// (neither `HOME` nor `XDG_CONFIG_HOME` set) yields `NoConfigDir`,
    /// the one case where the composite drops the oauth arm entirely.
    pub async fn open_default_degradable() -> OAuthResult<OpenOutcome> {
        let Ok(path) = file_io::default_path() else {
            return Ok(OpenOutcome::NoConfigDir);
        };
        Ok(OpenOutcome::Present(Self::open_or_degraded(path).await?))
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

    /// The sanitized degrade cause when this store was opened over a
    /// credentials file that failed to load, else `None`. `Some` gates
    /// every read (surfaces the cause) and every write (refuses to
    /// overwrite an unreadable file); cleared by a successful
    /// `reload_from_disk`.
    fn load_error_cause(&self) -> Option<String> {
        self.inner
            .load_error
            .read()
            .expect("load_error lock poisoned")
            .clone()
    }

    /// Read a record without expiry checking. Internal use only --
    /// the public `get()` enforces near-expiry semantics.
    async fn read_record(&self, provider: &str) -> OAuthResult<TokenRecord> {
        // A degraded store (broken credentials file at open) surfaces the
        // true cause here rather than a misleading `NotLoggedIn`: the file
        // could not be read, so "no credentials for X" would send the
        // operator down the wrong path.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
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

    /// Read the per-credential `session_id` recorded for `provider`
    /// (a seat key), if any. Read-only: no expiry check, no refresh, no
    /// network. Returns `None` when the provider has no stored record
    /// (not logged in) OR when the record carries no `session_id` (a
    /// pre-existing credential minted before session-id support, or one
    /// that has only ever been refreshed). The anthropic-api factory
    /// reads this once at build time to populate
    /// `AnthropicApiConfig.session_id` for the Claude-Code session-id
    /// header.
    pub async fn peek_session_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.session_id.clone())
    }

    /// Read the Cloud Code project id recorded for `provider` (a seat
    /// key), if any. Read-only: no expiry check, no refresh, no network.
    /// Returns `None` when the provider has no stored record (not logged
    /// in) OR when the record carries no `cloud_project_id` (a
    /// credential minted before Cloud Code support, or one that has not
    /// yet resolved a project id). The Gemini provider reads this at
    /// startup to skip the project-id resolution round trip on warm
    /// restarts.
    pub async fn peek_cloud_project_id(&self, provider: &str) -> Option<String> {
        self.inner
            .file
            .read()
            .await
            .get(provider)
            .and_then(|rec| rec.cloud_project_id.clone())
    }

    /// Persist a resolved Cloud Code project id for `provider` (a seat
    /// key). Looks up the existing record, sets `cloud_project_id`, and
    /// writes back atomically using the same disk-first ordering as
    /// `write_record`. Returns `OAuthError::NotLoggedIn` when no record
    /// exists for `provider` -- the Gemini provider must be logged in
    /// before a project id can be cached.
    pub async fn set_cloud_project_id(&self, provider: &str, project_id: &str) -> OAuthResult<()> {
        // A degraded store must never overwrite a file it could not read
        // (a schema-mismatched or momentarily unreadable file would be
        // lost). Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let project_id = project_id.to_string();
        let (merged, found) = file_io::update_under_lock(&self.inner.path, {
            let provider = provider.clone();
            move |cf| match cf.get(&provider).cloned() {
                Some(mut rec) => {
                    rec.cloud_project_id = Some(project_id);
                    cf.upsert(&provider, rec);
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Write,
                        report: true,
                    }
                }
                // Absent from the disk-fresh state (never logged in, or a
                // sibling logged out): do not create a seat -- report it so
                // the caller surfaces NotLoggedIn.
                None => file_io::Mutation {
                    directive: file_io::WriteDirective::Skip,
                    report: false,
                },
            }
        })
        .await?;
        // Commit the merged disk-fresh state to the cache even on the
        // not-found path: a sibling's logout observed on disk must clear
        // the stale in-memory seat immediately, not at the next reload.
        *guard = merged;
        if !found {
            return Err(OAuthError::NotLoggedIn(provider));
        }
        Ok(())
    }

    /// Compare-and-clear the persisted Cloud Code project id for
    /// `provider` (a seat key). Clears the `cloud_project_id` field only
    /// when it equals `expected`, using the same disk-first
    /// `update_under_lock` discipline as `set_cloud_project_id`. Returns
    /// `Ok(true)` when it matched and was cleared (persisted to disk),
    /// `Ok(false)` when the stored id differed, was absent, or the record
    /// itself was missing (no write in any of those cases).
    ///
    /// The equality guard is the whole point: a late failure carrying a
    /// stale id must not wipe a fresh id a concurrent request already
    /// re-resolved. The durable copy is what survives restarts, so the
    /// clear persists rather than only dropping the in-memory value.
    /// A missing record is not an error -- an un-onboarded seat has
    /// nothing to clear.
    pub async fn clear_cloud_project_id_if_matches(
        &self,
        provider: &str,
        expected: &str,
    ) -> OAuthResult<bool> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let expected = expected.to_string();
        let (merged, cleared) = file_io::update_under_lock(&self.inner.path, {
            let provider = provider.clone();
            move |cf| match cf.get(&provider).cloned() {
                Some(mut rec) if rec.cloud_project_id.as_deref() == Some(expected.as_str()) => {
                    rec.cloud_project_id = None;
                    cf.upsert(&provider, rec);
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Write,
                        report: true,
                    }
                }
                // Record present but the id differs or is absent, or no
                // record at all: nothing to clear. Leave the file
                // byte-identical.
                _ => file_io::Mutation {
                    directive: file_io::WriteDirective::Skip,
                    report: false,
                },
            }
        })
        .await?;
        // Commit the merged disk-fresh state to the cache on every path so
        // a sibling's concurrent change observed on disk is not lost.
        *guard = merged;
        Ok(cleared)
    }

    /// Persist a token record. Takes the in-process write guard first, then
    /// merges the one-seat upsert onto the disk-fresh state under the
    /// cross-process advisory lock (`file_io::update_under_lock`), and
    /// commits the returned merged file to the in-memory cache. Re-reading
    /// under the lock is what stops a stale cache from clobbering a seat a
    /// sibling process wrote concurrently; a failed disk write leaves both
    /// halves consistent (the cache is committed only after the write
    /// succeeds). Login upserts UNCONDITIONALLY -- it is the one mutation
    /// that establishes a seat regardless of the prior on-disk state.
    pub(crate) async fn write_record(&self, provider: &str, rec: TokenRecord) -> OAuthResult<()> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let seat_key = provider.clone();
        let (merged, ()) = file_io::update_under_lock(&self.inner.path, move |cf| {
            cf.upsert(&provider, rec);
            file_io::Mutation {
                directive: file_io::WriteDirective::Write,
                report: (),
            }
        })
        .await?;
        *guard = merged;
        // Reset trigger: a login/writeback for this seat supersedes any
        // stale transient cooldown -- the credential state just changed.
        self.clear_cooldown(&seat_key);
        Ok(())
    }

    /// Remove a provider's tokens (used by `routectl logout`). Same
    /// re-read-under-lock merge as `write_record`: the removal targets the
    /// DISK-FRESH state, so a sibling seat written since the cache loaded
    /// survives. Reports whether the seat was present in the disk-fresh
    /// state (`Ok(false)` when absent, preserving first-time-logout
    /// semantics), and writes nothing when there was nothing to remove.
    pub(crate) async fn remove_provider(&self, provider: &str) -> OAuthResult<bool> {
        // A degraded store must never overwrite a file it could not read.
        // Refuse before the lock helper and surface the cause.
        if let Some(cause) = self.load_error_cause() {
            return Err(OAuthError::Degraded(cause));
        }
        let mut guard = self.inner.file.write().await;
        let provider = provider.to_string();
        let seat_key = provider.clone();
        let (merged, was_present) = file_io::update_under_lock(&self.inner.path, move |cf| {
            let was_present = cf.remove(&provider).is_some();
            let directive = if was_present {
                file_io::WriteDirective::Write
            } else {
                file_io::WriteDirective::Skip
            };
            file_io::Mutation {
                directive,
                report: was_present,
            }
        })
        .await?;
        *guard = merged;
        // Reset trigger: a logout for this seat clears any stale
        // cooldown so a subsequent re-login starts from a clean slate.
        self.clear_cooldown(&seat_key);
        Ok(was_present)
    }

    /// Snapshot the set of credential (seat) keys currently in the
    /// in-memory cache -- every key across the `providers` map, including
    /// labeled seats (`provider#label`). The reload coordinator snapshots
    /// this before and after `reload_from_disk` to decide whether the seat
    /// set actually changed (a login/logout adds or removes a key) versus a
    /// routine token-value-only refresh (same keys), gating an expensive
    /// Router rebuild on the former. Read under the same file `RwLock` as
    /// `list`, so the snapshot is consistent with the cache it reflects.
    pub async fn credential_keys(&self) -> std::collections::BTreeSet<String> {
        self.inner
            .file
            .read()
            .await
            .providers
            .keys()
            .cloned()
            .collect()
    }

    /// Read-only credential probe for one provider. Reports token
    /// presence from the in-memory cache WITHOUT any network I/O -- never
    /// calls `get`/`refresh_under_lock`, never touches the token endpoint.
    /// Consumed by the activation compute; the resolution semantics are
    /// deliberately more lenient than `get`'s near-expiry refresh trigger.
    ///
    /// Any seat of the provider (bare or labeled) resolving counts as
    /// `Present`. `Present` when a seat's access token is unexpired
    /// (raw `expires_at_unix > now`, NOT the 300s near-expiry lead) OR a
    /// refresh token is stored. `Expired` when a record exists but every
    /// seat's access token is expired AND carries no refresh token.
    /// `Missing` when no record exists for the provider. Never returns
    /// `StoreUnavailable` -- that is a caller-side value for when no oauth
    /// store exists at all.
    pub async fn probe_local(&self, provider_id: &str) -> LocalProbe {
        let guard = self.inner.file.read().await;
        let seats = guard.seats_for_provider(provider_id);
        if seats.is_empty() {
            return LocalProbe::Missing;
        }
        let now = unix_now();
        for seat in &seats {
            let Some(rec) = guard.get(seat) else {
                continue;
            };
            if rec.is_locally_usable(now) {
                return LocalProbe::Present;
            }
        }
        LocalProbe::Expired
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
    async fn force_refresh_seat(
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
        // Bump the reload generation counter while the write lock is held.
        // Any concurrent `refresh_under_lock` that snapshots the counter
        // before this line and then checks it again (under its own write
        // lock acquisition, which must come after we release here) will
        // see the mismatch and discard its stale refresh result rather
        // than clobbering the freshly-loaded cache.
        self.inner
            .reload_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);
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
    fn clear_cooldown(&self, seat: &str) {
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
    async fn refresh_under_lock(
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

        // Step 3: actually refresh. Inspect the `OAuthError` variant
        // BEFORE the blanket `Error::Auth` wrap so a transient failure
        // (network / 5xx / malformed body) arms the per-seat cooldown,
        // while a terminal `RefreshExpired` stays entirely outside the
        // cooldown mechanism (never enters it, never suppressed by it --
        // re-login semantics preserved).
        let flow = self.resolve_flow(provider)?;
        let mut new_rec = match flow
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

        // Preserve the per-credential `session_id` across token
        // rotation. The OAuthFlow trait has no slot for the prior
        // record, so the codex flow always returns a record whose
        // `session_id` is None on refresh; upstream expects one stable
        // session-id across the credential's lifetime, so refresh
        // preserves the prior value. Backfilling here also covers the
        // v0.7.0 -> v0.7.1
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
            // No intra-process reload raced us: commit under the
            // cross-process advisory lock, merging the one-seat rotation
            // onto the disk-fresh state so a sibling seat survives (the
            // `reload_gen` guard above covers the intra-process reload race;
            // this lock covers the cross-process merge -- they are
            // complementary). NO-RESURRECT: if the seat is absent from the
            // disk-fresh state (a sibling logged it out mid-refresh), that
            // logout is authoritative -- discard the refresh result rather
            // than re-adding the seat. Only login (`write_record`) upserts
            // unconditionally.
            // A degraded store must never commit over a file it could not
            // read. Defensive: every refresh enters through `read_record`
            // (which already gates a degraded store out before the network
            // POST), and `load_error` is only ever set at open and cleared
            // at reload -- so this guard is a belt-and-braces boundary at
            // the actual write site rather than a reachable path today.
            if let Some(cause) = self.load_error_cause() {
                return Err(Error::from(OAuthError::Degraded(cause)));
            }
            let seat_owned = seat.to_string();
            let new_rec_for_commit = new_rec.clone();
            let (merged, seat_present) = file_io::update_under_lock(&self.inner.path, move |cf| {
                if cf.get(&seat_owned).is_some() {
                    cf.upsert(&seat_owned, new_rec_for_commit);
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Write,
                        report: true,
                    }
                } else {
                    file_io::Mutation {
                        directive: file_io::WriteDirective::Skip,
                        report: false,
                    }
                }
            })
            .await
            .map_err(Error::from)?;
            *wguard = merged;
            if !seat_present {
                return Err(Error::from(OAuthError::NotLoggedIn(seat.to_string())));
            }
        }
        // Ok-clear on the commit path: the refresh committed to disk, so
        // any prior cooldown for this seat is cleared here (after the
        // write), not on the network return. Recovery is logged once.
        self.clear_cooldown_on_success(provider, seat);
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

/// Map an OAuth-store LOAD failure to the path-free, value-free cause a
/// degraded store surfaces at request time. Only the failure CLASS (plus
/// the store basename and, for a schema mismatch, the version numbers)
/// survives -- never a variant's raw `Display`, which interpolates the
/// operator's home-directory path (`open <path>: ...`) or the on-disk
/// permission bits. Mirrors the request-time taxonomy in
/// `doctor::sanitize_store_open_error`, and appends the reload hint: a
/// degraded store recovers the moment the file is fixed, without a
/// restart. The `_` arm is itself a class message (not a passthrough), so
/// a future path-bearing `OAuthError` variant fails safe.
fn sanitize_open_error(err: &OAuthError) -> String {
    const STORE_BASENAME: &str = "credentials.json";
    const RELOAD_HINT: &str = "fix the file and it reloads without restart";
    let class = match err {
        OAuthError::SchemaMismatch {
            found, expected, ..
        } => format!(
            "credentials store schema is v{found}; this binary expects v{expected}; \
             upgrade routectl or delete {STORE_BASENAME} and re-run `routectl login`"
        ),
        OAuthError::CorruptedFile { .. } => {
            format!("oauth credentials file ({STORE_BASENAME}) is corrupted")
        }
        OAuthError::Io(_) => {
            format!("oauth credentials file ({STORE_BASENAME}) could not be read")
        }
        _ => format!("oauth credentials store ({STORE_BASENAME}) could not be opened"),
    };
    format!("{class}; {RELOAD_HINT}")
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
                .refresh_under_lock(provider, &seat, &rec, false, false)
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
        self.force_refresh_seat(provider, &seat_key(provider, label.as_deref()), false)
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

    async fn peek_session_id(&self, secret_ref: &SecretRef) -> Option<String> {
        // Non-oauth refs carry no session metadata. Unlike `account_id`,
        // the trait signature returns `Option` (not `Result`), so a
        // non-oauth ref maps to `None` rather than an error -- the
        // caller treats "no session id" identically to "not an oauth
        // ref".
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return None,
        };
        Self::peek_session_id(self, &seat_key(provider, label.as_deref())).await
    }

    async fn peek_cloud_project_id(&self, secret_ref: &SecretRef) -> Option<String> {
        // Non-oauth refs carry no project-id metadata; map to None
        // rather than an error (same pattern as peek_session_id).
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return None,
        };
        Self::peek_cloud_project_id(self, &seat_key(provider, label.as_deref())).await
    }

    async fn set_cloud_project_id(&self, secret_ref: &SecretRef, project_id: &str) -> Result<()> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return Ok(()),
        };
        Self::set_cloud_project_id(self, &seat_key(provider, label.as_deref()), project_id)
            .await
            .map_err(Error::from)
    }

    async fn clear_cloud_project_id_if_matches(
        &self,
        secret_ref: &SecretRef,
        expected: &str,
    ) -> Result<bool> {
        let (provider, label) = match secret_ref {
            SecretRef::OAuth { provider, label } => (provider, label),
            _ => return Ok(false),
        };
        Self::clear_cloud_project_id_if_matches(
            self,
            &seat_key(provider, label.as_deref()),
            expected,
        )
        .await
        .map_err(Error::from)
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
            cloud_project_id: None,
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
        /// Simulate a transient upstream failure (network / 5xx) that
        /// the cooldown classifier accepts. Drives the cooldown tests.
        Transient,
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
        fn new(outcome: RefreshOutcome) -> Self {
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
        fn set_outcome(&self, outcome: RefreshOutcome) {
            *self.outcome.lock().unwrap() = outcome;
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
                RefreshOutcome::RefreshExpired => {
                    Err(OAuthError::RefreshExpired("anthropic".into()))
                }
                RefreshOutcome::Transient => {
                    Err(OAuthError::Network("simulated upstream outage".into()))
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

        let new_rec = store.force_refresh("anthropic", None).await.unwrap();
        assert_eq!(new_rec.access_token.expose(), "tok-cli-refresh");
        assert!(new_rec.expires_at_unix > unix_now());
    }

    #[tokio::test]
    async fn refresh_label_targets_named_seat() {
        // `force_refresh(provider, Some(label))` must refresh ONLY the
        // named seat's record and leave the default seat byte-for-byte
        // intact. Drives the `routectl refresh <provider> --label <name>`
        // store path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        // Both seats healthy: only the forced refresh on seat-b runs.
        seed.write_record(
            "anthropic",
            rec_named("tok-default-orig", unix_now() + 3600),
        )
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
            "tok-b-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        let new_rec = store
            .force_refresh("anthropic", Some("seat-b"))
            .await
            .unwrap();
        assert_eq!(new_rec.access_token.expose(), "tok-b-refreshed");
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // seat-b rotated; the default seat is untouched.
        let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b-refreshed"
        );
        assert_eq!(
            listed["anthropic"].access_token.expose(),
            "tok-default-orig",
            "the default seat must be untouched by a labeled refresh"
        );
    }

    #[tokio::test]
    async fn login_with_label_does_not_overwrite_default_seat() {
        // The login write path persists through `write_record(seat_key)`.
        // Writing a labeled seat after a default is present must leave the
        // default intact -- both keys coexist. Pins the
        // `routectl login <provider> --label <name>` non-overwrite
        // contract at the store layer (the live login flow's only mutation
        // is this `write_record`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_named("tok-default", unix_now() + 3600))
            .await
            .unwrap();

        // Labeled login effect: write under the seat key.
        store
            .write_record(
                &seat_key("anthropic", Some("seat-b")),
                rec_named("tok-seat-b", unix_now() + 3600),
            )
            .await
            .unwrap();

        let listed: BTreeMap<String, TokenRecord> = store.list().await.into_iter().collect();
        assert_eq!(listed.len(), 2, "both seats must be present");
        assert_eq!(listed["anthropic"].access_token.expose(), "tok-default");
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-seat-b"
        );
    }

    #[tokio::test]
    async fn login_without_label_writes_bare_provider_unchanged() {
        // Back-compat pin: a label-less login writes the bare provider
        // key (`seat_key(provider, None) == provider`), byte-for-byte as
        // before. A subsequent labeled write does not move it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record(
                &seat_key("anthropic", None),
                rec_named("tok-default", unix_now() + 3600),
            )
            .await
            .unwrap();

        let listed: Vec<String> = store.list().await.into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            listed,
            vec!["anthropic"],
            "no-label login must write exactly the bare provider key"
        );
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
                load_error: std::sync::RwLock::new(None),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                reload_gen: std::sync::atomic::AtomicU64::new(0),
                refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
                refresh_flow: None,
                now_override: std::sync::atomic::AtomicU64::new(0),
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

    /// The shared OAuth transport client is identity-neutral: it stamps
    /// NO per-provider fingerprint (no codex originator/residency, no
    /// codex User-Agent). Per-provider identity is applied per-request
    /// inside each `OAuthFlow` so one provider's fingerprint never leaks
    /// onto another provider's token endpoint. (The codex fingerprint is
    /// now proven present on the codex POSTs by the `codex_identity`
    /// tests in `providers/codex.rs`.)
    #[tokio::test]
    async fn shared_client_is_identity_neutral() {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        // Arrange: stand up an OAuthStore so its production client
        // builder runs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        // Capture the headers of the next inbound request via a
        // wiremock mock that records the body+headers and answers 200.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        // Act: drive a real request through `store.http()` with no
        // per-request identity stamped.
        let resp = store
            .http()
            .post(server.uri())
            .send()
            .await
            .expect("request send");
        assert_eq!(resp.status().as_u16(), 200);

        // Assert: the recorded request carries NO codex fingerprint.
        let received: Vec<Request> = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "one request reached the mock");
        let req = &received[0];
        let header = |name: &str| -> Option<String> {
            req.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert!(
            header("originator").is_none(),
            "shared client must NOT stamp the codex originator header",
        );
        assert!(
            header("x-openai-internal-codex-residency").is_none(),
            "shared client must NOT stamp the codex residency header",
        );
        let ua = header("user-agent");
        // A None/absent UA also satisfies this: the claim is "the codex UA
        // prefix is not stamped on the shared client," not "some UA is
        // always present." is_none_or(..) makes absence pass by design.
        assert!(
            ua.as_deref()
                .is_none_or(|u| !u.starts_with("codex_cli_rs/")),
            "shared client must NOT stamp the codex User-Agent, got: {ua:?}",
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
        assert_eq!(flow.call_count(), 2, "one refresh per seat");
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

    // ---- peek_session_id ----

    #[tokio::test]
    async fn peek_session_id_returns_per_seat_value_for_labeled_ref() {
        // The labeled ref must resolve THAT seat's session_id, distinct
        // from the default seat's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let mut default = rec_at(unix_now() + 3600);
        default.session_id = Some("session-default".into());
        store.write_record("anthropic", default).await.unwrap();
        let mut seat_b = rec_at(unix_now() + 3600);
        seat_b.session_id = Some("session-seat-b".into());
        store
            .write_record("anthropic#seat-b", seat_b)
            .await
            .unwrap();

        let via_label = SecretStore::peek_session_id(
            &store,
            &SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat-b".into()),
            },
        )
        .await;
        assert_eq!(via_label.as_deref(), Some("session-seat-b"));

        let via_default = SecretStore::peek_session_id(
            &store,
            &SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
        )
        .await;
        assert_eq!(via_default.as_deref(), Some("session-default"));
    }

    #[tokio::test]
    async fn peek_session_id_none_for_missing_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        let sid = SecretStore::peek_session_id(
            &store,
            &SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
        )
        .await;
        assert!(sid.is_none(), "missing record must yield None");
    }

    #[tokio::test]
    async fn peek_session_id_none_for_record_without_session_id() {
        // A record with session_id: None (e.g. a pre-existing credential)
        // yields None.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();

        let sid = SecretStore::peek_session_id(
            &store,
            &SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
        )
        .await;
        assert!(sid.is_none(), "record without session_id must yield None");
    }

    #[tokio::test]
    async fn peek_session_id_none_for_non_oauth_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();

        let sid = SecretStore::peek_session_id(&store, &SecretRef::Env("FOO".into())).await;
        assert!(sid.is_none(), "non-oauth ref must yield None");
    }

    // ---- cloud_project_id ----

    #[tokio::test]
    async fn set_cloud_project_id_then_peek_returns_value() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        // Act
        store
            .set_cloud_project_id("anthropic", "projects/my-project")
            .await
            .unwrap();
        // Assert
        let pid = store.peek_cloud_project_id("anthropic").await;
        assert_eq!(
            pid.as_deref(),
            Some("projects/my-project"),
            "peek after set must return the stored value"
        );
    }

    #[tokio::test]
    async fn set_cloud_project_id_persists_across_reload() {
        // Arrange: write a record, set the project id, reload, verify
        // it survived.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        store
            .set_cloud_project_id("anthropic", "projects/persistent")
            .await
            .unwrap();
        // Reopen from disk.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let pid = reopened.peek_cloud_project_id("anthropic").await;
        assert_eq!(
            pid.as_deref(),
            Some("projects/persistent"),
            "cloud_project_id must survive reload_from_disk"
        );
    }

    #[tokio::test]
    async fn set_cloud_project_id_errors_when_no_record() {
        // Arrange: empty store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        // Act
        let result = store
            .set_cloud_project_id("anthropic", "projects/no-record")
            .await;
        // Assert
        assert!(
            result.is_err(),
            "set_cloud_project_id on a missing record must return an error"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("routectl login anthropic") || msg.contains("no credentials"),
            "expected NotLoggedIn guidance, got: {msg}"
        );
    }

    #[tokio::test]
    async fn peek_cloud_project_id_none_for_missing_record() {
        // Arrange: empty store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        // Act + Assert
        assert!(
            store.peek_cloud_project_id("anthropic").await.is_none(),
            "missing record must yield None"
        );
    }

    #[tokio::test]
    async fn peek_cloud_project_id_via_secret_store_trait_none_for_non_oauth_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let pid = SecretStore::peek_cloud_project_id(&store, &SecretRef::Env("FOO".into())).await;
        assert!(pid.is_none(), "non-oauth ref must yield None");
    }

    #[tokio::test]
    async fn set_cloud_project_id_via_secret_store_trait_non_oauth_ref_is_noop() {
        // Non-oauth refs use the default no-op; must not error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let result =
            SecretStore::set_cloud_project_id(&store, &SecretRef::Env("FOO".into()), "proj").await;
        assert!(
            result.is_ok(),
            "non-oauth ref must be a no-op, not an error"
        );
    }

    // ---- probe_local ----

    /// A record with a specific access-token expiry and an EMPTY refresh
    /// token -- the "no refresh token stored" shape probe_local reads as
    /// non-revivable once expired.
    fn rec_no_refresh(expires_at: u64) -> TokenRecord {
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

    #[tokio::test]
    async fn probe_local_present_when_access_token_unexpired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
    }

    #[tokio::test]
    async fn probe_local_present_when_near_expiry_but_not_yet_expired() {
        // Inside the 300s refresh lead but NOT yet expired: probe_local
        // uses raw `expires_at_unix > now`, so this is Present (no
        // inventory flap on the refresh lead).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_no_refresh(unix_now() + 10))
            .await
            .unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
    }

    #[tokio::test]
    async fn probe_local_present_when_expired_but_refresh_token_stored() {
        // Expired access token but a refresh token is stored: revives
        // transparently on first use, so Present rather than Expired.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        // rec_at seeds a non-empty refresh token ("rtok-xyz").
        store
            .write_record("anthropic", rec_at(unix_now().saturating_sub(10)))
            .await
            .unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
    }

    #[tokio::test]
    async fn probe_local_expired_when_expired_and_no_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_no_refresh(unix_now().saturating_sub(10)))
            .await
            .unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Expired);
    }

    #[tokio::test]
    async fn probe_local_missing_when_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Missing);
    }

    #[tokio::test]
    async fn probe_local_present_when_any_seat_resolves() {
        // The default seat is expired-no-refresh (would be Expired alone),
        // but a labeled seat is healthy: ANY seat resolving counts as
        // Present.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_no_refresh(unix_now().saturating_sub(10)))
            .await
            .unwrap();
        store
            .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
            .await
            .unwrap();
        assert_eq!(store.probe_local("anthropic").await, LocalProbe::Present);
    }

    #[tokio::test]
    async fn probe_local_never_triggers_refresh() {
        // Using the fake OAuthFlow seam: probe_local must NOT invoke the
        // refresh flow for present, near-expiry, or expired inputs. Seed
        // all three seat shapes and assert zero refresh calls.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        // Present (fresh) default seat.
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        // Near-expiry seat (inside the 300s lead) -- get() would refresh
        // this; probe_local must not.
        seed.write_record("anthropic#near", rec_named("tok-near", unix_now() + 10))
            .await
            .unwrap();
        // Expired-no-refresh seat.
        seed.write_record(
            "anthropic#dead",
            rec_no_refresh(unix_now().saturating_sub(10)),
        )
        .await
        .unwrap();
        drop(seed);

        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-should-not-be-minted".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        // Any seat resolving makes the aggregate Present; the point of
        // this test is the refresh-call count, not the discriminant.
        let _ = store.probe_local("anthropic").await;
        assert_eq!(
            flow.call_count(),
            0,
            "probe_local must never touch the refresh flow"
        );
    }

    // ---- cross-process re-read-under-lock merge ----
    //
    // Two `OAuthStore::open` handles on ONE credentials file model the
    // daemon and a `routectl login`/`refresh`/`logout` CLI process writing
    // the same file. A handle whose in-memory cache is stale must NOT erase
    // a seat a sibling wrote since the cache loaded: every mutation re-reads
    // the disk-fresh state under the advisory lock and merges its single-seat
    // change onto it, rather than atomic-renaming a whole-file clone of the
    // stale cache.

    #[tokio::test]
    async fn stale_handle_write_preserves_sibling_seat() {
        // Arrange: two handles open on one empty file. Handle 1's cache is
        // captured empty and never reloaded, so it is stale after handle 2
        // writes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let handle1 = OAuthStore::open(&path).await.unwrap();
        let handle2 = OAuthStore::open(&path).await.unwrap();

        // Act: sibling writes seat B; then the stale handle writes seat A.
        handle2
            .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
            .await
            .unwrap();
        handle1
            .write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
            .await
            .unwrap();

        // Assert: the on-disk file carries BOTH seats. Pre-fix, handle 1's
        // whole-file clone of its stale (empty) cache clobbers seat B.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b",
            "sibling seat B must survive the stale-handle write"
        );
        assert_eq!(
            listed["anthropic"].access_token.expose(),
            "tok-a",
            "seat A must be written"
        );
    }

    #[tokio::test]
    async fn remove_does_not_clobber_sibling_seat() {
        // Arrange: seed seat A so the stale handle's cache holds it; two
        // handles open on it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);
        let handle1 = OAuthStore::open(&path).await.unwrap();
        let handle2 = OAuthStore::open(&path).await.unwrap();

        // Act: sibling writes seat B; the stale handle removes seat A.
        handle2
            .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
            .await
            .unwrap();
        let removed = handle1.remove_provider("anthropic").await.unwrap();

        // Assert: seat A removed, sibling seat B preserved.
        assert!(removed, "seat A was present in the disk-fresh state");
        let reopened = OAuthStore::open(&path).await.unwrap();
        let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
        assert!(
            !listed.contains_key("anthropic"),
            "seat A must be removed from disk"
        );
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b",
            "sibling seat B must survive the removal"
        );
    }

    #[tokio::test]
    async fn remove_absent_seat_reports_false_against_disk_fresh_state() {
        // A logout of a seat absent from the disk-fresh state reports
        // Ok(false) and writes nothing (preserving the remove-absent
        // semantics against the re-read state, not the stale cache).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let removed = store.remove_provider("anthropic").await.unwrap();
        assert!(!removed, "removing an absent seat reports no removal");
    }

    #[tokio::test]
    async fn set_project_id_does_not_clobber_sibling_seat() {
        // Arrange: seed seat A; two handles.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_named("tok-a", unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);
        let handle1 = OAuthStore::open(&path).await.unwrap();
        let handle2 = OAuthStore::open(&path).await.unwrap();

        // Act: sibling writes seat B; the stale handle sets a project id on
        // seat A.
        handle2
            .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
            .await
            .unwrap();
        handle1
            .set_cloud_project_id("anthropic", "projects/foo")
            .await
            .unwrap();

        // Assert: seat A carries the project id, sibling seat B preserved.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
        assert_eq!(
            listed["anthropic"].cloud_project_id.as_deref(),
            Some("projects/foo"),
            "seat A's project id must be written"
        );
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b",
            "sibling seat B must survive set_cloud_project_id"
        );
    }

    #[tokio::test]
    async fn set_project_id_clears_stale_seat_when_sibling_logged_out() {
        // Arrange: seed seat A with a project id on disk, then open a handle
        // whose cache holds it. A sibling logs the seat OUT on disk out of
        // band, leaving the first handle's cache stale.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        seed.set_cloud_project_id("anthropic", "projects/original")
            .await
            .unwrap();
        drop(seed);
        let stale = OAuthStore::open(&path).await.unwrap();
        let sibling = OAuthStore::open(&path).await.unwrap();
        sibling.logout("anthropic").await.unwrap();
        drop(sibling);

        // Precondition: the stale handle's cache still holds seat A -- the
        // sibling logout has not yet been observed through this handle.
        assert_eq!(
            stale.peek_cloud_project_id("anthropic").await.as_deref(),
            Some("projects/original"),
            "stale cache must still hold the seat before the not-found merge"
        );

        // Act: set a project id through the stale handle. The re-read under
        // the lock sees the disk-fresh (empty) state and reports not-found.
        let result = stale
            .set_cloud_project_id("anthropic", "projects/new")
            .await;

        // Assert: the call surfaces NotLoggedIn AND the stale seat is cleared
        // from the in-memory cache immediately -- a subsequent read through
        // the same handle no longer sees it (not deferred to a reload).
        assert!(
            matches!(result, Err(OAuthError::NotLoggedIn(_))),
            "setting a project id on a sibling-logged-out seat must return NotLoggedIn"
        );
        assert!(
            stale.read_record("anthropic").await.is_err(),
            "the stale seat must be cleared from the cache on the not-found path"
        );
        assert!(
            stale.peek_cloud_project_id("anthropic").await.is_none(),
            "a subsequent read through the same handle must not see the stale seat"
        );
    }

    #[tokio::test]
    async fn refresh_commit_does_not_clobber_sibling_seat() {
        // Arrange: seed seat A near-expiry so a `get` triggers a refresh;
        // open a handle with the fake flow (cache holds only seat A).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();
        drop(seed);
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-a-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        // A sibling writes seat B to disk out of band, after the flow-backed
        // handle's cache loaded.
        let sibling = OAuthStore::open(&path).await.unwrap();
        sibling
            .write_record("anthropic#seat-b", rec_named("tok-b", unix_now() + 3600))
            .await
            .unwrap();
        drop(sibling);

        // Act: trigger seat A's refresh through the near-expiry get path.
        let tok = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(tok, "tok-a-refreshed");
        assert_eq!(flow.call_count(), 1, "exactly one refresh fired");

        // Assert: the refresh commit merged onto the disk-fresh state, so the
        // sibling seat survives alongside the rotated seat A.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let listed: BTreeMap<String, TokenRecord> = reopened.list().await.into_iter().collect();
        assert_eq!(
            listed["anthropic"].access_token.expose(),
            "tok-a-refreshed",
            "seat A must carry the refreshed token"
        );
        assert_eq!(
            listed["anthropic#seat-b"].access_token.expose(),
            "tok-b",
            "sibling seat B must survive the refresh commit"
        );
    }

    #[tokio::test]
    async fn refresh_does_not_resurrect_logged_out_seat() {
        // Arrange: seed a seat, then open a flow-backed handle whose cache
        // still holds it. A sibling logs the seat OUT on disk before the
        // handle's refresh commits.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Mint(
            "tok-refreshed".into(),
        )));
        let store = open_with_flow(&path, flow.clone()).await;

        // Sibling logs the seat out on disk out of band.
        let sibling = OAuthStore::open(&path).await.unwrap();
        assert!(sibling.logout("anthropic").await.unwrap());
        drop(sibling);

        // Act: force a refresh from the stale handle. The POST runs, but the
        // commit re-reads the disk-fresh state (seat gone).
        let result = store.force_refresh("anthropic", None).await;

        // Assert: the sibling logout is authoritative -- the refresh must NOT
        // re-add the seat, and the operation surfaces the logged-out state.
        assert!(
            result.is_err(),
            "refresh against a logged-out seat must not succeed"
        );
        assert_eq!(
            flow.call_count(),
            1,
            "the refresh POST ran but its result was discarded"
        );
        let reopened = OAuthStore::open(&path).await.unwrap();
        assert!(
            reopened.list().await.is_empty(),
            "a logged-out seat must not be resurrected on disk"
        );
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

    fn oauth_ref(provider: &str) -> SecretRef {
        SecretRef::OAuth {
            provider: provider.to_string(),
            label: None,
        }
    }

    /// Write raw bytes to a credentials path with owner-only `0600` perms
    /// on Unix, so the file passes the loader's permission hygiene and the
    /// degrade under test is the JSON/schema failure, not the perms check.
    fn write_creds_0600(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn degraded_store_surfaces_perms_cause_not_missing_home() {
        use std::os::unix::fs::PermissionsExt;
        // Arrange: a valid-JSON credentials file with world-readable 0644
        // perms -- the loader refuses it (the file holds refresh tokens).
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Act: the serve start-and-degrade path always constructs a store.
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();
        let msg = store
            .get(&oauth_ref("anthropic"))
            .await
            .unwrap_err()
            .to_string();

        // Assert: the true perms class, NOT the misleading HOME/XDG string,
        // and path-free / perms-value-free.
        assert!(
            msg.contains("could not be read"),
            "expected the perms class, got: {msg}"
        );
        assert!(
            !msg.contains("HOME") && !msg.contains("XDG"),
            "a degraded perms cause must not surface the no-config-dir string: {msg}"
        );
        assert!(
            !msg.contains(&dir_str) && !msg.contains("644"),
            "the cause must be path-free and perms-value-free: {msg}"
        );
        assert!(
            msg.contains("reloads without restart"),
            "expected the recovery hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn degraded_store_surfaces_corrupt_cause() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let path = dir.path().join("credentials.json");
        write_creds_0600(&path, b"<<corrupt-json>>");

        // Act
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();
        let msg = store
            .get(&oauth_ref("anthropic"))
            .await
            .unwrap_err()
            .to_string();

        // Assert
        assert!(
            msg.contains("corrupted"),
            "expected the corrupt class, got: {msg}"
        );
        assert!(
            !msg.contains("HOME") && !msg.contains("XDG"),
            "must not surface the no-config-dir string: {msg}"
        );
        assert!(
            !msg.contains(&dir_str),
            "the cause must be path-free: {msg}"
        );
        assert!(
            msg.contains("reloads without restart"),
            "expected the recovery hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn degraded_store_surfaces_schema_mismatch_cause() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let path = dir.path().join("credentials.json");
        write_creds_0600(&path, br#"{"schema_version":99,"providers":{}}"#);

        // Act
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();
        let msg = store
            .get(&oauth_ref("anthropic"))
            .await
            .unwrap_err()
            .to_string();

        // Assert: the schema class carries the version numbers (permitted)
        // and the re-login guidance, but no filesystem path.
        assert!(
            msg.contains("schema is v99"),
            "expected the found version, got: {msg}"
        );
        assert!(
            msg.contains("expects v1"),
            "expected the wanted version, got: {msg}"
        );
        assert!(
            msg.contains("routectl login"),
            "expected the re-login guidance, got: {msg}"
        );
        assert!(
            !msg.contains(&dir_str),
            "the cause must be path-free: {msg}"
        );
        assert!(
            msg.contains("reloads without restart"),
            "expected the recovery hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn degraded_store_refuses_all_writes_and_preserves_file() {
        // Arrange: a corrupt file the store could not read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_creds_0600(&path, b"<<corrupt-json>>");
        let before = std::fs::read(&path).unwrap();
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();

        // Act / Assert: every mutation is refused with the degrade cause --
        // a store that could not READ the file must never OVERWRITE it.
        assert!(
            matches!(
                store
                    .write_record("anthropic", rec_at(unix_now() + 3600))
                    .await,
                Err(OAuthError::Degraded(_))
            ),
            "write_record must be refused on a degraded store"
        );
        assert!(
            matches!(
                store.remove_provider("anthropic").await,
                Err(OAuthError::Degraded(_))
            ),
            "remove_provider must be refused on a degraded store"
        );
        assert!(
            matches!(
                store.set_cloud_project_id("anthropic", "projects/x").await,
                Err(OAuthError::Degraded(_))
            ),
            "set_cloud_project_id must be refused on a degraded store"
        );

        // The unreadable file must be byte-identical -- no clobber.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a degraded store must not overwrite the file it could not read"
        );
    }

    #[tokio::test]
    async fn corrupt_file_hot_reloads_without_restart() {
        // Arrange: a corrupt file -> degraded store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_creds_0600(&path, b"<<corrupt-json>>");
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();
        // Degraded before the fix: the request errors.
        assert!(
            store.get(&oauth_ref("anthropic")).await.is_err(),
            "a degraded store must error before the file is fixed"
        );

        // Act: the operator fixes the file with a valid, fresh (not
        // near-expiry, so no network refresh) seat, then the existing
        // reload path runs -- exactly what the file-watch coordinator does.
        let valid = serde_json::json!({
            "schema_version": 1,
            "providers": {
                "anthropic": {
                    "access_token": "tok-recovered",
                    "refresh_token": "rtok",
                    "token_type": "Bearer",
                    "expires_at_unix": unix_now() + 3600,
                    "scopes": ["user:inference"],
                    "obtained_at_unix": unix_now()
                }
            }
        });
        write_creds_0600(&path, &serde_json::to_vec_pretty(&valid).unwrap());
        store.reload_from_disk().await.unwrap();

        // Assert: recovered WITHOUT a restart -- the same handle resolves.
        let tok = store.get(&oauth_ref("anthropic")).await.unwrap();
        assert_eq!(tok, "tok-recovered");
    }

    #[tokio::test]
    async fn open_or_degraded_missing_file_is_not_degraded() {
        // A missing file is first-run, NOT a degrade: the request surfaces
        // the normal NotLoggedIn guidance, not a degrade cause.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = OAuthStore::open_or_degraded(&path).await.unwrap();

        let msg = store
            .get(&oauth_ref("anthropic"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("no credentials"),
            "first-run must be NotLoggedIn, got: {msg}"
        );
        assert!(
            !msg.contains("reloads without restart"),
            "a missing file is not a degrade: {msg}"
        );
    }

    #[tokio::test]
    async fn open_or_degraded_valid_file_resolves_like_open() {
        // A clean file loads live: reads resolve exactly as `open` would.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_named("tok-live", unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);

        let store = OAuthStore::open_or_degraded(&path).await.unwrap();
        let tok = store.get(&oauth_ref("anthropic")).await.unwrap();
        assert_eq!(tok, "tok-live");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn open_default_degradable_yields_no_config_dir_without_home_or_xdg() {
        // Arrange: neither HOME nor XDG_CONFIG_HOME set -- the one case
        // that drops the oauth arm entirely.
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("HOME");
        }

        // Act
        let outcome = OAuthStore::open_default_degradable().await;

        // Restore env BEFORE asserting so a failure cannot leak into
        // sibling serial tests.
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        // Assert
        assert!(
            matches!(outcome.unwrap(), OpenOutcome::NoConfigDir),
            "no HOME/XDG must yield NoConfigDir, not a degraded Present store"
        );
    }

    /// Helper: seed a near-expiry record on disk, then open a store whose
    /// refresh path is the injected counting fake. Returns the store.
    async fn seed_near_expiry_with_flow(
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

    fn anthropic_ref() -> SecretRef {
        SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        }
    }

    #[tokio::test]
    async fn transient_failure_enters_cooldown_second_call_skips_flow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
        let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
        store.set_test_now(1_000);

        // First get: the flow fires once and fails transiently, arming
        // the per-seat cooldown (5s base).
        let first = store.get(&anthropic_ref()).await;
        assert!(first.is_err(), "transient refresh failure must surface");
        assert_eq!(flow.call_count(), 1, "first wave POSTs exactly once");

        // Second get inside the cooldown window: must fail fast WITHOUT a
        // second POST. The flow count stays 1.
        let second = store.get(&anthropic_ref()).await;
        let err = second.expect_err("cooldown must fail fast");
        assert!(
            err.to_string().contains("temporarily unavailable"),
            "suppressed error must be the retryable cooldown message: {err}"
        );
        assert_eq!(
            flow.call_count(),
            1,
            "second call within cooldown must not invoke the flow"
        );
    }

    #[tokio::test]
    async fn cooldown_expiry_allows_exactly_one_retry_under_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient).with_yield());
        let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
        store.set_test_now(1_000);

        // Arm the cooldown: one failed POST -> next_allowed = 1005.
        let _ = store.get(&anthropic_ref()).await;
        assert_eq!(flow.call_count(), 1);
        let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
        assert_eq!((consecutive, next_allowed), (1, 1_005));

        // Advance to the boundary (window elapsed) and fire two concurrent
        // callers. The per-seat single-flight lets exactly one through the
        // POST; the other parks on the lock, re-double-checks, and is then
        // suppressed by the freshly re-armed cooldown. Net: +1 POST only.
        store.set_test_now(1_005);
        let ref_a = anthropic_ref();
        let ref_b = anthropic_ref();
        let (a, b) = tokio::join!(store.get(&ref_a), store.get(&ref_b));
        assert!(a.is_err() && b.is_err());
        assert_eq!(
            flow.call_count(),
            2,
            "exactly one retry POST fires past the cooldown window"
        );
        // The retry re-armed the cooldown at the next exponential step
        // (consecutive 2 -> 10s window).
        let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
        assert_eq!((consecutive, next_allowed), (2, 1_015));
    }

    #[tokio::test]
    async fn success_clears_cooldown_and_resets_consecutive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
        let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
        store.set_test_now(1_000);

        // Fail once to arm the cooldown.
        let _ = store.get(&anthropic_ref()).await;
        assert!(store.cooldown_snapshot("anthropic").is_some());

        // Recover: advance past the window, flip the flow to success.
        store.set_test_now(1_005);
        flow.set_outcome(RefreshOutcome::Mint("tok-ok".into()));
        let tok = store.get(&anthropic_ref()).await.unwrap();
        assert_eq!(tok, "tok-ok");
        assert_eq!(flow.call_count(), 2);
        assert!(
            store.cooldown_snapshot("anthropic").is_none(),
            "a successful refresh must clear the seat's cooldown"
        );

        // A subsequent transient failure re-enters at the 5s base, proving
        // consecutive reset to zero (not carried over from before).
        store.set_test_now(2_000);
        store.record_transient_failure("anthropic", "anthropic", &OAuthError::Network("x".into()));
        let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
        assert_eq!(
            (consecutive, next_allowed),
            (1, 2_005),
            "post-recovery backoff restarts at the 5s base"
        );
    }

    #[tokio::test]
    async fn refresh_expired_never_enters_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::RefreshExpired));
        let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
        store.set_test_now(1_000);

        // A terminal RefreshExpired must not arm the cooldown, so both
        // calls attempt a POST (two attempts, no suppression).
        let first = store.get(&anthropic_ref()).await;
        let second = store.get(&anthropic_ref()).await;
        assert!(first.is_err() && second.is_err());
        assert_eq!(
            flow.call_count(),
            2,
            "RefreshExpired must never be suppressed by a cooldown"
        );
        assert!(
            store.cooldown_snapshot("anthropic").is_none(),
            "RefreshExpired must never enter the cooldown"
        );
    }

    #[tokio::test]
    async fn reset_triggers_clear_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // A plain store is enough; the cooldown is armed directly.
        let seed = OAuthStore::open(&path).await.unwrap();
        seed.write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        drop(seed);
        let store = OAuthStore::open(&path).await.unwrap();
        store.set_test_now(1_000);
        let arm = |s: &OAuthStore| {
            s.record_transient_failure("anthropic", "anthropic", &OAuthError::Network("x".into()));
        };

        // reload_from_disk clears the WHOLE map.
        arm(&store);
        assert!(store.cooldown_snapshot("anthropic").is_some());
        store.reload_from_disk().await.unwrap();
        assert!(
            store.cooldown_snapshot("anthropic").is_none(),
            "reload_from_disk must clear the cooldown map"
        );

        // write_record clears the seat.
        arm(&store);
        store
            .write_record("anthropic", rec_at(unix_now() + 3600))
            .await
            .unwrap();
        assert!(
            store.cooldown_snapshot("anthropic").is_none(),
            "write_record must clear the seat's cooldown"
        );

        // remove_provider clears the seat.
        arm(&store);
        store.remove_provider("anthropic").await.unwrap();
        assert!(
            store.cooldown_snapshot("anthropic").is_none(),
            "remove_provider must clear the seat's cooldown"
        );
    }

    #[tokio::test]
    async fn cli_force_refresh_bypasses_active_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let flow = Arc::new(CountingFlow::new(RefreshOutcome::Transient));
        let store = seed_near_expiry_with_flow(&path, flow.clone()).await;
        store.set_test_now(1_000);

        // Arm the cooldown via a request-time refresh.
        let _ = store.get(&anthropic_ref()).await;
        assert_eq!(flow.call_count(), 1);

        // A request-time get() inside the window is suppressed (no POST).
        let _ = store.get(&anthropic_ref()).await;
        assert_eq!(flow.call_count(), 1, "request-time path stays suppressed");

        // The CLI force-refresh escape hatch POSTs despite the cooldown.
        let forced = store.force_refresh("anthropic", None).await;
        assert!(forced.is_err(), "the forced POST still failed transiently");
        assert_eq!(
            flow.call_count(),
            2,
            "CLI force-refresh must bypass the cooldown and attempt the POST"
        );

        // The forced call's transient outcome must still re-arm the
        // cooldown for the request-time paths: consecutive advances 1 -> 2
        // (10s window) at the pinned clock (1_000 + 10 = 1_010).
        let (consecutive, next_allowed, _) = store.cooldown_snapshot("anthropic").unwrap();
        assert_eq!(
            (consecutive, next_allowed),
            (2, 1_010),
            "the bypassed force-refresh still records its transient outcome"
        );
    }

    #[test]
    fn transient_classifier_matches_decision_taxonomy() {
        // Network -> transient.
        assert!(is_transient_refresh_error(&OAuthError::Network(
            "reset".into()
        )));
        // TokenEndpoint 429 / 5xx -> transient.
        assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
            "429 https://idp.example/token".into()
        )));
        assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
            "503 https://idp.example/token".into()
        )));
        // TokenEndpoint 4xx (bad request / dead grant) -> terminal.
        for code in ["400", "401", "403"] {
            assert!(
                !is_transient_refresh_error(&OAuthError::TokenEndpoint(format!(
                    "{code} https://idp.example/token"
                ))),
                "{code} must be terminal"
            );
        }
        // Unparseable TokenEndpoint body -> transient (outage-like).
        assert!(is_transient_refresh_error(&OAuthError::TokenEndpoint(
            "token response is not valid UTF-8".into()
        )));
        // RefreshExpired and other variants -> terminal.
        assert!(!is_transient_refresh_error(&OAuthError::RefreshExpired(
            "anthropic".into()
        )));
        assert!(!is_transient_refresh_error(&OAuthError::NotLoggedIn(
            "anthropic".into()
        )));
    }

    #[test]
    fn cooldown_reason_is_class_only_and_drops_urls() {
        // TokenEndpoint "{status} {url}" -> class + status, no URL.
        assert_eq!(
            cooldown_reason(&OAuthError::TokenEndpoint(
                "503 https://console.anthropic.com/v1/oauth/token".into()
            )),
            "token_endpoint 503"
        );
        // TokenEndpoint with no parseable leading status -> bare class.
        assert_eq!(
            cooldown_reason(&OAuthError::TokenEndpoint(
                "token response is not valid UTF-8".into()
            )),
            "token_endpoint"
        );
        // Network errors carry no endpoint detail worth retaining.
        assert_eq!(
            cooldown_reason(&OAuthError::Network(
                "connection reset by peer to https://idp.example/token".into()
            )),
            "network"
        );
    }

    #[tokio::test]
    async fn cooldown_observability_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store.set_test_now(1_000);
        // Provider refresh errors format as "{status} {url}"; the retained
        // reason and the log field must reduce that to a class-only label
        // with no URL.
        let url = "https://console.anthropic.com/v1/oauth/token";
        let boom = || OAuthError::TokenEndpoint(format!("503 {url}"));

        // Drive the observability surface synchronously through the
        // private state transitions so the captured subscriber sees every
        // event on this thread: one entry, two suppressed attempts, one
        // extension, then recovery.
        let events = routectl_testkit::capture_events(|| {
            store.record_transient_failure("anthropic", "anthropic", &boom());
            assert!(store.cooldown_remaining("anthropic").is_some());
            assert!(store.cooldown_remaining("anthropic").is_some());
            store.record_transient_failure("anthropic", "anthropic", &boom());
            store.clear_cooldown_on_success("anthropic", "anthropic");
        });

        let entered: Vec<_> = events
            .iter()
            .filter(|e| e.message == "oauth_refresh_cooldown_entered")
            .collect();
        assert_eq!(
            entered.len(),
            2,
            "WARN fires once per entry/extension, never per suppressed attempt"
        );
        for e in &entered {
            assert_eq!(e.level, tracing::Level::WARN);
            assert_eq!(e.field("provider"), Some("anthropic"));
            assert_eq!(e.field("seat"), Some("anthropic"));
            assert_eq!(e.field("failure_class"), Some("token_endpoint"));
            assert!(e.field("consecutive_failures").is_some());
            assert!(e.field("cooldown_ms").is_some());
            // Class-only reason: the leading status survives, the URL never
            // reaches the log field.
            assert_eq!(e.field("reason"), Some("token_endpoint 503"));
            assert!(
                !e.field("reason").unwrap().contains(url),
                "cooldown reason must not carry the token-endpoint URL"
            );
        }
        // Entry then extension: 5s then 10s windows.
        assert_eq!(entered[0].field("cooldown_ms"), Some("5000"));
        assert_eq!(entered[1].field("cooldown_ms"), Some("10000"));

        let recovered: Vec<_> = events
            .iter()
            .filter(|e| e.message == "oauth_refresh_recovered")
            .collect();
        assert_eq!(recovered.len(), 1, "recovery INFO fires exactly once");
        assert_eq!(recovered[0].level, tracing::Level::INFO);
        assert_eq!(
            recovered[0].field("suppressed_attempts"),
            Some("2"),
            "recovery reports the accumulated suppressed count"
        );
        assert_eq!(recovered[0].field("consecutive_failures"), Some("2"));
    }
}
