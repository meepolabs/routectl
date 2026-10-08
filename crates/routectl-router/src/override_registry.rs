//! Operator capability-override registry: the one keyed read-model that
//! flattens `[capability.overrides]` into a single `(target_spec,
//! normalized_capability_key)` map of override verdicts.
//!
//! Built purely from [`Config`] at Router construction (and therefore
//! rebuilt on every reload, since a reload constructs a fresh Router).
//! Two sources feed the map:
//!
//! - `[capability.overrides.<spec>].unsupported` ->
//!   [`OverrideVerdict::RouteAway`];
//! - `[capability.overrides.<spec>].force_supported` ->
//!   [`OverrideVerdict::ForceSupported`].
//!
//! The spec is either a provider name or `provider:nickname`. Every key is
//! normalized at build via [`normalize_capability_key`]
//! with the target's provider kind, so a stored override and a later
//! normalized lookup meet on identical strings.
//!
//! # Precedence
//!
//! Model-scoped (`provider:nickname`) and provider-scoped (`provider`)
//! specs are distinct cells and never collide in the map. Precedence is a
//! query-time concern: [`OverrideRegistry::resolve`] consults the
//! model-scoped cell first and falls back to the provider-scoped cell, so
//! a model override wins over a provider override for the same
//! capability.
//!
//! # Conflicts
//!
//! Within a single cell, a `RouteAway` contribution and a
//! `ForceSupported` contribution are contradictory (an operator both
//! forced a capability off and forced it on for the same target). That is
//! a config error surfaced by [`validate_capability_overrides`], wired
//! into the shared validator so `serve` load, `config check`, and the
//! `config migrate` gate all reject it. Semantically identical duplicates
//! (two `RouteAway` contributions, e.g. two `unsupported` entries that
//! normalize to the same capability) are not a conflict.

use std::collections::HashMap;

use routectl_core::capability::normalize_capability_key;

use crate::config::Config;

/// Verdict a resolved override cell carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideVerdict {
    /// Route away from the target for this capability -- a hard negative,
    /// the semantics of an `overrides.*.unsupported` entry.
    RouteAway,
    /// Force the capability supported for the target, overriding a
    /// learned or catalog negative back to available.
    ForceSupported,
}

/// Internal cell key: a two-tier target spec (`provider` or
/// `provider:nickname`) paired with the normalized capability key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CellKey {
    target_spec: String,
    capability_key: String,
}

/// Snapshot row -- the fixed contract shape downstream consumers read,
/// mirroring `LearnedCapabilityRegistry::snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideRow {
    /// The target this override applies to.
    pub target_spec: String,
    /// The capability key being overridden.
    pub capability_key: String,
    /// The override verdict.
    pub verdict: OverrideVerdict,
}

/// One raw contribution before cells are folded. Carries the source
/// label so a conflict can name both sides.
struct Contribution {
    target_spec: String,
    capability_key: String,
    verdict: OverrideVerdict,
    source: String,
}

/// Immutable, config-derived override read-model held on the Router.
#[derive(Debug, Default)]
pub struct OverrideRegistry {
    cells: HashMap<CellKey, OverrideVerdict>,
}

impl OverrideRegistry {
    /// Flatten every override source in `config` into the keyed model,
    /// normalizing each capability key with its target's provider kind.
    ///
    /// Infallible: a contradictory cell (which
    /// [`validate_capability_overrides`] rejects at config-load time)
    /// resolves conservatively to `RouteAway` here so a direct
    /// construction that bypassed validation still yields a usable,
    /// safe-by-default registry. Emits the dead-key WARN only for an
    /// operator override key whose NORMALIZED form is not itself
    /// normalization-stable -- a genuinely unreachable cell, since every
    /// capability lookup is normalized before it is matched. A key that
    /// normalization merely reduces to a stable form (e.g. a Bedrock
    /// request-bag path collapsing to its capability leaf) stays reachable
    /// and is not flagged.
    pub fn build(config: &Config) -> Self {
        warn_dead_override_keys(config);

        let mut cells: HashMap<CellKey, OverrideVerdict> = HashMap::new();
        for contribution in collect_contributions(config) {
            let key = CellKey {
                target_spec: contribution.target_spec,
                capability_key: contribution.capability_key,
            };
            let incoming = contribution.verdict;
            match cells.get_mut(&key) {
                None => {
                    cells.insert(key, incoming);
                }
                Some(existing) => *existing = merge_verdict(*existing, incoming),
            }
        }
        Self { cells }
    }

    /// Resolve the override verdict for a concrete target, honoring
    /// model-over-provider precedence: the `provider:nickname` cell is
    /// consulted first, then the bare `provider` cell. Returns `None`
    /// when neither scope carries an override for this capability.
    pub fn resolve(
        &self,
        provider_name: &str,
        nickname: &str,
        capability_raw: &str,
        provider_kind: &str,
    ) -> Option<OverrideVerdict> {
        let capability_key = normalize_capability_key(capability_raw, provider_kind);
        let model_spec = format!("{provider_name}:{nickname}");
        let model_key = CellKey {
            target_spec: model_spec,
            capability_key: capability_key.clone(),
        };
        if let Some(verdict) = self.cells.get(&model_key) {
            return Some(*verdict);
        }
        let provider_key = CellKey {
            target_spec: provider_name.to_string(),
            capability_key,
        };
        self.cells.get(&provider_key).copied()
    }

    /// Snapshot every resident cell in the fixed contract shape.
    pub fn snapshot(&self) -> Vec<OverrideRow> {
        self.cells
            .iter()
            .map(|(key, verdict)| OverrideRow {
                target_spec: key.target_spec.clone(),
                capability_key: key.capability_key.clone(),
                verdict: *verdict,
            })
            .collect()
    }

    /// Number of resident cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether the registry holds no cells.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Fold a second contribution into an existing cell. Two identical
/// verdicts collapse. Contradictory verdicts resolve to `RouteAway` (the
/// conservative, route-away-by-default choice); this path is unreachable
/// for a config that passed [`validate_capability_overrides`].
fn merge_verdict(existing: OverrideVerdict, incoming: OverrideVerdict) -> OverrideVerdict {
    if existing == incoming {
        existing
    } else {
        OverrideVerdict::RouteAway
    }
}

/// Collect every override contribution from `[capability.overrides]` with
/// keys normalized to each target's provider kind.
fn collect_contributions(config: &Config) -> Vec<Contribution> {
    let mut out = Vec::new();

    for (spec, override_entry) in &config.capability.overrides {
        let kind = provider_kind_for(config, provider_of_spec(spec));
        for raw in &override_entry.unsupported {
            out.push(Contribution {
                target_spec: spec.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::RouteAway,
                source: format!("[capability.overrides.{spec}].unsupported"),
            });
        }
        for raw in &override_entry.force_supported {
            out.push(Contribution {
                target_spec: spec.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::ForceSupported,
                source: format!("[capability.overrides.{spec}].force_supported"),
            });
        }
    }

    out
}

/// Split a two-tier target spec (`"provider_name"` or
/// `"provider_name:nickname"`) into its provider segment and, when present,
/// its nickname segment.
///
/// The split is on the FIRST `:` only: a model nickname may itself contain
/// `:` (nothing in the `[models]` table or in [`OverrideRegistry::resolve`]
/// bans it), so everything after the first colon is the nickname, not a
/// second delimiter. This is the one place that decision is made; both
/// override extraction ([`provider_of_spec`]) and the `[fidelity]`
/// `prefix_impact_opt_in` validator call through here so the two stay in
/// lockstep with how `resolve` itself reads a spec.
pub fn split_target_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once(':') {
        Some((provider, nickname)) => (provider, Some(nickname)),
        None => (spec, None),
    }
}

/// The provider name a two-tier override spec targets: the segment
/// before the first `:` for a model-scoped spec, or the whole string for
/// a provider-scoped spec.
fn provider_of_spec(spec: &str) -> &str {
    split_target_spec(spec).0
}

/// Provider kind (`kind_str`) for `provider_name`, or `""` when the
/// provider is not configured. An unconfigured provider kind normalizes
/// as a pass-through (only the exact `bedrock` kind rewrites keys), and
/// a model / override referencing an unknown provider is rejected
/// elsewhere by the provider-reference validators.
fn provider_kind_for<'a>(config: &'a Config, provider_name: &str) -> &'a str {
    config
        .providers
        .get(provider_name)
        .map_or("", |entry| entry.kind_str())
}

/// Emit one structured WARN per operator override key that normalization
/// rewrites into an unreachable cell: a key whose normalized form is not
/// itself a normalization fixed point. Every capability lookup value passes
/// through normalization before it is matched against a stored cell, so the
/// reachable keys are exactly normalization's fixed points; a stored key that
/// normalization would rewrite again lies outside that image and can never
/// match. A key that normalization only reduces to a stable form (e.g. a
/// Bedrock request-bag path collapsing to its capability leaf) stays
/// reachable and is NOT flagged. Only the capability token reaches the log
/// line, never a request body.
fn warn_dead_override_keys(config: &Config) {
    for (spec, override_entry) in &config.capability.overrides {
        let kind = provider_kind_for(config, provider_of_spec(spec));
        for raw in override_entry
            .unsupported
            .iter()
            .chain(&override_entry.force_supported)
        {
            let stored_key = normalize_capability_key(raw, kind);
            if override_key_reachable(&stored_key, kind) {
                continue;
            }
            let lookup_key = normalize_capability_key(&stored_key, kind);
            tracing::warn!(
                event = "dead_override_key",
                target_spec = %spec,
                raw_key = %raw,
                stored_key = %stored_key,
                lookup_key = %lookup_key,
                "capability override key normalizes to a form that is not \
                 normalization-stable; the cell is stored under a key no \
                 normalized capability lookup can produce, so the override is \
                 unreachable",
            );
        }
    }
}

/// Whether a stored (already-normalized) override key can ever be matched by
/// a capability lookup. Every lookup value passes through
/// [`normalize_capability_key`] before comparison, so the reachable keys are
/// exactly normalization's fixed points: a key that normalization would
/// rewrite again can never equal any lookup key.
fn override_key_reachable(stored_key: &str, provider_kind: &str) -> bool {
    normalize_capability_key(stored_key, provider_kind) == stored_key
}

/// Reject a config whose override sources place contradictory verdicts on
/// one `(target, capability)` cell -- an `unsupported` entry marking a
/// capability route-away while a `force_supported` entry marks the SAME
/// capability supported for the SAME target. Semantically identical duplicates (both
/// route-away) pass. Wired into [`collect_config_validation`] so every
/// config surface rejects the conflict uniformly.
///
/// Returns the first conflict in deterministic (target, capability)
/// order, naming the cell and both sources.
///
/// [`collect_config_validation`]: crate::factory::collect_config_validation
pub fn validate_capability_overrides(config: &Config) -> Result<(), String> {
    let mut route_away: HashMap<CellKey, String> = HashMap::new();
    let mut force_supported: HashMap<CellKey, String> = HashMap::new();

    for contribution in collect_contributions(config) {
        let key = CellKey {
            target_spec: contribution.target_spec,
            capability_key: contribution.capability_key,
        };
        let bucket = match contribution.verdict {
            OverrideVerdict::RouteAway => &mut route_away,
            OverrideVerdict::ForceSupported => &mut force_supported,
        };
        bucket.entry(key).or_insert(contribution.source);
    }

    let mut conflicts: Vec<(CellKey, String, String)> = route_away
        .iter()
        .filter_map(|(key, away_source)| {
            force_supported
                .get(key)
                .map(|forced_source| (key.clone(), away_source.clone(), forced_source.clone()))
        })
        .collect();
    conflicts.sort_by(|a, b| {
        a.0.target_spec
            .cmp(&b.0.target_spec)
            .then_with(|| a.0.capability_key.cmp(&b.0.capability_key))
    });

    if let Some((key, away_source, forced_source)) = conflicts.into_iter().next() {
        return Err(format!(
            "[capability.overrides.{}] contradictory override for capability `{}`: \
             {away_source} marks it route-away while {forced_source} marks it \
             force-supported -- one target/capability cannot be both",
            key.target_spec, key.capability_key,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a full [`Config`] from a TOML fragment (with the required
    /// version stamp prepended), matching the repo's config-test style so
    /// providers, models, and overrides all deserialize through the real
    /// serde path.
    fn config(toml_body: &str) -> Config {
        toml::from_str(&format!("version = 5\n{toml_body}")).expect("config parses")
    }

    const OPENAI_P: &str = "[providers.p]\n\
        kind = \"openai-compat\"\n\
        base_url = \"https://x\"\n\
        api_key_ref = \"literal:k\"\n";

    #[test]
    fn provider_scoped_unsupported_snapshots_as_route_away_override() {
        // Arrange
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        let rows = registry.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target_spec, "p");
        assert_eq!(rows[0].capability_key, "web_search");
        assert_eq!(rows[0].verdict, OverrideVerdict::RouteAway);
    }

    #[test]
    fn model_scoped_unsupported_snapshots_under_provider_nickname_spec() {
        // Arrange
        let config = config(&format!(
            "{OPENAI_P}\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             [capability.overrides.\"p:nick\"]\n\
             unsupported = [\"computer_use\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        let rows = registry.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target_spec, "p:nick");
        assert_eq!(rows[0].capability_key, "computer_use");
        assert_eq!(rows[0].verdict, OverrideVerdict::RouteAway);
    }

    #[test]
    fn unsupported_and_force_supported_resolve_to_their_verdicts() {
        // Arrange
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n\
             force_supported = [\"structured_output\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        assert_eq!(
            registry.resolve("p", "any", "web_search", "openai-compat"),
            Some(OverrideVerdict::RouteAway)
        );
        assert_eq!(
            registry.resolve("p", "any", "structured_output", "openai-compat"),
            Some(OverrideVerdict::ForceSupported)
        );
    }

    #[test]
    fn model_scoped_override_wins_over_provider_scoped() {
        // Arrange -- provider says route-away, model says force-supported
        // for the same capability. Distinct cells; the model-scoped cell
        // wins at resolve time.
        let config = config(&format!(
            "{OPENAI_P}\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n\
             [capability.overrides.\"p:nick\"]\n\
             force_supported = [\"web_search\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert -- model-scoped force-supported wins for the concrete
        // target; a different model on the same provider still sees the
        // provider-scoped route-away.
        assert_eq!(
            registry.resolve("p", "nick", "web_search", "openai-compat"),
            Some(OverrideVerdict::ForceSupported)
        );
        assert_eq!(
            registry.resolve("p", "other", "web_search", "openai-compat"),
            Some(OverrideVerdict::RouteAway)
        );
    }

    #[test]
    fn duplicate_route_away_collapses_to_one_cell_and_does_not_conflict() {
        // Arrange -- one unsupported list names the same capability twice.
        // Same verdict: the entries collapse into one cell and validation
        // passes.
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\", \"web_search\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        assert!(validate_capability_overrides(&config).is_ok());
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.resolve("p", "any", "web_search", "openai-compat"),
            Some(OverrideVerdict::RouteAway)
        );
    }

    #[test]
    fn contradictory_unsupported_and_force_supported_names_both_sources() {
        // Arrange -- unsupported routes away while force_supported marks
        // the same capability supported for the same target.
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n\
             force_supported = [\"web_search\"]\n"
        ));

        // Act
        let err = validate_capability_overrides(&config)
            .expect_err("contradictory cell must fail validation");

        // Assert -- the message names the cell and both sources.
        assert!(err.contains("web_search"), "got: {err}");
        assert!(
            err.contains("[capability.overrides.p].unsupported "),
            "got: {err}"
        );
        assert!(
            err.contains("[capability.overrides.p].force_supported"),
            "got: {err}"
        );
    }

    #[test]
    fn contradictory_within_one_override_entry_fails_validation() {
        // Arrange -- the same override entry lists a capability under both
        // unsupported and force_supported.
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"computer_use\"]\n\
             force_supported = [\"computer_use\"]\n"
        ));

        // Act / Assert
        let err = validate_capability_overrides(&config)
            .expect_err("self-contradictory entry must fail validation");
        assert!(err.contains("computer_use"), "got: {err}");
    }

    /// A dotted Bedrock request-bag override key normalizes to its
    /// capability leaf (`additionalModelRequestFields.anthropic_beta` ->
    /// `anthropic_beta`), a normalization-stable key a normalized lookup
    /// reproduces. The override is reachable and must NOT trip the dead-key
    /// guard -- the guard previously mislabeled this working key as
    /// unreachable purely because normalization rewrote its raw form.
    /// Bedrock is the only kind that rewrites keys, so this test needs the
    /// feature.
    #[cfg(feature = "bedrock")]
    #[test]
    fn reachable_bedrock_bag_prefixed_override_emits_no_dead_key_warn() {
        // Arrange
        let config = config(
            "[providers.br]\n\
             kind = \"bedrock\"\n\
             region = \"us-east-1\"\n\
             creds = { kind = \"default-chain\" }\n\
             [capability.overrides.br]\n\
             unsupported = [\"additionalModelRequestFields.anthropic_beta\"]\n",
        );

        // Act
        let events = routectl_testkit::capture_events(|| {
            let _ = OverrideRegistry::build(&config);
        });

        // Assert -- no dead-key WARN, and the normalized key resolves to a
        // live cell (reachable via a normalized capability lookup).
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a bag-prefixed key that normalizes to a reachable leaf must not \
             trip the dead-key guard"
        );
        let registry = OverrideRegistry::build(&config);
        assert_eq!(
            registry.resolve("br", "any", "anthropic_beta", "bedrock"),
            Some(OverrideVerdict::RouteAway)
        );
    }

    /// The dead-key guard's reachability core. A key already in normalized
    /// (fixed-point) form is reachable, while a key normalization would
    /// rewrite again is not: stored under that form it could never equal a
    /// normalized capability lookup, so the guard flags it as the genuinely
    /// unreachable cell -- distinct from a merely-rewritten-but-stable key.
    #[test]
    fn override_key_reachability_distinguishes_stable_from_rewritten_keys() {
        // Normalization-stable keys are reachable.
        assert!(override_key_reachable("anthropic_beta", "bedrock"));
        assert!(override_key_reachable("web_search", "openai-compat"));
        // A dotted Bedrock request-bag key is not a normalization fixed
        // point: no normalized lookup key can ever equal it.
        assert!(!override_key_reachable(
            "additionalModelRequestFields.anthropic_beta",
            "bedrock"
        ));
    }

    #[test]
    fn idempotent_override_key_emits_no_dead_key_warn() {
        // Arrange -- an openai-compat key that normalization leaves alone.
        let config = config(&format!(
            "{OPENAI_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
        ));

        // Act
        let events = routectl_testkit::capture_events(|| {
            let _ = OverrideRegistry::build(&config);
        });

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a normalization-stable key must not trip the dead-key guard"
        );
    }

    #[test]
    fn empty_config_yields_empty_registry() {
        // Arrange / Act
        let registry = OverrideRegistry::build(&Config::default());

        // Assert
        assert!(registry.is_empty());
        assert_eq!(
            registry.resolve("p", "n", "web_search", "openai-compat"),
            None
        );
    }
}
