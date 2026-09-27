//! Hot-reload carry-over of runtime-state slots is keyed by state key AND
//! slot owner: a key that survives the rebuild under a different upstream
//! identity starts fresh, while a key whose owner is unchanged keeps the
//! exact prior slot.

use super::*;
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;
use std::collections::BTreeMap;
use std::time::Duration;

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

const PROVIDERS: &[&str] = &["p1", "p2", "anthropic-a"];

fn direct(nickname: &str, provider_name: &str) -> Arc<ResolvedModel> {
    Arc::new(ResolvedModel::new(
        nickname,
        provider_name,
        Arc::new(NullProvider),
        "u",
    ))
}

fn pooled(nickname: &str, members: &[&str]) -> Arc<ResolvedModel> {
    let seats: Vec<SeatTarget> = members
        .iter()
        .map(|member| SeatTarget {
            provider_name: (*member).to_string(),
            provider: Arc::new(NullProvider),
            auth_secret_ref: None,
        })
        .collect();
    Arc::new(
        ResolvedModel::new(nickname, "pool", Arc::new(NullProvider), "u")
            .with_seats(Arc::from(seats)),
    )
}

/// A freshly built Router over the shared provider set with `model` installed
/// under its own nickname -- the shape each side of a reload has.
fn built_with(model: Arc<ResolvedModel>) -> Router {
    let mut config = Config::default();
    for name in PROVIDERS {
        config
            .providers
            .insert((*name).to_string(), ProviderEntry::anthropic_api("env://K"));
    }
    let mut router = Router::new(Arc::new(config));
    let table: BTreeMap<_, _> = std::iter::once((model.nickname.clone(), model)).collect();
    router.install_resolved_models(table);
    router
}

fn tripped(router: &Router, key: &str) -> Arc<Mutex<crate::runtime_state::ProviderState>> {
    assert!(
        router.force_open_breaker(key, Duration::from_hours(1)),
        "`{key}` must own a slot to trip"
    );
    assert_eq!(router.breaker_open_for(key), Some(true));
    router.state[key].clone()
}

#[test]
fn a_direct_model_moved_to_another_provider_starts_with_fresh_state() {
    // Arrange
    let old = built_with(direct("m", "p1"));
    let old_slot = tripped(&old, "m");
    let mut new = built_with(direct("m", "p2"));
    let fresh_slot = new.state["m"].clone();

    // Act
    new.carry_over_runtime_state_from(&old);

    // Assert
    assert!(!Arc::ptr_eq(&new.state["m"], &old_slot));
    assert!(Arc::ptr_eq(&new.state["m"], &fresh_slot));
    assert_eq!(new.breaker_open_for("m"), Some(false));
    assert_eq!(
        new.slot_owners.get("m"),
        Some(&state_slots::SlotOwner::Provider("p2".into()))
    );
}

#[test]
fn a_direct_model_turned_pooled_starts_with_fresh_state() {
    // Arrange
    let old = built_with(direct("opus", "p1"));
    let old_slot = tripped(&old, "opus");
    let mut new = built_with(pooled("opus", &["anthropic-a"]));
    let fresh_slot = new.state["opus"].clone();

    // Act
    new.carry_over_runtime_state_from(&old);

    // Assert
    assert!(!Arc::ptr_eq(&new.state["opus"], &old_slot));
    assert!(Arc::ptr_eq(&new.state["opus"], &fresh_slot));
    assert_eq!(new.breaker_open_for("opus"), Some(false));
    assert_eq!(new.breaker_open_for("opus#anthropic-a"), Some(false));
    assert_eq!(
        new.slot_owners.get("opus"),
        Some(&state_slots::SlotOwner::Pool("pool".into()))
    );
}

#[test]
fn a_direct_model_on_the_same_provider_keeps_its_exact_slot() {
    // Arrange
    let old = built_with(direct("m", "p1"));
    let old_slot = tripped(&old, "m");
    let mut new = built_with(direct("m", "p1"));

    // Act
    new.carry_over_runtime_state_from(&old);

    // Assert
    assert!(Arc::ptr_eq(&new.state["m"], &old_slot));
    assert_eq!(new.breaker_open_for("m"), Some(true));
}

#[test]
fn a_pooled_model_on_the_same_pool_keeps_its_exact_nickname_and_seat_slots() {
    // Arrange
    let old = built_with(pooled("opus", &["anthropic-a"]));
    let old_nickname_slot = tripped(&old, "opus");
    let old_seat_slot = tripped(&old, "opus#anthropic-a");
    let mut new = built_with(pooled("opus", &["anthropic-a"]));

    // Act
    new.carry_over_runtime_state_from(&old);

    // Assert
    assert!(Arc::ptr_eq(&new.state["opus"], &old_nickname_slot));
    assert!(Arc::ptr_eq(&new.state["opus#anthropic-a"], &old_seat_slot));
    assert_eq!(new.breaker_open_for("opus"), Some(true));
    assert_eq!(new.breaker_open_for("opus#anthropic-a"), Some(true));
}
