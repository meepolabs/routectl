use std::collections::HashMap;

use async_trait::async_trait;
use routectl_core::{Error, Result};
use tokio::sync::RwLock;

use crate::{SecretRef, SecretStore};

pub struct MemoryStore {
    map: RwLock<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        match secret_ref {
            SecretRef::Env(var) => {
                std::env::var(var)
                    .map_err(|_| Error::Auth(format!("env var {var} not set")))
            }
            SecretRef::Literal(s) => Ok(s.clone()),
            SecretRef::Keychain { .. } => {
                let key = secret_ref.to_string();
                let map = self.map.read().await;
                map.get(&key)
                    .cloned()
                    .ok_or_else(|| Error::Auth(format!("no entry for {key}")))
            }
        }
    }

    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()> {
        match secret_ref {
            SecretRef::Env(_) => {
                Err(Error::Auth("env vars are read-only via routectl".into()))
            }
            SecretRef::Literal(_) => {
                Err(Error::Auth("literal secrets are read-only".into()))
            }
            SecretRef::Keychain { .. } => {
                let key = secret_ref.to_string();
                let mut map = self.map.write().await;
                map.insert(key, value.to_string());
                Ok(())
            }
        }
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<()> {
        match secret_ref {
            SecretRef::Env(_) => {
                Err(Error::Auth("env vars are read-only via routectl".into()))
            }
            SecretRef::Literal(_) => {
                Err(Error::Auth("literal secrets are read-only".into()))
            }
            SecretRef::Keychain { .. } => {
                let key = secret_ref.to_string();
                let mut map = self.map.write().await;
                if map.remove(&key).is_none() {
                    return Err(Error::Auth(format!("no entry for {key}")));
                }
                Ok(())
            }
        }
    }
}
