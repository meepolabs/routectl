//! `OAuthStoreProjectCache`: a `CloudProjectCache` backed by the OAuth
//! credentials store.
//!
//! Wires the `CloudProjectCache` trait (defined in `routectl-core`)
//! to the `SecretStore` trait (defined in this crate) so the Gemini
//! provider can hold a single `Arc<dyn CloudProjectCache>` without
//! importing the full OAuth surface.
//!
//! `get` delegates to `SecretStore::peek_cloud_project_id`; `put`
//! delegates to `SecretStore::set_cloud_project_id`. The backing store
//! persists the value to disk (atomic write via `OAuthStore`) so the
//! Gemini provider skips the project-id resolution round trip on warm
//! restarts.

use std::sync::Arc;

use async_trait::async_trait;
use routectl_core::{CloudProjectCache, Result};

use crate::{SecretRef, SecretStore};

/// `CloudProjectCache` adapter that persists the Cloud Code project id
/// in the OAuth credentials store. The Gemini provider factory
/// constructs one of these and stores it behind `Arc<dyn
/// CloudProjectCache>`; the factory wiring happens in a later slice.
///
/// Holds a `SecretRef::OAuth` ref that identifies the credential seat
/// whose record carries the `cloud_project_id` field.
pub struct OAuthStoreProjectCache {
    store: Arc<dyn SecretStore>,
    secret_ref: SecretRef,
}

impl std::fmt::Debug for OAuthStoreProjectCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthStoreProjectCache")
            .field("secret_ref", &self.secret_ref)
            .finish_non_exhaustive()
    }
}

impl OAuthStoreProjectCache {
    /// Construct with `store` and the `SecretRef` that identifies the
    /// credential whose `cloud_project_id` should be read/written.
    pub fn new(store: Arc<dyn SecretStore>, secret_ref: SecretRef) -> Self {
        Self { store, secret_ref }
    }
}

#[async_trait]
impl CloudProjectCache for OAuthStoreProjectCache {
    async fn get(&self) -> Option<String> {
        self.store.peek_cloud_project_id(&self.secret_ref).await
    }

    async fn put(&self, project_id: String) -> Result<()> {
        self.store
            .set_cloud_project_id(&self.secret_ref, &project_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::store::OAuthStore;
    use crate::oauth::types::{unix_now, AccountInfo, SecretToken, TokenRecord};

    fn test_record() -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new("tok"),
            refresh_token: SecretToken::new("rtok"),
            token_type: "Bearer".into(),
            expires_at_unix: unix_now() + 3600,
            scopes: vec![],
            account: AccountInfo::default(),
            obtained_at_unix: 0,
            session_id: None,
            cloud_project_id: None,
        }
    }

    #[tokio::test]
    async fn get_returns_none_before_put() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", test_record())
            .await
            .unwrap();
        let secret_ref = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };
        let cache = OAuthStoreProjectCache::new(Arc::new(store), secret_ref);
        // Act + Assert
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn put_then_get_returns_value() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        store
            .write_record("anthropic", test_record())
            .await
            .unwrap();
        let secret_ref = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };
        let cache = OAuthStoreProjectCache::new(Arc::new(store), secret_ref);
        // Act
        cache.put("projects/my-project".into()).await.unwrap();
        // Assert
        assert_eq!(cache.get().await.as_deref(), Some("projects/my-project"));
    }

    #[tokio::test]
    async fn put_persists_across_reload() {
        // Arrange: open a store, seed a credential, set the project id,
        // then reload from disk and verify the value survived.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        {
            let store = OAuthStore::open(&path).await.unwrap();
            store
                .write_record("anthropic", test_record())
                .await
                .unwrap();
            let secret_ref = SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            };
            let cache = OAuthStoreProjectCache::new(Arc::new(store), secret_ref);
            cache.put("projects/persisted-id".into()).await.unwrap();
        }
        // Reopen from disk.
        let reopened = OAuthStore::open(&path).await.unwrap();
        let val = reopened.peek_cloud_project_id("anthropic").await;
        assert_eq!(
            val.as_deref(),
            Some("projects/persisted-id"),
            "project id must survive reload from disk"
        );
    }

    #[tokio::test]
    async fn put_errors_when_no_record_exists() {
        // Arrange: store with no credential for "anthropic".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let secret_ref = SecretRef::OAuth {
            provider: "anthropic".into(),
            label: None,
        };
        let cache = OAuthStoreProjectCache::new(Arc::new(store), secret_ref);
        // Act
        let result = cache.put("projects/orphan".into()).await;
        // Assert: must error because no record to attach to.
        assert!(
            result.is_err(),
            "put on a missing record must return an error"
        );
    }
}
