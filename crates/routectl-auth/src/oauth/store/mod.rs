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

use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::oauth::file_io;
use crate::oauth::types::{CredentialsFile, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};

#[cfg(test)]
use crate::oauth::providers;

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

mod crud;
mod refresh;
mod seat;

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "open_tests.rs"]
mod open_tests;
