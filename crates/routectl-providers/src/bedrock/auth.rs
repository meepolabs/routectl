//! AWS credential resolution for the Bedrock provider.
//!
//! Maps `BedrockCreds` (already-resolved plaintext, see
//! `crates/routectl-router/src/factory.rs::resolve_bedrock_creds`)
//! into a runtime credentials handle:
//!
//! - `Bearer` -- short-circuit. No SigV4. The signing layer attaches
//!   `Authorization: Bearer <key>` directly.
//! - `Sigv4 { provider }` -- a `SharedCredentialsProvider` that the
//!   signing layer queries on every request. The provider does its own
//!   caching and (for Profile / DefaultChain) SSO auto-refresh.
//!
//! `aws-config` does the heavy lifting for Profile and DefaultChain.
//! Static and Bearer don't need it.

use std::sync::Arc;

use aws_credential_types::provider::future as creds_future;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;

use routectl_core::{Error, Result};

use super::BedrockCreds;

/// Credential resolution result. The signing layer dispatches on this.
#[derive(Clone)]
pub enum ResolvedCreds {
    /// SigV4-signed requests. The provider is queried per request and
    /// is responsible for any caching/refresh behavior (for SSO etc.).
    Sigv4 { provider: SharedCredentialsProvider },
    /// Bedrock console short-term API key. SigV4 is skipped; the
    /// signing layer attaches `Authorization: Bearer <key>`.
    Bearer { key: String },
}

impl std::fmt::Debug for ResolvedCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sigv4 { .. } => f.write_str("ResolvedCreds::Sigv4"),
            Self::Bearer { .. } => f.write_str("ResolvedCreds::Bearer"),
        }
    }
}

/// Build a runtime credentials handle from the configured `BedrockCreds`.
/// Async because `Profile` and `DefaultChain` may need to load credential
/// chains (and on a cold start, hit SSO endpoints) before returning.
pub async fn resolve(creds: &BedrockCreds, region: &str) -> Result<ResolvedCreds> {
    match creds {
        BedrockCreds::BearerKey { key } => Ok(ResolvedCreds::Bearer { key: key.clone() }),

        BedrockCreds::Static {
            access_key,
            secret_key,
            session_token,
        } => {
            let credentials = Credentials::new(
                access_key.clone(),
                secret_key.clone(),
                session_token.clone(),
                None, // expiration -- not known for raw env creds
                "routectl-static",
            );
            Ok(ResolvedCreds::Sigv4 {
                provider: SharedCredentialsProvider::new(StaticProvider {
                    inner: Arc::new(credentials),
                }),
            })
        }

        BedrockCreds::Profile { name } => {
            let provider = aws_config::profile::ProfileFileCredentialsProvider::builder()
                .profile_name(name)
                .build();
            // Probe once so configuration errors surface here, not on
            // the first chat request.
            provider.provide_credentials().await.map_err(|e| {
                tracing::warn!(
                    auth_kind = "Profile",
                    profile = %name,
                    region = %region,
                    error = %e,
                    "bedrock credential resolution failed",
                );
                Error::Auth(format!("bedrock: failed to load AWS profile `{name}`: {e}"))
            })?;
            Ok(ResolvedCreds::Sigv4 {
                provider: SharedCredentialsProvider::new(provider),
            })
        }

        BedrockCreds::DefaultChain => {
            let region_obj = aws_types::region::Region::new(region.to_string());
            let chain =
                aws_config::default_provider::credentials::DefaultCredentialsChain::builder()
                    .region(region_obj)
                    .build()
                    .await;
            // Probe once for fail-fast.
            chain.provide_credentials().await.map_err(|e| {
                tracing::warn!(
                    auth_kind = "DefaultChain",
                    region = %region,
                    error = %e,
                    "bedrock credential resolution failed",
                );
                Error::Auth(format!(
                    "bedrock: AWS default credentials chain failed: {e}"
                ))
            })?;
            Ok(ResolvedCreds::Sigv4 {
                provider: SharedCredentialsProvider::new(chain),
            })
        }
    }
}

/// Tiny `ProvideCredentials` impl that always returns the same value.
/// Used for the Static path where there is nothing to refresh.
#[derive(Debug)]
struct StaticProvider {
    inner: Arc<Credentials>,
}

impl ProvideCredentials for StaticProvider {
    fn provide_credentials<'a>(&'a self) -> creds_future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        creds_future::ProvideCredentials::ready(Ok((*self.inner).clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bearer_key_resolves_to_bearer_variant() {
        let creds = BedrockCreds::BearerKey {
            key: "test-bearer-key".into(),
        };
        let resolved = resolve(&creds, "us-west-2").await.expect("resolve");
        match resolved {
            ResolvedCreds::Bearer { key } => assert_eq!(key, "test-bearer-key"),
            _ => panic!("expected Bearer"),
        }
    }

    #[tokio::test]
    async fn static_creds_resolve_to_sigv4_variant_with_static_provider() {
        let creds = BedrockCreds::Static {
            access_key: "testkey-redacted".into(),
            secret_key: "testkey-secret-xyz".into(),
            session_token: Some("session-test".into()),
        };
        let resolved = resolve(&creds, "us-west-2").await.expect("resolve");
        let provider = match resolved {
            ResolvedCreds::Sigv4 { provider } => provider,
            _ => panic!("expected Sigv4"),
        };
        let fetched = provider.provide_credentials().await.expect("provide");
        assert_eq!(fetched.access_key_id(), "testkey-redacted");
        assert_eq!(fetched.secret_access_key(), "testkey-secret-xyz");
        assert_eq!(fetched.session_token(), Some("session-test"));
    }

    #[tokio::test]
    async fn profile_creds_resolve_returns_either_ok_or_clean_auth_error() {
        // aws-config's profile loader is tolerant -- it may return
        // a working provider even when the profile is missing if env
        // vars happen to be present. So we can't deterministically
        // assert Err. What we DO assert: if we get an error, it's a
        // clean `Error::Auth` with our prefix (not a panic, not a
        // leaked SDK type), and the path doesn't loop.
        let creds = BedrockCreds::Profile {
            name: "definitely-not-a-real-profile-xyzzy-routectl-test".into(),
        };
        match resolve(&creds, "us-west-2").await {
            Ok(_) => {} // tolerated -- env-driven happy path
            Err(Error::Auth(msg)) => {
                assert!(
                    msg.contains("bedrock:") && msg.contains("profile"),
                    "error should be tagged and mention profile: {msg}"
                );
            }
            Err(other) => panic!("expected Auth or Ok, got {other:?}"),
        }
    }
}
