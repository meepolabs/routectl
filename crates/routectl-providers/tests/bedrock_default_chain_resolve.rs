//! Default-chain credential resolution under a scrubbed AWS environment.
//!
//! Building aws-config's default chain eagerly constructs its web-identity
//! provider, whose STS client requires an async sleep implementation. Without
//! one the build PANICS at router build -- it never reaches the probe that
//! would turn a credential-less environment into a clean `Error::Auth`.
//!
//! # Why this is its own test binary
//!
//! The chain reads `AWS_*` variables from the PROCESS environment, so each
//! case scrubs every one of them and points the shared config / credentials
//! files at paths that do not exist. Doing that inside a shared test binary
//! would race every sibling that resolves AWS credentials, so this binary
//! carries only these cases and marks each `#[serial_test::serial]`.

#![cfg(feature = "bedrock")]

use std::ffi::OsString;
use std::time::Duration;

use aws_credential_types::provider::ProvideCredentials;
use routectl_core::Error;
use routectl_providers::bedrock::BedrockCreds;
use routectl_providers::bedrock::auth::{ResolvedCreds, resolve};
use routectl_testkit::ScopedEnv;

const REGION: &str = "us-west-2";

/// Far above a credential-less resolve (no network: IMDS is disabled and no
/// container / web-identity endpoint is configured), far below a hang.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(10);

/// Remove every inherited `AWS_*` variable, disable IMDS, and point the shared
/// config and credentials files at paths that do not exist, so no ambient
/// developer or CI credential can satisfy the chain.
fn scrubbed_aws_env() -> Vec<ScopedEnv> {
    let inherited: Vec<OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("AWS_"))
        .collect();
    let mut guards: Vec<ScopedEnv> = inherited.iter().map(ScopedEnv::unset).collect();

    let absent = std::env::temp_dir().join(format!(
        "routectl-bedrock-default-chain-absent-{}",
        std::process::id()
    ));
    guards.push(ScopedEnv::set("AWS_EC2_METADATA_DISABLED", "true"));
    guards.push(ScopedEnv::set("AWS_CONFIG_FILE", absent.join("config")));
    guards.push(ScopedEnv::set(
        "AWS_SHARED_CREDENTIALS_FILE",
        absent.join("credentials"),
    ));
    guards
}

async fn resolve_default_chain() -> routectl_core::Result<ResolvedCreds> {
    tokio::time::timeout(
        RESOLVE_DEADLINE,
        resolve(&BedrockCreds::DefaultChain, REGION),
    )
    .await
    .expect("default-chain resolve must finish inside the deadline")
}

/// With no credential source anywhere, the chain fails its probe cleanly as
/// the fixed auth literal instead of panicking while it is being built.
#[tokio::test]
#[serial_test::serial]
async fn default_chain_without_any_credential_source_fails_as_auth_error() {
    let _env = scrubbed_aws_env();

    let result = resolve_default_chain().await;

    match result {
        Err(Error::Auth(msg)) => assert_eq!(msg, "aws credentials unavailable"),
        other => panic!("expected Error::Auth, got {other:?}"),
    }
}

/// Environment credentials are the first link of the chain: they resolve to a
/// SigV4 provider that hands back exactly those values.
#[tokio::test]
#[serial_test::serial]
async fn default_chain_with_environment_credentials_resolves_to_them() {
    let _env = scrubbed_aws_env();
    let _key = ScopedEnv::set("AWS_ACCESS_KEY_ID", "AKIDSYNTHETICTEST");
    let _secret = ScopedEnv::set("AWS_SECRET_ACCESS_KEY", "synthetic-secret-test");

    let resolved = resolve_default_chain().await.expect("resolve");

    let ResolvedCreds::Sigv4 { provider } = resolved else {
        panic!("expected Sigv4, got {resolved:?}");
    };
    let creds = provider.provide_credentials().await.expect("provide");
    assert_eq!(creds.access_key_id(), "AKIDSYNTHETICTEST");
    assert_eq!(creds.secret_access_key(), "synthetic-secret-test");
}
