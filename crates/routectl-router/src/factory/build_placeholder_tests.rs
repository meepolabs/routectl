//! A pool member whose credential probe fails is kept as a placeholder seat
//! whose cause is chosen by the store failure's variant, and a pool of
//! placeholders alone builds.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use routectl_auth::{CredentialProbeFailure, MemoryStore, OAuthStore, SecretRef, SecretStore};
use routectl_core::Result;

use super::build_resolved_models_reported;
use crate::config::{Config, ModelEntry, PoolEntry, ProviderEntry};
use crate::factory::BuildOptions;
use crate::pool_build::PoolPlaceholder;
use crate::runtime_state::UnusableCause;

const UNSET_ENV: &str = "ROUTECTL_PLACEHOLDER_BUILD_TEST_UNSET";

/// `oauth://` refs through a real `OAuthStore`, everything else through the
/// default resolver, the routing the daemon's composite store does.
struct Composite {
    oauth: OAuthStore,
    fallback: MemoryStore,
}

#[async_trait]
impl SecretStore for Composite {
    async fn get(&self, sr: &SecretRef) -> Result<String> {
        match sr {
            SecretRef::OAuth { .. } => self.oauth.get(sr).await,
            _ => self.fallback.get(sr).await,
        }
    }
    async fn set(&self, sr: &SecretRef, v: &str) -> Result<()> {
        self.fallback.set(sr, v).await
    }
    async fn delete(&self, sr: &SecretRef) -> Result<()> {
        self.fallback.delete(sr).await
    }
    async fn probe_credential(
        &self,
        sr: &SecretRef,
    ) -> std::result::Result<(), CredentialProbeFailure> {
        match sr {
            SecretRef::OAuth { .. } => self.oauth.probe_credential(sr).await,
            _ => self.fallback.probe_credential(sr).await,
        }
    }
    async fn account_id(&self, sr: &SecretRef) -> Result<Option<String>> {
        match sr {
            SecretRef::OAuth { .. } => self.oauth.account_id(sr).await,
            _ => self.fallback.account_id(sr).await,
        }
    }
}

/// A composite store over an empty credentials file: every OAuth seat is
/// logged out.
async fn logged_out_store() -> (tempfile::TempDir, Arc<dyn SecretStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let oauth = OAuthStore::open(dir.path().join("credentials.json"))
        .await
        .expect("open an empty store");
    let store = Composite {
        oauth,
        fallback: MemoryStore::new(),
    };
    (dir, Arc::new(store))
}

/// Pool `the-pool` over `members`, with model `opus` routed at it.
fn pool_config(members: Vec<(&str, ProviderEntry)>, upstream: &str) -> Config {
    let names: Vec<String> = members.iter().map(|(n, _)| (*n).to_string()).collect();
    let providers: BTreeMap<String, ProviderEntry> = members
        .into_iter()
        .map(|(n, e)| (n.to_string(), e))
        .collect();
    Config {
        providers,
        models: BTreeMap::from([("opus".to_string(), ModelEntry::new("the-pool", upstream))]),
        pools: BTreeMap::from([("the-pool".to_string(), PoolEntry::new(names))]),
        ..Config::default()
    }
}

fn logged_out_anthropic_seat() -> ProviderEntry {
    ProviderEntry::anthropic_api("oauth://anthropic#seat-a")
        .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer)
}

#[tokio::test]
async fn a_pool_of_unreadable_members_builds_with_one_placeholder_per_member() {
    // Arrange: seat-a is a logged-out OAuth seat, seat-b reads an unset env
    // var. The factory entry point is called directly: config validation is
    // a separate gate.
    let (_dir, store) = logged_out_store().await;
    assert!(
        std::env::var(UNSET_ENV).is_err(),
        "premise: the var is unset"
    );
    let cfg = pool_config(
        vec![
            ("seat-a", logged_out_anthropic_seat()),
            (
                "seat-b",
                ProviderEntry::anthropic_api(format!("env://{UNSET_ENV}")),
            ),
        ],
        "claude-opus-4-7",
    );

    // Act
    let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
        .await
        .expect("a pool of placeholders builds");

    // Assert
    let seats = built.models["opus"].seats.as_ref().expect("seats");
    assert!(
        seats
            .iter()
            .all(crate::seat_pool::SeatTarget::is_placeholder)
    );
    let report = &built.pool_reports[0];
    assert_eq!(report.usable_members, 0);
    assert_eq!(report.configured_members, 2);
    assert!(!report.is_unavailable(), "placeholders keep the pool");
    assert_eq!(
        report.placeholders,
        vec![
            PoolPlaceholder {
                member: "seat-a".into(),
                provider_kind: "anthropic-api",
                cause: UnusableCause::NotLoggedIn,
                account: Some("anthropic#seat-a".into()),
            },
            PoolPlaceholder {
                member: "seat-b".into(),
                provider_kind: "anthropic-api",
                cause: UnusableCause::Unreadable,
                account: Some("#seat-b".into()),
            },
        ]
    );
    let rendered = format!("{report:?}");
    for banned in [
        "routectl login",
        "no credentials",
        "not set",
        UNSET_ENV,
        "credentials.json",
        "oauth://",
    ] {
        assert!(
            !rendered.contains(banned),
            "the report must carry no error text (`{banned}`): {rendered}"
        );
    }
}

#[tokio::test]
async fn an_all_placeholder_pool_projects_an_inert_provider_that_refuses_locally() {
    // Arrange
    let (_dir, store) = logged_out_store().await;
    let cfg = pool_config(
        vec![("seat-a", logged_out_anthropic_seat())],
        "claude-opus-4-7",
    );

    // Act
    let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
        .await
        .expect("a pool of placeholders builds");
    let err = built.models["opus"]
        .provider
        .complete(routectl_core::ChatRequest::default())
        .await
        .unwrap_err();

    // Assert
    assert!(
        matches!(
            err,
            routectl_core::Error::LocallyUnavailable {
                reason: routectl_core::EligibilityReason::CredentialUnusable,
                retry_after: None,
            }
        ),
        "{err:?}"
    );
}

#[cfg(feature = "openai-responses")]
#[tokio::test]
async fn a_logged_out_codex_member_is_a_placeholder_not_a_build_error() {
    // Arrange: a chatgpt-oauth member has no account id to build with while
    // logged out, so only the placeholder keeps it in the pool.
    let (_dir, store) = logged_out_store().await;
    let codex = ProviderEntry::openai_responses("oauth://codex#seat-a")
        .with_openai_responses_auth_kind(
            routectl_providers::openai_responses::AuthKind::ChatgptOauth,
        );
    let cfg = pool_config(vec![("seat-a", codex)], "gpt-5-codex");

    // Act
    let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
        .await
        .expect("a logged-out codex member must not fail the build");

    // Assert
    assert!(built.failed.is_empty(), "{:?}", built.failed);
    let report = &built.pool_reports[0];
    assert!(report.omissions.is_empty(), "{:?}", report.omissions);
    assert_eq!(report.placeholders.len(), 1);
    assert_eq!(report.placeholders[0].cause, UnusableCause::NotLoggedIn);
    assert_eq!(
        report.placeholders[0].account.as_deref(),
        Some("codex#seat-a")
    );
}
