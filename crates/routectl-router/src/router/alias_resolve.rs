//! Config-only alias resolution: the router's wire-name precedence and
//! chain expansion, read off a `Config` alone (no installed providers, no
//! dispatch side effects). For offline reports and read-side views that
//! must name the chain a wire name would dispatch to.

use routectl_core::{Error, Result};

use crate::config::{AliasValue, Config};
use crate::glob::AliasPattern;

use super::ALIAS_MAX_RECURSION_DEPTH;

/// Whether a top-level chain step names a model nickname or another alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStepKind {
    /// The entry is a `[models]` nickname.
    Model,
    /// The entry is an `[aliases]` key, expanded in place.
    Alias,
}

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
    if let Some(value) = config.aliases.get(wire) {
        return Some(value.clone());
    }
    if let Some(value) = longest_glob_match(config, wire) {
        return Some(value.clone());
    }
    if config.models.contains_key(wire) {
        return Some(AliasValue::Single(wire.to_string()));
    }
    config.aliases.get("default").cloned()
}

/// Longest matching prefix-glob key. Malformed glob keys are skipped; the
/// startup validators reject them.
fn longest_glob_match<'a>(config: &'a Config, wire: &str) -> Option<&'a AliasValue> {
    config
        .aliases
        .iter()
        .filter_map(|(key, value)| match AliasPattern::parse(key) {
            Ok(pattern @ AliasPattern::Prefix(_)) if pattern.matches(wire) => {
                Some((pattern.prefix_len(), value))
            }
            _ => None,
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, value)| value)
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
    let mut steps = Vec::new();
    for entry in value.nicknames() {
        if let Some(nested) = config.aliases.get(entry) {
            let mut models = Vec::new();
            expand_nicknames(config, nested, 1, &mut models)?;
            steps.push(AliasStep {
                name: entry.to_string(),
                kind: AliasStepKind::Alias,
                models,
            });
        } else if config.models.contains_key(entry) {
            steps.push(AliasStep {
                name: entry.to_string(),
                kind: AliasStepKind::Model,
                models: vec![entry.to_string()],
            });
        }
    }
    Ok(steps)
}

fn expand_nicknames(
    config: &Config,
    value: &AliasValue,
    depth: usize,
    out: &mut Vec<String>,
) -> Result<()> {
    if depth > ALIAS_MAX_RECURSION_DEPTH {
        return Err(Error::Config(format!(
            "alias chain recursion exceeded depth {ALIAS_MAX_RECURSION_DEPTH}; \
             run `routectl config check` to surface the offending alias"
        )));
    }
    for entry in value.nicknames() {
        if let Some(nested) = config.aliases.get(entry) {
            expand_nicknames(config, nested, depth + 1, out)?;
        } else if config.models.contains_key(entry) {
            out.push(entry.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "alias_resolve_tests.rs"]
mod alias_resolve_tests;
