//! Cache abstraction for a Cloud Code project id.
//!
//! Providers that need to resolve a Cloud Code project id hold an
//! `Arc<dyn CloudProjectCache>` rather than baking in the id at
//! construction time. The project id is not a secret but may be unknown
//! at startup ("not yet onboarded") and resolved lazily on first use;
//! this trait captures both states.
//!
//! The trait lives in `routectl-core` -- not `routectl-auth` -- so
//! `routectl-providers` can depend on it without inheriting the
//! `SecretRef`/`SecretStore` surface. The bedrock module already
//! maintains this separation; the Cloud Code project id follows the
//! same pattern.

use async_trait::async_trait;

use crate::Result;

/// Caches the resolved Cloud Code project id for a provider's outbound
/// requests. Implementations may store in memory only (`InMemoryProjectCache`)
/// or persist across restarts (the auth-crate adapter `OAuthStoreProjectCache`
/// persists to the OAuth credentials store).
#[async_trait]
pub trait CloudProjectCache: Send + Sync + std::fmt::Debug {
    /// Cached Cloud Code project id, if one has been resolved. `None`
    /// means the project id is not yet known ("not yet onboarded").
    async fn get(&self) -> Option<String>;

    /// Persist a freshly-resolved project id. Errors propagate (e.g. a
    /// disk write failure from a persistent backend). In-memory
    /// implementations always return `Ok(())`.
    async fn put(&self, project_id: String) -> Result<()>;
}

/// In-memory `CloudProjectCache`. Suitable for tests and for providers
/// that resolve the project id at startup and do not need persistence
/// across restarts. Cheap to clone behind `Arc`.
pub struct InMemoryProjectCache {
    inner: std::sync::RwLock<Option<String>>,
}

impl InMemoryProjectCache {
    /// Construct with no cached value.
    pub fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(None),
        }
    }

    /// Construct with `project_id` pre-seeded.
    pub fn with(project_id: impl Into<String>) -> Self {
        Self {
            inner: std::sync::RwLock::new(Some(project_id.into())),
        }
    }
}

impl Default for InMemoryProjectCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InMemoryProjectCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Project id is not a secret, but include only whether one is
        // cached rather than the value itself -- keeps Debug output
        // stable regardless of the id string.
        let has_value = self.inner.read().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("InMemoryProjectCache")
            .field("cached", &has_value)
            .finish()
    }
}

#[async_trait]
impl CloudProjectCache for InMemoryProjectCache {
    async fn get(&self) -> Option<String> {
        self.inner
            .read()
            .expect("InMemoryProjectCache lock poisoned")
            .clone()
    }

    async fn put(&self, project_id: String) -> Result<()> {
        *self
            .inner
            .write()
            .expect("InMemoryProjectCache lock poisoned") = Some(project_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn get_returns_none_initially() {
        // Arrange
        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::new());
        // Act + Assert
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn put_then_get_returns_value() {
        // Arrange
        let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::new());
        // Act
        cache.put("projects/my-project".into()).await.unwrap();
        // Assert
        assert_eq!(cache.get().await.as_deref(), Some("projects/my-project"));
    }

    #[tokio::test]
    async fn with_seeds_initial_value() {
        // Arrange
        let cache: Arc<dyn CloudProjectCache> =
            Arc::new(InMemoryProjectCache::with("projects/seeded"));
        // Act + Assert
        assert_eq!(cache.get().await.as_deref(), Some("projects/seeded"));
    }

    #[test]
    fn debug_does_not_leak_project_id() {
        // The debug output should not contain the raw project id value.
        let cache = InMemoryProjectCache::with("projects/secret-looking-id");
        let dbg = format!("{cache:?}");
        assert!(!dbg.contains("secret-looking-id"), "debug leaked: {dbg}");
        assert!(
            dbg.contains("cached"),
            "expected 'cached' field, got: {dbg}"
        );
    }

    #[tokio::test]
    async fn can_be_cloned_through_arc() {
        // Arrange
        let cache = Arc::new(InMemoryProjectCache::new());
        let cache2 = cache.clone();
        // Act: put via one handle, read via the other.
        cache.put("projects/shared".into()).await.unwrap();
        // Assert
        assert_eq!(cache2.get().await.as_deref(), Some("projects/shared"));
    }
}
