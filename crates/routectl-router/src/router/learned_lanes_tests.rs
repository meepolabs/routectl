//! The learned-lane projection agrees with dispatch: a built router reads its
//! lanes off the targets chain expansion mints, and the config-only walk
//! projects the same lanes for the same configuration.

use super::*;

use routectl_auth::{MemoryStore, SecretStore};

use crate::config::{ModelEntry, PoolEntry};
use crate::factory::{BuildOptions, build_resolved_models};

const KIND: &str = "openai-compat";

fn entry() -> ProviderEntry {
    ProviderEntry::openai_compat(
        "https://example.invalid/v1",
        crate::test_secret::file_ref("k"),
    )
}

/// Two nicknames on one upstream of `p`, a third on another upstream, a
/// pooled model over members `m1` / `m2`, and a disabled model on its own
/// upstream of `p`.
fn config() -> Config {
    let mut config = Config::default();
    for name in ["p", "m1", "m2"] {
        config.providers.insert(name.to_string(), entry());
    }
    config.pools.insert(
        "pool".to_string(),
        PoolEntry::new(vec!["m1".to_string(), "m2".to_string()]),
    );
    for (nickname, provider, upstream) in [
        ("alpha", "p", "up"),
        ("beta", "p", "up"),
        ("gamma", "p", "other"),
        ("pooled", "pool", "seat-up"),
    ] {
        config
            .models
            .insert(nickname.to_string(), ModelEntry::new(provider, upstream));
    }
    let mut disabled = ModelEntry::new("p", "parked");
    disabled.selectable = false;
    config.models.insert("delta".to_string(), disabled);
    config
}

async fn built_router(config: Config) -> Router {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let (models, failed) = build_resolved_models(&config, store, BuildOptions::default())
        .await
        .expect("the fixture builds");
    assert!(failed.is_empty(), "no model fails to build: {failed:?}");
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn summary(projection: &LearnedLaneProjection) -> Vec<(String, Vec<String>, &'static str, bool)> {
    projection
        .lanes()
        .iter()
        .map(|l| {
            (
                l.lane.as_lane_key().to_string(),
                l.nicknames.clone(),
                l.provider_kind,
                l.routed,
            )
        })
        .collect()
}

fn row(lane: &str, nicknames: &[&str], routed: bool) -> (String, Vec<String>, &'static str, bool) {
    (
        lane.to_string(),
        nicknames.iter().map(|n| (*n).to_string()).collect(),
        KIND,
        routed,
    )
}

#[test]
fn the_config_projection_names_each_lane_once_with_the_nicknames_on_it() {
    let projection = LearnedLaneProjection::from_config(&config());

    assert_eq!(
        summary(&projection),
        [
            row("m1#seat-up", &["pooled"], true),
            row("m2#seat-up", &["pooled"], true),
            row("p#other", &["gamma"], true),
            row("p#parked", &["delta"], false),
            row("p#up", &["alpha", "beta"], true),
        ]
    );
    assert_eq!(
        projection
            .lane_for("pooled", "m2")
            .map(|l| l.lane.as_lane_key()),
        Some("m2#seat-up")
    );
    assert!(projection.lane_for("pooled", "pool").is_none());
}

#[tokio::test]
async fn the_router_projection_matches_the_config_projection_for_every_dispatched_lane() {
    let config = config();
    let from_config = LearnedLaneProjection::from_config(&config);

    let router = built_router(config).await;
    let from_router = router.learned_lane_projection();

    let routed: Vec<_> = summary(&from_config)
        .into_iter()
        .filter(|(_, _, _, routed)| *routed)
        .collect();
    assert_eq!(summary(&from_router), routed);
}

#[tokio::test]
async fn every_status_target_carries_the_lane_dispatch_mints_for_it() {
    let router = built_router(config()).await;
    let projection = router.learned_lane_projection();

    let targets = router.status_targets(std::time::Instant::now());

    assert_eq!(targets.len(), 5, "alpha, beta, gamma and two pooled seats");
    for target in &targets {
        let lane = target
            .learned_lane
            .as_ref()
            .expect("every target has a lane");
        let projected = projection
            .lane_for(&target.nickname, &target.provider_name)
            .unwrap_or_else(|| panic!("no projected lane for {}", target.state_key));
        assert_eq!(lane, &projected.lane, "target {}", target.state_key);
        assert_eq!(lane.upstream(), target.upstream);
        assert_ne!(
            lane.as_lane_key(),
            target.state_key,
            "the lane is not the runtime key"
        );
    }
    let pooled: Vec<&str> = targets
        .iter()
        .filter(|t| t.nickname == "pooled")
        .filter_map(|t| t.learned_lane.as_ref().map(StateKey::as_lane_key))
        .collect();
    assert_eq!(pooled, ["m1#seat-up", "m2#seat-up"]);
}
