//! Shared helpers for the Bedrock mantle lanes.
//!
//! The URL builders are pure and dependency-free: they derive the
//! mantle host and per-vocabulary bases from a region string alone, so
//! both the Anthropic and OpenAI lanes agree on one source of truth and
//! never carry a manually configured base URL. Signing reuses the
//! parameterized Bedrock SigV4 signer under the `bedrock-mantle` scope.

/// AWS SigV4 service scope for the Bedrock mantle lanes.
pub const MANTLE_SERVICE: &str = "bedrock-mantle";

/// Mantle host for `region`, without a trailing slash.
///
/// Shape: `https://bedrock-mantle.<region>.api.aws`. Path-free so lane
/// bases can append their own vocabulary segment.
pub fn mantle_host(region: &str) -> String {
    format!("https://bedrock-mantle.{region}.api.aws")
}

/// Anthropic-vocabulary base URL for `region`, without a trailing slash.
///
/// The Anthropic client appends `/v1/messages`, so this base ends at
/// `/anthropic`.
pub fn mantle_anthropic_base(region: &str) -> String {
    format!("{}/anthropic", mantle_host(region))
}

/// OpenAI-vocabulary base URL for `region`, without a trailing slash.
///
/// Shape: `<host>/openai/v1`.
pub fn mantle_openai_base(region: &str) -> String {
    format!("{}/openai/v1", mantle_host(region))
}

/// Bedrock mantle authentication shared by the mantle egress lanes.
///
/// Present (a provider config's `mantle` field is `Some`) selects the
/// mantle lane: the request body is serialized to bytes and
/// SigV4/bearer-signed under the `bedrock-mantle` scope before egress,
/// and the lane's first-party header plumbing (`x-api-key`, Claude-Code,
/// or codex identity) is bypassed. `region` is the single source of truth
/// for both the derived endpoint host and the SigV4 signing scope;
/// `creds` carries the resolved AWS credential shape (bearer key or SigV4
/// provider). Shared home for the Anthropic and OpenAI mantle lanes.
#[cfg(feature = "bedrock")]
#[derive(Clone)]
pub struct MantleAuth {
    /// AWS region driving the derived host and the SigV4 signing scope.
    pub region: String,
    /// Resolved credential shape (bearer key or SigV4 provider).
    pub creds: crate::bedrock::auth::ResolvedCreds,
}

#[cfg(feature = "bedrock")]
impl MantleAuth {
    /// Observability discriminator for the credential shape:
    /// `"bearer"` for a Bedrock console API key, `"sigv4"` for a signed
    /// AWS credential. Never carries any secret material.
    pub(crate) const fn auth_mode(&self) -> &'static str {
        match self.creds {
            crate::bedrock::auth::ResolvedCreds::Bearer { .. } => "bearer",
            crate::bedrock::auth::ResolvedCreds::Sigv4 { .. } => "sigv4",
        }
    }
}

#[cfg(feature = "bedrock")]
impl std::fmt::Debug for MantleAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The region is non-secret. `creds` carries credential material,
        // so surface only its shape discriminator (never the key or the
        // provider), mirroring the redacting Debug on `BedrockCreds`.
        f.debug_struct("MantleAuth")
            .field("region", &self.region)
            .field("auth_mode", &self.auth_mode())
            .finish()
    }
}

/// Sign `req` in place for a mantle lane under the `bedrock-mantle` SigV4
/// scope.
///
/// Delegates to the parameterized Bedrock signer, handling both credential
/// shapes: `Bearer` attaches `Authorization: Bearer <key>`, `Sigv4` signs
/// the request and merges the AWS auth headers.
#[cfg(feature = "bedrock")]
pub async fn sign(
    req: &mut reqwest::Request,
    creds: &crate::bedrock::auth::ResolvedCreds,
    region: &str,
) -> routectl_core::Result<()> {
    crate::bedrock::signing::apply_with_service(req, creds, region, MANTLE_SERVICE).await
}

/// Probe a mantle lane by resolving its credential, mirroring the Bedrock
/// provider's probe posture.
///
/// The mantle endpoint authenticates with SigV4/bearer, not the
/// first-party `x-api-key`, and exposes no free models-list surface, so a
/// reachability probe must NOT dial the inference host. Instead it checks
/// the credential is live: a `Bearer` key is a static secret (trivially
/// reachable), a `Sigv4` provider re-provides its chain (catching an
/// expired SSO / broken profile after startup). Reason strings are fixed
/// literals so no profile name, ARN, or SDK detail leaks into an
/// operator-facing message.
#[cfg(feature = "bedrock")]
pub async fn probe(creds: &crate::bedrock::auth::ResolvedCreds) -> routectl_core::ProbeOutcome {
    use aws_credential_types::provider::ProvideCredentials;

    use crate::bedrock::auth::ResolvedCreds;

    match creds {
        ResolvedCreds::Bearer { .. } => routectl_core::ProbeOutcome::Reachable,
        ResolvedCreds::Sigv4 { provider } => {
            match tokio::time::timeout(crate::probe::PROBE_TIMEOUT, provider.provide_credentials())
                .await
            {
                Ok(Ok(_)) => routectl_core::ProbeOutcome::Reachable,
                Ok(Err(e)) => {
                    // Log the real SDK error server-side (an expired SSO,
                    // missing profile, and unreachable IMDS look identical
                    // otherwise), but keep a fixed literal in the outcome so
                    // no profile name, ARN, or SDK detail leaks to the
                    // operator surface -- mirrors `bedrock::auth::resolve`.
                    tracing::warn!(error = %e, "mantle credential resolution failed");
                    routectl_core::ProbeOutcome::AuthFailed("credential resolution failed".into())
                }
                Err(_) => routectl_core::ProbeOutcome::Unreachable(
                    "credential resolution timed out".into(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mantle_anthropic_base, mantle_host, mantle_openai_base};

    #[test]
    fn host_has_expected_shape_and_no_trailing_slash() {
        let host = mantle_host("us-east-1");
        assert_eq!(host, "https://bedrock-mantle.us-east-1.api.aws");
        assert!(!host.ends_with('/'));
    }

    #[test]
    fn anthropic_base_ends_at_anthropic_segment() {
        let base = mantle_anthropic_base("eu-west-1");
        assert_eq!(base, "https://bedrock-mantle.eu-west-1.api.aws/anthropic");
        assert!(!base.ends_with('/'));
    }

    #[test]
    fn openai_base_has_v1_suffix() {
        let base = mantle_openai_base("ap-southeast-2");
        assert_eq!(
            base,
            "https://bedrock-mantle.ap-southeast-2.api.aws/openai/v1"
        );
        assert!(!base.ends_with('/'));
    }

    #[cfg(feature = "bedrock")]
    mod signing {
        use reqwest::header::AUTHORIZATION;

        use super::super::sign;
        use crate::bedrock::BedrockCreds;
        use crate::bedrock::auth::resolve;
        #[tokio::test]
        async fn sigv4_credential_scope_uses_mantle_service() {
            let resolved = resolve(
                &BedrockCreds::Static {
                    access_key: "testkey-sign-xyz".into(),
                    secret_key: "test-secret-key".into(),
                    session_token: None,
                },
                "us-west-2",
            )
            .await
            .unwrap();

            let client = reqwest::Client::new();
            let mut req = client
                .post("https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages")
                .body("{}")
                .build()
                .unwrap();

            sign(&mut req, &resolved, "us-west-2").await.unwrap();

            let auth = req
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .expect("Authorization header set");
            assert!(
                auth.starts_with("AWS4-HMAC-SHA256 "),
                "expected SigV4 prefix, got: {auth}"
            );
            assert!(
                auth.contains("/us-west-2/bedrock-mantle/aws4_request"),
                "missing mantle service scope, got: {auth}"
            );
        }

        #[tokio::test]
        async fn bearer_path_attaches_authorization_header() {
            let resolved = resolve(
                &BedrockCreds::BearerKey {
                    key: "mantle-api-key-xyz".into(),
                },
                "us-west-2",
            )
            .await
            .unwrap();

            let client = reqwest::Client::new();
            let mut req = client
                .post("https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages")
                .body("{}")
                .build()
                .unwrap();

            sign(&mut req, &resolved, "us-west-2").await.unwrap();
            assert_eq!(
                req.headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer mantle-api-key-xyz")
            );
        }
    }

    /// `MantleAuth` Debug surfaces only the non-secret region and the auth
    /// shape discriminator -- never the bearer key or the SigV4 provider.
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn mantle_auth_debug_redacts_credential_material() {
        use super::MantleAuth;
        use crate::bedrock::BedrockCreds;
        use crate::bedrock::auth::resolve;

        let creds = resolve(
            &BedrockCreds::BearerKey {
                key: "super-secret-bearer-value".into(),
            },
            "eu-west-1",
        )
        .await
        .unwrap();
        let auth = MantleAuth {
            region: "eu-west-1".into(),
            creds,
        };

        let rendered = format!("{auth:?}");
        assert!(
            rendered.contains("eu-west-1"),
            "region is non-secret and must render: {rendered}"
        );
        assert!(
            rendered.contains("bearer"),
            "auth-mode discriminator must render: {rendered}"
        );
        assert!(
            !rendered.contains("super-secret-bearer-value"),
            "credential material must never render in Debug: {rendered}"
        );
    }
}
