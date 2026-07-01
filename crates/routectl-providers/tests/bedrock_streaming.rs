//! Scoped bedrock integration tests exercising the credential-resolution
//! and auth-dispatch public API.
//!
//! # Scope and rationale
//!
//! `BedrockProvider` has no `base_url` override, so a full wiremock HTTP
//! test driving `BedrockProvider::stream()` is impossible from a `tests/`
//! file without editing `src/`. Similarly, `signing::apply` requires
//! a `reqwest::Request`, and `eventstream::invoke_stream` requires
//! `aws-smithy-types` + `aws-smithy-eventstream` frame builders -- both
//! of which are optional `[dependencies]` that are NOT in
//! `[dev-dependencies]` and therefore not directly importable by
//! integration test binaries.
//!
//! The inline unit tests already cover those paths thoroughly:
//!   - `src/bedrock/signing.rs` tests `apply` with Static, Bearer,
//!     session-token, and non-ASCII-header cases, pinning the
//!     `AWS4-HMAC-SHA256` prefix + credential scope.
//!   - `src/bedrock/eventstream.rs` tests `invoke_stream` with prelude-
//!     split frames, truncation, ping frames, text-delta decode, and
//!     malformed-frame recovery.
//!
//! What this file adds as external integration coverage:
//!   - `auth::resolve` maps credential variants to `ResolvedCreds`
//!     variants correctly (visible from the public enum alone, no AWS
//!     crates needed).
//!   - The `BedrockCreds` enum is round-trippable (all variants that
//!     `resolve` handles asynchronously complete without error on the
//!     static/bearer paths).

#![cfg(feature = "bedrock")]

use routectl_providers::bedrock::BedrockCreds;
use routectl_providers::bedrock::auth::{ResolvedCreds, resolve};

// ---------------------------------------------------------------------------
// auth::resolve -- credential variant mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bearer_key_resolves_to_bearer_variant() {
    let creds = BedrockCreds::BearerKey {
        key: "bedrock-console-api-key".into(),
    };
    let resolved = resolve(&creds, "us-east-1")
        .await
        .expect("BearerKey must resolve without error");
    assert!(
        matches!(resolved, ResolvedCreds::Bearer { .. }),
        "BearerKey must resolve to ResolvedCreds::Bearer; got: {resolved:?}"
    );
}

#[tokio::test]
async fn static_creds_resolve_to_sigv4_variant() {
    let creds = BedrockCreds::Static {
        access_key: "testkey-stream-001".into(),
        secret_key: "test-secret-key".into(),
        session_token: None,
    };
    let resolved = resolve(&creds, "us-east-1")
        .await
        .expect("Static creds must resolve without error");
    assert!(
        matches!(resolved, ResolvedCreds::Sigv4 { .. }),
        "Static creds must resolve to ResolvedCreds::Sigv4; got: {resolved:?}"
    );
}

#[tokio::test]
async fn static_creds_with_session_token_resolve_to_sigv4_variant() {
    let creds = BedrockCreds::Static {
        access_key: "testkey-stream-002".into(),
        secret_key: "test-secret-key".into(),
        session_token: Some("test-session-token".into()),
    };
    let resolved = resolve(&creds, "ap-northeast-1")
        .await
        .expect("Static+session-token must resolve without error");
    assert!(
        matches!(resolved, ResolvedCreds::Sigv4 { .. }),
        "Static+session-token must resolve to ResolvedCreds::Sigv4; got: {resolved:?}"
    );
}

/// Pin that different regions do not cause resolve() to fail on the
/// static-creds path. Region is embedded in the signing scope at
/// sign-time, not at resolve-time, so this is primarily a smoke test.
#[tokio::test]
async fn static_creds_resolve_succeeds_across_regions() {
    let creds = BedrockCreds::Static {
        access_key: "testkey-stream-003".into(),
        secret_key: "test-secret-key".into(),
        session_token: None,
    };
    for region in ["us-east-1", "us-west-2", "eu-west-1", "ap-northeast-1"] {
        resolve(&creds, region)
            .await
            .unwrap_or_else(|e| panic!("resolve failed for region {region}: {e}"));
    }
}
