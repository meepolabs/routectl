//! Config-load validation of every AWS region that derives an endpoint
//! host: native Bedrock `region` and each lane's `bedrock_mantle.region`.
//! The parse itself is the providers crate's
//! [`routectl_providers::aws_region::parse_aws_region`], shared with the
//! endpoint builders and signers so config load and request time cannot
//! disagree on what a region is.

use crate::config::{Config, ProviderEntry};
use routectl_core::{Error, Result};
use routectl_providers::aws_region::parse_aws_region;

/// Reject any provider whose endpoint-deriving AWS region is not a
/// canonical region identifier. Runs on every config-validation path via
/// [`super::collect_config_validation`]. The message names the provider
/// and the field but never echoes the value, which is untrusted input.
pub fn validate_aws_regions(config: &Config) -> Result<()> {
    for (name, entry) in &config.providers {
        if let Some((field, region)) = endpoint_region(entry) {
            parse_aws_region(region).map_err(|e| {
                Error::Config(format!(
                    "provider `{name}`: {field} is {e}; the endpoint host and SigV4 scope \
                     derive from it"
                ))
            })?;
        }
    }
    Ok(())
}

fn endpoint_region(entry: &ProviderEntry) -> Option<(&'static str, &str)> {
    const MANTLE_FIELD: &str = "bedrock_mantle.region";
    match entry {
        ProviderEntry::Bedrock { region, .. } => Some(("region", region)),
        ProviderEntry::AnthropicApi {
            bedrock_mantle: Some(mantle),
            ..
        }
        | ProviderEntry::OpenaiCompat {
            bedrock_mantle: Some(mantle),
            ..
        } => Some((MANTLE_FIELD, &mantle.region)),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses {
            bedrock_mantle: Some(mantle),
            ..
        } => Some((MANTLE_FIELD, &mantle.region)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "validate_region_tests.rs"]
mod tests;
