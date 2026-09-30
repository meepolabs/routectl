//! Config-load validation of each `[providers.X.cloak]` table's
//! `sensitive_words` bounds. The rule itself is the providers crate's
//! [`routectl_providers::anthropic_api::validate_sensitive_words`], shared with
//! the matcher so config load and request time cannot disagree on it.

use crate::config::{Config, ProviderEntry};
use routectl_core::{Error, Result};
use routectl_providers::anthropic_api::validate_sensitive_words;

/// Reject any provider whose `cloak.sensitive_words` list is outside its
/// count or folded-length bound, reporting every offending provider. Runs on
/// every config-validation path via [`super::collect_config_validation`].
/// The message names the provider and the bound only; never the configured
/// word, which is operator content.
pub(super) fn validate_cloak_sensitive_words(config: &Config) -> Result<()> {
    let refusals: Vec<String> = config
        .providers
        .iter()
        .filter_map(|(name, entry)| {
            let ProviderEntry::AnthropicApi { cloak, .. } = entry else {
                return None;
            };
            validate_sensitive_words(&cloak.sensitive_words)
                .err()
                .map(|bound| format!("provider `{name}`: cloak {bound}"))
        })
        .collect();
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(refusals.join("; ")))
    }
}

#[cfg(test)]
#[path = "validate_cloak_tests.rs"]
mod tests;
