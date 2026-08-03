//! Pure, provenance-annotated derivation of the effective config view that
//! backs `config show --effective`.
//!
//! Three surfaces have more than one layer competing for a value, so only
//! those carry provenance: a model's catalog cell (the compiled-in baked
//! table vs an operator overlay), a retry class's policy (the baked class
//! default vs a `[retry.classes.<class>]` leaf), and a capability cell (the
//! config-derived override layer, tagged with the source that set it). Every
//! other config field is trivially the value in `config.toml`; annotating it
//! would be noise.
//!
//! The derivation is a PURE function over `(&Config, &CatalogOverlay)`. It
//! runs the SAME `(provider_kind, upstream)` catalog lookups the router's
//! chain-build merge runs, so an operator inspecting the view sees exactly the
//! cells dispatch prices against -- with no secret resolution, no provider
//! construction, and no network.
//!
//! The capability layer carries only the config-derived override cells (the
//! same read-model the routing filter consults). Learned-capability negatives
//! are in-memory runtime state on a live router, not config, so they are out
//! of scope for this pure view.

use routectl_core::failure_class::FailureClass;

use crate::catalog::{EffectiveRow, lookup_baked_with_overrides, lookup_overlay_cell, merge};
use crate::catalog_overlay::CatalogOverlay;
use crate::class_policy::ConfigFailureClass;
use crate::config::{Config, ProviderEntry};
use crate::override_registry::{OverrideRegistry, OverrideRow};

/// The catalog cell a single `[models.X]` entry resolves to, tagged with the
/// `(provider_kind, upstream)` selector it was looked up under.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCell {
    /// The `[models.<nickname>]` key.
    pub nickname: String,
    /// The `provider` this model references.
    pub provider: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown -- the same fallback the chain-build merge uses).
    pub provider_kind: String,
    /// The upstream model id forwarded to the provider.
    pub upstream: String,
    /// The merged catalog row and its winning-layer provenance.
    pub row: EffectiveRow,
}

/// The resolved retry/fallback policy for one failure class, tagged with the
/// layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassPolicyCell {
    /// The failure class this policy governs.
    pub class: ConfigFailureClass,
    /// The resolved same-provider retry cap.
    pub retry_cap: u32,
    /// Whether this class falls back to the next provider in the chain.
    pub fallback: bool,
    /// Which layer won.
    pub source: ClassPolicySource,
}

/// Which layer supplied a [`ClassPolicyCell`]'s value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassPolicySource {
    /// A `[retry.classes.<class>]` leaf the operator set.
    Config,
    /// The compiled-in baked class default (no operator leaf for this class).
    BakedDefault,
}

/// One `[aliases]` entry flattened into its ordered fallback chain. A
/// `Single` alias yields a one-element chain; a `Chain` keeps the operator's
/// order verbatim, since that order IS the fallback sequence dispatch walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasChain {
    /// The `[aliases]` table key (may be a suffix-glob pattern).
    pub alias: String,
    /// The model nicknames this alias falls back through, in order.
    pub chain: Vec<String>,
}

/// The provenance-annotated effective view: the layered surfaces only.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveView {
    /// One entry per `[models.X]` table, in nickname order.
    pub models: Vec<ModelCell>,
    /// One entry per operator-nameable failure class, in a stable order.
    pub classes: Vec<ClassPolicyCell>,
    /// One entry per config-derived capability-override cell, in a stable
    /// `(target_spec, capability_key)` order. Empty when the config carries
    /// no capability overrides. Learned-capability negatives are runtime
    /// state on a live router, not config, so they are not part of this
    /// pure view.
    pub capabilities: Vec<OverrideRow>,
    /// One entry per `[aliases]` table entry, in alias-key order.
    pub aliases: Vec<AliasChain>,
    /// The `[providers.X]` table keys, in provider-key order.
    pub provider_ids: Vec<String>,
}

/// Every operator-nameable failure class, in a stable render order.
const ALL_CONFIG_CLASSES: [ConfigFailureClass; 10] = [
    ConfigFailureClass::RateLimited,
    ConfigFailureClass::Auth,
    ConfigFailureClass::BadRequest,
    ConfigFailureClass::ContentPolicy,
    ConfigFailureClass::ContextWindow,
    ConfigFailureClass::ServerError,
    ConfigFailureClass::Timeout,
    ConfigFailureClass::NetworkError,
    ConfigFailureClass::Overloaded,
    ConfigFailureClass::FeatureUnsupported,
];

/// Derive the provenance-annotated effective view from a parsed config and its
/// catalog overlay. Pure: the model rows come from the same lookups the router
/// stamps onto `ResolvedModel::effective_row` at chain-build; the class rows
/// come from [`crate::config::RetryPolicy::resolved_class`].
#[must_use]
pub fn derive_effective_view(config: &Config, overlay: &CatalogOverlay) -> EffectiveView {
    let models = config
        .models
        .iter()
        .map(|(nickname, entry)| {
            let provider_kind = config
                .providers
                .get(&entry.provider)
                .map_or("", ProviderEntry::kind_str);
            let baked = lookup_baked_with_overrides(
                provider_kind,
                &entry.upstream,
                None,
                &config.cache_pricing,
            );
            let overlay_cell = lookup_overlay_cell(provider_kind, &entry.upstream, overlay);
            ModelCell {
                nickname: nickname.clone(),
                provider: entry.provider.clone(),
                provider_kind: provider_kind.to_string(),
                upstream: entry.upstream.clone(),
                row: merge(baked.as_ref(), overlay_cell),
            }
        })
        .collect();

    let classes = ALL_CONFIG_CLASSES
        .iter()
        .map(|&class| {
            let canonical: FailureClass = class.to_failure_class();
            let (retry_cap, fallback) = config.retry.resolved_class(&canonical);
            let source = if config.retry.classes.contains_key(&class) {
                ClassPolicySource::Config
            } else {
                ClassPolicySource::BakedDefault
            };
            ClassPolicyCell {
                class,
                retry_cap,
                fallback,
                source,
            }
        })
        .collect();

    let mut capabilities = OverrideRegistry::build(config).snapshot();
    capabilities.sort_by(|a, b| {
        a.target_spec
            .cmp(&b.target_spec)
            .then_with(|| a.capability_key.cmp(&b.capability_key))
    });

    let aliases = config
        .aliases
        .iter()
        .map(|(alias, value)| AliasChain {
            alias: alias.clone(),
            chain: value.nicknames().map(str::to_string).collect(),
        })
        .collect();

    let provider_ids = config.providers.keys().cloned().collect();

    EffectiveView {
        models,
        classes,
        capabilities,
        aliases,
        provider_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

    use crate::catalog::Source;
    use crate::catalog_overlay::CatalogOverlay;
    use crate::class_policy::ClassPolicy;
    use crate::config::Config;
    use crate::factory::apply_catalog_overlay;
    use crate::override_registry::{OverrideProvenance, OverrideVerdict};
    use crate::resolved::ResolvedModel;

    struct StubProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    fn config_with_opus_model() -> Config {
        toml::from_str(
            r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
"#,
        )
        .expect("valid config")
    }

    #[test]
    fn model_row_matches_apply_catalog_overlay_stamp() {
        // Arrange: same (provider_kind, upstream) fed to both paths.
        let config = config_with_opus_model();
        let overlay = CatalogOverlay::default();

        let provider: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let mut resolved = BTreeMap::new();
        resolved.insert(
            "opus".to_string(),
            Arc::new(ResolvedModel::new(
                "opus",
                "anthropic",
                provider,
                "claude-opus-4-8",
            )),
        );

        // Act
        let stamped = apply_catalog_overlay(resolved, &config, &overlay);
        let view = derive_effective_view(&config, &overlay);

        // Assert: the derivation reproduces the stamped merge exactly.
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.row, stamped["opus"].effective_row);
        assert!(matches!(
            derived.row,
            EffectiveRow::Present {
                source: Source::Baked,
                ..
            }
        ));
    }

    #[test]
    fn overlay_user_cell_tags_user_source() {
        // Arrange: a user overlay cell for the opus selector.
        let config = config_with_opus_model();
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": {
                "anthropic-api:claude-opus-4-8": {
                    "source": "user",
                    "verified_at": "2026-07-01",
                    "rm": 0.05
                }
            }
        }))
        .expect("valid overlay");

        // Act
        let view = derive_effective_view(&config, &overlay);

        // Assert
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        match &derived.row {
            EffectiveRow::Present { source, row, .. } => {
                assert_eq!(*source, Source::User);
                assert_eq!(row.rm, 0.05);
            }
            other => panic!("expected Present/User, got {other:?}"),
        }
    }

    #[test]
    fn disable_overlay_cell_yields_disabled_row() {
        let config = config_with_opus_model();
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": { "anthropic-api:claude-opus-4-8": null }
        }))
        .expect("valid overlay");

        let view = derive_effective_view(&config, &overlay);
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.row, EffectiveRow::Disabled);
    }

    #[test]
    fn unpriced_selector_yields_missing_row() {
        // A model whose provider is unknown resolves to an empty
        // provider_kind, which matches no baked cell -- the same fallback the
        // chain-build merge uses -- so neither layer has a row.
        let mut config = config_with_opus_model();
        config.models.get_mut("opus").unwrap().provider = "ghost".to_string();

        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.provider_kind, "");
        assert_eq!(derived.row, EffectiveRow::Missing);
    }

    #[test]
    fn class_with_operator_leaf_tags_config_others_baked_default() {
        // Arrange: an operator leaf on rate-limited only.
        let mut config = Config::default();
        config.retry.classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(7),
                fallback: None,
            },
        );

        // Act
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        // Assert: rate-limited tagged config with the overridden cap; a class
        // with no leaf tags baked-default.
        let rate_limited = view
            .classes
            .iter()
            .find(|c| c.class == ConfigFailureClass::RateLimited)
            .unwrap();
        assert_eq!(rate_limited.source, ClassPolicySource::Config);
        assert_eq!(rate_limited.retry_cap, 7);

        let auth = view
            .classes
            .iter()
            .find(|c| c.class == ConfigFailureClass::Auth)
            .unwrap();
        assert_eq!(auth.source, ClassPolicySource::BakedDefault);

        // Every class is represented exactly once.
        assert_eq!(view.classes.len(), ALL_CONFIG_CLASSES.len());
    }

    #[test]
    fn seeded_override_appears_as_capability_cell_with_override_source() {
        // Arrange: an operator capability override force-marks web_search
        // unsupported for the opus model's provider.
        let mut config = config_with_opus_model();
        config.capability.overrides.insert(
            "anthropic".to_string(),
            crate::config::OverrideEntry {
                unsupported: vec!["web_search".to_string()],
                force_supported: Vec::new(),
            },
        );

        // Act
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        // Assert: the capability layer carries the cell, tagged with the
        // Override provenance (the "override" token of the routing filter's
        // source contract) and the route-away verdict.
        let cell = view
            .capabilities
            .iter()
            .find(|c| c.target_spec == "anthropic" && c.capability_key == "web_search")
            .expect("seeded override must surface as a capability cell");
        assert_eq!(cell.verdict, OverrideVerdict::RouteAway);
        assert_eq!(cell.provenance, OverrideProvenance::Override);
    }

    #[test]
    fn capability_layer_empty_when_no_overrides() {
        // A config with no capability overrides yields an empty capability
        // layer -- rendered empty, never an error.
        let config = config_with_opus_model();
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        assert!(view.capabilities.is_empty());
    }

    #[test]
    fn derivation_source_is_pure_no_router_build() {
        // Structural guard: the production portion of this module never calls
        // into the secret-resolving / provider-constructing build path. The
        // pure `(&Config, &CatalogOverlay)` signature is the real invariant;
        // this scan tripwires an accidental reintroduction.
        let src = include_str!("config_effective.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("build_resolved_models"));
        assert!(!production.contains("build_provider"));
    }
}
