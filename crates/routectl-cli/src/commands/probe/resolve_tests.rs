use super::*;

use routectl_router::{Config, ModelEntry};

fn config_with_models(models: &[(&str, ModelEntry)]) -> Config {
    let mut config = Config::default();
    for (nickname, entry) in models {
        config.models.insert((*nickname).to_string(), entry.clone());
    }
    config
}

#[test]
fn alias_resolves_state_key_provider_and_model() {
    let config = config_with_models(&[("opus", ModelEntry::new("anthropic", "claude-opus"))]);

    let target = resolve_probe_target(&config, None, Some("opus")).expect("known alias");

    assert_eq!(
        target,
        ResolvedProbeTarget {
            state_key: "opus".to_string(),
            provider: "anthropic".to_string(),
            model_id: "claude-opus".to_string(),
        },
        "an alias is itself the routing state key"
    );
}

#[test]
fn unknown_alias_is_an_error() {
    let config = config_with_models(&[("opus", ModelEntry::new("anthropic", "claude-opus"))]);

    let err = resolve_probe_target(&config, None, Some("ghost")).expect_err("unknown alias errors");

    assert!(err.contains("ghost"), "err: {err}");
}

#[test]
fn bare_provider_resolves_its_single_selectable_model_and_uses_the_nickname() {
    let config = config_with_models(&[("sonnet", ModelEntry::new("anthropic", "claude-sonnet"))]);

    let target = resolve_probe_target(&config, Some("anthropic"), None).expect("single model");

    assert_eq!(
        target,
        ResolvedProbeTarget {
            state_key: "sonnet".to_string(),
            provider: "anthropic".to_string(),
            model_id: "claude-sonnet".to_string(),
        },
        "the resolved model's nickname is the state key, not the provider name"
    );
}

#[test]
fn bare_provider_with_no_selectable_model_is_an_error() {
    let mut only_disabled = ModelEntry::new("anthropic", "claude-sonnet");
    only_disabled.selectable = false;
    let config = config_with_models(&[("sonnet", only_disabled)]);

    let err = resolve_probe_target(&config, Some("anthropic"), None)
        .expect_err("no selectable model errors");

    assert!(err.contains("no selectable model"), "err: {err}");
}

#[test]
fn bare_provider_referenced_by_multiple_models_is_an_error() {
    let config = config_with_models(&[
        ("a", ModelEntry::new("anthropic", "claude-a")),
        ("b", ModelEntry::new("anthropic", "claude-b")),
    ]);

    let err = resolve_probe_target(&config, Some("anthropic"), None)
        .expect_err("ambiguous provider errors");

    assert!(err.contains("multiple models"), "err: {err}");
}

#[test]
fn neither_target_is_an_error() {
    let config = Config::default();

    let err = resolve_probe_target(&config, None, None).expect_err("no target errors");

    assert!(err.contains("--provider or --alias"), "err: {err}");
}

#[test]
fn resolve_provider_and_model_is_the_pair_view() {
    let config = config_with_models(&[("opus", ModelEntry::new("anthropic", "claude-opus"))]);

    let pair = resolve_provider_and_model(&config, None, Some("opus")).expect("known alias");

    assert_eq!(pair, ("anthropic".to_string(), "claude-opus".to_string()));
}
