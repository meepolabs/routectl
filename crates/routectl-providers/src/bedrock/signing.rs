//! SigV4 signing for Bedrock-runtime requests.
//!
//! `apply` is the single entry point. Given a fully-built
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
//! The signing service scope is a parameter: `apply` signs in the
//! `bedrock` scope, while `apply_with_service` lets other AWS-signed
//! lanes (e.g. mantle) pass their own scope. The region comes from the
//! caller and is also part of the signing scope.

use std::time::SystemTime;

use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

use routectl_core::{Error, Result};

use super::auth::ResolvedCreds;

/// Apply the appropriate authentication scheme to `req` in place,
/// signing SigV4 requests in the `bedrock` service scope.
///
/// For `Bearer` -- attaches `Authorization: Bearer <key>`.
/// For `Sigv4`  -- fetches the latest credentials from the provider,
/// SigV4-signs the request, and merges the auth headers into `req`.
pub async fn apply(req: &mut reqwest::Request, creds: &ResolvedCreds, region: &str) -> Result<()> {
    apply_with_service(req, creds, region, "bedrock").await
}

/// Apply the appropriate authentication scheme to `req` in place,
/// signing SigV4 requests in the given `service` scope.
///
/// Identical to [`apply`] except the SigV4 service name is a parameter,
/// letting non-`bedrock` AWS-signed lanes select their own scope.
///
/// For `Bearer` -- attaches `Authorization: Bearer <key>`.
/// For `Sigv4`  -- fetches the latest credentials from the provider,
/// SigV4-signs the request in `service` scope, and merges the auth
/// headers into `req`.
pub async fn apply_with_service(
    req: &mut reqwest::Request,
    creds: &ResolvedCreds,
    region: &str,
    service: &str,
) -> Result<()> {
    match creds {
        ResolvedCreds::Bearer { key } => {
            tracing::debug!(auth_kind = "Bearer", region = %region, service = %service, "applying bedrock auth");
            let value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| {
                tracing::error!(
                    failure_kind = "bearer_header_invalid",
                    service = %service,
                    error = %e,
                    "bedrock auth failed",
                );
                Error::Auth(format!("{service}: invalid bearer key: {e}"))
            })?;
            req.headers_mut().insert(AUTHORIZATION, value);
            Ok(())
        }
        ResolvedCreds::Sigv4 { provider } => {
            tracing::debug!(auth_kind = "Sigv4", region = %region, service = %service, "applying bedrock auth");
            let credentials = provider.provide_credentials().await.map_err(|e| {
                tracing::error!(
                    failure_kind = "creds_unavailable",
                    region = %region,
                    service = %service,
                    error = %e,
                    "bedrock auth failed",
                );
                Error::Auth(format!("{service}: credentials unavailable: {e}"))
            })?;
            sigv4_sign(req, &credentials, region, service)
        }
    }
}

fn sigv4_sign(
    req: &mut reqwest::Request,
    credentials: &aws_credential_types::Credentials,
    region: &str,
    service: &str,
) -> Result<()> {
    // Materialize body bytes for the canonical-request hash. Bedrock
    // request bodies are JSON we built ourselves with `.body(Vec<u8>)`,
    // so they always serialize to in-memory bytes. Refuse anything
    // else explicitly: signing an empty hash for a streaming body
    // would produce a syntactically valid request that AWS rejects
    // with a cryptic 403 (signature mismatch) -- diagnosing that
    // failure is much harder than this up-front error.
    // Borrow the body bytes directly. reqwest's `Body::as_bytes()` returns
    // `&[u8]` borrowed from the request itself; sign() consumes the
    // SignableRequest synchronously, so the borrow is released before we
    // touch `req.headers_mut()` below. Avoids a per-request copy.
    let body_bytes = req.body().and_then(|b| b.as_bytes()).ok_or_else(|| {
        tracing::error!(failure_kind = "body_unbuffered", service = %service, "bedrock auth failed",);
        Error::Auth(format!(
            "{service}: cannot SigV4-sign a streaming or unbuffered body; \
                 body() must resolve to in-memory bytes"
        ))
    })?;
    let body = SignableBody::Bytes(body_bytes);

    let identity = credentials.clone().into();
    let signing_settings = SigningSettings::default();

    let v4_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .map_err(|e| {
            tracing::error!(
                failure_kind = "signing_params_build",
                service = %service,
                error = %e,
                "bedrock auth failed",
            );
            Error::Auth(format!("{service}: signing params build failed: {e}"))
        })?;
    let signing_params = v4_params.into();

    // Collect headers as (&str, &str) pairs for SignableRequest. Non-ASCII
    // values (HeaderValue::to_str() failure) MUST NOT be silently dropped:
    // the actual outbound request still carries the header, but the signing
    // input wouldn't, yielding `SignatureDoesNotMatch` on the AWS side that
    // is opaque to debug. Fail fast and name the offending header.
    let header_pairs: Vec<(&str, &str)> = req
        .headers()
        .iter()
        .map(|(k, v)| {
            v.to_str().map(|val| (k.as_str(), val)).map_err(|e| {
                tracing::error!(
                    failure_kind = "non_ascii_header",
                    header = %k.as_str(),
                    service = %service,
                    error = %e,
                    "bedrock auth failed",
                );
                Error::Auth(format!(
                    "{service}: header `{}` has non-ASCII value, cannot SigV4-sign: {e}",
                    k.as_str()
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let method = req.method().as_str();
    let url = req.url().as_str();

    let signable =
        SignableRequest::new(method, url, header_pairs.into_iter(), body).map_err(|e| {
            tracing::error!(
                failure_kind = "signable_request_build",
                service = %service,
                error = %e,
                "bedrock auth failed",
            );
            Error::Auth(format!("{service}: signable request build failed: {e}"))
        })?;

    let (instructions, _signature) = sign(signable, &signing_params)
        .map_err(|e| {
            tracing::error!(
                failure_kind = "sigv4_sign",
                region = %region,
                service = %service,
                error = %e,
                "bedrock auth failed",
            );
            Error::Auth(format!("{service}: SigV4 sign failed: {e}"))
        })?
        .into_parts();

    // Merge the signing instructions back into the reqwest::Request.
    let (added_headers, added_params) = instructions.into_parts();
    for header in added_headers {
        let name = HeaderName::from_bytes(header.name().as_bytes()).map_err(|e| {
            tracing::error!(
                failure_kind = "signed_header_name_invalid",
                name = %header.name(),
                service = %service,
                error = %e,
                "bedrock auth failed",
            );
            Error::Auth(format!("{service}: signed header name invalid: {e}"))
        })?;
        let value = HeaderValue::from_str(header.value()).map_err(|e| {
            tracing::error!(
                failure_kind = "signed_header_value_invalid",
                service = %service,
                error = %e,
                "bedrock auth failed",
            );
            Error::Auth(format!("{service}: signed header value invalid: {e}"))
        })?;
        req.headers_mut().insert(name, value);
    }

    if !added_params.is_empty() {
        // SigV4 query-string signing isn't expected for these POST
        // requests (we use header signing). If the SDK ever returns
        // params, surface that loudly so we can investigate.
        tracing::error!(
            failure_kind = "unexpected_query_params",
            service = %service,
            params = ?added_params,
            "bedrock auth failed",
        );
        return Err(Error::Auth(format!(
            "{service}: unexpected SigV4 query-string params from signer: {added_params:?}"
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

        apply(&mut req, &resolved, "us-west-2").await.unwrap();
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
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/model/test/invoke")
            .body("{}")
            .build()
            .unwrap();

        apply(&mut req, &resolved, "us-west-2").await.unwrap();

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
    async fn non_ascii_header_value_is_explicit_error_not_silent_drop() {
        // Defends against the "header silently dropped from signing input"
        // class of bugs: the signed-header set must equal the actual sent
        // headers, otherwise AWS returns SignatureDoesNotMatch which is
        // opaque to debug. We surface the offending header name up-front.
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
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/model/test/invoke")
            .body("{}")
            .build()
            .unwrap();
        // Insert a non-ASCII byte sequence that to_str() refuses.
        let bad_value = HeaderValue::from_bytes(b"\xC0\xC1 oops").expect("constructable");
        req.headers_mut().insert("x-routectl-bad", bad_value);

        let err = apply(&mut req, &resolved, "us-west-2")
            .await
            .expect_err("non-ASCII header must error explicitly");
        let msg = err.to_string();
        assert!(msg.contains("x-routectl-bad"), "error names header: {msg}");
        assert!(
            msg.contains("non-ASCII") || msg.contains("cannot SigV4-sign"),
            "error explains why: {msg}"
        );
    }

    #[tokio::test]
    async fn static_creds_with_session_token_includes_security_token_header() {
        let resolved = resolve(
            &BedrockCreds::Static {
                access_key: "testkey-sign-xyz".into(),
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

        apply(&mut req, &resolved, "us-west-2").await.unwrap();
        let token = req
            .headers()
            .get("x-amz-security-token")
            .and_then(|v| v.to_str().ok());
        assert_eq!(token, Some("session-token-test"));
    }
}
