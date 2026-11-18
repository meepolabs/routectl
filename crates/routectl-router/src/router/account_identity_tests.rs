//! `Router::account_identities` and `AccountIdentities::from_config` name the
//! same accounts under the same identity: the smallest entry naming an
//! account, and the label of its own `oauth://` credential.

use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_providers::anthropic_api::AuthKind;

use super::{AccountIdentities, AccountIdentity, Router};
use crate::config::{Auth, Config, ModelEntry, PoolEntry, ProviderEntry};
use crate::factory::{BuildOptions, build_resolved_models};

/// Resolves every reference to a fixed token, so no test reads an
/// environment variable, a file or the managed credential store.
struct FixedTokenStore;

#[async_trait::async_trait]
impl SecretStore for FixedTokenStore {
    async fn get(&self, _: &SecretRef) -> routectl_core::Result<String> {
        Ok("token".into())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
}

fn oauth_entry(uri: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(uri).with_auth_kind(AuthKind::OauthBearer)
}

async fn built_router(config: Config) -> Router {
    let store: Arc<dyn SecretStore> = Arc::new(FixedTokenStore);
    let (models, failed) = build_resolved_models(&config, store, BuildOptions::default())
        .await
        .expect("the fixture builds");
    assert!(failed.is_empty(), "no model fails to build: {failed:?}");
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn listed(identities: &AccountIdentities) -> Vec<(String, String, Option<String>)> {
    identities
        .iter()
        .map(|(account, identity)| {
            (
                account.to_string(),
                identity.provider_entry.clone(),
                identity.login_label.clone(),
            )
        })
        .collect()
}

fn row(account: &str, entry: &str, label: Option<&str>) -> (String, String, Option<String>) {
    (
        account.to_string(),
        entry.to_string(),
        label.map(str::to_string),
    )
}

/// One login reached directly through `zeta` and pooled through `alpha`, a
/// second login only in the pool, an unlabeled login, an `env://` key and a
/// forwarded entry. `model_names` orders the models so the larger entry is
/// met first in one call and last in the other.
fn mixed_config(model_names: [&str; 4]) -> Config {
    let mut config = Config::default();
    for (entry, provider) in [
        ("zeta", oauth_entry("oauth://anthropic#work")),
        ("alpha", oauth_entry("oauth://anthropic#work")),
        ("bob-seat", oauth_entry("oauth://anthropic#bob")),
        ("plain", oauth_entry("oauth://anthropic")),
        (
            "keyed",
            ProviderEntry::anthropic_api("env://ROUTECTL_ACCOUNT_IDENTITY_TEST_KEY"),
        ),
        (
            "relay",
            ProviderEntry::anthropic_api("forwarded")
                .with_credential("forwarded")
                .with_auth(Auth::Bearer),
        ),
    ] {
        config.providers.insert(entry.into(), provider);
    }
    config.pools.insert(
        "team".into(),
        PoolEntry::new(vec!["bob-seat".into(), "alpha".into()]),
    );
    let [direct, pooled, plain, rest] = model_names;
    for (nickname, provider) in [(direct, "zeta"), (pooled, "team"), (plain, "plain")] {
        config.models.insert(
            nickname.into(),
            ModelEntry::new(provider, "claude-opus-4-7"),
        );
    }
    config
        .models
        .insert(rest.into(), ModelEntry::new("keyed", "claude-opus-4-7"));
    config.models.insert(
        "z-relay".into(),
        ModelEntry::new("relay", "claude-opus-4-7"),
    );
    config
}

fn expected() -> Vec<(String, String, Option<String>)> {
    vec![
        row("#keyed", "keyed", None),
        row("anthropic", "plain", None),
        row("anthropic#bob", "bob-seat", Some("bob")),
        row("anthropic#work", "alpha", Some("work")),
    ]
}

#[tokio::test]
async fn a_built_router_lists_each_account_once_under_its_smallest_entry() {
    for names in [
        ["a-direct", "b-pooled", "c-plain", "d-keyed"],
        ["d-direct", "a-pooled", "c-plain", "b-keyed"],
    ] {
        // Arrange
        let config = mixed_config(names);
        crate::factory::validate_pools(&config).expect("a servable pool");
        let router = built_router(config).await;

        // Act
        let identities = router.account_identities();

        // Assert
        assert_eq!(listed(&identities), expected(), "{names:?}");
    }
}

/// The config walk and the built router name the same accounts under the
/// same identity, so a surface holding only a config agrees with the
/// status panels.
#[tokio::test]
async fn the_config_walk_matches_the_built_router() {
    for names in [
        ["a-direct", "b-pooled", "c-plain", "d-keyed"],
        ["d-direct", "a-pooled", "c-plain", "b-keyed"],
    ] {
        // Arrange
        let config = mixed_config(names);
        let router = built_router(config.clone()).await;

        // Act
        let from_config = AccountIdentities::from_config(&config);

        // Assert
        assert_eq!(from_config, router.account_identities(), "{names:?}");
        assert_eq!(listed(&from_config), expected(), "{names:?}");
    }
}

#[test]
fn lookups_answer_for_listed_accounts_only() {
    // Arrange
    let identities = AccountIdentities::from_config(&mixed_config([
        "a-direct", "b-pooled", "c-plain", "d-keyed",
    ]));

    // Act
    let work = identities.get("anthropic#work");

    // Assert
    assert_eq!(
        work,
        Some(&AccountIdentity {
            provider_entry: "alpha".into(),
            login_label: Some("work".into()),
        })
    );
    assert!(identities.contains("#keyed"));
    assert!(!identities.contains("anthropic#gone"));
    assert!(
        !identities.contains("#relay"),
        "a forwarded entry names no account"
    );
    assert_eq!(identities.get("anthropic#gone"), None);
}
