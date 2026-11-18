//! A route target's `login_label` is the label of its own `oauth://` seat
//! credential, read from a router the factory built from config.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use routectl_auth::{SecretRef, SecretStore};
use routectl_providers::anthropic_api::AuthKind;

use super::Router;
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

fn forwarded_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("forwarded")
        .with_credential("forwarded")
        .with_auth(Auth::Bearer)
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

/// `(state_key, (seat_label, login_label))` for every status target.
fn identities(router: &Router) -> BTreeMap<String, (Option<String>, Option<String>)> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .map(|t| (t.state_key, (t.seat_label, t.login_label)))
        .collect()
}

#[tokio::test]
async fn a_single_target_carries_the_label_of_its_oauth_credential_only() {
    let rows: [(&str, ProviderEntry, Option<&str>); 5] = [
        (
            "labeled oauth seat",
            oauth_entry("oauth://anthropic#work"),
            Some("work"),
        ),
        (
            "unlabeled oauth seat",
            oauth_entry("oauth://anthropic"),
            None,
        ),
        (
            "env credential",
            ProviderEntry::anthropic_api("env://ROUTECTL_LOGIN_LABEL_TEST_KEY"),
            None,
        ),
        (
            "file credential",
            ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            None,
        ),
        ("forwarded credential", forwarded_entry(), None),
    ];
    for (name, entry, expected) in rows {
        // Arrange
        let mut config = Config::default();
        config.providers.insert("entry".into(), entry);
        config
            .models
            .insert("opus".into(), ModelEntry::new("entry", "claude-opus-4-7"));
        let router = built_router(config).await;

        // Act
        let targets = router.status_targets(Instant::now());

        // Assert
        assert_eq!(targets.len(), 1, "{name}");
        assert_eq!(targets[0].login_label.as_deref(), expected, "{name}");
    }
}

#[tokio::test]
async fn each_pool_seat_carries_its_own_login_label_beside_its_member_entry() {
    // Arrange: member entry names differ from the login labels, so a label
    // read off the entry name rather than the credential shows here.
    let mut config = Config::default();
    for (member, uri) in [
        ("member-a", "oauth://anthropic#alice"),
        ("member-b", "oauth://anthropic#bob"),
        ("member-c", "oauth://anthropic"),
    ] {
        config.providers.insert(member.into(), oauth_entry(uri));
    }
    config.pools.insert(
        "pool".into(),
        PoolEntry::new(vec![
            "member-a".into(),
            "member-b".into(),
            "member-c".into(),
        ]),
    );
    config
        .models
        .insert("opus".into(), ModelEntry::new("pool", "claude-opus-4-7"));
    crate::factory::validate_pools(&config).expect("a servable pool");
    let router = built_router(config).await;

    // Act
    let rows = identities(&router);

    // Assert
    let seat =
        |member: &str, label: Option<&str>| (Some(member.to_string()), label.map(str::to_string));
    let expected: BTreeMap<String, (Option<String>, Option<String>)> = [
        ("opus#member-a".to_string(), seat("member-a", Some("alice"))),
        ("opus#member-b".to_string(), seat("member-b", Some("bob"))),
        ("opus#member-c".to_string(), seat("member-c", None)),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows, expected);
}
