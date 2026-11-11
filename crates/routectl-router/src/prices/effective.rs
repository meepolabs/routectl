//! Effective per-field pricing for one served target, with the source of
//! every field.
//!
//! Precedence, field by field, where each cache-write lifetime is its own
//! field:
//!
//! 1. `[models.X.pricing]` on the served model entry.
//! 2. The matching `[registry]` row. It wins whole and verbatim: a field it
//!    leaves unset stays unknown, and neither the tables nor the derived
//!    figure fill it.
//! 3. The refreshed table, then the shipped table, each selected for the
//!    target as the module's provider selection describes.
//! 4. A derived 1-hour (3600 s) cache write for the Anthropic family: twice the
//!    effective input price, only when a refreshed or shipped row was
//!    selected from the `anthropic` table provider (or for a `bedrock` target
//!    whose upstream is an Anthropic model) and no source above gives the
//!    field. A gateway priced from another table provider gets no derived
//!    figure, whatever its kind.
//! 5. Unknown.

use std::collections::BTreeMap;

use routectl_core::provider_kind::config as kind;
use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

use super::{PriceTable, SelectedRow};
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
    /// Whether this source is a price table layer: `refreshed`, `shipped`,
    /// or `derived` from one of them.
    pub const fn is_table(self) -> bool {
        matches!(self, Self::Refreshed | Self::Shipped | Self::Derived)
    }

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceSources {
    /// Source of `input_per_mtok`.
    pub input: PriceSource,
    /// Source of `output_per_mtok`.
    pub output: PriceSource,
    /// Source of `cache_read_per_mtok`.
    pub cache_read: PriceSource,
    /// Source of each priced cache-write lifetime, keyed by lifetime in
    /// seconds (see [`PricingConfig::cache_write_rates`]). A lifetime with no
    /// entry is unknown.
    pub cache_write: BTreeMap<u32, PriceSource>,
}

impl PriceSources {
    /// Every field unknown.
    pub const UNKNOWN: Self = Self {
        input: PriceSource::Unknown,
        output: PriceSource::Unknown,
        cache_read: PriceSource::Unknown,
        cache_write: BTreeMap::new(),
    };

    /// The source of the cache-write price for the lifetime `seconds`.
    pub fn cache_write_at(&self, seconds: u32) -> PriceSource {
        self.cache_write
            .get(&seconds)
            .copied()
            .unwrap_or(PriceSource::Unknown)
    }

    /// Every field's source: input, output, cache read, then each
    /// cache-write lifetime in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = PriceSource> + '_ {
        [self.input, self.output, self.cache_read]
            .into_iter()
            .chain(self.cache_write.values().copied())
    }

    /// Whether every field is unknown.
    pub fn is_unknown(&self) -> bool {
        self.iter().all(|source| source == PriceSource::Unknown)
    }

    /// Whether any field came from a price table layer (`refreshed`,
    /// `shipped` or `derived`). A `[registry]` row wins whole, so a row it
    /// prices never answers true.
    pub fn any_from_table(&self) -> bool {
        self.iter().any(PriceSource::is_table)
    }
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

    let mut layers: Vec<Layer<'_>> = Vec::with_capacity(4);
    layers.extend(model_config.map(|p| Layer::new(p, PriceSource::ModelConfig)));
    layers.extend(registry.map(|p| Layer::new(p, PriceSource::Registry)));
    layers.extend(
        tables
            .refreshed
            .as_ref()
            .map(|row| Layer::new(&row.prices, PriceSource::Refreshed)),
    );
    layers.extend(
        tables
            .shipped
            .as_ref()
            .map(|row| Layer::new(&row.prices, PriceSource::Shipped)),
    );

    let mut pricing = PricingConfig::default();
    let mut sources = PriceSources::UNKNOWN;
    for field in Field::all(&layers) {
        if let Some((value, source)) = layers
            .iter()
            .find_map(|layer| field.get(layer).map(|v| (v, layer.source)))
        {
            field.set(&mut pricing, &mut sources, value, source);
        }
    }

    if tables.derives_1h_write(subject.upstream) {
        derive_1h_write(&mut pricing, &mut sources);
    }

    if registry.is_none() && sources.is_unknown() {
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
struct TableRows<'t> {
    kind: Option<&'t str>,
    refreshed: Option<SelectedRow<'t>>,
    shipped: Option<SelectedRow<'t>>,
}

impl<'t> TableRows<'t> {
    fn select(
        config: &'t Config,
        subject: &PriceSubject<'t>,
        refreshed: Option<&'t PriceTable>,
        shipped: Option<&'t PriceTable>,
    ) -> Self {
        match config.provider_entry_for_target(subject.provider) {
            Some((_, entry)) => {
                let lookup = |table: &'t PriceTable| {
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
                    |table: &'t PriceTable| table.price_for_target(kind, None, subject.upstream);
                Self {
                    kind: Some(kind),
                    refreshed: refreshed.and_then(lookup),
                    shipped: shipped.and_then(lookup),
                }
            }
        }
    }

    /// Whether a derived 1-hour write applies: a selected row sits under the
    /// `anthropic` table provider, or the target is Bedrock serving an
    /// Anthropic model.
    fn derives_1h_write(&self, upstream: &str) -> bool {
        let rows = [self.refreshed.as_ref(), self.shipped.as_ref()];
        if rows.iter().all(Option::is_none) {
            return false;
        }
        let anthropic_row = rows
            .iter()
            .flatten()
            .any(|row| row.provider == ANTHROPIC_TABLE_PROVIDER);
        let bedrock_anthropic =
            self.kind == Some(kind::BEDROCK) && anthropic_family(upstream) == AnthropicFamily::Yes;
        anthropic_row || bedrock_anthropic
    }
}

/// The table provider whose rows carry Anthropic's first-party prices.
const ANTHROPIC_TABLE_PROVIDER: &str = "anthropic";

fn derive_1h_write(pricing: &mut PricingConfig, sources: &mut PriceSources) {
    const ONE_HOUR: u32 = PricingConfig::CACHE_WRITE_1H_SECONDS;
    if pricing.cache_write_rates().contains_key(&ONE_HOUR) {
        return;
    }
    let Some(input) = pricing.input_per_mtok else {
        return;
    };
    Field::CacheWrite(ONE_HOUR).set(
        pricing,
        sources,
        input * ANTHROPIC_1H_WRITE_INPUT_MULTIPLE,
        PriceSource::Derived,
    );
}

/// One pricing layer with its cache-write prices resolved to lifetimes.
struct Layer<'a> {
    pricing: &'a PricingConfig,
    cache_write: BTreeMap<u32, f64>,
    source: PriceSource,
}

impl<'a> Layer<'a> {
    fn new(pricing: &'a PricingConfig, source: PriceSource) -> Self {
        Self {
            pricing,
            cache_write: pricing.cache_write_rates(),
            source,
        }
    }
}

/// One price field, addressed the same way in a [`PricingConfig`] and a
/// [`PriceSources`]. Each cache-write lifetime is its own field.
#[derive(Clone, Copy)]
enum Field {
    Input,
    Output,
    CacheRead,
    CacheWrite(u32),
}

impl Field {
    /// The fixed fields, then every cache-write lifetime any layer prices,
    /// in ascending order.
    fn all(layers: &[Layer<'_>]) -> Vec<Self> {
        let lifetimes: std::collections::BTreeSet<u32> = layers
            .iter()
            .flat_map(|layer| layer.cache_write.keys().copied())
            .collect();
        [Self::Input, Self::Output, Self::CacheRead]
            .into_iter()
            .chain(lifetimes.into_iter().map(Self::CacheWrite))
            .collect()
    }

    fn get(self, layer: &Layer<'_>) -> Option<f64> {
        match self {
            Self::Input => layer.pricing.input_per_mtok,
            Self::Output => layer.pricing.output_per_mtok,
            Self::CacheRead => layer.pricing.cache_read_per_mtok,
            Self::CacheWrite(seconds) => layer.cache_write.get(&seconds).copied(),
        }
    }

    fn set(
        self,
        pricing: &mut PricingConfig,
        sources: &mut PriceSources,
        value: f64,
        source: PriceSource,
    ) {
        match self {
            Self::Input => {
                pricing.input_per_mtok = Some(value);
                sources.input = source;
            }
            Self::Output => {
                pricing.output_per_mtok = Some(value);
                sources.output = source;
            }
            Self::CacheRead => {
                pricing.cache_read_per_mtok = Some(value);
                sources.cache_read = source;
            }
            Self::CacheWrite(seconds) => {
                pricing.set_cache_write_rate(seconds, value);
                sources.cache_write.insert(seconds, source);
            }
        }
    }
}
