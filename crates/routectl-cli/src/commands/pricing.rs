//! The CLI's shared cost-pricing seams: the pinned price tables a cost read
//! resolves against, the one conversion from router pricing to the usage
//! leaf's `Rates`, the one managed-subscription predicate every cost surface
//! consults before looking a rate up, and the rate-coverage rule an
//! API-equivalent value is gated on.
//!
//! Lives CLI-side because `Rates` belongs to `routectl-usage`, a zero-dep
//! leaf the router must not depend on: the router owns the per-field price
//! precedence (`routectl_router::prices::effective_pricing`), and this module
//! carries the last step onto the leaf's own struct.

use std::sync::Arc;

use routectl_router::prices::{PriceBook, PriceSource, PriceSources, PriceTable, shipped_table};

use crate::commands::prices::PRICES_FILE;
use routectl_router::{Config, PricingConfig};
use routectl_usage::Rates;

/// The book over `prices.json` in the shared config folder, the file
/// `routectl prices refresh` writes. Nothing is read until a snapshot is
/// pinned from it.
pub fn default_price_book() -> PriceBook {
    PriceBook::new(routectl_core::config_dir::config_dir_or_relative().join(PRICES_FILE))
}

/// The price tables one cost read resolves against: the refreshed table the
/// book held when the snapshot was pinned, and the shipped table.
///
/// Pinned once per read (a `routectl usage` run, one `/status/query`
/// request), so every row of one result prices against the same tables even
/// when a new `prices.json` is renamed in mid-read; the next read sees it.
#[derive(Debug, Clone, Default)]
pub struct PriceSnapshot {
    refreshed: Option<Arc<PriceTable>>,
    shipped: Option<&'static PriceTable>,
}

impl PriceSnapshot {
    /// The book's current table plus the shipped table.
    pub fn pin(book: &PriceBook) -> Self {
        Self::from_tables(book.current(), shipped_table())
    }

    /// A snapshot over explicit tables; `None` leaves that layer empty.
    pub const fn from_tables(
        refreshed: Option<Arc<PriceTable>>,
        shipped: Option<&'static PriceTable>,
    ) -> Self {
        Self { refreshed, shipped }
    }

    /// The refreshed table layer.
    pub fn refreshed(&self) -> Option<&PriceTable> {
        self.refreshed.as_deref()
    }

    /// The shipped table layer.
    pub const fn shipped(&self) -> Option<&'static PriceTable> {
        self.shipped
    }
}

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

/// Whether a resolved rate counts toward a complete API-equivalent value: a
/// rate is present and not negative. A known `0` counts (a free dimension is
/// priced at nothing, which is a fact), while an absent rate never does.
pub(super) fn rate_covers(rate: Option<f64>) -> bool {
    rate.is_some_and(|r| r >= 0.0)
}

/// Whether a priced row resolved from `sources` is complete-or-absent: a
/// dimension it used with no rate leaves it unpriced. True for every row
/// except one a `[registry]` row prices, which counts an absent rate as `0`
/// as the operator wrote it. A table gap or a partial `[models.X.pricing]`
/// block is not a statement that the dimension is free.
pub(super) fn is_complete_or_absent(sources: &PriceSources) -> bool {
    ![
        sources.input,
        sources.output,
        sources.cache_read,
        sources.cache_write_5m,
        sources.cache_write_1h,
    ]
    .contains(&PriceSource::Registry)
}

/// The rate dimensions an API-equivalent value needs priced, in the order a
/// diagnostic names them: the labels of the ones [`rate_covers`] refuses.
///
/// The usage report's complete-or-absent equivalent applies the same rule,
/// minus the token counts. A diagnostic has no usage to weigh, so it asks which
/// dimensions RESOLVE at all rather than which of the used ones did.
///
/// Reasoning is not its own dimension: a disjoint-reasoning row bills thinking
/// tokens at the OUTPUT rate, so the output entry already covers it.
pub(super) fn missing_equivalence_dimensions(pricing: &PricingConfig) -> Vec<&'static str> {
    [
        ("input", pricing.input_per_mtok),
        ("output", pricing.output_per_mtok),
        ("cache read", pricing.cache_read_per_mtok),
        ("cache write 5m", pricing.cache_write_5m_per_mtok),
        ("cache write 1h", pricing.cache_write_1h_per_mtok),
    ]
    .into_iter()
    .filter(|(_, rate)| !rate_covers(*rate))
    .map(|(label, _)| label)
    .collect()
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
