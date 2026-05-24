use async_trait::async_trait;
use routectl_core::Result;

use crate::SecretRef;

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String>;
    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()>;
    async fn delete(&self, secret_ref: &SecretRef) -> Result<()>;

    /// Hook invoked by the router when an upstream returns 401 against
    /// a credential resolved from this store. Default no-op: env://,
    /// file://, and literal: cannot self-heal -- the operator has to
    /// rotate the underlying secret manually. The OAuth store overrides
    /// this to refresh + writeback. The router retries the original
    /// request exactly once after `Ok(())`.
    async fn on_auth_failure(&self, _secret_ref: &SecretRef) -> Result<()> {
        Ok(())
    }
}
