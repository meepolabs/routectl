pub mod atomic_write;
pub mod memory_store;
#[cfg(feature = "oauth")]
pub mod oauth;
pub mod secret_capture;
pub mod secret_ref;
pub mod store;

pub use memory_store::MemoryStore;
pub use secret_capture::{
    ManagedSecretStore, SecretCaptureError, SecretCaptureResult, default_secret_dir, env_ref,
};
pub use secret_ref::SecretRef;
pub use store::SecretStore;

#[cfg(feature = "oauth")]
pub use oauth::{
    LocalProbe, LoginOptions, OAuthError, OAuthStore, OAuthStoreProjectCache, OpenOutcome,
    SecretToken,
};
