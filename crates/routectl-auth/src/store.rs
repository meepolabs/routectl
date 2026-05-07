use async_trait::async_trait;
use routectl_core::Result;

use crate::SecretRef;

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String>;
    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()>;
    async fn delete(&self, secret_ref: &SecretRef) -> Result<()>;
}
