//! The bounded parser and the one validation step every table passes.

use std::collections::{BTreeMap, BTreeSet};

use super::bounded::parse_canonical;
use super::format::{ApiBase, RawModel, RawTable, SCHEMA_VERSION, SchemaProbe, TableDate};
use super::{PriceTable, PriceTableError, ProviderPrices};
use crate::config::PricingConfig;

/// Largest input, in decoded bytes, either parser accepts. A caller reading
/// a stream caps it here (read at most one byte more to detect overflow).
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

pub(super) const MAX_ROWS: usize = 100_000;
pub(super) const MAX_PROVIDERS: usize = 100_000;
pub(super) const MAX_PROVIDER_ID_BYTES: usize = 128;
pub(super) const MAX_MODEL_ID_BYTES: usize = 512;
pub(super) const MAX_PRICE_PER_MTOK: f64 = 10_000.0;

/// Parses and validates a canonical price table.
pub fn parse_and_validate(bytes: &[u8]) -> Result<PriceTable, PriceTableError> {
    check_size(bytes)?;
    let probe: SchemaProbe =
        serde_json::from_slice(bytes).map_err(|e| PriceTableError::syntax(&e))?;
    if probe.schema_version.and_then(|v| v.0) != Some(SCHEMA_VERSION) {
        return Err(PriceTableError::UnsupportedSchemaVersion);
    }
    validate(parse_canonical(bytes)?)
}

pub(super) const fn check_size(bytes: &[u8]) -> Result<(), PriceTableError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(PriceTableError::TooLarge);
    }
    Ok(())
}

/// Applies every per-row rule to a raw table and builds the sorted,
/// sanitized form. The row and provider counts are bounded earlier, while
/// the raw table is deserialized.
///
/// Structure fails closed: a bad date, a duplicate provider or (provider,
/// model) row, and a table with no priced model refuse the whole table. A
/// single row a newer writer might shape differently does not: a provider
/// or model id that is empty, over its byte bound or outside printable
/// ASCII is skipped and counted (its prices stay unknown and the id is never
/// rendered), and an `api_host` that does not normalize leaves that provider
/// reachable only through the first-party fallback.
pub(super) fn validate(raw: RawTable) -> Result<PriceTable, PriceTableError> {
    let generated_at = TableDate::parse(&raw.generated_at).ok_or(PriceTableError::InvalidDate)?;
    let mut seen = BTreeSet::new();
    let mut skipped_rows = 0usize;
    let mut providers = Vec::with_capacity(raw.providers.len());
    for provider in raw.providers {
        if !id_usable(&provider.id, MAX_PROVIDER_ID_BYTES) {
            skipped_rows = skipped_rows.saturating_add(provider.models.len());
            continue;
        }
        if !seen.insert(provider.id.clone()) {
            return Err(PriceTableError::DuplicateProvider);
        }
        let (models, skipped) = validate_models(provider.models)?;
        skipped_rows = skipped_rows.saturating_add(skipped);
        providers.push(ProviderPrices {
            id: provider.id,
            api: provider.api_host.as_deref().and_then(ApiBase::parse),
            models,
        });
    }
    if !providers
        .iter()
        .flat_map(|p| p.models.values())
        .any(is_priced)
    {
        return Err(PriceTableError::NoPricedModels);
    }
    providers.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(PriceTable {
        generated_at,
        providers,
        skipped_rows,
    })
}

/// The provider's usable rows and how many were skipped for their id.
fn validate_models(
    models: Vec<RawModel>,
) -> Result<(BTreeMap<String, PricingConfig>, usize), PriceTableError> {
    let mut out = BTreeMap::new();
    let mut skipped = 0usize;
    for model in models {
        if !id_usable(&model.id, MAX_MODEL_ID_BYTES) {
            skipped += 1;
            continue;
        }
        let prices = PricingConfig {
            input_per_mtok: model.input.and_then(admit_price),
            output_per_mtok: model.output.and_then(admit_price),
            cache_read_per_mtok: model.cache_read.and_then(admit_price),
            cache_write_5m_per_mtok: model.cache_write_5m.and_then(admit_price),
            cache_write_1h_per_mtok: model.cache_write_1h.and_then(admit_price),
        };
        if out.insert(model.id, prices).is_some() {
            return Err(PriceTableError::DuplicateModel);
        }
    }
    Ok((out, skipped))
}

/// Whether `id` can key a lookup: non-empty, within `limit` bytes, and
/// printable ASCII only.
fn id_usable(id: &str, limit: usize) -> bool {
    id_within(id, limit) && id_charset_ok(id)
}

const fn id_within(id: &str, limit: usize) -> bool {
    !id.is_empty() && id.len() <= limit
}

/// Printable ASCII without the space: every byte in `0x21..=0x7E`.
fn id_charset_ok(id: &str) -> bool {
    id.bytes().all(|b| matches!(b, 0x21..=0x7E))
}

/// A price outside `[0, MAX_PRICE_PER_MTOK]` is unknown; NaN and the
/// infinities fall outside the range.
pub(super) fn admit_price(price: f64) -> Option<f64> {
    (0.0..=MAX_PRICE_PER_MTOK).contains(&price).then_some(price)
}

const fn is_priced(prices: &PricingConfig) -> bool {
    prices.input_per_mtok.is_some()
        || prices.output_per_mtok.is_some()
        || prices.cache_read_per_mtok.is_some()
        || prices.cache_write_5m_per_mtok.is_some()
        || prices.cache_write_1h_per_mtok.is_some()
}
