//! What every member of one `[pools.<name>]` block must share.
//!
//! A pool spreads requests across interchangeable seats, so two members may
//! differ only in per-seat tuning (rate limit, breaker, timeouts). Anything
//! that changes where a request goes, how it authenticates, which managed
//! login it rides, or how it is cloaked would make the pool's behavior
//! depend on which seat a request lands on. [`PoolKey`] is that shared
//! part of a provider entry, reduced to comparable values.

use routectl_auth::{CredentialRef, SecretRef};
use routectl_providers::anthropic_api::{CloakConfig, CloakMode};

use super::ProviderEntry;
use super::provider_descriptor::{Auth, Format};

/// The part of a provider entry every member of one pool must agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolKey {
    format: Format,
    destination: String,
    auth: Auth,
    oauth_vendor: Option<String>,
    cloak_mode: CloakMode,
    cloak_strict_mode: bool,
}

/// One [`PoolKey`] component, named the way an operator-facing message
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKeyField {
    Format,
    Destination,
    Auth,
    OauthVendor,
    CloakMode,
    CloakStrictMode,
}

impl PoolKeyField {
    /// The field as a message names it: the config key, or a description
    /// of the keys it is derived from.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Format => "`format`",
            Self::Destination => "destination (`base_url` or `cloud`)",
            Self::Auth => "`auth`",
            Self::OauthVendor => "OAuth vendor of `credential`",
            Self::CloakMode => "`cloak_mode`",
            Self::CloakStrictMode => "`cloak_strict_mode`",
        }
    }
}

impl PoolKey {
    /// The key of one provider entry. Reads no credential.
    pub fn of(entry: &ProviderEntry) -> Self {
        let cloak = entry.effective_cloak();
        Self {
            format: entry.format,
            destination: normalized_destination(entry),
            auth: entry.auth,
            oauth_vendor: oauth_vendor(&entry.credential),
            cloak_mode: cloak.mode,
            cloak_strict_mode: cloak.strict_mode,
        }
    }

    /// Every field on which `self` and `other` disagree, in declaration
    /// order. Empty when the two entries may share a pool.
    pub fn differing_fields(&self, other: &Self) -> Vec<PoolKeyField> {
        [
            (self.format != other.format, PoolKeyField::Format),
            (
                self.destination != other.destination,
                PoolKeyField::Destination,
            ),
            (self.auth != other.auth, PoolKeyField::Auth),
            (
                self.oauth_vendor != other.oauth_vendor,
                PoolKeyField::OauthVendor,
            ),
            (self.cloak_mode != other.cloak_mode, PoolKeyField::CloakMode),
            (
                self.cloak_strict_mode != other.cloak_strict_mode,
                PoolKeyField::CloakStrictMode,
            ),
        ]
        .into_iter()
        .filter_map(|(differs, field)| differs.then_some(field))
        .collect()
    }
}

impl ProviderEntry {
    /// The cloak settings this entry's provider is built with: the provider
    /// crate's defaults, overridden by whichever cloak keys the entry sets,
    /// so an unset key equals its explicit default.
    pub(crate) fn effective_cloak(&self) -> CloakConfig {
        let mut cloak = CloakConfig::default();
        if let Some(mode) = self.cloak_mode {
            cloak.mode = mode;
        }
        if let Some(strict) = self.cloak_strict_mode {
            cloak.strict_mode = strict;
        }
        cloak
    }
}

/// Where the entry's requests go, in a form two equivalent spellings share.
///
/// A base URL is parsed the way the HTTP client parses it (scheme and host
/// lowercased, a default port dropped, the path kept), then loses every
/// trailing `/`, which providers strip before appending their endpoint
/// path. A base URL that does not parse is compared as written, minus
/// trailing `/`; base URL validation refuses it separately. A cloud entry
/// is `aws:<region>`.
fn normalized_destination(entry: &ProviderEntry) -> String {
    if let Some(cloud) = &entry.cloud {
        return format!("aws:{}", cloud.aws_region());
    }
    let raw = entry.base_url.as_deref().unwrap_or_default();
    let parsed = url::Url::parse(raw).map_or_else(|_| raw.to_owned(), String::from);
    parsed.trim_end_matches('/').to_owned()
}

/// The vendor id of an `oauth://<vendor>[#label]` credential, label
/// dropped; `None` for any other credential.
fn oauth_vendor(credential: &str) -> Option<String> {
    match CredentialRef::parse(credential) {
        Ok(CredentialRef::Secret(SecretRef::OAuth { provider, .. })) => Some(provider),
        _ => None,
    }
}

#[cfg(test)]
#[path = "pool_key_tests.rs"]
mod tests;
