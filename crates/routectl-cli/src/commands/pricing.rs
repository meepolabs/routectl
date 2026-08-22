//! The CLI's shared cost-pricing seams: the one conversion from router
//! pricing to the usage leaf's `Rates`, and the one managed-subscription
//! predicate every cost surface consults before looking a rate up.
//!
//! Lives CLI-side because `Rates` belongs to `routectl-usage`, a zero-dep
//! leaf the router must not depend on: the router owns the per-token ->
//! per-million conversion and the registry-vs-catalog precedence
//! (`routectl_router::effective_pricing`), and this module carries the last
//! step onto the leaf's own struct.

use routectl_router::{Config, PricingConfig};
use routectl_usage::Rates;

/// True iff `target` bills as a managed-OAuth subscription rather than per
/// token.
///
/// `target` may name a `[providers]` entry (its own `api_key_ref` decides) or a
/// `[pools]` block, because `[models.X] provider` resolves against both in one
/// namespace. A pool answers from its MEMBERS: `validate_pools` requires every
/// member to authenticate with an `oauth://` credential, so a pool whose
/// members resolve is a subscription -- checking members rather than assuming
/// keeps the predicate honest if that restriction is later widened to API-key
/// members. A name unknown to the config, or a provider declaring no ref at
/// all, is NOT subscription (it simply carries no cost). This is the ONLY
/// subscription signal -- `auth_kind` is deliberately not consulted.
///
/// THE shared predicate: every cost surface (the usage report's row
/// classification, the doctor pricing section, the probe estimate) checks it
/// FIRST, before any rate lookup. A subscription is billed by seat, so what
/// its per-token rates would have been is not a fact about the bill, and a
/// surface that resolved rates anyway would render a figure the other
/// surfaces correctly refuse to.
pub(super) fn is_subscription(config: &Config, target: &str) -> bool {
    if let Some(entry) = config.providers.get(target) {
        return entry
            .api_key_ref()
            .is_some_and(|r| r.starts_with("oauth://"));
    }
    config.pools.get(target).is_some_and(|pool| {
        let mut members = pool
            .members
            .iter()
            .filter_map(|member| config.providers.get(member))
            .peekable();
        members.peek().is_some()
            && members.all(|entry| {
                entry
                    .api_key_ref()
                    .is_some_and(|r| r.starts_with("oauth://"))
            })
    })
}

/// Convert the router's per-million-token pricing into the usage crate's
/// leaf-safe `Rates`. Both units are USD per million tokens, so this is a
/// field-for-field carry, not a conversion.
///
/// `reasoning_per_mtok` starts unset: whether reasoning tokens bill as their
/// own dimension is a property of the row being priced, not of the rate
/// table, so only a caller that knows a row's reasoning structure promotes it.
pub(super) const fn rates_from_pricing(pricing: &PricingConfig) -> Rates {
    Rates {
        input_per_mtok: pricing.input_per_mtok,
        output_per_mtok: pricing.output_per_mtok,
        reasoning_per_mtok: None,
        cache_read_per_mtok: pricing.cache_read_per_mtok,
        cache_write_5m_per_mtok: pricing.cache_write_5m_per_mtok,
        cache_write_1h_per_mtok: pricing.cache_write_1h_per_mtok,
    }
}
