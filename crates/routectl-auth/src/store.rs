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

    /// Enumerate the concrete seat refs a (possibly pooled) credential
    /// reference resolves to. Default: a single-element vec echoing the
    /// input ref -- `env://`, `file://`, `literal:`, and an already-pinned
    /// or single-seat `oauth://` ref are all one credential, so the
    /// router sees exactly one target. The OAuth store overrides this so
    /// a bare `oauth://<provider>` pool ref expands to one ref per stored
    /// seat (default seat first, then sorted labels), letting the factory
    /// build a multi-seat credential pool from one TOML entry. A ref that
    /// pins a specific seat (`oauth://<provider>#<label>`) returns just
    /// that seat -- the operator already selected it.
    async fn list_seats(&self, secret_ref: &SecretRef) -> Result<Vec<SecretRef>> {
        Ok(vec![secret_ref.clone()])
    }
}
