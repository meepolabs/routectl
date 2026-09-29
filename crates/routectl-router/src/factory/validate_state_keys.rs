//! Config-load validation of the names that become runtime-state keys.
//!
//! Model nicknames, pooled seats (`nickname#member`), and provider names all
//! key ONE runtime-state map (breaker, RPM bucket, probe slot, learned state).
//! The grammar lives in [`crate::seat_pool`]; this pass enforces it over EVERY
//! `[models]` and `[providers]` key -- selectable or not, referenced or not --
//! so no entry can reach that map under a key another identity also composes.
//! Provider names also key durable paid-probe reservations, which store the
//! name hex-encoded, so they carry a byte ceiling as well.

use crate::config::Config;
use crate::seat_pool::check_state_key_name;
use routectl_core::{Error, Result};

/// Longest accepted provider name, in bytes. A paid-probe reservation key
/// carries the name hex-encoded (two bytes per name byte) in durable storage,
/// so the ceiling bounds that key; it sits far above any name a config author
/// writes by hand.
const MAX_PROVIDER_NAME_BYTES: usize = 128;

/// Reject a separator-bearing model nickname or provider name, a provider name
/// longer than [`MAX_PROVIDER_NAME_BYTES`], and a model
/// nickname equal to the name of a provider it does not dispatch through.
/// A model named after its OWN provider (the shape `routectl init` writes)
/// shares a slot seeded from that same provider's policy, which is one gate
/// rather than two -- sound only while provider-name slots stay seed
/// placeholders no production path looks up (see
/// `Router::install_resolved_models`). Runs on every config-validation path via
/// [`super::collect_config_validation`], reporting every offending key.
pub(super) fn validate_state_key_names(config: &Config) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for name in config.providers.keys() {
        if name.len() > MAX_PROVIDER_NAME_BYTES {
            // The name itself is withheld: echoing an oversized key would put
            // the unbounded value into every error surface that renders this.
            errors.push(format!(
                "a provider name is {} bytes; the maximum is {MAX_PROVIDER_NAME_BYTES}",
                name.len()
            ));
            continue;
        }
        errors.extend(check_state_key_name("provider name", name).err());
    }
    for (nickname, entry) in &config.models {
        errors.extend(check_state_key_name("model nickname", nickname).err());
        if config.providers.contains_key(nickname) && entry.provider != *nickname {
            errors.push(format!(
                "model nickname `{nickname}` equals provider name `{nickname}` but the model \
                 dispatches through `{}`; both would key the same runtime state (circuit \
                 breaker, rate limit, probes) -- rename the model or the provider",
                entry.provider
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

#[cfg(test)]
#[path = "validate_state_keys_tests.rs"]
mod tests;
