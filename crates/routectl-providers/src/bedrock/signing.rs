//! SigV4 signing for Bedrock-runtime requests.
//!
//! `apply_auth` is the single entry point. Given a fully-built
//! `reqwest::Request` (method, URL, headers, body) and the resolved
//! credentials, it either:
//!
//! 1. attaches `Authorization: Bearer <key>` (BearerKey path), or
//! 2. queries the `SharedCredentialsProvider` for a fresh AWS access
//!    key + secret + optional session token, signs the request via
//!    `aws_sigv4::http_request::sign`, and merges the resulting
//!    headers (`Authorization`, `x-amz-date`, `x-amz-content-sha256`,
//!    `x-amz-security-token` when present) into the request.
//!
//! All signing is in the `bedrock` service scope. The region comes
//! from `BedrockConfig.region` and is part of the signing scope.

use std::time::SystemTime;

use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    sign, SignableBody, SignableRequest, SigningSettings,
};
use aws_sigv4::sign::v4;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};

use routectl_core::{Error, Result};

use super::auth::ResolvedCreds;

/// Apply the appropriate authentication scheme to `req` in place.
///
/// For `Bearer` -- attaches `Authorization: Bearer <key>`.
/// For `Sigv4`  -- fetches the latest credentials from the provider,
/// SigV4-signs the request, and merges the auth headers into `req`.
pub async fn apply_auth(
    req: &mut reqwest::Request,
    creds: &ResolvedCreds,
    region: &str,
) -> Result<()> {
    match creds {
        ResolvedCreds::Bearer { key } => {
            let value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|e| Error::Auth(format!("bedrock: invalid bearer key: {e}")))?;
            req.headers_mut().insert(AUTHORIZATION, value);
            Ok(())
        }
        ResolvedCreds::Sigv4 { provider } => {
            let credentials = provider
                .provide_credentials()
                .await
                .map_err(|e| Error::Auth(format!("bedrock: credentials unavailable: {e}")))?;
            sigv4_sign(req, &credentials, region)
        }
    }
}

fn sigv4_sign(
    req: &mut reqwest::Request,
    credentials: &aws_credential_types::Credentials,
    region: &str,
) -> Result<()> {
    // Materialize body bytes for the canonical-request hash. Bedrock
    // request bodies are JSON we built ourselves with `.body(Vec<u8>)`,
    // so they always serialize to in-memory bytes. Refuse anything
    // else explicitly: signing an empty hash for a streaming body
    // would produce a syntactically valid request that AWS rejects
    // with a cryptic 403 (signature mismatch) -- diagnosing that
    // failure is much harder than this up-front error.
    let body_bytes = req
        .body()
        .and_then(|b| b.as_bytes())
        .ok_or_else(|| {
            Error::Auth(
                "bedrock: cannot SigV4-sign a streaming or unbuffered body; \
                 body() must resolve to in-memory bytes"
                    .into(),
            )
        })?
        .to_vec();
    let body = SignableBody::Bytes(&body_bytes);

    let identity = credentials.clone().into();
    let signing_settings = SigningSettings::default();

    let v4_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("bedrock")
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .map_err(|e| Error::Auth(format!("bedrock: signing params build failed: {e}")))?;
    let signing_params = v4_params.into();

    // Collect headers as (&str, &str) pairs for SignableRequest.
    let header_pairs: Vec<(&str, &str)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.as_str(), val)))
        .collect();

    let method = req.method().as_str();
    let url = req.url().as_str();

    let signable = SignableRequest::new(method, url, header_pairs.into_iter(), body)
        .map_err(|e| Error::Auth(format!("bedrock: signable request build failed: {e}")))?;

    let (instructions, _signature) = sign(signable, &signing_params)
        .map_err(|e| Error::Auth(format!("bedrock: SigV4 sign failed: {e}")))?
        .into_parts();

    // Merge the signing instructions back into the reqwest::Request.
    let (added_headers, added_params) = instructions.into_parts();
    for header in added_headers {
        let name = HeaderName::from_bytes(header.name().as_bytes())
            .map_err(|e| Error::Auth(format!("bedrock: signed header name invalid: {e}")))?;
        let value = HeaderValue::from_str(header.value())
            .map_err(|e| Error::Auth(format!("bedrock: signed header value invalid: {e}")))?;
        req.headers_mut().insert(name, value);
    }

    if !added_params.is_empty() {
        // SigV4 query-string signing isn't expected for Bedrock POST
        // requests (we use header signing). If the SDK ever returns
        // params, surface that loudly so we can investigate.
        return Err(Error::Auth(format!(
            "bedrock: unexpected SigV4 query-string params from signer: {added_params:?}"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bedrock::BedrockCreds;
    use crate::bedrock::auth::resolve;

    #[tokio::test]
    async fn bearer_path_attaches_authorization_header() {
        let resolved = resolve(
            &BedrockCreds::BearerKey {
                key: "bedrock-api-key-xyz".into(),
            },
            "us-west-2",
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let mut req = client
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/model/test/invoke")
            .body("{}")
            .build()
            .unwrap();

        apply_auth(&mut req, &resolved, "us-west-2").await.unwrap();
        assert_eq!(
            req.headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer bedrock-api-key-xyz")
        );
    }

    #[tokio::test]
    async fn static_creds_path_attaches_sigv4_authorization_and_amz_date() {
        let resolved = resolve(
            &BedrockCreds::Static {
                access_key: "AKIATESTAKID".into(),
                secret_key: "test-secret-key".into(),
                session_token: None,
            },
            "us-west-2",
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let mut req = client
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/model/test/invoke")
            .body("{}")
            .build()
            .unwrap();

        apply_auth(&mut req, &resolved, "us-west-2").await.unwrap();

        // Authorization must be present and start with the SigV4 algo.
        let auth = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .expect("Authorization header set");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 "),
            "expected SigV4 prefix, got: {auth}"
        );
        // Credential scope embedded in Authorization must include
        // service=bedrock + region.
        assert!(
            auth.contains("/us-west-2/bedrock/aws4_request"),
            "missing region/service scope, got: {auth}"
        );
        // x-amz-date is mandatory for SigV4.
        assert!(req.headers().contains_key("x-amz-date"));
    }

    #[tokio::test]
    async fn static_creds_with_session_token_includes_security_token_header() {
        let resolved = resolve(
            &BedrockCreds::Static {
                access_key: "AKIATESTAKID".into(),
                secret_key: "test-secret-key".into(),
                session_token: Some("session-token-test".into()),
            },
            "us-west-2",
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        let mut req = client
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/model/test/invoke")
            .body("{}")
            .build()
            .unwrap();

        apply_auth(&mut req, &resolved, "us-west-2").await.unwrap();
        let token = req
            .headers()
            .get("x-amz-security-token")
            .and_then(|v| v.to_str().ok());
        assert_eq!(token, Some("session-token-test"));
    }
}
