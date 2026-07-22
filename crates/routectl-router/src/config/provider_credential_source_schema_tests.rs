//! Config-BOUNDARY (parse-via-`toml`) schema tests for
//! `ProviderEntry::AnthropicApi.credential_source`: field-coherence
//! (host pin, `api_key_ref` matrix) lives in
//! `factory::validate_provider_credential_sources_tests` -- this
//! module pins only the serde SHAPE: default value, round-trip, and
//! that `deny_unknown_fields` makes the field unrepresentable on
//! every other `[providers.X]` kind.

use crate::config::{Config, CredentialSource, ProviderEntry};

#[test]
fn anthropic_api_credential_source_defaults_to_own_when_omitted() {
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default");
    match cfg.providers.get("anthropic").expect("anthropic provider") {
        ProviderEntry::AnthropicApi {
            credential_source, ..
        } => assert_eq!(*credential_source, CredentialSource::Own),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// The 4-line forwarded block from the docs/spec: no `api_key_ref`
/// line at all, `credential_source = "forwarded"`. Must parse
/// cleanly -- `api_key_ref` is `#[serde(default)]` precisely so this
/// shape is representable.
#[test]
fn anthropic_api_forwarded_block_with_no_api_key_ref_parses() {
    let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("forwarded block must parse");
    match cfg
        .providers
        .get("anthropic-forwarded")
        .expect("anthropic-forwarded provider")
    {
        ProviderEntry::AnthropicApi {
            credential_source,
            api_key_ref,
            ..
        } => {
            assert_eq!(*credential_source, CredentialSource::Forwarded);
            assert!(api_key_ref.is_empty(), "got: {api_key_ref:?}");
        }
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// An empty `api_key_ref` (the forwarded shape) must NOT surface as a
/// secret URI to resolve -- `commands::config::check` iterates every
/// `secret_uris()` entry through `SecretRef::parse`, which would
/// reject an empty string as an unrecognized scheme and fail an
/// otherwise-clean forwarded provider.
#[test]
fn forwarded_entry_reports_no_secret_uris() {
    let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get("anthropic-forwarded").unwrap();
    assert!(
        entry.secret_uris().is_empty(),
        "got: {:?}",
        entry.secret_uris()
    );
}

#[test]
fn anthropic_api_credential_source_round_trips_through_toml() {
    let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let reserialized = toml::to_string(&cfg).expect("re-serialize");
    let cfg2: Config = toml::from_str(&reserialized).expect("re-parse");
    match cfg2.providers.get("anthropic-forwarded").unwrap() {
        ProviderEntry::AnthropicApi {
            credential_source, ..
        } => assert_eq!(*credential_source, CredentialSource::Forwarded),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

#[test]
fn anthropic_api_rejects_unknown_credential_source_value() {
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
credential_source = "borrowed"
"#;
    let err = toml::from_str::<Config>(toml_text)
        .expect_err("unknown credential_source value must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("borrowed") || msg.contains("unknown variant") || msg.contains("expected"),
        "expected an unknown-variant parse error; got: {msg}"
    );
}

/// `deny_unknown_fields` at the enum level makes `credential_source`
/// unrepresentable on the `openai-compat` kind -- the field lives
/// ONLY on the `AnthropicApi` variant.
#[test]
fn credential_source_is_rejected_on_openai_compat() {
    let toml_text = r#"
[providers.example]
kind = "openai-compat"
base_url = "https://api.openai.com"
api_key_ref = "env://OPENAI_API_KEY"
credential_source = "forwarded"
"#;
    let err = toml::from_str::<Config>(toml_text)
        .expect_err("credential_source must not parse on openai-compat");
    let msg = err.to_string();
    assert!(
        msg.contains("credential_source") || msg.contains("unknown field"),
        "expected unknown-field error naming credential_source; got: {msg}"
    );
}

/// Same guarantee as `credential_source_is_rejected_on_openai_compat`,
/// pinned separately for the `openai-responses` kind -- the task's
/// acceptance criteria name both kinds explicitly.
#[cfg(feature = "openai-responses")]
#[test]
fn credential_source_is_rejected_on_openai_responses() {
    let toml_text = r#"
[providers.example]
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
auth_kind = "api-key"
credential_source = "forwarded"
"#;
    let err = toml::from_str::<Config>(toml_text)
        .expect_err("credential_source must not parse on openai-responses");
    let msg = err.to_string();
    assert!(
        msg.contains("credential_source") || msg.contains("unknown field"),
        "expected unknown-field error naming credential_source; got: {msg}"
    );
}
