//! The config-only resolver against the router's own dispatch: for every
//! precedence step, the chain read off the config matches the chain
//! `dispatch_chain` builds from the installed model table.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

use super::*;
use crate::config::ModelEntry;
use crate::resolved::ResolvedModel;
use crate::router::Router;

struct InertProvider;

#[async_trait]
impl Provider for InertProvider {
    fn id(&self) -> &'static str {
        "inert"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("inert", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("inert", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("inert", "unused"))
    }
}

fn single(target: &str) -> AliasValue {
    AliasValue::Single(target.to_string())
}

fn chain(targets: &[&str]) -> AliasValue {
    AliasValue::Chain(targets.iter().map(|t| (*t).to_string()).collect())
}

fn config_with(models: &[&str], aliases: &[(&str, AliasValue)]) -> Config {
    let mut config = Config::default();
    for nickname in models {
        config.models.insert(
            (*nickname).to_string(),
            ModelEntry::new(format!("p-{nickname}"), format!("u-{nickname}")),
        );
    }
    for (key, value) in aliases {
        config.aliases.insert((*key).to_string(), value.clone());
    }
    config
}

/// A router whose installed model table holds every `[models]` nickname
/// of `config`, so dispatch and the config-only resolver see one world.
fn router_for(config: &Config) -> Router {
    let mut router = Router::new(Arc::new(config.clone()));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, entry) in &config.models {
        let provider: Arc<dyn Provider> = Arc::new(InertProvider);
        resolved.insert(
            nickname.clone(),
            Arc::new(ResolvedModel::new(
                nickname.clone(),
                entry.provider.clone(),
                provider,
                entry.upstream.clone(),
            )),
        );
    }
    router.install_resolved_models(resolved);
    router
}

fn dispatched_nicknames(router: &Router, wire: &str) -> Vec<String> {
    router
        .dispatch_chain(wire, None)
        .expect("dispatch_chain resolves the wire name")
        .into_iter()
        .map(|target| target.nickname.expect("a direct target names its model"))
        .collect()
}

fn resolved_nicknames(config: &Config, wire: &str) -> Vec<String> {
    let value = resolve_alias_value(config, wire).expect("the wire name resolves");
    alias_steps(config, &value)
        .expect("the chain is within the depth bound")
        .into_iter()
        .flat_map(|step| step.models)
        .collect()
}

fn precedence_config() -> Config {
    config_with(
        &[
            "m-a",
            "m-b",
            "m-c",
            "shadowed",
            "claude-x-direct",
            "glob-shadowed-1",
        ],
        &[
            ("fast", chain(&["m-b", "m-a"])),
            ("claude-*", single("m-a")),
            ("claude-x-*", chain(&["m-b", "m-c"])),
            ("shadowed", chain(&["m-c", "m-a"])),
            ("glob-shadowed-*", single("m-b")),
            ("default", chain(&["m-c", "m-b"])),
        ],
    )
}

#[test]
fn resolve_alias_value_picks_the_chain_dispatch_chain_dispatches() {
    // Arrange
    let config = precedence_config();
    let router = router_for(&config);
    let rows: [(&str, &str, &[&str]); 7] = [
        ("exact key", "fast", &["m-b", "m-a"]),
        ("shorter of two globs", "claude-y", &["m-a"]),
        ("longest of two globs", "claude-x-9", &["m-b", "m-c"]),
        ("direct nickname", "m-c", &["m-c"]),
        ("alias shadows a nickname", "shadowed", &["m-c", "m-a"]),
        ("glob shadows a nickname", "glob-shadowed-1", &["m-b"]),
        ("default only", "no-such-wire", &["m-c", "m-b"]),
    ];

    for (case, wire, expected) in rows {
        // Act
        let resolved = resolved_nicknames(&config, wire);
        let dispatched = dispatched_nicknames(&router, wire);

        // Assert
        assert_eq!(resolved, dispatched, "{case}: resolver vs dispatch");
        assert_eq!(resolved, expected, "{case}: expected chain");
    }
}

#[test]
fn resolve_alias_value_glob_shadows_a_nickname_it_also_prefixes() {
    // Arrange: `claude-x-direct` is a model nickname AND matches both globs.
    let config = precedence_config();
    let router = router_for(&config);

    // Act
    let resolved = resolved_nicknames(&config, "claude-x-direct");

    // Assert
    assert_eq!(resolved, dispatched_nicknames(&router, "claude-x-direct"));
    assert_eq!(resolved, ["m-b", "m-c"]);
}

#[test]
fn resolve_alias_value_is_none_without_a_match_or_default() {
    // Arrange
    let config = config_with(&["m-a"], &[("fast", single("m-a"))]);

    // Act / Assert: positive control first, then the miss.
    assert!(matches!(
        resolve_alias_value(&config, "m-a"),
        Some(AliasValue::Single(name)) if name == "m-a"
    ));
    assert!(resolve_alias_value(&config, "unknown").is_none());
}

#[test]
fn alias_steps_flatten_to_the_router_expansion_for_nested_aliases() {
    // Arrange: `top` mixes a model, a two-deep alias, an unknown entry and
    // an alias that shadows a model nickname.
    let config = config_with(
        &["m-a", "m-b", "m-c", "m-d", "both"],
        &[
            ("top", chain(&["m-a", "mid", "ghost", "both"])),
            ("mid", chain(&["inner", "m-b"])),
            ("inner", chain(&["m-c", "ghost"])),
            ("both", single("m-d")),
        ],
    );
    let router = router_for(&config);

    // Act
    let steps = alias_steps(&config, &config.aliases["top"]).expect("within bound");
    let expanded: Vec<String> = router
        .resolve_v6_alias("top")
        .expect("within bound")
        .expect("top resolves")
        .iter()
        .map(|m| m.nickname.clone())
        .collect();

    // Assert
    let flattened: Vec<String> = steps.iter().flat_map(|s| s.models.clone()).collect();
    assert_eq!(flattened, expanded);
    assert_eq!(
        steps,
        vec![
            AliasStep {
                name: "m-a".into(),
                kind: AliasStepKind::Model,
                models: vec!["m-a".into()],
            },
            AliasStep {
                name: "mid".into(),
                kind: AliasStepKind::Alias,
                models: vec!["m-c".into(), "m-b".into()],
            },
            AliasStep {
                name: "both".into(),
                kind: AliasStepKind::Alias,
                models: vec!["m-d".into()],
            },
        ]
    );
}

/// `a0 -> a1 -> ... -> a(hops-1) -> model`: `hops` alias keys deep.
fn linear_chain_config(hops: usize) -> Config {
    let mut aliases = Vec::new();
    for i in 0..hops {
        let next = if i + 1 == hops {
            "model".to_string()
        } else {
            format!("a{}", i + 1)
        };
        aliases.push((format!("a{i}"), single(&next)));
    }
    let borrowed: Vec<(&str, AliasValue)> = aliases
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    config_with(&["model"], &borrowed)
}

#[test]
fn alias_steps_errors_for_a_chain_deeper_than_the_recursion_cap() {
    // Arrange: a chain of alias keys that never reaches a model nickname
    // within ALIAS_MAX_RECURSION_DEPTH hops.
    let mut config = Config::default();
    for i in 0..ALIAS_MAX_RECURSION_DEPTH + 4 {
        config
            .aliases
            .insert(format!("a{i}"), single(&format!("a{}", i + 1)));
    }

    // Act
    let result = alias_steps(&config, &config.aliases["a0"]);

    // Assert
    let err = result.expect_err("the depth cap stops the walk");
    assert!(matches!(err, Error::Config(_)), "{err}");
}

#[test]
fn alias_steps_depth_bound_matches_the_router_at_every_length() {
    for hops in 1..=ALIAS_MAX_RECURSION_DEPTH + 3 {
        // Arrange
        let config = linear_chain_config(hops);
        let router = router_for(&config);

        // Act
        let steps = alias_steps(&config, &config.aliases["a0"]);
        let dispatch = router.resolve_v6_alias("a0");

        // Assert
        assert_eq!(steps.is_ok(), dispatch.is_ok(), "hops = {hops}");
        if let Ok(steps) = steps {
            assert_eq!(steps[0].models, ["model"], "hops = {hops}");
        }
    }
}
