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

    /// Best-effort read of a stable account identifier associated with
    /// a credential, without exposing the secret itself. Returns
    /// `Ok(None)` by default: env://, file://, and literal: refs carry
    /// no account metadata. The OAuth store overrides this to return
    /// the `chatgpt_account_id` recorded at login (stable across token
    /// rotations), so the openai-responses factory can derive the
    /// account id from a logged-in `oauth://codex` session instead of
    /// requiring the operator to repeat it in TOML. Reading a provider
    /// with no stored record yields `Ok(None)` (treated by the caller
    /// as "not derivable -- run `routectl login`"), not an error.
    async fn account_id(&self, _secret_ref: &SecretRef) -> Result<Option<String>> {
        Ok(None)
    }

    /// Best-effort read or lazy mint of the per-credential
    /// `session_id` (UUIDv4) used in the codex `session-id` HTTP
    /// header on outbound chatgpt-oauth traffic. Returns `Ok(None)`
    /// by default: env://, file://, and literal: refs carry no
    /// session metadata. The OAuth store overrides this to read from
    /// (or lazily backfill into) credentials.json so the upstream
    /// upstream sees the same session id across the credential's
    /// lifetime. Reading a provider with no stored record yields
    /// `Ok(None)`.
    async fn session_id(&self, _secret_ref: &SecretRef) -> Result<Option<String>> {
        Ok(None)
    }
}
