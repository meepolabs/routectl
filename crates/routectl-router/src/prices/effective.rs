//! Effective per-field pricing for one served target, with the source of
//! every field.
//!
//! Precedence, field by field:
//!
//! 1. `[models.X.pricing]` on the served model entry.
//! 2. The matching `[registry]` row. It wins whole and verbatim: a field it
//!    leaves unset stays unknown, and neither the tables nor the derived
//!    figure fill it.
//! 3. The refreshed table, then the shipped table, each selected for the
//!    target as [`PriceTable::price_for`] describes.
//! 4. A derived `cache_write_1h` for the Anthropic family (an `anthropic-api`
//!    target, or a `bedrock` target whose upstream is an Anthropic model):
//!    twice the effective input price, only when a refreshed or shipped row
//!    was selected and no source above gives the field.
//! 5. Unknown.

use routectl_core::provider_kind::config as kind;
use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

use super::PriceTable;
use super::select::provider_entry_for_target;
use crate::config::{Config, PricingConfig};

/// Anthropic bills a 1-hour cache write at twice the base input price.
const ANTHROPIC_1H_WRITE_INPUT_MULTIPLE: f64 = 2.0;

/// Where one effective price field came from. Each variant renders as one
/// fixed token through [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// The served model's `[models.X.pricing]` block.
    ModelConfig,
    /// The matching `[registry]` row.
    Registry,
    /// The refreshed price table in the config directory.
    Refreshed,
    /// The price table shipped with the release.
    Shipped,
    /// Derived from the input price by a per-vendor rule.
    Derived,
    /// No source prices the field.
    Unknown,
}

impl PriceSource {
    /// The fixed token for this source.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelConfig => "model-config",
            Self::Registry => "registry",
            Self::Refreshed => "refreshed",
            Self::Shipped => "shipped",
            Self::Derived => "derived",
            Self::Unknown => "unknown",
        }
    }
}

/// The source of each field of an effective [`PricingConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceSources {
    /// Source of `input_per_mtok`.
    pub input: PriceSource,
    /// Source of `output_per_mtok`.
    pub output: PriceSource,
    /// Source of `cache_read_per_mtok`.
    pub cache_read: PriceSource,
    /// Source of `cache_write_5m_per_mtok`.
    pub cache_write_5m: PriceSource,
    /// Source of `cache_write_1h_per_mtok`.
    pub cache_write_1h: PriceSource,
}

impl PriceSources {
    pub(crate) const UNKNOWN: Self = Self {
        input: PriceSource::Unknown,
        output: PriceSource::Unknown,
        cache_read: PriceSource::Unknown,
        cache_write_5m: PriceSource::Unknown,
        cache_write_1h: PriceSource::Unknown,
    };
}

/// The served target a price is looked up for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceSubject<'a> {
    /// The served `[models]` nickname, whose `[models.X.pricing]` applies
    /// when that entry still names `upstream`. `None` when unknown.
    pub model: Option<&'a str>,
    /// The `[providers]` entry that served the request. A `[pools]` key
    /// resolves to its first member; a usage row passes the served member.
    pub provider: &'a str,
    /// The upstream model id, matched exactly.
    pub upstream: &'a str,
    /// The provider kind recorded with a usage row. Used only when
    /// `provider` no longer names a config entry, so a renamed or removed
    /// provider still prices a first-party upstream.
    pub persisted_kind: Option<&'a str>,
}

/// The effective pricing for `subject` under `config`, given the refreshed
/// and shipped tables, with the source of each field (precedence in the
/// module docs). `None` when no `[registry]` row matches and every field is
/// unknown; a matching `[registry]` row is returned even when it prices
/// nothing, as written.
///
/// A managed-subscription (`oauth://`) provider resolves like any other, so
/// its API-equivalent value uses the same precedence.
pub fn effective_pricing(
    config: &Config,
    subject: &PriceSubject<'_>,
    refreshed: Option<&PriceTable>,
    shipped: Option<&PriceTable>,
) -> Option<(PricingConfig, PriceSources)> {
    let model_config = model_pricing(config, subject);
    let registry = config.pricing_for(subject.upstream, subject.provider);
    let tables = match registry {
        Some(_) => TableRows::default(),
        None => TableRows::select(config, subject, refreshed, shipped),
    };

    let mut layers: Vec<(&PricingConfig, PriceSource)> = Vec::with_capacity(4);
    layers.extend(model_config.map(|p| (p, PriceSource::ModelConfig)));
    layers.extend(registry.map(|p| (p, PriceSource::Registry)));
    layers.extend(
        tables
            .refreshed
            .as_ref()
            .map(|p| (p, PriceSource::Refreshed)),
    );
    layers.extend(tables.shipped.as_ref().map(|p| (p, PriceSource::Shipped)));

    let mut pricing = PricingConfig::default();
    let mut sources = PriceSources::UNKNOWN;
    for field in Field::ALL {
        if let Some((value, source)) = layers
            .iter()
            .find_map(|(layer, source)| field.get(layer).map(|v| (v, *source)))
        {
            *field.value_mut(&mut pricing) = Some(value);
            *field.source_mut(&mut sources) = source;
        }
    }

    if tables.has_row() && is_anthropic_family(tables.kind, subject.upstream) {
        derive_1h_write(&mut pricing, &mut sources);
    }

    if registry.is_none() && sources == PriceSources::UNKNOWN {
        return None;
    }
    Some((pricing, sources))
}

/// The served model's `[models.X.pricing]`, when that entry still names the
/// subject's upstream.
fn model_pricing<'a>(config: &'a Config, subject: &PriceSubject<'_>) -> Option<&'a PricingConfig> {
    let entry = config.models.get(subject.model?)?;
    if entry.upstream != subject.upstream {
        return None;
    }
    entry.pricing.as_ref()
}

/// The table rows selected for a subject and the provider kind that
/// selected them.
#[derive(Default)]
struct TableRows<'a> {
    kind: Option<&'a str>,
    refreshed: Option<PricingConfig>,
    shipped: Option<PricingConfig>,
}

impl<'a> TableRows<'a> {
    fn select(
        config: &'a Config,
        subject: &PriceSubject<'a>,
        refreshed: Option<&PriceTable>,
        shipped: Option<&PriceTable>,
    ) -> Self {
        match provider_entry_for_target(config, subject.provider) {
            Some(entry) => {
                let lookup = |table: &PriceTable| {
                    table.price_for(config, subject.provider, subject.upstream)
                };
                Self {
                    kind: Some(entry.kind_str()),
                    refreshed: refreshed.and_then(lookup),
                    shipped: shipped.and_then(lookup),
                }
            }
            None => {
                let Some(kind) = subject.persisted_kind else {
                    return Self::default();
                };
                let lookup =
                    |table: &PriceTable| table.price_for_target(kind, None, subject.upstream);
                Self {
                    kind: Some(kind),
                    refreshed: refreshed.and_then(lookup),
                    shipped: shipped.and_then(lookup),
                }
            }
        }
    }

    const fn has_row(&self) -> bool {
        self.refreshed.is_some() || self.shipped.is_some()
    }
}

fn is_anthropic_family(provider_kind: Option<&str>, upstream: &str) -> bool {
    match provider_kind {
        Some(kind::ANTHROPIC_API) => true,
        Some(kind::BEDROCK) => anthropic_family(upstream) == AnthropicFamily::Yes,
        _ => false,
    }
}

fn derive_1h_write(pricing: &mut PricingConfig, sources: &mut PriceSources) {
    if pricing.cache_write_1h_per_mtok.is_some() {
        return;
    }
    let Some(input) = pricing.input_per_mtok else {
        return;
    };
    pricing.cache_write_1h_per_mtok = Some(input * ANTHROPIC_1H_WRITE_INPUT_MULTIPLE);
    sources.cache_write_1h = PriceSource::Derived;
}

/// One price field, addressed the same way in a [`PricingConfig`] and a
/// [`PriceSources`].
#[derive(Clone, Copy)]
enum Field {
    Input,
    Output,
    CacheRead,
    CacheWrite5m,
    CacheWrite1h,
}

impl Field {
    const ALL: [Self; 5] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite5m,
        Self::CacheWrite1h,
    ];

    const fn get(self, pricing: &PricingConfig) -> Option<f64> {
        match self {
            Self::Input => pricing.input_per_mtok,
            Self::Output => pricing.output_per_mtok,
            Self::CacheRead => pricing.cache_read_per_mtok,
            Self::CacheWrite5m => pricing.cache_write_5m_per_mtok,
            Self::CacheWrite1h => pricing.cache_write_1h_per_mtok,
        }
    }

    const fn value_mut(self, pricing: &mut PricingConfig) -> &mut Option<f64> {
        match self {
            Self::Input => &mut pricing.input_per_mtok,
            Self::Output => &mut pricing.output_per_mtok,
            Self::CacheRead => &mut pricing.cache_read_per_mtok,
            Self::CacheWrite5m => &mut pricing.cache_write_5m_per_mtok,
            Self::CacheWrite1h => &mut pricing.cache_write_1h_per_mtok,
        }
    }

    const fn source_mut(self, sources: &mut PriceSources) -> &mut PriceSource {
        match self {
            Self::Input => &mut sources.input,
            Self::Output => &mut sources.output,
            Self::CacheRead => &mut sources.cache_read,
            Self::CacheWrite5m => &mut sources.cache_write_5m,
            Self::CacheWrite1h => &mut sources.cache_write_1h,
        }
    }
}
