//! `OAuthStore`: routectl's `SecretStore` for `oauth://<provider>` refs.
//!
//! Owns the in-memory cache of `CredentialsFile` and serialises all
//! disk reads/writes through async-aware locks. PR1 ships read-only
//! resolution + login writeback; PR2 adds the refresh single-flight
//! and on_auth_failure -> refresh + persist hook.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use routectl_core::{Error, Result};
use tokio::sync::RwLock;

use crate::oauth::file_io;
use crate::oauth::providers;
use crate::oauth::types::{unix_now, CredentialsFile, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};
use crate::{SecretRef, SecretStore};

/// Re-read window: a `get()` call within `near_expiry(REFRESH_LEAD_SECS)`
/// triggers refresh in PR2. PR1 reports an error and tells the operator
/// to re-login.
pub(crate) const REFRESH_LEAD_SECS: u64 = 60;

/// Routectl-managed OAuth credentials store. Cheap to clone; the
/// inner state is `Arc`-shared so multiple `Provider` instances can
/// share one store (and the future single-flight refresh gate).
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
}

impl OAuthStore {
    /// Open the store at `path`, loading any existing credentials. A
    /// missing file yields an empty in-memory store -- first run is
    /// not an error.
    pub async fn open(path: impl Into<PathBuf>) -> OAuthResult<Self> {
        let path: PathBuf = path.into();
        let cf = file_io::load(&path).await?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| OAuthError::Internal(format!("reqwest client: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                file: RwLock::new(cf),
                http,
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
            // PR1: no refresh yet. Tell the operator clearly.
            // PR2 replaces this branch with refresh + return new token.
            tracing::warn!(
                provider = %provider,
                expires_at_unix = rec.expires_at_unix,
                "oauth access token near expiry; re-run `routectl login {}` to mint a new token",
                provider,
            );
            return Err(Error::Auth(format!(
                "oauth access token for `{provider}` is near expiry or expired; \
                 re-run `routectl login {provider}` to mint a new token",
            )));
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
        // Use the provider name from the SecretRef so the operator gets
        // an actionable message ("re-run routectl login anthropic")
        // instead of generic guidance. PR1 has no refresh path; PR2
        // wires this into the single-flight refresh gate and returns
        // Ok(()) on a successful refresh.
        let provider = match secret_ref {
            SecretRef::OAuth { provider } => provider.as_str(),
            _ => "<unknown>",
        };
        Err(Error::Auth(format!(
            "oauth token refresh is not yet supported; \
             re-run `routectl login {provider}` to obtain a fresh token",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord};

    fn rec_at(expires_at: u64) -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new("tok-abc"),
            refresh_token: SecretToken::new("rtok-xyz"),
            token_type: "Bearer".into(),
            expires_at_unix: expires_at,
            scopes: vec!["user:inference".into()],
            account: AccountInfo::default(),
            obtained_at_unix: 0,
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
    async fn get_errors_when_near_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", rec_at(unix_now() + 10))
            .await
            .unwrap();

        let err = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("near expiry") && msg.contains("routectl login anthropic"),
            "expected near-expiry guidance, got: {msg}"
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
    async fn on_auth_failure_reports_provider_in_message() {
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
