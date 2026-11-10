//! Usage-cost price tables: the routectl-owned canonical on-disk format, its
//! bounded parser, the models.dev converter, and provider selection.
//!
//! A table is display-only data: it feeds usage cost and nothing in routing
//! or the providers reads it. Its bytes come from a download (or an operator
//! file) and are untrusted, so every path into a [`PriceTable`] goes through
//! one validation step that fails closed on malformed input.
//!
//! # Canonical format (schema version 1)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "generated_at": "YYYY-MM-DD",
//!   "providers": [
//!     {
//!       "id": "opencode-go",
//!       "api_host": "https://opencode.ai/zen/go/v1",
//!       "models": [
//!         { "id": "glm-5", "input": 1.0, "output": 3.2, "cache_read": 0.2,
//!           "cache_write_5m": 1.25, "cache_write_1h": 2.0 }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Prices are USD per million tokens and every one is optional. `api_host`
//! is optional (first-party providers carry none) and is written with a
//! lowercased host and no default port; any form the origin normalizer
//! accepts reads back to the same provider. Unknown fields are
//! tolerated at every level so a newer writer stays readable; a
//! `schema_version` other than 1 is refused. [`PriceTable::to_canonical_json`]
//! writes the format pretty-printed with providers and models sorted by id.
//!
//! # Validation
//!
//! [`parse_and_validate`] and [`from_models_dev`] share one rule set: input
//! over [`MAX_INPUT_BYTES`] is refused; more than 100000 source model entries
//! (counted while parsing, priced or not) or 100000 providers, a provider id
//! empty or over 128 bytes, a model id empty or over 512 bytes, any id byte
//! outside printable ASCII `0x21..=0x7E`, a duplicate provider, a duplicate
//! (provider, model) row, an `api_host` that does not normalize to an origin,
//! a `generated_at` that is not a calendar date, and a table with no priced
//! model are all malformed. A price that is not finite, is negative or
//! exceeds 10000 $/Mtok is dropped to `None` (unknown, never zero); a known
//! `0` is kept.
//!
//! # No download string is rendered
//!
//! Provider ids, model ids and `api_host` values never leave this module
//! except as the comparison side of a lookup the caller keys with its own
//! strings, and in the canonical bytes written back to disk. Errors carry
//! fixed text and positions only, `Debug` on a table prints its date and
//! counts, and [`TableDate`] renders from parsed numbers.
//!
//! # Provider selection
//!
//! [`PriceTable::price_for`] picks the table provider for a config target:
//!
//! 1. The normalized origin of the target's egress base URL against each
//!    provider's `api_host`; a loopback base URL never matches. Several
//!    providers on one origin are split by the longest common path prefix,
//!    and a tie is unknown.
//! 2. With no origin match, a first-party fallback for the kinds with a fixed
//!    vendor: anthropic-api -> `anthropic`, openai-responses -> `openai`,
//!    gemini -> `google`, bedrock -> `amazon-bedrock`.
//! 3. The exact upstream id inside the selected provider. A miss is unknown.
//!
//! A pool target uses its first member that names a provider entry, the same
//! member the config's provider-kind resolution reads.
//!
//! # The refreshed table file
//!
//! [`PriceBook`] holds the table the refresh writes into the config
//! directory and re-reads it when the file's modification time or length
//! changes, keeping the last good table when a new copy is refused.
//!
//! # The shipped table
//!
//! [`shipped_table`] is the table embedded in the build
//! (`prices_shipped.json`, the same refresh's output at release time, under
//! the models.dev MIT license in `prices_shipped.NOTICE`), parsed once.
//!
//! # Effective pricing
//!
//! [`effective_pricing`] combines `[models.X.pricing]`, a matching
//! `[registry]` row, the refreshed and shipped tables and a derived 1-hour
//! cache-write price per field, naming each field's [`PriceSource`].

mod book;
mod bounded;
mod effective;
mod format;
mod models_dev;
mod select;
mod shipped;
mod validate;

use std::collections::BTreeMap;
use std::fmt;

use crate::config::{Config, PricingConfig};

pub use book::PriceBook;
pub use effective::{PriceSource, PriceSources, PriceSubject, effective_pricing};
pub use format::TableDate;
pub use models_dev::from_models_dev;
pub(crate) use select::provider_entry_for_target;
pub use shipped::shipped_table;
pub use validate::{MAX_INPUT_BYTES, parse_and_validate};

/// A validated price table. Built only by [`parse_and_validate`] or
/// [`from_models_dev`], so every instance satisfies the module's bounds.
#[derive(Clone, PartialEq)]
pub struct PriceTable {
    generated_at: TableDate,
    /// Sorted by id; ids are unique.
    providers: Vec<ProviderPrices>,
}

/// One table provider. Fields stay private to the module: the id and
/// `api_host` are download strings.
#[derive(Clone, PartialEq)]
struct ProviderPrices {
    id: String,
    api: Option<format::ApiBase>,
    models: BTreeMap<String, PricingConfig>,
}

impl PriceTable {
    /// The date the table was generated.
    pub const fn generated_at(&self) -> TableDate {
        self.generated_at
    }

    /// How many providers the table lists.
    pub const fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// How many model rows the table holds across every provider.
    pub fn model_count(&self) -> usize {
        self.providers.iter().map(|p| p.models.len()).sum()
    }

    /// Prices for `upstream` served through `provider` (a `[providers]` key
    /// or a `[pools]` key) under `config`, selected as the module docs
    /// describe. `None` when no provider is selected or the selected provider
    /// has no row for the exact upstream id. A returned field left `None` is
    /// an unknown price.
    ///
    /// A usage row passes the provider entry that served the request, never
    /// the pool it was routed through: a pool resolves to its first member,
    /// which may not be the member that answered. A row whose provider entry
    /// no longer exists in `config` uses [`Self::price_for_target`] with the
    /// kind and base URL it recorded.
    pub fn price_for(
        &self,
        config: &Config,
        provider: &str,
        upstream: &str,
    ) -> Option<PricingConfig> {
        let entry = select::provider_entry_for_target(config, provider)?;
        let base_url = entry.classifiable_egress_base_url();
        self.price_for_target(entry.kind_str(), base_url.as_deref(), upstream)
    }

    /// Prices for `upstream` on a target of provider `kind` (a
    /// `[providers]` `kind` string) egressing to `base_url`, selected as the
    /// module docs describe, with no config lookup. This is the path for a
    /// usage row whose provider entry no longer exists in the config; pass
    /// `None` for `base_url` when none was recorded, which leaves only the
    /// first-party fallback.
    pub fn price_for_target(
        &self,
        kind: &str,
        base_url: Option<&str>,
        upstream: &str,
    ) -> Option<PricingConfig> {
        self.select_provider(kind, base_url)?
            .models
            .get(upstream)
            .cloned()
    }

    /// The table serialized in the canonical format: pretty-printed, sorted,
    /// newline-terminated. This is the one place table strings leave the
    /// module, as the bytes written back to the table file. An error means
    /// nothing should be written.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, PriceTableError> {
        format::to_canonical_json(self)
    }

    fn provider(&self, id: &str) -> Option<&ProviderPrices> {
        self.providers
            .binary_search_by(|p| p.id.as_str().cmp(id))
            .ok()
            .map(|index| &self.providers[index])
    }
}

impl fmt::Debug for PriceTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PriceTable")
            .field("generated_at", &self.generated_at)
            .field("providers", &self.providers.len())
            .field(
                "models",
                &self.providers.iter().map(|p| p.models.len()).sum::<usize>(),
            )
            .finish()
    }
}

/// Why a table was refused. Every variant carries fixed text and numbers
/// only, never bytes from the input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PriceTableError {
    /// The input is larger than [`MAX_INPUT_BYTES`].
    #[error("price table input exceeds {MAX_INPUT_BYTES} bytes")]
    TooLarge,
    /// The input is not JSON of the expected shape.
    #[error("price table is malformed at line {line}, column {column}")]
    Syntax {
        /// 1-based line of the parse failure.
        line: usize,
        /// 1-based column of the parse failure.
        column: usize,
    },
    /// `schema_version` is missing or not 1.
    #[error("price table schema_version is not supported (expected 1)")]
    UnsupportedSchemaVersion,
    /// `generated_at` is not a `YYYY-MM-DD` calendar date.
    #[error("price table generated_at is not a YYYY-MM-DD date")]
    InvalidDate,
    /// The table holds more model rows than the bound allows.
    #[error("price table has more than {limit} model rows")]
    TooManyRows {
        /// The row bound.
        limit: usize,
    },
    /// A provider id is empty or longer than the bound.
    #[error("price table provider id is empty or longer than {limit} bytes")]
    ProviderIdLength {
        /// The byte bound.
        limit: usize,
    },
    /// A model id is empty or longer than the bound.
    #[error("price table model id is empty or longer than {limit} bytes")]
    ModelIdLength {
        /// The byte bound.
        limit: usize,
    },
    /// The table lists more providers than the bound allows.
    #[error("price table has more than {limit} providers")]
    TooManyProviders {
        /// The provider bound.
        limit: usize,
    },
    /// A provider id holds a byte outside printable ASCII `0x21..=0x7E`.
    #[error("price table provider id holds a byte outside printable ASCII")]
    ProviderIdCharset,
    /// A model id holds a byte outside printable ASCII `0x21..=0x7E`.
    #[error("price table model id holds a byte outside printable ASCII")]
    ModelIdCharset,
    /// Two providers share an id.
    #[error("price table lists a provider twice")]
    DuplicateProvider,
    /// One provider lists a model id twice.
    #[error("price table lists a (provider, model) row twice")]
    DuplicateModel,
    /// A provider's `api_host` does not normalize to an http(s) origin.
    #[error("price table api_host is not a usable http(s) URL")]
    InvalidApiHost,
    /// No model carries any admitted price.
    #[error("price table has no priced model")]
    NoPricedModels,
    /// The table could not be serialized; nothing should be written.
    #[error("price table could not be serialized")]
    Serialize,
}

impl PriceTableError {
    fn syntax(err: &serde_json::Error) -> Self {
        Self::Syntax {
            line: err.line(),
            column: err.column(),
        }
    }
}

#[cfg(test)]
#[path = "bounds_tests.rs"]
mod bounds_tests;
#[cfg(test)]
#[path = "effective_tests.rs"]
mod effective_tests;
#[cfg(test)]
#[path = "models_dev_tests.rs"]
mod models_dev_tests;
#[cfg(test)]
mod prices_test_support;
#[cfg(test)]
#[path = "select_tests.rs"]
mod select_tests;
#[cfg(test)]
#[path = "shipped_tests.rs"]
mod shipped_tests;
#[cfg(test)]
#[path = "slice_weld_tests.rs"]
mod slice_weld_tests;
#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
