//! Secret references and the stores that resolve them for routectl.
//!
//! A [`SecretRef`] names a credential by scheme (`env://`, `file://`,
//! `literal:`, or `oauth://`); a [`SecretStore`] resolves a reference to
//! its current value and mediates rotation on authentication failure.
//! [`MemoryStore`] serves the non-managed schemes, and
//! [`ManagedSecretStore`] captures operator-provided secrets into a
//! permission-restricted directory.
//!
//! With the `oauth` feature enabled, the [`oauth`] module adds
//! subscription-OAuth credentials: a login flow, an on-disk credential
//! store, and near-expiry token refresh behind a single-flight gate.
//! Disabling the feature reduces the crate to the `env://`, `file://`,
//! and `literal:` schemes with a leaner dependency tree.
#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]

pub mod atomic_write;
mod memory_store;
#[cfg(feature = "oauth")]
pub mod oauth;
mod secret_capture;
mod secret_ref;
mod store;

pub use memory_store::MemoryStore;
pub use secret_capture::{
    ManagedSecretStore, SecretCaptureError, SecretCaptureResult, default_secret_dir, env_ref,
};
pub use secret_ref::SecretRef;
pub use store::SecretStore;

#[cfg(feature = "oauth")]
pub use oauth::{
    LocalProbe, LoginOptions, OAuthError, OAuthStore, OAuthStoreProjectCache, OpenOutcome,
};
