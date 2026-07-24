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
}
