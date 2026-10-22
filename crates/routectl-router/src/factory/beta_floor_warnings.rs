//! Advisory (never fatal) load-time check on the beta-override surface: an
//! `unsupported` `beta:<flag>` override on an anthropic-api provider naming a
//! flag the egress re-adds AFTER the router's withhold, which makes the
//! override inert wherever that re-add fires.
//!
//! The model here mirrors the anthropic-api egress header composition site by
//! site; the enumerated list of those sites, with their conditions, is the
//! module doc of `warnings_beta_floor_tests.rs`, and every site has a row
//! there. A change to the egress composition updates that list, this model
//! and the table together.
//!
//! The egress facts this model relies on are pinned on the provider side by
//! `client_beta_floor_site_tests.rs` in `routectl-providers`
//! (`anthropic_api`), which drives `build_headers` per site with the
//! condition true and false; its rows carry the same S1-S8 labels as the
//! table here.

use routectl_core::identity::anthropic::{
    EFFORT_BETA, OAUTH_ANTHROPIC_BETA, REDACT_THINKING_BETA, STRUCTURED_OUTPUTS_BETA,
    default_claude_code_anthropic_betas, is_anthropic_api_host,
};
use routectl_providers::anthropic_api::{AuthKind, CONTEXT_MANAGEMENT_BETA, CloakMode};

use crate::beta_capability::{beta_flag_of, beta_override_key};
use crate::config::{Config, CredentialSource, OverrideEntry, ProviderEntry};
use crate::override_registry::split_target_spec;

const OWN_OAUTH_LANE: &str =
    "on an oauth-bearer provider on api.anthropic.com using its own credential";

/// One warning per `unsupported` `beta:<flag>` entry whose flag some site
/// re-adds for a target of that override cell, naming every such site.
///
/// Flags compare in the override's canonical form (case-insensitive ASCII,
/// [`beta_override_key`]), matching how the override itself is looked up.
/// `force_supported` entries are not inspected -- sending the flag is what
/// the egress does anyway. Bedrock's `anthropic_beta_floor()` is not
/// consulted: it is empty for every non-Bedrock entry.
pub(super) fn beta_floor_override_warnings(config: &Config) -> Vec<String> {
    config
        .capability
        .overrides
        .iter()
        .flat_map(|(spec, entry)| cell_warnings(config, spec, entry))
        .collect()
}

fn cell_warnings(config: &Config, spec: &str, entry: &OverrideEntry) -> Vec<String> {
    let (provider_name, nickname) = split_target_spec(spec);
    let Some(provider @ ProviderEntry::AnthropicApi { .. }) = config.providers.get(provider_name)
    else {
        return Vec::new();
    };
    let Some(models) = cell_models(config, provider_name, nickname) else {
        return Vec::new();
    };
    let cell = Cell {
        config,
        spec,
        provider_name,
        provider,
        is_model_scoped: nickname.is_some(),
        models,
    };
    entry
        .unsupported
        .iter()
        .filter_map(|raw| cell.flag_warning(raw))
        .collect()
}

/// One override cell resolved against the config: its provider entry and the
/// models its targets carry.
struct Cell<'c> {
    config: &'c Config,
    spec: &'c str,
    provider_name: &'c str,
    provider: &'c ProviderEntry,
    is_model_scoped: bool,
    models: Vec<&'c str>,
}

impl Cell<'_> {
    fn flag_warning(&self, raw: &str) -> Option<String> {
        let key = beta_override_key(raw);
        let flag = beta_flag_of(&key)?;
        let reached = self.models_decided_here(&key)?;
        let reasons: Vec<String> = egress_reassertions(self.config, self.provider, &reached)
            .into_iter()
            .filter(|site| site.flag.to_ascii_lowercase() == flag)
            .map(|site| site.reason)
            .collect();
        if reasons.is_empty() {
            return None;
        }
        Some(format!(
            "[capability.overrides.\"{spec}\"] unsupported `{raw}` cannot withhold this flag on \
             anthropic-api provider `{provider_name}`: the egress re-adds it after the override \
             is applied ({}), so the override has no effect there.",
            reasons.join("; "),
            spec = self.spec,
            provider_name = self.provider_name,
        ))
    }

    /// The models whose targets this cell decides `key` for. A
    /// provider-scoped cell yields to a model cell holding the same key,
    /// since the lookup tries the model cell first. `None` when every model
    /// yields, so the cell reaches no target; a provider with no models is
    /// still checked against its provider-level sites.
    fn models_decided_here(&self, key: &str) -> Option<Vec<&str>> {
        if self.is_model_scoped {
            return Some(self.models.clone());
        }
        let reached: Vec<&str> = self
            .models
            .iter()
            .copied()
            .filter(|m| !model_cell_decides(self.config, self.provider_name, m, key))
            .collect();
        (reached.is_empty() == self.models.is_empty()).then_some(reached)
    }
}

/// The models a cell's targets carry: the named model for a model-scoped
/// spec, every model dispatching to the provider for a provider-scoped one.
/// `None` when a model-scoped spec names a model that never dispatches to the
/// provider -- that cell matches no target.
fn cell_models<'c>(
    config: &'c Config,
    provider_name: &str,
    nickname: Option<&'c str>,
) -> Option<Vec<&'c str>> {
    match nickname {
        Some(nickname) => config
            .models
            .get(nickname)
            .is_some_and(|model| dispatches_to(config, &model.provider, provider_name))
            .then(|| vec![nickname]),
        None => Some(
            config
                .models
                .iter()
                .filter(|(_, model)| dispatches_to(config, &model.provider, provider_name))
                .map(|(nickname, _)| nickname.as_str())
                .collect(),
        ),
    }
}

/// Whether a model whose `provider` is `provider_or_pool` dispatches to the
/// `[providers]` entry `provider_name`: directly, or as a member of the pool.
fn dispatches_to(config: &Config, provider_or_pool: &str, provider_name: &str) -> bool {
    provider_or_pool == provider_name
        || config
            .pools
            .get(provider_or_pool)
            .is_some_and(|pool| pool.members.iter().any(|member| member == provider_name))
}

/// Whether the model-scoped cell `provider:nickname` holds an entry for `key`.
/// The override lookup tries that cell first, so the provider cell never
/// decides the flag for that model.
fn model_cell_decides(config: &Config, provider_name: &str, nickname: &str, key: &str) -> bool {
    config
        .capability
        .overrides
        .get(&format!("{provider_name}:{nickname}"))
        .is_some_and(|entry| {
            entry
                .unsupported
                .iter()
                .chain(&entry.force_supported)
                .any(|raw| beta_override_key(raw) == key)
        })
}

/// A flag one egress site re-adds, in the spelling it ships, with the reason
/// an operator reads.
struct Reassertion {
    flag: String,
    reason: String,
}

/// The static facts of one anthropic-api provider entry that decide which
/// egress sites fire for it.
struct EgressLane {
    /// Every dispatched request takes the forwarded leg: the router refuses a
    /// forwarded target with no captured bearer, and validation pins the host.
    forwarded: bool,
    /// The cloak lane: oauth-bearer, own credential, on the Anthropic API
    /// host. A mantle entry has no such host (the factory derives its URL).
    own_oauth: bool,
    cloak_mode: CloakMode,
    context_management: bool,
}

impl EgressLane {
    fn of(provider: &ProviderEntry) -> Option<Self> {
        let ProviderEntry::AnthropicApi {
            auth_kind,
            credential_source,
            cloak,
            context_management,
            ..
        } = provider
        else {
            return None;
        };
        let on_anthropic_host = provider
            .anthropic_api_base_url()
            .is_some_and(is_anthropic_api_host);
        Some(Self {
            forwarded: provider
                .forwarded_base_url()
                .is_some_and(is_anthropic_api_host),
            own_oauth: *auth_kind == AuthKind::OauthBearer
                && *credential_source == CredentialSource::Own
                && on_anthropic_host,
            cloak_mode: cloak.mode,
            context_management: *context_management,
        })
    }

    /// The flags the egress itself mints for this lane, before removals.
    fn minted(&self) -> Vec<Reassertion> {
        let mut out = Vec::new();
        if self.own_oauth {
            out.push(reassertion(
                OAUTH_ANTHROPIC_BETA,
                format!("OAuth gate: always re-added {OWN_OAUTH_LANE}"),
            ));
            if let Some(when) = self.claude_code_floor_condition() {
                out.extend(
                    default_claude_code_anthropic_betas().iter().map(|flag| {
                        reassertion(flag, format!("Claude Code floor: re-added {when}"))
                    }),
                );
            }
        }
        if !self.forwarded {
            out.push(reassertion(
                STRUCTURED_OUTPUTS_BETA,
                "structured outputs: re-added whenever the request carries \
                 output_config.format"
                    .to_string(),
            ));
        }
        if self.own_oauth {
            out.push(reassertion(
                EFFORT_BETA,
                format!(
                    "effort: re-added whenever the request carries output_config.effort, \
                     {OWN_OAUTH_LANE}"
                ),
            ));
        }
        out
    }

    fn claude_code_floor_condition(&self) -> Option<String> {
        match self.cloak_mode {
            CloakMode::Never => None,
            CloakMode::Always => Some(format!(
                "always, {OWN_OAUTH_LANE} with cloak.mode = \"always\""
            )),
            CloakMode::Auto => Some(format!(
                "on requests routectl classifies as non-Claude-Code (normally: no Claude Code \
                 session header), {OWN_OAUTH_LANE}"
            )),
        }
    }

    /// Apply the post-union removals: the context-management strip drops its
    /// flag outright; the display-vs-redaction drop is body-dependent, so it
    /// qualifies the reason instead. Both match the shipped spelling exactly,
    /// as the egress does.
    fn after_removals(&self, site: Reassertion) -> Option<Reassertion> {
        if self.context_management && site.flag == CONTEXT_MANAGEMENT_BETA {
            return None;
        }
        if !self.forwarded && site.flag == REDACT_THINKING_BETA {
            return Some(Reassertion {
                reason: format!(
                    "{} unless the request carries thinking.display, which removes it",
                    site.reason
                ),
                ..site
            });
        }
        Some(site)
    }
}

fn reassertion(flag: &str, reason: String) -> Reassertion {
    Reassertion {
        flag: flag.to_string(),
        reason,
    }
}

/// Every flag the egress ships for this provider and `models` regardless of
/// a withhold: the minted sites plus the provider and model `anthropic-beta`
/// pins, after the post-union removals.
fn egress_reassertions(
    config: &Config,
    provider: &ProviderEntry,
    models: &[&str],
) -> Vec<Reassertion> {
    let Some(lane) = EgressLane::of(provider) else {
        return Vec::new();
    };
    let no_model_extras = std::collections::BTreeMap::new();
    let provider_pins =
        crate::beta_capability::operator_betas(Some(provider.header_extras()), &no_model_extras)
            .into_iter()
            .map(|flag| Reassertion {
                flag,
                reason: "provider header_extras: the provider's anthropic-beta pin always ships"
                    .to_string(),
            });
    let model_pins = models
        .iter()
        .filter_map(|nickname| Some((*nickname, config.models.get(*nickname)?)))
        .flat_map(|(nickname, model)| {
            crate::beta_capability::operator_betas(None, &model.header_extras)
                .into_iter()
                .map(move |flag| Reassertion {
                    flag,
                    reason: format!(
                        "model header_extras of `{nickname}`: the model's anthropic-beta pin \
                         always ships"
                    ),
                })
        });
    lane.minted()
        .into_iter()
        .chain(provider_pins)
        .chain(model_pins)
        .filter_map(|site| lane.after_removals(site))
        .collect()
}

#[cfg(test)]
#[path = "warnings_beta_floor_tests.rs"]
mod tests;
