//! Pre-dispatch opening lookups: publication generation, route-head lane,
//! and per-lane calibrated estimate. Every lookup is also checked for what
//! it must NOT do -- move a rotation cursor, mint a sticky pin, count a pool
//! dispatch, or record calibration evidence -- with a positive control
//! proving the fixture would show the mutation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, Result};

use super::super::Router;
use crate::config::{
    AliasValue, Config, CredentialSource, PoolEntry, ProviderEntry, SeatSelection,
};
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;

struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(routectl_core::Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("opening lookups never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("opening lookups never dispatch")
    }
}

fn stub() -> Arc<dyn Provider> {
    Arc::new(StubProvider)
}

const OPENAI_KIND: &str = "openai-compat";
const ANTHROPIC_KIND: &str = "anthropic-api";

/// Two standalone models on distinct provider kinds, an alias chain whose
/// head is the second one, and a `default` catch-all naming the first.
fn standalone_config(calibration_enabled: bool) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "compat".into(),
        ProviderEntry::openai_compat("https://example.invalid/v1", "env://K"),
    );
    config
        .providers
        .insert("claude".into(), ProviderEntry::anthropic_api("env://K"));
    config.aliases.insert(
        "smart".into(),
        AliasValue::Chain(vec!["opus".into(), "glm".into()]),
    );
    config
        .aliases
        .insert("default".into(), AliasValue::Single("glm".into()));
    config.calibration.enabled = calibration_enabled;
    config
}

fn install_standalone(config: Config) -> Router {
    let mut router = Router::new(Arc::new(config));
    let mut models = BTreeMap::new();
    models.insert(
        "glm".to_string(),
        Arc::new(ResolvedModel::new("glm", "compat", stub(), "glm-4.6")),
    );
    models.insert(
        "opus".to_string(),
        Arc::new(ResolvedModel::new(
            "opus",
            "claude",
            stub(),
            "claude-opus-4-7",
        )),
    );
    router.install_resolved_models(models);
    router
}

fn standalone_router() -> Router {
    install_standalone(standalone_config(true))
}

/// One model on a two-member pool with the given seat selection.
fn pooled_router(selection: SeatSelection) -> Router {
    const MEMBERS: [&str; 2] = ["seat-a", "seat-b"];
    let mut config = Config::default();
    for member in MEMBERS {
        config.providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    config.pools.insert(
        "pool".into(),
        PoolEntry::new(MEMBERS.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(selection),
    );
    let seats: Arc<[SeatTarget]> = MEMBERS
        .iter()
        .map(|member| SeatTarget {
            provider_name: (*member).to_string(),
            provider: stub(),
            auth_secret_ref: None,
        })
        .collect::<Vec<_>>()
        .into();
    let mut router = Router::new(Arc::new(config));
    let mut models = BTreeMap::new();
    models.insert(
        "opus".to_string(),
        Arc::new(ResolvedModel::new("opus", "pool", stub(), "claude-opus-4-7").with_seats(seats)),
    );
    router.install_resolved_models(models);
    router
}

fn lead_member(router: &Router) -> String {
    router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .swap_remove(0)
        .provider_name
}

fn feed_lane(router: &Router, kind: &str, nickname: &str, ts: SystemTime) {
    for i in 0..9 {
        router.record_calibration_sample(
            Some(kind),
            Some(nickname),
            Some(&format!("caller-{}", i % 3)),
            1_000,
            2_000,
            ts,
        );
    }
}

fn retained_samples(router: &Router) -> usize {
    router
        .calibration_store
        .export_entries()
        .iter()
        .map(|(_, samples)| samples.iter().count())
        .sum()
}

// ------------------------------------------------- publication generation

#[test]
fn publication_generation_is_stable_within_one_publication() {
    let router = standalone_router();

    let first = router.publication_generation();
    let second = router.publication_generation();

    assert_eq!(first, second);
}

#[test]
fn a_config_only_republication_advances_the_generation_but_not_the_registry() {
    // Arrange: an outgoing Router, and a replacement built from the same
    // config and attached exactly as a config-only reload attaches it.
    let before = Arc::new(standalone_router());
    let mut after = standalone_router();
    after.carry_over_learned_from(&before);
    let after = Arc::new(after);
    let outgoing = before.publication_generation();

    // Act
    after.publish_probe_incarnation_into(|_| {});

    // Assert
    assert!(after.publication_generation() > outgoing);
    assert_eq!(
        before.publication_generation(),
        outgoing,
        "the outgoing Router keeps the generation it was published under"
    );
    assert_eq!(
        after.registry_generation(),
        before.registry_generation(),
        "negative control: the registry generation does not move on a \
         config-only reload, so it cannot stand in for the publication"
    );
}

#[test]
fn every_republication_draws_a_strictly_greater_generation() {
    let first = Arc::new(standalone_router());
    let mut second = standalone_router();
    second.carry_over_learned_from(&first);
    let second = Arc::new(second);
    second.publish_probe_incarnation_into(|_| {});
    let mut third = standalone_router();
    third.carry_over_learned_from(&second);
    let third = Arc::new(third);

    third.publish_probe_incarnation_into(|_| {});

    assert!(first.publication_generation() < second.publication_generation());
    assert!(second.publication_generation() < third.publication_generation());
}

// ---------------------------------------------------------- opening lane

#[test]
fn an_alias_resolves_to_its_head_target_lane() {
    let router = standalone_router();

    let lane = router.opening_lane("smart").expect("alias resolves");

    assert_eq!(lane.provider_kind, ANTHROPIC_KIND);
    assert_eq!(lane.nickname, "opus");
    assert_eq!(lane.upstream_model, "claude-opus-4-7");
    assert_eq!(lane.generation, router.publication_generation());
}

#[test]
fn a_direct_nickname_resolves_to_its_own_lane() {
    let router = standalone_router();

    let lane = router.opening_lane("glm").expect("nickname resolves");

    assert_eq!(lane.provider_kind, OPENAI_KIND);
    assert_eq!(lane.nickname, "glm");
    assert_eq!(lane.upstream_model, "glm-4.6");
}

#[test]
fn an_unmatched_model_falls_to_the_default_catch_all_like_dispatch() {
    let router = standalone_router();

    let lane = router
        .opening_lane("some-unlisted-model")
        .expect("default resolves");

    assert_eq!(lane.nickname, "glm");
}

#[test]
fn an_unresolvable_model_has_no_lane() {
    let mut config = standalone_config(true);
    config.aliases.remove("default");
    let router = install_standalone(config);

    assert_eq!(router.opening_lane("some-unlisted-model"), None);
}

#[test]
fn a_head_target_without_a_provider_kind_has_no_lane() {
    // The model's provider has no config entry, so dispatch would carry no
    // provider kind for it either.
    let mut router = Router::new(Arc::new(Config::default()));
    let mut models = BTreeMap::new();
    models.insert(
        "orphan".to_string(),
        Arc::new(ResolvedModel::new("orphan", "missing", stub(), "wire-id")),
    );
    router.install_resolved_models(models);

    assert_eq!(router.opening_lane("orphan"), None);
}

#[test]
fn a_forwarded_credential_head_reports_the_requested_model_as_its_wire_id() {
    let mut config = Config::default();
    config.providers.insert(
        "fwd".into(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    let mut router = Router::new(Arc::new(config));
    let mut models = BTreeMap::new();
    models.insert(
        "opus".to_string(),
        Arc::new(ResolvedModel::new("opus", "fwd", stub(), "claude-opus-4-7")),
    );
    router.install_resolved_models(models);

    let lane = router.opening_lane("opus").expect("resolves");

    assert_eq!(lane.upstream_model, "opus");
}

#[test]
fn a_pooled_head_names_the_model_lane_not_a_seat() {
    let router = pooled_router(SeatSelection::FillFirst);

    let lane = router.opening_lane("opus").expect("pooled model resolves");

    assert_eq!(lane.provider_kind, ANTHROPIC_KIND);
    assert_eq!(lane.nickname, "opus");
    assert_eq!(lane.upstream_model, "claude-opus-4-7");
}

#[test]
fn a_pool_mixing_forwarded_and_own_seats_has_no_lane() {
    let mut router = pooled_router(SeatSelection::FillFirst);
    let mut config = (*router.config).clone();
    config.providers.insert(
        "seat-b".into(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    router.config = Arc::new(config);

    assert_eq!(router.opening_lane("opus"), None);
}

#[test]
fn the_opening_lane_lookup_does_not_advance_seat_rotation() {
    // Arrange
    let router = pooled_router(SeatSelection::RoundRobin);

    // Act: many lookups, then one real chain resolution.
    for _ in 0..5 {
        let _ = router.opening_lane("opus");
    }
    let first_dispatch_lead = lead_member(&router);
    let second_dispatch_lead = lead_member(&router);

    // Assert: the first real dispatch still starts at the first seat.
    assert_eq!(first_dispatch_lead, "seat-a");
    assert_eq!(
        second_dispatch_lead, "seat-b",
        "positive control: a real chain resolution does rotate"
    );
}

#[test]
fn the_opening_lane_lookup_mints_no_sticky_pin_and_counts_no_pool_dispatch() {
    // Arrange
    let router = pooled_router(SeatSelection::StickyLeastLoaded);

    // Act
    let _ = router.opening_lane("opus");
    let pins_after_lookup = router.sticky_pins.len();
    let pool_dispatches_after_lookup = router.metrics.pool_totals().dispatch;
    let _ = router.dispatch_chain("opus", Some("session"));

    // Assert
    assert_eq!(pins_after_lookup, 0);
    assert_eq!(pool_dispatches_after_lookup, 0);
    assert_eq!(
        router.sticky_pins.len(),
        1,
        "positive control: a keyed chain resolution does pin"
    );
    assert_eq!(router.metrics.pool_totals().dispatch, 1);
}

// ---------------------------------------------------- calibrated estimate

#[test]
fn a_calibrated_lane_corrects_the_raw_estimate() {
    let router = standalone_router();
    feed_lane(&router, OPENAI_KIND, "glm", SystemTime::now());

    let corrected = router.calibrated_estimate(OPENAI_KIND, "glm", 10_000);

    assert_eq!(corrected, Some(20_000));
}

#[test]
fn a_factor_applies_only_to_its_own_lane() {
    let router = standalone_router();
    feed_lane(&router, OPENAI_KIND, "glm", SystemTime::now());

    assert_eq!(
        router.calibrated_estimate(OPENAI_KIND, "opus", 10_000),
        None
    );
    assert_eq!(
        router.calibrated_estimate(ANTHROPIC_KIND, "glm", 10_000),
        None
    );
}

#[test]
fn a_cold_lane_has_no_calibrated_estimate() {
    let router = standalone_router();

    assert_eq!(router.calibrated_estimate(OPENAI_KIND, "glm", 10_000), None);
}

#[test]
fn a_stale_lane_has_no_calibrated_estimate() {
    let router = standalone_router();
    feed_lane(&router, OPENAI_KIND, "glm", SystemTime::UNIX_EPOCH);

    assert_eq!(router.calibrated_estimate(OPENAI_KIND, "glm", 10_000), None);
}

#[test]
fn the_calibration_kill_switch_disables_the_estimate_and_keeps_the_evidence() {
    let router = install_standalone(standalone_config(false));
    feed_lane(&router, OPENAI_KIND, "glm", SystemTime::now());

    assert_eq!(router.calibrated_estimate(OPENAI_KIND, "glm", 10_000), None);
    assert_eq!(retained_samples(&router), 9);
}

#[test]
fn a_calibrated_lookup_records_no_evidence() {
    // Arrange: one calibrated lane, and a lookup on an unseen one.
    let router = standalone_router();
    feed_lane(&router, OPENAI_KIND, "glm", SystemTime::now());
    let before = retained_samples(&router);

    // Act
    let _ = router.calibrated_estimate(OPENAI_KIND, "glm", 10_000);
    let _ = router.calibrated_estimate(ANTHROPIC_KIND, "opus", 10_000);

    // Assert
    assert_eq!(retained_samples(&router), before);
    assert_eq!(
        router.calibration_lanes(),
        vec![(OPENAI_KIND.to_string(), "glm".to_string())],
        "a lookup on an unseen lane must not create it"
    );
}
