//! What is still missing between a seat that config reaches and traffic
//! actually arriving there: a model naming the seat, and an alias naming
//! that model.
//!
//! Two boolean scans over the typed config, rendered after the login
//! auto-surface settles (a committed write, or a config that already
//! reaches the seat). Pure -- the caller prints.
//!
//! Deliberately NOT routing advice: the scans answer "can anything reach
//! this seat at all", and the rendered shapes carry a placeholder wherever
//! the answer depends on operator intent. The upstream model id in
//! particular is NEVER guessed -- a plausible-looking wrong id is a 404 at
//! the first request, and the catalog, not this module, is where real ids
//! live.

use routectl_router::Config;

use super::login_provider_block::{toml_key, toml_string};

/// The placeholder written where an upstream model id belongs. Not a
/// guessable value: the ids differ per account tier and change per release,
/// so the operator fills it in from the provider's own model list.
const UPSTREAM_PLACEHOLDER: &str = "<upstream model id>";

/// The placeholder nickname of a suggested `[models.<nickname>]` block.
const NICKNAME_PLACEHOLDER: &str = "<nickname>";

/// The routing gap between the seat served by `entry_name` (pooled as
/// `pool`, when it is pooled) and a servable request, or `None` when a
/// model names the seat AND an alias reaches that model.
///
/// At most one gap is reported: with no model at all the alias question is
/// not yet answerable, so naming both would tell the operator to do the
/// second step before the first.
#[must_use]
pub fn availability_gap(config: &Config, entry_name: &str, pool: Option<&str>) -> Option<String> {
    let models = models_naming(config, entry_name, pool);
    if models.is_empty() {
        return Some(missing_model_hint(entry_name, pool));
    }
    if aliases_reaching(config, &models).is_empty() {
        return Some(missing_alias_hint(&models));
    }
    None
}

/// Model nicknames whose `provider` names the entry or its pool.
///
/// A model may name either, so both are servable targets; this is a
/// membership scan, never a suggestion about which one it should name.
fn models_naming(config: &Config, entry_name: &str, pool: Option<&str>) -> Vec<String> {
    config
        .models
        .iter()
        .filter(|(_, model)| {
            model.provider == entry_name || pool.is_some_and(|p| model.provider == p)
        })
        .map(|(nickname, _)| nickname.clone())
        .collect()
}

/// Alias names whose chain reaches any of `models`.
fn aliases_reaching(config: &Config, models: &[String]) -> Vec<String> {
    config
        .aliases
        .iter()
        .filter(|(_, value)| {
            value
                .nicknames()
                .any(|nickname| models.iter().any(|m| m == nickname))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The `[models.<nickname>]` shape that makes the seat servable. The
/// `provider` value is the POOL when the seat is pooled -- a model naming
/// the pool is what lets a later login's seat serve the same model.
fn missing_model_hint(entry_name: &str, pool: Option<&str>) -> String {
    let target = pool.unwrap_or(entry_name);
    format!(
        "No model routes to this account yet, so nothing can be requested from it. \
         Add a model entry naming it, with the upstream id taken from the provider's \
         own model list:\n\n\
         [models.{}]\n\
         provider = {}\n\
         upstream = {}",
        NICKNAME_PLACEHOLDER,
        toml_string(target),
        toml_string(UPSTREAM_PLACEHOLDER),
    )
}

/// The alias gap: a model reaches the seat but no alias reaches the model,
/// so a client asking for an alias never lands there.
fn missing_alias_hint(models: &[String]) -> String {
    let listed: Vec<String> = models.iter().map(|m| toml_key(m)).collect();
    format!(
        "Model(s) {} route to this account, but no alias reaches them, so a client \
         asking for an alias never lands there. Point one at a model:\n\n\
         [aliases]\n\
         default = {}",
        listed.join(", "),
        toml_string(&models[0]),
    )
}

#[cfg(test)]
#[path = "login_surface_availability_tests.rs"]
mod tests;
