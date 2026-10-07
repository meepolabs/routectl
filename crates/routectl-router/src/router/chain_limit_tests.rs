//! Defensive runtime bounds also apply when a library caller skips validation.

use super::*;
use crate::alias_limits::MAX_EXPANDED_TARGETS;
use crate::config::{AliasValue, ModelEntry, PoolEntry, ProviderEntry};
use crate::factory::validate_alias_chain_targets;
use crate::seat_pool::SeatTarget;

struct UncalledProvider;

#[async_trait::async_trait]
impl Provider for UncalledProvider {
    fn id(&self) -> &'static str {
        "native"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        unreachable!()
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn configured_router(
    aliases: BTreeMap<String, AliasValue>,
    native: bool,
    seat_count: usize,
) -> Router {
    let provider: Arc<dyn Provider> = Arc::new(UncalledProvider);
    let mut cfg = Config {
        aliases,
        ..Config::default()
    };
    let entry = if native {
        ProviderEntry::anthropic_api("env://UNUSED_TEST_KEY")
    } else {
        ProviderEntry::openai_compat("https://example.invalid/v1", "env://UNUSED_TEST_KEY")
    };
    cfg.providers.insert("native".into(), entry);
    cfg.models
        .insert("leaf".into(), ModelEntry::new("native", "vendor/model"));
    let model = if seat_count > 0 {
        cfg.models.get_mut("leaf").unwrap().provider = "pool".into();
        cfg.pools.insert(
            "pool".into(),
            PoolEntry::new(vec!["native".into(); seat_count]),
        );
        let seats = (0..seat_count)
            .map(|i| SeatTarget {
                provider_name: format!("seat-{i}"),
                provider: provider.clone(),
                auth_secret_ref: None,
            })
            .collect::<Vec<_>>();
        ResolvedModel::new("leaf", "pool", provider, "vendor/model").with_seats(seats.into())
    } else {
        ResolvedModel::new("leaf", "native", provider, "vendor/model")
    };
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(BTreeMap::from([("leaf".into(), Arc::new(model))]));
    router
}

fn chain_of(entry: &str, count: usize) -> BTreeMap<String, AliasValue> {
    BTreeMap::from([("root".into(), AliasValue::Chain(vec![entry.into(); count]))])
}

#[test]
fn runtime_exact_and_over_bound_for_native_and_compat_model_chains() {
    for native in [true, false] {
        let router = configured_router(chain_of("leaf", MAX_EXPANDED_TARGETS), native, 0);
        validate_alias_chain_targets(&router.config).unwrap();
        assert_eq!(
            router.dispatch_chain("root", None).unwrap().len(),
            MAX_EXPANDED_TARGETS
        );
        let router = configured_router(chain_of("leaf", MAX_EXPANDED_TARGETS + 1), native, 0);
        let err = router
            .dispatch_chain("root", None)
            .err()
            .expect("must reject expansion")
            .to_string();
        assert!(
            err.contains("4096") && err.contains("config check"),
            "{err}"
        );
    }
}

#[test]
fn runtime_counts_pool_seats_before_building_or_rotating_targets() {
    let router = configured_router(chain_of("leaf", MAX_EXPANDED_TARGETS / 2), true, 2);
    assert_eq!(
        router.dispatch_chain("root", None).unwrap().len(),
        MAX_EXPANDED_TARGETS
    );
    let router = configured_router(chain_of("leaf", MAX_EXPANDED_TARGETS / 2 + 1), true, 2);
    assert!(
        router
            .dispatch_chain("root", None)
            .err()
            .expect("must reject expansion")
            .to_string()
            .contains("4096")
    );
    assert_eq!(
        router.metrics.pool_totals().dispatch,
        0,
        "oversize fails before pool side effects"
    );
}

#[test]
fn runtime_preserves_shared_dag_duplicates_in_depth_first_retry_order() {
    let aliases = BTreeMap::from([
        (
            "root".into(),
            AliasValue::Chain(vec!["shared".into(), "leaf".into(), "shared".into()]),
        ),
        (
            "shared".into(),
            AliasValue::Chain(vec!["leaf".into(), "leaf".into()]),
        ),
    ]);
    let router = configured_router(aliases, true, 2);
    let chain = router.dispatch_chain("root", None).unwrap();
    let names: Vec<_> = chain.iter().map(|t| t.provider_name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "seat-0", "seat-1", "seat-0", "seat-1", "seat-0", "seat-1", "seat-0", "seat-1",
            "seat-0", "seat-1"
        ]
    );
}

#[test]
fn runtime_rejects_million_leaf_dag_even_when_all_models_are_unavailable() {
    let mut aliases = BTreeMap::new();
    let mut next = "missing".to_string();
    for i in (0..6).rev() {
        let key = format!("level-{i}");
        aliases.insert(key.clone(), AliasValue::Chain(vec![next; 10]));
        next = key;
    }
    let router = configured_router(aliases, true, 0);
    assert!(
        router
            .dispatch_chain("level-0", None)
            .err()
            .expect("must reject expansion")
            .to_string()
            .contains("4096")
    );
}

#[test]
fn runtime_empty_branch_graph_has_a_work_bound_too() {
    let mut aliases = BTreeMap::from([("empty".into(), AliasValue::Chain(vec![]))]);
    let mut next = "empty".to_string();
    for i in (0..6).rev() {
        let key = format!("level-{i}");
        aliases.insert(key.clone(), AliasValue::Chain(vec![next; 10]));
        next = key;
    }
    aliases.insert("root".into(), AliasValue::Single("level-0".into()));
    let router = configured_router(aliases, true, 0);
    assert!(
        router
            .dispatch_chain("root", None)
            .err()
            .expect("must reject expansion")
            .to_string()
            .contains("4096")
    );
}

#[test]
fn runtime_depth_and_cycle_safety_still_fail_closed() {
    let mut aliases = BTreeMap::new();
    let mut next = "leaf".to_string();
    for i in (0..=ALIAS_MAX_RECURSION_DEPTH).rev() {
        let key = format!("level-{i}");
        aliases.insert(key.clone(), AliasValue::Single(next));
        next = key;
    }
    let router = configured_router(aliases.clone(), true, 0);
    assert_eq!(router.dispatch_chain("level-0", None).unwrap().len(), 1);
    aliases.insert("root".into(), AliasValue::Single("level-0".into()));
    let router = configured_router(aliases, true, 0);
    assert!(
        router
            .dispatch_chain("root", None)
            .err()
            .expect("must reject expansion")
            .to_string()
            .contains("depth 8")
    );
    let router = configured_router(chain_of("root", 1), true, 0);
    assert!(
        router
            .dispatch_chain("root", None)
            .err()
            .expect("must reject expansion")
            .to_string()
            .contains("depth 8")
    );
}

#[test]
fn default_and_glob_aliases_obey_the_same_runtime_bound_without_echoing_input() {
    let hostile = "hostile\n\x1b[31m";
    let mut aliases = chain_of("leaf", MAX_EXPANDED_TARGETS + 1);
    let value = aliases.remove("root").unwrap();
    aliases.insert("default".into(), value.clone());
    aliases.insert("hostile*".into(), value);
    let router = configured_router(aliases, true, 0);
    for wire in [hostile, "unknown"] {
        let err = router
            .dispatch_chain(wire, None)
            .err()
            .expect("must reject expansion")
            .to_string();
        assert!(
            !err.contains(hostile) && !err.contains('\n') && !err.contains('\x1b'),
            "{err:?}"
        );
        assert!(err.contains("4096"), "{err}");
    }
}
