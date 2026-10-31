//! Scripted refresh seam for tests: an `OAuthStore` whose token-endpoint
//! POST is replaced by a canned result, so a caller's tests can drive every
//! `refresh_if_due` outcome without a real identity provider. The whole
//! module is compiled only under `cfg(any(test, feature = "test-utils"))`;
//! the feature is enabled through dev-dependencies alone, so a production
//! build carries no override.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::oauth::file_io;
use crate::oauth::providers::{AuthParams, OAuthFlow};
use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord, unix_now};
use crate::oauth::{OAuthError, OAuthResult};

use super::{Inner, OAuthStore};

/// What the scripted token endpoint answers on the next refresh POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedRefresh {
    /// The refresh succeeds and mints a record with this access token and
    /// an expiry one hour from now.
    Mint(String),
    /// The identity provider refuses the refresh token itself.
    RefreshExpired,
    /// The POST fails at the transport layer (connection reset, DNS).
    Transient,
    /// The token endpoint answers with this HTTP status, formatted the way
    /// the provider flows report it (`"{status} {url}"`).
    TokenEndpoint(u16),
}

impl ScriptedRefresh {
    /// The `refresh_token` result this script stands for.
    pub(super) fn to_result(&self) -> OAuthResult<TokenRecord> {
        match self {
            Self::Mint(access) => Ok(token_record(access, unix_now() + 3600)),
            Self::RefreshExpired => Err(OAuthError::RefreshExpired("scripted".into())),
            Self::Transient => Err(OAuthError::Network("scripted upstream outage".into())),
            Self::TokenEndpoint(status) => Err(OAuthError::TokenEndpoint(format!(
                "{status} https://idp.example/token"
            ))),
        }
    }
}

/// A synthetic token record with `access` as its access token.
pub(super) fn token_record(access: &str, expires_at_unix: u64) -> TokenRecord {
    TokenRecord {
        access_token: SecretToken::new(access),
        refresh_token: SecretToken::new("rtok-xyz"),
        token_type: "Bearer".into(),
        expires_at_unix,
        scopes: vec!["user:inference".into()],
        account: AccountInfo::default(),
        obtained_at_unix: 0,
        session_id: None,
        cloud_project_id: None,
    }
}

/// Token-endpoint stand-in that answers every refresh POST from a mutable
/// script and counts the POSTs it received. Shared by every provider on
/// the store it is installed in.
#[derive(Debug)]
pub struct ScriptedRefreshFlow {
    script: Mutex<ScriptedRefresh>,
    posts: AtomicU32,
}

impl ScriptedRefreshFlow {
    /// A flow that answers with `script` until [`Self::set_script`]
    /// changes it.
    pub const fn new(script: ScriptedRefresh) -> Self {
        Self {
            script: Mutex::new(script),
            posts: AtomicU32::new(0),
        }
    }

    /// Replace the answer for every later refresh POST.
    pub fn set_script(&self, script: ScriptedRefresh) {
        *self.script.lock().expect("script mutex poisoned") = script;
    }

    /// Refresh POSTs received so far.
    pub fn post_count(&self) -> u32 {
        self.posts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OAuthFlow for ScriptedRefreshFlow {
    fn provider_id(&self) -> &'static str {
        "scripted"
    }
    fn display_name(&self) -> &'static str {
        "Scripted refresh"
    }
    fn auth_url(&self, _params: &AuthParams<'_>) -> url::Url {
        unimplemented!("the scripted flow only answers refresh POSTs")
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
        unimplemented!("the scripted flow only answers refresh POSTs")
    }
    async fn refresh_token(
        &self,
        _http: &reqwest::Client,
        _refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        let script = self.script.lock().expect("script mutex poisoned").clone();
        script.to_result()
    }
}

impl OAuthStore {
    /// Open the store at `path` like [`OAuthStore::open`], but answer every
    /// refresh POST from `flow` instead of the provider's token endpoint.
    #[doc(hidden)]
    pub async fn open_with_scripted_refresh_for_tests(
        path: impl Into<PathBuf>,
        flow: Arc<ScriptedRefreshFlow>,
    ) -> OAuthResult<Self> {
        let path: PathBuf = path.into();
        let cf = file_io::load(&path).await?;
        let http = Self::build_http_client()?;
        let flow: Arc<dyn OAuthFlow> = flow;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                file: RwLock::new(cf),
                load_error: std::sync::RwLock::new(None),
                http,
                refresh_locks: std::sync::Mutex::new(BTreeMap::new()),
                reload_gen: AtomicU64::new(0),
                refresh_cooldowns: std::sync::Mutex::new(BTreeMap::new()),
                refresh_flow: Some(flow),
                now_override: AtomicU64::new(0),
            }),
        })
    }

    /// Pin the refresh-cooldown clock to `now` unix seconds (`0` restores
    /// the wall clock). Governs the cooldown window only: whether a token
    /// is due or expired is always judged against the wall clock.
    #[doc(hidden)]
    pub fn set_cooldown_clock_for_tests(&self, now: u64) {
        self.inner.now_override.store(now, Ordering::SeqCst);
    }

    /// The pinned cooldown clock, or `None` when the wall clock applies.
    pub(super) fn pinned_now(&self) -> Option<u64> {
        match self.inner.now_override.load(Ordering::SeqCst) {
            0 => None,
            pinned => Some(pinned),
        }
    }
}

#[cfg(test)]
#[path = "scripted_tests.rs"]
mod scripted_tests;
