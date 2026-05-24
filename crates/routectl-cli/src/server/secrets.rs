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
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no credentials"),
            "expected OAuthStore NotLoggedIn, got: {err}"
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
        // OAuthStore returns an error in a prior change (refresh deferred); we
        // assert that signal reaches the caller -- proof that the
        // composite did NOT silently absorb it via the no-op fallback.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CompositeStore::open_at(&path).await.unwrap();
        let err = store
            .on_auth_failure(&SecretRef::OAuth {
                provider: "anthropic".into(),
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
            })
            .await
            .expect_err("oauth:// must error when OAuth arm is unavailable");
        let msg = err.to_string();
        assert!(
            msg.contains("OAuth store unavailable"),
            "expected unavailable-OAuth message, got: {msg}"
        );
    }
}
