//! `Router::has_forwarded_provider()`: build-time cached, `true` iff
//! `config.providers` contains a `ProviderEntry::AnthropicApi` with
//! `credential_source == Forwarded`. Replaces the removed `[mitm]
//! credential_source` read as the CAPTURE gate's "configured
//! capability" half (see `routectl_cli::handlers::ingress_handle`).
use super::*;
use crate::config::{CredentialSource, ProviderEntry};
use std::collections::BTreeMap;

fn router_with_providers(providers: BTreeMap<String, ProviderEntry>) -> Router {
    Router::new(Arc::new(Config {
        providers,
        ..Default::default()
    }))
}

#[test]
fn false_when_no_providers_configured() {
    let router = router_with_providers(BTreeMap::new());
    assert!(!router.has_forwarded_provider());
}

#[test]
fn false_when_only_own_credential_providers_configured() {
    let mut providers = BTreeMap::new();
    providers.insert(
        "own-anthropic".to_string(),
        ProviderEntry::anthropic_api("literal:k").with_credential_source(CredentialSource::Own),
    );
    providers.insert(
        "own-compat".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
    );
    let router = router_with_providers(providers);
    assert!(!router.has_forwarded_provider());
}

#[test]
fn true_when_a_forwarded_anthropic_provider_is_configured() {
    let mut providers = BTreeMap::new();
    providers.insert(
        "forwarded".to_string(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    let router = router_with_providers(providers);
    assert!(router.has_forwarded_provider());
}

#[test]
fn true_when_a_forwarded_provider_coexists_with_own_credential_providers() {
    let mut providers = BTreeMap::new();
    providers.insert(
        "own-compat".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", "literal:k"),
    );
    providers.insert(
        "forwarded".to_string(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    let router = router_with_providers(providers);
    assert!(
        router.has_forwarded_provider(),
        "coexistence must not hide the forwarded provider"
    );
}
