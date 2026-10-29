//! The dispatch meta a token count returns: the walk advances it on this
//! surface as on the message surfaces.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{ChatRequest, Error, Result};

use super::Router;
use super::scripted_provider_test_support::ScriptedProvider;
use crate::config::{AliasValue, Config, ProviderEntry};
use crate::resolved::ResolvedModel;

const ALIAS: &str = "a";
const FIRST: &str = "m1";
const SECOND: &str = "m2";

fn cannot_count(_: &ChatRequest) -> Result<()> {
    Err(Error::upstream("scripted", 501, "cannot count"))
}

fn counts(_: &ChatRequest) -> Result<()> {
    Ok(())
}

/// Two count-capable seats, each its own anthropic-api provider entry.
fn two_seat_router(first: Arc<ScriptedProvider>, second: Arc<ScriptedProvider>) -> Router {
    let mut config = Config::default();
    let mut models = BTreeMap::new();
    for (nickname, provider) in [(FIRST, first), (SECOND, second)] {
        let provider_name = format!("p-{nickname}");
        config.providers.insert(
            provider_name.clone(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        let model = ResolvedModel::new(nickname, provider_name, provider, format!("up-{nickname}"));
        models.insert(nickname.to_string(), Arc::new(model));
    }
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(vec![FIRST.into(), SECOND.into()]),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn a_count_served_by_the_second_seat_records_both_attempts_the_hop_and_the_serving_seat() {
    // Arrange
    let first = ScriptedProvider::new(None, cannot_count);
    let second = ScriptedProvider::new(None, counts);
    let router = two_seat_router(first.clone(), second.clone());

    // Act
    let counted = Box::pin(router.count_tokens_with_meta(ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    }))
    .await;

    // Assert
    assert!(counted.result.is_ok(), "the second seat counts");
    assert_eq!((first.calls(), second.calls()), (1, 1));
    let meta = counted.meta;
    assert_eq!(
        (
            meta.attempt_count,
            meta.fallback_count,
            meta.served_provider.as_deref(),
            meta.served_model.as_deref(),
            meta.served_upstream.as_deref(),
        ),
        (2, 1, Some("p-m2"), Some(SECOND), Some("up-m2")),
    );
}
