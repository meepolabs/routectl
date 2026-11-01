//! The pool-member build `config check` runs, offline.
//!
//! Serve and hot reload refuse a config whose pool member cannot be built
//! once its credential reads. `config check` runs the SAME member build
//! (`routectl_router::pool_member_refusals`) so the three surfaces refuse the
//! same configs, but over [`OfflineCheckStore`]: a store that answers every
//! read from the credentials file as it stands and never refreshes, so the
//! check dials no token endpoint and rewrites no credential. The build also
//! runs with installation-id minting off: an existing id is adopted, a
//! missing one is left missing.

use std::sync::Arc;

use async_trait::async_trait;
use routectl_auth::oauth::{TokenRecord, known_provider_ids, seat_key, unix_now};
use routectl_auth::{CredentialProbeFailure, MemoryStore, OAuthStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{BuildOptions, Config};

/// A `SecretStore` for a read-only command. `oauth://` refs are answered
/// from the stored record alone: a seat that could serve or renew counts as
/// readable, its access token is returned as stored (expired or not), and
/// nothing is refreshed or written back. Every other scheme resolves through
/// the default resolver, a local env or file read.
pub struct OfflineCheckStore {
    oauth: Option<OAuthStore>,
    fallback: MemoryStore,
}

impl OfflineCheckStore {
    /// Read the default credentials file. A file that cannot be opened
    /// leaves every `oauth://` member unreadable, which keeps it as an
    /// unusable seat rather than refusing the config.
    pub async fn open_default() -> Self {
        Self::over(OAuthStore::open_default().await.ok())
    }

    /// Answer `oauth://` refs from `oauth`, or treat them all as unreadable
    /// when it is `None`.
    #[must_use]
    pub const fn over(oauth: Option<OAuthStore>) -> Self {
        Self {
            oauth,
            fallback: MemoryStore::new(),
        }
    }

    /// The stored record for an `oauth://` ref, or `None` when the provider
    /// is unknown, the store is absent, or no seat is stored under the key.
    async fn record(&self, provider: &str, label: Option<&str>) -> Option<TokenRecord> {
        if !known_provider_ids().contains(&provider) {
            return None;
        }
        let wanted = seat_key(provider, label);
        self.oauth
            .as_ref()?
            .list()
            .await
            .into_iter()
            .find_map(|(key, record)| (key == wanted).then_some(record))
    }
}

fn read_only_refusal() -> Error {
    Error::Auth("config check does not write credentials".into())
}

#[async_trait]
impl SecretStore for OfflineCheckStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        let SecretRef::OAuth { provider, label } = secret_ref else {
            return self.fallback.get(secret_ref).await;
        };
        match self.record(provider, label.as_deref()).await {
            Some(record) if record.is_locally_usable(unix_now()) => {
                Ok(record.access_token.expose().to_string())
            }
            _ => Err(Error::Auth(
                "no usable stored credential for the seat this ref names".into(),
            )),
        }
    }

    async fn probe_credential(
        &self,
        secret_ref: &SecretRef,
    ) -> std::result::Result<(), CredentialProbeFailure> {
        let SecretRef::OAuth { provider, label } = secret_ref else {
            return self.fallback.probe_credential(secret_ref).await;
        };
        if self.oauth.is_none() || !known_provider_ids().contains(&provider.as_str()) {
            return Err(CredentialProbeFailure::Unreadable);
        }
        match self.record(provider, label.as_deref()).await {
            None => Err(CredentialProbeFailure::NotLoggedIn),
            Some(record) if record.is_locally_usable(unix_now()) => Ok(()),
            Some(_) => Err(CredentialProbeFailure::Unreadable),
        }
    }

    async fn set(&self, _secret_ref: &SecretRef, _value: &str) -> Result<()> {
        Err(read_only_refusal())
    }

    async fn delete(&self, _secret_ref: &SecretRef) -> Result<()> {
        Err(read_only_refusal())
    }

    async fn account_id(&self, secret_ref: &SecretRef) -> Result<Option<String>> {
        match secret_ref {
            SecretRef::OAuth { provider, label } => Ok(match &self.oauth {
                Some(oauth) => {
                    oauth
                        .peek_account_id(&seat_key(provider, label.as_deref()))
                        .await
                }
                None => None,
            }),
            _ => self.fallback.account_id(secret_ref).await,
        }
    }

    async fn peek_session_id(&self, secret_ref: &SecretRef) -> Option<String> {
        match (secret_ref, &self.oauth) {
            (SecretRef::OAuth { .. }, Some(oauth)) => {
                SecretStore::peek_session_id(oauth, secret_ref).await
            }
            _ => None,
        }
    }

    async fn peek_cloud_project_id(&self, secret_ref: &SecretRef) -> Option<String> {
        match (secret_ref, &self.oauth) {
            (SecretRef::OAuth { .. }, Some(oauth)) => {
                SecretStore::peek_cloud_project_id(oauth, secret_ref).await
            }
            _ => None,
        }
    }

    async fn set_cloud_project_id(&self, _secret_ref: &SecretRef, _project_id: &str) -> Result<()> {
        Ok(())
    }
}

/// Build every pool member a selectable model routes at with the options
/// serve builds with, minus installation-id minting, and return each pool's
/// refusal as a `config: `-prefixed error line, the text serve and reload
/// report for the same config.
pub async fn member_build_errors(config: &Config, secrets: Arc<dyn SecretStore>) -> Vec<String> {
    let opts = BuildOptions::new()
        .with_strict_translation(config.server.strict_translation)
        .with_normalize_tools(config.cache.normalize_tools)
        .with_mint_installation_id(false);
    routectl_router::pool_member_refusals(config, secrets, opts)
        .await
        .into_iter()
        .map(|refusal| format!("config: {refusal}"))
        .collect()
}

#[cfg(test)]
#[path = "config_member_build_tests.rs"]
mod config_member_build_tests;
