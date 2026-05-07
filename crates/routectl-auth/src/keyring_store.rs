use async_trait::async_trait;
use routectl_core::{Error, Result};

use crate::{SecretRef, SecretStore};

pub struct KeyringStore;

impl KeyringStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for KeyringStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                let service = service.clone();
                let account = account.clone();
                tokio::task::spawn_blocking(move || {
                    keyring::Entry::new(&service, &account)
                        .and_then(|e| e.get_password())
                        .map_err(|e| Error::Auth(format!("keyring: {e}")))
                })
                .await
                .map_err(|e| Error::Auth(format!("spawn_blocking: {e}")))?
            }
            SecretRef::Env(var) => {
                std::env::var(var)
                    .map_err(|_| Error::Auth(format!("env var {var} not set")))
            }
            SecretRef::Literal(s) => Ok(s.clone()),
        }
    }

    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()> {
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                let service = service.clone();
                let account = account.clone();
                let value = value.to_string();
                tokio::task::spawn_blocking(move || {
                    keyring::Entry::new(&service, &account)
                        .and_then(|e| e.set_password(&value))
                        .map_err(|e| Error::Auth(format!("keyring: {e}")))
                })
                .await
                .map_err(|e| Error::Auth(format!("spawn_blocking: {e}")))?
            }
            SecretRef::Env(_) => {
                Err(Error::Auth("env vars are read-only via routectl".into()))
            }
            SecretRef::Literal(_) => {
                Err(Error::Auth("literal secrets are read-only".into()))
            }
        }
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<()> {
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                let service = service.clone();
                let account = account.clone();
                tokio::task::spawn_blocking(move || {
                    keyring::Entry::new(&service, &account)
                        .and_then(|e| e.delete_credential())
                        .map_err(|e| Error::Auth(format!("keyring: {e}")))
                })
                .await
                .map_err(|e| Error::Auth(format!("spawn_blocking: {e}")))?
            }
            SecretRef::Env(_) => {
                Err(Error::Auth("env vars are read-only via routectl".into()))
            }
            SecretRef::Literal(_) => {
                Err(Error::Auth("literal secrets are read-only".into()))
            }
        }
    }
}
