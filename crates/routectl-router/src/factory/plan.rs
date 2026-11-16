//! The one decision taken about a provider entry before any of its
//! credentials is read.

use crate::config::ProviderEntry;
use crate::config::provider_descriptor::{client_identity, containment};
use routectl_core::Result;

/// What the factory decided about one provider entry, from the entry alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPlan {
    /// Whether the provider presents its vendor's generated client identity.
    pub client_identity: bool,
}

/// Plan the build of `entry` (configured as provider `name`) without reading
/// any credential.
///
/// Refuses an entry that carries a routectl-managed vendor token outside
/// that vendor's own lane; the error names the provider and the vendor's
/// rule, never the base URL, reference or seat label. Every build path calls
/// this first: no `SecretStore` read (token, probe, account id, session id,
/// project id) and no AWS credential resolution may happen for an entry
/// whose plan was refused.
pub fn plan_provider(name: &str, entry: &ProviderEntry) -> Result<ProviderPlan> {
    containment(name, entry)?;
    Ok(ProviderPlan {
        client_identity: client_identity(entry),
    })
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plan_identity_wire_tests.rs"]
mod identity_wire_tests;
