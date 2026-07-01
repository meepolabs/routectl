//! `CompositeStore`: dispatch `SecretRef` schemes between routectl's
//! two `SecretStore` impls.
//!
//! `oauth://<provider>` -> `routectl_auth::OAuthStore` (reads the
//! routectl-managed credentials.json, refreshes per token lifecycle).
//! `env://`, `file://`, `literal:` -> `MemoryStore` (the default
//! resolver: process env, on-disk secret files, inline literals).
//!
//! All `SecretStore` impls in this crate go through `CompositeStore`
//! when the binary's `oauth` feature is on. The trait dispatch is
//! `match` on the `SecretRef` variant -- not a fallback chain --
//! so each scheme has exactly one resolver responsible for it.

use std::sync::Arc;

use async_trait::async_trait;
use routectl_auth::{MemoryStore, OAuthStore, SecretRef, SecretStore};
use routectl_core::Result;

/// Composite resolver wired by `routectl serve` / `test` / `config`.
/// Cheap to clone (both inner stores are Arc-shared internally).
///
/// `oauth` is optional so that operators running configs that only use
/// `env://` / `file://` / `literal:` refs can still bring up routectl
/// in an environment where neither `HOME` nor `XDG_CONFIG_HOME` is set
/// (e.g. minimal CI containers, sandboxed test runners). When `oauth`
/// is `None`, attempts to resolve an `oauth://` ref return a clear
/// `Error::Auth` instead of the binary failing to start.
#[derive(Clone)]
pub struct CompositeStore {
    oauth: Option<OAuthStore>,
    fallback: MemoryStore,
}

impl CompositeStore {
    /// Build a CompositeStore at the default credentials path
    /// (`$XDG_CONFIG_HOME/routectl/credentials.json`). If neither
    /// `HOME` nor `XDG_CONFIG_HOME` is set the OAuth arm is dropped
    /// (with a `tracing::warn!`) and only the MemoryStore arm is wired
    /// up. Configs that don't use `oauth://` refs continue to work;
    /// configs that do will surface a clear `Error::Auth` at request
    /// time.
    pub async fn open_default() -> Result<Self> {
        match OAuthStore::open_default().await {
            Ok(oauth) => Ok(Self::with_oauth(MemoryStore::new(), oauth)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "OAuth credentials store unavailable; oauth:// refs will fail to resolve"
                );
                Ok(Self::without_oauth(MemoryStore::new()))
            }
        }
    }

    /// Build a CompositeStore at an explicit path. Used by tests and
    /// by future "alternate credential profile" flags.
    pub async fn open_at(path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let oauth = OAuthStore::open(path)
            .await
            .map_err(|e| routectl_core::Error::Auth(e.to_string()))?;
        Ok(Self::with_oauth(MemoryStore::new(), oauth))
    }

    /// Build a CompositeStore from an already-constructed memory store
    /// + OAuth store. The common construction path.
    fn with_oauth(fallback: MemoryStore, oauth: OAuthStore) -> Self {
        Self {
            oauth: Some(oauth),
            fallback,
        }
    }

    /// Build a CompositeStore with only the MemoryStore arm. Used when
    /// the OAuth store cannot be opened (no HOME/XDG) but the operator's
    /// config might not need it.
    fn without_oauth(fallback: MemoryStore) -> Self {
        Self {
            oauth: None,
            fallback,
        }
    }

    /// Direct access to the inner OAuth store. Used by `routectl
    /// login` / `whoami` / `logout` subcommands to read+write the
    /// credentials file without going through the trait surface.
    /// Returns `None` when the OAuth arm was dropped at construction
    /// because neither `HOME` nor `XDG_CONFIG_HOME` was set.
    pub fn oauth(&self) -> Option<&OAuthStore> {
        self.oauth.as_ref()
    }

    /// Owned `Arc<OAuthStore>` handle for the inner OAuth store, if
    /// any. Used by the file-watch / SIGHUP coordinator so a reload
    /// task can call `OAuthStore::reload_from_disk()` without holding
    /// a borrow into the (possibly cloned) CompositeStore. Returns
    /// `None` when the OAuth arm was dropped at construction. The
    /// returned Arc shares the same `Inner` (and therefore the same
    /// in-memory cache + per-provider single-flight mutexes) as every
    /// other handle to this store -- a reload here is observable to
    /// all in-flight `get()` callers.
    pub fn oauth_store(&self) -> Option<Arc<OAuthStore>> {
        self.oauth.as_ref().map(|s| Arc::new(s.clone()))
    }
}

/// Build the `Error::Auth` returned when an `oauth://` ref is resolved
/// against a CompositeStore whose OAuth arm was dropped at construction
/// (no HOME / no XDG_CONFIG_HOME). Centralized so the error message
/// stays consistent across `get` / `set` / `delete` / `on_auth_failure`.
fn oauth_unavailable_err() -> routectl_core::Error {
    routectl_core::Error::Auth(
        "OAuth store unavailable: neither HOME nor XDG_CONFIG_HOME is set, so \
         routectl cannot locate `credentials.json`. Use env:// / file:// / \
         literal: refs, or set XDG_CONFIG_HOME and re-run."
            .into(),
    )
}

#[async_trait]
impl SecretStore for CompositeStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        match secret_ref {
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.get(secret_ref).await,
                None => Err(oauth_unavailable_err()),
            },
            _ => self.fallback.get(secret_ref).await,
        }
    }

    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()> {
        match secret_ref {
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.set(secret_ref, value).await,
                None => Err(oauth_unavailable_err()),
            },
            _ => self.fallback.set(secret_ref, value).await,
        }
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<()> {
        match secret_ref {
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.delete(secret_ref).await,
                None => Err(oauth_unavailable_err()),
            },
            _ => self.fallback.delete(secret_ref).await,
        }
    }

    async fn on_auth_failure(&self, secret_ref: &SecretRef) -> Result<()> {
        match secret_ref {
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.on_auth_failure(secret_ref).await,
                None => Err(oauth_unavailable_err()),
            },
            // `MemoryStore::on_auth_failure` defaults to no-op via the
            // trait's provided method; explicit dispatch here keeps the
            // shape symmetric and audit-readable.
            _ => self.fallback.on_auth_failure(secret_ref).await,
        }
    }

    async fn account_id(&self, secret_ref: &SecretRef) -> Result<Option<String>> {
        match secret_ref {
            // Route oauth:// account-id reads to the OAuth arm so the
            // openai-responses factory can derive the chatgpt account id
            // from a logged-in session. When the OAuth arm is absent
            // (no HOME/XDG) there is no record to read; return Ok(None)
            // so the factory falls through to its actionable
            // "run `routectl login`" guidance rather than failing here.
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.account_id(secret_ref).await,
                None => Ok(None),
            },
            // env:// / file:// / literal: carry no account metadata;
            // the MemoryStore default returns Ok(None).
            _ => self.fallback.account_id(secret_ref).await,
        }
    }

    async fn list_seats(&self, secret_ref: &SecretRef) -> Result<Vec<SecretRef>> {
        match secret_ref {
            // Route oauth:// seat enumeration to the OAuth arm so the
            // factory can expand a bare pool ref into one ref per stored
            // seat. When the OAuth arm is absent (no HOME/XDG) there is
            // no credentials file to enumerate; fall back to the
            // single-ref default so the downstream resolve surfaces the
            // clear "OAuth store unavailable" error rather than an empty
            // pool here.
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => oauth.list_seats(secret_ref).await,
                None => Ok(vec![secret_ref.clone()]),
            },
            // env:// / file:// / literal: are single-credential refs;
            // the MemoryStore default echoes the input ref.
            _ => self.fallback.list_seats(secret_ref).await,
        }
    }

    async fn peek_session_id(&self, secret_ref: &SecretRef) -> Option<String> {
        match secret_ref {
            // Route oauth:// session-id reads to the OAuth arm so the
            // anthropic-api factory can stamp the Claude-Code session-id
            // header from a logged-in session. When the OAuth arm is
            // absent (no HOME/XDG) there is no record; return None.
            // Fully-qualified call: OAuthStore also has an inherent
            // `peek_session_id(&str)`, so plain method syntax would bind
            // the inherent method instead of the trait one.
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => SecretStore::peek_session_id(oauth, secret_ref).await,
                None => None,
            },
            _ => self.fallback.peek_session_id(secret_ref).await,
        }
    }

    async fn peek_cloud_project_id(&self, secret_ref: &SecretRef) -> Option<String> {
        match secret_ref {
            // Route oauth:// project-id reads to the OAuth arm so the
            // Gemini Cloud Code provider can skip the onboarding round
            // trip on warm restarts. Without this override the read falls
            // through to the trait no-op default and the cache always
            // misses. When the OAuth arm is absent (no HOME/XDG) there is
            // no record; return None. Fully-qualified call: OAuthStore
            // also has an inherent `peek_cloud_project_id(&str)`.
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => SecretStore::peek_cloud_project_id(oauth, secret_ref).await,
                None => None,
            },
            _ => self.fallback.peek_cloud_project_id(secret_ref).await,
        }
    }

    async fn set_cloud_project_id(&self, secret_ref: &SecretRef, project_id: &str) -> Result<()> {
        match secret_ref {
            // Route oauth:// project-id writes to the OAuth arm so a
            // resolved Cloud Code project id persists to the credentials
            // file. Mirrors the `set` write path: when the OAuth arm is
            // absent (no HOME/XDG) the write has no backing store and
            // surfaces the same clear "OAuth store unavailable" error.
            // Fully-qualified call: OAuthStore also has an inherent
            // `set_cloud_project_id(&str, &str)`.
            SecretRef::OAuth { .. } => match &self.oauth {
                Some(oauth) => {
                    SecretStore::set_cloud_project_id(oauth, secret_ref, project_id).await
                }
                None => Err(oauth_unavailable_err()),
            },
            _ => {
                self.fallback
                    .set_cloud_project_id(secret_ref, project_id)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn dispatches_env_to_memory_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();

        std::env::set_var("ROUTECTL_TEST_COMPOSITE_ENV", "value-via-env");
        let v = store
            .get(&SecretRef::Env("ROUTECTL_TEST_COMPOSITE_ENV".into()))
            .await
            .unwrap();
        assert_eq!(v, "value-via-env");
        std::env::remove_var("ROUTECTL_TEST_COMPOSITE_ENV");
    }

    #[tokio::test]
    async fn dispatches_literal_to_memory_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();
        let v = store
            .get(&SecretRef::Literal("inline-value".into()))
            .await
            .unwrap();
        assert_eq!(v, "inline-value");
    }

    #[tokio::test]
    async fn dispatches_oauth_to_oauth_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();
        // No tokens -> NotLoggedIn from OAuthStore (proves dispatch).
        let err = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no credentials"),
            "expected OAuthStore NotLoggedIn, got: {err}"
        );
    }

    /// Seed a credentials.json under `path` with one provider record.
    /// Mirrors the v1 `CredentialsFile` schema and applies the 0o600
    /// hygiene `OAuthStore::open` enforces on Unix. `expires_at_unix` is
    /// the caller's responsibility -- a future value avoids triggering a
    /// refresh roundtrip; a past value would force one (and a
    /// network-stub-less test must therefore use a future value).
    fn seed_credentials_file(
        path: &std::path::Path,
        provider: &str,
        access_token: &str,
        expires_at_unix: u64,
    ) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let creds = serde_json::json!({
            "schema_version": 1,
            "providers": {
                provider: {
                    "access_token": access_token,
                    "refresh_token": format!("seeded-refresh-{provider}"),
                    "token_type": "Bearer",
                    "expires_at_unix": expires_at_unix,
                    "scopes": ["openid", "offline_access"],
                    "account": {
                        "email": null,
                        "account_id": format!("acct-{provider}")
                    },
                    "obtained_at_unix": now
                }
            }
        });
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&creds).expect("serialize creds"),
        )
        .expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
    }

    /// `oauth://codex` resolves end-to-end through CompositeStore: a
    /// TokenRecord seeded on disk is returned through the OAuth arm
    /// without triggering a refresh roundtrip (expiry is 1h in the
    /// future, well past `REFRESH_LEAD_SECS`). Pins the codex provider
    /// dispatch path -- a regression that loses the codex registry
    /// entry, drops the OAuth arm, or routes oauth:// to the
    /// MemoryStore would all fail this test.
    ///
    /// Single-flight coverage: the inner OAuthStore enforces a
    /// per-provider single-flight refresh mutex (see
    /// `routectl_auth::oauth::store::tests::concurrent_get_calls_collapse_to_single_refresh`).
    /// CompositeStore is a thin dispatcher with no extra concurrency
    /// state of its own, so concurrent oauth:// reads through the
    /// composite inherit that guarantee from the inner store; a
    /// duplicated single-flight assertion at the composite layer
    /// would only re-test the inner store.
    #[tokio::test]
    async fn dispatches_oauth_codex_resolves_seeded_token_via_composite() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        seed_credentials_file(&path, "codex", "codex-seeded-access-token", now + 3600);
        let store = CompositeStore::open_at(&path).await.unwrap();

        // Act
        let token = store
            .get(&SecretRef::OAuth {
                provider: "codex".into(),
                label: None,
            })
            .await
            .expect("oauth://codex should resolve via composite");

        // Assert
        assert_eq!(token, "codex-seeded-access-token");
    }

    /// `file://` references must fall through to the MemoryStore arm,
    /// not the OAuth arm. Pins the dispatch table for the file://
    /// scheme (a regression that routes all non-oauth refs to the
    /// OAuth store would surface as "OAuthStore only handles oauth://
    /// refs" here).
    #[tokio::test]
    async fn dispatches_file_to_memory_store() {
        // Arrange: write a secret file under tempdir + 0o600 perms so
        // the MemoryStore's permission check accepts it on Unix.
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("credentials.json");
        let secret_path = dir.path().join("secret-key");
        std::fs::write(&secret_path, "sk-from-file\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
        let store = CompositeStore::open_at(&creds_path).await.unwrap();

        // Act
        let value = store
            .get(&SecretRef::File(secret_path))
            .await
            .expect("file:// should resolve via memory store");

        // Assert: trimmed contents match what MemoryStore would return
        // standalone -- the trailing newline is stripped.
        assert_eq!(value, "sk-from-file");
    }

    /// `oauth://<unknown-provider>` returns a clear, operator-actionable
    /// error -- not a panic, not a stringly-typed default, not a
    /// NotLoggedIn miss. The error must name the unknown provider so
    /// an operator can correlate the misconfigured TOML entry.
    #[tokio::test]
    async fn dispatches_oauth_unknown_provider_returns_clear_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = CompositeStore::open_at(&path).await.unwrap();

        // Act
        let err = store
            .get(&SecretRef::OAuth {
                provider: "made-up-provider".into(),
                label: None,
            })
            .await
            .expect_err("unknown oauth provider must error, not panic");

        // Assert: the error must mention the unknown-provider category
        // and include the offending name for operator correlation.
        let msg = err.to_string();
        assert!(
            msg.contains("unknown oauth provider"),
            "expected unknown-provider category in error, got: {msg}"
        );
        assert!(
            msg.contains("made-up-provider"),
            "expected provider name in error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn on_auth_failure_no_op_for_non_oauth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();
        // env:// has the default no-op on_auth_failure.
        store
            .on_auth_failure(&SecretRef::Env("X".into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn composite_dispatches_oauth_on_auth_failure_to_oauth_store() {
        // Composite must route on_auth_failure for oauth:// refs to the
        // OAuthStore arm, not the MemoryStore default no-op. The
        // OAuthStore returns an error when refresh is unavailable; we
        // assert that signal reaches the caller -- proof that the
        // composite did NOT silently absorb it via the no-op fallback.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();
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
            "expected OAuthStore guidance, got: {msg}"
        );
    }

    /// RAII helper used by the env-mutating test below so an `assert!`
    /// failure can't leak modified env into sibling tests.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Pin: `routectl config check` and `routectl test` must not require
    /// `HOME` or `XDG_CONFIG_HOME` for configs that use only `env://`,
    /// `file://`, or `literal:` refs. Pre-fix, `OAuthStore::open_default`
    /// errored when neither env var was set, which propagated through
    /// `CompositeStore::open_default` and broke binary startup. After the
    /// fix, the OAuth arm is dropped (with a tracing warn) and resolving
    /// an `oauth://` ref returns a clear `Error::Auth` instead.
    #[tokio::test]
    #[serial_test::serial]
    async fn open_default_succeeds_when_xdg_and_home_unset() {
        let _xdg = EnvGuard::unset("XDG_CONFIG_HOME");
        let _home = EnvGuard::unset("HOME");

        let store = CompositeStore::open_default()
            .await
            .expect("CompositeStore::open_default must tolerate missing HOME/XDG");

        // env:// refs still resolve through the MemoryStore arm.
        std::env::set_var("ROUTECTL_TEST_COMPOSITE_NO_HOME", "value-via-env");
        let v = store
            .get(&SecretRef::Env("ROUTECTL_TEST_COMPOSITE_NO_HOME".into()))
            .await
            .expect("env:// resolves with no HOME");
        assert_eq!(v, "value-via-env");
        std::env::remove_var("ROUTECTL_TEST_COMPOSITE_NO_HOME");

        // oauth:// refs return a clear Error::Auth, not a panic.
        let err = store
            .get(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .expect_err("oauth:// must error when OAuth arm is unavailable");
        let msg = err.to_string();
        assert!(
            msg.contains("OAuth store unavailable"),
            "expected unavailable-OAuth message, got: {msg}"
        );
    }

    /// `list_seats` for an oauth:// ref must route through the OAuth arm
    /// (so a bare pool ref expands to one ref per stored seat), while a
    /// non-oauth ref falls through to the MemoryStore single-ref default.
    #[tokio::test]
    async fn composite_store_forwards_list_seats_to_oauth_arm() {
        // Arrange: seed two seats for one provider on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let creds = serde_json::json!({
            "schema_version": 1,
            "providers": {
                "anthropic": {
                    "access_token": "tok-default",
                    "refresh_token": "rtok-default",
                    "token_type": "Bearer",
                    "expires_at_unix": now + 3600,
                    "scopes": ["user:inference"],
                    "obtained_at_unix": now
                },
                "anthropic#seat-b": {
                    "access_token": "tok-seat-b",
                    "refresh_token": "rtok-seat-b",
                    "token_type": "Bearer",
                    "expires_at_unix": now + 3600,
                    "scopes": ["user:inference"],
                    "obtained_at_unix": now
                }
            }
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&creds).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let store = CompositeStore::open_at(&path).await.unwrap();

        // Act: a bare oauth:// pool ref expands through the OAuth arm.
        let seats = store
            .list_seats(&SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            })
            .await
            .unwrap();

        // Assert: default seat first, labeled seat second.
        assert_eq!(
            seats,
            vec![
                SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: None,
                },
                SecretRef::OAuth {
                    provider: "anthropic".into(),
                    label: Some("seat-b".into()),
                },
            ]
        );

        // A non-oauth ref falls through to the single-ref default.
        let env_ref = SecretRef::Env("FOO".into());
        let env_seats = store.list_seats(&env_ref).await.unwrap();
        assert_eq!(env_seats, vec![env_ref]);
    }

    /// The Cloud Code project-id cache round-trips through the REAL
    /// composite -> OAuth seam: `set_cloud_project_id` then
    /// `peek_cloud_project_id` on the same oauth:// ref returns the stored
    /// value. Pre-fix the composite did not override these methods, so
    /// they fell through to the trait no-op defaults -- the write was
    /// silently dropped and every read missed, forcing the Cloud Code
    /// onboarding round trip to re-run on every request. This test drives
    /// the composite (not a raw OAuthStore) precisely because that seam
    /// was the one that was untested.
    #[tokio::test]
    async fn composite_forwards_cloud_project_id_round_trip_to_oauth_arm() {
        // Arrange: seed a record so the OAuth arm has a writable seat
        // (set_cloud_project_id writes back to an existing record).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        seed_credentials_file(&path, "anthropic", "seed-access-token", now + 3600);
        let store = CompositeStore::open_at(&path).await.unwrap();
        let secret_ref = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };

        // Act: write then read the project id THROUGH the composite.
        store
            .set_cloud_project_id(&secret_ref, "projects/round-trip")
            .await
            .expect("set_cloud_project_id must persist via the OAuth arm");
        let got = store.peek_cloud_project_id(&secret_ref).await;

        // Assert: the value persisted and reads back via the composite.
        assert_eq!(got.as_deref(), Some("projects/round-trip"));
    }
}
