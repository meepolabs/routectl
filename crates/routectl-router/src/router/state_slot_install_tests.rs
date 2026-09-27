//! The install boundary of the runtime-state map: `install_resolved_models`
//! is public and takes any table, so it must refuse a model whose state key
//! another identity already holds or could compose, while an exact identity
//! reinstall keeps the same slots.

use super::*;
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;
use std::collections::BTreeMap;

struct NullProvider;

#[async_trait::async_trait]
impl Provider for NullProvider {
    fn id(&self) -> &'static str {
        "null"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<routectl_core::ChatResponse> {
        Err(Error::normalize_response("null", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<routectl_core::ChatResponse> {
        unreachable!("never dispatched")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<routectl_core::ChatChunk>>> {
        unreachable!("never dispatched")
    }
}

fn provider() -> Arc<dyn Provider> {
    Arc::new(NullProvider)
}

fn direct(nickname: &str, provider_name: &str) -> Arc<ResolvedModel> {
    Arc::new(ResolvedModel::new(nickname, provider_name, provider(), "u"))
}

fn pooled(nickname: &str, members: &[&str]) -> Arc<ResolvedModel> {
    let seats: Vec<SeatTarget> = members
        .iter()
        .map(|member| SeatTarget {
            provider_name: (*member).to_string(),
            provider: provider(),
            auth_secret_ref: None,
        })
        .collect();
    Arc::new(ResolvedModel::new(nickname, "pool", provider(), "u").with_seats(Arc::from(seats)))
}

/// A Router over an UNVALIDATED config declaring `providers` -- the shape a
/// library caller reaches without the config suite.
fn router_with_providers(providers: &[&str]) -> Router {
    let mut config = Config::default();
    for name in providers {
        config
            .providers
            .insert((*name).to_string(), ProviderEntry::anthropic_api("env://K"));
    }
    Router::new(Arc::new(config))
}

fn table(models: Vec<(&str, Arc<ResolvedModel>)>) -> BTreeMap<String, Arc<ResolvedModel>> {
    models
        .into_iter()
        .map(|(key, model)| (key.to_string(), model))
        .collect()
}

fn state_keys(router: &Router) -> Vec<&str> {
    router.state.keys().map(String::as_str).collect()
}

#[test]
fn valid_direct_and_pooled_models_keep_their_exact_state_key_bytes() {
    // Positive control for every refusal below, in the same shape.
    let mut router = router_with_providers(&["anthropic-a", "anthropic-b", "direct-p"]);

    router.install_resolved_models(table(vec![
        ("haiku", direct("haiku", "direct-p")),
        ("opus", pooled("opus", &["anthropic-a", "anthropic-b"])),
    ]));

    assert_eq!(
        state_keys(&router),
        vec![
            "anthropic-a",
            "anthropic-b",
            "direct-p",
            "haiku",
            "opus",
            "opus#anthropic-a",
            "opus#anthropic-b",
        ]
    );
    assert!(router.resolved_models.contains_key("haiku"));
    assert!(router.resolved_models.contains_key("opus"));
}

#[test]
fn a_direct_nickname_carrying_the_separator_is_refused_at_install() {
    // `opus#anthropic-a` is the key pooled `opus` composes for that member.
    let mut router = router_with_providers(&["anthropic-a", "direct-p"]);

    router.install_resolved_models(table(vec![
        ("opus", pooled("opus", &["anthropic-a"])),
        ("opus#anthropic-a", direct("opus#anthropic-a", "direct-p")),
    ]));

    assert!(!router.resolved_models.contains_key("opus#anthropic-a"));
    assert!(
        router.resolved_models.contains_key("opus"),
        "the valid sibling still installs"
    );
    assert!(
        router
            .status_targets(Instant::now())
            .iter()
            .all(|t| t.nickname != "opus#anthropic-a"),
        "a refused model must not surface in status"
    );
}

#[test]
fn a_direct_nickname_carrying_the_separator_gets_no_slot_of_its_own() {
    let mut router = router_with_providers(&["direct-p"]);

    router.install_resolved_models(table(vec![("a#b", direct("a#b", "direct-p"))]));

    assert!(!router.resolved_models.contains_key("a#b"));
    assert!(!router.state.contains_key("a#b"));
}

#[test]
fn a_pooled_member_name_carrying_the_separator_is_refused_at_install() {
    // Model `a` on member `b#c` composes `a#b#c`, the same key model `a#b`
    // on member `c` would compose.
    let mut router = router_with_providers(&["b#c"]);

    router.install_resolved_models(table(vec![("a", pooled("a", &["b#c"]))]));

    assert!(!router.resolved_models.contains_key("a"));
    assert!(!router.state.contains_key("a#b#c"));
    assert!(!router.state.contains_key("a"));
}

#[test]
fn a_direct_nickname_equal_to_another_providers_slot_is_refused_at_install() {
    // Arrange: `shared` is a provider slot Router::new seeded from provider
    // `shared`; the model dispatches through `other`.
    let mut router = router_with_providers(&["shared", "other"]);
    let provider_slot = router.state.get("shared").cloned().expect("provider slot");

    // Act
    router.install_resolved_models(table(vec![("shared", direct("shared", "other"))]));

    // Assert
    assert!(!router.resolved_models.contains_key("shared"));
    assert!(
        Arc::ptr_eq(&router.state["shared"], &provider_slot),
        "the provider's slot is left untouched"
    );
}

#[test]
fn a_direct_model_named_after_its_own_provider_reuses_that_slot() {
    // Positive control for the refusal above: the occupied slot was seeded
    // from the very provider this model dispatches through, so it is the
    // same gate rather than someone else's.
    let mut router = router_with_providers(&["m1"]);
    let provider_slot = router.state.get("m1").cloned().expect("provider slot");

    router.install_resolved_models(table(vec![("m1", direct("m1", "m1"))]));

    assert!(router.resolved_models.contains_key("m1"));
    assert!(Arc::ptr_eq(&router.state["m1"], &provider_slot));
}

#[test]
fn a_pooled_seat_key_already_held_by_a_provider_slot_is_refused_at_install() {
    // Neither half carries the separator, so only the occupancy check sees
    // that an unvalidated provider entry already holds `opus#m`.
    let mut router = router_with_providers(&["m", "opus#m"]);
    let provider_slot = router.state.get("opus#m").cloned().expect("provider slot");

    router.install_resolved_models(table(vec![
        ("opus", pooled("opus", &["m"])),
        ("haiku", direct("haiku", "m")),
    ]));

    assert!(!router.resolved_models.contains_key("opus"));
    assert!(!router.state.contains_key("opus"));
    assert!(Arc::ptr_eq(&router.state["opus#m"], &provider_slot));
    assert!(
        router.resolved_models.contains_key("haiku"),
        "the valid sibling still installs"
    );
}

#[test]
fn a_model_filed_under_a_different_key_than_its_own_nickname_is_refused() {
    // Dispatch keys a model's state by its own nickname, so a table key that
    // disagrees would leave dispatch on a slot install never checked.
    let mut router = router_with_providers(&["shared"]);

    router.install_resolved_models(table(vec![("elsewhere", direct("shared", "shared"))]));

    assert!(router.resolved_models.is_empty());
    assert!(!router.state.contains_key("elsewhere"));
}

#[test]
fn an_exact_identity_reinstall_keeps_every_slot() {
    // Arrange
    let mut router = router_with_providers(&["anthropic-a", "direct-p"]);
    let models = || {
        table(vec![
            ("haiku", direct("haiku", "direct-p")),
            ("opus", pooled("opus", &["anthropic-a"])),
        ])
    };
    router.install_resolved_models(models());
    let before: Vec<_> = ["haiku", "opus", "opus#anthropic-a"]
        .iter()
        .map(|key| router.state[*key].clone())
        .collect();

    // Act
    router.install_resolved_models(models());

    // Assert
    assert!(router.resolved_models.contains_key("haiku"));
    assert!(router.resolved_models.contains_key("opus"));
    for (key, slot) in ["haiku", "opus", "opus#anthropic-a"].iter().zip(&before) {
        assert!(
            Arc::ptr_eq(&router.state[*key], slot),
            "{key} kept its slot"
        );
    }
}

#[test]
fn a_key_readded_under_another_provider_after_an_empty_table_stays_refused() {
    // Arrange: `m` is seeded from `p1`, then the whole table is removed. The
    // slot outlives the table, so its owner must too -- otherwise `m` via a
    // provider literally named `m` would pass as "a model named after its own
    // provider" and inherit `p1`'s breaker and RPM bucket.
    let mut router = router_with_providers(&["p1"]);
    router.install_resolved_models(table(vec![("m", direct("m", "p1"))]));
    let p1_slot = router.state.get("m").cloned().expect("model slot");
    router.install_resolved_models(BTreeMap::new());
    assert!(router.resolved_models.is_empty());

    // Act
    router.install_resolved_models(table(vec![("m", direct("m", "m"))]));

    // Assert
    assert!(!router.resolved_models.contains_key("m"));
    assert!(
        Arc::ptr_eq(&router.state["m"], &p1_slot),
        "the retained slot is neither replaced nor handed to the new identity"
    );
}

#[test]
fn a_pooled_nickname_readded_as_a_direct_model_after_an_empty_table_stays_refused() {
    // The pooled model's own slot was seeded from pool `pool`; a direct model
    // through a provider named `opus` is a different identity.
    let mut router = router_with_providers(&["anthropic-a"]);
    router.install_resolved_models(table(vec![("opus", pooled("opus", &["anthropic-a"]))]));
    let pool_slot = router.state.get("opus").cloned().expect("model slot");
    router.install_resolved_models(BTreeMap::new());

    router.install_resolved_models(table(vec![("opus", direct("opus", "opus"))]));

    assert!(!router.resolved_models.contains_key("opus"));
    assert!(Arc::ptr_eq(&router.state["opus"], &pool_slot));
}

#[test]
fn an_exact_identity_readded_after_an_empty_table_reuses_its_retained_slots() {
    // Positive control for the two refusals above, over the same sequence.
    let mut router = router_with_providers(&["anthropic-a", "p1"]);
    let models = || {
        table(vec![
            ("m", direct("m", "p1")),
            ("opus", pooled("opus", &["anthropic-a"])),
        ])
    };
    router.install_resolved_models(models());
    let before: Vec<_> = ["m", "opus", "opus#anthropic-a"]
        .iter()
        .map(|key| router.state[*key].clone())
        .collect();
    router.install_resolved_models(BTreeMap::new());

    router.install_resolved_models(models());

    assert!(router.resolved_models.contains_key("m"));
    assert!(router.resolved_models.contains_key("opus"));
    for (key, slot) in ["m", "opus", "opus#anthropic-a"].iter().zip(&before) {
        assert!(
            Arc::ptr_eq(&router.state[*key], slot),
            "{key} kept its slot"
        );
    }
}

fn pooled_via(nickname: &str, pool: &str, members: &[&str]) -> Arc<ResolvedModel> {
    let seats: Vec<SeatTarget> = members
        .iter()
        .map(|member| SeatTarget {
            provider_name: (*member).to_string(),
            provider: provider(),
            auth_secret_ref: None,
        })
        .collect();
    Arc::new(ResolvedModel::new(nickname, pool, provider(), "u").with_seats(Arc::from(seats)))
}

#[test]
fn a_pooled_model_named_after_a_provider_is_refused_at_install() {
    // Arrange: `anthropic-work` is both a provider slot and the nickname of a
    // model dispatching through a pool of that very provider.
    let mut router = router_with_providers(&["anthropic-work"]);
    let provider_slot = router.state.get("anthropic-work").cloned().expect("slot");

    // Act
    router.install_resolved_models(table(vec![(
        "anthropic-work",
        pooled_via("anthropic-work", "anthropic", &["anthropic-work"]),
    )]));

    // Assert
    assert!(router.resolved_models.is_empty());
    assert!(Arc::ptr_eq(&router.state["anthropic-work"], &provider_slot));
    assert!(!router.state.contains_key("anthropic-work#anthropic-work"));
}

#[test]
fn a_pooled_model_named_after_a_provider_is_refused_even_when_its_pool_shares_that_name() {
    // An unvalidated table can name the pool after the provider too. The
    // provider slot is still a provider's, not the pool's, so the same-name
    // exception for direct models must not apply.
    let mut router = router_with_providers(&["anthropic-work", "anthropic-b"]);
    let provider_slot = router.state.get("anthropic-work").cloned().expect("slot");

    router.install_resolved_models(table(vec![(
        "anthropic-work",
        pooled_via("anthropic-work", "anthropic-work", &["anthropic-b"]),
    )]));

    assert!(router.resolved_models.is_empty());
    assert!(Arc::ptr_eq(&router.state["anthropic-work"], &provider_slot));
    assert!(!router.state.contains_key("anthropic-work#anthropic-b"));
}
