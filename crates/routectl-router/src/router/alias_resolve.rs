//! Config-only alias resolution: the router's wire-name precedence and
//! chain expansion, read off a `Config` alone (no installed providers, no
//! dispatch side effects). For offline reports and read-side views that
//! must name the chain a wire name would dispatch to.

use routectl_core::Result;

use crate::config::{AliasValue, Config};

use super::alias_match::{ROUTE_PRECEDENCE, RouteMatch, expand_entry, glob_index, route_match};

pub use super::alias_match::AliasStepKind;

/// One top-level entry of an alias chain and the model nicknames it
/// expands to, in dispatch order. A `Model` step holds exactly its own
/// nickname; an `Alias` step holds its nested expansion (possibly empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasStep {
    /// The entry as written in the chain.
    pub name: String,
    /// Whether the entry names a model or an alias.
    pub kind: AliasStepKind,
    /// The model nicknames the entry expands to, in dispatch order.
    pub models: Vec<String>,
}

/// The alias value a wire model name resolves to, by the router's
/// precedence: exact `[aliases]` key, longest-prefix glob key, direct
/// `[models]` nickname (as a single-entry value), then the `default` key.
/// `None` when nothing matches.
///
/// Unlike dispatch, a matched alias whose targets all fail to expand does
/// not fall through to the later steps. Startup validation rejects a
/// config whose alias targets are unknown, so dispatch falls through only
/// when its installed model table lacks every target (a provider that
/// failed to build), which a config alone cannot show.
#[must_use]
pub fn resolve_alias_value(config: &Config, wire: &str) -> Option<AliasValue> {
    let globs = glob_index(&config.aliases, |_, _| {});
    let lookup = configured_nickname(config);
    ROUTE_PRECEDENCE.into_iter().find_map(|step| {
        match route_match(&config.aliases, &globs, wire, step, &lookup)? {
            RouteMatch::Alias(value) => Some(value),
            RouteMatch::Nickname(nickname) => Some(AliasValue::Single(nickname)),
        }
    })
}

/// Each top-level entry of `value` with the model nicknames it expands to.
///
/// Follows the dispatch expansion: an entry that is an `[aliases]` key is
/// expanded (alias keys win over model nicknames), an entry that is a
/// `[models]` nickname is a model step, and any other entry is dropped.
/// Concatenating every step's `models` yields the dispatch chain's
/// nickname order.
///
/// # Errors
/// `Error::Config` when nesting exceeds `ALIAS_MAX_RECURSION_DEPTH`, which
/// is how a cycle surfaces.
pub fn alias_steps(config: &Config, value: &AliasValue) -> Result<Vec<AliasStep>> {
    let lookup = configured_nickname(config);
    let mut steps = Vec::new();
    for entry in value.nicknames() {
        let mut models = Vec::new();
        if let Some(kind) = expand_entry(&config.aliases, entry, 0, &lookup, &mut models)? {
            steps.push(AliasStep {
                name: entry.to_string(),
                kind,
                models,
            });
        }
    }
    Ok(steps)
}

/// A nickname resolves when `[models]` configures it.
fn configured_nickname(config: &Config) -> impl Fn(&str) -> Option<String> + '_ {
    |nickname| {
        config
            .models
            .contains_key(nickname)
            .then(|| nickname.to_string())
    }
}

#[cfg(test)]
#[path = "alias_resolve_tests.rs"]
mod alias_resolve_tests;
