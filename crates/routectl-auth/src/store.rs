use async_trait::async_trait;
use routectl_core::Result;

use crate::SecretRef;

/// Resolves [`SecretRef`] values to their current secret and mediates
/// rotation. Implementations back the individual reference schemes.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Resolve a reference to its current secret value.
    async fn get(&self, secret_ref: &SecretRef) -> Result<String>;
    /// Store a value for a reference whose backing store is writable.
    async fn set(&self, secret_ref: &SecretRef, value: &str) -> Result<()>;
    /// Remove a stored value for a reference whose backing store is
    /// writable.
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

    /// Best-effort read of the per-credential `session_id` recorded at
    /// login, without exposing the secret itself. Returns `None` by
    /// default: `env://`, `file://`, and `literal:` refs carry no
    /// session metadata. The OAuth store overrides this to return the
    /// `session_id` of the named seat's record, so the anthropic-api
    /// factory can stamp the Claude-Code session-id header from a
    /// logged-in `oauth://anthropic` session. A non-oauth ref or a
    /// missing record yields `None`.
    async fn peek_session_id(&self, _secret_ref: &SecretRef) -> Option<String> {
        None
    }

    /// Best-effort read of the Cloud Code project id persisted for a
    /// credential. Returns `None` by default: `env://`, `file://`, and
    /// `literal:` refs carry no project-id metadata. The OAuth store
    /// overrides this to return the `cloud_project_id` of the named
    /// seat's record, so the Gemini provider can skip the project-id
    /// resolution round trip on warm restarts. A non-oauth ref or a
    /// missing or un-onboarded record yields `None`.
    async fn peek_cloud_project_id(&self, _secret_ref: &SecretRef) -> Option<String> {
        None
    }

    /// Persist a resolved Cloud Code project id for the credential
    /// named by `secret_ref`. Default no-op for `env://`, `file://`,
    /// and `literal:` refs (they have no writable backing store). The
    /// OAuth store overrides this to write back to the credentials file
    /// atomically. Errors propagate (e.g. disk write failures).
    async fn set_cloud_project_id(&self, _secret_ref: &SecretRef, _project_id: &str) -> Result<()> {
        Ok(())
    }

    /// Compare-and-clear a persisted Cloud Code project id for the
    /// credential named by `secret_ref`. Clears only when the stored id
    /// equals `expected`; returns `Ok(true)` when it matched and was
    /// cleared, `Ok(false)` when it did not match (value retained).
    /// Default no-op returning `Ok(false)` for `env://`, `file://`, and
    /// `literal:` refs (they have no writable backing store). The OAuth
    /// store overrides this to clear the field in the credentials file
    /// atomically. The equality guard keeps a late stale-id failure from
    /// wiping a fresh id a concurrent request already re-resolved.
    async fn clear_cloud_project_id_if_matches(
        &self,
        _secret_ref: &SecretRef,
        _expected: &str,
    ) -> Result<bool> {
        Ok(false)
    }
}
