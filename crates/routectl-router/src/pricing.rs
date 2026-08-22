//! The ONE price-unit conversion + precedence boundary.
//!
//! UNITS. The operator-facing canonical unit is USD PER MILLION tokens: that
//! is what `[registry.*] pricing` carries, what the economics surfaces
//! render, and what the usage leaf's rate struct expects. The baked catalog
//! table's internal unit is USD PER TOKEN. This module holds the single
//! conversion between them; the per-token fields are crate-visible so no
//! consumer outside this crate can join the two layers raw.
//!
//! PRECEDENCE. An operator `[registry]` pricing row wins WHOLE and verbatim.
//! Only when no registry row prices the `(upstream, provider)` pair does the
//! two-layer catalog row fill in. See [`effective_pricing`].

use crate::catalog::{self, CatalogRow};
use crate::catalog_overlay::CatalogOverlay;
use crate::config::{Config, PricingConfig};

/// Which layer supplied an [`effective_pricing`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingSource {
    /// An operator `[registry.*] pricing` row, returned verbatim.
    Registry,
    /// The two-layer catalog effective row, converted to per-million.
    Catalog,
}

/// The quantum the per-token -> per-million conversion rounds to: 1e-4 USD
/// per million tokens.
///
/// Rounding ONCE to this quantum is what makes the conversion exact. A raw
/// `f64::from(f32) * 1e6` multiply leaves representation dust (a baked
/// `1.5e-5` lands on `15.000000...7` per Mtok), which would make every
/// round-trip assertion an epsilon comparison and would render a
/// hair-off rate to the operator. Every distinct baked rate, and both
/// endpoints of the range the table spans, recovers its exact decimal
/// value at this quantum.
const PER_MTOK_QUANTUM: f64 = 1e4;

/// Tokens per million: the per-token -> per-million scale factor.
const TOKENS_PER_MILLION: f64 = 1e6;

/// Convert one catalog row's base rates into per-million pricing.
///
/// Fills the INPUT and OUTPUT dimensions ONLY, and returns `None` when the
/// row prices neither -- an absent rate stays absent, never a fabricated
/// zero.
///
/// The cache `_per_mtok` dimensions are deliberately left unset and are NOT
/// derived from the row's `wm` / `rm` multipliers: catalog codegen emits
/// economics-unconfirmed cells carrying SENTINEL multipliers alongside real
/// base rates, and the row carries no flag distinguishing a sentinel
/// multiplier from a measured one. Multiplying a real rate by a sentinel
/// multiplier would fabricate a dollar figure, so cache rates remain
/// `[registry]`-only.
fn pricing_from_catalog_row(row: &CatalogRow) -> Option<PricingConfig> {
    let input_per_mtok = row.input_cost_per_token.map(per_mtok);
    let output_per_mtok = row.output_cost_per_token.map(per_mtok);
    if input_per_mtok.is_none() && output_per_mtok.is_none() {
        return None;
    }
    Some(PricingConfig {
        input_per_mtok,
        output_per_mtok,
        cache_read_per_mtok: None,
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
    })
}

/// Scale one per-token rate to per-million, rounded once to
/// [`PER_MTOK_QUANTUM`].
fn per_mtok(per_token: f32) -> f64 {
    (f64::from(per_token) * TOKENS_PER_MILLION * PER_MTOK_QUANTUM).round() / PER_MTOK_QUANTUM
}

/// Resolve the effective per-million pricing for an `(upstream, provider)`
/// pair, together with the layer it came from.
///
/// An operator `[registry]` row wins WHOLE and verbatim: it is returned as
/// written, never merged per field with a catalog row. `PricingConfig` has no
/// explicitly-unpriced sentinel, so a per-field fill could not tell a
/// deliberate omission from an absent value and would silently overwrite the
/// operator's intent.
///
/// Otherwise the two-layer catalog row for `(provider_kind, upstream)` fills
/// in. A `Disabled` or `Missing` cell yields `None`, as does a present row
/// that prices neither the input nor the output dimension -- unpriced, never
/// zero.
///
/// An EMPTY `provider_kind` never fills from the catalog: the kind is half
/// the catalog key, so an empty one identifies no cell. Callers pass an empty
/// kind for a subject whose kind is unknown (an unresolvable provider entry, a
/// ledger row persisted before the kind column was populated), and such a
/// subject fails closed to unpriced rather than borrowing a catch-all's rates.
#[must_use]
pub fn effective_pricing(
    config: &Config,
    overlay: &CatalogOverlay,
    provider_kind: &str,
    upstream: &str,
    provider: &str,
) -> Option<(PricingConfig, PricingSource)> {
    if let Some(pricing) = config.pricing_for(upstream, provider) {
        return Some((pricing.clone(), PricingSource::Registry));
    }
    if provider_kind.is_empty() {
        return None;
    }
    let row = catalog::resolve_effective_row(
        provider_kind,
        upstream,
        None,
        &config.cache_pricing,
        overlay,
    );
    let pricing = pricing_from_catalog_row(row.priced()?)?;
    Some((pricing, PricingSource::Catalog))
}

#[cfg(test)]
#[path = "pricing_tests.rs"]
mod tests;
