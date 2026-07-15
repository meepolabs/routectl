//! Operator capability-override registry: the one keyed read-model that
//! flattens config overrides and the legacy provider / model
//! `unsupported_features` lists into a single `(target_spec,
//! normalized_capability_key)` map carrying PROVENANCE.
//!
//! Built purely from [`Config`] at Router construction (and therefore
//! rebuilt on every reload, since a reload constructs a fresh Router).
//! Four sources feed the map:
//!
//! - legacy `[providers.X].unsupported_features` -> [`OverrideVerdict::RouteAway`]
//!   / [`OverrideProvenance::ProviderStatic`], keyed by the provider name;
//! - legacy `[models.X].unsupported_features` -> `RouteAway` /
//!   [`OverrideProvenance::ModelStatic`], keyed by `provider:nickname`;
//! - new `[capability.overrides.<spec>].unsupported` -> `RouteAway` /
//!   [`OverrideProvenance::Override`];
//! - new `[capability.overrides.<spec>].force_supported` ->
//!   [`OverrideVerdict::ForceSupported`] / `Override`.
//!
//! Legacy entries keep their static provenance so an existing config's
//! routing behavior AND its source labels stay byte-identical once the
//! consult reads this model instead of the two raw lists. Every key is
//! normalized at build via
//! [`normalize_capability_key`](routectl_core::capability::normalize_capability_key)
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
//! (two `RouteAway` contributions, e.g. a legacy list and a new
//! `unsupported` entry naming the same capability) are not a conflict.

use std::collections::HashMap;

use routectl_core::capability::normalize_capability_key;

use crate::config::Config;

/// Verdict a resolved override cell carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideVerdict {
    /// Route away from the target for this capability -- a hard negative,
    /// the semantics of both the legacy `unsupported_features` lists and
    /// a new `overrides.*.unsupported` entry.
    RouteAway,
    /// Force the capability supported for the target, overriding a
    /// learned or catalog negative back to available.
    ForceSupported,
}

/// Where a cell's verdict came from. Legacy static provenance is
/// preserved so the routing consult emits the same source labels it
/// always has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideProvenance {
    /// Legacy `[providers.X].unsupported_features`.
    ProviderStatic,
    /// Legacy `[models.X].unsupported_features`.
    ModelStatic,
    /// New `[capability.overrides.<spec>]`.
    Override,
}

/// Internal cell key: a two-tier target spec (`provider` or
/// `provider:nickname`) paired with the normalized capability key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CellKey {
    target_spec: String,
    capability_key: String,
}

/// Resolved cell: the single verdict + provenance that survived
/// flattening for one `(target_spec, capability_key)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    verdict: OverrideVerdict,
    provenance: OverrideProvenance,
}

/// Snapshot row -- the fixed contract shape downstream consumers read,
/// mirroring `LearnedCapabilityRegistry::snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideRow {
    pub target_spec: String,
    pub capability_key: String,
    pub verdict: OverrideVerdict,
    pub provenance: OverrideProvenance,
}

/// One raw contribution before cells are folded. Carries the source
/// label so a conflict can name both sides.
struct Contribution {
    target_spec: String,
    capability_key: String,
    verdict: OverrideVerdict,
    provenance: OverrideProvenance,
    source: String,
}

/// Immutable, config-derived override read-model held on the Router.
#[derive(Debug, Default)]
pub struct OverrideRegistry {
    cells: HashMap<CellKey, Cell>,
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

        let mut cells: HashMap<CellKey, Cell> = HashMap::new();
        for contribution in collect_contributions(config) {
            let key = CellKey {
                target_spec: contribution.target_spec,
                capability_key: contribution.capability_key,
            };
            let incoming = Cell {
                verdict: contribution.verdict,
                provenance: contribution.provenance,
            };
            match cells.get_mut(&key) {
                None => {
                    cells.insert(key, incoming);
                }
                Some(existing) => *existing = merge_cell(*existing, incoming),
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
    ) -> Option<(OverrideVerdict, OverrideProvenance)> {
        let capability_key = normalize_capability_key(capability_raw, provider_kind);
        let model_spec = format!("{provider_name}:{nickname}");
        let model_key = CellKey {
            target_spec: model_spec,
            capability_key: capability_key.clone(),
        };
        if let Some(cell) = self.cells.get(&model_key) {
            return Some((cell.verdict, cell.provenance));
        }
        let provider_key = CellKey {
            target_spec: provider_name.to_string(),
            capability_key,
        };
        self.cells
            .get(&provider_key)
            .map(|cell| (cell.verdict, cell.provenance))
    }

    /// Snapshot every resident cell in the fixed contract shape.
    pub fn snapshot(&self) -> Vec<OverrideRow> {
        self.cells
            .iter()
            .map(|(key, cell)| OverrideRow {
                target_spec: key.target_spec.clone(),
                capability_key: key.capability_key.clone(),
                verdict: cell.verdict,
                provenance: cell.provenance,
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
/// verdicts collapse, preferring static provenance so legacy labels
/// survive a duplicate new entry. Contradictory verdicts resolve to
/// `RouteAway` (the conservative, route-away-by-default choice); this
/// path is unreachable for a config that passed
/// [`validate_capability_overrides`].
fn merge_cell(existing: Cell, incoming: Cell) -> Cell {
    if existing.verdict == incoming.verdict {
        let provenance = if existing.provenance == OverrideProvenance::Override {
            incoming.provenance
        } else {
            existing.provenance
        };
        return Cell {
            verdict: existing.verdict,
            provenance,
        };
    }
    let route_away = if existing.verdict == OverrideVerdict::RouteAway {
        existing
    } else {
        incoming
    };
    Cell {
        verdict: OverrideVerdict::RouteAway,
        provenance: route_away.provenance,
    }
}

/// Collect every override contribution from all four sources with keys
/// normalized to each target's provider kind.
fn collect_contributions(config: &Config) -> Vec<Contribution> {
    let mut out = Vec::new();

    for (provider_name, entry) in &config.providers {
        let kind = entry.kind_str();
        for raw in &entry.runtime().unsupported_features {
            out.push(Contribution {
                target_spec: provider_name.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::RouteAway,
                provenance: OverrideProvenance::ProviderStatic,
                source: format!("[providers.{provider_name}].unsupported_features"),
            });
        }
    }

    for (nickname, model) in &config.models {
        let kind = provider_kind_for(config, &model.provider);
        let target_spec = format!("{}:{}", model.provider, nickname);
        for raw in &model.unsupported_features {
            out.push(Contribution {
                target_spec: target_spec.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::RouteAway,
                provenance: OverrideProvenance::ModelStatic,
                source: format!("[models.{nickname}].unsupported_features"),
            });
        }
    }

    for (spec, override_entry) in &config.capability.overrides {
        let kind = provider_kind_for(config, provider_of_spec(spec));
        for raw in &override_entry.unsupported {
            out.push(Contribution {
                target_spec: spec.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::RouteAway,
                provenance: OverrideProvenance::Override,
                source: format!("[capability.overrides.{spec}].unsupported"),
            });
        }
        for raw in &override_entry.force_supported {
            out.push(Contribution {
                target_spec: spec.clone(),
                capability_key: normalize_capability_key(raw, kind),
                verdict: OverrideVerdict::ForceSupported,
                provenance: OverrideProvenance::Override,
                source: format!("[capability.overrides.{spec}].force_supported"),
            });
        }
    }

    out
}

/// The provider name a two-tier override spec targets: the segment
/// before the first `:` for a model-scoped spec, or the whole string for
/// a provider-scoped spec.
fn provider_of_spec(spec: &str) -> &str {
    spec.split_once(':').map_or(spec, |(provider, _)| provider)
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
/// one `(target, capability)` cell -- a legacy `unsupported_features`
/// entry (or a new `unsupported` entry) marking a capability route-away
/// while a `force_supported` entry marks the SAME capability supported
/// for the SAME target. Semantically identical duplicates (both
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
        toml::from_str(&format!("version = 3\n{toml_body}")).expect("config parses")
    }

    const OPENAI_P: &str = "[providers.p]\n\
        kind = \"openai-compat\"\n\
        base_url = \"https://x\"\n\
        api_key_ref = \"literal:k\"\n";

    #[test]
    fn provider_legacy_list_maps_to_route_away_provider_static() {
        // Arrange
        let config = config(&format!(
            "{OPENAI_P}unsupported_features = [\"web_search\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        let rows = registry.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target_spec, "p");
        assert_eq!(rows[0].capability_key, "web_search");
        assert_eq!(rows[0].verdict, OverrideVerdict::RouteAway);
        assert_eq!(rows[0].provenance, OverrideProvenance::ProviderStatic);
    }

    #[test]
    fn model_legacy_list_maps_to_route_away_model_static() {
        // Arrange
        let config = config(&format!(
            "{OPENAI_P}\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             unsupported_features = [\"computer_use\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        let row = registry
            .snapshot()
            .into_iter()
            .find(|r| r.provenance == OverrideProvenance::ModelStatic)
            .expect("model-static row present");
        assert_eq!(row.target_spec, "p:nick");
        assert_eq!(row.capability_key, "computer_use");
        assert_eq!(row.verdict, OverrideVerdict::RouteAway);
    }

    #[test]
    fn new_override_unsupported_and_force_supported_carry_override_provenance() {
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
            Some((OverrideVerdict::RouteAway, OverrideProvenance::Override))
        );
        assert_eq!(
            registry.resolve("p", "any", "structured_output", "openai-compat"),
            Some((
                OverrideVerdict::ForceSupported,
                OverrideProvenance::Override
            ))
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
            Some((
                OverrideVerdict::ForceSupported,
                OverrideProvenance::Override
            ))
        );
        assert_eq!(
            registry.resolve("p", "other", "web_search", "openai-compat"),
            Some((OverrideVerdict::RouteAway, OverrideProvenance::Override))
        );
    }

    #[test]
    fn duplicate_route_away_prefers_static_provenance_and_does_not_conflict() {
        // Arrange -- legacy provider list AND a new unsupported entry name
        // the same capability. Same verdict: they collapse, keeping the
        // static label, and validation passes.
        let config = config(&format!(
            "{OPENAI_P}unsupported_features = [\"web_search\"]\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
        ));

        // Act
        let registry = OverrideRegistry::build(&config);

        // Assert
        assert!(validate_capability_overrides(&config).is_ok());
        assert_eq!(
            registry.resolve("p", "any", "web_search", "openai-compat"),
            Some((
                OverrideVerdict::RouteAway,
                OverrideProvenance::ProviderStatic
            ))
        );
    }

    #[test]
    fn contradictory_legacy_and_force_supported_fails_validation() {
        // Arrange -- legacy provider list routes away, new force_supported
        // marks the same capability supported for the same target.
        let config = config(&format!(
            "{OPENAI_P}unsupported_features = [\"web_search\"]\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n"
        ));

        // Act
        let err = validate_capability_overrides(&config)
            .expect_err("contradictory cell must fail validation");

        // Assert -- the message names the cell and both sources.
        assert!(err.contains("web_search"), "got: {err}");
        assert!(
            err.contains("[providers.p].unsupported_features"),
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
            Some((OverrideVerdict::RouteAway, OverrideProvenance::Override))
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
